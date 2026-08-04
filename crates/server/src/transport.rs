use std::fmt;

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

/// One complete JSON-RPC message.
#[derive(Clone, PartialEq)]
pub enum MessageFrame {
    Text(String),
    Value(Value),
}

impl fmt::Debug for MessageFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => formatter.write_str("MessageFrame::Text(<redacted>)"),
            Self::Value(_) => formatter.write_str("MessageFrame::Value(<redacted>)"),
        }
    }
}

#[async_trait]
pub trait MessageStream: Send {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError>;
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport closed")]
    Closed,
    #[error("websocket error: {0}")]
    WebSocket(#[from] axum::Error),
}

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
impl MessageStream for InProcessStream {
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
