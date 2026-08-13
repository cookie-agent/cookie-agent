use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    future::Future,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[cfg(test)]
use std::sync::mpsc as std_mpsc;

use arc_swap::ArcSwap;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_config::LoadedConfiguration;
use cookie_agent_models::{
    ModelManager,
    manifests::{ManifestError, ModelSnapshotManifestStore, RehydrationError},
};
use cookie_agent_protocol::{
    AgentId, AgentMode, ApprovalConstraints, ApprovalDecisionSource, ApprovalEvaluation,
    ApprovalFinalDecision, ApprovalFinalOutcome, ApprovalId, ApprovalInternalDecision,
    ApprovalInternalDecisionKind, ApprovalListResult, ApprovalReasonCode, ApprovalRecord,
    ApprovalRequest, ApprovalRespondErrorCode, ApprovalRespondParams, ApprovalRespondResult,
    ApprovalStatus, ApprovalTrigger, ApprovalUserDecision, ArtifactReference, ChildSummary,
    ContextCheckpoint, ContextCheckpointBoundaries, ContextCheckpointBudgets,
    ContextCheckpointCommit, ContextRehydratedFile, EventPayload as Event,
    EventSubscriptionMessage, EventsSubscribeResult, InternalAgentBackend, InternalAgentFailure,
    InternalAgentInvocationId, InternalAgentKind, InternalAgentRunId, InternalSummaryCheckpoint,
    InvocationId, OperationFingerprint, OutputStream, PermissionMode, PersistedAssistantPart,
    PersistedModelTurn, PersistedToolResult as ToolResult, PreparedOperationIdentity,
    ProviderConnectParams, ProviderConnectResult, ProviderDisconnectParams,
    ProviderDisconnectResult, RunCancelResult, RunId, RunRecallSteerResult, RunSelection,
    RunStartParams, RunStartResult, RunSteerResult, RunToolStdinParams, RunToolStdinResult,
    RuntimeChangeReason, RuntimeChangedNotification, RuntimeSnapshotResult, SafeCode,
    SafeDisplayText, SafeErrorMessage, SafeInternalAgentCall, SafeInternalAgentResult,
    SafeToolError, SessionForkResult, SessionId, SessionMeta, SessionOrigin, SessionRenameChange,
    SessionRenameParams, SessionRenameResult, SessionRevertResult, SessionStatus, SessionTitle,
    SessionTitleChange, Sha256Digest, StoredEvent, SummaryByteLimit, ToolAttachment, ToolCallId,
    ToolCallPresentation, ToolCallStart, ToolCallTermination, ToolOutputTruncation,
    ToolTerminationOutcome, TreeApprovalGrant,
};
use futures_util::StreamExt;
use oven_sdk::{JsonSchema, ModelError, Request as ModelRequest, ToolDefinition};
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, fchmod, fsync, openat, renameat, statat, unlinkat,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    actor::SessionActor,
    events::{self, EventLogError, OutputHub},
    grant_journal::{GrantInvalidationJournal, GrantJournalError},
    journal::{self, DelegationJournal, JournalError},
    media::approved_media_type,
    model_bridge::{AbortBridge, TurnAccumulator},
    model_history::{
        self, assemble_model_context, persist_turn, replay_decisions_with_preflight, wire_model,
    },
    model_policy::{ErrorPolicy, classify as classify_model_error, summary as model_error_summary},
    model_snapshots::{prepare_runtime_manifest, validate_referenced_binding},
    permissions::{ApprovalStore, PermissionPipeline},
    policy::{
        self, FrozenRunPolicy, freeze_delegated_agent_policy, freeze_root_agent_policy,
        policy_for_session_selection, policy_from_snapshot, resolve_agent,
    },
    runtime_snapshot::{
        AgentRegistry, PublishedRuntime, RuntimePublication, build_runtime_snapshot,
    },
    session::{self, SessionError, SessionStore},
};

mod admission;
mod approval_api;
mod approval_flow;
mod approval_projection;
mod artifacts;
pub(crate) mod compaction;
mod delegation;
mod helpers;
mod internal_agents;
mod mailbox;
mod model_loop;
mod recovery;
mod runs;
mod sessions;
mod titles;
pub(crate) mod tool_execution;

use admission::*;
use approval_flow::*;
use approval_projection::*;
pub(crate) use artifacts::{ArtifactStore, OutputCapture};
use compaction::*;
use delegation::*;
use helpers::*;
use internal_agents::*;
use tool_execution::*;

use crate::{
    delegation_api::{DelegateAwait, DelegateHandle, DelegateInvocation},
    tool_api::{
        PreparedExecutorCell, PreparedSerializationKey, PreparedTool, ProgressSink,
        SessionToolContext, StdinWrite, ToolCall, ToolError, ToolExecutionContext,
        ToolPreparationContext, ToolProvider, ToolStdin,
    },
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
    #[error("no_runnable_model")]
    NoRunnableModel,
    #[error("provider_store_reload_failed")]
    ProviderStoreReloadFailed,
    #[error("runtime_compile_failed")]
    RuntimeCompileFailed,
    #[error("invalid runtime agent `{0}`")]
    InvalidRuntimeAgent(AgentId),
    #[error(transparent)]
    ModelManager(#[from] cookie_agent_models::ModelManagerError),
    #[error(transparent)]
    Manifest(ManifestError),
    #[error(transparent)]
    SnapshotRehydration(RehydrationError),
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
    fallback_index: AtomicU64,
}

struct AttemptTurn {
    turn: PersistedModelTurn,
    model_turn_seq: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct ContextTokenEstimator {
    tokens_per_byte: f64,
    last_committed_input_tokens: u64,
}

struct PredictiveCompactionInput<'a> {
    session: SessionId,
    run: RunId,
    serialized_message_bytes: usize,
    policy: &'a FrozenRunPolicy,
    fallback_index: usize,
    cancellation: &'a CancellationToken,
    actor_direct: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingInput {
    admission_seq: u64,
    input: String,
}

struct PendingPromotionState {
    promoted: bool,
    pending: Vec<PendingInput>,
    continue_run: bool,
}

impl ContextTokenEstimator {
    fn record_committed_turn(
        &mut self,
        serialized_history_bytes: usize,
        input_tokens: Option<u64>,
    ) {
        self.last_committed_input_tokens = input_tokens.unwrap_or(0);
        if serialized_history_bytes > 0
            && let Some(input_tokens) = input_tokens.filter(|tokens| *tokens > 0)
        {
            self.tokens_per_byte = input_tokens as f64 / serialized_history_bytes as f64;
        }
    }

    fn projected_tokens(self, serialized_message_bytes: usize) -> Option<u64> {
        (self.tokens_per_byte > 0.0).then(|| {
            self.last_committed_input_tokens
                .saturating_add((serialized_message_bytes as f64 * self.tokens_per_byte) as u64)
        })
    }

    fn should_compact(self, serialized_message_bytes: usize, soft_tokens: u64) -> bool {
        self.projected_tokens(serialized_message_bytes)
            .is_some_and(|projected| projected >= soft_tokens)
    }

    fn record_compaction(&mut self, estimated_input_tokens: u64) {
        self.last_committed_input_tokens = estimated_input_tokens;
    }
}

fn should_run_predictive_compaction(
    estimator: ContextTokenEstimator,
    serialized_message_bytes: usize,
    soft_tokens: u64,
    session_persisted: bool,
) -> bool {
    session_persisted && estimator.should_compact(serialized_message_bytes, soft_tokens)
}

#[derive(Clone, Debug)]
pub(crate) struct ApprovalOutcome {
    pub(crate) approved: bool,
    pub(crate) feedback: Option<String>,
}

pub(crate) struct PendingApproval {
    pub(crate) sender: oneshot::Sender<ApprovalOutcome>,
    pub(crate) executor: PreparedExecutorCell,
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

struct ApprovalToolInput<'a> {
    name: &'a str,
    normalized_parameters: &'a Value,
}

struct ModelApprovalInput<'a> {
    operation: &'a PreparedOperationIdentity,
    policy_labels: &'a [String],
    executor: PreparedExecutorCell,
    message: Option<String>,
    tool: ApprovalToolInput<'a>,
}

struct PreparedToolCall {
    call: ToolCall,
    presentation: ToolCallPresentation,
    prepared: Result<PreparedTool, ToolFailure>,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolFailure {
    pub(crate) code: ToolCallFailureCode,
    pub(crate) message: String,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ToolCallFailureCode {
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
pub(crate) struct FrozenInternalAgentPolicy {
    pub(crate) agent: cookie_agent_protocol::AgentSnapshot,
    pub(crate) models: Vec<cookie_agent_protocol::FrozenModelBinding>,
    pub(crate) runtime: Option<Arc<PublishedRuntime>>,
    pub(crate) limits: InternalAgentLimits,
}

#[derive(Clone, Debug)]
pub(crate) struct InternalAgentLimits {
    pub(crate) max_input_tokens: u64,
    pub(crate) max_output_tokens: u64,
    pub(crate) timeout_ms: u64,
}

struct InternalAgentTextResult {
    invocation_id: InternalAgentInvocationId,
    internal_run_id: InternalAgentRunId,
    text: String,
}

struct InternalAgentHistoryInput {
    history: Vec<oven_sdk::HistoryTurn>,
    summary_source: String,
    tools: Vec<ToolDefinition>,
    reject_non_text: bool,
}

#[derive(Clone, Copy)]
struct InternalAgentExecution<'a> {
    cancellation: &'a CancellationToken,
    actor_direct: bool,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredRuntimeRevisionMapping {
    protocol_version: cookie_agent_protocol::ProtocolVersion,
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
        for record in events::load_jsonl::<StoredRuntimeRevisionMapping>(&path)? {
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
                protocol_version: cookie_agent_protocol::ProtocolVersion::current(),
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

const SESSION_MAILBOX_CAPACITY: usize = 256;
const PERSISTED_SUBSCRIBER_QUEUE_CAPACITY: usize = 256;
const MAX_PENDING_PREPARED_TOOLS: usize = 64;
/// Semantic revision of the no-model builtin runtime contract.
/// This is intentionally independent of the protocol and event schema version.
pub(crate) const UNAVAILABLE_BUILTIN_REVISION: &str = "internal-agent.unavailable.runtime.1";

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
        reply: oneshot::Sender<Result<bool, EngineError>>,
    },
    Revert {
        through_seq: u64,
        reply: oneshot::Sender<Result<SessionRevertResult, EngineError>>,
    },
    Fork {
        through_seq: u64,
        reply: oneshot::Sender<Result<SessionForkResult, EngineError>>,
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
    PromotePendingOrComplete {
        run: RunId,
        final_text: Option<String>,
        complete_if_empty: bool,
        reply: oneshot::Sender<Result<bool, EngineError>>,
    },
    PromptSnapshot {
        run: RunId,
        reply: oneshot::Sender<Result<Vec<StoredEvent>, EngineError>>,
    },
}

impl SessionCommand {
    fn compaction_deferred_kind(&self) -> Option<CompactionDeferredKind> {
        match self {
            Self::PromptSnapshot { .. } => Some(CompactionDeferredKind::PromptSnapshot),
            Self::PromotePendingOrComplete { .. } => {
                Some(CompactionDeferredKind::PromotePendingOrComplete)
            }
            Self::Resume { .. } => Some(CompactionDeferredKind::Resume),
            _ => None,
        }
    }

    fn reject_duplicate_during_compaction(self, session: SessionId) {
        match self {
            Self::PromptSnapshot { reply, .. } => {
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum CompactionDeferredKind {
    PromptSnapshot,
    PromotePendingOrComplete,
    Resume,
}

const MAX_COMPACTION_DEFERRED_COMMANDS: usize = 3;

pub(crate) struct Inner {
    config: LoadedConfiguration,
    pub(crate) artifacts: Arc<ArtifactStore>,
    mutation_locks: Mutex<HashMap<PreparedSerializationKey, Arc<tokio::sync::Mutex<()>>>>,
    pub(crate) store: Arc<SessionStore>,
    pub(crate) journal: Arc<DelegationJournal>,
    grant_journal: Arc<GrantInvalidationJournal>,
    pub(crate) model_manager: Arc<ModelManager>,
    published_runtime: ArcSwap<PublishedRuntime>,
    runtime_mutation: Mutex<()>,
    runtime_notifications: broadcast::Sender<RuntimeChangedNotification>,
    runtime_revision_index: Mutex<RuntimeRevisionIndex>,
    manifest_store: ModelSnapshotManifestStore,
    tools: Mutex<Vec<Arc<dyn ToolProvider>>>,
    approvals: ApprovalStore,
    permissions: PermissionPipeline,
    active: Mutex<HashMap<RunId, Arc<ActiveRun>>>,
    inflight_delegations: Mutex<HashMap<InvocationId, HashMap<u64, InflightDelegation>>>,
    delegation_admission: tokio::sync::Mutex<()>,
    next_admission_generation: AtomicU64,
    subscribers: Mutex<HashMap<SessionId, Vec<PersistedSubscriber>>>,
    actors: Mutex<HashMap<SessionId, SessionActor<SessionCommand>>>,
    output_hubs: Mutex<HashMap<ToolCallId, OutputHub>>,
    finalized_output_hubs: Mutex<VecDeque<ToolCallId>>,
    pub(crate) pending_approvals: Mutex<HashMap<(SessionId, ApprovalId), PendingApproval>>,
    permission_modes: Mutex<HashMap<SessionId, PermissionMode>>,
    compaction_auto_disabled: Mutex<HashSet<SessionId>>,
    compaction_postcheck_pending: Mutex<HashSet<SessionId>>,
    compaction_in_progress: Mutex<HashSet<SessionId>>,
    compaction_deferred: Mutex<HashMap<SessionId, VecDeque<SessionCommand>>>,
    context_token_estimators: Mutex<HashMap<SessionId, ContextTokenEstimator>>,
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
    #[cfg(test)]
    pub(crate) publication_failure: AtomicBool,
}

/// Cloneable in-process client facade. It contains no transport concerns and
/// is safe for tool providers to call while their parent call is executing.
#[derive(Clone)]
pub struct Engine {
    pub(crate) inner: Arc<Inner>,
}
pub type EngineClient = Engine;

impl Engine {
    pub fn open(options: EngineOptions) -> Result<Self, EngineError> {
        let current_models = options.model_manager.current();
        let authored_agents = options.config.agent_registry();
        let agents = Arc::new(AgentRegistry::resolve(&authored_agents, &current_models)?);
        let manifest_store = ModelSnapshotManifestStore::open(&options.cwd)?;
        let prepared_manifest = prepare_runtime_manifest(&manifest_store, &current_models)?;
        let snapshot = build_runtime_snapshot(&current_models, &agents)?;
        let published_runtime = Arc::new(PublishedRuntime {
            result: RuntimeSnapshotResult { snapshot },
            models: Arc::clone(&current_models),
            agents,
            manifests: prepared_manifest.index,
            current_manifest: prepared_manifest.manifest,
        });
        let store = SessionStore::open(&options.data_dir, &options.cwd)?;
        let artifacts = ArtifactStore::open(store.project_dir_path().join("artifacts"))?;
        let journal = DelegationJournal::open(store.project_dir_path().join("delegations.jsonl"))?;
        let grant_journal = GrantInvalidationJournal::open(
            store.project_dir_path().join("grant-invalidations.jsonl"),
        )?;
        let (runtime_notifications, _) = broadcast::channel(64);
        let mut runtime_revision_index = RuntimeRevisionIndex::open(
            store.project_dir_path().join("runtime-revisions-v8.jsonl"),
        )?;
        runtime_revision_index.record(
            published_runtime.result.snapshot.runtime_revision.clone(),
            current_models.runtime_revision().clone(),
        )?;
        let engine = Self {
            inner: Arc::new(Inner {
                config: options.config,
                artifacts,
                mutation_locks: Mutex::new(HashMap::new()),
                store,
                journal,
                grant_journal,
                model_manager: options.model_manager,
                published_runtime: ArcSwap::from(published_runtime),
                runtime_mutation: Mutex::new(()),
                runtime_notifications,
                runtime_revision_index: Mutex::new(runtime_revision_index),
                manifest_store,
                tools: Mutex::new(options.tools),
                approvals: ApprovalStore::default(),
                permissions: PermissionPipeline::default(),
                active: Mutex::new(HashMap::new()),
                inflight_delegations: Mutex::new(HashMap::new()),
                delegation_admission: tokio::sync::Mutex::new(()),
                next_admission_generation: AtomicU64::new(1),
                subscribers: Mutex::new(HashMap::new()),
                actors: Mutex::new(HashMap::new()),
                output_hubs: Mutex::new(HashMap::new()),
                finalized_output_hubs: Mutex::new(VecDeque::new()),
                pending_approvals: Mutex::new(HashMap::new()),
                permission_modes: Mutex::new(HashMap::new()),
                compaction_auto_disabled: Mutex::new(HashSet::new()),
                compaction_postcheck_pending: Mutex::new(HashSet::new()),
                compaction_in_progress: Mutex::new(HashSet::new()),
                compaction_deferred: Mutex::new(HashMap::new()),
                context_token_estimators: Mutex::new(HashMap::new()),
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
                #[cfg(test)]
                publication_failure: AtomicBool::new(false),
            }),
        };
        engine.validate_referenced_manifests()?;
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
        if self.inner.publication_failure.swap(false, Ordering::AcqRel) {
            return Err(EngineError::RuntimeCompileFailed);
        }
        let authored_agents = self.inner.config.agent_registry();
        let agents = Arc::new(AgentRegistry::resolve(&authored_agents, models)?);
        let prepared = prepare_runtime_manifest(&self.inner.manifest_store, models)?;
        let snapshot = build_runtime_snapshot(models, &agents)?;
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
                manifests: prepared.index,
                current_manifest: prepared.manifest,
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

    fn validate_referenced_manifests(&self) -> Result<(), EngineError> {
        let runtime = self.current_runtime();
        for session in self.inner.store.all() {
            for event in session.log.events() {
                match event.payload {
                    Event::SessionCreated { creation_agent, .. } => {
                        for binding in &creation_agent.fallback_chain {
                            let validation = validate_referenced_binding(
                                &runtime.manifests,
                                &runtime.models,
                                binding,
                            );
                            if !matches!(
                                &validation,
                                Ok(())
                                    | Err(EngineError::SnapshotRehydration(
                                        RehydrationError::SnapshotConfigMismatch
                                            | RehydrationError::SnapshotCredentialsUnavailable
                                    ))
                            ) {
                                validation?;
                            }
                        }
                    }
                    Event::RunStarted {
                        selected_suffix, ..
                    } => {
                        for binding in &selected_suffix {
                            let validation = validate_referenced_binding(
                                &runtime.manifests,
                                &runtime.models,
                                binding,
                            );
                            if !matches!(
                                &validation,
                                Ok(())
                                    | Err(EngineError::SnapshotRehydration(
                                        RehydrationError::SnapshotConfigMismatch
                                            | RehydrationError::SnapshotCredentialsUnavailable
                                    ))
                            ) {
                                validation?;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        for entry in self.inner.journal.entries() {
            let manifest = runtime
                .manifests
                .require(&entry.revisions.manifest_revision)?;
            if manifest.payload.catalog_revision != entry.revisions.catalog_revision
                || manifest.payload.provider_state_revision
                    != entry.revisions.provider_state_revision
                || manifest.payload.model_revision != entry.revisions.model_revision
                || manifest.payload.recipe_registry_revision
                    != entry.revisions.recipe_registry_revision
                || crate::runtime_snapshot::projection::runtime_revision(
                    &entry.revisions.recipe_registry_revision,
                    &entry.revisions.catalog_revision,
                    &entry.revisions.provider_state_revision,
                    &entry.revisions.model_revision,
                    &entry.revisions.agent_revision,
                )? != entry.revisions.runtime_revision
            {
                return Err(EngineError::RuntimeCompileFailed);
            }
            for binding in &entry.selected_suffix {
                let validation =
                    validate_referenced_binding(&runtime.manifests, &runtime.models, binding);
                if !matches!(
                    &validation,
                    Ok(())
                        | Err(EngineError::SnapshotRehydration(
                            RehydrationError::SnapshotConfigMismatch
                                | RehydrationError::SnapshotCredentialsUnavailable
                        ))
                ) {
                    validation?;
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn client(&self) -> EngineClient {
        self.clone()
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
