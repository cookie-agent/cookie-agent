use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    AgentUsageParams, AgentUsageResult, ApprovalListParams, ApprovalListResult,
    ApprovalRespondParams, ApprovalRespondResult, ClientHello, ClientRenameId, ErrorResponse,
    EventsSubscribeParams, EventsSubscribeResult, GlobalUsageParams, GlobalUsageResult,
    JsonRpcError, JsonRpcId, JsonRpcVersion, McpAuthBeginParams, McpAuthBeginResult,
    McpAuthCancelParams, McpAuthCancelResult, McpServerAddParams, McpServerEditParams,
    McpServerListParams, McpServerListResult, McpServerMutationResult, McpServerNameParams,
    McpServerPersistParams, McpServerSetEnabledParams, MessageFrame, Notification,
    ProviderConnectParams, ProviderConnectResult, ProviderDisconnectParams,
    ProviderDisconnectResult, Request, Response, RunCancelParams, RunCancelResult,
    RunRecallSteerParams, RunRecallSteerResult, RunStartParams, RunStartResult, RunSteerParams,
    RunSteerResult, RunToolStdinParams, RunToolStdinResult, RuntimeSnapshotGetParams,
    RuntimeSnapshotResult, ServerHello, SessionChildrenParams, SessionChildrenResult,
    SessionCompactParams, SessionCompactResult, SessionCreateParams, SessionCreateResult,
    SessionForkParams, SessionForkResult, SessionGetParams, SessionGetResult, SessionId,
    SessionListParams, SessionListResult, SessionPermissionClearParams, SessionPermissionGetParams,
    SessionPermissionGetResult, SessionPermissionMutationResult, SessionPermissionSetParams,
    SessionRenameChange, SessionRenameError, SessionRenameErrorCode, SessionRenameParams,
    SessionRenameResult, SessionResumeParams, SessionResumeResult, SessionRevertParams,
    SessionRevertResult, SessionSetPermissionModeParams, SessionSetPermissionModeResult,
    SessionTitle, SessionTreeParams, SessionTreeResult, SessionTreeUsageResult, SessionUsageParams,
    SessionUsageResult, SkillsGetParams, SkillsGetResult, SkillsListParams, SkillsListResult,
    SuccessResponse, Transport, TransportError,
};

const OUTBOUND_QUEUE_CAPACITY: usize = 512;
const MAX_RAW_RENAME_PARAMS_BYTES: usize = 4 * 1024;

/// Per-connection facilities available to a server implementation.
#[derive(Clone)]
pub struct ServerContext {
    notifications: mpsc::Sender<Value>,
    shutdown: CancellationToken,
    subscribed_sessions: Arc<Mutex<HashSet<SessionId>>>,
}

impl ServerContext {
    /// Emit one JSON-RPC notification in connection order.
    pub async fn notify<T: Serialize>(
        &self,
        method: &str,
        params: &T,
    ) -> Result<(), TransportError> {
        let params = serde_json::to_value(params)?;
        let notification = serde_json::to_value(Notification::new(method, Some(params)))?;
        tokio::select! {
            () = self.shutdown.cancelled() => Err(TransportError::Closed),
            result = self.notifications.send(notification) => result.map_err(|_| TransportError::Closed),
        }
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
pub fn test_server_context() -> (ServerContext, mpsc::Receiver<Value>) {
    let (notifications, receiver) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
    (
        ServerContext {
            notifications,
            shutdown: CancellationToken::new(),
            subscribed_sessions: Arc::new(Mutex::new(HashSet::new())),
        },
        receiver,
    )
}

/// Server end of the protocol contract after session-level validation.
#[async_trait]
pub trait ServerProtocol: Send + Sync + 'static {
    /// Called after the exact-version handshake response has been sent.
    async fn connected(&self, _context: ServerContext) {}

    async fn create_session(
        &self,
        params: SessionCreateParams,
    ) -> Result<SessionCreateResult, ServerFault>;
    async fn list_sessions(
        &self,
        params: SessionListParams,
    ) -> Result<SessionListResult, ServerFault>;
    async fn get_session(&self, params: SessionGetParams) -> Result<SessionGetResult, ServerFault>;
    async fn session_usage(
        &self,
        params: SessionUsageParams,
    ) -> Result<SessionUsageResult, ServerFault>;
    async fn session_tree_usage(
        &self,
        params: SessionUsageParams,
    ) -> Result<SessionTreeUsageResult, ServerFault>;
    async fn agent_usage(&self, params: AgentUsageParams) -> Result<AgentUsageResult, ServerFault>;
    async fn global_usage(
        &self,
        params: GlobalUsageParams,
    ) -> Result<GlobalUsageResult, ServerFault>;
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
    ) -> Result<SessionRenameResult, ServerFault>;
    async fn set_permission_mode(
        &self,
        params: SessionSetPermissionModeParams,
    ) -> Result<SessionSetPermissionModeResult, ServerFault>;
    async fn get_session_permissions(
        &self,
        params: SessionPermissionGetParams,
    ) -> Result<SessionPermissionGetResult, ServerFault>;
    async fn set_session_permission(
        &self,
        params: SessionPermissionSetParams,
    ) -> Result<SessionPermissionMutationResult, ServerFault>;
    async fn clear_session_permission(
        &self,
        params: SessionPermissionClearParams,
    ) -> Result<SessionPermissionMutationResult, ServerFault>;
    async fn list_skills(&self, params: SkillsListParams) -> Result<SkillsListResult, ServerFault>;
    async fn get_skill(&self, params: SkillsGetParams) -> Result<SkillsGetResult, ServerFault>;
    async fn compact_session(
        &self,
        params: SessionCompactParams,
    ) -> Result<SessionCompactResult, ServerFault>;
    async fn revert_session(
        &self,
        params: SessionRevertParams,
    ) -> Result<SessionRevertResult, ServerFault>;
    async fn fork_session(
        &self,
        params: SessionForkParams,
    ) -> Result<SessionForkResult, ServerFault>;
    async fn start_run(&self, params: RunStartParams) -> Result<RunStartResult, ServerFault>;
    async fn steer_run(&self, params: RunSteerParams) -> Result<RunSteerResult, ServerFault>;
    async fn recall_steer(
        &self,
        params: RunRecallSteerParams,
    ) -> Result<RunRecallSteerResult, ServerFault>;
    async fn cancel_run(&self, params: RunCancelParams) -> Result<RunCancelResult, ServerFault>;
    async fn tool_stdin(
        &self,
        params: RunToolStdinParams,
    ) -> Result<RunToolStdinResult, ServerFault>;
    async fn subscribe_events(
        &self,
        params: EventsSubscribeParams,
        context: &ServerContext,
    ) -> Result<EventsSubscribeResult, ServerFault>;
    async fn respond_approval(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<ApprovalRespondResult, ServerFault>;
    async fn list_approvals(
        &self,
        params: ApprovalListParams,
    ) -> Result<ApprovalListResult, ServerFault>;
    async fn begin_mcp_auth(
        &self,
        params: McpAuthBeginParams,
    ) -> Result<McpAuthBeginResult, ServerFault>;
    async fn cancel_mcp_auth(
        &self,
        params: McpAuthCancelParams,
    ) -> Result<McpAuthCancelResult, ServerFault>;
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
        params: McpServerNameParams,
    ) -> Result<McpServerMutationResult, ServerFault>;
    async fn persist_mcp_server(
        &self,
        params: McpServerPersistParams,
    ) -> Result<McpServerMutationResult, ServerFault>;
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
    pub message: &'static str,
    pub data: Option<Value>,
}

impl ServerFault {
    #[must_use]
    pub const fn new(code: i32, message: &'static str, data: Option<Value>) -> Self {
        Self {
            code,
            message,
            data,
        }
    }

    #[must_use]
    pub const fn method_not_found() -> Self {
        Self::new(-32601, "method not found", None)
    }

    #[must_use]
    pub const fn invalid_params() -> Self {
        Self::new(-32602, "invalid params", None)
    }

    #[must_use]
    pub const fn internal() -> Self {
        Self::new(-32603, "internal error", None)
    }
}

struct ConnectionShutdown(CancellationToken);

impl Drop for ConnectionShutdown {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Run one complete server-side protocol session over a transport.
pub async fn serve<T, S>(
    server: Arc<S>,
    mut transport: T,
    shutdown: CancellationToken,
) -> Result<(), TransportError>
where
    T: Transport,
    S: ServerProtocol,
{
    let (notifications, mut notification_rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
    let context = ServerContext {
        notifications,
        shutdown,
        subscribed_sessions: Arc::new(Mutex::new(HashSet::new())),
    };
    let _guard = ConnectionShutdown(context.shutdown());
    let mut handshaken = false;
    loop {
        tokio::select! {
            () = context.shutdown.cancelled() => return Ok(()),
            incoming = transport.recv() => {
                let Some(frame) = incoming? else { return Ok(()); };
                let incoming = match parse_incoming(frame).and_then(classify_incoming) {
                    Ok(incoming) => incoming,
                    Err(fault) => {
                        transport.send(MessageFrame::Value(error_response(None, fault)?)).await?;
                        continue;
                    }
                };
                match incoming {
                    Incoming::Request { id, method, params } => {
                        let (response, connected) = if !handshaken {
                            if method != "handshake" {
                                (error_response(Some(id), handshake_required())?, false)
                            } else {
                                match decode_handshake(params) {
                                    Ok(()) => {
                                        handshaken = true;
                                        (success_response(id, &ServerHello { protocol_version: crate::ProtocolVersion::current() })?, true)
                                    }
                                    Err(fault) => (error_response(Some(id), fault)?, false),
                                }
                            }
                        } else {
                            let result = if method == "handshake" {
                                decode_handshake(params).map(|()| {
                                    serde_json::to_value(ServerHello {
                                        protocol_version: crate::ProtocolVersion::current(),
                                    })
                                    .expect("server hello serializes")
                                })
                            } else {
                                dispatch(server.as_ref(), &method, params, &context, true).await
                            };
                            (match result {
                                Ok(value) => success_response(id, &value)?,
                                Err(fault) => error_response(Some(id), fault)?,
                            }, false)
                        };
                        transport.send(MessageFrame::Value(response)).await?;
                        if connected {
                            server.connected(context.clone()).await;
                        }
                    }
                    Incoming::Notification { method, params } => {
                        if handshaken {
                            if method == "handshake" {
                                let _ = decode_handshake(params);
                            } else {
                                let _ = dispatch(server.as_ref(), &method, params, &context, false).await;
                            }
                        }
                    }
                }
            }
            Some(notification) = notification_rx.recv() => {
                transport.send(MessageFrame::Value(notification)).await?;
            }
        }
    }
}

async fn dispatch<S: ServerProtocol>(
    server: &S,
    method: &str,
    params: Option<Value>,
    context: &ServerContext,
    has_request_id: bool,
) -> Result<Value, ServerFault> {
    match method {
        "session.create" => value(server.create_session(decode(params)?).await?),
        "session.list" => value(server.list_sessions(decode_default(params)?).await?),
        "session.get" => value(server.get_session(decode(params)?).await?),
        "session.usage" => value(server.session_usage(decode(params)?).await?),
        "session.tree_usage" => value(server.session_tree_usage(decode(params)?).await?),
        "agent.usage" => value(server.agent_usage(decode(params)?).await?),
        "usage.global" => value(server.global_usage(decode_default(params)?).await?),
        "session.children" => value(server.session_children(decode(params)?).await?),
        "session.tree" => value(server.session_tree(decode(params)?).await?),
        "session.resume" => value(server.resume_session(decode(params)?).await?),
        "session.rename" => value(server.rename_session(decode_rename(params)?).await?),
        "session.set_permission_mode" => value(server.set_permission_mode(decode(params)?).await?),
        "session.permission.get" => value(server.get_session_permissions(decode(params)?).await?),
        "session.permission.set" => value(server.set_session_permission(decode(params)?).await?),
        "session.permission.clear" => {
            value(server.clear_session_permission(decode(params)?).await?)
        }
        "skills.list" => value(server.list_skills(decode(params)?).await?),
        "skills.get" => value(server.get_skill(decode(params)?).await?),
        "session.compact" => value(server.compact_session(decode(params)?).await?),
        "session.revert" => value(server.revert_session(decode(params)?).await?),
        "session.fork" => value(server.fork_session(decode(params)?).await?),
        "run.start" => value(server.start_run(decode(params)?).await?),
        "run.steer" => value(server.steer_run(decode(params)?).await?),
        "run.recall_steer" => value(server.recall_steer(decode(params)?).await?),
        "run.cancel" => value(server.cancel_run(decode(params)?).await?),
        "run.tool_stdin" => value(server.tool_stdin(decode(params)?).await?),
        "events.subscribe" => value(server.subscribe_events(decode(params)?, context).await?),
        "approval.respond" => value(server.respond_approval(decode(params)?).await?),
        "approval.list" => value(server.list_approvals(decode(params)?).await?),
        "mcp.auth.begin" => value(server.begin_mcp_auth(decode(params)?).await?),
        "mcp.auth.cancel" => value(server.cancel_mcp_auth(decode(params)?).await?),
        "mcp.server.list" => value(server.list_mcp_servers(decode_default(params)?).await?),
        "mcp.server.add" => value(server.add_mcp_server(decode(params)?).await?),
        "mcp.server.edit" => value(server.edit_mcp_server(decode(params)?).await?),
        "mcp.server.remove" => value(server.remove_mcp_server(decode(params)?).await?),
        "mcp.server.set_enabled" => value(server.set_mcp_server_enabled(decode(params)?).await?),
        "mcp.server.reconnect" => value(server.reconnect_mcp_server(decode(params)?).await?),
        "mcp.server.persist" => value(server.persist_mcp_server(decode(params)?).await?),
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
        | crate::PROVIDER_DISCONNECT_METHOD => {
            Err(ServerFault::new(-32600, "request id required", None))
        }
        _ => Err(ServerFault::method_not_found()),
    }
}

fn decode<T: serde::de::DeserializeOwned>(params: Option<Value>) -> Result<T, ServerFault> {
    serde_json::from_value(params.unwrap_or(Value::Null)).map_err(|_| ServerFault::invalid_params())
}

fn decode_default<T>(params: Option<Value>) -> Result<T, ServerFault>
where
    T: serde::de::DeserializeOwned + Default,
{
    match params {
        Some(params) => serde_json::from_value(params).map_err(|_| ServerFault::invalid_params()),
        None => Ok(T::default()),
    }
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
    if let Ok(request) = serde_json::from_value::<SessionRenameParams>(params.clone()) {
        return Ok(request);
    }
    if serde_json::to_vec(&params)
        .map_err(|_| ServerFault::invalid_params())?
        .len()
        > MAX_RAW_RENAME_PARAMS_BYTES
    {
        return Err(ServerFault::invalid_params());
    }
    let raw: RawSessionRenameParams =
        serde_json::from_value(params).map_err(|_| ServerFault::invalid_params())?;
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
    ServerFault::new(
        -32602,
        "session rename rejected",
        serde_json::to_value(SessionRenameError {
            code: SessionRenameErrorCode::InvalidTitle,
            session_id: raw.session_id,
            client_rename_id: raw.client_rename_id.clone(),
        })
        .ok(),
    )
}

fn value<T: Serialize>(result: T) -> Result<Value, ServerFault> {
    serde_json::to_value(result).map_err(|_| ServerFault::internal())
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

fn parse_incoming(frame: MessageFrame) -> Result<Value, ServerFault> {
    match frame {
        MessageFrame::Value(value) => Ok(value),
        MessageFrame::Text(text) => {
            serde_json::from_str(&text).map_err(|_| ServerFault::new(-32700, "parse error", None))
        }
    }
}

fn classify_incoming(value: Value) -> Result<Incoming, ServerFault> {
    let object = value.as_object().ok_or_else(|| {
        ServerFault::new(
            -32600,
            "invalid request",
            Some(json!({ "detail": "a batch or scalar is not a request" })),
        )
    })?;
    if object.contains_key("id") {
        let request: Request = serde_json::from_value(value).map_err(|_| {
            ServerFault::new(
                -32600,
                "invalid request",
                Some(json!({ "detail": "invalid JSON-RPC request envelope" })),
            )
        })?;
        if request.method.is_empty() {
            return Err(ServerFault::new(
                -32600,
                "invalid request",
                Some(json!({ "detail": "method must be non-empty" })),
            ));
        }
        Ok(Incoming::Request {
            id: request.id,
            method: request.method,
            params: request.params,
        })
    } else {
        let notification: Notification = serde_json::from_value(value).map_err(|_| {
            ServerFault::new(
                -32600,
                "invalid request",
                Some(json!({ "detail": "invalid JSON-RPC notification envelope" })),
            )
        })?;
        if notification.method.is_empty() {
            return Err(ServerFault::new(
                -32600,
                "invalid request",
                Some(json!({ "detail": "method must be non-empty" })),
            ));
        }
        Ok(Incoming::Notification {
            method: notification.method,
            params: notification.params,
        })
    }
}

fn decode_handshake(params: Option<Value>) -> Result<(), ServerFault> {
    serde_json::from_value::<ClientHello>(params.unwrap_or(Value::Null))
        .map(|_| ())
        .map_err(|_| ServerFault::invalid_params())
}

fn handshake_required() -> ServerFault {
    ServerFault::new(-32001, "handshake required", None)
}

fn success_response<T: Serialize>(id: JsonRpcId, result: &T) -> Result<Value, TransportError> {
    Ok(serde_json::to_value(Response::Success(SuccessResponse {
        jsonrpc: JsonRpcVersion::current(),
        id,
        result: serde_json::to_value(result)?,
    }))?)
}

fn error_response(id: Option<JsonRpcId>, fault: ServerFault) -> Result<Value, TransportError> {
    match id {
        Some(id) => Ok(serde_json::to_value(Response::Error(ErrorResponse {
            jsonrpc: JsonRpcVersion::current(),
            id,
            error: JsonRpcError {
                code: fault.code,
                message: fault.message.into(),
                data: fault.data,
            },
        }))?),
        None => Ok(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": fault.code, "message": fault.message, "data": fault.data },
        })),
    }
}
