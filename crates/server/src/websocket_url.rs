use std::net::IpAddr;

use thiserror::Error;
use url::{Host, Url};

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
    /// The URL host is not loopback.
    #[error("daemon WebSocket URL host must be loopback")]
    NonLoopbackHost,
    /// The URL does not target the exact daemon endpoint.
    #[error("daemon WebSocket URL path must be exactly /ws without query or fragment")]
    InvalidEndpoint,
}

/// Validates that a URL targets the daemon's exact loopback WebSocket endpoint.
pub fn validate_websocket_url(value: &str) -> Result<(), WebSocketUrlError> {
    let url = Url::parse(value).map_err(WebSocketUrlError::Parse)?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(WebSocketUrlError::InvalidScheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(WebSocketUrlError::Credentials);
    }
    let loopback = match url.host().ok_or(WebSocketUrlError::MissingHost)? {
        Host::Domain(host) => host.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => IpAddr::V4(address).is_loopback(),
        Host::Ipv6(address) => IpAddr::V6(address).is_loopback(),
    };
    if !loopback {
        return Err(WebSocketUrlError::NonLoopbackHost);
    }
    if url.path() != "/ws" || url.query().is_some() || url.fragment().is_some() {
        return Err(WebSocketUrlError::InvalidEndpoint);
    }
    Ok(())
}
