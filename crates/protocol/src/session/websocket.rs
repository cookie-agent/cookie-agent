//! Client-side WebSocket transport for reaching a local cookie daemon.

use std::net::IpAddr;

use async_trait::async_trait;
use futures_util::{SinkExt as _, StreamExt as _};
use thiserror::Error;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest as _},
};
use url::{Host, Url};
use zeroize::Zeroizing;

use super::{Client, ClientError, MessageFrame, Transport, TransportError};
use crate::diagnostics;

/// Length of the base64url encoding of the daemon's 32-byte bearer token.
const TOKEN_ENCODED_BYTES: usize = 43;

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

impl Client {
    /// Connect to a validated daemon endpoint using the per-run bearer token.
    pub async fn connect_websocket_with_token(url: &str, token: &str) -> Result<Self, ClientError> {
        WebSocketTransport::connect_with_token(url, token)
            .await
            .map(Self::connect_stream)
            .map_err(|error| {
                ClientError::WebSocket(diagnostics::sanitize(
                    &diagnostics::error_chain(&error),
                    4096,
                ))
            })
    }
}

/// Tokio WebSocket transport for protocol clients.
pub struct WebSocketTransport {
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
}

impl WebSocketTransport {
    /// Connect to a validated daemon endpoint with an explicit per-run bearer token.
    pub async fn connect_with_token(url: &str, token: &str) -> Result<Self, TransportError> {
        validate_websocket_url(url).map_err(|error| TransportError::Other(error.to_string()))?;
        let request = authenticated_request(url, token)?;
        let (socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|error| websocket_error(&error))?;
        Ok(Self { socket })
    }
}

fn websocket_error(error: &tokio_tungstenite::tungstenite::Error) -> TransportError {
    let mut message = error.to_string();
    if let tokio_tungstenite::tungstenite::Error::Http(response) = error
        && let Some(body) = response.body()
    {
        message.push_str(&format!(
            "\nResponse body:\n{}",
            diagnostics::sanitize(&String::from_utf8_lossy(body), 4096)
        ));
    }
    TransportError::Other(diagnostics::sanitize(&message, 4096))
}

#[async_trait]
impl Transport for WebSocketTransport {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        let text = match frame {
            MessageFrame::Text(text) => text,
            MessageFrame::Value(value) => serde_json::to_string(&value)?,
        };
        self.socket
            .send(Message::Text(text.into()))
            .await
            .map_err(|error| websocket_error(&error))
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return Ok(Some(MessageFrame::Text(text.to_string())));
                }
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    return Err(websocket_error(&error));
                }
            }
        }
    }
}

fn authenticated_request(
    url: &str,
    token: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, TransportError> {
    if token.len() != TOKEN_ENCODED_BYTES
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(TransportError::Other(
            "invalid daemon authentication token".into(),
        ));
    }
    let mut request = url
        .into_client_request()
        .map_err(|error| TransportError::Other(error.to_string()))?;
    let authorization = Zeroizing::new(format!("Bearer {token}"));
    let value = authorization
        .parse()
        .map_err(|_| TransportError::Other("invalid authorization header".into()))?;
    request.headers_mut().insert("authorization", value);
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::authenticated_request;

    #[test]
    fn websocket_rejection_preserves_decoded_body_and_context() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder().status(403)
            .body(Some(br#"{"message":"review\u002dsecret","password":"review\"secret-tail","reason":"Useful cause"}"#.to_vec())).unwrap();
        let error = super::websocket_error(&tokio_tungstenite::tungstenite::Error::Http(Box::new(
            response,
        )))
        .to_string();
        assert!(error.contains("403"));
        assert!(error.contains("Useful cause"));
        assert!(error.contains("review-secret"));
        assert!(error.contains("secret-tail"));
    }

    #[test]
    fn websocket_rejection_shows_body_including_echoed_token() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(403)
            .header("set-cookie", "private-cookie")
            .body(Some(
                b"Gateway denied the connection; echoed opaque-known-value".to_vec(),
            ))
            .unwrap();
        let error = super::websocket_error(&tokio_tungstenite::tungstenite::Error::Http(Box::new(
            response,
        )))
        .to_string();
        assert!(error.contains("403"));
        assert!(error.contains("Gateway denied the connection"));
        assert!(error.contains("opaque-known-value"));
        assert!(!error.contains("private-cookie"));
    }

    #[test]
    fn websocket_auth_uses_a_bearer_header_without_url_credentials() {
        let token = "A".repeat(43);
        let request =
            authenticated_request("ws://127.0.0.1:7419/ws", &token).expect("authenticated request");
        assert_eq!(request.uri().to_string(), "ws://127.0.0.1:7419/ws");
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some(format!("Bearer {token}").as_str())
        );
        assert!(!request.uri().to_string().contains(&token));
        assert!(authenticated_request("ws://127.0.0.1:7419/ws", "sentinel-secret").is_err());
    }
}
