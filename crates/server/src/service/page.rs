//! The web client's page on the daemon's listener (spec 7.1): `GET /` is
//! `index.html` and every other file of the bundle is served at its path.
//! The page holds no secret, so it needs no token; the connections it opens
//! authenticate in their handshake and follow `[server] allowed_origins`
//! like any other browser connection.

use axum::{
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};

/// `Cache-Control` of a file whose name carries a hash of its content.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// `Cache-Control` of `index.html` and other files whose names do not
/// change with their content, so a restarted daemon's page always matches
/// its own build.
const REVALIDATE: &str = "no-cache";

/// Answers a request no other route took: a file of the page, or 404.
pub(super) async fn page(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = uri.path().strip_prefix('/').unwrap_or(uri.path());
    let path = if path.is_empty() { "index.html" } else { path };
    let Some(asset) = cookie_agent_web_assets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cache = if asset.immutable {
        IMMUTABLE
    } else {
        REVALIDATE
    };
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response_headers.insert(header::ETAG, HeaderValue::from_static(asset.etag));
    response_headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    let revalidated = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|tags| {
            tags.split(',')
                .any(|tag| tag.trim() == asset.etag || tag.trim() == "*")
        });
    if revalidated {
        return (StatusCode::NOT_MODIFIED, response_headers).into_response();
    }
    (StatusCode::OK, response_headers, asset.bytes).into_response()
}

/// The page's URL for the daemon whose client endpoint is `client_url`:
/// `ws`→`http`, `wss`→`https`, and the endpoint's final `ws` dropped, so
/// `ws://127.0.0.1:7419/ws` gives `http://127.0.0.1:7419/`. `None` for a
/// URL that is no `ws`/`wss` endpoint ending in `/ws`.
#[must_use]
pub fn page_url(client_url: &str) -> Option<String> {
    let (scheme, rest) = client_url.split_once("://")?;
    let scheme = match scheme {
        "ws" => "http",
        "wss" => "https",
        _ => return None,
    };
    let rest = rest.strip_suffix("ws").filter(|rest| rest.ends_with('/'))?;
    if rest.contains(['?', '#']) || !rest.contains('/') {
        return None;
    }
    Some(format!("{scheme}://{rest}"))
}

#[cfg(test)]
mod tests {
    use super::page_url;

    #[test]
    fn the_page_is_the_client_endpoint_over_http_without_its_ws() {
        assert_eq!(
            page_url("ws://127.0.0.1:7419/ws").as_deref(),
            Some("http://127.0.0.1:7419/")
        );
        assert_eq!(
            page_url("wss://cookie.example/ws").as_deref(),
            Some("https://cookie.example/")
        );
        assert_eq!(
            page_url("wss://abc.efg/cookie/ws").as_deref(),
            Some("https://abc.efg/cookie/")
        );
        for url in [
            "http://127.0.0.1:7419/ws",
            "ws://127.0.0.1:7419/runtime",
            "ws://127.0.0.1:7419/news",
            "ws://127.0.0.1:7419",
        ] {
            assert_eq!(page_url(url), None, "{url}");
        }
    }
}
