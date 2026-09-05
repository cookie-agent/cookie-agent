use cookie_agent_protocol::JsonRpcError;
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
    /// The SDK's bounded pending-request table is full.
    #[error("too many plugin requests are awaiting engine replies")]
    TooManyPendingRequests,
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

/// A tool-handler failure returned as a JSON-RPC error.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ToolFailure {
    /// The JSON-RPC error code.
    pub code: i32,
    /// The human-readable error message.
    pub message: String,
    /// Optional structured error data.
    pub data: Option<Value>,
}

impl ToolFailure {
    /// Creates a plugin-defined tool failure using the server-error code.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: message.into(),
            data: None,
        }
    }

    /// Creates a tool failure with an explicit JSON-RPC error code.
    #[must_use]
    pub fn with_code(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Attaches structured JSON data to this failure.
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub(crate) fn into_rpc(self) -> JsonRpcError {
        JsonRpcError {
            code: self.code,
            message: self.message,
            data: self.data,
        }
    }
}
