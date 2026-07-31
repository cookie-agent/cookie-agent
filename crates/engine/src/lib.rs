//! The transport-free single-conversation cookie agent runtime.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
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
use cookie_agent_config::{
    AgentType as ConfigAgentType, Config, DepthLimit as ConfigDepthLimit, PolicySnapshot,
};
use cookie_agent_protocol::{
    AgentDescriptor, AgentListResult, AgentType, ApprovalDecision, ApprovalRespondResult,
    ApprovedScope, ChildSummary, Event, EventEnvelope, EventSubscriptionMessage,
    EventsSubscribeResult, InvocationId, ModelRef, RunCancelResult, RunId, RunStartParams,
    RunStartResult, RunSteerResult, RunToolStdinParams, RunToolStdinResult, SessionId, SessionMeta,
    SessionOrigin, SessionStatus, ToolCallId, TurnOpaque,
};
use cookie_agent_providers::{
    ContentPart, ModelId, ModelRef as ProviderModelRef, NormalizedEvent, Provider, ProviderError,
    ProviderErrorClass, ProviderMessage, ProviderProtocol, ProviderRequest, StopReason,
    ToolDefinition,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub mod actor;
pub mod events;
pub mod journal;
pub mod permissions;
pub mod run;
pub mod session;

use actor::SessionActor;
use events::{EventLogError, OutputHub};
use journal::{DelegationJournal, JournalError};
use permissions::{ApprovalStore, PermissionPipeline};
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
pub struct ToolResult {
    pub content: String,
    pub truncated: bool,
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
    pub profile: String,
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
}
impl ProgressSink {
    #[must_use]
    pub fn new(sender: mpsc::Sender<ToolProgress>, output: OutputHub) -> Self {
        Self { sender, output }
    }
    pub async fn send(&self, progress: ToolProgress) -> Result<(), ToolError> {
        self.sender
            .send(progress)
            .await
            .map_err(|_| ToolError::ProgressSinkClosed)
    }
    pub fn output(&self, stream: cookie_agent_protocol::OutputStream, data: &[u8]) {
        self.output.emit(stream, data);
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

#[derive(Debug)]
pub struct ToolInvocationContext {
    pub session: SessionId,
    pub run: RunId,
    /// Resolved working directory frozen in the session metadata.
    pub cwd: PathBuf,
    /// Workspace root used for permission canonicalization.
    pub workspace_root: PathBuf,
    pub progress: ProgressSink,
    pub cancellation: CancellationToken,
    pub stdin: Option<ToolStdin>,
}
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool progress sink closed")]
    ProgressSinkClosed,
    #[error("tool failed: {0}")]
    Failed(String),
}
#[async_trait]
pub trait ToolProvider: Send + Sync {
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError>;
    async fn invoke(
        &self,
        ctx: ToolInvocationContext,
        call: ToolCall,
    ) -> Result<ToolResult, ToolError>;
}

#[derive(Clone)]
pub struct EngineOptions {
    pub data_dir: PathBuf,
    pub cwd: PathBuf,
    pub config: Config,
    pub providers: HashMap<String, Arc<dyn Provider>>,
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
    #[error("configuration error: {0}")]
    Config(#[source] Box<cookie_agent_config::ConfigError>),
    #[error("profile `{0}` is subagent-only")]
    SubagentOnly(String),
    #[error("run {0} not found")]
    MissingRun(RunId),
    #[error("session {0} is already running")]
    SessionRunning(SessionId),
    #[error("client run id conflicts with durable run parameters")]
    RunIdempotencyConflict,
    #[error("tool call is not running or is not interactive")]
    StdinUnavailable,
    #[error("invalid base64 stdin: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("provider failure: {0}")]
    Provider(#[from] cookie_agent_providers::ProviderError),
    #[error("tool `{0}` is unavailable")]
    MissingTool(String),
    #[error("session actor for {0} is unavailable")]
    MissingActor(SessionId),
    #[error("session actor stopped before replying")]
    ActorStopped,
}

#[derive(Debug)]
struct ActiveRun {
    session: SessionId,
    cancellation: CancellationToken,
    cancelled_committed: Mutex<bool>,
    stdin: Mutex<HashMap<ToolCallId, mpsc::Sender<StdinWrite>>>,
    /// Last persisted event included in the current provider request.
    prompt_seq: AtomicU64,
}

struct AttemptEvents {
    events: Vec<NormalizedEvent>,
    protocol: Option<ProviderProtocol>,
}

struct EmittedToolCall {
    assistant_index: usize,
    call_index: usize,
    segment: u64,
    run_id: RunId,
    provider_tool_call_id: String,
    result: Option<cookie_agent_providers::ToolResult>,
}

#[cfg(test)]
struct PromptSnapshotHook {
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
    ToolResult {
        run: RunId,
        tool_call_id: ToolCallId,
        result: Result<ToolResult, String>,
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
        reply: oneshot::Sender<Result<Vec<EventEnvelope>, EngineError>>,
    },
}

struct Inner {
    config: Config,
    store: Arc<SessionStore>,
    journal: Arc<DelegationJournal>,
    providers: HashMap<String, Arc<dyn Provider>>,
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
    pending_approvals: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    runtime: Option<tokio::runtime::Handle>,
    admission_tasks: Mutex<Vec<JoinHandle<()>>>,
    admission_blocking_tasks: Mutex<Vec<JoinHandle<()>>>,
    admission_tasks_closing: AtomicBool,
    recovery_waiters: Mutex<HashSet<(SessionId, RunId, ToolCallId)>>,
    #[cfg(test)]
    prompt_snapshot_hook: Mutex<Option<Arc<PromptSnapshotHook>>>,
    #[cfg(test)]
    gap_send_hook: Mutex<Option<GapSendHook>>,
    #[cfg(test)]
    admission_confirmation_hook: Mutex<Option<Arc<AdmissionConfirmationHook>>>,
    #[cfg(test)]
    admission_blocking_hook: Mutex<Option<AdmissionBlockingHook>>,
    #[cfg(test)]
    abandoned_sweep_hook: Mutex<Option<AbandonedSweepHook>>,
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
    pub fn open(options: EngineOptions) -> Result<Self, EngineError> {
        let store = SessionStore::open(&options.data_dir, &options.cwd)?;
        let journal = DelegationJournal::open(store.project_dir_path().join("delegations.jsonl"))?;
        let engine = Self {
            inner: Arc::new(Inner {
                config: options.config,
                store,
                journal,
                providers: options.providers,
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
            engine.spawn_actor(session.meta.id);
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

    pub fn create_session(
        &self,
        cwd: impl AsRef<Path>,
        profile: &str,
    ) -> Result<SessionMeta, EngineError> {
        let policy = self
            .inner
            .config
            .materialize_policy(profile)
            .map_err(|error| EngineError::Config(Box::new(error)))?;
        if matches!(
            policy.profile.r#type,
            ConfigAgentType::Subagent | ConfigAgentType::Internal
        ) {
            return Err(EngineError::SubagentOnly(profile.into()));
        }
        let id = SessionId::new_v7();
        let meta = session_meta(id, SessionOrigin::Root, cwd.as_ref(), &policy);
        self.inner.store.create(meta.clone(), policy)?;
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
        profile: &str,
        request_fingerprint: String,
        request: journal::DelegateRequestPayload,
        admission: Option<(InvocationId, u64)>,
    ) -> Result<SessionMeta, EngineError> {
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
        let parent_limit = parent.policy.delegation.depth_limit;
        if !parent.policy.delegation.enabled
            || !parent_limit.allows_delegation()
            || !parent.policy.delegation.allowed_profiles.contains(profile)
        {
            return Err(EngineError::MissingTool("delegate admission denied".into()));
        }
        let child_policy = self
            .inner
            .config
            .materialize_child_policy(profile, &parent.policy)
            .map_err(|error| EngineError::Config(Box::new(error)))?;
        let invocation_id = invocation_id(parent_session_id, parent_run_id, parent_tool_call_id);
        let journal = self.inner.journal.clone();
        let journal_policy = child_policy.clone();
        let entry = self
            .spawn_admission_blocking(move || {
                journal.reserve(
                    invocation_id,
                    parent_session_id,
                    parent_run_id,
                    parent_tool_call_id,
                    journal_policy,
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
            self.ensure_parent_link(
                parent_session_id,
                parent_run_id,
                parent_tool_call_id,
                existing.meta.id,
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
        let meta = session_meta(
            entry.reservation.child_session_id,
            origin,
            Path::new(&parent.meta.cwd),
            &child_policy,
        );
        let store = self.inner.store.clone();
        let creation_meta = meta.clone();
        self.spawn_admission_blocking(move || {
            store.create_with_status(creation_meta, child_policy)
        })
        .await?;
        self.spawn_actor(meta.id);
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
            meta.id,
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
                Event::ToolCallCompleted {
                    tool_call_id: parent_tool_call_id,
                    result: cookie_agent_protocol::ToolResult {
                        content: result.content,
                        truncated: false,
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
        let child_policy = self
            .inner
            .config
            .materialize_child_policy(&invocation.profile, &parent.policy)
            .map_err(|error| EngineError::Config(Box::new(error)))?;
        let fingerprint = serde_json::to_string(&(
            &invocation.profile,
            &invocation.task,
            &invocation.context,
            &invocation.success_criteria,
            &invocation.expected_output,
            &child_policy,
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
                &invocation.profile,
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
        self.publish_admission_child(invocation_id, generation, child.id)?;
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
            child_session_id: child.id,
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
                    let report = child
                        .runs
                        .get(&handle.child_run_id)
                        .and_then(|run| run.final_text.clone())
                        .unwrap_or_else(|| "child completed without a final report".into());
                    let result = bound_delegate_result(
                        report,
                        child.policy.result_limits.delegate_result_bytes,
                    );
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
            Event::ToolCallCompleted {
                tool_call_id,
                result: cookie_agent_protocol::ToolResult {
                    content: result.content,
                    truncated: result.truncated,
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
                            client_run_id,
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
                session.meta.id,
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
        match self.append_direct(session, Some(run_id), Event::RunCancelled { reason }) {
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
                event.run_id == Some(run_id) && matches!(event.event, Event::RunCancelled { .. })
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
                    && known.contains(&(invocation_id, parent_session_id, child.meta.id)) =>
                {
                    Some(ChildSummary {
                        id: child.meta.id,
                        profile: child.meta.profile.name.clone(),
                        task_excerpt: child
                            .runs
                            .values()
                            .min_by_key(|run| run.id.to_string())
                            .map(|run| run.input.chars().take(160).collect()),
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
                .map(|child| self.tree(child.id))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
    pub async fn resume(&self, id: SessionId) -> Result<SessionMeta, EngineError> {
        self.request(id, |reply| SessionCommand::Resume { reply })
            .await
    }
    #[must_use]
    pub fn list_agents(&self) -> AgentListResult {
        AgentListResult {
            agents: self
                .inner
                .config
                .agents
                .iter()
                .filter(|(_, profile)| {
                    profile.enabled
                        && matches!(
                            profile.r#type,
                            ConfigAgentType::Primary | ConfigAgentType::All
                        )
                })
                .map(|(name, profile)| AgentDescriptor {
                    name: name.clone(),
                    agent_type: agent_type(profile.r#type),
                    enabled: profile.enabled,
                    models: profile
                        .models
                        .iter()
                        .map(|model| ModelRef {
                            provider: model.provider.clone(),
                            model: model.model.clone(),
                        })
                        .collect(),
                })
                .collect(),
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
        result: Result<ToolResult, String>,
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
                        event: envelope.clone(),
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
    ) -> Result<Vec<EventEnvelope>, EngineError> {
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
                            matches!(event.event, Event::ToolCallLinked { tool_call_id: linked_call, child_session_id: linked_child }
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
                let result = self
                    .inner
                    .active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&run)
                    .cloned()
                    .filter(|active| active.session == session)
                    .ok_or(EngineError::MissingRun(run))
                    .map(|active| {
                        active.cancellation.cancel();
                        active
                            .stdin
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .clear();
                        RunCancelResult { cancelled: true }
                    });
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
                    let event = match result {
                        Ok(result) => Event::ToolCallCompleted {
                            tool_call_id,
                            result: cookie_agent_protocol::ToolResult {
                                content: result.content,
                                truncated: result.truncated,
                            },
                        },
                        Err(message) => Event::ToolCallFailed {
                            tool_call_id,
                            message,
                        },
                    };
                    self.append_direct(session, Some(run), event).map(|()| true)
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
                                    && matches!(event.event, Event::UserInputSubmitted { .. })
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
                            .filter_map(|event| match &event.event {
                                Event::UserInputApplied { user_input_seq }
                                    if event.run_id == Some(run) =>
                                {
                                    Some(*user_input_seq)
                                }
                                _ => None,
                            })
                            .collect();
                        for user_input_seq in events.iter().filter_map(|event| match &event.event {
                            Event::UserInputSubmitted { .. }
                                if event.run_id == Some(run) && !applied.contains(&event.seq) =>
                            {
                                Some(event.seq)
                            }
                            _ => None,
                        }) {
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
            if run.input != params.input {
                return Err(EngineError::RunIdempotencyConflict);
            }
            return Ok(RunStartResult { run_id: run.id });
        }
        if session.status == SessionStatus::Running {
            return Err(EngineError::SessionRunning(params.session_id));
        }
        self.resolve_interrupted_direct(params.session_id).await?;
        let run_id = RunId::new_v7();
        self.append_direct(
            params.session_id,
            Some(run_id),
            Event::RunStarted {
                client_run_id: params.client_run_id,
                input: params.input,
            },
        )?;
        let active = Arc::new(ActiveRun {
            session: params.session_id,
            cancellation: CancellationToken::new(),
            cancelled_committed: Mutex::new(false),
            stdin: Mutex::new(HashMap::new()),
            prompt_seq: AtomicU64::new(0),
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
            let session = self.inner.store.get(active.session)?;
            let tools = self.tool_definitions(active.session)?;
            let chain: Vec<_> = session
                .policy
                .models
                .iter()
                .map(|model| ProviderModelRef {
                    provider: model.provider.clone(),
                    model: ModelId(model.model.clone()),
                })
                .collect();
            let prompt_events = self.prompt_events(active.session, run_id).await?;
            let attempt = match self
                .stream_attempt(
                    active.session,
                    run_id,
                    &active.cancellation,
                    &chain,
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
                            message: error.to_string(),
                        },
                    )
                    .await?;
                    return Ok(());
                }
            };
            let mut calls: Vec<(ToolCallId, String, String)> = Vec::new();
            let mut args: HashMap<String, String> = HashMap::new();
            let mut final_text = String::new();
            let mut tool_use = false;
            let attempt_protocol = attempt.protocol;
            for event in attempt.events {
                match event {
                    NormalizedEvent::TextDelta { text } => final_text.push_str(&text),
                    NormalizedEvent::ReasoningDelta { .. } => {}
                    NormalizedEvent::ToolCallStart { tool_call_id, tool } => {
                        // Provider IDs are transport-local correlation keys only.
                        // Persisted invocation IDs are allocated by the engine.
                        let id = ToolCallId::new_v7();
                        args.insert(tool_call_id.clone(), String::new());
                        calls.push((id, tool_call_id, tool));
                        tool_use = true;
                    }
                    NormalizedEvent::ToolArgsDelta {
                        tool_call_id,
                        delta,
                    } => args.entry(tool_call_id).or_default().push_str(&delta),
                    NormalizedEvent::ToolCallEnd { .. } => {}
                    NormalizedEvent::Usage { .. } => {}
                    NormalizedEvent::TurnOpaque { .. } => {}
                    NormalizedEvent::Stop { reason } => {
                        if reason == StopReason::Cancelled {
                            active.cancellation.cancel();
                        }
                    }
                }
            }
            if active.cancellation.is_cancelled() {
                self.append_run_cancelled_once(&active, run_id, None)?;
                return Ok(());
            }
            if !tool_use {
                let steering = self
                    .request(active.session, |reply| {
                        SessionCommand::CompleteIfNoSteering {
                            run: run_id,
                            final_text: (!final_text.is_empty()).then_some(final_text),
                            reply,
                        }
                    })
                    .await?;
                if steering {
                    continue;
                }
                return Ok(());
            }
            let mut tasks = Vec::new();
            for (id, raw_id, tool) in &calls {
                let arguments =
                    serde_json::from_str(args.get(raw_id).map(String::as_str).unwrap_or("{}"))
                        .unwrap_or(Value::Object(Default::default()));
                self.inner
                    .output_hubs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .entry(*id)
                    .or_insert_with(|| OutputHub::new(*id, 64 * 1024));
                self.append(
                    active.session,
                    Some(run_id),
                    Event::ToolCallStarted {
                        tool_call_id: *id,
                        tool: tool.clone(),
                        arguments: arguments.clone(),
                        provider_tool_call_id: attempt_protocol.map(|_| raw_id.clone()),
                        provider_protocol: attempt_protocol.map(wire_provider_protocol),
                    },
                )
                .await?;
                tasks.push(self.spawn_tool(
                    active.clone(),
                    run_id,
                    ToolCall {
                        id: *id,
                        name: tool.clone(),
                        arguments,
                    },
                ));
            }
            // Awaiting task handles is outside any session actor. Results are
            // committed in provider tool-call order, regardless of completion order.
            for (id, task) in calls.iter().map(|call| call.0).zip(tasks) {
                if active.cancellation.is_cancelled() {
                    self.append_run_cancelled_once(&active, run_id, None)?;
                    return Ok(());
                }
                let task_result = task.await;
                if active.cancellation.is_cancelled() {
                    self.append_run_cancelled_once(&active, run_id, None)?;
                    return Ok(());
                }
                let result = match task_result {
                    Ok(result) => result,
                    Err(error) => Err(error.to_string()),
                };
                self.submit_tool_result_status(active.session, run_id, id, result)
                    .await?;
            }
        }
    }

    /// Streams one model attempt directly into the session actor.  The event
    /// vector is retained only for the current attempt so a failed fallback
    /// never contributes partial output to the next attempt's tool handling.
    #[allow(clippy::too_many_arguments)]
    async fn stream_attempt(
        &self,
        session: SessionId,
        run: RunId,
        cancellation: &CancellationToken,
        chain: &[ProviderModelRef],
        sticky_entry: &mut usize,
        prompt_events: Vec<EventEnvelope>,
        tools: Vec<ToolDefinition>,
    ) -> Result<AttemptEvents, ProviderError> {
        let mut entry = *sticky_entry;
        let mut last_error = ProviderError::EntryTerminal {
            message: "model fallback chain is empty".into(),
        };
        let mut first_request = true;
        while entry < chain.len() {
            let model = &chain[entry];
            let Some(provider) = self.inner.providers.get(&model.provider) else {
                last_error = ProviderError::EntryTerminal {
                    message: format!("provider '{}' is not registered", model.provider),
                };
                if let Some(next) = chain.get(entry + 1) {
                    self.append(
                        session,
                        Some(run),
                        Event::ModelFallback {
                            from: wire_model(model),
                            to: wire_model(next),
                            reason: last_error.to_string(),
                            attempts: 0,
                        },
                    )
                    .await
                    .map_err(provider_persistence_error)?;
                    entry += 1;
                    *sticky_entry = entry;
                    first_request = false;
                    continue;
                }
                return Err(last_error);
            };
            let mut attempts = 0;
            loop {
                let request_events = if first_request {
                    first_request = false;
                    prompt_events.clone()
                } else {
                    self.prompt_events(session, run).await.map_err(|error| {
                        ProviderError::RunTerminal {
                            message: error.to_string(),
                        }
                    })?
                };
                let protocol = provider.protocol(&model.model);
                let persisted_turns = assemble_persisted_turns(&request_events, protocol);
                let request = ProviderRequest {
                    model: model.model.clone(),
                    messages: persisted_turns
                        .iter()
                        .map(|turn| turn.message.clone())
                        .collect(),
                    persisted_turns,
                    tools: tools.clone(),
                    ..ProviderRequest::default()
                };
                let stream = tokio::select! {
                    result = provider.stream(request) => result,
                    _ = cancellation.cancelled() => Err(ProviderError::RunTerminal { message: "cancelled".into() }),
                };
                let (result, meaningful_output) = match stream {
                    Ok(mut stream) => {
                        let mut events = Vec::new();
                        let mut failure = None;
                        let mut meaningful_output = false;
                        loop {
                            let item = tokio::select! {
                                item = stream.next() => item,
                                _ = cancellation.cancelled() => {
                                    failure = Some(ProviderError::RunTerminal { message: "cancelled".into() });
                                    break;
                                },
                            };
                            let Some(item) = item else { break };
                            match item {
                                Ok(event) => {
                                    meaningful_output |= is_meaningful_output(&event);
                                    match &event {
                                        NormalizedEvent::TextDelta { text } => {
                                            self.append(
                                                session,
                                                Some(run),
                                                Event::TextDelta { text: text.clone() },
                                            )
                                            .await
                                            .map_err(provider_persistence_error)?;
                                        }
                                        NormalizedEvent::ReasoningDelta { text } => {
                                            self.append(
                                                session,
                                                Some(run),
                                                Event::ReasoningDelta { text: text.clone() },
                                            )
                                            .await
                                            .map_err(provider_persistence_error)?;
                                        }
                                        NormalizedEvent::Usage {
                                            input_tokens,
                                            output_tokens,
                                            cache_read_tokens,
                                        } => {
                                            self.append(
                                                session,
                                                Some(run),
                                                Event::UsageReported {
                                                    model: wire_model(model),
                                                    usage: cookie_agent_protocol::Usage {
                                                        input_tokens: *input_tokens,
                                                        output_tokens: *output_tokens,
                                                        cached_input_tokens: Some(
                                                            *cache_read_tokens,
                                                        ),
                                                    },
                                                },
                                            )
                                            .await
                                            .map_err(provider_persistence_error)?;
                                        }
                                        NormalizedEvent::TurnOpaque { state } => {
                                            self.append(
                                                session,
                                                Some(run),
                                                Event::TurnOpaque {
                                                    state: TurnOpaque {
                                                        provider: wire_provider_protocol(
                                                            state.provider,
                                                        ),
                                                        payload: state.payload.clone(),
                                                    },
                                                },
                                            )
                                            .await
                                            .map_err(provider_persistence_error)?;
                                        }
                                        _ => {}
                                    }
                                    events.push(event);
                                }
                                Err(error) => {
                                    failure = Some(error);
                                    break;
                                }
                            }
                        }
                        (failure.map_or(Ok(events), Err), meaningful_output)
                    }
                    Err(error) => (Err(error), false),
                };
                match result {
                    Ok(events) => {
                        return Ok(AttemptEvents { events, protocol });
                    }
                    Err(error) if error.class() == ProviderErrorClass::RunTerminal => {
                        self.append(session, Some(run), Event::AttemptAbandoned)
                            .await
                            .map_err(provider_persistence_error)?;
                        return Err(error);
                    }
                    Err(error)
                        if error.class() == ProviderErrorClass::EntryRetryable
                            && attempts < 2
                            && !meaningful_output =>
                    {
                        self.append(session, Some(run), Event::AttemptAbandoned)
                            .await
                            .map_err(provider_persistence_error)?;
                        attempts += 1;
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_millis(100_u64 << (attempts - 1))) => {}
                            _ = cancellation.cancelled() => return Err(ProviderError::RunTerminal { message: "cancelled".into() }),
                        }
                    }
                    Err(error) => {
                        self.append(session, Some(run), Event::AttemptAbandoned)
                            .await
                            .map_err(provider_persistence_error)?;
                        last_error = error;
                        break;
                    }
                }
            }
            let Some(next) = chain.get(entry + 1) else {
                return Err(last_error);
            };
            self.append(
                session,
                Some(run),
                Event::ModelFallback {
                    from: wire_model(model),
                    to: wire_model(next),
                    reason: last_error.to_string(),
                    attempts: attempts + 1,
                },
            )
            .await
            .map_err(provider_persistence_error)?;
            entry += 1;
            *sticky_entry = entry;
        }
        Err(last_error)
    }

    fn spawn_tool(
        &self,
        active: Arc<ActiveRun>,
        run: RunId,
        call: ToolCall,
    ) -> JoinHandle<Result<ToolResult, String>> {
        let engine = self.clone();
        tokio::spawn(async move {
            let session = match engine.inner.store.get(active.session) {
                Ok(session) => session,
                Err(error) => return Err(error.to_string()),
            };
            let delegate_enabled = session.policy.delegation.enabled
                && session.policy.delegation.depth_limit.allows_delegation()
                && !session.policy.delegation.allowed_profiles.is_empty();
            if (call.name == "delegate" && !delegate_enabled)
                || (call.name != "delegate" && !session.policy.tools.contains(&call.name))
            {
                return Err(format!(
                    "tool `{}` is not enabled for this session",
                    call.name
                ));
            }
            let action = PermissionPipeline::action_for_tool(&call.name)
                .map_err(|error| error.to_string())?;
            let root = root_id(&session.meta.origin, active.session);
            let raw_resource = resource_for(&call);
            let resources = if action == cookie_agent_protocol::ActionKind::Bash {
                permissions::bash_subcommands(&raw_resource)
            } else if matches!(
                action,
                cookie_agent_protocol::ActionKind::Read
                    | cookie_agent_protocol::ActionKind::Write
                    | cookie_agent_protocol::ActionKind::List
            ) {
                permissions::canonical_resource(
                    Path::new(&session.meta.cwd),
                    Path::new(&raw_resource),
                )
                .map(|(resource, external)| {
                    if external {
                        vec![format!("external:{resource}"), resource]
                    } else {
                        vec![resource]
                    }
                })
                .unwrap_or_else(|_| vec![format!("external:{raw_resource}"), raw_resource])
            } else {
                vec![raw_resource]
            };
            let resources = resources
                .into_iter()
                .map(|resource| match resource.strip_prefix("external:") {
                    Some(resource) => (
                        cookie_agent_protocol::ActionKind::ExternalDirectory,
                        resource.to_owned(),
                    ),
                    None => (action, resource),
                })
                .collect();
            let permission = engine.inner.permissions.decide_resources(
                &session.policy,
                &engine.inner.approvals,
                root,
                active.session,
                resources,
            );
            let resource = permission.trace.normalized_resource.clone();
            if permission.effect != cookie_agent_protocol::Effect::Allow {
                if permission.effect == cookie_agent_protocol::Effect::Ask {
                    let approval_id = format!("{}:{}", run, call.id);
                    let suggested_pattern = permission
                        .asking_resources
                        .first()
                        .map(|resource| resource.suggested_pattern.clone())
                        .unwrap_or_else(|| format!("{resource} *"));
                    let (approval_tx, approval_rx) = oneshot::channel();
                    engine
                        .inner
                        .pending_approvals
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(approval_id.clone(), approval_tx);
                    if engine
                        .append(
                            active.session,
                            Some(run),
                            Event::ApprovalRequested {
                                approval_id: approval_id.clone(),
                                action: permission.trace.action,
                                resource: resource.clone(),
                                suggested_pattern,
                                resources: permission.asking_resources.clone(),
                                decision_trace: permission.trace,
                            },
                        )
                        .await
                        .is_err()
                    {
                        engine
                            .inner
                            .pending_approvals
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&approval_id);
                        return Err("could not persist approval request".into());
                    }
                    let decision = tokio::select! {
                        decision = approval_rx => decision.map_err(|_| "approval request was abandoned".to_owned())?,
                        _ = active.cancellation.cancelled() => {
                            engine.inner.pending_approvals.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&approval_id);
                            return Err("cancelled".into());
                        }
                    };
                    if matches!(decision, ApprovalDecision::Once | ApprovalDecision::Always) {
                        // The actual approval is persisted by approval_respond;
                        // this task merely resumes after the actor releases it.
                    } else {
                        return Err("permission refused by user".into());
                    }
                } else {
                    return Err("permission denied".into());
                }
            }
            let providers = engine
                .inner
                .tools
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let provider = providers
                .iter()
                .find(|provider| {
                    provider
                        .tools_for_session(&SessionToolContext {
                            session: active.session,
                        })
                        .ok()
                        .is_some_and(|tools| tools.iter().any(|tool| tool.name == call.name))
                })
                .cloned()
                .ok_or_else(|| EngineError::MissingTool(call.name.clone()).to_string())?;
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
            let (stdin_tx, stdin) = ToolStdin::channel(64);
            if interactive {
                active
                    .stdin
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(call.id, stdin_tx);
            }
            let invoke = provider.invoke(
                ToolInvocationContext {
                    session: active.session,
                    run,
                    cwd: resolved_session_cwd(&session.meta.cwd),
                    workspace_root: resolved_session_cwd(&session.meta.cwd),
                    progress: ProgressSink::new(progress_tx, hub.clone()),
                    cancellation: active.cancellation.child_token(),
                    stdin: interactive.then_some(stdin),
                },
                call.clone(),
            );
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
                        return result.map(bound_tool_result).map_err(|error| error.to_string());
                    }
                    Some(progress) = progress_rx.recv() => {
                        let _ = engine.append(active.session, Some(run), Event::ToolCallProgress { tool_call_id: progress.tool_call_id, message: progress.message }).await;
                    }
                    _ = active.cancellation.cancelled() => {
                        active
                            .stdin
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&call.id);
                        hub.finalize();
                        engine.retain_finalized_output_hub(call.id);
                        return Err("cancelled".into());
                    }
                }
            }
        })
    }

    pub async fn approval_respond(
        &self,
        session: SessionId,
        approval_id: String,
        decision: ApprovalDecision,
        scope: Option<String>,
        feedback: Option<String>,
    ) -> Result<ApprovalRespondResult, EngineError> {
        let projection = self.inner.store.get(session)?;
        let requested = projection
            .log
            .events()
            .into_iter()
            .find_map(|event| match event.event {
                Event::ApprovalRequested {
                    approval_id: ref id,
                    action,
                    ref resource,
                    ref suggested_pattern,
                    ref resources,
                    ref decision_trace,
                    ..
                } if *id == approval_id => Some((
                    action,
                    resource.clone(),
                    suggested_pattern.clone(),
                    resources.clone(),
                    decision_trace
                        .precedence_reason
                        .starts_with("doom-loop guard"),
                )),
                _ => None,
            });
        let decision = if requested
            .as_ref()
            .is_some_and(|(_, _, _, _, doom_loop)| *doom_loop)
            && decision == ApprovalDecision::Always
        {
            ApprovalDecision::Reject
        } else {
            decision
        };
        let effective_scope = requested.as_ref().map(|(_, _, suggested_pattern, _, _)| {
            scope.clone().unwrap_or_else(|| suggested_pattern.clone())
        });
        let mut approved_scopes = Vec::new();
        if let Some((action, primary_resource, _, resources, _)) = requested
            && decision == ApprovalDecision::Always
        {
            for resource in resources {
                let scope = if resource.action == action && resource.resource == primary_resource {
                    effective_scope
                        .clone()
                        .expect("requested approval has scope")
                } else {
                    resource.suggested_pattern
                };
                self.inner.approvals.grant(
                    root_id(&projection.meta.origin, session),
                    resource.action,
                    scope.clone(),
                );
                approved_scopes.push(ApprovedScope {
                    action: resource.action,
                    resource: resource.resource,
                    scope,
                });
            }
        }
        self.append(
            session,
            None,
            Event::ApprovalResolved {
                approval_id: approval_id.clone(),
                decision,
                approved_scope: (decision == ApprovalDecision::Always)
                    .then_some(effective_scope)
                    .flatten(),
                approved_scopes,
                feedback,
            },
        )
        .await?;
        if let Some(sender) = self
            .inner
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&approval_id)
        {
            let _ = sender.send(decision);
        }
        Ok(ApprovalRespondResult {
            approval_id,
            decision,
        })
    }

    fn tool_definitions(&self, session: SessionId) -> Result<Vec<ToolDefinition>, EngineError> {
        let policy = self.inner.store.get(session)?.policy;
        let delegate_enabled = policy.delegation.enabled
            && policy.delegation.depth_limit.allows_delegation()
            && !policy.delegation.allowed_profiles.is_empty();
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
                if ((tool.name != "delegate" && policy.tools.contains(&tool.name))
                    || (tool.name == "delegate" && delegate_enabled))
                    && names.insert(tool.name.clone())
                {
                    output.push(ToolDefinition {
                        name: tool.name,
                        description: tool.description,
                        input_schema: tool.parameters,
                    });
                }
            }
        }
        Ok(output)
    }

    fn reconcile(&self) -> Result<(), EngineError> {
        // Every active run from a previous process is terminally interrupted.
        for session in self.inner.store.all() {
            for run in session
                .runs
                .values()
                .filter(|run| run.status == SessionStatus::Running)
            {
                self.append_blocking(
                    session.meta.id,
                    Some(run.id),
                    Event::RunInterrupted {
                        reason: Some("daemon restart".into()),
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
                                reason: Some("parent delegate run was cancelled".into()),
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
                if session.runs.is_empty() {
                    let orphan_run = RunId::new_v7();
                    self.append_blocking(
                        session.meta.id,
                        Some(orphan_run),
                        Event::RunStarted {
                            client_run_id: format!("orphan:{invocation_id}"),
                            input: "orphaned delegated session".into(),
                        },
                    )?;
                    self.append_blocking(
                        session.meta.id,
                        Some(orphan_run),
                        Event::RunInterrupted {
                            reason: Some(
                                "orphaned delegated session without journal reservation".into(),
                            ),
                        },
                    )?;
                } else {
                    for run in session
                        .runs
                        .values()
                        .filter(|run| run.status != SessionStatus::Interrupted)
                    {
                        self.append_blocking(
                            session.meta.id,
                            Some(run.id),
                            Event::RunInterrupted {
                                reason: Some(
                                    "orphaned delegated session without journal reservation".into(),
                                ),
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
                        self.append_direct(
                            session_id,
                            Some(run.id),
                            Event::ToolCallCompleted {
                                tool_call_id: *call,
                                result: cookie_agent_protocol::ToolResult {
                                    content: delegate_failure_result(
                                        None,
                                        "delegate interrupted by daemon restart: no durable reservation",
                                    )
                                    .content,
                                    truncated: false,
                                },
                            },
                        )?;
                        continue;
                    };
                    let child_id = entry.reservation.child_session_id;
                    if run.status == SessionStatus::Cancelled {
                        let result = cancelled_delegate_result_with_reason(
                            Some(child_id),
                            "parent delegate run was cancelled",
                        );
                        self.append_direct(
                            session_id,
                            Some(run.id),
                            Event::ToolCallCompleted {
                                tool_call_id: *call,
                                result: cookie_agent_protocol::ToolResult {
                                    content: result.content,
                                    truncated: false,
                                },
                            },
                        )?;
                        continue;
                    }
                    let child = match self.inner.store.get(child_id) {
                        Ok(child) => child,
                        Err(_) => {
                            self.append_direct(
                                session_id,
                                Some(run.id),
                                Event::ToolCallCompleted {
                                    tool_call_id: *call,
                                    result: cookie_agent_protocol::ToolResult {
                                        content: delegate_failure_result(
                                            Some(child_id),
                                            "delegate child session is missing",
                                        )
                                        .content,
                                        truncated: false,
                                    },
                                },
                            )?;
                            continue;
                        }
                    };
                    if child.status == SessionStatus::Completed {
                        let report = entry
                            .child_run_id
                            .and_then(|run_id| child.runs.get(&run_id))
                            .and_then(|child_run| child_run.final_text.clone())
                            .unwrap_or_else(|| "child completed without a final report".into());
                        let result = bound_delegate_result(
                            report,
                            child.policy.result_limits.delegate_result_bytes,
                        );
                        self.append_direct(
                            session_id,
                            Some(run.id),
                            Event::ToolCallCompleted {
                                tool_call_id: *call,
                                result: cookie_agent_protocol::ToolResult {
                                    content: result.content,
                                    truncated: result.truncated,
                                },
                            },
                        )?;
                    } else if child.status == SessionStatus::Cancelled {
                        let result = cancelled_delegate_result(child_id, None);
                        self.append_direct(
                            session_id,
                            Some(run.id),
                            Event::ToolCallCompleted {
                                tool_call_id: *call,
                                result: cookie_agent_protocol::ToolResult {
                                    content: result.content,
                                    truncated: result.truncated,
                                },
                            },
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
                        self.append_direct(
                            session_id,
                            Some(run.id),
                            Event::ToolCallCompleted {
                                tool_call_id: *call,
                                result: cookie_agent_protocol::ToolResult {
                                    content: delegate_failure_result(
                                        Some(child_id),
                                        "delegate child interrupted by daemon restart",
                                    )
                                    .content,
                                    truncated: false,
                                },
                            },
                        )?;
                    }
                } else {
                    self.append_direct(
                        session_id,
                        Some(run.id),
                        Event::ToolCallFailed {
                            tool_call_id: *call,
                            message: "interrupted by daemon restart".into(),
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    fn rebuild_approvals(&self) {
        for session in self.inner.store.all() {
            let mut pending = HashMap::new();
            for envelope in session.log.events() {
                match envelope.event {
                    Event::ApprovalRequested {
                        approval_id,
                        action,
                        suggested_pattern,
                        resources,
                        ..
                    } => {
                        pending.insert(approval_id, (action, suggested_pattern, resources));
                    }
                    Event::ApprovalResolved {
                        approval_id,
                        decision: ApprovalDecision::Always,
                        approved_scope,
                        approved_scopes,
                        ..
                    } => {
                        if !approved_scopes.is_empty() {
                            for scope in approved_scopes {
                                self.inner.approvals.grant(
                                    root_id(&session.meta.origin, session.meta.id),
                                    scope.action,
                                    scope.scope,
                                );
                            }
                        } else if let Some((action, suggested_pattern, _)) =
                            pending.get(&approval_id)
                        {
                            self.inner.approvals.grant(
                                root_id(&session.meta.origin, session.meta.id),
                                *action,
                                approved_scope.unwrap_or_else(|| suggested_pattern.clone()),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn session_meta(
    id: SessionId,
    origin: SessionOrigin,
    cwd: &Path,
    policy: &PolicySnapshot,
) -> SessionMeta {
    let profile = cookie_agent_protocol::ProfileSnapshot {
        name: policy.profile.name.clone(),
        agent_type: agent_type(policy.profile.r#type),
        models: policy
            .models
            .iter()
            .map(|model| ModelRef {
                provider: model.provider.clone(),
                model: model.model.clone(),
            })
            .collect(),
        tools: policy.tools.iter().cloned().collect(),
        delegation: cookie_agent_protocol::DelegationSnapshot {
            enabled: policy.delegation.enabled,
            allowed_profiles: policy.delegation.allowed_profiles.iter().cloned().collect(),
            depth_limit: depth(policy.delegation.depth_limit),
            result_limit_bytes: policy.result_limits.delegate_result_bytes as u64,
        },
        permission_rules: policy
            .permissions
            .rules
            .iter()
            .filter_map(|rule| {
                PermissionPipeline::action_for_tool(&rule.action)
                    .ok()
                    .map(|action| cookie_agent_protocol::PermissionRule {
                        id: rule.id.clone(),
                        action,
                        resource: rule.resource.clone(),
                        effect: match rule.effect.as_str() {
                            "allow" => cookie_agent_protocol::Effect::Allow,
                            "deny" => cookie_agent_protocol::Effect::Deny,
                            _ => cookie_agent_protocol::Effect::Ask,
                        },
                        hard: rule.hard,
                    })
            })
            .collect(),
    };
    SessionMeta {
        id,
        origin,
        cwd: cwd.to_string_lossy().into_owned(),
        profile,
    }
}
fn agent_type(value: ConfigAgentType) -> AgentType {
    match value {
        ConfigAgentType::All => AgentType::All,
        ConfigAgentType::Primary => AgentType::Primary,
        ConfigAgentType::Subagent => AgentType::SubAgent,
        ConfigAgentType::Internal => AgentType::Internal,
    }
}
fn depth(value: ConfigDepthLimit) -> cookie_agent_protocol::DepthLimit {
    match value {
        ConfigDepthLimit::Finite(value) => cookie_agent_protocol::DepthLimit::Finite(value),
        ConfigDepthLimit::Unlimited => cookie_agent_protocol::DepthLimit::Unlimited,
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

fn resolved_session_cwd(cwd: &str) -> PathBuf {
    let cwd = PathBuf::from(cwd);
    cwd.canonicalize().unwrap_or(cwd)
}

fn wire_model(model: &ProviderModelRef) -> ModelRef {
    ModelRef {
        provider: model.provider.clone(),
        model: model.model.0.clone(),
    }
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
fn resource_for(call: &ToolCall) -> String {
    call.arguments
        .get("regex")
        .or_else(|| call.arguments.get("pattern"))
        .or_else(|| call.arguments.get("path"))
        .or_else(|| call.arguments.get("command"))
        .or_else(|| call.arguments.get("profile"))
        .and_then(Value::as_str)
        .unwrap_or(&call.name)
        .to_owned()
}

fn bound_tool_result(mut result: ToolResult) -> ToolResult {
    const MODEL_RESULT_LIMIT: usize = 32 * 1024;
    if result.content.len() > MODEL_RESULT_LIMIT {
        result.content.truncate(MODEL_RESULT_LIMIT);
        result.truncated = true;
    }
    result
}

fn delegate_client_run_id(invocation_id: InvocationId) -> String {
    format!("delegate:{invocation_id}")
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

fn bound_delegate_result(content: String, limit: usize) -> ToolResult {
    if content.len() <= limit {
        return ToolResult {
            content,
            truncated: false,
        };
    }
    let mut boundary = limit.min(content.len());
    while boundary > 0 && !content.is_char_boundary(boundary) {
        boundary -= 1;
    }
    ToolResult {
        content: content[..boundary].to_owned(),
        truncated: true,
    }
}

fn cancelled_delegate_result(
    child_session_id: SessionId,
    partial_report: Option<String>,
) -> ToolResult {
    ToolResult {
        content: serde_json::json!({
            "status": "cancelled",
            "child_session_id": child_session_id,
            "partial_report": partial_report,
        })
        .to_string(),
        truncated: false,
    }
}

fn cancelled_delegate_result_with_reason(
    child_session_id: Option<SessionId>,
    reason: &str,
) -> ToolResult {
    ToolResult {
        content: serde_json::json!({
            "status": "cancelled",
            "child_session_id": child_session_id,
            "reason": reason,
        })
        .to_string(),
        truncated: false,
    }
}

fn delegate_failure_result(child_session_id: Option<SessionId>, reason: &str) -> ToolResult {
    ToolResult {
        content: serde_json::json!({
            "status": "failed",
            "child_session_id": child_session_id,
            "reason": reason,
        })
        .to_string(),
        truncated: false,
    }
}

fn is_journal_append_failure(error: &EngineError) -> bool {
    matches!(
        error,
        EngineError::Journal(
            JournalError::Event(_) | JournalError::Poisoned | JournalError::Stopped
        ) | EngineError::ActorStopped
    )
}

fn wire_provider_protocol(protocol: ProviderProtocol) -> cookie_agent_protocol::ProviderProtocol {
    match protocol {
        ProviderProtocol::AnthropicMessages => {
            cookie_agent_protocol::ProviderProtocol::AnthropicMessages
        }
        ProviderProtocol::OpenAiChatCompletions => {
            cookie_agent_protocol::ProviderProtocol::OpenAiChatCompletions
        }
        ProviderProtocol::OpenAiResponses => {
            cookie_agent_protocol::ProviderProtocol::OpenAiResponses
        }
        ProviderProtocol::OpenAiCompatible => {
            cookie_agent_protocol::ProviderProtocol::OpenAiCompatible
        }
    }
}

fn provider_protocol(protocol: cookie_agent_protocol::ProviderProtocol) -> ProviderProtocol {
    match protocol {
        cookie_agent_protocol::ProviderProtocol::AnthropicMessages => {
            ProviderProtocol::AnthropicMessages
        }
        cookie_agent_protocol::ProviderProtocol::OpenAiChatCompletions => {
            ProviderProtocol::OpenAiChatCompletions
        }
        cookie_agent_protocol::ProviderProtocol::OpenAiResponses => {
            ProviderProtocol::OpenAiResponses
        }
        cookie_agent_protocol::ProviderProtocol::OpenAiCompatible => {
            ProviderProtocol::OpenAiCompatible
        }
    }
}

fn is_meaningful_output(event: &NormalizedEvent) -> bool {
    matches!(
        event,
        NormalizedEvent::TextDelta { .. }
            | NormalizedEvent::ReasoningDelta { .. }
            | NormalizedEvent::ToolCallStart { .. }
            | NormalizedEvent::ToolArgsDelta { .. }
            | NormalizedEvent::ToolCallEnd { .. }
    )
}

fn provider_persistence_error(error: EngineError) -> ProviderError {
    ProviderError::RunTerminal {
        message: format!("could not persist provider attempt state: {error}"),
    }
}

#[cfg(test)]
fn assemble_messages(events: &[EventEnvelope]) -> Vec<ProviderMessage> {
    assemble_persisted_turns(events, None)
        .into_iter()
        .map(|turn| turn.message)
        .collect()
}

fn assemble_persisted_turns(
    events: &[EventEnvelope],
    target_protocol: Option<ProviderProtocol>,
) -> Vec<cookie_agent_providers::PersistedTurn> {
    let mut output = Vec::new();
    let mut assistant_text = String::new();
    let mut assistant_calls = Vec::new();
    let mut assistant_call_ids: Vec<(ToolCallId, RunId)> = Vec::new();
    let mut assistant_opaque = None;
    let mut assistant_opaque_usable = true;
    let mut pending_steering = HashMap::new();
    let mut segment = 0_u64;
    let mut emitted_calls = Vec::new();
    let mut pending_calls: HashMap<(ToolCallId, RunId), VecDeque<usize>> = HashMap::new();
    let flush_assistant =
        |output: &mut Vec<cookie_agent_providers::PersistedTurn>,
         text: &mut String,
         calls: &mut Vec<cookie_agent_providers::ToolCall>,
         call_ids: &mut Vec<(ToolCallId, RunId)>,
         opaque: &mut Option<TurnOpaque>,
         opaque_usable: &mut bool,
         segment: u64,
         emitted_calls: &mut Vec<EmittedToolCall>,
         pending_calls: &mut HashMap<(ToolCallId, RunId), VecDeque<usize>>| {
            if !text.is_empty() || !calls.is_empty() || opaque.is_some() {
                let emitted_ids = std::mem::take(call_ids);
                let emitted_provider_ids: Vec<_> =
                    calls.iter().map(|call| call.id.clone()).collect();
                let assistant_index = output.len();
                let content = if !text.is_empty() {
                    vec![ContentPart::Text {
                        text: std::mem::take(text),
                    }]
                } else {
                    Vec::new()
                };
                output.push(cookie_agent_providers::PersistedTurn {
                    message: ProviderMessage::Assistant {
                        content,
                        tool_calls: std::mem::take(calls),
                    },
                    opaque: if *opaque_usable {
                        opaque
                            .take()
                            .map(|state| cookie_agent_providers::AssistantTurnOpaque {
                                provider: provider_protocol(state.provider),
                                payload: state.payload,
                            })
                    } else {
                        let _ = opaque.take();
                        None
                    },
                });
                for (call_index, ((tool_call_id, run_id), provider_tool_call_id)) in emitted_ids
                    .into_iter()
                    .zip(emitted_provider_ids)
                    .enumerate()
                {
                    let emitted_index = emitted_calls.len();
                    emitted_calls.push(EmittedToolCall {
                        assistant_index,
                        call_index,
                        segment,
                        run_id,
                        provider_tool_call_id,
                        result: None,
                    });
                    pending_calls
                        .entry((tool_call_id, run_id))
                        .or_default()
                        .push_back(emitted_index);
                }
                *opaque_usable = true;
            }
        };
    for event in events {
        match &event.event {
            Event::RunStarted { input, .. } => {
                flush_assistant(
                    &mut output,
                    &mut assistant_text,
                    &mut assistant_calls,
                    &mut assistant_call_ids,
                    &mut assistant_opaque,
                    &mut assistant_opaque_usable,
                    segment,
                    &mut emitted_calls,
                    &mut pending_calls,
                );
                segment += 1;
                output.push(cookie_agent_providers::PersistedTurn {
                    message: ProviderMessage::User {
                        content: vec![ContentPart::Text {
                            text: input.clone(),
                        }],
                    },
                    opaque: None,
                });
            }
            Event::UserInputSubmitted { input } => {
                pending_steering.insert(event.seq, input.clone());
            }
            Event::UserInputApplied { user_input_seq } => {
                if let Some(input) = pending_steering.remove(user_input_seq) {
                    flush_assistant(
                        &mut output,
                        &mut assistant_text,
                        &mut assistant_calls,
                        &mut assistant_call_ids,
                        &mut assistant_opaque,
                        &mut assistant_opaque_usable,
                        segment,
                        &mut emitted_calls,
                        &mut pending_calls,
                    );
                    output.push(cookie_agent_providers::PersistedTurn {
                        message: ProviderMessage::User {
                            content: vec![ContentPart::Text { text: input }],
                        },
                        opaque: None,
                    });
                }
            }
            Event::TextDelta { text } => assistant_text.push_str(text),
            Event::ToolCallStarted {
                tool_call_id,
                tool,
                arguments,
                provider_tool_call_id,
                provider_protocol: call_protocol,
            } => {
                let Some(run_id) = event.run_id else {
                    assistant_opaque_usable = false;
                    continue;
                };
                let canonical_id = tool_call_id.to_string();
                let provider_id = provider_tool_call_id
                    .as_ref()
                    .filter(|_| {
                        matches!(
                            (call_protocol.map(provider_protocol), target_protocol),
                            (Some(call_protocol), Some(target_protocol))
                                if call_protocol == target_protocol
                        )
                    })
                    .cloned()
                    .unwrap_or_else(|| canonical_id.clone());
                assistant_call_ids.push((*tool_call_id, run_id));
                assistant_calls.push(cookie_agent_providers::ToolCall {
                    id: provider_id,
                    name: tool.clone(),
                    arguments: arguments.clone(),
                });
            }
            Event::AttemptAbandoned => {
                assistant_text.clear();
                assistant_calls.clear();
                assistant_call_ids.clear();
                assistant_opaque = None;
                assistant_opaque_usable = true;
                segment += 1;
            }
            Event::TurnOpaque { state } => assistant_opaque = Some(state.clone()),
            Event::ToolCallCompleted {
                tool_call_id,
                result,
            } => {
                let Some(run_id) = event.run_id else {
                    continue;
                };
                if assistant_call_ids.contains(&(*tool_call_id, run_id)) {
                    flush_assistant(
                        &mut output,
                        &mut assistant_text,
                        &mut assistant_calls,
                        &mut assistant_call_ids,
                        &mut assistant_opaque,
                        &mut assistant_opaque_usable,
                        segment,
                        &mut emitted_calls,
                        &mut pending_calls,
                    );
                }
                let occurrence =
                    pending_calls
                        .get_mut(&(*tool_call_id, run_id))
                        .and_then(|pending| {
                            if pending.is_empty() {
                                return None;
                            }
                            let position = pending
                                .iter()
                                .position(|index| {
                                    emitted_calls[*index].segment == segment
                                        && emitted_calls[*index].run_id == run_id
                                })
                                .unwrap_or(0);
                            pending.remove(position)
                        });
                let Some(occurrence) = occurrence else {
                    continue;
                };
                let provider_tool_call_id = emitted_calls[occurrence].provider_tool_call_id.clone();
                emitted_calls[occurrence].result = Some(cookie_agent_providers::ToolResult {
                    tool_call_id: provider_tool_call_id,
                    content: result.content.clone(),
                    is_error: false,
                });
            }
            Event::ToolCallFailed {
                tool_call_id,
                message,
            } => {
                let Some(run_id) = event.run_id else {
                    continue;
                };
                if assistant_call_ids.contains(&(*tool_call_id, run_id)) {
                    flush_assistant(
                        &mut output,
                        &mut assistant_text,
                        &mut assistant_calls,
                        &mut assistant_call_ids,
                        &mut assistant_opaque,
                        &mut assistant_opaque_usable,
                        segment,
                        &mut emitted_calls,
                        &mut pending_calls,
                    );
                }
                let occurrence =
                    pending_calls
                        .get_mut(&(*tool_call_id, run_id))
                        .and_then(|pending| {
                            if pending.is_empty() {
                                return None;
                            }
                            let position = pending
                                .iter()
                                .position(|index| {
                                    emitted_calls[*index].segment == segment
                                        && emitted_calls[*index].run_id == run_id
                                })
                                .unwrap_or(0);
                            pending.remove(position)
                        });
                let Some(occurrence) = occurrence else {
                    continue;
                };
                let provider_tool_call_id = emitted_calls[occurrence].provider_tool_call_id.clone();
                emitted_calls[occurrence].result = Some(cookie_agent_providers::ToolResult {
                    tool_call_id: provider_tool_call_id,
                    content: message.clone(),
                    is_error: true,
                });
            }
            Event::RunCompleted { .. } => {
                flush_assistant(
                    &mut output,
                    &mut assistant_text,
                    &mut assistant_calls,
                    &mut assistant_call_ids,
                    &mut assistant_opaque,
                    &mut assistant_opaque_usable,
                    segment,
                    &mut emitted_calls,
                    &mut pending_calls,
                );
                segment += 1;
            }
            Event::RunFailed { .. } | Event::RunCancelled { .. } | Event::RunInterrupted { .. } => {
                if assistant_calls.is_empty() {
                    assistant_text.clear();
                    assistant_opaque = None;
                    assistant_opaque_usable = true;
                    assistant_call_ids.clear();
                } else {
                    flush_assistant(
                        &mut output,
                        &mut assistant_text,
                        &mut assistant_calls,
                        &mut assistant_call_ids,
                        &mut assistant_opaque,
                        &mut assistant_opaque_usable,
                        segment,
                        &mut emitted_calls,
                        &mut pending_calls,
                    );
                }
                segment += 1;
            }
            _ => {}
        }
    }
    flush_assistant(
        &mut output,
        &mut assistant_text,
        &mut assistant_calls,
        &mut assistant_call_ids,
        &mut assistant_opaque,
        &mut assistant_opaque_usable,
        segment,
        &mut emitted_calls,
        &mut pending_calls,
    );
    finalize_persisted_turns(output, emitted_calls)
}

fn finalize_persisted_turns(
    output: Vec<cookie_agent_providers::PersistedTurn>,
    emitted_calls: Vec<EmittedToolCall>,
) -> Vec<cookie_agent_providers::PersistedTurn> {
    let mut results_by_assistant: HashMap<usize, Vec<(usize, cookie_agent_providers::ToolResult)>> =
        HashMap::new();
    for call in emitted_calls {
        if let Some(result) = call.result {
            results_by_assistant
                .entry(call.assistant_index)
                .or_default()
                .push((call.call_index, result));
        }
    }
    let mut finalized = Vec::new();
    for (assistant_index, turn) in output.into_iter().enumerate() {
        let cookie_agent_providers::PersistedTurn { message, opaque } = turn;
        let ProviderMessage::Assistant {
            content,
            tool_calls,
        } = message
        else {
            finalized.push(cookie_agent_providers::PersistedTurn { message, opaque });
            continue;
        };
        let mut results = results_by_assistant
            .remove(&assistant_index)
            .unwrap_or_default();
        results.sort_by_key(|(call_index, _)| *call_index);
        let retained_indices: HashSet<_> =
            results.iter().map(|(call_index, _)| *call_index).collect();
        let original_call_count = tool_calls.len();
        let tool_calls: Vec<_> = tool_calls
            .into_iter()
            .enumerate()
            .filter_map(|(call_index, call)| retained_indices.contains(&call_index).then_some(call))
            .collect();
        let opaque = (tool_calls.len() == original_call_count)
            .then_some(opaque)
            .flatten();
        if !content.is_empty() || !tool_calls.is_empty() || opaque.is_some() {
            finalized.push(cookie_agent_providers::PersistedTurn {
                message: ProviderMessage::Assistant {
                    content,
                    tool_calls,
                },
                opaque,
            });
            finalized.extend(results.into_iter().map(|(_, result)| {
                cookie_agent_providers::PersistedTurn {
                    message: ProviderMessage::Tool { result },
                    opaque: None,
                }
            }));
        }
    }
    finalized
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::Write,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use cookie_agent_config::{
        AgentProfile, DelegationConfig, ModelConfig, ProviderConfig, ProviderType,
    };
    use futures_util::{StreamExt, stream};
    use tokio::sync::{Barrier, Notify};

    use super::*;

    struct NoopProvider;

    #[async_trait]
    impl Provider for NoopProvider {
        fn capabilities(&self, _: &ModelId) -> cookie_agent_providers::ProviderCapabilities {
            cookie_agent_providers::ProviderCapabilities::default()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            Ok(stream::iter([Ok(NormalizedEvent::Stop {
                reason: StopReason::EndTurn,
            })])
            .boxed())
        }
    }

    struct ReportProvider;

    #[async_trait]
    impl Provider for ReportProvider {
        fn capabilities(&self, _: &ModelId) -> cookie_agent_providers::ProviderCapabilities {
            cookie_agent_providers::ProviderCapabilities::default()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            Ok(stream::iter([
                Ok(NormalizedEvent::TextDelta {
                    text: "child report".into(),
                }),
                Ok(NormalizedEvent::Stop {
                    reason: StopReason::EndTurn,
                }),
            ])
            .boxed())
        }
    }

    struct TwoTurnBatchProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for TwoTurnBatchProvider {
        fn capabilities(&self, _: &ModelId) -> cookie_agent_providers::ProviderCapabilities {
            cookie_agent_providers::ProviderCapabilities::default()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let events = if call == 0 {
                vec![
                    Ok(NormalizedEvent::ToolCallStart {
                        tool_call_id: "delegate-call".into(),
                        tool: "delegate".into(),
                    }),
                    Ok(NormalizedEvent::ToolCallEnd {
                        tool_call_id: "delegate-call".into(),
                    }),
                    Ok(NormalizedEvent::ToolCallStart {
                        tool_call_id: "read-call".into(),
                        tool: "read".into(),
                    }),
                    Ok(NormalizedEvent::ToolArgsDelta {
                        tool_call_id: "read-call".into(),
                        delta: r#"{"path":"file"}"#.into(),
                    }),
                    Ok(NormalizedEvent::ToolCallEnd {
                        tool_call_id: "read-call".into(),
                    }),
                    Ok(NormalizedEvent::Stop {
                        reason: StopReason::EndTurn,
                    }),
                ]
            } else {
                vec![
                    Ok(NormalizedEvent::TextDelta {
                        text: "advanced".into(),
                    }),
                    Ok(NormalizedEvent::Stop {
                        reason: StopReason::EndTurn,
                    }),
                ]
            };
            Ok(stream::iter(events).boxed())
        }
    }

    struct BatchToolProvider {
        release_delegate: Notify,
    }

    struct InteractiveProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for InteractiveProvider {
        fn capabilities(&self, _: &ModelId) -> cookie_agent_providers::ProviderCapabilities {
            cookie_agent_providers::ProviderCapabilities::default()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            let events = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                vec![
                    Ok(NormalizedEvent::ToolCallStart {
                        tool_call_id: "interactive".into(),
                        tool: "bash".into(),
                    }),
                    Ok(NormalizedEvent::ToolArgsDelta {
                        tool_call_id: "interactive".into(),
                        delta: r#"{"command":"cat","interactive":true}"#.into(),
                    }),
                    Ok(NormalizedEvent::ToolCallEnd {
                        tool_call_id: "interactive".into(),
                    }),
                    Ok(NormalizedEvent::Stop {
                        reason: StopReason::EndTurn,
                    }),
                ]
            } else {
                vec![Ok(NormalizedEvent::Stop {
                    reason: StopReason::EndTurn,
                })]
            };
            Ok(stream::iter(events).boxed())
        }
    }

    struct InteractiveTool {
        started: Notify,
        hold: Option<Arc<Notify>>,
        consumed: Mutex<Option<oneshot::Sender<()>>>,
        writes: Mutex<Vec<StdinWrite>>,
    }

    #[async_trait]
    impl ToolProvider for InteractiveTool {
        fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
            Ok(vec![ToolSpec {
                name: "bash".into(),
                description: "test interactive bash".into(),
                parameters: Value::Null,
            }])
        }

        async fn invoke(
            &self,
            mut ctx: ToolInvocationContext,
            _: ToolCall,
        ) -> Result<ToolResult, ToolError> {
            let mut stdin = ctx.stdin.take().expect("interactive stdin");
            let mut consumed = self
                .consumed
                .lock()
                .expect("interactive consumed lock poisoned")
                .take();
            self.started.notify_one();
            if let Some(hold) = &self.hold {
                hold.notified().await;
            }
            while let Some(write) = stdin.recv().await {
                ctx.progress
                    .output(cookie_agent_protocol::OutputStream::Stdout, &write.data);
                let eof = write.eof;
                self.writes
                    .lock()
                    .expect("interactive writes lock poisoned")
                    .push(write);
                if let Some(consumed) = consumed.take() {
                    let _ = consumed.send(());
                }
                if eof {
                    break;
                }
            }
            Ok(ToolResult {
                content: "interactive complete".into(),
                truncated: false,
            })
        }
    }

    #[async_trait]
    impl ToolProvider for BatchToolProvider {
        fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
            Ok(vec![
                ToolSpec {
                    name: "delegate".into(),
                    description: "test delegate".into(),
                    parameters: Value::Null,
                },
                ToolSpec {
                    name: "read".into(),
                    description: "test read".into(),
                    parameters: Value::Null,
                },
            ])
        }

        async fn invoke(
            &self,
            _: ToolInvocationContext,
            call: ToolCall,
        ) -> Result<ToolResult, ToolError> {
            if call.name == "delegate" {
                self.release_delegate.notified().await;
                Ok(ToolResult {
                    content: "late delegate result".into(),
                    truncated: false,
                })
            } else {
                Ok(ToolResult {
                    content: "legitimate read result".into(),
                    truncated: false,
                })
            }
        }
    }

    struct SteeringProvider {
        calls: AtomicUsize,
        first_started: Arc<Barrier>,
        release_first: Notify,
        requests: Mutex<Vec<Vec<ProviderMessage>>>,
    }

    #[async_trait]
    impl Provider for SteeringProvider {
        fn capabilities(&self, _: &ModelId) -> cookie_agent_providers::ProviderCapabilities {
            cookie_agent_providers::ProviderCapabilities::default()
        }

        async fn stream(
            &self,
            request: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            self.requests
                .lock()
                .expect("requests lock poisoned")
                .push(request.messages);
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.first_started.wait().await;
                self.release_first.notified().await;
            }
            Ok(stream::iter([
                Ok(NormalizedEvent::TextDelta {
                    text: "done".into(),
                }),
                Ok(NormalizedEvent::Stop {
                    reason: StopReason::EndTurn,
                }),
            ])
            .boxed())
        }
    }

    struct RetrySteeringProvider {
        calls: AtomicUsize,
        first_started: Arc<Barrier>,
        release_first: Notify,
        requests: Mutex<Vec<Vec<ProviderMessage>>>,
    }

    struct OpaqueRecordingProvider {
        protocol: ProviderProtocol,
        artifact: Value,
        fail_after_first: bool,
        calls: AtomicUsize,
        requests: Mutex<Vec<ProviderRequest>>,
    }

    struct RecordingNoopProvider {
        protocol: Option<ProviderProtocol>,
        requests: Mutex<Vec<ProviderRequest>>,
    }

    #[async_trait]
    impl Provider for RecordingNoopProvider {
        fn capabilities(&self, _: &ModelId) -> cookie_agent_providers::ProviderCapabilities {
            cookie_agent_providers::ProviderCapabilities::default()
        }

        fn protocol(&self, _: &ModelId) -> Option<ProviderProtocol> {
            self.protocol
        }

        async fn stream(
            &self,
            request: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            self.requests
                .lock()
                .expect("requests lock poisoned")
                .push(request);
            Ok(stream::iter([Ok(NormalizedEvent::Stop {
                reason: StopReason::EndTurn,
            })])
            .boxed())
        }
    }

    #[async_trait]
    impl Provider for OpaqueRecordingProvider {
        fn capabilities(&self, _: &ModelId) -> cookie_agent_providers::ProviderCapabilities {
            cookie_agent_providers::ProviderCapabilities::default()
        }

        fn protocol(&self, _: &ModelId) -> Option<ProviderProtocol> {
            Some(self.protocol)
        }

        async fn stream(
            &self,
            request: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            self.requests
                .lock()
                .expect("requests lock poisoned")
                .push(request);
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(stream::iter([
                    Ok(NormalizedEvent::TextDelta {
                        text: "first".into(),
                    }),
                    Ok(NormalizedEvent::TurnOpaque {
                        state: cookie_agent_providers::AssistantTurnOpaque {
                            provider: self.protocol,
                            payload: self.artifact.clone(),
                        },
                    }),
                    Ok(NormalizedEvent::Stop {
                        reason: StopReason::EndTurn,
                    }),
                ])
                .boxed());
            }
            if self.fail_after_first {
                return Err(ProviderError::EntryTerminal {
                    message: "advance fallback".into(),
                });
            }
            Ok(stream::iter([Ok(NormalizedEvent::Stop {
                reason: StopReason::EndTurn,
            })])
            .boxed())
        }
    }

    struct MeaningfulFailureProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for MeaningfulFailureProvider {
        fn capabilities(&self, _: &ModelId) -> cookie_agent_providers::ProviderCapabilities {
            cookie_agent_providers::ProviderCapabilities::default()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(stream::iter([
                Ok(NormalizedEvent::TextDelta {
                    text: "partial".into(),
                }),
                Err(ProviderError::EntryRetryable {
                    message: "dropped".into(),
                }),
            ])
            .boxed())
        }
    }

    struct BlockingProvider {
        calls: AtomicUsize,
        release: Notify,
    }

    #[async_trait]
    impl Provider for BlockingProvider {
        fn capabilities(&self, _: &ModelId) -> cookie_agent_providers::ProviderCapabilities {
            cookie_agent_providers::ProviderCapabilities::default()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
            Ok(stream::iter([Ok(NormalizedEvent::Stop {
                reason: StopReason::EndTurn,
            })])
            .boxed())
        }
    }

    #[async_trait]
    impl Provider for RetrySteeringProvider {
        fn capabilities(&self, _: &ModelId) -> cookie_agent_providers::ProviderCapabilities {
            cookie_agent_providers::ProviderCapabilities::default()
        }

        async fn stream(
            &self,
            request: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            self.requests
                .lock()
                .expect("requests lock poisoned")
                .push(request.messages);
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                self.first_started.wait().await;
                self.release_first.notified().await;
            }
            if call < 2 {
                return Ok(stream::iter([Err(ProviderError::EntryRetryable {
                    message: "retry".into(),
                })])
                .boxed());
            }
            Ok(stream::iter([Ok(NormalizedEvent::Stop {
                reason: StopReason::EndTurn,
            })])
            .boxed())
        }
    }

    fn test_config() -> Config {
        let mut config = Config::default();
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                kind: ProviderType::OpenAi,
                api_key_env: None,
                base_url: None,
                api: None,
            },
        );
        config.agents = BTreeMap::from([
            (
                "test".into(),
                AgentProfile {
                    r#type: ConfigAgentType::Primary,
                    models: vec![ModelConfig {
                        provider: "test".into(),
                        model: "test-model".into(),
                    }],
                    delegation: DelegationConfig {
                        enabled: true,
                        allowed_profiles: vec!["worker".into()],
                        limit: None,
                    },
                    ..AgentProfile::default()
                },
            ),
            (
                "worker".into(),
                AgentProfile {
                    r#type: ConfigAgentType::Subagent,
                    models: vec![ModelConfig {
                        provider: "test".into(),
                        model: "test-model".into(),
                    }],
                    ..AgentProfile::default()
                },
            ),
        ]);
        config
    }

    fn test_engine(provider: Arc<dyn Provider>) -> (tempfile::TempDir, Engine) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: test_config(),
            providers: HashMap::from([("test".into(), provider)]),
            tools: Vec::new(),
        })
        .expect("open engine");
        (directory, engine)
    }

    async fn reopen_test_engine(
        directory: &tempfile::TempDir,
        provider: Arc<dyn Provider>,
    ) -> Engine {
        let data_dir = directory.path().join("data");
        let cwd = directory.path().to_owned();
        tokio::task::spawn_blocking(move || {
            Engine::open(EngineOptions {
                data_dir,
                cwd,
                config: test_config(),
                providers: HashMap::from([("test".into(), provider)]),
                tools: Vec::new(),
            })
        })
        .await
        .expect("reopen task")
        .expect("reopen engine")
    }

    fn reopen_test_engine_in_runtime(
        directory: &tempfile::TempDir,
        provider: Arc<dyn Provider>,
    ) -> Engine {
        Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: test_config(),
            providers: HashMap::from([("test".into(), provider)]),
            tools: Vec::new(),
        })
        .expect("reopen engine from Tokio runtime")
    }

    async fn reconcile_test_engine(engine: &Engine) {
        let engine = engine.clone();
        tokio::task::spawn_blocking(move || engine.reconcile())
            .await
            .expect("reconcile task")
            .expect("reconcile engine");
    }

    async fn pending_delegate_parent(
        engine: &Engine,
    ) -> (SessionId, RunId, ToolCallId, DelegateInvocation) {
        let parent = engine
            .create_session(".", "test")
            .expect("create parent")
            .id;
        let parent_run = RunId::new_v7();
        let call = ToolCallId::new_v7();
        engine
            .append(
                parent,
                Some(parent_run),
                Event::RunStarted {
                    client_run_id: "parent".into(),
                    input: "delegate".into(),
                },
            )
            .await
            .expect("start parent");
        engine
            .append(
                parent,
                Some(parent_run),
                Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "delegate".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: None,
                    provider_protocol: None,
                },
            )
            .await
            .expect("start delegate call");
        (
            parent,
            parent_run,
            call,
            delegate_invocation(parent, parent_run, call, "child task"),
        )
    }

    fn delegate_request(invocation: &DelegateInvocation) -> journal::DelegateRequestPayload {
        journal::DelegateRequestPayload {
            task: invocation.task.clone(),
            context: invocation.context.clone(),
            success_criteria: invocation.success_criteria.clone(),
            expected_output: invocation.expected_output.clone(),
        }
    }

    fn child_policy_for(engine: &Engine, invocation: &DelegateInvocation) -> PolicySnapshot {
        let parent = engine
            .inner
            .store
            .get(invocation.parent_session_id)
            .expect("parent projection");
        engine
            .inner
            .config
            .materialize_child_policy(&invocation.profile, &parent.policy)
            .expect("child policy")
    }

    fn write_started_delegation(
        engine: &Engine,
        invocation: &DelegateInvocation,
        child_session_id: SessionId,
        child_policy: &PolicySnapshot,
    ) -> InvocationId {
        let invocation_id = invocation_id(
            invocation.parent_session_id,
            invocation.parent_run_id,
            invocation.parent_tool_call_id,
        );
        let request = delegate_request(invocation);
        let request_fingerprint = serde_json::to_string(&(
            &invocation.profile,
            &invocation.task,
            &invocation.context,
            &invocation.success_criteria,
            &invocation.expected_output,
            child_policy,
        ))
        .expect("delegate fingerprint");
        events::append_jsonl(
            engine.inner.journal.path(),
            &journal::JournalRecord::DelegationStarted {
                reservation: journal::DelegationReservation {
                    invocation_id,
                    parent_session_id: invocation.parent_session_id,
                    parent_run_id: invocation.parent_run_id,
                    parent_tool_call_id: invocation.parent_tool_call_id,
                    child_session_id,
                },
                child_policy: Box::new(child_policy.clone()),
                request_fingerprint,
                task: request.task.clone(),
                request,
            },
        )
        .expect("write delegation reservation");
        invocation_id
    }

    fn write_linked_delegation(engine: &Engine, invocation_id: InvocationId) {
        events::append_jsonl(
            engine.inner.journal.path(),
            &journal::JournalRecord::DelegationLinked { invocation_id },
        )
        .expect("write delegation link");
    }

    fn write_run_started_delegation(
        engine: &Engine,
        invocation_id: InvocationId,
        child_run_id: RunId,
    ) {
        events::append_jsonl(
            engine.inner.journal.path(),
            &journal::JournalRecord::DelegationRunStarted {
                invocation_id,
                child_run_id,
            },
        )
        .expect("write delegation run confirmation");
    }

    fn persist_delegated_child(
        engine: &Engine,
        invocation: &DelegateInvocation,
        child_session_id: SessionId,
        child_policy: PolicySnapshot,
    ) {
        let parent = engine
            .inner
            .store
            .get(invocation.parent_session_id)
            .expect("parent projection");
        let meta = session_meta(
            child_session_id,
            SessionOrigin::Delegated {
                root_session_id: invocation.parent_session_id,
                parent_session_id: invocation.parent_session_id,
                parent_run_id: invocation.parent_run_id,
                parent_tool_call_id: invocation.parent_tool_call_id,
                invocation_id: invocation_id(
                    invocation.parent_session_id,
                    invocation.parent_run_id,
                    invocation.parent_tool_call_id,
                ),
                depth: 1,
            },
            std::path::Path::new(&parent.meta.cwd),
            &child_policy,
        );
        engine
            .inner
            .store
            .create(meta, child_policy)
            .expect("persist child session");
    }

    fn append_child_event(engine: &Engine, child: SessionId, run: RunId, event: Event) {
        let log = engine.inner.store.get(child).expect("child projection").log;
        log.append(Some(run), event).expect("append child event");
        engine
            .inner
            .store
            .update(child)
            .expect("refresh child projection");
    }

    fn journal_records(engine: &Engine) -> Vec<journal::JournalRecord> {
        events::load_jsonl(engine.inner.journal.path()).expect("read delegation journal")
    }

    fn append_torn_journal_tail(engine: &Engine, tail: &[u8]) {
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(engine.inner.journal.path())
            .expect("open delegation journal");
        journal.write_all(tail).expect("write torn journal tail");
        journal.sync_all().expect("sync torn journal tail");
    }

    fn obstruct_journal_appends(engine: &Engine) -> std::path::PathBuf {
        let path = engine.inner.journal.path();
        let saved = path.with_extension("poisoned");
        std::fs::rename(path, &saved).expect("park journal");
        std::fs::create_dir(path).expect("replace journal with directory");
        saved
    }

    fn restore_journal_path(engine: &Engine, saved: &std::path::Path) {
        let path = engine.inner.journal.path();
        std::fs::remove_dir(path).expect("remove journal obstruction");
        std::fs::rename(saved, path).expect("restore journal");
    }

    async fn reserve_live_delegation(
        engine: &Engine,
        invocation: &DelegateInvocation,
        child_policy: PolicySnapshot,
    ) -> journal::JournalEntry {
        let fingerprint = serde_json::to_string(&(
            &invocation.profile,
            &invocation.task,
            &invocation.context,
            &invocation.success_criteria,
            &invocation.expected_output,
            &child_policy,
        ))
        .expect("delegate fingerprint");
        let journal = engine.inner.journal.clone();
        let invocation_id = invocation_id(
            invocation.parent_session_id,
            invocation.parent_run_id,
            invocation.parent_tool_call_id,
        );
        let parent_session_id = invocation.parent_session_id;
        let parent_run_id = invocation.parent_run_id;
        let parent_tool_call_id = invocation.parent_tool_call_id;
        let request = delegate_request(invocation);
        tokio::task::spawn_blocking(move || {
            journal.reserve(
                invocation_id,
                parent_session_id,
                parent_run_id,
                parent_tool_call_id,
                child_policy,
                fingerprint,
                request,
            )
        })
        .await
        .expect("reserve task")
        .expect("reserve delegation")
    }

    async fn wait_for_session_status(
        engine: &Engine,
        session: SessionId,
        expected: &SessionStatus,
    ) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if &engine
                    .inner
                    .store
                    .get(session)
                    .expect("session projection")
                    .status
                    == expected
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("session did not reach expected status");
    }

    fn journal_link_count(engine: &Engine, invocation_id: InvocationId) -> usize {
        journal_records(engine)
            .iter()
            .filter(|record| {
                matches!(record, journal::JournalRecord::DelegationLinked { invocation_id: id } if *id == invocation_id)
            })
            .count()
    }

    fn journal_run_count(
        engine: &Engine,
        invocation_id: InvocationId,
        child_run_id: RunId,
    ) -> usize {
        journal_records(engine)
            .iter()
            .filter(|record| {
                matches!(record, journal::JournalRecord::DelegationRunStarted { invocation_id: id, child_run_id: run } if *id == invocation_id && *run == child_run_id)
            })
            .count()
    }

    fn child_run_start_count(engine: &Engine, child: SessionId) -> usize {
        engine
            .inner
            .store
            .get(child)
            .expect("child projection")
            .log
            .events()
            .iter()
            .filter(|event| matches!(event.event, Event::RunStarted { .. }))
            .count()
    }

    fn child_run_id(engine: &Engine, child: SessionId) -> RunId {
        *engine
            .inner
            .store
            .get(child)
            .expect("child projection")
            .runs
            .keys()
            .next()
            .expect("child run")
    }

    fn run_cancel_count(engine: &Engine, session: SessionId) -> usize {
        engine
            .inner
            .store
            .get(session)
            .expect("session projection")
            .log
            .events()
            .iter()
            .filter(|event| matches!(event.event, Event::RunCancelled { .. }))
            .count()
    }

    fn parent_delegate_completion_count(
        engine: &Engine,
        parent: SessionId,
        call: ToolCallId,
    ) -> usize {
        engine
            .inner
            .store
            .get(parent)
            .expect("parent projection")
            .log
            .events()
            .iter()
            .filter(|event| {
                matches!(event.event, Event::ToolCallCompleted { tool_call_id, .. } if tool_call_id == call)
            })
            .count()
    }

    async fn wait_for_delegate_completion(
        engine: &Engine,
        parent: SessionId,
        call: ToolCallId,
    ) -> cookie_agent_protocol::ToolResult {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(result) = engine
                    .inner
                    .store
                    .get(parent)
                    .expect("parent projection")
                    .log
                    .events()
                    .into_iter()
                    .find_map(|event| match event.event {
                        Event::ToolCallCompleted {
                            tool_call_id,
                            result,
                        } if tool_call_id == call => Some(result),
                        _ => None,
                    })
                {
                    return result;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delegate result was not committed")
    }

    fn approval_request_event(
        approval_id: &str,
        resources: Vec<cookie_agent_protocol::ApprovalResource>,
        precedence_reason: &str,
    ) -> Event {
        let (action, resource, suggested_pattern) = {
            let primary = resources.first().expect("approval has a resource");
            (
                primary.action,
                primary.resource.clone(),
                primary.suggested_pattern.clone(),
            )
        };
        Event::ApprovalRequested {
            approval_id: approval_id.into(),
            action,
            resource: resource.clone(),
            suggested_pattern,
            resources,
            decision_trace: cookie_agent_protocol::DecisionTrace {
                action,
                normalized_resource: resource,
                candidates: Vec::new(),
                effect: cookie_agent_protocol::Effect::Ask,
                precedence_reason: precedence_reason.into(),
            },
        }
    }

    async fn start_with_admission(
        engine: &Engine,
        child: SessionId,
        invocation_id: InvocationId,
        generation: u64,
        input: String,
    ) -> Result<RunStartResult, EngineError> {
        let actor = engine
            .inner
            .actors
            .lock()
            .expect("actor registry lock poisoned")
            .get(&child)
            .cloned()
            .expect("child actor");
        let (reply, receiver) = tokio::sync::oneshot::channel();
        actor
            .send(SessionCommand::Start {
                params: RunStartParams {
                    session_id: child,
                    client_run_id: delegate_client_run_id(invocation_id),
                    input,
                },
                admission: Some((invocation_id, generation)),
                reply,
            })
            .await
            .expect("queue child start");
        receiver.await.expect("child start reply")
    }

    fn envelope(seq: u64, event: Event) -> EventEnvelope {
        EventEnvelope {
            session_id: SessionId::new_v7(),
            run_id: Some(RunId::new_v7()),
            seq,
            timestamp: jiff::Timestamp::now(),
            event,
        }
    }

    fn delegate_invocation(
        session: SessionId,
        run: RunId,
        call: ToolCallId,
        task: &str,
    ) -> DelegateInvocation {
        DelegateInvocation {
            parent_session_id: session,
            parent_run_id: run,
            parent_tool_call_id: call,
            profile: "worker".into(),
            task: task.into(),
            context: vec![serde_json::json!({"note": "context"})],
            success_criteria: vec!["done".into()],
            expected_output: serde_json::json!({"format": "text"}),
        }
    }

    fn visible_messages(messages: &[ProviderMessage]) -> Vec<(&'static str, String)> {
        messages
            .iter()
            .filter_map(|message| match message {
                ProviderMessage::User { content } => {
                    content.first().and_then(|content| match content {
                        ContentPart::Text { text } => Some(("user", text.clone())),
                        _ => None,
                    })
                }
                ProviderMessage::Assistant { content, .. } => {
                    content.first().and_then(|content| match content {
                        ContentPart::Text { text } => Some(("assistant", text.clone())),
                        _ => None,
                    })
                }
                _ => None,
            })
            .collect()
    }

    async fn running_delegate(engine: &Engine, provider: &BlockingProvider) -> DelegateHandle {
        let parent = engine
            .create_session(".", "test")
            .expect("create parent")
            .id;
        let parent_run = engine
            .start_run(RunStartParams {
                session_id: parent,
                client_run_id: "parent".into(),
                input: "delegate".into(),
            })
            .await
            .expect("start parent")
            .run_id;
        tokio::time::timeout(Duration::from_secs(2), async {
            while provider.calls.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("parent provider did not start");
        let call = ToolCallId::new_v7();
        engine
            .append(
                parent,
                Some(parent_run),
                Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "delegate".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: None,
                    provider_protocol: None,
                },
            )
            .await
            .expect("delegate call");
        let handle = engine
            .delegate_invoke(delegate_invocation(parent, parent_run, call, "child task"))
            .await
            .expect("delegate invoke");
        tokio::time::timeout(Duration::from_secs(2), async {
            while provider.calls.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("child provider did not start");
        handle
    }

    #[tokio::test]
    async fn concurrent_appends_do_not_deadlock() {
        let (_directory, engine) = test_engine(Arc::new(NoopProvider));
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let appends = (0..512).map(|index| {
            let engine = engine.clone();
            tokio::spawn(async move {
                if index % 3 == 0 {
                    engine
                        .submit_tool_result(
                            session,
                            RunId::new_v7(),
                            ToolCallId::new_v7(),
                            Ok(ToolResult {
                                content: index.to_string(),
                                truncated: false,
                            }),
                        )
                        .await
                } else {
                    engine
                        .append(
                            session,
                            None,
                            Event::ToolCallProgress {
                                tool_call_id: ToolCallId::new_v7(),
                                message: index.to_string(),
                            },
                        )
                        .await
                }
            })
        });
        tokio::time::timeout(
            Duration::from_secs(2),
            futures_util::future::join_all(appends),
        )
        .await
        .expect("concurrent appends timed out")
        .into_iter()
        .enumerate()
        .for_each(|(index, result)| {
            let result = result.expect("append task panicked");
            if index % 3 == 0 {
                assert!(matches!(result, Err(EngineError::MissingTool(_))));
            } else {
                result.expect("append failed");
            }
        });
    }

    #[tokio::test]
    async fn delegate_invocation_starts_the_child_exactly_once_and_rejects_fingerprint_conflicts() {
        let (_directory, engine) = test_engine(Arc::new(NoopProvider));
        let parent = engine
            .create_session(".", "test")
            .expect("create parent")
            .id;
        let parent_run = RunId::new_v7();
        let call = ToolCallId::new_v7();
        engine
            .append(
                parent,
                Some(parent_run),
                Event::RunStarted {
                    client_run_id: "parent".into(),
                    input: "delegate".into(),
                },
            )
            .await
            .expect("parent start");
        engine
            .append(
                parent,
                Some(parent_run),
                Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "delegate".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: None,
                    provider_protocol: None,
                },
            )
            .await
            .expect("delegate call");
        let request = delegate_invocation(parent, parent_run, call, "child task");
        let left_engine = engine.clone();
        let right_engine = engine.clone();
        let (first, second) = tokio::join!(
            left_engine.delegate_invoke(request.clone()),
            right_engine.delegate_invoke(request)
        );
        let first = first.expect("first invocation");
        let second = second.expect("redelivery");
        assert_eq!(first, second);
        let child = engine
            .inner
            .store
            .get(first.child_session_id)
            .expect("child");
        let parent_events = engine.inner.store.get(parent).expect("parent").log.events();
        let links: Vec<_> = parent_events
            .iter()
            .filter(|event| {
                matches!(event.event, Event::ToolCallLinked { tool_call_id, child_session_id }
                    if tool_call_id == call && child_session_id == first.child_session_id)
            })
            .collect();
        assert_eq!(links.len(), 1);
        let call_started = parent_events
            .iter()
            .find(|event| {
                matches!(event.event, Event::ToolCallStarted { tool_call_id, .. } if tool_call_id == call)
            })
            .expect("parent tool call");
        assert!(call_started.seq < links[0].seq);
        let child_run_started = child
            .log
            .events()
            .into_iter()
            .find(|event| matches!(event.event, Event::RunStarted { .. }))
            .expect("child run start");
        assert!(links[0].timestamp <= child_run_started.timestamp);
        assert_eq!(
            child
                .log
                .events()
                .into_iter()
                .filter(|event| matches!(event.event, Event::RunStarted { .. }))
                .count(),
            1
        );
        let conflict = engine
            .delegate_invoke(delegate_invocation(
                parent,
                parent_run,
                call,
                "different task",
            ))
            .await;
        assert!(matches!(
            conflict,
            Err(EngineError::Journal(JournalError::Corrupt(_)))
        ));
    }

    #[tokio::test]
    async fn parent_cancellation_cancels_child_and_returns_structured_delegate_result() {
        let provider = Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            release: Notify::new(),
        });
        let (_directory, engine) = test_engine(provider.clone());
        let parent = engine
            .create_session(".", "test")
            .expect("create parent")
            .id;
        let parent_run = engine
            .start_run(RunStartParams {
                session_id: parent,
                client_run_id: "parent".into(),
                input: "delegate".into(),
            })
            .await
            .expect("start parent")
            .run_id;
        tokio::time::timeout(Duration::from_secs(2), async {
            while provider.calls.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("parent provider did not start");
        let call = ToolCallId::new_v7();
        engine
            .append(
                parent,
                Some(parent_run),
                Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "delegate".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: None,
                    provider_protocol: None,
                },
            )
            .await
            .expect("delegate call");
        let handle = tokio::time::timeout(
            Duration::from_secs(2),
            engine.delegate_invoke(delegate_invocation(parent, parent_run, call, "child task")),
        )
        .await
        .expect("delegate invocation timed out")
        .expect("delegate invoke");
        tokio::time::timeout(Duration::from_secs(2), async {
            while provider.calls.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("child provider did not start");
        tokio::time::timeout(Duration::from_secs(2), engine.cancel_run(parent_run))
            .await
            .expect("parent cancellation timed out")
            .expect("cancel parent");
        let result = tokio::time::timeout(Duration::from_secs(2), engine.await_delegate(handle))
            .await
            .expect("delegate cancellation did not settle")
            .expect("delegate result");
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).expect("structured result")["status"],
            "cancelled"
        );
        assert_eq!(
            engine
                .inner
                .store
                .get(handle.child_session_id)
                .expect("child")
                .status,
            SessionStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn dropping_delegate_wait_cancels_the_child() {
        let provider = Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            release: Notify::new(),
        });
        let (_directory, engine) = test_engine(provider.clone());
        let handle = running_delegate(&engine, &provider).await;
        drop(engine.await_delegate(handle));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if engine
                    .inner
                    .store
                    .get(handle.child_session_id)
                    .expect("child")
                    .status
                    == SessionStatus::Cancelled
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropped wait did not cancel child");
    }

    #[tokio::test]
    async fn unstarted_linked_child_starts_once_during_resume_reconciliation() {
        let (_directory, engine) = test_engine(Arc::new(NoopProvider));
        let parent = engine
            .create_session(".", "test")
            .expect("create parent")
            .id;
        let parent_run = RunId::new_v7();
        let call = ToolCallId::new_v7();
        engine
            .append(
                parent,
                Some(parent_run),
                Event::RunStarted {
                    client_run_id: "parent".into(),
                    input: "delegate".into(),
                },
            )
            .await
            .expect("parent start");
        engine
            .append(
                parent,
                Some(parent_run),
                Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "delegate".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: None,
                    provider_protocol: None,
                },
            )
            .await
            .expect("delegate call");
        engine
            .append(
                parent,
                Some(parent_run),
                Event::RunInterrupted { reason: None },
            )
            .await
            .expect("interrupt parent");
        let child = engine
            .create_child(
                parent,
                parent_run,
                call,
                "worker",
                "crash-window-fingerprint".into(),
                journal::DelegateRequestPayload {
                    task: "child task".into(),
                    context: Vec::new(),
                    success_criteria: Vec::new(),
                    expected_output: Value::Null,
                },
                None,
            )
            .await
            .expect("create unstarted child");
        assert!(
            engine
                .inner
                .store
                .get(child.id)
                .expect("child")
                .runs
                .is_empty()
        );
        engine.resume(parent).await.expect("resume parent");
        engine.resume(parent).await.expect("repeat resume");
        assert_eq!(
            engine
                .inner
                .store
                .get(child.id)
                .expect("child")
                .log
                .events()
                .into_iter()
                .filter(|event| matches!(event.event, Event::RunStarted { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn delegate_result_bounding_preserves_utf8() {
        let result = bound_delegate_result("éé".into(), 3);
        assert_eq!(result.content, "é");
        assert!(result.truncated);
    }

    #[test]
    fn assembled_tool_transcript_snapshot_is_stable() {
        let call = ToolCallId(Uuid::from_u128(8));
        let session = SessionId(Uuid::from_u128(9));
        let run = RunId(Uuid::from_u128(10));
        let envelope = |seq, event| EventEnvelope {
            session_id: session,
            run_id: Some(run),
            seq,
            timestamp: jiff::Timestamp::now(),
            event,
        };
        let messages = assemble_messages(&[
            envelope(
                1,
                Event::RunStarted {
                    client_run_id: "snapshot".into(),
                    input: "inspect the workspace".into(),
                },
            ),
            envelope(
                2,
                Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "read".into(),
                    arguments: serde_json::json!({"path": "README.md"}),
                    provider_tool_call_id: None,
                    provider_protocol: None,
                },
            ),
            envelope(
                3,
                Event::ToolCallCompleted {
                    tool_call_id: call,
                    result: cookie_agent_protocol::ToolResult {
                        content: "contents".into(),
                        truncated: false,
                    },
                },
            ),
        ]);
        insta::assert_json_snapshot!(messages);
    }

    #[tokio::test]
    async fn interactive_stdin_preserves_order_eof_and_rejects_after_completion() {
        let mut config = test_config();
        let profile = config.agents.get_mut("test").expect("test profile");
        profile.tools = vec!["bash".into()];
        profile.permissions.exec = Some("allow".into());
        let directory = tempfile::tempdir().expect("temporary directory");
        let tool = Arc::new(InteractiveTool {
            started: Notify::new(),
            hold: None,
            consumed: Mutex::new(None),
            writes: Mutex::new(Vec::new()),
        });
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config,
            providers: HashMap::from([(
                "test".into(),
                Arc::new(InteractiveProvider {
                    calls: AtomicUsize::new(0),
                }) as Arc<dyn Provider>,
            )]),
            tools: vec![tool.clone() as Arc<dyn ToolProvider>],
        })
        .expect("open engine");
        let session = engine.create_session(".", "test").expect("session").id;
        let run = engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "interactive".into(),
                input: "run bash".into(),
            })
            .await
            .expect("start run")
            .run_id;
        tool.started.notified().await;
        let call = engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events()
            .into_iter()
            .find_map(|event| match event.event {
                Event::ToolCallStarted {
                    tool_call_id,
                    ref tool,
                    ..
                } if tool == "bash" => Some(tool_call_id),
                _ => None,
            })
            .expect("interactive call");
        let (_, mut stdout) = engine
            .subscribe_tool_output(call, cookie_agent_protocol::OutputStream::Stdout)
            .expect("output hub");

        for (data, eof) in [(b"first".as_slice(), false), (b"second".as_slice(), true)] {
            assert!(
                engine
                    .tool_stdin(RunToolStdinParams {
                        run_id: run,
                        call_id: call,
                        data: Some(STANDARD.encode(data)),
                        eof,
                    })
                    .await
                    .expect("submit stdin")
                    .accepted
            );
        }
        match stdout.recv().await.expect("stdout delta") {
            events::OutputMessage::Delta(delta) => {
                assert_eq!(delta.data, STANDARD.encode(b"first"))
            }
            events::OutputMessage::Gap(_) => panic!("unexpected output gap"),
        }
        wait_for_session_status(&engine, session, &SessionStatus::Completed).await;
        match stdout.recv().await.expect("final stdout delta") {
            events::OutputMessage::Delta(delta) => {
                assert_eq!(delta.data, STANDARD.encode(b"second"))
            }
            events::OutputMessage::Gap(_) => panic!("unexpected output gap"),
        }
        assert!(stdout.recv().await.is_none());
        assert_eq!(
            tool.writes
                .lock()
                .expect("interactive writes lock poisoned")
                .iter()
                .map(|write| (write.data.clone(), write.eof))
                .collect::<Vec<_>>(),
            vec![(b"first".to_vec(), false), (b"second".to_vec(), true)]
        );
        assert!(
            engine
                .tool_stdin(RunToolStdinParams {
                    run_id: run,
                    call_id: call,
                    data: None,
                    eof: true,
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancellation_discards_queued_interactive_stdin() {
        let mut config = test_config();
        let profile = config.agents.get_mut("test").expect("test profile");
        profile.tools = vec!["bash".into()];
        profile.permissions.exec = Some("allow".into());
        let directory = tempfile::tempdir().expect("temporary directory");
        let release = Arc::new(Notify::new());
        let (consumed, consumed_rx) = oneshot::channel();
        let tool = Arc::new(InteractiveTool {
            started: Notify::new(),
            hold: Some(release.clone()),
            consumed: Mutex::new(Some(consumed)),
            writes: Mutex::new(Vec::new()),
        });
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config,
            providers: HashMap::from([(
                "test".into(),
                Arc::new(InteractiveProvider {
                    calls: AtomicUsize::new(0),
                }) as Arc<dyn Provider>,
            )]),
            tools: vec![tool.clone() as Arc<dyn ToolProvider>],
        })
        .expect("open engine");
        let session = engine.create_session(".", "test").expect("session").id;
        let run = engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "cancel-interactive".into(),
                input: "run bash".into(),
            })
            .await
            .expect("start run")
            .run_id;
        tool.started.notified().await;
        let call = engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events()
            .into_iter()
            .find_map(|event| match event.event {
                Event::ToolCallStarted {
                    tool_call_id,
                    ref tool,
                    ..
                } if tool == "bash" => Some(tool_call_id),
                _ => None,
            })
            .expect("interactive call");
        engine
            .tool_stdin(RunToolStdinParams {
                run_id: run,
                call_id: call,
                data: Some(STANDARD.encode(b"queued")),
                eof: false,
            })
            .await
            .expect("queue stdin before cancellation");
        engine.cancel_run(run).await.expect("cancel run");
        wait_for_session_status(&engine, session, &SessionStatus::Cancelled).await;
        release.notify_one();
        assert!(
            consumed_rx.await.is_err(),
            "releasing the held invocation consumed stdin after cancellation"
        );
        assert!(
            tool.writes
                .lock()
                .expect("interactive writes lock poisoned")
                .is_empty()
        );
        assert!(
            engine
                .tool_stdin(RunToolStdinParams {
                    run_id: run,
                    call_id: call,
                    data: None,
                    eof: true,
                })
                .await
                .is_err()
        );
    }

    #[test]
    fn reconciliation_marks_journalless_orphans_interrupted() {
        let (_directory, engine) = test_engine(Arc::new(NoopProvider));
        let policy = engine
            .inner
            .config
            .materialize_policy("worker")
            .expect("worker policy");
        let orphan = SessionId::new_v7();
        let meta = session_meta(
            orphan,
            SessionOrigin::Delegated {
                root_session_id: SessionId::new_v7(),
                parent_session_id: SessionId::new_v7(),
                parent_run_id: RunId::new_v7(),
                parent_tool_call_id: ToolCallId::new_v7(),
                invocation_id: InvocationId(Uuid::from_u128(9)),
                depth: 1,
            },
            std::path::Path::new("."),
            &policy,
        );
        engine
            .inner
            .store
            .create(meta, policy)
            .expect("create orphan");
        engine.spawn_actor(orphan);
        engine.reconcile().expect("reconcile orphan");
        assert_eq!(
            engine.inner.store.get(orphan).expect("orphan").status,
            SessionStatus::Interrupted
        );
    }

    #[tokio::test]
    async fn steering_after_prompt_snapshot_restarts_no_tool_turn() {
        let provider = Arc::new(SteeringProvider {
            calls: AtomicUsize::new(0),
            first_started: Arc::new(Barrier::new(2)),
            release_first: Notify::new(),
            requests: Mutex::new(Vec::new()),
        });
        let (_directory, engine) = test_engine(provider.clone());
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let (reached, reached_rx) = oneshot::channel();
        let hook = Arc::new(PromptSnapshotHook {
            reached: Mutex::new(Some(reached)),
            release: Notify::new(),
        });
        *engine
            .inner
            .prompt_snapshot_hook
            .lock()
            .expect("prompt snapshot hook lock poisoned") = Some(hook.clone());
        let run = engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "first".into(),
                input: "initial".into(),
            })
            .await
            .expect("start run")
            .run_id;
        tokio::time::timeout(Duration::from_secs(2), reached_rx)
            .await
            .expect("prompt snapshot was not reached")
            .expect("prompt snapshot hook dropped");
        let steering = engine
            .steer(run, "steering input".into())
            .await
            .expect("steer run");
        assert!(steering.accepted);
        hook.release.notify_one();
        provider.first_started.wait().await;
        let (_, mut events) = engine.subscribe(session, None).await.expect("subscribe");
        provider.release_first.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                if matches!(
                    event,
                    EventSubscriptionMessage::Event {
                        event: EventEnvelope {
                            event: Event::RunCompleted { .. },
                            ..
                        }
                    }
                ) {
                    return;
                }
            }
            panic!("event subscription closed before completion");
        })
        .await
        .expect("run did not complete");
        let requests = provider.requests.lock().expect("requests lock poisoned");
        assert_eq!(requests.len(), 2);
        let initial = requests[1]
            .iter()
            .position(|message| {
                matches!(message, ProviderMessage::User { content }
                if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "initial"))
            })
            .expect("initial user input");
        let assistant = requests[1]
            .iter()
            .position(|message| {
                matches!(message, ProviderMessage::Assistant { content, .. }
                if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "done"))
            })
            .expect("first assistant response");
        let steering = requests[1]
            .iter()
            .position(|message| matches!(message, ProviderMessage::User { content }
                if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "steering input")))
            .expect("steering input");
        assert!(initial < assistant && assistant < steering);
    }

    #[test]
    fn three_turn_steering_boundaries_are_durable_and_distinct() {
        let events = vec![
            envelope(
                1,
                Event::RunStarted {
                    client_run_id: "run".into(),
                    input: "initial".into(),
                },
            ),
            envelope(2, Event::TextDelta { text: "one".into() }),
            envelope(
                3,
                Event::UserInputSubmitted {
                    input: "steer-one".into(),
                },
            ),
            envelope(
                4,
                Event::TextDelta {
                    text: "-tail".into(),
                },
            ),
            envelope(5, Event::UserInputApplied { user_input_seq: 3 }),
            envelope(6, Event::TextDelta { text: "two".into() }),
            envelope(
                7,
                Event::UserInputSubmitted {
                    input: "steer-two".into(),
                },
            ),
            envelope(
                8,
                Event::TextDelta {
                    text: "-tail".into(),
                },
            ),
            envelope(9, Event::UserInputApplied { user_input_seq: 7 }),
            envelope(
                10,
                Event::TextDelta {
                    text: "three".into(),
                },
            ),
            envelope(11, Event::RunCompleted { final_text: None }),
        ];
        let expected = vec![
            ("user", "initial".into()),
            ("assistant", "one-tail".into()),
            ("user", "steer-one".into()),
            ("assistant", "two-tail".into()),
            ("user", "steer-two".into()),
            ("assistant", "three".into()),
        ];
        assert_eq!(visible_messages(&assemble_messages(&events)), expected);
        assert_eq!(visible_messages(&assemble_messages(&events)), expected);
    }

    #[tokio::test]
    async fn steering_during_failed_attempt_is_included_in_retry() {
        let provider = Arc::new(RetrySteeringProvider {
            calls: AtomicUsize::new(0),
            first_started: Arc::new(Barrier::new(2)),
            release_first: Notify::new(),
            requests: Mutex::new(Vec::new()),
        });
        let (_directory, engine) = test_engine(provider.clone());
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let run = engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "retry".into(),
                input: "initial".into(),
            })
            .await
            .expect("start run")
            .run_id;
        provider.first_started.wait().await;
        engine
            .steer(run, "retry steering".into())
            .await
            .expect("steer");
        provider.release_first.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if provider.calls.load(Ordering::SeqCst) >= 3 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retry did not start");
        let requests = provider.requests.lock().expect("requests lock poisoned");
        assert!(requests[2].iter().any(|message| {
            matches!(message, ProviderMessage::User { content }
                if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "retry steering"))
        }));
        assert_eq!(
            engine
                .inner
                .store
                .get(session)
                .expect("session")
                .log
                .events()
                .iter()
                .filter(|event| matches!(event.event, Event::UserInputApplied { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn completed_and_reopened_sessions_replay_durable_steering_boundaries() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let run = RunId::new_v7();
        for event in [
            Event::RunStarted {
                client_run_id: "completed".into(),
                input: "initial".into(),
            },
            Event::TextDelta {
                text: "answer".into(),
            },
            Event::UserInputSubmitted {
                input: "steering".into(),
            },
        ] {
            engine
                .append(session, Some(run), event)
                .await
                .expect("append event");
        }
        let events = engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events();
        let steering_seq = events
            .iter()
            .find(|event| matches!(event.event, Event::UserInputSubmitted { .. }))
            .expect("steering event")
            .seq;
        engine
            .append(
                session,
                Some(run),
                Event::UserInputApplied {
                    user_input_seq: steering_seq,
                },
            )
            .await
            .expect("correct boundary");
        engine
            .append(
                session,
                Some(run),
                Event::TextDelta {
                    text: "second".into(),
                },
            )
            .await
            .expect("second answer");
        engine
            .append(
                session,
                Some(run),
                Event::UserInputSubmitted {
                    input: "second steering".into(),
                },
            )
            .await
            .expect("second steering");
        let second_steering_seq = engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events()
            .into_iter()
            .rev()
            .find(|event| matches!(event.event, Event::UserInputSubmitted { .. }))
            .expect("second steering event")
            .seq;
        engine
            .append(
                session,
                Some(run),
                Event::UserInputApplied {
                    user_input_seq: second_steering_seq,
                },
            )
            .await
            .expect("second boundary");
        engine
            .append(
                session,
                Some(run),
                Event::TextDelta {
                    text: "third".into(),
                },
            )
            .await
            .expect("third answer");
        engine
            .append(session, Some(run), Event::RunCompleted { final_text: None })
            .await
            .expect("complete run");
        engine.shutdown().await;
        drop(engine);
        let reopened = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: test_config(),
            providers: HashMap::from([(
                "test".into(),
                Arc::new(NoopProvider) as Arc<dyn Provider>,
            )]),
            tools: Vec::new(),
        })
        .expect("reopen engine");
        let replay = reopened
            .inner
            .store
            .get(session)
            .expect("replayed session")
            .log
            .events();
        assert_eq!(
            visible_messages(&assemble_messages(&replay)),
            vec![
                ("user", "initial".into()),
                ("assistant", "answer".into()),
                ("user", "steering".into()),
                ("assistant", "second".into()),
                ("user", "second steering".into()),
                ("assistant", "third".into()),
            ]
        );
    }

    #[tokio::test]
    async fn resumed_jsonl_rebuild_preserves_applied_steering_boundary() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let run = RunId::new_v7();
        for event in [
            Event::RunStarted {
                client_run_id: "interrupted".into(),
                input: "initial".into(),
            },
            Event::TextDelta {
                text: "partial".into(),
            },
            Event::UserInputSubmitted {
                input: "steering".into(),
            },
        ] {
            engine
                .append(session, Some(run), event)
                .await
                .expect("append event");
        }
        let steering_seq = engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events()
            .into_iter()
            .find(|event| matches!(event.event, Event::UserInputSubmitted { .. }))
            .expect("steering event")
            .seq;
        engine
            .append(
                session,
                Some(run),
                Event::UserInputApplied {
                    user_input_seq: steering_seq,
                },
            )
            .await
            .expect("boundary");
        engine
            .append(
                session,
                Some(run),
                Event::TextDelta {
                    text: "later partial".into(),
                },
            )
            .await
            .expect("later partial");
        engine
            .append(
                session,
                Some(run),
                Event::UserInputSubmitted {
                    input: "later steering".into(),
                },
            )
            .await
            .expect("later steering");
        let later_steering_seq = engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events()
            .into_iter()
            .rev()
            .find(|event| matches!(event.event, Event::UserInputSubmitted { .. }))
            .expect("later steering event")
            .seq;
        engine
            .append(
                session,
                Some(run),
                Event::UserInputApplied {
                    user_input_seq: later_steering_seq,
                },
            )
            .await
            .expect("later boundary");
        engine
            .append(session, Some(run), Event::RunInterrupted { reason: None })
            .await
            .expect("interrupt run");
        engine.shutdown().await;
        drop(engine);
        let reopened = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: test_config(),
            providers: HashMap::from([(
                "test".into(),
                Arc::new(NoopProvider) as Arc<dyn Provider>,
            )]),
            tools: Vec::new(),
        })
        .expect("reopen engine");
        reopened.resume(session).await.expect("resume session");
        let replay = reopened
            .inner
            .store
            .get(session)
            .expect("replayed session")
            .log
            .events();
        assert_eq!(
            visible_messages(&assemble_messages(&replay)),
            vec![
                ("user", "initial".into()),
                ("assistant", "partial".into()),
                ("user", "steering".into()),
                ("assistant", "later partial".into()),
                ("user", "later steering".into()),
            ]
        );
    }

    #[tokio::test]
    async fn resume_resolves_interrupted_calls_through_the_actor() {
        let (_directory, engine) = test_engine(Arc::new(NoopProvider));
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let run = RunId::new_v7();
        let call = ToolCallId::new_v7();
        engine
            .append(
                session,
                Some(run),
                Event::RunStarted {
                    client_run_id: "interrupted".into(),
                    input: "input".into(),
                },
            )
            .await
            .expect("start event");
        engine
            .append(
                session,
                Some(run),
                Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "read".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: None,
                    provider_protocol: None,
                },
            )
            .await
            .expect("tool event");
        engine
            .append(session, Some(run), Event::RunInterrupted { reason: None })
            .await
            .expect("interrupt event");
        engine.resume(session).await.expect("resume");
        let (replay, _live) = engine.subscribe(session, None).await.expect("subscribe");
        assert!(replay.events.into_iter().any(|event| {
            matches!(
                event.event,
                Event::ToolCallFailed { tool_call_id, .. } if tool_call_id == call
            )
        }));
    }

    #[tokio::test]
    async fn terminal_recovery_replays_only_paired_tool_calls_and_results() {
        let provider = Arc::new(RecordingNoopProvider {
            protocol: None,
            requests: Mutex::new(Vec::new()),
        });
        let (_directory, engine) = test_engine(provider.clone());
        for (index, terminal) in [
            Event::RunInterrupted { reason: None },
            Event::RunCancelled { reason: None },
        ]
        .into_iter()
        .enumerate()
        {
            let session = engine
                .create_session(".", "test")
                .expect("create session")
                .id;
            let run = RunId::new_v7();
            let call = ToolCallId::new_v7();
            engine
                .append(
                    session,
                    Some(run),
                    Event::RunStarted {
                        client_run_id: format!("terminal-{index}"),
                        input: "input".into(),
                    },
                )
                .await
                .expect("start event");
            engine
                .append(
                    session,
                    Some(run),
                    Event::ToolCallStarted {
                        tool_call_id: call,
                        tool: "read".into(),
                        arguments: Value::Null,
                        provider_tool_call_id: None,
                        provider_protocol: None,
                    },
                )
                .await
                .expect("tool event");
            engine
                .append(session, Some(run), terminal)
                .await
                .expect("terminal event");
            engine.resume(session).await.expect("recover tool result");
            engine
                .start_run(RunStartParams {
                    session_id: session,
                    client_run_id: format!("follow-up-{index}"),
                    input: "follow up".into(),
                })
                .await
                .expect("start follow-up");
            wait_for_session_status(&engine, session, &SessionStatus::Completed).await;
            let request = provider
                .requests
                .lock()
                .expect("requests lock poisoned")
                .last()
                .cloned()
                .expect("follow-up request");
            let calls: std::collections::HashSet<_> = request
                .persisted_turns
                .iter()
                .flat_map(|turn| match &turn.message {
                    ProviderMessage::Assistant { tool_calls, .. } => tool_calls
                        .iter()
                        .map(|call| call.id.clone())
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .collect();
            let results: std::collections::HashSet<_> = request
                .persisted_turns
                .iter()
                .filter_map(|turn| match &turn.message {
                    ProviderMessage::Tool { result } => Some(result.tool_call_id.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(calls, results, "terminal recovery must preserve only pairs");
            assert_eq!(calls, std::collections::HashSet::from([call.to_string()]));
        }
    }

    #[tokio::test]
    async fn completed_tool_call_and_result_remain_in_the_follow_up_request() {
        let provider = Arc::new(RecordingNoopProvider {
            protocol: None,
            requests: Mutex::new(Vec::new()),
        });
        let (_directory, engine) = test_engine(provider.clone());
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let run = RunId::new_v7();
        let call = ToolCallId::new_v7();
        for event in [
            Event::RunStarted {
                client_run_id: "completed".into(),
                input: "input".into(),
            },
            Event::ToolCallStarted {
                tool_call_id: call,
                tool: "read".into(),
                arguments: Value::Null,
                provider_tool_call_id: None,
                provider_protocol: None,
            },
            Event::ToolCallCompleted {
                tool_call_id: call,
                result: cookie_agent_protocol::ToolResult {
                    content: "result".into(),
                    truncated: false,
                },
            },
            Event::RunCompleted { final_text: None },
        ] {
            engine
                .append(session, Some(run), event)
                .await
                .expect("completed history event");
        }
        engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "follow-up".into(),
                input: "follow up".into(),
            })
            .await
            .expect("start follow-up");
        wait_for_session_status(&engine, session, &SessionStatus::Completed).await;
        let request = provider
            .requests
            .lock()
            .expect("requests lock poisoned")
            .last()
            .cloned()
            .expect("follow-up request");
        assert!(request.persisted_turns.iter().any(|turn| {
            matches!(&turn.message, ProviderMessage::Assistant { tool_calls, .. } if tool_calls.iter().any(|tool| tool.id == call.to_string()))
        }));
        assert!(request.persisted_turns.iter().any(|turn| {
            matches!(&turn.message, ProviderMessage::Tool { result } if result.tool_call_id == call.to_string())
        }));
    }

    #[tokio::test]
    async fn abandoned_tool_call_and_late_native_result_are_omitted_from_request() {
        let provider = Arc::new(RecordingNoopProvider {
            protocol: Some(ProviderProtocol::OpenAiResponses),
            requests: Mutex::new(Vec::new()),
        });
        let (_directory, engine) = test_engine(provider.clone());
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let run = RunId::new_v7();
        let call = ToolCallId::new_v7();
        for event in [
            Event::RunStarted {
                client_run_id: "abandoned".into(),
                input: "input".into(),
            },
            Event::ToolCallStarted {
                tool_call_id: call,
                tool: "read".into(),
                arguments: Value::Null,
                provider_tool_call_id: Some("native-orphan".into()),
                provider_protocol: Some(cookie_agent_protocol::ProviderProtocol::OpenAiResponses),
            },
            Event::AttemptAbandoned,
            Event::ToolCallFailed {
                tool_call_id: call,
                message: "late synthetic failure".into(),
            },
            Event::RunCompleted { final_text: None },
        ] {
            engine
                .append(session, Some(run), event)
                .await
                .expect("abandoned history event");
        }
        engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "follow-up".into(),
                input: "follow up".into(),
            })
            .await
            .expect("start follow-up");
        wait_for_session_status(&engine, session, &SessionStatus::Completed).await;
        let request = provider
            .requests
            .lock()
            .expect("requests lock poisoned")
            .last()
            .cloned()
            .expect("follow-up request");
        assert!(!request.persisted_turns.iter().any(|turn| {
            matches!(&turn.message, ProviderMessage::Assistant { tool_calls, .. } if tool_calls.iter().any(|tool| tool.id == call.to_string() || tool.id == "native-orphan"))
                || matches!(&turn.message, ProviderMessage::Tool { result } if result.tool_call_id == call.to_string() || result.tool_call_id == "native-orphan")
        }));
    }

    #[tokio::test]
    async fn mixed_tool_batch_emits_only_the_call_with_a_result() {
        let provider = Arc::new(RecordingNoopProvider {
            protocol: None,
            requests: Mutex::new(Vec::new()),
        });
        let (_directory, engine) = test_engine(provider.clone());
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let run = RunId::new_v7();
        let paired = ToolCallId::new_v7();
        let unpaired = ToolCallId::new_v7();
        for event in [
            Event::RunStarted {
                client_run_id: "mixed".into(),
                input: "input".into(),
            },
            Event::ToolCallStarted {
                tool_call_id: paired,
                tool: "read".into(),
                arguments: Value::Null,
                provider_tool_call_id: None,
                provider_protocol: None,
            },
            Event::ToolCallStarted {
                tool_call_id: unpaired,
                tool: "write".into(),
                arguments: Value::Null,
                provider_tool_call_id: None,
                provider_protocol: None,
            },
            Event::ToolCallCompleted {
                tool_call_id: paired,
                result: cookie_agent_protocol::ToolResult {
                    content: "result".into(),
                    truncated: false,
                },
            },
            Event::RunCompleted { final_text: None },
        ] {
            engine
                .append(session, Some(run), event)
                .await
                .expect("mixed history event");
        }
        engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "follow-up".into(),
                input: "follow up".into(),
            })
            .await
            .expect("start follow-up");
        wait_for_session_status(&engine, session, &SessionStatus::Completed).await;
        let request = provider
            .requests
            .lock()
            .expect("requests lock poisoned")
            .last()
            .cloned()
            .expect("follow-up request");
        let calls: std::collections::HashSet<_> = request
            .persisted_turns
            .iter()
            .flat_map(|turn| match &turn.message {
                ProviderMessage::Assistant { tool_calls, .. } => tool_calls
                    .iter()
                    .map(|tool| tool.id.clone())
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        let results: std::collections::HashSet<_> = request
            .persisted_turns
            .iter()
            .filter_map(|turn| match &turn.message {
                ProviderMessage::Tool { result } => Some(result.tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(calls, std::collections::HashSet::from([paired.to_string()]));
        assert_eq!(results, calls);
        assert!(!calls.contains(&unpaired.to_string()));
    }

    #[tokio::test]
    async fn delayed_result_is_replayed_next_to_its_call_before_later_turns() {
        let provider = Arc::new(RecordingNoopProvider {
            protocol: None,
            requests: Mutex::new(Vec::new()),
        });
        let (_directory, engine) = test_engine(provider.clone());
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let interrupted_run = RunId::new_v7();
        let later_run = RunId::new_v7();
        let call = ToolCallId::new_v7();
        for (run, event) in [
            (
                interrupted_run,
                Event::RunStarted {
                    client_run_id: "interrupted".into(),
                    input: "original user".into(),
                },
            ),
            (
                interrupted_run,
                Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "read".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: None,
                    provider_protocol: None,
                },
            ),
            (interrupted_run, Event::RunInterrupted { reason: None }),
            (
                later_run,
                Event::RunStarted {
                    client_run_id: "later".into(),
                    input: "intervening user".into(),
                },
            ),
            (
                later_run,
                Event::TextDelta {
                    text: "intervening model".into(),
                },
            ),
            (later_run, Event::RunCompleted { final_text: None }),
            (
                interrupted_run,
                Event::ToolCallFailed {
                    tool_call_id: call,
                    message: "late synthetic failure".into(),
                },
            ),
        ] {
            engine
                .append(session, Some(run), event)
                .await
                .expect("delayed history event");
        }
        engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "follow-up".into(),
                input: "follow up".into(),
            })
            .await
            .expect("start follow-up");
        wait_for_session_status(&engine, session, &SessionStatus::Completed).await;
        let request = provider
            .requests
            .lock()
            .expect("requests lock poisoned")
            .last()
            .cloned()
            .expect("follow-up request");
        let call_index = request
            .persisted_turns
            .iter()
            .position(|turn| {
                matches!(&turn.message, ProviderMessage::Assistant { tool_calls, .. } if tool_calls.iter().any(|tool| tool.id == call.to_string()))
            })
            .expect("assistant call");
        assert!(matches!(
            &request.persisted_turns[call_index + 1].message,
            ProviderMessage::Tool { result } if result.tool_call_id == call.to_string()
        ));
        let later_user_index = request
            .persisted_turns
            .iter()
            .position(|turn| {
                matches!(&turn.message, ProviderMessage::User { content } if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "intervening user"))
            })
            .expect("intervening user");
        assert!(call_index + 1 < later_user_index);
    }

    #[tokio::test]
    async fn repeated_tool_id_keeps_only_the_occurrence_with_a_result() {
        let provider = Arc::new(RecordingNoopProvider {
            protocol: None,
            requests: Mutex::new(Vec::new()),
        });
        let (_directory, engine) = test_engine(provider.clone());
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let shared_call = ToolCallId::new_v7();
        let first_run = RunId::new_v7();
        let second_run = RunId::new_v7();
        for (run, event) in [
            (
                first_run,
                Event::RunStarted {
                    client_run_id: "first".into(),
                    input: "first user".into(),
                },
            ),
            (
                first_run,
                Event::ToolCallStarted {
                    tool_call_id: shared_call,
                    tool: "read".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: None,
                    provider_protocol: None,
                },
            ),
            (first_run, Event::RunCompleted { final_text: None }),
            (
                second_run,
                Event::RunStarted {
                    client_run_id: "second".into(),
                    input: "second user".into(),
                },
            ),
            (
                second_run,
                Event::ToolCallStarted {
                    tool_call_id: shared_call,
                    tool: "write".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: None,
                    provider_protocol: None,
                },
            ),
            (
                second_run,
                Event::ToolCallFailed {
                    tool_call_id: shared_call,
                    message: "paired second occurrence".into(),
                },
            ),
            (second_run, Event::RunCompleted { final_text: None }),
        ] {
            engine
                .append(session, Some(run), event)
                .await
                .expect("repeated-id history event");
        }
        engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "follow-up".into(),
                input: "follow up".into(),
            })
            .await
            .expect("start follow-up");
        wait_for_session_status(&engine, session, &SessionStatus::Completed).await;
        let request = provider
            .requests
            .lock()
            .expect("requests lock poisoned")
            .last()
            .cloned()
            .expect("follow-up request");
        let calls: Vec<_> = request
            .persisted_turns
            .iter()
            .flat_map(|turn| match &turn.message {
                ProviderMessage::Assistant { tool_calls, .. } => tool_calls
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        let results: Vec<_> = request
            .persisted_turns
            .iter()
            .filter_map(|turn| match &turn.message {
                ProviderMessage::Tool { result } => Some(result.content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(calls, vec!["write"]);
        assert_eq!(results, vec!["paired second occurrence"]);
    }

    #[tokio::test]
    async fn delayed_repeated_id_results_match_their_own_run_occurrence() {
        let provider = Arc::new(RecordingNoopProvider {
            protocol: Some(ProviderProtocol::OpenAiResponses),
            requests: Mutex::new(Vec::new()),
        });
        let (_directory, engine) = test_engine(provider.clone());
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let shared_call = ToolCallId::new_v7();
        let first_run = RunId::new_v7();
        let second_run = RunId::new_v7();
        let orphan_run = RunId::new_v7();
        for (run, event) in [
            (
                first_run,
                Event::RunStarted {
                    client_run_id: "first".into(),
                    input: "first user".into(),
                },
            ),
            (
                first_run,
                Event::ToolCallStarted {
                    tool_call_id: shared_call,
                    tool: "read".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: Some("native-first".into()),
                    provider_protocol: Some(
                        cookie_agent_protocol::ProviderProtocol::OpenAiResponses,
                    ),
                },
            ),
            (first_run, Event::RunInterrupted { reason: None }),
            (
                second_run,
                Event::RunStarted {
                    client_run_id: "second".into(),
                    input: "second user".into(),
                },
            ),
            (
                second_run,
                Event::ToolCallStarted {
                    tool_call_id: shared_call,
                    tool: "write".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: Some("native-second".into()),
                    provider_protocol: Some(
                        cookie_agent_protocol::ProviderProtocol::OpenAiResponses,
                    ),
                },
            ),
            (second_run, Event::RunInterrupted { reason: None }),
            (
                second_run,
                Event::ToolCallFailed {
                    tool_call_id: shared_call,
                    message: "second delayed result".into(),
                },
            ),
            (
                first_run,
                Event::ToolCallFailed {
                    tool_call_id: shared_call,
                    message: "first delayed result".into(),
                },
            ),
            (
                orphan_run,
                Event::ToolCallFailed {
                    tool_call_id: shared_call,
                    message: "orphaned delayed result".into(),
                },
            ),
        ] {
            engine
                .append(session, Some(run), event)
                .await
                .expect("repeated delayed history event");
        }
        engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "follow-up".into(),
                input: "follow up".into(),
            })
            .await
            .expect("start follow-up");
        wait_for_session_status(&engine, session, &SessionStatus::Completed).await;
        let request = provider
            .requests
            .lock()
            .expect("requests lock poisoned")
            .last()
            .cloned()
            .expect("follow-up request");
        let pairs: Vec<_> = request
            .persisted_turns
            .windows(2)
            .filter_map(|entries| match (&entries[0].message, &entries[1].message) {
                (
                    ProviderMessage::Assistant { tool_calls, .. },
                    ProviderMessage::Tool { result },
                ) if tool_calls.len() == 1 => {
                    Some((tool_calls[0].id.clone(), result.content.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("native-first".into(), "first delayed result".into()),
                ("native-second".into(), "second delayed result".into()),
            ]
        );
        assert!(!request.persisted_turns.iter().any(|turn| {
            matches!(&turn.message, ProviderMessage::Tool { result } if result.content == "orphaned delayed result")
        }));
    }

    #[tokio::test]
    async fn terminal_event_overflow_emits_a_gap() {
        let (_directory, engine) = test_engine(Arc::new(NoopProvider));
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let (_, mut live) = engine.subscribe(session, None).await.expect("subscribe");
        for index in 0..255 {
            engine
                .append(
                    session,
                    None,
                    Event::TextDelta {
                        text: index.to_string(),
                    },
                )
                .await
                .expect("append");
        }
        let (reached, wait_for_gap) = std_mpsc::channel();
        let (release_gap, released) = std_mpsc::channel();
        *engine
            .inner
            .gap_send_hook
            .lock()
            .expect("gap send hook lock poisoned") = Some(GapSendHook {
            reached,
            release: released,
        });
        let append_engine = engine.clone();
        let terminal = tokio::spawn(async move {
            append_engine
                .append(session, None, Event::RunCompleted { final_text: None })
                .await
        });
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || wait_for_gap.recv()),
        )
        .await
        .expect("gap was not sent")
        .expect("gap wait task panicked")
        .expect("gap sender dropped");
        let drain = tokio::spawn(async move {
            let mut gap = None;
            loop {
                match live.recv().await {
                    Some(EventSubscriptionMessage::Gap {
                        session_id,
                        last_delivered_seq,
                    }) => {
                        assert_eq!(session_id, session);
                        gap = Some(last_delivered_seq);
                        let _ = release_gap.send(());
                    }
                    Some(EventSubscriptionMessage::Event { .. }) => {}
                    None => break,
                }
            }
            (gap, true)
        });
        terminal
            .await
            .expect("terminal append task panicked")
            .expect("terminal append");
        let (gap, closed) = tokio::time::timeout(Duration::from_secs(2), drain)
            .await
            .expect("live subscription did not close")
            .expect("drain task panicked");
        assert_eq!(gap, Some(256));
        assert!(closed, "subscription did not close after Gap");
    }

    #[tokio::test]
    async fn reopen_reserved_delegation_without_child_resolves_structured_failure_once() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let (parent, _parent_run, _call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let child = SessionId::new_v7();
        let invocation_id = write_started_delegation(&engine, &invocation, child, &policy);
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(NoopProvider)).await;
        assert!(
            reopened.inner.store.get(child).is_err(),
            "missing child was recreated"
        );
        reopened.resume(parent).await.expect("resume parent");
        reopened.resume(parent).await.expect("repeat resume parent");

        let completions: Vec<_> = reopened
            .inner
            .store
            .get(parent)
            .expect("parent projection")
            .log
            .events()
            .into_iter()
            .filter_map(|event| match event.event {
                Event::ToolCallCompleted { result, .. } => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(completions.len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&completions[0].content).expect("structured failure")["status"],
            "failed"
        );
        assert_eq!(
            journal_records(&reopened)
                .iter()
                .filter(|record| matches!(record, journal::JournalRecord::DelegationStarted { reservation, .. } if reservation.invocation_id == invocation_id))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn reopen_inside_runtime_interrupts_a_partial_run_without_panicking() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let session = engine.create_session(".", "test").expect("session").id;
        let run = RunId::new_v7();
        engine
            .append(
                session,
                Some(run),
                Event::RunStarted {
                    client_run_id: "crashed".into(),
                    input: "before restart".into(),
                },
            )
            .await
            .expect("persist run start");
        engine
            .append(
                session,
                Some(run),
                Event::TextDelta {
                    text: "partial output".into(),
                },
            )
            .await
            .expect("persist partial output");
        drop(engine);

        let reopened = reopen_test_engine_in_runtime(&directory, Arc::new(NoopProvider));
        let projection = reopened
            .inner
            .store
            .get(session)
            .expect("reconciled session");
        assert_eq!(
            projection.runs.get(&run).expect("interrupted run").status,
            SessionStatus::Interrupted
        );
        assert!(projection.log.events().iter().any(|event| {
            matches!(event.event, Event::RunInterrupted { ref reason }
                if reason.as_deref() == Some("daemon restart"))
        }));
        reopened
            .resume(session)
            .await
            .expect("resume interrupted session");
        reopened
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "continued".into(),
                input: "continue".into(),
            })
            .await
            .expect("continue after reconciliation");
        wait_for_session_status(&reopened, session, &SessionStatus::Completed).await;
    }

    #[tokio::test]
    async fn reopen_inside_runtime_interrupts_a_run_with_pending_approval_without_panicking() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let session = engine.create_session(".", "test").expect("session").id;
        let run = RunId::new_v7();
        engine
            .append(
                session,
                Some(run),
                Event::RunStarted {
                    client_run_id: "approval-crash".into(),
                    input: "await approval".into(),
                },
            )
            .await
            .expect("persist run start");
        engine
            .append(
                session,
                Some(run),
                approval_request_event(
                    "pending-restart-approval",
                    vec![cookie_agent_protocol::ApprovalResource {
                        action: cookie_agent_protocol::ActionKind::Bash,
                        resource: "git status".into(),
                        suggested_pattern: "git status *".into(),
                    }],
                    "awaiting approval",
                ),
            )
            .await
            .expect("persist approval request");
        drop(engine);

        let reopened = reopen_test_engine_in_runtime(&directory, Arc::new(NoopProvider));
        let projection = reopened
            .inner
            .store
            .get(session)
            .expect("reconciled session");
        assert_eq!(
            projection.runs.get(&run).expect("interrupted run").status,
            SessionStatus::Interrupted
        );
        assert!(projection.log.events().iter().any(|event| {
            matches!(&event.event, Event::ApprovalRequested { approval_id, .. }
                if approval_id == "pending-restart-approval")
        }));
        reopened
            .resume(session)
            .await
            .expect("resume approval session");
    }

    #[tokio::test]
    async fn reopen_inside_runtime_replays_committed_prefix_before_continuation() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let session = engine.create_session(".", "test").expect("session").id;
        let run = RunId::new_v7();
        engine
            .append(
                session,
                Some(run),
                Event::RunStarted {
                    client_run_id: "crashed".into(),
                    input: "original input".into(),
                },
            )
            .await
            .expect("persist run start");
        engine
            .append(
                session,
                Some(run),
                Event::TextDelta {
                    text: "committed prefix".into(),
                },
            )
            .await
            .expect("persist partial output");
        engine
            .append(
                session,
                Some(run),
                Event::UserInputSubmitted {
                    input: "preserve committed prefix".into(),
                },
            )
            .await
            .expect("persist steering input");
        let steering_seq = engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events()
            .into_iter()
            .rev()
            .find(|event| matches!(event.event, Event::UserInputSubmitted { .. }))
            .expect("steering event")
            .seq;
        engine
            .append(
                session,
                Some(run),
                Event::UserInputApplied {
                    user_input_seq: steering_seq,
                },
            )
            .await
            .expect("persist steering boundary");
        drop(engine);

        let provider = Arc::new(RecordingNoopProvider {
            protocol: None,
            requests: Mutex::new(Vec::new()),
        });
        let reopened = reopen_test_engine_in_runtime(&directory, provider.clone());
        reopened
            .resume(session)
            .await
            .expect("resume interrupted session");
        reopened
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "continued".into(),
                input: "continue from prefix".into(),
            })
            .await
            .expect("start continuation");
        wait_for_session_status(&reopened, session, &SessionStatus::Completed).await;
        let request = provider
            .requests
            .lock()
            .expect("requests lock poisoned")
            .last()
            .cloned()
            .expect("continuation request");
        assert_eq!(
            visible_messages(&request.messages),
            vec![
                ("user", "original input".into()),
                ("assistant", "committed prefix".into()),
                ("user", "preserve committed prefix".into()),
                ("user", "continue from prefix".into()),
            ]
        );
    }

    #[tokio::test]
    async fn reopen_repairs_missing_parent_link_exactly_once() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let (parent, _parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let child = SessionId::new_v7();
        let invocation_id = write_started_delegation(&engine, &invocation, child, &policy);
        persist_delegated_child(&engine, &invocation, child, policy);
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(NoopProvider)).await;
        reconcile_test_engine(&reopened).await;
        let parent_events = reopened
            .inner
            .store
            .get(parent)
            .expect("parent projection")
            .log
            .events();
        assert_eq!(
            parent_events
                .iter()
                .filter(|event| {
                    matches!(event.event, Event::ToolCallLinked { tool_call_id, child_session_id }
                        if tool_call_id == call && child_session_id == child)
                })
                .count(),
            1
        );
        assert_eq!(journal_link_count(&reopened, invocation_id), 1);
    }

    #[tokio::test]
    async fn reopen_confirms_link_and_redelivery_uses_existing_child() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let (parent, parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let child = SessionId::new_v7();
        let invocation_id = write_started_delegation(&engine, &invocation, child, &policy);
        persist_delegated_child(&engine, &invocation, child, policy);
        engine
            .append(
                parent,
                Some(parent_run),
                Event::ToolCallLinked {
                    tool_call_id: call,
                    child_session_id: child,
                },
            )
            .await
            .expect("persist parent link");
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(NoopProvider)).await;
        assert_eq!(journal_link_count(&reopened, invocation_id), 1);
        let handle = reopened
            .delegate_invoke(invocation)
            .await
            .expect("redeliver delegate invocation");
        assert_eq!(handle.child_session_id, child);
        assert_eq!(journal_link_count(&reopened, invocation_id), 1);
        assert_eq!(
            reopened
                .children(parent)
                .into_iter()
                .filter(|summary| summary.id == child)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn reopen_confirms_existing_child_run_without_double_starting_it() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let (parent, _parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let child = SessionId::new_v7();
        let invocation_id = write_started_delegation(&engine, &invocation, child, &policy);
        persist_delegated_child(&engine, &invocation, child, policy);
        write_linked_delegation(&engine, invocation_id);
        let child_run = RunId::new_v7();
        append_child_event(
            &engine,
            child,
            child_run,
            Event::RunStarted {
                client_run_id: delegate_client_run_id(invocation_id),
                input: render_delegate_input(&delegate_request(&invocation)),
            },
        );
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(NoopProvider)).await;
        reopened.resume(parent).await.expect("resume parent");
        reopened.resume(parent).await.expect("repeat resume parent");
        let result = wait_for_delegate_completion(&reopened, parent, call).await;
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).expect("structured failure")["status"],
            "failed"
        );
        assert_eq!(child_run_start_count(&reopened, child), 1);
        assert_eq!(journal_run_count(&reopened, invocation_id, child_run), 1);
    }

    #[tokio::test]
    async fn reopen_reconstructs_completed_delegate_result_once_with_utf8_boundary() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let (parent, _parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let limit = policy.result_limits.delegate_result_bytes;
        let child = SessionId::new_v7();
        let invocation_id = write_started_delegation(&engine, &invocation, child, &policy);
        persist_delegated_child(&engine, &invocation, child, policy);
        write_linked_delegation(&engine, invocation_id);
        let child_run = RunId::new_v7();
        append_child_event(
            &engine,
            child,
            child_run,
            Event::RunStarted {
                client_run_id: delegate_client_run_id(invocation_id),
                input: "completed child".into(),
            },
        );
        append_child_event(
            &engine,
            child,
            child_run,
            Event::RunCompleted {
                final_text: Some(format!("{}é", "x".repeat(limit.saturating_sub(1)))),
            },
        );
        write_run_started_delegation(&engine, invocation_id, child_run);
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(NoopProvider)).await;
        reopened.resume(parent).await.expect("resume parent");
        reopened.resume(parent).await.expect("repeat resume parent");
        let results: Vec<_> = reopened
            .inner
            .store
            .get(parent)
            .expect("parent projection")
            .log
            .events()
            .into_iter()
            .filter_map(|event| match event.event {
                Event::ToolCallCompleted {
                    tool_call_id,
                    result,
                } if tool_call_id == call => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].truncated);
        assert_eq!(results[0].content, "x".repeat(limit.saturating_sub(1)));
        assert!(results[0].content.len() <= limit);
    }

    #[tokio::test]
    async fn reopen_marks_journalless_orphan_interrupted_without_hiding_journaled_child() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let parent = engine
            .create_session(".", "test")
            .expect("create parent")
            .id;
        let run = RunId::new_v7();
        let call = ToolCallId::new_v7();
        let invocation = delegate_invocation(parent, run, call, "journaled child");
        let policy = child_policy_for(&engine, &invocation);
        let journaled_child = SessionId::new_v7();
        write_started_delegation(&engine, &invocation, journaled_child, &policy);
        persist_delegated_child(&engine, &invocation, journaled_child, policy.clone());
        let orphan = SessionId::new_v7();
        let orphan_meta = session_meta(
            orphan,
            SessionOrigin::Delegated {
                root_session_id: parent,
                parent_session_id: parent,
                parent_run_id: RunId::new_v7(),
                parent_tool_call_id: ToolCallId::new_v7(),
                invocation_id: InvocationId(Uuid::from_u128(42)),
                depth: 1,
            },
            directory.path(),
            &policy,
        );
        engine
            .inner
            .store
            .create(orphan_meta, policy)
            .expect("persist orphan child");
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(NoopProvider)).await;
        reconcile_test_engine(&reopened).await;
        assert_eq!(
            reopened
                .inner
                .store
                .get(orphan)
                .expect("orphan projection")
                .status,
            SessionStatus::Interrupted
        );
        let children = reopened.children(parent);
        assert!(children.iter().any(|child| child.id == journaled_child));
        assert!(!children.iter().any(|child| child.id == orphan));
        assert_eq!(
            reopened
                .inner
                .store
                .get(orphan)
                .expect("orphan projection")
                .log
                .events()
                .into_iter()
                .filter(|event| matches!(event.event, Event::RunInterrupted { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn reopen_does_not_autostart_reserved_child_after_parent_cancellation() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let (parent, parent_run, _call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let child = SessionId::new_v7();
        let invocation_id = write_started_delegation(&engine, &invocation, child, &policy);
        engine
            .append(
                parent,
                Some(parent_run),
                Event::RunCancelled { reason: None },
            )
            .await
            .expect("cancel parent");
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(NoopProvider)).await;
        reconcile_test_engine(&reopened).await;
        assert!(
            reopened.inner.store.get(child).is_err(),
            "reserved child became a zombie"
        );
        assert_eq!(
            journal_records(&reopened)
                .iter()
                .filter(|record| {
                    matches!(record, journal::JournalRecord::DelegationRunStarted { invocation_id: id, .. } if *id == invocation_id)
                })
                .count(),
            0
        );
        reopened
            .resume(parent)
            .await
            .expect("resume cancelled parent");
        reopened
            .resume(parent)
            .await
            .expect("repeat cancelled resume");
        let cancelled_results = reopened
            .inner
            .store
            .get(parent)
            .expect("parent")
            .log
            .events()
            .into_iter()
            .filter(|event| {
                matches!(&event.event, Event::ToolCallCompleted { result, .. }
                if serde_json::from_str::<Value>(&result.content).ok()
                    .is_some_and(|value| value["status"] == "cancelled"))
            })
            .count();
        assert_eq!(cancelled_results, 1);
    }

    #[tokio::test]
    async fn live_redelivery_after_parent_cancellation_never_creates_child() {
        let (_directory, engine) = test_engine(Arc::new(NoopProvider));
        let (parent, parent_run, _call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let child = SessionId::new_v7();
        write_started_delegation(&engine, &invocation, child, &policy);
        engine
            .append(
                parent,
                Some(parent_run),
                Event::RunCancelled { reason: None },
            )
            .await
            .expect("cancel parent");
        assert!(engine.delegate_invoke(invocation).await.is_err());
        assert!(engine.inner.store.get(child).is_err());
        let parent = engine.inner.store.get(parent).expect("parent");
        assert!(parent.log.events().into_iter().any(|event| {
            matches!(&event.event, Event::ToolCallCompleted { result, .. }
                if serde_json::from_str::<Value>(&result.content).ok()
                    .is_some_and(|value| value["status"] == "cancelled"))
        }));
    }

    #[tokio::test]
    async fn reopen_cancels_started_child_for_cancelled_parent_once() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let (parent, parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let child = SessionId::new_v7();
        write_started_delegation(&engine, &invocation, child, &policy);
        persist_delegated_child(&engine, &invocation, child, policy);
        engine
            .append(
                parent,
                Some(parent_run),
                Event::ToolCallLinked {
                    tool_call_id: call,
                    child_session_id: child,
                },
            )
            .await
            .expect("link child");
        let child_run = RunId::new_v7();
        append_child_event(
            &engine,
            child,
            child_run,
            Event::RunStarted {
                client_run_id: "delegate:crash-window".into(),
                input: "child task".into(),
            },
        );
        engine
            .append(
                parent,
                Some(parent_run),
                Event::RunCancelled { reason: None },
            )
            .await
            .expect("cancel parent");
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(NoopProvider)).await;
        assert_eq!(
            reopened.inner.store.get(child).expect("child").status,
            SessionStatus::Cancelled
        );
        reopened.resume(parent).await.expect("resume parent");
        reopened.resume(parent).await.expect("repeat resume parent");
        let completed = reopened
            .inner
            .store
            .get(parent)
            .expect("parent")
            .log
            .events()
            .into_iter()
            .filter(|event| {
                matches!(&event.event, Event::ToolCallCompleted { tool_call_id, result }
                if *tool_call_id == call
                    && serde_json::from_str::<Value>(&result.content).ok()
                        .is_some_and(|value| value["status"] == "cancelled"))
            })
            .count();
        assert_eq!(completed, 1);
    }

    #[tokio::test]
    async fn reopen_linked_unstarted_child_starts_once_and_commits_delegate_result() {
        let (directory, engine) = test_engine(Arc::new(ReportProvider));
        let (parent, _parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let child = SessionId::new_v7();
        let invocation_id = write_started_delegation(&engine, &invocation, child, &policy);
        persist_delegated_child(&engine, &invocation, child, policy);
        engine
            .append(
                parent,
                Some(invocation.parent_run_id),
                Event::ToolCallLinked {
                    tool_call_id: call,
                    child_session_id: child,
                },
            )
            .await
            .expect("persist parent link");
        write_linked_delegation(&engine, invocation_id);
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(ReportProvider)).await;
        reopened.resume(parent).await.expect("resume parent");
        let result = wait_for_delegate_completion(&reopened, parent, call).await;
        assert_eq!(result.content, "child report");
        assert!(!result.truncated);
        assert_eq!(child_run_start_count(&reopened, child), 1);
        assert_eq!(
            reopened
                .inner
                .store
                .get(parent)
                .expect("parent")
                .log
                .events()
                .into_iter()
                .filter(|event| {
                    matches!(event.event, Event::ToolCallCompleted { tool_call_id, .. } if tool_call_id == call)
                })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn reopen_pending_delegate_without_journal_resolves_failure_without_fabricating_state() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let (parent, _parent_run, call, _invocation) = pending_delegate_parent(&engine).await;
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(NoopProvider)).await;
        reopened.resume(parent).await.expect("resume parent");
        let result = wait_for_delegate_completion(&reopened, parent, call).await;
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).expect("structured failure")["status"],
            "failed"
        );
        assert!(journal_records(&reopened).is_empty());
        assert_eq!(reopened.list_sessions().len(), 1);
    }

    #[test]
    fn journal_reservation_append_failure_requires_reopen_before_retry() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing_parent = directory.path().join("missing");
        let journal_path = missing_parent.join("delegations.jsonl");
        let journal = DelegationJournal::open(journal_path.clone()).expect("open journal");
        let invocation = InvocationId(Uuid::from_u128(7));
        let parent = SessionId::new_v7();
        let run = RunId::new_v7();
        let call = ToolCallId::new_v7();
        let policy = test_config()
            .materialize_policy("worker")
            .expect("worker policy");
        let request = journal::DelegateRequestPayload {
            task: "task".into(),
            ..journal::DelegateRequestPayload::default()
        };

        let failed = journal.reserve(
            invocation,
            parent,
            run,
            call,
            policy.clone(),
            "fingerprint".into(),
            request.clone(),
        );
        assert!(matches!(
            failed,
            Err(JournalError::Event(events::EventLogError::Io { .. }))
        ));
        assert!(
            journal.get(invocation).is_none(),
            "failed reservation remained live"
        );
        assert!(
            journal.entries().is_empty(),
            "failed reservation leaked into index"
        );

        assert!(matches!(
            journal.reserve(
                invocation,
                parent,
                run,
                call,
                policy.clone(),
                "fingerprint".into(),
                request.clone(),
            ),
            Err(JournalError::Poisoned)
        ));
        journal.shutdown();
        std::fs::create_dir(&missing_parent).expect("create journal parent");
        let reopened = DelegationJournal::open(journal_path.clone()).expect("reopen journal");
        let entry = reopened
            .reserve(
                invocation,
                parent,
                run,
                call,
                policy,
                "fingerprint".into(),
                request,
            )
            .expect("retry reservation");
        assert_eq!(
            reopened
                .get(invocation)
                .expect("retry reservation indexed")
                .reservation,
            entry.reservation
        );
        assert_eq!(
            events::load_jsonl::<journal::JournalRecord>(&journal_path)
                .expect("read journal")
                .len(),
            1
        );
        reopened.shutdown();
    }

    #[tokio::test]
    async fn reopen_discards_mid_line_delegation_journal_torn_tail() {
        let (directory, engine) = test_engine(Arc::new(ReportProvider));
        let (parent, _parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let child = SessionId::new_v7();
        let invocation_id = write_started_delegation(&engine, &invocation, child, &policy);
        persist_delegated_child(&engine, &invocation, child, policy);
        write_linked_delegation(&engine, invocation_id);
        append_torn_journal_tail(&engine, b"{\"type\":\"delegation_run_started");
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(ReportProvider)).await;
        assert_eq!(journal_records(&reopened).len(), 2);
        assert_eq!(
            reopened
                .inner
                .journal
                .get(invocation_id)
                .expect("preserved reservation")
                .reservation
                .child_session_id,
            child
        );
        reopened.resume(parent).await.expect("resume parent");
        assert_eq!(
            wait_for_delegate_completion(&reopened, parent, call)
                .await
                .content,
            "child report"
        );
        assert_eq!(child_run_start_count(&reopened, child), 1);
    }

    #[tokio::test]
    async fn reopen_discards_partial_json_tail_after_complete_delegation_records() {
        let (directory, engine) = test_engine(Arc::new(ReportProvider));
        let (parent, _parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let child = SessionId::new_v7();
        let invocation_id = write_started_delegation(&engine, &invocation, child, &policy);
        persist_delegated_child(&engine, &invocation, child, policy);
        write_linked_delegation(&engine, invocation_id);
        append_torn_journal_tail(
            &engine,
            b"{\"type\":\"delegation_started\",\"reservation\":{\"invocation_id\":",
        );
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(ReportProvider)).await;
        let records = journal_records(&reopened);
        assert_eq!(records.len(), 2);
        assert!(matches!(
            records[0],
            journal::JournalRecord::DelegationStarted { ref reservation, .. }
                if reservation.invocation_id == invocation_id
        ));
        assert!(matches!(
            records[1],
            journal::JournalRecord::DelegationLinked { invocation_id: id } if id == invocation_id
        ));
        reopened.resume(parent).await.expect("resume parent");
        assert_eq!(
            wait_for_delegate_completion(&reopened, parent, call)
                .await
                .content,
            "child report"
        );
        assert_eq!(child_run_start_count(&reopened, child), 1);
    }

    #[tokio::test]
    async fn mark_linked_poison_resolves_parent_once_without_starting_child() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let (parent, parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let entry = reserve_live_delegation(&engine, &invocation, policy.clone()).await;
        let child = entry.reservation.child_session_id;
        persist_delegated_child(&engine, &invocation, child, policy);
        engine.spawn_actor(child);
        engine
            .append(
                parent,
                Some(parent_run),
                Event::ToolCallLinked {
                    tool_call_id: call,
                    child_session_id: child,
                },
            )
            .await
            .expect("persist parent link");
        let saved = obstruct_journal_appends(&engine);
        let journal = engine.inner.journal.clone();
        let link_error = tokio::task::spawn_blocking(move || {
            journal.mark_linked(entry.reservation.invocation_id)
        })
        .await
        .expect("mark link task");
        assert!(matches!(
            link_error,
            Err(JournalError::Event(events::EventLogError::Io { .. }))
        ));
        restore_journal_path(&engine, &saved);

        let invocation_id = entry.reservation.invocation_id;
        assert!(
            engine.inner.journal.get(invocation_id).is_some(),
            "poison hid durable reservation"
        );
        assert!(matches!(
            engine
                .inner
                .journal
                .mark_run_started(invocation_id, RunId::new_v7()),
            Err(JournalError::Poisoned)
        ));
        assert!(matches!(
            engine.delegate_invoke(invocation).await,
            Err(EngineError::Journal(JournalError::Poisoned))
        ));
        let result = wait_for_delegate_completion(&engine, parent, call).await;
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).expect("structured failure")["status"],
            "failed"
        );
        assert_eq!(child_run_start_count(&engine, child), 0);
        assert_eq!(
            engine
                .inner
                .store
                .get(parent)
                .expect("parent")
                .log
                .events()
                .into_iter()
                .filter(|event| {
                    matches!(event.event, Event::ToolCallCompleted { tool_call_id, .. } if tool_call_id == call)
                })
                .count(),
            1
        );
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(NoopProvider)).await;
        assert_eq!(journal_link_count(&reopened, invocation_id), 1);
        assert_eq!(child_run_start_count(&reopened, child), 0);
    }

    #[tokio::test]
    async fn first_run_confirmation_failure_resolves_parent_and_cancels_child_once() {
        let provider = Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            release: Notify::new(),
        });
        let (directory, engine) = test_engine(provider);
        let (parent, parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let entry = reserve_live_delegation(&engine, &invocation, policy.clone()).await;
        let child = entry.reservation.child_session_id;
        persist_delegated_child(&engine, &invocation, child, policy);
        engine.spawn_actor(child);
        engine
            .append(
                parent,
                Some(parent_run),
                Event::ToolCallLinked {
                    tool_call_id: call,
                    child_session_id: child,
                },
            )
            .await
            .expect("persist parent link");
        engine
            .inner
            .journal
            .mark_linked(entry.reservation.invocation_id)
            .expect("confirm parent link");
        let saved = obstruct_journal_appends(&engine);
        let error = engine.delegate_invoke(invocation).await;
        restore_journal_path(&engine, &saved);
        assert!(
            matches!(
                error,
                Err(EngineError::Journal(JournalError::Event(
                    events::EventLogError::Io { .. }
                )))
            ),
            "unexpected run-confirmation result: {error:?}"
        );
        wait_for_session_status(&engine, child, &SessionStatus::Cancelled).await;
        let result = wait_for_delegate_completion(&engine, parent, call).await;
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).expect("structured failure")["status"],
            "failed"
        );
        assert_eq!(
            engine
                .inner
                .store
                .get(child)
                .expect("child")
                .log
                .events()
                .into_iter()
                .filter(|event| matches!(event.event, Event::RunCancelled { .. }))
                .count(),
            1
        );
        assert!(matches!(
            engine
                .inner
                .journal
                .mark_run_started(entry.reservation.invocation_id, RunId::new_v7()),
            Err(JournalError::Poisoned)
        ));
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(
            &directory,
            Arc::new(BlockingProvider {
                calls: AtomicUsize::new(0),
                release: Notify::new(),
            }),
        )
        .await;
        assert_eq!(
            reopened.inner.store.get(child).expect("child").status,
            SessionStatus::Cancelled
        );
        reopened.resume(parent).await.expect("resume parent");
        reopened.resume(parent).await.expect("repeat resume parent");
        assert_eq!(
            reopened
                .inner
                .store
                .get(parent)
                .expect("parent")
                .log
                .events()
                .into_iter()
                .filter(|event| {
                    matches!(event.event, Event::ToolCallCompleted { tool_call_id, .. } if tool_call_id == call)
                })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn poisoned_journal_keeps_reads_available_but_rejects_all_mutations() {
        let (_directory, engine) = test_engine(Arc::new(NoopProvider));
        let journal = engine.inner.journal.clone();
        let invocation = InvocationId(Uuid::from_u128(91));
        let policy = test_config()
            .materialize_policy("worker")
            .expect("worker policy");
        let entry = journal
            .reserve(
                invocation,
                SessionId::new_v7(),
                RunId::new_v7(),
                ToolCallId::new_v7(),
                policy,
                "fingerprint".into(),
                journal::DelegateRequestPayload::default(),
            )
            .expect("reserve journal entry");
        let saved = obstruct_journal_appends(&engine);
        assert!(matches!(
            journal.mark_linked(invocation),
            Err(JournalError::Event(events::EventLogError::Io { .. }))
        ));
        restore_journal_path(&engine, &saved);

        assert_eq!(
            journal
                .get(invocation)
                .expect("poisoned journal read")
                .reservation,
            entry.reservation
        );
        assert_eq!(journal.entries().len(), 1);
        assert!(matches!(
            journal.mark_linked(invocation),
            Err(JournalError::Poisoned)
        ));
        assert!(matches!(
            journal.mark_run_started(invocation, RunId::new_v7()),
            Err(JournalError::Poisoned)
        ));
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn poison_recovery_stale_delegate_result_advances_run_and_commits_parallel_result() {
        let mut config = test_config();
        let profile = config.agents.get_mut("test").expect("test profile");
        profile.tools = vec!["delegate".into(), "read".into()];
        profile.permissions.delegate = Some("allow".into());
        profile.permissions.read = Some("allow".into());
        let directory = tempfile::tempdir().expect("temporary directory");
        let provider = Arc::new(TwoTurnBatchProvider {
            calls: AtomicUsize::new(0),
        });
        let tools = Arc::new(BatchToolProvider {
            release_delegate: Notify::new(),
        });
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config,
            providers: HashMap::from([("test".into(), provider.clone() as Arc<dyn Provider>)]),
            tools: vec![tools.clone() as Arc<dyn ToolProvider>],
        })
        .expect("open engine");
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let run = engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "poison-recovery".into(),
                input: "run tools".into(),
            })
            .await
            .expect("start run")
            .run_id;
        let (delegate_call, read_call) = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let events = engine
                    .inner
                    .store
                    .get(session)
                    .expect("session")
                    .log
                    .events();
                let delegate = events.iter().find_map(|event| match event.event {
                    Event::ToolCallStarted {
                        tool_call_id,
                        ref tool,
                        ..
                    } if tool == "delegate" => Some(tool_call_id),
                    _ => None,
                });
                let read = events.iter().find_map(|event| match event.event {
                    Event::ToolCallStarted {
                        tool_call_id,
                        ref tool,
                        ..
                    } if tool == "read" => Some(tool_call_id),
                    _ => None,
                });
                if let (Some(delegate), Some(read)) = (delegate, read) {
                    return (delegate, read);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("tool calls were not persisted");
        assert!(
            engine
                .resolve_delegate_failure_if_pending(
                    session,
                    run,
                    delegate_call,
                    delegate_failure_result(None, "delegate journal append failed"),
                )
                .await
                .expect("resolve poison failure")
        );
        tools.release_delegate.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while provider.calls.load(Ordering::SeqCst) < 2
                || engine.inner.store.get(session).expect("session").status
                    != SessionStatus::Completed
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("run did not advance to next model turn");
        let events = engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    matches!(event.event, Event::ToolCallCompleted { tool_call_id, .. } if tool_call_id == delegate_call)
                })
                .count(),
            1
        );
        assert!(events.iter().any(|event| {
            matches!(&event.event, Event::ToolCallCompleted { tool_call_id, result }
                if *tool_call_id == read_call && result.content == "legitimate read result")
        }));
        assert!(events.iter().any(|event| {
            matches!(&event.event, Event::RunCompleted { final_text }
                if final_text.as_deref() == Some("advanced"))
        }));
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn conflicting_delegate_fingerprint_leaves_original_call_pending_with_its_child() {
        let provider = Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            release: Notify::new(),
        });
        let (_directory, engine) = test_engine(provider.clone());
        let (parent, parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let handle = engine
            .delegate_invoke(invocation.clone())
            .await
            .expect("start original delegation");
        tokio::time::timeout(Duration::from_secs(2), async {
            while provider.calls.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("original child did not start");
        assert!(matches!(
            engine
                .delegate_invoke(delegate_invocation(
                    parent,
                    parent_run,
                    call,
                    "conflicting task"
                ))
                .await,
            Err(EngineError::Journal(JournalError::Corrupt(_)))
        ));
        let parent_projection = engine.inner.store.get(parent).expect("parent");
        assert_eq!(
            parent_projection
                .runs
                .get(&parent_run)
                .expect("parent run")
                .pending_calls
                .get(&call)
                .map(String::as_str),
            Some("delegate")
        );
        assert_eq!(
            parent_projection
                .log
                .events()
                .into_iter()
                .filter(|event| {
                    matches!(event.event, Event::ToolCallCompleted { tool_call_id, .. } if tool_call_id == call)
                })
                .count(),
            0
        );
        assert_eq!(child_run_start_count(&engine, handle.child_session_id), 1);
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn always_approval_discloses_all_bash_resources_and_persists_effective_scope() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let resources = vec![
            cookie_agent_protocol::ApprovalResource {
                action: cookie_agent_protocol::ActionKind::Bash,
                resource: "git status --short".into(),
                suggested_pattern: "git status *".into(),
            },
            cookie_agent_protocol::ApprovalResource {
                action: cookie_agent_protocol::ActionKind::Bash,
                resource: "git log -1".into(),
                suggested_pattern: "git log *".into(),
            },
        ];
        engine
            .append(
                session,
                None,
                approval_request_event("bash-batch", resources.clone(), "aggregate ask"),
            )
            .await
            .expect("persist approval request");
        let decision = engine
            .approval_respond(
                session,
                "bash-batch".into(),
                ApprovalDecision::Always,
                None,
                None,
            )
            .await
            .expect("approve batch");
        assert_eq!(decision.decision, ApprovalDecision::Always);
        let events = engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events();
        assert!(events.iter().any(|event| {
            matches!(&event.event, Event::ApprovalRequested { resources: disclosed, .. }
                if disclosed == &resources)
        }));
        assert!(events.iter().any(|event| {
            matches!(&event.event, Event::ApprovalResolved { approval_id, decision: ApprovalDecision::Always, approved_scope, .. }
                if approval_id == "bash-batch" && approved_scope.as_deref() == Some("git status *"))
        }));
        assert!(engine.inner.approvals.allows(
            session,
            cookie_agent_protocol::ActionKind::Bash,
            "git status --short"
        ));
        assert!(engine.inner.approvals.allows(
            session,
            cookie_agent_protocol::ActionKind::Bash,
            "git log -1"
        ));
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(&directory, Arc::new(NoopProvider)).await;
        for resource in ["git status --short", "git log -1"] {
            assert!(reopened.inner.approvals.allows(
                session,
                cookie_agent_protocol::ActionKind::Bash,
                resource
            ));
        }
    }

    #[tokio::test]
    async fn external_directory_always_approval_uses_external_action_and_scope() {
        let (_directory, engine) = test_engine(Arc::new(NoopProvider));
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let resource = cookie_agent_protocol::ApprovalResource {
            action: cookie_agent_protocol::ActionKind::ExternalDirectory,
            resource: "/outside/workspace/file".into(),
            suggested_pattern: "/outside/workspace/file *".into(),
        };
        engine
            .append(
                session,
                None,
                approval_request_event("external", vec![resource.clone()], "external guard"),
            )
            .await
            .expect("persist external approval");
        engine
            .approval_respond(
                session,
                "external".into(),
                ApprovalDecision::Always,
                None,
                None,
            )
            .await
            .expect("approve external path");
        let events = engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events();
        assert!(events.iter().any(|event| {
            matches!(&event.event, Event::ApprovalRequested { action, resources, .. }
                if *action == cookie_agent_protocol::ActionKind::ExternalDirectory
                    && resources == &vec![resource.clone()])
        }));
        assert!(engine.inner.approvals.allows(
            session,
            cookie_agent_protocol::ActionKind::ExternalDirectory,
            "/outside/workspace/file"
        ));
        assert!(!engine.inner.approvals.allows(
            session,
            cookie_agent_protocol::ActionKind::Read,
            "/outside/workspace/file"
        ));
        let mut policy = engine.inner.store.get(session).expect("session").policy;
        policy.permissions.read = cookie_agent_config::PermissionEffect::Allow;
        assert_eq!(
            engine
                .inner
                .permissions
                .decide_resources(
                    &policy,
                    &engine.inner.approvals,
                    session,
                    session,
                    vec![
                        (
                            cookie_agent_protocol::ActionKind::ExternalDirectory,
                            "/outside/workspace/file".into(),
                        ),
                        (
                            cookie_agent_protocol::ActionKind::Read,
                            "/outside/workspace/file".into(),
                        ),
                    ],
                )
                .effect,
            cookie_agent_protocol::Effect::Allow
        );
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn doom_always_is_rejected_and_non_always_responses_persist_no_scope() {
        let (_directory, engine) = test_engine(Arc::new(NoopProvider));
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        let resources = vec![
            cookie_agent_protocol::ApprovalResource {
                action: cookie_agent_protocol::ActionKind::Bash,
                resource: "git status".into(),
                suggested_pattern: "git status *".into(),
            },
            cookie_agent_protocol::ApprovalResource {
                action: cookie_agent_protocol::ActionKind::Bash,
                resource: "git log -1".into(),
                suggested_pattern: "git log *".into(),
            },
        ];
        engine
            .append(
                session,
                None,
                approval_request_event(
                    "doom",
                    resources.clone(),
                    "doom-loop guard (third identical call)",
                ),
            )
            .await
            .expect("persist doom approval");
        let response = engine
            .approval_respond(session, "doom".into(), ApprovalDecision::Always, None, None)
            .await
            .expect("respond to doom approval");
        assert_eq!(response.decision, ApprovalDecision::Reject);
        for resource in &resources {
            assert!(
                !engine
                    .inner
                    .approvals
                    .allows(session, resource.action, &resource.resource)
            );
        }
        for (approval_id, decision) in [
            ("once", ApprovalDecision::Once),
            ("reject", ApprovalDecision::Reject),
        ] {
            engine
                .append(
                    session,
                    None,
                    approval_request_event(approval_id, resources.clone(), "aggregate ask"),
                )
                .await
                .expect("persist approval");
            engine
                .approval_respond(
                    session,
                    approval_id.into(),
                    decision,
                    Some("caller scope".into()),
                    None,
                )
                .await
                .expect("respond to approval");
        }
        let resolved: Vec<_> = engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events()
            .into_iter()
            .filter_map(|event| match event.event {
                Event::ApprovalResolved {
                    approval_id,
                    decision,
                    approved_scope,
                    ..
                } => Some((approval_id, decision, approved_scope)),
                _ => None,
            })
            .collect();
        assert!(resolved.iter().any(|(id, decision, scope)| {
            id == "doom" && *decision == ApprovalDecision::Reject && scope.is_none()
        }));
        assert!(resolved.iter().any(|(id, decision, scope)| {
            id == "once" && *decision == ApprovalDecision::Once && scope.is_none()
        }));
        assert!(resolved.iter().any(|(id, decision, scope)| {
            id == "reject" && *decision == ApprovalDecision::Reject && scope.is_none()
        }));
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn abandoned_generation_rejects_its_queued_child_start_without_creating_a_run() {
        let (_directory, engine) = test_engine(Arc::new(NoopProvider));
        let child = engine.create_session(".", "test").expect("create child").id;
        let invocation = InvocationId(Uuid::from_u128(101));
        let abandoned = AdmissionGuard::begin(engine.inner.clone(), invocation, RunId::new_v7());
        let generation = abandoned.generation;
        drop(abandoned);

        assert!(matches!(
            start_with_admission(
                &engine,
                child,
                invocation,
                generation,
                "queued child start".into(),
            )
            .await,
            Err(EngineError::MissingTool(_))
        ));
        assert!(
            engine
                .inner
                .store
                .get(child)
                .expect("child")
                .runs
                .is_empty(),
            "abandoned admission created an untracked child run"
        );
        assert!(
            engine
                .inner
                .active
                .lock()
                .expect("active run lock poisoned")
                .is_empty()
        );
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn superseded_stale_start_noops_while_retry_starts_one_linked_child_run() {
        let (_directory, engine) = test_engine(Arc::new(NoopProvider));
        let (parent, parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let entry = reserve_live_delegation(&engine, &invocation, policy.clone()).await;
        let child = entry.reservation.child_session_id;
        persist_delegated_child(&engine, &invocation, child, policy);
        engine.spawn_actor(child);
        engine
            .append(
                parent,
                Some(parent_run),
                Event::ToolCallLinked {
                    tool_call_id: call,
                    child_session_id: child,
                },
            )
            .await
            .expect("persist parent link");
        let stale = AdmissionGuard::begin(
            engine.inner.clone(),
            entry.reservation.invocation_id,
            parent_run,
        );
        let stale_generation = stale.generation;
        drop(stale);
        let mut retry = AdmissionGuard::begin(
            engine.inner.clone(),
            entry.reservation.invocation_id,
            parent_run,
        );

        assert!(matches!(
            start_with_admission(
                &engine,
                child,
                entry.reservation.invocation_id,
                stale_generation,
                render_delegate_input(&entry.request),
            )
            .await,
            Err(EngineError::MissingTool(_))
        ));
        start_with_admission(
            &engine,
            child,
            entry.reservation.invocation_id,
            retry.generation,
            render_delegate_input(&entry.request),
        )
        .await
        .expect("retry start");
        retry.complete();
        assert_eq!(child_run_start_count(&engine, child), 1);
        assert_eq!(
            engine
                .inner
                .store
                .get(parent)
                .expect("parent")
                .log
                .events()
                .into_iter()
                .filter(|event| {
                    matches!(event.event, Event::ToolCallLinked { tool_call_id, child_session_id }
                        if tool_call_id == call && child_session_id == child)
                })
                .count(),
            1
        );
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn undeliverable_child_start_reply_cancels_durably_across_reopen() {
        let (directory, engine) = test_engine(Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            release: Notify::new(),
        }));
        let child = engine.create_session(".", "test").expect("create child").id;
        let invocation = InvocationId(Uuid::from_u128(102));
        let mut admission =
            AdmissionGuard::begin(engine.inner.clone(), invocation, RunId::new_v7());
        let actor = engine
            .inner
            .actors
            .lock()
            .expect("actor registry lock poisoned")
            .get(&child)
            .cloned()
            .expect("child actor");
        let (reply, receiver) = tokio::sync::oneshot::channel();
        drop(receiver);
        actor
            .send(SessionCommand::Start {
                params: RunStartParams {
                    session_id: child,
                    client_run_id: delegate_client_run_id(invocation),
                    input: "reply abandoned".into(),
                },
                admission: Some((invocation, admission.generation)),
                reply,
            })
            .await
            .expect("queue child start");
        wait_for_session_status(&engine, child, &SessionStatus::Cancelled).await;
        admission.complete();
        assert!(
            engine
                .inner
                .store
                .get(child)
                .expect("child")
                .log
                .events()
                .into_iter()
                .any(|event| matches!(event.event, Event::RunCancelled { .. }))
        );
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(
            &directory,
            Arc::new(BlockingProvider {
                calls: AtomicUsize::new(0),
                release: Notify::new(),
            }),
        )
        .await;
        assert_eq!(
            reopened.inner.store.get(child).expect("child").status,
            SessionStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn superseded_retry_completes_child_and_parent_result_once() {
        let (_directory, engine) = test_engine(Arc::new(ReportProvider));
        let (parent, parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let entry = reserve_live_delegation(&engine, &invocation, policy.clone()).await;
        let child = entry.reservation.child_session_id;
        persist_delegated_child(&engine, &invocation, child, policy);
        engine.spawn_actor(child);
        engine
            .append(
                parent,
                Some(parent_run),
                Event::ToolCallLinked {
                    tool_call_id: call,
                    child_session_id: child,
                },
            )
            .await
            .expect("persist parent link");
        let stale = AdmissionGuard::begin(
            engine.inner.clone(),
            entry.reservation.invocation_id,
            parent_run,
        );
        let stale_generation = stale.generation;
        drop(stale);
        assert!(matches!(
            start_with_admission(
                &engine,
                child,
                entry.reservation.invocation_id,
                stale_generation,
                render_delegate_input(&entry.request),
            )
            .await,
            Err(EngineError::MissingTool(_))
        ));

        let handle = engine
            .delegate_invoke(invocation)
            .await
            .expect("superseding retry");
        let result = engine.await_delegate(handle).await.expect("child result");
        assert_eq!(result.content, "child report");
        engine
            .submit_tool_result(parent, parent_run, call, Ok(result))
            .await
            .expect("commit parent result");
        assert_eq!(child_run_start_count(&engine, child), 1);
        assert_eq!(
            engine
                .inner
                .store
                .get(parent)
                .expect("parent")
                .log
                .events()
                .into_iter()
                .filter(|event| {
                    matches!(event.event, Event::ToolCallCompleted { tool_call_id, .. } if tool_call_id == call)
                })
                .count(),
            1
        );
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn drop_after_start_reply_during_confirmation_cancels_once_and_survives_reopen() {
        let (directory, engine) = test_engine(Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            release: Notify::new(),
        }));
        let (parent, parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let invocation_id = invocation_id(parent, parent_run, call);
        let (reached, mut confirmations) = mpsc::unbounded_channel();
        let release = Arc::new(Barrier::new(2));
        *engine
            .inner
            .admission_confirmation_hook
            .lock()
            .expect("admission confirmation hook lock poisoned") =
            Some(Arc::new(AdmissionConfirmationHook {
                reached,
                release: release.clone(),
            }));
        let task = tokio::spawn({
            let engine = engine.clone();
            async move { engine.delegate_invoke(invocation).await }
        });
        tokio::time::timeout(Duration::from_secs(2), confirmations.recv())
            .await
            .expect("child start reply did not reach confirmation")
            .expect("child start reply reached confirmation");
        let entry = engine
            .journal_get(invocation_id)
            .await
            .expect("journal lookup")
            .expect("journal entry");
        let child = entry.reservation.child_session_id;
        task.abort();
        let _ = task.await;
        wait_for_session_status(&engine, child, &SessionStatus::Cancelled).await;
        let result = wait_for_delegate_completion(&engine, parent, call).await;
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).expect("cancelled result")["status"],
            "cancelled"
        );
        release.wait().await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if journal_run_count(&engine, invocation_id, child_run_id(&engine, child)) == 1 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actor-owned confirmation did not finish");
        assert_eq!(run_cancel_count(&engine, child), 1);
        assert_eq!(parent_delegate_completion_count(&engine, parent, call), 1);
        tokio::task::yield_now().await;
        engine.shutdown().await;
        drop(engine);

        let reopened = reopen_test_engine(
            &directory,
            Arc::new(BlockingProvider {
                calls: AtomicUsize::new(0),
                release: Notify::new(),
            }),
        )
        .await;
        assert_eq!(
            reopened.inner.store.get(child).expect("child").status,
            SessionStatus::Cancelled
        );
        assert_eq!(run_cancel_count(&reopened, child), 1);
    }

    #[tokio::test]
    async fn abandoned_concurrent_delegate_observer_does_not_cancel_survivor() {
        let (_directory, engine) = test_engine(Arc::new(ReportProvider));
        let (_parent, _parent_run, _call, invocation) = pending_delegate_parent(&engine).await;
        let (reached, mut confirmations) = mpsc::unbounded_channel();
        let release = Arc::new(Barrier::new(3));
        *engine
            .inner
            .admission_confirmation_hook
            .lock()
            .expect("admission confirmation hook lock poisoned") =
            Some(Arc::new(AdmissionConfirmationHook {
                reached,
                release: release.clone(),
            }));
        let abandoned = tokio::spawn({
            let engine = engine.clone();
            let invocation = invocation.clone();
            async move { engine.delegate_invoke(invocation).await }
        });
        confirmations.recv().await.expect("first confirmation");
        let survivor = tokio::spawn({
            let engine = engine.clone();
            async move { engine.delegate_invoke(invocation).await }
        });
        confirmations.recv().await.expect("second confirmation");
        abandoned.abort();
        let _ = abandoned.await;
        release.wait().await;
        let handle = survivor
            .await
            .expect("survivor task")
            .expect("surviving delegate admission");
        let result = engine
            .await_delegate(handle)
            .await
            .expect("surviving child result");
        assert_eq!(result.content, "child report");
        assert_eq!(run_cancel_count(&engine, handle.child_session_id), 0);
    }

    #[tokio::test]
    async fn retry_after_stale_sweep_snapshot_keeps_its_child_and_parent_pending() {
        let (_directory, engine) = test_engine(Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            release: Notify::new(),
        }));
        let (parent, parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let entry = reserve_live_delegation(&engine, &invocation, policy.clone()).await;
        let child = entry.reservation.child_session_id;
        persist_delegated_child(&engine, &invocation, child, policy);
        engine.spawn_actor(child);
        let run = engine
            .start_run(RunStartParams {
                session_id: child,
                client_run_id: delegate_client_run_id(entry.reservation.invocation_id),
                input: render_delegate_input(&entry.request),
            })
            .await
            .expect("start child")
            .run_id;
        let journal = engine.inner.journal.clone();
        let invocation_id = entry.reservation.invocation_id;
        tokio::task::spawn_blocking(move || journal.mark_run_started(invocation_id, run))
            .await
            .expect("journal task")
            .expect("confirm child run");

        let mut stale = AdmissionGuard::begin(engine.inner.clone(), invocation_id, parent_run);
        stale.set_parent(parent, call);
        engine
            .publish_admission_run(invocation_id, stale.generation, child, run)
            .expect("publish stale run");
        engine
            .inner
            .inflight_delegations
            .lock()
            .expect("inflight delegation lock poisoned")
            .get_mut(&invocation_id)
            .expect("admission entries")
            .get_mut(&stale.generation)
            .expect("stale generation")
            .cancelled = true;
        let stale_generation = stale.generation;
        let (reached, mut sweep_observed) = mpsc::unbounded_channel();
        let (captured, mut captured_targets) = mpsc::unbounded_channel();
        let release_sweep = Arc::new(Notify::new());
        *engine
            .inner
            .abandoned_sweep_hook
            .lock()
            .expect("abandoned sweep hook lock poisoned") = Some(AbandonedSweepHook {
            reached,
            captured,
            release: release_sweep.clone(),
        });
        let sweep = tokio::spawn({
            let engine = engine.clone();
            async move {
                engine
                    .sweep_abandoned_admission(invocation_id, stale_generation)
                    .await
            }
        });
        sweep_observed
            .recv()
            .await
            .expect("sweeper did not observe all generations abandoned");
        assert_eq!(
            captured_targets
                .recv()
                .await
                .expect("sweeper did not capture its target set"),
            vec![run]
        );
        let mut retry = AdmissionGuard::begin(engine.inner.clone(), invocation_id, parent_run);
        retry.set_parent(parent, call);
        assert!(
            engine
                .inner
                .inflight_delegations
                .lock()
                .expect("inflight delegation lock poisoned")
                .get(&invocation_id)
                .is_some_and(|entries| entries.contains_key(&stale_generation))
        );
        release_sweep.notify_one();
        sweep.await.expect("sweep task").expect("stale sweep");
        assert!(
            engine
                .inner
                .inflight_delegations
                .lock()
                .expect("inflight delegation lock poisoned")
                .get(&invocation_id)
                .is_some_and(|entries| {
                    !entries.contains_key(&stale_generation)
                        && entries.contains_key(&retry.generation)
                }),
            "the sweep did not consume exactly the pre-retry stale generation"
        );
        assert_eq!(
            engine.inner.store.get(child).expect("child").status,
            SessionStatus::Running
        );
        assert_eq!(parent_delegate_completion_count(&engine, parent, call), 0);
        stale.complete();
        retry.complete();
        engine.cancel_run(run).await.expect("cancel child");
    }

    #[tokio::test]
    async fn abandoned_confirmed_run_attachment_publishes_run_for_sweep() {
        let (_directory, engine) = test_engine(Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            release: Notify::new(),
        }));
        let (parent, parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let entry = reserve_live_delegation(&engine, &invocation, policy.clone()).await;
        let child = entry.reservation.child_session_id;
        persist_delegated_child(&engine, &invocation, child, policy);
        engine.spawn_actor(child);
        let run = engine
            .start_run(RunStartParams {
                session_id: child,
                client_run_id: delegate_client_run_id(entry.reservation.invocation_id),
                input: render_delegate_input(&entry.request),
            })
            .await
            .expect("start child")
            .run_id;
        let journal = engine.inner.journal.clone();
        let invocation_id = entry.reservation.invocation_id;
        tokio::task::spawn_blocking(move || journal.mark_run_started(invocation_id, run))
            .await
            .expect("journal task")
            .expect("confirm child run");
        let confirmed = engine
            .journal_get(invocation_id)
            .await
            .expect("journal lookup")
            .expect("confirmed entry");

        let attachment = AdmissionGuard::begin(engine.inner.clone(), invocation_id, parent_run);
        attachment.set_parent(parent, call);
        assert_eq!(
            engine
                .ensure_delegate_run(&confirmed, Some((invocation_id, attachment.generation)))
                .await
                .expect("attach confirmed run"),
            run
        );
        drop(attachment);
        wait_for_session_status(&engine, child, &SessionStatus::Cancelled).await;
        let _ = wait_for_delegate_completion(&engine, parent, call).await;
        assert_eq!(parent_delegate_completion_count(&engine, parent, call), 1);
    }

    #[tokio::test]
    async fn durable_cancel_record_prevents_a_duplicate_after_projection_failure_window() {
        let (_directory, engine) = test_engine(Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            release: Notify::new(),
        }));
        let session = engine.create_session(".", "test").expect("session").id;
        let run = engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "cancel-reconcile".into(),
                input: "input".into(),
            })
            .await
            .expect("start run")
            .run_id;
        // Model the window after JSONL append but before cache/projection refresh.
        append_child_event(&engine, session, run, Event::RunCancelled { reason: None });
        assert!(
            !engine
                .cancel_run_durably(run, Some("retry".into()))
                .expect("reconcile durable cancel")
        );
        assert_eq!(run_cancel_count(&engine, session), 1);
    }

    #[tokio::test]
    async fn three_failed_zero_record_cancellations_retain_active_tombstone() {
        let provider = Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            release: Notify::new(),
        });
        let (_directory, engine) = test_engine(provider);
        let session = engine.create_session(".", "test").expect("session").id;
        let run = engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "zero-record-cancel".into(),
                input: "input".into(),
            })
            .await
            .expect("start run")
            .run_id;
        let log_path = engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .path()
            .to_owned();
        let saved = log_path.with_extension("cancel-fail");
        std::fs::rename(&log_path, &saved).expect("park event log");
        std::fs::create_dir(&log_path).expect("obstruct event log appends");
        let error = engine.cancel_run_durably(run, Some("test failure".into()));
        assert!(matches!(
            error,
            Err(EngineError::Event(EventLogError::Io { .. }))
        ));
        assert!(
            engine
                .inner
                .active
                .lock()
                .expect("active run lock poisoned")
                .contains_key(&run)
        );
        assert_eq!(run_cancel_count(&engine, session), 0);
        std::fs::remove_dir(&log_path).expect("remove event log obstruction");
        std::fs::rename(saved, log_path).expect("restore event log");
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_completes_with_a_pending_admission_task() {
        let (_directory, engine) = test_engine(Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            release: Notify::new(),
        }));
        let (_, _, _, invocation) = pending_delegate_parent(&engine).await;
        let (reached, mut confirmations) = mpsc::unbounded_channel();
        let release = Arc::new(Barrier::new(2));
        *engine
            .inner
            .admission_confirmation_hook
            .lock()
            .expect("admission confirmation hook lock poisoned") =
            Some(Arc::new(AdmissionConfirmationHook { reached, release }));
        let admission = tokio::spawn({
            let engine = engine.clone();
            async move { engine.delegate_invoke(invocation).await }
        });
        confirmations
            .recv()
            .await
            .expect("pending admission confirmation");
        tokio::time::timeout(Duration::from_secs(1), engine.shutdown())
            .await
            .expect("shutdown deadlocked on admission task");
        assert!(matches!(
            admission.await.expect("admission join"),
            Err(EngineError::ActorStopped)
        ));
    }

    #[tokio::test]
    async fn shutdown_waits_for_registered_blocking_admission_mutation() {
        let (directory, engine) = test_engine(Arc::new(NoopProvider));
        let marker = directory.path().join("blocking-admission-finished");
        let (reached, reached_rx) = std_mpsc::channel();
        let (release_tx, release) = std_mpsc::channel();
        *engine
            .inner
            .admission_blocking_hook
            .lock()
            .expect("admission blocking hook lock poisoned") =
            Some(AdmissionBlockingHook { reached, release });
        let mutation = tokio::spawn({
            let engine = engine.clone();
            let marker = marker.clone();
            async move {
                engine
                    .spawn_admission_blocking(move || {
                        std::fs::write(&marker, b"complete").map_err(|source| {
                            EngineError::Event(EventLogError::Io {
                                path: marker.clone(),
                                source,
                            })
                        })?;
                        Ok::<_, EngineError>(())
                    })
                    .await
            }
        });
        tokio::task::spawn_blocking(move || reached_rx.recv())
            .await
            .expect("blocking checkpoint task")
            .expect("blocking checkpoint sender dropped");
        let mut shutdown = tokio::spawn({
            let engine = engine.clone();
            async move { engine.shutdown().await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
                .await
                .is_err(),
            "shutdown returned before the blocking admission closure finished"
        );
        release_tx.send(()).expect("release blocking admission");
        mutation
            .await
            .expect("mutation task")
            .expect("blocking mutation");
        shutdown.await.expect("shutdown task");
        assert_eq!(std::fs::read(marker).expect("read marker"), b"complete");
    }

    #[tokio::test]
    async fn poisoned_active_and_actor_locks_do_not_block_abandoned_delegate_terminalization() {
        let (_directory, engine) = test_engine(Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            release: Notify::new(),
        }));
        let (parent, parent_run, call, invocation) = pending_delegate_parent(&engine).await;
        let policy = child_policy_for(&engine, &invocation);
        let entry = reserve_live_delegation(&engine, &invocation, policy.clone()).await;
        let child = entry.reservation.child_session_id;
        persist_delegated_child(&engine, &invocation, child, policy);
        engine.spawn_actor(child);
        let admission = AdmissionGuard::begin(
            engine.inner.clone(),
            entry.reservation.invocation_id,
            parent_run,
        );
        admission.set_parent(parent, call);
        let _run = engine
            .ensure_delegate_run(
                &entry,
                Some((entry.reservation.invocation_id, admission.generation)),
            )
            .await
            .expect("start child run");

        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = engine
                    .inner
                    .active
                    .lock()
                    .expect("active lock before poison");
                panic!("poison active registry");
            }))
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = engine
                    .inner
                    .actors
                    .lock()
                    .expect("actor lock before poison");
                panic!("poison actor registry");
            }))
            .is_err()
        );
        let active = engine
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&_run)
            .cloned()
            .expect("active child run");
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = active.stdin.lock().expect("stdin lock before poison");
                panic!("poison child stdin");
            }))
            .is_err()
        );

        drop(admission);
        wait_for_session_status(&engine, child, &SessionStatus::Cancelled).await;
        let result = wait_for_delegate_completion(&engine, parent, call).await;
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).expect("cancelled result")["status"],
            "cancelled"
        );
        assert_eq!(run_cancel_count(&engine, child), 1);
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn opaque_artifacts_persist_and_replay_after_reopen() {
        let artifact = serde_json::json!({
            "items": [{"type": "reasoning", "id": "reasoning_native", "encrypted_content": "ciphertext"}]
        });
        let provider = Arc::new(OpaqueRecordingProvider {
            protocol: ProviderProtocol::OpenAiResponses,
            artifact: artifact.clone(),
            fail_after_first: false,
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let (directory, engine) = test_engine(provider.clone());
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "first".into(),
                input: "first input".into(),
            })
            .await
            .expect("start first run");
        wait_for_session_status(&engine, session, &SessionStatus::Completed).await;
        assert!(engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events()
            .iter()
            .any(|event| matches!(&event.event, Event::TurnOpaque { state } if state.payload == artifact)));
        engine.shutdown().await;

        let reopened = reopen_test_engine(&directory, provider.clone()).await;
        reopened
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "second".into(),
                input: "second input".into(),
            })
            .await
            .expect("start second run");
        wait_for_session_status(&reopened, session, &SessionStatus::Completed).await;
        let requests = provider.requests.lock().expect("requests lock poisoned");
        let replay = requests.last().expect("second provider request");
        assert!(replay.persisted_turns.iter().any(|turn| {
            turn.opaque.as_ref().is_some_and(|opaque| {
                opaque.provider == ProviderProtocol::OpenAiResponses && opaque.payload == artifact
            })
        }));
    }

    #[tokio::test]
    async fn fallback_discards_foreign_opaque_without_removing_it_from_the_log() {
        let artifact = serde_json::json!({"message": {"role": "assistant", "content": "native"}});
        let primary = Arc::new(OpaqueRecordingProvider {
            protocol: ProviderProtocol::AnthropicMessages,
            artifact: artifact.clone(),
            fail_after_first: true,
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let fallback = Arc::new(OpaqueRecordingProvider {
            protocol: ProviderProtocol::OpenAiChatCompletions,
            artifact: serde_json::json!({"message": {"role": "assistant", "content": "fallback"}}),
            fail_after_first: false,
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut config = test_config();
        config.providers.insert(
            "fallback".into(),
            ProviderConfig {
                kind: ProviderType::OpenAi,
                api_key_env: None,
                base_url: None,
                api: None,
            },
        );
        config
            .agents
            .get_mut("test")
            .expect("test profile")
            .models
            .push(ModelConfig {
                provider: "fallback".into(),
                model: "fallback-model".into(),
            });
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config,
            providers: HashMap::<String, Arc<dyn Provider>>::from([
                ("test".into(), primary as Arc<dyn Provider>),
                ("fallback".into(), fallback.clone() as Arc<dyn Provider>),
            ]),
            tools: Vec::new(),
        })
        .expect("open engine");
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        for (client_run_id, input) in [("first", "first input"), ("second", "second input")] {
            engine
                .start_run(RunStartParams {
                    session_id: session,
                    client_run_id: client_run_id.into(),
                    input: input.into(),
                })
                .await
                .expect("start run");
            wait_for_session_status(&engine, session, &SessionStatus::Completed).await;
        }
        let requests = fallback.requests.lock().expect("requests lock poisoned");
        let request = requests.first().expect("fallback request");
        assert!(request.persisted_turns.iter().any(|turn| {
            turn.opaque
                .as_ref()
                .is_some_and(|opaque| opaque.provider == ProviderProtocol::AnthropicMessages)
        }));
        assert!(
            cookie_agent_providers::openai::encode_history(
                &request.persisted_turns,
                cookie_agent_providers::openai::OpenAiEndpoint::ChatCompletions,
            )
            .discarded_opaque
        );
        assert!(engine
            .inner
            .store
            .get(session)
            .expect("session")
            .log
            .events()
            .iter()
            .any(|event| matches!(&event.event, Event::TurnOpaque { state } if state.payload == artifact)));
    }

    #[test]
    fn tool_call_ids_are_native_only_for_matching_protocol_replay() {
        let session = SessionId::new_v7();
        let run = RunId::new_v7();
        let call = ToolCallId::new_v7();
        let events = vec![
            EventEnvelope {
                session_id: session,
                run_id: Some(run),
                seq: 1,
                timestamp: jiff::Timestamp::now(),
                event: Event::RunStarted {
                    client_run_id: "run".into(),
                    input: "input".into(),
                },
            },
            EventEnvelope {
                session_id: session,
                run_id: Some(run),
                seq: 2,
                timestamp: jiff::Timestamp::now(),
                event: Event::TurnOpaque {
                    state: TurnOpaque {
                        provider: cookie_agent_protocol::ProviderProtocol::OpenAiResponses,
                        payload: serde_json::json!({"items": [{"type": "function_call", "call_id": "native-call"}]}),
                    },
                },
            },
            EventEnvelope {
                session_id: session,
                run_id: Some(run),
                seq: 3,
                timestamp: jiff::Timestamp::now(),
                event: Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "read".into(),
                    arguments: serde_json::json!({"path": "file"}),
                    provider_tool_call_id: Some("native-call".into()),
                    provider_protocol: Some(
                        cookie_agent_protocol::ProviderProtocol::OpenAiResponses,
                    ),
                },
            },
            EventEnvelope {
                session_id: session,
                run_id: Some(run),
                seq: 4,
                timestamp: jiff::Timestamp::now(),
                event: Event::ToolCallCompleted {
                    tool_call_id: call,
                    result: cookie_agent_protocol::ToolResult {
                        content: "result".into(),
                        truncated: false,
                    },
                },
            },
        ];
        let same = assemble_persisted_turns(&events, Some(ProviderProtocol::OpenAiResponses));
        let foreign =
            assemble_persisted_turns(&events, Some(ProviderProtocol::OpenAiChatCompletions));
        let tool_id = |turns: &[cookie_agent_providers::PersistedTurn]| match &turns[2].message {
            ProviderMessage::Tool { result } => result.tool_call_id.clone(),
            _ => panic!("tool result expected"),
        };
        assert_eq!(tool_id(&same), "native-call");
        assert_eq!(tool_id(&foreign), call.to_string());
        assert!(matches!(
            &foreign[1].message,
            ProviderMessage::Assistant { tool_calls, .. } if tool_calls[0].id == call.to_string()
        ));
        let mut unknown_events = events.clone();
        if let Event::ToolCallStarted {
            provider_protocol, ..
        } = &mut unknown_events[2].event
        {
            *provider_protocol = None;
        } else {
            panic!("tool call expected");
        }
        let unknown = assemble_persisted_turns(&unknown_events, None);
        assert_eq!(tool_id(&unknown), call.to_string());
        assert!(matches!(
            &unknown[1].message,
            ProviderMessage::Assistant { tool_calls, .. } if tool_calls[0].id == call.to_string()
        ));
    }

    #[tokio::test]
    async fn meaningful_stream_failure_advances_without_a_same_entry_retry() {
        let primary = Arc::new(MeaningfulFailureProvider {
            calls: AtomicUsize::new(0),
        });
        let fallback = Arc::new(OpaqueRecordingProvider {
            protocol: ProviderProtocol::OpenAiChatCompletions,
            artifact: serde_json::json!({"message": {"role": "assistant", "content": "fallback"}}),
            fail_after_first: false,
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut config = test_config();
        config.providers.insert(
            "fallback".into(),
            ProviderConfig {
                kind: ProviderType::OpenAi,
                api_key_env: None,
                base_url: None,
                api: None,
            },
        );
        config
            .agents
            .get_mut("test")
            .expect("test profile")
            .models
            .push(ModelConfig {
                provider: "fallback".into(),
                model: "fallback-model".into(),
            });
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config,
            providers: HashMap::<String, Arc<dyn Provider>>::from([
                ("test".into(), primary.clone() as Arc<dyn Provider>),
                ("fallback".into(), fallback.clone() as Arc<dyn Provider>),
            ]),
            tools: Vec::new(),
        })
        .expect("open engine");
        let session = engine
            .create_session(".", "test")
            .expect("create session")
            .id;
        engine
            .start_run(RunStartParams {
                session_id: session,
                client_run_id: "run".into(),
                input: "input".into(),
            })
            .await
            .expect("start run");
        wait_for_session_status(&engine, session, &SessionStatus::Completed).await;
        assert_eq!(primary.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback.calls.load(Ordering::SeqCst), 1);
        let fallback_request = fallback
            .requests
            .lock()
            .expect("requests lock poisoned")
            .first()
            .cloned()
            .expect("fallback request");
        assert!(!fallback_request.persisted_turns.iter().any(|turn| {
            matches!(
                &turn.message,
                ProviderMessage::Assistant { content, .. }
                    if content.iter().any(|part| matches!(part, ContentPart::Text { text } if text == "partial"))
            )
        }));
        assert!(
            engine
                .inner
                .store
                .get(session)
                .expect("session")
                .log
                .events()
                .iter()
                .any(|event| matches!(event.event, Event::AttemptAbandoned))
        );
    }
}
