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

/// How long a dial-in runtime's connection may be quiet before the runtime
/// sends a WebSocket ping, which the daemon answers with a pong.
pub const DIAL_IN_PING_AFTER: std::time::Duration = std::time::Duration::from_secs(15);

/// How long a dial-in runtime's connection may go without any frame, a pong
/// included, before the runtime takes the daemon for gone and dials in
/// again: a connection that died without a close, such as across a sleep
/// or a network change, otherwise looks open for good.
pub const DIAL_IN_DEAD_AFTER: std::time::Duration = std::time::Duration::from_secs(45);

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

/// Why a runtime could not dial in to a daemon.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct DialInError {
    message: String,
    permanent: bool,
}

impl DialInError {
    fn permanent(message: String) -> Self {
        Self {
            message,
            permanent: true,
        }
    }

    /// Whether dialing in again cannot succeed until the runtime's ID, its
    /// token, or the daemon's configuration changes: a malformed URL or
    /// token, or a refusal the runtime protocol calls permanent
    /// ([`crate::runtime_protocol::dial_in::is_permanent_status`]). An
    /// unreachable daemon and an ID that is still connected are not.
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        self.permanent
    }
}

/// Tokio WebSocket transport for protocol clients.
pub struct WebSocketTransport {
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    /// Pings a quiet connection and ends a silent one; `None` waits for the
    /// other end as long as it takes.
    keepalive: Option<Keepalive>,
}

/// A connection's keepalive: when it last heard a frame, and whether a ping
/// has gone out since.
struct Keepalive {
    ping_after: std::time::Duration,
    dead_after: std::time::Duration,
    heard: tokio::time::Instant,
    pinged: bool,
}

/// What a keepalive whose deadline passed does.
enum KeepaliveDue {
    Ping,
    Dead,
    /// The deadline moved; wait again.
    Nothing,
}

impl Keepalive {
    fn new(ping_after: std::time::Duration, dead_after: std::time::Duration) -> Self {
        Self {
            ping_after,
            dead_after,
            heard: tokio::time::Instant::now(),
            pinged: false,
        }
    }

    fn deadline(&self) -> tokio::time::Instant {
        if self.pinged {
            self.heard + self.dead_after
        } else {
            self.heard + self.ping_after.min(self.dead_after)
        }
    }

    fn due(&mut self) -> KeepaliveDue {
        let now = tokio::time::Instant::now();
        if now >= self.heard + self.dead_after {
            KeepaliveDue::Dead
        } else if !self.pinged && now >= self.heard + self.ping_after {
            self.pinged = true;
            KeepaliveDue::Ping
        } else {
            KeepaliveDue::Nothing
        }
    }

    fn heard(&mut self) {
        self.heard = tokio::time::Instant::now();
        self.pinged = false;
    }
}

impl WebSocketTransport {
    /// Connect to a validated daemon endpoint with an explicit per-run bearer token.
    pub async fn connect_with_token(url: &str, token: &str) -> Result<Self, TransportError> {
        validate_websocket_url(url).map_err(|error| TransportError::Other(error.to_string()))?;
        Self::connect_request(url, token).await
    }

    /// Dial in to the daemon whose client endpoint is `url` as runtime
    /// `runtime_id`, with the daemon's bearer token. The error says
    /// whether dialing in again can succeed.
    pub async fn connect_runtime(
        url: &str,
        token: &str,
        runtime_id: &str,
    ) -> Result<Self, DialInError> {
        let url = runtime_dial_in_url(url, runtime_id)
            .map_err(|error| DialInError::permanent(error.to_string()))?;
        let request = authenticated_request(&url, token)
            .map_err(|error| DialInError::permanent(error.to_string()))?;
        match tokio_tungstenite::connect_async_with_config(request, Some(websocket_config()), false)
            .await
        {
            Ok((socket, _)) => Ok(Self {
                socket,
                keepalive: None,
            }
            .keep_alive(DIAL_IN_PING_AFTER, DIAL_IN_DEAD_AFTER)),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                let status = response.status();
                let body = response
                    .body()
                    .as_deref()
                    .map(|body| diagnostics::sanitize(String::from_utf8_lossy(body).trim(), 4096))
                    .filter(|body| !body.is_empty())
                    .unwrap_or_else(|| status.canonical_reason().unwrap_or("").to_owned());
                Err(DialInError {
                    message: format!(
                        "the daemon refused the runtime (HTTP {}): {body}",
                        status.as_u16()
                    ),
                    permanent: crate::runtime_protocol::dial_in::is_permanent_status(
                        status.as_u16(),
                    ),
                })
            }
            Err(error) => Err(DialInError {
                message: websocket_error(&error).to_string(),
                permanent: false,
            }),
        }
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
        Ok(Self {
            socket,
            keepalive: None,
        })
    }

    /// Pings the other end once the connection has been quiet for
    /// `ping_after`, and ends the connection with an error once it has gone
    /// `dead_after` without any frame. A dial-in runtime's connection does
    /// this with [`DIAL_IN_PING_AFTER`] and [`DIAL_IN_DEAD_AFTER`].
    #[must_use]
    pub fn keep_alive(
        mut self,
        ping_after: std::time::Duration,
        dead_after: std::time::Duration,
    ) -> Self {
        self.keepalive = Some(Keepalive::new(ping_after, dead_after));
        self
    }

    /// The next message, pinging a quiet connection and ending a silent
    /// one. Cancel-safe: the keepalive's state lives in `self`.
    async fn next_message(
        &mut self,
    ) -> Result<Option<Result<Message, tokio_tungstenite::tungstenite::Error>>, TransportError>
    {
        loop {
            let Some(keepalive) = &mut self.keepalive else {
                return Ok(self.socket.next().await);
            };
            let deadline = keepalive.deadline();
            let next = tokio::select! {
                next = self.socket.next() => Some(next),
                () = tokio::time::sleep_until(deadline) => None,
            };
            let Some(keepalive) = &mut self.keepalive else {
                unreachable!("checked above");
            };
            match next {
                Some(next) => {
                    keepalive.heard();
                    return Ok(next);
                }
                None => match keepalive.due() {
                    KeepaliveDue::Ping => {
                        self.socket
                            .send(Message::Ping(Vec::new().into()))
                            .await
                            .map_err(|error| websocket_error(&error))?;
                    }
                    KeepaliveDue::Dead => {
                        return Err(TransportError::Other(format!(
                            "nothing arrived from the daemon for {}",
                            crate::diagnostics::duration(keepalive.dead_after)
                        )));
                    }
                    KeepaliveDue::Nothing => {}
                },
            }
        }
    }

    async fn connect_request(url: &str, token: &str) -> Result<Self, TransportError> {
        let request = authenticated_request(url, token)?;
        let (socket, _) =
            tokio_tungstenite::connect_async_with_config(request, Some(websocket_config()), false)
                .await
                .map_err(|error| websocket_error(&error))?;
        Ok(Self {
            socket,
            keepalive: None,
        })
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
            match self.next_message().await? {
                Some(Ok(Message::Text(text))) => {
                    return Ok(Some(MessageFrame::Bytes(text.into())));
                }
                Some(Ok(Message::Close(frame))) => return closed(frame),
                None => return Ok(None),
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

/// What a close frame the daemon sent means: a normal closure, going away,
/// or a frame without a code ends the connection cleanly; a restart (1012)
/// ends it saying the daemon is restarting; another code,
/// such as a dial-in runtime that did not initialize, ends it with the
/// daemon's reason.
fn closed(
    frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>,
) -> Result<Option<MessageFrame>, TransportError> {
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    match frame {
        None => Ok(None),
        Some(frame) if matches!(frame.code, CloseCode::Normal | CloseCode::Away) => Ok(None),
        Some(frame) if u16::from(frame.code) == crate::RESTART_CLOSE_CODE => {
            Err(TransportError::Other("the daemon is restarting".into()))
        }
        Some(frame) => {
            let code = u16::from(frame.code);
            let reason = diagnostics::sanitize(frame.reason.as_str(), 4096);
            Err(TransportError::Other(if reason.is_empty() {
                format!("the daemon closed the connection with code {code}")
            } else {
                format!("the daemon closed the connection with code {code}: {reason}")
            }))
        }
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

    /// A daemon that closes to restart says so; a normal closure ends the
    /// connection cleanly; another code keeps its reason.
    #[test]
    fn a_restart_close_says_the_daemon_is_restarting() {
        use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};
        let frame = |code: u16, reason: &str| {
            Some(CloseFrame {
                code: CloseCode::from(code),
                reason: reason.to_owned().into(),
            })
        };
        let restart = super::closed(frame(
            crate::RESTART_CLOSE_CODE,
            crate::RESTART_CLOSE_REASON,
        ))
        .expect_err("a restart is not a clean end");
        assert_eq!(
            restart.to_string(),
            "transport error: the daemon is restarting"
        );
        assert!(matches!(super::closed(frame(1000, "")), Ok(None)));
        let other = super::closed(frame(1011, "broken")).expect_err("an abnormal close");
        assert!(other.to_string().contains("1011: broken"), "{other}");
    }

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

    /// A transport with a keepalive pings a quiet daemon, which answers
    /// with pongs while it reads, and gives up on one that stopped reading,
    /// as a daemon whose connection died without a close looks.
    #[tokio::test]
    async fn a_keepalive_pings_a_quiet_daemon_and_ends_a_silent_one() {
        use crate::Transport as _;
        const PING_AFTER: std::time::Duration = std::time::Duration::from_millis(50);
        const DEAD_AFTER: std::time::Duration = std::time::Duration::from_millis(250);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let (pings_sender, mut pings) = tokio::sync::mpsc::unbounded_channel();
        let (stop_reading, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("WebSocket accept");
            // Reading answers each ping with a pong.
            let reading = async {
                while let Some(Ok(message)) = socket.next().await {
                    if matches!(message, Message::Ping(_)) {
                        let _ = pings_sender.send(());
                    }
                }
            };
            tokio::select! {
                () = reading => {}
                _ = stopped => {}
            }
            // Holds the socket open without reading, so no pong goes out.
            std::future::pending::<()>().await;
            drop(socket);
        });
        let mut transport = super::WebSocketTransport::connect_with_token(
            &format!("ws://{address}/ws"),
            &"A".repeat(43),
        )
        .await
        .expect("connect")
        .keep_alive(PING_AFTER, DEAD_AFTER);
        // Answered pings keep it open well past the silence it allows.
        let quiet = tokio::time::timeout(DEAD_AFTER * 4, transport.recv()).await;
        assert!(quiet.is_err(), "a daemon that answers keeps it open");
        assert!(
            pings.recv().await.is_some(),
            "the quiet connection was pinged"
        );
        stop_reading.send(()).expect("server running");
        let error = tokio::time::timeout(DEAD_AFTER * 8, transport.recv())
            .await
            .expect("a silent daemon ends the connection")
            .expect_err("ends with an error");
        assert_eq!(
            error.to_string(),
            "transport error: nothing arrived from the daemon for 250 ms"
        );
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
