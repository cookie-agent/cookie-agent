//! Share links on the daemon's listeners (`docs/site/specs/share-links.md`
//! section 5): `/s/<id>` is a link's landing page, `/s/<id>/raw/<name>` its
//! asset's bytes inline, and `/s/<id>/download/<name>` the same as a
//! download. No token, ticket, or origin is checked: the ID is the only
//! key. Every failure answers the same 404 page, so nobody learns whether
//! a link ever existed.
//!
//! A conversation share's `/s/<id>` is the guest page
//! (`docs/site/specs/conversation-shares.md` section 6): the web client's own
//! page, whose files are at `/s/_app/`, in a sandbox that gives it an opaque
//! origin, so it cannot touch what the daemon's page keeps on this origin.

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use cookie_agent_engine::{Engine, GuestLink, OpenedLink};
use cookie_agent_protocol::ShareId;

use super::{share_landing, websocket::WebSocketState};

const PREFIX: &str = "/s/";
/// The landing page's font, below [`PREFIX`]: no ID is this short.
pub(super) const FONT_PATH: &str = "_font/inter.woff2";
/// Where the web bundle keeps the font.
const BUNDLED_FONT: &str = "fonts/InterVariable.woff2";

/// The policy of shared HTML: the viewer's, whose `sandbox` gives the page
/// an opaque origin even when opened at the top level.
const HTML_POLICY: &str = cookie_agent_web_assets::VIEWER_POLICY;
/// The policy of shared SVG: sandboxed, and no scripts.
const SVG_POLICY: &str = "sandbox; default-src 'none'; style-src 'unsafe-inline'; \
     img-src data:; font-src data:; frame-ancestors 'self'";
/// The policy of every other inline type but PDF, which the browser's
/// viewer cannot show sandboxed.
const PLAIN_POLICY: &str = "sandbox; default-src 'none'; img-src 'self'; media-src 'self'; \
     style-src 'unsafe-inline'; frame-ancestors 'self'";
const PDF_POLICY: &str = "frame-ancestors 'self'";

/// The directory below `/s/` the guest page's files are in: no ID is this
/// short.
const APP_PREFIX: &str = "_app/";
/// The sandbox of the guest page. Links it opens are ordinary tabs.
const GUEST_SANDBOX: &str = "sandbox allow-scripts allow-forms allow-modals allow-downloads \
     allow-popups allow-popups-to-escape-sandbox";

/// What a path below `/s/` asks for.
#[derive(Debug, Eq, PartialEq)]
enum Wanted {
    Landing(ShareId),
    /// `/s/<id>/`: the landing page without its trailing slash.
    Slash(ShareId),
    Raw(ShareId),
    Download(ShareId),
    Font,
    /// A file of the guest page.
    App(String),
    /// A guest's connection.
    Socket(ShareId),
}

fn wanted(path: &str) -> Option<Wanted> {
    let below = path.strip_prefix(PREFIX)?;
    if below == FONT_PATH {
        return Some(Wanted::Font);
    }
    if let Some(file) = below.strip_prefix(APP_PREFIX) {
        return Some(Wanted::App(file.to_owned()));
    }
    let (id, rest) = match below.find('/') {
        Some(slash) => (&below[..slash], &below[slash..]),
        None => (below, ""),
    };
    let id = ShareId::new(id).ok()?;
    match rest {
        "" => Some(Wanted::Landing(id)),
        "/" => Some(Wanted::Slash(id)),
        rest => {
            let (kind, _name) = match rest[1..].find('/') {
                Some(slash) => (&rest[1..=slash], &rest[slash + 2..]),
                None => (&rest[1..], ""),
            };
            match kind {
                "raw" => Some(Wanted::Raw(id)),
                "download" => Some(Wanted::Download(id)),
                "ws" if rest == "/ws" => Some(Wanted::Socket(id)),
                _ => None,
            }
        }
    }
}

/// Serves a path below `/s/`.
pub(crate) async fn share_pages(State(state): State<WebSocketState>, request: Request) -> Response {
    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return gone();
    }
    let Some(wanted) = wanted(request.uri().path()) else {
        return gone();
    };
    if wanted == Wanted::Font {
        return font();
    }
    if let Wanted::App(file) = &wanted {
        return app_file(file, request.headers());
    }
    if let Wanted::Socket(id) = wanted {
        return super::guest::guest_upgrade(state, id, request).await;
    }
    let Some(server) = state.server.get() else {
        return gone();
    };
    let engine = &server.engine;
    let id = match &wanted {
        Wanted::Landing(id) | Wanted::Slash(id) | Wanted::Raw(id) | Wanted::Download(id) => {
            id.clone()
        }
        Wanted::Font | Wanted::App(_) | Wanted::Socket(_) => unreachable!("answered above"),
    };
    let open = match engine.open_share(id.clone()).await {
        Ok(Some(OpenedLink::Asset(open))) => *open,
        Ok(Some(OpenedLink::Session(link))) => {
            return match wanted {
                Wanted::Slash(id) => redirect(&format!("../{id}")),
                Wanted::Landing(_) => guest_page(engine, &link),
                _ => gone(),
            };
        }
        _ => return gone(),
    };
    match wanted {
        Wanted::Slash(id) => redirect(&format!("../{id}")),
        Wanted::Landing(_) => {
            let kind = share_landing::Kind::of(&open.asset.media_type);
            let content = if kind.reads_content() {
                match engine.read_shared_asset(&open.asset).await {
                    Ok(Some(bytes)) => Some(bytes),
                    _ => return gone(),
                }
            } else {
                None
            };
            if method == Method::GET {
                engine.count_share_view(&id);
            }
            let page = share_landing::page(&open, kind, content.as_deref(), jiff::Timestamp::now());
            let mut headers = common_headers();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            headers.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(share_landing::POLICY),
            );
            (StatusCode::OK, headers, page).into_response()
        }
        Wanted::Raw(_) | Wanted::Download(_) => {
            let Ok(Some(bytes)) = engine.read_shared_asset(&open.asset).await else {
                return gone();
            };
            let download = matches!(wanted, Wanted::Download(_))
                || !share_landing::Kind::of(&open.asset.media_type).inline();
            bytes_response(
                request.headers(),
                &open.asset.media_type,
                &share_landing::file_name(&open.asset),
                download,
                bytes,
            )
        }
        Wanted::Font | Wanted::App(_) | Wanted::Socket(_) => unreachable!("answered above"),
    }
}

/// A file of the guest page. Its page has an opaque origin, so every file it
/// loads is another origin's and answers to any.
fn app_file(file: &str, headers: &HeaderMap) -> Response {
    // A frame of `viewer.html` is framed by the guest page, whose opaque
    // origin is never `'self'`; nothing is in it until the page hands it over.
    let framed = cookie_agent_web_assets::VIEWER_POLICY
        .replace("frame-ancestors 'self'", "frame-ancestors *");
    let policy = if file == cookie_agent_web_assets::VIEWER_PATH {
        framed.as_str()
    } else {
        cookie_agent_web_assets::content_security_policy()
    };
    // `index.html` is the page itself, which is only served in its sandbox.
    let served = (file != "index.html")
        .then(|| super::page::file(file, headers, policy))
        .flatten();
    let Some(mut response) = served else {
        return gone();
    };
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response
}

/// The guest page: the web client's page, reading its files from
/// [`APP_PREFIX`], in its sandbox. The first thing a link preview sees of it
/// is the session's title.
fn guest_page(engine: &Engine, link: &GuestLink) -> Response {
    let Some(index) = cookie_agent_web_assets::get("index.html") else {
        return gone();
    };
    let title = engine
        .session_meta(link.session)
        .ok()
        .and_then(|meta| meta.title)
        .map_or_else(
            || "A shared conversation".to_owned(),
            |title| title.as_str().to_owned(),
        );
    let page = guest_page_html(&String::from_utf8_lossy(index.bytes), &title);
    let mut headers = common_headers();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    if let Ok(policy) = HeaderValue::from_str(&guest_policy()) {
        headers.insert(header::CONTENT_SECURITY_POLICY, policy);
    }
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    (StatusCode::OK, headers, page).into_response()
}

/// The page's own policy with the sandbox, and a `<base>` for its files.
fn guest_policy() -> String {
    let policy = cookie_agent_web_assets::content_security_policy()
        .replace("base-uri 'none'", "base-uri 'self'");
    format!("{GUEST_SANDBOX}; {policy}")
}

/// `index` as the guest page: its relative URLs read below [`APP_PREFIX`],
/// its stylesheet loads across origins as its integrity check needs, and it
/// carries `title` for link previews.
fn guest_page_html(index: &str, title: &str) -> String {
    let title = share_landing::escape(title);
    let head = format!(
        "<head><base href=\"{APP_PREFIX}\" />\
         <meta property=\"og:title\" content=\"{title}\" />\
         <meta property=\"og:type\" content=\"website\" />\
         <meta property=\"og:description\" content=\"A conversation shared from cookie agent\" />"
    );
    index
        .replacen("<head>", &head, 1)
        .replace(
            "<link rel=\"stylesheet\"",
            "<link crossorigin=\"anonymous\" rel=\"stylesheet\"",
        )
        .replacen(
            "<title>Cookie Agent</title>",
            &format!("<title>{title}</title>"),
            1,
        )
}

/// Serves only `/s/…`; anything else on the share-only listener is 404.
pub(crate) async fn not_shared() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// The headers every answer carries.
fn common_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex, nofollow"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers
}

/// The one answer to every link that does not work. Any page may read it: a
/// guest page, whose origin is opaque, asks after its own link.
fn gone() -> Response {
    let mut headers = common_headers();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(share_landing::POLICY),
    );
    (StatusCode::NOT_FOUND, headers, share_landing::GONE_PAGE).into_response()
}

fn redirect(location: &str) -> Response {
    let mut headers = common_headers();
    match HeaderValue::from_str(location) {
        Ok(location) => {
            headers.insert(header::LOCATION, location);
            (StatusCode::PERMANENT_REDIRECT, headers).into_response()
        }
        Err(_) => gone(),
    }
}

fn font() -> Response {
    let Some(font) = cookie_agent_web_assets::get(BUNDLED_FONT) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("font/woff2"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    (StatusCode::OK, headers, font.bytes).into_response()
}

/// The policy a shared asset's bytes are served with inline.
fn inline_policy(media_type: &str) -> &'static str {
    match media_type {
        "text/html" => HTML_POLICY,
        "image/svg+xml" => SVG_POLICY,
        "application/pdf" => PDF_POLICY,
        _ => PLAIN_POLICY,
    }
}

/// The `Content-Type` a shared asset's bytes are served with: text as
/// UTF-8 plain text, so a browser never runs what it is not meant to.
fn served_type(media_type: &str) -> String {
    match media_type {
        "text/html" => "text/html; charset=utf-8".to_owned(),
        "image/svg+xml" => media_type.to_owned(),
        _ if share_landing::Kind::of(media_type) == share_landing::Kind::Text
            || share_landing::Kind::of(media_type) == share_landing::Kind::Markdown =>
        {
            "text/plain; charset=utf-8".to_owned()
        }
        _ => media_type.to_owned(),
    }
}

/// `Content-Disposition` for `name`, with an ASCII fallback and the UTF-8
/// name (RFC 6266).
fn disposition(kind: &str, name: &str) -> String {
    let ascii = name
        .chars()
        .map(|character| {
            if character.is_ascii_graphic() && character != '"' && character != '\\' {
                character
            } else if character == ' ' {
                ' '
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!(
        "{kind}; filename=\"{ascii}\"; filename*=UTF-8''{}",
        share_landing::percent_encode(name)
    )
}

fn bytes_response(
    request: &HeaderMap,
    media_type: &str,
    name: &str,
    download: bool,
    bytes: Vec<u8>,
) -> Response {
    let mut headers = common_headers();
    let content_type = if download {
        "application/octet-stream".to_owned()
    } else {
        served_type(media_type)
    };
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&disposition(
        if download { "attachment" } else { "inline" },
        name,
    )) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    if !download {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(inline_policy(media_type)),
        );
    }
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let total = bytes.len() as u64;
    match request
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(|value| byte_range(value, total))
    {
        None => (StatusCode::OK, headers, Body::from(bytes)).into_response(),
        Some(Some((start, end))) => {
            if let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")) {
                headers.insert(header::CONTENT_RANGE, value);
            }
            let part = bytes[start as usize..=end as usize].to_vec();
            (StatusCode::PARTIAL_CONTENT, headers, Body::from(part)).into_response()
        }
        Some(None) => {
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{total}")) {
                headers.insert(header::CONTENT_RANGE, value);
            }
            (StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response()
        }
    }
}

/// The one range `bytes=a-b`, `bytes=a-`, or `bytes=-n` asks for of
/// `total` bytes, inclusive; `None` when it cannot be satisfied. Several
/// ranges are answered with the first.
fn byte_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    let first = spec.split(',').next()?.trim();
    let (start, end) = first.split_once('-')?;
    if total == 0 {
        return None;
    }
    let last = total - 1;
    let (start, end) = match (start.trim(), end.trim()) {
        ("", suffix) => {
            let length = suffix.parse::<u64>().ok().filter(|length| *length > 0)?;
            (total.saturating_sub(length), last)
        }
        (start, "") => (start.parse().ok()?, last),
        (start, end) => (start.parse().ok()?, end.parse::<u64>().ok()?.min(last)),
    };
    (start <= end && start <= last).then_some((start, end))
}

#[cfg(test)]
mod daemon_tests;
#[cfg(test)]
mod guest_tests;

#[cfg(test)]
mod tests {
    use super::{Wanted, byte_range, disposition, wanted};

    const ID: &str = "q3Vt0x9LmA2bCdEfGh1iJk";

    #[test]
    fn paths_name_a_link_and_what_of_it() {
        let id = || cookie_agent_protocol::ShareId::new(ID).unwrap();
        assert_eq!(wanted(&format!("/s/{ID}")), Some(Wanted::Landing(id())));
        assert_eq!(wanted(&format!("/s/{ID}/")), Some(Wanted::Slash(id())));
        assert_eq!(
            wanted(&format!("/s/{ID}/raw/loss.html")),
            Some(Wanted::Raw(id()))
        );
        assert_eq!(wanted(&format!("/s/{ID}/raw")), Some(Wanted::Raw(id())));
        assert_eq!(
            wanted(&format!("/s/{ID}/download/a/b.png")),
            Some(Wanted::Download(id()))
        );
        assert_eq!(wanted("/s/_font/inter.woff2"), Some(Wanted::Font));
        for path in [
            "/s/",
            "/s/short",
            &format!("/s/{ID}/other/x"),
            &format!("/s/{ID}x"),
            "/plugins/x",
        ] {
            assert_eq!(wanted(path), None, "{path}");
        }
    }

    #[test]
    fn a_range_is_one_satisfiable_span() {
        assert_eq!(byte_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(byte_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(byte_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(byte_range("bytes=-2000", 1000), Some((0, 999)));
        assert_eq!(byte_range("bytes=990-2000", 1000), Some((990, 999)));
        assert_eq!(byte_range("bytes=0-0,5-9", 1000), Some((0, 0)));
        assert_eq!(byte_range("bytes=1000-", 1000), None);
        assert_eq!(byte_range("bytes=5-4", 1000), None);
        assert_eq!(byte_range("bytes=0-", 0), None);
        assert_eq!(byte_range("items=0-1", 1000), None);
    }

    #[test]
    fn a_disposition_keeps_an_ascii_name_and_the_utf8_one() {
        assert_eq!(
            disposition("attachment", "loss curves.png"),
            "attachment; filename=\"loss curves.png\"; filename*=UTF-8''loss%20curves.png"
        );
        assert_eq!(
            disposition("inline", "résumé\".md"),
            "inline; filename=\"r_sum__.md\"; filename*=UTF-8''r%C3%A9sum%C3%A9%22.md"
        );
    }
}
