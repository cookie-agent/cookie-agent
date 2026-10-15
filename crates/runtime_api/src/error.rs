use thiserror::Error;

/// Why preparing or executing a tool call failed.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool progress sink closed")]
    ProgressSinkClosed,
    #[error("tool failed: {0}")]
    Failed(String),
    #[error("prepared operation changed: {0}")]
    OperationChanged(String),
    #[error("unsupported prepared-operation security: {0}")]
    UnsupportedSecurity(String),
    #[error("prepared operation is unsupported on this platform: {0}")]
    UnsupportedPlatform(String),
    #[error("prepared capability resource limit exceeded: {0}")]
    ResourceLimit(String),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("redirect error: {0}")]
    RedirectError(String),
    #[error("request timed out: {0}")]
    Timeout(String),
    #[error("transport error: {0}")]
    TransportError(String),
    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),
    /// The runtime or MCP server that owns the tool is reconnecting or
    /// gone; the call was not attempted or did not complete.
    #[error("runtime unavailable: {0}")]
    RuntimeUnavailable(String),
}

impl ToolError {
    #[must_use]
    pub fn operation_changed(message: impl Into<String>) -> Self {
        Self::OperationChanged(message.into())
    }

    #[must_use]
    pub fn unsupported_security(message: impl Into<String>) -> Self {
        Self::UnsupportedSecurity(message.into())
    }

    #[must_use]
    pub fn unsupported_platform(message: impl Into<String>) -> Self {
        Self::UnsupportedPlatform(message.into())
    }

    #[must_use]
    pub fn resource_limit(message: impl Into<String>) -> Self {
        Self::ResourceLimit(message.into())
    }

    #[must_use]
    pub fn runtime_unavailable(message: impl Into<String>) -> Self {
        Self::RuntimeUnavailable(message.into())
    }

    #[must_use]
    pub fn execution(message: impl Into<String>) -> Self {
        Self::Failed(cookie_agent_protocol::diagnostics::detail(&message.into()).to_string())
    }

    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::ProgressSinkClosed => "tool progress sink closed".into(),
            Self::Failed(message)
            | Self::OperationChanged(message)
            | Self::UnsupportedSecurity(message)
            | Self::UnsupportedPlatform(message)
            | Self::InvalidUrl(message)
            | Self::PermissionDenied(message)
            | Self::RedirectError(message)
            | Self::Timeout(message)
            | Self::TransportError(message)
            | Self::UnsupportedContentType(message)
            | Self::RuntimeUnavailable(message)
            | Self::ResourceLimit(message) => message.clone(),
        }
    }
}
