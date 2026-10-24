//! ACP agent processes (`docs/site/specs/acp-agents.md` section 7).
//!
//! One process per agent per runtime, shared by every session of the
//! agent: started on first use with `agent/start`, initialized over ACP,
//! and stopped after `[acp] idle_shutdown_ms` with no run in progress, when
//! its runtime's connection ends, or when it exits. The engine speaks ACP to
//! it through a [`Peer`] whose frames travel as `agent/message`
//! notifications of the runtime connection: [`RelayHandler`] hands each
//! message from the runtime to its process.
//!
//! Each live ACP session has a channel the process routes the session's
//! `session/update` notifications and `session/request_permission`
//! requests to; the runner of the session's turn reads it.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use cookie_agent_protocol::{MessageFrame, SessionId, Transport, TransportError};
use cookie_agent_runtime_api::{
    protocol::{
        AgentExitedParams, AgentMcpClosedParams, AgentMcpParams, AgentMessageParams, AgentRef,
        Empty, method,
    },
    rpc::{Incoming, IncomingRequest, Peer, RpcError},
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::wire;
use crate::runtime_snapshot::RuntimeAgent;
use crate::runtimes::RuntimeClient;

/// The most sessions whose updates a process keeps before they are routed,
/// and the most updates it keeps of each.
const MAX_EARLY_SESSIONS: usize = 64;
const MAX_EARLY_UPDATES: usize = 256;

/// How long the runtime gets to start an agent and the agent to answer
/// `initialize`.
pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(60);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What a process routes to the runner of one of its sessions.
pub(crate) enum SessionEvent {
    /// A `session/update`'s `update`.
    Update(Value),
    /// A `session/request_permission` to answer.
    Permission(Box<wire::PermissionRequest>, IncomingRequest),
    /// A call of one of cookie's tools through the MCP bridge, to run as a
    /// call of the agent's turn; `reply` takes the MCP `tools/call` result.
    BridgeCall {
        name: String,
        arguments: Value,
        reply: tokio::sync::oneshot::Sender<Value>,
    },
}

/// A process by runtime and the agent's ID on it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ProcessKey {
    pub runtime: String,
    pub agent: String,
}

/// One running agent.
pub(crate) struct AgentProcess {
    pub key: ProcessKey,
    pub client: RuntimeClient,
    pub peer: Peer,
    /// Frames from the runtime, to the peer.
    frames: mpsc::UnboundedSender<Value>,
    /// What the agent answered `initialize` with.
    init: std::sync::OnceLock<wire::InitializeResponse>,
    /// The live sessions' channels, by ACP session ID.
    sessions: Mutex<HashMap<String, mpsc::UnboundedSender<SessionEvent>>>,
    /// Updates of sessions not routed yet, such as the commands an agent
    /// announces right after answering `session/new`: they go to the
    /// session's channel once it is routed.
    early: Mutex<HashMap<String, Vec<Value>>>,
    /// Cancelled when the process is gone.
    pub exited: CancellationToken,
    exit_reason: Mutex<Option<String>>,
    /// Asks the dispatcher to answer once it has routed every message that
    /// arrived before.
    barriers: mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>,
    barrier_queue: Mutex<Option<mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<()>>>>,
    /// Runs in progress.
    busy: AtomicUsize,
    /// Bumped whenever the process is used, so an idle timer that started
    /// before knows it is stale.
    used: AtomicU64,
}

impl AgentProcess {
    /// What the agent answered `initialize` with.
    pub(crate) fn init(&self) -> &wire::InitializeResponse {
        static NONE: std::sync::OnceLock<wire::InitializeResponse> = std::sync::OnceLock::new();
        self.init
            .get()
            .unwrap_or_else(|| NONE.get_or_init(wire::InitializeResponse::default))
    }

    /// Why the process is gone, once it is.
    pub(crate) fn exit_reason(&self) -> Option<String> {
        lock(&self.exit_reason).clone()
    }

    /// Ends the process's life on the engine's side for `reason`.
    fn gone(&self, reason: String) {
        lock(&self.exit_reason).get_or_insert(reason.clone());
        self.peer.close_with(reason);
        self.exited.cancel();
        lock(&self.sessions).clear();
        lock(&self.early).clear();
    }

    /// Routes session `session`'s updates and permission requests to the
    /// returned channel, replacing an earlier route.
    pub(crate) fn route(&self, session: &str) -> mpsc::UnboundedReceiver<SessionEvent> {
        let (sender, receiver) = mpsc::unbounded_channel();
        for update in lock(&self.early).remove(session).unwrap_or_default() {
            let _ = sender.send(SessionEvent::Update(update));
        }
        lock(&self.sessions).insert(session.to_owned(), sender);
        receiver
    }

    /// Waits until every message the agent sent before now is routed to
    /// its session, such as the updates before a prompt's answer.
    pub(crate) async fn flush(&self) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        if self.barriers.send(sender).is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(5), receiver).await;
        }
    }

    /// Stops routing session `session`.
    pub(crate) fn unroute(&self, session: &str) {
        lock(&self.sessions).remove(session);
    }

    /// Hands `event` to the runner of session `session`; the event back when
    /// the session is not routed.
    pub(crate) fn send_to(&self, session: &str, event: SessionEvent) -> Result<(), SessionEvent> {
        match lock(&self.sessions).get(session) {
            Some(sender) => sender.send(event).map_err(|error| error.0),
            None => Err(event),
        }
    }

    fn deliver(&self, session: &str, event: SessionEvent) -> Result<(), SessionEvent> {
        match lock(&self.sessions).get(session) {
            Some(sender) => sender.send(event).map_err(|error| error.0),
            None => match event {
                SessionEvent::Update(update) => {
                    let mut early = lock(&self.early);
                    if early.len() < MAX_EARLY_SESSIONS || early.contains_key(session) {
                        let updates = early.entry(session.to_owned()).or_default();
                        if updates.len() < MAX_EARLY_UPDATES {
                            updates.push(update);
                        }
                    }
                    Ok(())
                }
                other => Err(other),
            },
        }
    }
}

/// Keeps a process from idling out while a run uses it.
pub(crate) struct ProcessUse {
    process: Arc<AgentProcess>,
    processes: Weak<super::super::Inner>,
}

impl Drop for ProcessUse {
    fn drop(&mut self) {
        if self.process.busy.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let stamp = self.process.used.fetch_add(1, Ordering::AcqRel) + 1;
        let Some(inner) = self.processes.upgrade() else {
            return;
        };
        let idle = inner.config.load().runtime.acp.idle_shutdown();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let process = Arc::clone(&self.process);
        let weak = self.processes.clone();
        handle.spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(idle) => {}
                () = process.exited.cancelled() => return,
            }
            if process.busy.load(Ordering::Acquire) != 0
                || process.used.load(Ordering::Acquire) != stamp
            {
                return;
            }
            if let Some(inner) = weak.upgrade() {
                inner.acp.processes.stop(&process, "idle").await;
            }
        });
    }
}

/// Every agent process of the engine.
#[derive(Default)]
pub(crate) struct Processes {
    running: Mutex<HashMap<ProcessKey, Arc<AgentProcess>>>,
    /// Serializes starts, so one agent starts once.
    starting: tokio::sync::Mutex<()>,
    /// The ACP session each cookie session talks in, while its process
    /// lives.
    pub(crate) sessions: Mutex<HashMap<SessionId, super::runner::LiveSession>>,
    /// Which cookie session each MCP bridge serves, by runtime and bridge.
    pub(crate) bridges: Mutex<HashMap<(String, String), super::mcp::BridgeBinding>>,
    /// What wakes the runner of a session's turn when the session takes
    /// input to steer, while that turn can steer its agent.
    steering: Mutex<HashMap<SessionId, Arc<tokio::sync::Notify>>>,
}

impl Processes {
    /// Registers `session`'s turn for steering wake-ups; the returned
    /// notify fires when the session takes input to steer.
    pub(crate) fn watch_steering(&self, session: SessionId) -> Arc<tokio::sync::Notify> {
        Arc::clone(lock(&self.steering).entry(session).or_default())
    }

    pub(crate) fn unwatch_steering(&self, session: SessionId) {
        lock(&self.steering).remove(&session);
    }

    /// Wakes the turn of `session` that steers its agent, if one runs.
    pub(crate) fn steering_input(&self, session: SessionId) {
        if let Some(notify) = lock(&self.steering).get(&session) {
            notify.notify_one();
        }
    }

    /// The running process of `key`, if any.
    pub(crate) fn running(&self, key: &ProcessKey) -> Option<Arc<AgentProcess>> {
        lock(&self.running)
            .get(key)
            .filter(|process| !process.exited.is_cancelled())
            .cloned()
    }

    fn by_connection(&self, client: &RuntimeClient, agent: &str) -> Option<Arc<AgentProcess>> {
        lock(&self.running)
            .get(&ProcessKey {
                runtime: client.runtime_id().to_owned(),
                agent: agent.to_owned(),
            })
            .filter(|process| process.client.epoch() == client.epoch())
            .cloned()
    }

    /// The process of `agent`, started and initialized when it is not
    /// running, marked in use until the returned guard drops.
    pub(crate) async fn acquire(
        &self,
        inner: &Arc<super::super::Inner>,
        agent: &RuntimeAgent,
    ) -> Result<(Arc<AgentProcess>, ProcessUse), String> {
        let key = ProcessKey {
            runtime: agent.runtime.to_string(),
            agent: agent.agent.to_string(),
        };
        let process = match self.running(&key) {
            Some(process) => process,
            None => {
                let _starting = self.starting.lock().await;
                match self.running(&key) {
                    Some(process) => process,
                    None => self.start(inner, key).await?,
                }
            }
        };
        process.busy.fetch_add(1, Ordering::AcqRel);
        process.used.fetch_add(1, Ordering::AcqRel);
        let guard = ProcessUse {
            process: Arc::clone(&process),
            processes: Arc::downgrade(inner),
        };
        Ok((process, guard))
    }

    async fn start(
        &self,
        inner: &Arc<super::super::Inner>,
        key: ProcessKey,
    ) -> Result<Arc<AgentProcess>, String> {
        let connection = inner.runtimes.agent_connection(&key.runtime).await?;
        let client = connection.0;
        client
            .agent_request::<_, Empty>(
                method::AGENT_START,
                &AgentRef {
                    agent: key.agent.clone(),
                },
                START_TIMEOUT,
            )
            .await
            .map_err(|reason| {
                format!(
                    "runtime `{}` could not start ACP agent `{}`: {reason}",
                    key.runtime, key.agent
                )
            })?;
        let (frames, frames_rx) = mpsc::unbounded_channel();
        let closed = CancellationToken::new();
        let (peer, incoming) = Peer::start(Box::new(RelayTransport {
            client: client.clone(),
            agent: key.agent.clone(),
            frames: frames_rx,
            closed: closed.clone(),
        }));
        // Registered before `initialize`, so its answer reaches the peer.
        let (barriers, barrier_queue) = mpsc::unbounded_channel();
        let pending = Arc::new(AgentProcess {
            barriers,
            barrier_queue: Mutex::new(Some(barrier_queue)),
            key: key.clone(),
            client: client.clone(),
            peer: peer.clone(),
            frames,
            init: std::sync::OnceLock::new(),
            sessions: Mutex::default(),
            early: Mutex::default(),
            exited: closed,
            exit_reason: Mutex::default(),
            busy: AtomicUsize::new(0),
            used: AtomicU64::new(0),
        });
        lock(&self.running).insert(key.clone(), Arc::clone(&pending));
        let initialized = tokio::time::timeout(
            START_TIMEOUT,
            peer.request::<_, wire::InitializeResponse>(
                wire::method::INITIALIZE,
                &wire::InitializeRequest {
                    protocol_version: wire::PROTOCOL_VERSION,
                    client_capabilities: serde_json::json!({
                        "fs": {"readTextFile": false, "writeTextFile": false},
                        "terminal": false,
                        // Notices and compaction updates as their own
                        // updates rather than message text or a synthetic
                        // tool call (the session notices and compaction
                        // RFDs, which Claude Code's and Codex's adapters
                        // follow).
                        // Boolean config options (`type: boolean`, such as
                        // Claude Code's fast mode) as toggles rather than an
                        // on/off select.
                        "session": {
                            "notices": {},
                            "compaction": {},
                            "configOptions": {"boolean": {}},
                        },
                        // The output of the commands the agent runs, as
                        // `_meta` keys of their tool calls (the terminal
                        // extension of Claude Code's and Codex's adapters).
                        // It asks the client to run nothing: `terminal`
                        // stays `false`.
                        "_meta": {"terminal_output": true},
                    }),
                    client_info: serde_json::json!({
                        "name": "cookie-agent",
                        "title": "Cookie Agent",
                        "version": env!("CARGO_PKG_VERSION"),
                    }),
                },
            ),
        )
        .await;
        let init = match initialized {
            Ok(Ok(init)) => init,
            Ok(Err(error)) => {
                let reason = pending
                    .exit_reason()
                    .unwrap_or_else(|| agent_error_message(&error));
                self.stop(&pending, &reason).await;
                return Err(format!(
                    "ACP agent `{}` of runtime `{}` failed to initialize: {reason}",
                    key.agent, key.runtime
                ));
            }
            Err(_) => {
                self.stop(&pending, "it did not answer `initialize`").await;
                return Err(format!(
                    "ACP agent `{}` of runtime `{}` did not answer `initialize` within {} s",
                    key.agent,
                    key.runtime,
                    START_TIMEOUT.as_secs()
                ));
            }
        };
        if init.protocol_version != wire::PROTOCOL_VERSION {
            let reason = format!(
                "it speaks ACP version {}, and cookie speaks version {}",
                init.protocol_version,
                wire::PROTOCOL_VERSION
            );
            self.stop(&pending, &reason).await;
            return Err(format!(
                "ACP agent `{}` of runtime `{}` cannot be used: {reason}",
                key.agent, key.runtime
            ));
        }
        let _ = pending.init.set(init);
        let barriers = lock(&pending.barrier_queue)
            .take()
            .expect("the dispatcher starts once");
        tokio::spawn(dispatch(Arc::downgrade(&pending), incoming, barriers));
        Ok(pending)
    }

    /// Stops `process` for `reason`: it is gone at once, and the runtime is
    /// asked to stop it.
    pub(crate) async fn stop(&self, process: &Arc<AgentProcess>, reason: &str) {
        {
            let mut running = lock(&self.running);
            if running
                .get(&process.key)
                .is_some_and(|current| Arc::ptr_eq(current, process))
            {
                running.remove(&process.key);
            }
        }
        process.gone(format!("the agent's process was stopped ({reason})"));
        self.forget_sessions_of(process);
        let _ = process
            .client
            .agent_request::<_, Empty>(
                method::AGENT_STOP,
                &AgentRef {
                    agent: process.key.agent.clone(),
                },
                Duration::from_secs(10),
            )
            .await;
    }

    /// Stops every process, as the engine shuts down.
    pub(crate) async fn stop_all(&self) {
        let running = lock(&self.running).drain().collect::<Vec<_>>();
        for (_, process) in running {
            process.gone("the daemon is shutting down".to_owned());
            let _ = process
                .client
                .agent_request::<_, Empty>(
                    method::AGENT_STOP,
                    &AgentRef {
                        agent: process.key.agent.clone(),
                    },
                    Duration::from_secs(3),
                )
                .await;
        }
    }

    fn exited(&self, client: &RuntimeClient, agent: &str, reason: String) {
        let Some(process) = self.by_connection(client, agent) else {
            return;
        };
        lock(&self.running).remove(&process.key);
        process.gone(reason);
        self.forget_sessions_of(&process);
    }

    /// Drops the ACP sessions of `process`, which is gone, and the bridges
    /// bound to them: a later turn of one of those cookie sessions resumes
    /// its ACP session in a new process, over a new bridge.
    fn forget_sessions_of(&self, process: &Arc<AgentProcess>) {
        let mut bridges = Vec::new();
        lock(&self.sessions).retain(|_, live| {
            if !Arc::ptr_eq(&live.process, process) {
                return true;
            }
            if let Some(bridge) = &live.bridge {
                bridges.push((process.key.runtime.clone(), bridge.clone()));
            }
            false
        });
        let mut bound = lock(&self.bridges);
        for bridge in bridges {
            bound.remove(&bridge);
        }
    }

    fn connection_lost(&self, client: &RuntimeClient) {
        let lost = {
            let mut running = lock(&self.running);
            let keys = running
                .iter()
                .filter(|(_, process)| {
                    process.client.runtime_id() == client.runtime_id()
                        && process.client.epoch() == client.epoch()
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| running.remove(&key))
                .collect::<Vec<_>>()
        };
        for process in lost {
            process.gone(format!(
                "the connection to runtime `{}` ended",
                client.runtime_id()
            ));
            self.forget_sessions_of(&process);
        }
    }
}

/// The message of an error an agent answered, or why there was no answer.
pub(crate) fn agent_error_message(error: &RpcError) -> String {
    match error {
        RpcError::Remote { message, data, .. } => match data
            .as_ref()
            .and_then(|data| data.get("message").or_else(|| data.get("details")))
            .and_then(Value::as_str)
        {
            Some(detail) if !detail.is_empty() && detail != message => {
                format!("{message}: {detail}")
            }
            _ => message.clone(),
        },
        other => other.to_string(),
    }
}

/// Routes what the agent sends: updates and permission requests to their
/// session, other requests refused.
async fn dispatch(
    process: Weak<AgentProcess>,
    mut incoming: mpsc::Receiver<Incoming>,
    mut barriers: mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<()>>,
) {
    loop {
        // Messages first: a barrier is answered only once every message
        // that arrived before it is routed.
        let message = tokio::select! {
            biased;
            message = incoming.recv() => match message {
                Some(message) => message,
                None => return,
            },
            barrier = barriers.recv() => {
                if let Some(barrier) = barrier {
                    let _ = barrier.send(());
                }
                continue;
            }
        };
        let Some(process) = process.upgrade() else {
            return;
        };
        match message {
            Incoming::Notification(notification) => {
                if notification.method != wire::method::SESSION_UPDATE {
                    continue;
                }
                if let Ok(update) = notification.params::<wire::SessionNotification>() {
                    let _ =
                        process.deliver(&update.session_id, SessionEvent::Update(update.update));
                }
            }
            Incoming::Request(request) => {
                if request.method != wire::method::SESSION_REQUEST_PERMISSION {
                    let method = request.method.clone();
                    tokio::spawn(async move {
                        let _ = request
                            .fail(RpcError::remote(
                                -32601,
                                format!("cookie does not offer `{method}`"),
                            ))
                            .await;
                    });
                    continue;
                }
                let params = match request.params::<wire::PermissionRequest>() {
                    Ok(params) => params,
                    Err(error) => {
                        tokio::spawn(async move {
                            let _ = request.fail(error).await;
                        });
                        continue;
                    }
                };
                let session = params.session_id.clone();
                if let Err(SessionEvent::Permission(_, request)) = process.deliver(
                    &session,
                    SessionEvent::Permission(Box::new(params), request),
                ) {
                    // No turn of the session runs: nothing can answer.
                    tokio::spawn(async move {
                        let _ = request.respond(&wire::permission_outcome(None)).await;
                    });
                }
            }
        }
    }
}

/// An agent's ACP messages over its runtime connection.
struct RelayTransport {
    client: RuntimeClient,
    agent: String,
    frames: mpsc::UnboundedReceiver<Value>,
    closed: CancellationToken,
}

#[async_trait]
impl Transport for RelayTransport {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        let message = match frame {
            MessageFrame::Value(value) => value,
            MessageFrame::Text(text) => serde_json::from_str(&text)?,
            MessageFrame::Bytes(bytes) => serde_json::from_slice(&bytes)?,
        };
        self.client
            .agent_notify(
                method::AGENT_MESSAGE,
                &AgentMessageParams {
                    agent: self.agent.clone(),
                    message,
                },
            )
            .await
            .map_err(TransportError::Other)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        tokio::select! {
            () = self.closed.cancelled() => Ok(None),
            frame = self.frames.recv() => Ok(frame.map(MessageFrame::Value)),
        }
    }
}

/// What the engine does with the runtimes' `agent/*` notifications.
pub(crate) struct RelayHandler {
    inner: Weak<super::super::Inner>,
}

impl RelayHandler {
    pub(crate) fn new(inner: Weak<super::super::Inner>) -> Self {
        Self { inner }
    }
}

impl crate::runtimes::AcpRelayHandler for RelayHandler {
    fn message(&self, client: &RuntimeClient, params: AgentMessageParams) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if let Some(process) = inner.acp.processes.by_connection(client, &params.agent) {
            let _ = process.frames.send(params.message);
        }
    }

    fn exited(&self, client: &RuntimeClient, params: AgentExitedParams) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut reason = match (params.status, params.message) {
            (Some(status), _) => format!("the agent's process exited with status {status}"),
            (None, Some(message)) => format!("the agent's process ended: {message}"),
            (None, None) => "the agent's process ended".to_owned(),
        };
        if !params.stderr.is_empty() {
            reason.push_str("; stderr:\n");
            reason.push_str(&params.stderr);
        }
        inner.acp.processes.exited(client, &params.agent, reason);
    }

    fn mcp(&self, client: &RuntimeClient, params: AgentMcpParams) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        super::mcp::message(super::super::Engine { inner }, client.clone(), params);
    }

    fn mcp_closed(&self, client: &RuntimeClient, params: AgentMcpClosedParams) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        super::mcp::connection_closed(&inner, client, &params);
    }

    fn connection_lost(&self, client: &RuntimeClient) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        inner.acp.processes.connection_lost(client);
    }
}
