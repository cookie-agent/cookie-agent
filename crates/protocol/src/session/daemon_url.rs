//! Daemon WebSocket endpoints, shared by the native and the browser
//! transports.

use thiserror::Error;
use url::Url;

/// Errors returned when a daemon WebSocket URL is not a safe attach endpoint.
#[derive(Debug, Error)]
pub enum WebSocketUrlError {
    /// The URL could not be parsed.
    #[error("parse daemon WebSocket URL: {0}")]
    Parse(#[source] url::ParseError),
    /// The URL does not use WebSocket transport.
    #[error("daemon WebSocket URL scheme must be ws or wss")]
    InvalidScheme,
    /// The URL embeds credentials.
    #[error("daemon WebSocket URL must not contain credentials")]
    Credentials,
    /// The URL has no host.
    #[error("daemon WebSocket URL requires a host")]
    MissingHost,
    /// The URL does not target the exact daemon endpoint.
    #[error("daemon WebSocket URL path must be exactly /ws without query or fragment")]
    InvalidEndpoint,
    /// The URL a page connects to does not end in the daemon endpoint.
    #[error("daemon WebSocket URL path must end in /ws without query or fragment")]
    InvalidPageEndpoint,
}

/// Validates that a URL targets a daemon's exact client WebSocket endpoint:
/// `ws://` or `wss://` (TLS), any host, the path `/ws`, no credentials. A
/// `ws://` URL off loopback sends the token in the clear; the daemon decides
/// where it listens.
pub fn validate_websocket_url(value: &str) -> Result<(), WebSocketUrlError> {
    parse_daemon_url(value).map(drop)
}

/// Validates the daemon endpoint a browser page connects to: as
/// [`validate_websocket_url`], except that the path is any path ending in
/// `/ws`, for a daemon a proxy serves under a subpath
/// (`wss://example.org/cookie/ws`).
pub fn validate_page_websocket_url(value: &str) -> Result<(), WebSocketUrlError> {
    let url = parse_websocket_url(value)?;
    if !url.path().ends_with("/ws") || url.query().is_some() || url.fragment().is_some() {
        return Err(WebSocketUrlError::InvalidPageEndpoint);
    }
    Ok(())
}

pub(super) fn parse_daemon_url(value: &str) -> Result<Url, WebSocketUrlError> {
    let url = parse_websocket_url(value)?;
    if url.path() != "/ws" || url.query().is_some() || url.fragment().is_some() {
        return Err(WebSocketUrlError::InvalidEndpoint);
    }
    Ok(url)
}

/// A `ws://` or `wss://` URL with a host and no credentials.
fn parse_websocket_url(value: &str) -> Result<Url, WebSocketUrlError> {
    let url = Url::parse(value).map_err(WebSocketUrlError::Parse)?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(WebSocketUrlError::InvalidScheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(WebSocketUrlError::Credentials);
    }
    url.host().ok_or(WebSocketUrlError::MissingHost)?;
    Ok(url)
}

/// The dial-in endpoint for runtime `runtime_id` of the daemon whose client
/// endpoint is `value`: the same host with the path `/runtime?id=<id>`.
pub fn runtime_dial_in_url(value: &str, runtime_id: &str) -> Result<String, WebSocketUrlError> {
    let mut url = parse_daemon_url(value)?;
    url.set_path("/runtime");
    url.query_pairs_mut().append_pair("id", runtime_id);
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::{WebSocketUrlError, validate_page_websocket_url, validate_websocket_url};

    #[test]
    fn a_page_may_reach_the_daemon_under_a_subpath_ending_in_ws() {
        for url in [
            "ws://127.0.0.1:7419/ws",
            "wss://abc.efg/cookie/ws",
            "wss://abc.efg/a/b/ws",
        ] {
            assert!(validate_page_websocket_url(url).is_ok(), "{url}");
        }
        for url in [
            "wss://abc.efg/cookie/wss",
            "wss://abc.efg/cookiews",
            "wss://abc.efg/",
            "wss://abc.efg/cookie/ws?x=1",
            "wss://abc.efg/cookie/ws#x",
        ] {
            assert!(
                matches!(
                    validate_page_websocket_url(url),
                    Err(WebSocketUrlError::InvalidPageEndpoint)
                ),
                "{url}"
            );
        }
        assert!(matches!(
            validate_page_websocket_url("https://abc.efg/ws"),
            Err(WebSocketUrlError::InvalidScheme)
        ));
        assert!(matches!(
            validate_page_websocket_url("wss://user:pass@abc.efg/ws"),
            Err(WebSocketUrlError::Credentials)
        ));
        // A native client still names the exact endpoint.
        assert!(matches!(
            validate_websocket_url("wss://abc.efg/cookie/ws"),
            Err(WebSocketUrlError::InvalidEndpoint)
        ));
    }
}
