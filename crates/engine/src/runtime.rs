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
    permissions::{ApprovalStore, PermissionPipeline},
    policy::{
        self, FrozenRunPolicy, freeze_delegated_agent_policy, freeze_root_agent_policy,
        policy_for_session_selection, policy_from_snapshot, resolve_agent,
    },
    session::{self, SessionError, SessionStore},
};

mod admission;
mod approval_api;
mod approval_flow;
mod approval_projection;
mod artifacts;
mod compaction;
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
use delegation::*;
use helpers::*;
use internal_agents::*;
use tool_execution::*;

#[cfg(test)]
pub(crate) use approval_projection::{approval_records, doom_loop_repetitions};
#[cfg(test)]
pub(crate) use delegation::{completed_delegate_result, freeze_delegated_child_policy};
#[cfg(test)]
pub(crate) use helpers::{cwd_identity, invocation_id, protocol_digest};
#[cfg(test)]
pub(crate) use recovery::{restart_approval_decision, restart_tool_failure};
#[cfg(test)]
pub(crate) use sessions::session_meta;
#[cfg(test)]
pub(crate) use titles::{active_fallback_index, title_regeneration_target};
#[cfg(test)]
pub(crate) use tool_execution::{safe_tool_presentation, validate_attachment};

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
    pub(crate) models: Vec<cookie_agent_models::FrozenModelBinding>,
    model_snapshot: Option<Arc<cookie_agent_models::ModelSnapshot>>,
    limits: InternalAgentLimits,
}

pub(crate) struct InternalAgentRuntime {
    approval: FrozenInternalAgentPolicy,
    context_compaction: FrozenInternalAgentPolicy,
    session_title: FrozenInternalAgentPolicy,
}

impl InternalAgentRuntime {
    pub(crate) fn freeze() -> Self {
        Self {
            approval: unavailable_internal_policy(30_000, 16_384, 2_048),
            context_compaction: unavailable_internal_policy(30_000, 16_384, 2_048),
            session_title: unavailable_internal_policy(10_000, 4_096, 128),
        }
    }

    pub(crate) fn policy(
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
pub(crate) const BOUNDED_SUMMARY_BUILTIN_REVISION: &str =
    "context-compaction.bounded-summary.prompt-runtime.1";
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

pub(crate) struct Inner {
    config: LoadedConfiguration,
    pub(crate) artifacts: Arc<ArtifactStore>,
    mutation_locks: Mutex<HashMap<PreparedSerializationKey, Arc<tokio::sync::Mutex<()>>>>,
    pub(crate) store: Arc<SessionStore>,
    pub(crate) journal: Arc<DelegationJournal>,
    grant_journal: Arc<GrantInvalidationJournal>,
    pub(crate) model_manager: Arc<ModelSetManager>,
    pub(crate) session_model_snapshots:
        Mutex<HashMap<SessionId, Arc<cookie_agent_models::ModelSnapshot>>>,
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
    pub(crate) pending_approvals: Mutex<HashMap<(SessionId, ApprovalId), PendingApproval>>,
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

/// Cloneable in-process client facade. It contains no transport concerns and
/// is safe for tool providers to call while their parent call is executing.
#[derive(Clone)]
pub struct Engine {
    pub(crate) inner: Arc<Inner>,
}
pub type EngineClient = Engine;

impl Engine {
    pub(crate) fn materialize_agents(
        &self,
        model_set: &cookie_agent_models::ModelSet,
    ) -> Result<Arc<AgentRegistry>, EngineError> {
        self.inner
            .config
            .resolve_agents(model_set)
            .map(Arc::new)
            .map_err(|error| EngineError::Config(Box::new(error)))
    }

    pub(super) fn session_model_snapshot(
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
