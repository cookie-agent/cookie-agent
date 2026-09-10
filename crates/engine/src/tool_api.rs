use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use cookie_agent_protocol::{
    AdaptorId, AgentId, ModelCapabilities, ModelKey, PersistedToolResult as ToolResult,
    PreparedOperationIdentity, RunId, SessionId, Sha256Digest, ToolAttachment, ToolCallId,
    ToolCallPresentation,
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
    #[serde(skip)]
    prompt_delegate_targets: Option<Vec<(AgentId, String)>>,
}

impl SessionToolContext {
    #[must_use]
    pub const fn new(session: SessionId) -> Self {
        Self {
            session,
            prompt_delegate_targets: None,
        }
    }

    pub(crate) fn for_prompt_composition(
        session: SessionId,
        delegate_targets: Vec<(AgentId, String)>,
    ) -> Self {
        Self {
            session,
            prompt_delegate_targets: Some(delegate_targets),
        }
    }

    /// Delegate targets frozen for the run currently composing its prompt.
    pub fn prompt_delegate_targets(
        &self,
    ) -> Option<impl ExactSizeIterator<Item = (&AgentId, &str)>> {
        self.prompt_delegate_targets.as_ref().map(|targets| {
            targets
                .iter()
                .map(|(id, description)| (id, description.as_str()))
        })
    }
}

/// One labeled system-prompt section contributed by a tool provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptSection {
    /// Short human-meaningful title used to identify validation failures.
    pub title: String,
    /// Markdown-like body rendered inside the provider provenance wrapper.
    pub body: String,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub permission_name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default)]
    pub concurrency: ToolConcurrency,
    #[serde(default)]
    pub result_truncation: ToolResultTruncationPolicy,
    #[serde(default)]
    pub output: cookie_agent_protocol::ToolOutputDeclaration,
}

/// Completion supplies output once, or explicitly finalizes already accepted deltas.
#[derive(Clone, Debug)]
pub struct ToolCompletion {
    pub output: cookie_agent_protocol::ToolCompletionOutput,
    pub result: ToolResult,
    pub failed: bool,
}

impl ToolCompletion {
    #[cfg(any(test, feature = "test-support"))]
    pub fn into_result_for_test(self) -> Result<ToolResult, ToolError> {
        let mut result = self.result;
        match self.output {
            cookie_agent_protocol::ToolCompletionOutput::Single { text } => result.output = text,
            _ => {
                return Err(ToolError::execution(
                    "streamed tests must use runtime capture",
                ));
            }
        }
        Ok(result)
    }

    pub fn single(mut result: ToolResult) -> Self {
        let text = std::mem::take(&mut result.output);
        if result.display.is_none() {
            result.display = Some(bounded_tool_display(&text));
        }
        Self {
            output: cookie_agent_protocol::ToolCompletionOutput::Single { text },
            result,
            failed: false,
        }
    }

    pub fn streamed(mut result: ToolResult) -> Self {
        if result.display.is_none() {
            result.display = Some(String::new());
        }
        Self {
            output: cookie_agent_protocol::ToolCompletionOutput::Streamed,
            result,
            failed: false,
        }
    }
}

pub(crate) fn bounded_tool_display(text: &str) -> String {
    sanitize_tool_display(text, cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES)
}

pub(crate) fn sanitize_tool_display(text: &str, maximum: usize) -> String {
    let mut display = String::new();
    for character in text.chars() {
        let character = if character.is_control() && !matches!(character, '\n' | '\t') {
            ' '
        } else {
            character
        };
        if display.len() + character.len_utf8() > maximum {
            break;
        }
        display.push(character);
    }
    display
}

/// Declares whether calls to a tool may overlap with sibling calls from one model turn.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolConcurrency {
    #[default]
    Exclusive,
    Parallel,
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
    pub display: Option<String>,
    #[serde(default)]
    pub output: Vec<cookie_agent_protocol::ToolOutputChunk>,
}

#[derive(Clone, Debug)]
pub struct ProgressSink {
    sender: mpsc::Sender<ToolProgress>,
    output: OutputHub,
    capture: Option<OutputCapture>,
    display_used: Arc<std::sync::Mutex<usize>>,
}
impl ProgressSink {
    #[cfg(feature = "test-support")]
    pub async fn for_test(
        sender: mpsc::Sender<ToolProgress>,
        output: OutputHub,
        directory: PathBuf,
        declaration: cookie_agent_protocol::ToolOutputDeclaration,
    ) -> Result<Self, ToolError> {
        let store =
            ArtifactStore::open(directory).map_err(|e| ToolError::execution(e.to_string()))?;
        let capture = OutputCapture::new(store, declaration, 2_000, 16 * 1024).await?;
        Ok(Self::with_capture(sender, output, capture))
    }

    #[must_use]
    pub fn new(sender: mpsc::Sender<ToolProgress>, output: OutputHub) -> Self {
        Self {
            sender,
            output,
            capture: None,
            display_used: Arc::new(std::sync::Mutex::new(0)),
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
            display_used: Arc::new(std::sync::Mutex::new(0)),
        }
    }
    pub async fn send(&self, mut progress: ToolProgress) -> Result<(), ToolError> {
        if !progress.output.is_empty() && self.capture.is_none() {
            return Err(ToolError::execution("output capture is unavailable"));
        }
        if progress.message.len() > cookie_agent_protocol::SafeDisplayText::MAX_BYTES
            || progress.display.as_ref().is_some_and(|display| {
                display.len() > cookie_agent_protocol::SafeDisplayText::MAX_BYTES
            })
        {
            return Err(ToolError::resource_limit(
                "tool display delta exceeds the display event bound",
            ));
        }
        if progress.output.len() > cookie_agent_protocol::MAX_TOOL_STREAMS
            || progress.output.iter().fold(0_usize, |total, chunk| {
                total.saturating_add(chunk.text.len())
            }) > cookie_agent_protocol::MAX_TOOL_DELTA_BYTES
        {
            return Err(ToolError::resource_limit(
                "tool output delta exceeds 64 KiB or 8 chunks",
            ));
        }
        for chunk in &progress.output {
            if let Some(name) = &chunk.stream {
                cookie_agent_protocol::validate_tool_stream_name(name)
                    .map_err(ToolError::execution)?;
            }
        }
        {
            let mut used = self.display_used.lock().unwrap_or_else(|p| p.into_inner());
            let remaining = cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES.saturating_sub(*used);
            progress.message = sanitize_tool_display(&progress.message, remaining);
            *used += progress.message.len();
            progress.display = progress
                .display
                .as_deref()
                .map(|text| {
                    sanitize_tool_display(
                        text,
                        cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES.saturating_sub(*used),
                    )
                })
                .filter(|text| !text.is_empty());
            *used += progress.display.as_ref().map_or(0, String::len);
        }
        let permit = if progress.message.is_empty() && progress.display.is_none() {
            None
        } else {
            Some(
                self.sender
                    .clone()
                    .reserve_owned()
                    .await
                    .map_err(|_| ToolError::ProgressSinkClosed)?,
            )
        };
        if let Some(capture) = &self.capture {
            return capture.emit(progress, self.output.clone(), permit).await;
        }
        if let Some(permit) = permit {
            permit.send(progress);
        }
        Ok(())
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
    /// Exact Oven adapter family, preserving compatible-family distinctions.
    pub adapter_family: cookie_agent_models::adapters::OvenAdapterFamily,
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
    pub async fn read_artifact(
        &self,
        path: &str,
        offset: u64,
        limit: u64,
    ) -> Result<crate::ArtifactReadPage, ToolError> {
        crate::runtime::read_artifact_async(self.artifacts.clone(), path, offset, limit).await
    }

    #[cfg(feature = "test-support")]
    pub fn for_test(
        artifact_directory: impl Into<PathBuf>,
        turn_context: Arc<TurnAgentContext>,
    ) -> Result<Self, ToolError> {
        let call_id = ToolCallId::new_v7();
        let (progress, _receiver) = mpsc::channel(1);
        Ok(Self {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            progress: ProgressSink::new(progress, OutputHub::new(call_id, 1024)),
            cancellation: CancellationToken::new(),
            stdin: None,
            turn_context,
            artifacts: ArtifactStore::open(artifact_directory.into())
                .map_err(|error| ToolError::execution(error.to_string()))?,
        })
    }

    pub fn retain_attachment(
        &self,
        mime_type: impl Into<String>,
        filename: Option<String>,
        bytes: &[u8],
    ) -> Result<ToolAttachment, ToolError> {
        let mime_type = mime_type.into();
        let path = filename.as_deref().map_or_else(PathBuf::new, PathBuf::from);
        validate_attachment(&mime_type, &path, bytes)?;
        self.retain_validated_attachment(mime_type, filename, bytes)
    }

    pub fn retain_validated_attachment(
        &self,
        mime_type: impl Into<String>,
        filename: Option<String>,
        bytes: &[u8],
    ) -> Result<ToolAttachment, ToolError> {
        let mime_type = mime_type.into();
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
        Self::Failed(cookie_agent_protocol::diagnostics::detail(&message.into()).to_string())
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
            Self::InvalidUrl(_) => ToolCallFailureCode::InvalidUrl,
            Self::PermissionDenied(_) => ToolCallFailureCode::PermissionDenied,
            Self::RedirectError(_) => ToolCallFailureCode::RedirectError,
            Self::Timeout(_) => ToolCallFailureCode::Timeout,
            Self::TransportError(_) => ToolCallFailureCode::TransportError,
            Self::UnsupportedContentType(_) => ToolCallFailureCode::UnsupportedContentType,
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
            | Self::InvalidUrl(message)
            | Self::PermissionDenied(message)
            | Self::RedirectError(message)
            | Self::Timeout(message)
            | Self::TransportError(message)
            | Self::UnsupportedContentType(message)
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
    ) -> Result<crate::ToolCompletion, ToolError>;
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

    #[cfg(feature = "test-support")]
    pub async fn execute_for_test(
        self,
        context: ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let executor = self
            .executor
            .lock()
            .await
            .take()
            .ok_or_else(|| ToolError::execution("prepared executor was already consumed"))?;
        executor.execute(context).await?.into_result_for_test()
    }
}

#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// Stable engine-facing identity used for system-prompt provenance labels.
    fn provider_id(&self) -> &'static str;

    /// Sections resolved once at run admission and frozen into the agent snapshot.
    fn prompt_sections(&self, _ctx: &SessionToolContext) -> Result<Vec<PromptSection>, ToolError> {
        Ok(Vec::new())
    }

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

    use super::{
        PreparedExecutor, PreparedTool, ToolConcurrency, ToolError, ToolExecutionContext, ToolSpec,
        async_trait,
    };

    struct NoopExecutor;

    #[async_trait]
    impl PreparedExecutor for NoopExecutor {
        async fn revalidate(&self) -> Result<(), ToolError> {
            Ok(())
        }

        async fn execute(
            self: Box<Self>,
            _context: ToolExecutionContext,
        ) -> Result<crate::ToolCompletion, ToolError> {
            let result: Result<ToolResult, ToolError> =
                async move { unreachable!("constructor validation test never executes") }.await;
            result.map(crate::ToolCompletion::single)
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
    fn tool_concurrency_defaults_to_exclusive_when_omitted() {
        let spec: ToolSpec = serde_json::from_value(serde_json::json!({
            "name":"external_tool",
            "permission_name":"external",
            "description":"External tool",
            "parameters":{"type":"object"}
        }))
        .expect("tool spec without concurrency");
        assert_eq!(spec.concurrency, ToolConcurrency::Exclusive);
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
