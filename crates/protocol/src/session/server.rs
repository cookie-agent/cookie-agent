use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroize as _;

use crate::{
    ApprovalListParams, ApprovalListResult, ApprovalRespondParams, ApprovalRespondResult,
    ClientHello, ClientRenameId, ErrorResponse, EventOrigin, EventSubscriptionMessage,
    EventsSubscribeParams, EventsSubscribeResult, JsonRpcError, JsonRpcId, JsonRpcVersion,
    McpAuthBeginParams, McpAuthBeginResult, McpAuthCancelParams, McpAuthCancelResult,
    McpAuthCompleteParams, McpAuthCompleteResult, McpServerAddParams, McpServerEditParams,
    McpServerListParams, McpServerListResult, McpServerMutationResult, McpServerNameParams,
    McpServerPersistParams, McpServerReconnectParams, McpServerSetEnabledParams, MessageFrame,
    Notification, NotificationRef, ProviderConnectParams, ProviderConnectResult,
    ProviderDisconnectParams, ProviderDisconnectResult, Request, Response, RunCancelParams,
    RunCancelResult, RunStartParams, RunStartResult, RunSteerParams, RunSteerResult,
    RuntimeSnapshotGetParams, RuntimeSnapshotResult, ServerHello, SessionChildrenParams,
    SessionChildrenResult, SessionCompactParams, SessionCompactResult, SessionCreateParams,
    SessionCreateResult, SessionForkParams, SessionForkResult, SessionGetParams, SessionGetResult,
    SessionGoalGetParams, SessionGoalGetResult, SessionGoalLifecycleParams,
    SessionGoalLifecycleResult, SessionGoalSetParams, SessionGoalSetResult, SessionId,
    SessionListParams, SessionListResult, SessionPermissionClearParams, SessionPermissionGetParams,
    SessionPermissionGetResult, SessionPermissionMutationResult, SessionPermissionPreviewParams,
    SessionPermissionPreviewResult, SessionPermissionSetParams, SessionProducersParams,
    SessionProducersResult, SessionRenameChange, SessionRenameParams, SessionRenameResult,
    SessionResumeParams, SessionResumeResult, SessionRevertParams, SessionRevertResult,
    SessionSetPermissionModeParams, SessionSetPermissionModeResult, SessionTitle,
    SessionTreeParams, SessionTreeResult, SessionTreeUsageResult, SessionUsageParams,
    SessionUsageResult, SkillsGetParams, SkillsGetResult, SkillsListParams, SkillsListResult,
    Transport, TransportError,
};

const OUTBOUND_QUEUE_CAPACITY: usize = 512;
const MAX_RAW_RENAME_PARAMS_BYTES: usize = 4 * 1024;

/// Requests one connection may have in flight at once. At the limit the
/// server stops reading until one finishes, which backpressures the client.
const MAX_IN_FLIGHT_REQUESTS: usize = 64;

/// Source of [`ServerContext::connection_id`].
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

/// Per-connection facilities available to a server implementation.
#[derive(Clone)]
pub struct ServerContext {
    connection_id: u64,
    /// The origin derived from the handshake's client identity.
    client_origin: Arc<std::sync::OnceLock<EventOrigin>>,
    /// The client's protocol version, from the handshake.
    client_version: Arc<std::sync::OnceLock<crate::SemVer>>,
    /// Serialized frames, written to the transport in queue order.
    notifications: mpsc::Sender<String>,
    shutdown: CancellationToken,
    subscribed_sessions: Arc<Mutex<HashSet<SessionId>>>,
    /// The task forwarding each session's live tail on this connection.
    session_tails: Arc<Mutex<HashMap<SessionId, tokio::task::JoinHandle<()>>>>,
    /// Opens once the response of the request this context was handed to has
    /// been queued. `None` for connection-scoped contexts.
    response_sent: Option<CancellationToken>,
}

impl ServerContext {
    fn new(notifications: mpsc::Sender<String>, shutdown: CancellationToken) -> Self {
        Self {
            connection_id: NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed),
            client_origin: Arc::new(std::sync::OnceLock::new()),
            client_version: Arc::new(std::sync::OnceLock::new()),
            notifications,
            shutdown,
            subscribed_sessions: Arc::new(Mutex::new(HashSet::new())),
            session_tails: Arc::new(Mutex::new(HashMap::new())),
            response_sent: None,
        }
    }

    /// This connection's identity, unique within the process. A server
    /// keys the live tail it registers for a session by it, so that a
    /// connection holds at most one tail per session.
    #[must_use]
    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    /// The origin stamped on events this connection's requests cause,
    /// derived from the client identity sent in the handshake. Requests run
    /// only after the handshake, so it is always set when a request sees it.
    #[must_use]
    pub fn client_origin(&self) -> EventOrigin {
        self.client_origin
            .get()
            .cloned()
            .expect("requests run only after the handshake set the client origin")
    }

    /// Records the handshake identity and version. A repeated handshake
    /// keeps the first.
    fn hello(&self, hello: &ClientHello, engine_version: String) -> ServerHello {
        let origin = self
            .client_origin
            .get_or_init(|| hello.client.origin())
            .clone();
        let _ = self.client_version.set(hello.protocol_version);
        ServerHello {
            protocol_version: crate::CLIENT_PROTOCOL_VERSION,
            engine_version,
            origin,
            machine_id: crate::paths::machine_id(),
            features: Vec::new(),
        }
    }

    /// What this connection's client receives of display blocks: the
    /// kinds its version includes (`docs/site/specs/compatibility-v1.md`
    /// section 12.4).
    #[must_use]
    pub fn display_audience(&self) -> crate::DisplayAudience {
        crate::DisplayAudience::new(self.client_version())
    }

    /// The client protocol version the client sent in its handshake: the
    /// daemon sends only what this version includes. Requests run only after
    /// the handshake, so it is set whenever a request sees it.
    #[must_use]
    pub fn client_version(&self) -> crate::SemVer {
        self.client_version
            .get()
            .copied()
            .unwrap_or(crate::CLIENT_PROTOCOL_VERSION)
    }

    /// Forward a session's live tail to this connection as
    /// `events.subscription` notifications until the tail or the connection
    /// ends.
    ///
    /// The connection holds one tail per session, so the producer must have
    /// closed any tail this one replaces (see [`Self::connection_id`]): the
    /// messages that tail still holds are forwarded first, then this one's,
    /// keeping the connection in publication order. Two tails of one session
    /// would each carry every message, and live-only output, which has no
    /// sequence to de-duplicate by, would reach the client twice.
    pub fn forward_session_tail(
        &self,
        session_id: SessionId,
        mut receiver: mpsc::Receiver<EventSubscriptionMessage>,
    ) {
        let mut tails = self
            .session_tails
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let replaced = tails.remove(&session_id);
        let context = self.clone();
        let forwarder = tokio::spawn(async move {
            if let Some(replaced) = replaced {
                let _ = replaced.await;
            }
            let shutdown = context.shutdown();
            loop {
                let message = tokio::select! {
                    () = shutdown.cancelled() => return,
                    message = receiver.recv() => match message {
                        Some(message) => message,
                        None => return,
                    },
                };
                let message = context.display_audience().message(message);
                if context
                    .notify("events.subscription", &message)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        tails.insert(session_id, forwarder);
    }

    /// Emit one JSON-RPC notification in connection order.
    ///
    /// A notification from work a request started (for example a live tail
    /// registered by `events.subscribe`) is held until that request's
    /// response is queued, so the client never sees the tail before the
    /// snapshot it continues.
    ///
    /// The notification is serialized once, straight to its frame text.
    pub async fn notify<T: Serialize + ?Sized>(
        &self,
        method: &str,
        params: &T,
    ) -> Result<(), TransportError> {
        let notification = NotificationRef::new(method, params).to_text()?;
        if let Some(response_sent) = &self.response_sent {
            tokio::select! {
                () = self.shutdown.cancelled() => return Err(TransportError::Closed),
                () = response_sent.cancelled() => {}
            }
        }
        tokio::select! {
            () = self.shutdown.cancelled() => Err(TransportError::Closed),
            result = self.notifications.send(notification) => result.map_err(|_| TransportError::Closed),
        }
    }

    fn for_request(&self) -> (Self, CancellationToken) {
        let response_sent = CancellationToken::new();
        (
            Self {
                response_sent: Some(response_sent.clone()),
                ..self.clone()
            },
            response_sent,
        )
    }

    /// A cancellation token scoped to this protocol connection.
    #[must_use]
    pub fn shutdown(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Marks a session as authorized for connection-scoped live notifications.
    pub fn register_session_subscription(&self, session_id: SessionId) {
        self.subscribed_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id);
    }

    /// Stops this connection's live tail for a session: no further
    /// `events.subscription` or plugin notification for it is sent. Returns
    /// whether the connection was subscribed.
    pub fn unregister_session_subscription(&self, session_id: SessionId) -> bool {
        if let Some(forwarder) = self
            .session_tails
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session_id)
        {
            forwarder.abort();
        }
        self.subscribed_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session_id)
    }

    /// The sessions this connection is subscribed to.
    #[must_use]
    pub fn subscribed_sessions(&self) -> Vec<SessionId> {
        self.subscribed_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .copied()
            .collect()
    }

    /// Returns whether this connection has successfully subscribed to the session.
    #[must_use]
    pub fn is_session_subscribed(&self, session_id: SessionId) -> bool {
        self.subscribed_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&session_id)
    }
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
#[must_use]
pub fn test_server_context() -> (ServerContext, mpsc::Receiver<String>) {
    let (notifications, receiver) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
    (
        ServerContext::new(notifications, CancellationToken::new()),
        receiver,
    )
}

/// Server end of the protocol contract after session-level validation.
#[async_trait]
pub trait ServerProtocol: Send + Sync + 'static {
    /// Called after the handshake response has been sent.
    async fn connected(&self, _context: ServerContext) {}

    /// The daemon's own version, which the handshake reports.
    fn engine_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    async fn create_session(
        &self,
        params: SessionCreateParams,
    ) -> Result<SessionCreateResult, ServerFault>;
    async fn list_sessions(
        &self,
        params: SessionListParams,
    ) -> Result<SessionListResult, ServerFault>;
    async fn get_session(&self, params: SessionGetParams) -> Result<SessionGetResult, ServerFault>;
    /// Returns the root session's current goal, if any.
    ///
    /// Implementations must reject child sessions. Goal access is root-only and
    /// is not a configurable permission surface.
    async fn get_session_goal(
        &self,
        params: SessionGoalGetParams,
    ) -> Result<SessionGoalGetResult, ServerFault>;
    /// Activates a user-owned goal on a root session.
    ///
    /// Implementations must reject child sessions and must reject activation
    /// while the current goal is active or paused. This user RPC must not be
    /// reused as a model/tool mutation path.
    async fn set_session_goal(
        &self,
        params: SessionGoalSetParams,
        context: &ServerContext,
    ) -> Result<SessionGoalSetResult, ServerFault>;
    /// Applies a user-owned lifecycle transition to a root session goal.
    ///
    /// Implementations must reject child sessions, terminal goals, mismatched
    /// goal IDs, and stale `expected_revision` values. This user RPC must not be
    /// exposed to model tools.
    async fn change_session_goal_lifecycle(
        &self,
        params: SessionGoalLifecycleParams,
        context: &ServerContext,
    ) -> Result<SessionGoalLifecycleResult, ServerFault>;
    /// Returns the runtime-only producer registry for the requested session.
    async fn session_producers(
        &self,
        params: SessionProducersParams,
    ) -> Result<SessionProducersResult, ServerFault>;
    async fn session_usage(
        &self,
        params: SessionUsageParams,
    ) -> Result<SessionUsageResult, ServerFault>;
    async fn session_tree_usage(
        &self,
        params: SessionUsageParams,
    ) -> Result<SessionTreeUsageResult, ServerFault>;
    async fn session_children(
        &self,
        params: SessionChildrenParams,
    ) -> Result<SessionChildrenResult, ServerFault>;
    async fn session_tree(
        &self,
        params: SessionTreeParams,
    ) -> Result<SessionTreeResult, ServerFault>;
    async fn resume_session(
        &self,
        params: SessionResumeParams,
    ) -> Result<SessionResumeResult, ServerFault>;
    async fn rename_session(
        &self,
        params: SessionRenameParams,
        context: &ServerContext,
    ) -> Result<SessionRenameResult, ServerFault>;
    async fn set_permission_mode(
        &self,
        params: SessionSetPermissionModeParams,
        context: &ServerContext,
    ) -> Result<SessionSetPermissionModeResult, ServerFault>;
    async fn get_session_permissions(
        &self,
        params: SessionPermissionGetParams,
    ) -> Result<SessionPermissionGetResult, ServerFault>;
    async fn set_session_permission(
        &self,
        params: SessionPermissionSetParams,
        context: &ServerContext,
    ) -> Result<SessionPermissionMutationResult, ServerFault>;
    async fn clear_session_permission(
        &self,
        params: SessionPermissionClearParams,
        context: &ServerContext,
    ) -> Result<SessionPermissionMutationResult, ServerFault>;
    async fn preview_session_permissions(
        &self,
        params: SessionPermissionPreviewParams,
    ) -> Result<SessionPermissionPreviewResult, ServerFault>;
    async fn list_skills(&self, params: SkillsListParams) -> Result<SkillsListResult, ServerFault>;
    async fn get_skill(&self, params: SkillsGetParams) -> Result<SkillsGetResult, ServerFault>;
    async fn compact_session(
        &self,
        params: SessionCompactParams,
        context: &ServerContext,
    ) -> Result<SessionCompactResult, ServerFault>;
    async fn cancel_compaction(
        &self,
        params: crate::SessionCompactCancelParams,
        context: &ServerContext,
    ) -> Result<crate::SessionCompactCancelResult, ServerFault>;
    async fn revert_session(
        &self,
        params: SessionRevertParams,
        context: &ServerContext,
    ) -> Result<SessionRevertResult, ServerFault>;
    async fn fork_session(
        &self,
        params: SessionForkParams,
        context: &ServerContext,
    ) -> Result<SessionForkResult, ServerFault>;
    async fn start_run(
        &self,
        params: RunStartParams,
        context: &ServerContext,
    ) -> Result<RunStartResult, ServerFault>;
    async fn steer_run(
        &self,
        params: RunSteerParams,
        context: &ServerContext,
    ) -> Result<RunSteerResult, ServerFault>;
    async fn select_model(
        &self,
        params: crate::SessionSelectModelParams,
        context: &ServerContext,
    ) -> Result<crate::SessionSelectModelResult, ServerFault>;
    async fn unsubscribe_events(
        &self,
        params: crate::EventsUnsubscribeParams,
        context: &ServerContext,
    ) -> Result<crate::EventsUnsubscribeResult, ServerFault>;
    async fn submit_input(
        &self,
        params: crate::SessionSubmitInputParams,
        context: &ServerContext,
    ) -> Result<crate::SessionSubmitInputResult, ServerFault>;
    async fn recall_input(
        &self,
        params: crate::SessionRecallInputParams,
        context: &ServerContext,
    ) -> Result<crate::SessionRecallInputResult, ServerFault>;
    async fn cancel_run(
        &self,
        params: RunCancelParams,
        context: &ServerContext,
    ) -> Result<RunCancelResult, ServerFault>;
    async fn subscribe_events(
        &self,
        params: EventsSubscribeParams,
        context: &ServerContext,
    ) -> Result<EventsSubscribeResult, ServerFault>;
    async fn respond_approval(
        &self,
        params: ApprovalRespondParams,
        context: &ServerContext,
    ) -> Result<ApprovalRespondResult, ServerFault>;
    async fn list_approvals(
        &self,
        params: ApprovalListParams,
    ) -> Result<ApprovalListResult, ServerFault>;
    async fn begin_mcp_auth(
        &self,
        params: McpAuthBeginParams,
    ) -> Result<McpAuthBeginResult, ServerFault>;
    async fn complete_mcp_auth(
        &self,
        params: McpAuthCompleteParams,
    ) -> Result<McpAuthCompleteResult, ServerFault>;
    async fn cancel_mcp_auth(
        &self,
        params: McpAuthCancelParams,
    ) -> Result<McpAuthCancelResult, ServerFault>;
    /// Attaches a runtime to a session with no active run.
    async fn attach_runtime(
        &self,
        params: crate::SessionRuntimeAttachParams,
        context: &ServerContext,
    ) -> Result<crate::SessionRuntimeAttachResult, ServerFault>;
    /// Detaches a runtime from a session with no active run.
    async fn detach_runtime(
        &self,
        params: crate::SessionRuntimeDetachParams,
        context: &ServerContext,
    ) -> Result<crate::SessionRuntimeDetachResult, ServerFault>;
    /// Lists runtime templates and instances.
    async fn list_runtimes(
        &self,
        params: crate::RuntimesListParams,
    ) -> Result<crate::RuntimesListResult, ServerFault>;
    /// Retries a runtime instance now.
    async fn reconnect_runtime(
        &self,
        params: crate::RuntimesReconnectParams,
    ) -> Result<crate::RuntimesReconnectResult, ServerFault>;
    /// Lists the machines.
    async fn list_machines(
        &self,
        params: crate::MachinesListParams,
    ) -> Result<crate::MachinesListResult, ServerFault>;
    /// Renames a machine and moves its memory folder.
    async fn rename_machine(
        &self,
        params: crate::MachinesRenameParams,
    ) -> Result<crate::MachinesListResult, ServerFault>;
    /// Pins a runtime ID to a machine.
    async fn assign_machine(
        &self,
        params: crate::MachinesAssignParams,
    ) -> Result<crate::MachinesListResult, ServerFault>;
    /// Removes a runtime's pin.
    async fn unassign_machine(
        &self,
        params: crate::MachinesUnassignParams,
    ) -> Result<crate::MachinesListResult, ServerFault>;
    /// Merges one machine into another.
    async fn merge_machines(
        &self,
        params: crate::MachinesMergeParams,
    ) -> Result<crate::MachinesListResult, ServerFault>;
    /// Removes an unused machine.
    async fn forget_machine(
        &self,
        params: crate::MachinesForgetParams,
    ) -> Result<crate::MachinesListResult, ServerFault>;
    /// Lists the configured plugins.
    async fn list_plugins(
        &self,
        params: crate::PluginsListParams,
    ) -> Result<crate::PluginsListResult, ServerFault>;
    /// Starts a plugin again now.
    async fn restart_plugin(
        &self,
        params: crate::PluginsRestartParams,
    ) -> Result<crate::PluginsRestartResult, ServerFault>;
    /// The configuration documents on disk.
    async fn get_config(
        &self,
        params: crate::ConfigGetParams,
    ) -> Result<crate::ConfigGetResult, ServerFault>;
    /// Changes the configuration.
    async fn apply_config(
        &self,
        params: crate::ConfigApplyParams,
    ) -> Result<crate::ConfigApplyResult, ServerFault>;
    /// Re-reads and applies the configuration.
    async fn reload_config(
        &self,
        params: crate::ConfigReloadParams,
    ) -> Result<crate::ConfigReloadResult, ServerFault>;
    /// Moves an executing call to the background.
    async fn background_tool(
        &self,
        params: crate::RunToolBackgroundParams,
        context: &ServerContext,
    ) -> Result<crate::RunToolBackgroundResult, ServerFault>;
    async fn list_background(
        &self,
        params: crate::SessionBackgroundListParams,
    ) -> Result<crate::SessionBackgroundListResult, ServerFault>;
    /// Stops a background call as the user.
    async fn stop_background(
        &self,
        params: crate::SessionBackgroundStopParams,
        context: &ServerContext,
    ) -> Result<crate::SessionBackgroundStopResult, ServerFault>;
    async fn list_mcp_servers(
        &self,
        params: McpServerListParams,
    ) -> Result<McpServerListResult, ServerFault>;
    async fn add_mcp_server(
        &self,
        params: McpServerAddParams,
    ) -> Result<McpServerMutationResult, ServerFault>;
    async fn edit_mcp_server(
        &self,
        params: McpServerEditParams,
    ) -> Result<McpServerMutationResult, ServerFault>;
    async fn remove_mcp_server(
        &self,
        params: McpServerNameParams,
    ) -> Result<McpServerMutationResult, ServerFault>;
    async fn set_mcp_server_enabled(
        &self,
        params: McpServerSetEnabledParams,
    ) -> Result<McpServerMutationResult, ServerFault>;
    async fn reconnect_mcp_server(
        &self,
        params: McpServerReconnectParams,
    ) -> Result<McpServerMutationResult, ServerFault>;
    async fn persist_mcp_server(
        &self,
        params: McpServerPersistParams,
    ) -> Result<McpServerMutationResult, ServerFault>;
    /// Starts an upload into the store of a session's tree. It belongs to
    /// this connection, which may have a bounded number in progress, and is
    /// dropped when the connection closes.
    async fn begin_upload(
        &self,
        params: crate::ArtifactUploadBeginParams,
        context: &ServerContext,
    ) -> Result<crate::ArtifactUploadBeginResult, ServerFault>;
    /// Appends the next chunk of one of this connection's uploads.
    async fn upload_chunk(
        &self,
        params: crate::ArtifactUploadChunkParams,
        context: &ServerContext,
    ) -> Result<crate::ArtifactUploadChunkResult, ServerFault>;
    /// Checks, validates, and stores one of this connection's uploads.
    async fn finish_upload(
        &self,
        params: crate::ArtifactUploadFinishParams,
        context: &ServerContext,
    ) -> Result<crate::ArtifactUploadFinishResult, ServerFault>;
    /// Drops one of this connection's uploads.
    async fn cancel_upload(
        &self,
        params: crate::ArtifactUploadCancelParams,
        context: &ServerContext,
    ) -> Result<crate::ArtifactUploadCancelResult, ServerFault>;
    /// Reads a range of any artifact in the store of a session's tree.
    async fn read_artifact(
        &self,
        params: crate::ArtifactReadParams,
    ) -> Result<crate::ArtifactReadResult, ServerFault>;
    async fn runtime_snapshot(
        &self,
        params: RuntimeSnapshotGetParams,
    ) -> Result<RuntimeSnapshotResult, ServerFault>;
    async fn connect_provider(
        &self,
        params: ProviderConnectParams,
    ) -> Result<ProviderConnectResult, ServerFault>;
    async fn disconnect_provider(
        &self,
        params: ProviderDisconnectParams,
    ) -> Result<ProviderDisconnectResult, ServerFault>;
}

/// JSON-RPC failure returned by a [`ServerProtocol`] implementation.
#[derive(Debug)]
pub struct ServerFault {
    pub code: i32,
    pub message: std::borrow::Cow<'static, str>,
    pub data: Option<Value>,
}

impl ServerFault {
    #[must_use]
    pub const fn new(code: i32, message: &'static str, data: Option<Value>) -> Self {
        Self {
            code,
            message: std::borrow::Cow::Borrowed(message),
            data,
        }
    }

    /// The fault that answers with `error`, such as one built from the
    /// error registry ([`crate::errors`]).
    #[must_use]
    pub fn from_error(error: JsonRpcError) -> Self {
        Self {
            code: error.code,
            message: std::borrow::Cow::Owned(error.message),
            data: error.data,
        }
    }

    /// `method_not_found`, naming `method`.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::from_error(
            crate::errors::UNKNOWN_METHOD
                .error()
                .field("method", method)
                .build(),
        )
    }

    /// `invalid_params`, with what was wrong as its detail.
    #[must_use]
    pub fn invalid_params(detail: impl Into<String>) -> Self {
        Self::from_error(crate::errors::BAD_PARAMS.error().detail(detail).build())
    }

    #[must_use]
    pub fn internal() -> Self {
        Self::from_error(crate::errors::INTERNAL.error().build())
    }

    /// An error of the registry's `code` with no fields, and `detail`.
    #[must_use]
    pub fn registry(code: &'static crate::errors::ErrorCode, detail: impl Into<String>) -> Self {
        Self::from_error(code.error().detail(detail).build())
    }
}

struct ConnectionShutdown(CancellationToken);

impl Drop for ConnectionShutdown {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// How a connection proves it may use the daemon.
#[derive(Clone)]
pub enum Authentication {
    /// The transport already proved it: an in-process connection, or a
    /// WebSocket upgrade that presented the token in `Authorization`.
    Authenticated,
    /// The connection must send this token in its `handshake`. Until it
    /// does, only `handshake` is answered, and a connection that has not
    /// authenticated within [`AUTHENTICATION_DEADLINE`] is closed.
    Token(Arc<zeroize::Zeroizing<String>>),
}

impl std::fmt::Debug for Authentication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authenticated => formatter.write_str("Authenticated"),
            Self::Token(_) => formatter.write_str("Token(<redacted>)"),
        }
    }
}

/// How long a connection that must authenticate in its handshake may take.
pub const AUTHENTICATION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Run one complete server-side protocol session over a transport.
///
/// The connection first authenticates and completes its handshake: before
/// it has authenticated only `handshake` is answered (anything else gets
/// `unauthenticated`), a wrong token is answered with `unauthenticated` and
/// closes the connection, and so does the [`AUTHENTICATION_DEADLINE`]
/// passing. After the handshake, requests run concurrently and each
/// response is written as soon as it is ready; clients match responses by
/// id. A slow request, such as a large replay, never holds up the ones
/// behind it.
pub async fn serve<T, S>(
    server: Arc<S>,
    transport: T,
    shutdown: CancellationToken,
    authentication: Authentication,
) -> Result<(), TransportError>
where
    T: Transport,
    S: ServerProtocol,
{
    serve_within(
        server,
        transport,
        shutdown,
        authentication,
        AUTHENTICATION_DEADLINE,
    )
    .await
}

async fn serve_within<T, S>(
    server: Arc<S>,
    mut transport: T,
    shutdown: CancellationToken,
    authentication: Authentication,
    deadline: std::time::Duration,
) -> Result<(), TransportError>
where
    T: Transport,
    S: ServerProtocol,
{
    let (outbound, mut outbound_rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
    let context = ServerContext::new(outbound, shutdown);
    let _guard = ConnectionShutdown(context.shutdown());
    let in_flight = Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS));
    let mut permit = None;
    let mut handshaken = false;
    let mut authentication = authentication;
    let unauthenticated_until = tokio::time::sleep(deadline);
    tokio::pin!(unauthenticated_until);
    loop {
        let authenticated = matches!(authentication, Authentication::Authenticated);
        tokio::select! {
            () = context.shutdown.cancelled() => return Ok(()),
            () = &mut unauthenticated_until, if !authenticated => return Ok(()),
            acquired = in_flight.clone().acquire_owned(), if permit.is_none() => {
                permit = Some(acquired.expect("the in-flight semaphore is never closed"));
            }
            incoming = transport.recv(), if permit.is_some() => {
                let Some(frame) = incoming? else { return Ok(()); };
                let incoming = match parse_incoming(frame).and_then(classify_incoming) {
                    Ok(incoming) => incoming,
                    Err(fault) => {
                        transport.send(MessageFrame::Text(error_response(None, fault)?)).await?;
                        continue;
                    }
                };
                if !handshaken {
                    let Incoming::Request { id, method, params } = incoming else {
                        continue;
                    };
                    let (response, close) = if method != "handshake" {
                        let fault = if authenticated {
                            handshake_required()
                        } else {
                            unauthenticated()
                        };
                        (error_response(Some(id), fault)?, false)
                    } else {
                        match decode_handshake(params) {
                            Ok(hello) => match authenticate(&authentication, &hello) {
                                Ok(()) => {
                                    authentication = Authentication::Authenticated;
                                    handshaken = true;
                                    let hello =
                                        server_hello(&context, &hello, server.engine_version());
                                    (success_response(&id, &hello)?, false)
                                }
                                Err(fault) => (error_response(Some(id), fault)?, true),
                            },
                            Err(fault) => (error_response(Some(id), fault)?, false),
                        }
                    };
                    transport.send(MessageFrame::Text(response)).await?;
                    if close {
                        return Ok(());
                    }
                    if handshaken {
                        server.connected(context.clone()).await;
                    }
                    continue;
                }
                let permit = permit.take();
                let server = server.clone();
                let (request_context, response_sent) = context.for_request();
                tokio::spawn(async move {
                    let _permit = permit;
                    // Opens the notification gate on every exit, a panic included.
                    let _response_sent = response_sent.drop_guard();
                    let Some(response) = handle(server.as_ref(), incoming, &request_context).await
                    else {
                        return;
                    };
                    tokio::select! {
                        () = request_context.shutdown.cancelled() => {}
                        _ = request_context.notifications.send(response) => {}
                    }
                });
            }
            Some(frame) = outbound_rx.recv() => {
                transport.send(MessageFrame::Text(frame)).await?;
            }
        }
    }
}

/// Checks a handshake's token where the transport did not authenticate the
/// connection. A connection the transport authenticated may send one too;
/// it is not needed and not checked.
fn authenticate(authentication: &Authentication, hello: &ClientHello) -> Result<(), ServerFault> {
    match (authentication, &hello.token) {
        (Authentication::Authenticated, _) => Ok(()),
        (Authentication::Token(expected), Some(token))
            if token_matches(token.as_str(), expected) =>
        {
            Ok(())
        }
        (Authentication::Token(_), _) => Err(unauthenticated()),
    }
}

/// Compares in time that depends only on the lengths.
fn token_matches(token: &str, expected: &str) -> bool {
    token.len() == expected.len()
        && token
            .bytes()
            .zip(expected.bytes())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
}

/// Runs one post-handshake message and builds its response, if it has one.
async fn handle<S: ServerProtocol>(
    server: &S,
    incoming: Incoming,
    context: &ServerContext,
) -> Option<String> {
    match incoming {
        Incoming::Request { id, method, params } => {
            let result = if method == "handshake" {
                decode_handshake(params)
                    .map(|hello| server_hello(context, &hello, server.engine_version()))
            } else {
                dispatch(server, &method, params, context, true).await
            };
            let response = match result {
                Ok(result) => success_response(&id, &result),
                Err(fault) => error_response(Some(id.clone()), fault),
            };
            Some(response.unwrap_or_else(|_| {
                error_response(Some(id), ServerFault::internal())
                    .expect("an error response without data serializes")
            }))
        }
        Incoming::Notification { method, params } => {
            if method == "handshake" {
                let _ = decode_handshake(params);
            } else {
                let _ = dispatch(server, &method, params, context, false).await;
            }
            None
        }
    }
}

fn server_hello(
    context: &ServerContext,
    hello: &ClientHello,
    engine_version: String,
) -> Box<RawValue> {
    serde_json::value::to_raw_value(&context.hello(hello, engine_version))
        .expect("server hello serializes")
}

async fn dispatch<S: ServerProtocol>(
    server: &S,
    method: &str,
    params: Option<Value>,
    context: &ServerContext,
    has_request_id: bool,
) -> Result<Box<RawValue>, ServerFault> {
    match method {
        "session.create" => value(server.create_session(decode(params)?).await?),
        "session.list" => value(server.list_sessions(decode_default(params)?).await?),
        "session.get" => value(server.get_session(decode(params)?).await?),
        crate::SESSION_GOAL_GET_METHOD => value(server.get_session_goal(decode(params)?).await?),
        crate::SESSION_GOAL_SET_METHOD => {
            value(server.set_session_goal(decode(params)?, context).await?)
        }
        crate::SESSION_GOAL_LIFECYCLE_METHOD => value(
            server
                .change_session_goal_lifecycle(decode(params)?, context)
                .await?,
        ),
        crate::SESSION_PRODUCERS_METHOD => value(server.session_producers(decode(params)?).await?),
        "session.usage" => value(server.session_usage(decode(params)?).await?),
        "session.tree_usage" => value(server.session_tree_usage(decode(params)?).await?),
        "session.children" => value(server.session_children(decode(params)?).await?),
        "session.tree" => value(server.session_tree(decode(params)?).await?),
        "session.resume" => value(server.resume_session(decode(params)?).await?),
        "session.rename" => value(
            server
                .rename_session(decode_rename(params)?, context)
                .await?,
        ),
        "session.set_permission_mode" => {
            value(server.set_permission_mode(decode(params)?, context).await?)
        }
        "session.permission.get" => value(server.get_session_permissions(decode(params)?).await?),
        "session.permission.set" => value(
            server
                .set_session_permission(decode(params)?, context)
                .await?,
        ),
        "session.permission.clear" => value(
            server
                .clear_session_permission(decode(params)?, context)
                .await?,
        ),
        "session.permission.preview" => {
            value(server.preview_session_permissions(decode(params)?).await?)
        }
        "skills.list" => value(server.list_skills(decode(params)?).await?),
        "skills.get" => value(server.get_skill(decode(params)?).await?),
        "session.compact" => value(server.compact_session(decode(params)?, context).await?),
        "session.revert" => value(server.revert_session(decode(params)?, context).await?),
        crate::SESSION_COMPACT_CANCEL_METHOD => {
            value(server.cancel_compaction(decode(params)?, context).await?)
        }
        "session.fork" => value(server.fork_session(decode(params)?, context).await?),
        "run.start" => value(server.start_run(decode(params)?, context).await?),
        "run.steer" => value(server.steer_run(decode(params)?, context).await?),
        crate::SESSION_SELECT_MODEL_METHOD => {
            value(server.select_model(decode(params)?, context).await?)
        }
        crate::EVENTS_UNSUBSCRIBE_METHOD => {
            value(server.unsubscribe_events(decode(params)?, context).await?)
        }
        crate::SESSION_SUBMIT_INPUT_METHOD => {
            value(server.submit_input(decode(params)?, context).await?)
        }
        crate::SESSION_RECALL_INPUT_METHOD => {
            value(server.recall_input(decode(params)?, context).await?)
        }
        "run.cancel" => value(server.cancel_run(decode(params)?, context).await?),
        "events.subscribe" => {
            let mut page = server.subscribe_events(decode(params)?, context).await?;
            let audience = context.display_audience();
            for event in &mut page.events {
                *event = audience.stored(event);
            }
            value(page)
        }
        "approval.respond" => value(server.respond_approval(decode(params)?, context).await?),
        "approval.list" => value(server.list_approvals(decode(params)?).await?),
        "mcp.auth.begin" => value(server.begin_mcp_auth(decode(params)?).await?),
        "mcp.auth.complete" => value(server.complete_mcp_auth(decode(params)?).await?),
        "mcp.auth.cancel" => value(server.cancel_mcp_auth(decode(params)?).await?),
        crate::SESSION_RUNTIME_ATTACH_METHOD => {
            value(server.attach_runtime(decode(params)?, context).await?)
        }
        crate::SESSION_RUNTIME_DETACH_METHOD => {
            value(server.detach_runtime(decode(params)?, context).await?)
        }
        crate::RUNTIMES_LIST_METHOD => value(server.list_runtimes(decode_default(params)?).await?),
        crate::RUNTIMES_RECONNECT_METHOD => value(server.reconnect_runtime(decode(params)?).await?),
        crate::MACHINES_LIST_METHOD => value(server.list_machines(decode_default(params)?).await?),
        crate::MACHINES_RENAME_METHOD => value(server.rename_machine(decode(params)?).await?),
        crate::MACHINES_ASSIGN_METHOD => value(server.assign_machine(decode(params)?).await?),
        crate::MACHINES_UNASSIGN_METHOD => value(server.unassign_machine(decode(params)?).await?),
        crate::MACHINES_MERGE_METHOD => value(server.merge_machines(decode(params)?).await?),
        crate::MACHINES_FORGET_METHOD => value(server.forget_machine(decode(params)?).await?),
        crate::PLUGINS_LIST_METHOD => value(server.list_plugins(decode_default(params)?).await?),
        crate::PLUGINS_RESTART_METHOD => value(server.restart_plugin(decode(params)?).await?),
        crate::CONFIG_RELOAD_METHOD => value(server.reload_config(decode_default(params)?).await?),
        crate::CONFIG_GET_METHOD => value(server.get_config(decode_default(params)?).await?),
        crate::CONFIG_APPLY_METHOD => value(server.apply_config(decode(params)?).await?),
        crate::RUN_TOOL_BACKGROUND_METHOD => {
            value(server.background_tool(decode(params)?, context).await?)
        }
        crate::SESSION_BACKGROUND_LIST_METHOD => {
            value(server.list_background(decode(params)?).await?)
        }
        crate::SESSION_BACKGROUND_STOP_METHOD => {
            value(server.stop_background(decode(params)?, context).await?)
        }
        "mcp.server.list" => value(server.list_mcp_servers(decode_default(params)?).await?),
        "mcp.server.add" => value(server.add_mcp_server(decode(params)?).await?),
        "mcp.server.edit" => value(server.edit_mcp_server(decode(params)?).await?),
        "mcp.server.remove" => value(server.remove_mcp_server(decode(params)?).await?),
        "mcp.server.set_enabled" => value(server.set_mcp_server_enabled(decode(params)?).await?),
        "mcp.server.reconnect" => value(server.reconnect_mcp_server(decode(params)?).await?),
        "mcp.server.persist" => value(server.persist_mcp_server(decode(params)?).await?),
        crate::ARTIFACT_UPLOAD_BEGIN_METHOD => {
            value(server.begin_upload(decode(params)?, context).await?)
        }
        crate::ARTIFACT_UPLOAD_CHUNK_METHOD => {
            value(server.upload_chunk(decode(params)?, context).await?)
        }
        crate::ARTIFACT_UPLOAD_FINISH_METHOD => {
            value(server.finish_upload(decode(params)?, context).await?)
        }
        crate::ARTIFACT_UPLOAD_CANCEL_METHOD => {
            value(server.cancel_upload(decode(params)?, context).await?)
        }
        crate::ARTIFACT_READ_METHOD => value(server.read_artifact(decode(params)?).await?),
        crate::RUNTIME_SNAPSHOT_GET_METHOD if has_request_id => {
            value(server.runtime_snapshot(decode_default(params)?).await?)
        }
        crate::PROVIDER_CONNECT_METHOD if has_request_id => {
            value(server.connect_provider(decode(params)?).await?)
        }
        crate::PROVIDER_DISCONNECT_METHOD if has_request_id => {
            value(server.disconnect_provider(decode(params)?).await?)
        }
        crate::RUNTIME_SNAPSHOT_GET_METHOD
        | crate::PROVIDER_CONNECT_METHOD
        | crate::PROVIDER_DISCONNECT_METHOD => Err(ServerFault::from_error(
            crate::errors::REQUEST_ID_REQUIRED
                .error()
                .field("method", method)
                .build(),
        )),
        _ => Err(ServerFault::method_not_found(method)),
    }
}

/// Params are commands: a member the method's schema does not list is
/// refused with its JSON path, even inside a type that also flows as
/// information and ignores unknown members there ([`crate::strict`]).
fn decode<T>(params: Option<Value>) -> Result<T, ServerFault>
where
    T: serde::de::DeserializeOwned + schemars::JsonSchema + 'static,
{
    crate::strict::decode_command(params.unwrap_or(Value::Null)).map_err(command_fault)
}

fn decode_default<T>(params: Option<Value>) -> Result<T, ServerFault>
where
    T: serde::de::DeserializeOwned + schemars::JsonSchema + Default + 'static,
{
    match params {
        Some(params) => decode(Some(params)),
        None => Ok(T::default()),
    }
}

fn command_fault(error: crate::strict::CommandDecodeError) -> ServerFault {
    ServerFault::from_error(error.to_rpc_error())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSessionRenameParams {
    session_id: SessionId,
    client_rename_id: ClientRenameId,
    change: RawSessionRenameChange,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RawSessionRenameChange {
    Set { title: String },
    Clear,
    Reset,
}

fn decode_rename(params: Option<Value>) -> Result<SessionRenameParams, ServerFault> {
    let params = params.unwrap_or(Value::Null);
    crate::strict::check_command(&schemars::schema_for!(SessionRenameParams), &params)
        .map_err(command_fault)?;
    if let Ok(request) = serde_json::from_value::<SessionRenameParams>(params.clone()) {
        return Ok(request);
    }
    if serde_json::to_vec(&params)
        .map_err(|error| ServerFault::invalid_params(error.to_string()))?
        .len()
        > MAX_RAW_RENAME_PARAMS_BYTES
    {
        return Err(ServerFault::invalid_params("the params are too large"));
    }
    let raw: RawSessionRenameParams = serde_json::from_value(params)
        .map_err(|error| ServerFault::invalid_params(error.to_string()))?;
    let invalid_title = rename_invalid_title(&raw);
    let change = match raw.change {
        RawSessionRenameChange::Set { title } => SessionRenameChange::Set {
            title: SessionTitle::new(title).map_err(|_| invalid_title)?,
        },
        RawSessionRenameChange::Clear => SessionRenameChange::Clear,
        RawSessionRenameChange::Reset => SessionRenameChange::Reset,
    };
    Ok(SessionRenameParams {
        session_id: raw.session_id,
        client_rename_id: raw.client_rename_id,
        change,
    })
}

fn rename_invalid_title(raw: &RawSessionRenameParams) -> ServerFault {
    ServerFault::from_error(
        crate::errors::INVALID_TITLE
            .error()
            .field("session_id", raw.session_id)
            .build(),
    )
}

/// A result serialized once, to the text its response frame embeds.
fn value<T: Serialize>(result: T) -> Result<Box<RawValue>, ServerFault> {
    serde_json::value::to_raw_value(&result).map_err(|_| ServerFault::internal())
}

enum Incoming {
    Request {
        id: JsonRpcId,
        method: String,
        params: Option<Value>,
    },
    Notification {
        method: String,
        params: Option<Value>,
    },
}

/// Parses a request frame. The frame's own buffer is wiped once parsed: a
/// `provider.connect` request carries credentials, and the typed params that
/// end up holding them zeroize when dropped.
fn parse_incoming(frame: MessageFrame) -> Result<Value, ServerFault> {
    let parsed = match frame {
        MessageFrame::Value(value) => return Ok(value),
        MessageFrame::Text(mut text) => {
            let parsed = serde_json::from_str(&text);
            text.zeroize();
            parsed
        }
        MessageFrame::Bytes(bytes) => {
            let parsed = serde_json::from_slice(&bytes);
            // Shared bytes another holder still reads are left alone.
            if let Ok(mut owned) = bytes.try_into_mut() {
                owned.as_mut().zeroize();
            }
            parsed
        }
    };
    parsed.map_err(|error| ServerFault::registry(&crate::errors::PARSE_FAILED, error.to_string()))
}

fn classify_incoming(value: Value) -> Result<Incoming, ServerFault> {
    let object = value.as_object().ok_or_else(|| {
        ServerFault::registry(
            &crate::errors::INVALID_MESSAGE,
            "a batch or scalar is not a request",
        )
    })?;
    if object.contains_key("id") {
        let request: Request = serde_json::from_value(value).map_err(|_| {
            ServerFault::registry(
                &crate::errors::INVALID_MESSAGE,
                "invalid JSON-RPC request envelope",
            )
        })?;
        if request.method.is_empty() {
            return Err(ServerFault::registry(
                &crate::errors::INVALID_MESSAGE,
                "method must be non-empty",
            ));
        }
        Ok(Incoming::Request {
            id: request.id,
            method: request.method,
            params: request.params,
        })
    } else {
        let notification: Notification = serde_json::from_value(value).map_err(|_| {
            ServerFault::registry(
                &crate::errors::INVALID_MESSAGE,
                "invalid JSON-RPC notification envelope",
            )
        })?;
        if notification.method.is_empty() {
            return Err(ServerFault::registry(
                &crate::errors::INVALID_MESSAGE,
                "method must be non-empty",
            ));
        }
        Ok(Incoming::Notification {
            method: notification.method,
            params: notification.params,
        })
    }
}

/// Reads the client's version from the raw params before anything else, so
/// a client of another version is answered with `unsupported_protocol`
/// naming both, then decodes the rest as a command.
fn decode_handshake(params: Option<Value>) -> Result<ClientHello, ServerFault> {
    let params = params.unwrap_or(Value::Null);
    crate::negotiate_version(crate::CLIENT_PROTOCOL_VERSION, &params)
        .map_err(|refusal| ServerFault::from_error(refusal.to_rpc_error()))?;
    decode(Some(params))
}

fn handshake_required() -> ServerFault {
    ServerFault::from_error(crate::errors::HANDSHAKE_FIRST.error().build())
}

fn unauthenticated() -> ServerFault {
    ServerFault::from_error(crate::errors::NOT_AUTHENTICATED.error().build())
}

/// A success response borrowing its id and its already-serialized result.
#[derive(Serialize)]
struct SuccessResponseRef<'a> {
    jsonrpc: JsonRpcVersion,
    id: &'a JsonRpcId,
    result: &'a RawValue,
}

/// Wraps an already-serialized result without re-serializing it: a replay
/// page can run to megabytes, and it is copied into the frame as text.
fn success_response(id: &JsonRpcId, result: &RawValue) -> Result<String, TransportError> {
    Ok(serde_json::to_string(&SuccessResponseRef {
        jsonrpc: JsonRpcVersion::current(),
        id,
        result,
    })?)
}

fn error_response(id: Option<JsonRpcId>, fault: ServerFault) -> Result<String, TransportError> {
    match id {
        Some(id) => Ok(serde_json::to_string(&Response::Error(ErrorResponse {
            jsonrpc: JsonRpcVersion::current(),
            id,
            error: JsonRpcError {
                code: fault.code,
                message: fault.message.into_owned(),
                data: fault.data,
            },
        }))?),
        None => Ok(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": fault.code, "message": fault.message, "data": fault.data },
        })
        .to_string()),
    }
}

#[cfg(test)]
mod tests;
