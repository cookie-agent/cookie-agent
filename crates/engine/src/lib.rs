//! The transport-free single-conversation cookie agent runtime.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    future::Future,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[cfg(test)]
use std::sync::mpsc as std_mpsc;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_config::{AgentRegistry, LoadedConfiguration};
use cookie_agent_models::ModelSetManager;
use cookie_agent_protocol::{
    AgentDescriptor, AgentId, AgentListResult, AgentMode, ApprovalConstraints,
    ApprovalDecisionSource, ApprovalEvaluation, ApprovalFinalDecision, ApprovalFinalOutcome,
    ApprovalId, ApprovalInternalDecision, ApprovalInternalDecisionKind, ApprovalListResult,
    ApprovalReasonCode, ApprovalRecord, ApprovalRequest, ApprovalRespondErrorCode,
    ApprovalRespondParams, ApprovalRespondResult, ApprovalStatus, ApprovalTrigger,
    ApprovalUserDecision, ArtifactReference, ChildSummary, ContextCheckpoint,
    ContextCheckpointBoundaries, ContextCheckpointBudgets, ContextCheckpointCommit,
    EventPayload as Event, EventSubscriptionMessage, EventsSubscribeResult, InternalAgentBackend,
    InternalAgentFailure, InternalAgentInvocationId, InternalAgentKind, InternalAgentRunId,
    InternalSummaryCheckpoint, InvocationId, NativeContextArtifact, OperationFingerprint,
    OutputStream, PersistedAssistantPart, PersistedModelTurn, PersistedToolResult as ToolResult,
    PreparedOperationIdentity, RunCancelResult, RunId, RunSelection, RunStartParams,
    RunStartResult, RunSteerResult, RunToolStdinParams, RunToolStdinResult, SafeCode,
    SafeDisplayText, SafeErrorMessage, SafeInternalAgentCall, SafeInternalAgentResult,
    SafeToolError, SessionId, SessionMeta, SessionOrigin, SessionRenameChange, SessionRenameParams,
    SessionRenameResult, SessionStatus, SessionTitle, SessionTitleChange, Sha256Digest,
    StoredEvent, SummaryByteLimit, ToolAttachment, ToolCallId, ToolCallPresentation, ToolCallStart,
    ToolCallTermination, ToolOutputTruncation, ToolTerminationOutcome, TreeApprovalGrant,
};
use futures_util::StreamExt;
use oven_sdk::{
    CompactionCapability, CompactionRequest, JsonSchema, ModelError, Request as ModelRequest,
    ToolDefinition,
};
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, fchmod, fsync, openat, renameat, statat, unlinkat,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub mod actor;
#[cfg(test)]
mod delegation_tests;
pub mod events;
pub mod grant_journal;
pub mod journal;
mod media;
mod model_bridge;
mod model_history;
mod model_policy;
pub mod permissions;
mod policy;
#[cfg(test)]
mod prepared_tests;
#[cfg(test)]
mod responses_fixture_tests;
pub mod run;
#[cfg(test)]
mod run_selection_tests;
#[cfg(test)]
mod security_tests;
pub mod session;
#[cfg(test)]
mod test_support;

pub use cookie_agent_protocol::PersistedToolResult;

use actor::SessionActor;
use events::{EventLogError, OutputHub};
use grant_journal::{GrantInvalidationJournal, GrantJournalError};
use journal::{DelegationJournal, JournalError};
pub use media::approved_media_type;
use model_bridge::{AbortBridge, TurnAccumulator};
use model_history::{
    assemble_model_context, persist_turn, replay_decisions_with_preflight, wire_model,
};
use model_policy::{ErrorPolicy, classify as classify_model_error, summary as model_error_summary};
use permissions::{ApprovalStore, PermissionPipeline};
use policy::{
    FrozenRunPolicy, freeze_delegated_agent_policy, freeze_root_agent_policy,
    policy_for_session_selection, policy_from_snapshot, resolve_agent,
};
use session::{SessionError, SessionStore};

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

/// Immutable arguments for one delegate-tool invocation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DelegateInvocation {
    pub parent_session_id: SessionId,
    pub parent_run_id: RunId,
    pub parent_tool_call_id: ToolCallId,
    pub agent: AgentId,
    pub task: String,
    pub context: Vec<Value>,
    pub success_criteria: Vec<String>,
    pub expected_output: Value,
}

/// Stable child identity returned to the delegate tool provider.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DelegateHandle {
    pub invocation_id: InvocationId,
    pub child_session_id: SessionId,
    pub child_run_id: RunId,
}

/// A delegate wait that cancels its child if its consumer abandons the wait.
pub struct DelegateAwait {
    future: Pin<Box<dyn Future<Output = Result<ToolResult, EngineError>> + Send>>,
    engine: Engine,
    runtime: Option<tokio::runtime::Handle>,
    handle: DelegateHandle,
    completed: bool,
}

impl Future for DelegateAwait {
    type Output = Result<ToolResult, EngineError>;

    fn poll(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let result = self.future.as_mut().poll(context);
        if result.is_ready() {
            self.completed = true;
        }
        result
    }
}

impl Drop for DelegateAwait {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        // Delegate waits are created and polled from the Tokio tool task. If that
        // task is dropped, retain the cancellation in a detached runtime task.
        // This closes the abandoned-tool-call child-run leak.
        if let Some(runtime) = self
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        {
            let engine = self.engine.clone();
            let cancel_engine = engine.clone();
            let handle = self.handle;
            let _ = engine.spawn_admission_task(&runtime, async move {
                let _ = cancel_engine.cancel_delegate(handle).await;
            });
        }
    }
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
    fn with_capture(
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
    artifacts: Arc<ArtifactStore>,
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
    operation: PreparedOperationIdentity,
    policy_labels: Vec<String>,
    serialization_key: Option<PreparedSerializationKey>,
    executor: PreparedExecutorCell,
}

type PreparedExecutorCell = Arc<tokio::sync::Mutex<Option<Box<dyn PreparedExecutor>>>>;

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

#[derive(Clone)]
pub struct EngineOptions {
    pub data_dir: PathBuf,
    pub cwd: PathBuf,
    pub config: LoadedConfiguration,
    pub model_manager: Arc<ModelSetManager>,
    pub tools: Vec<Arc<dyn ToolProvider>>,
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Event(#[from] EventLogError),
    #[error(transparent)]
    GrantJournal(#[from] GrantJournalError),
    #[error("tool output storage error: {0}")]
    ToolOutput(#[from] std::io::Error),
    #[error("configuration error: {0}")]
    Config(#[source] Box<cookie_agent_config::ConfigError>),
    #[error("agent `{0}` is not eligible in this session origin")]
    IneligibleAgent(AgentId),
    #[error("agent `{0}` is disabled")]
    DisabledAgent(AgentId),
    #[error("run {0} not found")]
    MissingRun(RunId),
    #[error("session {0} is already running")]
    SessionRunning(SessionId),
    #[error("client run id conflicts with durable run parameters")]
    RunIdempotencyConflict,
    #[error("tool call is not running or is not interactive")]
    StdinUnavailable,
    #[error("approval `{approval_id}` is not pending for session {session_id}")]
    ApprovalNotPending {
        session_id: SessionId,
        approval_id: ApprovalId,
    },
    #[error("approval response conflicts with durable approval state")]
    ApprovalConflict,
    #[error("approval response was rejected: {0:?}")]
    ApprovalResponse(Box<ApprovalRespondFailure>),
    #[error("client rename id conflicts with a durable rename operation")]
    RenameConflict,
    #[error("invalid base64 stdin: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("model failure: {0}")]
    Model(Box<ModelError>),
    #[error("model history failure: {0}")]
    ModelHistory(#[from] model_history::HistoryError),
    #[error("tool `{0}` is unavailable")]
    MissingTool(String),
    #[error("session actor for {0} is unavailable")]
    MissingActor(SessionId),
    #[error("session actor stopped before replying")]
    ActorStopped,
}

/// Atomic, secret-safe rejection details produced by the serialized approval transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalRespondFailure {
    pub code: ApprovalRespondErrorCode,
    pub session_id: SessionId,
    pub approval_id: ApprovalId,
    pub client_response_id: String,
    pub current_status: Option<ApprovalStatus>,
    pub current_revision: Option<u64>,
    pub current_expires_at: Option<jiff::Timestamp>,
    pub current_operation_fingerprint: Option<OperationFingerprint>,
}

impl From<ModelError> for EngineError {
    fn from(error: ModelError) -> Self {
        Self::Model(Box::new(error))
    }
}

#[derive(Debug)]
struct ActiveRun {
    session: SessionId,
    policy: Arc<FrozenRunPolicy>,
    cancellation: CancellationToken,
    cancelled_committed: Mutex<bool>,
    stdin: Mutex<HashMap<ToolCallId, mpsc::Sender<StdinWrite>>>,
    /// Last persisted event included in the current provider request.
    prompt_seq: AtomicU64,
    fallback_index: AtomicU64,
}

struct AttemptTurn {
    turn: PersistedModelTurn,
    model_turn_seq: u64,
}

#[derive(Clone, Debug)]
struct ApprovalOutcome {
    approved: bool,
    feedback: Option<String>,
}

struct PendingApproval {
    sender: oneshot::Sender<ApprovalOutcome>,
    executor: PreparedExecutorCell,
}

#[derive(Clone, Copy)]
enum PreparedApprovalInvalidation {
    OperationChanged,
    PreparedCapabilityLost,
}

#[derive(Clone, Copy)]
enum ApprovalTerminal {
    Cancelled,
    Expired,
}

enum ApprovalEvaluationTransition {
    Resolved(ApprovalOutcome),
    Escalated(oneshot::Receiver<ApprovalOutcome>),
}

struct PreparedToolCall {
    call: ToolCall,
    presentation: ToolCallPresentation,
    prepared: Result<PreparedTool, ToolFailure>,
}

#[derive(Clone, Debug)]
struct ToolFailure {
    code: ToolCallFailureCode,
    message: String,
}

#[derive(Clone, Copy, Debug)]
enum ToolCallFailureCode {
    ExecutionFailed,
    OperationChanged,
    PreparedCapabilityLost,
    UnsupportedPlatform,
}

impl ToolCallFailureCode {
    fn safe_code(self) -> SafeCode {
        safe_code(match self {
            Self::ExecutionFailed => "execution_failed",
            Self::OperationChanged => "operation_changed",
            Self::PreparedCapabilityLost => "prepared_capability_lost",
            Self::UnsupportedPlatform => "unsupported_platform",
        })
    }
}

impl From<ToolError> for ToolFailure {
    fn from(error: ToolError) -> Self {
        Self {
            code: error.code(),
            message: error.message(),
        }
    }
}

#[derive(Clone, Debug)]
struct FrozenInternalAgentPolicy {
    agent: cookie_agent_protocol::AgentSnapshot,
    models: Vec<cookie_agent_models::FrozenModelBinding>,
    model_snapshot: Option<Arc<cookie_agent_models::ModelSnapshot>>,
    limits: InternalAgentLimits,
}

struct InternalAgentRuntime {
    approval: FrozenInternalAgentPolicy,
    context_compaction: FrozenInternalAgentPolicy,
    session_title: FrozenInternalAgentPolicy,
}

impl InternalAgentRuntime {
    fn freeze() -> Self {
        Self {
            approval: unavailable_internal_policy(30_000, 16_384, 2_048),
            context_compaction: unavailable_internal_policy(30_000, 16_384, 2_048),
            session_title: unavailable_internal_policy(10_000, 4_096, 128),
        }
    }

    fn policy(
        &self,
        kind: InternalAgentKind,
        owner: &FrozenRunPolicy,
        active_suffix: &[cookie_agent_models::FrozenModelBinding],
    ) -> FrozenInternalAgentPolicy {
        let configured = match kind {
            InternalAgentKind::Approval => &self.approval,
            InternalAgentKind::ContextCompaction => &self.context_compaction,
            InternalAgentKind::SessionTitle => &self.session_title,
        };
        inherit_internal_policy(configured, owner, active_suffix)
    }
}

#[derive(Clone, Debug)]
struct InternalAgentLimits {
    max_input_tokens: u64,
    max_output_tokens: u64,
    timeout_ms: u64,
}

struct InternalAgentTextResult {
    invocation_id: InternalAgentInvocationId,
    internal_run_id: InternalAgentRunId,
    text: String,
}

enum PendingTool {
    Prepared(Box<PreparedToolCall>),
    ImmediateFailure(ToolFailure),
}

#[cfg(test)]
struct PromptSnapshotHook {
    reached: Mutex<Option<oneshot::Sender<()>>>,
    release: tokio::sync::Notify,
}

#[cfg(test)]
struct ApprovalEvaluationHook {
    reached: Mutex<Option<oneshot::Sender<()>>>,
    release: tokio::sync::Notify,
}

#[cfg(test)]
struct GapSendHook {
    reached: std_mpsc::Sender<()>,
    release: std_mpsc::Receiver<()>,
}

#[cfg(test)]
struct AdmissionConfirmationHook {
    reached: mpsc::UnboundedSender<()>,
    release: Arc<tokio::sync::Barrier>,
}

#[cfg(test)]
struct AdmissionBlockingHook {
    reached: std_mpsc::Sender<()>,
    release: std_mpsc::Receiver<()>,
}

#[cfg(test)]
#[derive(Clone)]
struct AbandonedSweepHook {
    reached: mpsc::UnboundedSender<()>,
    captured: mpsc::UnboundedSender<Vec<RunId>>,
    release: Arc<tokio::sync::Notify>,
}

#[derive(Debug)]
struct PersistedSubscriber {
    sender: mpsc::Sender<EventSubscriptionMessage>,
}

const SESSION_MAILBOX_CAPACITY: usize = 256;
const PERSISTED_SUBSCRIBER_QUEUE_CAPACITY: usize = 256;
const MAX_PENDING_PREPARED_TOOLS: usize = 64;
/// Semantic revision of the bounded-summary prompt/runtime contract.
/// This is intentionally independent of the protocol and event schema version.
const BOUNDED_SUMMARY_BUILTIN_REVISION: &str =
    "context-compaction.bounded-summary.prompt-runtime.1";
/// Semantic revision of the no-model builtin runtime contract.
/// This is intentionally independent of the protocol and event schema version.
const UNAVAILABLE_BUILTIN_REVISION: &str = "internal-agent.unavailable.runtime.1";

#[allow(clippy::large_enum_variant)]
enum SessionCommand {
    Append {
        run: Option<RunId>,
        event: Event,
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    EnsureToolCallLinked {
        run: RunId,
        tool_call_id: ToolCallId,
        child_session_id: SessionId,
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    Start {
        params: RunStartParams,
        admission: Option<(InvocationId, u64)>,
        reply: oneshot::Sender<Result<RunStartResult, EngineError>>,
    },
    Steer {
        run: RunId,
        input: String,
        reply: oneshot::Sender<Result<RunSteerResult, EngineError>>,
    },
    Cancel {
        run: RunId,
        reply: oneshot::Sender<Result<RunCancelResult, EngineError>>,
    },
    Stdin {
        params: RunToolStdinParams,
        reply: oneshot::Sender<Result<RunToolStdinResult, EngineError>>,
    },
    Subscribe {
        cursor: Option<u64>,
        reply: oneshot::Sender<
            Result<
                (
                    EventsSubscribeResult,
                    mpsc::Receiver<EventSubscriptionMessage>,
                ),
                EngineError,
            >,
        >,
    },
    Resume {
        reply: oneshot::Sender<Result<SessionMeta, EngineError>>,
    },
    Rename {
        params: SessionRenameParams,
        reply: oneshot::Sender<Result<SessionRenameResult, EngineError>>,
    },
    ApprovalRespond {
        params: ApprovalRespondParams,
        reply: oneshot::Sender<Result<ApprovalRespondResult, EngineError>>,
    },
    ApprovalCapabilityInvalid {
        params: ApprovalRespondParams,
        invalidation: PreparedApprovalInvalidation,
        reply: oneshot::Sender<Result<ApprovalRespondResult, EngineError>>,
    },
    ApprovalEvaluationComplete {
        run: RunId,
        request: ApprovalRequest,
        executor: PreparedExecutorCell,
        decision: ApprovalInternalDecisionKind,
        cancelled: bool,
        reply: oneshot::Sender<Result<ApprovalEvaluationTransition, EngineError>>,
    },
    ApprovalTerminal {
        run: RunId,
        approval_id: ApprovalId,
        terminal: ApprovalTerminal,
        reply: oneshot::Sender<Result<bool, EngineError>>,
    },
    ToolResult {
        run: RunId,
        tool_call_id: ToolCallId,
        result: Result<ToolResult, ToolFailure>,
        reply: oneshot::Sender<Result<bool, EngineError>>,
    },
    ResolveDelegateFailureIfPending {
        run: RunId,
        tool_call_id: ToolCallId,
        result: ToolResult,
        reply: oneshot::Sender<Result<bool, EngineError>>,
    },
    ResolveAbandonedDelegateFailureIfPending {
        invocation_id: InvocationId,
        generation: u64,
        run: RunId,
        tool_call_id: ToolCallId,
        result: ToolResult,
        reply: oneshot::Sender<Result<bool, EngineError>>,
    },
    CompleteIfNoSteering {
        run: RunId,
        final_text: Option<String>,
        reply: oneshot::Sender<Result<bool, EngineError>>,
    },
    PromptSnapshot {
        run: RunId,
        reply: oneshot::Sender<Result<Vec<StoredEvent>, EngineError>>,
    },
}

struct Inner {
    config: LoadedConfiguration,
    artifacts: Arc<ArtifactStore>,
    mutation_locks: Mutex<HashMap<PreparedSerializationKey, Arc<tokio::sync::Mutex<()>>>>,
    store: Arc<SessionStore>,
    journal: Arc<DelegationJournal>,
    grant_journal: Arc<GrantInvalidationJournal>,
    model_manager: Arc<ModelSetManager>,
    session_model_snapshots: Mutex<HashMap<SessionId, Arc<cookie_agent_models::ModelSnapshot>>>,
    run_model_snapshots: Mutex<HashMap<RunId, Arc<cookie_agent_models::ModelSnapshot>>>,
    internal_agents: InternalAgentRuntime,
    tools: Mutex<Vec<Arc<dyn ToolProvider>>>,
    approvals: ApprovalStore,
    permissions: PermissionPipeline,
    active: Mutex<HashMap<RunId, Arc<ActiveRun>>>,
    inflight_delegations: Mutex<HashMap<InvocationId, HashMap<u64, InflightDelegation>>>,
    next_admission_generation: AtomicU64,
    subscribers: Mutex<HashMap<SessionId, Vec<PersistedSubscriber>>>,
    actors: Mutex<HashMap<SessionId, SessionActor<SessionCommand>>>,
    output_hubs: Mutex<HashMap<ToolCallId, OutputHub>>,
    finalized_output_hubs: Mutex<VecDeque<ToolCallId>>,
    pending_approvals: Mutex<HashMap<(SessionId, ApprovalId), PendingApproval>>,
    runtime: Option<tokio::runtime::Handle>,
    admission_tasks: Mutex<Vec<JoinHandle<()>>>,
    admission_blocking_tasks: Mutex<Vec<JoinHandle<()>>>,
    admission_tasks_closing: AtomicBool,
    recovery_waiters: Mutex<HashSet<(SessionId, RunId, ToolCallId)>>,
    #[cfg(test)]
    prompt_snapshot_hook: Mutex<Option<Arc<PromptSnapshotHook>>>,
    #[cfg(test)]
    approval_evaluation_hook: Mutex<Option<Arc<ApprovalEvaluationHook>>>,
    #[cfg(test)]
    gap_send_hook: Mutex<Option<GapSendHook>>,
    #[cfg(test)]
    admission_confirmation_hook: Mutex<Option<Arc<AdmissionConfirmationHook>>>,
    #[cfg(test)]
    admission_blocking_hook: Mutex<Option<AdmissionBlockingHook>>,
    #[cfg(test)]
    abandoned_sweep_hook: Mutex<Option<AbandonedSweepHook>>,
}

const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug)]
struct ArtifactStore {
    directory_handle: Arc<fs::File>,
    writes: Mutex<()>,
}

impl ArtifactStore {
    fn open(directory: PathBuf) -> std::io::Result<Arc<Self>> {
        prepare_private_directory(&directory)?;
        let expected = fs::symlink_metadata(&directory)?;
        let handle = rustix::fs::open(
            &directory,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let handle = fs::File::from(handle);
        ensure_same_object(&handle.metadata()?, &expected)?;
        validate_owned_directory(&handle)?;
        fchmod(&handle, Mode::from_raw_mode(0o700))?;
        let store = Arc::new(Self {
            directory_handle: Arc::new(handle),
            writes: Mutex::new(()),
        });
        store.cleanup_temporary_artifacts()?;
        store.validate_existing_artifacts()?;
        Ok(store)
    }

    fn retain(&self, content: &[u8]) -> std::io::Result<(ArtifactReference, String)> {
        let digest = sha256_hex(content);
        let _write = self
            .writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(mut existing) = self.open_existing(&digest)? {
            if hash_file(&mut existing)?.0 != digest {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "artifact digest collision or corrupt retained artifact",
                ));
            }
        } else {
            let temporary_name = format!(".{digest}.{}.tmp", Uuid::now_v7());
            let result = (|| -> std::io::Result<()> {
                let temporary = openat(
                    &*self.directory_handle,
                    &temporary_name,
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(0o600),
                )?;
                let mut temporary = fs::File::from(temporary);
                validate_owned_regular_file(&temporary)?;
                temporary.write_all(content)?;
                temporary.sync_all()?;
                drop(temporary);
                renameat(
                    &*self.directory_handle,
                    &temporary_name,
                    &*self.directory_handle,
                    &digest,
                )?;
                let final_file = self
                    .open_existing(&digest)?
                    .ok_or_else(|| std::io::Error::other("retained artifact disappeared"))?;
                validate_owned_regular_file(&final_file)?;
                fchmod(&final_file, Mode::from_raw_mode(0o600))?;
                fsync(&*self.directory_handle)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = unlinkat(&*self.directory_handle, &temporary_name, AtFlags::empty());
            }
            result?;
        }
        Ok((
            ArtifactReference {
                uri: format!("artifact://sha256/{digest}"),
            },
            digest,
        ))
    }

    fn open_existing(&self, name: &str) -> std::io::Result<Option<fs::File>> {
        match openat(
            &*self.directory_handle,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(file) => {
                let file = fs::File::from(file);
                validate_owned_regular_file(&file)?;
                fchmod(&file, Mode::from_raw_mode(0o600))?;
                Ok(Some(file))
            }
            Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn cleanup_temporary_artifacts(&self) -> std::io::Result<()> {
        for name in directory_names(&self.directory_handle)? {
            if !valid_temporary_artifact_name(&name) {
                continue;
            }
            let stat = statat(&*self.directory_handle, &name, AtFlags::SYMLINK_NOFOLLOW)?;
            if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                continue;
            }
            validate_stat_owner(&stat, "temporary artifact")?;
            let Some(file) = self.open_existing(&name)? else {
                continue;
            };
            validate_owned_regular_file(&file)?;
            ensure_stat_same_object(&file.metadata()?, &stat)?;
            unlinkat(&*self.directory_handle, &name, AtFlags::empty())?;
        }
        fsync(&*self.directory_handle)?;
        Ok(())
    }

    fn validate_existing_artifacts(&self) -> std::io::Result<()> {
        for name in directory_names(&self.directory_handle)? {
            if is_digest_name(&name) {
                let mut file = self.open_existing(&name)?.ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "existing artifact disappeared during validation",
                    )
                })?;
                if hash_file(&mut file)?.0 != name {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "existing artifact content does not match its digest name",
                    ));
                }
            }
        }
        Ok(())
    }

    fn create_capture_file(&self, name: &str) -> std::io::Result<fs::File> {
        if !valid_temporary_artifact_name(name) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid capture artifact name",
            ));
        }
        let file = openat(
            &*self.directory_handle,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let file = fs::File::from(file);
        validate_owned_regular_file(&file)?;
        Ok(file)
    }

    fn commit_capture(&self, name: &str) -> std::io::Result<(CapturedArtifact, u64)> {
        let _write = self
            .writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut temporary = self
            .open_existing(name)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "capture missing"))?;
        let (digest, byte_length, newlines) = hash_file(&mut temporary)?;
        temporary.sync_all()?;
        drop(temporary);
        if let Some(mut existing) = self.open_existing(&digest)? {
            if hash_file(&mut existing)?.0 != digest {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "artifact digest collision or corrupt retained artifact",
                ));
            }
            unlinkat(&*self.directory_handle, name, AtFlags::empty())?;
        } else {
            renameat(
                &*self.directory_handle,
                name,
                &*self.directory_handle,
                &digest,
            )?;
            let final_file = self
                .open_existing(&digest)?
                .ok_or_else(|| std::io::Error::other("capture artifact disappeared"))?;
            validate_owned_regular_file(&final_file)?;
            fchmod(&final_file, Mode::from_raw_mode(0o600))?;
        }
        fsync(&*self.directory_handle)?;
        Ok((
            CapturedArtifact {
                reference: ArtifactReference {
                    uri: format!("artifact://sha256/{digest}"),
                },
                sha256: digest,
                byte_length,
            },
            newlines,
        ))
    }

    fn discard_capture(&self, name: &str) {
        if valid_temporary_artifact_name(name) {
            let _ = unlinkat(&*self.directory_handle, name, AtFlags::empty());
        }
    }

    fn preview(&self, digest: &str, max_bytes: usize) -> std::io::Result<(String, bool)> {
        let mut file = self
            .open_existing(digest)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "artifact missing"))?;
        let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
        std::io::Read::by_ref(&mut file)
            .take(max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut bytes)?;
        let truncated = bytes.len() > max_bytes;
        bytes.truncate(max_bytes);
        Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
    }

    fn read_verified_attachment(&self, attachment: &ToolAttachment) -> std::io::Result<Vec<u8>> {
        if !is_digest_name(attachment.sha256.as_str())
            || attachment.reference.uri != format!("artifact://sha256/{}", attachment.sha256)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "attachment reference and digest do not match",
            ));
        }
        let mut file = self
            .open_existing(attachment.sha256.as_str())?
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "attachment artifact is missing",
                )
            })?;
        let (digest, byte_length, _) = hash_file(&mut file)?;
        if digest != attachment.sha256.as_str() || byte_length != attachment.byte_length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "attachment artifact digest or length is corrupt",
            ));
        }
        file.seek(SeekFrom::Start(0))?;
        let capacity = usize::try_from(byte_length).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "attachment length does not fit in memory",
            )
        })?;
        let mut bytes = Vec::with_capacity(capacity);
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn read_verified_native_context(
        &self,
        artifact: &cookie_agent_protocol::NativeContextArtifact,
    ) -> std::io::Result<String> {
        if artifact.reference.uri != format!("artifact://sha256/{}", artifact.sha256) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "native context reference and digest do not match",
            ));
        }
        let mut file = self
            .open_existing(artifact.sha256.as_str())?
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "native context artifact is missing",
                )
            })?;
        let (digest, byte_length, _) = hash_file(&mut file)?;
        if digest != artifact.sha256.as_str() || byte_length != artifact.byte_length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "native context artifact digest or length is corrupt",
            ));
        }
        file.seek(SeekFrom::Start(0))?;
        let mut payload = String::new();
        file.read_to_string(&mut payload)?;
        Ok(payload)
    }
}

fn hash_file(file: &mut fs::File) -> std::io::Result<(String, u64, u64)> {
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut total = 0_u64;
    let mut newlines = 0_u64;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        total = total.saturating_add(count as u64);
        newlines = newlines.saturating_add(
            buffer[..count]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count() as u64,
        );
    }
    file.seek(SeekFrom::Start(0))?;
    let digest = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((digest, total, newlines))
}

#[derive(Clone, Debug)]
struct OutputCapture {
    store: Arc<ArtifactStore>,
    stdout: Arc<CaptureStream>,
    stderr: Arc<CaptureStream>,
    _cleanup: Arc<CaptureCleanup>,
}

#[derive(Debug)]
struct CaptureStream {
    name: String,
    file: Mutex<fs::File>,
    error: Mutex<Option<String>>,
}

#[derive(Debug)]
struct CaptureCleanup {
    store: Arc<ArtifactStore>,
    stdout_name: String,
    stderr_name: String,
}

impl Drop for CaptureCleanup {
    fn drop(&mut self) {
        self.store.discard_capture(&self.stdout_name);
        self.store.discard_capture(&self.stderr_name);
    }
}

#[derive(Clone, Debug, Serialize)]
struct CapturedArtifact {
    reference: ArtifactReference,
    sha256: String,
    byte_length: u64,
}

fn composed_bash_output_lines(base: &str, stdout_newlines: u64, stderr_newlines: u64) -> u64 {
    // The two fixed delimiters are "\n\nstdout:\n" and "\n\nstderr:\n":
    // six newline bytes total. split('\n') line count is newline count + one.
    base.split('\n').count() as u64 + stdout_newlines + stderr_newlines + 6
}

impl OutputCapture {
    fn new(store: Arc<ArtifactStore>) -> std::io::Result<Self> {
        let id = Uuid::now_v7();
        let stdout_name = format!(".capture-{id}-stdout.tmp");
        let stderr_name = format!(".capture-{id}-stderr.tmp");
        let stdout = store.create_capture_file(&stdout_name)?;
        let stderr = match store.create_capture_file(&stderr_name) {
            Ok(stderr) => stderr,
            Err(error) => {
                store.discard_capture(&stdout_name);
                return Err(error);
            }
        };
        Ok(Self {
            store: store.clone(),
            stdout: Arc::new(CaptureStream {
                name: stdout_name.clone(),
                file: Mutex::new(stdout),
                error: Mutex::new(None),
            }),
            stderr: Arc::new(CaptureStream {
                name: stderr_name.clone(),
                file: Mutex::new(stderr),
                error: Mutex::new(None),
            }),
            _cleanup: Arc::new(CaptureCleanup {
                store,
                stdout_name,
                stderr_name,
            }),
        })
    }

    fn write(&self, stream: OutputStream, data: &[u8]) {
        let capture = match stream {
            OutputStream::Stdout => &self.stdout,
            OutputStream::Stderr => &self.stderr,
        };
        if capture
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
        {
            return;
        }
        if let Err(error) = capture
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .write_all(data)
        {
            *capture
                .error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
        }
    }

    fn finish(
        &self,
        mut result: ToolResult,
        max_lines: usize,
        max_bytes: usize,
    ) -> std::io::Result<ToolResult> {
        for stream in [&self.stdout, &self.stderr] {
            if let Some(error) = stream
                .error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            {
                self.discard();
                return Err(std::io::Error::other(format!(
                    "tool output capture failed: {error}"
                )));
            }
            stream
                .file
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .sync_all()?;
        }
        let (stdout, stdout_newlines) = match self.store.commit_capture(&self.stdout.name) {
            Ok(stdout) => stdout,
            Err(error) => {
                self.discard();
                return Err(error);
            }
        };
        let (stderr, stderr_newlines) = match self.store.commit_capture(&self.stderr.name) {
            Ok(stderr) => stderr,
            Err(error) => {
                self.store.discard_capture(&self.stderr.name);
                return Err(error);
            }
        };
        let base_output_bytes = result.output.len() as u64;
        let original_lines =
            composed_bash_output_lines(&result.output, stdout_newlines, stderr_newlines);
        let preview_budget = max_bytes.saturating_sub(result.output.len()).max(1);
        let (stdout_preview, stdout_truncated) =
            self.store.preview(&stdout.sha256, preview_budget)?;
        let (stderr_preview, stderr_truncated) =
            self.store.preview(&stderr.sha256, preview_budget)?;
        let complete_for_budget = format!(
            "{}\n\nstdout:\n{}\n\nstderr:\n{}",
            result.output, stdout_preview, stderr_preview
        );
        let preview = truncate_tool_output(&complete_for_budget, max_lines, max_bytes)
            .map_or(complete_for_budget.clone(), |preview| preview.content);
        let stream_truncated = stdout_truncated || stderr_truncated;
        let output_truncated = preview != complete_for_budget || stream_truncated;
        result.output = preview;
        let streams = serde_json::json!({"stdout": stdout.clone(), "stderr": stderr.clone()});
        match &mut result.metadata {
            Value::Object(metadata) => {
                metadata.insert("streams".into(), streams.clone());
            }
            metadata => {
                *metadata = serde_json::json!({"tool": metadata.clone(), "streams": streams});
            }
        }
        if output_truncated {
            let manifest = serde_json::to_vec(&serde_json::json!({
                "title": result.title,
                "streams": streams,
            }))?;
            let (retained, _) = self.store.retain(&manifest)?;
            result.truncation = Some(ToolOutputTruncation {
                original_bytes: base_output_bytes + stdout.byte_length + stderr.byte_length + 20,
                original_lines,
                retained,
            });
        }
        Ok(result)
    }

    fn discard(&self) {
        self.store.discard_capture(&self.stdout.name);
        self.store.discard_capture(&self.stderr.name);
    }
}

fn validate_owned_directory(directory: &fs::File) -> std::io::Result<()> {
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact store root is not a directory",
        ));
    }
    validate_owner(&metadata, "artifact store root")
}

fn validate_owned_regular_file(file: &fs::File) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact object is not a regular file",
        ));
    }
    validate_owner(&metadata, "artifact object")
}

fn validate_owner(metadata: &fs::Metadata, object: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{object} is not owned by the current user"),
            ));
        }
    }
    Ok(())
}

fn validate_stat_owner(stat: &rustix::fs::Stat, object: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    if stat.st_uid != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{object} is not owned by the current user"),
        ));
    }
    Ok(())
}

fn ensure_same_object(opened: &fs::Metadata, path: &fs::Metadata) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if opened.dev() != path.dev() || opened.ino() != path.ino() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "authorized read target changed while it was being opened",
            ));
        }
    }
    #[cfg(not(unix))]
    if opened.is_file() != path.is_file()
        || opened.is_dir() != path.is_dir()
        || opened.len() != path.len()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "authorized read target changed while it was being opened",
        ));
    }
    Ok(())
}

fn ensure_stat_same_object(opened: &fs::Metadata, path: &rustix::fs::Stat) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if opened.dev() != path.st_dev || opened.ino() != path.st_ino {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "artifact object changed during validation",
            ));
        }
    }
    Ok(())
}

fn directory_names(directory: &fs::File) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    let mut entries = Dir::read_from(directory)?;
    for entry in &mut entries {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if matches!(name, b"." | b"..") {
            continue;
        }
        if let Ok(name) = std::str::from_utf8(name) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

fn valid_temporary_artifact_name(name: &str) -> bool {
    if let Some(value) = name
        .strip_prefix(".capture-")
        .and_then(|value| value.strip_suffix(".tmp"))
    {
        let Some((id, stream)) = value.rsplit_once('-') else {
            return false;
        };
        return matches!(stream, "stdout" | "stderr") && Uuid::parse_str(id).is_ok();
    }
    let Some(value) = name
        .strip_prefix('.')
        .and_then(|value| value.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((digest, id)) = value.split_once('.') else {
        return false;
    };
    is_digest_name(digest) && Uuid::parse_str(id).is_ok()
}

fn is_digest_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Copy)]
struct InflightDelegation {
    parent_run_id: RunId,
    parent_session_id: Option<SessionId>,
    parent_tool_call_id: Option<ToolCallId>,
    child_session_id: Option<SessionId>,
    child_run_id: Option<RunId>,
    starting: bool,
    cancelled: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct AbandonedAdmission {
    parent_session_id: SessionId,
    parent_run_id: RunId,
    parent_tool_call_id: ToolCallId,
    child_session_id: Option<SessionId>,
    child_run_id: Option<RunId>,
}

/// Removes only its own admission generation. Concurrent redeliveries retain
/// independent entries until every caller has completed or unwound.
struct AdmissionGuard {
    inner: Arc<Inner>,
    invocation_id: InvocationId,
    generation: u64,
    completed: bool,
}

impl AdmissionGuard {
    fn begin(inner: Arc<Inner>, invocation_id: InvocationId, parent_run_id: RunId) -> Self {
        let generation = inner
            .next_admission_generation
            .fetch_add(1, Ordering::Relaxed);
        let mut admissions = inner
            .inflight_delegations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entries = admissions.entry(invocation_id).or_default();
        // Keep abandoned generations until their sweeper has observed them.
        // Removing them here would discard the child/run identity needed to
        // finish a pending cancellation.
        entries.insert(
            generation,
            InflightDelegation {
                parent_run_id,
                parent_session_id: None,
                parent_tool_call_id: None,
                child_session_id: None,
                child_run_id: None,
                starting: false,
                cancelled: false,
            },
        );
        drop(admissions);
        Self {
            inner,
            invocation_id,
            generation,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
        self.remove();
    }

    fn handoff(&mut self) {
        // The successful admission is now observed by DelegateAwait. Keep its
        // generation live until the child reaches a terminal state so a stale
        // concurrent redelivery cannot cancel the shared child.
        self.completed = true;
    }

    fn set_parent(&self, parent_session_id: SessionId, parent_tool_call_id: ToolCallId) {
        if let Some(admission) = self
            .inner
            .inflight_delegations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(&self.invocation_id)
            .and_then(|entries| entries.get_mut(&self.generation))
        {
            admission.parent_session_id = Some(parent_session_id);
            admission.parent_tool_call_id = Some(parent_tool_call_id);
        }
    }

    fn remove(&self) {
        let mut admissions = self
            .inner
            .inflight_delegations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entries) = admissions.get_mut(&self.invocation_id) {
            entries.remove(&self.generation);
            if entries.is_empty() {
                admissions.remove(&self.invocation_id);
            }
        }
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut admissions = self
            .inner
            .inflight_delegations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let abandoned = if let Some(entries) = admissions.get_mut(&self.invocation_id)
            && let Some(admission) = entries.get_mut(&self.generation)
        {
            // An abandoned delegate_invoke has no handle for the caller to
            // cancel. Retain a cancellation gate so a concurrent creator
            // cannot start its child after this future is dropped.
            admission.cancelled = true;
            true
        } else {
            false
        };
        drop(admissions);
        if abandoned
            && let Some(runtime) = self
                .inner
                .runtime
                .clone()
                .or_else(|| tokio::runtime::Handle::try_current().ok())
        {
            let engine = Engine {
                inner: self.inner.clone(),
            };
            let sweep_engine = engine.clone();
            let invocation_id = self.invocation_id;
            let generation = self.generation;
            let _ = engine.spawn_admission_task(&runtime, async move {
                if let Err(error) = sweep_engine
                    .sweep_abandoned_admission(invocation_id, generation)
                    .await
                {
                    eprintln!("delegate admission sweep failed: {error}");
                }
            });
        }
    }
}

/// Cloneable in-process client facade. It contains no transport concerns and
/// is safe for tool providers to call while their parent call is executing.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}
pub type EngineClient = Engine;

impl Engine {
    fn materialize_agents(
        &self,
        model_set: &cookie_agent_models::ModelSet,
    ) -> Result<Arc<AgentRegistry>, EngineError> {
        self.inner
            .config
            .resolve_agents(model_set)
            .map(Arc::new)
            .map_err(|error| EngineError::Config(Box::new(error)))
    }

    fn session_model_snapshot(
        &self,
        session: &session::SessionProjection,
    ) -> Result<Arc<cookie_agent_models::ModelSnapshot>, EngineError> {
        if let Some(snapshot) = self
            .inner
            .session_model_snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session.meta.session_id)
            .cloned()
        {
            return Ok(snapshot);
        }
        let fingerprint =
            cookie_agent_models::Sha256Digest::new(session.model_snapshot_fingerprint.as_str())
                .map_err(|error| {
                    EngineError::from(ModelError::invalid_request(error.to_string()))
                })?;
        let snapshot = self
            .inner
            .model_manager
            .snapshot(&fingerprint)
            .ok_or_else(|| {
                EngineError::from(ModelError::invalid_request("obsolete_model_fingerprint"))
            })?;
        self.inner
            .session_model_snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session.meta.session_id, Arc::clone(&snapshot));
        Ok(snapshot)
    }

    fn historical_run_model_snapshot(
        &self,
        run: RunId,
    ) -> Result<Arc<cookie_agent_models::ModelSnapshot>, EngineError> {
        self.inner
            .run_model_snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run)
            .cloned()
            .ok_or_else(|| {
                EngineError::from(ModelError::invalid_request(
                    "historical_run_model_snapshot_unavailable",
                ))
            })
    }

    fn historical_title_policy(
        &self,
        events: &[StoredEvent],
        run: RunId,
    ) -> Result<FrozenRunPolicy, EngineError> {
        let (agent, suffix) = latest_run_policy(events, run)?;
        let model_snapshot = self.historical_run_model_snapshot(run)?;
        let agents = self.materialize_agents(model_snapshot.model_set())?;
        policy_from_snapshot(
            agent,
            suffix,
            agents,
            model_snapshot,
            self.inner.config.runtime.tool_output.max_lines,
            self.inner.config.runtime.tool_output.max_bytes,
        )
    }

    pub fn open(options: EngineOptions) -> Result<Self, EngineError> {
        options
            .config
            .resolve_agents(options.model_manager.current().model_set())
            .map_err(|error| EngineError::Config(Box::new(error)))?;
        let store = SessionStore::open(&options.data_dir, &options.cwd)?;
        let artifacts = ArtifactStore::open(store.project_dir_path().join("artifacts"))?;
        let journal = DelegationJournal::open(store.project_dir_path().join("delegations.jsonl"))?;
        let grant_journal = GrantInvalidationJournal::open(
            store.project_dir_path().join("grant-invalidations.jsonl"),
        )?;
        let internal_agents = InternalAgentRuntime::freeze();
        let engine = Self {
            inner: Arc::new(Inner {
                config: options.config,
                artifacts,
                mutation_locks: Mutex::new(HashMap::new()),
                store,
                journal,
                grant_journal,
                model_manager: options.model_manager,
                session_model_snapshots: Mutex::new(HashMap::new()),
                run_model_snapshots: Mutex::new(HashMap::new()),
                internal_agents,
                tools: Mutex::new(options.tools),
                approvals: ApprovalStore::default(),
                permissions: PermissionPipeline::default(),
                active: Mutex::new(HashMap::new()),
                inflight_delegations: Mutex::new(HashMap::new()),
                next_admission_generation: AtomicU64::new(1),
                subscribers: Mutex::new(HashMap::new()),
                actors: Mutex::new(HashMap::new()),
                output_hubs: Mutex::new(HashMap::new()),
                finalized_output_hubs: Mutex::new(VecDeque::new()),
                pending_approvals: Mutex::new(HashMap::new()),
                runtime: tokio::runtime::Handle::try_current().ok(),
                admission_tasks: Mutex::new(Vec::new()),
                admission_blocking_tasks: Mutex::new(Vec::new()),
                admission_tasks_closing: AtomicBool::new(false),
                recovery_waiters: Mutex::new(HashSet::new()),
                #[cfg(test)]
                prompt_snapshot_hook: Mutex::new(None),
                #[cfg(test)]
                approval_evaluation_hook: Mutex::new(None),
                #[cfg(test)]
                gap_send_hook: Mutex::new(None),
                #[cfg(test)]
                admission_confirmation_hook: Mutex::new(None),
                #[cfg(test)]
                admission_blocking_hook: Mutex::new(None),
                #[cfg(test)]
                abandoned_sweep_hook: Mutex::new(None),
            }),
        };
        for session in engine.inner.store.all() {
            engine.spawn_actor(session.meta.session_id);
        }
        engine.rebuild_approvals();
        // Reconciliation uses the synchronous actor facade. When open is
        // called by an async composition root, move that facade to a plain
        // thread so `blocking_send` never runs on a Tokio worker.
        if tokio::runtime::Handle::try_current().is_ok() {
            let reconcile_engine = engine.clone();
            std::thread::spawn(move || reconcile_engine.reconcile())
                .join()
                .map_err(|_| EngineError::ActorStopped)??;
        } else {
            engine.reconcile()?;
        }
        Ok(engine)
    }

    #[must_use]
    pub fn client(&self) -> EngineClient {
        self.clone()
    }

    fn mutation_lock(&self, key: &PreparedSerializationKey) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .mutation_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn spawn_admission_task<F>(&self, runtime: &tokio::runtime::Handle, task: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let Ok(mut tasks) = self.inner.admission_tasks.lock() else {
            return false;
        };
        if self.inner.admission_tasks_closing.load(Ordering::Acquire) {
            return false;
        }
        tasks.retain(|task| !task.is_finished());
        tasks.push(runtime.spawn(task));
        true
    }

    async fn spawn_admission_blocking<T, E, F>(&self, work: F) -> Result<T, EngineError>
    where
        T: Send + 'static,
        E: Into<EngineError> + Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        {
            let mut tasks = self
                .inner
                .admission_blocking_tasks
                .lock()
                .map_err(|_| EngineError::ActorStopped)?;
            if self.inner.admission_tasks_closing.load(Ordering::Acquire) {
                return Err(EngineError::ActorStopped);
            }
            tasks.retain(|task| !task.is_finished());
            #[cfg(test)]
            let hook = self
                .inner
                .admission_blocking_hook
                .lock()
                .expect("admission blocking hook lock poisoned")
                .take();
            tasks.push(tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                if let Some(hook) = hook {
                    let _ = hook.reached.send(());
                    let _ = hook.release.recv();
                }
                let _ = sender.send(work().map_err(Into::into));
            }));
        }
        receiver.await.map_err(|_| EngineError::ActorStopped)?
    }

    /// Registers a tool provider after engine open, allowing providers that
    /// require an EngineClient (notably delegate) to break the construction cycle.
    pub fn register_tool_provider(&self, provider: Arc<dyn ToolProvider>) {
        self.inner
            .tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(provider);
    }

    /// Stops new session mailbox traffic, cancels active work, and joins the
    /// journal worker. Existing client clones may keep a session mailbox alive.
    pub async fn shutdown(&self) {
        self.inner
            .admission_tasks_closing
            .store(true, Ordering::Release);
        let tasks = self
            .inner
            .admission_tasks
            .lock()
            .map(|mut tasks| tasks.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            if let Err(error) = task.await {
                eprintln!("admission task stopped during shutdown: {error}");
            }
        }
        let blocking_tasks = self
            .inner
            .admission_blocking_tasks
            .lock()
            .map(|mut tasks| tasks.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for task in blocking_tasks {
            if let Err(error) = task.await {
                eprintln!("admission blocking task stopped during shutdown: {error}");
            }
        }
        let active: Vec<_> = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        for run in active {
            run.cancellation.cancel();
        }
        self.inner
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.inner.journal.shutdown();
    }

    pub fn create_session(&self, selection: RunSelection) -> Result<SessionMeta, EngineError> {
        let snapshot = self.inner.model_manager.current();
        let agents = self.materialize_agents(snapshot.model_set())?;
        let agent = resolve_agent(&agents, &selection.agent)?.clone();
        if !agent.runnable_as_root {
            return Err(EngineError::IneligibleAgent(selection.agent));
        }
        let policy = freeze_root_agent_policy(
            &agent,
            agents,
            Arc::clone(&snapshot),
            &selection.model,
            policy::ResultLimits {
                tool_output_max_lines: self.inner.config.runtime.tool_output.max_lines,
                tool_output_max_bytes: self.inner.config.runtime.tool_output.max_bytes,
            },
        )?;
        let id = SessionId::new_v7();
        let cwd_identity = cwd_identity(self.inner.store.cwd())?;
        let creation = Event::SessionCreated {
            origin: SessionOrigin::Root,
            cwd_identity: cwd_identity.clone(),
            creation_selection: selection.clone(),
            creation_agent: Box::new(policy.agent.clone()),
            model_snapshot_fingerprint: protocol_digest(
                policy.model_snapshot.model_set().fingerprint(),
            )?,
        };
        let meta = session_meta(id, SessionOrigin::Root, cwd_identity, selection);
        self.inner.store.create(meta.clone(), creation)?;
        self.inner
            .session_model_snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, snapshot);
        self.spawn_actor(id);
        Ok(meta)
    }

    /// Privileged child creation used exclusively by a delegate tool provider.
    /// The origin fields are derived from the parent projection, never supplied
    /// by a caller.
    #[allow(dead_code)] // wired by the crate-internal delegation capability once tools exposes it
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create_child(
        &self,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        agent: &AgentId,
        child_policy: FrozenRunPolicy,
        request_fingerprint: String,
        request: journal::DelegateRequestPayload,
        admission: Option<(InvocationId, u64)>,
    ) -> Result<SessionMeta, EngineError> {
        let child_model_snapshot = Arc::clone(&child_policy.model_snapshot);
        let parent = self.inner.store.get(parent_session_id)?;
        if parent
            .runs
            .get(&parent_run_id)
            .and_then(|run| run.pending_calls.get(&parent_tool_call_id))
            .is_none_or(|tool| tool != "delegate")
        {
            return Err(EngineError::MissingTool(
                "delegate call is not pending".into(),
            ));
        }
        if self
            .terminal_parent_delegate(parent_session_id, parent_run_id, parent_tool_call_id)
            .await?
        {
            return Err(EngineError::MissingTool(
                "delegate parent run is terminal".into(),
            ));
        }
        let parent_policy = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&parent_run_id)
            .map(|active| active.policy.clone())
            .ok_or_else(|| EngineError::MissingTool("delegate parent run is not active".into()))?;
        let Some(parent_delegation) = &parent_policy.agent.delegation else {
            return Err(EngineError::MissingTool("delegate admission denied".into()));
        };
        let depth = session_depth(&parent.meta.origin);
        if depth >= parent_delegation.effective_depth_ceiling
            || !parent_delegation.targets.contains(agent)
        {
            return Err(EngineError::MissingTool("delegate admission denied".into()));
        }
        let invocation_id = invocation_id(parent_session_id, parent_run_id, parent_tool_call_id);
        let journal = self.inner.journal.clone();
        let journal_agent = child_policy.agent.clone();
        let journal_suffix = child_policy.selected_suffix_wire.clone();
        let entry = self
            .spawn_admission_blocking(move || {
                journal.reserve(
                    invocation_id,
                    parent_session_id,
                    parent_run_id,
                    parent_tool_call_id,
                    journal_agent,
                    journal_suffix,
                    request_fingerprint,
                    request,
                )
            })
            .await?;
        if admission.is_some_and(|(invocation_id, generation)| {
            !self.admission_generation_live(invocation_id, generation)
        }) {
            return Err(EngineError::MissingTool(
                "delegate admission was abandoned".into(),
            ));
        }
        // The reservation may have completed while the parent was cancelled.
        // Never turn that durable reservation into a child after cancellation.
        if self
            .terminal_parent_delegate(parent_session_id, parent_run_id, parent_tool_call_id)
            .await?
        {
            return Err(EngineError::MissingTool(
                "delegate parent run is terminal".into(),
            ));
        }
        if let Ok(existing) = self.inner.store.get(entry.reservation.child_session_id) {
            self.inner
                .session_model_snapshots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(existing.meta.session_id)
                .or_insert_with(|| Arc::clone(&child_model_snapshot));
            self.ensure_parent_link(
                parent_session_id,
                parent_run_id,
                parent_tool_call_id,
                existing.meta.session_id,
            )
            .await?;
            let journal = self.inner.journal.clone();
            self.spawn_admission_blocking(move || journal.mark_linked(invocation_id))
                .await?;
            return Ok(existing.meta);
        }
        let (root, depth) = match parent.meta.origin {
            SessionOrigin::Delegated {
                root_session_id,
                depth,
                ..
            } => (root_session_id, depth + 1),
            _ => (parent_session_id, 1),
        };
        let origin = SessionOrigin::Delegated {
            root_session_id: root,
            parent_session_id,
            parent_run_id,
            parent_tool_call_id,
            invocation_id,
            depth,
        };
        let selection = RunSelection {
            agent: agent.clone(),
            model: child_policy.selected_suffix[0].resolved.selection.clone(),
        };
        let meta = session_meta(
            entry.reservation.child_session_id,
            origin.clone(),
            parent.meta.cwd_identity.clone(),
            selection.clone(),
        );
        let creation = Event::SessionCreated {
            origin,
            cwd_identity: parent.meta.cwd_identity.clone(),
            creation_selection: selection,
            creation_agent: Box::new(child_policy.agent.clone()),
            model_snapshot_fingerprint: protocol_digest(
                child_policy.model_snapshot.model_set().fingerprint(),
            )?,
        };
        let store = self.inner.store.clone();
        let creation_meta = meta.clone();
        self.spawn_admission_blocking(move || store.create_with_status(creation_meta, creation))
            .await?;
        self.inner
            .session_model_snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(meta.session_id, child_model_snapshot);
        self.spawn_actor(meta.session_id);
        if admission.is_some_and(|(invocation_id, generation)| {
            !self.admission_generation_live(invocation_id, generation)
        }) {
            return Err(EngineError::MissingTool(
                "delegate admission was abandoned".into(),
            ));
        }
        self.ensure_parent_link(
            parent_session_id,
            parent_run_id,
            parent_tool_call_id,
            meta.session_id,
        )
        .await?;
        let journal = self.inner.journal.clone();
        self.spawn_admission_blocking(move || journal.mark_linked(invocation_id))
            .await?;
        Ok(meta)
    }

    fn admission_generation_live(&self, invocation_id: InvocationId, generation: u64) -> bool {
        self.inner
            .inflight_delegations
            .lock()
            .ok()
            .and_then(|admissions| {
                admissions
                    .get(&invocation_id)
                    .and_then(|entries| entries.get(&generation))
                    .is_some_and(|admission| !admission.cancelled)
                    .then_some(())
            })
            .is_some()
    }

    fn publish_admission_child(
        &self,
        invocation_id: InvocationId,
        generation: u64,
        child_session_id: SessionId,
    ) -> Result<(), EngineError> {
        let mut admissions = self
            .inner
            .inflight_delegations
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let admission = admissions
            .get_mut(&invocation_id)
            .and_then(|entries| entries.get_mut(&generation))
            .ok_or_else(|| EngineError::MissingTool("delegate admission disappeared".into()))?;
        admission.child_session_id = Some(child_session_id);
        Ok(())
    }

    fn publish_admission_run(
        &self,
        invocation_id: InvocationId,
        generation: u64,
        child_session_id: SessionId,
        child_run_id: RunId,
    ) -> Result<(), EngineError> {
        let mut admissions = self
            .inner
            .inflight_delegations
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let admission = admissions
            .get_mut(&invocation_id)
            .and_then(|entries| entries.get_mut(&generation))
            .ok_or_else(|| EngineError::MissingTool("delegate admission disappeared".into()))?;
        admission.child_session_id = Some(child_session_id);
        admission.child_run_id = Some(child_run_id);
        admission.starting = false;
        Ok(())
    }

    fn mark_admission_starting(
        &self,
        invocation_id: InvocationId,
        generation: u64,
    ) -> Result<(), EngineError> {
        let mut admissions = self
            .inner
            .inflight_delegations
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let admission = admissions
            .get_mut(&invocation_id)
            .and_then(|entries| entries.get_mut(&generation))
            .filter(|admission| !admission.cancelled)
            .ok_or_else(|| EngineError::MissingTool("delegate admission disappeared".into()))?;
        admission.starting = true;
        Ok(())
    }

    fn clear_admission_starting(&self, invocation_id: InvocationId, generation: u64) {
        if let Ok(mut admissions) = self.inner.inflight_delegations.lock()
            && let Some(admission) = admissions
                .get_mut(&invocation_id)
                .and_then(|entries| entries.get_mut(&generation))
        {
            admission.starting = false;
        }
    }

    /// Atomically revalidates the generation against the admission registry at
    /// the destructive cancellation point. A retry that enters first makes the
    /// all-abandoned predicate false; a retry that enters afterwards observes a
    /// sweep that was already linearized for the previous generation.
    fn observe_abandoned_admission(
        &self,
        invocation_id: InvocationId,
        generation: u64,
    ) -> Result<Option<AbandonedAdmission>, EngineError> {
        let admissions = self
            .inner
            .inflight_delegations
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let Some(entries) = admissions.get(&invocation_id) else {
            return Ok(None);
        };
        let Some(admission) = entries.get(&generation) else {
            return Ok(None);
        };
        if !admission.cancelled {
            return Ok(None);
        }
        if admission.starting && admission.child_run_id.is_none() {
            return Ok(None);
        }
        if entries.values().any(|entry| !entry.cancelled) {
            return Ok(None);
        }
        let (Some(parent_session_id), Some(parent_tool_call_id)) =
            (admission.parent_session_id, admission.parent_tool_call_id)
        else {
            return Ok(None);
        };
        let target = AbandonedAdmission {
            parent_session_id,
            parent_run_id: admission.parent_run_id,
            parent_tool_call_id,
            child_session_id: admission.child_session_id,
            child_run_id: admission.child_run_id,
        };
        Ok(Some(target))
    }

    fn cancel_abandoned_admission(
        &self,
        invocation_id: InvocationId,
        generation: u64,
        observed: AbandonedAdmission,
    ) -> Result<Option<AbandonedAdmission>, EngineError> {
        let mut admissions = self
            .inner
            .inflight_delegations
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let Some(entries) = admissions.get_mut(&invocation_id) else {
            return Ok(None);
        };
        let Some(admission) = entries.get(&generation).copied() else {
            return Ok(None);
        };
        if !admission.cancelled
            || (admission.starting && admission.child_run_id.is_none())
            || entries.values().any(|entry| !entry.cancelled)
        {
            entries.remove(&generation);
            if entries.is_empty() {
                admissions.remove(&invocation_id);
            }
            return Ok(None);
        }
        let (Some(parent_session_id), Some(parent_tool_call_id)) =
            (admission.parent_session_id, admission.parent_tool_call_id)
        else {
            return Ok(None);
        };
        let target = AbandonedAdmission {
            parent_session_id,
            parent_run_id: admission.parent_run_id,
            parent_tool_call_id,
            child_session_id: admission.child_session_id,
            child_run_id: admission.child_run_id,
        };
        if target != observed {
            return Ok(None);
        }
        if let Some(run_id) = target.child_run_id {
            match self.cancel_run_durably(run_id, Some("delegate admission was abandoned".into())) {
                Ok(_) => {}
                Err(EngineError::MissingRun(_))
                    if target.child_session_id.is_some_and(|child| {
                        self.inner
                            .store
                            .get(child)
                            .ok()
                            .and_then(|projection| projection.runs.get(&run_id).cloned())
                            .is_some_and(|run| run.status != SessionStatus::Running)
                    }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Some(target))
    }

    /// Drives an abandoned admission from engine-owned shared state. Parent
    /// resolution is revalidated by the parent actor immediately before its
    /// durable append, so a concurrent retry cannot be resolved by a stale sweep.
    async fn sweep_abandoned_admission(
        &self,
        invocation_id: InvocationId,
        generation: u64,
    ) -> Result<(), EngineError> {
        let observed = self.observe_abandoned_admission(invocation_id, generation)?;
        #[cfg(test)]
        let hook = self
            .inner
            .abandoned_sweep_hook
            .lock()
            .expect("abandoned sweep hook lock poisoned")
            .clone();
        #[cfg(test)]
        if let Some(observed) = observed
            && let Some(hook) = hook
        {
            let _ = hook.reached.send(());
            let _ = hook
                .captured
                .send(observed.child_run_id.into_iter().collect());
            hook.release.notified().await;
        }
        let Some(observed) = observed else {
            return Ok(());
        };
        let Some(target) = self.cancel_abandoned_admission(invocation_id, generation, observed)?
        else {
            return Ok(());
        };
        let result = cancelled_delegate_result_with_reason(
            target.child_session_id,
            "delegate admission was abandoned",
        );
        self.request(target.parent_session_id, |reply| {
            SessionCommand::ResolveAbandonedDelegateFailureIfPending {
                invocation_id,
                generation,
                run: target.parent_run_id,
                tool_call_id: target.parent_tool_call_id,
                result,
                reply,
            }
        })
        .await
        .map(|_| ())
    }

    /// Serializes the durable parent backlink per invocation. Every admission
    /// path re-checks under this barrier; only the first appends it.
    async fn ensure_parent_link(
        &self,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        child_session_id: SessionId,
    ) -> Result<(), EngineError> {
        self.request(parent_session_id, |reply| {
            SessionCommand::EnsureToolCallLinked {
                run: parent_run_id,
                tool_call_id: parent_tool_call_id,
                child_session_id,
                reply,
            }
        })
        .await
    }

    fn ensure_parent_link_blocking(
        &self,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        child_session_id: SessionId,
    ) -> Result<(), EngineError> {
        let ensure = || {
            self.request_blocking(parent_session_id, |reply| {
                SessionCommand::EnsureToolCallLinked {
                    run: parent_run_id,
                    tool_call_id: parent_tool_call_id,
                    child_session_id,
                    reply,
                }
            })
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|scope| {
                scope
                    .spawn(ensure)
                    .join()
                    .expect("ensure-link helper thread panicked")
            })
        } else {
            ensure()
        }
    }

    async fn terminal_parent_delegate(
        &self,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
    ) -> Result<bool, EngineError> {
        let parent = self.inner.store.get(parent_session_id)?;
        let Some(run) = parent.runs.get(&parent_run_id) else {
            return Ok(true);
        };
        if !matches!(
            run.status,
            SessionStatus::Cancelled | SessionStatus::Failed | SessionStatus::Completed
        ) {
            return Ok(false);
        }
        if run
            .pending_calls
            .get(&parent_tool_call_id)
            .is_some_and(|tool| tool == "delegate")
        {
            let result =
                cancelled_delegate_result_with_reason(None, "parent run was already terminal");
            self.append(
                parent_session_id,
                Some(parent_run_id),
                Event::ToolCallTerminated {
                    termination: ToolCallTermination {
                        tool_call_id: parent_tool_call_id,
                        owner: self.tool_call_owner(
                            parent_session_id,
                            parent_run_id,
                            parent_tool_call_id,
                        )?,
                        outcome: ToolTerminationOutcome::Cancelled,
                        result: Some(result),
                        error: Some(SafeToolError {
                            code: safe_code("parent_terminal"),
                            message: safe_error("parent run was already terminal"),
                        }),
                    },
                },
            )
            .await?;
        }
        Ok(true)
    }

    /// Admits a delegate invocation, creates/attaches its child, and starts the
    /// invocation-derived child run exactly once.
    pub async fn delegate_invoke(
        &self,
        invocation: DelegateInvocation,
    ) -> Result<DelegateHandle, EngineError> {
        let invocation_id = invocation_id(
            invocation.parent_session_id,
            invocation.parent_run_id,
            invocation.parent_tool_call_id,
        );
        let mut admission =
            AdmissionGuard::begin(self.inner.clone(), invocation_id, invocation.parent_run_id);
        admission.set_parent(invocation.parent_session_id, invocation.parent_tool_call_id);
        // The admission task, rather than this observer, owns the durable start
        // confirmation and therefore survives a dropped caller future.
        let (reply, receiver) = oneshot::channel();
        let engine = self.clone();
        let generation = admission.generation;
        let Some(runtime) = tokio::runtime::Handle::try_current().ok() else {
            return Err(EngineError::ActorStopped);
        };
        if !self.spawn_admission_task(&runtime, async move {
            let result = engine
                .delegate_invoke_admitted(invocation, invocation_id, generation)
                .await;
            let _ = reply.send(result);
        }) {
            return Err(EngineError::ActorStopped);
        }
        let result = receiver.await.map_err(|_| EngineError::ActorStopped)?;
        if result.is_ok() {
            admission.handoff();
        } else {
            admission.complete();
        }
        result
    }

    async fn delegate_invoke_admitted(
        &self,
        invocation: DelegateInvocation,
        invocation_id: InvocationId,
        generation: u64,
    ) -> Result<DelegateHandle, EngineError> {
        let parent = self.inner.store.get(invocation.parent_session_id)?;
        if parent
            .runs
            .get(&invocation.parent_run_id)
            .is_some_and(|run| run.status == SessionStatus::Interrupted)
            && self.journal_get(invocation_id).await?.is_none()
        {
            return Err(EngineError::MissingTool(
                "delegate parent run is interrupted; use recovery".into(),
            ));
        }
        let active_parent = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&invocation.parent_run_id)
            .cloned()
            .ok_or_else(|| EngineError::MissingTool("delegate parent run is not active".into()))?;
        let parent_policy = Arc::clone(&active_parent.policy);
        let parent_delegation = parent_policy
            .agent
            .delegation
            .as_ref()
            .ok_or_else(|| EngineError::MissingTool("delegation is disabled".into()))?;
        if !parent_delegation.targets.contains(&invocation.agent)
            || session_depth(&parent.meta.origin) >= parent_delegation.effective_depth_ceiling
        {
            return Err(EngineError::MissingTool(
                "delegate target or depth is not allowed".into(),
            ));
        }
        let child_agent = resolve_agent(&parent_policy.registry, &invocation.agent)?;
        if !child_agent.document.frontmatter.enabled
            || !matches!(
                child_agent.document.frontmatter.mode,
                cookie_agent_config::AgentMode::Subagent | cookie_agent_config::AgentMode::All
            )
        {
            return Err(EngineError::DisabledAgent(invocation.agent));
        }
        let fallback_index = active_parent.fallback_index.load(Ordering::Acquire) as usize;
        let inherited = parent_policy.active_suffix(fallback_index);
        let child_selection = child_agent
            .resolved_fallback
            .iter()
            .find(|selection| {
                parent_policy
                    .model_snapshot
                    .model_set()
                    .get(&selection.model)
                    .is_some_and(cookie_agent_models::ModelEntry::is_available)
            })
            .cloned()
            .or_else(|| {
                if child_agent.resolved_fallback.is_empty() {
                    inherited
                        .first()
                        .map(|binding| binding.resolved.selection.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| EngineError::MissingTool("delegate child has no model suffix".into()))?;
        let child_policy = freeze_delegated_child_policy(
            child_agent,
            &parent_policy,
            &child_selection,
            inherited,
            parent_delegation.effective_depth_ceiling,
            policy::ResultLimits {
                tool_output_max_lines: self.inner.config.runtime.tool_output.max_lines,
                tool_output_max_bytes: self.inner.config.runtime.tool_output.max_bytes,
            },
        )?;
        let fingerprint = serde_json::to_string(&(
            &invocation.agent,
            &invocation.task,
            &invocation.context,
            &invocation.success_criteria,
            &invocation.expected_output,
            &child_policy.agent,
            &child_policy.selected_suffix_wire,
        ))
        .expect("delegate invocation fingerprint serializes");
        let request = journal::DelegateRequestPayload {
            task: invocation.task,
            context: invocation.context,
            success_criteria: invocation.success_criteria,
            expected_output: invocation.expected_output,
        };
        let child = match self
            .create_child(
                invocation.parent_session_id,
                invocation.parent_run_id,
                invocation.parent_tool_call_id,
                &invocation.agent,
                child_policy,
                fingerprint,
                request,
                Some((invocation_id, generation)),
            )
            .await
        {
            Ok(child) => child,
            Err(error) => {
                if is_journal_append_failure(&error) {
                    let result = delegate_failure_result(None, "delegate journal append failed");
                    self.resolve_delegate_failure_if_pending(
                        invocation.parent_session_id,
                        invocation.parent_run_id,
                        invocation.parent_tool_call_id,
                        result,
                    )
                    .await?;
                }
                return Err(error);
            }
        };
        self.publish_admission_child(invocation_id, generation, child.session_id)?;
        let entry = self
            .journal_get(invocation_id)
            .await?
            .ok_or_else(|| EngineError::MissingTool("delegate reservation disappeared".into()))?;
        let child_run_id = match self
            .ensure_delegate_run(&entry, Some((invocation_id, generation)))
            .await
        {
            Ok(run_id) => run_id,
            Err(error) => {
                if is_journal_append_failure(&error) {
                    let result = delegate_failure_result(
                        Some(entry.reservation.child_session_id),
                        "delegate journal run confirmation failed",
                    );
                    self.resolve_delegate_failure_if_pending(
                        entry.reservation.parent_session_id,
                        entry.reservation.parent_run_id,
                        entry.reservation.parent_tool_call_id,
                        result,
                    )
                    .await?;
                }
                return Err(error);
            }
        };
        Ok(DelegateHandle {
            invocation_id,
            child_session_id: child.session_id,
            child_run_id,
        })
    }

    /// Waits for a child terminal state and returns the bounded model-visible
    /// delegate result. Cancellation is represented as structured JSON.
    pub fn await_delegate(&self, handle: DelegateHandle) -> DelegateAwait {
        let engine = self.clone();
        DelegateAwait {
            future: Box::pin(async move { engine.await_delegate_inner(handle).await }),
            engine: self.clone(),
            runtime: self.inner.runtime.clone(),
            handle,
            completed: false,
        }
    }

    async fn await_delegate_inner(
        &self,
        handle: DelegateHandle,
    ) -> Result<ToolResult, EngineError> {
        loop {
            let child = match self.inner.store.get(handle.child_session_id) {
                Ok(child) => child,
                Err(_) => {
                    return Ok(delegate_failure_result(
                        Some(handle.child_session_id),
                        "child session is missing",
                    ));
                }
            };
            match child.status {
                SessionStatus::Running | SessionStatus::Idle => {
                    let active = {
                        self.inner
                            .active
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .get(&handle.child_run_id)
                            .cloned()
                    };
                    if let Some(active) = active {
                        // Event-driven terminal-state wakeups are post-MVP; this
                        // bounded cancellation-aware wait keeps the MVP responsive.
                        tokio::select! {
                            () = active.cancellation.cancelled() => {},
                            () = tokio::time::sleep(std::time::Duration::from_millis(25)) => {},
                        }
                    } else {
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    }
                }
                SessionStatus::Completed => {
                    let result = completed_delegate_result(
                        &child,
                        Some(handle.child_run_id),
                        &self.inner.artifacts,
                    )?;
                    self.clear_delegate_admissions(handle.invocation_id);
                    return Ok(result);
                }
                SessionStatus::Cancelled => {
                    let result = cancelled_delegate_result(handle.child_session_id, None);
                    self.clear_delegate_admissions(handle.invocation_id);
                    return Ok(result);
                }
                SessionStatus::Failed | SessionStatus::Interrupted => {
                    let result = delegate_failure_result(
                        Some(handle.child_session_id),
                        child
                            .runs
                            .get(&handle.child_run_id)
                            .and_then(|run| run.final_text.as_deref())
                            .unwrap_or("child run failed or was interrupted"),
                    );
                    self.clear_delegate_admissions(handle.invocation_id);
                    return Ok(result);
                }
            }
        }
    }

    /// Cancels the child run and returns the structured delegate cancellation
    /// result used by the parent tool invocation.
    pub async fn cancel_delegate(&self, handle: DelegateHandle) -> Result<ToolResult, EngineError> {
        let child = match self.inner.store.get(handle.child_session_id) {
            Ok(child) => child,
            Err(_) => {
                return Ok(delegate_failure_result(
                    Some(handle.child_session_id),
                    "child session is missing",
                ));
            }
        };
        if !matches!(child.status, SessionStatus::Running | SessionStatus::Idle) {
            return self.await_delegate(handle).await;
        }
        let _ = self.cancel_run(handle.child_run_id).await;
        self.await_delegate(handle).await
    }

    async fn journal_get(
        &self,
        invocation_id: InvocationId,
    ) -> Result<Option<journal::JournalEntry>, EngineError> {
        let journal = self.inner.journal.clone();
        self.spawn_admission_blocking(move || Ok::<_, EngineError>(journal.get(invocation_id)))
            .await
    }

    fn clear_delegate_admissions(&self, invocation_id: InvocationId) {
        self.inner
            .inflight_delegations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&invocation_id);
    }

    async fn resolve_delegate_failure_if_pending(
        &self,
        session_id: SessionId,
        run_id: RunId,
        tool_call_id: ToolCallId,
        result: ToolResult,
    ) -> Result<bool, EngineError> {
        self.request(session_id, |reply| {
            SessionCommand::ResolveDelegateFailureIfPending {
                run: run_id,
                tool_call_id,
                result,
                reply,
            }
        })
        .await
    }

    fn resolve_delegate_failure_if_pending_direct(
        &self,
        session_id: SessionId,
        run_id: RunId,
        tool_call_id: ToolCallId,
        result: ToolResult,
    ) -> Result<bool, EngineError> {
        let pending = self
            .inner
            .store
            .get(session_id)?
            .runs
            .get(&run_id)
            .is_some_and(|run| {
                run.pending_calls
                    .get(&tool_call_id)
                    .is_some_and(|tool| tool == "delegate")
            });
        if !pending {
            return Ok(false);
        }
        self.append_direct(
            session_id,
            Some(run_id),
            Event::ToolCallTerminated {
                termination: ToolCallTermination {
                    tool_call_id,
                    owner: self.tool_call_owner(session_id, run_id, tool_call_id)?,
                    outcome: ToolTerminationOutcome::Failed,
                    result: Some(result),
                    error: Some(SafeToolError {
                        code: safe_code("delegate_failed"),
                        message: safe_error("delegated run failed"),
                    }),
                },
            },
        )?;
        Ok(true)
    }

    fn resolve_abandoned_delegate_failure_if_pending_direct(
        &self,
        invocation_id: InvocationId,
        generation: u64,
        session_id: SessionId,
        run_id: RunId,
        tool_call_id: ToolCallId,
        result: ToolResult,
    ) -> Result<bool, EngineError> {
        let mut admissions = self
            .inner
            .inflight_delegations
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let still_abandoned = admissions.get(&invocation_id).is_some_and(|entries| {
            entries
                .get(&generation)
                .is_some_and(|entry| entry.cancelled)
                && entries.values().all(|entry| entry.cancelled)
        });
        if !still_abandoned {
            return Ok(false);
        }
        let resolved = self.resolve_delegate_failure_if_pending_direct(
            session_id,
            run_id,
            tool_call_id,
            result,
        )?;
        admissions.remove(&invocation_id);
        Ok(resolved)
    }

    async fn ensure_delegate_run(
        &self,
        entry: &journal::JournalEntry,
        admission: Option<(InvocationId, u64)>,
    ) -> Result<RunId, EngineError> {
        if admission.is_some_and(|(invocation_id, generation)| {
            !self.admission_generation_live(invocation_id, generation)
        }) {
            return Err(EngineError::MissingTool(
                "delegate cancelled before child start".into(),
            ));
        }
        let child = self.inner.store.get(entry.reservation.child_session_id)?;
        if let Some((invocation_id, generation)) = admission {
            self.mark_admission_starting(invocation_id, generation)?;
        }
        if let Some(run_id) = entry.child_run_id
            && child.runs.contains_key(&run_id)
        {
            if let Some((invocation_id, generation)) = admission {
                self.publish_admission_run(
                    invocation_id,
                    generation,
                    entry.reservation.child_session_id,
                    run_id,
                )?;
                if !self.admission_generation_live(invocation_id, generation) {
                    self.sweep_abandoned_admission(invocation_id, generation)
                        .await?;
                    return Err(EngineError::MissingTool(
                        "delegate cancelled before child start".into(),
                    ));
                }
            }
            return Ok(run_id);
        }
        let client_run_id = delegate_client_run_id(entry.reservation.invocation_id);
        let existing_run = child
            .runs
            .values()
            .find(|run| run.client_run_id == client_run_id)
            .map(|run| run.id);
        let run_id = match existing_run {
            Some(run_id) => run_id,
            None => match self
                .request(entry.reservation.child_session_id, |reply| {
                    SessionCommand::Start {
                        params: RunStartParams {
                            session_id: entry.reservation.child_session_id,
                            client_run_id: client_run_id.clone(),
                            selection: child.meta.creation_selection.clone(),
                            input: render_delegate_input(&entry.request),
                        },
                        admission,
                        reply,
                    }
                })
                .await
            {
                Ok(started) => started.run_id,
                Err(error) => {
                    if let Some((invocation_id, generation)) = admission {
                        self.clear_admission_starting(invocation_id, generation);
                        if !self.admission_generation_live(invocation_id, generation) {
                            self.sweep_abandoned_admission(invocation_id, generation)
                                .await?;
                        }
                    }
                    return Err(error);
                }
            },
        };
        let cancelled = if let Some((invocation_id, generation)) = admission {
            // The Start actor published a newly-created run before delivering
            // its reply. Existing local runs reach the same state here.
            self.publish_admission_run(
                invocation_id,
                generation,
                entry.reservation.child_session_id,
                run_id,
            )?;
            !self.admission_generation_live(invocation_id, generation)
        } else {
            false
        };
        // The actor-owned confirmation must run even after its observer drops:
        // the start reply may already have created the child run.
        #[cfg(test)]
        let confirmation_hook = self
            .inner
            .admission_confirmation_hook
            .lock()
            .expect("admission confirmation hook lock poisoned")
            .clone();
        #[cfg(test)]
        if let Some(hook) = confirmation_hook {
            let _ = hook.reached.send(());
            hook.release.wait().await;
        }
        let journal = self.inner.journal.clone();
        let invocation_id = entry.reservation.invocation_id;
        let confirmation = self
            .spawn_admission_blocking(move || journal.mark_run_started(invocation_id, run_id))
            .await;
        if let Err(error) = confirmation {
            // A failed confirmation may have poisoned the sole journal writer.
            // The child already has an active run, so terminally cancel it before
            // the caller resolves the parent through its actor.
            let _ = self.cancel_run_durably(
                run_id,
                Some("delegate journal run confirmation failed".into()),
            );
            return Err(error);
        }
        if cancelled {
            if let Some((invocation_id, generation)) = admission {
                self.sweep_abandoned_admission(invocation_id, generation)
                    .await?;
            }
            return Err(EngineError::MissingTool(
                "delegate cancelled during child start".into(),
            ));
        }
        Ok(run_id)
    }

    pub async fn start_run(&self, params: RunStartParams) -> Result<RunStartResult, EngineError> {
        let session = params.session_id;
        self.request(session, |reply| SessionCommand::Start {
            params,
            admission: None,
            reply,
        })
        .await
    }

    /// Synchronous setup/CLI wrapper. Do not call from a Tokio runtime.
    pub fn start_run_blocking(
        &self,
        params: RunStartParams,
    ) -> Result<RunStartResult, EngineError> {
        let session = params.session_id;
        self.request_blocking(session, |reply| SessionCommand::Start {
            params,
            admission: None,
            reply,
        })
    }

    pub async fn steer(&self, run_id: RunId, input: String) -> Result<RunSteerResult, EngineError> {
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        self.request(active.session, |reply| SessionCommand::Steer {
            run: run_id,
            input,
            reply,
        })
        .await
    }

    /// Synchronous setup/CLI wrapper. Do not call from a Tokio runtime.
    pub fn steer_blocking(
        &self,
        run_id: RunId,
        input: String,
    ) -> Result<RunSteerResult, EngineError> {
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        self.request_blocking(active.session, |reply| SessionCommand::Steer {
            run: run_id,
            input,
            reply,
        })
    }

    pub async fn cancel_run(&self, run_id: RunId) -> Result<RunCancelResult, EngineError> {
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        let result = self
            .request(active.session, |reply| SessionCommand::Cancel {
                run: run_id,
                reply,
            })
            .await?;
        let inflight_runs: Vec<_> = {
            let mut inflight = self
                .inner
                .inflight_delegations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inflight
                .values_mut()
                .flat_map(|entries| entries.values_mut())
                .filter(|delegate| delegate.parent_run_id == run_id)
                .filter_map(|delegate| {
                    delegate.cancelled = true;
                    delegate.child_run_id
                })
                .collect()
        };
        let journal = self.inner.journal.clone();
        let children = self
            .spawn_admission_blocking(move || Ok::<_, EngineError>(journal.entries()))
            .await?;
        let mut pending = vec![run_id];
        pending.extend(inflight_runs);
        let mut visited = HashSet::new();
        while let Some(parent_run_id) = pending.pop() {
            if !visited.insert(parent_run_id) {
                continue;
            }
            let inflight_children: Vec<_> = {
                let mut inflight = self
                    .inner
                    .inflight_delegations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                inflight
                    .values_mut()
                    .flat_map(|entries| entries.values_mut())
                    .filter(|delegate| delegate.parent_run_id == parent_run_id)
                    .filter_map(|delegate| {
                        delegate.cancelled = true;
                        delegate.child_run_id
                    })
                    .collect()
            };
            pending.extend(inflight_children);
            for child_run_id in children
                .iter()
                .filter(|entry| entry.reservation.parent_run_id == parent_run_id)
                .filter_map(|entry| entry.child_run_id)
            {
                pending.push(child_run_id);
                if child_run_id == run_id {
                    continue;
                }
                let child_active = {
                    self.inner
                        .active
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&child_run_id)
                        .cloned()
                };
                if let Some(child_active) = child_active {
                    child_active.cancellation.cancel();
                    let _ = self
                        .request(child_active.session, |reply| SessionCommand::Cancel {
                            run: child_run_id,
                            reply,
                        })
                        .await;
                }
            }
        }
        Ok(result)
    }

    /// Cancels an active run and commits its terminal event under a per-run
    /// gate. The run loop observes the same gate, so concurrent cancellation
    /// paths cannot append two `RunCancelled` records.
    fn cancel_run_durably(
        &self,
        run_id: RunId,
        reason: Option<String>,
    ) -> Result<bool, EngineError> {
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned();
        let Some(active) = active else {
            let session = self
                .inner
                .store
                .all()
                .into_iter()
                .find(|session| session.runs.contains_key(&run_id))
                .ok_or(EngineError::MissingRun(run_id))?;
            let mut committed = false;
            return self.commit_run_cancelled_with_retry(
                session.meta.session_id,
                run_id,
                reason,
                &mut committed,
            );
        };
        active.cancellation.cancel();
        active
            .stdin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let mut committed = active
            .cancelled_committed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.commit_run_cancelled_with_retry(active.session, run_id, reason, &mut committed)
    }

    fn append_run_cancelled_once(
        &self,
        active: &ActiveRun,
        run_id: RunId,
        reason: Option<String>,
    ) -> Result<bool, EngineError> {
        let mut committed = active
            .cancelled_committed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.commit_run_cancelled_with_retry(active.session, run_id, reason, &mut committed)
    }

    fn commit_run_cancelled_with_retry(
        &self,
        session: SessionId,
        run_id: RunId,
        reason: Option<String>,
        committed: &mut bool,
    ) -> Result<bool, EngineError> {
        let mut last_error = None;
        for _ in 0..3 {
            match self.commit_run_cancelled_once(session, run_id, reason.clone(), committed) {
                Ok(result) => return Ok(result),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.expect("cancellation retry attempts are nonempty"))
    }

    fn commit_run_cancelled_once(
        &self,
        session: SessionId,
        run_id: RunId,
        reason: Option<String>,
        committed: &mut bool,
    ) -> Result<bool, EngineError> {
        if *committed {
            return Ok(false);
        }
        // `append_direct` can append to the log before a projection/cache
        // refresh fails. The event log is authoritative in that window.
        if self.run_cancelled_recorded(session, run_id)? {
            *committed = true;
            return Ok(false);
        }
        if self
            .inner
            .store
            .get(session)?
            .runs
            .get(&run_id)
            .is_none_or(|run| run.status != SessionStatus::Running)
        {
            return Ok(false);
        }
        match self.append_direct(
            session,
            Some(run_id),
            Event::RunCancelled {
                reason: reason.as_deref().map(safe_error),
            },
        ) {
            Ok(()) => {
                *committed = true;
                Ok(true)
            }
            Err(error) => {
                if self.run_cancelled_recorded(session, run_id)? {
                    *committed = true;
                    Ok(true)
                } else {
                    Err(error)
                }
            }
        }
    }

    fn run_cancelled_recorded(
        &self,
        session: SessionId,
        run_id: RunId,
    ) -> Result<bool, EngineError> {
        Ok(self
            .inner
            .store
            .get(session)?
            .log
            .events()
            .iter()
            .any(|event| {
                event.run_id == Some(run_id) && matches!(event.payload, Event::RunCancelled { .. })
            }))
    }

    pub async fn tool_stdin(
        &self,
        params: RunToolStdinParams,
    ) -> Result<RunToolStdinResult, EngineError> {
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&params.run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(params.run_id))?;
        self.request(active.session, |reply| SessionCommand::Stdin {
            params,
            reply,
        })
        .await
    }

    pub async fn subscribe(
        &self,
        session: SessionId,
        cursor: Option<u64>,
    ) -> Result<
        (
            EventsSubscribeResult,
            mpsc::Receiver<EventSubscriptionMessage>,
        ),
        EngineError,
    > {
        self.request(session, |reply| SessionCommand::Subscribe { cursor, reply })
            .await
    }

    /// Subscribes to a currently running call's retained output and live tail.
    /// Output is ephemeral and intentionally separate from event cursors.
    pub fn subscribe_tool_output(
        &self,
        call: ToolCallId,
        stream: cookie_agent_protocol::OutputStream,
    ) -> Option<(
        cookie_agent_protocol::OutputSnapshot,
        mpsc::Receiver<events::OutputMessage>,
    )> {
        self.inner
            .output_hubs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&call)
            .cloned()
            .map(|hub| hub.subscribe(stream, 256))
    }

    fn retain_finalized_output_hub(&self, call: ToolCallId) {
        const FINALIZED_HUB_RETENTION: usize = 128;
        let mut finalized = self
            .inner
            .finalized_output_hubs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        finalized.push_back(call);
        if finalized.len() > FINALIZED_HUB_RETENTION
            && let Some(expired) = finalized.pop_front()
        {
            self.inner
                .output_hubs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&expired);
        }
    }

    #[must_use]
    pub fn list_sessions(&self) -> Vec<SessionMeta> {
        self.inner
            .store
            .all()
            .into_iter()
            .map(|session| session.meta)
            .collect()
    }
    pub fn get_session(&self, id: SessionId) -> Result<SessionMeta, EngineError> {
        Ok(self.inner.store.get(id)?.meta)
    }
    pub fn delegate_targets(&self, id: SessionId) -> Result<Vec<AgentId>, EngineError> {
        let session = self.inner.store.get(id)?;
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .find(|active| active.session == id)
            .cloned();
        let Some(active) = active else {
            return Ok(Vec::new());
        };
        let depth = session_depth(&session.meta.origin);
        let Some(delegation) = &active.policy.agent.delegation else {
            return Ok(Vec::new());
        };
        if depth >= delegation.effective_depth_ceiling {
            return Ok(Vec::new());
        }
        Ok(delegation
            .targets
            .iter()
            .filter(|target| {
                active.policy.registry.get(target).is_some_and(|agent| {
                    agent.document.frontmatter.enabled
                        && matches!(
                            agent.document.frontmatter.mode,
                            cookie_agent_config::AgentMode::Subagent
                                | cookie_agent_config::AgentMode::All
                        )
                })
            })
            .cloned()
            .collect())
    }
    #[must_use]
    pub fn children(&self, id: SessionId) -> Vec<cookie_agent_protocol::ChildSummary> {
        let known: HashSet<_> = self
            .inner
            .journal
            .entries()
            .into_iter()
            .map(|entry| {
                (
                    entry.reservation.invocation_id,
                    entry.reservation.parent_session_id,
                    entry.reservation.child_session_id,
                )
            })
            .collect();
        self.inner
            .store
            .all()
            .into_iter()
            .filter_map(|child| match child.meta.origin {
                SessionOrigin::Delegated {
                    parent_session_id,
                    invocation_id,
                    ..
                } if parent_session_id == id
                    && known.contains(&(
                        invocation_id,
                        parent_session_id,
                        child.meta.session_id,
                    )) =>
                {
                    Some(ChildSummary {
                        session_id: child.meta.session_id,
                        agent: child.meta.creation_selection.agent.clone(),
                        title: child.meta.title.clone(),
                        title_updated_seq: child.meta.title_updated_seq,
                        status: child.status,
                        usage: child.usage,
                    })
                }
                _ => None,
            })
            .collect()
    }
    pub fn tree(&self, id: SessionId) -> Result<cookie_agent_protocol::SessionTree, EngineError> {
        Ok(cookie_agent_protocol::SessionTree {
            session: self.inner.store.get(id)?.meta,
            children: self
                .children(id)
                .into_iter()
                .map(|child| self.tree(child.session_id))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
    pub async fn resume(&self, id: SessionId) -> Result<SessionMeta, EngineError> {
        self.request(id, |reply| SessionCommand::Resume { reply })
            .await
    }
    pub async fn rename_session(
        &self,
        params: SessionRenameParams,
    ) -> Result<SessionRenameResult, EngineError> {
        let session_id = params.session_id;
        let reset = matches!(params.change, SessionRenameChange::Reset);
        let mut result = self
            .request(session_id, |reply| SessionCommand::Rename { params, reply })
            .await?;
        if reset {
            self.generate_title_after_reset(session_id).await?;
            result.session = self.inner.store.get(session_id)?.meta;
        }
        Ok(result)
    }
    #[must_use]
    pub fn list_agents(&self) -> AgentListResult {
        let snapshot = self.inner.model_manager.current();
        let agents = self
            .materialize_agents(snapshot.model_set())
            .expect("current model snapshot resolves authored agents")
            .descriptors()
            .into_iter()
            .map(wire_agent_descriptor)
            .collect::<Vec<_>>();
        let revision = cookie_agent_protocol::SnapshotRevision::new(format!(
            "sha256:{}",
            Sha256Digest::of_bytes(
                &serde_json::to_vec(&agents).expect("agent descriptors serialize")
            )
            .as_str()
        ))
        .expect("valid agent revision");
        AgentListResult {
            revision,
            model_revision: cookie_agent_protocol::SnapshotRevision::new(snapshot.revision())
                .expect("model manager revision is validated"),
            generated_at: snapshot
                .generated_at()
                .parse()
                .expect("model snapshot timestamp is valid"),
            agents,
        }
    }

    pub async fn append(
        &self,
        session: SessionId,
        run: Option<RunId>,
        event: Event,
    ) -> Result<(), EngineError> {
        self.request(session, |reply| SessionCommand::Append {
            run,
            event,
            reply,
        })
        .await
    }

    /// Synchronous setup/CLI wrapper. Do not call from a Tokio runtime.
    pub fn append_blocking(
        &self,
        session: SessionId,
        run: Option<RunId>,
        event: Event,
    ) -> Result<(), EngineError> {
        self.request_blocking(session, |reply| SessionCommand::Append {
            run,
            event,
            reply,
        })
    }

    /// Commits a completed tool invocation through its session actor.
    pub async fn submit_tool_result(
        &self,
        session: SessionId,
        run: RunId,
        tool_call_id: ToolCallId,
        result: Result<ToolResult, String>,
    ) -> Result<(), EngineError> {
        let result = result.map_err(|message| ToolFailure {
            code: ToolCallFailureCode::ExecutionFailed,
            message,
        });
        self.submit_tool_result_status(session, run, tool_call_id, result)
            .await
            .and_then(|committed| {
                committed.then_some(()).ok_or_else(|| {
                    EngineError::MissingTool("tool call is no longer pending".into())
                })
            })
    }

    async fn submit_tool_result_status(
        &self,
        session: SessionId,
        run: RunId,
        tool_call_id: ToolCallId,
        result: Result<ToolResult, ToolFailure>,
    ) -> Result<bool, EngineError> {
        self.request(session, |reply| SessionCommand::ToolResult {
            run,
            tool_call_id,
            result,
            reply,
        })
        .await
    }

    fn append_direct(
        &self,
        session: SessionId,
        run: Option<RunId>,
        event: Event,
    ) -> Result<(), EngineError> {
        let envelope = self.inner.store.get(session)?.log.append(run, event)?;
        self.inner.store.update(session)?;
        self.inner
            .subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(session)
            .or_default()
            .retain_mut(|subscriber| {
                // Reserve one queue slot for a control message. Once the
                // event capacity is reached, queue a gap and close this live
                // subscription; the gap is delivered even if this event is
                // terminal, and the client resumes from `last_delivered_seq`.
                let is_gap = subscriber.sender.capacity() <= 1;
                let message = if is_gap {
                    EventSubscriptionMessage::Gap {
                        session_id: session,
                        last_delivered_seq: envelope.seq.saturating_sub(1),
                    }
                } else {
                    EventSubscriptionMessage::Event {
                        event: Box::new(envelope.clone()),
                    }
                };
                match subscriber.sender.try_send(message) {
                    Ok(()) => {
                        #[cfg(test)]
                        if is_gap
                            && let Some(hook) = self
                                .inner
                                .gap_send_hook
                                .lock()
                                .expect("gap send hook lock poisoned")
                                .take()
                        {
                            let _ = hook.reached.send(());
                            let _ = hook.release.recv();
                        }
                        !is_gap
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => false,
                    Err(mpsc::error::TrySendError::Closed(_)) => false,
                }
            });
        Ok(())
    }

    async fn request<T>(
        &self,
        session: SessionId,
        command: impl FnOnce(oneshot::Sender<Result<T, EngineError>>) -> SessionCommand,
    ) -> Result<T, EngineError> {
        let actor = self
            .inner
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session)
            .cloned()
            .ok_or(EngineError::MissingActor(session))?;
        let (reply, receiver) = oneshot::channel();
        actor
            .send(command(reply))
            .await
            .map_err(|_| EngineError::ActorStopped)?;
        receiver.await.map_err(|_| EngineError::ActorStopped)?
    }

    async fn prompt_events(
        &self,
        session: SessionId,
        run: RunId,
    ) -> Result<Vec<StoredEvent>, EngineError> {
        let events = self
            .request(session, |reply| SessionCommand::PromptSnapshot {
                run,
                reply,
            })
            .await?;
        #[cfg(test)]
        if let Some(hook) = {
            self.inner
                .prompt_snapshot_hook
                .lock()
                .expect("prompt snapshot hook lock poisoned")
                .take()
        } {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("prompt snapshot reached lock poisoned")
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
        Ok(events)
    }

    fn request_blocking<T>(
        &self,
        session: SessionId,
        command: impl FnOnce(oneshot::Sender<Result<T, EngineError>>) -> SessionCommand,
    ) -> Result<T, EngineError> {
        let actor = self
            .inner
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session)
            .cloned()
            .ok_or(EngineError::MissingActor(session))?;
        let (reply, receiver) = oneshot::channel();
        actor
            .blocking_send(command(reply))
            .map_err(|_| EngineError::ActorStopped)?;
        receiver
            .blocking_recv()
            .map_err(|_| EngineError::ActorStopped)?
    }

    fn spawn_actor(&self, session: SessionId) {
        if self
            .inner
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&session)
        {
            return;
        }
        let engine = self.clone();
        let actor = SessionActor::spawn(SESSION_MAILBOX_CAPACITY, move |command| {
            let engine = engine.clone();
            async move { engine.handle_actor_command(session, command).await }
        });
        self.inner
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session, actor);
    }

    async fn handle_actor_command(&self, session: SessionId, command: SessionCommand) {
        match command {
            SessionCommand::Append { run, event, reply } => {
                let _ = reply.send(self.append_direct(session, run, event));
            }
            SessionCommand::EnsureToolCallLinked {
                run,
                tool_call_id,
                child_session_id,
                reply,
            } => {
                let result = (|| {
                    let linked = self
                        .inner
                        .store
                        .get(session)?
                        .log
                        .events()
                        .iter()
                        .any(|event| {
                            matches!(event.payload, Event::ToolCallLinked { tool_call_id: linked_call, child_session_id: linked_child }
                                if linked_call == tool_call_id && linked_child == child_session_id)
                        });
                    if !linked {
                        self.append_direct(
                            session,
                            Some(run),
                            Event::ToolCallLinked {
                                tool_call_id,
                                child_session_id,
                            },
                        )?;
                    }
                    Ok(())
                })();
                let _ = reply.send(result);
            }
            SessionCommand::Start {
                params,
                admission,
                reply,
            } => {
                let child_session_id = params.session_id;
                let mut result = self.start_run_direct(params, admission).await;
                if let (Some((invocation_id, generation)), Ok(started)) =
                    (admission, result.as_ref())
                    && let Err(error) = self.publish_admission_run(
                        invocation_id,
                        generation,
                        child_session_id,
                        started.run_id,
                    )
                {
                    let _ = self.cancel_run_durably(
                        started.run_id,
                        Some("delegate admission could not be published".into()),
                    );
                    result = Err(error);
                }
                let started = result.as_ref().ok().map(|result| result.run_id);
                if reply.send(result).is_err()
                    && admission.is_some()
                    && let Some(run_id) = started
                {
                    let _ = self.cancel_run_durably(
                        run_id,
                        Some("delegate admission reply abandoned".into()),
                    );
                }
            }
            SessionCommand::Steer { run, input, reply } => {
                let result = self
                    .inner
                    .active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&run)
                    .cloned()
                    .filter(|active| active.session == session)
                    .ok_or(EngineError::MissingRun(run))
                    .and_then(|_| {
                        let projection = self.inner.store.get(session)?;
                        let accepting = projection
                            .runs
                            .get(&run)
                            .is_some_and(|run| run.status == SessionStatus::Running);
                        if !accepting {
                            return Ok(RunSteerResult { accepted: false });
                        }
                        self.append_direct(
                            session,
                            Some(run),
                            Event::UserInputSubmitted { input },
                        )?;
                        Ok(RunSteerResult { accepted: true })
                    });
                let _ = reply.send(result);
            }
            SessionCommand::Cancel { run, reply } => {
                let result = (|| {
                    let active = self
                        .inner
                        .active
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&run)
                        .cloned()
                        .filter(|active| active.session == session)
                        .ok_or(EngineError::MissingRun(run))?;
                    active.cancellation.cancel();
                    active
                        .stdin
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clear();
                    let events = self.inner.store.get(session)?.log.events();
                    let pending = approval_records(session, &events)
                        .into_values()
                        .filter(|record| {
                            matches!(
                                record.status,
                                ApprovalStatus::Pending | ApprovalStatus::Escalated
                            ) && approval_run_id(&events, record.request.approval_id()) == Some(run)
                        })
                        .map(|record| record.request.approval_id())
                        .collect::<Vec<_>>();
                    for approval_id in pending {
                        self.approval_terminal_direct(
                            session,
                            run,
                            approval_id,
                            ApprovalTerminal::Cancelled,
                        )?;
                    }
                    Ok(RunCancelResult { cancelled: true })
                })();
                let _ = reply.send(result);
            }
            SessionCommand::Stdin { params, reply } => {
                let result = (|| {
                    let active = self
                        .inner
                        .active
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&params.run_id)
                        .cloned()
                        .filter(|active| active.session == session)
                        .ok_or(EngineError::MissingRun(params.run_id))?;
                    let data = params
                        .data
                        .map(|encoded| STANDARD.decode(encoded))
                        .transpose()?
                        .unwrap_or_default();
                    let sender = active
                        .stdin
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&params.call_id)
                        .cloned()
                        .ok_or(EngineError::StdinUnavailable)?;
                    sender
                        .try_send(StdinWrite {
                            data: data.clone(),
                            eof: params.eof,
                        })
                        .map_err(|_| EngineError::StdinUnavailable)?;
                    if params.eof {
                        active
                            .stdin
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&params.call_id);
                    }
                    self.append_direct(
                        session,
                        Some(params.run_id),
                        Event::ToolStdinSubmitted {
                            tool_call_id: params.call_id,
                            byte_count: data.len() as u64,
                        },
                    )?;
                    Ok(RunToolStdinResult { accepted: true })
                })();
                let _ = reply.send(result);
            }
            SessionCommand::Subscribe { cursor, reply } => {
                // Snapshot and registration share the actor turn, so appends
                // cannot land in the cursor-to-live handoff gap.
                let result = self.inner.store.get(session).map(|projection| {
                    let events = projection
                        .log
                        .events()
                        .into_iter()
                        .filter(|event| cursor.is_none_or(|cursor| event.seq > cursor))
                        .collect();
                    let (sender, receiver) = mpsc::channel(PERSISTED_SUBSCRIBER_QUEUE_CAPACITY);
                    self.inner
                        .subscribers
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .entry(session)
                        .or_default()
                        .push(PersistedSubscriber { sender });
                    (EventsSubscribeResult { events }, receiver)
                });
                let _ = reply.send(result.map_err(EngineError::from));
            }
            SessionCommand::Resume { reply } => {
                let result = self
                    .resolve_interrupted_direct(session)
                    .await
                    .and_then(|()| Ok(self.inner.store.get(session)?.meta));
                let _ = reply.send(result);
            }
            SessionCommand::Rename { params, reply } => {
                let result = (|| {
                    let projection = self.inner.store.get(session)?;
                    if let Some(record) = projection.rename_records.get(&params.client_rename_id) {
                        if record.conflicts_with(&params) {
                            return Err(EngineError::RenameConflict);
                        }
                        return Ok(SessionRenameResult {
                            client_rename_id: params.client_rename_id,
                            session: projection.meta,
                        });
                    }
                    let commit = match params.change {
                        SessionRenameChange::Set { title } => SessionTitleChange::UserSet {
                            title,
                            client_rename_id: params.client_rename_id.clone(),
                        },
                        SessionRenameChange::Clear => SessionTitleChange::UserClear {
                            client_rename_id: params.client_rename_id.clone(),
                        },
                        SessionRenameChange::Reset => SessionTitleChange::UserReset {
                            client_rename_id: params.client_rename_id.clone(),
                        },
                    };
                    let input_through_seq =
                        projection.log.events().last().map_or(0, |event| event.seq);
                    self.append_direct(
                        session,
                        None,
                        Event::SessionTitleCommitted {
                            input_through_seq,
                            change: commit,
                        },
                    )?;
                    Ok(SessionRenameResult {
                        client_rename_id: params.client_rename_id,
                        session: self.inner.store.get(session)?.meta,
                    })
                })();
                let _ = reply.send(result);
            }
            SessionCommand::ApprovalRespond { params, reply } => {
                let _ = reply.send(self.approval_respond_direct(params));
            }
            SessionCommand::ApprovalCapabilityInvalid {
                params,
                invalidation,
                reply,
            } => {
                let _ = reply.send(self.approval_capability_invalid_direct(params, invalidation));
            }
            SessionCommand::ApprovalEvaluationComplete {
                run,
                request,
                executor,
                decision,
                cancelled,
                reply,
            } => {
                let _ = reply.send(self.approval_evaluation_complete_direct(
                    session, run, request, executor, decision, cancelled,
                ));
            }
            SessionCommand::ApprovalTerminal {
                run,
                approval_id,
                terminal,
                reply,
            } => {
                let _ =
                    reply.send(self.approval_terminal_direct(session, run, approval_id, terminal));
            }
            SessionCommand::ToolResult {
                run,
                tool_call_id,
                result,
                reply,
            } => {
                let pending = self
                    .inner
                    .store
                    .get(session)
                    .ok()
                    .and_then(|projection| projection.runs.get(&run).cloned())
                    .is_some_and(|run| run.pending_calls.contains_key(&tool_call_id));
                let response = if !pending {
                    Ok(false)
                } else {
                    (|| {
                        let owner = self.tool_call_owner(session, run, tool_call_id)?;
                        let event = match result {
                            Ok(result) => Event::ToolCallTerminated {
                                termination: ToolCallTermination {
                                    tool_call_id,
                                    owner,
                                    outcome: ToolTerminationOutcome::Completed,
                                    result: Some(result),
                                    error: None,
                                },
                            },
                            Err(failure) => Event::ToolCallTerminated {
                                termination: ToolCallTermination {
                                    tool_call_id,
                                    owner,
                                    outcome: ToolTerminationOutcome::Failed,
                                    result: None,
                                    error: Some(SafeToolError {
                                        code: failure.code.safe_code(),
                                        message: safe_error(&failure.message),
                                    }),
                                },
                            },
                        };
                        self.append_direct(session, Some(run), event)?;
                        Ok(true)
                    })()
                };
                let _ = reply.send(response);
            }
            SessionCommand::ResolveDelegateFailureIfPending {
                run,
                tool_call_id,
                result,
                reply,
            } => {
                let result = self.resolve_delegate_failure_if_pending_direct(
                    session,
                    run,
                    tool_call_id,
                    result,
                );
                let _ = reply.send(result);
            }
            SessionCommand::ResolveAbandonedDelegateFailureIfPending {
                invocation_id,
                generation,
                run,
                tool_call_id,
                result,
                reply,
            } => {
                let result = self.resolve_abandoned_delegate_failure_if_pending_direct(
                    invocation_id,
                    generation,
                    session,
                    run,
                    tool_call_id,
                    result,
                );
                let _ = reply.send(result);
            }
            SessionCommand::CompleteIfNoSteering {
                run,
                final_text,
                reply,
            } => {
                let result = self
                    .inner
                    .active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&run)
                    .cloned()
                    .filter(|active| active.session == session)
                    .ok_or(EngineError::MissingRun(run))
                    .and_then(|active| {
                        let prompt_seq = active.prompt_seq.load(Ordering::Acquire);
                        let has_unseen_steering = self
                            .inner
                            .store
                            .get(session)?
                            .log
                            .events()
                            .iter()
                            .any(|event| {
                                event.seq > prompt_seq
                                    && event.run_id == Some(run)
                                    && matches!(event.payload, Event::UserInputSubmitted { .. })
                            });
                        if !has_unseen_steering {
                            self.append_direct(
                                session,
                                Some(run),
                                Event::RunCompleted { final_text },
                            )?;
                            Ok(false)
                        } else {
                            Ok(true)
                        }
                    });
                let _ = reply.send(result);
            }
            SessionCommand::PromptSnapshot { run, reply } => {
                let result = self
                    .inner
                    .active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&run)
                    .cloned()
                    .filter(|active| active.session == session)
                    .ok_or(EngineError::MissingRun(run))
                    .and_then(|active| {
                        let events = self.inner.store.get(session)?.log.events();
                        let applied: HashSet<u64> = events
                            .iter()
                            .filter_map(|event| match &event.payload {
                                Event::UserInputApplied { user_input_seq }
                                    if event.run_id == Some(run) =>
                                {
                                    Some(*user_input_seq)
                                }
                                _ => None,
                            })
                            .collect();
                        for user_input_seq in
                            events.iter().filter_map(|event| match &event.payload {
                                Event::UserInputSubmitted { .. }
                                    if event.run_id == Some(run)
                                        && !applied.contains(&event.seq) =>
                                {
                                    Some(event.seq)
                                }
                                _ => None,
                            })
                        {
                            self.append_direct(
                                session,
                                Some(run),
                                Event::UserInputApplied { user_input_seq },
                            )?;
                        }
                        let events = self.inner.store.get(session)?.log.events();
                        active.prompt_seq.store(
                            events.last().map_or(0, |event| event.seq),
                            Ordering::Release,
                        );
                        Ok(events)
                    });
                let _ = reply.send(result);
            }
        }
    }

    async fn start_run_direct(
        &self,
        params: RunStartParams,
        admission: Option<(InvocationId, u64)>,
    ) -> Result<RunStartResult, EngineError> {
        if let Some((invocation_id, generation)) = admission
            && !self.admission_generation_live(invocation_id, generation)
        {
            return Err(EngineError::MissingTool(
                "delegate admission was abandoned or superseded".into(),
            ));
        }
        let session = self.inner.store.get(params.session_id)?;
        if let Some(run) = session
            .runs
            .values()
            .find(|run| run.client_run_id == params.client_run_id)
        {
            if run.input != params.input || run.selection != params.selection {
                return Err(EngineError::RunIdempotencyConflict);
            }
            return Ok(RunStartResult { run_id: run.id });
        }
        if session.status == SessionStatus::Running {
            return Err(EngineError::SessionRunning(params.session_id));
        }
        self.resolve_interrupted_direct(params.session_id).await?;
        let result_limits = policy::ResultLimits {
            tool_output_max_lines: self.inner.config.runtime.tool_output.max_lines,
            tool_output_max_bytes: self.inner.config.runtime.tool_output.max_bytes,
        };
        let run_policy = match &session.meta.origin {
            SessionOrigin::Root => {
                let model_snapshot = self.inner.model_manager.current();
                let agents = self.materialize_agents(model_snapshot.model_set())?;
                let agent = resolve_agent(&agents, &params.selection.agent)?;
                if !agent.runnable_as_root {
                    return Err(EngineError::IneligibleAgent(params.selection.agent));
                }
                freeze_root_agent_policy(
                    agent,
                    Arc::clone(&agents),
                    model_snapshot,
                    &params.selection.model,
                    result_limits,
                )?
            }
            SessionOrigin::Delegated { .. } => {
                let model_snapshot = self.session_model_snapshot(&session)?;
                let agents = self.materialize_agents(model_snapshot.model_set())?;
                policy_for_session_selection(
                    session.creation_agent.clone(),
                    agents,
                    model_snapshot,
                    &params.selection,
                    result_limits.tool_output_max_lines,
                    result_limits.tool_output_max_bytes,
                )?
            }
        };
        let run_id = RunId::new_v7();
        let input_through_seq = session.meta.last_event_seq;
        self.append_direct(
            params.session_id,
            Some(run_id),
            Event::RunStarted {
                client_run_id: params.client_run_id.clone(),
                selection: params.selection.clone(),
                agent: Box::new(run_policy.agent.clone()),
                selected_suffix: run_policy.selected_suffix_wire.clone(),
                input_through_seq,
            },
        )?;
        self.append_direct(
            params.session_id,
            Some(run_id),
            Event::UserInputSubmitted {
                input: params.input,
            },
        )?;
        self.inner
            .run_model_snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id, Arc::clone(&run_policy.model_snapshot));
        let active = Arc::new(ActiveRun {
            session: params.session_id,
            policy: Arc::new(run_policy),
            cancellation: CancellationToken::new(),
            cancelled_committed: Mutex::new(false),
            stdin: Mutex::new(HashMap::new()),
            prompt_seq: AtomicU64::new(0),
            fallback_index: AtomicU64::new(0),
        });
        // A sweeper may have terminalized this durable run before active-run
        // registration. Never resurrect a cancelled run with a live loop.
        if self.run_cancelled_recorded(params.session_id, run_id)? {
            return Ok(RunStartResult { run_id });
        }
        if let Some((invocation_id, generation)) = admission
            && let Err(error) =
                self.publish_admission_run(invocation_id, generation, params.session_id, run_id)
        {
            self.inner
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(run_id, active);
            self.cancel_run_durably(run_id, Some("delegate admission publication failed".into()))?;
            return Err(error);
        }
        self.inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id, active.clone());
        if self.run_cancelled_recorded(params.session_id, run_id)? {
            self.inner
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&run_id);
            return Ok(RunStartResult { run_id });
        }
        let engine = self.clone();
        tokio::spawn(async move {
            if let Err(error) = engine.run_loop(run_id, active).await {
                // A provider-attempt persistence error may also prevent the
                // terminal append. Retain this active tombstone for reopen
                // reconciliation rather than clearing a durably Running run.
                eprintln!("run {run_id} terminalization failed: {error}");
                return;
            }
            if let Ok(mut active_runs) = engine.inner.active.lock() {
                active_runs.remove(&run_id);
            }
        });
        Ok(RunStartResult { run_id })
    }

    async fn run_loop(&self, run_id: RunId, active: Arc<ActiveRun>) -> Result<(), EngineError> {
        // Sticky chain position belongs to this run, not one agent-loop pass.
        let mut fallback_entry = 0_usize;
        loop {
            if active.cancellation.is_cancelled() {
                self.append_run_cancelled_once(&active, run_id, None)?;
                return Ok(());
            }
            let tools = self.tool_definitions(active.session, &active.policy)?;
            let prompt_events = self.prompt_events(active.session, run_id).await?;
            let attempt = match self
                .stream_attempt(
                    active.session,
                    run_id,
                    &active.cancellation,
                    &active.policy,
                    &mut fallback_entry,
                    prompt_events,
                    tools,
                )
                .await
            {
                Ok(events) => events,
                Err(error) => {
                    if active.cancellation.is_cancelled() {
                        self.append_run_cancelled_once(&active, run_id, None)?;
                        return Ok(());
                    }
                    self.append(
                        active.session,
                        Some(run_id),
                        Event::RunFailed {
                            error: safe_error(&error.to_string()),
                        },
                    )
                    .await?;
                    return Ok(());
                }
            };
            let final_text = attempt
                .turn
                .content
                .iter()
                .filter_map(|part| match part {
                    PersistedAssistantPart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            if matches!(
                attempt.turn.finish_reason,
                cookie_agent_protocol::ModelFinishReason::Cancelled
                    | cookie_agent_protocol::ModelFinishReason::Aborted
            ) {
                active.cancellation.cancel();
            }
            let in_stream_results = attempt
                .turn
                .content
                .iter()
                .filter_map(|part| match part {
                    PersistedAssistantPart::ToolResult { tool_call_id, .. } => {
                        Some(tool_call_id.as_str())
                    }
                    _ => None,
                })
                .collect::<HashSet<_>>();
            let approvals = attempt
                .turn
                .content
                .iter()
                .filter_map(|part| match part {
                    PersistedAssistantPart::ToolApproval {
                        tool_call_id,
                        message,
                        ..
                    } => Some((tool_call_id.as_str(), message.clone())),
                    _ => None,
                })
                .collect::<HashMap<_, _>>();
            let calls = attempt
                .turn
                .content
                .iter()
                .enumerate()
                .filter_map(|(content_index, part)| match part {
                    PersistedAssistantPart::ToolCall {
                        id,
                        provider_item_id,
                        name,
                        input,
                        ..
                    } if !in_stream_results.contains(id.as_str()) => Some((
                        ToolCallId::new_v7(),
                        content_index as u32,
                        id.clone(),
                        provider_item_id.clone(),
                        name.clone(),
                        input.clone(),
                        approvals.get(id.as_str()).cloned().flatten(),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if active.cancellation.is_cancelled() {
                self.append_run_cancelled_once(&active, run_id, None)?;
                return Ok(());
            }
            if calls.is_empty() {
                let steering = self
                    .request(active.session, |reply| {
                        SessionCommand::CompleteIfNoSteering {
                            run: run_id,
                            final_text: (!final_text.is_empty()).then_some(final_text.clone()),
                            reply,
                        }
                    })
                    .await?;
                if steering {
                    continue;
                }
                return Ok(());
            }
            if calls.len() > MAX_PENDING_PREPARED_TOOLS {
                return Err(ModelError::invalid_response(format!(
                    "model requested {} prepared tools; the limit is {MAX_PENDING_PREPARED_TOOLS}",
                    calls.len()
                ))
                .into());
            }
            let mut prepared = Vec::new();
            for (id, content_index, model_call_id, provider_item_id, tool, arguments, approval) in
                &calls
            {
                self.inner
                    .output_hubs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .entry(*id)
                    .or_insert_with(|| OutputHub::new(*id, 64 * 1024));
                let call = ToolCall {
                    id: *id,
                    name: tool.to_string(),
                    arguments: arguments.clone(),
                };
                let prepared_call = self
                    .prepare_tool_call(active.session, run_id, call, &active.policy)
                    .await;
                let operation_fingerprint = prepared_call.prepared.as_ref().map_or_else(
                    |_| fallback_operation_fingerprint(&prepared_call.call),
                    |prepared| OperationFingerprint::from_prepared_operation(prepared.operation()),
                );
                self.append(
                    active.session,
                    Some(run_id),
                    Event::ToolCallStarted {
                        start: ToolCallStart {
                            tool_call_id: *id,
                            owner: cookie_agent_protocol::AssistantToolCallRef {
                                model_turn_seq: attempt.model_turn_seq,
                                content_index: *content_index,
                                model_call_id: model_call_id.clone(),
                                provider_item_id: provider_item_id.clone(),
                            },
                            presentation: prepared_call.presentation.clone(),
                            operation_fingerprint,
                        },
                    },
                )
                .await?;
                prepared.push((prepared_call, approval.clone()));
            }
            let mut tasks = Vec::new();
            for (prepared, approval) in prepared {
                if prepared.prepared.is_err() {
                    let Err(error) = prepared.prepared else {
                        unreachable!()
                    };
                    tasks.push(PendingTool::ImmediateFailure(error));
                    continue;
                }
                if let Some(message) = approval {
                    let outcome = self
                        .request_model_approval(
                            &active,
                            run_id,
                            prepared
                                .prepared
                                .as_ref()
                                .expect("prepared operation")
                                .operation(),
                            &prepared
                                .prepared
                                .as_ref()
                                .expect("prepared operation")
                                .policy_labels,
                            prepared
                                .prepared
                                .as_ref()
                                .expect("prepared operation")
                                .executor
                                .clone(),
                            Some(message),
                        )
                        .await?;
                    if outcome.approved {
                        tasks.push(PendingTool::Prepared(Box::new(prepared)));
                    } else {
                        tasks.push(PendingTool::ImmediateFailure(ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message: denied_tool_failure(
                                ApprovalDecisionSource::Model,
                                "model approval refused by user",
                                outcome.feedback,
                            ),
                        }));
                    }
                } else {
                    tasks.push(PendingTool::Prepared(Box::new(prepared)));
                }
            }
            // Awaiting task handles is outside any session actor. Results are
            // committed in provider tool-call order, regardless of completion order.
            for (id, task) in calls.iter().map(|call| call.0).zip(tasks) {
                let result = if active.cancellation.is_cancelled() {
                    Err(ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: "tool call cancelled after it started".into(),
                    })
                } else {
                    match task {
                        PendingTool::Prepared(prepared) => {
                            self.execute_tool(active.clone(), run_id, *prepared).await
                        }
                        PendingTool::ImmediateFailure(failure) => Err(failure),
                    }
                };
                self.submit_tool_result_status(active.session, run_id, id, result)
                    .await?;
            }
            if active.cancellation.is_cancelled() {
                self.append_run_cancelled_once(&active, run_id, None)?;
                return Ok(());
            }
        }
    }

    /// Streams one Oven attempt directly into the session actor and commits a
    /// complete turn only after strict lifecycle validation succeeds.
    #[allow(clippy::too_many_arguments)]
    async fn stream_attempt(
        &self,
        session: SessionId,
        run: RunId,
        cancellation: &CancellationToken,
        policy: &FrozenRunPolicy,
        sticky_entry: &mut usize,
        prompt_events: Vec<StoredEvent>,
        tools: Vec<ToolDefinition>,
    ) -> Result<AttemptTurn, EngineError> {
        let chain = &policy.selected_suffix;
        let composed_prompt = &policy.agent.composed_prompt;
        let prompt_fingerprint = &policy.agent.prompt_fingerprint;
        let mut entry = *sticky_entry;
        let mut last_error = ModelError::invalid_request("model fallback chain is empty");
        let mut first_request = true;
        while entry < chain.len() {
            let binding = &chain[entry];
            let model = policy.model_snapshot.resolve(binding).map_err(|error| {
                EngineError::from(ModelError::invalid_request(error.to_string()))
            })?;
            let mut attempts = 0_u32;
            let mut context_recovery_attempted = false;
            loop {
                attempts += 1;
                let attempt_id = cookie_agent_protocol::AttemptId::new_v7();
                let attempt_ordinal = self
                    .inner
                    .store
                    .get(session)?
                    .log
                    .events()
                    .iter()
                    .filter(|event| {
                        event.run_id == Some(run)
                            && matches!(event.payload, Event::ModelAttemptStarted { .. })
                    })
                    .count() as u32
                    + 1;
                self.append(
                    session,
                    Some(run),
                    Event::ModelAttemptStarted {
                        attempt_id,
                        attempt_ordinal,
                        fallback_index: entry as u32,
                        retry_ordinal: attempts - 1,
                        resolved_model: wire_model(binding),
                        prompt_fingerprint: prompt_fingerprint.clone(),
                    },
                )
                .await?;
                let request_events = if first_request {
                    first_request = false;
                    prompt_events.clone()
                } else {
                    self.prompt_events(session, run).await?
                };
                let request_events = self
                    .maybe_compact_context(
                        session,
                        run,
                        cancellation,
                        binding,
                        &model,
                        &self.inner.internal_agents.policy(
                            InternalAgentKind::ContextCompaction,
                            policy,
                            &chain[entry..],
                        ),
                        request_events,
                        false,
                    )
                    .await?;
                let input_through_seq = request_events.last().map_or(0, |event| event.seq);
                let context = assemble_model_context(
                    &request_events,
                    &self.inner.artifacts,
                    binding,
                    composed_prompt,
                )?;
                let replay_preflight = context.replay_decisions;
                let mut request = ModelRequest::new(context.history).with_tools(tools.clone());
                if let Some(native_context) = context.native_context {
                    request = request.with_native_context(native_context);
                }
                let request = model.prepare_request(request);
                let abort = AbortBridge::new(cancellation.clone());
                let response = tokio::select! {
                    result = model.model().stream(request, abort.signal()) => result,
                    _ = cancellation.cancelled() => {
                        abort.abort();
                        Err(ModelError::abort("model request was cancelled"))
                    }
                };
                let (result, meaningful_output) = match response {
                    Ok(response) => {
                        let oven_sdk::StreamResponse {
                            mut stream,
                            request,
                            response,
                        } = response;
                        self.append(
                            session,
                            Some(run),
                            Event::ModelReplayEvaluated {
                                attempt_id,
                                resolved_model: wire_model(binding),
                                ordered_decisions: replay_decisions_with_preflight(
                                    &request.replay.decisions,
                                    binding,
                                    &replay_preflight,
                                ),
                            },
                        )
                        .await?;
                        let mut accumulator = TurnAccumulator::default();
                        let mut failure = None;
                        let mut meaningful_output = false;
                        loop {
                            let item = tokio::select! {
                                item = stream.next() => item,
                                _ = cancellation.cancelled() => {
                                    abort.abort();
                                    failure = Some(Box::new(ModelError::abort("model stream was cancelled")));
                                    break;
                                }
                            };
                            let Some(item) = item else { break };
                            match item {
                                Ok(part) => match accumulator.push(part) {
                                    Ok(effect) => {
                                        meaningful_output |= effect.meaningful;
                                        if let Some(text) = effect.text_delta {
                                            self.append(
                                                session,
                                                Some(run),
                                                Event::TextDelta { attempt_id, text },
                                            )
                                            .await?;
                                        }
                                        if let Some(text) = effect.reasoning_delta {
                                            self.append(
                                                session,
                                                Some(run),
                                                Event::ReasoningDelta { attempt_id, text },
                                            )
                                            .await?;
                                        }
                                    }
                                    Err(error) => {
                                        failure = Some(error);
                                        break;
                                    }
                                },
                                Err(error) => {
                                    failure = Some(Box::new(error));
                                    break;
                                }
                            }
                        }
                        let completed = match failure {
                            Some(error) => Err(error),
                            None => accumulator.finish(),
                        };
                        let completed = completed.map(|mut turn| {
                            for (key, value) in response.response_metadata {
                                turn.finish.response_metadata.entry(key).or_insert(value);
                            }
                            if let Some(status) = response.http_status {
                                turn.finish
                                    .response_metadata
                                    .entry("oven.http_status".into())
                                    .or_insert_with(|| serde_json::Value::from(status));
                            }
                            if let Some(request_id) = response.request_id {
                                turn.finish
                                    .response_metadata
                                    .entry("oven.request_id".into())
                                    .or_insert_with(|| serde_json::Value::from(request_id));
                            }
                            if !request.provider_metadata.is_empty() {
                                turn.finish.provider_metadata.insert(
                                    "oven.request".into(),
                                    serde_json::to_value(request.provider_metadata)
                                        .expect("safe request metadata serializes"),
                                );
                            }
                            turn
                        });
                        (completed, meaningful_output)
                    }
                    Err(error) => (Err(Box::new(error)), false),
                };
                match result {
                    Ok(turn) => {
                        let (turn, warnings) = persist_turn(turn, &self.inner.artifacts, binding)?;
                        let resolved_model = wire_model(binding);
                        let model_turn_seq = self.next_model_turn_seq(session)?;
                        self.append(
                            session,
                            Some(run),
                            Event::ModelTurnCommitted {
                                attempt_id,
                                model_turn_seq,
                                resolved_model,
                                input_through_seq,
                                turn: turn.clone(),
                                warnings,
                            },
                        )
                        .await?;
                        self.maybe_generate_session_title(
                            session,
                            run,
                            input_through_seq,
                            &turn,
                            cancellation,
                            &self.inner.internal_agents.policy(
                                InternalAgentKind::SessionTitle,
                                policy,
                                &chain[entry..],
                            ),
                        )
                        .await?;
                        return Ok(AttemptTurn {
                            turn,
                            model_turn_seq,
                        });
                    }
                    Err(error)
                        if error.kind == oven_sdk::ModelErrorKind::ContextLength
                            && !meaningful_output
                            && !context_recovery_attempted =>
                    {
                        self.append(session, Some(run), Event::AttemptAbandoned { attempt_id })
                            .await?;
                        context_recovery_attempted = true;
                        let before = self
                            .inner
                            .store
                            .get(session)?
                            .log
                            .events()
                            .iter()
                            .rev()
                            .find_map(|event| {
                                matches!(event.payload, Event::ContextCheckpointCommitted { .. })
                                    .then_some(event.seq)
                            })
                            .unwrap_or(0);
                        let recovery_events = self.prompt_events(session, run).await?;
                        let recovered = self
                            .maybe_compact_context(
                                session,
                                run,
                                cancellation,
                                binding,
                                &model,
                                &self.inner.internal_agents.policy(
                                    InternalAgentKind::ContextCompaction,
                                    policy,
                                    &chain[entry..],
                                ),
                                recovery_events,
                                true,
                            )
                            .await?;
                        let after = recovered
                            .iter()
                            .rev()
                            .find_map(|event| {
                                matches!(event.payload, Event::ContextCheckpointCommitted { .. })
                                    .then_some(event.seq)
                            })
                            .unwrap_or(0);
                        if after > before {
                            continue;
                        }
                        return Err(EngineError::Model(error));
                    }
                    Err(error) if classify_model_error(&error) == ErrorPolicy::FailRun => {
                        self.append(session, Some(run), Event::AttemptAbandoned { attempt_id })
                            .await?;
                        return Err(EngineError::Model(error));
                    }
                    Err(error)
                        if classify_model_error(&error) == ErrorPolicy::RetryEntry
                            && attempts <= 2
                            && !meaningful_output =>
                    {
                        self.append(session, Some(run), Event::AttemptAbandoned { attempt_id })
                            .await?;
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_millis(100_u64 << (attempts - 1))) => {}
                            _ = cancellation.cancelled() => return Err(ModelError::abort("model retry was cancelled").into()),
                        }
                    }
                    Err(error) => {
                        self.append(session, Some(run), Event::AttemptAbandoned { attempt_id })
                            .await?;
                        last_error = *error;
                        break;
                    }
                }
            }
            let Some(next) = chain.get(entry + 1) else {
                return Err(last_error.into());
            };
            self.append(
                session,
                Some(run),
                Event::ModelFallback {
                    from: wire_model(binding),
                    to: wire_model(next),
                    from_fallback_index: entry as u32,
                    to_fallback_index: entry as u32 + 1,
                    error: model_error_summary(&last_error),
                    attempts_on_from: attempts,
                },
            )
            .await?;
            entry += 1;
            *sticky_entry = entry;
            if let Ok(active_runs) = self.inner.active.lock()
                && let Some(active) = active_runs.get(&run)
            {
                active.fallback_index.store(entry as u64, Ordering::Release);
            }
        }
        Err(last_error.into())
    }

    #[allow(clippy::too_many_arguments)]
    async fn maybe_compact_context(
        &self,
        session: SessionId,
        run: RunId,
        cancellation: &CancellationToken,
        binding: &cookie_agent_models::FrozenModelBinding,
        model: &cookie_agent_models::ResolvedModel,
        internal_policy: &FrozenInternalAgentPolicy,
        events: Vec<StoredEvent>,
        force: bool,
    ) -> Result<Vec<StoredEvent>, EngineError> {
        let Some(context_limit) = binding.descriptor.capabilities.limits.context else {
            return Ok(events);
        };
        let config = &self.inner.config.runtime.context_compaction;
        let composed_prompt = self.run_agent_prompt(session, run)?;
        let context =
            assemble_model_context(&events, &self.inner.artifacts, binding, &composed_prompt)?;
        let serialized = serde_json::to_vec(&context.history)
            .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?;
        let input_tokens_before = (serialized.len() as u64).div_ceil(4);
        let soft_tokens = context_limit.saturating_mul(config.soft_threshold_percent as u64) / 100;
        let hard_tokens = context_limit.saturating_mul(config.hard_threshold_percent as u64) / 100;
        if !force && input_tokens_before < soft_tokens {
            return Ok(events);
        }
        let input_through_seq = events.last().map_or(0, |event| event.seq);
        if events.iter().rev().any(|event| {
            matches!(
                &event.payload,
                Event::ContextCheckpointCommitted { commit }
                    if commit.boundaries.input_through_seq >= input_through_seq
            )
        }) {
            return Ok(events);
        }
        let hard = input_tokens_before >= hard_tokens;
        let target_tokens = context_limit.saturating_mul(config.target_percent as u64) / 100;
        let previous = events.iter().rev().find_map(|event| match &event.payload {
            Event::ContextCheckpointCommitted { .. } => Some(event.seq),
            _ => None,
        });
        let source_from_seq = previous.map_or(1, |seq| seq.saturating_add(1));
        let boundaries = ContextCheckpointBoundaries {
            source_from_seq,
            source_through_seq: input_through_seq,
            input_through_seq,
            prior_checkpoint_seq: previous,
        };
        let summary_limit = SummaryByteLimit::new(config.max_summary_bytes as u64)
            .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?;

        let native_input_within_budget =
            input_tokens_before <= internal_policy.limits.max_input_tokens;
        if binding.descriptor.capabilities.compaction == CompactionCapability::Native
            && native_input_within_budget
        {
            let invocation_id = InternalAgentInvocationId::new_v7();
            let internal_run_id = InternalAgentRunId::new_v7();
            let backend = InternalAgentBackend::ProviderNative {
                resolved_model: wire_model(binding),
            };
            let digest = Sha256Digest::of_bytes(&serialized);
            self.append(
                session,
                Some(run),
                Event::InternalAgentStarted {
                    invocation_id,
                    internal_run_id,
                    kind: InternalAgentKind::ContextCompaction,
                    backend: backend.clone(),
                    call: SafeInternalAgentCall {
                        name: safe_code("provider_native_compaction"),
                        input_summary: safe_display(&format!("bounded native compaction input ({input_tokens_before} estimated tokens)")),
                        input_digest: digest,
                    },
                },
            )
            .await?;
            let mut request = ModelRequest::new(context.history);
            if let Some(native_context) = context.native_context {
                request = request.with_native_context(native_context);
            }
            let request = model.prepare_request(request);
            let abort = AbortBridge::new(cancellation.child_token());
            let compact_future = model
                .model()
                .compact(CompactionRequest::new(request), abort.signal());
            let compact = tokio::select! {
                result = tokio::time::timeout(
                    std::time::Duration::from_millis(internal_policy.limits.timeout_ms),
                    compact_future,
                ) => match result {
                    Ok(result) => result,
                    Err(_) => {
                        abort.abort();
                        Err(ModelError::timeout("provider-native compaction timed out"))
                    }
                },
                _ = cancellation.cancelled() => {
                    abort.abort();
                    self.append(
                        session,
                        Some(run),
                        Event::InternalAgentCancelled {
                            invocation_id,
                            internal_run_id,
                            kind: InternalAgentKind::ContextCompaction,
                            reason: Some(safe_error("parent run cancelled")),
                        },
                    ).await?;
                    return Err(ModelError::abort("context compaction was cancelled").into());
                }
            };
            match compact {
                Ok(result)
                    if result.native_context.adapter_id() == &binding.descriptor.adapter_id
                        && result.native_context.scope().provider_id
                            == binding.descriptor.identity.provider_id
                        && result.native_context.scope().model_id
                            == binding.descriptor.identity.model_id
                        && result.usage.input_tokens.is_none_or(|tokens| {
                            tokens <= internal_policy.limits.max_input_tokens
                        })
                        && result.usage.output_tokens.is_none_or(|tokens| {
                            tokens <= internal_policy.limits.max_output_tokens
                        }) =>
                {
                    let payload = serde_json::to_vec(&result.native_context).map_err(|error| {
                        EngineError::from(ModelError::native_context(error.to_string()))
                    })?;
                    if payload.len() <= config.max_native_context_bytes {
                        let (reference, digest) = self.inner.artifacts.retain(&payload)?;
                        let checkpoint = ContextCheckpoint::ProviderNative {
                            model: wire_model(binding),
                            native_context: Box::new(NativeContextArtifact {
                                adapter_id: safe_code(result.native_context.adapter_id().as_str()),
                                selection_fingerprint: wire_model(binding).selection_fingerprint,
                                scope: cookie_agent_protocol::NativeContextScope {
                                    provider_id: binding.resolved.provider_id.clone(),
                                    model_id: binding.resolved.model_id.clone(),
                                    resource_id: safe_display(
                                        result.native_context.scope().resource_id.as_str(),
                                    ),
                                },
                                byte_length: payload.len() as u64,
                                sha256: Sha256Digest::new(digest).map_err(|error| {
                                    EngineError::from(ModelError::native_context(error.to_string()))
                                })?,
                                reference,
                            }),
                        };
                        self.append(
                            session,
                            Some(run),
                            Event::InternalAgentCompleted {
                                invocation_id,
                                internal_run_id,
                                kind: InternalAgentKind::ContextCompaction,
                                result: SafeInternalAgentResult {
                                    output_summary: safe_display(&format!(
                                        "validated native context ({} bytes)",
                                        payload.len()
                                    )),
                                    output_digest: Sha256Digest::of_bytes(&payload),
                                },
                            },
                        )
                        .await?;
                        let budgets = ContextCheckpointBudgets {
                            context_limit_tokens: context_limit,
                            trigger_tokens: soft_tokens,
                            target_tokens,
                            input_tokens_before,
                            input_tokens_after: target_tokens,
                            max_summary_bytes: summary_limit,
                        };
                        let commit = ContextCheckpointCommit {
                            checkpoint,
                            boundaries,
                            budgets,
                        };
                        commit.validate().map_err(|error| {
                            EngineError::from(ModelError::native_context(error.to_string()))
                        })?;
                        self.append(
                            session,
                            Some(run),
                            Event::ContextCheckpointCommitted { commit },
                        )
                        .await?;
                        return Ok(self.inner.store.get(session)?.log.events());
                    }
                    self.append(
                        session,
                        Some(run),
                        Event::InternalAgentFailed {
                            invocation_id,
                            internal_run_id,
                            kind: InternalAgentKind::ContextCompaction,
                            failure: InternalAgentFailure {
                                code: safe_code("native_context_too_large"),
                                message: safe_error("provider-native context exceeded the configured persistence bound"),
                                retryable: false,
                                model_error: None,
                            },
                        },
                    ).await?;
                }
                Ok(_) => {
                    self.append(
                        session,
                        Some(run),
                        Event::InternalAgentFailed {
                            invocation_id,
                            internal_run_id,
                            kind: InternalAgentKind::ContextCompaction,
                            failure: InternalAgentFailure {
                                code: safe_code("native_context_scope_mismatch"),
                                message: safe_error("provider-native context did not match the exact configured model scope"),
                                retryable: false,
                                model_error: None,
                            },
                        },
                    ).await?;
                }
                Err(error) => {
                    self.append(
                        session,
                        Some(run),
                        Event::InternalAgentFailed {
                            invocation_id,
                            internal_run_id,
                            kind: InternalAgentKind::ContextCompaction,
                            failure: InternalAgentFailure {
                                code: safe_code("native_compaction_failed"),
                                message: safe_error(&error.message),
                                retryable: error.retryable,
                                model_error: Some(model_error_summary(&error)),
                            },
                        },
                    )
                    .await?;
                }
            }
            {
                let fallback_backend = internal_policy.models.first().map_or_else(
                    || InternalAgentBackend::Builtin {
                        name: safe_code("bounded_summary"),
                        revision: safe_display(BOUNDED_SUMMARY_BUILTIN_REVISION),
                    },
                    |binding| InternalAgentBackend::Model {
                        resolved_model: wire_model(binding),
                    },
                );
                self.append(
                    session,
                    Some(run),
                    Event::InternalAgentFallback {
                        invocation_id,
                        internal_run_id,
                        kind: InternalAgentKind::ContextCompaction,
                        from: backend,
                        to: fallback_backend,
                        failure: InternalAgentFailure {
                            code: safe_code("native_compaction_unusable"),
                            message: safe_error(
                                "provider-native compaction did not produce a valid checkpoint",
                            ),
                            retryable: false,
                            model_error: None,
                        },
                        attempts: 1,
                    },
                )
                .await?;
            }
        }

        let durable = serde_json::to_string(&events)
            .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?;
        let summary = self
            .run_internal_text_agent(
                session,
                Some(run),
                InternalAgentKind::ContextCompaction,
                internal_policy,
                format!(
                    "Summarize the durable conversation below without omitting system policy, approval boundaries, attachments, or complete tool call/result pairs. Return summary text only.\n{durable}"
                ),
                cancellation,
            )
            .await;
        match summary {
            Ok(summary) if !summary.text.trim().is_empty() => {
                let checkpoint = InternalSummaryCheckpoint::new(
                    summary.text,
                    summary.invocation_id,
                    summary.internal_run_id,
                    summary_limit,
                )
                .map_err(|error| {
                    EngineError::from(ModelError::invalid_response(error.to_string()))
                })?;
                let input_tokens_after = checkpoint.byte_length().div_ceil(4);
                let budgets = ContextCheckpointBudgets {
                    context_limit_tokens: context_limit,
                    trigger_tokens: soft_tokens,
                    target_tokens,
                    input_tokens_before,
                    input_tokens_after,
                    max_summary_bytes: summary_limit,
                };
                let commit = ContextCheckpointCommit {
                    checkpoint: ContextCheckpoint::InternalSummary { checkpoint },
                    boundaries,
                    budgets,
                };
                commit.validate().map_err(|error| {
                    EngineError::from(ModelError::invalid_response(error.to_string()))
                })?;
                self.append(
                    session,
                    Some(run),
                    Event::ContextCheckpointCommitted { commit },
                )
                .await?;
                Ok(self.inner.store.get(session)?.log.events())
            }
            _ if hard => Err(ModelError::new(
                oven_sdk::ModelErrorKind::ContextLength,
                "hard context limit reached and no valid context checkpoint could be produced",
            )
            .into()),
            _ => Ok(events),
        }
    }

    async fn maybe_generate_session_title(
        &self,
        session: SessionId,
        run: RunId,
        input_through_seq: u64,
        turn: &PersistedModelTurn,
        cancellation: &CancellationToken,
        internal_policy: &FrozenInternalAgentPolicy,
    ) -> Result<(), EngineError> {
        let policy = &self.inner.config.runtime.session_title;
        if !policy.generate_on_first_turn {
            return Ok(());
        }
        let events = self.inner.store.get(session)?.log.events();
        if !automatic_title_eligible(&events) {
            return Ok(());
        }
        let input = events
            .iter()
            .find_map(|event| match &event.payload {
                Event::UserInputSubmitted { input } if event.run_id == Some(run) => {
                    Some(input.clone())
                }
                _ => None,
            })
            .unwrap_or_default();
        let answer = turn
            .content
            .iter()
            .filter_map(|part| match part {
                PersistedAssistantPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let prompt = format!(
            "Return only a short plain-text session title. No quotes, markup, or explanation.\nUser: {}\nAssistant: {}",
            truncate_utf8(&input, 8 * 1024),
            truncate_utf8(&answer, 8 * 1024)
        );
        let generated = self
            .run_internal_text_agent(
                session,
                Some(run),
                InternalAgentKind::SessionTitle,
                internal_policy,
                prompt,
                cancellation,
            )
            .await;
        let commit = match generated {
            Ok(result) => validate_generated_title(&result.text, policy.max_chars)
                .map(|title| SessionTitleChange::InternalAgentSet {
                    title,
                    invocation_id: result.invocation_id,
                })
                .or_else(|| {
                    policy
                        .fallback_to_input_excerpt
                        .then(|| fallback_title(&input, policy.max_chars))
                        .flatten()
                        .map(|title| SessionTitleChange::FallbackSet { title })
                }),
            Err(_) => policy
                .fallback_to_input_excerpt
                .then(|| fallback_title(&input, policy.max_chars))
                .flatten()
                .map(|title| SessionTitleChange::FallbackSet { title }),
        };
        if let Some(commit) = commit {
            self.append(
                session,
                Some(run),
                Event::SessionTitleCommitted {
                    input_through_seq,
                    change: commit,
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn generate_title_after_reset(&self, session: SessionId) -> Result<(), EngineError> {
        let projection = self.inner.store.get(session)?;
        let events = projection.log.events();
        if !automatic_title_eligible(&events) {
            return Ok(());
        }
        let Some((run, input_through_seq, turn)) = title_regeneration_target(&events) else {
            return Ok(());
        };
        let frozen = self.historical_title_policy(&events, run)?;
        let active_index = active_fallback_index(&events, run);
        let internal = self.inner.internal_agents.policy(
            InternalAgentKind::SessionTitle,
            &frozen,
            frozen.active_suffix(active_index),
        );
        self.maybe_generate_session_title(
            session,
            run,
            input_through_seq,
            &turn,
            &CancellationToken::new(),
            &internal,
        )
        .await
    }

    async fn prepare_tool_call(
        &self,
        session_id: SessionId,
        run: RunId,
        call: ToolCall,
        policy: &FrozenRunPolicy,
    ) -> PreparedToolCall {
        let fallback_presentation = safe_tool_presentation(&call);
        let session = match self.inner.store.get(session_id) {
            Ok(session) => session,
            Err(error) => {
                return PreparedToolCall {
                    call,
                    presentation: fallback_presentation,
                    prepared: Err(ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: error.to_string(),
                    }),
                };
            }
        };
        let depth = session_depth(&session.meta.origin);
        let delegate_enabled = policy.agent.delegation.as_ref().is_some_and(|delegation| {
            depth < delegation.effective_depth_ceiling
                && delegation.targets.iter().any(|target| {
                    policy.registry.get(target).is_some_and(|agent| {
                        agent.document.frontmatter.enabled
                            && matches!(
                                agent.document.frontmatter.mode,
                                cookie_agent_config::AgentMode::Subagent
                                    | cookie_agent_config::AgentMode::All
                            )
                    })
                })
        });
        let enabled_tools = policy.tools();
        if (call.name == "delegate" && !delegate_enabled)
            || (call.name != "delegate"
                && (!enabled_tools.contains(&call.name)
                    || !PermissionPipeline::tool_visible(&policy.agent, &call.name)))
        {
            return PreparedToolCall {
                prepared: Err(ToolFailure {
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: format!("tool `{}` is not enabled for this session", call.name),
                }),
                call,
                presentation: fallback_presentation,
            };
        }
        let providers = self
            .inner
            .tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let provider = providers.into_iter().find(|provider| {
            provider
                .tools_for_session(&SessionToolContext {
                    session: session_id,
                })
                .ok()
                .is_some_and(|tools| tools.iter().any(|tool| tool.name == call.name))
        });
        let Some(provider) = provider else {
            return PreparedToolCall {
                prepared: Err(ToolFailure {
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: format!("tool `{}` is unavailable", call.name),
                }),
                call,
                presentation: fallback_presentation,
            };
        };
        let presentation = provider.presentation(&call);
        let context = ToolPreparationContext {
            session: session_id,
            run,
            cwd: self.inner.store.cwd().to_owned(),
            workspace_root: self.inner.store.cwd().to_owned(),
        };
        let prepared = provider
            .prepare(context, call.clone())
            .await
            .map_err(Into::into);
        PreparedToolCall {
            call,
            presentation,
            prepared,
        }
    }

    async fn run_internal_text_agent(
        &self,
        session: SessionId,
        parent_run: Option<RunId>,
        kind: InternalAgentKind,
        policy: &FrozenInternalAgentPolicy,
        input: String,
        cancellation: &CancellationToken,
    ) -> Result<InternalAgentTextResult, EngineError> {
        let name = match kind {
            InternalAgentKind::Approval => "approval",
            InternalAgentKind::ContextCompaction => "context_compaction",
            InternalAgentKind::SessionTitle => "session_title",
        };
        let policy = policy.clone();
        let max_input_bytes = usize::try_from(policy.limits.max_input_tokens)
            .unwrap_or(usize::MAX)
            .saturating_mul(4);
        let input = truncate_utf8(&input, max_input_bytes);
        let invocation_id = InternalAgentInvocationId::new_v7();
        let internal_run_id = InternalAgentRunId::new_v7();
        let call = SafeInternalAgentCall {
            name: safe_code(name),
            input_summary: safe_display(&format!("bounded {name} input ({} bytes)", input.len())),
            input_digest: Sha256Digest::of_bytes(input.as_bytes()),
        };
        let mut previous_backend = None;
        let mut last_failure = InternalAgentFailure {
            code: safe_code("agent_unavailable"),
            message: safe_error("no frozen internal model is available"),
            retryable: false,
            model_error: None,
        };
        for (index, binding) in policy.models.iter().enumerate() {
            let backend = InternalAgentBackend::Model {
                resolved_model: wire_model(binding),
            };
            if index == 0 {
                self.append(
                    session,
                    parent_run,
                    Event::InternalAgentStarted {
                        invocation_id,
                        internal_run_id,
                        kind,
                        backend: backend.clone(),
                        call: call.clone(),
                    },
                )
                .await?;
            } else if let Some(from) = previous_backend.take() {
                self.append(
                    session,
                    parent_run,
                    Event::InternalAgentFallback {
                        invocation_id,
                        internal_run_id,
                        kind,
                        from,
                        to: backend.clone(),
                        failure: last_failure.clone(),
                        attempts: index as u32,
                    },
                )
                .await?;
            }
            let model = match policy
                .model_snapshot
                .as_ref()
                .expect("internal model policy retains its executable snapshot")
                .resolve(binding)
            {
                Ok(model) => model,
                Err(error) => {
                    last_failure = InternalAgentFailure {
                        code: safe_code("model_unavailable"),
                        message: safe_error(&error.to_string()),
                        retryable: false,
                        model_error: None,
                    };
                    previous_backend = Some(backend);
                    continue;
                }
            };
            let history = vec![
                oven_sdk::HistoryTurn::system(oven_sdk::SystemMessage::new(vec![
                    oven_sdk::SystemPart::Text(oven_sdk::TextPart::new(
                        policy.agent.composed_prompt.clone(),
                    )),
                ])),
                oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(vec![
                    oven_sdk::InputPart::Text(oven_sdk::TextPart::new(input.clone())),
                ])),
            ];
            let mut request = ModelRequest::new(history);
            request.inference.max_output_tokens = Some(policy.limits.max_output_tokens);
            let request = model.prepare_request(request);
            let abort = AbortBridge::new(cancellation.child_token());
            let call_future = model.model().complete(request, abort.signal());
            let result = tokio::select! {
                result = tokio::time::timeout(
                    std::time::Duration::from_millis(policy.limits.timeout_ms),
                    call_future,
                ) => match result {
                    Ok(result) => result,
                    Err(_) => {
                        abort.abort();
                        Err(ModelError::timeout("internal agent timed out"))
                    },
                },
                _ = cancellation.cancelled() => {
                    abort.abort();
                    self.append(
                        session,
                        parent_run,
                        Event::InternalAgentCancelled {
                            invocation_id,
                            internal_run_id,
                            kind,
                            reason: Some(safe_error("parent run cancelled")),
                        },
                    ).await?;
                    return Err(ModelError::abort("internal agent was cancelled").into());
                }
            };
            match result {
                Ok(completed) => {
                    let output = completed
                        .turn
                        .message
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            oven_sdk::AssistantPart::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    let max_output_bytes = usize::try_from(policy.limits.max_output_tokens)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(4);
                    if output.len() > max_output_bytes {
                        last_failure = InternalAgentFailure {
                            code: safe_code("output_too_large"),
                            message: safe_error("internal agent output exceeded its hard bound"),
                            retryable: false,
                            model_error: None,
                        };
                        previous_backend = Some(backend);
                        continue;
                    }
                    self.append(
                        session,
                        parent_run,
                        Event::InternalAgentCompleted {
                            invocation_id,
                            internal_run_id,
                            kind,
                            result: SafeInternalAgentResult {
                                output_summary: safe_display(&format!(
                                    "validated {name} output ({} bytes)",
                                    output.len()
                                )),
                                output_digest: Sha256Digest::of_bytes(output.as_bytes()),
                            },
                        },
                    )
                    .await?;
                    return Ok(InternalAgentTextResult {
                        invocation_id,
                        internal_run_id,
                        text: output,
                    });
                }
                Err(error) => {
                    last_failure = InternalAgentFailure {
                        code: safe_code("model_failure"),
                        message: safe_error(&error.message),
                        retryable: error.retryable,
                        model_error: Some(model_error_summary(&error)),
                    };
                    previous_backend = Some(backend);
                }
            }
        }
        if policy.models.is_empty() {
            self.append(
                session,
                parent_run,
                Event::InternalAgentStarted {
                    invocation_id,
                    internal_run_id,
                    kind,
                    backend: InternalAgentBackend::Builtin {
                        name: safe_code("unavailable"),
                        revision: safe_display(UNAVAILABLE_BUILTIN_REVISION),
                    },
                    call,
                },
            )
            .await?;
        }
        self.append(
            session,
            parent_run,
            Event::InternalAgentFailed {
                invocation_id,
                internal_run_id,
                kind,
                failure: last_failure,
            },
        )
        .await?;
        Err(ModelError::invalid_response("internal agent failed safely").into())
    }

    fn active_internal_policy(
        &self,
        active: &ActiveRun,
        kind: InternalAgentKind,
    ) -> FrozenInternalAgentPolicy {
        let fallback_index = active.fallback_index.load(Ordering::Acquire) as usize;
        self.inner.internal_agents.policy(
            kind,
            &active.policy,
            active.policy.active_suffix(fallback_index),
        )
    }

    async fn request_model_approval(
        &self,
        active: &ActiveRun,
        run: RunId,
        operation: &PreparedOperationIdentity,
        policy_labels: &[String],
        executor: PreparedExecutorCell,
        message: Option<String>,
    ) -> Result<ApprovalOutcome, EngineError> {
        let approval_policy = self.active_internal_policy(active, InternalAgentKind::Approval);
        let request = approval_request_for_operation(
            ApprovalTrigger::ModelToolApproval,
            operation.clone(),
            operation
                .resources()
                .iter()
                .zip(policy_labels)
                .map(|(resource, label)| cookie_agent_protocol::DecisionTrace {
                    action: resource.capability,
                    normalized_resource: label.clone(),
                    candidates: Vec::new(),
                    effect: cookie_agent_protocol::PermissionEffect::Ask,
                    precedence_reason: message
                        .clone()
                        .unwrap_or_else(|| "model requested tool approval".into()),
                })
                .collect(),
            false,
            approval_expiry(approval_policy.limits.timeout_ms),
        );
        self.await_user_approval(active, run, request, executor, false)
            .await
    }

    async fn await_user_approval(
        &self,
        active: &ActiveRun,
        run: RunId,
        request: ApprovalRequest,
        executor: PreparedExecutorCell,
        allow_prior_grant: bool,
    ) -> Result<ApprovalOutcome, EngineError> {
        let approval_id = request.approval_id();
        let session = self.inner.store.get(active.session)?;
        let root = root_id(&session.meta.origin, active.session);
        self.append(
            active.session,
            Some(run),
            Event::ApprovalRequested {
                request: request.clone(),
            },
        )
        .await?;

        let repetitions = doom_loop_repetitions(
            &self.inner.store.get(active.session)?.log.events(),
            run,
            request.operation_fingerprint(),
        );
        if repetitions >= 4 {
            self.append(
                active.session,
                Some(run),
                Event::ApprovalDoomLoopDetected {
                    approval_id,
                    operation_fingerprint: request.operation_fingerprint().clone(),
                    repetitions,
                },
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                Event::ApprovalFinalized {
                    approval_id,
                    decision: ApprovalFinalDecision {
                        outcome: ApprovalFinalOutcome::Rejected,
                        source: ApprovalDecisionSource::DoomLoopGuard,
                        reason_code: ApprovalReasonCode::DoomLoopDetected,
                        feedback: None,
                        tree_grant_id: None,
                    },
                },
            )
            .await?;
            return Ok(ApprovalOutcome {
                approved: false,
                feedback: None,
            });
        }

        if allow_prior_grant
            && let Some(grant) = self.inner.approvals.matching(root, request.operation())
        {
            let decision = ApprovalInternalDecision {
                decision: ApprovalInternalDecisionKind::Allow,
                source: ApprovalDecisionSource::TreeGrant,
                reason_code: ApprovalReasonCode::TreeGrantMatched,
                evaluations: approval_evaluations(&request),
            };
            self.append(
                active.session,
                Some(run),
                Event::ApprovalEvaluated {
                    approval_id,
                    decision,
                },
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                Event::ApprovalFinalized {
                    approval_id,
                    decision: ApprovalFinalDecision {
                        outcome: ApprovalFinalOutcome::Approved,
                        source: ApprovalDecisionSource::TreeGrant,
                        reason_code: ApprovalReasonCode::TreeGrantMatched,
                        feedback: None,
                        tree_grant_id: Some(grant.grant_id),
                    },
                },
            )
            .await?;
            return Ok(ApprovalOutcome {
                approved: true,
                feedback: None,
            });
        }

        if approval_evaluations(&request)
            .iter()
            .any(|evaluation| evaluation.effect == cookie_agent_protocol::PermissionEffect::Deny)
        {
            let decision = ApprovalInternalDecision {
                decision: ApprovalInternalDecisionKind::Deny,
                source: ApprovalDecisionSource::Policy,
                reason_code: ApprovalReasonCode::PolicyDenied,
                evaluations: approval_evaluations(&request),
            };
            self.append(
                active.session,
                Some(run),
                Event::ApprovalEvaluated {
                    approval_id,
                    decision,
                },
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                Event::ApprovalFinalized {
                    approval_id,
                    decision: ApprovalFinalDecision {
                        outcome: ApprovalFinalOutcome::Rejected,
                        source: ApprovalDecisionSource::Policy,
                        reason_code: ApprovalReasonCode::PolicyDenied,
                        feedback: None,
                        tree_grant_id: None,
                    },
                },
            )
            .await?;
            return Ok(ApprovalOutcome {
                approved: false,
                feedback: None,
            });
        }

        let safe_resources = approval_evaluations(&request)
            .iter()
            .map(|evaluation| evaluation.trace.normalized_resource.clone())
            .collect::<Vec<_>>();
        let safe_operations = request
            .operation()
            .capabilities()
            .iter()
            .map(|capability| capability.operation.as_str())
            .collect::<Vec<_>>();
        let prompt = serde_json::to_string(&serde_json::json!({
            "instruction": "Return strict JSON only: {\"decision\":\"allow\"|\"deny\"|\"ask\"}.",
            "cwd_identity": session.meta.cwd_identity,
            "operations": safe_operations,
            "resource_labels": safe_resources,
        }))
        .expect("safe approval prompt serializes");
        #[cfg(test)]
        let hook = {
            self.inner
                .approval_evaluation_hook
                .lock()
                .expect("approval evaluation hook lock poisoned")
                .take()
        };
        #[cfg(test)]
        if let Some(hook) = hook {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("approval evaluation reached lock poisoned")
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
        let approval_policy = self.active_internal_policy(active, InternalAgentKind::Approval);
        let internal_kind = match self
            .run_internal_text_agent(
                active.session,
                Some(run),
                InternalAgentKind::Approval,
                &approval_policy,
                prompt,
                &active.cancellation,
            )
            .await
        {
            Ok(result) => {
                parse_internal_approval(&result.text).unwrap_or(ApprovalInternalDecisionKind::Ask)
            }
            Err(_) => ApprovalInternalDecisionKind::Ask,
        };
        let transition = self
            .request(active.session, |reply| {
                SessionCommand::ApprovalEvaluationComplete {
                    run,
                    request: request.clone(),
                    executor: executor.clone(),
                    decision: internal_kind,
                    cancelled: active.cancellation.is_cancelled(),
                    reply,
                }
            })
            .await?;
        let mut receiver = match transition {
            ApprovalEvaluationTransition::Resolved(outcome) => return Ok(outcome),
            ApprovalEvaluationTransition::Escalated(receiver) => receiver,
        };
        let expiry_wait = approval_expiry_wait(approval_constraints(&request).expires_at);
        tokio::select! {
            decision = &mut receiver => decision.map_err(|_| EngineError::ActorStopped),
            _ = active.cancellation.cancelled() => {
                let finalized = self.request(active.session, |reply| {
                    SessionCommand::ApprovalTerminal {
                        run,
                        approval_id,
                        terminal: ApprovalTerminal::Cancelled,
                        reply,
                    }
                }).await?;
                if finalized {
                    Ok(ApprovalOutcome {
                        approved: false,
                        feedback: Some("cancelled".into()),
                    })
                } else {
                    receiver.await.map_err(|_| EngineError::ActorStopped)
                }
            },
            _ = tokio::time::sleep(expiry_wait) => {
                let finalized = self.request(active.session, |reply| {
                    SessionCommand::ApprovalTerminal {
                        run,
                        approval_id,
                        terminal: ApprovalTerminal::Expired,
                        reply,
                    }
                }).await?;
                if finalized {
                    Ok(ApprovalOutcome {
                        approved: false,
                        feedback: Some("approval expired unattended".into()),
                    })
                } else {
                    receiver.await.map_err(|_| EngineError::ActorStopped)
                }
            }
        }
    }

    async fn execute_tool(
        &self,
        active: Arc<ActiveRun>,
        run: RunId,
        prepared: PreparedToolCall,
    ) -> Result<ToolResult, ToolFailure> {
        let engine = self.clone();
        {
            let PreparedToolCall { call, prepared, .. } = prepared;
            let prepared = prepared?;
            let operation = prepared.operation.clone();
            let policy_labels = prepared.policy_labels.clone();
            let _serialization_guard = if let Some(key) = &prepared.serialization_key {
                Some(engine.mutation_lock(key).lock_owned().await)
            } else {
                None
            };
            let permission = engine.inner.permissions.decide_operation(
                &active.policy.agent,
                &operation,
                &policy_labels,
            );
            if permission.effect != cookie_agent_protocol::PermissionEffect::Allow {
                if permission.effect == cookie_agent_protocol::PermissionEffect::Ask {
                    let allow_tree_grant = operation.resources().iter().all(|resource| {
                        resource.binding_lifetime
                            == cookie_agent_protocol::PreparedBindingLifetime::RestartStable
                            && !matches!(
                                resource.capability,
                                cookie_agent_protocol::PermissionAction::Read
                                    | cookie_agent_protocol::PermissionAction::Write
                                    | cookie_agent_protocol::PermissionAction::Grep
                                    | cookie_agent_protocol::PermissionAction::Glob
                                    | cookie_agent_protocol::PermissionAction::ExternalDirectory
                            )
                    });
                    let approval_policy =
                        engine.active_internal_policy(&active, InternalAgentKind::Approval);
                    let request = ApprovalRequest::new(
                        ApprovalId::new_v7(),
                        1,
                        ApprovalTrigger::PermissionPolicy,
                        operation.clone(),
                        permission.evaluations.clone(),
                        ApprovalConstraints {
                            allow_once: true,
                            allow_tree_grant,
                            cancellable: true,
                            expires_at: approval_expiry(approval_policy.limits.timeout_ms),
                        },
                    )
                    .map_err(|error| ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: error.to_string(),
                    })?;
                    let outcome = engine
                        .await_user_approval(&active, run, request, prepared.executor.clone(), true)
                        .await
                        .map_err(|error| ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message: error.to_string(),
                        })?;
                    if !outcome.approved {
                        return Err(ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message: denied_tool_failure(
                                ApprovalDecisionSource::Policy,
                                "permission refused by user",
                                outcome.feedback,
                            ),
                        });
                    }
                } else {
                    return Err(ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: denied_tool_failure(
                            ApprovalDecisionSource::Policy,
                            "permission denied",
                            None,
                        ),
                    });
                }
            }
            let executor = prepared
                .executor
                .lock()
                .await
                .take()
                .ok_or_else(|| ToolFailure {
                    code: ToolCallFailureCode::PreparedCapabilityLost,
                    message: "prepared executor capability was already consumed or lost".into(),
                })?;
            let (progress_tx, mut progress_rx) = mpsc::channel(64);
            let hub = engine
                .inner
                .output_hubs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(call.id)
                .or_insert_with(|| OutputHub::new(call.id, 64 * 1024))
                .clone();
            let interactive = call.name == "bash"
                && call
                    .arguments
                    .get("interactive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            let capture = (call.name == "bash")
                .then(|| OutputCapture::new(engine.inner.artifacts.clone()))
                .transpose()
                .map_err(|error| ToolFailure {
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: format!("tool output capture setup failed: {error}"),
                })?;
            let (stdin_tx, stdin) = ToolStdin::channel(64);
            if interactive {
                active
                    .stdin
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(call.id, stdin_tx);
            }
            let invoke = executor.execute(ToolExecutionContext {
                session: active.session,
                run,
                progress: capture.as_ref().map_or_else(
                    || ProgressSink::new(progress_tx.clone(), hub.clone()),
                    |capture| {
                        ProgressSink::with_capture(
                            progress_tx.clone(),
                            hub.clone(),
                            capture.clone(),
                        )
                    },
                ),
                cancellation: active.cancellation.child_token(),
                stdin: interactive.then_some(stdin),
                artifacts: engine.inner.artifacts.clone(),
            });
            tokio::pin!(invoke);
            loop {
                tokio::select! {
                    result = &mut invoke => {
                        active
                            .stdin
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&call.id);
                        // Tool implementations drain their producers before
                        // resolving.  Finalizing here makes all emitted deltas
                        // precede the completion notification committed by the
                        // session actor.
                        hub.finalize();
                        engine.retain_finalized_output_hub(call.id);
                        return match result {
                            Ok(result) => {
                                let bounded = if let Some(capture) = &capture {
                                    capture.finish(
                                        result,
                                        active.policy.result_limits.tool_output_max_lines,
                                        active.policy.result_limits.tool_output_max_bytes,
                                    )
                                } else {
                                    bound_tool_result(
                                        result,
                                        &call.name,
                                        call.id,
                                        &engine.inner.artifacts,
                                        active.policy.result_limits.tool_output_max_lines,
                                        active.policy.result_limits.tool_output_max_bytes,
                                    )
                                };
                                bounded.map_err(|error| ToolFailure {
                                    code: ToolCallFailureCode::ExecutionFailed,
                                    message: error.to_string(),
                                })
                            }
                            Err(error) => {
                                if let Some(capture) = &capture {
                                    capture.discard();
                                }
                                Err(error.into())
                            }
                        };
                    }
                    Some(progress) = progress_rx.recv() => {
                        let _ = engine.append(active.session, Some(run), Event::ToolCallProgress { tool_call_id: progress.tool_call_id, message: safe_display(&progress.message) }).await;
                    }
                    _ = active.cancellation.cancelled() => {
                        active
                            .stdin
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&call.id);
                        if let Some(capture) = &capture {
                            capture.discard();
                        }
                        hub.finalize();
                        engine.retain_finalized_output_hub(call.id);
                        return Err(ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message: "tool call cancelled after it started".into(),
                        });
                    }
                }
            }
        }
    }

    pub async fn approval_respond(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<ApprovalRespondResult, EngineError> {
        let executor = self
            .inner
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(params.session_id, params.approval_id))
            .map(|pending| pending.executor.clone());
        let Some(executor) = executor else {
            return self
                .request(params.session_id, |reply| SessionCommand::ApprovalRespond {
                    params,
                    reply,
                })
                .await;
        };

        let guard = executor.lock_owned().await;
        let invalidation = match guard.as_ref() {
            Some(executor) => executor.revalidate().await.err().map(|error| match error {
                ToolError::OperationChanged(_) => PreparedApprovalInvalidation::OperationChanged,
                _ => PreparedApprovalInvalidation::PreparedCapabilityLost,
            }),
            None => Some(PreparedApprovalInvalidation::PreparedCapabilityLost),
        };
        if let Some(invalidation) = invalidation {
            return self
                .request(params.session_id, |reply| {
                    SessionCommand::ApprovalCapabilityInvalid {
                        params,
                        invalidation,
                        reply,
                    }
                })
                .await;
        }
        self.request(params.session_id, |reply| SessionCommand::ApprovalRespond {
            params,
            reply,
        })
        .await
    }

    fn approval_evaluation_complete_direct(
        &self,
        session: SessionId,
        run: RunId,
        request: ApprovalRequest,
        executor: PreparedExecutorCell,
        decision: ApprovalInternalDecisionKind,
        cancelled: bool,
    ) -> Result<ApprovalEvaluationTransition, EngineError> {
        let approval_id = request.approval_id();
        let events = self.inner.store.get(session)?.log.events();
        let Some(record) = approval_records(session, &events).remove(&approval_id) else {
            return Err(EngineError::ApprovalNotPending {
                session_id: session,
                approval_id,
            });
        };
        if record.status != ApprovalStatus::Pending
            || approval_run_id(&events, approval_id) != Some(run)
        {
            return Err(EngineError::ApprovalNotPending {
                session_id: session,
                approval_id,
            });
        }
        if cancelled {
            self.approval_terminal_direct(session, run, approval_id, ApprovalTerminal::Cancelled)?;
            return Ok(ApprovalEvaluationTransition::Resolved(ApprovalOutcome {
                approved: false,
                feedback: Some("cancelled".into()),
            }));
        }
        if approval_deadline_exhausted(approval_constraints(&request).expires_at) {
            self.approval_terminal_direct(session, run, approval_id, ApprovalTerminal::Expired)?;
            return Ok(ApprovalEvaluationTransition::Resolved(ApprovalOutcome {
                approved: false,
                feedback: Some("approval expired unattended".into()),
            }));
        }

        let source = ApprovalDecisionSource::InternalAgent;
        let reason_code = match decision {
            ApprovalInternalDecisionKind::Allow => ApprovalReasonCode::InternalAgentAllowed,
            ApprovalInternalDecisionKind::Deny => ApprovalReasonCode::InternalAgentDenied,
            ApprovalInternalDecisionKind::Ask | ApprovalInternalDecisionKind::Escalate => {
                ApprovalReasonCode::Escalated
            }
        };
        self.append_direct(
            session,
            Some(run),
            Event::ApprovalEvaluated {
                approval_id,
                decision: ApprovalInternalDecision {
                    decision,
                    source,
                    reason_code,
                    evaluations: approval_evaluations(&request),
                },
            },
        )?;
        if matches!(
            decision,
            ApprovalInternalDecisionKind::Allow | ApprovalInternalDecisionKind::Deny
        ) {
            let approved = decision == ApprovalInternalDecisionKind::Allow;
            self.append_direct(
                session,
                Some(run),
                Event::ApprovalFinalized {
                    approval_id,
                    decision: ApprovalFinalDecision {
                        outcome: if approved {
                            ApprovalFinalOutcome::Approved
                        } else {
                            ApprovalFinalOutcome::Rejected
                        },
                        source,
                        reason_code,
                        feedback: None,
                        tree_grant_id: None,
                    },
                },
            )?;
            return Ok(ApprovalEvaluationTransition::Resolved(ApprovalOutcome {
                approved,
                feedback: None,
            }));
        }

        self.append_direct(
            session,
            Some(run),
            Event::ApprovalEscalated {
                approval_id,
                reason_code: ApprovalReasonCode::Escalated,
            },
        )?;
        let (sender, receiver) = oneshot::channel();
        let replaced = self
            .inner
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert((session, approval_id), PendingApproval { sender, executor });
        if replaced.is_some() {
            return Err(EngineError::ApprovalConflict);
        }
        Ok(ApprovalEvaluationTransition::Escalated(receiver))
    }

    fn approval_respond_direct(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<ApprovalRespondResult, EngineError> {
        let projection = self.inner.store.get(params.session_id)?;
        let events = projection.log.events();
        if let Some((recorded_approval_id, recorded_decision, recorded_feedback)) =
            events.iter().find_map(|event| match &event.payload {
                Event::ApprovalUserDecisionRecorded {
                    approval_id,
                    client_response_id,
                    decision,
                    feedback,
                } if client_response_id == &params.client_response_id => {
                    Some((*approval_id, *decision, feedback.clone()))
                }
                _ => None,
            })
        {
            let Some(request) =
                approval_records(params.session_id, &events).remove(&recorded_approval_id)
            else {
                return Err(approval_response_failure(
                    &params,
                    ApprovalRespondErrorCode::ApprovalNotFound,
                    None,
                ));
            };
            if recorded_approval_id != params.approval_id
                || recorded_decision != params.decision
                || recorded_feedback != params.feedback
                || approval_request_revision(&request.request) != params.request_revision
                || request.request.operation_fingerprint() != &params.operation_fingerprint
            {
                return Err(approval_response_failure(
                    &params,
                    ApprovalRespondErrorCode::IdempotencyConflict,
                    Some(&request),
                ));
            }
            return Ok(ApprovalRespondResult {
                client_response_id: params.client_response_id,
                approval: request,
            });
        }

        let mut records = approval_records(params.session_id, &events);
        let Some(record) = records.remove(&params.approval_id) else {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalNotFound,
                None,
            ));
        };
        if !matches!(
            record.status,
            ApprovalStatus::Pending | ApprovalStatus::Escalated
        ) {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalNotPending,
                Some(&record),
            ));
        }
        let run_id =
            approval_run_id(&events, params.approval_id).ok_or(EngineError::ApprovalConflict)?;
        if approval_deadline_exhausted(approval_constraints(&record.request).expires_at) {
            self.approval_terminal_direct(
                params.session_id,
                run_id,
                params.approval_id,
                ApprovalTerminal::Expired,
            )?;
            let current = approval_records(
                params.session_id,
                &self.inner.store.get(params.session_id)?.log.events(),
            )
            .remove(&params.approval_id);
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalNotPending,
                current.as_ref(),
            ));
        }
        if record.status != ApprovalStatus::Escalated {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalNotPending,
                Some(&record),
            ));
        }
        if approval_request_revision(&record.request) != params.request_revision {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalRevisionConflict,
                Some(&record),
            ));
        }
        if record.request.operation_fingerprint() != &params.operation_fingerprint {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::OperationFingerprintMismatch,
                Some(&record),
            ));
        }
        let allowed = match params.decision {
            ApprovalUserDecision::ApproveOnce => approval_constraints(&record.request).allow_once,
            ApprovalUserDecision::ApproveTree => {
                approval_constraints(&record.request).allow_tree_grant
            }
            ApprovalUserDecision::Reject => true,
            ApprovalUserDecision::Cancel => approval_constraints(&record.request).cancellable,
        };
        if !allowed {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::DecisionNotAllowed,
                Some(&record),
            ));
        }
        self.append_direct(
            params.session_id,
            Some(run_id),
            Event::ApprovalUserDecisionRecorded {
                approval_id: params.approval_id,
                client_response_id: params.client_response_id.clone(),
                decision: params.decision,
                feedback: params.feedback.clone(),
            },
        )?;
        let root = root_id(&projection.meta.origin, params.session_id);
        let grant = if params.decision == ApprovalUserDecision::ApproveTree {
            let grant = TreeApprovalGrant {
                grant_id: cookie_agent_protocol::TreeApprovalGrantId::new_v7(),
                root_session_id: root,
                approval_id: params.approval_id,
                operation_fingerprint: record.request.operation_fingerprint().clone(),
                capabilities: record.request.operation().capabilities().to_vec(),
                resources: record.request.operation().resources().to_vec(),
                created_at: jiff::Timestamp::now(),
            };
            grant
                .validate()
                .map_err(|_| EngineError::ApprovalConflict)?;
            Some(grant)
        } else {
            None
        };
        if let Some(grant) = &grant {
            self.append_direct(
                params.session_id,
                Some(run_id),
                Event::TreeApprovalGrantCommitted {
                    grant: grant.clone(),
                },
            )?;
            self.inner.approvals.grant(grant.clone());
        }
        let (outcome, reason_code, approved) = match params.decision {
            ApprovalUserDecision::ApproveOnce => (
                ApprovalFinalOutcome::Approved,
                ApprovalReasonCode::UserApprovedOnce,
                true,
            ),
            ApprovalUserDecision::ApproveTree => (
                ApprovalFinalOutcome::Approved,
                ApprovalReasonCode::UserApprovedTree,
                true,
            ),
            ApprovalUserDecision::Reject => (
                ApprovalFinalOutcome::Rejected,
                ApprovalReasonCode::UserRejected,
                false,
            ),
            ApprovalUserDecision::Cancel => (
                ApprovalFinalOutcome::Cancelled,
                ApprovalReasonCode::UserCancelled,
                false,
            ),
        };
        self.append_direct(
            params.session_id,
            Some(run_id),
            Event::ApprovalFinalized {
                approval_id: params.approval_id,
                decision: ApprovalFinalDecision {
                    outcome,
                    source: ApprovalDecisionSource::User,
                    reason_code,
                    feedback: params.feedback.clone(),
                    tree_grant_id: grant.as_ref().map(|grant| grant.grant_id),
                },
            },
        )?;
        if let Some(pending) = self
            .inner
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(params.session_id, params.approval_id))
        {
            let _ = pending.sender.send(ApprovalOutcome {
                approved,
                feedback: params
                    .feedback
                    .as_ref()
                    .map(|feedback| feedback.message.to_string()),
            });
        }
        let events = self.inner.store.get(params.session_id)?.log.events();
        let approval = approval_records(params.session_id, &events)
            .remove(&params.approval_id)
            .ok_or(EngineError::ApprovalConflict)?;
        Ok(ApprovalRespondResult {
            client_response_id: params.client_response_id,
            approval,
        })
    }

    fn approval_capability_invalid_direct(
        &self,
        params: ApprovalRespondParams,
        invalidation: PreparedApprovalInvalidation,
    ) -> Result<ApprovalRespondResult, EngineError> {
        let projection = self.inner.store.get(params.session_id)?;
        let events = projection.log.events();
        if events.iter().any(|event| {
            matches!(
                &event.payload,
                Event::ApprovalUserDecisionRecorded { client_response_id, .. }
                    if client_response_id == &params.client_response_id
            )
        }) {
            return self.approval_respond_direct(params);
        }
        let mut records = approval_records(params.session_id, &events);
        let Some(record) = records.remove(&params.approval_id) else {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalNotFound,
                None,
            ));
        };
        if record.status != ApprovalStatus::Escalated {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalNotPending,
                Some(&record),
            ));
        }
        if approval_request_revision(&record.request) != params.request_revision {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalRevisionConflict,
                Some(&record),
            ));
        }
        if record.request.operation_fingerprint() != &params.operation_fingerprint {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::OperationFingerprintMismatch,
                Some(&record),
            ));
        }
        let run_id =
            approval_run_id(&events, params.approval_id).ok_or(EngineError::ApprovalConflict)?;
        let reason_code = match invalidation {
            PreparedApprovalInvalidation::OperationChanged => ApprovalReasonCode::OperationChanged,
            PreparedApprovalInvalidation::PreparedCapabilityLost => {
                ApprovalReasonCode::PreparedCapabilityLost
            }
        };
        self.append_direct(
            params.session_id,
            Some(run_id),
            Event::ApprovalCancelled {
                approval_id: params.approval_id,
                reason_code,
            },
        )?;
        self.append_direct(
            params.session_id,
            Some(run_id),
            Event::ApprovalFinalized {
                approval_id: params.approval_id,
                decision: ApprovalFinalDecision {
                    outcome: ApprovalFinalOutcome::Cancelled,
                    source: ApprovalDecisionSource::System,
                    reason_code,
                    feedback: None,
                    tree_grant_id: None,
                },
            },
        )?;
        if let Some(pending) = self
            .inner
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(params.session_id, params.approval_id))
        {
            let _ = pending.sender.send(ApprovalOutcome {
                approved: false,
                feedback: Some(match invalidation {
                    PreparedApprovalInvalidation::OperationChanged => {
                        "prepared operation changed before approval response".into()
                    }
                    PreparedApprovalInvalidation::PreparedCapabilityLost => {
                        "prepared capability was lost before approval response".into()
                    }
                }),
            });
        }
        let current = approval_records(
            params.session_id,
            &self.inner.store.get(params.session_id)?.log.events(),
        )
        .remove(&params.approval_id);
        Err(approval_response_failure(
            &params,
            ApprovalRespondErrorCode::OperationChanged,
            current.as_ref(),
        ))
    }

    fn approval_terminal_direct(
        &self,
        session: SessionId,
        run: RunId,
        approval_id: ApprovalId,
        terminal: ApprovalTerminal,
    ) -> Result<bool, EngineError> {
        let events = self.inner.store.get(session)?.log.events();
        let Some(record) = approval_records(session, &events).remove(&approval_id) else {
            return Ok(false);
        };
        if !matches!(
            record.status,
            ApprovalStatus::Pending | ApprovalStatus::Escalated
        ) || approval_run_id(&events, approval_id) != Some(run)
        {
            return Ok(false);
        }
        let (reason_code, outcome, final_reason) = match terminal {
            ApprovalTerminal::Cancelled => (
                ApprovalReasonCode::RequestCancelled,
                ApprovalFinalOutcome::Cancelled,
                ApprovalReasonCode::RequestCancelled,
            ),
            ApprovalTerminal::Expired => (
                ApprovalReasonCode::ApprovalExpired,
                ApprovalFinalOutcome::Expired,
                ApprovalReasonCode::ApprovalExpired,
            ),
        };
        self.append_direct(
            session,
            Some(run),
            Event::ApprovalCancelled {
                approval_id,
                reason_code,
            },
        )?;
        self.append_direct(
            session,
            Some(run),
            Event::ApprovalFinalized {
                approval_id,
                decision: ApprovalFinalDecision {
                    outcome,
                    source: ApprovalDecisionSource::System,
                    reason_code: final_reason,
                    feedback: None,
                    tree_grant_id: None,
                },
            },
        )?;
        if let Some(pending) = self
            .inner
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(session, approval_id))
        {
            let _ = pending.sender.send(ApprovalOutcome {
                approved: false,
                feedback: Some(match terminal {
                    ApprovalTerminal::Cancelled => "cancelled".into(),
                    ApprovalTerminal::Expired => "approval expired unattended".into(),
                }),
            });
        }
        Ok(true)
    }

    #[must_use]
    pub fn list_approvals(
        &self,
        root_session_id: SessionId,
        status: Option<ApprovalStatus>,
    ) -> ApprovalListResult {
        let approvals = self
            .inner
            .store
            .all()
            .into_iter()
            .filter(|session| {
                root_id(&session.meta.origin, session.meta.session_id) == root_session_id
            })
            .flat_map(|session| {
                approval_records(session.meta.session_id, &session.log.events()).into_values()
            })
            .filter(|record| status.is_none_or(|status| record.status == status))
            .collect();
        ApprovalListResult {
            approvals,
            tree_grants: self.inner.approvals.for_root(root_session_id),
        }
    }

    fn tool_definitions(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
    ) -> Result<Vec<ToolDefinition>, EngineError> {
        let depth = session_depth(&self.inner.store.get(session)?.meta.origin);
        let delegate_enabled = policy.agent.delegation.as_ref().is_some_and(|delegation| {
            depth < delegation.effective_depth_ceiling
                && delegation.targets.iter().any(|target| {
                    policy.registry.get(target).is_some_and(|agent| {
                        agent.document.frontmatter.enabled
                            && matches!(
                                agent.document.frontmatter.mode,
                                cookie_agent_config::AgentMode::Subagent
                                    | cookie_agent_config::AgentMode::All
                            )
                    })
                })
        });
        let enabled_tools = policy.tools();
        let mut names = HashSet::new();
        let mut output = Vec::new();
        let providers = self
            .inner
            .tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for provider in &providers {
            for tool in provider
                .tools_for_session(&SessionToolContext { session })
                .map_err(|error| EngineError::MissingTool(error.to_string()))?
            {
                if ((tool.name != "delegate"
                    && enabled_tools.contains(&tool.name)
                    && PermissionPipeline::tool_visible(&policy.agent, &tool.name))
                    || (tool.name == "delegate" && delegate_enabled))
                    && names.insert(tool.name.clone())
                {
                    let schema = JsonSchema::new(tool.parameters).map_err(|error| {
                        EngineError::MissingTool(format!(
                            "tool `{}` has invalid JSON Schema: {error}",
                            tool.name
                        ))
                    })?;
                    output.push(ToolDefinition::new(tool.name, tool.description, schema));
                }
            }
        }
        output.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(output)
    }

    fn reconcile(&self) -> Result<(), EngineError> {
        // Every active run from a previous process is terminally interrupted.
        for session in self.inner.store.all() {
            let mut internal = HashMap::new();
            for event in session.log.events() {
                match event.payload {
                    Event::InternalAgentStarted {
                        invocation_id,
                        internal_run_id,
                        kind,
                        ..
                    } => {
                        internal.insert((invocation_id, internal_run_id), (kind, event.run_id));
                    }
                    Event::InternalAgentCompleted {
                        invocation_id,
                        internal_run_id,
                        ..
                    }
                    | Event::InternalAgentFailed {
                        invocation_id,
                        internal_run_id,
                        ..
                    }
                    | Event::InternalAgentCancelled {
                        invocation_id,
                        internal_run_id,
                        ..
                    }
                    | Event::InternalAgentInterrupted {
                        invocation_id,
                        internal_run_id,
                        ..
                    } => {
                        internal.remove(&(invocation_id, internal_run_id));
                    }
                    _ => {}
                }
            }
            for ((invocation_id, internal_run_id), (kind, parent_run)) in internal {
                self.append_blocking(
                    session.meta.session_id,
                    parent_run,
                    Event::InternalAgentInterrupted {
                        invocation_id,
                        internal_run_id,
                        kind,
                        reason: Some(safe_error("daemon restart")),
                    },
                )?;
            }
            for run in session
                .runs
                .values()
                .filter(|run| run.status == SessionStatus::Running)
            {
                self.append_blocking(
                    session.meta.session_id,
                    Some(run.id),
                    Event::RunInterrupted {
                        reason: Some(safe_error("daemon restart")),
                    },
                )?;
            }
            for run in session.runs.values() {
                for (tool_call_id, tool) in &run.pending_calls {
                    if tool == "delegate" {
                        continue;
                    }
                    let failure = restart_tool_failure();
                    self.append_blocking(
                        session.meta.session_id,
                        Some(run.id),
                        Event::ToolCallTerminated {
                            termination: ToolCallTermination {
                                tool_call_id: *tool_call_id,
                                owner: self.tool_call_owner(
                                    session.meta.session_id,
                                    run.id,
                                    *tool_call_id,
                                )?,
                                outcome: ToolTerminationOutcome::Interrupted,
                                result: None,
                                error: Some(SafeToolError {
                                    code: failure.code.safe_code(),
                                    message: safe_error(&failure.message),
                                }),
                            },
                        },
                    )?;
                }
            }
            for record in approval_records(session.meta.session_id, &session.log.events())
                .into_values()
                .filter(|record| {
                    matches!(
                        record.status,
                        ApprovalStatus::Pending | ApprovalStatus::Escalated
                    )
                })
            {
                let approval_run =
                    approval_run_id(&session.log.events(), record.request.approval_id())
                        .ok_or(EngineError::ApprovalConflict)?;
                self.append_blocking(
                    session.meta.session_id,
                    Some(approval_run),
                    Event::ApprovalCancelled {
                        approval_id: record.request.approval_id(),
                        reason_code: ApprovalReasonCode::PreparedCapabilityLost,
                    },
                )?;
                self.append_blocking(
                    session.meta.session_id,
                    Some(approval_run),
                    Event::ApprovalFinalized {
                        approval_id: record.request.approval_id(),
                        decision: ApprovalFinalDecision {
                            outcome: ApprovalFinalOutcome::Cancelled,
                            source: ApprovalDecisionSource::System,
                            reason_code: ApprovalReasonCode::PreparedCapabilityLost,
                            feedback: None,
                            tree_grant_id: None,
                        },
                    },
                )?;
            }
        }
        let journal_entries = self.inner.journal.entries();
        for entry in &journal_entries {
            if self
                .inner
                .store
                .get(entry.reservation.child_session_id)
                .is_ok()
            {
                let parent = self.inner.store.get(entry.reservation.parent_session_id)?;
                let parent_cancelled = parent
                    .runs
                    .get(&entry.reservation.parent_run_id)
                    .is_some_and(|run| run.status == SessionStatus::Cancelled);
                if parent_cancelled {
                    let child = self.inner.store.get(entry.reservation.child_session_id)?;
                    for run in child.runs.values().filter(|run| {
                        matches!(
                            run.status,
                            SessionStatus::Running | SessionStatus::Interrupted
                        )
                    }) {
                        self.append_blocking(
                            entry.reservation.child_session_id,
                            Some(run.id),
                            Event::RunCancelled {
                                reason: Some(safe_error("parent delegate run was cancelled")),
                            },
                        )?;
                    }
                }
                self.ensure_parent_link_blocking(
                    entry.reservation.parent_session_id,
                    entry.reservation.parent_run_id,
                    entry.reservation.parent_tool_call_id,
                    entry.reservation.child_session_id,
                )?;
                if !entry.linked {
                    self.inner
                        .journal
                        .mark_linked(entry.reservation.invocation_id)?;
                }
            }
        }
        let known_invocations: HashSet<_> = journal_entries
            .iter()
            .map(|entry| entry.reservation.invocation_id)
            .collect();
        for session in self.inner.store.all() {
            if let SessionOrigin::Delegated { invocation_id, .. } = session.meta.origin
                && !known_invocations.contains(&invocation_id)
            {
                // A valid delegated directory without a durable reservation is
                // foreign/orphaned. Preserve it for inspection but never attach it.
                if !session.runs.is_empty() {
                    for run in session
                        .runs
                        .values()
                        .filter(|run| run.status != SessionStatus::Interrupted)
                    {
                        self.append_blocking(
                            session.meta.session_id,
                            Some(run.id),
                            Event::RunInterrupted {
                                reason: Some(safe_error(
                                    "orphaned delegated session without journal reservation",
                                )),
                            },
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn resolve_interrupted_direct(&self, session_id: SessionId) -> Result<(), EngineError> {
        let session = self.inner.store.get(session_id)?;
        let approval_records = approval_records(session_id, &session.log.events());
        for record in approval_records.values().filter(|record| {
            matches!(
                record.status,
                ApprovalStatus::Pending | ApprovalStatus::Escalated
            )
        }) {
            let Some(run_id) = approval_run_id(&session.log.events(), record.request.approval_id())
            else {
                continue;
            };
            let decision = restart_approval_decision();
            self.append_direct(
                session_id,
                Some(run_id),
                Event::ApprovalCancelled {
                    approval_id: record.request.approval_id(),
                    reason_code: ApprovalReasonCode::PreparedCapabilityLost,
                },
            )?;
            self.append_direct(
                session_id,
                Some(run_id),
                Event::ApprovalFinalized {
                    approval_id: record.request.approval_id(),
                    decision,
                },
            )?;
        }
        for run in session.runs.values().filter(|run| {
            matches!(
                run.status,
                SessionStatus::Interrupted | SessionStatus::Cancelled
            )
        }) {
            for (call, tool) in &run.pending_calls {
                if tool == "delegate" {
                    let recovery_key = (session_id, run.id, *call);
                    if self
                        .inner
                        .recovery_waiters
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .contains(&recovery_key)
                    {
                        continue;
                    }
                    let invocation = invocation_id(session_id, run.id, *call);
                    let Some(entry) = self.journal_get(invocation).await? else {
                        self.terminate_tool_direct(
                            session_id,
                            run.id,
                            *call,
                            ToolTerminationOutcome::Interrupted,
                            Some(delegate_failure_result(
                                None,
                                "delegate interrupted by daemon restart: no durable reservation",
                            )),
                            Some(SafeToolError {
                                code: safe_code("restart_interrupted"),
                                message: safe_error(
                                    "delegate reservation is missing after restart",
                                ),
                            }),
                        )?;
                        continue;
                    };
                    let child_id = entry.reservation.child_session_id;
                    if run.status == SessionStatus::Cancelled {
                        let result = cancelled_delegate_result_with_reason(
                            Some(child_id),
                            "parent delegate run was cancelled",
                        );
                        self.terminate_tool_direct(
                            session_id,
                            run.id,
                            *call,
                            ToolTerminationOutcome::Cancelled,
                            Some(result),
                            Some(SafeToolError {
                                code: safe_code("parent_cancelled"),
                                message: safe_error("parent delegate run was cancelled"),
                            }),
                        )?;
                        continue;
                    }
                    let child = match self.inner.store.get(child_id) {
                        Ok(child) => child,
                        Err(_) => {
                            self.terminate_tool_direct(
                                session_id,
                                run.id,
                                *call,
                                ToolTerminationOutcome::Failed,
                                Some(delegate_failure_result(
                                    Some(child_id),
                                    "delegate child session is missing",
                                )),
                                Some(SafeToolError {
                                    code: safe_code("child_missing"),
                                    message: safe_error("delegate child session is missing"),
                                }),
                            )?;
                            continue;
                        }
                    };
                    if child.status == SessionStatus::Completed {
                        let result = completed_delegate_result(
                            &child,
                            entry.child_run_id,
                            &self.inner.artifacts,
                        )?;
                        self.terminate_tool_direct(
                            session_id,
                            run.id,
                            *call,
                            ToolTerminationOutcome::Completed,
                            Some(result),
                            None,
                        )?;
                    } else if child.status == SessionStatus::Cancelled {
                        let result = cancelled_delegate_result(child_id, None);
                        self.terminate_tool_direct(
                            session_id,
                            run.id,
                            *call,
                            ToolTerminationOutcome::Cancelled,
                            Some(result),
                            Some(SafeToolError {
                                code: safe_code("child_cancelled"),
                                message: safe_error("delegate child was cancelled"),
                            }),
                        )?;
                    } else if entry.child_run_id.is_none() {
                        self.inner
                            .recovery_waiters
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .insert(recovery_key);
                        let child_run_id = match self.ensure_delegate_run(&entry, None).await {
                            Ok(run_id) => run_id,
                            Err(error) => {
                                self.inner
                                    .recovery_waiters
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .remove(&recovery_key);
                                if is_journal_append_failure(&error) {
                                    let _ = self.resolve_delegate_failure_if_pending_direct(
                                        session_id,
                                        run.id,
                                        *call,
                                        delegate_failure_result(
                                            Some(child_id),
                                            "delegate journal run confirmation failed",
                                        ),
                                    );
                                }
                                return Err(error);
                            }
                        };
                        let engine = self.clone();
                        let parent_run_id = run.id;
                        let tool_call_id = *call;
                        tokio::spawn(async move {
                            let result = engine
                                .await_delegate(DelegateHandle {
                                    invocation_id: entry.reservation.invocation_id,
                                    child_session_id: child_id,
                                    child_run_id,
                                })
                                .await;
                            if let Ok(result) = result {
                                let _ = engine
                                    .submit_tool_result(
                                        session_id,
                                        parent_run_id,
                                        tool_call_id,
                                        Ok(result),
                                    )
                                    .await;
                            }
                            engine
                                .inner
                                .recovery_waiters
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .remove(&(session_id, parent_run_id, tool_call_id));
                        });
                    } else {
                        self.terminate_tool_direct(
                            session_id,
                            run.id,
                            *call,
                            ToolTerminationOutcome::Interrupted,
                            Some(delegate_failure_result(
                                Some(child_id),
                                "delegate child interrupted by daemon restart",
                            )),
                            Some(SafeToolError {
                                code: safe_code("child_interrupted"),
                                message: safe_error("delegate child interrupted by daemon restart"),
                            }),
                        )?;
                    }
                } else {
                    let failure = restart_tool_failure();
                    self.terminate_tool_direct(
                        session_id,
                        run.id,
                        *call,
                        ToolTerminationOutcome::Interrupted,
                        None,
                        Some(SafeToolError {
                            code: failure.code.safe_code(),
                            message: safe_error(&failure.message),
                        }),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn tool_call_owner(
        &self,
        session_id: SessionId,
        run_id: RunId,
        tool_call_id: ToolCallId,
    ) -> Result<cookie_agent_protocol::AssistantToolCallRef, EngineError> {
        self.inner
            .store
            .get(session_id)?
            .log
            .events()
            .into_iter()
            .find_map(|event| match event.payload {
                Event::ToolCallStarted { start }
                    if event.run_id == Some(run_id) && start.tool_call_id == tool_call_id =>
                {
                    Some(start.owner)
                }
                _ => None,
            })
            .ok_or_else(|| EngineError::MissingTool("tool ownership is missing".into()))
    }

    fn terminate_tool_direct(
        &self,
        session_id: SessionId,
        run_id: RunId,
        tool_call_id: ToolCallId,
        outcome: ToolTerminationOutcome,
        result: Option<ToolResult>,
        error: Option<SafeToolError>,
    ) -> Result<(), EngineError> {
        self.append_direct(
            session_id,
            Some(run_id),
            Event::ToolCallTerminated {
                termination: ToolCallTermination {
                    tool_call_id,
                    owner: self.tool_call_owner(session_id, run_id, tool_call_id)?,
                    outcome,
                    result,
                    error,
                },
            },
        )
    }

    fn next_model_turn_seq(&self, session_id: SessionId) -> Result<u64, EngineError> {
        Ok(self
            .inner
            .store
            .get(session_id)?
            .log
            .events()
            .iter()
            .filter(|event| matches!(event.payload, Event::ModelTurnCommitted { .. }))
            .count() as u64
            + 1)
    }

    fn run_agent_prompt(
        &self,
        session_id: SessionId,
        run_id: RunId,
    ) -> Result<String, EngineError> {
        self.inner
            .store
            .get(session_id)?
            .log
            .events()
            .into_iter()
            .find_map(|event| match event.payload {
                Event::RunStarted { agent, .. } if event.run_id == Some(run_id) => {
                    Some(agent.composed_prompt)
                }
                _ => None,
            })
            .ok_or(EngineError::MissingRun(run_id))
    }

    fn rebuild_approvals(&self) {
        for session in self.inner.store.all() {
            for envelope in session.log.events() {
                if let Event::TreeApprovalGrantCommitted { grant } = envelope.payload
                    && grant.resources.iter().all(|resource| {
                        resource.binding_lifetime
                            == cookie_agent_protocol::PreparedBindingLifetime::RestartStable
                    })
                {
                    self.inner.approvals.grant(grant);
                }
            }
        }
        self.inner
            .approvals
            .invalidate_grants(&self.inner.grant_journal.invalidated_ids());
    }
}

fn restart_approval_decision() -> ApprovalFinalDecision {
    ApprovalFinalDecision {
        outcome: ApprovalFinalOutcome::Cancelled,
        source: ApprovalDecisionSource::System,
        reason_code: ApprovalReasonCode::PreparedCapabilityLost,
        feedback: None,
        tree_grant_id: None,
    }
}

fn restart_tool_failure() -> ToolFailure {
    ToolFailure {
        code: ToolCallFailureCode::PreparedCapabilityLost,
        message: "prepared capability lost during daemon restart".into(),
    }
}

fn session_meta(
    id: SessionId,
    origin: SessionOrigin,
    cwd_identity: cookie_agent_protocol::CwdIdentity,
    creation_selection: RunSelection,
) -> SessionMeta {
    SessionMeta {
        meta_schema_version: cookie_agent_protocol::SessionMetaSchemaVersion::current(),
        session_id: id,
        origin,
        cwd_identity,
        creation_selection,
        title: None,
        title_updated_seq: 0,
        last_event_seq: 1,
        status: SessionStatus::Idle,
    }
}

fn freeze_delegated_child_policy(
    child_agent: &cookie_agent_config::ResolvedAgent,
    parent_policy: &FrozenRunPolicy,
    child_selection: &cookie_agent_protocol::ModelSelection,
    inherited_suffix: &[cookie_agent_models::FrozenModelBinding],
    inherited_depth_ceiling: u32,
    result_limits: policy::ResultLimits,
) -> Result<FrozenRunPolicy, EngineError> {
    freeze_delegated_agent_policy(
        child_agent,
        Arc::clone(&parent_policy.registry),
        Arc::clone(&parent_policy.model_snapshot),
        child_selection,
        inherited_suffix,
        inherited_depth_ceiling,
        result_limits,
    )
}

fn unavailable_internal_policy(
    timeout_ms: u64,
    max_input_tokens: u64,
    max_output_tokens: u64,
) -> FrozenInternalAgentPolicy {
    FrozenInternalAgentPolicy {
        agent: cookie_agent_protocol::AgentSnapshot {
            agent: AgentId::new("internal").expect("static agent id"),
            schema: cookie_agent_protocol::AgentSchemaVersion::current(),
            mode: AgentMode::All,
            description: "Internal engine work".into(),
            document_source: cookie_agent_protocol::AgentDocumentSource::BuiltIn,
            document_fingerprint: Sha256Digest::of_bytes(b"internal"),
            composed_prompt: "Perform the requested internal engine task safely.\n".into(),
            prompt_fingerprint: Sha256Digest::of_bytes(
                b"Perform the requested internal engine task safely.\n",
            ),
            tools: Vec::new(),
            permissions: Vec::new(),
            delegation: None,
            fallback_chain: Vec::new(),
            selected_suffix_start: 0,
        },
        models: Vec::new(),
        model_snapshot: None,
        limits: InternalAgentLimits {
            max_input_tokens,
            max_output_tokens,
            timeout_ms,
        },
    }
}

fn inherit_internal_policy(
    configured: &FrozenInternalAgentPolicy,
    owner: &FrozenRunPolicy,
    active_suffix: &[cookie_agent_models::FrozenModelBinding],
) -> FrozenInternalAgentPolicy {
    FrozenInternalAgentPolicy {
        agent: owner.agent.clone(),
        models: if configured.models.is_empty() {
            active_suffix.to_vec()
        } else {
            configured.models.clone()
        },
        model_snapshot: Some(Arc::clone(&owner.model_snapshot)),
        limits: configured.limits.clone(),
    }
}

fn wire_agent_descriptor(value: cookie_agent_config::AgentDescriptor) -> AgentDescriptor {
    AgentDescriptor {
        id: value.id,
        description: value.description,
        mode: match value.mode {
            cookie_agent_config::AgentMode::Primary => AgentMode::Primary,
            cookie_agent_config::AgentMode::Subagent => AgentMode::Subagent,
            cookie_agent_config::AgentMode::All => AgentMode::All,
        },
        enabled: value.enabled,
        runnable_as_root: value.runnable_as_root,
        resolved_fallback: value.resolved_fallback,
        tools: value
            .tools
            .into_iter()
            .map(|tool| match tool {
                cookie_agent_config::ToolName::Read => cookie_agent_protocol::ToolName::Read,
                cookie_agent_config::ToolName::Write => cookie_agent_protocol::ToolName::Write,
                cookie_agent_config::ToolName::Edit => cookie_agent_protocol::ToolName::Edit,
                cookie_agent_config::ToolName::Bash => cookie_agent_protocol::ToolName::Bash,
                cookie_agent_config::ToolName::Grep => cookie_agent_protocol::ToolName::Grep,
                cookie_agent_config::ToolName::Glob => cookie_agent_protocol::ToolName::Glob,
            })
            .collect(),
        delegation_targets: value.delegation_targets,
    }
}

fn root_id(origin: &SessionOrigin, session: SessionId) -> SessionId {
    match origin {
        SessionOrigin::Delegated {
            root_session_id, ..
        } => *root_session_id,
        _ => session,
    }
}

fn session_depth(origin: &SessionOrigin) -> u32 {
    match origin {
        SessionOrigin::Root => 0,
        SessionOrigin::Delegated { depth, .. } => *depth,
    }
}

fn cwd_identity(path: &Path) -> Result<cookie_agent_protocol::CwdIdentity, EngineError> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    cookie_agent_protocol::CwdIdentity::new(canonical.to_string_lossy()).map_err(|error| {
        EngineError::from(ModelError::invalid_request(format!(
            "invalid cwd identity: {error}"
        )))
    })
}

fn protocol_digest(value: &cookie_agent_models::Sha256Digest) -> Result<Sha256Digest, EngineError> {
    Sha256Digest::new(value.as_str())
        .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))
}

fn invocation_id(session: SessionId, run: RunId, call: ToolCallId) -> InvocationId {
    InvocationId(Uuid::from_u128(hash_parts(&[
        &session.to_string(),
        &run.to_string(),
        &call.to_string(),
    ])))
}
fn hash_parts(parts: &[&str]) -> u128 {
    use std::hash::{Hash, Hasher};
    let mut first = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut first);
    let high = first.finish() as u128;
    let mut second = std::collections::hash_map::DefaultHasher::new();
    "cookie_agent".hash(&mut second);
    parts.hash(&mut second);
    (high << 64) | second.finish() as u128
}

fn safe_code(value: &str) -> SafeCode {
    SafeCode::new(value).expect("static safe code")
}

fn safe_display(value: &str) -> SafeDisplayText {
    SafeDisplayText::new(sanitize_safe_text(value, SafeDisplayText::MAX_BYTES))
        .expect("sanitized display text")
}

fn safe_error(value: &str) -> SafeErrorMessage {
    SafeErrorMessage::new(sanitize_safe_text(value, SafeErrorMessage::MAX_BYTES))
        .expect("sanitized safe error")
}

fn sanitize_safe_text(value: &str, maximum: usize) -> String {
    let mut output = String::new();
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if output.len() + character.len_utf8() > maximum {
            break;
        }
        output.push(character);
    }
    if output.is_empty() {
        "unavailable".into()
    } else {
        output
    }
}

fn approval_evaluations(request: &ApprovalRequest) -> Vec<ApprovalEvaluation> {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| value.get("evaluations").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .expect("protocol approval request serializes evaluations")
}

fn approval_constraints(request: &ApprovalRequest) -> ApprovalConstraints {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| value.get("constraints").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .expect("protocol approval request serializes constraints")
}

fn latest_run_policy(
    events: &[StoredEvent],
    run_id: RunId,
) -> Result<
    (
        cookie_agent_protocol::AgentSnapshot,
        Vec<cookie_agent_protocol::FrozenModelBinding>,
    ),
    EngineError,
> {
    events
        .iter()
        .find_map(|event| match &event.payload {
            Event::RunStarted {
                agent,
                selected_suffix,
                ..
            } if event.run_id == Some(run_id) => {
                Some((agent.as_ref().clone(), selected_suffix.clone()))
            }
            _ => None,
        })
        .ok_or(EngineError::MissingRun(run_id))
}

fn title_regeneration_target(events: &[StoredEvent]) -> Option<(RunId, u64, PersistedModelTurn)> {
    events.iter().rev().find_map(|event| match &event.payload {
        Event::ModelTurnCommitted {
            input_through_seq,
            turn,
            ..
        } => event
            .run_id
            .map(|run| (run, *input_through_seq, turn.clone())),
        _ => None,
    })
}

fn active_fallback_index(events: &[StoredEvent], run_id: RunId) -> usize {
    events
        .iter()
        .rev()
        .find_map(|event| {
            if event.run_id != Some(run_id) {
                return None;
            }
            match &event.payload {
                Event::ModelAttemptStarted { fallback_index, .. } => Some(*fallback_index as usize),
                Event::ModelFallback {
                    to_fallback_index, ..
                } => Some(*to_fallback_index as usize),
                _ => None,
            }
        })
        .unwrap_or(0)
}

fn fallback_operation_fingerprint(call: &ToolCall) -> OperationFingerprint {
    let action = PermissionPipeline::action_for_tool(&call.name)
        .unwrap_or(cookie_agent_protocol::PermissionAction::Read);
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(
            &serde_json::to_vec(&call.arguments).expect("tool arguments serialize"),
        ),
        vec![cookie_agent_protocol::ApprovalCapability {
            action,
            operation: cookie_agent_protocol::PreparedCapabilityOperation::new(format!(
                "{}:prepare",
                call.name
                    .replace(|character: char| !character.is_ascii_alphanumeric(), "_")
            ))
            .unwrap_or_else(|_| {
                cookie_agent_protocol::PreparedCapabilityOperation::new("tool:prepare")
                    .expect("static operation")
            }),
        }],
        Vec::new(),
        Sha256Digest::of_bytes(b"prepare-failed"),
    )
    .expect("fallback prepared identity");
    OperationFingerprint::from_prepared_operation(&operation)
}

fn safe_tool_presentation(call: &ToolCall) -> ToolCallPresentation {
    let primary = match call.name.as_str() {
        "bash" => call.arguments.get("command").and_then(Value::as_str),
        "read" | "write" | "edit" => call
            .arguments
            .get("filePath")
            .or_else(|| call.arguments.get("file_path"))
            .or_else(|| call.arguments.get("path"))
            .and_then(Value::as_str),
        "grep" => call.arguments.get("pattern").and_then(Value::as_str),
        "glob" => call.arguments.get("pattern").and_then(Value::as_str),
        "delegate" => {
            let agent = call.arguments.get("agent").and_then(Value::as_str);
            let task = call.arguments.get("task").and_then(Value::as_str);
            return ToolCallPresentation {
                title: safe_display(&call.name),
                primary_argument: agent.map(|agent| {
                    let agent = redact_presentation(agent);
                    let task = task
                        .map(|task| redact_presentation(&truncate_utf8(task, 160)))
                        .filter(|task| !task.is_empty());
                    safe_display(&sanitize_safe_text(
                        &task.map_or(agent.clone(), |task| format!("{agent}: {task}")),
                        SafeDisplayText::MAX_BYTES,
                    ))
                }),
            };
        }
        _ => None,
    }
    .map(redact_presentation);
    ToolCallPresentation {
        title: safe_display(&call.name),
        primary_argument: primary.as_deref().map(safe_display),
    }
}

fn redact_presentation(value: &str) -> String {
    const SECRET_MARKERS: [&str; 6] = [
        "token",
        "secret",
        "password",
        "api_key",
        "apikey",
        "authorization",
    ];
    let mut sanitized = sanitize_safe_text(value, 512);
    let lowercase = sanitized.to_ascii_lowercase();
    if SECRET_MARKERS
        .iter()
        .any(|marker| lowercase.contains(marker))
    {
        sanitized = "<redacted>".into();
    }
    sanitized
}
fn approval_request_for_operation(
    trigger: ApprovalTrigger,
    operation: PreparedOperationIdentity,
    traces: Vec<cookie_agent_protocol::DecisionTrace>,
    allow_tree_grant: bool,
    expires_at: Option<jiff::Timestamp>,
) -> ApprovalRequest {
    let evaluations = operation
        .resources()
        .iter()
        .zip(traces)
        .map(|(resource, trace)| ApprovalEvaluation {
            resource_digest: resource.binding_digest.clone(),
            effect: trace.effect,
            trace,
        })
        .collect::<Vec<_>>();
    ApprovalRequest::new(
        ApprovalId::new_v7(),
        1,
        trigger,
        operation,
        evaluations,
        ApprovalConstraints {
            allow_once: true,
            allow_tree_grant,
            cancellable: true,
            expires_at,
        },
    )
    .expect("prepared approval request is complete")
}

fn approval_expiry(timeout_ms: u64) -> Option<jiff::Timestamp> {
    jiff::Timestamp::now()
        .checked_add(std::time::Duration::from_millis(timeout_ms))
        .ok()
}

fn approval_response_failure(
    params: &ApprovalRespondParams,
    code: ApprovalRespondErrorCode,
    current: Option<&ApprovalRecord>,
) -> EngineError {
    EngineError::ApprovalResponse(Box::new(ApprovalRespondFailure {
        code,
        session_id: params.session_id,
        approval_id: params.approval_id,
        client_response_id: params.client_response_id.to_string(),
        current_status: current.map(|record| record.status),
        current_revision: current.map(|record| approval_request_revision(&record.request)),
        current_expires_at: current
            .and_then(|record| approval_constraints(&record.request).expires_at),
        current_operation_fingerprint: current
            .map(|record| record.request.operation_fingerprint().clone()),
    }))
}

fn approval_deadline_exhausted(expires_at: Option<jiff::Timestamp>) -> bool {
    expires_at.is_some_and(|expires_at| expires_at <= jiff::Timestamp::now())
}

fn approval_expiry_wait(expires_at: Option<jiff::Timestamp>) -> std::time::Duration {
    let Some(expires_at) = expires_at else {
        return std::time::Duration::from_secs(100 * 365 * 24 * 60 * 60);
    };
    let now = jiff::Timestamp::now();
    if expires_at <= now {
        std::time::Duration::ZERO
    } else {
        expires_at.duration_since(now).unsigned_abs()
    }
}

fn approval_request_revision(request: &ApprovalRequest) -> u64 {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| value.get("revision").and_then(Value::as_u64))
        .expect("protocol approval request always serializes its revision")
}

fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn validate_generated_title(value: &str, max_chars: usize) -> Option<SessionTitle> {
    let value = value
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches(['"', '\'', '`'])
        .trim();
    if value.is_empty() {
        return None;
    }
    let bounded = value.chars().take(max_chars).collect::<String>();
    SessionTitle::new(bounded).ok()
}

fn automatic_title_eligible(events: &[StoredEvent]) -> bool {
    let mut latest_automatic = None;
    let mut latest_user = None;
    for event in events {
        if let Event::SessionTitleCommitted { change, .. } = &event.payload {
            match change {
                SessionTitleChange::InternalAgentSet { .. }
                | SessionTitleChange::FallbackSet { .. } => latest_automatic = Some(event.seq),
                SessionTitleChange::UserSet { .. } | SessionTitleChange::UserClear { .. } => {
                    latest_user = Some((event.seq, false));
                }
                SessionTitleChange::UserReset { .. } => latest_user = Some((event.seq, true)),
            }
        }
    }
    match latest_user {
        Some((_, false)) => false,
        Some((reset_seq, true)) => latest_automatic.is_none_or(|seq| seq < reset_seq),
        None => latest_automatic.is_none(),
    }
}

fn parse_internal_approval(value: &str) -> Option<ApprovalInternalDecisionKind> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Decision {
        decision: String,
    }
    match serde_json::from_str::<Decision>(value.trim())
        .ok()?
        .decision
        .as_str()
    {
        "allow" => Some(ApprovalInternalDecisionKind::Allow),
        "deny" => Some(ApprovalInternalDecisionKind::Deny),
        "ask" => Some(ApprovalInternalDecisionKind::Ask),
        _ => None,
    }
}

fn fallback_title(input: &str, max_chars: usize) -> Option<SessionTitle> {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded = normalized.chars().take(max_chars).collect::<String>();
    SessionTitle::new(bounded).ok()
}

fn approval_records(
    session_id: SessionId,
    events: &[StoredEvent],
) -> HashMap<ApprovalId, ApprovalRecord> {
    let mut records = HashMap::<ApprovalId, ApprovalRecord>::new();
    for envelope in events {
        match &envelope.payload {
            Event::ApprovalRequested { request } => {
                records.insert(
                    request.approval_id(),
                    ApprovalRecord {
                        session_id,
                        request: request.clone(),
                        status: ApprovalStatus::Pending,
                        internal_decision: None,
                        user_decision: None,
                        final_decision: None,
                    },
                );
            }
            Event::ApprovalEvaluated {
                approval_id,
                decision,
            } => {
                if let Some(record) = records.get_mut(approval_id) {
                    record.internal_decision = Some(decision.clone());
                }
            }
            Event::ApprovalEscalated { approval_id, .. } => {
                if let Some(record) = records.get_mut(approval_id) {
                    record.status = ApprovalStatus::Escalated;
                }
            }
            Event::ApprovalUserDecisionRecorded {
                approval_id,
                decision,
                ..
            } => {
                if let Some(record) = records.get_mut(approval_id) {
                    record.user_decision = Some(*decision);
                }
            }
            Event::ApprovalFinalized {
                approval_id,
                decision,
            } => {
                if let Some(record) = records.get_mut(approval_id) {
                    record.status = match decision.outcome {
                        ApprovalFinalOutcome::Approved => ApprovalStatus::Approved,
                        ApprovalFinalOutcome::Rejected => ApprovalStatus::Rejected,
                        ApprovalFinalOutcome::Cancelled => ApprovalStatus::Cancelled,
                        ApprovalFinalOutcome::Expired => ApprovalStatus::Expired,
                    };
                    record.final_decision = Some(decision.clone());
                }
            }
            Event::ApprovalCancelled { approval_id, .. } => {
                if let Some(record) = records.get_mut(approval_id) {
                    record.status = ApprovalStatus::Cancelled;
                }
            }
            _ => {}
        }
    }
    records
}

fn approval_run_id(events: &[StoredEvent], approval_id: ApprovalId) -> Option<RunId> {
    events.iter().find_map(|event| match &event.payload {
        Event::ApprovalRequested { request } if request.approval_id() == approval_id => {
            event.run_id
        }
        _ => None,
    })
}

fn doom_loop_repetitions(
    events: &[StoredEvent],
    run_id: RunId,
    fingerprint: &OperationFingerprint,
) -> u32 {
    let mut starts = HashMap::<ToolCallId, OperationFingerprint>::new();
    let mut repetitions = 0_u32;
    for event in events.iter().filter(|event| event.run_id == Some(run_id)) {
        match &event.payload {
            Event::UserInputSubmitted { .. } | Event::UserInputApplied { .. } => {
                repetitions = 0;
            }
            Event::ApprovalRequested { request }
                if request.operation_fingerprint() == fingerprint =>
            {
                repetitions = repetitions.saturating_add(1);
            }
            Event::ToolCallStarted { start } => {
                starts.insert(start.tool_call_id, start.operation_fingerprint.clone());
            }
            Event::ToolCallTerminated { termination }
                if termination.outcome == ToolTerminationOutcome::Completed
                    && starts
                        .get(&termination.tool_call_id)
                        .is_some_and(|completed| completed != fingerprint) =>
            {
                repetitions = 0;
            }
            _ => {}
        }
    }
    repetitions
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeniedToolFailure {
    kind: String,
    source: ApprovalDecisionSource,
    reason: String,
    feedback: Option<String>,
}

fn denied_tool_failure(
    source: ApprovalDecisionSource,
    reason: impl Into<String>,
    feedback: Option<String>,
) -> String {
    serde_json::to_string(&DeniedToolFailure {
        kind: "tool_denied".into(),
        source,
        reason: reason.into(),
        feedback,
    })
    .expect("denied tool failure serializes")
}

struct TruncatedToolOutput {
    content: String,
}

fn truncate_tool_output(
    output: &str,
    max_lines: usize,
    max_bytes: usize,
) -> Option<TruncatedToolOutput> {
    let lines = output.split('\n').collect::<Vec<_>>();
    let line_truncated = lines.len() > max_lines;
    let mut preview = lines
        .iter()
        .take(max_lines)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let byte_truncated = output.len() > max_bytes || preview.len() > max_bytes;
    if !line_truncated && !byte_truncated {
        return None;
    }
    if preview.len() > max_bytes {
        let mut boundary = max_bytes;
        while boundary > 0 && !preview.is_char_boundary(boundary) {
            boundary -= 1;
        }
        preview.truncate(boundary);
    }
    Some(TruncatedToolOutput { content: preview })
}

fn bound_tool_result(
    mut result: ToolResult,
    _tool_name: &str,
    _call_id: ToolCallId,
    artifacts: &ArtifactStore,
    max_lines: usize,
    max_bytes: usize,
) -> std::io::Result<ToolResult> {
    let Some(preview) = truncate_tool_output(&result.output, max_lines, max_bytes) else {
        return Ok(result);
    };
    let original_bytes = result.output.len() as u64;
    let original_lines = result.output.split('\n').count() as u64;
    let (retained, _) = artifacts.retain(result.output.as_bytes())?;
    result.output = preview.content;
    result.truncation = Some(ToolOutputTruncation {
        original_bytes,
        original_lines,
        retained,
    });
    Ok(result)
}

fn prepare_private_directory(directory: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "artifact store root must be a non-symlink directory",
                ));
            }
            validate_owner(&metadata, "artifact store root")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(directory)?;
            let metadata = fs::symlink_metadata(directory)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "artifact store root must be a non-symlink directory",
                ));
            }
            validate_owner(&metadata, "artifact store root")?;
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

fn sha256_hex(content: &[u8]) -> String {
    Sha256::digest(content)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_attachment(mime_type: &str, path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(ToolError::resource_limit(format!(
            "attachment is {} bytes; the limit is {MAX_ATTACHMENT_BYTES} bytes",
            bytes.len()
        )));
    }
    let validated = approved_media_type(path, bytes)?
        .ok_or_else(|| ToolError::execution("attachment is not a supported image or PDF"))?;
    if validated != mime_type {
        return Err(ToolError::execution(format!(
            "attachment MIME mismatch: declared {mime_type}, validated {validated}"
        )));
    }
    Ok(())
}

fn delegate_client_run_id(invocation_id: InvocationId) -> cookie_agent_protocol::ClientRunId {
    cookie_agent_protocol::ClientRunId::new(format!("delegate:{invocation_id}"))
        .expect("bounded delegate client run id")
}

fn render_delegate_input(request: &journal::DelegateRequestPayload) -> String {
    // Stable, provider-neutral child prompt rendering retained in the journal.
    // JSON preserves arbitrary structured context and expected-output details.
    format!(
        "Task:\n{}\n\nContext:\n{}\n\nSuccess criteria:\n{}\n\nExpected output:\n{}",
        request.task,
        serde_json::to_string(&request.context).expect("delegate context serializes"),
        serde_json::to_string(&request.success_criteria).expect("success criteria serialize"),
        serde_json::to_string(&request.expected_output).expect("expected output serializes"),
    )
}

fn completed_delegate_result(
    child: &session::SessionProjection,
    child_run_id: Option<RunId>,
    artifacts: &ArtifactStore,
) -> std::io::Result<ToolResult> {
    let output = child_run_id
        .and_then(|child_run_id| child.runs.get(&child_run_id))
        .and_then(|run| run.final_text.clone())
        .unwrap_or_else(|| "child completed without a final report".into());
    bound_tool_result(
        ToolResult {
            title: safe_display("Delegate report"),
            output,
            metadata: serde_json::json!({
                "status": "completed",
                "child_session_id": child.meta.session_id,
            }),
            truncation: None,
            attachments: Vec::new(),
        },
        "delegate",
        ToolCallId::new_v7(),
        artifacts,
        usize::MAX,
        32 * 1024,
    )
}

fn structured_delegate_result(title: &str, metadata: Value) -> ToolResult {
    ToolResult {
        title: safe_display(title),
        output: metadata.to_string(),
        metadata,
        truncation: None,
        attachments: Vec::new(),
    }
}

fn cancelled_delegate_result(
    child_session_id: SessionId,
    partial_report: Option<String>,
) -> ToolResult {
    structured_delegate_result(
        "Delegate cancelled",
        serde_json::json!({
            "status": "cancelled",
            "child_session_id": child_session_id,
            "partial_report": partial_report,
        }),
    )
}

fn cancelled_delegate_result_with_reason(
    child_session_id: Option<SessionId>,
    reason: &str,
) -> ToolResult {
    structured_delegate_result(
        "Delegate cancelled",
        serde_json::json!({
            "status": "cancelled",
            "child_session_id": child_session_id,
            "reason": reason,
        }),
    )
}

fn delegate_failure_result(child_session_id: Option<SessionId>, reason: &str) -> ToolResult {
    structured_delegate_result(
        "Delegate failed",
        serde_json::json!({
            "status": "failed",
            "child_session_id": child_session_id,
            "reason": reason,
        }),
    )
}

fn is_journal_append_failure(error: &EngineError) -> bool {
    matches!(
        error,
        EngineError::Journal(
            JournalError::Event(_) | JournalError::Poisoned | JournalError::Stopped
        ) | EngineError::ActorStopped
    )
}

#[cfg(test)]
mod builtin_revision_tests {
    use std::{collections::BTreeMap, sync::Arc};

    use cookie_agent_config::AgentRegistry;
    use cookie_agent_protocol::{
        AgentMode, ApprovalBoundary, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation,
        ApprovalId, ApprovalResourceSource, ApprovalTrigger, AssistantToolCallRef, DecisionTrace,
        EventPayload, EventSchemaVersion, ModelCallId, PermissionAction, PermissionEffect,
        PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
        PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity, RunId,
        SessionId, Sha256Digest, StoredEvent, ToolCallId, ToolCallPresentation, ToolCallStart,
        ToolCallTermination, ToolTerminationOutcome,
    };

    use super::{
        BOUNDED_SUMMARY_BUILTIN_REVISION, FrozenRunPolicy, InternalAgentKind, InternalAgentRuntime,
        UNAVAILABLE_BUILTIN_REVISION, active_fallback_index, doom_loop_repetitions,
        safe_tool_presentation,
    };
    use crate::{
        ToolCall,
        policy::{ResultLimits, wire_binding},
        test_support::{
            agent_snapshot, model_binding, model_set, model_snapshot, variant_model_binding,
        },
    };

    #[test]
    fn builtin_revisions_describe_semantic_contracts_not_protocol_versions() {
        assert_eq!(
            BOUNDED_SUMMARY_BUILTIN_REVISION,
            "context-compaction.bounded-summary.prompt-runtime.1"
        );
        assert_eq!(
            UNAVAILABLE_BUILTIN_REVISION,
            "internal-agent.unavailable.runtime.1"
        );
        for revision in [
            BOUNDED_SUMMARY_BUILTIN_REVISION,
            UNAVAILABLE_BUILTIN_REVISION,
        ] {
            assert!(!revision.starts_with('v'));
            assert!(!revision.contains("protocol"));
            assert!(!revision.contains("event-schema"));
        }
    }

    #[test]
    fn tool_presentations_accept_camel_and_snake_file_paths_and_delegate_excerpt() {
        for arguments in [
            serde_json::json!({"filePath":"nested/value.txt"}),
            serde_json::json!({"file_path":"nested/value.txt"}),
        ] {
            let presentation = safe_tool_presentation(&ToolCall {
                id: ToolCallId::new_v7(),
                name: "read".into(),
                arguments,
            });
            assert_eq!(
                presentation.primary_argument.expect("path").as_str(),
                "nested/value.txt"
            );
        }
        let presentation = safe_tool_presentation(&ToolCall {
            id: ToolCallId::new_v7(),
            name: "delegate".into(),
            arguments: serde_json::json!({
                "agent":"worker\nagent",
                "task":format!("inspect token safely {}", "x".repeat(600))
            }),
        });
        let primary = presentation
            .primary_argument
            .expect("delegate presentation");
        assert!(primary.as_str().starts_with("worker agent: <redacted>"));
        assert!(primary.as_str().len() <= cookie_agent_protocol::SafeDisplayText::MAX_BYTES);
    }

    #[test]
    fn every_internal_agent_kind_inherits_only_the_active_variant_suffix_and_prompt() {
        let set = model_set();
        let registry =
            Arc::new(AgentRegistry::resolve(BTreeMap::new(), &set).expect("empty registry"));
        let base = model_binding();
        let variant = variant_model_binding();
        let mut agent = agent_snapshot("owner", AgentMode::Primary);
        agent.composed_prompt = "Frozen owner prompt.\n".into();
        agent.prompt_fingerprint = Sha256Digest::of_bytes(agent.composed_prompt.as_bytes());
        let owner = FrozenRunPolicy {
            agent: agent.clone(),
            selected_suffix: vec![base, variant.clone()],
            selected_suffix_wire: vec![wire_binding(&variant).expect("wire binding")],
            model_snapshot: model_snapshot(),
            registry,
            result_limits: ResultLimits {
                tool_output_max_lines: 1,
                tool_output_max_bytes: 1,
            },
        };
        let runtime = InternalAgentRuntime::freeze();
        for kind in [
            InternalAgentKind::Approval,
            InternalAgentKind::ContextCompaction,
            InternalAgentKind::SessionTitle,
        ] {
            let inherited = runtime.policy(kind, &owner, &owner.selected_suffix[1..]);
            assert_eq!(inherited.models.as_slice(), std::slice::from_ref(&variant));
            assert_eq!(inherited.agent.composed_prompt, agent.composed_prompt);
            assert_eq!(
                inherited.models[0].resolved.selection.variant,
                variant.resolved.selection.variant
            );
        }
    }

    fn operation(label: &str) -> PreparedOperationIdentity {
        let digest = PreparedResourceDigest::from_canonical_binding_bytes(label.as_bytes());
        PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(label.as_bytes()),
            vec![ApprovalCapability {
                action: PermissionAction::Read,
                operation: PreparedCapabilityOperation::new("read:read").expect("operation"),
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Read,
                canonical: PreparedResourceIdentity::new(format!(
                    "file:{}",
                    Sha256Digest::of_bytes(label.as_bytes()).as_str()
                ))
                .expect("identity"),
                binding_digest: digest.clone(),
                binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"context"),
        )
        .expect("operation")
    }

    fn approval(label: &str) -> cookie_agent_protocol::ApprovalRequest {
        let operation = operation(label);
        cookie_agent_protocol::ApprovalRequest::new(
            ApprovalId::new_v7(),
            1,
            ApprovalTrigger::PermissionPolicy,
            operation.clone(),
            vec![ApprovalEvaluation {
                resource_digest: operation.resources()[0].binding_digest.clone(),
                effect: PermissionEffect::Ask,
                trace: DecisionTrace {
                    action: PermissionAction::Read,
                    normalized_resource: label.into(),
                    candidates: Vec::new(),
                    effect: PermissionEffect::Ask,
                    precedence_reason: "test".into(),
                },
            }],
            ApprovalConstraints {
                allow_once: true,
                allow_tree_grant: false,
                cancellable: true,
                expires_at: None,
            },
        )
        .expect("approval")
    }

    fn stored(run: RunId, seq: u64, payload: EventPayload) -> StoredEvent {
        StoredEvent {
            event_schema_version: EventSchemaVersion::current(),
            session_id: SessionId::new_v7(),
            run_id: Some(run),
            seq,
            timestamp: jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        }
    }

    #[test]
    fn doom_loop_fires_on_four_and_resets_on_input_or_different_success() {
        let run = RunId::new_v7();
        let request = approval("same");
        let fingerprint = request.operation_fingerprint().clone();
        let mut events = (1..=4)
            .map(|seq| {
                stored(
                    run,
                    seq,
                    EventPayload::ApprovalRequested {
                        request: request.clone(),
                    },
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(doom_loop_repetitions(&events[..3], run, &fingerprint), 3);
        assert_eq!(doom_loop_repetitions(&events, run, &fingerprint), 4);

        events.push(stored(
            run,
            5,
            EventPayload::UserInputSubmitted {
                input: "continue".into(),
            },
        ));
        events.push(stored(
            run,
            6,
            EventPayload::ApprovalRequested {
                request: request.clone(),
            },
        ));
        assert_eq!(doom_loop_repetitions(&events, run, &fingerprint), 1);

        let other = operation("different");
        let call = ToolCallId::new_v7();
        let owner = AssistantToolCallRef {
            model_turn_seq: 1,
            content_index: 0,
            model_call_id: ModelCallId::new("different-call").expect("model call"),
            provider_item_id: None,
        };
        events.push(stored(
            run,
            7,
            EventPayload::ToolCallStarted {
                start: ToolCallStart {
                    tool_call_id: call,
                    owner: owner.clone(),
                    presentation: ToolCallPresentation {
                        title: cookie_agent_protocol::SafeDisplayText::new("Read other")
                            .expect("title"),
                        primary_argument: None,
                    },
                    operation_fingerprint: cookie_agent_protocol::OperationFingerprint::from_prepared_operation(&other),
                },
            },
        ));
        events.push(stored(
            run,
            8,
            EventPayload::ToolCallTerminated {
                termination: ToolCallTermination {
                    tool_call_id: call,
                    owner,
                    outcome: ToolTerminationOutcome::Completed,
                    result: Some(cookie_agent_protocol::PersistedToolResult {
                        title: cookie_agent_protocol::SafeDisplayText::new("Read other")
                            .expect("title"),
                        output: String::new(),
                        metadata: serde_json::Value::Null,
                        truncation: None,
                        attachments: Vec::new(),
                    }),
                    error: None,
                },
            },
        ));
        events.push(stored(run, 9, EventPayload::ApprovalRequested { request }));
        assert_eq!(doom_loop_repetitions(&events, run, &fingerprint), 1);
    }

    #[test]
    fn direct_title_policy_uses_latest_attempt_suffix() {
        let run = RunId::new_v7();
        let binding = variant_model_binding();
        let events = vec![stored(
            run,
            1,
            EventPayload::ModelAttemptStarted {
                attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                attempt_ordinal: 1,
                fallback_index: 1,
                retry_ordinal: 0,
                resolved_model: crate::model_history::wire_model(&binding),
                prompt_fingerprint: Sha256Digest::of_bytes(b"prompt"),
            },
        )];
        assert_eq!(active_fallback_index(&events, run), 1);
    }
}
