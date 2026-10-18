//! A symmetric JSON-RPC 2.0 peer for the runtime protocol and the plugin
//! protocol.
//!
//! Both ends of the runtime protocol send requests and notifications, so one
//! [`Peer`] type serves the daemon's client and the runtime's server. It runs
//! over any frame [`Transport`]: an in-memory pair, or [`LineTransport`] over
//! a child's stdio. Every frame is bounded by [`MAX_FRAME_BYTES`]. The
//! envelope is information: members JSON-RPC does not define are ignored, and
//! a request ID may be an integer or a string (this peer sends integers). A
//! frame that is not JSON, or not a request, notification, or reply, ends the
//! connection.
//!
//! The transport's `recv` must be cancel-safe: the peer's I/O task selects
//! between receiving and sending.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{JsonRpcVersion, MessageFrame, Transport, TransportError};
use async_trait::async_trait;
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, value::RawValue};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt as _},
    sync::{mpsc, oneshot},
};
use tokio_util::{
    codec::{FramedRead, LinesCodec, LinesCodecError},
    sync::CancellationToken,
};

use crate::runtime_protocol::{MAX_FRAME_BYTES, error_code};

/// Why a request failed.
#[derive(Clone, Debug, thiserror::Error)]
pub enum RpcError {
    /// The connection ended; carries why.
    #[error("connection closed: {0}")]
    Closed(String),
    /// The other end answered with an error.
    #[error("{message}")]
    Remote {
        code: i32,
        message: String,
        data: Option<Value>,
    },
    /// A message could not be built or read.
    #[error("invalid runtime protocol message: {0}")]
    Protocol(String),
}

impl RpcError {
    /// An error answer with `code` and `message`.
    #[must_use]
    pub fn remote(code: i32, message: impl Into<String>) -> Self {
        Self::Remote {
            code,
            message: message.into(),
            data: None,
        }
    }
}

type PendingReply = oneshot::Sender<Result<Box<RawValue>, RpcError>>;

struct Pending {
    replies: HashMap<u64, PendingReply>,
    /// Set once the connection ended; no reply arrives after that.
    closed: Option<String>,
}

struct Shared {
    outbound: mpsc::Sender<String>,
    pending: Mutex<Pending>,
    next_id: AtomicU64,
    /// Stops the I/O task.
    stop: CancellationToken,
    /// Cancelled when the connection has ended.
    ended: CancellationToken,
}

/// One end of a runtime protocol connection. Clones share it. The
/// connection ends when the transport ends or [`Self::close`] is called;
/// dropping clones does not end it.
#[derive(Clone)]
pub struct Peer {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Peer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Peer")
            .field("ended", &self.shared.ended.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// A message the other end sent that is not a reply.
pub enum Incoming {
    Request(IncomingRequest),
    Notification(IncomingNotification),
}

/// A request to answer with [`IncomingRequest::respond`] or
/// [`IncomingRequest::fail`].
pub struct IncomingRequest {
    pub method: String,
    params: Option<Box<RawValue>>,
    id: RequestId,
    shared: Arc<Shared>,
}

/// A request ID as JSON-RPC 2.0 allows it: an integer or a string. A reply
/// echoes it exactly.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
enum RequestId {
    Unsigned(u64),
    Signed(i64),
    Text(String),
}

pub struct IncomingNotification {
    pub method: String,
    params: Option<Box<RawValue>>,
}

fn parse_params<T: DeserializeOwned>(params: Option<&RawValue>) -> Result<T, RpcError> {
    let text = params.map_or("null", RawValue::get);
    serde_json::from_str(text).map_err(|error| {
        RpcError::remote(
            error_code::INVALID_PARAMS,
            format!("invalid params: {error}"),
        )
    })
}

impl IncomingNotification {
    /// The params as `T`, rejecting unknown fields as `T` does.
    pub fn params<T: DeserializeOwned>(&self) -> Result<T, RpcError> {
        parse_params(self.params.as_deref())
    }
}

impl IncomingRequest {
    /// The params as `T`, rejecting unknown fields as `T` does.
    pub fn params<T: DeserializeOwned>(&self) -> Result<T, RpcError> {
        parse_params(self.params.as_deref())
    }

    /// Answers with `result`. A result too large for one frame is answered
    /// with an error instead.
    pub async fn respond<T: Serialize>(self, result: &T) -> Result<(), RpcError> {
        let text = match encode(&ResponseRef {
            jsonrpc: JsonRpcVersion::current(),
            id: &self.id,
            result,
        }) {
            Ok(text) => text,
            Err(error) => {
                return self
                    .fail(RpcError::remote(
                        error_code::REPLY_TOO_LARGE,
                        error.to_string(),
                    ))
                    .await;
            }
        };
        send(&self.shared, text).await
    }

    /// Answers with an error. A non-[`RpcError::Remote`] error is sent as an
    /// internal error.
    pub async fn fail(self, error: RpcError) -> Result<(), RpcError> {
        let (code, message, data) = match error {
            RpcError::Remote {
                code,
                message,
                data,
            } => (code, message, data),
            other => (-32603, other.to_string(), None),
        };
        let text = encode(&ErrorResponseRef {
            jsonrpc: JsonRpcVersion::current(),
            id: &self.id,
            error: WireRpcError {
                code,
                message,
                data,
            },
        })?;
        send(&self.shared, text).await
    }
}

#[derive(Serialize)]
struct RequestRef<'a, P: ?Sized> {
    jsonrpc: JsonRpcVersion,
    id: u64,
    method: &'a str,
    params: &'a P,
}

#[derive(Serialize)]
struct NotificationRef<'a, P: ?Sized> {
    jsonrpc: JsonRpcVersion,
    method: &'a str,
    params: &'a P,
}

#[derive(Serialize)]
struct ResponseRef<'a, R: ?Sized> {
    jsonrpc: JsonRpcVersion,
    id: &'a RequestId,
    result: &'a R,
}

#[derive(Serialize)]
struct ErrorResponseRef<'a> {
    jsonrpc: JsonRpcVersion,
    id: &'a RequestId,
    error: WireRpcError,
}

#[derive(Debug, Deserialize, Serialize)]
struct WireRpcError {
    code: i32,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

/// Any message, before it is classified. Exactly one shape is valid: a
/// request (`id`, `method`), a notification (`method`), or a reply (`id`
/// with `result` or `error`). Other members are ignored.
#[derive(Deserialize)]
struct Envelope {
    #[allow(dead_code)]
    jsonrpc: JsonRpcVersion,
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Box<RawValue>>,
    #[serde(default)]
    result: Option<Box<RawValue>>,
    #[serde(default)]
    error: Option<WireRpcError>,
}

fn encode(message: &impl Serialize) -> Result<String, RpcError> {
    let text = serde_json::to_string(message)
        .map_err(|error| RpcError::Protocol(format!("message does not serialize: {error}")))?;
    if text.len() > MAX_FRAME_BYTES {
        return Err(RpcError::Protocol(format!(
            "message of {} bytes exceeds the {MAX_FRAME_BYTES}-byte frame bound",
            text.len()
        )));
    }
    Ok(text)
}

async fn send(shared: &Shared, text: String) -> Result<(), RpcError> {
    shared
        .outbound
        .send(text)
        .await
        .map_err(|_| RpcError::Closed(closed_reason(shared)))
}

fn closed_reason(shared: &Shared) -> String {
    shared
        .pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .closed
        .clone()
        .unwrap_or_else(|| "connection closed".to_owned())
}

impl Peer {
    /// Starts a peer on `transport`. Returns the peer and the stream of
    /// requests and notifications the other end sends, which ends with the
    /// connection.
    #[must_use]
    pub fn start(transport: Box<dyn Transport>) -> (Self, mpsc::Receiver<Incoming>) {
        let (outbound, outbound_rx) = mpsc::channel(64);
        let (incoming, incoming_rx) = mpsc::channel(64);
        let shared = Arc::new(Shared {
            outbound,
            pending: Mutex::new(Pending {
                replies: HashMap::new(),
                closed: None,
            }),
            next_id: AtomicU64::new(1),
            stop: CancellationToken::new(),
            ended: CancellationToken::new(),
        });
        tokio::spawn(run_io(
            Arc::clone(&shared),
            transport,
            outbound_rx,
            incoming,
        ));
        (Self { shared }, incoming_rx)
    }

    /// Sends a request and waits for its reply.
    pub async fn request<P, R>(&self, method: &str, params: &P) -> Result<R, RpcError>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let text = encode(&RequestRef {
            jsonrpc: JsonRpcVersion::current(),
            id,
            method,
            params,
        })?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(reason) = &pending.closed {
                return Err(RpcError::Closed(reason.clone()));
            }
            pending.replies.insert(id, sender);
        }
        let guard = PendingGuard {
            shared: &self.shared,
            id,
        };
        send(&self.shared, text).await?;
        let reply = receiver
            .await
            .map_err(|_| RpcError::Closed(closed_reason(&self.shared)))??;
        drop(guard);
        serde_json::from_str(reply.get())
            .map_err(|error| RpcError::Protocol(format!("invalid reply to `{method}`: {error}")))
    }

    /// Sends a notification.
    pub async fn notify<P: Serialize + ?Sized>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<(), RpcError> {
        let text = encode(&NotificationRef {
            jsonrpc: JsonRpcVersion::current(),
            method,
            params,
        })?;
        send(&self.shared, text).await
    }

    /// Sends a notification without waiting, for use from `Drop`. It is
    /// lost if the outbound queue is full and no runtime can take it.
    pub fn notify_detached<P: Serialize + ?Sized>(&self, method: &str, params: &P) {
        let Ok(text) = encode(&NotificationRef {
            jsonrpc: JsonRpcVersion::current(),
            method,
            params,
        }) else {
            return;
        };
        match self.shared.outbound.try_send(text) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(text)) => {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let shared = Arc::clone(&self.shared);
                    handle.spawn(async move {
                        let _ = send(&shared, text).await;
                    });
                }
            }
        }
    }

    /// Ends the connection.
    pub fn close(&self) {
        self.shared.stop.cancel();
    }

    /// Whether the connection has ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.ended.is_cancelled()
    }

    /// Waits for the connection to end and returns why it did.
    pub async fn closed(&self) -> String {
        self.shared.ended.cancelled().await;
        closed_reason(&self.shared)
    }
}

/// Forgets a pending reply when its request future is dropped.
struct PendingGuard<'a> {
    shared: &'a Shared,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replies
            .remove(&self.id);
    }
}

async fn run_io(
    shared: Arc<Shared>,
    mut transport: Box<dyn Transport>,
    mut outbound: mpsc::Receiver<String>,
    incoming: mpsc::Sender<Incoming>,
) {
    let reason = loop {
        tokio::select! {
            () = shared.stop.cancelled() => {
                // Messages queued before the close, such as the reply to
                // `runtime/shutdown`, still go out.
                while let Ok(text) = outbound.try_recv() {
                    if transport.send(MessageFrame::Text(text)).await.is_err() {
                        break;
                    }
                }
                break "connection closed locally".to_owned();
            }
            text = outbound.recv() => {
                let Some(text) = text else {
                    break "connection closed locally".to_owned();
                };
                if let Err(error) = transport.send(MessageFrame::Text(text)).await {
                    break transport_reason(&error);
                }
            }
            frame = transport.recv() => match frame {
                Ok(Some(frame)) => {
                    if let Err(reason) = dispatch(&shared, frame, &incoming).await {
                        break reason;
                    }
                }
                Ok(None) => break "the other end closed the connection".to_owned(),
                Err(error) => break transport_reason(&error),
            },
        }
    };
    let replies = {
        let mut pending = shared
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.closed = Some(reason.clone());
        std::mem::take(&mut pending.replies)
    };
    for (_, reply) in replies {
        let _ = reply.send(Err(RpcError::Closed(reason.clone())));
    }
    shared.ended.cancel();
}

fn transport_reason(error: &TransportError) -> String {
    match error {
        TransportError::Closed => "the other end closed the connection".to_owned(),
        other => other.to_string(),
    }
}

/// Routes one frame. An error ends the connection.
async fn dispatch(
    shared: &Arc<Shared>,
    frame: MessageFrame,
    incoming: &mpsc::Sender<Incoming>,
) -> Result<(), String> {
    let envelope: Envelope = match frame {
        MessageFrame::Text(text) => parse_frame(text.as_bytes())?,
        MessageFrame::Bytes(bytes) => parse_frame(&bytes)?,
        MessageFrame::Value(value) => {
            let text =
                serde_json::to_string(&value).map_err(|error| format!("invalid frame: {error}"))?;
            parse_frame(text.as_bytes())?
        }
    };
    match envelope {
        Envelope {
            id: Some(id),
            method: Some(method),
            params,
            result: None,
            error: None,
            ..
        } => {
            let request = IncomingRequest {
                method,
                params,
                id,
                shared: Arc::clone(shared),
            };
            if let Err(mpsc::error::SendError(Incoming::Request(request))) =
                incoming.send(Incoming::Request(request)).await
            {
                let _ = request
                    .fail(RpcError::remote(-32603, "the receiver is shutting down"))
                    .await;
            }
            Ok(())
        }
        Envelope {
            id: None,
            method: Some(method),
            params,
            result: None,
            error: None,
            ..
        } => {
            let _ = incoming
                .send(Incoming::Notification(IncomingNotification {
                    method,
                    params,
                }))
                .await;
            Ok(())
        }
        Envelope {
            id: Some(id),
            method: None,
            params: None,
            result,
            error,
            ..
        } => {
            let reply = match (result, error) {
                (Some(result), None) => Ok(result),
                (None, Some(error)) => Err(RpcError::Remote {
                    code: error.code,
                    message: error.message,
                    data: error.data,
                }),
                _ => return Err("a reply needs exactly one of result and error".to_owned()),
            };
            // This peer's requests have unsigned IDs; a reply with another
            // ID answers nothing it sent.
            let sender = match id {
                RequestId::Unsigned(id) => shared
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .replies
                    .remove(&id),
                RequestId::Signed(_) | RequestId::Text(_) => None,
            };
            // A reply to a request whose caller gave up is dropped.
            if let Some(sender) = sender {
                let _ = sender.send(reply);
            }
            Ok(())
        }
        _ => Err("message is neither a request, a notification, nor a reply".to_owned()),
    }
}

fn parse_frame(bytes: &[u8]) -> Result<Envelope, String> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(format!(
            "frame of {} bytes exceeds the {MAX_FRAME_BYTES}-byte bound",
            bytes.len()
        ));
    }
    serde_json::from_slice(bytes).map_err(|error| format!("invalid frame: {error}"))
}

/// Newline-delimited frames over a byte stream, such as a child's stdio:
/// one JSON message per line, each at most
/// [`MAX_FRAME_BYTES`].
pub struct LineTransport<R, W> {
    reader: FramedRead<R, LinesCodec>,
    writer: W,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> LineTransport<R, W> {
    #[must_use]
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: FramedRead::new(reader, LinesCodec::new_with_max_length(MAX_FRAME_BYTES)),
            writer,
        }
    }
}

#[async_trait]
impl<R, W> Transport for LineTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        let mut text = frame.into_text()?;
        if text.contains('\n') {
            return Err(TransportError::Other("frame contains a newline".into()));
        }
        text.push('\n');
        self.writer
            .write_all(text.as_bytes())
            .await
            .map_err(|error| TransportError::Other(error.to_string()))?;
        self.writer
            .flush()
            .await
            .map_err(|error| TransportError::Other(error.to_string()))
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        match self.reader.next().await {
            Some(Ok(line)) => Ok(Some(MessageFrame::Text(line))),
            Some(Err(LinesCodecError::MaxLineLengthExceeded)) => Err(TransportError::Other(
                format!("frame exceeds the {MAX_FRAME_BYTES}-byte bound"),
            )),
            Some(Err(LinesCodecError::Io(error))) => Err(TransportError::Other(error.to_string())),
            None => Ok(None),
        }
    }
}
