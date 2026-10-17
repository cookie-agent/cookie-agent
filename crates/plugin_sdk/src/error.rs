use serde_json::Value;

/// An error returned while running or using a plugin server.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// Standard input or output failed.
    #[error("plugin transport failed: {0}")]
    Transport(#[from] std::io::Error),
    /// A JSON message could not be encoded or decoded.
    #[error("plugin JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    /// The engine sent a message that violates the extension protocol.
    #[error("plugin protocol violation: {0}")]
    Protocol(String),
    /// A declared tool is invalid.
    #[error("invalid tool declaration: {0}")]
    InvalidTool(String),
    /// No unexpired context grant is available for the requested session.
    #[error("no valid emit context is available for session {0}")]
    ContextUnavailable(cookie_agent_protocol::SessionId),
    /// Publishing to the requested target was not enabled on the server builder.
    #[error("{0} publishing is not enabled")]
    PublishingNotEnabled(&'static str),
    /// Producer messaging was not enabled on the server builder.
    #[error("producer messaging is not enabled")]
    ProducerMessagingNotEnabled,
    /// The engine rejected a plugin request.
    #[error("engine rejected plugin request ({code}): {message}")]
    EngineRequest {
        /// The JSON-RPC error code returned by the engine.
        code: i32,
        /// The JSON-RPC error message returned by the engine.
        message: String,
        /// Optional structured error data returned by the engine.
        data: Option<Value>,
    },
    /// A registered handler panicked.
    #[error("plugin handler panicked")]
    HandlerPanic,
    /// The plugin server stopped before an operation completed.
    #[error("plugin transport closed")]
    TransportClosed,
}

/// A tool handler's failure: the call fails with `message`.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ToolFailure {
    /// The human-readable error message.
    pub message: String,
}

impl ToolFailure {
    /// A tool failure with `message`.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
