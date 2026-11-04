//! A share link's landing page (`docs/site/specs/share-links.md` section
//! 5.1): the title, what the file is, a preview, *Download*, and *Open full
//! page*, as static HTML with no script, in the web client's Quiet Ink
//! palette. The mockup is `docs/site/specs/share-links/preview.html`.
//!
//! The page is served at `/s/<id>`, so its links are relative to `/s/`:
//! `<id>/raw/<name>`, `<id>/download/<name>`, and the font.

use std::fmt::Write as _;

use cookie_agent_engine::OpenShare;
use cookie_agent_protocol::{Asset, human_bytes};

use super::share_pages::FONT_PATH;

/// The landing page's policy: no script, its own styles, and the asset
/// from its own origin.
pub(super) const POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; \
     font-src 'self'; img-src 'self'; media-src 'self'; frame-src 'self'; \
     frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// How much of a text file the page shows.
pub(super) const TEXT_PREVIEW_BYTES: usize = 256 * 1024;

/// How the page previews an asset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Kind {
    Html,
    Image,
    Video,
    Audio,
    Pdf,
    Markdown,
    Text,
    File,
}

impl Kind {
    pub(super) fn of(media_type: &str) -> Self {
        let essence = media_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match essence.as_str() {
            "text/html" => Self::Html,
            "text/markdown" => Self::Markdown,
            "application/pdf" => Self::Pdf,
            "application/json"
            | "application/xml"
            | "application/x-yaml"
            | "application/yaml"
            | "application/toml"
            | "application/javascript" => Self::Text,
            other if other.starts_with("image/") => Self::Image,
            other if other.starts_with("video/") => Self::Video,
            other if other.starts_with("audio/") => Self::Audio,
            other if other.starts_with("text/") => Self::Text,
            _ => Self::File,
        }
    }

    /// Whether the page shows the content itself, read by the daemon.
    pub(super) const fn reads_content(self) -> bool {
        matches!(self, Self::Markdown | Self::Text)
    }

    /// Whether `raw` shows the bytes in the browser rather than downloading
    /// them.
    pub(super) const fn inline(self) -> bool {
        !matches!(self, Self::File)
    }

    /// Whether the page offers *Open full page*.
    const fn opens(self) -> bool {
        matches!(self, Self::Html | Self::Image | Self::Video | Self::Pdf)
    }

    fn label(self, media_type: &str) -> String {
        match self {
            Self::Html => "HTML page".to_owned(),
            Self::Markdown => "Markdown".to_owned(),
            Self::Pdf => "PDF".to_owned(),
            Self::Image if media_type == "image/svg+xml" => "SVG image".to_owned(),
            Self::Image | Self::Video | Self::Audio => {
                let (_, subtype) = media_type.split_once('/').unwrap_or(("", media_type));
                let noun = match self {
                    Self::Image => "image",
                    Self::Video => "video",
                    _ => "audio",
                };
                format!("{} {noun}", subtype.to_ascii_uppercase())
            }
            Self::Text | Self::File => media_type.to_owned(),
        }
    }
}

/// `&`, `<`, `>`, `"`, and `'` escaped for HTML text and attributes.
pub(super) fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// `text` percent-encoded as one URL path segment: everything but
/// unreserved characters.
pub(super) fn percent_encode(text: &str) -> String {
    text.bytes().fold(String::new(), |mut encoded, byte| {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
        encoded
    })
}

/// The extension a file of `media_type` is saved with.
fn extension(media_type: &str) -> &'static str {
    match media_type {
        "text/html" => "html",
        "text/markdown" => "md",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "application/json" => "json",
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "audio/wav" => "wav",
        "audio/ogg" => "ogg",
        _ => "bin",
    }
}

/// The name the asset is downloaded as: the file it was written from, or
/// its title slugged with its type's extension.
pub(crate) fn file_name(asset: &Asset) -> String {
    if let Some(name) = asset.filename.as_deref().filter(|name| !name.is_empty()) {
        return name.to_owned();
    }
    let mut slug = String::new();
    for character in asset.title.chars() {
        if character.is_alphanumeric() {
            slug.extend(character.to_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() { "asset" } else { slug };
    format!("{slug}.{}", extension(&asset.media_type))
}

/// "in 7 days", "in 5 hours", or "in a few minutes", each rounded to the
/// nearest, so a 7-day link opened a minute after it was made says 7 days;
/// from a day and a half on it counts days, from 23½ hours it is a day.
fn time_left(now: jiff::Timestamp, at: jiff::Timestamp) -> String {
    const HOUR: i64 = 3600;
    let seconds = at.as_second() - now.as_second();
    let hours = (seconds + HOUR / 2) / HOUR;
    if seconds >= 36 * HOUR {
        format!("in {} days", (seconds + 12 * HOUR) / (24 * HOUR))
    } else if seconds >= 23 * HOUR + HOUR / 2 {
        "in 1 day".to_owned()
    } else if hours >= 2 {
        format!("in {hours} hours")
    } else if hours == 1 {
        "in 1 hour".to_owned()
    } else {
        "in a few minutes".to_owned()
    }
}

/// Markdown rendered to HTML, with raw HTML in the source escaped as text.
fn markdown(source: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, html};

    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let events = Parser::new_ext(source, options).map(|event| match event {
        Event::Html(text) | Event::InlineHtml(text) => Event::Text(text),
        other => other,
    });
    let mut rendered = String::new();
    html::push_html(&mut rendered, events);
    rendered
}

/// At most [`TEXT_PREVIEW_BYTES`] of `bytes` as text, cut at a character,
/// and whether it was cut; `None` when it is not UTF-8.
fn text_preview(bytes: &[u8]) -> Option<(&str, bool)> {
    let cut = bytes.len() > TEXT_PREVIEW_BYTES;
    let mut end = bytes.len().min(TEXT_PREVIEW_BYTES);
    loop {
        match std::str::from_utf8(&bytes[..end]) {
            Ok(text) => return Some((text, cut)),
            // A character split by the cut: step back to its start.
            Err(error) if cut && error.error_len().is_none() => end = error.valid_up_to(),
            Err(_) => return None,
        }
    }
}

const MARK: &str = r##"<svg viewBox="0 0 100 100" aria-hidden="true"><defs><mask id="bite"><rect width="100" height="100" fill="#fff"/><circle cx="64" cy="14" r="7.5" fill="#000"/><circle cx="72.6" cy="23.9" r="8" fill="#000"/><circle cx="82.2" cy="32.4" r="7" fill="#000"/></mask></defs><circle cx="50" cy="50" r="36" fill="#ECB86E" mask="url(#bite)"/><g fill="#5E3A26"><circle cx="49.8" cy="54" r="4.8"/><circle cx="35.2" cy="28.8" r="3.9"/><circle cx="49.8" cy="34.6" r="3.5"/><circle cx="70.5" cy="46.4" r="4.7"/><circle cx="25.7" cy="47.7" r="4.8"/><circle cx="34.4" cy="70.5" r="4.7"/><circle cx="67.3" cy="69.7" r="4.7"/><circle cx="51.1" cy="71.7" r="3.4"/></g></svg>"##;

const OPEN_ICON: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M15 3h6v6"/><path d="M10 14 21 3"/><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/></svg>"#;

const DOWNLOAD_ICON: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 15V3"/><path d="m7 10 5 5 5-5"/><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/></svg>"#;

/// The page's styles: the Quiet Ink tokens, light and dark by the
/// browser's preference.
const STYLE: &str = r#"
@font-face { font-family: InterShare; src: url("FONT") format("woff2"); font-weight: 100 900; font-display: swap; }
:root {
  --bg2: #fafafa; --raised: #ffffff; --sunken: #f4f4f5;
  --fg: #09090b; --fg2: #3f3f46; --muted: #71717a; --faint: #a1a1aa;
  --line: rgba(0,0,0,.075); --line2: rgba(0,0,0,.13);
  --accent: #5b5bd6; --accent-fg: #ffffff;
  --shadow: 0 1px 2px rgba(0,0,0,.04), 0 4px 16px rgba(0,0,0,.05);
  color-scheme: light dark;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg2: #0a0a0c; --raised: #17171a; --sunken: #1b1b1f;
    --fg: #ededef; --fg2: #b4b4bc; --muted: #86868f; --faint: #5c5c66;
    --line: rgba(255,255,255,.07); --line2: rgba(255,255,255,.12);
    --accent: #7c7cff; --accent-fg: #0e0e10;
    --shadow: 0 1px 2px rgba(0,0,0,.3), 0 8px 24px rgba(0,0,0,.35);
  }
}
* { box-sizing: border-box; }
html, body { margin: 0; }
body { background: var(--bg2); color: var(--fg); font: 14.5px/1.5 InterShare, Inter, ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; -webkit-font-smoothing: antialiased; }
.page { max-width: 980px; margin: 0 auto; padding: 20px 20px 40px; }
.brand { display: flex; align-items: center; gap: 8px; color: var(--muted); font-size: 12.5px; }
.brand svg { width: 20px; height: 20px; }
.head { display: flex; flex-wrap: wrap; align-items: flex-end; justify-content: space-between; gap: 12px 24px; margin: 22px 0 16px; }
.head h1 { font-size: 22px; line-height: 1.25; font-weight: 600; letter-spacing: -.01em; margin: 0 0 4px; overflow-wrap: anywhere; }
.meta { color: var(--muted); font-size: 12.5px; display: flex; flex-wrap: wrap; gap: 4px 10px; }
.meta span + span::before { content: "·"; margin-right: 10px; color: var(--faint); }
.actions { display: flex; gap: 8px; }
.btn { display: inline-flex; align-items: center; gap: 6px; padding: 7px 12px; border-radius: 8px; font: 500 13.5px/1 inherit; font-family: inherit; text-decoration: none; border: 1px solid var(--line2); background: var(--raised); color: var(--fg); }
.btn:hover { background: var(--sunken); }
.btn.primary { background: var(--accent); color: var(--accent-fg); border-color: transparent; }
.btn.primary:hover { filter: brightness(1.08); }
.btn svg { width: 15px; height: 15px; }
.card { background: var(--raised); border: 1px solid var(--line); border-radius: 12px; box-shadow: var(--shadow); overflow: hidden; }
.bar { display: flex; justify-content: space-between; align-items: center; padding: 6px 10px 6px 14px; border-bottom: 1px solid var(--line); font-size: 12px; color: var(--muted); }
.bar a { color: var(--muted); text-decoration: none; }
.bar a:hover { color: var(--fg); }
.frame { display: block; width: 100%; height: min(72vh, 620px); border: 0; background: #fff; }
.img { display: grid; place-items: center; padding: 20px; background: conic-gradient(var(--sunken) 25%, transparent 0 50%, var(--sunken) 0 75%, transparent 0) 0 0 / 16px 16px; }
.img img { max-width: 100%; max-height: 72vh; height: auto; display: block; }
.media { display: block; width: 100%; max-height: 72vh; background: #000; }
.audio { padding: 28px 20px; }
.audio audio { width: 100%; }
.doc { padding: 28px clamp(20px, 5vw, 56px) 36px; max-width: 760px; margin: 0 auto; overflow-wrap: anywhere; }
.doc h1 { font-size: 22px; margin: 0 0 12px; }
.doc h2 { font-size: 17px; margin: 26px 0 8px; }
.doc h3 { font-size: 15px; margin: 22px 0 6px; }
.doc p, .doc li { color: var(--fg2); }
.doc a { color: var(--accent); }
.doc table { border-collapse: collapse; font-size: 13.5px; margin: 8px 0; display: block; overflow-x: auto; }
.doc th, .doc td { border-bottom: 1px solid var(--line); padding: 6px 14px 6px 0; text-align: left; }
.doc th { color: var(--muted); font-weight: 500; }
.doc code { background: var(--sunken); padding: 1px 5px; border-radius: 6px; font-size: 12.5px; }
.doc pre { background: var(--sunken); padding: 12px 14px; border-radius: 8px; overflow: auto; font-size: 12.5px; }
.doc pre code { background: none; padding: 0; }
.doc img { max-width: 100%; }
.doc blockquote { margin: 0; padding-left: 14px; border-left: 3px solid var(--line2); color: var(--muted); }
.text { margin: 0; padding: 16px 18px; overflow: auto; max-height: 72vh; font: 12.5px/1.55 ui-monospace, SFMono-Regular, Menlo, monospace; white-space: pre; tab-size: 4; }
.note { padding: 8px 14px; border-top: 1px solid var(--line); color: var(--muted); font-size: 12px; }
.file { display: flex; flex-direction: column; align-items: center; gap: 14px; padding: 56px 20px 60px; text-align: center; }
.file .icon { width: 64px; height: 76px; border-radius: 10px; background: var(--sunken); border: 1px solid var(--line2); display: grid; place-items: end center; padding-bottom: 10px; font: 600 11px/1 ui-monospace, monospace; color: var(--muted); }
.file .name { font-weight: 500; overflow-wrap: anywhere; }
.file .sub { color: var(--muted); font-size: 12.5px; margin-top: -8px; }
.foot { margin-top: 18px; display: flex; flex-wrap: wrap; justify-content: space-between; gap: 6px 16px; color: var(--faint); font-size: 12px; }
.foot .exp { color: var(--muted); }
.gone { max-width: 420px; margin: 18vh auto 0; padding: 0 20px; text-align: center; }
.gone h1 { font-size: 22px; font-weight: 600; margin: 18px 0 6px; }
.gone p { color: var(--muted); margin: 0; }
.gone svg { width: 44px; height: 44px; color: var(--muted); }
@media (max-width: 640px) {
  .page { padding: 14px 12px 28px; }
  .actions { width: 100%; }
  .actions .btn { flex: 1; justify-content: center; }
  .frame { height: 64vh; }
}
"#;

fn style() -> String {
    STYLE.replace("FONT", FONT_PATH)
}

/// The page every link that does not work answers with.
pub(super) const GONE_PAGE: &str = concat!(
    r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="robots" content="noindex, nofollow"><title>Link not available</title><style>"#,
    r#":root{color-scheme:light dark;--bg2:#fafafa;--fg:#09090b;--muted:#71717a}@media (prefers-color-scheme:dark){:root{--bg2:#0a0a0c;--fg:#ededef;--muted:#86868f}}"#,
    r#"body{margin:0;background:var(--bg2);color:var(--fg);font:14.5px/1.5 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif}.gone{max-width:420px;margin:18vh auto 0;padding:0 20px;text-align:center}.gone h1{font-size:22px;font-weight:600;margin:18px 0 6px}.gone p{color:var(--muted);margin:0}.gone svg{width:44px;height:44px;color:var(--muted)}"#,
    r#"</style></head><body><main class="gone"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 17H7A5 5 0 0 1 7 7"/><path d="M15 7h2a5 5 0 0 1 4 8"/><path d="M8 12h4"/><path d="m2 2 20 20"/></svg>"#,
    r#"<h1>This link isn't available</h1><p>It may have expired or been revoked. Ask whoever shared it for a new link.</p></main></body></html>"#
);

/// The landing page of `open` at `now`; `content` is the asset's bytes
/// when its kind [reads content](Kind::reads_content).
pub(super) fn page(
    open: &OpenShare,
    kind: Kind,
    content: Option<&[u8]>,
    now: jiff::Timestamp,
) -> String {
    let share = &open.share;
    let asset = &open.asset;
    let name = file_name(asset);
    let encoded = percent_encode(&name);
    let raw = format!("{}/raw/{encoded}", share.id);
    let download = format!("{}/download/{encoded}", share.id);
    let title = escape(&share.title);

    let mut meta = vec![
        kind.label(&asset.media_type),
        human_bytes(asset.byte_length),
    ];
    if name != share.title {
        meta.push(name.clone());
    }
    let meta = meta
        .iter()
        .map(|part| format!("<span>{}</span>", escape(part)))
        .collect::<String>();

    let open_button = if kind.opens() {
        format!(
            r#"<a class="btn" href="{raw}" target="_blank" rel="noopener noreferrer">{OPEN_ICON}Open full page</a>"#
        )
    } else {
        String::new()
    };
    let download_button =
        format!(r#"<a class="btn primary" href="{download}" download>{DOWNLOAD_ICON}Download</a>"#);

    let preview = match kind {
        Kind::Html => format!(
            r#"<div class="bar"><span>Interactive page · runs sandboxed</span><a href="{}" rel="noreferrer">Reload</a></div><iframe class="frame" src="{raw}" sandbox="allow-scripts allow-popups" referrerpolicy="no-referrer" title="{title}"></iframe>"#,
            share.id
        ),
        Kind::Image => format!(
            r#"<div class="img"><a href="{raw}" target="_blank" rel="noopener noreferrer"><img src="{raw}" alt="{title}"></a></div>"#
        ),
        Kind::Video => {
            format!(r#"<video class="media" src="{raw}" controls preload="metadata"></video>"#)
        }
        Kind::Audio => format!(
            r#"<div class="audio"><audio src="{raw}" controls preload="metadata"></audio></div>"#
        ),
        Kind::Pdf => format!(r#"<iframe class="frame" src="{raw}" title="{title}"></iframe>"#),
        Kind::Markdown => match content.and_then(text_preview) {
            Some((text, cut)) => format!(
                r#"<article class="doc">{}</article>{}"#,
                markdown(text),
                cut_note(cut)
            ),
            None => file_card(asset, &name, &download),
        },
        Kind::Text => match content.and_then(text_preview) {
            Some((text, cut)) => format!(
                r#"<pre class="text">{}</pre>{}"#,
                escape(text),
                cut_note(cut)
            ),
            None => file_card(asset, &name, &download),
        },
        Kind::File => file_card(asset, &name, &download),
    };

    let expiry = share.expires_at.map_or_else(String::new, |at| {
        let date = at
            .to_zoned(jiff::tz::TimeZone::UTC)
            .strftime("%b %-d, %Y")
            .to_string();
        let exact = at
            .to_zoned(jiff::tz::TimeZone::UTC)
            .strftime("%b %-d, %Y, %H:%M UTC")
            .to_string();
        format!(
            r#"<span class="exp" title="Expires {exact}">Link expires {} · {date}</span>"#,
            time_left(now, at)
        )
    });

    let og_image = match (kind, &share.url) {
        (Kind::Image, Some(url)) => format!(
            r#"<meta property="og:image" content="{}">"#,
            escape(&format!("{url}/raw/{encoded}"))
        ),
        _ => String::new(),
    };

    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="robots" content="noindex, nofollow"><meta name="referrer" content="no-referrer"><title>{title}</title><meta property="og:title" content="{title}"><meta property="og:type" content="website">{og_image}<style>{style}</style></head><body><main class="page"><div class="brand">{MARK}cookie agent</div><header class="head"><div><h1>{title}</h1><div class="meta">{meta}</div></div><div class="actions">{open_button}{download_button}</div></header><section class="card">{preview}</section><footer class="foot"><span>Shared from cookie agent</span>{expiry}</footer></main></body></html>"#,
        style = style(),
    )
}

fn cut_note(cut: bool) -> String {
    if cut {
        format!(
            r#"<div class="note">Showing the first {}. Download it for the rest.</div>"#,
            human_bytes(TEXT_PREVIEW_BYTES as u64)
        )
    } else {
        String::new()
    }
}

fn file_card(asset: &Asset, name: &str, download: &str) -> String {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| !extension.is_empty() && extension.len() <= 5)
        .unwrap_or("FILE")
        .to_ascii_uppercase();
    format!(
        r#"<div class="file"><div class="icon">{}</div><div class="name">{}</div><div class="sub">{} · {} · no preview for this type</div><a class="btn primary" href="{download}" download>{DOWNLOAD_ICON}Download</a></div>"#,
        escape(&extension),
        escape(name),
        escape(&asset.media_type),
        human_bytes(asset.byte_length),
    )
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        AgentId, AssetHandle, AssetId, AssetStamp, AssetStore, SessionId, Share, ShareCreated,
        ShareId,
    };

    use super::*;

    fn open(media_type: &str, filename: Option<&str>, title: &str) -> OpenShare {
        let session = SessionId::new_v7();
        let stamp = AssetStamp {
            at: "2026-11-03T00:00:00Z".parse().unwrap(),
            session,
            agent: Some(AgentId::new("primary").unwrap()),
        };
        let asset = Asset {
            id: AssetId::new_v7(),
            handle: AssetHandle::random(),
            store: AssetStore::Session { session },
            title: title.to_owned(),
            media_type: media_type.to_owned(),
            byte_length: 2048,
            sha256: "0".repeat(64),
            filename: filename.map(str::to_owned),
            created: stamp.clone(),
            updated: stamp,
        };
        let id = ShareId::new("q3Vt0x9LmA2bCdEfGh1iJk").unwrap();
        OpenShare {
            share: Share {
                path: format!("s/{id}"),
                url: Some(format!("https://share.example.com/s/{id}")),
                id,
                store: asset.store.clone(),
                asset: asset.id,
                title: title.to_owned(),
                media_type: media_type.to_owned(),
                byte_length: 2048,
                created: ShareCreated {
                    at: "2026-11-03T00:00:00Z".parse().unwrap(),
                    session: None,
                    agent: None,
                },
                expires_at: Some("2026-11-10T00:00:00Z".parse().unwrap()),
                views: 0,
                last_viewed: None,
            },
            asset,
        }
    }

    fn now() -> jiff::Timestamp {
        "2026-11-03T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn kinds_follow_the_media_type() {
        for (media_type, kind) in [
            ("text/html", Kind::Html),
            ("text/markdown", Kind::Markdown),
            ("text/csv", Kind::Text),
            ("application/json", Kind::Text),
            ("image/svg+xml", Kind::Image),
            ("video/mp4", Kind::Video),
            ("audio/mpeg", Kind::Audio),
            ("application/pdf", Kind::Pdf),
            ("application/x-npz", Kind::File),
        ] {
            assert_eq!(Kind::of(media_type), kind, "{media_type}");
        }
        assert!(!Kind::File.inline());
        assert!(Kind::Markdown.reads_content() && Kind::Text.reads_content());
        assert!(!Kind::Html.reads_content());
    }

    #[test]
    fn the_title_and_names_are_escaped() {
        let page = page(
            &open("text/html", Some("a<b>.html"), "<script>alert(1)</script>"),
            Kind::Html,
            None,
            now(),
        );
        assert!(!page.contains("<script>"));
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(page.contains("raw/a%3Cb%3E.html"));
    }

    #[test]
    fn an_html_page_runs_in_a_sandboxed_frame_of_raw() {
        let page = page(
            &open("text/html", Some("loss.html"), "Loss"),
            Kind::Html,
            None,
            now(),
        );
        assert!(page.contains(
            r#"src="q3Vt0x9LmA2bCdEfGh1iJk/raw/loss.html" sandbox="allow-scripts allow-popups""#
        ));
        assert!(!page.contains("allow-same-origin"));
        assert!(page.contains("Open full page"));
    }

    #[test]
    fn markdown_is_rendered_with_its_raw_html_escaped() {
        let source = b"# Results\n\n<script>alert(1)</script>\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let page = page(
            &open("text/markdown", Some("eval.md"), "Eval"),
            Kind::Markdown,
            Some(source),
            now(),
        );
        assert!(page.contains("<h1>Results</h1>"));
        assert!(page.contains("<table>"));
        assert!(!page.contains("<script>"));
    }

    #[test]
    fn long_text_is_cut_at_a_character_and_says_so() {
        let mut text = "a".repeat(TEXT_PREVIEW_BYTES - 1).into_bytes();
        text.extend("é and more".as_bytes());
        let (shown, cut) = text_preview(&text).unwrap();
        assert!(cut);
        assert_eq!(shown.len(), TEXT_PREVIEW_BYTES - 1);
        assert!(text_preview(b"short").is_some_and(|(_, cut)| !cut));
        assert!(text_preview(&[0xff, 0xfe]).is_none());
    }

    #[test]
    fn files_are_named_by_their_file_or_their_title() {
        let named = open("image/png", Some("loss.png"), "Loss curves");
        assert_eq!(file_name(&named.asset), "loss.png");
        let titled = open("image/png", None, "Loss curves: v8 / baseline");
        assert_eq!(file_name(&titled.asset), "loss-curves-v8-baseline.png");
        let empty = open("application/x-npz", None, "!!!");
        assert_eq!(file_name(&empty.asset), "asset.bin");
    }

    #[test]
    fn time_left_counts_days_then_hours() {
        let at = |time: &str| time.parse::<jiff::Timestamp>().unwrap();
        let now = at("2026-11-03T00:00:00Z");
        assert_eq!(time_left(now, at("2026-11-10T00:00:00Z")), "in 7 days");
        assert_eq!(time_left(now, at("2026-11-09T23:59:00Z")), "in 7 days");
        assert_eq!(time_left(now, at("2026-11-04T05:00:00Z")), "in 1 day");
        assert_eq!(time_left(now, at("2026-11-04T12:00:00Z")), "in 2 days");
        assert_eq!(time_left(now, at("2026-11-03T12:00:00Z")), "in 12 hours");
        assert_eq!(time_left(now, at("2026-11-03T05:20:00Z")), "in 5 hours");
        assert_eq!(time_left(now, at("2026-11-03T01:00:00Z")), "in 1 hour");
        assert_eq!(
            time_left(now, at("2026-11-03T00:20:00Z")),
            "in a few minutes"
        );
    }

    #[test]
    fn an_image_link_previews_in_chat_apps() {
        let page = page(
            &open("image/png", Some("loss.png"), "Loss"),
            Kind::Image,
            None,
            now(),
        );
        assert!(page.contains(
            r#"<meta property="og:image" content="https://share.example.com/s/q3Vt0x9LmA2bCdEfGh1iJk/raw/loss.png">"#
        ));
    }
}
