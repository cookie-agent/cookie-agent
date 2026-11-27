//! Web forwards on the daemon's listener (`docs/site/specs/forwards.md`
//! section 6): `/p/<handle>/<path>` serves a machine's web app through the
//! relay, its WebSockets included, on the page's own origin and
//! unsandboxed, as plugin pages are (decision 1). The user's route takes
//! the page ticket cookie or the token; a conversation share's guest
//! reaches its session's forwards below its share.
//!
//! Only headers are rewritten: the path loses its prefix, `Host` is the
//! target's, the daemon's ticket is taken out, and the `X-Forwarded-*`
//! headers say where the app is; a `Location` from the root, and a cookie
//! for the whole site, get the prefix.

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use cookie_agent_engine::Engine;

use super::{
    plugin_pages::{
        Outside, TICKET_COOKIE, cookies, has_access, html, remove_hop_by_hop, upgrading,
    },
    websocket::{PeerAddress, WebSocketState},
};

pub(crate) const PREFIX: &str = "/p/";

/// The one answer to every forward that does not work.
fn gone() -> Response {
    html(
        StatusCode::NOT_FOUND,
        "<!doctype html><meta charset=utf-8><title>Not available</title>\
         <p>This forward isn't available.</p>",
    )
}

fn refused(path: &str) -> Response {
    let _ = path;
    html(
        StatusCode::UNAUTHORIZED,
        "<!doctype html><meta charset=utf-8><title>Open cookie first</title>\
         <p>This forward opens from the cookie web page. Open the page, then the forward again.</p>",
    )
}

/// Serves a path below `/p/` for the user.
pub(crate) async fn forward_pages(
    State(state): State<WebSocketState>,
    request: Request,
) -> Response {
    let path = request.uri().path().to_owned();
    let Some(below) = path.strip_prefix(PREFIX) else {
        return gone();
    };
    if !has_access(request.headers(), state.token.as_str()) {
        return refused(&path);
    }
    let Some(server) = state.server.get() else {
        return gone();
    };
    let page_url = server.engine.page_url();
    let outside = Outside::of(page_url.as_deref(), request.headers(), state.tls);
    let root = if outside.root.is_empty() {
        String::new()
    } else {
        format!("/{}", outside.root.trim_start_matches('/'))
    };
    serve(
        &server.engine,
        below,
        &format!("{root}/p"),
        &outside,
        None,
        request,
    )
    .await
}

/// Serves `below` (`<handle>/<path>`) of a web forward: `base` is where
/// handles are from the outside (`/p`, or a guest's `/s/<id>/p`), and
/// `owner`, for a guest, the session tree the forward must belong to.
pub(crate) async fn serve(
    engine: &Engine,
    below: &str,
    base: &str,
    outside: &Outside,
    owner: Option<cookie_agent_protocol::SessionId>,
    request: Request,
) -> Response {
    let (handle, rest) = match below.find('/') {
        Some(slash) => (&below[..slash], &below[slash..]),
        None => (below, ""),
    };
    let Some(forward) = engine.web_forward(handle) else {
        return gone();
    };
    if owner.is_some_and(|owner| forward.owner != Some(owner)) {
        return gone();
    }
    if rest.is_empty() {
        // `/p/<handle>` → `/p/<handle>/`, relative so a sub-path keeps it.
        let query = request
            .uri()
            .query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default();
        return match HeaderValue::from_str(&format!("{handle}/{query}")) {
            Ok(location) => (
                StatusCode::PERMANENT_REDIRECT,
                [(header::LOCATION, location)],
            )
                .into_response(),
            Err(_) => gone(),
        };
    }
    serve_at(
        engine,
        &forward,
        rest,
        &format!("{base}/{handle}"),
        outside,
        request,
    )
    .await
}

/// Serves `rest` of web forward `forward`, whose app is at `prefix` from
/// the outside, such as a share link's `/s/<id>`.
pub(crate) async fn serve_at(
    engine: &Engine,
    forward: &cookie_agent_protocol::Forward,
    rest: &str,
    prefix: &str,
    outside: &Outside,
    request: Request,
) -> Response {
    let target = format!("{}:{}", forward.to.host, forward.to.port);
    proxy(
        engine,
        &forward.handle,
        prefix,
        &target,
        outside,
        rest,
        request,
    )
    .await
}

/// The request as the app receives it.
fn upstream_headers(
    headers: &HeaderMap,
    outside: &Outside,
    prefix: &str,
    target: &str,
    peer: Option<std::net::SocketAddr>,
    upgrade: bool,
) -> HeaderMap {
    let mut upstream = headers.clone();
    remove_hop_by_hop(&mut upstream, upgrade);
    // An upgrade names itself in `Connection`, which the hop-by-hop
    // headers include.
    if upgrade {
        upstream.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    }
    upstream.remove(header::AUTHORIZATION);
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
    set("x-forwarded-prefix", prefix);
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
    if let Ok(host) = HeaderValue::from_str(target) {
        upstream.insert(header::HOST, host);
    }
    upstream
}

/// The answer as the browser receives it: a redirect from the root, and a
/// cookie for the whole site, get `prefix`.
fn rewrite_response(headers: &mut HeaderMap, prefix: &str) {
    if let Some(location) = headers
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .filter(|location| location.starts_with('/') && !location.starts_with("//"))
        .map(|location| format!("{prefix}{location}"))
        && let Ok(value) = HeaderValue::from_str(&location)
    {
        headers.insert(header::LOCATION, value);
    }
    let cookies: Vec<HeaderValue> = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|cookie| cookie_with_prefix(cookie, prefix))
        .filter_map(|cookie| HeaderValue::from_str(&cookie).ok())
        .collect();
    if !cookies.is_empty() {
        headers.remove(header::SET_COOKIE);
        for cookie in cookies {
            headers.append(header::SET_COOKIE, cookie);
        }
    }
}

/// `cookie` with its `Path` below `prefix`: none, or `/`, become
/// `<prefix>/`, and one from the root gets `prefix` before it.
fn cookie_with_prefix(cookie: &str, prefix: &str) -> String {
    let mut parts: Vec<String> = cookie
        .split(';')
        .map(|part| part.trim().to_owned())
        .collect();
    let mut found = false;
    for part in parts.iter_mut().skip(1) {
        if let Some((name, value)) = part.split_once('=')
            && name.trim().eq_ignore_ascii_case("path")
        {
            found = true;
            let value = value.trim();
            *part = if value == "/" || value.is_empty() {
                format!("Path={prefix}/")
            } else if value.starts_with('/') {
                format!("Path={prefix}{value}")
            } else {
                part.clone()
            };
        }
    }
    if !found {
        parts.push(format!("Path={prefix}/"));
    }
    parts.join("; ")
}

/// Sends `request` to the forward's target as `rest` through a relayed
/// connection and streams its answer back; a WebSocket upgrade is joined
/// end to end.
async fn proxy(
    engine: &Engine,
    handle: &str,
    prefix: &str,
    target: &str,
    outside: &Outside,
    rest: &str,
    mut request: Request,
) -> Response {
    let upgrade = upgrading(request.headers());
    let peer = request
        .extensions()
        .get::<ConnectInfo<PeerAddress>>()
        .map(|ConnectInfo(PeerAddress(peer))| *peer);
    let headers = upstream_headers(request.headers(), outside, prefix, target, peer, upgrade);
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
    let stream = match engine
        .open_web_forward(
            handle,
            peer.map_or_else(|| "a browser".to_owned(), |peer| peer.to_string()),
        )
        .await
    {
        Ok(stream) => stream,
        Err(reason) => return unreachable_app(&reason),
    };
    let Ok((mut sender, connection)) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await
    else {
        return unreachable_app("the app did not answer");
    };
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let mut response = match sender.send_request(upstream).await {
        Ok(response) => response,
        Err(error) => return unreachable_app(&error.to_string()),
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
    rewrite_response(&mut parts.headers, prefix);
    if switching {
        parts
            .headers
            .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    }
    Response::from_parts(parts, Body::new(body))
}

fn unreachable_app(reason: &str) -> Response {
    let reason = reason
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    html(
        StatusCode::BAD_GATEWAY,
        &format!(
            "<!doctype html><meta charset=utf-8><title>Not reachable</title>\
             <p>The forwarded app did not answer: {reason}.</p>"
        ),
    )
}
