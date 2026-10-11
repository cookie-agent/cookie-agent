use std::fmt;

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

/// One complete JSON-RPC message exchanged by a protocol transport.
#[derive(Clone, PartialEq)]
pub enum MessageFrame {
    /// JSON text this process serialized.
    Text(String),
    /// JSON text as received from a socket, shared rather than copied. A
    /// reader parses it directly from these bytes; they are not checked to
    /// be UTF-8 until then.
    Bytes(bytes::Bytes),
    Value(Value),
}

impl MessageFrame {
    /// The frame's JSON text, for a transport that writes text. A
    /// [`Self::Value`] is serialized; a [`Self::Bytes`] that is not UTF-8
    /// is an invalid frame.
    pub fn into_text(self) -> Result<String, TransportError> {
        match self {
            Self::Text(text) => Ok(text),
            Self::Bytes(bytes) => String::from_utf8(bytes.into()).map_err(|error| {
                TransportError::Other(format!("frame is not UTF-8 text: {error}"))
            }),
            Self::Value(value) => Ok(serde_json::to_string(&value)?),
        }
    }
}

impl fmt::Debug for MessageFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => formatter.write_str("MessageFrame::Text(<redacted>)"),
            Self::Bytes(_) => formatter.write_str("MessageFrame::Bytes(<redacted>)"),
            Self::Value(_) => formatter.write_str("MessageFrame::Value(<redacted>)"),
        }
    }
}

/// Frame-level channel with no JSON-RPC semantics.
#[async_trait]
pub trait Transport: Send {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError>;
}

pub use Transport as MessageStream;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport closed")]
    Closed,
    #[error("invalid transport frame: {0}")]
    InvalidFrame(#[from] serde_json::Error),
    #[error("transport error: {0}")]
    Other(String),
}

/// One end of an in-memory frame channel created by [`in_process_pair`].
pub struct InProcessStream {
    sender: mpsc::Sender<MessageFrame>,
    receiver: mpsc::Receiver<MessageFrame>,
}

/// Creates two connected in-memory transports, typically a client end and the
/// server end handed to `serve`. Each direction buffers up to `capacity`
/// frames.
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
