//! The web client's page on the daemon's listener (spec 7.1): `GET /` is
//! `index.html` and every other file of the bundle is served at its path,
//! gzip-compressed when the browser accepts it, under the page's
//! Content-Security-Policy.
//! The page holds no secret, so it needs no token; the connections it opens
//! authenticate in their handshake like any other client connection.

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
    let compressed = asset.gzip.filter(|_| accepts_gzip(&headers));
    // The compressed copy is another representation, with its own tag.
    let etag = match compressed {
        Some(_) => format!("{}-gzip\"", asset.etag.trim_end_matches('"')),
        None => asset.etag.to_owned(),
    };
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(etag) = HeaderValue::from_str(&etag) {
        response_headers.insert(header::ETAG, etag);
    }
    response_headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response_headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(cookie_agent_web_assets::content_security_policy()),
    );
    if asset.gzip.is_some() {
        response_headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    }
    let revalidated = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|tags| {
            tags.split(',')
                .any(|tag| tag.trim() == etag || tag.trim() == "*")
        });
    if revalidated {
        return (StatusCode::NOT_MODIFIED, response_headers).into_response();
    }
    match compressed {
        Some(bytes) => {
            response_headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            (StatusCode::OK, response_headers, bytes).into_response()
        }
        None => (StatusCode::OK, response_headers, asset.bytes).into_response(),
    }
}

/// Whether `Accept-Encoding` takes gzip: listed (or `*`) without `q=0`.
fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|coding| {
            let mut parts = coding.split(';');
            let name = parts.next().unwrap_or_default().trim();
            let refused = parts.any(|parameter| {
                parameter
                    .trim()
                    .strip_prefix("q=")
                    .is_some_and(|quality| quality.trim().parse::<f32>().is_ok_and(|q| q == 0.0))
            });
            (name.eq_ignore_ascii_case("gzip") || name == "*") && !refused
        })
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
    use axum::http::{HeaderMap, HeaderValue, header};

    use super::{accepts_gzip, page_url};

    #[test]
    fn gzip_is_sent_only_where_accepted() {
        let accepts = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::ACCEPT_ENCODING,
                HeaderValue::from_str(value).unwrap(),
            );
            accepts_gzip(&headers)
        };
        assert!(accepts("gzip, deflate, br"));
        assert!(accepts("br;q=1.0, GZIP;q=0.5"));
        assert!(accepts("*"));
        assert!(!accepts("br"));
        assert!(!accepts("gzip;q=0"));
        assert!(!accepts("identity"));
        assert!(!accepts_gzip(&HeaderMap::new()));
    }

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
