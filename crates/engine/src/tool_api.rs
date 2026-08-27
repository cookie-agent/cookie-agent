use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use cookie_agent_protocol::{
    AdaptorId, AgentId, ModelCapabilities, ModelKey, OutputStream,
    PersistedToolResult as ToolResult, PreparedOperationIdentity, RunId, SessionId, Sha256Digest,
    ToolAttachment, ToolCallId, ToolCallPresentation,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    events::OutputHub,
    runtime::tool_execution::validate_attachment,
    runtime::{ArtifactStore, OutputCapture, ToolCallFailureCode},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionToolContext {
    pub session: SessionId,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub permission_name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default)]
    pub result_truncation: ToolResultTruncationPolicy,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultTruncationPolicy {
    #[default]
    Bounded,
    OptOut,
}

pub(crate) const UNSCOPED_PERMISSION_RESOURCE_DISPLAY: &str = "<permission-name-only>";
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
    pub output_chunk: Option<String>,
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

/// Immutable harness context captured for one tool preparation/execution batch.
///
/// This is tool-facing but harness-private metadata. Tool providers must not forward it to
/// external systems, including MCP wrappers. Compaction rehydration uses the context of the owner
/// policy and model binding that triggered the checkpoint.
#[derive(Debug)]
pub struct TurnAgentContext {
    /// Agent that owns the tool call.
    pub agent: AgentId,
    /// Exact model selected for this turn.
    pub model: ModelKey,
    /// Frozen wire adapter family used to deliver tool results.
    pub adapter: AdaptorId,
    /// Public capabilities of the exact model binding that produced the tool call.
    pub capabilities: ModelCapabilities,
}

#[derive(Clone, Debug)]
pub struct ToolPreparationContext {
    pub session: SessionId,
    pub run: RunId,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
    /// Static agent/model context shared with execution for this batch.
    pub turn_context: Arc<TurnAgentContext>,
}

#[derive(Debug)]
pub struct ToolExecutionContext {
    pub session: SessionId,
    pub run: RunId,
    pub progress: ProgressSink,
    pub cancellation: CancellationToken,
    pub stdin: Option<ToolStdin>,
    /// Static agent/model context shared with preparation for this batch.
    pub turn_context: Arc<TurnAgentContext>,
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
    pub(crate) policy_labels: Vec<Option<String>>,
    pub(crate) normalized_arguments: serde_json::Value,
    pub(crate) serialization_key: Option<PreparedSerializationKey>,
    pub(crate) executor: PreparedExecutorCell,
}

pub(crate) type PreparedExecutorCell = Arc<tokio::sync::Mutex<Option<Box<dyn PreparedExecutor>>>>;

impl PreparedTool {
    pub fn new(
        operation: PreparedOperationIdentity,
        normalized_arguments: serde_json::Value,
        serialization_key: Option<PreparedSerializationKey>,
        executor: Box<dyn PreparedExecutor>,
    ) -> Result<Self, ToolError> {
        if operation.resources().is_empty() {
            return Err(ToolError::execution(
                "prepared tool requires at least one permission resource",
            ));
        }
        if normalized_arguments.is_null() {
            return Err(ToolError::execution(
                "prepared normalized arguments must not be null",
            ));
        }
        let policy_labels = operation
            .resources()
            .iter()
            .map(|resource| Some(resource.canonical.as_str().to_owned()))
            .collect();
        Ok(Self {
            operation,
            policy_labels,
            normalized_arguments,
            serialization_key,
            executor: Arc::new(tokio::sync::Mutex::new(Some(executor))),
        })
    }

    #[must_use]
    pub const fn operation(&self) -> &PreparedOperationIdentity {
        &self.operation
    }

    pub fn with_policy_labels(mut self, labels: Vec<String>) -> Result<Self, ToolError> {
        if labels.is_empty() || labels.len() != self.operation.resources().len() {
            return Err(ToolError::execution(
                "prepared policy labels must cover every resource",
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
        self.policy_labels = labels.into_iter().map(Some).collect();
        Ok(self)
    }

    pub fn with_permission_resource(mut self, resource: Option<String>) -> Result<Self, ToolError> {
        if resource.as_ref().is_some_and(String::is_empty) {
            return Err(ToolError::execution(
                "permission resource must not be empty",
            ));
        }
        self.policy_labels.fill(resource);
        Ok(self)
    }

    #[must_use]
    pub const fn normalized_arguments(&self) -> &serde_json::Value {
        &self.normalized_arguments
    }

    #[must_use]
    pub fn policy_labels(&self) -> &[Option<String>] {
        &self.policy_labels
    }
}

#[async_trait]
pub trait ToolProvider: Send + Sync {
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError>;
    /// Claims a currently undiscovered dynamic tool, allowing preparation to make it available.
    fn permission_for_unlisted_tool(
        &self,
        _tool_name: &str,
    ) -> Result<Option<&'static str>, ToolError> {
        Ok(None)
    }
    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError>
    where
        Self: Sized;
    fn get_permission_resource(
        &self,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<(&'static str, Option<String>), ToolError>;
    fn get_display_argument(&self, name: &str, arguments: &Value) -> Result<String, ToolError>;

    fn presentation(&self, call: &ToolCall) -> ToolCallPresentation {
        match self.get_display_argument(&call.name, &call.arguments) {
            Ok(display) => crate::runtime::tool_execution::tool_presentation(&call.name, &display),
            Err(_) => crate::runtime::tool_execution::tool_title_only(&call.name),
        }
    }
    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError>;
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        PersistedToolResult as ToolResult, PreparedOperationIdentity, Sha256Digest,
    };

    use super::{PreparedExecutor, PreparedTool, ToolError, ToolExecutionContext, async_trait};

    struct NoopExecutor;

    #[async_trait]
    impl PreparedExecutor for NoopExecutor {
        async fn revalidate(&self) -> Result<(), ToolError> {
            Ok(())
        }

        async fn execute(
            self: Box<Self>,
            _context: ToolExecutionContext,
        ) -> Result<ToolResult, ToolError> {
            unreachable!("constructor validation test never executes")
        }
    }

    fn operation() -> PreparedOperationIdentity {
        let label = "command:test";
        PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"arguments"),
            vec![cookie_agent_protocol::ApprovalCapability {
                action: cookie_agent_protocol::PermissionAction::Bash,
                operation: cookie_agent_protocol::PreparedCapabilityOperation::new("bash:execute")
                    .expect("capability operation"),
            }],
            vec![cookie_agent_protocol::PreparedApprovalResource {
                capability: cookie_agent_protocol::PermissionAction::Bash,
                canonical: cookie_agent_protocol::PreparedResourceIdentity::new(format!(
                    "command:{}",
                    Sha256Digest::of_bytes(label.as_bytes())
                ))
                .expect("resource identity"),
                binding_digest:
                    cookie_agent_protocol::PreparedResourceDigest::from_canonical_binding_bytes(
                        label.as_bytes(),
                    ),
                binding_lifetime: cookie_agent_protocol::PreparedBindingLifetime::ProcessLocal,
                boundary: cookie_agent_protocol::ApprovalBoundary::Exact,
                source: cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"context"),
        )
        .expect("prepared operation")
    }

    #[test]
    fn prepared_tool_rejects_null_normalized_arguments() {
        let error = match PreparedTool::new(
            operation(),
            serde_json::Value::Null,
            None,
            Box::new(NoopExecutor),
        ) {
            Ok(_) => panic!("null normalized arguments must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("must not be null"));
    }

    #[test]
    fn absent_permission_resource_sets_the_loose_policy_marker() {
        let prepared = PreparedTool::new(
            operation(),
            serde_json::json!({}),
            None,
            Box::new(NoopExecutor),
        )
        .expect("prepared tool")
        .with_permission_resource(None)
        .expect("loose permission resource");
        assert_eq!(prepared.policy_labels(), [None]);
    }
}
