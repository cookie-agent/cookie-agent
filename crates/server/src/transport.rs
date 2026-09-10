use async_trait::async_trait;
use cookie_agent_protocol::{MessageFrame, Transport, TransportError};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest as _},
};
use zeroize::Zeroizing;

use crate::{load_auth_token, validate_websocket_url};

pub struct InProcessStream {
    sender: mpsc::Sender<MessageFrame>,
    receiver: mpsc::Receiver<MessageFrame>,
}

#[must_use]
pub fn in_process_pair(capacity: usize) -> (InProcessStream, InProcessStream) {
    let (client_to_server_tx, client_to_server_rx) = mpsc::channel(capacity);
    let (server_to_client_tx, server_to_client_rx) = mpsc::channel(capacity);
    (
        InProcessStream {
            sender: client_to_server_tx,
            receiver: server_to_client_rx,
        },
        InProcessStream {
            sender: server_to_client_tx,
            receiver: client_to_server_rx,
        },
    )
}

#[async_trait]
impl Transport for InProcessStream {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.sender
            .send(frame)
            .await
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        Ok(self.receiver.recv().await)
    }
}

/// Tokio WebSocket transport for protocol clients.
pub struct WebSocketTransport {
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
}

impl WebSocketTransport {
    /// Connect to a validated daemon endpoint using the standard bearer token.
    pub async fn connect(url: &str) -> Result<Self, TransportError> {
        let token = Zeroizing::new(
            load_auth_token().map_err(|error| TransportError::Other(error.to_string()))?,
        );
        Self::connect_with_token(url, &token).await
    }

    /// Connect with an explicit bearer token, primarily for isolated clients and tests.
    pub async fn connect_with_token(url: &str, token: &str) -> Result<Self, TransportError> {
        validate_websocket_url(url).map_err(|error| TransportError::Other(error.to_string()))?;
        let request = authenticated_request(url, token)?;
        let (socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|error| websocket_error(&error, &[token]))?;
        Ok(Self { socket })
    }
}

fn websocket_error(
    error: &tokio_tungstenite::tungstenite::Error,
    secrets: &[&str],
) -> TransportError {
    let mut message = error.to_string();
    if let tokio_tungstenite::tungstenite::Error::Http(response) = error
        && let Some(body) = response.body()
    {
        message.push_str(&format!(
            "\nResponse body:\n{}",
            cookie_agent_protocol::diagnostics::sanitize(
                &String::from_utf8_lossy(body),
                secrets,
                4096
            )
        ));
    }
    TransportError::Other(cookie_agent_protocol::diagnostics::sanitize(
        &message, secrets, 4096,
    ))
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
            .map_err(|error| websocket_error(&error, &[]))
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
                    return Err(websocket_error(&error, &[]));
                }
            }
        }
    }
}

pub(crate) fn authenticated_request(
    url: &str,
    token: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, TransportError> {
    if token.len() != 43
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
    fn websocket_rejection_scrubs_decoded_body_before_adding_context() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder().status(403)
            .body(Some(br#"{"message":"review\u002dsecret","password":"review\"secret-tail","reason":"Useful cause"}"#.to_vec())).unwrap();
        let error = super::websocket_error(
            &tokio_tungstenite::tungstenite::Error::Http(Box::new(response)),
            &["review-secret"],
        )
        .to_string();
        assert!(error.contains("403"));
        assert!(error.contains("Useful cause"));
        assert!(!error.contains("review"));
        assert!(!error.contains("secret-tail"));
    }

    #[test]
    fn websocket_rejection_shows_body_without_headers_or_known_token() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(403)
            .header("set-cookie", "private-cookie")
            .body(Some(
                b"Gateway denied the connection; echoed opaque-known-value".to_vec(),
            ))
            .unwrap();
        let error = super::websocket_error(
            &tokio_tungstenite::tungstenite::Error::Http(Box::new(response)),
            &["opaque-known-value"],
        )
        .to_string();
        assert!(error.contains("403"));
        assert!(error.contains("Gateway denied the connection"));
        assert!(!error.contains("opaque-known-value"));
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
