//! Transport-neutral JSON-RPC client used by the terminal UI.

use std::{
    collections::{HashMap, HashSet},
    env, fmt, fs,
    net::IpAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use cookie_agent_protocol::{
    AgentListParams, AgentListResult, ApprovalListParams, ApprovalListResult,
    ApprovalRespondParams, ApprovalRespondResult, CatalogModelListParams, CatalogModelListResult,
    CatalogProviderListParams, CatalogProviderListResult, ClientHello, Event, EventEnvelope,
    EventSubscriptionMessage, EventsSubscribeParams, EventsSubscribeResult, JsonRpcError,
    JsonRpcId, ModelListParams, ModelListResult, Notification, OutputDelta, OutputGap,
    OutputSnapshotEnvelope, OutputStream, ProtocolVersion, ProviderConnectParams,
    ProviderConnectResult, Response, RunCancelParams, RunCancelResult, RunStartParams,
    RunStartResult, RunSteerParams, RunSteerResult, RunToolStdinParams, RunToolStdinResult,
    ServerHello, SessionCreateParams, SessionCreateResult, SessionId, SessionListParams,
    SessionListResult, SessionRenameParams, SessionRenameResult, SessionTreeParams,
    SessionTreeResult, ToolCallId,
};
use cookie_agent_server::{MessageFrame, MessageStream, Server, TransportError, in_process_pair};
use futures_util::{SinkExt, StreamExt};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::Uri};
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
        event: Box<EventEnvelope>,
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

struct ProviderConnectGuard(ProviderConnectParams);

impl Drop for ProviderConnectGuard {
    fn drop(&mut self) {
        for credential in self.0.credentials.values.values_mut() {
            credential.zeroize();
        }
        record_provider_connect_wipe();
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
    #[cfg(test)]
    pending_command_count: Arc<AtomicUsize>,
}

impl Client {
    /// Connect an already-created message stream and start its routing task.
    pub fn connect_stream<S>(stream: S) -> Self
    where
        S: MessageStream + 'static,
    {
        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        // The connection task is the RPC router and never awaits UI
        // consumption. This sole-consumer queue is lossless; a permanently
        // stalled UI may grow memory, but it is terminal rather than lossy.
        let (delivery_sender, delivery_receiver) = mpsc::unbounded_channel();
        let deliveries = Arc::new(StdMutex::new(Some(delivery_receiver)));
        let subscriptions = Arc::new(Mutex::new(HashMap::new()));
        let (recovery_sender, recovery_receiver) = mpsc::unbounded_channel();
        let recovery = Arc::new(RecoveryQueue {
            sender: recovery_sender,
            state: StdMutex::new(RecoveryQueueState::default()),
        });
        let (control_sender, control_rx) = mpsc::unbounded_channel();
        let pending_command_count = Arc::new(AtomicUsize::new(0));
        tokio::spawn(connection_task(
            stream,
            command_rx,
            control_rx,
            delivery_sender,
            subscriptions.clone(),
            recovery.clone(),
            pending_command_count.clone(),
        ));
        let client = Self {
            commands,
            deliveries,
            subscriptions,
            recovery: recovery.clone(),
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

    /// Pair an in-process client stream with a server task.
    pub fn connect_in_process(server: Arc<Server>) -> Self {
        let (client_stream, server_stream) = in_process_pair(COMMAND_QUEUE_CAPACITY);
        tokio::spawn(async move {
            let _ = server.serve_stream(server_stream).await;
        });
        Self::connect_stream(client_stream)
    }

    /// Connect to the daemon's WebSocket endpoint.
    pub async fn connect_websocket(url: &str) -> Result<Self, ClientError> {
        let token = read_daemon_token()?;
        Self::connect_websocket_with_token(url, &token).await
    }

    /// Connect with an explicit bearer token. The token is used only to build
    /// the WebSocket upgrade request and is never retained by the client.
    pub async fn connect_websocket_with_token(url: &str, token: &str) -> Result<Self, ClientError> {
        validate_websocket_url(url)?;
        let request = authenticated_request(url, token)?;
        let (socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|error| ClientError::WebSocket(error.to_string()))?;
        Ok(Self::connect_stream(WebSocketClientStream { socket }))
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

    pub async fn session_tree(
        &self,
        params: SessionTreeParams,
    ) -> Result<SessionTreeResult, ClientError> {
        self.call("session.tree", &params).await
    }

    pub async fn rename_session(
        &self,
        params: SessionRenameParams,
    ) -> Result<SessionRenameResult, ClientError> {
        self.call("session.rename", &params).await
    }

    pub async fn list_catalog_providers(
        &self,
        params: CatalogProviderListParams,
    ) -> Result<CatalogProviderListResult, ClientError> {
        self.call("catalog.provider.list", &params).await
    }

    pub async fn list_catalog_models(
        &self,
        params: CatalogModelListParams,
    ) -> Result<CatalogModelListResult, ClientError> {
        self.call("catalog.model.list", &params).await
    }

    pub fn connect_provider(
        &self,
        params: ProviderConnectParams,
    ) -> impl std::future::Future<Output = Result<ProviderConnectResult, ClientError>> + '_ {
        let params = ProviderConnectGuard(params);
        async move {
            let value =
                send_command(&self.commands, "provider.connect", &params.0, None, true).await?;
            Ok(serde_json::from_value(value)?)
        }
    }

    pub async fn list_agents(
        &self,
        params: AgentListParams,
    ) -> Result<AgentListResult, ClientError> {
        self.call("agent.list", &params).await
    }

    pub async fn list_models(
        &self,
        params: ModelListParams,
    ) -> Result<ModelListResult, ClientError> {
        self.call("model.list", &params).await
    }

    pub async fn start_run(&self, params: RunStartParams) -> Result<RunStartResult, ClientError> {
        self.call("run.start", &params).await
    }

    pub async fn steer_run(&self, params: RunSteerParams) -> Result<RunSteerResult, ClientError> {
        self.call("run.steer", &params).await
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

    async fn call<P, R>(&self, method: &str, params: &P) -> Result<R, ClientError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let value = send_command(&self.commands, method, params, None, false).await?;
        Ok(serde_json::from_value(value)?)
    }

    #[cfg(test)]
    fn pending_command_count(&self) -> usize {
        self.pending_command_count.load(Ordering::Relaxed)
    }
}

/// Validate an attach endpoint before opening a network connection.
pub fn validate_websocket_url(url: &str) -> Result<(), ClientError> {
    let uri = url
        .parse::<Uri>()
        .map_err(|error| ClientError::WebSocket(format!("invalid WebSocket URL: {error}")))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| ClientError::WebSocket("WebSocket URL must include a scheme".into()))?;
    if !scheme.eq_ignore_ascii_case("ws") && !scheme.eq_ignore_ascii_case("wss") {
        return Err(ClientError::WebSocket(
            "WebSocket URL must use the ws or wss scheme".into(),
        ));
    }
    let authority = uri
        .authority()
        .ok_or_else(|| ClientError::WebSocket("WebSocket URL must include a host".into()))?;
    if authority.as_str().contains('@') {
        return Err(ClientError::WebSocket(
            "WebSocket URL must not contain credentials".into(),
        ));
    }
    let host = authority.host();
    let host_without_brackets = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host_without_brackets
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !loopback {
        return Err(ClientError::WebSocket(
            "WebSocket URL host must be loopback".into(),
        ));
    }
    Ok(())
}

fn authenticated_request(
    url: &str,
    token: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, ClientError> {
    if token.len() != 43
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ClientError::UnsafeToken);
    }
    let mut request = url
        .into_client_request()
        .map_err(|error| ClientError::WebSocket(error.to_string()))?;
    let authorization = Zeroizing::new(format!("Bearer {token}"));
    let value = authorization
        .parse()
        .map_err(|_| ClientError::UnsafeToken)?;
    request.headers_mut().insert("authorization", value);
    Ok(request)
}

/// Read the daemon bearer token from its private per-user location.
pub fn read_daemon_token() -> Result<Zeroizing<String>, ClientError> {
    let home = env::var_os("HOME").ok_or(ClientError::TokenUnavailable)?;
    let path = PathBuf::from(home).join(".local/share/cookie_agent/daemon/token-v1");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = fs::symlink_metadata(&path).map_err(|_| ClientError::TokenUnavailable)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != rustix_uid()
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
            || metadata.len() != 43
        {
            return Err(ClientError::UnsafeToken);
        }
    }
    #[cfg(not(unix))]
    return Err(ClientError::UnsafeToken);

    #[cfg(unix)]
    {
        let token = fs::read_to_string(path).map_err(|_| ClientError::TokenUnavailable)?;
        authenticated_request("ws://127.0.0.1/", &token)?;
        Ok(Zeroizing::new(token))
    }
}

#[cfg(unix)]
fn rustix_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and does not retain pointers.
    unsafe { libc::geteuid() }
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
    P: Serialize,
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

async fn connection_task<S>(
    mut stream: S,
    mut commands: mpsc::Receiver<Command>,
    mut controls: mpsc::UnboundedReceiver<ConnectionControl>,
    deliveries: mpsc::UnboundedSender<ClientDelivery>,
    subscriptions: Arc<Mutex<HashMap<SessionId, Subscription>>>,
    recovery: Arc<RecoveryQueue>,
    pending_command_count: Arc<AtomicUsize>,
) where
    S: MessageStream,
{
    let mut pending = HashMap::new();
    let mut tool_sessions = HashMap::new();
    let mut replay_timeout = tokio::time::interval(Duration::from_millis(25));
    loop {
        tokio::select! {
            Some(command) = commands.recv() => {
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
            Some(control) = controls.recv() => match control {
                ConnectionControl::RecoveryFailed { session_id, error } => {
                    let _ = deliveries.send(ClientDelivery::RecoveryFailed { session_id, error });
                }
            },
            _ = replay_timeout.tick() => {
                prune_cancelled_commands(&mut pending);
                release_expired_replays(&subscriptions, &deliveries, &recovery, &mut tool_sessions).await;
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
                    &deliveries,
                    &subscriptions,
                    &recovery,
                    &mut tool_sessions,
                ).await {
                    resolve_malformed_response(error, &mut pending);
                }
            }
            else => break,
        }
        pending_command_count.store(pending.len(), Ordering::Relaxed);
    }
    for (_, pending) in pending {
        let _ = pending.response.send(Err(ClientError::Closed));
    }
    pending_command_count.store(0, Ordering::Relaxed);
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
    S: MessageStream,
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
    deliveries: &mpsc::UnboundedSender<ClientDelivery>,
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
        _ => {}
    }
    Ok(())
}

async fn begin_replay(
    replay: ReplayRequest,
    events: Vec<EventEnvelope>,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    deliveries: &mpsc::UnboundedSender<ClientDelivery>,
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
    let _ = deliveries.send(ClientDelivery::ReplayStart {
        session_id: replay.session_id,
        generation: replay.generation,
        final_seq,
        rebuild: replay.rebuild,
    });
    for event in events {
        if let Event::ToolCallStarted { tool_call_id, .. } = &event.event {
            tool_sessions.insert(*tool_call_id, event.session_id);
        }
        let _ = deliveries.send(ClientDelivery::ReplayEvent {
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

fn active_tools(events: &[EventEnvelope]) -> HashSet<ToolCallId> {
    let mut tools = HashSet::new();
    for event in events {
        match &event.event {
            Event::ToolCallStarted { tool_call_id, .. } => {
                tools.insert(*tool_call_id);
            }
            Event::ToolCallCompleted { tool_call_id, .. }
            | Event::ToolCallFailed { tool_call_id, .. } => {
                tools.remove(tool_call_id);
            }
            _ => {}
        }
    }
    tools
}

async fn route_live(
    message: EventSubscriptionMessage,
    deliveries: &mpsc::UnboundedSender<ClientDelivery>,
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    recovery: &Arc<RecoveryQueue>,
    tool_sessions: &mut HashMap<ToolCallId, SessionId>,
) {
    let session_id = match &message {
        EventSubscriptionMessage::Event { event } => {
            if let Event::ToolCallStarted { tool_call_id, .. } = &event.event {
                tool_sessions.insert(*tool_call_id, event.session_id);
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
    let _ = deliveries.send(publish);
    if let Some((full, session)) = recover {
        Client::schedule_recovery_queue(recovery, full, session);
    }
}

async fn route_output(
    delivery: ClientDelivery,
    deliveries: &mpsc::UnboundedSender<ClientDelivery>,
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
        let _ = deliveries.send(delivery);
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
    deliveries: &mpsc::UnboundedSender<ClientDelivery>,
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
    let _ = deliveries.send(ClientDelivery::ReplayEnd {
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
            if let Event::ToolCallStarted { tool_call_id, .. } = &event.event {
                tool_sessions.insert(*tool_call_id, event.session_id);
            }
        }
        let _ = deliveries.send(delivery);
    }
    if let Some(full) = recovery_requested.or(tail_has_gap.then_some(false)) {
        Client::schedule_recovery_queue(recovery, full, (!full).then_some(session_id));
    }
}

async fn release_expired_replays(
    subscriptions: &Arc<Mutex<HashMap<SessionId, Subscription>>>,
    deliveries: &mpsc::UnboundedSender<ClientDelivery>,
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

/// A `MessageStream` adapter for text WebSocket frames.
struct WebSocketClientStream {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

#[async_trait]
impl MessageStream for WebSocketClientStream {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        let text = match frame {
            MessageFrame::Text(text) => text,
            MessageFrame::Value(value) => {
                serde_json::to_string(&value).map_err(|_| TransportError::Closed)?
            }
        };
        self.socket
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return Ok(Some(MessageFrame::Text(text.to_string())));
                }
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                Some(Ok(_)) => continue,
                Some(Err(_)) => return Err(TransportError::Closed),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::*;
    use crate::state::StateStore;

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

    #[test]
    fn websocket_auth_uses_a_bearer_header_without_url_credentials() {
        let token = "A".repeat(43);
        let request =
            authenticated_request("ws://127.0.0.1:7419/ws", &token).expect("authenticated request");
        assert_eq!(request.uri().to_string(), "ws://127.0.0.1:7419/ws");
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some(format!("Bearer {token}").as_str())
        );
        assert!(!request.uri().to_string().contains(&token));
        assert!(authenticated_request("ws://127.0.0.1:7419/ws", "sentinel-secret").is_err());
    }

    fn event(session_id: SessionId, seq: u64) -> EventEnvelope {
        EventEnvelope {
            schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
            session_id,
            run_id: None,
            seq,
            timestamp: Timestamp::now(),
            event: Event::TextDelta {
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
                event: event(session_id, 2),
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
                event: event(session_id, 3),
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
        let mut store = StateStore::default();
        while let Some(delivery) = receiver.recv().await {
            let ended = matches!(&delivery, ClientDelivery::ReplayEnd { .. });
            assert!(!matches!(
                store.apply_delivery(delivery),
                crate::state::DeliveryOutcome::ReplayFailed { .. }
            ));
            if ended {
                break;
            }
        }
        task.await.expect("replay task");
        assert_eq!(store.sessions[&session_id].last_seq, 2_000);
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
                            cwd: "/workspace".into(),
                            profile: "primary".into(),
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

        let list = tokio::spawn({
            let client = client.clone();
            async move { client.list_agents(AgentListParams::default()).await }
        });
        let MessageFrame::Value(request) =
            tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
                .await
                .expect("agent.list request timeout")
                .expect("connection open")
        else {
            panic!("expected value request");
        };
        assert_eq!(request["method"], "agent.list");
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": { "agents": [] },
            })))
            .expect("agent.list response");
        assert!(
            list.await
                .expect("list task")
                .expect("agent.list result")
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
                    .connect_provider(ProviderConnectParams {
                        client_connect_id: "connect-test".into(),
                        provider_id: "test".into(),
                        catalog_revision: "catalog-test".into(),
                        credentials: cookie_agent_protocol::ProviderCredentials {
                            values: std::collections::BTreeMap::from([(
                                "API_KEY".into(),
                                "sentinel-secret".into(),
                            )]),
                        },
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
        let connect = client.connect_provider(ProviderConnectParams {
            client_connect_id: "unpolled".into(),
            provider_id: "test".into(),
            catalog_revision: "catalog-test".into(),
            credentials: cookie_agent_protocol::ProviderCredentials {
                values: std::collections::BTreeMap::from([(
                    "API_KEY".into(),
                    "sentinel-secret".into(),
                )]),
            },
        });
        drop(connect);
        assert!(PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed) > source_before);
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
                    event: event(session_id, seq),
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
        let started = EventEnvelope {
            schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
            session_id,
            run_id: None,
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::ToolCallStarted {
                tool_call_id: call_id,
                model_call_id: "model-call".into(),
                provider_item_id: None,
                tool: "bash".into(),
                arguments: serde_json::json!({}),
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
                event: event(session_id, 2),
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
