//! Plugin pages on the daemon's listener: `/plugins/<name>/…` forwarded to
//! the plugin's own HTTP server on loopback, behind the page ticket
//! (`docs/site/specs/plugin-web-ui.md` sections 3 and 4).

use std::net::SocketAddr;

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use cookie_agent_engine::{PluginPage, PluginPageTarget, percent_decode};
use subtle::ConstantTimeEq as _;
use tokio::net::TcpStream;

use super::websocket::{PeerAddress, WebSocketState, authorized};

/// The cookie the web page keeps the page ticket in.
pub(crate) const TICKET_COOKIE: &str = "cookie_page_ticket";

/// What the page ticket is derived for; a new derivation gets a new label.
const TICKET_LABEL: &[u8] = b"cookie-page-ticket/1";

/// The page ticket of daemon token `token`: `HMAC-SHA256(token, label)`,
/// hex-encoded. Stateless, so rotating the token revokes every ticket.
pub(crate) fn page_ticket(token: &str) -> String {
    use std::fmt::Write as _;
    let key = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, token.as_bytes());
    let tag = aws_lc_rs::hmac::sign(&key, TICKET_LABEL);
    tag.as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

const PREFIX: &str = "/plugins/";

/// The page a request without access gets, linking to the web page at
/// the daemon's root, relative to `path` so a sub-path keeps it.
fn refused_page(path: &str) -> String {
    let root = "../".repeat(path.matches('/').count().saturating_sub(1));
    let root = if root.is_empty() {
        "./".to_owned()
    } else {
        root
    };
    format!(
        "<!doctype html><meta charset=utf-8><title>Not available</title>\
         <p>Open this from the cookie web client. <a href=\"{root}\">Open the web client</a></p>"
    )
}

/// Serves `/plugins/<name>/…`, in the order of the spec's section 4: a
/// connected plugin's public path is forwarded; anything else needs the
/// ticket cookie or the daemon token; then an unknown plugin is 404, one
/// not connected 503, and the rest is forwarded.
pub(crate) async fn plugin_pages(
    State(state): State<WebSocketState>,
    request: Request,
) -> Response {
    let path = request.uri().path().to_owned();
    let Some(below) = path.strip_prefix(PREFIX) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (raw_name, rest) = match below.find('/') {
        Some(slash) => (&below[..slash], &below[slash..]),
        None => (below, ""),
    };
    let name = percent_decode(raw_name).filter(|name| !name.is_empty());
    let page = match (&name, state.server.get()) {
        (None, _) => PluginPage::Unknown,
        (Some(_), None) => PluginPage::Unavailable(cookie_agent_protocol::PluginState::Starting),
        (Some(name), Some(server)) => server.engine.plugin_page(name),
    };
    let public = match &page {
        PluginPage::Serving(target) => target.is_public(if rest.is_empty() { "/" } else { rest }),
        _ => false,
    };
    if !public && !has_access(request.headers(), state.token.as_str()) {
        return html(StatusCode::UNAUTHORIZED, &refused_page(&path));
    }
    let (name, target) = match page {
        PluginPage::Serving(target) => (name.unwrap_or_default(), target),
        PluginPage::Unknown => return StatusCode::NOT_FOUND.into_response(),
        PluginPage::Unavailable(plugin_state) => {
            let state = serde_json::to_value(plugin_state)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unavailable".to_owned());
            return html(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!(
                    "<!doctype html><meta charset=utf-8><title>Not running</title>\
                     <p>This plugin is not running now (state: {state}).</p>"
                ),
            );
        }
    };
    if rest.is_empty() {
        // `/plugins/<name>` → `/plugins/<name>/`, relative so a page served
        // under a sub-path keeps it.
        let query = request
            .uri()
            .query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default();
        let location = format!("{raw_name}/{query}");
        return match HeaderValue::from_str(&location) {
            Ok(location) => (
                StatusCode::PERMANENT_REDIRECT,
                [(header::LOCATION, location)],
            )
                .into_response(),
            Err(_) => StatusCode::NOT_FOUND.into_response(),
        };
    }
    let rest = rest.to_owned();
    forward(&state, &name, &target, &rest, request).await
}

/// Whether `headers` carry the page ticket cookie or the daemon token.
fn has_access(headers: &HeaderMap, token: &str) -> bool {
    if authorized(headers, token) {
        return true;
    }
    let expected = page_ticket(token);
    cookies(headers).any(|(name, value)| {
        name == TICKET_COOKIE
            && value.len() == expected.len()
            && bool::from(value.as_bytes().ct_eq(expected.as_bytes()))
    })
}

/// Every `name=value` pair of the request's `Cookie` headers.
fn cookies(headers: &HeaderMap) -> impl Iterator<Item = (&str, &str)> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            Some((name.trim(), value.trim()))
        })
}

fn html(status: StatusCode, body: &str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body.to_owned(),
    )
        .into_response()
}

/// Headers that describe one connection, not the message
/// (RFC 9110 section 7.6.1), never forwarded either way.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Removes the hop-by-hop headers of `headers`, those its `Connection`
/// names included, and `upgrade` unless `upgrading`.
fn remove_hop_by_hop(headers: &mut HeaderMap, upgrading: bool) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in named {
        if !(upgrading && name == header::UPGRADE) {
            headers.remove(name);
        }
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
    if !upgrading {
        headers.remove(header::UPGRADE);
    }
}

/// Where the page is reached from outside: scheme, host, and the path the
/// daemon's root is at, for the `X-Forwarded-*` headers.
struct Outside {
    proto: String,
    host: Option<String>,
    root: String,
}

impl Outside {
    /// From `[server] page_url` when it is set, else from a front's own
    /// `X-Forwarded-*` headers, else from the request.
    fn of(page_url: Option<&str>, headers: &HeaderMap, tls: bool) -> Self {
        if let Some((scheme, rest)) = page_url.and_then(|url| url.split_once("://")) {
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            return Self {
                proto: scheme.to_ascii_lowercase(),
                host: Some(host.to_owned()),
                root: path.trim_end_matches('/').to_owned(),
            };
        }
        let value = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(',').next())
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        Self {
            proto: value("x-forwarded-proto")
                .unwrap_or_else(|| if tls { "https" } else { "http" }.to_owned()),
            host: value("x-forwarded-host").or_else(|| {
                headers
                    .get(header::HOST)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            }),
            root: value("x-forwarded-prefix")
                .map(|prefix| prefix.trim_matches('/').to_owned())
                .unwrap_or_default(),
        }
    }

    /// `X-Forwarded-Prefix` of plugin `name`'s pages.
    fn prefix(&self, name: &str) -> String {
        let page = cookie_agent_protocol::plugin_page_path(name);
        let page = page.trim_end_matches('/');
        if self.root.is_empty() {
            format!("/{page}")
        } else {
            format!("/{}/{page}", self.root.trim_start_matches('/'))
        }
    }
}

/// The request as the plugin receives it: its path below the plugin's
/// root, `Host` the plugin's, the daemon's token and ticket removed, and
/// the `X-Forwarded-*` headers set.
fn upstream_headers(
    state: &WebSocketState,
    name: &str,
    target: &PluginPageTarget,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    upgrading: bool,
) -> HeaderMap {
    let page_url = match state.server.get() {
        Some(server) => server.engine.page_url(),
        None => state
            .shared
            .page_url
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
    };
    let outside = Outside::of(page_url.as_deref(), headers, state.tls);
    let mut upstream = headers.clone();
    remove_hop_by_hop(&mut upstream, upgrading);
    if authorized(headers, state.token.as_str()) {
        upstream.remove(header::AUTHORIZATION);
    }
    let kept: Vec<String> = cookies(headers)
        .filter(|(cookie, _)| *cookie != TICKET_COOKIE)
        .map(|(cookie, value)| format!("{cookie}={value}"))
        .collect();
    upstream.remove(header::COOKIE);
    if !kept.is_empty()
        && let Ok(value) = HeaderValue::from_str(&kept.join("; "))
    {
        upstream.insert(header::COOKIE, value);
    }
    let forwarded_for = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    for name in [
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-forwarded-prefix",
        "forwarded",
    ] {
        upstream.remove(name);
    }
    let mut set = |name: &'static str, value: &str| {
        if let Ok(value) = HeaderValue::from_str(value) {
            upstream.insert(HeaderName::from_static(name), value);
        }
    };
    set("x-forwarded-prefix", &outside.prefix(name));
    set("x-forwarded-proto", &outside.proto);
    if let Some(host) = &outside.host {
        set("x-forwarded-host", host);
    }
    if let Some(peer) = peer {
        let chain = match forwarded_for {
            Some(chain) => format!("{chain}, {}", peer.ip()),
            None => peer.ip().to_string(),
        };
        set("x-forwarded-for", &chain);
    }
    if let Ok(host) = HeaderValue::from_str(&target.authority) {
        upstream.insert(header::HOST, host);
    }
    upstream
}

/// Whether `headers` ask to upgrade the connection, as a WebSocket does.
fn upgrading(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE)
        && headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
}

/// Sends `request` to the plugin's server as `rest` and streams its answer
/// back; a WebSocket upgrade is joined end to end.
async fn forward(
    state: &WebSocketState,
    name: &str,
    target: &PluginPageTarget,
    rest: &str,
    mut request: Request,
) -> Response {
    let upgrade = upgrading(request.headers());
    let peer = request
        .extensions()
        .get::<ConnectInfo<PeerAddress>>()
        .map(|ConnectInfo(PeerAddress(peer))| *peer);
    let headers = upstream_headers(state, name, target, request.headers(), peer, upgrade);
    let path_and_query = match request.uri().query() {
        Some(query) => format!("{rest}?{query}"),
        None => rest.to_owned(),
    };
    let Ok(uri) = path_and_query.parse::<Uri>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let incoming_upgrade = upgrade.then(|| hyper::upgrade::on(&mut request));
    let (parts, body) = request.into_parts();
    let mut upstream = hyper::Request::new(body);
    *upstream.method_mut() = parts.method;
    *upstream.uri_mut() = uri;
    *upstream.headers_mut() = headers;
    let Ok(stream) = TcpStream::connect(&target.authority).await else {
        return bad_gateway();
    };
    let _ = stream.set_nodelay(true);
    let Ok((mut sender, connection)) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await
    else {
        return bad_gateway();
    };
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let Ok(mut response) = sender.send_request(upstream).await else {
        return bad_gateway();
    };
    let switching = response.status() == StatusCode::SWITCHING_PROTOCOLS;
    if switching && let Some(incoming_upgrade) = incoming_upgrade {
        let outgoing_upgrade = hyper::upgrade::on(&mut response);
        tokio::spawn(async move {
            let (Ok(incoming), Ok(outgoing)) = (incoming_upgrade.await, outgoing_upgrade.await)
            else {
                return;
            };
            let mut incoming = hyper_util::rt::TokioIo::new(incoming);
            let mut outgoing = hyper_util::rt::TokioIo::new(outgoing);
            let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
        });
    }
    let (mut parts, body) = response.into_parts();
    remove_hop_by_hop(&mut parts.headers, switching);
    if switching {
        parts
            .headers
            .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    }
    Response::from_parts(parts, Body::new(body))
}

fn bad_gateway() -> Response {
    html(
        StatusCode::BAD_GATEWAY,
        "<!doctype html><meta charset=utf-8><title>Not reachable</title>\
         <p>The plugin's server did not answer.</p>",
    )
}

#[cfg(test)]
mod daemon_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refused_page_links_to_the_root() {
        assert!(refused_page("/plugins/gcal/").contains("href=\"../../\""));
        assert!(refused_page("/plugins/gcal/a/b").contains("href=\"../../../\""));
        assert!(refused_page("/plugins/gcal").contains("href=\"../\""));
    }

    #[test]
    fn the_ticket_is_a_keyed_hash_of_the_token() {
        let ticket = page_ticket("token-a");
        assert_eq!(ticket.len(), 64);
        assert!(ticket.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(ticket, page_ticket("token-a"));
        assert_ne!(ticket, page_ticket("token-b"));
        assert_ne!(ticket, "token-a");
    }

    #[test]
    fn access_takes_the_ticket_cookie_or_the_token() {
        let token = "secret-token";
        let mut headers = HeaderMap::new();
        assert!(!has_access(&headers, token));
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!(
                "theme=dark; {TICKET_COOKIE}={}",
                page_ticket(token)
            ))
            .unwrap(),
        );
        assert!(has_access(&headers, token));
        assert!(!has_access(&headers, "another-token"));
        let mut wrong = HeaderMap::new();
        wrong.insert(
            header::COOKIE,
            HeaderValue::from_static("cookie_page_ticket=00"),
        );
        assert!(!has_access(&wrong, token));
        // The token itself as the cookie is not the ticket.
        wrong.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{TICKET_COOKIE}={token}")).unwrap(),
        );
        assert!(!has_access(&wrong, token));
        let mut bearer = HeaderMap::new();
        bearer.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        assert!(has_access(&bearer, token));
    }

    #[test]
    fn the_outside_address_prefers_page_url_then_the_front_then_the_request() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:7777"));
        let plain = Outside::of(None, &headers, false);
        assert_eq!(plain.proto, "http");
        assert_eq!(plain.host.as_deref(), Some("127.0.0.1:7777"));
        assert_eq!(plain.prefix("gcal"), "/plugins/gcal");
        assert_eq!(Outside::of(None, &headers, true).proto, "https");

        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        headers.insert("x-forwarded-host", HeaderValue::from_static("box.ts.net"));
        headers.insert("x-forwarded-prefix", HeaderValue::from_static("/cookie/"));
        let front = Outside::of(None, &headers, false);
        assert_eq!(front.proto, "https");
        assert_eq!(front.host.as_deref(), Some("box.ts.net"));
        assert_eq!(front.prefix("my plugin"), "/cookie/plugins/my%20plugin");

        let configured = Outside::of(Some("https://page.example/sub/"), &headers, false);
        assert_eq!(configured.proto, "https");
        assert_eq!(configured.host.as_deref(), Some("page.example"));
        assert_eq!(configured.prefix("gcal"), "/sub/plugins/gcal");
    }

    #[test]
    fn hop_by_hop_headers_are_removed_and_upgrade_kept_only_when_upgrading() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-private"),
        );
        headers.insert("x-private", HeaderValue::from_static("1"));
        headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
        let mut plain = headers.clone();
        remove_hop_by_hop(&mut plain, false);
        assert_eq!(plain.len(), 1);
        assert!(plain.contains_key(header::ACCEPT));
        remove_hop_by_hop(&mut headers, true);
        assert!(headers.contains_key(header::UPGRADE));
        assert!(!headers.contains_key("x-private"));
    }
}
