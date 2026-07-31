//! The transport-free single-conversation CookieCode runtime.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[cfg(test)]
use std::sync::mpsc as std_mpsc;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookiecode_config::{
    AgentType as ConfigAgentType, Config, DepthLimit as ConfigDepthLimit, PolicySnapshot,
};
use cookiecode_protocol::{
    AgentDescriptor, AgentListResult, AgentType, ApprovalDecision, ApprovalRespondResult, Event,
    EventEnvelope, EventSubscriptionMessage, EventsSubscribeResult, InvocationId, ModelRef,
    RunCancelResult, RunId, RunStartParams, RunStartResult, RunSteerResult, RunToolStdinParams,
    RunToolStdinResult, SessionId, SessionMeta, SessionOrigin, SessionStatus, ToolCallId,
};
use cookiecode_providers::{
    ContentPart, ModelId, ModelRef as ProviderModelRef, NormalizedEvent, Provider, ProviderError,
    ProviderErrorClass, ProviderMessage, ProviderRequest, StopReason, ToolDefinition,
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
    pub fn output(&self, stream: cookiecode_protocol::OutputStream, data: &[u8]) {
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
    Config(#[source] Box<cookiecode_config::ConfigError>),
    #[error("profile `{0}` is subagent-only")]
    SubagentOnly(String),
    #[error("run {0} not found")]
    MissingRun(RunId),
    #[error("session {0} is already running")]
    SessionRunning(SessionId),
    #[error("tool call is not running or is not interactive")]
    StdinUnavailable,
    #[error("invalid base64 stdin: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("provider failure: {0}")]
    Provider(#[from] cookiecode_providers::ProviderError),
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
    stdin: Mutex<HashMap<ToolCallId, mpsc::Sender<StdinWrite>>>,
    /// Last persisted event included in the current provider request.
    prompt_seq: AtomicU64,
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
    Start {
        params: RunStartParams,
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
        reply: oneshot::Sender<Result<(), EngineError>>,
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
    tools: Vec<Arc<dyn ToolProvider>>,
    approvals: ApprovalStore,
    permissions: PermissionPipeline,
    active: Mutex<HashMap<RunId, Arc<ActiveRun>>>,
    subscribers: Mutex<HashMap<SessionId, Vec<PersistedSubscriber>>>,
    actors: Mutex<HashMap<SessionId, SessionActor<SessionCommand>>>,
    output_hubs: Mutex<HashMap<ToolCallId, OutputHub>>,
    pending_approvals: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    #[cfg(test)]
    prompt_snapshot_hook: Mutex<Option<Arc<PromptSnapshotHook>>>,
    #[cfg(test)]
    gap_send_hook: Mutex<Option<GapSendHook>>,
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
                tools: options.tools,
                approvals: ApprovalStore::default(),
                permissions: PermissionPipeline::default(),
                active: Mutex::new(HashMap::new()),
                subscribers: Mutex::new(HashMap::new()),
                actors: Mutex::new(HashMap::new()),
                output_hubs: Mutex::new(HashMap::new()),
                pending_approvals: Mutex::new(HashMap::new()),
                #[cfg(test)]
                prompt_snapshot_hook: Mutex::new(None),
                #[cfg(test)]
                gap_send_hook: Mutex::new(None),
            }),
        };
        for session in engine.inner.store.all() {
            engine.spawn_actor(session.meta.id);
        }
        engine.rebuild_approvals();
        engine.reconcile()?;
        Ok(engine)
    }

    #[must_use]
    pub fn client(&self) -> EngineClient {
        self.clone()
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
    pub(crate) async fn create_child(
        &self,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        profile: &str,
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
        let request_fingerprint = serde_json::to_string(&(profile, &child_policy))
            .expect("delegation policy fingerprint serializes");
        let journal = self.inner.journal.clone();
        let journal_policy = child_policy.clone();
        let entry = tokio::task::spawn_blocking(move || {
            journal.reserve(
                invocation_id,
                parent_session_id,
                parent_run_id,
                parent_tool_call_id,
                journal_policy,
                request_fingerprint,
            )
        })
        .await
        .map_err(|_| EngineError::ActorStopped)??;
        if let Ok(existing) = self.inner.store.get(entry.reservation.child_session_id) {
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
        tokio::task::spawn_blocking(move || store.create(creation_meta, child_policy))
            .await
            .map_err(|_| EngineError::ActorStopped)??;
        self.spawn_actor(meta.id);
        self.append(
            parent_session_id,
            Some(parent_run_id),
            Event::ToolCallLinked {
                tool_call_id: parent_tool_call_id,
                child_session_id: meta.id,
            },
        )
        .await?;
        let journal = self.inner.journal.clone();
        tokio::task::spawn_blocking(move || journal.mark_linked(invocation_id))
            .await
            .map_err(|_| EngineError::ActorStopped)??;
        Ok(meta)
    }

    pub async fn start_run(&self, params: RunStartParams) -> Result<RunStartResult, EngineError> {
        let session = params.session_id;
        self.request(session, |reply| SessionCommand::Start { params, reply })
            .await
    }

    /// Synchronous setup/CLI wrapper. Do not call from a Tokio runtime.
    pub fn start_run_blocking(
        &self,
        params: RunStartParams,
    ) -> Result<RunStartResult, EngineError> {
        let session = params.session_id;
        self.request_blocking(session, |reply| SessionCommand::Start { params, reply })
    }

    pub async fn steer(&self, run_id: RunId, input: String) -> Result<RunSteerResult, EngineError> {
        let active = self
            .inner
            .active
            .lock()
            .expect("active run lock poisoned")
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
            .expect("active run lock poisoned")
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
            .expect("active run lock poisoned")
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        self.request(active.session, |reply| SessionCommand::Cancel {
            run: run_id,
            reply,
        })
        .await
    }

    pub async fn tool_stdin(
        &self,
        params: RunToolStdinParams,
    ) -> Result<RunToolStdinResult, EngineError> {
        let active = self
            .inner
            .active
            .lock()
            .expect("active run lock poisoned")
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
        stream: cookiecode_protocol::OutputStream,
    ) -> Option<(
        cookiecode_protocol::OutputSnapshot,
        mpsc::Receiver<events::OutputMessage>,
    )> {
        self.inner
            .output_hubs
            .lock()
            .expect("output hub registry lock poisoned")
            .get(&call)
            .cloned()
            .map(|hub| hub.subscribe(stream, 256))
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
    pub fn children(&self, id: SessionId) -> Vec<cookiecode_protocol::ChildSummary> {
        self.inner.store.children(id)
    }
    pub fn tree(&self, id: SessionId) -> Result<cookiecode_protocol::SessionTree, EngineError> {
        Ok(self.inner.store.tree(id)?)
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
            .expect("subscriber lock poisoned")
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
            .expect("actor registry lock poisoned")
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

    async fn prompt_messages(
        &self,
        session: SessionId,
        run: RunId,
    ) -> Result<Vec<ProviderMessage>, EngineError> {
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
        Ok(assemble_messages(&events))
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
            .expect("actor registry lock poisoned")
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
            .expect("actor registry lock poisoned")
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
            .expect("actor registry lock poisoned")
            .insert(session, actor);
    }

    async fn handle_actor_command(&self, session: SessionId, command: SessionCommand) {
        match command {
            SessionCommand::Append { run, event, reply } => {
                let _ = reply.send(self.append_direct(session, run, event));
            }
            SessionCommand::Start { params, reply } => {
                let _ = reply.send(self.start_run_direct(params).await);
            }
            SessionCommand::Steer { run, input, reply } => {
                let result = self
                    .inner
                    .active
                    .lock()
                    .expect("active run lock poisoned")
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
                    .expect("active run lock poisoned")
                    .get(&run)
                    .cloned()
                    .filter(|active| active.session == session)
                    .ok_or(EngineError::MissingRun(run))
                    .map(|active| {
                        active.cancellation.cancel();
                        active.stdin.lock().expect("stdin lock poisoned").clear();
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
                        .expect("active run lock poisoned")
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
                        .expect("stdin lock poisoned")
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
                            .expect("stdin lock poisoned")
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
                        .expect("subscriber lock poisoned")
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
                let event = match result {
                    Ok(result) => Event::ToolCallCompleted {
                        tool_call_id,
                        result: cookiecode_protocol::ToolResult {
                            content: result.content,
                            truncated: result.truncated,
                        },
                    },
                    Err(message) => Event::ToolCallFailed {
                        tool_call_id,
                        message,
                    },
                };
                let _ = reply.send(self.append_direct(session, Some(run), event));
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
                    .expect("active run lock poisoned")
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
                    .expect("active run lock poisoned")
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
    ) -> Result<RunStartResult, EngineError> {
        let session = self.inner.store.get(params.session_id)?;
        if let Some(run) = session
            .runs
            .values()
            .find(|run| run.client_run_id == params.client_run_id)
        {
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
            stdin: Mutex::new(HashMap::new()),
            prompt_seq: AtomicU64::new(0),
        });
        self.inner
            .active
            .lock()
            .expect("active run lock poisoned")
            .insert(run_id, active.clone());
        let engine = self.clone();
        tokio::spawn(async move {
            let _ = engine.run_loop(run_id, active).await;
            engine
                .inner
                .active
                .lock()
                .expect("active run lock poisoned")
                .remove(&run_id);
        });
        Ok(RunStartResult { run_id })
    }

    async fn run_loop(&self, run_id: RunId, active: Arc<ActiveRun>) -> Result<(), EngineError> {
        // Sticky chain position belongs to this run, not one agent-loop pass.
        let mut fallback_entry = 0_usize;
        loop {
            if active.cancellation.is_cancelled() {
                self.append(
                    active.session,
                    Some(run_id),
                    Event::RunCancelled { reason: None },
                )
                .await?;
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
            let messages = self.prompt_messages(active.session, run_id).await?;
            let events = match self
                .stream_attempt(
                    active.session,
                    run_id,
                    &active.cancellation,
                    &chain,
                    &mut fallback_entry,
                    messages,
                    tools,
                )
                .await
            {
                Ok(events) => events,
                Err(error) => {
                    if active.cancellation.is_cancelled() {
                        self.append(
                            active.session,
                            Some(run_id),
                            Event::RunCancelled { reason: None },
                        )
                        .await?;
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
            for event in events {
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
                self.append(
                    active.session,
                    Some(run_id),
                    Event::ToolCallStarted {
                        tool_call_id: *id,
                        tool: tool.clone(),
                        arguments: arguments.clone(),
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
                    self.append(
                        active.session,
                        Some(run_id),
                        Event::RunCancelled { reason: None },
                    )
                    .await?;
                    return Ok(());
                }
                let task_result = task.await;
                if active.cancellation.is_cancelled() {
                    self.append(
                        active.session,
                        Some(run_id),
                        Event::RunCancelled { reason: None },
                    )
                    .await?;
                    return Ok(());
                }
                let result = match task_result {
                    Ok(result) => result,
                    Err(error) => Err(error.to_string()),
                };
                self.submit_tool_result(active.session, run_id, id, result)
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
        messages: Vec<ProviderMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<Vec<NormalizedEvent>, ProviderError> {
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
                    let _ = self
                        .append(
                            session,
                            Some(run),
                            Event::ModelFallback {
                                from: wire_model(model),
                                to: wire_model(next),
                                reason: last_error.to_string(),
                                attempts: 0,
                            },
                        )
                        .await;
                    entry += 1;
                    *sticky_entry = entry;
                    first_request = false;
                    continue;
                }
                return Err(last_error);
            };
            let mut attempts = 0;
            loop {
                let request_messages = if first_request {
                    first_request = false;
                    messages.clone()
                } else {
                    self.prompt_messages(session, run).await.map_err(|error| {
                        ProviderError::RunTerminal {
                            message: error.to_string(),
                        }
                    })?
                };
                let request = ProviderRequest {
                    model: model.model.clone(),
                    messages: request_messages,
                    tools: tools.clone(),
                    ..ProviderRequest::default()
                };
                let stream = tokio::select! {
                    result = provider.stream(request) => result,
                    _ = cancellation.cancelled() => return Err(ProviderError::RunTerminal { message: "cancelled".into() }),
                };
                let result = match stream {
                    Ok(mut stream) => {
                        let mut events = Vec::new();
                        let mut failure = None;
                        loop {
                            let item = tokio::select! {
                                item = stream.next() => item,
                                _ = cancellation.cancelled() => return Err(ProviderError::RunTerminal { message: "cancelled".into() }),
                            };
                            let Some(item) = item else { break };
                            match item {
                                Ok(event) => {
                                    match &event {
                                        NormalizedEvent::TextDelta { text } => {
                                            let _ = self
                                                .append(
                                                    session,
                                                    Some(run),
                                                    Event::TextDelta { text: text.clone() },
                                                )
                                                .await;
                                        }
                                        NormalizedEvent::ReasoningDelta { text } => {
                                            let _ = self
                                                .append(
                                                    session,
                                                    Some(run),
                                                    Event::ReasoningDelta { text: text.clone() },
                                                )
                                                .await;
                                        }
                                        NormalizedEvent::Usage {
                                            input_tokens,
                                            output_tokens,
                                            cache_read_tokens,
                                        } => {
                                            let _ = self
                                                .append(
                                                    session,
                                                    Some(run),
                                                    Event::UsageReported {
                                                        model: wire_model(model),
                                                        usage: cookiecode_protocol::Usage {
                                                            input_tokens: *input_tokens,
                                                            output_tokens: *output_tokens,
                                                            cached_input_tokens: Some(
                                                                *cache_read_tokens,
                                                            ),
                                                        },
                                                    },
                                                )
                                                .await;
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
                        failure.map_or(Ok(events), Err)
                    }
                    Err(error) => Err(error),
                };
                match result {
                    Ok(events) => return Ok(events),
                    Err(error) if error.class() == ProviderErrorClass::RunTerminal => {
                        return Err(error);
                    }
                    Err(error)
                        if error.class() == ProviderErrorClass::EntryRetryable && attempts < 2 =>
                    {
                        attempts += 1;
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_millis(1_u64 << (attempts - 1))) => {}
                            _ = cancellation.cancelled() => return Err(ProviderError::RunTerminal { message: "cancelled".into() }),
                        }
                    }
                    Err(error) => {
                        last_error = error;
                        break;
                    }
                }
            }
            let Some(next) = chain.get(entry + 1) else {
                return Err(last_error);
            };
            let _ = self
                .append(
                    session,
                    Some(run),
                    Event::ModelFallback {
                        from: wire_model(model),
                        to: wire_model(next),
                        reason: last_error.to_string(),
                        attempts: attempts + 1,
                    },
                )
                .await;
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
            if !session.policy.tools.contains(&call.name) {
                return Err(format!(
                    "tool `{}` is not enabled for this session",
                    call.name
                ));
            }
            let action = PermissionPipeline::action_for_tool(&call.name)
                .map_err(|error| error.to_string())?;
            let root = root_id(&session.meta.origin, active.session);
            let raw_resource = resource_for(&call);
            let resources = if action == cookiecode_protocol::ActionKind::Bash {
                permissions::bash_subcommands(&raw_resource)
            } else if matches!(
                action,
                cookiecode_protocol::ActionKind::Read
                    | cookiecode_protocol::ActionKind::Write
                    | cookiecode_protocol::ActionKind::List
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
                .unwrap_or_else(|_| vec![raw_resource])
            } else {
                vec![raw_resource]
            };
            let mut permission = None;
            for resource in resources {
                let (decision_action, resource) = match resource.strip_prefix("external:") {
                    Some(resource) => (
                        cookiecode_protocol::ActionKind::ExternalDirectory,
                        resource.to_owned(),
                    ),
                    None => (action, resource),
                };
                let decision = engine.inner.permissions.decide(
                    &session.policy,
                    &engine.inner.approvals,
                    root,
                    active.session,
                    decision_action,
                    resource,
                );
                if decision.effect != cookiecode_protocol::Effect::Allow {
                    permission = Some(decision);
                    break;
                }
            }
            let permission = permission.unwrap_or_else(|| {
                engine.inner.permissions.decide(
                    &session.policy,
                    &engine.inner.approvals,
                    root,
                    active.session,
                    action,
                    resource_for(&call),
                )
            });
            let resource = permission.trace.normalized_resource.clone();
            if permission.effect != cookiecode_protocol::Effect::Allow {
                if permission.effect == cookiecode_protocol::Effect::Ask {
                    let approval_id = format!("{}:{}", run, call.id);
                    let suggested_pattern = format!("{resource} *");
                    let (approval_tx, approval_rx) = oneshot::channel();
                    engine
                        .inner
                        .pending_approvals
                        .lock()
                        .expect("pending approval lock poisoned")
                        .insert(approval_id.clone(), approval_tx);
                    if engine
                        .append(
                            active.session,
                            Some(run),
                            Event::ApprovalRequested {
                                approval_id: approval_id.clone(),
                                action,
                                resource: resource.clone(),
                                suggested_pattern,
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
                            .expect("pending approval lock poisoned")
                            .remove(&approval_id);
                        return Err("could not persist approval request".into());
                    }
                    let decision = tokio::select! {
                        decision = approval_rx => decision.map_err(|_| "approval request was abandoned".to_owned())?,
                        _ = active.cancellation.cancelled() => {
                            engine.inner.pending_approvals.lock().expect("pending approval lock poisoned").remove(&approval_id);
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
            let provider = engine
                .inner
                .tools
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
            let hub = OutputHub::new(call.id, 64 * 1024);
            engine
                .inner
                .output_hubs
                .lock()
                .expect("output hub registry lock poisoned")
                .insert(call.id, hub.clone());
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
                    .expect("stdin lock poisoned")
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
                        active.stdin.lock().expect("stdin lock poisoned").remove(&call.id);
                        // Tool implementations drain their producers before
                        // resolving.  Finalizing here makes all emitted deltas
                        // precede the completion notification committed by the
                        // session actor.
                        hub.finalize();
                        engine.inner.output_hubs.lock().expect("output hub registry lock poisoned").remove(&call.id);
                        return result.map(bound_tool_result).map_err(|error| error.to_string());
                    }
                    Some(progress) = progress_rx.recv() => {
                        let _ = engine.append(active.session, Some(run), Event::ToolCallProgress { tool_call_id: progress.tool_call_id, message: progress.message }).await;
                    }
                    _ = active.cancellation.cancelled() => {
                        active.stdin.lock().expect("stdin lock poisoned").remove(&call.id);
                        hub.finalize();
                        engine.inner.output_hubs.lock().expect("output hub registry lock poisoned").remove(&call.id);
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
                    ref suggested_pattern,
                    ..
                } if *id == approval_id => Some((action, suggested_pattern.clone())),
                _ => None,
            });
        if let Some((action, suggested_pattern)) = requested
            && decision == ApprovalDecision::Always
        {
            self.inner.approvals.grant(
                root_id(&projection.meta.origin, session),
                action,
                scope.clone().unwrap_or(suggested_pattern),
            );
        }
        self.append(
            session,
            None,
            Event::ApprovalResolved {
                approval_id: approval_id.clone(),
                decision,
                approved_scope: scope,
                feedback,
            },
        )
        .await?;
        if let Some(sender) = self
            .inner
            .pending_approvals
            .lock()
            .expect("pending approval lock poisoned")
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
        let mut names = HashSet::new();
        let mut output = Vec::new();
        for provider in &self.inner.tools {
            for tool in provider
                .tools_for_session(&SessionToolContext { session })
                .map_err(|error| EngineError::MissingTool(error.to_string()))?
            {
                if policy.tools.contains(&tool.name) && names.insert(tool.name.clone()) {
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
        for entry in self.inner.journal.entries() {
            if self
                .inner
                .store
                .get(entry.reservation.child_session_id)
                .is_ok()
            {
                let parent = self.inner.store.get(entry.reservation.parent_session_id)?;
                let parent_has_link = parent.log.events().iter().any(|envelope| {
                    matches!(
                        envelope.event,
                        Event::ToolCallLinked { tool_call_id, child_session_id }
                            if tool_call_id == entry.reservation.parent_tool_call_id
                                && child_session_id == entry.reservation.child_session_id
                    )
                });
                if !parent_has_link {
                    self.append_blocking(
                        entry.reservation.parent_session_id,
                        Some(entry.reservation.parent_run_id),
                        Event::ToolCallLinked {
                            tool_call_id: entry.reservation.parent_tool_call_id,
                            child_session_id: entry.reservation.child_session_id,
                        },
                    )?;
                }
                if !entry.linked {
                    self.inner
                        .journal
                        .mark_linked(entry.reservation.invocation_id)?;
                }
            }
        }
        Ok(())
    }

    async fn resolve_interrupted_direct(&self, session_id: SessionId) -> Result<(), EngineError> {
        let session = self.inner.store.get(session_id)?;
        for run in session
            .runs
            .values()
            .filter(|run| run.status == SessionStatus::Interrupted)
        {
            for (call, tool) in &run.pending_calls {
                if tool == "delegate" {
                    let invocation = invocation_id(session_id, run.id, *call);
                    let journal = self.inner.journal.clone();
                    let entry = tokio::task::spawn_blocking(move || journal.get(invocation))
                        .await
                        .map_err(|_| EngineError::ActorStopped)?;
                    if let Some(entry) = entry
                        && let Ok(child) = self.inner.store.get(entry.reservation.child_session_id)
                        && child.status == SessionStatus::Completed
                    {
                        let report = child
                            .runs
                            .values()
                            .find_map(|child_run| child_run.final_text.clone())
                            .unwrap_or_else(|| "child completed without a final report".into());
                        self.append_direct(
                            session_id,
                            Some(run.id),
                            Event::ToolCallCompleted {
                                tool_call_id: *call,
                                result: cookiecode_protocol::ToolResult {
                                    content: report,
                                    truncated: false,
                                },
                            },
                        )?;
                        continue;
                    }
                    self.append_direct(
                        session_id,
                        Some(run.id),
                        Event::ToolCallFailed {
                            tool_call_id: *call,
                            message: "delegate interrupted by daemon restart".into(),
                        },
                    )?;
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
                        ..
                    } => {
                        pending.insert(approval_id, (action, suggested_pattern));
                    }
                    Event::ApprovalResolved {
                        approval_id,
                        decision: ApprovalDecision::Always,
                        approved_scope,
                        ..
                    } => {
                        if let Some((action, suggested_pattern)) = pending.get(&approval_id) {
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
    let profile = cookiecode_protocol::ProfileSnapshot {
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
        delegation: cookiecode_protocol::DelegationSnapshot {
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
                    .map(|action| cookiecode_protocol::PermissionRule {
                        id: rule.id.clone(),
                        action,
                        resource: rule.resource.clone(),
                        effect: match rule.effect.as_str() {
                            "allow" => cookiecode_protocol::Effect::Allow,
                            "deny" => cookiecode_protocol::Effect::Deny,
                            _ => cookiecode_protocol::Effect::Ask,
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
fn depth(value: ConfigDepthLimit) -> cookiecode_protocol::DepthLimit {
    match value {
        ConfigDepthLimit::Finite(value) => cookiecode_protocol::DepthLimit::Finite(value),
        ConfigDepthLimit::Unlimited => cookiecode_protocol::DepthLimit::Unlimited,
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
    "cookiecode".hash(&mut second);
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
fn assemble_messages(events: &[EventEnvelope]) -> Vec<ProviderMessage> {
    let mut output = Vec::new();
    let mut assistant_text = String::new();
    let mut assistant_calls = Vec::new();
    let mut pending_steering = HashMap::new();
    let flush_assistant =
        |output: &mut Vec<ProviderMessage>,
         text: &mut String,
         calls: &mut Vec<cookiecode_providers::ToolCall>| {
            if !text.is_empty() || !calls.is_empty() {
                let content = if !text.is_empty() {
                    vec![ContentPart::Text {
                        text: std::mem::take(text),
                    }]
                } else {
                    Vec::new()
                };
                output.push(ProviderMessage::Assistant {
                    content,
                    tool_calls: std::mem::take(calls),
                });
            }
        };
    for event in events {
        match &event.event {
            Event::RunStarted { input, .. } => {
                flush_assistant(&mut output, &mut assistant_text, &mut assistant_calls);
                output.push(ProviderMessage::User {
                    content: vec![ContentPart::Text {
                        text: input.clone(),
                    }],
                });
            }
            Event::UserInputSubmitted { input } => {
                pending_steering.insert(event.seq, input.clone());
            }
            Event::UserInputApplied { user_input_seq } => {
                if let Some(input) = pending_steering.remove(user_input_seq) {
                    flush_assistant(&mut output, &mut assistant_text, &mut assistant_calls);
                    output.push(ProviderMessage::User {
                        content: vec![ContentPart::Text { text: input }],
                    });
                }
            }
            Event::TextDelta { text } => assistant_text.push_str(text),
            Event::ToolCallStarted {
                tool_call_id,
                tool,
                arguments,
            } => assistant_calls.push(cookiecode_providers::ToolCall {
                id: tool_call_id.to_string(),
                name: tool.clone(),
                arguments: arguments.clone(),
            }),
            Event::ToolCallCompleted {
                tool_call_id,
                result,
            } => {
                flush_assistant(&mut output, &mut assistant_text, &mut assistant_calls);
                output.push(ProviderMessage::Tool {
                    result: cookiecode_providers::ToolResult {
                        tool_call_id: tool_call_id.to_string(),
                        content: result.content.clone(),
                        is_error: false,
                    },
                });
            }
            Event::ToolCallFailed {
                tool_call_id,
                message,
            } => {
                flush_assistant(&mut output, &mut assistant_text, &mut assistant_calls);
                output.push(ProviderMessage::Tool {
                    result: cookiecode_providers::ToolResult {
                        tool_call_id: tool_call_id.to_string(),
                        content: message.clone(),
                        is_error: true,
                    },
                });
            }
            Event::RunCompleted { .. }
            | Event::RunFailed { .. }
            | Event::RunCancelled { .. }
            | Event::RunInterrupted { .. } => {
                flush_assistant(&mut output, &mut assistant_text, &mut assistant_calls);
            }
            _ => {}
        }
    }
    flush_assistant(&mut output, &mut assistant_text, &mut assistant_calls);
    output
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use cookiecode_config::{AgentProfile, ModelConfig, ProviderConfig, ProviderType};
    use futures_util::{StreamExt, stream};
    use tokio::sync::{Barrier, Notify};

    use super::*;

    struct NoopProvider;

    #[async_trait]
    impl Provider for NoopProvider {
        fn capabilities(&self, _: &ModelId) -> cookiecode_providers::ProviderCapabilities {
            cookiecode_providers::ProviderCapabilities::default()
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

    struct SteeringProvider {
        calls: AtomicUsize,
        first_started: Arc<Barrier>,
        release_first: Notify,
        requests: Mutex<Vec<Vec<ProviderMessage>>>,
    }

    #[async_trait]
    impl Provider for SteeringProvider {
        fn capabilities(&self, _: &ModelId) -> cookiecode_providers::ProviderCapabilities {
            cookiecode_providers::ProviderCapabilities::default()
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

    #[async_trait]
    impl Provider for RetrySteeringProvider {
        fn capabilities(&self, _: &ModelId) -> cookiecode_providers::ProviderCapabilities {
            cookiecode_providers::ProviderCapabilities::default()
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
        config.agents = BTreeMap::from([(
            "test".into(),
            AgentProfile {
                r#type: ConfigAgentType::Primary,
                models: vec![ModelConfig {
                    provider: "test".into(),
                    model: "test-model".into(),
                }],
                ..AgentProfile::default()
            },
        )]);
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

    fn envelope(seq: u64, event: Event) -> EventEnvelope {
        EventEnvelope {
            session_id: SessionId::new_v7(),
            run_id: Some(RunId::new_v7()),
            seq,
            timestamp: jiff::Timestamp::now(),
            event,
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
        .for_each(|result| {
            result
                .expect("append task panicked")
                .expect("append failed")
        });
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
                if provider.calls.load(Ordering::SeqCst) >= 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retry did not start");
        let requests = provider.requests.lock().expect("requests lock poisoned");
        assert!(requests[1].iter().any(|message| {
            matches!(message, ProviderMessage::User { content }
                if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "retry steering"))
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
                .any(|event| matches!(event.event, Event::UserInputApplied { .. }))
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
                    Some(EventSubscriptionMessage::Gap { last_delivered_seq }) => {
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
}
