//! Transport-neutral protocol-v7 JSON-RPC service.

use std::{
    env, fmt, fs,
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use axum::{
    Router,
    extract::{ConnectInfo, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cookie_agent_config::LoadedConfiguration;
use cookie_agent_engine::{ApprovalRespondFailure, Engine, EngineError, events::OutputMessage};
use cookie_agent_models::{
    AuthDefinition, Catalog, CredentialConnectRequest, CredentialStoreError, ModelSetManager,
    ModelSetManagerError, ProviderDefinition,
};
use cookie_agent_protocol::{
    AgentListParams, AgentListResult, ApprovalListParams, ApprovalRespondError,
    ApprovalRespondErrorCode, ApprovalRespondParams, CatalogError, CatalogErrorCode,
    CatalogModelListParams, CatalogModelListResult, CatalogProvider, CatalogProviderListParams,
    CatalogProviderListResult, CatalogRevision, CatalogSnapshot, ClientConnectId, ClientHello,
    ClientRenameId, ClientResponseId, CredentialFieldName, ErrorResponse, EventPayload,
    EventSubscriptionMessage, EventsSubscribeParams, JsonRpcError, JsonRpcId, JsonRpcVersion,
    ModelListError, ModelListErrorCode, ModelListParams, ModelListResult, Notification,
    OutputSnapshotEnvelope, OutputStream, ProtocolVersion, ProviderConnectError,
    ProviderConnectErrorCode, ProviderConnectParams, ProviderConnectResult, ProviderConnection,
    ProviderId, Request as RpcRequest, Response as RpcResponse, RunCancelParams, RunStartConflict,
    RunStartConflictCode, RunStartParams, RunSteerParams, RunToolStdinParams, ServerHello,
    SessionChildrenParams, SessionChildrenResult, SessionCreateParams, SessionCreateResult,
    SessionGetParams, SessionGetResult, SessionId, SessionListParams, SessionListResult,
    SessionRenameChange, SessionRenameError, SessionRenameErrorCode, SessionRenameParams,
    SessionResumeParams, SessionResumeResult, SessionTitle, SessionTreeParams, SessionTreeResult,
    SnapshotRevision, SuccessResponse,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
    time::{Duration, sleep},
};
use tokio_util::sync::CancellationToken;

const OUTBOUND_QUEUE_CAPACITY: usize = 512;
const TOKEN_FILE: &str = "token-v1";
const TOKEN_BYTES: usize = 32;
const TOKEN_ENCODED_BYTES: usize = 43;
const MAX_RAW_RENAME_PARAMS_BYTES: usize = 4 * 1024;

struct ConnectionShutdown(CancellationToken);

impl Drop for ConnectionShutdown {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// One complete JSON-RPC message.
#[derive(Clone, PartialEq)]
pub enum MessageFrame {
    Text(String),
    Value(Value),
}

impl fmt::Debug for MessageFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => formatter.write_str("MessageFrame::Text(<redacted>)"),
            Self::Value(_) => formatter.write_str("MessageFrame::Value(<redacted>)"),
        }
    }
}

#[async_trait]
pub trait MessageStream: Send {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError>;
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport closed")]
    Closed,
    #[error("websocket error: {0}")]
    WebSocket(#[from] axum::Error),
}

pub struct InProcessStream {
    sender: mpsc::Sender<MessageFrame>,
    receiver: mpsc::Receiver<MessageFrame>,
}

#[must_use]
pub fn in_process_pair(capacity: usize) -> (InProcessStream, InProcessStream) {
    let (client_to_server_tx, client_to_server_rx) = mpsc::channel(capacity);
    let (server_to_client_tx, server_to_client_rx) = mpsc::channel(capacity);
    (
        InProcessStream {
            sender: client_to_server_tx,
            receiver: server_to_client_rx,
        },
        InProcessStream {
            sender: server_to_client_tx,
            receiver: client_to_server_rx,
        },
    )
}

#[async_trait]
impl MessageStream for InProcessStream {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.sender
            .send(frame)
            .await
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        Ok(self.receiver.recv().await)
    }
}

struct WebSocketStream {
    socket: axum::extract::ws::WebSocket,
}

#[async_trait]
impl MessageStream for WebSocketStream {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        let text = match frame {
            MessageFrame::Text(text) => text,
            MessageFrame::Value(value) => {
                serde_json::to_string(&value).map_err(|_| TransportError::Closed)?
            }
        };
        self.socket
            .send(axum::extract::ws::Message::Text(text.into()))
            .await
            .map_err(TransportError::from)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        loop {
            match self.socket.next().await {
                Some(Ok(axum::extract::ws::Message::Text(text))) => {
                    return Ok(Some(MessageFrame::Text(text.to_string())));
                }
                Some(Ok(axum::extract::ws::Message::Close(_))) | None => return Ok(None),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(TransportError::from(error)),
            }
        }
    }
}

/// Protocol service composed with one engine and its atomic model manager.
#[derive(Clone)]
pub struct Server {
    engine: Engine,
    model_manager: Arc<ModelSetManager>,
    catalog: Arc<Catalog>,
    configuration: Arc<LoadedConfiguration>,
    shutdown: CancellationToken,
    token_path: PathBuf,
}

impl Server {
    #[must_use]
    pub fn new(
        engine: Engine,
        model_manager: Arc<ModelSetManager>,
        catalog: Arc<Catalog>,
        configuration: Arc<LoadedConfiguration>,
    ) -> Self {
        Self {
            engine,
            model_manager,
            catalog,
            configuration,
            shutdown: CancellationToken::new(),
            token_path: standard_token_path().unwrap_or_default(),
        }
    }

    pub fn router(self: Arc<Self>) -> Result<Router, ServerError> {
        let token = Arc::new(load_or_create_token(&self.token_path)?);
        Ok(Router::new()
            .route("/ws", get(websocket_upgrade))
            .with_state(WebSocketState {
                server: self,
                token,
            }))
    }

    pub async fn serve(self: Arc<Self>, port: u16) -> Result<RunningServer, ServerError> {
        let router = self.clone().router()?;
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            .await
            .map_err(ServerError::Listen)?;
        let address = listener.local_addr().map_err(ServerError::Listen)?;
        let shutdown = self.shutdown.clone();
        let task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await;
        });
        Ok(RunningServer { address, task })
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    pub async fn serve_stream<S>(self: Arc<Self>, mut stream: S) -> Result<(), TransportError>
    where
        S: MessageStream,
    {
        let (notifications, mut notification_rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let mut handshaken = false;
        let connection_shutdown = self.shutdown.child_token();
        let _guard = ConnectionShutdown(connection_shutdown.clone());
        loop {
            tokio::select! {
                _ = connection_shutdown.cancelled() => return Ok(()),
                incoming = stream.recv() => {
                    let Some(frame) = incoming? else { return Ok(()); };
                    let incoming = match parse_incoming(frame).and_then(classify_incoming) {
                        Ok(incoming) => incoming,
                        Err(error) => {
                            stream.send(MessageFrame::Value(error_response(None, error)?)).await?;
                            continue;
                        }
                    };
                    match incoming {
                        Incoming::Request { id, method, params } => {
                            let result = self.route_after_handshake(
                                &mut handshaken,
                                &method,
                                params,
                                true,
                                notifications.clone(),
                                &connection_shutdown,
                            ).await;
                            let response = match result {
                                Ok(RouteResult::Handshake) => success_response(id, &ServerHello { protocol_version: ProtocolVersion::current() })?,
                                Ok(RouteResult::Value(value)) => success_response(id, &value)?,
                                Err(error) => error_response(Some(id), error)?,
                            };
                            stream.send(MessageFrame::Value(response)).await?;
                        }
                        Incoming::Notification { method, params } => {
                            let _ = self.route_after_handshake(
                                &mut handshaken,
                                &method,
                                params,
                                false,
                                notifications.clone(),
                                &connection_shutdown,
                            ).await;
                        }
                    }
                }
                Some(notification) = notification_rx.recv() => {
                    stream.send(MessageFrame::Value(notification)).await?;
                }
            }
        }
    }

    async fn route_after_handshake(
        &self,
        handshaken: &mut bool,
        method: &str,
        params: Option<Value>,
        has_request_id: bool,
        notifications: mpsc::Sender<Value>,
        shutdown: &CancellationToken,
    ) -> Result<RouteResult, RpcFault> {
        let result = if !*handshaken && method != "handshake" {
            Err(RpcFault::handshake_required())
        } else {
            self.route(method, params, has_request_id, notifications, shutdown)
                .await
        };
        if matches!(result, Ok(RouteResult::Handshake)) {
            *handshaken = true;
        }
        result
    }

    async fn route(
        &self,
        method: &str,
        params: Option<Value>,
        has_request_id: bool,
        notifications: mpsc::Sender<Value>,
        shutdown: &CancellationToken,
    ) -> Result<RouteResult, RpcFault> {
        match method {
            "handshake" => {
                if !has_request_id {
                    return Err(RpcFault::request_id_required());
                }
                let _: ClientHello = decode_params(params)?;
                Ok(RouteResult::Handshake)
            }
            "session.create" => {
                let request: SessionCreateParams = decode_params(params)?;
                let session = self
                    .engine
                    .create_session(request.selection)
                    .map_err(engine_fault)?;
                value(SessionCreateResult { session })
            }
            "session.list" => {
                let request: SessionListParams = params_or_default(params)?;
                let sessions = self
                    .engine
                    .list_sessions()
                    .into_iter()
                    .filter(|session| {
                        request
                            .cwd_identity
                            .as_ref()
                            .is_none_or(|cwd| &session.cwd_identity == cwd)
                    })
                    .collect();
                value(SessionListResult { sessions })
            }
            "session.get" => {
                let request: SessionGetParams = decode_params(params)?;
                value(SessionGetResult {
                    session: self
                        .engine
                        .get_session(request.session_id)
                        .map_err(engine_fault)?,
                })
            }
            "session.children" => {
                let request: SessionChildrenParams = decode_params(params)?;
                value(SessionChildrenResult {
                    children: self.engine.children(request.session_id),
                })
            }
            "session.tree" => {
                let request: SessionTreeParams = decode_params(params)?;
                value(SessionTreeResult {
                    tree: self.engine.tree(request.session_id).map_err(engine_fault)?,
                })
            }
            "session.resume" => {
                let request: SessionResumeParams = decode_params(params)?;
                let session = self
                    .engine
                    .resume(request.session_id)
                    .await
                    .map_err(engine_fault)?;
                value(SessionResumeResult { session })
            }
            "session.rename" => {
                let request = decode_rename_params(params)?;
                let result = self
                    .engine
                    .rename_session(request.clone())
                    .await
                    .map_err(|error| rename_fault(&request, error))?;
                value(result)
            }
            "run.start" => {
                let request: RunStartParams = decode_params(params)?;
                match self.engine.start_run(request.clone()).await {
                    Ok(result) => value(result),
                    Err(EngineError::RunIdempotencyConflict) => {
                        Err(RpcFault::run_start_conflict(&request))
                    }
                    Err(error) => Err(engine_fault(error)),
                }
            }
            "run.steer" => {
                let request: RunSteerParams = decode_params(params)?;
                value(
                    self.engine
                        .steer(request.run_id, request.input)
                        .await
                        .map_err(engine_fault)?,
                )
            }
            "run.cancel" => {
                let request: RunCancelParams = decode_params(params)?;
                value(
                    self.engine
                        .cancel_run(request.run_id)
                        .await
                        .map_err(engine_fault)?,
                )
            }
            "run.tool_stdin" => value(
                self.engine
                    .tool_stdin(decode_params::<RunToolStdinParams>(params)?)
                    .await
                    .map_err(engine_fault)?,
            ),
            "events.subscribe" => {
                let request: EventsSubscribeParams = decode_params(params)?;
                let (result, receiver) = self
                    .engine
                    .subscribe(request.session_id, request.cursor)
                    .await
                    .map_err(engine_fault)?;
                self.start_event_tail(receiver, notifications.clone(), shutdown.child_token());
                for event in &result.events {
                    if let EventPayload::ToolCallStarted { start } = &event.payload {
                        self.start_output_tail(
                            start.tool_call_id,
                            notifications.clone(),
                            shutdown.child_token(),
                        );
                    }
                }
                value(result)
            }
            "approval.respond" => {
                let request: ApprovalRespondParams = decode_params(params)?;
                match self.engine.approval_respond(request.clone()).await {
                    Ok(result) => value(result),
                    Err(error) => Err(approval_fault(&request, error)),
                }
            }
            "approval.list" => {
                let request: ApprovalListParams = decode_params(params)?;
                value(
                    self.engine
                        .list_approvals(request.root_session_id, request.status),
                )
            }
            "catalog.provider.list" => {
                let _: CatalogProviderListParams = params_or_default(params)?;
                value(self.list_catalog_providers()?)
            }
            "catalog.model.list" => {
                let request: CatalogModelListParams = params_or_default(params)?;
                value(self.list_catalog_models(&request)?)
            }
            "provider.connect" => {
                if !has_request_id {
                    return Err(RpcFault::request_id_required());
                }
                value(self.connect_provider(decode_params(params)?)?)
            }
            "model.list" => {
                let _: ModelListParams = params_or_default(params)?;
                value(self.list_models()?)
            }
            "agent.list" => {
                let _: AgentListParams = params_or_default(params)?;
                value(self.list_agents()?)
            }
            _ => Err(RpcFault::method_not_found()),
        }
    }

    fn list_models(&self) -> Result<ModelListResult, RpcFault> {
        let snapshot = self.model_manager.current();
        Ok(ModelListResult {
            revision: SnapshotRevision::new(snapshot.revision())
                .map_err(|_| RpcFault::model_list(ModelListErrorCode::ModelSnapshotInvalid))?,
            generated_at: snapshot
                .generated_at()
                .parse()
                .map_err(|_| RpcFault::model_list(ModelListErrorCode::ModelSnapshotInvalid))?,
            catalog_revision: CatalogRevision::new(snapshot.catalog_revision())
                .map_err(|_| RpcFault::model_list(ModelListErrorCode::ModelSnapshotInvalid))?,
            models: project_value(&snapshot.model_set().descriptors())?,
        })
    }

    fn list_agents(&self) -> Result<AgentListResult, RpcFault> {
        Ok(self.engine.list_agents())
    }

    fn list_catalog_providers(&self) -> Result<CatalogProviderListResult, RpcFault> {
        let providers = self
            .configuration
            .runtime
            .providers
            .iter()
            .filter_map(|(provider_id, definition)| match definition {
                ProviderDefinition::ModelsDev(provider)
                    if matches!(provider.auth, AuthDefinition::CredentialStore) =>
                {
                    self.catalog.providers().get(provider_id.as_str())
                }
                ProviderDefinition::ModelsDev(_) | ProviderDefinition::Explicit(_) => None,
            })
            .map(project_catalog_provider)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CatalogProviderListResult {
            snapshot: catalog_snapshot(&self.catalog)?,
            providers,
        })
    }

    fn list_catalog_models(
        &self,
        request: &CatalogModelListParams,
    ) -> Result<CatalogModelListResult, RpcFault> {
        let models = self
            .catalog
            .models()
            .iter()
            .filter(|model| {
                request
                    .provider_id
                    .as_ref()
                    .is_none_or(|provider| model.provider_id == provider.as_str())
            })
            // The vendored source retains upstream records with zero or
            // contradictory limits. They are not valid protocol-v7 catalog
            // descriptors and are excluded rather than emitting invalid wire.
            .filter_map(|model| project_value(model).ok())
            .collect::<Vec<_>>();
        Ok(CatalogModelListResult {
            snapshot: catalog_snapshot(&self.catalog)?,
            models,
        })
    }

    fn connect_provider(
        &self,
        request: ProviderConnectParams,
    ) -> Result<ProviderConnectResult, RpcFault> {
        let manager_request = into_manager_connect_request(request);
        let receipt = self
            .model_manager
            .connect(&manager_request)
            .map_err(|error| provider_connect_fault(&manager_request, error))?;
        Ok(ProviderConnectResult {
            client_connect_id: ClientConnectId::new(receipt.client_connect_id)
                .map_err(|_| RpcFault::internal())?,
            connection: ProviderConnection {
                provider_id: receipt.provider_id,
                credential_fields: receipt
                    .credential_fields
                    .into_iter()
                    .map(CredentialFieldName::new)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| RpcFault::internal())?,
                connected_at: receipt
                    .connected_at
                    .parse()
                    .map_err(|_| RpcFault::internal())?,
                catalog_revision: CatalogRevision::new(receipt.catalog_revision)
                    .map_err(|_| RpcFault::internal())?,
            },
            model_revision: SnapshotRevision::new(receipt.model_revision)
                .map_err(|_| RpcFault::internal())?,
        })
    }

    fn start_event_tail(
        &self,
        mut receiver: mpsc::Receiver<EventSubscriptionMessage>,
        notifications: mpsc::Sender<Value>,
        shutdown: CancellationToken,
    ) {
        let server = self.clone();
        tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    message = receiver.recv() => match message {
                        Some(message) => message,
                        None => return,
                    },
                };
                let tool_call_id = match &message {
                    EventSubscriptionMessage::Event { event } => match &event.payload {
                        EventPayload::ToolCallStarted { start } => Some(start.tool_call_id),
                        _ => None,
                    },
                    EventSubscriptionMessage::Gap { .. } => None,
                };
                if send_notification(&notifications, &shutdown, "events.subscription", &message)
                    .await
                    .is_err()
                {
                    return;
                }
                if let Some(tool_call_id) = tool_call_id {
                    server.start_output_tail(
                        tool_call_id,
                        notifications.clone(),
                        shutdown.child_token(),
                    );
                }
            }
        });
    }

    fn start_output_tail(
        &self,
        tool_call_id: cookie_agent_protocol::ToolCallId,
        notifications: mpsc::Sender<Value>,
        shutdown: CancellationToken,
    ) {
        let engine = self.engine.clone();
        tokio::spawn(async move {
            for _ in 0..10 {
                let stdout = engine.subscribe_tool_output(tool_call_id, OutputStream::Stdout);
                let stderr = engine.subscribe_tool_output(tool_call_id, OutputStream::Stderr);
                if stdout.is_some() || stderr.is_some() {
                    if let Some((snapshot, receiver)) = stdout {
                        tokio::spawn(forward_output(
                            OutputStream::Stdout,
                            snapshot,
                            receiver,
                            notifications.clone(),
                            shutdown.child_token(),
                        ));
                    }
                    if let Some((snapshot, receiver)) = stderr {
                        tokio::spawn(forward_output(
                            OutputStream::Stderr,
                            snapshot,
                            receiver,
                            notifications,
                            shutdown.child_token(),
                        ));
                    }
                    return;
                }
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = sleep(Duration::from_millis(5)) => {}
                }
            }
        });
    }
}

fn project_catalog_provider(
    provider: &cookie_agent_models::CatalogProvider,
) -> Result<CatalogProvider, RpcFault> {
    let mut credential_fields = provider
        .credential_fields
        .iter()
        .cloned()
        .map(CredentialFieldName::new)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| RpcFault::internal())?;
    credential_fields.sort();
    Ok(CatalogProvider {
        id: cookie_agent_protocol::CatalogIdentifier::new(provider.id.clone())
            .map_err(|_| RpcFault::internal())?,
        name: cookie_agent_protocol::CatalogText::new(provider.name.clone())
            .map_err(|_| RpcFault::internal())?,
        credential_fields,
        npm: cookie_agent_protocol::CatalogText::new(provider.npm.clone())
            .map_err(|_| RpcFault::internal())?,
        api: provider
            .api
            .clone()
            .map(cookie_agent_protocol::CatalogText::new)
            .transpose()
            .map_err(|_| RpcFault::internal())?,
        documentation_url: cookie_agent_protocol::CatalogText::new(
            provider.documentation_url.clone(),
        )
        .map_err(|_| RpcFault::internal())?,
    })
}

fn into_manager_connect_request(request: ProviderConnectParams) -> CredentialConnectRequest {
    CredentialConnectRequest {
        client_connect_id: request.client_connect_id.to_string(),
        provider_id: request.provider_id,
        catalog_revision: request.catalog_revision.to_string(),
        credentials: request
            .credentials
            .values
            .into_iter()
            .map(|(field, value)| (field.to_string(), value))
            .collect(),
    }
}

fn project_value<T, U>(value: &T) -> Result<U, RpcFault>
where
    T: Serialize,
    U: DeserializeOwned,
{
    serde_json::to_value(value)
        .map_err(|_| RpcFault::internal())
        .and_then(|value| serde_json::from_value(value).map_err(|_| RpcFault::internal()))
}

fn catalog_snapshot(catalog: &Catalog) -> Result<CatalogSnapshot, RpcFault> {
    let snapshot = catalog.snapshot();
    Ok(CatalogSnapshot {
        revision: CatalogRevision::new(snapshot.revision)
            .map_err(|_| RpcFault::catalog(CatalogErrorCode::CatalogSnapshotInvalid, None))?,
        source: cookie_agent_protocol::CatalogText::new(snapshot.source)
            .map_err(|_| RpcFault::catalog(CatalogErrorCode::CatalogSnapshotInvalid, None))?,
        fetched_at: snapshot
            .fetched_at
            .parse()
            .map_err(|_| RpcFault::catalog(CatalogErrorCode::CatalogSnapshotInvalid, None))?,
    })
}

fn provider_connect_fault(
    request: &CredentialConnectRequest,
    error: ModelSetManagerError,
) -> RpcFault {
    let (code, missing_credential_fields) = match error {
        ModelSetManagerError::UnknownProvider => {
            (ProviderConnectErrorCode::UnknownProvider, Vec::new())
        }
        ModelSetManagerError::ProviderDoesNotUseCredentialStore => {
            (ProviderConnectErrorCode::UnsupportedProvider, Vec::new())
        }
        ModelSetManagerError::CatalogRevisionConflict => (
            ProviderConnectErrorCode::CatalogRevisionConflict,
            Vec::new(),
        ),
        ModelSetManagerError::MissingCredentials(fields) => {
            let fields = fields
                .into_iter()
                .map(CredentialFieldName::new)
                .collect::<Result<Vec<_>, _>>();
            let Ok(fields) = fields else {
                return RpcFault::internal();
            };
            (ProviderConnectErrorCode::MissingCredential, fields)
        }
        ModelSetManagerError::InvalidCredentials
        | ModelSetManagerError::Credentials(CredentialStoreError::InvalidRequest) => {
            (ProviderConnectErrorCode::InvalidCredential, Vec::new())
        }
        ModelSetManagerError::Credentials(CredentialStoreError::IdempotencyConflict) => {
            (ProviderConnectErrorCode::IdempotencyConflict, Vec::new())
        }
        ModelSetManagerError::Credentials(
            CredentialStoreError::HomeUnavailable
            | CredentialStoreError::UnsupportedPlatform
            | CredentialStoreError::UnsafePath
            | CredentialStoreError::InvalidStore
            | CredentialStoreError::Io(_)
            | CredentialStoreError::Json(_)
            | CredentialStoreError::Clock,
        ) => (
            ProviderConnectErrorCode::CredentialStorageFailed,
            Vec::new(),
        ),
        ModelSetManagerError::Credentials(CredentialStoreError::CandidateRejected)
        | ModelSetManagerError::CandidateRejected
        | ModelSetManagerError::Models(_)
        | ModelSetManagerError::Set(_)
        | ModelSetManagerError::ObsoleteModelFingerprint => return RpcFault::internal(),
    };
    let client_connect_id = ClientConnectId::new(request.client_connect_id.clone())
        .unwrap_or_else(|_| ClientConnectId::new("invalid-connect-id").expect("static ID"));
    RpcFault::provider_connect_parts(
        &request.provider_id,
        &client_connect_id,
        code,
        missing_credential_fields,
    )
}

fn rename_fault(request: &SessionRenameParams, error: EngineError) -> RpcFault {
    let code = match error {
        EngineError::RenameConflict => SessionRenameErrorCode::IdempotencyConflict,
        EngineError::Session(cookie_agent_engine::session::SessionError::Missing(_))
        | EngineError::MissingActor(_) => SessionRenameErrorCode::SessionNotFound,
        _ => return engine_fault(error),
    };
    RpcFault::rename(request, code)
}

fn approval_fault(request: &ApprovalRespondParams, error: EngineError) -> RpcFault {
    match error {
        EngineError::ApprovalResponse(failure) => RpcFault::approval_failure(request, &failure),
        EngineError::ApprovalNotPending { .. } => RpcFault::approval(
            request,
            ApprovalRespondErrorCode::ApprovalNotPending,
            None,
            None,
        ),
        EngineError::Session(cookie_agent_engine::session::SessionError::Missing(_))
        | EngineError::MissingActor(_) => RpcFault::approval(
            request,
            ApprovalRespondErrorCode::ApprovalNotFound,
            None,
            None,
        ),
        _ => engine_fault(error),
    }
}

pub struct RunningServer {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl RunningServer {
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    pub async fn wait(self) {
        let _ = self.task.await;
    }
}

#[derive(Clone)]
struct WebSocketState {
    server: Arc<Server>,
    token: Arc<String>,
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("could not bind localhost websocket listener: {0}")]
    Listen(#[source] io::Error),
    #[error("could not prepare websocket authentication token")]
    Token(#[from] TokenError),
}

async fn websocket_upgrade(
    State(state): State<WebSocketState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !peer.ip().is_loopback() || headers.contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let server = state.server;
    upgrade.on_upgrade(move |socket| async move {
        let _ = server.serve_stream(WebSocketStream { socket }).await;
    })
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    token.len() == expected.len() && bool::from(token.as_bytes().ct_eq(expected.as_bytes()))
}

fn standard_token_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/share/cookie_agent/daemon/token-v1"))
}

/// Loads the same private bearer token used by the localhost WebSocket daemon.
pub fn load_auth_token() -> Result<String, TokenError> {
    let path = standard_token_path().ok_or(TokenError::HomeUnavailable)?;
    load_or_create_token(&path)
}

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("home directory is unavailable")]
    HomeUnavailable,
    #[error("token path is unsafe")]
    UnsafePath,
    #[error("token file is invalid")]
    InvalidToken,
    #[error("token storage failed")]
    Io(#[source] io::Error),
}

fn load_or_create_token(path: &Path) -> Result<String, TokenError> {
    #[cfg(unix)]
    {
        load_or_create_token_unix(path)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(TokenError::UnsafePath)
    }
}

#[cfg(unix)]
fn load_or_create_token_unix(path: &Path) -> Result<String, TokenError> {
    if path.as_os_str().is_empty()
        || path.file_name().and_then(|name| name.to_str()) != Some(TOKEN_FILE)
    {
        return Err(TokenError::UnsafePath);
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(TokenError::HomeUnavailable)?;
    let expected = home.join(".local/share/cookie_agent/daemon/token-v1");
    #[cfg(not(test))]
    if path != expected {
        return Err(TokenError::UnsafePath);
    }
    let parent = if path == expected {
        let home = open_trusted_token_anchor(&home)?;
        let local = open_or_create_safe_anchor_dir(&home, ".local")?;
        let share = open_or_create_safe_anchor_dir(&local, "share")?;
        let cookie_agent = open_or_create_private_dir(&share, "cookie_agent")?;
        open_or_create_private_dir(&cookie_agent, "daemon")?
    } else {
        #[cfg(test)]
        {
            let parent_path = path.parent().ok_or(TokenError::UnsafePath)?;
            fs::create_dir_all(parent_path).map_err(TokenError::Io)?;
            let directory = fs::File::open(parent_path).map_err(TokenError::Io)?;
            rustix::fs::fchmod(&directory, rustix::fs::Mode::RWXU)
                .map_err(|error| TokenError::Io(error.into()))?;
            directory
        }
        #[cfg(not(test))]
        unreachable!()
    };
    load_or_create_token_from_parent(&parent)
}

#[cfg(unix)]
fn load_or_create_token_from_parent(parent: &fs::File) -> Result<String, TokenError> {
    if let Some(mut file) = open_token_file(parent)? {
        return read_token(&mut file);
    }
    let token = generate_token()?;
    let temporary = format!(".token-v1.tmp-{}", &token[..12]);
    let mut file = create_token_file(parent, &temporary)?;
    let result = (|| {
        file.write_all(token.as_bytes()).map_err(TokenError::Io)?;
        file.sync_all().map_err(TokenError::Io)?;
        match rustix::fs::renameat_with(
            parent,
            temporary.as_str(),
            parent,
            TOKEN_FILE,
            rustix::fs::RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {
                rustix::fs::unlinkat(parent, temporary.as_str(), rustix::fs::AtFlags::empty())
                    .map_err(|error| TokenError::Io(error.into()))?;
                let mut existing = open_token_file(parent)?.ok_or(TokenError::UnsafePath)?;
                return read_token(&mut existing);
            }
            Err(error) => return Err(token_path_error(error)),
        }
        rustix::fs::fsync(parent).map_err(|error| TokenError::Io(error.into()))?;
        Ok(token)
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(parent, temporary.as_str(), rustix::fs::AtFlags::empty());
    }
    result
}

#[cfg(unix)]
fn open_trusted_token_anchor(path: &Path) -> Result<fs::File, TokenError> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(TokenError::UnsafePath);
    }
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let mut current = fs::File::from(
        rustix::fs::open("/", flags, rustix::fs::Mode::empty()).map_err(token_path_error)?,
    );
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = fs::File::from(
                    rustix::fs::openat(&current, name, flags, rustix::fs::Mode::empty())
                        .map_err(token_path_error)?,
                );
            }
            _ => return Err(TokenError::UnsafePath),
        }
    }
    validate_safe_anchor(&current.metadata().map_err(TokenError::Io)?)?;
    Ok(current)
}

#[cfg(unix)]
fn open_or_create_safe_anchor_dir(parent: &fs::File, name: &str) -> Result<fs::File, TokenError> {
    match open_directory_at(parent, name) {
        Ok(directory) => {
            validate_safe_anchor(&directory.metadata().map_err(TokenError::Io)?)?;
            Ok(directory)
        }
        Err(TokenError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            rustix::fs::mkdirat(parent, name, rustix::fs::Mode::RWXU)
                .map_err(|error| TokenError::Io(error.into()))?;
            let directory = open_directory_at(parent, name)?;
            validate_private_directory(&directory.metadata().map_err(TokenError::Io)?)?;
            Ok(directory)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_or_create_private_dir(parent: &fs::File, name: &str) -> Result<fs::File, TokenError> {
    match open_directory_at(parent, name) {
        Ok(directory) => {
            validate_private_directory(&directory.metadata().map_err(TokenError::Io)?)?;
            Ok(directory)
        }
        Err(TokenError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            match rustix::fs::mkdirat(parent, name, rustix::fs::Mode::RWXU) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(TokenError::Io(error.into())),
            }
            let directory = open_directory_at(parent, name)?;
            rustix::fs::fchmod(&directory, rustix::fs::Mode::RWXU)
                .map_err(|error| TokenError::Io(error.into()))?;
            validate_private_directory(&directory.metadata().map_err(TokenError::Io)?)?;
            Ok(directory)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_directory_at(parent: &fs::File, name: &str) -> Result<fs::File, TokenError> {
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty())
        .map(fs::File::from)
        .map_err(token_path_error)
}

#[cfg(unix)]
fn open_token_file(parent: &fs::File) -> Result<Option<fs::File>, TokenError> {
    let flags =
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC;
    match rustix::fs::openat(parent, TOKEN_FILE, flags, rustix::fs::Mode::empty()) {
        Ok(file) => {
            let file = fs::File::from(file);
            validate_token_file(&file.metadata().map_err(TokenError::Io)?)?;
            Ok(Some(file))
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(token_path_error(error)),
    }
}

#[cfg(unix)]
fn create_token_file(parent: &fs::File, name: &str) -> Result<fs::File, TokenError> {
    let flags = rustix::fs::OFlags::WRONLY
        | rustix::fs::OFlags::CREATE
        | rustix::fs::OFlags::EXCL
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let mode = rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR;
    let file =
        fs::File::from(rustix::fs::openat(parent, name, flags, mode).map_err(token_path_error)?);
    rustix::fs::fchmod(&file, mode).map_err(|error| TokenError::Io(error.into()))?;
    Ok(file)
}

#[cfg(unix)]
fn validate_safe_anchor(metadata: &fs::Metadata) -> Result<(), TokenError> {
    use std::os::unix::fs::MetadataExt as _;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        Err(TokenError::UnsafePath)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn validate_private_directory(metadata: &fs::Metadata) -> Result<(), TokenError> {
    use std::os::unix::fs::MetadataExt as _;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        Err(TokenError::UnsafePath)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn validate_token_file(metadata: &fs::Metadata) -> Result<(), TokenError> {
    use std::os::unix::fs::MetadataExt as _;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != TOKEN_ENCODED_BYTES as u64
    {
        Err(TokenError::UnsafePath)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn token_path_error(error: rustix::io::Errno) -> TokenError {
    let error: io::Error = error.into();
    if matches!(error.raw_os_error(), Some(code) if code == libc::ELOOP || code == libc::ENOTDIR) {
        TokenError::UnsafePath
    } else {
        TokenError::Io(error)
    }
}

fn read_token(file: &mut fs::File) -> Result<String, TokenError> {
    let mut token = String::new();
    file.take((TOKEN_ENCODED_BYTES + 1) as u64)
        .read_to_string(&mut token)
        .map_err(TokenError::Io)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(token.as_bytes())
        .map_err(|_| TokenError::InvalidToken)?;
    if token.len() != TOKEN_ENCODED_BYTES || decoded.len() != TOKEN_BYTES {
        return Err(TokenError::InvalidToken);
    }
    Ok(token)
}

fn generate_token() -> Result<String, TokenError> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    let mut filled = 0;
    while filled < bytes.len() {
        let initialized =
            rustix::rand::getrandom(&mut bytes[filled..], rustix::rand::GetRandomFlags::empty())
                .map_err(|error| TokenError::Io(error.into()))?;
        if initialized == 0 {
            return Err(TokenError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "secure random source returned no bytes",
            )));
        }
        filled += initialized;
    }
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

enum RouteResult {
    Handshake,
    Value(Value),
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

#[derive(Debug)]
struct RpcFault {
    code: i32,
    message: &'static str,
    data: Option<Value>,
}

impl RpcFault {
    const fn parse_error() -> Self {
        Self {
            code: -32700,
            message: "parse error",
            data: None,
        }
    }

    fn invalid_request(detail: &'static str) -> Self {
        Self {
            code: -32600,
            message: "invalid request",
            data: Some(json!({ "detail": detail })),
        }
    }

    const fn method_not_found() -> Self {
        Self {
            code: -32601,
            message: "method not found",
            data: None,
        }
    }

    const fn invalid_params() -> Self {
        Self {
            code: -32602,
            message: "invalid params",
            data: None,
        }
    }

    fn run_start_conflict(params: &RunStartParams) -> Self {
        Self {
            code: -32602,
            message: "idempotency conflict",
            data: typed_error_data(RunStartConflict {
                code: RunStartConflictCode::IdempotencyConflict,
                session_id: params.session_id,
                client_run_id: params.client_run_id.clone(),
            }),
        }
    }

    const fn handshake_required() -> Self {
        Self {
            code: -32001,
            message: "handshake required",
            data: None,
        }
    }

    const fn request_id_required() -> Self {
        Self {
            code: -32600,
            message: "request id required",
            data: None,
        }
    }

    const fn engine() -> Self {
        Self {
            code: -32000,
            message: "engine error",
            data: None,
        }
    }

    const fn internal() -> Self {
        Self {
            code: -32603,
            message: "internal error",
            data: None,
        }
    }

    fn catalog(code: CatalogErrorCode, revision: Option<CatalogRevision>) -> Self {
        Self {
            code: -32010,
            message: "catalog error",
            data: typed_error_data(CatalogError { code, revision }),
        }
    }

    fn provider_connect_parts(
        provider_id: &ProviderId,
        client_connect_id: &ClientConnectId,
        code: ProviderConnectErrorCode,
        missing_credential_fields: Vec<CredentialFieldName>,
    ) -> Self {
        Self {
            code: -32011,
            message: "provider connect error",
            data: typed_error_data(ProviderConnectError {
                code,
                provider_id: provider_id.clone(),
                client_connect_id: client_connect_id.clone(),
                missing_credential_fields,
            }),
        }
    }

    fn model_list(code: ModelListErrorCode) -> Self {
        Self {
            code: -32012,
            message: "model list error",
            data: typed_error_data(ModelListError { code }),
        }
    }

    fn rename(request: &SessionRenameParams, code: SessionRenameErrorCode) -> Self {
        Self::rename_parts(request.session_id, request.client_rename_id.clone(), code)
    }

    fn rename_parts(
        session_id: SessionId,
        client_rename_id: ClientRenameId,
        code: SessionRenameErrorCode,
    ) -> Self {
        Self {
            code: -32602,
            message: "session rename rejected",
            data: typed_error_data(SessionRenameError {
                code,
                session_id,
                client_rename_id,
            }),
        }
    }

    fn approval(
        request: &ApprovalRespondParams,
        code: ApprovalRespondErrorCode,
        expected_revision: Option<u64>,
        found_revision: Option<u64>,
    ) -> Self {
        Self {
            code: -32602,
            message: "approval response rejected",
            data: typed_error_data(ApprovalRespondError {
                code,
                session_id: request.session_id,
                approval_id: request.approval_id,
                client_response_id: request.client_response_id.clone(),
                expected_revision,
                found_revision,
            }),
        }
    }

    fn approval_failure(request: &ApprovalRespondParams, failure: &ApprovalRespondFailure) -> Self {
        let (expected_revision, found_revision) =
            if failure.code == ApprovalRespondErrorCode::ApprovalRevisionConflict {
                (failure.current_revision, Some(request.request_revision))
            } else {
                (None, None)
            };
        let client_response_id = ClientResponseId::new(failure.client_response_id.clone())
            .unwrap_or_else(|_| ClientResponseId::new("invalid-response-id").expect("static ID"));
        Self {
            code: -32602,
            message: "approval response rejected",
            data: typed_error_data(ApprovalRespondError {
                code: failure.code,
                session_id: failure.session_id,
                approval_id: failure.approval_id,
                client_response_id,
                expected_revision,
                found_revision,
            }),
        }
    }
}

fn typed_error_data(error: impl Serialize) -> Option<Value> {
    serde_json::to_value(error).ok()
}

fn engine_fault(_: EngineError) -> RpcFault {
    RpcFault::engine()
}

fn parse_incoming(frame: MessageFrame) -> Result<Value, RpcFault> {
    match frame {
        MessageFrame::Value(value) => Ok(value),
        MessageFrame::Text(text) => {
            serde_json::from_str(&text).map_err(|_| RpcFault::parse_error())
        }
    }
}

fn classify_incoming(value: Value) -> Result<Incoming, RpcFault> {
    let object = value
        .as_object()
        .ok_or_else(|| RpcFault::invalid_request("a batch or scalar is not a request"))?;
    if object.contains_key("id") {
        let request: RpcRequest = serde_json::from_value(value)
            .map_err(|_| RpcFault::invalid_request("invalid JSON-RPC request envelope"))?;
        if request.method.is_empty() {
            return Err(RpcFault::invalid_request("method must be non-empty"));
        }
        Ok(Incoming::Request {
            id: request.id,
            method: request.method,
            params: request.params,
        })
    } else {
        let notification: Notification = serde_json::from_value(value)
            .map_err(|_| RpcFault::invalid_request("invalid JSON-RPC notification envelope"))?;
        if notification.method.is_empty() {
            return Err(RpcFault::invalid_request("method must be non-empty"));
        }
        Ok(Incoming::Notification {
            method: notification.method,
            params: notification.params,
        })
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

fn decode_rename_params(params: Option<Value>) -> Result<SessionRenameParams, RpcFault> {
    let params = params.unwrap_or(Value::Null);
    if let Ok(request) = serde_json::from_value::<SessionRenameParams>(params.clone()) {
        return Ok(request);
    }
    if serde_json::to_vec(&params)
        .map_err(|_| RpcFault::invalid_params())?
        .len()
        > MAX_RAW_RENAME_PARAMS_BYTES
    {
        return Err(RpcFault::invalid_params());
    }
    let raw: RawSessionRenameParams =
        serde_json::from_value(params).map_err(|_| RpcFault::invalid_params())?;
    let change = match raw.change {
        RawSessionRenameChange::Set { title } => SessionRenameChange::Set {
            title: SessionTitle::new(title).map_err(|_| {
                RpcFault::rename_parts(
                    raw.session_id,
                    raw.client_rename_id.clone(),
                    SessionRenameErrorCode::InvalidTitle,
                )
            })?,
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

fn decode_params<T: DeserializeOwned>(params: Option<Value>) -> Result<T, RpcFault> {
    serde_json::from_value(params.unwrap_or(Value::Null)).map_err(|_| RpcFault::invalid_params())
}

fn params_or_default<T>(params: Option<Value>) -> Result<T, RpcFault>
where
    T: DeserializeOwned + Default,
{
    match params {
        Some(params) => serde_json::from_value(params).map_err(|_| RpcFault::invalid_params()),
        None => Ok(T::default()),
    }
}

fn value<T: Serialize>(value: T) -> Result<RouteResult, RpcFault> {
    serde_json::to_value(value)
        .map(RouteResult::Value)
        .map_err(|_| RpcFault::internal())
}

fn success_response<T: Serialize>(id: JsonRpcId, result: &T) -> Result<Value, TransportError> {
    serde_json::to_value(RpcResponse::Success(SuccessResponse {
        jsonrpc: JsonRpcVersion::current(),
        id,
        result: serde_json::to_value(result).map_err(|_| TransportError::Closed)?,
    }))
    .map_err(|_| TransportError::Closed)
}

fn error_response(id: Option<JsonRpcId>, fault: RpcFault) -> Result<Value, TransportError> {
    match id {
        Some(id) => serde_json::to_value(RpcResponse::Error(ErrorResponse {
            jsonrpc: JsonRpcVersion::current(),
            id,
            error: JsonRpcError {
                code: fault.code,
                message: fault.message.into(),
                data: fault.data,
            },
        }))
        .map_err(|_| TransportError::Closed),
        None => Ok(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": fault.code, "message": fault.message, "data": fault.data },
        })),
    }
}

async fn send_notification<T: Serialize>(
    sender: &mpsc::Sender<Value>,
    shutdown: &CancellationToken,
    method: &str,
    params: &T,
) -> Result<(), ()> {
    let notification = serde_json::to_value(Notification::new(
        method,
        Some(serde_json::to_value(params).map_err(|_| ())?),
    ))
    .map_err(|_| ())?;
    tokio::select! {
        _ = shutdown.cancelled() => Err(()),
        result = sender.send(notification) => result.map_err(|_| ()),
    }
}

async fn forward_output(
    stream: OutputStream,
    snapshot: cookie_agent_protocol::OutputSnapshot,
    mut receiver: mpsc::Receiver<OutputMessage>,
    notifications: mpsc::Sender<Value>,
    shutdown: CancellationToken,
) {
    let held_delta = match receiver.try_recv() {
        Ok(OutputMessage::Gap(gap)) => {
            if send_notification(&notifications, &shutdown, "events.tool_output_gap", &gap)
                .await
                .is_err()
            {
                return;
            }
            None
        }
        Ok(OutputMessage::Delta(delta)) => Some(delta),
        Err(_) => None,
    };
    if send_notification(
        &notifications,
        &shutdown,
        "events.tool_output_snapshot",
        &OutputSnapshotEnvelope { stream, snapshot },
    )
    .await
    .is_err()
    {
        return;
    }
    if let Some(delta) = held_delta
        && send_notification(
            &notifications,
            &shutdown,
            "events.tool_output_delta",
            &delta,
        )
        .await
        .is_err()
    {
        return;
    }
    loop {
        let message = tokio::select! {
            _ = shutdown.cancelled() => return,
            message = receiver.recv() => match message {
                Some(message) => message,
                None => return,
            },
        };
        let result = match message {
            OutputMessage::Delta(delta) => {
                send_notification(
                    &notifications,
                    &shutdown,
                    "events.tool_output_delta",
                    &delta,
                )
                .await
            }
            OutputMessage::Gap(gap) => {
                send_notification(&notifications, &shutdown, "events.tool_output_gap", &gap).await
            }
        };
        if result.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt as _};

    use cookie_agent_config::load_from_roots;
    use cookie_agent_engine::EngineOptions;
    use cookie_agent_models::{CredentialStore, MODELS_DEV_ARTIFACT_SHA256};
    use cookie_agent_protocol::{AgentId, ModelSelection, RunSelection};
    use futures_util::{SinkExt as _, StreamExt as _};
    use tempfile::TempDir;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    use super::*;

    struct Harness {
        _directory: TempDir,
        engine: Engine,
        server: Arc<Server>,
        selection: RunSelection,
    }

    fn write_agent(root: &Path, model: &str) {
        fs::create_dir_all(root.join("agents")).expect("create agents");
        fs::write(
            root.join("agents/primary.md"),
            format!(
                "---\nschema: 1\ndescription: Primary test agent\nmode: primary\nenabled: true\nmodel_fallback: [{{ model: \"{model}\" }}]\ntools: []\npermissions: []\n---\nTest system prompt.\n"
            ),
        )
        .expect("write agent");
    }

    fn harness(credential_store: bool) -> Harness {
        let directory = TempDir::new().expect("tempdir");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private tempdir");
        let root = directory.path().join(".cookie-agent");
        fs::create_dir(&root).expect("create root");
        let (config, model) = if credential_store {
            (
                format!(
                    "schema_version = 6\n[providers.openai]\nsource = \"models_dev\"\ncatalog_revision = \"sha256:{MODELS_DEV_ARTIFACT_SHA256}\"\nauth = {{ type = \"credential_store\" }}\n[providers.openai.models.\"gpt-5.6-sol\"]\n"
                ),
                "openai/gpt-5.6-sol",
            )
        } else {
            (
                "schema_version = 6\n[providers.test]\nsource = \"explicit\"\nendpoint = \"https://example.test/v1\"\nadaptor = \"openai-compatible\"\nauth = { type = \"none\" }\n[providers.test.models.model]\ndisplay_name = \"Model\"\ndefault_variant = \"fast\"\n[providers.test.models.model.capabilities]\ninput = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 8192\noutput_tokens = 2048\ntool_calling = true\nparallel_tool_calls = false\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = true\nnative_replay = \"unsupported\"\nnative_compaction = \"unsupported\"\ncancellation = \"local_only\"\nmedia = {}\n[providers.test.models.model.variants.fast]\noperation = \"add\"\ndefaults = { temperature = 0.1 }\n".into(),
                "test/model",
            )
        };
        fs::write(root.join("config.toml"), config).expect("write config");
        write_agent(&root, model);
        let loaded = load_from_roots(None, Some(&root)).expect("load config");
        let catalog = Arc::new(Catalog::embedded().expect("catalog"));
        let model_manager = Arc::new(
            ModelSetManager::new(
                loaded.runtime.providers.clone(),
                Arc::clone(&catalog),
                CredentialStore::new(directory.path().join("credentials")),
            )
            .expect("model manager"),
        );
        let model = model.parse().expect("model key");
        let variant = model_manager
            .current()
            .model_set()
            .get(&model)
            .expect("model")
            .default_variant()
            .cloned();
        let selection = RunSelection {
            agent: AgentId::new("primary").expect("agent"),
            model: ModelSelection { model, variant },
        };
        let loaded = Arc::new(loaded);
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: (*loaded).clone(),
            model_manager: Arc::clone(&model_manager),
            tools: Vec::new(),
        })
        .expect("engine");
        let server = Arc::new(Server {
            token_path: directory.path().join("daemon/token-v1"),
            ..Server::new(engine.clone(), model_manager, catalog, loaded)
        });
        Harness {
            _directory: directory,
            engine,
            server,
            selection,
        }
    }

    async fn request(stream: &mut InProcessStream, id: i64, method: &str, params: Value) -> Value {
        stream
            .send(MessageFrame::Value(json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params
            })))
            .await
            .expect("send");
        loop {
            let frame = stream.recv().await.expect("recv").expect("open");
            let value = match frame {
                MessageFrame::Value(value) => value,
                MessageFrame::Text(text) => serde_json::from_str(&text).expect("JSON"),
            };
            if value.get("id") == Some(&json!(id)) {
                return value;
            }
        }
    }

    async fn handshake(stream: &mut InProcessStream) {
        let response = request(stream, 1, "handshake", json!({ "protocol_version": 7 })).await;
        assert_eq!(response["result"]["protocol_version"], 7);
    }

    #[tokio::test]
    async fn v7_handshake_is_required_and_v6_is_rejected() {
        let harness = harness(false);
        let (mut client, server_stream) = in_process_pair(8);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        let blocked = request(&mut client, 1, "model.list", json!({})).await;
        assert_eq!(blocked["error"]["code"], -32001);
        let old = request(
            &mut client,
            2,
            "handshake",
            json!({ "protocol_version": 6 }),
        )
        .await;
        assert_eq!(old["error"]["code"], -32602);
        handshake(&mut client).await;
        drop(client);
        task.await.expect("join").expect("serve");
        harness.engine.shutdown().await;
    }

    #[tokio::test]
    async fn model_agent_catalog_and_session_projections_are_v7_and_exact() {
        let harness = harness(false);
        let (mut client, server_stream) = in_process_pair(16);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;

        let models = request(&mut client, 2, "model.list", json!({})).await["result"].clone();
        assert_eq!(models["models"][0]["key"], "test/model");
        assert_eq!(models["models"][0]["default_variant"], "fast");
        assert_eq!(models["models"][0]["variants"][0]["id"], "fast");
        assert!(models["revision"].as_str().unwrap().starts_with("sha256:"));

        let agents = request(&mut client, 3, "agent.list", json!({})).await["result"].clone();
        assert_eq!(agents["model_revision"], models["revision"]);
        assert_eq!(agents["agents"][0]["id"], "primary");
        assert_eq!(
            agents["agents"][0]["resolved_fallback"][0]["variant"],
            "fast"
        );

        let catalog =
            request(&mut client, 4, "catalog.provider.list", json!({})).await["result"].clone();
        assert_eq!(
            catalog["snapshot"]["revision"],
            CatalogRevision::current().to_string()
        );
        let ids = catalog["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|provider| provider["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(ids.is_empty());
        let catalog_models = request(
            &mut client,
            5,
            "catalog.model.list",
            json!({ "provider_id": "anthropic" }),
        )
        .await["result"]
            .clone();
        let catalog_models: CatalogModelListResult =
            serde_json::from_value(catalog_models).expect("valid catalog model projection");
        assert!(!catalog_models.models.is_empty());
        assert!(catalog_models.models.iter().all(|model| {
            model.provider_id.as_str() == "anthropic"
                && model.limits.context > 0
                && model.limits.output > 0
        }));

        let created = request(
            &mut client,
            6,
            "session.create",
            serde_json::to_value(SessionCreateParams {
                selection: harness.selection.clone(),
            })
            .unwrap(),
        )
        .await["result"]["session"]
            .clone();
        assert_eq!(created["creation_selection"]["agent"], "primary");
        assert_eq!(created["title_updated_seq"], 0);
        assert_eq!(created["last_event_seq"], 1);

        drop(client);
        task.await.expect("join").expect("serve");
        harness.engine.shutdown().await;
    }

    #[tokio::test]
    async fn title_sequence_and_stored_event_projection_are_monotonic() {
        let harness = harness(false);
        let session = harness
            .engine
            .create_session(harness.selection.clone())
            .expect("session");
        let (mut client, server_stream) = in_process_pair(16);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;

        let renamed = request(
            &mut client,
            2,
            "session.rename",
            json!({
                "session_id": session.session_id,
                "client_rename_id": "rename-1",
                "change": { "type": "set", "title": "New title" }
            }),
        )
        .await["result"]["session"]
            .clone();
        assert_eq!(renamed["title"], "New title");
        assert_eq!(renamed["title_updated_seq"], renamed["last_event_seq"]);

        let events = request(
            &mut client,
            3,
            "events.subscribe",
            json!({ "session_id": session.session_id, "cursor": null }),
        )
        .await["result"]["events"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(events[0]["event_schema_version"], 7);
        assert_eq!(events[0]["seq"], 1);
        assert_eq!(
            events.last().unwrap()["payload"]["type"],
            "session_title_committed"
        );

        drop(client);
        task.await.expect("join").expect("serve");
        harness.engine.shutdown().await;
    }

    #[tokio::test]
    async fn approval_and_tool_routes_keep_exact_v7_fields() {
        let harness = harness(false);
        let session = harness
            .engine
            .create_session(harness.selection.clone())
            .expect("session");
        let (mut client, server_stream) = in_process_pair(16);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;

        let approval = request(
            &mut client,
            2,
            "approval.respond",
            json!({
                "session_id": session.session_id,
                "approval_id": cookie_agent_protocol::ApprovalId::new_v7(),
                "request_revision": 1,
                "operation_fingerprint": { "digest": "00".repeat(32) },
                "client_response_id": "response-1",
                "decision": "approve_once",
                "feedback": null
            }),
        )
        .await;
        assert_eq!(approval["error"]["data"]["code"], "approval_not_found");

        let old_tool_stdin = request(
            &mut client,
            3,
            "run.tool_stdin",
            json!({
                "run_id": cookie_agent_protocol::RunId::new_v7(),
                "tool_call_id": cookie_agent_protocol::ToolCallId::new_v7(),
                "input": "legacy"
            }),
        )
        .await;
        assert_eq!(old_tool_stdin["error"]["code"], -32602);

        drop(client);
        task.await.expect("join").expect("serve");
        harness.engine.shutdown().await;
    }

    #[tokio::test]
    async fn provider_connect_uses_only_declared_credential_store_providers() {
        let harness = harness(true);
        let (mut client, server_stream) = in_process_pair(16);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;
        let params = json!({
            "client_connect_id": "connect-1",
            "provider_id": "openai",
            "catalog_revision": CatalogRevision::current(),
            "credentials": { "values": { "OPENAI_API_KEY": "sentinel-secret" } }
        });
        let first = request(&mut client, 2, "provider.connect", params.clone()).await;
        let replay = request(&mut client, 3, "provider.connect", params).await;
        assert_eq!(first["result"], replay["result"]);
        assert_eq!(first["result"]["connection"]["provider_id"], "openai");
        assert!(!first.to_string().contains("sentinel-secret"));
        let agents = request(&mut client, 5, "agent.list", json!({})).await;
        assert_eq!(
            agents["result"]["model_revision"],
            first["result"]["model_revision"]
        );
        assert_eq!(agents["result"]["agents"][0]["runnable_as_root"], true);
        let created = request(
            &mut client,
            6,
            "session.create",
            serde_json::to_value(SessionCreateParams {
                selection: harness.selection.clone(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(
            created["result"]["session"]["creation_selection"]["agent"],
            "primary"
        );

        let catalog = request(&mut client, 7, "catalog.provider.list", json!({})).await;
        assert_eq!(catalog["result"]["providers"].as_array().unwrap().len(), 1);
        assert_eq!(catalog["result"]["providers"][0]["id"], "openai");

        let missing = request(
            &mut client,
            8,
            "provider.connect",
            json!({
                "client_connect_id": "missing",
                "provider_id": "openai",
                "catalog_revision": CatalogRevision::current(),
                "credentials": { "values": {} }
            }),
        )
        .await;
        assert_eq!(missing["error"]["data"]["code"], "missing_credential");
        assert_eq!(
            missing["error"]["data"]["missing_credential_fields"],
            json!(["OPENAI_API_KEY"])
        );

        let unknown = request(
            &mut client,
            9,
            "provider.connect",
            json!({
                "client_connect_id": "unknown",
                "provider_id": "unknown",
                "catalog_revision": CatalogRevision::current(),
                "credentials": { "values": { "OPENAI_API_KEY": "secret" } }
            }),
        )
        .await;
        assert_eq!(unknown["error"]["data"]["code"], "unknown_provider");

        drop(client);
        task.await.expect("join").expect("serve");
        harness.engine.shutdown().await;
    }

    #[tokio::test]
    async fn configured_explicit_provider_connect_is_unsupported_not_unknown() {
        let harness = harness(false);
        let (mut client, server_stream) = in_process_pair(8);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;
        let response = request(
            &mut client,
            2,
            "provider.connect",
            json!({
                "client_connect_id": "explicit",
                "provider_id": "test",
                "catalog_revision": CatalogRevision::current(),
                "credentials": { "values": {} }
            }),
        )
        .await;
        assert_eq!(response["error"]["data"]["code"], "unsupported_provider");
        assert_eq!(
            response["error"]["data"]["missing_credential_fields"],
            json!([])
        );
        drop(client);
        task.await.expect("join").expect("serve");
        harness.engine.shutdown().await;
    }

    #[test]
    fn auth_is_strict_and_debug_redacts_frames() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer correct".parse().unwrap());
        assert!(authorized(&headers, "correct"));
        assert!(!authorized(&headers, "incorrect"));
        headers.insert(header::ORIGIN, "https://example.test".parse().unwrap());
        assert!(headers.contains_key(header::ORIGIN));

        let sentinel = "SENTINEL_SECRET";
        let frame = MessageFrame::Value(json!({ "credentials": sentinel }));
        assert_eq!(format!("{frame:?}"), "MessageFrame::Value(<redacted>)");
        assert!(!format!("{frame:?}").contains(sentinel));
    }

    #[tokio::test]
    async fn authenticated_websocket_transport_serves_v7_and_rejects_origin() {
        let harness = harness(false);
        let token = load_or_create_token(&harness.server.token_path).expect("token");
        let running = harness.server.clone().serve(0).await.expect("serve");
        let url = format!("ws://{}/ws", running.address());

        let mut request = url.clone().into_client_request().unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("authenticated websocket");
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "handshake",
                    "params": { "protocol_version": 7 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let response = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["result"]["protocol_version"], 7);
        drop(socket);

        let mut origin = url.into_client_request().unwrap();
        origin
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        origin
            .headers_mut()
            .insert("origin", "https://example.test".parse().unwrap());
        let error = tokio_tungstenite::connect_async(origin)
            .await
            .expect_err("origin must be rejected");
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            panic!("expected HTTP rejection")
        };
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        harness.server.shutdown();
        running.wait().await;
        harness.engine.shutdown().await;
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_private_and_links_are_rejected() {
        let directory = TempDir::new().expect("tempdir");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("daemon/token-v1");
        let token = load_or_create_token(&path).expect("create token");
        assert_eq!(token.len(), TOKEN_ENCODED_BYTES);
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

        fs::remove_file(&path).unwrap();
        let target = directory.path().join("target");
        fs::write(&target, "not-a-token").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(matches!(
            load_or_create_token(&path),
            Err(TokenError::UnsafePath)
        ));
    }

    #[test]
    fn old_profile_alias_and_approval_shapes_are_invalid() {
        assert!(
            serde_json::from_value::<SessionCreateParams>(json!({
                "cwd": "/tmp", "profile": "primary"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ApprovalRespondParams>(json!({
                "session_id": SessionId::new_v7(),
                "approval_id": cookie_agent_protocol::ApprovalId::new_v7(),
                "decision": "once"
            }))
            .is_err()
        );
    }

    #[test]
    fn manager_request_moves_and_redacts_credentials() {
        let request: ProviderConnectParams = serde_json::from_value(json!({
            "client_connect_id": "move",
            "provider_id": "openai",
            "catalog_revision": CatalogRevision::current(),
            "credentials": { "values": { "OPENAI_API_KEY": "sentinel" } }
        }))
        .unwrap();
        let manager = into_manager_connect_request(request);
        assert_eq!(
            manager.credentials,
            BTreeMap::from([("OPENAI_API_KEY".into(), "sentinel".into())])
        );
        assert!(!format!("{manager:?}").contains("sentinel"));
    }
}
