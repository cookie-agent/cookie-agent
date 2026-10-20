//! Client-side WebSocket transport for reaching a cookie daemon, as a client
//! (`/ws`) or as a dial-in runtime (`/runtime`).

use async_trait::async_trait;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest as _, protocol::WebSocketConfig},
};
use zeroize::Zeroizing;

use super::{
    Client, ClientError, MAX_WEBSOCKET_MESSAGE_BYTES, MessageFrame, Transport, TransportError,
    daemon_url::{parse_daemon_url, runtime_dial_in_url, validate_websocket_url},
};
use crate::diagnostics;

/// Length of the base64url encoding of the daemon's 32-byte bearer token.
const TOKEN_ENCODED_BYTES: usize = 43;

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

    /// Connect to the `ws://` daemon endpoint `url` over `stream`, a TCP
    /// connection to its host the caller has already opened, using the
    /// bearer token. A caller that bounds the TCP connect apart from the
    /// WebSocket upgrade opens the stream itself.
    pub async fn connect_websocket_over(
        stream: tokio::net::TcpStream,
        url: &str,
        token: &str,
    ) -> Result<Self, ClientError> {
        WebSocketTransport::connect_over(stream, url, token)
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
        Self::connect_request(url, token).await
    }

    /// Dial in to the daemon whose client endpoint is `url` as runtime
    /// `runtime_id`, with the daemon's bearer token.
    pub async fn connect_runtime(
        url: &str,
        token: &str,
        runtime_id: &str,
    ) -> Result<Self, TransportError> {
        let url = runtime_dial_in_url(url, runtime_id)
            .map_err(|error| TransportError::Other(error.to_string()))?;
        Self::connect_request(&url, token).await
    }

    /// Upgrade `stream`, an open TCP connection to the host of the `ws://`
    /// daemon endpoint `url`, with an explicit per-run bearer token. The
    /// stream carries no TLS, so a `wss://` URL is refused.
    pub async fn connect_over(
        stream: tokio::net::TcpStream,
        url: &str,
        token: &str,
    ) -> Result<Self, TransportError> {
        let parsed =
            parse_daemon_url(url).map_err(|error| TransportError::Other(error.to_string()))?;
        if parsed.scheme() != "ws" {
            return Err(TransportError::Other(
                "a daemon reached over an open TCP connection needs a ws:// URL".into(),
            ));
        }
        let request = authenticated_request(url, token)?;
        let (socket, _) = tokio_tungstenite::client_async_with_config(
            request,
            MaybeTlsStream::Plain(stream),
            Some(websocket_config()),
        )
        .await
        .map_err(|error| websocket_error(&error))?;
        Ok(Self { socket })
    }

    async fn connect_request(url: &str, token: &str) -> Result<Self, TransportError> {
        let request = authenticated_request(url, token)?;
        let (socket, _) =
            tokio_tungstenite::connect_async_with_config(request, Some(websocket_config()), false)
                .await
                .map_err(|error| websocket_error(&error))?;
        Ok(Self { socket })
    }
}

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_WEBSOCKET_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_WEBSOCKET_MESSAGE_BYTES))
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
        self.socket
            .send(Message::Text(frame.into_text()?.into()))
            .await
            .map_err(|error| websocket_error(&error))
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return Ok(Some(MessageFrame::Bytes(text.into())));
                }
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    return Err(websocket_error(&error));
                }
            }
        }
    }

    /// Sends a normal-closure close frame and waits for the daemon's
    /// answer. When the daemon closed first, sending fails and reading on
    /// sends the answer the socket queued.
    async fn close(&mut self) {
        let _ = self.socket.close(Some(normal_closure())).await;
        while let Some(Ok(_)) = self.socket.next().await {}
    }
}

/// A close frame with code 1000 (normal closure).
fn normal_closure() -> tokio_tungstenite::tungstenite::protocol::CloseFrame {
    tokio_tungstenite::tungstenite::protocol::CloseFrame {
        code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
        reason: "".into(),
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
    use futures_util::{SinkExt as _, StreamExt as _};
    use serde_json::{Value, json};
    use tokio_tungstenite::tungstenite::Message;

    use super::authenticated_request;
    use crate::Client;

    /// A replay page or a single large event (a model turn, a native context
    /// window) arrives as one frame well past tungstenite's 16 MiB default.
    #[tokio::test]
    async fn a_response_past_the_default_frame_limit_arrives_whole() {
        const BLOB_BYTES: usize = 40 * 1024 * 1024;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("WebSocket accept");
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let request: Value = serde_json::from_str(&text).expect("request");
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": { "blob": "x".repeat(BLOB_BYTES) },
                });
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .expect("send response");
            }
        });
        let client =
            Client::connect_websocket_with_token(&format!("ws://{address}/ws"), &"A".repeat(43))
                .await
                .expect("connect");
        let result: Value = client
            .call("test.large", &json!({}))
            .await
            .expect("large response");
        assert_eq!(result["blob"].as_str().map(str::len), Some(BLOB_BYTES));
        client.shutdown();
        server.abort();
    }

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
