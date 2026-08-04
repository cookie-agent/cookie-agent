use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use cookie_agent_protocol::{
    OutputStream, PersistedToolResult as ToolResult, PreparedOperationIdentity, RunId, SessionId,
    Sha256Digest, ToolAttachment, ToolCallId, ToolCallPresentation,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    events::OutputHub,
    runtime::tool_execution::{safe_tool_presentation, validate_attachment},
    runtime::{ArtifactStore, OutputCapture, ToolCallFailureCode},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionToolContext {
    pub session: SessionId,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: Value,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolProgress {
    pub tool_call_id: ToolCallId,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct ProgressSink {
    sender: mpsc::Sender<ToolProgress>,
    output: OutputHub,
    capture: Option<OutputCapture>,
}
impl ProgressSink {
    #[must_use]
    pub fn new(sender: mpsc::Sender<ToolProgress>, output: OutputHub) -> Self {
        Self {
            sender,
            output,
            capture: None,
        }
    }
    pub(crate) fn with_capture(
        sender: mpsc::Sender<ToolProgress>,
        output: OutputHub,
        capture: OutputCapture,
    ) -> Self {
        Self {
            sender,
            output,
            capture: Some(capture),
        }
    }
    pub async fn send(&self, progress: ToolProgress) -> Result<(), ToolError> {
        self.sender
            .send(progress)
            .await
            .map_err(|_| ToolError::ProgressSinkClosed)
    }
    pub fn output(&self, stream: OutputStream, data: &[u8]) {
        self.output.emit(stream, data);
        if let Some(capture) = &self.capture {
            capture.write(stream, data);
        }
    }
}

#[derive(Debug)]
pub struct ToolStdin {
    receiver: mpsc::Receiver<StdinWrite>,
}
impl ToolStdin {
    /// Builds the sender/receiver pair used by interactive tool tests and by
    /// the engine's per-call stdin registry.
    #[must_use]
    pub fn channel(capacity: usize) -> (mpsc::Sender<StdinWrite>, Self) {
        let (sender, receiver) = mpsc::channel(capacity);
        (sender, Self { receiver })
    }

    #[must_use]
    pub fn from_receiver(receiver: mpsc::Receiver<StdinWrite>) -> Self {
        Self { receiver }
    }

    pub async fn recv(&mut self) -> Option<StdinWrite> {
        self.receiver.recv().await
    }
}
#[derive(Clone, Debug)]
pub struct StdinWrite {
    pub data: Vec<u8>,
    pub eof: bool,
}

#[derive(Clone, Debug)]
pub struct ToolPreparationContext {
    pub session: SessionId,
    pub run: RunId,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
}

#[derive(Debug)]
pub struct ToolExecutionContext {
    pub session: SessionId,
    pub run: RunId,
    pub progress: ProgressSink,
    pub cancellation: CancellationToken,
    pub stdin: Option<ToolStdin>,
    pub(crate) artifacts: Arc<ArtifactStore>,
}

impl ToolExecutionContext {
    pub fn retain_attachment(
        &self,
        mime_type: impl Into<String>,
        filename: Option<String>,
        bytes: &[u8],
    ) -> Result<ToolAttachment, ToolError> {
        let mime_type = mime_type.into();
        let path = filename.as_deref().map_or_else(PathBuf::new, PathBuf::from);
        validate_attachment(&mime_type, &path, bytes)?;
        let (reference, sha256) = self
            .artifacts
            .retain(bytes)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        Ok(ToolAttachment {
            mime_type: cookie_agent_protocol::MimeType::new(mime_type)
                .map_err(|error| ToolError::execution(error.to_string()))?,
            filename,
            byte_length: bytes.len() as u64,
            sha256: Sha256Digest::new(sha256)
                .map_err(|error| ToolError::execution(error.to_string()))?,
            reference,
        })
    }
}
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
    pub fn execution(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    #[must_use]
    pub(crate) const fn code(&self) -> ToolCallFailureCode {
        match self {
            Self::ProgressSinkClosed | Self::Failed(_) => ToolCallFailureCode::ExecutionFailed,
            Self::OperationChanged(_) => ToolCallFailureCode::OperationChanged,
            Self::UnsupportedSecurity(_) | Self::ResourceLimit(_) => {
                ToolCallFailureCode::ExecutionFailed
            }
            Self::UnsupportedPlatform(_) => ToolCallFailureCode::UnsupportedPlatform,
        }
    }

    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::ProgressSinkClosed => "tool progress sink closed".into(),
            Self::Failed(message)
            | Self::OperationChanged(message)
            | Self::UnsupportedSecurity(message)
            | Self::UnsupportedPlatform(message)
            | Self::ResourceLimit(message) => message.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PreparedSerializationKey(Vec<u8>);

impl PreparedSerializationKey {
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }
}

#[async_trait]
pub trait PreparedExecutor: Send + Sync {
    async fn revalidate(&self) -> Result<(), ToolError>;

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolResult, ToolError>;
}

pub struct PreparedTool {
    pub(crate) operation: PreparedOperationIdentity,
    pub(crate) policy_labels: Vec<String>,
    pub(crate) serialization_key: Option<PreparedSerializationKey>,
    pub(crate) executor: PreparedExecutorCell,
}

pub(crate) type PreparedExecutorCell = Arc<tokio::sync::Mutex<Option<Box<dyn PreparedExecutor>>>>;

impl PreparedTool {
    #[must_use]
    pub fn new(
        operation: PreparedOperationIdentity,
        serialization_key: Option<PreparedSerializationKey>,
        executor: Box<dyn PreparedExecutor>,
    ) -> Self {
        let policy_labels = operation
            .resources()
            .iter()
            .map(|resource| resource.canonical.as_str().to_owned())
            .collect();
        Self {
            operation,
            policy_labels,
            serialization_key,
            executor: Arc::new(tokio::sync::Mutex::new(Some(executor))),
        }
    }

    #[must_use]
    pub const fn operation(&self) -> &PreparedOperationIdentity {
        &self.operation
    }

    pub fn with_policy_labels(mut self, labels: Vec<String>) -> Result<Self, ToolError> {
        if labels.len() != self.operation.resources().len() {
            return Err(ToolError::execution(
                "prepared policy labels do not cover every resource",
            ));
        }
        for (resource, label) in self.operation.resources().iter().zip(&labels) {
            let expected = Sha256Digest::of_bytes(label.as_bytes());
            if resource
                .canonical
                .as_str()
                .rsplit_once(':')
                .is_none_or(|(_, digest)| digest != expected.as_str())
            {
                return Err(ToolError::execution(
                    "prepared policy label does not match its immutable resource identity",
                ));
            }
        }
        self.policy_labels = labels;
        Ok(self)
    }

    #[must_use]
    pub fn policy_labels(&self) -> &[String] {
        &self.policy_labels
    }
}

#[async_trait]
pub trait ToolProvider: Send + Sync {
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError>;
    fn presentation(&self, call: &ToolCall) -> ToolCallPresentation {
        safe_tool_presentation(call)
    }
    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError>;
}
