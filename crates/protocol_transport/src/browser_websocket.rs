//! Browser WebSocket transport for reaching a cookie daemon from a page.
//!
//! The browser's `WebSocket` is not `Send`, so it lives in one local task on
//! the page's event loop, which exchanges frames with the transport over
//! channels; the client's own futures keep their bounds. A browser cannot set
//! `Authorization` on a WebSocket, so this transport sends no token: the
//! client authenticates in its handshake ([`Client::handshake_with_token`]).

use async_trait::async_trait;
use tokio::sync::mpsc;
use wasm_bindgen::{JsCast as _, closure::Closure};
use web_sys::{BinaryType, CloseEvent, MessageEvent, WebSocket};

use cookie_agent_protocol::{
    MAX_WEBSOCKET_MESSAGE_BYTES, MessageFrame, Transport, TransportError, diagnostics,
};

use crate::{Client, ClientError, daemon_url::validate_page_websocket_url};

impl Client {
    /// Connect to the daemon endpoint `url` from a browser page: `ws://` or
    /// `wss://`, its path ending in `/ws` (a proxy may serve the daemon
    /// under a subpath).
    /// Authenticate with [`Self::handshake_with_token`] next.
    ///
    /// A connection the daemon refuses or closes before its handshake, or
    /// one nothing answers, fails with
    /// [`ClientError::ConnectionRefusedBeforeHandshake`]: the browser shows
    /// the page no HTTP status, so that is all it can tell.
    pub async fn connect_browser_websocket(url: &str) -> Result<Self, ClientError> {
        BrowserWebSocketTransport::connect(url)
            .await
            .map(Self::connect_stream)
    }
}

/// What the socket's handlers saw, in the order the browser reported it.
enum SocketEvent {
    Open,
    Text(String),
    /// A message longer than [`MAX_WEBSOCKET_MESSAGE_BYTES`], in bytes.
    TooLarge(usize),
    Closed {
        code: u16,
        reason: String,
        clean: bool,
    },
}

/// A socket with its handlers attached. Dropping it detaches them, so the
/// browser never calls a dropped handler, and closes the socket.
struct Socket {
    socket: WebSocket,
    _open: Closure<dyn FnMut()>,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _close: Closure<dyn FnMut(CloseEvent)>,
}

impl Socket {
    fn open(url: &str, events: &mpsc::UnboundedSender<SocketEvent>) -> Result<Self, ClientError> {
        let socket = WebSocket::new(url).map_err(|error| {
            ClientError::WebSocket(diagnostics::sanitize(&format!("{error:?}"), 4096))
        })?;
        socket.set_binary_type(BinaryType::Arraybuffer);
        let open = {
            let events = events.clone();
            Closure::<dyn FnMut()>::new(move || {
                let _ = events.send(SocketEvent::Open);
            })
        };
        let message = {
            let events = events.clone();
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                // Like the native transport, only text messages carry frames.
                let Some(text) = event.data().as_string() else {
                    return;
                };
                let _ = events.send(if text.len() > MAX_WEBSOCKET_MESSAGE_BYTES {
                    SocketEvent::TooLarge(text.len())
                } else {
                    SocketEvent::Text(text)
                });
            })
        };
        let close = {
            let events = events.clone();
            Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
                let _ = events.send(SocketEvent::Closed {
                    code: event.code(),
                    reason: event.reason(),
                    clean: event.was_clean(),
                });
            })
        };
        socket.set_onopen(Some(open.as_ref().unchecked_ref()));
        socket.set_onmessage(Some(message.as_ref().unchecked_ref()));
        socket.set_onclose(Some(close.as_ref().unchecked_ref()));
        Ok(Self {
            socket,
            _open: open,
            _message: message,
            _close: close,
        })
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.socket.set_onopen(None);
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        let _ = self.socket.close();
    }
}

/// Browser WebSocket transport for protocol clients on wasm32.
pub struct BrowserWebSocketTransport {
    outgoing: mpsc::UnboundedSender<String>,
    events: mpsc::UnboundedReceiver<SocketEvent>,
    ended: bool,
}

impl BrowserWebSocketTransport {
    /// Open a WebSocket to the daemon endpoint `url`, whose path ends in
    /// `/ws`, and wait for it to open. A socket that closes first fails with
    /// [`ClientError::ConnectionRefusedBeforeHandshake`].
    pub async fn connect(url: &str) -> Result<Self, ClientError> {
        validate_page_websocket_url(url)
            .map_err(|error| ClientError::WebSocket(error.to_string()))?;
        let (event_sender, mut events) = mpsc::unbounded_channel();
        // Dropped with this future if the caller stops waiting.
        let socket = Socket::open(url, &event_sender)?;
        drop(event_sender);
        loop {
            match events.recv().await {
                Some(SocketEvent::Open) => break,
                Some(SocketEvent::Closed { code, reason, .. }) => {
                    return Err(ClientError::ConnectionRefusedBeforeHandshake {
                        code,
                        reason: diagnostics::sanitize(&reason, 4096),
                    });
                }
                Some(SocketEvent::Text(_) | SocketEvent::TooLarge(_)) => {}
                None => return Err(ClientError::Closed),
            }
        }
        let (outgoing, outgoing_frames) = mpsc::unbounded_channel();
        wasm_bindgen_futures::spawn_local(socket_task(socket, outgoing_frames));
        Ok(Self {
            outgoing,
            events,
            ended: false,
        })
    }
}

/// What a connection the daemon closed to restart ends with.
const RESTARTING: &str = "the daemon is restarting";

/// Owns the socket: writes each outgoing frame to it, and closes it once the
/// transport is dropped.
async fn socket_task(socket: Socket, mut frames: mpsc::UnboundedReceiver<String>) {
    while let Some(text) = frames.recv().await {
        // A socket that has closed discards the frame; its close event
        // ends the transport.
        if socket.socket.send_with_str(&text).is_err() {
            break;
        }
    }
}

#[async_trait]
impl Transport for BrowserWebSocketTransport {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.outgoing
            .send(frame.into_text()?)
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        loop {
            if self.ended {
                return Ok(None);
            }
            match self.events.recv().await {
                Some(SocketEvent::Text(text)) => return Ok(Some(MessageFrame::Text(text))),
                Some(SocketEvent::TooLarge(bytes)) => {
                    self.ended = true;
                    return Err(TransportError::Other(format!(
                        "a message of {bytes} bytes is larger than the \
                         {MAX_WEBSOCKET_MESSAGE_BYTES}-byte limit"
                    )));
                }
                Some(SocketEvent::Closed {
                    code,
                    reason,
                    clean,
                }) => {
                    self.ended = true;
                    if code == cookie_agent_protocol::RESTART_CLOSE_CODE {
                        return Err(TransportError::Other(RESTARTING.into()));
                    }
                    if clean {
                        return Ok(None);
                    }
                    let mut message = format!("the connection closed abnormally (code {code})");
                    if !reason.is_empty() {
                        message.push_str(&format!(": {reason}"));
                    }
                    return Err(TransportError::Other(diagnostics::sanitize(&message, 4096)));
                }
                Some(SocketEvent::Open) => {}
                None => return Ok(None),
            }
        }
    }
}
