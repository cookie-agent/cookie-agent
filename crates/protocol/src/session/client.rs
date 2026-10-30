//! Shared protocol-10 JSON-RPC client session.

mod runtime;

use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use super::rt;
use crate::{
    ApprovalListParams, ApprovalListResult, ApprovalRespondParams, ApprovalRespondResult,
    ClientEvent, ClientHello, ClientIdentity, ClientKind, ClientSubscriptionMessage, EventOrigin,
    EventsSubscribeParams, HandshakeToken, JsonRpcError, JsonRpcId, JsonRpcVersion,
    McpAuthBeginParams, McpAuthBeginResult, McpAuthCancelParams, McpAuthCancelResult,
    McpAuthCompleteParams, McpAuthCompleteResult, McpServerAddParams, McpServerEditParams,
    McpServerListParams, McpServerListResult, McpServerMutationResult, McpServerNameParams,
    McpServerPersistParams, McpServerReconnectParams, McpServerSetEnabledParams, MessageFrame,
    RunCancelParams, RunCancelResult, RunStartParams, RunStartResult, RunSteerParams,
    RunSteerResult, ServerHello, SessionChildrenParams, SessionChildrenResult,
    SessionCompactParams, SessionCompactResult, SessionCreateParams, SessionCreateResult,
    SessionForkParams, SessionForkResult, SessionGetParams, SessionGetResult, SessionGoalGetParams,
    SessionGoalGetResult, SessionGoalLifecycleParams, SessionGoalLifecycleResult,
    SessionGoalSetParams, SessionGoalSetResult, SessionId, SessionListParams, SessionListResult,
    SessionPermissionClearParams, SessionPermissionGetParams, SessionPermissionGetResult,
    SessionPermissionMutationResult, SessionPermissionPreviewParams,
    SessionPermissionPreviewResult, SessionPermissionSetParams, SessionProducersParams,
    SessionProducersResult, SessionRenameParams, SessionRenameResult, SessionResumeParams,
    SessionResumeResult, SessionRevertParams, SessionRevertResult, SessionSetPermissionModeParams,
    SessionSetPermissionModeResult, SessionTreeParams, SessionTreeResult, SessionTreeUsageResult,
    SessionUsageParams, SessionUsageResult, SkillsGetParams, SkillsGetResult, SkillsListParams,
    SkillsListResult, Transport, TransportError,
};
use jiff::Timestamp;
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::{Value, value::RawValue};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};
use zeroize::{Zeroize, Zeroizing};

const COMMAND_QUEUE_CAPACITY: usize = 128;
const RECOVERY_ATTEMPTS: usize = 6;
/// How long the daemon may take to answer one replay page once its request
/// has been written. It bounds the daemon's work, not this client's queue.
const REPLAY_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one replay page may wait in this client's own queue to be written.
const REPLAY_QUEUE_TIMEOUT: Duration = Duration::from_secs(30);
/// Events per replay page. Pages keep each response small enough to answer
/// well inside [`REPLAY_RESPONSE_TIMEOUT`] however long the session is.
const REPLAY_PAGE_EVENTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(2_000).unwrap();

/// The sole ordered delivery stream consumed by the UI.
///
/// The connection task is its only producer. Replays are injected into this
/// stream before the live notifications buffered while their RPC is in flight.
///
/// The client reads events as [`ClientEvent`]s: one this build cannot read is
/// delivered as [`ClientDelivery::UnknownEvent`] in its place, so that it
/// still advances the session's sequence, and live-only output it cannot read
/// is dropped.
#[derive(Clone, Debug)]
pub enum ClientDelivery {
    Live {
        message: Box<crate::EventSubscriptionMessage>,
        generation: u64,
    },
    ReplayStart {
        session_id: SessionId,
        generation: u64,
        final_seq: u64,
        rebuild: bool,
    },
    ReplayEvent {
        session_id: SessionId,
        generation: u64,
        final_seq: u64,
        event: Box<crate::StoredEvent>,
    },
    /// A durable event this build could not read, live or in a replay (with
    /// the replay's `final_seq`). It advances the session's sequence and
    /// changes nothing else.
    UnknownEvent {
        event: Box<crate::UnknownEvent>,
        generation: u64,
        replay_final_seq: Option<u64>,
    },
    ReplayEnd {
        session_id: SessionId,
        generation: u64,
        final_seq: u64,
    },
    PluginEvent(crate::ExtensionBusEventParams),
    RecoveryFailed {
        session_id: Option<SessionId>,
        error: String,
    },
    Disconnected {
        error: String,
    },
    RuntimeChanged(Box<crate::RuntimeChangedNotification>),
    SessionsChanged(Box<crate::SessionsChangedNotification>),
    /// MCP server configuration or status changed; refetch
    /// `mcp.server.list`. Carries connection failures for subscribed
    /// sessions.
    McpChanged(crate::McpChangedNotification),
    /// A runtime template or instance status changed; refetch
    /// `runtimes.list`. Carries instance failures for subscribed sessions.
    RuntimesChanged(crate::RuntimesChangedNotification),
    /// A subscribed session's background calls changed.
    BackgroundChanged(crate::BackgroundChangedNotification),
    /// A plugin's status changed; refetch `plugins.list`.
    PluginsChanged(crate::PluginsChangedNotification),
    /// The machine registry changed, or a runtime joined a new machine;
    /// refetch `machines.list` and `runtimes.list`.
    MachinesChanged(crate::MachinesChangedNotification),
    /// A memory file changed through the daemon; refetch `memory.list`.
    MemoryChanged(crate::MemoryChangedNotification),
    /// A project was created, changed, or deleted; refetch
    /// `projects.list`.
    ProjectsChanged(crate::ProjectsChangedNotification),
    /// A cron job was created, changed, fired, or deleted; refetch
    /// `crons.list`.
    CronsChanged(crate::CronsChangedNotification),
    /// The push subscriptions changed; refetch `notifications.get`.
    NotificationsChanged(crate::NotificationsChangedNotification),
    /// The daemon will restart: when, and how many runs it still waits
    /// for. The connection then closes and the client reconnects.
    DaemonRestarting(crate::DaemonRestartingNotification),
    /// Whether the daemon is set up changed: the new `onboarding.get`
    /// answer.
    OnboardingChanged(crate::OnboardingStatus),
    /// A notification this build could not read was dropped. No call fails
    /// for it; the message says which notification and why.
    Diagnostic {
        message: String,
    },
}

type Deliveries = mpsc::UnboundedSender<ClientDelivery>;

/// Errors returned by the transport or a JSON-RPC operation.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("transport closed")]
    Closed,
    #[error("transport failed: {0}")]
    Transport(String),
    #[error("invalid JSON-RPC frame: {0}")]
    InvalidFrame(#[from] serde_json::Error),
    #[error("{}", crate::diagnostics::rpc(.0))]
    Rpc(JsonRpcError),
    #[error("websocket error: {0}")]
    WebSocket(String),
    /// The connection closed before the daemon answered its handshake, as
    /// when the daemon refuses the WebSocket upgrade. A browser shows a page
    /// no HTTP status for that: it sees only this, with `code` 1006.
    #[error("{}", refused_before_handshake(*.code, .reason))]
    ConnectionRefusedBeforeHandshake { code: u16, reason: String },
    #[error("a subscription replay is already in progress")]
    ReplayInProgress,
    #[error("subscription replay response timed out")]
    ReplayTimedOut,
    /// The daemon answered with an outcome this build does not know.
    #[error("the daemon answered {method} with `{outcome}`, which this client does not know")]
    UnknownOutcome { method: String, outcome: String },
    /// The daemon does not know a method this client expected it to have.
    #[error("the daemon is too old for {method}")]
    DaemonTooOld { method: String },
}

fn refused_before_handshake(code: u16, reason: &str) -> String {
    let mut message = format!("the connection closed before the handshake (code {code})");
    if !reason.is_empty() {
        message.push_str(&format!(": {reason}"));
    }
    message
}

impl ClientError {
    /// What the daemon's error says beyond its message, read per code.
    #[must_use]
    pub fn error_data(&self) -> Option<crate::errors::ErrorData> {
        match self {
            Self::Rpc(rpc) => Some(crate::errors::ErrorData::of(rpc)),
            _ => None,
        }
    }

    /// The daemon error's stable `data.code`, such as `lock_contention` or
    /// `session_owned_elsewhere`. Clients branch only on it.
    #[must_use]
    pub fn error_code(&self) -> Option<&str> {
        match self {
            Self::Rpc(rpc) => rpc
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str),
            _ => None,
        }
    }

    /// Whether the daemon answered with the registry's `code`.
    #[must_use]
    pub fn is(&self, code: &crate::errors::ErrorCode) -> bool {
        self.error_code() == Some(code.code)
    }

    /// The technical detail of a daemon error, shown on request.
    #[must_use]
    pub fn detail(&self) -> Option<String> {
        self.error_data()
            .and_then(|data| data.detail)
            .filter(|detail| !detail.is_empty())
    }
}

#[derive(Clone, Copy)]
struct ReplayRequest {
    session_id: SessionId,
    generation: u64,
    rebuild: bool,
    attempt: u64,
}

struct Command {
    id: i64,
    method: String,
    params: SerializedParams,
    replay: Option<ReplayRequest>,
    /// Signalled once the request frame is on the transport.
    written: Option<oneshot::Sender<()>>,
    response: oneshot::Sender<CommandResult>,
}

/// A call's result as the server serialized it, decoded by the caller into
/// the type it expects.
type CommandResult = Result<Box<RawValue>, ClientError>;

struct SerializedParams {
    value: Value,
    sensitive: bool,
}

impl Drop for SerializedParams {
    fn drop(&mut self) {
        if self.sensitive {
            zeroize_json(&mut self.value);
            record_sensitive_serialized_wipe();
        }
    }
}

pub(in crate::session::client) struct SensitiveJson(Value);

impl SensitiveJson {
    pub(in crate::session::client) fn object() -> Self {
        Self(Value::Object(serde_json::Map::new()))
    }

    pub(in crate::session::client) fn object_mut(&mut self) -> &mut serde_json::Map<String, Value> {
        self.0
            .as_object_mut()
            .expect("sensitive JSON owner was created as an object")
    }

    fn take(&mut self) -> Value {
        std::mem::replace(&mut self.0, Value::Null)
    }
}

impl Drop for SensitiveJson {
    fn drop(&mut self) {
        if self.0 != Value::Null {
            zeroize_json(&mut self.0);
            record_sensitive_serialized_wipe();
        }
    }
}

#[derive(Serialize)]
struct BorrowedRequest<'a> {
    jsonrpc: &'static str,
    id: i64,
    method: &'a str,
    params: &'a Value,
}

enum OutboundFrame {
    Public(MessageFrame),
    Sensitive(SensitiveFrame),
}

/// A serialized secret-bearing request retained in a wiping buffer until the
/// transport accepts ownership. Once dispatched, WebSocket/channel/kernel
/// buffers are transport-owned and cannot honestly be promised to be wiped by
/// the TUI.
struct SensitiveFrame {
    text: Zeroizing<String>,
}

impl SensitiveFrame {
    fn new(text: String) -> Self {
        Self {
            text: Zeroizing::new(text),
        }
    }
}

impl fmt::Debug for SensitiveFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SensitiveFrame(<redacted>)")
    }
}

impl Drop for SensitiveFrame {
    fn drop(&mut self) {
        if !self.text.is_empty() {
            self.text.zeroize();
            record_sensitive_frame_wipe();
        }
    }
}

struct PendingCommand {
    replay: Option<ReplayRequest>,
    response: oneshot::Sender<CommandResult>,
}

#[derive(Default)]
struct Subscription {
    cursor: u64,
    /// Timestamp of the event at `cursor`, when this view received it. A
    /// replay from `cursor` sends it so the server can tell whether a revert
    /// has since replaced that event (`stale_cursor`).
    cursor_timestamp: Option<Timestamp>,
    rollback_cursor: u64,
    rollback_cursor_timestamp: Option<Timestamp>,
    generation: u64,
    next_attempt: u64,
    active_attempt: u64,
    fetching: bool,
    rebuild: bool,
    final_seq: u64,
    final_timestamp: Option<Timestamp>,
    /// Live messages that arrived while a replay was in flight, with the
    /// generation they arrived in.
    buffered: Vec<(ClientSubscriptionMessage, u64)>,
    recovery_requested: Option<bool>,
    /// Earlier pages of the replay attempt `replay_pages_attempt`, held until
    /// its final page arrives and the whole replay is delivered at once.
    replay_pages: Vec<ClientEvent>,
    replay_pages_attempt: u64,
    /// The view unsubscribed: live messages still in flight for the session
    /// are dropped and recovery leaves it alone until it subscribes again.
    closed: bool,
}

impl Subscription {
    fn abort_replay(&mut self) {
        self.cursor = self.rollback_cursor;
        self.cursor_timestamp = self.rollback_cursor_timestamp;
        self.fetching = false;
        self.replay_pages = Vec::new();
    }
}

struct RecoveryQueue {
    sender: mpsc::UnboundedSender<(bool, Option<SessionId>)>,
    state: StdMutex<RecoveryQueueState>,
}

#[derive(Default)]
struct RecoveryQueueState {
    queued: bool,
    follow_up: bool,
    follow_up_full: bool,
    follow_up_session: Option<SessionId>,
}

enum ConnectionControl {
    RecoveryFailed {
        session_id: Option<SessionId>,
        error: String,
    },
}

/// A cloneable client handle. Calls and the single ordered delivery stream
/// share one serialized message stream, regardless of transport.
#[derive(Clone)]
pub struct Client {
    commands: mpsc::Sender<Command>,
    deliveries: Arc<StdMutex<Option<mpsc::UnboundedReceiver<ClientDelivery>>>>,
    subscriptions: Arc<Mutex<HashMap<SessionId, Subscription>>>,
    recovery: Arc<RecoveryQueue>,
    shutdown: Arc<ClientShutdown>,
    /// The origin the daemon stamps on this connection's events, from the
    /// handshake response.
    origin: Arc<StdMutex<Option<EventOrigin>>>,
    /// The daemon's client protocol version, from the handshake response.
    daemon_version: Arc<StdMutex<Option<crate::SemVer>>>,
    #[cfg(all(test, not(target_arch = "wasm32")))]
    pending_command_count: Arc<AtomicUsize>,
}

struct ClientShutdown(tokio_util::sync::CancellationToken);

impl Drop for ClientShutdown {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Client {
    /// Connect an already-created message stream and start its routing task.
    pub fn connect_stream<T>(transport: T) -> Self
    where
        T: Transport + 'static,
    {
        // The connection task is the RPC router and never awaits UI
        // consumption. This sole-consumer queue is lossless; a permanently
        // stalled UI may grow memory, but it is terminal rather than lossy.
        let (delivery_sender, delivery_receiver) = mpsc::unbounded_channel();
        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let deliveries = Arc::new(StdMutex::new(Some(delivery_receiver)));
        let subscriptions = Arc::new(Mutex::new(HashMap::new()));
        let (recovery_sender, recovery_receiver) = mpsc::unbounded_channel();
        let recovery = Arc::new(RecoveryQueue {
            sender: recovery_sender,
            state: StdMutex::new(RecoveryQueueState::default()),
        });
        let (control_sender, control_rx) = mpsc::unbounded_channel();
        let pending_command_count = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(ClientShutdown(tokio_util::sync::CancellationToken::new()));
        rt::spawn(connection_task(
            transport,
            ConnectionTask {
                commands: command_rx,
                controls: control_rx,
                deliveries: delivery_sender,
                subscriptions: subscriptions.clone(),
                recovery: recovery.clone(),
                pending_command_count: pending_command_count.clone(),
                shutdown: shutdown.0.clone(),
            },
        ));
        let client = Self {
            commands,
            deliveries,
            subscriptions,
            recovery: recovery.clone(),
            shutdown,
            origin: Arc::new(StdMutex::new(None)),
            daemon_version: Arc::new(StdMutex::new(None)),
            #[cfg(all(test, not(target_arch = "wasm32")))]
            pending_command_count,
        };
        spawn_recovery_worker(
            client.commands.clone(),
            client.subscriptions.clone(),
            recovery_receiver,
            recovery,
            control_sender,
            REPLAY_RESPONSE_TIMEOUT,
        );
        client
    }

    /// Complete the versioned protocol handshake as a generic `client` with
    /// a fresh instance id. Long-lived clients that must recognize their own
    /// events across reconnects use [`Self::handshake_as`].
    pub async fn handshake(&self) -> Result<ServerHello, ClientError> {
        self.handshake_as(ClientIdentity::generate(
            ClientKind::new("client").expect("static client kind is valid"),
        ))
        .await
    }

    /// Complete the versioned protocol handshake as `client`. The transport
    /// is expected to have presented the daemon's token (the WebSocket
    /// transport sends it in `Authorization`); [`Self::handshake_with_token`]
    /// sends it in the handshake instead.
    pub async fn handshake_as(&self, client: ClientIdentity) -> Result<ServerHello, ClientError> {
        self.handshake_with(ClientHello::new(client)).await
    }

    /// Complete the versioned protocol handshake as `client`, authenticating
    /// with the daemon's `token` in the handshake, as a browser must: it
    /// cannot set `Authorization` on a WebSocket.
    pub async fn handshake_with_token(
        &self,
        client: ClientIdentity,
        token: HandshakeToken,
    ) -> Result<ServerHello, ClientError> {
        let mut hello = ClientHello::new(client);
        hello.token = Some(token);
        self.handshake_with(hello).await
    }

    /// Complete the protocol handshake with `hello`. A daemon of another
    /// client protocol version refuses it with `unsupported_protocol`, and
    /// a daemon whose version this build cannot speak with is refused here.
    /// A hello carrying a token is sent from wiping buffers.
    pub async fn handshake_with(&self, hello: ClientHello) -> Result<ServerHello, ClientError> {
        let raw: Value = if hello.token.is_some() {
            self.call_sensitive("handshake", hello).await?
        } else {
            self.call("handshake", &hello).await?
        };
        crate::negotiate_version(crate::CLIENT_PROTOCOL_VERSION, &raw)
            .map_err(|refusal| ClientError::Rpc(refusal.to_rpc_error()))?;
        let hello: ServerHello = serde_json::from_value(raw)?;
        *self.origin.lock().expect("client origin lock") = Some(hello.origin.clone());
        *self.daemon_version.lock().expect("daemon version lock") = Some(hello.protocol_version);
        Ok(hello)
    }

    /// The daemon's client protocol version, once the handshake has
    /// completed.
    #[must_use]
    pub fn daemon_version(&self) -> Option<crate::SemVer> {
        *self.daemon_version.lock().expect("daemon version lock")
    }

    /// The origin the daemon stamps on events this connection causes, once
    /// the handshake has completed.
    #[must_use]
    pub fn origin(&self) -> Option<EventOrigin> {
        self.origin.lock().expect("client origin lock").clone()
    }

    /// Take the sole live and replay delivery receiver. It never
    /// backpressures the connection task.
    #[must_use]
    pub fn subscribe_deliveries(&self) -> Option<mpsc::UnboundedReceiver<ClientDelivery>> {
        self.deliveries
            .lock()
            .expect("delivery receiver lock")
            .take()
    }

    /// End the connection and fail outstanding calls with [`ClientError::Closed`].
    pub fn shutdown(&self) {
        self.shutdown.0.cancel();
    }

    pub async fn create_session(
        &self,
        params: SessionCreateParams,
    ) -> Result<SessionCreateResult, ClientError> {
        self.call("session.create", &params).await
    }

    pub async fn list_sessions(
        &self,
        params: SessionListParams,
    ) -> Result<SessionListResult, ClientError> {
        self.call("session.list", &params).await
    }

    pub async fn get_session(
        &self,
        params: SessionGetParams,
    ) -> Result<SessionGetResult, ClientError> {
        self.call("session.get", &params).await
    }

    pub async fn get_session_goal(
        &self,
        params: SessionGoalGetParams,
    ) -> Result<SessionGoalGetResult, ClientError> {
        self.call(crate::SESSION_GOAL_GET_METHOD, &params).await
    }

    pub async fn set_session_goal(
        &self,
        params: SessionGoalSetParams,
    ) -> Result<SessionGoalSetResult, ClientError> {
        self.call(crate::SESSION_GOAL_SET_METHOD, &params).await
    }

    pub async fn change_session_goal_lifecycle(
        &self,
        params: SessionGoalLifecycleParams,
    ) -> Result<SessionGoalLifecycleResult, ClientError> {
        self.call(crate::SESSION_GOAL_LIFECYCLE_METHOD, &params)
            .await
    }

    pub async fn session_producers(
        &self,
        params: SessionProducersParams,
    ) -> Result<SessionProducersResult, ClientError> {
        self.call(crate::SESSION_PRODUCERS_METHOD, &params).await
    }

    pub async fn session_usage(
        &self,
        params: SessionUsageParams,
    ) -> Result<SessionUsageResult, ClientError> {
        self.call("session.usage", &params).await
    }

    pub async fn session_tree_usage(
        &self,
        params: SessionUsageParams,
    ) -> Result<SessionTreeUsageResult, ClientError> {
        self.call("session.tree_usage", &params).await
    }

    pub async fn session_children(
        &self,
        params: SessionChildrenParams,
    ) -> Result<SessionChildrenResult, ClientError> {
        self.call("session.children", &params).await
    }

    pub async fn session_tree(
        &self,
        params: SessionTreeParams,
    ) -> Result<SessionTreeResult, ClientError> {
        self.call("session.tree", &params).await
    }

    pub async fn resume_session(
        &self,
        params: SessionResumeParams,
    ) -> Result<SessionResumeResult, ClientError> {
        self.call("session.resume", &params).await
    }

    pub async fn rename_session(
        &self,
        params: SessionRenameParams,
    ) -> Result<SessionRenameResult, ClientError> {
        self.call("session.rename", &params).await
    }

    pub async fn set_permission_mode(
        &self,
        params: SessionSetPermissionModeParams,
    ) -> Result<SessionSetPermissionModeResult, ClientError> {
        self.call("session.set_permission_mode", &params).await
    }

    pub async fn get_session_permissions(
        &self,
        params: SessionPermissionGetParams,
    ) -> Result<SessionPermissionGetResult, ClientError> {
        self.call("session.permission.get", &params).await
    }

    pub async fn set_session_permission(
        &self,
        params: SessionPermissionSetParams,
    ) -> Result<SessionPermissionMutationResult, ClientError> {
        self.call("session.permission.set", &params).await
    }

    pub async fn clear_session_permission(
        &self,
        params: SessionPermissionClearParams,
    ) -> Result<SessionPermissionMutationResult, ClientError> {
        self.call("session.permission.clear", &params).await
    }

    pub async fn preview_session_permissions(
        &self,
        params: SessionPermissionPreviewParams,
    ) -> Result<SessionPermissionPreviewResult, ClientError> {
        self.call("session.permission.preview", &params).await
    }

    pub async fn list_skills(
        &self,
        params: SkillsListParams,
    ) -> Result<SkillsListResult, ClientError> {
        self.call("skills.list", &params).await
    }

    pub async fn get_skill(&self, params: SkillsGetParams) -> Result<SkillsGetResult, ClientError> {
        self.call("skills.get", &params).await
    }

    /// Refreshes the daemon's official skills now.
    pub async fn update_skills(&self) -> Result<crate::SkillsUpdateResult, ClientError> {
        self.call(crate::SKILLS_UPDATE_METHOD, &crate::SkillsUpdateParams {})
            .await
    }

    /// Refreshes the daemon's official agents now.
    pub async fn update_agents(&self) -> Result<crate::AgentsUpdateResult, ClientError> {
        self.call(crate::AGENTS_UPDATE_METHOD, &crate::AgentsUpdateParams {})
            .await
    }

    pub async fn compact_session(
        &self,
        params: SessionCompactParams,
    ) -> Result<SessionCompactResult, ClientError> {
        self.call("session.compact", &params).await
    }

    pub async fn revert_session(
        &self,
        params: SessionRevertParams,
    ) -> Result<SessionRevertResult, ClientError> {
        self.call("session.revert", &params).await
    }

    /// Deletes a root session's whole tree (client protocol 1.5). A daemon
    /// older than 1.5 answers [`ClientError::DaemonTooOld`].
    pub async fn delete_session(
        &self,
        params: crate::SessionDeleteParams,
    ) -> Result<crate::SessionDeleteResult, ClientError> {
        self.call(crate::SESSION_DELETE_METHOD, &params).await
    }

    pub async fn fork_session(
        &self,
        params: SessionForkParams,
    ) -> Result<SessionForkResult, ClientError> {
        self.call("session.fork", &params).await
    }

    pub async fn start_run(&self, params: RunStartParams) -> Result<RunStartResult, ClientError> {
        self.call("run.start", &params).await
    }

    pub async fn steer_run(&self, params: RunSteerParams) -> Result<RunSteerResult, ClientError> {
        self.call("run.steer", &params).await
    }

    /// Set the session's model selection for every client
    /// (`session.select_model`).
    pub async fn select_model(
        &self,
        params: crate::SessionSelectModelParams,
    ) -> Result<crate::SessionSelectModelResult, ClientError> {
        self.call(crate::SESSION_SELECT_MODEL_METHOD, &params).await
    }

    /// Clear this held session's hold and send its pending producer
    /// messages (`session.held.resume`, client protocol 1.34.0).
    pub async fn resume_held_session(
        &self,
        params: crate::SessionHeldResumeParams,
    ) -> Result<crate::SessionHeldResumeResult, ClientError> {
        self.call(crate::SESSION_HELD_RESUME_METHOD, &params).await
    }

    /// Cancel this session's running manual compaction
    /// (`session.compact.cancel`).
    pub async fn cancel_compaction(
        &self,
        params: crate::SessionCompactCancelParams,
    ) -> Result<crate::SessionCompactCancelResult, ClientError> {
        self.call(crate::SESSION_COMPACT_CANCEL_METHOD, &params)
            .await
    }

    /// Start a run, steer the running one, or queue behind it, decided
    /// atomically by the daemon (`session.submit_input`).
    pub async fn submit_input(
        &self,
        params: crate::SessionSubmitInputParams,
    ) -> Result<crate::SessionSubmitInputResult, ClientError> {
        known_outcome(
            crate::SESSION_SUBMIT_INPUT_METHOD,
            self.call(crate::SESSION_SUBMIT_INPUT_METHOD, &params)
                .await?,
        )
    }

    /// Take back one of this client's pending or queued inputs
    /// (`session.recall_input`).
    pub async fn recall_input(
        &self,
        params: crate::SessionRecallInputParams,
    ) -> Result<crate::SessionRecallInputResult, ClientError> {
        known_outcome(
            crate::SESSION_RECALL_INPUT_METHOD,
            self.call(crate::SESSION_RECALL_INPUT_METHOD, &params)
                .await?,
        )
    }

    /// Start an upload into a session tree's store (`artifact.upload.begin`).
    pub async fn begin_upload(
        &self,
        params: crate::ArtifactUploadBeginParams,
    ) -> Result<crate::ArtifactUploadBeginResult, ClientError> {
        self.call(crate::ARTIFACT_UPLOAD_BEGIN_METHOD, &params)
            .await
    }

    /// Send an upload's next chunk (`artifact.upload.chunk`).
    pub async fn upload_chunk(
        &self,
        params: crate::ArtifactUploadChunkParams,
    ) -> Result<crate::ArtifactUploadChunkResult, ClientError> {
        self.call(crate::ARTIFACT_UPLOAD_CHUNK_METHOD, &params)
            .await
    }

    /// Store a complete upload (`artifact.upload.finish`).
    pub async fn finish_upload(
        &self,
        params: crate::ArtifactUploadFinishParams,
    ) -> Result<crate::ArtifactUploadFinishResult, ClientError> {
        self.call(crate::ARTIFACT_UPLOAD_FINISH_METHOD, &params)
            .await
    }

    /// Drop an upload in progress (`artifact.upload.cancel`).
    pub async fn cancel_upload(
        &self,
        params: crate::ArtifactUploadCancelParams,
    ) -> Result<crate::ArtifactUploadCancelResult, ClientError> {
        self.call(crate::ARTIFACT_UPLOAD_CANCEL_METHOD, &params)
            .await
    }

    /// Read a range of an artifact of a session's tree (`artifact.read`).
    pub async fn read_artifact(
        &self,
        params: crate::ArtifactReadParams,
    ) -> Result<crate::ArtifactReadResult, ClientError> {
        self.call(crate::ARTIFACT_READ_METHOD, &params).await
    }

    pub async fn cancel_run(
        &self,
        params: RunCancelParams,
    ) -> Result<RunCancelResult, ClientError> {
        self.call("run.cancel", &params).await
    }

    pub async fn respond_approval(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<ApprovalRespondResult, ClientError> {
        self.call("approval.respond", &params).await
    }

    pub async fn list_approvals(
        &self,
        params: ApprovalListParams,
    ) -> Result<ApprovalListResult, ClientError> {
        self.call("approval.list", &params).await
    }

    /// Answers question `params.elicitation_id` an agent asked the user.
    pub async fn respond_elicitation(
        &self,
        params: crate::ElicitationRespondParams,
    ) -> Result<crate::ElicitationRespondResult, ClientError> {
        self.call(crate::ELICITATION_RESPOND_METHOD, &params).await
    }

    pub async fn begin_mcp_auth(
        &self,
        params: McpAuthBeginParams,
    ) -> Result<McpAuthBeginResult, ClientError> {
        self.call("mcp.auth.begin", &params).await
    }

    pub async fn complete_mcp_auth(
        &self,
        params: McpAuthCompleteParams,
    ) -> Result<McpAuthCompleteResult, ClientError> {
        self.call("mcp.auth.complete", &params).await
    }

    pub async fn cancel_mcp_auth(
        &self,
        params: McpAuthCancelParams,
    ) -> Result<McpAuthCancelResult, ClientError> {
        self.call("mcp.auth.cancel", &params).await
    }

    /// Starts a provider's interactive sign-in (client protocol 1.49.0).
    pub async fn begin_provider_auth(
        &self,
        params: crate::ProviderAuthBeginParams,
    ) -> Result<crate::ProviderAuthBeginResult, ClientError> {
        self.call(crate::PROVIDER_AUTH_BEGIN_METHOD, &params).await
    }

    /// Finishes a provider's sign-in with the address the browser landed on.
    pub async fn complete_provider_auth(
        &self,
        params: crate::ProviderAuthCompleteParams,
    ) -> Result<crate::ProviderAuthCompleteResult, ClientError> {
        self.call(crate::PROVIDER_AUTH_COMPLETE_METHOD, &params)
            .await
    }

    /// Drops a provider's sign-in in progress.
    pub async fn cancel_provider_auth(
        &self,
        params: crate::ProviderAuthCancelParams,
    ) -> Result<crate::ProviderAuthCancelResult, ClientError> {
        self.call(crate::PROVIDER_AUTH_CANCEL_METHOD, &params).await
    }

    /// Attaches a runtime to a session with no active run.
    pub async fn attach_runtime(
        &self,
        params: crate::SessionRuntimeAttachParams,
    ) -> Result<crate::SessionRuntimeAttachResult, ClientError> {
        self.call(crate::SESSION_RUNTIME_ATTACH_METHOD, &params)
            .await
    }

    /// Detaches a runtime from a session with no active run.
    pub async fn detach_runtime(
        &self,
        params: crate::SessionRuntimeDetachParams,
    ) -> Result<crate::SessionRuntimeDetachResult, ClientError> {
        self.call(crate::SESSION_RUNTIME_DETACH_METHOD, &params)
            .await
    }

    /// Lists runtime templates and the instances the daemon started.
    /// Moves an executing call to the background.
    pub async fn background_tool(
        &self,
        params: crate::RunToolBackgroundParams,
    ) -> Result<crate::RunToolBackgroundResult, ClientError> {
        self.call(crate::RUN_TOOL_BACKGROUND_METHOD, &params).await
    }

    pub async fn list_background(
        &self,
        session_id: SessionId,
    ) -> Result<crate::SessionBackgroundListResult, ClientError> {
        self.call(
            crate::SESSION_BACKGROUND_LIST_METHOD,
            &crate::SessionBackgroundListParams { session_id },
        )
        .await
    }

    /// Stops a background call as the user.
    pub async fn stop_background(
        &self,
        session_id: SessionId,
        job: String,
    ) -> Result<crate::SessionBackgroundStopResult, ClientError> {
        self.call(
            crate::SESSION_BACKGROUND_STOP_METHOD,
            &crate::SessionBackgroundStopParams { session_id, job },
        )
        .await
    }

    pub async fn list_runtimes(&self) -> Result<crate::RuntimesListResult, ClientError> {
        self.call(crate::RUNTIMES_LIST_METHOD, &crate::RuntimesListParams {})
            .await
    }

    /// Retries a runtime instance now, such as one that failed.
    pub async fn reconnect_runtime(
        &self,
        params: crate::RuntimesReconnectParams,
    ) -> Result<crate::RuntimesReconnectResult, ClientError> {
        self.call(crate::RUNTIMES_RECONNECT_METHOD, &params).await
    }

    /// Connects a runtime the daemon starts, to try that it starts (client
    /// protocol 1.55.0).
    pub async fn connect_runtime(
        &self,
        params: crate::RuntimesConnectParams,
    ) -> Result<crate::RuntimesConnectResult, ClientError> {
        self.call(crate::RUNTIMES_CONNECT_METHOD, &params).await
    }

    /// Disconnects a runtime the daemon starts; refused while a run or
    /// background call uses it (client protocol 1.55.0).
    pub async fn disconnect_runtime(
        &self,
        params: crate::RuntimesConnectParams,
    ) -> Result<crate::RuntimesConnectResult, ClientError> {
        self.call(crate::RUNTIMES_DISCONNECT_METHOD, &params).await
    }

    /// A runtime's ACP agent declarations (client protocol 1.37.0).
    pub async fn list_runtime_agents(
        &self,
        params: crate::RuntimesAgentsListParams,
    ) -> Result<crate::RuntimesAgentsResult, ClientError> {
        self.call(crate::RUNTIMES_AGENTS_LIST_METHOD, &params).await
    }

    /// Adds or replaces one of a runtime's ACP agent declarations.
    pub async fn set_runtime_agent(
        &self,
        params: crate::RuntimesAgentsSetParams,
    ) -> Result<crate::RuntimesAgentsResult, ClientError> {
        self.call(crate::RUNTIMES_AGENTS_SET_METHOD, &params).await
    }

    /// Removes one of a runtime's ACP agent declarations.
    pub async fn remove_runtime_agent(
        &self,
        params: crate::RuntimesAgentsRemoveParams,
    ) -> Result<crate::RuntimesAgentsResult, ClientError> {
        self.call(crate::RUNTIMES_AGENTS_REMOVE_METHOD, &params)
            .await
    }

    /// Renames one of a runtime's ACP agent declarations.
    pub async fn rename_runtime_agent(
        &self,
        params: crate::RuntimesAgentsRenameParams,
    ) -> Result<crate::RuntimesAgentsRenameResult, ClientError> {
        self.call(crate::RUNTIMES_AGENTS_RENAME_METHOD, &params)
            .await
    }

    /// Every machine of the daemon's machine registry.
    pub async fn list_machines(&self) -> Result<crate::MachinesListResult, ClientError> {
        self.call(crate::MACHINES_LIST_METHOD, &crate::MachinesListParams {})
            .await
    }

    /// Renames `machine` to `name`; its memory folder moves with it.
    pub async fn rename_machine(
        &self,
        machine: crate::MachineName,
        name: crate::MachineName,
    ) -> Result<crate::MachinesListResult, ClientError> {
        self.call(
            crate::MACHINES_RENAME_METHOD,
            &crate::MachinesRenameParams { machine, name },
        )
        .await
    }

    /// Pins `runtime` to `machine`, created when the name is new.
    pub async fn assign_machine(
        &self,
        runtime: crate::RuntimeId,
        machine: crate::MachineName,
    ) -> Result<crate::MachinesListResult, ClientError> {
        self.call(
            crate::MACHINES_ASSIGN_METHOD,
            &crate::MachinesAssignParams { runtime, machine },
        )
        .await
    }

    /// Removes `runtime`'s pin.
    pub async fn unassign_machine(
        &self,
        runtime: crate::RuntimeId,
    ) -> Result<crate::MachinesListResult, ClientError> {
        self.call(
            crate::MACHINES_UNASSIGN_METHOD,
            &crate::MachinesUnassignParams { runtime },
        )
        .await
    }

    /// Merges `from` into `into`, memories included.
    pub async fn merge_machines(
        &self,
        from: crate::MachineName,
        into: crate::MachineName,
    ) -> Result<crate::MachinesListResult, ClientError> {
        self.call(
            crate::MACHINES_MERGE_METHOD,
            &crate::MachinesMergeParams { from, into },
        )
        .await
    }

    /// Removes `machine`, which has no connected runtimes and no memories.
    pub async fn forget_machine(
        &self,
        machine: crate::MachineName,
    ) -> Result<crate::MachinesListResult, ClientError> {
        self.call(
            crate::MACHINES_FORGET_METHOD,
            &crate::MachinesForgetParams { machine },
        )
        .await
    }

    /// Drops the record of `runtime`, which is not connected.
    pub async fn forget_machine_runtime(
        &self,
        runtime: crate::RuntimeId,
    ) -> Result<crate::MachinesListResult, ClientError> {
        self.call(
            crate::MACHINES_FORGET_RUNTIME_METHOD,
            &crate::MachinesForgetRuntimeParams { runtime },
        )
        .await
    }

    /// The memory files `params` asks for, valid or not.
    pub async fn list_memory(
        &self,
        params: crate::MemoryListParams,
    ) -> Result<crate::MemoryListResult, ClientError> {
        self.call(crate::MEMORY_LIST_METHOD, &params).await
    }

    /// One memory's whole text and revision.
    pub async fn read_memory(
        &self,
        url: impl Into<String>,
    ) -> Result<crate::MemoryReadResult, ClientError> {
        self.call(
            crate::MEMORY_READ_METHOD,
            &crate::MemoryReadParams { url: url.into() },
        )
        .await
    }

    /// Creates the memory `url` from `text` (`revision` absent), or replaces
    /// it at `revision`.
    pub async fn write_memory(
        &self,
        url: impl Into<String>,
        text: impl Into<String>,
        revision: Option<String>,
    ) -> Result<crate::MemoryWriteResult, ClientError> {
        self.call(
            crate::MEMORY_WRITE_METHOD,
            &crate::MemoryWriteParams {
                url: url.into(),
                text: text.into(),
                revision,
            },
        )
        .await
    }

    /// Deletes the memory `url` at `revision`.
    pub async fn delete_memory(
        &self,
        url: impl Into<String>,
        revision: impl Into<String>,
    ) -> Result<crate::MemoryDeleteResult, ClientError> {
        self.call(
            crate::MEMORY_DELETE_METHOD,
            &crate::MemoryDeleteParams {
                url: url.into(),
                revision: revision.into(),
            },
        )
        .await
    }

    /// Every project, in name order.
    pub async fn list_projects(&self) -> Result<crate::ProjectsListResult, ClientError> {
        self.call(crate::PROJECTS_LIST_METHOD, &crate::ProjectsListParams {})
            .await
    }

    /// One project with its `AGENTS.md`.
    pub async fn get_project(
        &self,
        project: crate::ProjectId,
    ) -> Result<crate::ProjectGetResult, ClientError> {
        self.call(
            crate::PROJECT_GET_METHOD,
            &crate::ProjectGetParams { project },
        )
        .await
    }

    /// Creates a project.
    pub async fn create_project(
        &self,
        params: crate::ProjectCreateParams,
    ) -> Result<crate::ProjectResult, ClientError> {
        self.call(crate::PROJECT_CREATE_METHOD, &params).await
    }

    /// Changes a project at the revision last read.
    pub async fn update_project(
        &self,
        params: crate::ProjectUpdateParams,
    ) -> Result<crate::ProjectResult, ClientError> {
        self.call(crate::PROJECT_UPDATE_METHOD, &params).await
    }

    /// Deletes a project at `revision`, with its sessions and memories.
    pub async fn delete_project(
        &self,
        project: crate::ProjectId,
        revision: impl Into<String>,
    ) -> Result<crate::ProjectDeleteResult, ClientError> {
        self.call(
            crate::PROJECT_DELETE_METHOD,
            &crate::ProjectDeleteParams {
                project,
                revision: revision.into(),
            },
        )
        .await
    }

    /// Moves the root session `session_id` into `project`, or out of any
    /// project with `None`.
    pub async fn set_session_project(
        &self,
        session_id: SessionId,
        project: Option<crate::ProjectId>,
    ) -> Result<crate::SessionProjectSetResult, ClientError> {
        self.call(
            crate::SESSION_PROJECT_SET_METHOD,
            &crate::SessionProjectSetParams {
                session_id,
                project,
            },
        )
        .await
    }

    /// Sets option `option_id` of the ACP agent of session `session_id` to
    /// `value`; returns the agent's options after the change.
    pub async fn set_session_agent_option(
        &self,
        session_id: SessionId,
        option_id: String,
        value: String,
    ) -> Result<crate::SessionAgentOptionSetResult, ClientError> {
        self.call(
            crate::SESSION_AGENT_OPTION_SET_METHOD,
            &crate::SessionAgentOptionSetParams {
                session_id,
                option_id,
                value,
            },
        )
        .await
    }

    /// Every cron job with its state and history.
    pub async fn list_crons(&self) -> Result<crate::CronsListResult, ClientError> {
        self.call(crate::CRONS_LIST_METHOD, &crate::CronsListParams {})
            .await
    }

    /// Creates a cron job.
    pub async fn create_cron(
        &self,
        params: crate::CronCreateParams,
    ) -> Result<crate::CronJobResult, ClientError> {
        self.call(crate::CRONS_CREATE_METHOD, &params).await
    }

    /// Changes a cron job.
    pub async fn update_cron(
        &self,
        params: crate::CronUpdateParams,
    ) -> Result<crate::CronJobResult, ClientError> {
        self.call(crate::CRONS_UPDATE_METHOD, &params).await
    }

    /// Deletes a cron job; its run sessions stay.
    pub async fn delete_cron(
        &self,
        id: crate::CronJobId,
    ) -> Result<crate::CronDeleteResult, ClientError> {
        self.call(crate::CRONS_DELETE_METHOD, &crate::CronDeleteParams { id })
            .await
    }

    /// Fires a cron job now.
    pub async fn run_cron(
        &self,
        id: crate::CronJobId,
    ) -> Result<crate::CronRunResult, ClientError> {
        self.call(crate::CRONS_RUN_METHOD, &crate::CronRunParams { id })
            .await
    }

    /// The daemon's push key and the browsers it pushes to.
    pub async fn get_notifications(&self) -> Result<crate::NotificationsGetResult, ClientError> {
        self.call(
            crate::NOTIFICATIONS_GET_METHOD,
            &crate::NotificationsGetParams {},
        )
        .await
    }

    /// Registers a browser's push subscription.
    pub async fn subscribe_notifications(
        &self,
        params: crate::NotificationsSubscribeParams,
    ) -> Result<crate::NotificationsSubscribeResult, ClientError> {
        self.call(crate::NOTIFICATIONS_SUBSCRIBE_METHOD, &params)
            .await
    }

    /// Removes a push subscription.
    pub async fn unsubscribe_notifications(
        &self,
        id: String,
    ) -> Result<crate::NotificationsUnsubscribeResult, ClientError> {
        self.call(
            crate::NOTIFICATIONS_UNSUBSCRIBE_METHOD,
            &crate::NotificationsUnsubscribeParams { id },
        )
        .await
    }

    /// Sends a test push to one subscription.
    pub async fn test_notifications(
        &self,
        id: String,
    ) -> Result<crate::NotificationsTestResult, ClientError> {
        self.call(
            crate::NOTIFICATIONS_TEST_METHOD,
            &crate::NotificationsTestParams { id },
        )
        .await
    }

    /// Tells the daemon what this connection shows the user: a session the
    /// user looks at, or `None` when the user is not there.
    pub async fn set_presence(
        &self,
        session: Option<crate::SessionId>,
    ) -> Result<crate::ClientPresenceResult, ClientError> {
        self.call(
            crate::CLIENT_PRESENCE_METHOD,
            &crate::ClientPresenceParams { session },
        )
        .await
    }

    /// Marks the root sessions `session_ids` unread or read (client
    /// protocol 1.51.0).
    pub async fn set_session_unread(
        &self,
        session_ids: Vec<crate::SessionId>,
        unread: bool,
    ) -> Result<crate::SessionUnreadSetResult, ClientError> {
        self.call(
            crate::SESSION_UNREAD_SET_METHOD,
            &crate::SessionUnreadSetParams {
                session_ids,
                unread,
            },
        )
        .await
    }

    /// The configured plugins, in the order their hooks run.
    pub async fn list_plugins(&self) -> Result<crate::PluginsListResult, ClientError> {
        self.call(crate::PLUGINS_LIST_METHOD, &crate::PluginsListParams {})
            .await
    }

    /// Starts plugin `name` again now.
    pub async fn restart_plugin(
        &self,
        name: String,
    ) -> Result<crate::PluginsRestartResult, ClientError> {
        self.call(
            crate::PLUGINS_RESTART_METHOD,
            &crate::PluginsRestartParams { name },
        )
        .await
    }

    /// The daemon's configuration documents on disk.
    pub async fn get_config(&self) -> Result<crate::ConfigGetResult, ClientError> {
        self.call(crate::CONFIG_GET_METHOD, &crate::ConfigGetParams {})
            .await
    }

    /// Changes the daemon's configuration; see [`crate::ConfigApplyParams`].
    pub async fn apply_config(
        &self,
        params: crate::ConfigApplyParams,
    ) -> Result<crate::ConfigApplyResult, ClientError> {
        self.call(crate::CONFIG_APPLY_METHOD, &params).await
    }

    /// Re-reads the daemon's configuration from disk and applies it.
    pub async fn reload_config(&self) -> Result<crate::ConfigReloadResult, ClientError> {
        self.call(crate::CONFIG_RELOAD_METHOD, &crate::ConfigReloadParams {})
            .await
    }

    /// Asks the daemon to restart `when` it says; see
    /// [`crate::DaemonRestartParams`].
    pub async fn restart_daemon(
        &self,
        when: crate::RestartWhen,
    ) -> Result<crate::DaemonRestartResult, ClientError> {
        self.call(
            crate::DAEMON_RESTART_METHOD,
            &crate::DaemonRestartParams { when },
        )
        .await
    }

    /// Whether the daemon is set up.
    pub async fn get_onboarding(&self) -> Result<crate::OnboardingStatus, ClientError> {
        self.call(crate::ONBOARDING_GET_METHOD, &crate::OnboardingGetParams {})
            .await
    }

    /// What installing the starter kit from `source` would do; writes
    /// nothing.
    pub async fn preview_onboarding(
        &self,
        source: Option<crate::OnboardingKitSource>,
    ) -> Result<crate::OnboardingPreviewResult, ClientError> {
        self.call(
            crate::ONBOARDING_PREVIEW_METHOD,
            &crate::OnboardingPreviewParams { source },
        )
        .await
    }

    /// Sets the models and installs the starter kit; see
    /// [`crate::OnboardingRunParams`].
    pub async fn run_onboarding(
        &self,
        params: crate::OnboardingRunParams,
    ) -> Result<crate::OnboardingRunResult, ClientError> {
        self.call(crate::ONBOARDING_RUN_METHOD, &params).await
    }

    pub async fn list_mcp_servers(&self) -> Result<McpServerListResult, ClientError> {
        self.call("mcp.server.list", &McpServerListParams {}).await
    }

    pub async fn add_mcp_server(
        &self,
        params: McpServerAddParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.call("mcp.server.add", &params).await
    }

    pub async fn edit_mcp_server(
        &self,
        params: McpServerEditParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.call("mcp.server.edit", &params).await
    }

    pub async fn remove_mcp_server(
        &self,
        params: McpServerNameParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.call("mcp.server.remove", &params).await
    }

    /// Renames a server (client protocol 1.37.0).
    pub async fn rename_mcp_server(
        &self,
        params: crate::McpServerRenameParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.call(crate::MCP_SERVER_RENAME_METHOD, &params).await
    }

    pub async fn set_mcp_server_enabled(
        &self,
        params: McpServerSetEnabledParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.call("mcp.server.set_enabled", &params).await
    }

    pub async fn reconnect_mcp_server(
        &self,
        params: McpServerReconnectParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.call("mcp.server.reconnect", &params).await
    }

    pub async fn persist_mcp_server(
        &self,
        params: McpServerPersistParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.call("mcp.server.persist", &params).await
    }

    /// Start an initial cursor replay. Its contents are delivered only through
    /// [`Client::subscribe_deliveries`].
    pub async fn subscribe_events(
        &self,
        session_id: SessionId,
        cursor: Option<u64>,
    ) -> Result<(), ClientError> {
        let cursor = cursor.unwrap_or(0);
        let request = self
            .prepare_subscription(session_id, cursor, false, cursor == 0)
            .await?;
        // The replay runs to completion in its own task. A caller that stops
        // waiting must not strand it half-fetched: the session would stay
        // marked as fetching and refuse every later replay.
        let commands = self.commands.clone();
        let subscriptions = self.subscriptions.clone();
        let recovery = self.recovery.clone();
        rt::spawn(async move {
            let result = request_replay(&commands, request, cursor, None).await;
            if result.is_err() {
                abort_replay(&subscriptions, session_id).await;
                Self::schedule_recovery_queue(&recovery, cursor == 0, Some(session_id));
            }
            result
        })
        .await
        .map_err(|_| ClientError::Closed)?
    }

    /// End this connection's live tail for `session_id`
    /// (`events.unsubscribe`). Messages still in flight for it are dropped,
    /// and recovery leaves it alone until [`Self::subscribe_events`] is
    /// called for it again.
    pub async fn unsubscribe_events(
        &self,
        session_id: SessionId,
    ) -> Result<crate::EventsUnsubscribeResult, ClientError> {
        if let Some(subscription) = self.subscriptions.lock().await.get_mut(&session_id) {
            subscription.closed = true;
        }
        self.call(
            crate::EVENTS_UNSUBSCRIBE_METHOD,
            &crate::EventsUnsubscribeParams { session_id },
        )
        .await
    }

    /// Recover one session. `full_replay` is used when its local projection is
    /// no longer trustworthy.
    pub fn recover_session(&self, session_id: SessionId, full_replay: bool) {
        Self::schedule_recovery_queue(&self.recovery, full_replay, Some(session_id));
    }

    fn schedule_recovery_queue(
        recovery: &Arc<RecoveryQueue>,
        full_replay: bool,
        session_id: Option<SessionId>,
    ) {
        let mut state = recovery.state.lock().expect("recovery queue lock");
        if state.queued {
            state.follow_up = true;
            state.follow_up_full |= full_replay;
            if state.follow_up_session != session_id {
                state.follow_up_session = None;
            }
            return;
        }
        state.queued = true;
        state.follow_up_session = session_id;
        if recovery.sender.send((full_replay, session_id)).is_err() {
            state.queued = false;
        }
    }

    async fn prepare_subscription(
        &self,
        session_id: SessionId,
        cursor: u64,
        recovering: bool,
        rebuild: bool,
    ) -> Result<ReplayRequest, ClientError> {
        prepare_subscription(&self.subscriptions, session_id, cursor, recovering, rebuild).await
    }

    /// Calls `method`. A daemon that does not know the method fails the call
    /// with [`ClientError::DaemonTooOld`].
    pub async fn call<P, R>(&self, method: &str, params: &P) -> Result<R, ClientError>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        // Serialized here, so the request's future below is one type for
        // every method rather than one per parameter type: a client of many
        // methods (the browser's) would otherwise carry a copy of it each.
        let params = serde_json::to_value(params)?;
        let result = send_command(&self.commands, method, params, None, false)
            .await
            .map_err(|error| too_old(method, error))?;
        Ok(serde_json::from_str(result.get())?)
    }

    /// Send a secret-bearing request from wiping process-owned buffers.
    ///
    /// The owned params are dropped immediately after serialization. The
    /// serialized JSON is wiped when dispatch completes or is cancelled.
    pub async fn call_sensitive<P, R>(&self, method: &str, params: P) -> Result<R, ClientError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let value = SensitiveJson(serde_json::to_value(&params)?);
        drop(params);
        let result = send_sensitive_command(&self.commands, method, value)
            .await
            .map_err(|error| too_old(method, error))?;
        Ok(serde_json::from_str(result.get())?)
    }

    #[cfg(all(test, not(target_arch = "wasm32")))]
    fn pending_command_count(&self) -> usize {
        self.pending_command_count.load(Ordering::Relaxed)
    }
}

/// `-32601` for a method this client calls: the daemon is older than the
/// version that added it.
fn too_old(method: &str, error: ClientError) -> ClientError {
    match error {
        ClientError::Rpc(rpc) if rpc.code == crate::errors::METHOD_NOT_FOUND => {
            ClientError::DaemonTooOld {
                method: method.to_owned(),
            }
        }
        error => error,
    }
}

/// The outcome of an open result, or [`ClientError::UnknownOutcome`].
fn known_outcome<T: crate::OpenTagged>(
    method: &str,
    result: crate::Open<T>,
) -> Result<T, ClientError> {
    match result {
        crate::Open::Known(result) => Ok(result),
        crate::Open::Unknown { tag, .. } => Err(ClientError::UnknownOutcome {
            method: method.to_owned(),
            outcome: tag,
        }),
    }
}

async fn prepare_subscription(
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    session_id: SessionId,
    cursor: u64,
    recovering: bool,
    rebuild: bool,
) -> Result<ReplayRequest, ClientError> {
    let mut subscriptions = subscriptions.lock().await;
    let subscription = subscriptions.entry(session_id).or_default();
    subscription.closed = false;
    if subscription.fetching {
        if recovering {
            subscription.recovery_requested =
                Some(subscription.recovery_requested.unwrap_or(false) || rebuild);
        }
        return Err(ClientError::ReplayInProgress);
    }
    subscription.rollback_cursor = subscription.cursor;
    subscription.rollback_cursor_timestamp = subscription.cursor_timestamp;
    if cursor != subscription.cursor {
        subscription.cursor_timestamp = None;
    }
    subscription.cursor = cursor;
    subscription.fetching = true;
    subscription.rebuild = rebuild;
    if rebuild && recovering {
        subscription.generation += 1;
    }
    subscription.next_attempt += 1;
    subscription.active_attempt = subscription.next_attempt;
    Ok(ReplayRequest {
        session_id,
        generation: subscription.generation,
        rebuild,
        attempt: subscription.active_attempt,
    })
}

async fn abort_replay(
    subscriptions: &Mutex<HashMap<SessionId, Subscription>>,
    session_id: SessionId,
) {
    if let Some(subscription) = subscriptions.lock().await.get_mut(&session_id) {
        subscription.abort_replay();
    }
}

async fn request_replay(
    commands: &mpsc::Sender<Command>,
    replay: ReplayRequest,
    cursor: u64,
    cursor_timestamp: Option<Timestamp>,
) -> Result<(), ClientError> {
    request_replay_with_timeout(
        commands,
        replay,
        cursor,
        cursor_timestamp,
        REPLAY_RESPONSE_TIMEOUT,
    )
    .await
}

/// Fetch a replay page by page. The connection task holds the pages and
/// delivers the whole replay once the final page arrives; each page's
/// response names the cursor of the next one, or is `null` after the last.
/// Only the first page checks `cursor_timestamp`: later pages start from
/// events this same replay just received.
async fn request_replay_with_timeout(
    commands: &mpsc::Sender<Command>,
    replay: ReplayRequest,
    mut cursor: u64,
    mut cursor_timestamp: Option<Timestamp>,
    timeout: Duration,
) -> Result<(), ClientError> {
    loop {
        let params = EventsSubscribeParams {
            session_id: replay.session_id,
            cursor: Some(cursor),
            cursor_timestamp: cursor_timestamp.take(),
            limit: Some(REPLAY_PAGE_EVENTS),
        };
        let (response, receiver) = oneshot::channel();
        let (written, written_receiver) = oneshot::channel();
        let command = Command {
            id: NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            method: "events.subscribe".to_owned(),
            params: SerializedParams {
                value: serde_json::to_value(params)?,
                sensitive: false,
            },
            replay: Some(replay),
            written: Some(written),
            response,
        };
        rt::timeout(REPLAY_QUEUE_TIMEOUT, async {
            commands
                .send(command)
                .await
                .map_err(|_| ClientError::Closed)?;
            // A command dropped unwritten still resolves through `receiver`.
            let _ = written_receiver.await;
            Ok::<_, ClientError>(())
        })
        .await
        .map_err(|_| ClientError::ReplayTimedOut)??;
        let next = rt::timeout(timeout, receiver)
            .await
            .map_err(|_| ClientError::ReplayTimedOut)?
            .map_err(|_| ClientError::Closed)??;
        match serde_json::from_str::<Option<u64>>(next.get())? {
            Some(next) => cursor = next,
            None => return Ok(()),
        }
    }
}

async fn send_command(
    commands: &mpsc::Sender<Command>,
    method: &str,
    params: serde_json::Value,
    replay: Option<ReplayRequest>,
    sensitive: bool,
) -> CommandResult {
    let (response, receiver) = oneshot::channel();
    let id = NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let params = SerializedParams {
        value: params,
        sensitive,
    };
    commands
        .send(Command {
            id,
            method: method.to_owned(),
            params,
            replay,
            written: None,
            response,
        })
        .await
        .map_err(|_| ClientError::Closed)?;
    receiver.await.map_err(|_| ClientError::Closed)?
}

pub(in crate::session::client) async fn send_sensitive_command(
    commands: &mpsc::Sender<Command>,
    method: &str,
    mut params: SensitiveJson,
) -> CommandResult {
    let (response, receiver) = oneshot::channel();
    let id = NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let params = SerializedParams {
        value: params.take(),
        sensitive: true,
    };
    commands
        .send(Command {
            id,
            method: method.to_owned(),
            params,
            replay: None,
            written: None,
            response,
        })
        .await
        .map_err(|_| ClientError::Closed)?;
    receiver.await.map_err(|_| ClientError::Closed)?
}

fn zeroize_json(value: &mut Value) {
    match value {
        Value::String(value) => value.zeroize(),
        Value::Array(values) => values.iter_mut().for_each(zeroize_json),
        Value::Object(values) => values.values_mut().for_each(zeroize_json),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn spawn_recovery_worker(
    commands: mpsc::Sender<Command>,
    subscriptions: Arc<Mutex<HashMap<SessionId, Subscription>>>,
    mut receiver: mpsc::UnboundedReceiver<(bool, Option<SessionId>)>,
    recovery: Arc<RecoveryQueue>,
    controls: mpsc::UnboundedSender<ConnectionControl>,
    replay_timeout: Duration,
) {
    let weak_recovery = Arc::downgrade(&recovery);
    rt::spawn(async move {
        while let Some((mut full_replay, mut only_session)) = receiver.recv().await {
            loop {
                for attempt in 0..RECOVERY_ATTEMPTS {
                    if weak_recovery.upgrade().is_none() {
                        return;
                    }
                    match recover_all(
                        &commands,
                        &subscriptions,
                        full_replay,
                        only_session,
                        replay_timeout,
                    )
                    .await
                    {
                        Ok(()) => break,
                        Err(error) => {
                            if attempt + 1 == RECOVERY_ATTEMPTS {
                                let _ = controls.send(ConnectionControl::RecoveryFailed {
                                    session_id: only_session,
                                    error: error.to_string(),
                                });
                                break;
                            }
                            let delay = 50_u64.saturating_mul(1_u64 << attempt).min(5_000);
                            rt::sleep(Duration::from_millis(delay)).await;
                        }
                    }
                }
                let next = {
                    let Some(recovery) = weak_recovery.upgrade() else {
                        return;
                    };
                    let mut state = recovery.state.lock().expect("recovery queue lock");
                    if state.follow_up {
                        state.follow_up = false;
                        Some((
                            std::mem::take(&mut state.follow_up_full),
                            state.follow_up_session.take(),
                        ))
                    } else {
                        state.queued = false;
                        None
                    }
                };
                let Some(next) = next else { break };
                (full_replay, only_session) = next;
            }
        }
    });
}

async fn recover_all(
    commands: &mpsc::Sender<Command>,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    full_replay: bool,
    only_session: Option<SessionId>,
    replay_timeout: Duration,
) -> Result<(), ClientError> {
    let targets = {
        let mut subscriptions = subscriptions.lock().await;
        subscriptions
            .iter_mut()
            .filter_map(|(session_id, subscription)| {
                if only_session.is_some_and(|wanted| wanted != *session_id) || subscription.closed {
                    return None;
                }
                if subscription.fetching {
                    subscription.recovery_requested =
                        Some(subscription.recovery_requested.unwrap_or(false) || full_replay);
                    return None;
                }
                subscription.rollback_cursor = subscription.cursor;
                subscription.rollback_cursor_timestamp = subscription.cursor_timestamp;
                let rebuild = full_replay;
                let cursor = if rebuild { 0 } else { subscription.cursor };
                if rebuild {
                    subscription.cursor_timestamp = None;
                }
                subscription.cursor = cursor;
                subscription.fetching = true;
                subscription.rebuild = rebuild;
                if rebuild {
                    subscription.generation += 1;
                }
                subscription.next_attempt += 1;
                subscription.active_attempt = subscription.next_attempt;
                Some((
                    ReplayRequest {
                        session_id: *session_id,
                        generation: subscription.generation,
                        rebuild,
                        attempt: subscription.active_attempt,
                    },
                    cursor,
                    subscription.cursor_timestamp,
                ))
            })
            .collect::<Vec<_>>()
    };
    for (request, cursor, cursor_timestamp) in targets {
        let session_id = request.session_id;
        if let Err(error) =
            request_replay_with_timeout(commands, request, cursor, cursor_timestamp, replay_timeout)
                .await
        {
            let mut subscriptions = subscriptions.lock().await;
            if let Some(subscription) = subscriptions.get_mut(&session_id) {
                subscription.abort_replay();
            }
            return Err(error);
        }
    }
    Ok(())
}

static NEXT_REQUEST_ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

struct ConnectionTask {
    commands: mpsc::Receiver<Command>,
    controls: mpsc::UnboundedReceiver<ConnectionControl>,
    deliveries: Deliveries,
    subscriptions: Arc<Mutex<HashMap<SessionId, Subscription>>>,
    recovery: Arc<RecoveryQueue>,
    pending_command_count: Arc<AtomicUsize>,
    shutdown: tokio_util::sync::CancellationToken,
}

async fn connection_task<S>(mut stream: S, mut task: ConnectionTask)
where
    S: Transport,
{
    let mut pending = HashMap::new();
    let mut failure = None;
    // Drops calls whose callers stopped waiting; it only runs while calls are pending.
    let mut prune = rt::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            () = task.shutdown.cancelled() => break,
            Some(command) = task.commands.recv() => {
                prune_cancelled_commands(&mut pending);
                if !command.response.is_closed() {
                    let request = BorrowedRequest {
                        jsonrpc: "2.0",
                        id: command.id,
                        method: &command.method,
                        params: &command.params.value,
                    };
                    match serialize_outbound_frame(request, command.params.sensitive) {
                        Ok(frame) => match send_outbound_frame(&mut stream, frame).await {
                            Ok(()) => {
                                if let Some(written) = command.written {
                                    let _ = written.send(());
                                }
                                pending.insert(command.id, PendingCommand { replay: command.replay, response: command.response });
                            }
                            Err(error) => {
                                let message = format!("sending {}: {}", command.method, crate::diagnostics::error_chain(&error));
                                let _ = command.response.send(Err(ClientError::Transport(message.clone())));
                                failure = Some(message);
                                break;
                            }
                        },
                        Err(error) => { let _ = command.response.send(Err(ClientError::InvalidFrame(error))); }
                    }
                }
            }
            Some(control) = task.controls.recv() => match control {
                ConnectionControl::RecoveryFailed { session_id, error } => {
                    let _ = task.deliveries.send(ClientDelivery::RecoveryFailed { session_id, error });
                }
            },
            _ = prune.tick(), if !pending.is_empty() => {
                prune_cancelled_commands(&mut pending);
            }
            incoming = stream.recv() => {
                let frame = match incoming {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break,
                    Err(error) => {
                        failure = Some(format!("receiving response: {}", crate::diagnostics::error_chain(&error)));
                        break;
                    }
                };
                prune_cancelled_commands(&mut pending);
                if let Err(error) = handle_frame(
                    frame,
                    &mut pending,
                    &task.deliveries,
                    &task.subscriptions,
                    &task.recovery,
                ).await {
                    // A frame that is not JSON-RPC at all ends the
                    // connection; an unreadable notification never gets here.
                    failure = Some(format!(
                        "the daemon sent a frame that is not JSON-RPC: {}",
                        crate::diagnostics::error_chain(&error)
                    ));
                    break;
                }
            }
            else => break,
        }
        task.pending_command_count
            .store(pending.len(), Ordering::Relaxed);
    }
    for (_, pending) in pending {
        let error = failure.as_ref().map_or(ClientError::Closed, |message| {
            ClientError::Transport(message.clone())
        });
        let _ = pending.response.send(Err(error));
    }
    if let Some(error) = failure {
        let _ = task.deliveries.send(ClientDelivery::Disconnected { error });
    }
    task.pending_command_count.store(0, Ordering::Relaxed);
}

fn serialize_outbound_frame(
    request: BorrowedRequest<'_>,
    sensitive: bool,
) -> Result<OutboundFrame, serde_json::Error> {
    if sensitive {
        serde_json::to_string(&request)
            .map(SensitiveFrame::new)
            .map(OutboundFrame::Sensitive)
    } else {
        serde_json::to_string(&request)
            .map(MessageFrame::Text)
            .map(OutboundFrame::Public)
    }
}

async fn send_outbound_frame<S>(stream: &mut S, frame: OutboundFrame) -> Result<(), TransportError>
where
    S: Transport,
{
    match frame {
        OutboundFrame::Public(frame) => stream.send(frame).await,
        OutboundFrame::Sensitive(mut frame) => {
            // Move the sole serialized allocation into the transport. Dropping
            // before this point wipes it; after this point the transport owns
            // any frame/socket copies and defines their lifetime.
            let text = std::mem::take(&mut *frame.text);
            stream.send(MessageFrame::Text(text)).await
        }
    }
}

#[cfg(test)]
static PROVIDER_CONNECT_WIPE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
static SENSITIVE_SERIALIZED_WIPE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
static SENSITIVE_FRAME_WIPE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
fn record_provider_connect_wipe() {
    PROVIDER_CONNECT_WIPE_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(test))]
fn record_provider_connect_wipe() {}

#[cfg(test)]
fn record_sensitive_serialized_wipe() {
    SENSITIVE_SERIALIZED_WIPE_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(test))]
fn record_sensitive_serialized_wipe() {}

#[cfg(test)]
fn record_sensitive_frame_wipe() {
    SENSITIVE_FRAME_WIPE_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(test))]
fn record_sensitive_frame_wipe() {}

fn prune_cancelled_commands(pending: &mut HashMap<i64, PendingCommand>) {
    pending.retain(|_, command| !command.response.is_closed());
}

/// A JSON-RPC message from the server. Its result or params stay
/// unparsed text until the receiver decodes them into the type it expects.
/// The envelope is information: members it does not name are ignored.
#[derive(Deserialize)]
struct IncomingMessage<'a> {
    /// Checked while parsing; nothing reads it after.
    #[serde(rename = "jsonrpc")]
    _jsonrpc: JsonRpcVersion,
    #[serde(default, deserialize_with = "present")]
    id: Option<JsonRpcId>,
    #[serde(default, borrow)]
    method: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "present")]
    params: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "present")]
    result: Option<&'a RawValue>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

/// A field that is present, even as `null`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Just the id of a message that failed to parse, to fail the call it answers.
#[derive(Deserialize)]
struct IncomingId {
    #[serde(default)]
    id: Option<JsonRpcId>,
}

async fn handle_frame(
    frame: MessageFrame,
    pending: &mut HashMap<i64, PendingCommand>,
    deliveries: &Deliveries,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    recovery: &Arc<RecoveryQueue>,
) -> Result<(), ClientError> {
    let serialized;
    let bytes: &[u8] = match &frame {
        MessageFrame::Text(text) => text.as_bytes(),
        MessageFrame::Bytes(bytes) => bytes,
        MessageFrame::Value(value) => {
            serialized = serde_json::to_vec(value)?;
            &serialized
        }
    };
    let message =
        match serde_json::from_slice::<IncomingMessage<'_>>(bytes).and_then(validate_incoming) {
            Ok(message) => message,
            Err(error) => {
                if let Ok(IncomingId {
                    id: Some(JsonRpcId::Number(id)),
                }) = serde_json::from_slice(bytes)
                    && let Some(command) = pending.remove(&id)
                {
                    let _ = command.response.send(Err(ClientError::InvalidFrame(error)));
                    return Ok(());
                }
                return Err(ClientError::InvalidFrame(error));
            }
        };
    if let Some(id) = message.id {
        let result = match (message.result, message.error) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) => Err(ClientError::Rpc(error)),
            _ => unreachable!("validated as a response"),
        };
        if let JsonRpcId::Number(id) = id
            && let Some(command) = pending.remove(&id)
        {
            match (command.replay, result) {
                (Some(replay), Ok(result)) => {
                    match serde_json::from_str::<ReplayPageWire>(result.get()) {
                        Ok(result) if result.stale_cursor => {
                            // A revert replaced the event at the cursor while
                            // this view was away: what it holds past the cut
                            // is wrong, so it starts over from scratch. The
                            // replay ends before its caller hears back, so a
                            // recovery worker waiting on it finds the session
                            // free for the full replay queued here.
                            abort_stale_replay(replay, subscriptions).await;
                            Client::schedule_recovery_queue(
                                recovery,
                                true,
                                Some(replay.session_id),
                            );
                            let _ = command.response.send(Ok(replay_next(None)));
                        }
                        Ok(result) => {
                            let has_more = result.has_more;
                            let page = result.read(replay.session_id, deliveries);
                            let page = if has_more {
                                match hold_replay_page(replay, page, subscriptions).await {
                                    Ok(next) => {
                                        let _ = command.response.send(Ok(replay_next(Some(next))));
                                        return Ok(());
                                    }
                                    Err(page) => page,
                                }
                            } else {
                                page.events
                            };
                            let mut events = take_replay_pages(replay, subscriptions).await;
                            events.extend(page);
                            // The application can now start draining the sole
                            // mpsc consumer while this task injects a large
                            // replay under natural channel backpressure.
                            let _ = command.response.send(Ok(replay_next(None)));
                            begin_replay(replay, events, subscriptions, deliveries, recovery).await;
                        }
                        Err(error) => {
                            let _ = command.response.send(Err(ClientError::InvalidFrame(error)));
                        }
                    }
                }
                (_, result) => {
                    let _ = command.response.send(result.map(RawValue::to_owned));
                }
            }
        }
        return Ok(());
    }
    let Some(method) = message.method else {
        return Ok(());
    };
    // A notification that does not decode is dropped and reported: no call
    // fails for it, and the next one is read as usual.
    if let Err(error) =
        route_notification(&method, message.params, deliveries, subscriptions, recovery).await
    {
        let _ = deliveries.send(ClientDelivery::Diagnostic {
            message: format!("dropped a `{method}` notification this client cannot read: {error}"),
        });
    }
    Ok(())
}

/// Delivers one notification. A method this build does not know, and a
/// message of an open type with a tag it does not know, are ignored.
async fn route_notification(
    method: &str,
    params: Option<&RawValue>,
    deliveries: &Deliveries,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    recovery: &Arc<RecoveryQueue>,
) -> Result<(), serde_json::Error> {
    let params = params.map_or("null", RawValue::get);
    match method {
        "events.subscription" => {
            if let crate::Open::Known(message) =
                serde_json::from_str::<crate::Open<ClientSubscriptionMessage>>(params)?
            {
                route_live(message, deliveries, subscriptions, recovery).await;
            }
        }
        "events.plugin" => {
            let event = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::PluginEvent(event));
        }
        crate::RUNTIME_CHANGED_METHOD => {
            let changed = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::RuntimeChanged(Box::new(changed)));
        }
        crate::SESSIONS_CHANGED_METHOD => {
            // A change this build does not know may have changed anything
            // the list shows, so it is read as `resync`.
            let changed =
                serde_json::from_str::<crate::Open<crate::SessionsChangedNotification>>(params)?
                    .into_known()
                    .unwrap_or(crate::SessionsChangedNotification::Resync);
            let _ = deliveries.send(ClientDelivery::SessionsChanged(Box::new(changed)));
        }
        crate::MCP_CHANGED_METHOD => {
            let changed: crate::McpChangedNotification = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::McpChanged(changed));
        }
        crate::RUNTIMES_CHANGED_METHOD => {
            let changed: crate::RuntimesChangedNotification = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::RuntimesChanged(changed));
        }
        crate::BACKGROUND_CHANGED_METHOD => {
            let changed: crate::BackgroundChangedNotification = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::BackgroundChanged(changed));
        }
        crate::PLUGINS_CHANGED_METHOD => {
            let changed: crate::PluginsChangedNotification = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::PluginsChanged(changed));
        }
        crate::MACHINES_CHANGED_METHOD => {
            let changed: crate::MachinesChangedNotification = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::MachinesChanged(changed));
        }
        crate::MEMORY_CHANGED_METHOD => {
            let changed: crate::MemoryChangedNotification = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::MemoryChanged(changed));
        }
        crate::PROJECTS_CHANGED_METHOD => {
            let changed: crate::ProjectsChangedNotification = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::ProjectsChanged(changed));
        }
        crate::CRONS_CHANGED_METHOD => {
            let changed: crate::CronsChangedNotification = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::CronsChanged(changed));
        }
        crate::NOTIFICATIONS_CHANGED_METHOD => {
            let changed: crate::NotificationsChangedNotification = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::NotificationsChanged(changed));
        }
        crate::DAEMON_RESTARTING_METHOD => {
            let restarting: crate::DaemonRestartingNotification = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::DaemonRestarting(restarting));
        }
        crate::ONBOARDING_CHANGED_METHOD => {
            let changed: crate::OnboardingStatus = serde_json::from_str(params)?;
            let _ = deliveries.send(ClientDelivery::OnboardingChanged(changed));
        }
        _ => {}
    }
    Ok(())
}

/// An `events.subscribe` page as a client reads it. Each event is read on its
/// own, so one this build cannot read fails neither the page nor the replay.
#[derive(Deserialize)]
struct ReplayPageWire {
    events: Vec<Box<RawValue>>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    stale_cursor: bool,
}

/// A page's events and the sequence of its last record, readable or not:
/// the next page starts after it.
struct ReplayPage {
    events: Vec<ClientEvent>,
    last_seq: Option<u64>,
}

impl ReplayPageWire {
    /// The page's events. An event whose type this build does not know, or
    /// whose payload it cannot read, is a [`ClientEvent`] with an unknown
    /// payload; one whose envelope it cannot read is dropped with a
    /// [`ClientDelivery::Diagnostic`].
    fn read(self, session_id: SessionId, deliveries: &Deliveries) -> ReplayPage {
        let count = self.events.len();
        let mut last_seq = None;
        let events = self
            .events
            .into_iter()
            .enumerate()
            .filter_map(|(index, event)| {
                match serde_json::from_str::<ClientEvent>(event.get()) {
                    Ok(event) => {
                        if index + 1 == count {
                            last_seq = Some(event.seq);
                        }
                        Some(event)
                    }
                    Err(error) => {
                        let seq = event_seq(&event);
                        if index + 1 == count {
                            last_seq = seq;
                        }
                        let _ = deliveries.send(ClientDelivery::Diagnostic {
                            message: format!(
                                "dropped an event of session {session_id} (seq {}) this client cannot read: {error}",
                                seq.map_or_else(|| "null".to_owned(), |seq| seq.to_string())
                            ),
                        });
                        None
                    }
                }
            })
            .collect();
        ReplayPage { events, last_seq }
    }
}

/// The sequence of an event this client could not read, when its envelope
/// has one.
fn event_seq(event: &RawValue) -> Option<u64> {
    #[derive(Deserialize)]
    struct Seq {
        seq: Option<u64>,
    }
    serde_json::from_str::<Seq>(event.get())
        .ok()
        .and_then(|event| event.seq)
}

/// Checks that a message is exactly a response (an id with one of a result
/// or an error) or a notification (a method, maybe params, and no id).
fn validate_incoming(
    message: IncomingMessage<'_>,
) -> Result<IncomingMessage<'_>, serde_json::Error> {
    let valid = if message.id.is_some() {
        message.method.is_none()
            && message.params.is_none()
            && (message.result.is_some() != message.error.is_some())
    } else {
        message.method.is_some() && message.result.is_none() && message.error.is_none()
    };
    if valid {
        Ok(message)
    } else {
        Err(serde::de::Error::custom(
            "message is neither a JSON-RPC response nor a notification",
        ))
    }
}

/// What a replay page tells the replay loop: the cursor of the next page,
/// or `null` once the replay has ended.
fn replay_next(next: Option<u64>) -> Box<RawValue> {
    serde_json::value::to_raw_value(&next).expect("a cursor serializes")
}

/// Whether `replay` is still the attempt its session is fetching.
fn replay_is_current(subscription: &Subscription, replay: &ReplayRequest) -> bool {
    subscription.fetching
        && subscription.generation == replay.generation
        && subscription.active_attempt == replay.attempt
}

/// Hold one non-final page of the current attempt and return the cursor the
/// next page starts from. A superseded attempt (or an empty page) gets its
/// events back, and the caller ends it as if this were the final page.
async fn hold_replay_page(
    replay: ReplayRequest,
    page: ReplayPage,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
) -> Result<u64, Vec<ClientEvent>> {
    let ReplayPage { events, last_seq } = page;
    let Some(next) = last_seq else {
        return Err(events);
    };
    let mut subscriptions = subscriptions.lock().await;
    let Some(subscription) = subscriptions
        .get_mut(&replay.session_id)
        .filter(|subscription| replay_is_current(subscription, &replay))
    else {
        return Err(events);
    };
    if subscription.replay_pages_attempt != replay.attempt {
        subscription.replay_pages = Vec::new();
        subscription.replay_pages_attempt = replay.attempt;
    }
    subscription.replay_pages.extend(events);
    Ok(next)
}

async fn take_replay_pages(
    replay: ReplayRequest,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
) -> Vec<ClientEvent> {
    let mut subscriptions = subscriptions.lock().await;
    match subscriptions.get_mut(&replay.session_id) {
        Some(subscription) if subscription.replay_pages_attempt == replay.attempt => {
            std::mem::take(&mut subscription.replay_pages)
        }
        _ => Vec::new(),
    }
}

/// Ends `replay` without delivering anything, if it is still the current
/// attempt.
async fn abort_stale_replay(
    replay: ReplayRequest,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
) {
    let mut subscriptions = subscriptions.lock().await;
    if let Some(subscription) = subscriptions
        .get_mut(&replay.session_id)
        .filter(|subscription| replay_is_current(subscription, &replay))
    {
        subscription.abort_replay();
    }
}

async fn begin_replay(
    replay: ReplayRequest,
    events: Vec<ClientEvent>,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    deliveries: &Deliveries,
    recovery: &Arc<RecoveryQueue>,
) {
    let final_seq = {
        let mut subscriptions = subscriptions.lock().await;
        let Some(subscription) = subscriptions.get_mut(&replay.session_id) else {
            return;
        };
        if !replay_is_current(subscription, &replay) {
            return;
        }
        let final_seq = events
            .iter()
            .map(|event| event.seq)
            .max()
            .unwrap_or(subscription.cursor);
        subscription.final_timestamp = match events.last() {
            Some(last) => Some(last.timestamp),
            None => subscription.cursor_timestamp,
        };
        subscription.final_seq = final_seq;
        subscription.rebuild = replay.rebuild;
        final_seq
    };
    let _ = deliveries.send(ClientDelivery::ReplayStart {
        session_id: replay.session_id,
        generation: replay.generation,
        final_seq,
        rebuild: replay.rebuild,
    });
    for event in events {
        let _ = deliveries.send(match event.into_known() {
            Ok(event) => ClientDelivery::ReplayEvent {
                session_id: replay.session_id,
                generation: replay.generation,
                final_seq,
                event: Box::new(event),
            },
            Err(event) => ClientDelivery::UnknownEvent {
                event: Box::new(event),
                generation: replay.generation,
                replay_final_seq: Some(final_seq),
            },
        });
    }
    finish_ready_replay(replay.session_id, subscriptions, deliveries, recovery).await;
}

/// Delivers one live message. A durable event this build could not read is
/// delivered as [`ClientDelivery::UnknownEvent`]; live-only output it could
/// not read is dropped.
fn deliver_live(deliveries: &Deliveries, message: ClientSubscriptionMessage, generation: u64) {
    let message = match message {
        ClientSubscriptionMessage::Event { event } => {
            match std::sync::Arc::unwrap_or_clone(event).into_known() {
                Ok(event) => crate::EventSubscriptionMessage::Event {
                    event: std::sync::Arc::new(event),
                },
                Err(event) => {
                    let _ = deliveries.send(ClientDelivery::UnknownEvent {
                        event: Box::new(event),
                        generation,
                        replay_final_seq: None,
                    });
                    return;
                }
            }
        }
        ClientSubscriptionMessage::Transient { event } => {
            let event = std::sync::Arc::unwrap_or_clone(event);
            let crate::Open::Known(payload) = event.payload else {
                return;
            };
            crate::EventSubscriptionMessage::Transient {
                event: std::sync::Arc::new(crate::TransientEvent {
                    session_id: event.session_id,
                    run_id: event.run_id,
                    after_seq: event.after_seq,
                    timestamp: event.timestamp,
                    payload,
                }),
            }
        }
        ClientSubscriptionMessage::Gap {
            session_id,
            last_delivered_seq,
        } => crate::EventSubscriptionMessage::Gap {
            session_id,
            last_delivered_seq,
        },
        ClientSubscriptionMessage::Rewound {
            session_id,
            through_seq,
        } => crate::EventSubscriptionMessage::Rewound {
            session_id,
            through_seq,
        },
    };
    let _ = deliveries.send(ClientDelivery::Live {
        message: Box::new(message),
        generation,
    });
}

async fn route_live(
    message: ClientSubscriptionMessage,
    deliveries: &Deliveries,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    recovery: &Arc<RecoveryQueue>,
) {
    let session_id = match &message {
        ClientSubscriptionMessage::Event { event } => event.session_id,
        ClientSubscriptionMessage::Transient { event } => event.session_id,
        ClientSubscriptionMessage::Gap { session_id, .. }
        | ClientSubscriptionMessage::Rewound { session_id, .. } => *session_id,
    };
    let publish;
    let mut recover = None;
    {
        let mut subscriptions = subscriptions.lock().await;
        let subscription = subscriptions.entry(session_id).or_default();
        if subscription.closed {
            return;
        }
        let generation = subscription.generation;
        if subscription.fetching {
            match message {
                ClientSubscriptionMessage::Gap { .. } => {
                    subscription.recovery_requested =
                        Some(subscription.recovery_requested.unwrap_or(false));
                }
                // The replay in flight may already hold removed events.
                ClientSubscriptionMessage::Rewound { .. } => {
                    subscription.recovery_requested = Some(true);
                }
                _ => {}
            }
            subscription.buffered.push((message, generation));
            return;
        }
        match &message {
            ClientSubscriptionMessage::Event { event } if event.seq > subscription.cursor + 1 => {
                recover = Some((false, Some(session_id)));
            }
            ClientSubscriptionMessage::Event { event } if event.seq == subscription.cursor + 1 => {
                subscription.cursor = event.seq;
                subscription.cursor_timestamp = Some(event.timestamp);
            }
            ClientSubscriptionMessage::Gap {
                last_delivered_seq, ..
            } => {
                if *last_delivered_seq != subscription.cursor {
                    subscription.cursor_timestamp = None;
                }
                subscription.cursor = *last_delivered_seq;
                recover = Some((false, Some(session_id)));
            }
            // Later appends reuse the removed sequence numbers, so they
            // follow on from the cut while a full replay rebuilds the view.
            ClientSubscriptionMessage::Rewound { through_seq, .. } => {
                if *through_seq < subscription.cursor {
                    subscription.cursor = *through_seq;
                    subscription.cursor_timestamp = None;
                }
                recover = Some((true, Some(session_id)));
            }
            // Live-only output belongs right after the durable event it
            // follows. Anywhere else it is stale or outruns a durable event
            // this view has not seen yet, and nothing can replay it.
            ClientSubscriptionMessage::Transient { event }
                if event.after_seq != subscription.cursor =>
            {
                return;
            }
            _ => {}
        }
        publish = (message, generation);
    }
    deliver_live(deliveries, publish.0, publish.1);
    if let Some((full, session)) = recover {
        Client::schedule_recovery_queue(recovery, full, session);
    }
}

async fn finish_ready_replay(
    session_id: SessionId,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    deliveries: &Deliveries,
    recovery: &Arc<RecoveryQueue>,
) {
    let (generation, final_seq, buffered, recovery_requested, tail_has_gap) = {
        let mut subscriptions = subscriptions.lock().await;
        let Some(subscription) = subscriptions.get_mut(&session_id) else {
            return;
        };
        if !subscription.fetching {
            return;
        }
        subscription.fetching = false;
        // Durable events the replay already covered are dropped, and so is
        // live-only output that does not directly follow the durable event
        // it was published after.
        let mut cursor = subscription.final_seq;
        let mut cursor_timestamp = subscription.final_timestamp;
        let mut tail_has_gap = false;
        let mut buffered = std::mem::take(&mut subscription.buffered);
        buffered.retain(|(message, _)| match message {
            ClientSubscriptionMessage::Event { event } if event.seq <= cursor => false,
            ClientSubscriptionMessage::Event { event } if event.seq == cursor + 1 => {
                cursor = event.seq;
                cursor_timestamp = Some(event.timestamp);
                true
            }
            ClientSubscriptionMessage::Transient { event } => {
                !tail_has_gap && event.after_seq == cursor
            }
            ClientSubscriptionMessage::Event { .. }
            | ClientSubscriptionMessage::Gap { .. }
            | ClientSubscriptionMessage::Rewound { .. } => {
                tail_has_gap = true;
                true
            }
        });
        subscription.cursor = cursor;
        subscription.cursor_timestamp = cursor_timestamp;
        (
            subscription.generation,
            subscription.final_seq,
            buffered,
            subscription.recovery_requested.take(),
            tail_has_gap,
        )
    };
    let _ = deliveries.send(ClientDelivery::ReplayEnd {
        session_id,
        generation,
        final_seq,
    });
    for (message, generation) in buffered {
        deliver_live(deliveries, message, generation);
    }
    if let Some(full) = recovery_requested.or(tail_has_gap.then_some(false)) {
        Client::schedule_recovery_queue(recovery, full, (!full).then_some(session_id));
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
