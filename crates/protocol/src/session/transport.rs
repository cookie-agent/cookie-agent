use std::fmt;

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

/// One complete JSON-RPC message exchanged by a protocol transport.
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
