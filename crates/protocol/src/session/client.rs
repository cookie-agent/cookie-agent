//! Shared protocol-9 JSON-RPC client session.

mod runtime;

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use crate::{
    AgentUsageParams, AgentUsageResult, ApprovalListParams, ApprovalListResult,
    ApprovalRespondParams, ApprovalRespondResult, ClientHello, EventPayload,
    EventSubscriptionMessage, EventsSubscribeParams, EventsSubscribeResult, GlobalUsageParams,
    GlobalUsageResult, JsonRpcError, JsonRpcId, McpAuthBeginParams, McpAuthBeginResult,
    McpAuthCancelParams, McpAuthCancelResult, McpServerAddParams, McpServerEditParams,
    McpServerListParams, McpServerListResult, McpServerMutationResult, McpServerNameParams,
    McpServerPersistParams, McpServerSetEnabledParams, MessageFrame, Notification, OutputDelta,
    OutputGap, OutputSnapshotEnvelope, OutputStream, ProtocolVersion, ProviderConnectParams,
    ProviderConnectResult, ProviderDisconnectParams, ProviderDisconnectResult, Response,
    RunCancelParams, RunCancelResult, RunRecallSteerParams, RunRecallSteerResult, RunStartParams,
    RunStartResult, RunSteerParams, RunSteerResult, RunToolStdinParams, RunToolStdinResult,
    RuntimeSnapshotResult, ServerHello, SessionChildrenParams, SessionChildrenResult,
    SessionCompactParams, SessionCompactResult, SessionCreateParams, SessionCreateResult,
    SessionForkParams, SessionForkResult, SessionGetParams, SessionGetResult, SessionId,
    SessionListParams, SessionListResult, SessionPermissionClearParams, SessionPermissionGetParams,
    SessionPermissionGetResult, SessionPermissionMutationResult, SessionPermissionSetParams,
    SessionRenameParams, SessionRenameResult, SessionResumeParams, SessionResumeResult,
    SessionRevertParams, SessionRevertResult, SessionSetPermissionModeParams,
    SessionSetPermissionModeResult, SessionTreeParams, SessionTreeResult, SessionUsageParams,
    SessionUsageResult, SkillsGetParams, SkillsGetResult, SkillsListParams, SkillsListResult,
    StoredEvent, ToolCallId, Transport, TransportError,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::Instant;
use zeroize::{Zeroize, Zeroizing};

const COMMAND_QUEUE_CAPACITY: usize = 128;
const RECOVERY_ATTEMPTS: usize = 6;
const REPLAY_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The sole ordered delivery stream consumed by the UI.
///
/// The connection task is its only producer. Replays are injected into this
/// stream before the live notifications buffered while their RPC is in flight.
#[derive(Clone, Debug)]
pub enum ClientDelivery {
    Live {
        message: Box<EventSubscriptionMessage>,
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
        event: Box<StoredEvent>,
    },
    ReplayEnd {
        session_id: SessionId,
        generation: u64,
        final_seq: u64,
    },
    OutputSnapshot(OutputSnapshotEnvelope),
    OutputDelta(OutputDelta),
    OutputGap(OutputGap),
    RecoveryFailed {
        session_id: Option<SessionId>,
        error: String,
    },
    RuntimeChanged(Box<crate::RuntimeChangedNotification>),
}

/// Consumer of ordered protocol notifications and replay deliveries.
pub trait ClientEventSink: Send + Sync + 'static {
    fn deliver(&self, delivery: ClientDelivery);
}

/// Client end of the protocol contract over shared session mechanics.
#[async_trait::async_trait]
pub trait ClientProtocol: Send + Sync {
    async fn handshake(&self) -> Result<ServerHello, ClientError>;
    async fn create_session(
        &self,
        params: SessionCreateParams,
    ) -> Result<SessionCreateResult, ClientError>;
    async fn list_sessions(
        &self,
        params: SessionListParams,
    ) -> Result<SessionListResult, ClientError>;
    async fn get_session(&self, params: SessionGetParams) -> Result<SessionGetResult, ClientError>;
    async fn session_usage(
        &self,
        params: SessionUsageParams,
    ) -> Result<SessionUsageResult, ClientError>;
    async fn agent_usage(&self, params: AgentUsageParams) -> Result<AgentUsageResult, ClientError>;
    async fn global_usage(&self) -> Result<GlobalUsageResult, ClientError>;
    async fn session_children(
        &self,
        params: SessionChildrenParams,
    ) -> Result<SessionChildrenResult, ClientError>;
    async fn session_tree(
        &self,
        params: SessionTreeParams,
    ) -> Result<SessionTreeResult, ClientError>;
    async fn resume_session(
        &self,
        params: SessionResumeParams,
    ) -> Result<SessionResumeResult, ClientError>;
    async fn rename_session(
        &self,
        params: SessionRenameParams,
    ) -> Result<SessionRenameResult, ClientError>;
    async fn set_permission_mode(
        &self,
        params: SessionSetPermissionModeParams,
    ) -> Result<SessionSetPermissionModeResult, ClientError>;
    async fn get_session_permissions(
        &self,
        params: SessionPermissionGetParams,
    ) -> Result<SessionPermissionGetResult, ClientError>;
    async fn set_session_permission(
        &self,
        params: SessionPermissionSetParams,
    ) -> Result<SessionPermissionMutationResult, ClientError>;
    async fn clear_session_permission(
        &self,
        params: SessionPermissionClearParams,
    ) -> Result<SessionPermissionMutationResult, ClientError>;
    async fn list_skills(&self, params: SkillsListParams) -> Result<SkillsListResult, ClientError>;
    async fn get_skill(&self, params: SkillsGetParams) -> Result<SkillsGetResult, ClientError>;
    async fn compact_session(
        &self,
        params: SessionCompactParams,
    ) -> Result<SessionCompactResult, ClientError>;
    async fn revert_session(
        &self,
        params: SessionRevertParams,
    ) -> Result<SessionRevertResult, ClientError>;
    async fn fork_session(
        &self,
        params: SessionForkParams,
    ) -> Result<SessionForkResult, ClientError>;
    async fn start_run(&self, params: RunStartParams) -> Result<RunStartResult, ClientError>;
    async fn steer_run(&self, params: RunSteerParams) -> Result<RunSteerResult, ClientError>;
    async fn recall_steer(
        &self,
        params: RunRecallSteerParams,
    ) -> Result<RunRecallSteerResult, ClientError>;
    async fn cancel_run(&self, params: RunCancelParams) -> Result<RunCancelResult, ClientError>;
    async fn tool_stdin(
        &self,
        params: RunToolStdinParams,
    ) -> Result<RunToolStdinResult, ClientError>;
    async fn respond_approval(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<ApprovalRespondResult, ClientError>;
    async fn list_approvals(
        &self,
        params: ApprovalListParams,
    ) -> Result<ApprovalListResult, ClientError>;
    async fn begin_mcp_auth(
        &self,
        params: McpAuthBeginParams,
    ) -> Result<McpAuthBeginResult, ClientError>;
    async fn cancel_mcp_auth(
        &self,
        params: McpAuthCancelParams,
    ) -> Result<McpAuthCancelResult, ClientError>;
    async fn list_mcp_servers(&self) -> Result<McpServerListResult, ClientError>;
    async fn add_mcp_server(
        &self,
        params: McpServerAddParams,
    ) -> Result<McpServerMutationResult, ClientError>;
    async fn edit_mcp_server(
        &self,
        params: McpServerEditParams,
    ) -> Result<McpServerMutationResult, ClientError>;
    async fn remove_mcp_server(
        &self,
        params: McpServerNameParams,
    ) -> Result<McpServerMutationResult, ClientError>;
    async fn set_mcp_server_enabled(
        &self,
        params: McpServerSetEnabledParams,
    ) -> Result<McpServerMutationResult, ClientError>;
    async fn reconnect_mcp_server(
        &self,
        params: McpServerNameParams,
    ) -> Result<McpServerMutationResult, ClientError>;
    async fn persist_mcp_server(
        &self,
        params: McpServerPersistParams,
    ) -> Result<McpServerMutationResult, ClientError>;
    async fn subscribe_events(
        &self,
        session_id: SessionId,
        cursor: Option<u64>,
    ) -> Result<(), ClientError>;
    async fn runtime_snapshot(&self) -> Result<RuntimeSnapshotResult, ClientError>;
    async fn connect_provider(
        &self,
        params: ProviderConnectParams,
    ) -> Result<ProviderConnectResult, ClientError>;
    async fn disconnect_provider(
        &self,
        params: ProviderDisconnectParams,
    ) -> Result<ProviderDisconnectResult, ClientError>;
    fn recover_session(&self, session_id: SessionId, full_replay: bool);
    fn shutdown(&self);
}

impl ClientEventSink for mpsc::UnboundedSender<ClientDelivery> {
    fn deliver(&self, delivery: ClientDelivery) {
        let _ = self.send(delivery);
    }
}

/// Errors returned by the transport or a JSON-RPC operation.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("transport closed")]
    Closed,
    #[error("invalid JSON-RPC frame: {0}")]
    InvalidFrame(#[from] serde_json::Error),
    #[error("JSON-RPC error: {0:?}")]
    Rpc(JsonRpcError),
    #[error("websocket error: {0}")]
    WebSocket(String),
    #[error("daemon authentication token is unavailable")]
    TokenUnavailable,
    #[error("daemon authentication token path is unsafe")]
    UnsafeToken,
    #[error("a subscription replay is already in progress")]
    ReplayInProgress,
    #[error("subscription replay response timed out")]
    ReplayTimedOut,
}

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
    response: oneshot::Sender<Result<Value, ClientError>>,
}

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
    response: oneshot::Sender<Result<Value, ClientError>>,
}

#[derive(Default)]
struct Subscription {
    cursor: u64,
    rollback_cursor: u64,
    generation: u64,
    next_attempt: u64,
    active_attempt: u64,
    fetching: bool,
    rebuild: bool,
    final_seq: u64,
    replay_tools: HashSet<ToolCallId>,
    awaiting_snapshots: HashSet<(ToolCallId, bool)>,
    snapshot_deadline: Option<Instant>,
    buffered: Vec<ClientDelivery>,
    recovery_requested: Option<bool>,
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
    #[cfg(test)]
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
        Self::connect_stream_with_sink(transport, delivery_sender, Some(delivery_receiver))
    }

    /// Connect a transport and forward ordered protocol deliveries to a consumer-defined sink.
    pub fn connect_with_event_sink<T, E>(transport: T, sink: E) -> Self
    where
        T: Transport + 'static,
        E: ClientEventSink,
    {
        Self::connect_stream_with_sink(transport, sink, None)
    }

    fn connect_stream_with_sink<T, E>(
        transport: T,
        sink: E,
        delivery_receiver: Option<mpsc::UnboundedReceiver<ClientDelivery>>,
    ) -> Self
    where
        T: Transport + 'static,
        E: ClientEventSink,
    {
        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let deliveries = Arc::new(StdMutex::new(delivery_receiver));
        let subscriptions = Arc::new(Mutex::new(HashMap::new()));
        let (recovery_sender, recovery_receiver) = mpsc::unbounded_channel();
        let recovery = Arc::new(RecoveryQueue {
            sender: recovery_sender,
            state: StdMutex::new(RecoveryQueueState::default()),
        });
        let (control_sender, control_rx) = mpsc::unbounded_channel();
        let pending_command_count = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(ClientShutdown(tokio_util::sync::CancellationToken::new()));
        tokio::spawn(connection_task(
            transport,
            ConnectionTask {
                commands: command_rx,
                controls: control_rx,
                deliveries: Arc::new(sink),
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
            #[cfg(test)]
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

    /// Complete the versioned protocol handshake.
    pub async fn handshake(&self) -> Result<ServerHello, ClientError> {
        let hello: ServerHello = self
            .call(
                "handshake",
                &ClientHello {
                    protocol_version: ProtocolVersion::current(),
                },
            )
            .await?;
        Ok(hello)
    }

    /// Take the sole live, replay, and output delivery receiver. It never
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

    pub async fn session_usage(
        &self,
        params: SessionUsageParams,
    ) -> Result<SessionUsageResult, ClientError> {
        self.call("session.usage", &params).await
    }

    pub async fn agent_usage(
        &self,
        params: AgentUsageParams,
    ) -> Result<AgentUsageResult, ClientError> {
        self.call("agent.usage", &params).await
    }

    pub async fn global_usage(&self) -> Result<GlobalUsageResult, ClientError> {
        self.call("usage.global", &GlobalUsageParams {}).await
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

    pub async fn list_skills(
        &self,
        params: SkillsListParams,
    ) -> Result<SkillsListResult, ClientError> {
        self.call("skills.list", &params).await
    }

    pub async fn get_skill(&self, params: SkillsGetParams) -> Result<SkillsGetResult, ClientError> {
        self.call("skills.get", &params).await
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

    pub async fn recall_steer(
        &self,
        params: RunRecallSteerParams,
    ) -> Result<RunRecallSteerResult, ClientError> {
        self.call("run.recall_steer", &params).await
    }

    pub async fn cancel_run(
        &self,
        params: RunCancelParams,
    ) -> Result<RunCancelResult, ClientError> {
        self.call("run.cancel", &params).await
    }

    pub async fn tool_stdin(
        &self,
        params: RunToolStdinParams,
    ) -> Result<RunToolStdinResult, ClientError> {
        self.call("run.tool_stdin", &params).await
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

    pub async fn begin_mcp_auth(
        &self,
        params: McpAuthBeginParams,
    ) -> Result<McpAuthBeginResult, ClientError> {
        self.call("mcp.auth.begin", &params).await
    }

    pub async fn cancel_mcp_auth(
        &self,
        params: McpAuthCancelParams,
    ) -> Result<McpAuthCancelResult, ClientError> {
        self.call("mcp.auth.cancel", &params).await
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

    pub async fn set_mcp_server_enabled(
        &self,
        params: McpServerSetEnabledParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.call("mcp.server.set_enabled", &params).await
    }

    pub async fn reconnect_mcp_server(
        &self,
        params: McpServerNameParams,
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
        if let Err(error) = request_replay(&self.commands, request, cursor).await {
            self.abort_replay(session_id).await;
            Self::schedule_recovery_queue(&self.recovery, cursor == 0, Some(session_id));
            return Err(error);
        }
        Ok(())
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

    async fn abort_replay(&self, session_id: SessionId) {
        let mut subscriptions = self.subscriptions.lock().await;
        if let Some(subscription) = subscriptions.get_mut(&session_id) {
            subscription.cursor = subscription.rollback_cursor;
            subscription.fetching = false;
            subscription.awaiting_snapshots.clear();
            subscription.replay_tools.clear();
        }
    }

    pub async fn call<P, R>(&self, method: &str, params: &P) -> Result<R, ClientError>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let value = send_command(&self.commands, method, params, None, false).await?;
        Ok(serde_json::from_value(value)?)
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
        let result = send_sensitive_command(&self.commands, method, value).await?;
        Ok(serde_json::from_value(result)?)
    }

    #[cfg(test)]
    fn pending_command_count(&self) -> usize {
        self.pending_command_count.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl ClientProtocol for Client {
    async fn handshake(&self) -> Result<ServerHello, ClientError> {
        Client::handshake(self).await
    }

    async fn create_session(
        &self,
        params: SessionCreateParams,
    ) -> Result<SessionCreateResult, ClientError> {
        Client::create_session(self, params).await
    }
    async fn list_sessions(
        &self,
        params: SessionListParams,
    ) -> Result<SessionListResult, ClientError> {
        Client::list_sessions(self, params).await
    }
    async fn get_session(&self, params: SessionGetParams) -> Result<SessionGetResult, ClientError> {
        Client::get_session(self, params).await
    }
    async fn session_usage(
        &self,
        params: SessionUsageParams,
    ) -> Result<SessionUsageResult, ClientError> {
        Client::session_usage(self, params).await
    }
    async fn agent_usage(&self, params: AgentUsageParams) -> Result<AgentUsageResult, ClientError> {
        Client::agent_usage(self, params).await
    }
    async fn global_usage(&self) -> Result<GlobalUsageResult, ClientError> {
        Client::global_usage(self).await
    }
    async fn session_children(
        &self,
        params: SessionChildrenParams,
    ) -> Result<SessionChildrenResult, ClientError> {
        Client::session_children(self, params).await
    }
    async fn session_tree(
        &self,
        params: SessionTreeParams,
    ) -> Result<SessionTreeResult, ClientError> {
        Client::session_tree(self, params).await
    }
    async fn resume_session(
        &self,
        params: SessionResumeParams,
    ) -> Result<SessionResumeResult, ClientError> {
        Client::resume_session(self, params).await
    }
    async fn rename_session(
        &self,
        params: SessionRenameParams,
    ) -> Result<SessionRenameResult, ClientError> {
        Client::rename_session(self, params).await
    }
    async fn set_permission_mode(
        &self,
        params: SessionSetPermissionModeParams,
    ) -> Result<SessionSetPermissionModeResult, ClientError> {
        Client::set_permission_mode(self, params).await
    }
    async fn get_session_permissions(
        &self,
        params: SessionPermissionGetParams,
    ) -> Result<SessionPermissionGetResult, ClientError> {
        Client::get_session_permissions(self, params).await
    }
    async fn set_session_permission(
        &self,
        params: SessionPermissionSetParams,
    ) -> Result<SessionPermissionMutationResult, ClientError> {
        Client::set_session_permission(self, params).await
    }
    async fn clear_session_permission(
        &self,
        params: SessionPermissionClearParams,
    ) -> Result<SessionPermissionMutationResult, ClientError> {
        Client::clear_session_permission(self, params).await
    }
    async fn list_skills(&self, params: SkillsListParams) -> Result<SkillsListResult, ClientError> {
        Client::list_skills(self, params).await
    }
    async fn get_skill(&self, params: SkillsGetParams) -> Result<SkillsGetResult, ClientError> {
        Client::get_skill(self, params).await
    }
    async fn compact_session(
        &self,
        params: SessionCompactParams,
    ) -> Result<SessionCompactResult, ClientError> {
        Client::compact_session(self, params).await
    }
    async fn revert_session(
        &self,
        params: SessionRevertParams,
    ) -> Result<SessionRevertResult, ClientError> {
        Client::revert_session(self, params).await
    }
    async fn fork_session(
        &self,
        params: SessionForkParams,
    ) -> Result<SessionForkResult, ClientError> {
        Client::fork_session(self, params).await
    }
    async fn start_run(&self, params: RunStartParams) -> Result<RunStartResult, ClientError> {
        Client::start_run(self, params).await
    }
    async fn steer_run(&self, params: RunSteerParams) -> Result<RunSteerResult, ClientError> {
        Client::steer_run(self, params).await
    }
    async fn recall_steer(
        &self,
        params: RunRecallSteerParams,
    ) -> Result<RunRecallSteerResult, ClientError> {
        Client::recall_steer(self, params).await
    }
    async fn cancel_run(&self, params: RunCancelParams) -> Result<RunCancelResult, ClientError> {
        Client::cancel_run(self, params).await
    }
    async fn tool_stdin(
        &self,
        params: RunToolStdinParams,
    ) -> Result<RunToolStdinResult, ClientError> {
        Client::tool_stdin(self, params).await
    }
    async fn respond_approval(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<ApprovalRespondResult, ClientError> {
        Client::respond_approval(self, params).await
    }
    async fn list_approvals(
        &self,
        params: ApprovalListParams,
    ) -> Result<ApprovalListResult, ClientError> {
        Client::list_approvals(self, params).await
    }
    async fn begin_mcp_auth(
        &self,
        params: McpAuthBeginParams,
    ) -> Result<McpAuthBeginResult, ClientError> {
        Client::begin_mcp_auth(self, params).await
    }
    async fn cancel_mcp_auth(
        &self,
        params: McpAuthCancelParams,
    ) -> Result<McpAuthCancelResult, ClientError> {
        Client::cancel_mcp_auth(self, params).await
    }
    async fn list_mcp_servers(&self) -> Result<McpServerListResult, ClientError> {
        Client::list_mcp_servers(self).await
    }
    async fn add_mcp_server(
        &self,
        params: McpServerAddParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        Client::add_mcp_server(self, params).await
    }
    async fn edit_mcp_server(
        &self,
        params: McpServerEditParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        Client::edit_mcp_server(self, params).await
    }
    async fn remove_mcp_server(
        &self,
        params: McpServerNameParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        Client::remove_mcp_server(self, params).await
    }
    async fn set_mcp_server_enabled(
        &self,
        params: McpServerSetEnabledParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        Client::set_mcp_server_enabled(self, params).await
    }
    async fn reconnect_mcp_server(
        &self,
        params: McpServerNameParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        Client::reconnect_mcp_server(self, params).await
    }
    async fn persist_mcp_server(
        &self,
        params: McpServerPersistParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        Client::persist_mcp_server(self, params).await
    }
    async fn subscribe_events(
        &self,
        session_id: SessionId,
        cursor: Option<u64>,
    ) -> Result<(), ClientError> {
        Client::subscribe_events(self, session_id, cursor).await
    }
    async fn runtime_snapshot(&self) -> Result<RuntimeSnapshotResult, ClientError> {
        Client::runtime_snapshot(self).await
    }
    async fn connect_provider(
        &self,
        params: ProviderConnectParams,
    ) -> Result<ProviderConnectResult, ClientError> {
        Client::connect_provider(self, params).await
    }
    async fn disconnect_provider(
        &self,
        params: ProviderDisconnectParams,
    ) -> Result<ProviderDisconnectResult, ClientError> {
        Client::disconnect_provider(self, params).await
    }

    fn recover_session(&self, session_id: SessionId, full_replay: bool) {
        Client::recover_session(self, session_id, full_replay);
    }

    fn shutdown(&self) {
        Client::shutdown(self);
    }
}

#[async_trait::async_trait]
impl<T> ClientProtocol for T
where
    T: std::ops::Deref<Target = Client> + Send + Sync,
{
    async fn handshake(&self) -> Result<ServerHello, ClientError> {
        self.deref().handshake().await
    }
    async fn create_session(
        &self,
        params: SessionCreateParams,
    ) -> Result<SessionCreateResult, ClientError> {
        self.deref().create_session(params).await
    }
    async fn list_sessions(
        &self,
        params: SessionListParams,
    ) -> Result<SessionListResult, ClientError> {
        self.deref().list_sessions(params).await
    }
    async fn get_session(&self, params: SessionGetParams) -> Result<SessionGetResult, ClientError> {
        self.deref().get_session(params).await
    }
    async fn session_usage(
        &self,
        params: SessionUsageParams,
    ) -> Result<SessionUsageResult, ClientError> {
        self.deref().session_usage(params).await
    }
    async fn agent_usage(&self, params: AgentUsageParams) -> Result<AgentUsageResult, ClientError> {
        self.deref().agent_usage(params).await
    }
    async fn global_usage(&self) -> Result<GlobalUsageResult, ClientError> {
        self.deref().global_usage().await
    }
    async fn session_children(
        &self,
        params: SessionChildrenParams,
    ) -> Result<SessionChildrenResult, ClientError> {
        self.deref().session_children(params).await
    }
    async fn session_tree(
        &self,
        params: SessionTreeParams,
    ) -> Result<SessionTreeResult, ClientError> {
        self.deref().session_tree(params).await
    }
    async fn resume_session(
        &self,
        params: SessionResumeParams,
    ) -> Result<SessionResumeResult, ClientError> {
        self.deref().resume_session(params).await
    }
    async fn rename_session(
        &self,
        params: SessionRenameParams,
    ) -> Result<SessionRenameResult, ClientError> {
        self.deref().rename_session(params).await
    }
    async fn set_permission_mode(
        &self,
        params: SessionSetPermissionModeParams,
    ) -> Result<SessionSetPermissionModeResult, ClientError> {
        self.deref().set_permission_mode(params).await
    }
    async fn get_session_permissions(
        &self,
        params: SessionPermissionGetParams,
    ) -> Result<SessionPermissionGetResult, ClientError> {
        self.deref().get_session_permissions(params).await
    }
    async fn set_session_permission(
        &self,
        params: SessionPermissionSetParams,
    ) -> Result<SessionPermissionMutationResult, ClientError> {
        self.deref().set_session_permission(params).await
    }
    async fn clear_session_permission(
        &self,
        params: SessionPermissionClearParams,
    ) -> Result<SessionPermissionMutationResult, ClientError> {
        self.deref().clear_session_permission(params).await
    }
    async fn list_skills(&self, params: SkillsListParams) -> Result<SkillsListResult, ClientError> {
        self.deref().list_skills(params).await
    }
    async fn get_skill(&self, params: SkillsGetParams) -> Result<SkillsGetResult, ClientError> {
        self.deref().get_skill(params).await
    }
    async fn compact_session(
        &self,
        params: SessionCompactParams,
    ) -> Result<SessionCompactResult, ClientError> {
        self.deref().compact_session(params).await
    }
    async fn revert_session(
        &self,
        params: SessionRevertParams,
    ) -> Result<SessionRevertResult, ClientError> {
        self.deref().revert_session(params).await
    }
    async fn fork_session(
        &self,
        params: SessionForkParams,
    ) -> Result<SessionForkResult, ClientError> {
        self.deref().fork_session(params).await
    }
    async fn start_run(&self, params: RunStartParams) -> Result<RunStartResult, ClientError> {
        self.deref().start_run(params).await
    }
    async fn steer_run(&self, params: RunSteerParams) -> Result<RunSteerResult, ClientError> {
        self.deref().steer_run(params).await
    }
    async fn recall_steer(
        &self,
        params: RunRecallSteerParams,
    ) -> Result<RunRecallSteerResult, ClientError> {
        self.deref().recall_steer(params).await
    }
    async fn cancel_run(&self, params: RunCancelParams) -> Result<RunCancelResult, ClientError> {
        self.deref().cancel_run(params).await
    }
    async fn tool_stdin(
        &self,
        params: RunToolStdinParams,
    ) -> Result<RunToolStdinResult, ClientError> {
        self.deref().tool_stdin(params).await
    }
    async fn respond_approval(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<ApprovalRespondResult, ClientError> {
        self.deref().respond_approval(params).await
    }
    async fn list_approvals(
        &self,
        params: ApprovalListParams,
    ) -> Result<ApprovalListResult, ClientError> {
        self.deref().list_approvals(params).await
    }
    async fn begin_mcp_auth(
        &self,
        params: McpAuthBeginParams,
    ) -> Result<McpAuthBeginResult, ClientError> {
        self.deref().begin_mcp_auth(params).await
    }
    async fn cancel_mcp_auth(
        &self,
        params: McpAuthCancelParams,
    ) -> Result<McpAuthCancelResult, ClientError> {
        self.deref().cancel_mcp_auth(params).await
    }
    async fn list_mcp_servers(&self) -> Result<McpServerListResult, ClientError> {
        self.deref().list_mcp_servers().await
    }
    async fn add_mcp_server(
        &self,
        params: McpServerAddParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.deref().add_mcp_server(params).await
    }
    async fn edit_mcp_server(
        &self,
        params: McpServerEditParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.deref().edit_mcp_server(params).await
    }
    async fn remove_mcp_server(
        &self,
        params: McpServerNameParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.deref().remove_mcp_server(params).await
    }
    async fn set_mcp_server_enabled(
        &self,
        params: McpServerSetEnabledParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.deref().set_mcp_server_enabled(params).await
    }
    async fn reconnect_mcp_server(
        &self,
        params: McpServerNameParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.deref().reconnect_mcp_server(params).await
    }
    async fn persist_mcp_server(
        &self,
        params: McpServerPersistParams,
    ) -> Result<McpServerMutationResult, ClientError> {
        self.deref().persist_mcp_server(params).await
    }
    async fn subscribe_events(
        &self,
        session_id: SessionId,
        cursor: Option<u64>,
    ) -> Result<(), ClientError> {
        self.deref().subscribe_events(session_id, cursor).await
    }
    async fn runtime_snapshot(&self) -> Result<RuntimeSnapshotResult, ClientError> {
        self.deref().runtime_snapshot().await
    }
    async fn connect_provider(
        &self,
        params: ProviderConnectParams,
    ) -> Result<ProviderConnectResult, ClientError> {
        self.deref().connect_provider(params).await
    }
    async fn disconnect_provider(
        &self,
        params: ProviderDisconnectParams,
    ) -> Result<ProviderDisconnectResult, ClientError> {
        self.deref().disconnect_provider(params).await
    }
    fn recover_session(&self, session_id: SessionId, full_replay: bool) {
        self.deref().recover_session(session_id, full_replay);
    }
    fn shutdown(&self) {
        self.deref().shutdown();
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
    if subscription.fetching {
        if recovering {
            subscription.recovery_requested =
                Some(subscription.recovery_requested.unwrap_or(false) || rebuild);
        }
        return Err(ClientError::ReplayInProgress);
    }
    subscription.rollback_cursor = subscription.cursor;
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

async fn request_replay(
    commands: &mpsc::Sender<Command>,
    replay: ReplayRequest,
    cursor: u64,
) -> Result<(), ClientError> {
    request_replay_with_timeout(commands, replay, cursor, REPLAY_RESPONSE_TIMEOUT).await
}

async fn request_replay_with_timeout(
    commands: &mpsc::Sender<Command>,
    replay: ReplayRequest,
    cursor: u64,
    timeout: Duration,
) -> Result<(), ClientError> {
    let params = EventsSubscribeParams {
        session_id: replay.session_id,
        cursor: Some(cursor),
    };
    let _ = tokio::time::timeout(
        timeout,
        send_command(commands, "events.subscribe", &params, Some(replay), false),
    )
    .await
    .map_err(|_| ClientError::ReplayTimedOut)??;
    Ok(())
}

async fn send_command<P>(
    commands: &mpsc::Sender<Command>,
    method: &str,
    params: &P,
    replay: Option<ReplayRequest>,
    sensitive: bool,
) -> Result<Value, ClientError>
where
    P: Serialize + ?Sized,
{
    let (response, receiver) = oneshot::channel();
    let id = NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let params = SerializedParams {
        value: serde_json::to_value(params)?,
        sensitive,
    };
    commands
        .send(Command {
            id,
            method: method.to_owned(),
            params,
            replay,
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
) -> Result<Value, ClientError> {
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
    tokio::spawn(async move {
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
                            tokio::time::sleep(Duration::from_millis(delay)).await;
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
                if only_session.is_some_and(|wanted| wanted != *session_id) {
                    return None;
                }
                if subscription.fetching {
                    subscription.recovery_requested =
                        Some(subscription.recovery_requested.unwrap_or(false) || full_replay);
                    return None;
                }
                subscription.rollback_cursor = subscription.cursor;
                let rebuild = full_replay;
                let cursor = if rebuild { 0 } else { subscription.cursor };
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
                ))
            })
            .collect::<Vec<_>>()
    };
    for (request, cursor) in targets {
        let session_id = request.session_id;
        if let Err(error) =
            request_replay_with_timeout(commands, request, cursor, replay_timeout).await
        {
            let mut subscriptions = subscriptions.lock().await;
            if let Some(subscription) = subscriptions.get_mut(&session_id) {
                subscription.cursor = subscription.rollback_cursor;
                subscription.fetching = false;
                subscription.awaiting_snapshots.clear();
                subscription.replay_tools.clear();
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
    deliveries: Arc<dyn ClientEventSink>,
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
    let mut tool_sessions = HashMap::new();
    let mut replay_timeout = tokio::time::interval(Duration::from_millis(25));
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
                            Ok(()) => { pending.insert(command.id, PendingCommand { replay: command.replay, response: command.response }); }
                            Err(_) => { let _ = command.response.send(Err(ClientError::Closed)); break; }
                        },
                        Err(error) => { let _ = command.response.send(Err(ClientError::InvalidFrame(error))); }
                    }
                }
            }
            Some(control) = task.controls.recv() => match control {
                ConnectionControl::RecoveryFailed { session_id, error } => {
                    task.deliveries.deliver(ClientDelivery::RecoveryFailed { session_id, error });
                }
            },
            _ = replay_timeout.tick() => {
                prune_cancelled_commands(&mut pending);
                release_expired_replays(&task.subscriptions, task.deliveries.as_ref(), &task.recovery, &mut tool_sessions).await;
            }
            incoming = stream.recv() => {
                let frame = match incoming {
                    Ok(Some(frame)) => frame,
                    Ok(None) | Err(_) => break,
                };
                prune_cancelled_commands(&mut pending);
                if let Err(error) = handle_frame(
                    frame,
                    &mut pending,
                    task.deliveries.as_ref(),
                    &task.subscriptions,
                    &task.recovery,
                    &mut tool_sessions,
                ).await {
                    resolve_malformed_response(error, &mut pending);
                }
            }
            else => break,
        }
        task.pending_command_count
            .store(pending.len(), Ordering::Relaxed);
    }
    for (_, pending) in pending {
        let _ = pending.response.send(Err(ClientError::Closed));
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
        serde_json::to_value(request)
            .map(MessageFrame::Value)
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

fn resolve_malformed_response(error: ClientError, pending: &mut HashMap<i64, PendingCommand>) {
    let detail = error.to_string();
    let mut pending = std::mem::take(pending).into_iter();
    if let Some((_, command)) = pending.next() {
        let _ = command.response.send(Err(error));
    }
    for (_, command) in pending {
        let error = serde_json::Error::io(std::io::Error::other(detail.clone()));
        let _ = command.response.send(Err(ClientError::InvalidFrame(error)));
    }
}

async fn handle_frame(
    frame: MessageFrame,
    pending: &mut HashMap<i64, PendingCommand>,
    deliveries: &dyn ClientEventSink,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    recovery: &Arc<RecoveryQueue>,
    tool_sessions: &mut HashMap<ToolCallId, SessionId>,
) -> Result<(), ClientError> {
    let value = match frame {
        MessageFrame::Value(value) => value,
        MessageFrame::Text(text) => serde_json::from_str(&text)?,
    };
    if value.get("id").is_some() {
        let request_id = value
            .get("id")
            .cloned()
            .and_then(|id| serde_json::from_value::<JsonRpcId>(id).ok());
        let response: Response = match serde_json::from_value(value) {
            Ok(response) => response,
            Err(error) => {
                if let Some(JsonRpcId::Number(id)) = request_id
                    && let Some(command) = pending.remove(&id)
                {
                    let _ = command.response.send(Err(ClientError::InvalidFrame(error)));
                    return Ok(());
                }
                return Err(ClientError::InvalidFrame(error));
            }
        };
        let (id, result) = match response {
            Response::Success(response) => (response.id, Ok(response.result)),
            Response::Error(response) => (response.id, Err(ClientError::Rpc(response.error))),
        };
        if let JsonRpcId::Number(id) = id
            && let Some(command) = pending.remove(&id)
        {
            match (command.replay, result) {
                (Some(replay), Ok(value)) => {
                    match serde_json::from_value::<EventsSubscribeResult>(value) {
                        Ok(result) => {
                            // The application can now start draining the sole
                            // mpsc consumer while this task injects a large
                            // replay under natural channel backpressure.
                            let _ = command.response.send(Ok(Value::Null));
                            begin_replay(
                                replay,
                                result.events,
                                subscriptions,
                                deliveries,
                                recovery,
                                tool_sessions,
                            )
                            .await;
                        }
                        Err(error) => {
                            let _ = command.response.send(Err(ClientError::InvalidFrame(error)));
                        }
                    }
                }
                (_, result) => {
                    let _ = command.response.send(result);
                }
            }
        }
        return Ok(());
    }
    let notification: Notification = serde_json::from_value(value)?;
    let Some(params) = notification.params else {
        return Ok(());
    };
    match notification.method.as_str() {
        "events.subscription" => {
            let message: EventSubscriptionMessage = serde_json::from_value(params)?;
            route_live(message, deliveries, subscriptions, recovery, tool_sessions).await;
        }
        "events.tool_output_snapshot" => {
            let snapshot = serde_json::from_value(params)?;
            route_output(
                ClientDelivery::OutputSnapshot(snapshot),
                deliveries,
                subscriptions,
                recovery,
                tool_sessions,
            )
            .await;
        }
        "events.tool_output_delta" => {
            let delta = serde_json::from_value(params)?;
            route_output(
                ClientDelivery::OutputDelta(delta),
                deliveries,
                subscriptions,
                recovery,
                tool_sessions,
            )
            .await;
        }
        "events.tool_output_gap" => {
            let gap = serde_json::from_value(params)?;
            route_output(
                ClientDelivery::OutputGap(gap),
                deliveries,
                subscriptions,
                recovery,
                tool_sessions,
            )
            .await;
        }
        crate::RUNTIME_CHANGED_METHOD => {
            let changed = serde_json::from_value(params)?;
            deliveries.deliver(ClientDelivery::RuntimeChanged(Box::new(changed)));
        }
        _ => {}
    }
    Ok(())
}

async fn begin_replay(
    replay: ReplayRequest,
    events: Vec<StoredEvent>,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    deliveries: &dyn ClientEventSink,
    recovery: &Arc<RecoveryQueue>,
    tool_sessions: &mut HashMap<ToolCallId, SessionId>,
) {
    let active_tools = active_tools(&events);
    let final_seq = {
        let mut subscriptions = subscriptions.lock().await;
        let Some(subscription) = subscriptions.get_mut(&replay.session_id) else {
            return;
        };
        if !subscription.fetching
            || subscription.generation != replay.generation
            || subscription.active_attempt != replay.attempt
        {
            return;
        }
        let final_seq = events
            .iter()
            .map(|event| event.seq)
            .max()
            .unwrap_or(subscription.cursor);
        subscription.final_seq = final_seq;
        subscription.rebuild = replay.rebuild;
        subscription.replay_tools = active_tools.iter().copied().collect();
        subscription.awaiting_snapshots = active_tools
            .iter()
            .flat_map(|call_id| [(*call_id, false), (*call_id, true)])
            .collect();
        subscription.snapshot_deadline = (!subscription.awaiting_snapshots.is_empty())
            .then(|| Instant::now() + Duration::from_millis(250));
        final_seq
    };
    deliveries.deliver(ClientDelivery::ReplayStart {
        session_id: replay.session_id,
        generation: replay.generation,
        final_seq,
        rebuild: replay.rebuild,
    });
    for event in events {
        if let EventPayload::ToolCallStarted { start } = &event.payload {
            tool_sessions.insert(start.tool_call_id, event.session_id);
        }
        deliveries.deliver(ClientDelivery::ReplayEvent {
            session_id: replay.session_id,
            generation: replay.generation,
            final_seq,
            event: Box::new(event),
        });
    }
    finish_ready_replay(
        replay.session_id,
        subscriptions,
        deliveries,
        recovery,
        tool_sessions,
    )
    .await;
}

fn active_tools(events: &[StoredEvent]) -> HashSet<ToolCallId> {
    let mut tools = HashSet::new();
    for event in events {
        match &event.payload {
            EventPayload::ToolCallStarted { start } => {
                tools.insert(start.tool_call_id);
            }
            EventPayload::ToolCallTerminated { termination } => {
                tools.remove(&termination.tool_call_id);
            }
            _ => {}
        }
    }
    tools
}

async fn route_live(
    message: EventSubscriptionMessage,
    deliveries: &dyn ClientEventSink,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    recovery: &Arc<RecoveryQueue>,
    tool_sessions: &mut HashMap<ToolCallId, SessionId>,
) {
    let session_id = match &message {
        EventSubscriptionMessage::Event { event } => {
            if let EventPayload::ToolCallStarted { start } = &event.payload {
                tool_sessions.insert(start.tool_call_id, event.session_id);
            }
            event.session_id
        }
        EventSubscriptionMessage::Gap { session_id, .. } => *session_id,
    };
    let publish;
    let mut recover = None;
    {
        let mut subscriptions = subscriptions.lock().await;
        let subscription = subscriptions.entry(session_id).or_default();
        let generation = subscription.generation;
        if subscription.fetching {
            if matches!(message, EventSubscriptionMessage::Gap { .. }) {
                subscription.recovery_requested =
                    Some(subscription.recovery_requested.unwrap_or(false));
            }
            subscription.buffered.push(ClientDelivery::Live {
                message: Box::new(message),
                generation,
            });
            return;
        }
        match &message {
            EventSubscriptionMessage::Event { event } if event.seq > subscription.cursor + 1 => {
                recover = Some((false, Some(session_id)));
            }
            EventSubscriptionMessage::Event { event } if event.seq == subscription.cursor + 1 => {
                subscription.cursor = event.seq;
            }
            EventSubscriptionMessage::Gap {
                last_delivered_seq, ..
            } => {
                subscription.cursor = *last_delivered_seq;
                recover = Some((false, Some(session_id)));
            }
            _ => {}
        }
        publish = ClientDelivery::Live {
            message: Box::new(message),
            generation,
        };
    }
    deliveries.deliver(publish);
    if let Some((full, session)) = recover {
        Client::schedule_recovery_queue(recovery, full, session);
    }
}

async fn route_output(
    delivery: ClientDelivery,
    deliveries: &dyn ClientEventSink,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    recovery: &Arc<RecoveryQueue>,
    tool_sessions: &mut HashMap<ToolCallId, SessionId>,
) {
    let (call_id, stream) = match &delivery {
        ClientDelivery::OutputSnapshot(snapshot) => {
            (snapshot.snapshot.call_id, stream_key(snapshot.stream))
        }
        ClientDelivery::OutputDelta(delta) => (delta.call_id, stream_key(delta.stream)),
        ClientDelivery::OutputGap(gap) => (gap.call_id, stream_key(gap.stream)),
        _ => return,
    };
    let mut buffered = false;
    let mut completes_replay = None;
    if let Some(session_id) = tool_sessions.get(&call_id).copied() {
        let mut subscriptions = subscriptions.lock().await;
        if let Some(subscription) = subscriptions.get_mut(&session_id)
            && subscription.fetching
        {
            let replay_output = subscription.replay_tools.contains(&call_id);
            if matches!(&delivery, ClientDelivery::OutputSnapshot(_))
                && subscription.awaiting_snapshots.remove(&(call_id, stream))
            {
                completes_replay = Some(session_id);
            } else if !replay_output {
                subscription.buffered.push(delivery.clone());
                buffered = true;
            }
        }
    }
    if !buffered {
        deliveries.deliver(delivery);
    }
    if let Some(session_id) = completes_replay {
        finish_ready_replay(
            session_id,
            subscriptions,
            deliveries,
            recovery,
            tool_sessions,
        )
        .await;
    }
    let _ = recovery;
}

async fn finish_ready_replay(
    session_id: SessionId,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    deliveries: &dyn ClientEventSink,
    recovery: &Arc<RecoveryQueue>,
    tool_sessions: &mut HashMap<ToolCallId, SessionId>,
) {
    let (generation, final_seq, buffered, recovery_requested, tail_has_gap) = {
        let mut subscriptions = subscriptions.lock().await;
        let Some(subscription) = subscriptions.get_mut(&session_id) else {
            return;
        };
        if !subscription.fetching || !subscription.awaiting_snapshots.is_empty() {
            return;
        }
        subscription.fetching = false;
        let (tail_cursor, tail_has_gap) = subscription.buffered.iter().fold(
            (subscription.final_seq, false),
            |(cursor, has_gap), delivery| match delivery {
                ClientDelivery::Live { message, .. } => match message.as_ref() {
                    EventSubscriptionMessage::Event { event } if event.seq <= cursor => {
                        (cursor, has_gap)
                    }
                    EventSubscriptionMessage::Event { event } if event.seq == cursor + 1 => {
                        (event.seq, has_gap)
                    }
                    EventSubscriptionMessage::Event { .. }
                    | EventSubscriptionMessage::Gap { .. } => (cursor, true),
                },
                _ => (cursor, has_gap),
            },
        );
        subscription.cursor = tail_cursor;
        subscription.replay_tools.clear();
        subscription.snapshot_deadline = None;
        (
            subscription.generation,
            subscription.final_seq,
            std::mem::take(&mut subscription.buffered),
            subscription.recovery_requested.take(),
            tail_has_gap,
        )
    };
    deliveries.deliver(ClientDelivery::ReplayEnd {
        session_id,
        generation,
        final_seq,
    });
    for delivery in buffered {
        if let ClientDelivery::Live { message, .. } = &delivery
            && let EventSubscriptionMessage::Event { event } = message.as_ref()
        {
            if event.seq <= final_seq {
                continue;
            }
            if let EventPayload::ToolCallStarted { start } = &event.payload {
                tool_sessions.insert(start.tool_call_id, event.session_id);
            }
        }
        deliveries.deliver(delivery);
    }
    if let Some(full) = recovery_requested.or(tail_has_gap.then_some(false)) {
        Client::schedule_recovery_queue(recovery, full, (!full).then_some(session_id));
    }
}

async fn release_expired_replays(
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    deliveries: &dyn ClientEventSink,
    recovery: &Arc<RecoveryQueue>,
    tool_sessions: &mut HashMap<ToolCallId, SessionId>,
) {
    let sessions = {
        let subscriptions = subscriptions.lock().await;
        subscriptions
            .iter()
            .filter_map(|(session_id, subscription)| {
                (subscription.fetching
                    && subscription
                        .snapshot_deadline
                        .is_some_and(|deadline| deadline <= Instant::now()))
                .then_some(*session_id)
            })
            .collect::<Vec<_>>()
    };
    // Server output-tail setup is bounded. Do not hold an event replay open
    // indefinitely if a call completed between its persisted start and output
    // hub registration; live output remains ordered through the same stream.
    for session_id in sessions {
        if let Some(subscription) = subscriptions.lock().await.get_mut(&session_id) {
            subscription.awaiting_snapshots.clear();
        }
        finish_ready_replay(
            session_id,
            subscriptions,
            deliveries,
            recovery,
            tool_sessions,
        )
        .await;
    }
    let _ = recovery;
}

fn stream_key(stream: OutputStream) -> bool {
    matches!(stream, OutputStream::Stderr)
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use jiff::Timestamp;

    use super::*;
    use crate as cookie_agent_protocol;
    use crate::MessageStream;

    fn delivery_channel() -> (
        mpsc::UnboundedSender<ClientDelivery>,
        mpsc::UnboundedReceiver<ClientDelivery>,
    ) {
        mpsc::unbounded_channel()
    }

    fn recovery() -> (
        Arc<RecoveryQueue>,
        mpsc::UnboundedReceiver<(bool, Option<SessionId>)>,
    ) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(RecoveryQueue {
                sender,
                state: StdMutex::new(RecoveryQueueState::default()),
            }),
            receiver,
        )
    }

    fn runtime_snapshot_json(digit: &str) -> Value {
        let revision = format!("sha256:{}", digit.repeat(64 / digit.len()));
        serde_json::json!({
            "snapshot_schema_version": crate::RuntimeSnapshotSchemaVersion::current(),
            "recipe_registry_revision": revision,
            "catalog_revision": revision,
            "catalog_source": "bootstrap",
            "catalog_state": {
                "stale": true,
                "provider_quarantine_count": 0,
                "model_quarantine_count": 0,
                "quarantine_digest": digit.repeat(64 / digit.len()),
                "last_error": null
            },
            "provider_state_revision": revision,
            "provider_store_generation": 1,
            "model_revision": revision,
            "agent_revision": revision,
            "runtime_revision": revision,
            "providers": [],
            "models": [],
            "agents": []
        })
    }

    fn credential_values(secret: &str) -> crate::ProviderCredentialValues {
        let serialized = Zeroizing::new(format!(r#"{{"api_key":"{secret}"}}"#).into_bytes());
        serde_json::from_slice(&serialized).expect("credential values")
    }

    fn event(session_id: SessionId, seq: u64) -> StoredEvent {
        StoredEvent {
            event_schema_version: crate::EventSchemaVersion::current(),
            session_id,
            run_id: Some(crate::RunId::new_v7()),
            seq,
            timestamp: Timestamp::now(),
            payload: EventPayload::TextDelta {
                attempt_id: crate::AttemptId::new_v7(),
                text: seq.to_string(),
            },
        }
    }

    struct ClosingStream;

    #[async_trait]
    impl MessageStream for ClosingStream {
        async fn send(&mut self, _: MessageFrame) -> Result<(), TransportError> {
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
            Ok(None)
        }
    }

    struct ScriptedStream {
        incoming: mpsc::UnboundedReceiver<MessageFrame>,
        sent: mpsc::UnboundedSender<MessageFrame>,
    }

    #[async_trait]
    impl MessageStream for ScriptedStream {
        async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
            self.sent.send(frame).map_err(|_| TransportError::Closed)
        }

        async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
            Ok(self.incoming.recv().await)
        }
    }

    #[tokio::test]
    async fn live_event_racing_replay_is_delivered_after_replay_end() {
        let session_id = SessionId::new_v7();
        let subscriptions = Arc::new(Mutex::new(HashMap::new()));
        let request = prepare_subscription(&subscriptions, session_id, 0, false, true)
            .await
            .expect("prepare replay");
        let (deliveries, mut receiver) = delivery_channel();
        let (recovery, mut recovery_receiver) = recovery();
        let mut tools = HashMap::new();

        route_live(
            EventSubscriptionMessage::Event {
                event: Box::new(event(session_id, 2)),
            },
            &deliveries,
            &subscriptions,
            &recovery,
            &mut tools,
        )
        .await;
        begin_replay(
            request,
            vec![event(session_id, 1)],
            &subscriptions,
            &deliveries,
            &recovery,
            &mut tools,
        )
        .await;

        assert!(matches!(
            receiver.recv().await,
            Some(ClientDelivery::ReplayStart { .. })
        ));
        assert!(
            matches!(receiver.recv().await, Some(ClientDelivery::ReplayEvent { event, .. }) if event.seq == 1)
        );
        assert!(matches!(
            receiver.recv().await,
            Some(ClientDelivery::ReplayEnd { .. })
        ));
        assert!(
            matches!(receiver.recv().await, Some(ClientDelivery::Live { message, .. }) if matches!(message.as_ref(), EventSubscriptionMessage::Event { event } if event.seq == 2))
        );
        route_live(
            EventSubscriptionMessage::Event {
                event: Box::new(event(session_id, 3)),
            },
            &deliveries,
            &subscriptions,
            &recovery,
            &mut tools,
        )
        .await;
        assert!(matches!(
            receiver.recv().await,
            Some(ClientDelivery::Live { message, .. })
                if matches!(message.as_ref(), EventSubscriptionMessage::Event { event } if event.seq == 3)
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), recovery_receiver.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn replay_larger_than_delivery_capacity_reduces_completely() {
        let session_id = SessionId::new_v7();
        let subscriptions = Arc::new(Mutex::new(HashMap::new()));
        let request = prepare_subscription(&subscriptions, session_id, 0, false, true)
            .await
            .expect("prepare replay");
        let (deliveries, mut receiver) = mpsc::unbounded_channel();
        let (recovery, _recovery_receiver) = recovery();
        let events = (1..=2_000).map(|seq| event(session_id, seq)).collect();
        let task = tokio::spawn({
            let subscriptions = subscriptions.clone();
            let recovery = recovery.clone();
            async move {
                begin_replay(
                    request,
                    events,
                    &subscriptions,
                    &deliveries,
                    &recovery,
                    &mut HashMap::new(),
                )
                .await;
            }
        });
        let mut replayed = 0;
        while let Some(delivery) = receiver.recv().await {
            let ended = matches!(&delivery, ClientDelivery::ReplayEnd { .. });
            replayed += usize::from(matches!(delivery, ClientDelivery::ReplayEvent { .. }));
            if ended {
                break;
            }
        }
        task.await.expect("replay task");
        assert_eq!(replayed, 2_000);
    }

    #[tokio::test]
    async fn rpc_completes_while_an_unconsumed_replay_backlog_grows() {
        let session_id = SessionId::new_v7();
        let (incoming, incoming_rx) = mpsc::unbounded_channel();
        let (sent, mut sent_rx) = mpsc::unbounded_channel();
        let client = Client::connect_stream(ScriptedStream {
            incoming: incoming_rx,
            sent,
        });
        let _deliveries = client.subscribe_deliveries().expect("delivery receiver");
        let subscribe = tokio::spawn({
            let client = client.clone();
            async move { client.subscribe_events(session_id, None).await }
        });
        let MessageFrame::Value(subscribe_request) =
            sent_rx.recv().await.expect("subscribe request")
        else {
            panic!("expected value request");
        };
        assert_eq!(subscribe_request["method"], "events.subscribe");
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": subscribe_request["id"],
                "result": { "events": (1..=2_000).map(|seq| event(session_id, seq)).collect::<Vec<_>>() },
            })))
            .expect("replay response");
        subscribe
            .await
            .expect("subscribe task")
            .expect("subscribe result");

        let tree = tokio::spawn({
            let client = client.clone();
            async move { client.session_tree(SessionTreeParams { session_id }).await }
        });
        let MessageFrame::Value(tree_request) =
            tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
                .await
                .expect("tree request was not blocked")
                .expect("connection open")
        else {
            panic!("expected value request");
        };
        assert_eq!(tree_request["method"], "session.tree");
        tree.abort();
    }

    #[tokio::test]
    async fn timed_out_calls_are_pruned_and_late_responses_are_harmless() {
        let (incoming, incoming_rx) = mpsc::unbounded_channel();
        let (sent, mut sent_rx) = mpsc::unbounded_channel();
        let client = Client::connect_stream(ScriptedStream {
            incoming: incoming_rx,
            sent,
        });
        let mut timed_out_requests = Vec::new();

        for _ in 0..32 {
            let call = tokio::spawn({
                let client = client.clone();
                async move {
                    tokio::time::timeout(
                        Duration::from_millis(5),
                        client.create_session(SessionCreateParams {
                            selection: cookie_agent_protocol::RunSelection {
                                agent: cookie_agent_protocol::AgentId::new("primary")
                                    .expect("agent id"),
                                model: cookie_agent_protocol::ModelSelection {
                                    model: "gateway/arbitrary-model"
                                        .parse::<cookie_agent_protocol::ModelKey>()
                                        .expect("model key"),
                                    variant: None,
                                },
                            },
                        }),
                    )
                    .await
                }
            });
            let MessageFrame::Value(request) =
                tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
                    .await
                    .expect("session.create request timeout")
                    .expect("connection open")
            else {
                panic!("expected value request");
            };
            assert_eq!(request["method"], "session.create");
            timed_out_requests.push(request);
            assert!(call.await.expect("call task").is_err());
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            while client.pending_command_count() != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cancelled requests were not pruned");
        assert_eq!(client.pending_command_count(), 0);

        for request in timed_out_requests {
            incoming
                .send(MessageFrame::Value(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": null,
                })))
                .expect("late response");
        }

        let snapshot = tokio::spawn({
            let client = client.clone();
            async move { client.runtime_snapshot().await }
        });
        let MessageFrame::Value(request) =
            tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
                .await
                .expect("runtime snapshot request timeout")
                .expect("connection open")
        else {
            panic!("expected value request");
        };
        assert_eq!(request["method"], "runtime.snapshot.get");
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": { "snapshot": runtime_snapshot_json("0") },
            })))
            .expect("runtime snapshot response");
        assert!(
            snapshot
                .await
                .expect("snapshot task")
                .expect("runtime snapshot result")
                .snapshot
                .agents
                .is_empty()
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while client.pending_command_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completed request remained pending");
        assert_eq!(client.pending_command_count(), 0);
    }

    #[tokio::test]
    async fn runtime_changed_notifications_reach_the_ui_in_transport_order() {
        let (incoming, incoming_rx) = mpsc::unbounded_channel();
        let (sent, _sent_rx) = mpsc::unbounded_channel();
        let client = Client::connect_stream(ScriptedStream {
            incoming: incoming_rx,
            sent,
        });
        let mut deliveries = client.subscribe_deliveries().expect("delivery receiver");
        for (previous, digit, reason) in
            [(None, "1", "startup"), (Some("1"), "2", "config_reloaded")]
        {
            let previous_revision = previous.map(|digit| {
                serde_json::json!(format!("sha256:{}", digit.repeat(64 / digit.len())))
            });
            incoming
                .send(MessageFrame::Value(serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "runtime.changed",
                    "params": {
                        "previous_revision": previous_revision,
                        "snapshot": runtime_snapshot_json(digit),
                        "reasons": [reason]
                    }
                })))
                .expect("runtime notification");
        }
        let first = deliveries.recv().await.expect("first delivery");
        let second = deliveries.recv().await.expect("second delivery");
        let ClientDelivery::RuntimeChanged(first) = first else {
            panic!("runtime delivery")
        };
        let ClientDelivery::RuntimeChanged(second) = second else {
            panic!("runtime delivery")
        };
        assert_eq!(first.previous_revision, None);
        assert_eq!(
            second.previous_revision.as_ref(),
            Some(&first.snapshot.runtime_revision)
        );
    }

    #[tokio::test]
    async fn cancelled_provider_connect_wipes_source_and_serialized_credentials() {
        let source_before = PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed);
        let serialized_before = SENSITIVE_SERIALIZED_WIPE_COUNT.load(Ordering::Relaxed);
        let (_incoming, incoming_rx) = mpsc::unbounded_channel();
        let (sent, mut sent_rx) = mpsc::unbounded_channel();
        let client = Client::connect_stream(ScriptedStream {
            incoming: incoming_rx,
            sent,
        });
        let connect = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .connect_provider(cookie_agent_protocol::ProviderConnectParams {
                        client_connect_id: cookie_agent_protocol::ClientConnectId::new(
                            "connect-test",
                        )
                        .expect("client connect id"),
                        provider_id: cookie_agent_protocol::ProviderId::new("test")
                            .expect("provider id"),
                        expected_catalog_revision: cookie_agent_protocol::CatalogRevision::new(
                            format!("sha256:{}", "1".repeat(64)),
                        )
                        .expect("catalog revision"),
                        setup_values: std::collections::BTreeMap::new(),
                        auth_method: cookie_agent_protocol::AuthMethodId::new("api-key")
                            .expect("auth method"),
                        auth_values: credential_values("sentinel-secret"),
                    })
                    .await
            }
        });
        let mut request = tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
            .await
            .expect("provider.connect request timeout")
            .expect("connection open");
        assert!(matches!(
            &request,
            MessageFrame::Text(text)
                if text.contains("\"method\":\"provider.connect\"")
                    && text.contains("sentinel-secret")
        ));
        if let MessageFrame::Text(text) = &mut request {
            text.zeroize();
        }
        drop(request);
        connect.abort();
        let _ = connect.await;

        tokio::time::timeout(Duration::from_secs(1), async {
            while PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed) <= source_before
                || SENSITIVE_SERIALIZED_WIPE_COUNT.load(Ordering::Relaxed) <= serialized_before
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("credential guards were not dropped after cancellation");
    }

    #[test]
    fn sensitive_frame_debug_is_redacted_and_drop_is_observable() {
        let before = SENSITIVE_FRAME_WIPE_COUNT.load(Ordering::Relaxed);
        let frame = SensitiveFrame::new("sentinel-secret".into());
        let debug = format!("{frame:?}");
        assert_eq!(debug, "SensitiveFrame(<redacted>)");
        assert!(!debug.contains("sentinel-secret"));
        drop(frame);
        assert!(SENSITIVE_FRAME_WIPE_COUNT.load(Ordering::Relaxed) > before);
    }

    #[test]
    fn secret_json_owner_wipes_its_sentinel_tree_on_drop() {
        let before = SENSITIVE_SERIALIZED_WIPE_COUNT.load(Ordering::Relaxed);
        let mut value = SensitiveJson::object();
        value.object_mut().insert(
            "auth_values".into(),
            serde_json::json!({"api_key": "sentinel-secret"}),
        );
        drop(value);
        assert!(SENSITIVE_SERIALIZED_WIPE_COUNT.load(Ordering::Relaxed) > before);
    }

    #[test]
    fn cancelled_unpolled_sensitive_dispatch_drops_the_wiping_frame() {
        let before = SENSITIVE_FRAME_WIPE_COUNT.load(Ordering::Relaxed);
        let mut stream = ClosingStream;
        let dispatch = send_outbound_frame(
            &mut stream,
            OutboundFrame::Sensitive(SensitiveFrame::new("sentinel-secret".into())),
        );
        drop(dispatch);
        assert!(SENSITIVE_FRAME_WIPE_COUNT.load(Ordering::Relaxed) > before);
    }

    #[tokio::test]
    async fn unpolled_provider_connect_future_still_wipes_owned_credentials() {
        let source_before = PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed);
        let client = Client::connect_stream(ClosingStream);
        let connect = client.connect_provider(cookie_agent_protocol::ProviderConnectParams {
            client_connect_id: cookie_agent_protocol::ClientConnectId::new("unpolled")
                .expect("client connect id"),
            provider_id: cookie_agent_protocol::ProviderId::new("test").expect("provider id"),
            expected_catalog_revision: cookie_agent_protocol::CatalogRevision::new(format!(
                "sha256:{}",
                "1".repeat(64)
            ))
            .expect("catalog revision"),
            setup_values: std::collections::BTreeMap::new(),
            auth_method: cookie_agent_protocol::AuthMethodId::new("api-key").expect("auth method"),
            auth_values: credential_values("sentinel-secret"),
        });
        drop(connect);
        assert!(PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed) > source_before);
    }

    #[tokio::test]
    async fn failed_provider_connect_wipes_source_and_sensitive_json_tree() {
        let source_before = PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed);
        let serialized_before = SENSITIVE_SERIALIZED_WIPE_COUNT.load(Ordering::Relaxed);
        let client = Client::connect_stream(ClosingStream);
        let result = client
            .connect_provider(cookie_agent_protocol::ProviderConnectParams {
                client_connect_id: cookie_agent_protocol::ClientConnectId::new("failed-connect")
                    .expect("client connect id"),
                provider_id: cookie_agent_protocol::ProviderId::new("test").expect("provider id"),
                expected_catalog_revision: cookie_agent_protocol::CatalogRevision::new(format!(
                    "sha256:{}",
                    "1".repeat(64)
                ))
                .expect("catalog revision"),
                setup_values: std::collections::BTreeMap::new(),
                auth_method: cookie_agent_protocol::AuthMethodId::new("api-key")
                    .expect("auth method"),
                auth_values: credential_values("sentinel-secret"),
            })
            .await;
        assert!(result.is_err());
        assert!(PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed) > source_before);
        assert!(SENSITIVE_SERIALIZED_WIPE_COUNT.load(Ordering::Relaxed) > serialized_before);
    }

    #[tokio::test]
    async fn non_contiguous_buffered_tail_keeps_prefix_cursor_and_recovers() {
        let session_id = SessionId::new_v7();
        let subscriptions = Arc::new(Mutex::new(HashMap::new()));
        let request = prepare_subscription(&subscriptions, session_id, 0, false, true)
            .await
            .expect("prepare replay");
        let (deliveries, _receiver) = delivery_channel();
        let (recovery, mut recovery_receiver) = recovery();
        let mut tools = HashMap::new();
        for seq in [11, 13] {
            route_live(
                EventSubscriptionMessage::Event {
                    event: Box::new(event(session_id, seq)),
                },
                &deliveries,
                &subscriptions,
                &recovery,
                &mut tools,
            )
            .await;
        }
        begin_replay(
            request,
            (1..=10).map(|seq| event(session_id, seq)).collect(),
            &subscriptions,
            &deliveries,
            &recovery,
            &mut tools,
        )
        .await;
        assert_eq!(
            subscriptions.lock().await[&session_id].cursor,
            11,
            "cursor stops before the missing sequence"
        );
        assert_eq!(
            recovery_receiver.recv().await,
            Some((false, Some(session_id)))
        );
    }

    #[tokio::test]
    async fn buffered_gap_schedules_one_recovery() {
        let session_id = SessionId::new_v7();
        let subscriptions = Arc::new(Mutex::new(HashMap::new()));
        let request = prepare_subscription(&subscriptions, session_id, 0, false, true)
            .await
            .expect("prepare replay");
        let (deliveries, _receiver) = delivery_channel();
        let (recovery, mut recovery_receiver) = recovery();
        let mut tools = HashMap::new();
        route_live(
            EventSubscriptionMessage::Gap {
                session_id,
                last_delivered_seq: 0,
            },
            &deliveries,
            &subscriptions,
            &recovery,
            &mut tools,
        )
        .await;
        begin_replay(
            request,
            vec![event(session_id, 1)],
            &subscriptions,
            &deliveries,
            &recovery,
            &mut tools,
        )
        .await;
        assert_eq!(
            recovery_receiver.recv().await,
            Some((false, Some(session_id)))
        );
        assert!(recovery_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn stale_replay_attempt_response_is_discarded() {
        let session_id = SessionId::new_v7();
        let subscriptions = Arc::new(Mutex::new(HashMap::new()));
        let first = prepare_subscription(&subscriptions, session_id, 0, false, true)
            .await
            .expect("first request");
        subscriptions
            .lock()
            .await
            .get_mut(&session_id)
            .expect("subscription")
            .fetching = false;
        let second = prepare_subscription(&subscriptions, session_id, 0, false, true)
            .await
            .expect("second request");
        let (deliveries, mut receiver) = delivery_channel();
        let (recovery, _recovery_receiver) = recovery();
        let mut tools = HashMap::new();
        begin_replay(
            first,
            vec![event(session_id, 1)],
            &subscriptions,
            &deliveries,
            &recovery,
            &mut tools,
        )
        .await;
        assert!(receiver.try_recv().is_err());
        begin_replay(
            second,
            vec![event(session_id, 1)],
            &subscriptions,
            &deliveries,
            &recovery,
            &mut tools,
        )
        .await;
        assert!(matches!(
            receiver.recv().await,
            Some(ClientDelivery::ReplayStart { .. })
        ));
    }

    #[tokio::test]
    async fn replayed_active_tool_snapshots_precede_replay_end() {
        let session_id = SessionId::new_v7();
        let call_id = ToolCallId::new_v7();
        let subscriptions = Arc::new(Mutex::new(HashMap::new()));
        let request = prepare_subscription(&subscriptions, session_id, 0, false, true)
            .await
            .expect("prepare replay");
        let (deliveries, mut receiver) = delivery_channel();
        let (recovery, _recovery_receiver) = recovery();
        let mut tools = HashMap::new();
        let started = StoredEvent {
            event_schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
            session_id,
            run_id: Some(cookie_agent_protocol::RunId::new_v7()),
            seq: 1,
            timestamp: Timestamp::now(),
            payload: EventPayload::ToolCallStarted {
                start: cookie_agent_protocol::ToolCallStart {
                    tool_call_id: call_id,
                    owner: cookie_agent_protocol::AssistantToolCallRef {
                        model_turn_seq: 1,
                        content_index: 0,
                        model_call_id: cookie_agent_protocol::ModelCallId::new("model-call")
                            .expect("model call id"),
                        provider_item_id: None,
                    },
                    presentation: cookie_agent_protocol::ToolCallPresentation {
                        title: cookie_agent_protocol::SafeDisplayText::new("bash")
                            .expect("presentation title"),
                        primary_argument: None,
                    },
                    operation_fingerprint:
                        cookie_agent_protocol::OperationFingerprint::from_prepared_operation(
                            &cookie_agent_protocol::PreparedOperationIdentity::new(
                                cookie_agent_protocol::Sha256Digest::of_bytes(b"arguments"),
                                vec![cookie_agent_protocol::ApprovalCapability {
                                    action: cookie_agent_protocol::PermissionAction::Bash,
                                    operation:
                                        cookie_agent_protocol::PreparedCapabilityOperation::new(
                                            "execute",
                                        )
                                        .expect("capability operation"),
                                }],
                                vec![cookie_agent_protocol::PreparedApprovalResource {
                                    capability: cookie_agent_protocol::PermissionAction::Bash,
                                    canonical:
                                        cookie_agent_protocol::PreparedResourceIdentity::new(
                                            "command:replay",
                                        )
                                        .expect("resource identity"),
                                    binding_digest:
                                        cookie_agent_protocol::PreparedResourceDigest::from_canonical_binding_bytes(
                                            b"replay",
                                        ),
                                    binding_lifetime:
                                        cookie_agent_protocol::PreparedBindingLifetime::ProcessLocal,
                                    boundary: cookie_agent_protocol::ApprovalBoundary::Exact,
                                    source:
                                        cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
                                }],
                                cookie_agent_protocol::Sha256Digest::of_bytes(b"context"),
                            )
                            .expect("prepared operation"),
                        ),
                },
            },
        };
        begin_replay(
            request,
            vec![started],
            &subscriptions,
            &deliveries,
            &recovery,
            &mut tools,
        )
        .await;
        for stream in [OutputStream::Stdout, OutputStream::Stderr] {
            route_output(
                ClientDelivery::OutputSnapshot(OutputSnapshotEnvelope {
                    stream,
                    snapshot: cookie_agent_protocol::OutputSnapshot {
                        call_id,
                        start_offset: 0,
                        end_offset: 0,
                        chunks: Vec::new(),
                    },
                }),
                &deliveries,
                &subscriptions,
                &recovery,
                &mut tools,
            )
            .await;
        }

        assert!(matches!(
            receiver.recv().await,
            Some(ClientDelivery::ReplayStart { .. })
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(ClientDelivery::ReplayEvent { .. })
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(ClientDelivery::OutputSnapshot(_))
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(ClientDelivery::OutputSnapshot(_))
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(ClientDelivery::ReplayEnd { .. })
        ));
    }

    #[tokio::test]
    async fn discontinuity_queues_targeted_recovery() {
        let session_id = SessionId::new_v7();
        let subscriptions = Arc::new(Mutex::new(HashMap::new()));
        let (deliveries, _receiver) = delivery_channel();
        let (recovery, mut receiver) = recovery();
        route_live(
            EventSubscriptionMessage::Event {
                event: Box::new(event(session_id, 2)),
            },
            &deliveries,
            &subscriptions,
            &recovery,
            &mut HashMap::new(),
        )
        .await;
        assert_eq!(receiver.recv().await, Some((false, Some(session_id))));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn recovery_worker_retries_then_reports_failure() {
        let session_id = SessionId::new_v7();
        let (commands, mut command_receiver) = mpsc::channel(8);
        let subscriptions = Arc::new(Mutex::new(HashMap::from([(
            session_id,
            Subscription::default(),
        )])));
        let (recovery, recovery_receiver) = recovery();
        let (controls, mut control_receiver) = mpsc::unbounded_channel();
        spawn_recovery_worker(
            commands,
            subscriptions,
            recovery_receiver,
            recovery.clone(),
            controls,
            Duration::from_millis(10),
        );
        Client::schedule_recovery_queue(&recovery, true, Some(session_id));
        for _ in 0..RECOVERY_ATTEMPTS {
            let command = command_receiver.recv().await.expect("recovery request");
            assert_eq!(command.method, "events.subscribe");
            command
                .response
                .send(Err(ClientError::Closed))
                .expect("fail replay");
        }
        assert!(matches!(
            control_receiver.recv().await,
            Some(ConnectionControl::RecoveryFailed {
                session_id: Some(id),
                ..
            }) if id == session_id
        ));
    }

    #[tokio::test]
    async fn recovery_timeout_retries_then_gives_up() {
        let session_id = SessionId::new_v7();
        let (commands, mut command_receiver) = mpsc::channel(8);
        let subscriptions = Arc::new(Mutex::new(HashMap::from([(
            session_id,
            Subscription::default(),
        )])));
        let (recovery, recovery_receiver) = recovery();
        let (controls, mut control_receiver) = mpsc::unbounded_channel();
        spawn_recovery_worker(
            commands,
            subscriptions,
            recovery_receiver,
            recovery.clone(),
            controls,
            Duration::from_millis(10),
        );
        Client::schedule_recovery_queue(&recovery, true, Some(session_id));
        let mut held_commands = Vec::new();
        for _ in 0..RECOVERY_ATTEMPTS {
            held_commands.push(command_receiver.recv().await.expect("recovery request"));
        }
        assert!(matches!(
            control_receiver.recv().await,
            Some(ConnectionControl::RecoveryFailed { error, .. }) if error.contains("timed out")
        ));
    }

    #[tokio::test]
    async fn queued_recovery_dies_with_a_disconnected_connection() {
        let client = Client::connect_stream(ClosingStream);
        let recovery = Arc::downgrade(&client.recovery);
        let mut deliveries = client.subscribe_deliveries().expect("delivery receiver");
        assert!(deliveries.recv().await.is_none());
        let session_id = SessionId::new_v7();
        client
            .subscriptions
            .lock()
            .await
            .insert(session_id, Subscription::default());
        client.recover_session(session_id, true);
        drop(client);
        tokio::time::timeout(Duration::from_secs(1), async {
            while recovery.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("recovery worker released after disconnect");
    }
}
