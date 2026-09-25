use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};

use arc_swap::ArcSwap;
use cookie_agent_config::LoadedConfiguration;
use cookie_agent_identity::ModelKey;
use cookie_agent_models::{ModelManager, manifests::ManifestError};
use cookie_agent_protocol::{
    AgentId, ApprovalId, ApprovalInternalDecisionKind, ApprovalRequest, ApprovalRespondErrorCode,
    ApprovalRespondParams, ApprovalRespondResult, ApprovalStatus, EventPayload as Event,
    EventSubscriptionMessage, EventsSubscribeResult, InvocationId, OperationFingerprint,
    PermissionMode, PersistedModelTurn, PersistedToolResult as ToolResult, ProviderConnectParams,
    ProviderConnectResult, ProviderDisconnectParams, ProviderDisconnectResult, RunCancelResult,
    RunId, RunRecallSteerResult, RunStartParams, RunStartResult, RunSteerResult,
    RunToolStdinParams, RunToolStdinResult, RuntimeChangeReason, RuntimeChangedNotification,
    RuntimeSnapshotResult, SafeCode, SessionId, SessionMeta, SessionRenameParams,
    SessionRenameResult, SessionRevertResult, ToolCallId, ToolCallPresentation,
};
use oven_sdk::{ModelError, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    delegation_events::{DelegationEventError, DelegationEventStore},
    events::{self, EventLogError},
    grant_journal::{GrantJournalError, GrantJournals},
    model_history,
    model_snapshots::prepare_runtime_manifest,
    permissions::PermissionPipeline,
    policy::FrozenRunPolicy,
    runtime_snapshot::{
        AgentRegistry, PublishedRuntime, RuntimePublication, build_runtime_snapshot,
    },
    session::{SessionError, SessionStore},
    tool_api::ToolPreparationContext,
};

mod admission;
mod agent_md;
#[cfg(test)]
pub(crate) use agent_md::AGENT_MD_MAX_BYTES;
mod approval_api;
mod approval_flow;
mod approval_projection;
mod artifact_reads;
pub(crate) mod artifacts;
mod blocking_io;
pub(crate) mod compaction;
mod delegation;
mod get_history;
pub(crate) mod handles;
mod helpers;
mod internal_agents;
mod mailbox;
pub(crate) mod messaging_api;
mod model_loop;
mod output_capture;
pub(crate) mod plugin_diagnostics;
mod producer_claims;
pub(crate) mod producers;
mod prompt_blocks;
mod recovery;
mod residency;
mod runs;
mod sessions;
mod skills;
/// Test-only hooks; the module body is `#![cfg(test)]`.
pub(crate) mod test_hooks;
#[cfg(test)]
mod tests;
mod titles;
pub(crate) mod tool_execution;
mod tool_prompts;
mod working_directory;

pub use artifact_reads::ArtifactReadPage;
pub(crate) use artifact_reads::read_artifact_async;
pub(crate) use artifacts::ArtifactRouter;
pub(crate) use delegation::render_subagent_notification;

/// Session id for artifact/capture tests that do not model tree placement.
#[cfg(test)]
pub(crate) fn test_session_id() -> cookie_agent_protocol::SessionId {
    cookie_agent_protocol::SessionId::new_v7()
}
use approval_flow::{
    ApprovalEvaluationTransition, ApprovalRuntimeState, ApprovalTerminal, ApprovalToolInput,
    ModelApprovalInput, PreparedApprovalInvalidation,
};
pub(crate) use approval_flow::{ApprovalOutcome, PendingApproval};
#[cfg(test)]
pub(crate) use artifacts::ArtifactStore;
#[cfg(test)]
pub(crate) use blocking_io::gate as block_artifact_io_for_test;
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use compaction::ContextTokenEstimator;
use compaction::{
    CompactionDeferredKind, CompactionState, PredictiveCompactionInput,
    should_run_predictive_compaction,
};
use delegation::DelegationRuntimeState;
pub use get_history::EngineHistoryView;
use helpers::safe_code;
pub(crate) use internal_agents::FrozenInternalAgentPolicy;
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use internal_agents::InternalAgentLimits;
use internal_agents::{InternalAgentExecution, InternalAgentHistoryInput};
use mailbox::SessionRuntimeState;
pub use messaging_api::{AgentMessageHandle, AgentMessageInvocation, AgentRecipientState};
pub(crate) use output_capture::OutputCapture;
use output_capture::OutputState;
pub(crate) use output_capture::finish_page;
use plugin_diagnostics::{PluginDiagnosticsState, run_plugin_diagnostic_aggregator};
pub use skills::SkillInvocation;
use skills::SkillRuntimeState;
#[cfg(test)]
pub(crate) use test_hooks::*;

// A terminal result was finalized successfully before cancellation won its commit.
pub(crate) const CANCELLED_AFTER_COMPLETION: &str = "cancelled_after_completion";

use crate::tool_api::{
    PreparedExecutorCell, PreparedSerializationKey, PreparedTool, StdinWrite, ToolCall,
    ToolConcurrency, ToolError, ToolProvider, ToolSpec, TurnAgentContext,
};

#[derive(Clone)]
pub struct EngineOptions {
    pub data_dir: PathBuf,
    pub cwd: PathBuf,
    pub config: LoadedConfiguration,
    pub model_manager: Arc<ModelManager>,
    pub tools: Vec<Arc<dyn ToolProvider>>,
}

#[derive(Debug, Error)]
pub enum EngineError {
    // Not `transparent`: the manual `From<SessionError>` below unwraps lazy tree
    // load rejections back to their engine-side type, and a transparent variant
    // may not carry an explicit `#[source]`.
    #[error("{0}")]
    Session(#[source] SessionError),
    #[error(transparent)]
    DelegationEvents(#[from] DelegationEventError),
    #[error(transparent)]
    Event(#[from] EventLogError),
    #[error(transparent)]
    GrantJournal(#[from] GrantJournalError),
    #[error("tool output storage error: {0}")]
    ToolOutput(#[from] std::io::Error),
    #[error("AGENTS.md context read failed at {path}: {source}")]
    AgentMdIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration error: {0}")]
    Config(#[source] Box<cookie_agent_config::ConfigError>),
    #[error("agent `{0}` is not eligible in this session origin")]
    IneligibleAgent(AgentId),
    #[error(
        "agent `{agent}` references unknown model `{model}`; refresh models.dev or choose an available model"
    )]
    UnknownAgentModel { agent: AgentId, model: ModelKey },
    #[error(
        "agent `{agent}` references catalog model `{model}`, but it is unavailable in the compiled runtime"
    )]
    UnavailableAgentModel { agent: AgentId, model: ModelKey },
    #[error("agent `{0}` is disabled")]
    DisabledAgent(AgentId),
    #[error("run {0} not found")]
    MissingRun(RunId),
    #[error("session {0} is already running")]
    SessionRunning(SessionId),
    #[error("client run id conflicts with durable run parameters")]
    RunIdempotencyConflict,
    #[error("input was handled by plugin: {0}")]
    InputHandled(String),
    #[error("model selection was blocked by plugin: {0}")]
    ModelSelectionBlocked(String),
    #[error("session operation was blocked by plugin: {0}")]
    SessionOperationBlocked(String),
    #[error("compaction cancelled by plugin: {0}")]
    CompactionCancelled(String),
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
    /// A tool, skill, or delegation operation was rejected; the message is
    /// self-contained and safe to surface to the model and the client.
    #[error("{0}")]
    ToolFailed(String),
    #[error("tool prompt composition failed: {0}")]
    ToolPrompt(String),
    #[error("session actor for {0} is unavailable")]
    MissingActor(SessionId),
    #[error("session {0} is owned by another cookie process")]
    SessionOwnedByAnotherProcess(SessionId),
    #[error("session actor stopped before replying")]
    ActorStopped,
    #[error("no_runnable_model")]
    NoRunnableModel,
    #[error("unknown agent preset `{0}`")]
    UnknownAgentPreset(String),
    #[error("provider_store_reload_failed")]
    ProviderStoreReloadFailed,
    #[error("runtime_compile_failed")]
    RuntimeCompileFailed,
    #[error("invalid runtime agent `{0}`")]
    InvalidRuntimeAgent(AgentId),
    #[error("MCP configuration error: {0}")]
    Mcp(String),
    #[error("cache strategy configuration error: {0}")]
    CacheStrategy(String),
    #[error("session permission error: {0}")]
    Permission(String),
    #[error("goal operation rejected: {0}")]
    Goal(String),
    #[error("producer operation rejected: {0}")]
    Producer(String),
    #[error("messaging: {0}")]
    Messaging(String),
    #[error(transparent)]
    ModelManager(#[from] cookie_agent_models::ModelManagerError),
    #[error(transparent)]
    Manifest(ManifestError),
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

impl From<SessionError> for EngineError {
    fn from(error: SessionError) -> Self {
        match error {
            // A lazy tree load the engine rejected carries its own failure; keep
            // the original type so callers see what actually went wrong.
            SessionError::TreeRejected(inner) => *inner,
            other => Self::Session(other),
        }
    }
}

impl EngineError {
    /// Explicit user-facing rendering; Debug is not a diagnostic transport.
    pub fn user_message(&self) -> String {
        match self {
            Self::Model(error) => {
                cookie_agent_protocol::diagnostics::model(&crate::model_policy::summary(error))
            }
            _ => cookie_agent_protocol::diagnostics::error_chain(self),
        }
    }
}

#[derive(Debug)]
struct ActiveRun {
    session: SessionId,
    policy: Arc<FrozenRunPolicy>,
    cancellation: CancellationToken,
    cancelled_committed: Mutex<bool>,
    stdin: Mutex<HashMap<ToolCallId, mpsc::Sender<StdinWrite>>>,
    fallback_index: AtomicU64,
    auto_compaction_failures: AtomicU8,
    auto_compaction_diagnostic_emitted: AtomicBool,
}

struct AttemptTurn {
    turn: PersistedModelTurn,
    model_turn_seq: u64,
    turn_context: Arc<TurnAgentContext>,
    /// Persisted model call IDs whose tool-call name was normalized away from
    /// invalid provider output. Dispatch must fail these instead of executing a
    /// possibly-aliased tool.
    normalized_tool_calls: HashSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingInput {
    admission_seq: u64,
    origin: cookie_agent_protocol::EventOrigin,
    input: String,
}

struct DelegatedResumeAdmission {
    accepted: bool,
    admission_seq: Option<u64>,
}

struct PendingPromotionState {
    promoted: bool,
    pending: Vec<PendingInput>,
    continue_run: bool,
}

pub(super) enum UserInputInterception {
    Accepted {
        input: String,
        original_input: Option<String>,
    },
    Handled {
        reason: String,
    },
}

struct PreparedToolCall {
    call: ToolCall,
    permission_name: Option<String>,
    presentation: ToolCallPresentation,
    prepared: Result<PreparedTool, ToolFailure>,
    interception: Option<ToolInterceptionContext>,
    intercepted_arguments: Arc<Mutex<Value>>,
}

impl PreparedToolCall {
    fn concurrency(&self) -> ToolConcurrency {
        self.interception
            .as_ref()
            .map_or(ToolConcurrency::Exclusive, |context| {
                context.spec.concurrency
            })
    }

    /// Calls sharing a serialization key must execute in model call order:
    /// chained preparations build on each other's output.
    fn serialization_key(&self) -> Option<crate::PreparedSerializationKey> {
        self.prepared
            .as_ref()
            .ok()
            .and_then(|prepared| prepared.serialization_key.clone())
    }
}

struct ToolInterceptionContext {
    provider: Arc<dyn ToolProvider>,
    spec: ToolSpec,
    preparation: ToolPreparationContext,
    permission_name: String,
    permission_resource: Option<String>,
}

struct PublishedTool {
    provider: Arc<dyn ToolProvider>,
    spec: ToolSpec,
}

struct PublishedToolSet {
    definitions: Vec<ToolDefinition>,
    tools: HashMap<String, PublishedTool>,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolFailure {
    pub(crate) code: ToolCallFailureCode,
    pub(crate) message: String,
    pub(crate) partial_output: Option<Box<ToolResult>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ToolCallFailureCode {
    InvalidUrl,
    PermissionDenied,
    RedirectError,
    Timeout,
    TransportError,
    UnsupportedContentType,
    ExecutionFailed,
    OperationChanged,
    PreparedCapabilityLost,
    UnsupportedPlatform,
}

impl ToolCallFailureCode {
    fn safe_code(self) -> SafeCode {
        safe_code(match self {
            Self::InvalidUrl => "invalid_url",
            Self::PermissionDenied => "permission_denied",
            Self::RedirectError => "redirect_error",
            Self::Timeout => "timeout",
            Self::TransportError => "transport_error",
            Self::UnsupportedContentType => "unsupported_content_type",
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
            partial_output: None,
        }
    }
}

enum PendingTool {
    Prepared {
        prepared: Box<PreparedToolCall>,
        permission: crate::permissions::PermissionDecision,
    },
    ImmediateFailure(ToolFailure),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredRuntimeRevisionMapping {
    /// Protocol the mapping was written under. A record of history, not the
    /// current wire format, so a protocol bump never makes an existing index
    /// unreadable.
    protocol_version: u32,
    runtime_revision: cookie_agent_protocol::RuntimeRevision,
    model_runtime_revision: cookie_agent_protocol::RuntimeRevision,
}

#[derive(Debug)]
struct RuntimeRevisionIndex {
    path: PathBuf,
    mappings:
        HashMap<cookie_agent_protocol::RuntimeRevision, cookie_agent_protocol::RuntimeRevision>,
}

impl RuntimeRevisionIndex {
    fn open(path: PathBuf) -> Result<Self, EngineError> {
        let mut mappings = HashMap::new();
        for record in events::load_jsonl_shared::<StoredRuntimeRevisionMapping>(&path)? {
            match mappings.insert(
                record.runtime_revision.clone(),
                record.model_runtime_revision.clone(),
            ) {
                Some(existing) if existing != record.model_runtime_revision => {
                    return Err(EngineError::RuntimeCompileFailed);
                }
                _ => {}
            }
        }
        Ok(Self { path, mappings })
    }

    fn record(
        &mut self,
        runtime_revision: cookie_agent_protocol::RuntimeRevision,
        model_runtime_revision: cookie_agent_protocol::RuntimeRevision,
    ) -> Result<(), EngineError> {
        if let Some(existing) = self.mappings.get(&runtime_revision) {
            return if existing == &model_runtime_revision {
                Ok(())
            } else {
                Err(EngineError::RuntimeCompileFailed)
            };
        }
        events::append_jsonl(
            &self.path,
            &StoredRuntimeRevisionMapping {
                protocol_version: cookie_agent_protocol::PROTOCOL_VERSION,
                runtime_revision: runtime_revision.clone(),
                model_runtime_revision: model_runtime_revision.clone(),
            },
        )?;
        self.mappings
            .insert(runtime_revision, model_runtime_revision);
        Ok(())
    }

    fn resolve(
        &self,
        runtime_revision: &cookie_agent_protocol::RuntimeRevision,
    ) -> Option<cookie_agent_protocol::RuntimeRevision> {
        self.mappings.get(runtime_revision).cloned()
    }
}

/// How long shutdown waits, in total, for the cancelled run tasks to write
/// their terminal events.
///
/// A cancelled run only has to unwind its current tool call and append one
/// event, so this is generous; a task still running when it expires is aborted
/// so shutdown never depends on a wedged provider or tool.
const RUN_TASK_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

const SESSION_MAILBOX_CAPACITY: usize = 256;
const MAX_PENDING_PREPARED_TOOLS: usize = 64;
/// Semantic revision of the no-model builtin runtime contract.
/// This is intentionally independent of the protocol and event schema version.
pub(crate) const UNAVAILABLE_BUILTIN_REVISION: &str = "internal-agent.unavailable.runtime.1";

pub(super) fn event_origin(value: &'static str) -> cookie_agent_protocol::EventOrigin {
    cookie_agent_protocol::EventOrigin::new(value).expect("static event origin is valid")
}

enum SessionCommand {
    Producer(producers::ProducerCommand),
    Append {
        run: Option<RunId>,
        origin: cookie_agent_protocol::EventOrigin,
        event: Box<Event>,
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
        origin: cookie_agent_protocol::EventOrigin,
        admission: Option<(InvocationId, u64)>,
        reply: oneshot::Sender<Result<RunStartResult, EngineError>>,
    },
    Steer {
        run: RunId,
        origin: cookie_agent_protocol::EventOrigin,
        input: String,
        original_input: Option<String>,
        reply: oneshot::Sender<Result<RunSteerResult, EngineError>>,
    },
    AdmitDelegatedResume {
        run: RunId,
        input: String,
        reply: oneshot::Sender<Result<DelegatedResumeAdmission, EngineError>>,
    },
    RecallDelegatedResume {
        run: RunId,
        admission_seq: u64,
        reply: oneshot::Sender<Result<bool, EngineError>>,
    },
    RecallSteer {
        run: RunId,
        reply: oneshot::Sender<Result<RunRecallSteerResult, EngineError>>,
    },
    CommitPendingPromotion {
        run: RunId,
        through_admission_seq: u64,
        final_text: Option<String>,
        complete_if_empty: bool,
        already_promoted: bool,
        reply: oneshot::Sender<Result<PendingPromotionState, EngineError>>,
    },
    Compact {
        focus: Option<String>,
        origin: cookie_agent_protocol::EventOrigin,
        reply: oneshot::Sender<Result<cookie_agent_protocol::SessionCompactResult, EngineError>>,
    },
    Revert {
        through_seq: u64,
        origin: cookie_agent_protocol::EventOrigin,
        instructions_override: Option<String>,
        reply: oneshot::Sender<Result<SessionRevertResult, EngineError>>,
    },
    CompactionFinished {
        reply: oneshot::Sender<Result<(), EngineError>>,
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
        origin: cookie_agent_protocol::EventOrigin,
        reply: oneshot::Sender<Result<SessionRenameResult, EngineError>>,
    },
    ApprovalRespond {
        params: ApprovalRespondParams,
        origin: cookie_agent_protocol::EventOrigin,
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
        permission_mode: PermissionMode,
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
        cancelled: bool,
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
    PromotePendingOrComplete {
        run: RunId,
        final_text: Option<String>,
        complete_if_empty: bool,
        reply: oneshot::Sender<Result<bool, EngineError>>,
    },
    PromotePendingInputs {
        run: RunId,
        reply: oneshot::Sender<Result<producer_claims::ClaimedPrompt, EngineError>>,
    },
    EvictionBarrier {
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
}

impl SessionCommand {
    fn compaction_deferred_kind(&self) -> Option<CompactionDeferredKind> {
        match self {
            Self::Start { .. } => Some(CompactionDeferredKind::Start),
            Self::PromotePendingInputs { .. } => Some(CompactionDeferredKind::PromotePendingInputs),
            Self::PromotePendingOrComplete { .. } => {
                Some(CompactionDeferredKind::PromotePendingOrComplete)
            }
            Self::Resume { .. } => Some(CompactionDeferredKind::Resume),
            _ => None,
        }
    }

    fn reject_duplicate_during_compaction(self, session: SessionId) {
        match self {
            Self::Start { reply, .. } => {
                let _ = reply.send(Err(EngineError::SessionRunning(session)));
            }
            Self::PromotePendingInputs { reply, .. } => {
                let _ = reply.send(Err(EngineError::SessionRunning(session)));
            }
            Self::PromotePendingOrComplete { reply, .. } => {
                let _ = reply.send(Ok(false));
            }
            Self::Resume { reply } => {
                let _ = reply.send(Err(EngineError::SessionRunning(session)));
            }
            _ => unreachable!("only barrier-sensitive commands are superseded"),
        }
    }
}

const MAX_COMPACTION_DEFERRED_COMMANDS: usize = 4;

pub(crate) struct Inner {
    config: LoadedConfiguration,
    pub(crate) config_store: Mutex<crate::config_store::ConfigStore>,
    pub(crate) artifacts: Arc<ArtifactRouter>,
    mutation_locks: Mutex<HashMap<PreparedSerializationKey, Arc<tokio::sync::Mutex<()>>>>,
    pub(crate) store: Arc<SessionStore>,
    pub(crate) delegation_events: Arc<DelegationEventStore>,
    pub(crate) grant_journals: GrantJournals,
    pub(crate) model_manager: Arc<ModelManager>,
    published_runtime: ArcSwap<PublishedRuntime>,
    runtime_mutation: Mutex<()>,
    runtime_notifications: broadcast::Sender<RuntimeChangedNotification>,
    engine_events: broadcast::Sender<crate::EngineEvent>,
    plugin_diagnostics: PluginDiagnosticsState,
    runtime_revision_index: Mutex<RuntimeRevisionIndex>,
    tools: Mutex<Vec<Arc<dyn ToolProvider>>>,
    provider_ids: Mutex<HashSet<&'static str>>,
    pub(crate) mcp: Arc<crate::McpRegistry>,
    pub(crate) plugins: Arc<crate::PluginRegistry>,
    pub(crate) mcp_mutation: tokio::sync::Mutex<()>,
    permissions: PermissionPipeline,
    pub(crate) skills: Arc<cookie_agent_config::SkillRegistry>,
    pub(crate) sessions: SessionRuntimeState,
    pub(crate) delegation: DelegationRuntimeState,
    pub(crate) approvals: ApprovalRuntimeState,
    pub(crate) skills_runtime: SkillRuntimeState,
    pub(crate) output: OutputState,
    pub(crate) compaction: CompactionState,
    runtime: Option<tokio::runtime::Handle>,
    janitor_task: Mutex<Option<JoinHandle<()>>>,
    #[cfg(test)]
    pub(crate) test_hooks: test_hooks::TestHooks,
}

/// Cloneable in-process engine handle. It contains no transport concerns and
/// is safe for tool providers to call while their parent call is executing.
#[derive(Clone)]
pub struct Engine {
    pub(crate) inner: Arc<Inner>,
}

impl Engine {
    pub fn open(options: EngineOptions) -> Result<Self, EngineError> {
        let mut provider_ids = HashSet::new();
        for provider in &options.tools {
            reserve_provider_id(&mut provider_ids, provider.as_ref())?;
        }
        let skills = Arc::new(options.config.skills.clone());
        let config_store = crate::config_store::ConfigStore::new(&options.config);
        let current_models = options.model_manager.current();
        let (agents, agent_presets) = resolve_agent_registries(&options.config, &current_models)?;
        let current_manifest = prepare_runtime_manifest(&current_models)?;
        let snapshot = build_runtime_snapshot(&current_models, &agents, &agent_presets)?;
        let published_runtime = Arc::new(PublishedRuntime {
            result: RuntimeSnapshotResult { snapshot },
            models: Arc::clone(&current_models),
            agents,
            agent_presets,
            current_manifest,
        });
        let store = SessionStore::open(&options.data_dir, &options.cwd)?;
        let artifacts = ArtifactRouter::for_store(&store)?;
        // Every write belongs to the directory of the writing session's root.
        let placement = Arc::clone(&store);
        artifacts.install_tree_resolver(Arc::new(move |session| {
            Some(placement.root_of(session).unwrap_or(session))
        }));
        // A sweep reuses what a tree load harvested only while the store
        // still agrees the harvest is current, so it has to ask the store:
        // resident tip and durable length, the same pair a fold is verified
        // against (§3.3, §5.2).
        let fingerprinting = Arc::clone(&store);
        artifacts.install_log_fingerprint_probe(Arc::new(move |session| {
            fingerprinting.log_fingerprint(session)
        }));
        let mcp = Arc::new(
            crate::McpRegistry::new(
                options.config.mcp_servers.clone(),
                options.data_dir.join("mcp-oauth.json"),
            )
            .map_err(|error| EngineError::ToolFailed(error.to_string()))?,
        );
        reserve_provider_id(&mut provider_ids, mcp.as_ref())?;
        for provider in &options.tools {
            mcp.reserve_provider(provider.as_ref())
                .map_err(|error| EngineError::ToolFailed(error.to_string()))?;
        }
        let plugins = Arc::new(crate::PluginRegistry::new(
            options.config.plugins.clone(),
            Arc::clone(&mcp),
        ));
        reserve_provider_id(&mut provider_ids, plugins.as_ref())?;
        let mut tools = options.tools;
        tools.push(mcp.clone());
        tools.push(plugins.clone());
        let delegation_events = DelegationEventStore::new(Arc::clone(&store));
        let grant_journals = GrantJournals::new(store.workdir_dir_path());
        let (runtime_notifications, _) = broadcast::channel(64);
        let (engine_events, _) = broadcast::channel(256);
        let plugin_diagnostics =
            Arc::new(plugin_diagnostics::PluginDiagnosticAccumulator::default());
        let mut runtime_revision_index = RuntimeRevisionIndex::open(
            store.workdir_dir_path().join("runtime-revisions-v8.jsonl"),
        )?;
        runtime_revision_index.record(
            published_runtime.result.snapshot.runtime_revision.clone(),
            current_models.runtime_revision().clone(),
        )?;
        let engine = Self {
            inner: Arc::new(Inner {
                config: options.config,
                config_store: Mutex::new(config_store),
                artifacts,
                mutation_locks: Mutex::new(HashMap::new()),
                store,
                delegation_events,
                grant_journals,
                model_manager: options.model_manager,
                published_runtime: ArcSwap::from(published_runtime),
                runtime_mutation: Mutex::new(()),
                runtime_notifications,
                engine_events,
                plugin_diagnostics: PluginDiagnosticsState {
                    accumulator: Arc::clone(&plugin_diagnostics),
                    task: Mutex::new(None),
                },
                runtime_revision_index: Mutex::new(runtime_revision_index),
                tools: Mutex::new(tools),
                provider_ids: Mutex::new(provider_ids),
                mcp,
                plugins,
                mcp_mutation: tokio::sync::Mutex::new(()),
                permissions: PermissionPipeline::default(),
                skills,
                sessions: SessionRuntimeState::default(),
                delegation: DelegationRuntimeState::default(),
                approvals: ApprovalRuntimeState::default(),
                skills_runtime: SkillRuntimeState::default(),
                output: OutputState::default(),
                compaction: CompactionState::default(),
                runtime: tokio::runtime::Handle::try_current().ok(),
                janitor_task: Mutex::new(None),
                #[cfg(test)]
                test_hooks: test_hooks::TestHooks::default(),
            }),
        };
        let weak = Arc::downgrade(&engine.inner);
        engine
            .inner
            .plugins
            .set_emit_handler(Arc::new(move |request| {
                let weak = weak.clone();
                Box::pin(async move {
                    let Some(inner) = weak.upgrade() else {
                        return crate::plugin::PluginEmitOutcome {
                            bus: cookie_agent_protocol::ExtensionEmitStatus::Dropped,
                            durable: cookie_agent_protocol::ExtensionEmitStatus::Rejected,
                            reason: Some("engine is shutting down".into()),
                        };
                    };
                    Engine { inner }.publish_plugin_emit(request).await
                })
            }));
        if let Some(runtime) = &engine.inner.runtime {
            let weak = Arc::downgrade(&engine.inner);
            let task = runtime.spawn(async move {
                run_plugin_diagnostic_aggregator(weak, plugin_diagnostics).await;
            });
            *engine
                .inner
                .plugin_diagnostics
                .task
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(task);
        }
        // No session state is read here: delegation records, grants, and
        // producer state arrive with each tree's load (tree-local B1, C1).
        engine.install_tree_load_observer()?;
        engine.install_producer_runtime();
        if let Some(runtime) = &engine.inner.runtime {
            engine.inner.mcp.start_eager(runtime);
            engine.inner.plugins.start_eager(runtime);
        }
        engine.start_subagent_janitor();
        Ok(engine)
    }

    #[must_use]
    pub fn current_runtime(&self) -> Arc<PublishedRuntime> {
        self.inner.published_runtime.load_full()
    }

    pub fn runtime_snapshot(&self) -> Result<RuntimeSnapshotResult, EngineError> {
        self.reconcile_provider_store()?;
        Ok(self.current_runtime().result.clone())
    }

    #[must_use]
    pub fn subscribe_runtime_changes(&self) -> broadcast::Receiver<RuntimeChangedNotification> {
        self.inner.runtime_notifications.subscribe()
    }

    #[must_use]
    pub fn subscribe_engine_events(&self) -> broadcast::Receiver<crate::EngineEvent> {
        self.inner.engine_events.subscribe()
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn publish_engine_event_for_test(&self, event: crate::EngineEvent) {
        let _ = self.inner.engine_events.send(event);
    }

    pub fn connect_provider(
        &self,
        params: ProviderConnectParams,
    ) -> Result<ProviderConnectResult, EngineError> {
        use cookie_agent_models::provider_store::{
            ClientConnectId, ProviderAuthValues, ProviderStoreMutation,
        };

        let _mutation = self
            .inner
            .runtime_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let provider_id = params.provider_id.clone();
        let auth_values = params
            .auth_values
            .field_names()
            .map(|name| {
                let field = cookie_agent_protocol::AuthFieldName::new(name.to_owned())
                    .map_err(|_| EngineError::RuntimeCompileFailed)?;
                let value = params
                    .auth_values
                    .get(&field)
                    .ok_or(EngineError::RuntimeCompileFailed)?
                    .to_owned();
                Ok((field, value))
            })
            .collect::<Result<_, EngineError>>()?;
        let request = cookie_agent_models::ProviderConnectRequest {
            provider_id,
            expected_catalog_revision: params.expected_catalog_revision,
            setup_values: params
                .setup_values
                .into_iter()
                .map(|(id, value)| {
                    let value = serde_json::from_value(
                        serde_json::to_value(value)
                            .map_err(|_| EngineError::RuntimeCompileFailed)?,
                    )
                    .map_err(|_| EngineError::RuntimeCompileFailed)?;
                    Ok((id, value))
                })
                .collect::<Result<_, EngineError>>()?,
            auth_method: params.auth_method,
            auth_values: ProviderAuthValues::new(auth_values)
                .map_err(cookie_agent_models::ModelManagerError::from)?,
            client_connect_id: ClientConnectId::new(params.client_connect_id.as_str())
                .map_err(cookie_agent_models::ModelManagerError::from)?,
        };
        let previous = self.current_runtime();
        let result = self.inner.model_manager.connect(request, |candidate, _| {
            self.prepare_publication(
                candidate,
                &previous,
                vec![RuntimeChangeReason::ProviderConnected],
            )
            .map_err(|_| cookie_agent_models::ModelManagerError::RuntimeCompileFailed)
        })?;
        let runtime = result.publication.map_or_else(
            || self.current_runtime(),
            |publication| self.publish(publication),
        );
        let durable_connection = match &result.mutation {
            ProviderStoreMutation::Connect {
                durable_connection, ..
            } => {
                crate::runtime_snapshot::projection::project_durable_connection(durable_connection)?
            }
            ProviderStoreMutation::Disconnect { .. } => {
                return Err(EngineError::RuntimeCompileFailed);
            }
        };
        Ok(ProviderConnectResult {
            durable_connection,
            effective_auth_source: crate::runtime_snapshot::projection::effective_auth_source(
                result.effective_auth,
            )?,
            runtime: runtime.result.snapshot.clone(),
            replayed: result.replayed,
        })
    }

    pub fn disconnect_provider(
        &self,
        params: ProviderDisconnectParams,
    ) -> Result<ProviderDisconnectResult, EngineError> {
        use cookie_agent_models::provider_store::{ClientRequestId, ProviderStoreMutation};

        let _mutation = self
            .inner
            .runtime_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = self.current_runtime();
        let expected_model_runtime_revision = self
            .inner
            .runtime_revision_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resolve(&params.expected_runtime_revision)
            .ok_or({
                EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(
                    cookie_agent_models::provider_store::ProviderStoreError::RuntimeRevisionConflict,
                ))
            })?;
        let request = cookie_agent_models::ProviderDisconnectRequest {
            provider_id: params.provider_id.clone(),
            expected_runtime_revision: expected_model_runtime_revision,
            expected_provider_state_revision: params.expected_provider_state_revision,
            expected_connection_generation: params
                .expected_connection_generation
                .map(|value| {
                    cookie_agent_models::provider_store::ProviderConnectionGeneration::new(
                        value.get(),
                    )
                })
                .transpose()
                .map_err(cookie_agent_models::ModelManagerError::from)?,
            client_request_id: ClientRequestId::new(params.client_request_id.as_str())
                .map_err(cookie_agent_models::ModelManagerError::from)?,
        };
        let result = self
            .inner
            .model_manager
            .disconnect(request, |candidate, _| {
                self.prepare_publication(
                    candidate,
                    &previous,
                    vec![RuntimeChangeReason::ProviderDisconnected],
                )
                .map_err(|_| cookie_agent_models::ModelManagerError::RuntimeCompileFailed)
            })?;
        let runtime = result.publication.map_or_else(
            || self.current_runtime(),
            |publication| self.publish(publication),
        );
        if !matches!(result.mutation, ProviderStoreMutation::Disconnect { .. }) {
            return Err(EngineError::RuntimeCompileFailed);
        }
        let receipt = result.mutation.durable_receipt();
        Ok(ProviderDisconnectResult {
            durable_receipt: cookie_agent_protocol::DurableProviderReceipt {
                receipt_id: receipt
                    .receipt_id
                    .to_string()
                    .parse()
                    .map_err(|_| EngineError::RuntimeCompileFailed)?,
                store_revision: receipt.store_revision.clone(),
                provider_state_revision: receipt.provider_state_revision.clone(),
                committed_at: receipt.committed_at,
            },
            provider_id: params.provider_id,
            disconnected: true,
            effective_auth_state: crate::runtime_snapshot::projection::effective_auth_state(
                result.effective_auth,
            ),
            runtime: runtime.result.clone(),
            replayed: result.replayed,
        })
    }

    pub fn reconcile_provider_store(&self) -> Result<bool, EngineError> {
        let _mutation = self
            .inner
            .runtime_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = self.current_runtime();
        let reloaded = self
            .inner
            .model_manager
            .reload_store_if_changed(|candidate| {
                self.prepare_publication(
                    candidate,
                    &previous,
                    vec![
                        RuntimeChangeReason::ProviderStoreChanged,
                        RuntimeChangeReason::ProviderStoreReloaded,
                    ],
                )
                .map_err(|_| cookie_agent_models::ModelManagerError::RuntimeCompileFailed)
            })
            .map_err(|_| EngineError::ProviderStoreReloadFailed)?;
        if let Some((_, publication)) = reloaded {
            self.publish(publication);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Atomically recompiles and publishes a newly acquired catalog snapshot.
    pub fn refresh_catalog(
        &self,
        catalog: Arc<cookie_agent_models::catalog::CatalogSnapshot>,
    ) -> Result<RuntimeSnapshotResult, EngineError> {
        let _mutation = self
            .inner
            .runtime_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = self.current_runtime();
        let authored = previous.models.authored().clone();
        let reason = match catalog.source {
            cookie_agent_models::catalog::CatalogSource::Network => {
                RuntimeChangeReason::CatalogRefreshed
            }
            cookie_agent_models::catalog::CatalogSource::Cache
            | cookie_agent_models::catalog::CatalogSource::Bootstrap => {
                RuntimeChangeReason::CatalogFallback
            }
        };
        let (_, publication) =
            self.inner
                .model_manager
                .reload_inputs(authored, catalog, |candidate| {
                    self.prepare_publication(candidate, &previous, vec![reason])
                        .map_err(|_| cookie_agent_models::ModelManagerError::RuntimeCompileFailed)
                })?;
        Ok(self.publish(publication).result.clone())
    }

    fn prepare_publication(
        &self,
        models: &Arc<cookie_agent_models::CompiledModelRuntime>,
        previous: &Arc<PublishedRuntime>,
        mut reasons: Vec<RuntimeChangeReason>,
    ) -> Result<RuntimePublication, EngineError> {
        #[cfg(test)]
        if self
            .inner
            .test_hooks
            .publication_failure
            .swap(false, Ordering::AcqRel)
        {
            return Err(EngineError::RuntimeCompileFailed);
        }
        let (agents, agent_presets) = resolve_agent_registries(&self.inner.config, models)?;
        let current_manifest = prepare_runtime_manifest(models)?;
        let snapshot = build_runtime_snapshot(models, &agents, &agent_presets)?;
        self.inner
            .runtime_revision_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(
                snapshot.runtime_revision.clone(),
                models.runtime_revision().clone(),
            )?;
        reasons.sort();
        reasons.dedup();
        let notification = RuntimeChangedNotification {
            previous_revision: Some(previous.result.snapshot.runtime_revision.clone()),
            snapshot: snapshot.clone(),
            reasons,
        };
        Ok(RuntimePublication {
            runtime: Arc::new(PublishedRuntime {
                result: RuntimeSnapshotResult { snapshot },
                models: Arc::clone(models),
                agents,
                agent_presets,
                current_manifest,
            }),
            notification,
        })
    }

    fn publish(&self, publication: RuntimePublication) -> Arc<PublishedRuntime> {
        self.inner
            .published_runtime
            .store(Arc::clone(&publication.runtime));
        let _ = self
            .inner
            .runtime_notifications
            .send(publication.notification);
        publication.runtime
    }

    /// Receives the products of a lazy tree load and folds them into the engine
    /// singletons (§3.3(d)). Validation runs first so a rejected load cannot
    /// leave half-applied state behind, and the artifact router is told the tree
    /// is loaded last: until this function returns `Ok` the load is not a
    /// completed one, and a sweep must still treat the child logs as unproven.
    fn apply_tree_load(
        &self,
        products: Arc<crate::session::TreeLoadProducts>,
    ) -> Result<(), EngineError> {
        let extended = self
            .inner
            .delegation_events
            .extend_from_payloads(&products.delegations)?;
        if !extended.is_empty() {
            // Only this tree's records are rebuilt; no log is read to do it
            // (tree-local C3).
            self.rebuild_delegation_registry(products.root, false)?;
        }
        let invalidated = self
            .inner
            .grant_journals
            .for_root(products.root)?
            .invalidated_ids();
        for grant in &products.grants {
            if !invalidated.contains(&grant.grant_id) {
                self.inner.approvals.store.grant(grant.clone());
            }
        }
        self.reconcile_loaded_tree_producers(products.producer_projections.clone());
        // Its child logs are now part of the durable live set for artifact
        // collection, and its own directory may be collected: the set the one
        // fold harvested replaces any later child-log scan (§3.3(b), §5.2).
        self.inner.artifacts.note_tree_live_refs(
            products.root,
            products.artifact_refs.clone(),
            products.child_log_fingerprints.clone(),
        );
        self.inner.artifacts.note_tree_loaded(products.root);
        Ok(())
    }

    /// Installs the store-side hook and delivers everything a load that already
    /// completed was holding back (§3.3, D5).
    fn install_tree_load_observer(&self) -> Result<(), EngineError> {
        struct Observer(std::sync::Weak<Inner>);

        impl crate::session::TreeLoadObserver for Observer {
            fn tree_loaded(
                &self,
                products: Arc<crate::session::TreeLoadProducts>,
            ) -> Result<(), EngineError> {
                let inner = self.0.upgrade().ok_or(EngineError::ActorStopped)?;
                Engine { inner }.apply_tree_load(products)
            }
        }

        self.inner
            .store
            .set_tree_load_observer(Arc::new(Observer(Arc::downgrade(&self.inner))))?;
        Ok(())
    }

    /// Completes the tree of `id` before it is used (§3.2/§3.3), then delivers
    /// any load this process finished while no hook was installed yet (D5).
    /// Cheap once the tree is loaded.
    pub(crate) fn ensure_tree_loaded(&self, id: SessionId) -> Result<(), EngineError> {
        self.inner.store.ensure_tree_for(id)?;
        self.inner.store.drain_pending_loads()?;
        Ok(())
    }

    /// Producer reconciliation for children surfaced by a tree load. Runs on the
    /// engine runtime when one is available; otherwise the periodic plugin
    /// producer scan picks them up, since loaded trees cache their hits (§4.4).
    fn reconcile_loaded_tree_producers(
        &self,
        projections: Vec<(SessionId, crate::goal_projection::GoalProducerProjection)>,
    ) {
        if projections.is_empty() {
            return;
        }
        let Some(handle) = self
            .inner
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        handle.spawn(async move {
            for (session, projection) in projections {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let _ = Engine { inner }
                    .request(session, |reply| {
                        SessionCommand::Producer(
                            crate::runtime::producers::ProducerCommand::Reconcile {
                                projection: Some(projection.clone()),
                                reply,
                            },
                        )
                    })
                    .await;
            }
        });
    }

    pub(super) fn mutation_lock(
        &self,
        key: &PreparedSerializationKey,
    ) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .mutation_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Registers a tool provider after engine open, allowing providers that
    /// require an Engine (notably delegate) to break the construction cycle.
    pub fn register_tool_provider(&self, provider: Arc<dyn ToolProvider>) {
        self.try_register_tool_provider(provider)
            .expect("tool provider collision");
    }

    pub fn try_register_tool_provider(
        &self,
        provider: Arc<dyn ToolProvider>,
    ) -> Result<(), EngineError> {
        let mut provider_ids = self
            .inner
            .provider_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reserve_provider_id(&mut provider_ids, provider.as_ref())?;
        if let Err(error) = self.inner.mcp.reserve_provider(provider.as_ref()) {
            provider_ids.remove(provider.provider_id());
            return Err(EngineError::ToolFailed(error.to_string()));
        }
        self.inner
            .tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(provider);
        Ok(())
    }

    #[must_use]
    pub fn mcp_statuses(&self) -> Vec<crate::McpServerStatus> {
        self.inner.mcp.statuses()
    }

    #[must_use]
    pub fn plugin_statuses(&self) -> Vec<crate::PluginStatus> {
        self.inner.plugins.statuses()
    }

    #[cfg(test)]
    pub(crate) fn block_plugin_diagnostic_appends_for_test(&self) {
        *self
            .inner
            .test_hooks
            .plugin_diagnostic_append_block
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Arc::new(tokio::sync::Notify::new()));
    }

    #[cfg(test)]
    pub(crate) fn block_tool_progress_appends_for_test(&self) -> Arc<tokio::sync::Notify> {
        let reached = Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .tool_progress_append_block
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Arc::new(ToolProgressAppendBlock {
                reached: reached.clone(),
                release: tokio::sync::Notify::new(),
            }));
        reached
    }

    #[cfg(test)]
    pub(crate) fn pending_plugin_diagnostic_keys_for_test(&self) -> usize {
        self.inner.plugin_diagnostics.accumulator.key_count()
    }

    pub async fn ping_plugin(&self, name: &str) -> Result<(), String> {
        self.inner.plugins.ping(name).await
    }

    /// Stops new session mailbox traffic and cancels active work. Existing
    /// client clones may keep a session mailbox alive.
    pub async fn shutdown(&self) {
        self.inner
            .delegation
            .admission_tasks_closing
            .store(true, Ordering::Release);
        let janitor = self
            .inner
            .janitor_task
            .lock()
            .ok()
            .and_then(|mut task| task.take());
        if let Some(task) = janitor {
            task.abort();
            let _ = task.await;
        }
        let tasks = self
            .inner
            .delegation
            .admission_tasks
            .lock()
            .map(|mut tasks| tasks.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                eprintln!("admission task stopped during shutdown: {error}");
            }
        }
        let blocking_tasks = self
            .inner
            .delegation
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
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        for run in active {
            run.cancellation.cancel();
        }
        // Joined here, before the plugin and MCP transports go away and well
        // before the actors are cleared: a run task may be mid tool call, and
        // it needs live transports to unwind and a live store to append its
        // `RunCancelled`. Anything still running when the bound expires is
        // aborted, so shutdown stays bounded.
        self.join_run_tasks(RUN_TASK_SHUTDOWN_TIMEOUT).await;
        self.inner.plugins.shutdown().await;
        self.inner.mcp.shutdown().await;
        self.inner
            .plugin_diagnostics
            .accumulator
            .shutdown
            .store(true, Ordering::Release);
        self.inner
            .plugin_diagnostics
            .accumulator
            .notify
            .notify_one();
        let diagnostic_task = self
            .inner
            .plugin_diagnostics
            .task
            .lock()
            .ok()
            .and_then(|mut task| task.take());
        if let Some(mut task) = diagnostic_task
            && tokio::time::timeout(
                plugin_diagnostics::PLUGIN_DIAGNOSTIC_SHUTDOWN_TIMEOUT,
                &mut task,
            )
            .await
            .is_err()
        {
            for plugin in self.inner.plugin_diagnostics.accumulator.offenders() {
                self.inner.plugins.note_offender_diagnostic(
                    &plugin,
                    "plugin diagnostic drain incomplete: shutdown deadline exceeded".into(),
                );
            }
            task.abort();
            let _ = task.await;
        }
        self.inner
            .sessions
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.inner
            .tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.inner.store.release_ownership();
    }
}

fn reserve_provider_id(
    provider_ids: &mut HashSet<&'static str>,
    provider: &dyn ToolProvider,
) -> Result<(), EngineError> {
    let provider_id = provider.provider_id();
    if !provider_ids.insert(provider_id) {
        return Err(EngineError::ToolFailed(format!(
            "tool provider ID `{provider_id}` is already registered"
        )));
    }
    Ok(())
}

type ResolvedAgentRegistries = (Arc<AgentRegistry>, BTreeMap<String, Arc<AgentRegistry>>);

fn resolve_agent_registries(
    config: &LoadedConfiguration,
    models: &Arc<cookie_agent_models::CompiledModelRuntime>,
) -> Result<ResolvedAgentRegistries, EngineError> {
    let shared = Arc::new(AgentRegistry::resolve(
        &config.agent_registry(),
        models,
        None,
    )?);
    let presets = config
        .agent_preset_registries()
        .into_iter()
        .map(|(name, authored)| {
            AgentRegistry::resolve(&authored, models, Some(name.clone()))
                .map(|registry| (name, Arc::new(registry)))
        })
        .collect::<Result<_, _>>()?;
    Ok((shared, presets))
}

#[cfg(test)]
mod context_token_estimator_tests {
    use super::{ContextTokenEstimator, should_run_predictive_compaction};

    #[test]
    fn learns_from_committed_usage_and_projects() {
        let mut estimator = ContextTokenEstimator::default();
        estimator.record_committed_turn(200, Some(50));

        assert_eq!(estimator.tokens_per_byte, 0.25);
        assert_eq!(estimator.last_committed_input_tokens, 50);
        assert_eq!(estimator.projected_tokens(40), Some(60));
        assert_eq!(estimator.estimated_context_tokens(40), Some(10));
        assert_eq!(estimator.estimated_context_tokens(41), Some(11));
    }

    #[test]
    fn skips_degenerate_ratio_updates() {
        let mut estimator = ContextTokenEstimator {
            tokens_per_byte: 0.5,
            last_committed_input_tokens: 10,
        };
        estimator.record_committed_turn(0, Some(20));
        assert_eq!(estimator.tokens_per_byte, 0.5);
        assert_eq!(estimator.last_committed_input_tokens, 20);

        estimator.record_committed_turn(100, None);
        assert_eq!(estimator.tokens_per_byte, 0.5);
        assert_eq!(estimator.last_committed_input_tokens, 0);

        estimator.record_committed_turn(100, Some(0));
        assert_eq!(estimator.tokens_per_byte, 0.5);
        assert_eq!(estimator.last_committed_input_tokens, 0);
    }

    #[test]
    fn predictive_trigger_crosses_or_stays_below_effective_limit() {
        let estimator = ContextTokenEstimator {
            tokens_per_byte: 0.5,
            last_committed_input_tokens: 60,
        };

        assert!(estimator.should_compact(20, 70));
        assert!(!estimator.should_compact(18, 70));
        assert!(!ContextTokenEstimator::default().should_compact(usize::MAX, 1));
    }

    #[test]
    fn predictive_compaction_is_disabled_until_session_persistence() {
        let estimator = ContextTokenEstimator {
            tokens_per_byte: 1.0,
            last_committed_input_tokens: 100,
        };

        assert!(!should_run_predictive_compaction(estimator, 100, 70, false));
        assert!(should_run_predictive_compaction(estimator, 100, 70, true));
    }
}
