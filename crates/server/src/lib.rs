//! Transport-neutral JSON-RPC server for the cookie agent engine.

use std::{
    env, fmt, fs,
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(test)]
use std::sync::Mutex;

use async_trait::async_trait;
use axum::{
    Router,
    extract::{ConnectInfo, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cookie_agent_engine::{ApprovalRespondFailure, Engine, EngineError, events::OutputMessage};
use cookie_agent_models::{
    Catalog, CatalogModel as RuntimeCatalogModel, CatalogModelStatus as RuntimeCatalogModelStatus,
    CatalogProvider as RuntimeCatalogProvider, CredentialConnectRequest, CredentialStoreError,
    ModelSetManager, ModelSetManagerError,
};
use cookie_agent_protocol::{
    AgentListParams, AgentListResult, AgentType, ApprovalListParams, ApprovalRespondError,
    ApprovalRespondErrorCode, ApprovalRespondParams, CatalogError, CatalogErrorCode, CatalogModel,
    CatalogModelCapabilities, CatalogModelLimits, CatalogModelListParams, CatalogModelListResult,
    CatalogModelModalities, CatalogModelStatus, CatalogProvider, CatalogProviderListParams,
    CatalogProviderListResult, CatalogSnapshot, ClientHello, ClientRenameId, ErrorResponse, Event,
    EventSubscriptionMessage, EventsSubscribeParams, JsonRpcError, JsonRpcId, JsonRpcVersion,
    ModelListError, ModelListErrorCode, ModelListParams, ModelListResult, ModelRef, Notification,
    OutputSnapshotEnvelope, OutputStream, ProtocolVersion, ProviderConnectError,
    ProviderConnectErrorCode, ProviderConnectParams, ProviderConnectResult, ProviderConnection,
    Request as RpcRequest, Response as RpcResponse, RunCancelParams, RunStartConflict,
    RunStartConflictCode, RunStartParams, RunSteerParams, RunToolStdinParams, ServerHello,
    SessionChildrenParams, SessionChildrenResult, SessionCreateParams, SessionCreateResult,
    SessionGetParams, SessionGetResult, SessionId, SessionListParams, SessionListResult,
    SessionRenameChange, SessionRenameError, SessionRenameErrorCode, SessionRenameParams,
    SessionResumeParams, SessionResumeResult, SessionTitle, SessionTreeParams, SessionTreeResult,
    SuccessResponse,
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

/// Cancels one connection's subscription and output-forwarding tasks whenever
/// its routing future is dropped, including on panic or task abort.
struct ConnectionShutdown(CancellationToken);

impl Drop for ConnectionShutdown {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// One complete JSON-RPC message. WebSockets use [`Self::Text`]; the
/// in-process adapter passes [`Self::Value`] directly and avoids a JSON
/// serialization round-trip.
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

/// A transport-independent, message-boundary-preserving JSON-RPC stream.
#[async_trait]
pub trait MessageStream: Send {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError>;
}

/// Transport-level failures, separate from JSON-RPC error responses.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport closed")]
    Closed,
    #[error("websocket error: {0}")]
    WebSocket(#[from] axum::Error),
}

/// One endpoint of the in-process transport. [`in_process_pair`] returns a
/// client endpoint followed by its server endpoint.
pub struct InProcessStream {
    sender: mpsc::Sender<MessageFrame>,
    receiver: mpsc::Receiver<MessageFrame>,
}

/// Creates paired in-memory message streams for embedding a daemon.
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
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(TransportError::from(error)),
            }
        }
    }
}

/// Transport-facing composition of the engine and its shared runtime model manager.
#[derive(Clone)]
pub struct Server {
    engine: Engine,
    model_manager: Arc<ModelSetManager>,
    catalog: Arc<Catalog>,
    shutdown: CancellationToken,
    token_path: PathBuf,
    #[cfg(test)]
    connection_observer: Arc<Mutex<Option<mpsc::Sender<CancellationToken>>>>,
}

impl Server {
    #[must_use]
    pub fn new(engine: Engine, model_manager: Arc<ModelSetManager>, catalog: Arc<Catalog>) -> Self {
        Self {
            engine,
            model_manager,
            catalog,
            shutdown: CancellationToken::new(),
            token_path: standard_token_path().unwrap_or_default(),
            #[cfg(test)]
            connection_observer: Arc::new(Mutex::new(None)),
        }
    }

    /// Builds the WebSocket router. The listener is deliberately created by
    /// [`Self::serve`] so it can enforce the localhost-only binding policy.
    pub fn router(self: Arc<Self>) -> Result<Router, ServerError> {
        let token = Arc::new(load_or_create_token(&self.token_path)?);
        Ok(Router::new()
            .route("/ws", get(websocket_upgrade))
            .with_state(WebSocketState {
                server: self,
                token,
            }))
    }

    /// Starts the default localhost-only WebSocket transport. `port = 0`
    /// requests an ephemeral localhost port, which is useful to embedders and
    /// integration tests.
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

    /// Requests graceful termination of all listeners and connections created
    /// from this server. Existing engine runs are detached: they remain owned
    /// by the engine and are not cancelled by a transport shutdown.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    /// Runs JSON-RPC routing over any supported message stream. This is the
    /// entry point used by the in-process transport and WebSocket adapter.
    pub async fn serve_stream<S>(self: Arc<Self>, mut stream: S) -> Result<(), TransportError>
    where
        S: MessageStream,
    {
        let (notifications, mut notification_rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let mut handshaken = false;
        let connection_shutdown = self.shutdown.child_token();
        let _connection_shutdown = ConnectionShutdown(connection_shutdown.clone());
        #[cfg(test)]
        if let Some(observer) = self
            .connection_observer
            .lock()
            .expect("connection observer lock poisoned")
            .take()
        {
            let _ = observer.try_send(connection_shutdown.clone());
        }

        let result: Result<(), TransportError> = async {
            loop {
            tokio::select! {
                _ = connection_shutdown.cancelled() => break Ok(()),
                incoming = stream.recv() => {
                    let Some(frame) = incoming? else { break Ok(()); };
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
                    stream.send(MessageFrame::Value(notification)).await?
                }
            }
            }
        }
        .await;
        result
    }

    #[cfg(test)]
    fn observe_next_connection(&self, observer: mpsc::Sender<CancellationToken>) {
        *self
            .connection_observer
            .lock()
            .expect("connection observer lock poisoned") = Some(observer);
    }

    #[cfg(test)]
    fn with_token_path(mut self, token_path: PathBuf) -> Self {
        self.token_path = token_path;
        self
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
                    .create_session(request.cwd, &request.profile)
                    .map_err(engine_fault)?;
                value(SessionCreateResult { session })
            }
            "session.list" => {
                let request: SessionListParams = params_or_default(params)?;
                let sessions = self
                    .engine
                    .list_sessions()
                    .into_iter()
                    .filter(|session| request.cwd.as_ref().is_none_or(|cwd| &session.cwd == cwd))
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
                let params: RunStartParams = decode_params(params)?;
                let result = match self.engine.start_run(params.clone()).await {
                    Ok(result) => result,
                    Err(EngineError::RunIdempotencyConflict) => {
                        return Err(RpcFault::run_start_conflict(&params));
                    }
                    Err(error) => return Err(engine_fault(error)),
                };
                value(result)
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
                    if let Event::ToolCallStarted { tool_call_id, .. } = event.event {
                        self.start_output_tail(
                            tool_call_id,
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
                let request: ProviderConnectParams = decode_params(params)?;
                value(self.connect_provider(request)?)
            }
            "model.list" => {
                let _: ModelListParams = params_or_default(params)?;
                value(self.list_models()?)
            }
            "agent.list" => {
                let request: AgentListParams = params_or_default(params)?;
                if matches!(
                    request.agent_type,
                    Some(AgentType::SubAgent | AgentType::Internal)
                ) {
                    return Err(RpcFault::invalid_params());
                }
                let mut result: AgentListResult = self.engine.list_agents();
                if let Some(agent_type) = request.agent_type {
                    result.agents.retain(|agent| agent.agent_type == agent_type);
                }
                value(result)
            }
            _ => Err(RpcFault::method_not_found()),
        }
    }

    fn list_models(&self) -> Result<ModelListResult, RpcFault> {
        let snapshot = self.model_manager.current();
        let generated_at = snapshot
            .generated_at()
            .parse()
            .map_err(|_| RpcFault::model_list(ModelListErrorCode::ModelSnapshotInvalid))?;
        if snapshot.revision().is_empty() {
            return Err(RpcFault::model_list(
                ModelListErrorCode::ModelSnapshotInvalid,
            ));
        }
        Ok(ModelListResult {
            revision: snapshot.revision().to_owned(),
            generated_at,
            catalog_revision: snapshot.catalog_revision().map(ToOwned::to_owned),
            models: snapshot
                .model_set()
                .aliases()
                .filter_map(|alias| snapshot.model_set().get(alias))
                .map(|entry| ModelRef {
                    name: entry.alias().to_owned(),
                    provider_id: entry.descriptor().identity.provider_id.as_str().to_owned(),
                    model_id: entry.descriptor().identity.model_id.as_str().to_owned(),
                    adapter_id: entry.descriptor().adapter_id.as_str().to_owned(),
                })
                .collect(),
        })
    }

    fn list_catalog_providers(&self) -> Result<CatalogProviderListResult, RpcFault> {
        let snapshot = catalog_snapshot(&self.catalog)?;
        let providers = self
            .catalog
            .providers()
            .values()
            .map(project_catalog_provider)
            .collect();
        Ok(CatalogProviderListResult {
            snapshot,
            providers,
        })
    }

    fn list_catalog_models(
        &self,
        request: &CatalogModelListParams,
    ) -> Result<CatalogModelListResult, RpcFault> {
        let snapshot = catalog_snapshot(&self.catalog)?;
        let models = self
            .catalog
            .models()
            .iter()
            .filter(|model| {
                request
                    .provider_id
                    .as_ref()
                    .is_none_or(|provider_id| &model.provider_id == provider_id)
            })
            .map(project_catalog_model)
            .collect();
        Ok(CatalogModelListResult { snapshot, models })
    }

    fn connect_provider(
        &self,
        request: ProviderConnectParams,
    ) -> Result<ProviderConnectResult, RpcFault> {
        let provider = self.catalog.providers().get(&request.provider_id);
        if request.credentials.values.is_empty() {
            return Err(RpcFault::provider_connect(
                &request,
                ProviderConnectErrorCode::MissingCredential,
            ));
        }
        if let Some(provider) = provider
            && (request.credentials.values.len() != 1
                || request
                    .credentials
                    .values
                    .iter()
                    .any(|(field, credential)| {
                        credential.is_empty() || !provider.credential_fields.contains(field)
                    }))
        {
            return Err(RpcFault::provider_connect(
                &request,
                ProviderConnectErrorCode::InvalidCredential,
            ));
        }
        let manager_request = into_manager_connect_request(request);
        let receipt = self
            .model_manager
            .connect(&manager_request)
            .map_err(|error| provider_connect_fault(&manager_request, error))?;
        let connected_at = receipt
            .connected_at
            .parse()
            .map_err(|_| RpcFault::internal())?;
        Ok(ProviderConnectResult {
            client_connect_id: receipt.client_connect_id,
            connection: ProviderConnection {
                provider_id: receipt.provider_id,
                credential_fields: receipt.credential_fields,
                connected_at,
                catalog_revision: receipt.catalog_revision,
            },
            model_revision: receipt.model_revision,
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
                let call_id = match &message {
                    EventSubscriptionMessage::Event {
                        event:
                            cookie_agent_protocol::EventEnvelope {
                                event: Event::ToolCallStarted { tool_call_id, .. },
                                ..
                            },
                    } => Some(*tool_call_id),
                    _ => None,
                };
                if send_notification(&notifications, &shutdown, "events.subscription", &message)
                    .await
                    .is_err()
                {
                    return;
                }
                if let Some(call_id) = call_id {
                    server.start_output_tail(
                        call_id,
                        notifications.clone(),
                        shutdown.child_token(),
                    );
                }
            }
        });
    }

    fn start_output_tail(
        &self,
        call_id: cookie_agent_protocol::ToolCallId,
        notifications: mpsc::Sender<Value>,
        shutdown: CancellationToken,
    ) {
        let engine = self.engine.clone();
        tokio::spawn(async move {
            // The durable ToolCallStarted event is committed just before the
            // task installs its hub. A short bounded retry closes that gap.
            for _ in 0..10 {
                let stdout = engine.subscribe_tool_output(call_id, OutputStream::Stdout);
                let stderr = engine.subscribe_tool_output(call_id, OutputStream::Stderr);
                if stdout.is_some() || stderr.is_some() {
                    if let Some((snapshot, receiver)) = stdout {
                        let notifications = notifications.clone();
                        let output_shutdown = shutdown.child_token();
                        tokio::spawn(async move {
                            let _ = forward_output(
                                OutputStream::Stdout,
                                snapshot,
                                receiver,
                                notifications,
                                output_shutdown,
                            )
                            .await;
                        });
                    }
                    if let Some((snapshot, receiver)) = stderr {
                        let output_shutdown = shutdown.child_token();
                        tokio::spawn(async move {
                            let _ = forward_output(
                                OutputStream::Stderr,
                                snapshot,
                                receiver,
                                notifications,
                                output_shutdown,
                            )
                            .await;
                        });
                    }
                    return;
                }
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = sleep(Duration::from_millis(5)) => {},
                }
            }
        });
    }
}

fn into_manager_connect_request(request: ProviderConnectParams) -> CredentialConnectRequest {
    let ProviderConnectParams {
        client_connect_id,
        provider_id,
        catalog_revision,
        credentials,
    } = request;
    CredentialConnectRequest {
        client_connect_id,
        provider_id,
        catalog_revision,
        credentials: credentials.values,
    }
}

fn catalog_snapshot(catalog: &Catalog) -> Result<CatalogSnapshot, RpcFault> {
    let snapshot = catalog.snapshot();
    if snapshot.revision.is_empty() || snapshot.source.is_empty() {
        return Err(RpcFault::catalog(
            CatalogErrorCode::CatalogSnapshotInvalid,
            Some(snapshot.revision),
        ));
    }
    let fetched_at = snapshot.fetched_at.parse().map_err(|_| {
        RpcFault::catalog(
            CatalogErrorCode::CatalogSnapshotInvalid,
            Some(snapshot.revision.clone()),
        )
    })?;
    Ok(CatalogSnapshot {
        revision: snapshot.revision,
        source: snapshot.source,
        fetched_at,
    })
}

fn project_catalog_provider(provider: &RuntimeCatalogProvider) -> CatalogProvider {
    let mut credential_fields = provider.credential_fields.clone();
    credential_fields.sort();
    CatalogProvider {
        id: provider.id.clone(),
        name: provider.name.clone(),
        credential_fields,
        npm: Some(provider.npm.clone()),
        api: provider.api.clone(),
        documentation_url: Some(provider.documentation_url.clone()),
    }
}

fn project_catalog_model(model: &RuntimeCatalogModel) -> CatalogModel {
    CatalogModel {
        provider_id: model.provider_id.clone(),
        model_id: model.model_id.clone(),
        canonical_model_id: model.canonical_model_id.clone(),
        name: model.name.clone(),
        family: model.family.clone(),
        capabilities: CatalogModelCapabilities {
            attachment: model.capabilities.attachment,
            reasoning: model.capabilities.reasoning,
            tool_call: model.capabilities.tool_call,
            structured_output: model.capabilities.structured_output,
            temperature: model.capabilities.temperature,
        },
        limits: CatalogModelLimits {
            context: model.limits.context,
            input: model.limits.input,
            output: model.limits.output,
        },
        modalities: CatalogModelModalities {
            input: model.modalities.input.clone(),
            output: model.modalities.output.clone(),
        },
        status: match model.status {
            RuntimeCatalogModelStatus::Stable => CatalogModelStatus::Stable,
            RuntimeCatalogModelStatus::Alpha => CatalogModelStatus::Alpha,
            RuntimeCatalogModelStatus::Beta => CatalogModelStatus::Beta,
            RuntimeCatalogModelStatus::Deprecated => CatalogModelStatus::Deprecated,
        },
        release_date: model.release_date.clone(),
        last_updated: model.last_updated.clone(),
    }
}

fn provider_connect_fault(
    request: &CredentialConnectRequest,
    error: ModelSetManagerError,
) -> RpcFault {
    let code = match error {
        ModelSetManagerError::UnknownProvider => ProviderConnectErrorCode::UnknownProvider,
        ModelSetManagerError::UnsupportedProvider => ProviderConnectErrorCode::UnsupportedProvider,
        ModelSetManagerError::CatalogRevisionConflict => {
            ProviderConnectErrorCode::CatalogRevisionConflict
        }
        ModelSetManagerError::InvalidCredentials
        | ModelSetManagerError::Catalog(cookie_agent_models::CatalogBuildError::Credentials)
        | ModelSetManagerError::Credentials(CredentialStoreError::InvalidRequest) => {
            ProviderConnectErrorCode::InvalidCredential
        }
        ModelSetManagerError::Credentials(CredentialStoreError::IdempotencyConflict) => {
            ProviderConnectErrorCode::IdempotencyConflict
        }
        ModelSetManagerError::Credentials(
            CredentialStoreError::HomeUnavailable
            | CredentialStoreError::UnsupportedPlatform
            | CredentialStoreError::UnsafePath
            | CredentialStoreError::InvalidStore
            | CredentialStoreError::Io(_)
            | CredentialStoreError::Json(_)
            | CredentialStoreError::Clock,
        ) => ProviderConnectErrorCode::CredentialStorageFailed,
        ModelSetManagerError::Credentials(CredentialStoreError::CandidateRejected)
        | ModelSetManagerError::StaticAliasCollision(_)
        | ModelSetManagerError::CandidateRejected
        | ModelSetManagerError::RetainedSnapshotNotFound
        | ModelSetManagerError::Catalog(_)
        | ModelSetManagerError::Models(_)
        | ModelSetManagerError::Set(_)
        | ModelSetManagerError::Canonical(_)
        | ModelSetManagerError::Clock => return RpcFault::internal(),
    };
    RpcFault::provider_connect_parts(&request.provider_id, &request.client_connect_id, code)
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
        // This variant now represents an engine invariant failure rather than
        // a client-visible rejection; structured rejections use ApprovalResponse.
        EngineError::ApprovalConflict => RpcFault::internal(),
        _ => engine_fault(error),
    }
}

/// A running localhost listener returned by [`Server::serve`].
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
    Listen(#[source] std::io::Error),
    #[error("could not prepare websocket authentication token")]
    Token(#[from] TokenError),
}

async fn websocket_upgrade(
    State(state): State<WebSocketState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !peer.ip().is_loopback() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if headers.contains_key(header::ORIGIN) {
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
    if path.as_os_str().is_empty() {
        return Err(TokenError::HomeUnavailable);
    }
    if path.file_name().and_then(|name| name.to_str()) != Some(TOKEN_FILE) {
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
        let cookie_agent =
            open_or_create_private_dir(&share, std::ffi::OsStr::new("cookie_agent"))?;
        open_or_create_private_dir(&cookie_agent, std::ffi::OsStr::new("daemon"))?
    } else {
        #[cfg(test)]
        {
            let parent_path = path.parent().ok_or(TokenError::UnsafePath)?;
            let anchor_path = parent_path.parent().ok_or(TokenError::UnsafePath)?;
            let relative = path
                .strip_prefix(anchor_path)
                .map_err(|_| TokenError::UnsafePath)?;
            open_test_token_parent(anchor_path, relative)?
        }
        #[cfg(not(test))]
        unreachable!("non-standard token path rejected above")
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
    let write_result = (|| {
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
    if write_result.is_err() {
        let _ = rustix::fs::unlinkat(parent, temporary.as_str(), rustix::fs::AtFlags::empty());
    }
    write_result
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
                validate_directory_type(&current.metadata().map_err(TokenError::Io)?)?;
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(TokenError::UnsafePath);
            }
        }
    }
    validate_safe_anchor(&current.metadata().map_err(TokenError::Io)?)?;
    Ok(current)
}

#[cfg(all(unix, test))]
fn open_test_token_parent(anchor: &Path, relative: &Path) -> Result<fs::File, TokenError> {
    let mut current = open_trusted_token_anchor(anchor)?;
    let mut components = private_components(relative)?;
    if components.pop().as_deref() != Some(std::ffi::OsStr::new(TOKEN_FILE)) {
        return Err(TokenError::UnsafePath);
    }
    for component in components {
        current = open_or_create_private_dir(&current, &component)?;
    }
    Ok(current)
}

#[cfg(all(unix, test))]
fn private_components(path: &Path) -> Result<Vec<std::ffi::OsString>, TokenError> {
    use std::path::Component;

    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => components.push(name.to_owned()),
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(TokenError::UnsafePath);
            }
        }
    }
    if components.len() < 2 {
        return Err(TokenError::UnsafePath);
    }
    Ok(components)
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
            rustix::fs::fsync(parent).map_err(|error| TokenError::Io(error.into()))?;
            let directory = open_directory_at(parent, name)?;
            rustix::fs::fchmod(&directory, rustix::fs::Mode::RWXU)
                .map_err(|error| TokenError::Io(error.into()))?;
            rustix::fs::fsync(&directory).map_err(|error| TokenError::Io(error.into()))?;
            validate_private_directory(&directory.metadata().map_err(TokenError::Io)?)?;
            Ok(directory)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_or_create_private_dir(
    parent: &fs::File,
    name: &std::ffi::OsStr,
) -> Result<fs::File, TokenError> {
    match open_directory_at(parent, name) {
        Ok(directory) => {
            validate_private_directory(&directory.metadata().map_err(TokenError::Io)?)?;
            Ok(directory)
        }
        Err(TokenError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            let created = match rustix::fs::mkdirat(parent, name, rustix::fs::Mode::RWXU) {
                Ok(()) => true,
                Err(rustix::io::Errno::EXIST) => false,
                Err(error) => return Err(TokenError::Io(error.into())),
            };
            rustix::fs::fsync(parent).map_err(|error| TokenError::Io(error.into()))?;
            let directory = open_directory_at(parent, name)?;
            if created {
                rustix::fs::fchmod(&directory, rustix::fs::Mode::RWXU)
                    .map_err(|error| TokenError::Io(error.into()))?;
                rustix::fs::fsync(&directory).map_err(|error| TokenError::Io(error.into()))?;
            }
            validate_private_directory(&directory.metadata().map_err(TokenError::Io)?)?;
            Ok(directory)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_directory_at(
    parent: &fs::File,
    name: impl rustix::path::Arg,
) -> Result<fs::File, TokenError> {
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
fn validate_directory_type(metadata: &fs::Metadata) -> Result<(), TokenError> {
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(TokenError::UnsafePath)
    }
}

#[cfg(unix)]
fn validate_safe_anchor(metadata: &fs::Metadata) -> Result<(), TokenError> {
    use std::os::unix::fs::MetadataExt as _;

    validate_directory_type(metadata)?;
    if metadata.uid() != rustix::process::getuid().as_raw() || metadata.mode() & 0o022 != 0 {
        Err(TokenError::UnsafePath)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn validate_private_directory(metadata: &fs::Metadata) -> Result<(), TokenError> {
    use std::os::unix::fs::MetadataExt as _;

    validate_directory_type(metadata)?;
    if metadata.uid() != rustix::process::getuid().as_raw() || metadata.mode() & 0o777 != 0o700 {
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
        return Err(TokenError::UnsafePath);
    }
    Ok(())
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
    fn invalid_request(message: &'static str) -> Self {
        Self {
            code: -32600,
            message: "invalid request",
            data: Some(json!({ "detail": message })),
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
            data: Some(
                serde_json::to_value(RunStartConflict {
                    code: RunStartConflictCode::IdempotencyConflict,
                    session_id: params.session_id,
                    client_run_id: params.client_run_id.clone(),
                })
                .expect("run-start conflict data serializes"),
            ),
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
    fn catalog(code: CatalogErrorCode, revision: Option<String>) -> Self {
        Self {
            code: -32010,
            message: "catalog error",
            data: typed_error_data(CatalogError { code, revision }),
        }
    }
    fn provider_connect(request: &ProviderConnectParams, code: ProviderConnectErrorCode) -> Self {
        Self::provider_connect_parts(&request.provider_id, &request.client_connect_id, code)
    }
    fn provider_connect_parts(
        provider_id: &str,
        client_connect_id: &str,
        code: ProviderConnectErrorCode,
    ) -> Self {
        Self {
            code: -32011,
            message: "provider connect error",
            data: typed_error_data(ProviderConnectError {
                code,
                provider_id: provider_id.to_owned(),
                client_connect_id: client_connect_id.to_owned(),
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
        Self {
            code: -32602,
            message: "approval response rejected",
            data: typed_error_data(ApprovalRespondError {
                code: failure.code,
                session_id: failure.session_id,
                approval_id: failure.approval_id,
                client_response_id: failure.client_response_id.clone(),
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
            return Err(RpcFault::invalid_request(
                "method must be a non-empty string",
            ));
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
            return Err(RpcFault::invalid_request(
                "method must be a non-empty string",
            ));
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
    let encoded = serde_json::to_vec(&params).map_err(|_| RpcFault::invalid_params())?;
    if encoded.len() > MAX_RAW_RENAME_PARAMS_BYTES {
        return Err(RpcFault::invalid_params());
    }
    let raw: RawSessionRenameParams =
        serde_json::from_value(params).map_err(|_| RpcFault::invalid_params())?;
    let change = match raw.change {
        RawSessionRenameChange::Set { title } => {
            let title = SessionTitle::new(title).map_err(|_| {
                RpcFault::rename_parts(
                    raw.session_id,
                    raw.client_rename_id.clone(),
                    SessionRenameErrorCode::InvalidTitle,
                )
            })?;
            SessionRenameChange::Set { title }
        }
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
    let params = serde_json::to_value(params).map_err(|_| ())?;
    let notification =
        serde_json::to_value(Notification::new(method, Some(params))).map_err(|_| ())?;
    tokio::select! {
        _ = shutdown.cancelled() => Err(()),
        sent = sender.send(notification) => sent.map_err(|_| ()),
    }
}

async fn forward_output(
    stream: OutputStream,
    snapshot: cookie_agent_protocol::OutputSnapshot,
    mut receiver: mpsc::Receiver<OutputMessage>,
    notifications: mpsc::Sender<Value>,
    shutdown: CancellationToken,
) -> Result<(), ()> {
    // Snapshot eviction gaps describe a prefix the following snapshot no
    // longer contains, so deliver them before establishing its end cursor.
    let held_delta = match receiver.try_recv() {
        Ok(OutputMessage::Gap(gap)) => {
            send_notification(&notifications, &shutdown, "events.tool_output_gap", &gap).await?;
            None
        }
        Ok(OutputMessage::Delta(delta)) => Some(delta),
        Err(_) => None,
    };
    // An empty snapshot is still meaningful: its envelope identifies the
    // stream and establishes the snapshot-to-live handoff boundary.
    send_notification(
        &notifications,
        &shutdown,
        "events.tool_output_snapshot",
        &OutputSnapshotEnvelope { stream, snapshot },
    )
    .await?;
    if let Some(delta) = held_delta {
        send_notification(
            &notifications,
            &shutdown,
            "events.tool_output_delta",
            &delta,
        )
        .await?;
    }
    loop {
        let message = tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            message = receiver.recv() => match message {
                Some(message) => message,
                None => return Ok(()),
            },
        };
        match message {
            OutputMessage::Delta(delta) => {
                send_notification(
                    &notifications,
                    &shutdown,
                    "events.tool_output_delta",
                    &delta,
                )
                .await?
            }
            OutputMessage::Gap(gap) => {
                send_notification(&notifications, &shutdown, "events.tool_output_gap", &gap).await?
            }
        }
    }
}

/// Compatibility placeholder while the binary's composition root is added.
/// It intentionally does not open a listener without an engine/configuration.
pub async fn daemon() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    use cookie_agent_config::{Config, load_layered};
    use cookie_agent_engine::EngineOptions;
    use cookie_agent_models::{CredentialStore, ModelSetManager};
    use cookie_agent_protocol::{PROTOCOL_VERSION, Request};
    use futures_util::{SinkExt, StreamExt};
    use serde::Serialize;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use super::*;

    struct ErroringStream {
        release: Arc<tokio::sync::Notify>,
    }

    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CaptureWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture lock poisoned")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl MessageStream for ErroringStream {
        async fn send(&mut self, _: MessageFrame) -> Result<(), TransportError> {
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
            self.release.notified().await;
            Err(TransportError::Closed)
        }
    }

    struct Harness {
        _directory: tempfile::TempDir,
        engine: Engine,
        server: Arc<Server>,
    }

    struct FilesystemBindingTool {
        target: PathBuf,
        executed: Arc<AtomicBool>,
    }

    struct FilesystemBindingExecutor {
        target: PathBuf,
        expected: cookie_agent_protocol::Sha256Digest,
        executed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl cookie_agent_engine::ToolProvider for FilesystemBindingTool {
        fn tools_for_session(
            &self,
            _: &cookie_agent_engine::SessionToolContext,
        ) -> Result<Vec<cookie_agent_engine::ToolSpec>, cookie_agent_engine::ToolError> {
            Ok(vec![cookie_agent_engine::ToolSpec {
                name: "read".into(),
                description: "Read one descriptor-bound integration fixture.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": { "filePath": { "type": "string" } },
                    "required": ["filePath"],
                    "additionalProperties": false
                }),
            }])
        }

        async fn prepare(
            &self,
            ctx: cookie_agent_engine::ToolPreparationContext,
            call: cookie_agent_engine::ToolCall,
        ) -> Result<cookie_agent_engine::PreparedTool, cookie_agent_engine::ToolError> {
            let requested = call
                .arguments
                .get("filePath")
                .and_then(Value::as_str)
                .ok_or_else(|| cookie_agent_engine::ToolError::execution("missing filePath"))?;
            if call.name != "read" || ctx.cwd.join(requested) != self.target {
                return Err(cookie_agent_engine::ToolError::execution(
                    "unexpected filesystem fixture call",
                ));
            }
            let bytes = fs::read(&self.target)
                .map_err(|error| cookie_agent_engine::ToolError::execution(error.to_string()))?;
            let expected = cookie_agent_protocol::Sha256Digest::of_bytes(&bytes);
            let label = self.target.display().to_string();
            let label_digest = cookie_agent_protocol::Sha256Digest::of_bytes(label.as_bytes());
            let resource = cookie_agent_protocol::PreparedApprovalResource {
                capability: cookie_agent_protocol::ActionKind::Read,
                canonical: cookie_agent_protocol::PreparedResourceIdentity::new(format!(
                    "file:{}",
                    label_digest.as_str()
                ))
                .map_err(|error| cookie_agent_engine::ToolError::execution(error.to_string()))?,
                binding_digest:
                    cookie_agent_protocol::PreparedResourceDigest::from_canonical_binding_bytes(
                        expected.as_str().as_bytes(),
                    ),
                binding_lifetime: cookie_agent_protocol::PreparedBindingLifetime::ProcessLocal,
                boundary: cookie_agent_protocol::ApprovalBoundary::Exact,
                source: cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
            };
            let normalized_arguments = serde_json::to_vec(&call.arguments)
                .map_err(|error| cookie_agent_engine::ToolError::execution(error.to_string()))?;
            let operation = cookie_agent_protocol::PreparedOperationIdentity::new(
                cookie_agent_protocol::Sha256Digest::of_bytes(&normalized_arguments),
                vec![cookie_agent_protocol::ApprovalCapability {
                    action: cookie_agent_protocol::ActionKind::Read,
                    operation: cookie_agent_protocol::PreparedCapabilityOperation::new("read:read")
                        .map_err(|error| {
                            cookie_agent_engine::ToolError::execution(error.to_string())
                        })?,
                }],
                vec![resource],
                cookie_agent_protocol::Sha256Digest::of_bytes(
                    ctx.cwd.as_os_str().as_encoded_bytes(),
                ),
            )
            .map_err(|error| cookie_agent_engine::ToolError::execution(error.to_string()))?;
            cookie_agent_engine::PreparedTool::new(
                operation,
                None,
                Box::new(FilesystemBindingExecutor {
                    target: self.target.clone(),
                    expected,
                    executed: Arc::clone(&self.executed),
                }),
            )
            .with_policy_labels(vec![label])
        }
    }

    #[async_trait]
    impl cookie_agent_engine::PreparedExecutor for FilesystemBindingExecutor {
        async fn revalidate(&self) -> Result<(), cookie_agent_engine::ToolError> {
            let bytes = fs::read(&self.target).map_err(|error| {
                cookie_agent_engine::ToolError::operation_changed(error.to_string())
            })?;
            if cookie_agent_protocol::Sha256Digest::of_bytes(&bytes) != self.expected {
                return Err(cookie_agent_engine::ToolError::operation_changed(
                    "prepared filesystem binding changed before approval response",
                ));
            }
            Ok(())
        }

        async fn execute(
            self: Box<Self>,
            _: cookie_agent_engine::ToolExecutionContext,
        ) -> Result<cookie_agent_engine::ToolResult, cookie_agent_engine::ToolError> {
            self.executed.store(true, Ordering::SeqCst);
            Ok(cookie_agent_engine::ToolResult {
                title: "filesystem fixture executed".into(),
                output: "unexpected execution".into(),
                metadata: Value::Null,
                truncation: None,
                attachments: Vec::new(),
            })
        }
    }

    async fn serve_responses_fixture(listener: tokio::net::TcpListener, response_body: String) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let (mut socket, _) = listener.accept().await.expect("accept model request");
        let mut request = Vec::new();
        let header_end = loop {
            let mut buffer = [0_u8; 4096];
            let count = socket.read(&mut buffer).await.expect("read model request");
            assert_ne!(count, 0, "model request ended before headers");
            request.extend_from_slice(&buffer[..count]);
            if let Some(offset) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .expect("model request content length");
        while request.len() < header_end + content_length {
            let mut buffer = [0_u8; 4096];
            let count = socket
                .read(&mut buffer)
                .await
                .expect("read model request body");
            assert_ne!(count, 0, "model request ended before body");
            request.extend_from_slice(&buffer[..count]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write model response");
    }

    fn test_config_at(directory: &tempfile::TempDir, tool: Option<&str>, endpoint: &str) -> Config {
        let tools = tool.map_or_else(String::new, |tool| format!("tools = [\"{tool}\"]"));
        let source = format!(
            r#"
[models.scripted]
provider_id = "test"
model_id = "scripted"
endpoint = "{endpoint}"
adaptor = "openai-responses"

[models.scripted.auth]
type = "openai"
api_key = "test-secret"

[models.scripted.capabilities]
features = ["max_output_tokens", "tool_calling", "usage"]
cancellation = "local_only"
compaction = "unsupported"

[models.scripted.capabilities.limits]
context = 4096

[models.scripted.capabilities.modalities]
input = ["text"]
output = ["text"]

[models.scripted.capabilities.media]
input = {{}}

[models.scripted.capabilities.replay]
policy = "never"
capability = "unsupported"
reasoning = false

[models.scripted.settings]

[agents.primary]
type = "primary"
models = ["scripted"]
{tools}

[agents.draft]
type = "primary"
models = ["scripted"]
"#
        );
        let path = directory.path().join("config.toml");
        fs::write(&path, source).expect("write test config");
        load_layered(None, Some(&path)).expect("load test config")
    }

    fn test_config(directory: &tempfile::TempDir, with_bash: bool) -> Config {
        test_config_at(
            directory,
            with_bash.then_some("bash"),
            "https://example.test/v1",
        )
    }

    fn harness() -> Harness {
        let directory = tempfile::tempdir().expect("temporary directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("private temporary directory");
        }
        let config = test_config(&directory, false);
        let catalog = Arc::new(Catalog::embedded().expect("catalog"));
        let model_manager = Arc::new(
            ModelSetManager::new(
                config.models.clone(),
                Arc::clone(&catalog),
                CredentialStore::new(directory.path().join("credentials")),
            )
            .expect("model manager"),
        );
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: config.clone(),
            model_manager: Arc::clone(&model_manager),
            tools: Vec::new(),
        })
        .expect("open engine");
        let token_path = directory.path().join("daemon/token-v1");
        Harness {
            _directory: directory,
            server: Arc::new(
                Server::new(engine.clone(), model_manager, catalog).with_token_path(token_path),
            ),
            engine,
        }
    }

    fn approval_harness() -> Harness {
        let directory = tempfile::tempdir().expect("temporary directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("private temporary directory");
        }
        let config = test_config(&directory, true);
        let catalog = Arc::new(Catalog::embedded().expect("catalog"));
        let model_manager = Arc::new(
            ModelSetManager::new(
                config.models.clone(),
                Arc::clone(&catalog),
                CredentialStore::new(directory.path().join("credentials")),
            )
            .expect("model manager"),
        );
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: config.clone(),
            model_manager: Arc::clone(&model_manager),
            tools: Vec::new(),
        })
        .expect("open engine");
        let token_path = directory.path().join("daemon/token-v1");
        Harness {
            _directory: directory,
            server: Arc::new(
                Server::new(engine.clone(), model_manager, catalog).with_token_path(token_path),
            ),
            engine,
        }
    }

    fn catalog_agent_harness(alias: &str) -> Harness {
        let directory = tempfile::tempdir().expect("temporary directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("private temporary directory");
        }
        let mut config = test_config(&directory, false);
        config
            .agents
            .get_mut("draft")
            .expect("draft profile")
            .models = vec![alias.to_owned()];
        let catalog = Arc::new(Catalog::embedded().expect("catalog"));
        let model_manager = Arc::new(
            ModelSetManager::new(
                config.models.clone(),
                Arc::clone(&catalog),
                CredentialStore::new(directory.path().join("credentials")),
            )
            .expect("model manager"),
        );
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config,
            model_manager: Arc::clone(&model_manager),
            tools: Vec::new(),
        })
        .expect("open engine");
        let token_path = directory.path().join("daemon/token-v1");
        Harness {
            _directory: directory,
            server: Arc::new(
                Server::new(engine.clone(), model_manager, catalog).with_token_path(token_path),
            ),
            engine,
        }
    }

    fn connect_params(harness: &Harness, id: &str, provider_id: &str, secret: &str) -> Value {
        let provider = harness
            .server
            .catalog
            .providers()
            .get(provider_id)
            .expect("catalog provider");
        json!({
            "client_connect_id": id,
            "provider_id": provider_id,
            "catalog_revision": harness.server.catalog.revision(),
            "credentials": {
                "values": std::collections::BTreeMap::from([
                    (provider.credential_fields[0].clone(), secret.to_owned())
                ])
            }
        })
    }

    async fn seed_escalated_approval(
        harness: &Harness,
        session_id: cookie_agent_protocol::SessionId,
        request: cookie_agent_protocol::ApprovalRequest,
    ) {
        let (_, mut events) = harness
            .engine
            .subscribe(session_id, None)
            .await
            .expect("subscribe before approval escalation");
        let run_id = cookie_agent_protocol::RunId::new_v7();
        harness
            .engine
            .append(
                session_id,
                Some(run_id),
                Event::ApprovalRequested {
                    request: request.clone(),
                },
            )
            .await
            .expect("append approval request");
        harness
            .engine
            .append(
                session_id,
                Some(run_id),
                Event::ApprovalEscalated {
                    approval_id: request.approval_id(),
                    reason_code: cookie_agent_protocol::ApprovalReasonCode::Escalated,
                },
            )
            .await
            .expect("append approval escalation");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let EventSubscriptionMessage::Event { event } =
                    events.recv().await.expect("approval subscription closed")
                    && matches!(
                        event.event,
                        Event::ApprovalEscalated { approval_id, .. }
                            if approval_id == request.approval_id()
                    )
                {
                    return;
                }
            }
        })
        .await
        .expect("durable approval escalation");
    }

    fn approval_request(
        approval_id: cookie_agent_protocol::ApprovalId,
        identity: &str,
        revision: u64,
        allow_tree_grant: bool,
        cancellable: bool,
    ) -> cookie_agent_protocol::ApprovalRequest {
        let binding_digest =
            cookie_agent_protocol::PreparedResourceDigest::from_canonical_binding_bytes(
                format!("{identity}:binding").as_bytes(),
            );
        let resource = cookie_agent_protocol::PreparedApprovalResource {
            capability: cookie_agent_protocol::ActionKind::Bash,
            canonical: cookie_agent_protocol::PreparedResourceIdentity::new(format!(
                "command:{}",
                cookie_agent_protocol::Sha256Digest::of_bytes(identity.as_bytes()).as_str()
            ))
            .expect("prepared resource identity"),
            binding_digest: binding_digest.clone(),
            binding_lifetime: cookie_agent_protocol::PreparedBindingLifetime::RestartStable,
            boundary: cookie_agent_protocol::ApprovalBoundary::CommandPrefix {
                prefix: identity.to_owned(),
            },
            source: cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
        };
        let operation = cookie_agent_protocol::PreparedOperationIdentity::new(
            cookie_agent_protocol::Sha256Digest::of_bytes(
                format!("{identity}:arguments").as_bytes(),
            ),
            vec![cookie_agent_protocol::ApprovalCapability {
                action: cookie_agent_protocol::ActionKind::Bash,
                operation: cookie_agent_protocol::PreparedCapabilityOperation::new("execute")
                    .expect("prepared capability operation"),
            }],
            vec![resource],
            cookie_agent_protocol::Sha256Digest::of_bytes(format!("{identity}:context").as_bytes()),
        )
        .expect("prepared approval operation");
        let evaluation = cookie_agent_protocol::ApprovalEvaluation {
            resource_digest: binding_digest,
            effect: cookie_agent_protocol::Effect::Ask,
            trace: cookie_agent_protocol::DecisionTrace {
                action: cookie_agent_protocol::ActionKind::Bash,
                normalized_resource: identity.to_owned(),
                candidates: Vec::new(),
                effect: cookie_agent_protocol::Effect::Ask,
                precedence_reason: "server approval fixture".into(),
            },
        };
        cookie_agent_protocol::ApprovalRequest::new(
            approval_id,
            revision,
            cookie_agent_protocol::ApprovalTrigger::PermissionPolicy,
            operation,
            vec![evaluation],
            cookie_agent_protocol::ApprovalConstraints {
                allow_once: true,
                allow_tree_grant,
                cancellable,
                expires_at: None,
            },
        )
        .expect("approval request")
    }

    async fn rpc<S: MessageStream, T: Serialize>(
        stream: &mut S,
        id: i64,
        method: &str,
        params: T,
    ) -> Value {
        let response = rpc_response(stream, id, method, params).await;
        assert!(response.get("error").is_none(), "rpc error: {response}");
        response["result"].clone()
    }

    async fn rpc_response<S: MessageStream, T: Serialize>(
        stream: &mut S,
        id: i64,
        method: &str,
        params: T,
    ) -> Value {
        let request = Request::new(
            JsonRpcId::Number(id),
            method,
            Some(serde_json::to_value(params).expect("params")),
        );
        stream
            .send(MessageFrame::Value(
                serde_json::to_value(request).expect("request value"),
            ))
            .await
            .expect("send request");
        loop {
            let frame = stream
                .recv()
                .await
                .expect("receive frame")
                .expect("open stream");
            let value = frame_value(frame);
            if value.get("id") == Some(&json!(id)) {
                return value;
            }
        }
    }

    fn frame_value(frame: MessageFrame) -> Value {
        match frame {
            MessageFrame::Value(value) => value,
            MessageFrame::Text(text) => serde_json::from_str(&text).expect("JSON frame"),
        }
    }

    fn websocket_http_status(error: tokio_tungstenite::tungstenite::Error) -> StatusCode {
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => response.status(),
            other => panic!("expected HTTP websocket rejection, got {other}"),
        }
    }

    async fn handshake<S: MessageStream>(stream: &mut S) {
        let result = rpc(
            stream,
            1,
            "handshake",
            ClientHello {
                protocol_version: ProtocolVersion::current(),
            },
        )
        .await;
        assert_eq!(result, json!({ "protocol_version": PROTOCOL_VERSION }));
    }

    async fn next_value<S: MessageStream>(stream: &mut S) -> Value {
        frame_value(
            stream
                .recv()
                .await
                .expect("receive frame")
                .expect("open stream"),
        )
    }

    #[tokio::test]
    async fn notification_ingress_never_receives_a_response() {
        let harness = harness();
        let (mut client, server_stream) = in_process_pair(2);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;
        client
            .send(MessageFrame::Value(json!({
                "jsonrpc": "2.0", "method": "session.list", "params": {}
            })))
            .await
            .expect("send notification");
        client
            .send(MessageFrame::Value(json!({
                "jsonrpc": "2.0", "method": "session.get", "params": {}
            })))
            .await
            .expect("send erroring notification");
        client
            .send(MessageFrame::Value(json!({
                "jsonrpc": "2.0", "method": "unknown.method"
            })))
            .await
            .expect("send unknown notification");
        assert!(
            tokio::time::timeout(Duration::from_millis(25), client.recv())
                .await
                .is_err()
        );

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn malformed_json_and_invalid_requests_have_distinct_codes() {
        let harness = harness();
        let (mut client, server_stream) = in_process_pair(2);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));

        client
            .send(MessageFrame::Text("{".into()))
            .await
            .expect("send malformed JSON");
        assert_eq!(next_value(&mut client).await["error"]["code"], -32700);

        client
            .send(MessageFrame::Text(r#"{"jsonrpc":"2.0","id":1}"#.into()))
            .await
            .expect("send invalid request");
        assert_eq!(next_value(&mut client).await["error"]["code"], -32600);

        for batch in [
            r#"[{"jsonrpc":"2.0","method":"session.list"}]"#,
            r#"[{"jsonrpc":"2.0","method":"session.list"},{"jsonrpc":"2.0","id":1,"method":"session.list"}]"#,
        ] {
            client
                .send(MessageFrame::Text(batch.into()))
                .await
                .expect("send batch");
            assert_eq!(next_value(&mut client).await["error"]["code"], -32600);
        }

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn unsupported_handshake_version_is_invalid_exact_v6_params() {
        let harness = harness();
        let (mut client, server_stream) = in_process_pair(2);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        client
            .send(MessageFrame::Value(json!({
                "jsonrpc": "2.0", "id": 1, "method": "handshake",
                "params": { "protocol_version": PROTOCOL_VERSION + 1 }
            })))
            .await
            .expect("send handshake");
        let response = next_value(&mut client).await;
        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"].get("data").is_none());

        handshake(&mut client).await;
        let old_approval = rpc_response(
            &mut client,
            2,
            "approval.respond",
            json!({
                "session_id": cookie_agent_protocol::SessionId::new_v7(),
                "approval_id": cookie_agent_protocol::ApprovalId::new_v7(),
                "decision": "once",
                "scope": "git status *"
            }),
        )
        .await;
        assert_eq!(old_approval["error"]["code"], -32602);
        assert!(old_approval["error"].get("data").is_none());

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn explicit_null_id_is_a_request_and_echoes_null() {
        let harness = harness();
        let (mut client, server_stream) = in_process_pair(2);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;
        client
            .send(MessageFrame::Value(json!({
                "jsonrpc": "2.0", "id": null, "method": "session.list", "params": {}
            })))
            .await
            .expect("send null-id request");
        let response = next_value(&mut client).await;
        assert!(response.get("result").is_some());
        assert!(response["id"].is_null());

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn model_list_exposes_only_immutable_configured_aliases() {
        let harness = harness();
        let (mut client, server_stream) = in_process_pair(2);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;

        let result = rpc(&mut client, 2, "model.list", json!({})).await;
        assert_eq!(result["models"][0]["name"], "scripted");
        assert_eq!(result["models"][0]["provider_id"], "test");
        assert_eq!(result["models"][0]["model_id"], "scripted");
        assert!(result["models"][0]["adapter_id"].is_string());

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn protocol_routes_require_handshake_and_return_exact_v6_shapes() {
        let harness = harness();
        let (mut client, server_stream) = in_process_pair(4);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));

        client
            .send(MessageFrame::Value(json!({
                "jsonrpc": "2.0",
                "method": "handshake",
                "params": { "protocol_version": PROTOCOL_VERSION }
            })))
            .await
            .expect("send handshake notification");
        let blocked = rpc_response(&mut client, 1, "catalog.provider.list", json!({})).await;
        assert_eq!(blocked["jsonrpc"], "2.0");
        assert_eq!(blocked["error"]["code"], -32001);
        handshake(&mut client).await;
        let models = rpc(&mut client, 2, "model.list", json!({})).await;
        assert!(
            models["revision"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert!(models["generated_at"].is_string());
        assert!(models["catalog_revision"].is_null());
        assert!(models["models"].is_array());

        client
            .send(MessageFrame::Value(json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "model.list",
                "params": {},
                "legacy": true
            })))
            .await
            .expect("send old envelope");
        assert_eq!(next_value(&mut client).await["error"]["code"], -32600);

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn catalog_routes_are_sorted_offline_projections() {
        let harness = harness();
        let (mut client, server_stream) = in_process_pair(4);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;

        let providers = rpc(&mut client, 2, "catalog.provider.list", json!({})).await;
        assert_eq!(
            providers["snapshot"]["revision"],
            harness.server.catalog.revision()
        );
        let provider_ids = providers["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .map(|provider| provider["id"].as_str().expect("provider id"))
            .collect::<Vec<_>>();
        assert!(provider_ids.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(providers.to_string().contains("credential_fields"));

        let models = rpc(
            &mut client,
            3,
            "catalog.model.list",
            json!({ "provider_id": "anthropic" }),
        )
        .await;
        let model_ids = models["models"]
            .as_array()
            .expect("models")
            .iter()
            .map(|model| model["model_id"].as_str().expect("model id"))
            .collect::<Vec<_>>();
        assert!(!model_ids.is_empty());
        assert!(model_ids.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(
            models["models"]
                .as_array()
                .expect("models")
                .iter()
                .all(|model| model["provider_id"] == "anthropic")
        );

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn provider_connect_maps_typed_errors_and_idempotency() {
        let harness = harness();
        let (mut client, server_stream) = in_process_pair(8);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;

        let missing = rpc_response(
            &mut client,
            2,
            "provider.connect",
            json!({
                "client_connect_id": "missing",
                "provider_id": "anthropic",
                "catalog_revision": harness.server.catalog.revision(),
                "credentials": { "values": {} }
            }),
        )
        .await;
        assert_eq!(missing["error"]["code"], -32011);
        assert_eq!(missing["error"]["data"]["code"], "missing_credential");

        let invalid = rpc_response(
            &mut client,
            3,
            "provider.connect",
            json!({
                "client_connect_id": "invalid",
                "provider_id": "anthropic",
                "catalog_revision": harness.server.catalog.revision(),
                "credentials": { "values": { "WRONG_KEY": "sentinel-invalid" } }
            }),
        )
        .await;
        assert_eq!(invalid["error"]["data"]["code"], "invalid_credential");

        let unknown = rpc_response(
            &mut client,
            4,
            "provider.connect",
            json!({
                "client_connect_id": "unknown",
                "provider_id": "not-a-provider",
                "catalog_revision": harness.server.catalog.revision(),
                "credentials": { "values": { "KEY": "sentinel-unknown" } }
            }),
        )
        .await;
        assert_eq!(unknown["error"]["data"]["code"], "unknown_provider");

        let unsupported = rpc_response(
            &mut client,
            5,
            "provider.connect",
            connect_params(
                &harness,
                "unsupported",
                "amazon-bedrock",
                "sentinel-unsupported",
            ),
        )
        .await;
        assert_eq!(unsupported["error"]["data"]["code"], "unsupported_provider");

        let mut stale = connect_params(&harness, "stale", "anthropic", "sentinel-stale");
        stale["catalog_revision"] = json!("sha256:stale");
        let stale = rpc_response(&mut client, 6, "provider.connect", stale).await;
        assert_eq!(stale["error"]["data"]["code"], "catalog_revision_conflict");

        let first_params = connect_params(&harness, "connect-1", "anthropic", "sentinel-success");
        let first = rpc(&mut client, 7, "provider.connect", first_params.clone()).await;
        let replay = rpc(&mut client, 8, "provider.connect", first_params).await;
        assert_eq!(first, replay);
        assert_eq!(first["client_connect_id"], "connect-1");
        assert!(first["connection"].get("credentials").is_none());

        let conflict = rpc_response(
            &mut client,
            9,
            "provider.connect",
            connect_params(&harness, "connect-1", "anthropic", "sentinel-conflict"),
        )
        .await;
        assert_eq!(conflict["error"]["data"]["code"], "idempotency_conflict");

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn provider_connect_maps_storage_failures_without_details() {
        use std::os::unix::fs::PermissionsExt as _;

        let harness = harness();
        let credential_root = harness._directory.path().join("credentials");
        fs::set_permissions(&credential_root, fs::Permissions::from_mode(0o755))
            .expect("make credential root unsafe");
        let (mut client, server_stream) = in_process_pair(4);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;

        let response = rpc_response(
            &mut client,
            2,
            "provider.connect",
            connect_params(&harness, "storage", "anthropic", "sentinel-storage"),
        )
        .await;
        assert_eq!(response["error"]["code"], -32011);
        assert_eq!(
            response["error"]["data"]["code"],
            "credential_storage_failed"
        );
        assert!(!response.to_string().contains("sentinel-storage"));

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[test]
    fn request_frame_debug_never_exposes_text_or_json_credentials() {
        let sentinel = "SENTINEL_FRAME_DEBUG_SECRET_a118";
        let text = MessageFrame::Text(format!(
            r#"{{"method":"provider.connect","credentials":{{"values":{{"API_KEY":"{sentinel}"}}}}}}"#
        ));
        let value = MessageFrame::Value(json!({
            "method": "provider.connect",
            "credentials": { "values": { "API_KEY": sentinel } }
        }));

        let text_debug = format!("{text:?}");
        let value_debug = format!("{value:?}");
        assert_eq!(text_debug, "MessageFrame::Text(<redacted>)");
        assert_eq!(value_debug, "MessageFrame::Value(<redacted>)");
        assert!(!text_debug.contains(sentinel));
        assert!(!value_debug.contains(sentinel));
    }

    #[test]
    fn provider_connect_moves_credential_allocations_into_manager_request() {
        let sentinel = "SENTINEL_MOVED_CREDENTIAL_7c2e";
        let request: ProviderConnectParams = serde_json::from_value(json!({
            "client_connect_id": "move-test",
            "provider_id": "provider",
            "catalog_revision": "sha256:catalog",
            "credentials": { "values": { "API_KEY": sentinel } }
        }))
        .expect("provider connect params");
        let (source_key_pointer, source_value_pointer) = request
            .credentials
            .values
            .iter()
            .next()
            .map(|(key, value)| (key.as_ptr(), value.as_ptr()))
            .expect("credential");

        let manager_request = into_manager_connect_request(request);
        let (moved_key, moved_value) = manager_request
            .credentials
            .iter()
            .next()
            .expect("moved credential");
        assert!(std::ptr::eq(moved_key.as_ptr(), source_key_pointer));
        assert!(std::ptr::eq(moved_value.as_ptr(), source_value_pointer));
        assert_eq!(moved_value, sentinel);
        assert!(!format!("{manager_request:?}").contains(sentinel));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn credential_sentinels_never_leak_from_success_error_replay_or_conflict() {
        use tracing::instrument::WithSubscriber as _;

        let harness = harness();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || CaptureWriter(Arc::clone(&writer)))
            .finish();
        let success = "SENTINEL_CONNECT_SUCCESS_7f92";
        let conflict = "SENTINEL_CONNECT_CONFLICT_37ad";
        let error = "SENTINEL_CONNECT_ERROR_9b11";

        let responses = async {
            let (mut client, server_stream) = in_process_pair(8);
            let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
            handshake(&mut client).await;
            let params = connect_params(&harness, "secret-capture", "anthropic", success);
            let first = rpc_response(&mut client, 2, "provider.connect", params.clone()).await;
            let replay = rpc_response(&mut client, 3, "provider.connect", params).await;
            let conflict_response = rpc_response(
                &mut client,
                4,
                "provider.connect",
                connect_params(&harness, "secret-capture", "anthropic", conflict),
            )
            .await;
            let error_response = rpc_response(
                &mut client,
                5,
                "provider.connect",
                json!({
                    "client_connect_id": "secret-error",
                    "provider_id": "unknown-provider",
                    "catalog_revision": harness.server.catalog.revision(),
                    "credentials": { "values": { "KEY": error } }
                }),
            )
            .await;
            drop(client);
            task.await.expect("server task").expect("stream result");
            vec![first, replay, conflict_response, error_response]
        }
        .with_subscriber(subscriber)
        .await;

        let surfaces = format!(
            "{}\n{}",
            serde_json::to_string(&responses).expect("responses"),
            String::from_utf8(captured.lock().expect("capture lock").clone())
                .expect("UTF-8 tracing")
        );
        for sentinel in [success, conflict, error] {
            assert!(!surfaces.contains(sentinel), "secret leaked: {sentinel}");
        }
        let debug: ProviderConnectParams =
            serde_json::from_value(connect_params(&harness, "debug", "anthropic", success))
                .expect("params");
        assert!(!format!("{debug:?}").contains(success));
        let manager_request = into_manager_connect_request(debug);
        assert!(!format!("{manager_request:?}").contains(success));
        let internal = provider_connect_fault(
            &manager_request,
            ModelSetManagerError::StaticAliasCollision("safe-alias".into()),
        );
        assert_eq!(internal.code, -32603);
        assert!(internal.data.is_none());
        assert!(!format!("{internal:?}").contains(success));
    }

    #[tokio::test]
    async fn provider_connect_notification_never_mutates_and_connect_publishes_for_lists() {
        let alias = "anthropic/claude-opus-4-6";
        let harness = catalog_agent_harness(alias);
        let before_revision = harness.server.model_manager.current().revision().to_owned();
        let (mut client, server_stream) = in_process_pair(8);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;

        let params = connect_params(&harness, "notification", "anthropic", "sentinel-notify");
        client
            .send(MessageFrame::Value(json!({
                "jsonrpc": "2.0",
                "method": "provider.connect",
                "params": params
            })))
            .await
            .expect("send connect notification");
        tokio::task::yield_now().await;
        assert_eq!(
            harness.server.model_manager.current().revision(),
            before_revision
        );

        let before_agents = rpc(&mut client, 2, "agent.list", json!({})).await;
        let draft_before = before_agents["agents"]
            .as_array()
            .expect("agents")
            .iter()
            .find(|agent| agent["name"] == "draft")
            .expect("draft");
        assert_eq!(draft_before["enabled"], false);

        let connected = rpc(
            &mut client,
            3,
            "provider.connect",
            connect_params(&harness, "request", "anthropic", "sentinel-request"),
        )
        .await;
        let models = rpc(&mut client, 4, "model.list", json!({})).await;
        assert_eq!(models["revision"], connected["model_revision"]);
        assert_eq!(
            models["catalog_revision"],
            harness.server.catalog.revision()
        );
        assert!(
            models["models"]
                .as_array()
                .expect("models")
                .iter()
                .any(|model| model["name"] == alias)
        );

        let agents = rpc(&mut client, 5, "agent.list", json!({})).await;
        let draft = agents["agents"]
            .as_array()
            .expect("agents")
            .iter()
            .find(|agent| agent["name"] == "draft")
            .expect("draft");
        assert_eq!(draft["enabled"], true);
        assert_eq!(draft["models"][0]["name"], alias);

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[test]
    fn stable_catalog_and_model_error_codes_use_typed_safe_data() {
        for code in [
            CatalogErrorCode::CatalogUnavailable,
            CatalogErrorCode::CatalogSnapshotInvalid,
            CatalogErrorCode::CatalogRevisionNotFound,
        ] {
            let fault = RpcFault::catalog(code, Some("sha256:safe".into()));
            assert_eq!(fault.code, -32010);
            assert_eq!(fault.data.expect("catalog data")["revision"], "sha256:safe");
        }
        for code in [
            ModelListErrorCode::ModelSnapshotUnavailable,
            ModelListErrorCode::ModelSnapshotInvalid,
        ] {
            let fault = RpcFault::model_list(code);
            assert_eq!(fault.code, -32012);
            assert!(fault.data.expect("model data")["code"].is_string());
        }
        let internal = RpcFault::internal();
        assert_eq!(internal.code, -32603);
        assert!(internal.data.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn websocket_token_loader_rejects_symlink_files() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("daemon");
        fs::create_dir(&parent).expect("daemon directory");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))
            .expect("private daemon directory");
        let target = directory.path().join("target");
        fs::write(&target, "not-a-token").expect("target");
        let token_path = parent.join(TOKEN_FILE);
        symlink(&target, &token_path).expect("token symlink");
        assert!(matches!(
            load_or_create_token(&token_path),
            Err(TokenError::UnsafePath)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn websocket_token_loader_rejects_unsafe_anchors_and_managed_ancestors() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempfile::tempdir().expect("temporary directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private anchor");
        let target = directory.path().join("target");
        fs::create_dir(&target).expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).expect("private target");
        symlink(&target, directory.path().join("managed")).expect("managed symlink");
        assert!(matches!(
            open_test_token_parent(directory.path(), Path::new("managed/daemon/token-v1")),
            Err(TokenError::UnsafePath)
        ));
        assert_eq!(fs::read_dir(&target).expect("target entries").count(), 0);

        fs::remove_file(directory.path().join("managed")).expect("remove symlink");
        fs::create_dir(directory.path().join("managed")).expect("managed directory");
        fs::set_permissions(
            directory.path().join("managed"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("unsafe managed mode");
        assert!(matches!(
            open_test_token_parent(directory.path(), Path::new("managed/daemon/token-v1")),
            Err(TokenError::UnsafePath)
        ));

        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o777))
            .expect("unsafe anchor mode");
        assert!(matches!(
            open_test_token_parent(directory.path(), Path::new("managed/daemon/token-v1")),
            Err(TokenError::UnsafePath)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn websocket_token_ancestor_replacement_race_never_redirects_writes() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempfile::tempdir().expect("temporary directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private anchor");
        let managed = directory.path().join("managed");
        fs::create_dir(&managed).expect("managed");
        fs::set_permissions(&managed, fs::Permissions::from_mode(0o700)).expect("private managed");
        let daemon = managed.join("daemon");
        fs::create_dir(&daemon).expect("daemon");
        fs::set_permissions(&daemon, fs::Permissions::from_mode(0o700)).expect("private daemon");
        let parked = directory.path().join("managed-parked");
        let target = directory.path().join("attacker-target");
        fs::create_dir(&target).expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).expect("private target");
        let stop = Arc::new(AtomicBool::new(false));
        let attacker_stop = Arc::clone(&stop);
        let attacker = thread::spawn(move || {
            while !attacker_stop.load(Ordering::Relaxed) {
                if fs::rename(&managed, &parked).is_ok() {
                    let _ = symlink(&target, &managed);
                    thread::yield_now();
                    let _ = fs::remove_file(&managed);
                    let _ = fs::rename(&parked, &managed);
                } else {
                    thread::yield_now();
                }
            }
            let _ = fs::remove_file(&managed);
            if parked.exists() {
                let _ = fs::rename(&parked, &managed);
            }
            target
        });

        for _ in 0..32 {
            match open_test_token_parent(directory.path(), Path::new("managed/daemon/token-v1"))
                .and_then(|parent| load_or_create_token_from_parent(&parent))
            {
                Ok(_) | Err(TokenError::UnsafePath | TokenError::Io(_)) => {}
                Err(error) => panic!("unexpected fail-closed race result: {error}"),
            }
        }
        stop.store(true, Ordering::Relaxed);
        let target = attacker.join().expect("attacker thread");
        assert_eq!(fs::read_dir(target).expect("target entries").count(), 0);
    }

    #[test]
    fn model_list_uses_one_complete_snapshot_during_refresh() {
        let harness = harness();
        harness
            .server
            .model_manager
            .connect(&CredentialConnectRequest {
                client_connect_id: "coherent-connect".into(),
                provider_id: "anthropic".into(),
                catalog_revision: harness.server.catalog.revision().to_owned(),
                credentials: std::collections::BTreeMap::from([(
                    harness.server.catalog.providers()["anthropic"].credential_fields[0].clone(),
                    "sentinel-coherent".into(),
                )]),
            })
            .expect("connect");
        let manager = Arc::clone(&harness.server.model_manager);
        let refresher = std::thread::spawn(move || {
            for _ in 0..20 {
                manager.refresh().expect("refresh");
            }
        });
        for _ in 0..100 {
            let result = harness.server.list_models().expect("model list");
            assert!(result.revision.starts_with("sha256:"));
            assert_eq!(
                result.catalog_revision.as_deref(),
                Some(harness.server.catalog.revision())
            );
            assert!(
                result
                    .models
                    .iter()
                    .any(|model| model.name.starts_with("anthropic/"))
            );
        }
        refresher.join().expect("refresh thread");
    }

    #[tokio::test]
    async fn concurrent_connections_route_independently() {
        let harness = harness();
        let (mut first, first_server) = in_process_pair(4);
        let (mut second, second_server) = in_process_pair(4);
        let first_task = tokio::spawn(harness.server.clone().serve_stream(first_server));
        let second_task = tokio::spawn(harness.server.clone().serve_stream(second_server));
        tokio::join!(handshake(&mut first), handshake(&mut second));
        let (first_session, second_session) = tokio::join!(
            rpc(
                &mut first,
                2,
                "session.create",
                json!({ "cwd": harness._directory.path(), "profile": "primary" }),
            ),
            rpc(
                &mut second,
                2,
                "session.create",
                json!({ "cwd": harness._directory.path(), "profile": "primary" }),
            )
        );
        assert_ne!(
            first_session["session"]["id"],
            second_session["session"]["id"]
        );

        drop(first);
        drop(second);
        first_task
            .await
            .expect("first task")
            .expect("stream result");
        second_task
            .await
            .expect("second task")
            .expect("stream result");
    }

    #[tokio::test]
    async fn disconnect_cancels_output_forwarding() {
        let harness = harness();
        let (observer, mut observed) = mpsc::channel(1);
        harness.server.observe_next_connection(observer);
        let release = Arc::new(tokio::sync::Notify::new());
        let stream_task = tokio::spawn(harness.server.clone().serve_stream(ErroringStream {
            release: release.clone(),
        }));
        let connection_shutdown = observed.recv().await.expect("connection token");
        let (_output_tx, output_rx) = mpsc::channel(1);
        let (notifications, mut notification_rx) = mpsc::channel(1);
        let forward_task = tokio::spawn(forward_output(
            OutputStream::Stdout,
            cookie_agent_protocol::OutputSnapshot {
                call_id: cookie_agent_protocol::ToolCallId::new_v7(),
                start_offset: 0,
                end_offset: 0,
                chunks: Vec::new(),
            },
            output_rx,
            notifications,
            connection_shutdown,
        ));
        let _ = notification_rx.recv().await.expect("snapshot notification");
        release.notify_one();
        assert!(matches!(
            stream_task.await.expect("stream task"),
            Err(TransportError::Closed)
        ));
        let _ = tokio::time::timeout(Duration::from_secs(1), forward_task)
            .await
            .expect("forwarder stopped")
            .expect("forwarder task");
    }

    #[tokio::test]
    async fn output_forwarding_sends_pre_read_gap_then_snapshot_then_live_delta() {
        let call_id = cookie_agent_protocol::ToolCallId::new_v7();
        let (output_tx, output_rx) = mpsc::channel(2);
        output_tx
            .send(OutputMessage::Gap(cookie_agent_protocol::OutputGap {
                call_id,
                stream: OutputStream::Stdout,
                next_offset: 3,
            }))
            .await
            .expect("queue eviction gap");
        output_tx
            .send(OutputMessage::Delta(cookie_agent_protocol::OutputDelta {
                call_id,
                stream: OutputStream::Stdout,
                byte_offset: 6,
                data: "bGl2ZQ==".into(),
            }))
            .await
            .expect("queue live delta");
        let (notifications, mut notification_rx) = mpsc::channel(3);
        let task = tokio::spawn(forward_output(
            OutputStream::Stdout,
            cookie_agent_protocol::OutputSnapshot {
                call_id,
                start_offset: 3,
                end_offset: 6,
                chunks: Vec::new(),
            },
            output_rx,
            notifications,
            CancellationToken::new(),
        ));

        assert_eq!(
            notification_rx.recv().await.expect("gap")["method"],
            "events.tool_output_gap"
        );
        assert_eq!(
            notification_rx.recv().await.expect("snapshot")["method"],
            "events.tool_output_snapshot"
        );
        assert_eq!(
            notification_rx.recv().await.expect("live delta")["method"],
            "events.tool_output_delta"
        );
        drop(output_tx);
        task.await
            .expect("forward task")
            .expect("output forwarding");
    }

    #[tokio::test]
    async fn output_forwarding_keeps_a_pre_read_delta_without_a_gap() {
        let call_id = cookie_agent_protocol::ToolCallId::new_v7();
        let (output_tx, output_rx) = mpsc::channel(1);
        output_tx
            .send(OutputMessage::Delta(cookie_agent_protocol::OutputDelta {
                call_id,
                stream: OutputStream::Stdout,
                byte_offset: 0,
                data: "bGl2ZQ==".into(),
            }))
            .await
            .expect("queue delta between subscription and forwarding");
        let (notifications, mut notification_rx) = mpsc::channel(2);
        let task = tokio::spawn(forward_output(
            OutputStream::Stdout,
            cookie_agent_protocol::OutputSnapshot {
                call_id,
                start_offset: 0,
                end_offset: 0,
                chunks: Vec::new(),
            },
            output_rx,
            notifications,
            CancellationToken::new(),
        ));

        assert_eq!(
            notification_rx.recv().await.expect("snapshot")["method"],
            "events.tool_output_snapshot"
        );
        assert_eq!(
            notification_rx.recv().await.expect("held delta")["method"],
            "events.tool_output_delta"
        );
        drop(output_tx);
        task.await
            .expect("forward task")
            .expect("output forwarding");
    }

    #[tokio::test]
    async fn connection_shutdown_guard_cancels_during_unwind() {
        let token = CancellationToken::new();
        let observed = token.clone();
        let task = tokio::spawn(async move {
            let _guard = ConnectionShutdown(token);
            panic!("simulated connection panic");
        });
        assert!(task.await.expect_err("task panicked").is_panic());
        tokio::time::timeout(Duration::from_secs(1), observed.cancelled())
            .await
            .expect("connection token was cancelled during unwind");
    }

    #[tokio::test]
    async fn in_process_session_run_idempotency_steering_and_cursor_replay() {
        let harness = harness();
        let (mut client, server_stream) = in_process_pair(4);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;

        let created = rpc(
            &mut client,
            2,
            "session.create",
            json!({
                "cwd": harness._directory.path(), "profile": "primary"
            }),
        )
        .await;
        let session_id = created["session"]["id"].clone();
        let first = rpc(
            &mut client,
            3,
            "run.start",
            json!({
                "session_id": session_id, "client_run_id": "client-run", "input": "hello"
            }),
        )
        .await;
        let second = rpc(
            &mut client,
            4,
            "run.start",
            json!({
                "session_id": session_id, "client_run_id": "client-run", "input": "hello"
            }),
        )
        .await;
        assert_eq!(first, second);
        client
            .send(MessageFrame::Value(json!({
                "jsonrpc": "2.0", "id": 5, "method": "run.start",
                "params": {
                    "session_id": session_id,
                    "client_run_id": "client-run",
                    "input": "different input"
                }
            })))
            .await
            .expect("send conflicting start");
        let conflict = next_value(&mut client).await;
        assert_eq!(conflict["error"]["code"], -32602);
        assert_eq!(conflict["error"]["data"]["code"], "idempotency_conflict");
        let steered = rpc(
            &mut client,
            6,
            "run.steer",
            json!({
                "run_id": first["run_id"], "input": "more context"
            }),
        )
        .await;
        assert_eq!(steered, json!({ "accepted": true }));

        let replay = rpc(
            &mut client,
            7,
            "events.subscribe",
            json!({
                "session_id": session_id, "cursor": 0
            }),
        )
        .await;
        assert!(replay["events"].as_array().expect("event replay").len() >= 3);

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn in_process_gap_can_be_resubscribed_from_its_exclusive_cursor() {
        let harness = harness();
        let session = harness
            .engine
            .create_session(harness._directory.path(), "primary")
            .expect("session");
        let (mut client, server_stream) = in_process_pair(1);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;
        let _ = rpc(
            &mut client,
            2,
            "events.subscribe",
            json!({ "session_id": session.id }),
        )
        .await;

        for index in 0..800 {
            harness
                .engine
                .append(
                    session.id,
                    None,
                    Event::TextDelta {
                        text: index.to_string(),
                    },
                )
                .await
                .expect("append");
        }
        let mut gap_cursor = None;
        for _ in 0..1_100 {
            let frame = client
                .recv()
                .await
                .expect("notification")
                .expect("open stream");
            let notification = frame_value(frame);
            if notification["method"] == "events.subscription"
                && notification["params"]["type"] == "gap"
            {
                assert_eq!(
                    notification["params"]["session_id"],
                    json!(session.id.to_string())
                );
                gap_cursor = notification["params"]["last_delivered_seq"].as_u64();
                break;
            }
        }
        let gap_cursor = gap_cursor.expect("bounded subscription emits a gap");
        let replay = rpc(
            &mut client,
            3,
            "events.subscribe",
            json!({
                "session_id": session.id, "cursor": gap_cursor
            }),
        )
        .await;
        assert!(
            !replay["events"]
                .as_array()
                .expect("replay events")
                .is_empty()
        );

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn multi_session_subscription_gaps_identify_their_sessions() {
        let harness = harness();
        let first = harness
            .engine
            .create_session(harness._directory.path(), "primary")
            .expect("first session");
        let second = harness
            .engine
            .create_session(harness._directory.path(), "primary")
            .expect("second session");
        let (mut client, server_stream) = in_process_pair(1);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;
        let _ = rpc(
            &mut client,
            2,
            "events.subscribe",
            json!({ "session_id": first.id }),
        )
        .await;
        let _ = rpc(
            &mut client,
            3,
            "events.subscribe",
            json!({ "session_id": second.id }),
        )
        .await;

        for session in [first.id, second.id] {
            for index in 0..800 {
                harness
                    .engine
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
        }

        let mut gap_sessions = std::collections::HashSet::new();
        for _ in 0..2_400 {
            let notification = next_value(&mut client).await;
            if notification["method"] == "events.subscription"
                && notification["params"]["type"] == "gap"
            {
                gap_sessions.insert(notification["params"]["session_id"].clone());
                if gap_sessions.len() == 2 {
                    break;
                }
            }
        }
        assert_eq!(
            gap_sessions,
            std::collections::HashSet::from([
                json!(first.id.to_string()),
                json!(second.id.to_string())
            ])
        );

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn session_rename_replays_and_rejects_conflicts() {
        let harness = harness();
        let session = harness
            .engine
            .create_session(harness._directory.path(), "primary")
            .expect("session");
        let (mut client, server_stream) = in_process_pair(4);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;

        let params = json!({
            "session_id": session.id,
            "client_rename_id": "rename-1",
            "change": { "type": "set", "title": "Exact title" }
        });
        let first = rpc(&mut client, 2, "session.rename", params.clone()).await;
        let replay = rpc(&mut client, 3, "session.rename", params).await;
        assert_eq!(first, replay);
        assert_eq!(first["session"]["title"], "Exact title");

        let conflict = rpc_response(
            &mut client,
            4,
            "session.rename",
            json!({
                "session_id": session.id,
                "client_rename_id": "rename-1",
                "change": { "type": "clear" }
            }),
        )
        .await;
        assert_eq!(conflict["error"]["code"], -32602);
        assert_eq!(conflict["error"]["data"]["code"], "idempotency_conflict");

        let malformed = rpc_response(
            &mut client,
            5,
            "session.rename",
            json!({
                "session_id": session.id,
                "client_rename_id": "rename-2",
                "change": { "type": "set", "title": "" }
            }),
        )
        .await;
        assert_eq!(malformed["error"]["code"], -32602);
        assert_eq!(malformed["error"]["data"]["code"], "invalid_title");
        assert_eq!(
            malformed["error"]["data"]["session_id"],
            session.id.to_string()
        );
        assert_eq!(malformed["error"]["data"]["client_rename_id"], "rename-2");

        for (id, title) in [
            ("rename-control", "bad\ntitle".to_owned()),
            ("rename-too-long", "x".repeat(SessionTitle::MAX_BYTES + 1)),
        ] {
            let invalid = rpc_response(
                &mut client,
                6,
                "session.rename",
                json!({
                    "session_id": session.id,
                    "client_rename_id": id,
                    "change": { "type": "set", "title": title }
                }),
            )
            .await;
            assert_eq!(invalid["error"]["data"]["code"], "invalid_title");
            assert_eq!(invalid["error"]["data"]["client_rename_id"], id);
        }

        let invalid_shape = rpc_response(
            &mut client,
            7,
            "session.rename",
            json!({
                "session_id": session.id,
                "client_rename_id": "rename-shape",
                "change": { "type": "set", "title": "valid", "legacy": true }
            }),
        )
        .await;
        assert_eq!(invalid_shape["error"]["code"], -32602);
        assert!(invalid_shape["error"].get("data").is_none());

        let missing = rpc_response(
            &mut client,
            8,
            "session.rename",
            json!({
                "session_id": cookie_agent_protocol::SessionId::new_v7(),
                "client_rename_id": "rename-missing",
                "change": { "type": "clear" }
            }),
        )
        .await;
        assert_eq!(missing["error"]["data"]["code"], "session_not_found");

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn run_start_profile_override_passes_through_unchanged() {
        let harness = harness();
        let session = harness
            .engine
            .create_session(harness._directory.path(), "primary")
            .expect("session");
        let (mut client, server_stream) = in_process_pair(4);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;

        let _ = rpc(
            &mut client,
            2,
            "run.start",
            json!({
                "session_id": session.id,
                "client_run_id": "profile-run",
                "input": "use the draft profile",
                "profile": "draft"
            }),
        )
        .await;
        let (replay, receiver) = harness
            .engine
            .subscribe(session.id, None)
            .await
            .expect("events");
        drop(receiver);
        let started = replay
            .events
            .iter()
            .find_map(|envelope| match &envelope.event {
                Event::RunStarted {
                    client_run_id,
                    current_profile,
                    ..
                } if client_run_id == "profile-run" => Some(current_profile),
                _ => None,
            });
        assert_eq!(started.expect("run started").name, "draft");

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn approval_respond_routes_to_its_session() {
        let harness = approval_harness();
        let session = harness
            .engine
            .create_session(harness._directory.path(), "primary")
            .expect("session");
        let approval_id = cookie_agent_protocol::ApprovalId::new_v7();
        let request = approval_request(approval_id, "approval-test", 1, false, true);
        let operation_fingerprint = request.operation_fingerprint().clone();
        let (_, mut events) = harness
            .engine
            .subscribe(session.id, None)
            .await
            .expect("subscribe before approval request");
        let run_id = cookie_agent_protocol::RunId::new_v7();
        harness
            .engine
            .append(
                session.id,
                Some(run_id),
                Event::ApprovalRequested {
                    request: request.clone(),
                },
            )
            .await
            .expect("append approval request");
        let (mut client, server_stream) = in_process_pair(4);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;
        let response_params = json!({
            "session_id": session.id,
            "approval_id": approval_id,
            "request_revision": 1,
            "operation_fingerprint": operation_fingerprint,
            "client_response_id": "approval-response-1",
            "decision": "approve_once"
        });
        let too_early =
            rpc_response(&mut client, 2, "approval.respond", response_params.clone()).await;
        assert_eq!(too_early["error"]["data"]["code"], "approval_not_pending");
        harness
            .engine
            .append(
                session.id,
                Some(run_id),
                Event::ApprovalEscalated {
                    approval_id,
                    reason_code: cookie_agent_protocol::ApprovalReasonCode::Escalated,
                },
            )
            .await
            .expect("append approval escalation");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let EventSubscriptionMessage::Event { event } =
                    events.recv().await.expect("approval subscription closed")
                    && matches!(
                        event.event,
                        Event::ApprovalEscalated { approval_id: found, .. }
                            if found == approval_id
                    )
                {
                    return;
                }
            }
        })
        .await
        .expect("durable approval escalation");
        let result = rpc(&mut client, 3, "approval.respond", response_params).await;
        assert_eq!(result["client_response_id"], "approval-response-1");
        assert_eq!(result["approval"]["status"], "approved");
        assert_eq!(result["approval"]["request"], json!(request));

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn approval_respond_revalidates_live_filesystem_binding_before_execution() {
        let directory = tempfile::tempdir().expect("temporary directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("private temporary directory");
        }
        let target = directory.path().join("target.txt");
        fs::write(&target, "original").expect("write filesystem fixture");
        let executed = Arc::new(AtomicBool::new(false));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind model fixture");
        let address = listener.local_addr().expect("model fixture address");
        let arguments =
            serde_json::to_string(&json!({ "filePath": "target.txt" })).expect("tool arguments");
        let model_event = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_filesystem_binding",
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "id": "fc_filesystem_binding",
                    "call_id": "call_filesystem_binding",
                    "name": "read",
                    "arguments": arguments
                }],
                "usage": { "input_tokens": 3, "output_tokens": 4 }
            }
        });
        let model_task = tokio::spawn(serve_responses_fixture(
            listener,
            format!("data: {model_event}\n\n"),
        ));
        let config = test_config_at(&directory, Some("read"), &format!("http://{address}/v1"));
        let catalog = Arc::new(Catalog::embedded().expect("catalog"));
        let model_manager = Arc::new(
            ModelSetManager::new(
                config.models.clone(),
                Arc::clone(&catalog),
                CredentialStore::new(directory.path().join("credentials")),
            )
            .expect("model manager"),
        );
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config,
            model_manager: Arc::clone(&model_manager),
            tools: vec![Arc::new(FilesystemBindingTool {
                target: target.clone(),
                executed: Arc::clone(&executed),
            })],
        })
        .expect("open engine");
        let server = Arc::new(Server::new(
            engine.clone(),
            model_manager,
            Arc::clone(&catalog),
        ));
        let session = engine
            .create_session(directory.path(), "primary")
            .expect("session");
        let (_, mut events) = engine
            .subscribe(session.id, None)
            .await
            .expect("subscribe before run");
        let (mut client, server_stream) = in_process_pair(8);
        let server_task = tokio::spawn(server.serve_stream(server_stream));
        handshake(&mut client).await;
        let _ = rpc(
            &mut client,
            2,
            "run.start",
            json!({
                "session_id": session.id,
                "client_run_id": "filesystem-binding-run",
                "input": "read target.txt"
            }),
        )
        .await;
        let request = tokio::time::timeout(Duration::from_secs(2), async {
            let mut request = None;
            loop {
                let EventSubscriptionMessage::Event { event } =
                    events.recv().await.expect("approval subscription closed")
                else {
                    continue;
                };
                match event.event {
                    Event::ApprovalRequested { request: found } => {
                        assert!(!found.operation().resources().is_empty());
                        assert!(!found.evaluations().is_empty());
                        request = Some(found);
                    }
                    Event::ApprovalEscalated { approval_id, .. }
                        if request
                            .as_ref()
                            .is_some_and(|request| request.approval_id() == approval_id) =>
                    {
                        return request.expect("approval request before escalation");
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("filesystem approval escalation");
        model_task.await.expect("model fixture task");

        fs::rename(&target, directory.path().join("original-target.txt"))
            .expect("replace prepared target");
        fs::write(&target, "replacement").expect("write replacement target");
        let response = rpc_response(
            &mut client,
            3,
            "approval.respond",
            json!({
                "session_id": session.id,
                "approval_id": request.approval_id(),
                "request_revision": 1,
                "operation_fingerprint": request.operation_fingerprint(),
                "client_response_id": "filesystem-binding-response",
                "decision": "approve_once"
            }),
        )
        .await;
        assert_eq!(response["error"]["code"], -32602);
        let failure: ApprovalRespondError =
            serde_json::from_value(response["error"]["data"].clone())
                .expect("typed approval response error");
        assert_eq!(failure.code, ApprovalRespondErrorCode::OperationChanged);
        assert_eq!(failure.session_id, session.id);
        assert_eq!(failure.approval_id, request.approval_id());
        assert_eq!(failure.client_response_id, "filesystem-binding-response");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let EventSubscriptionMessage::Event { event } =
                    events.recv().await.expect("run subscription closed")
                    && matches!(event.event, Event::ToolCallFailed { .. })
                {
                    return;
                }
            }
        })
        .await
        .expect("tool failure after invalidated approval");
        assert!(!executed.load(Ordering::SeqCst));
        let (replay, live) = engine
            .subscribe(session.id, None)
            .await
            .expect("replay after invalidation");
        drop(live);
        assert!(
            !replay
                .events
                .iter()
                .any(|event| { matches!(event.event, Event::ToolCallCompleted { .. }) })
        );

        drop(client);
        server_task
            .await
            .expect("server task")
            .expect("stream result");
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn approval_list_is_handshake_gated_and_uses_exact_v6_shapes() {
        let harness = approval_harness();
        let session = harness
            .engine
            .create_session(harness._directory.path(), "primary")
            .expect("session");
        let approval_id = cookie_agent_protocol::ApprovalId::new_v7();
        let request = approval_request(approval_id, "approval-list", 1, false, true);
        seed_escalated_approval(&harness, session.id, request).await;
        let (mut client, server_stream) = in_process_pair(4);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));

        let blocked = rpc_response(
            &mut client,
            1,
            "approval.list",
            json!({ "root_session_id": session.id }),
        )
        .await;
        assert_eq!(blocked["jsonrpc"], "2.0");
        assert_eq!(blocked["error"]["code"], -32001);
        handshake(&mut client).await;

        let listed = rpc(
            &mut client,
            2,
            "approval.list",
            json!({
                "root_session_id": session.id,
                "status": "escalated"
            }),
        )
        .await;
        assert_eq!(listed["approvals"].as_array().expect("approvals").len(), 1);
        assert_eq!(listed["approvals"][0]["status"], "escalated");
        assert_eq!(
            listed["approvals"][0]["request"]["approval_id"],
            json!(approval_id)
        );
        assert_eq!(listed["tree_grants"], json!([]));

        let pending = rpc(
            &mut client,
            3,
            "approval.list",
            json!({
                "root_session_id": session.id,
                "status": "pending"
            }),
        )
        .await;
        assert_eq!(pending, json!({ "approvals": [], "tree_grants": [] }));

        let malformed = rpc_response(
            &mut client,
            4,
            "approval.list",
            json!({
                "root_session_id": session.id,
                "status": "legacy_waiting"
            }),
        )
        .await;
        assert_eq!(malformed["jsonrpc"], "2.0");
        assert_eq!(malformed["error"]["code"], -32602);
        assert!(malformed["error"].get("data").is_none());

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn approval_respond_maps_atomic_structured_failures_and_exact_replay() {
        let harness = approval_harness();
        let session = harness
            .engine
            .create_session(harness._directory.path(), "primary")
            .expect("session");
        let approval_id = cookie_agent_protocol::ApprovalId::new_v7();
        let request = approval_request(approval_id, "bound-operation", 7, false, false);
        let fingerprint = request.operation_fingerprint().clone();
        seed_escalated_approval(&harness, session.id, request.clone()).await;
        let (mut client, server_stream) = in_process_pair(8);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;

        let base = json!({
            "session_id": session.id,
            "approval_id": approval_id,
            "request_revision": 7,
            "operation_fingerprint": fingerprint,
            "client_response_id": "bound-response",
            "decision": "approve_once"
        });
        let mut stale = base.clone();
        stale["request_revision"] = json!(6);
        let stale = rpc_response(&mut client, 2, "approval.respond", stale).await;
        assert_eq!(stale["error"]["code"], -32602);
        assert_eq!(stale["error"]["data"]["code"], "approval_revision_conflict");
        assert_eq!(stale["error"]["data"]["expected_revision"], 7);
        assert_eq!(stale["error"]["data"]["found_revision"], 6);

        let mut changed = base.clone();
        changed["operation_fingerprint"] = json!(
            approval_request(
                cookie_agent_protocol::ApprovalId::new_v7(),
                "changed-operation",
                1,
                false,
                true,
            )
            .operation_fingerprint()
            .clone()
        );
        let changed = rpc_response(&mut client, 3, "approval.respond", changed).await;
        assert_eq!(
            changed["error"]["data"]["code"],
            "operation_fingerprint_mismatch"
        );
        assert!(changed["error"]["data"].get("expected_revision").is_none());

        let mut disallowed = base.clone();
        disallowed["decision"] = json!("approve_tree");
        let disallowed = rpc_response(&mut client, 4, "approval.respond", disallowed).await;
        assert_eq!(disallowed["error"]["data"]["code"], "decision_not_allowed");

        let accepted = rpc(&mut client, 5, "approval.respond", base.clone()).await;
        let replay = rpc(&mut client, 6, "approval.respond", base.clone()).await;
        assert_eq!(accepted, replay);

        let mut conflicting = base;
        conflicting["decision"] = json!("reject");
        let conflicting = rpc_response(&mut client, 7, "approval.respond", conflicting).await;
        assert_eq!(conflicting["error"]["data"]["code"], "idempotency_conflict");

        let not_pending = rpc_response(
            &mut client,
            8,
            "approval.respond",
            json!({
                "session_id": session.id,
                "approval_id": approval_id,
                "request_revision": 7,
                "operation_fingerprint": fingerprint,
                "client_response_id": "after-final",
                "decision": "reject"
            }),
        )
        .await;
        assert_eq!(not_pending["error"]["data"]["code"], "approval_not_pending");

        let not_found = rpc_response(
            &mut client,
            9,
            "approval.respond",
            json!({
                "session_id": session.id,
                "approval_id": cookie_agent_protocol::ApprovalId::new_v7(),
                "request_revision": 1,
                "operation_fingerprint": fingerprint,
                "client_response_id": "missing-approval",
                "decision": "reject"
            }),
        )
        .await;
        assert_eq!(not_found["error"]["data"]["code"], "approval_not_found");

        let missing_session = rpc_response(
            &mut client,
            10,
            "approval.respond",
            json!({
                "session_id": cookie_agent_protocol::SessionId::new_v7(),
                "approval_id": cookie_agent_protocol::ApprovalId::new_v7(),
                "request_revision": 1,
                "operation_fingerprint": fingerprint,
                "client_response_id": "missing-session",
                "decision": "reject"
            }),
        )
        .await;
        assert_eq!(
            missing_session["error"]["data"]["code"],
            "approval_not_found"
        );

        drop(client);
        task.await.expect("server task").expect("stream result");
    }

    #[tokio::test]
    async fn concurrent_approval_response_cancel_and_expiry_mapping_is_atomic() {
        let harness = approval_harness();
        let session = harness
            .engine
            .create_session(harness._directory.path(), "primary")
            .expect("session");

        let response_id = cookie_agent_protocol::ApprovalId::new_v7();
        let response_request = approval_request(response_id, "response-race", 1, false, true);
        let response_fingerprint = response_request.operation_fingerprint().clone();
        seed_escalated_approval(&harness, session.id, response_request).await;
        let (mut first, first_server) = in_process_pair(4);
        let (mut second, second_server) = in_process_pair(4);
        let first_task = tokio::spawn(harness.server.clone().serve_stream(first_server));
        let second_task = tokio::spawn(harness.server.clone().serve_stream(second_server));
        tokio::join!(handshake(&mut first), handshake(&mut second));
        let base = json!({
            "session_id": session.id,
            "approval_id": response_id,
            "request_revision": 1,
            "operation_fingerprint": response_fingerprint,
            "decision": "reject"
        });
        let mut first_params = base.clone();
        first_params["client_response_id"] = json!("response-race-first");
        let mut second_params = base;
        second_params["client_response_id"] = json!("response-race-second");
        let (first_result, second_result) = tokio::join!(
            rpc_response(&mut first, 2, "approval.respond", first_params),
            rpc_response(&mut second, 2, "approval.respond", second_params),
        );
        let results = [first_result, second_result];
        assert_eq!(
            results
                .iter()
                .filter(|result| result.get("result").is_some())
                .count(),
            1
        );
        let loser = results
            .iter()
            .find(|result| result.get("error").is_some())
            .expect("one losing response");
        assert_eq!(loser["error"]["data"]["code"], "approval_not_pending");
        drop(first);
        drop(second);
        first_task
            .await
            .expect("first task")
            .expect("stream result");
        second_task
            .await
            .expect("second task")
            .expect("stream result");

        let cancel_id = cookie_agent_protocol::ApprovalId::new_v7();
        let cancel_request = approval_request(cancel_id, "cancel-race", 1, false, true);
        let cancel_fingerprint = cancel_request.operation_fingerprint().clone();
        seed_escalated_approval(&harness, session.id, cancel_request).await;
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let cancel_task = {
            let engine = harness.engine.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                engine
                    .append(
                        session.id,
                        None,
                        Event::ApprovalCancelled {
                            approval_id: cancel_id,
                            reason_code:
                                cookie_agent_protocol::ApprovalReasonCode::RequestCancelled,
                        },
                    )
                    .await
            })
        };
        let (mut cancel_client, cancel_server) = in_process_pair(4);
        let cancel_server_task = tokio::spawn(harness.server.clone().serve_stream(cancel_server));
        handshake(&mut cancel_client).await;
        barrier.wait().await;
        let cancel_response = rpc_response(
            &mut cancel_client,
            2,
            "approval.respond",
            json!({
                "session_id": session.id,
                "approval_id": cancel_id,
                "request_revision": 1,
                "operation_fingerprint": cancel_fingerprint,
                "client_response_id": "cancel-race-response",
                "decision": "reject"
            }),
        )
        .await;
        cancel_task
            .await
            .expect("cancel task")
            .expect("append cancellation");
        if let Some(error) = cancel_response.get("error") {
            assert_eq!(error["data"]["code"], "approval_not_pending");
        } else {
            assert!(cancel_response.get("result").is_some());
        }
        drop(cancel_client);
        cancel_server_task
            .await
            .expect("cancel server task")
            .expect("stream result");

        let expiry_id = cookie_agent_protocol::ApprovalId::new_v7();
        let mut expiry_wire =
            serde_json::to_value(approval_request(expiry_id, "expiry-race", 1, false, true))
                .expect("expiry request wire");
        expiry_wire["constraints"]["expires_at"] = json!("2000-01-01T00:00:00Z");
        let expiry_request: cookie_agent_protocol::ApprovalRequest =
            serde_json::from_value(expiry_wire).expect("expiry request");
        let expiry_fingerprint = expiry_request.operation_fingerprint().clone();
        seed_escalated_approval(&harness, session.id, expiry_request).await;
        let (mut expiry_first, expiry_first_server) = in_process_pair(4);
        let (mut expiry_second, expiry_second_server) = in_process_pair(4);
        let expiry_first_task =
            tokio::spawn(harness.server.clone().serve_stream(expiry_first_server));
        let expiry_second_task =
            tokio::spawn(harness.server.clone().serve_stream(expiry_second_server));
        tokio::join!(handshake(&mut expiry_first), handshake(&mut expiry_second));
        let expiry_params = json!({
            "session_id": session.id,
            "approval_id": expiry_id,
            "request_revision": 1,
            "operation_fingerprint": expiry_fingerprint,
            "client_response_id": "expiry-first",
            "decision": "reject"
        });
        let mut expiry_other = expiry_params.clone();
        expiry_other["client_response_id"] = json!("expiry-second");
        let (expiry_first_result, expiry_second_result) = tokio::join!(
            rpc_response(&mut expiry_first, 2, "approval.respond", expiry_params),
            rpc_response(&mut expiry_second, 2, "approval.respond", expiry_other),
        );
        for response in [expiry_first_result, expiry_second_result] {
            assert_eq!(response["error"]["data"]["code"], "approval_not_pending");
        }
        drop(expiry_first);
        drop(expiry_second);
        expiry_first_task
            .await
            .expect("expiry first task")
            .expect("stream result");
        expiry_second_task
            .await
            .expect("expiry second task")
            .expect("stream result");
    }

    #[tokio::test]
    async fn websocket_gate_rejects_missing_wrong_and_browser_credentials() {
        let harness = harness();
        let running = harness
            .server
            .clone()
            .serve(0)
            .await
            .expect("serve localhost");
        let url = format!("ws://{}/ws", running.address());
        let token = load_or_create_token(&harness.server.token_path).expect("websocket token");

        let missing = tokio_tungstenite::connect_async(url.clone())
            .await
            .expect_err("missing token rejected");
        assert_eq!(websocket_http_status(missing), StatusCode::UNAUTHORIZED);

        let mut wrong = url.clone().into_client_request().expect("request");
        wrong.headers_mut().insert(
            "Authorization",
            "Bearer wrong-token".parse().expect("authorization"),
        );
        let wrong = tokio_tungstenite::connect_async(wrong)
            .await
            .expect_err("wrong token rejected");
        assert_eq!(websocket_http_status(wrong), StatusCode::UNAUTHORIZED);

        let mut browser = url.clone().into_client_request().expect("request");
        browser.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}").parse().expect("authorization"),
        );
        browser
            .headers_mut()
            .insert("Origin", "https://example.test".parse().expect("origin"));
        let browser = tokio_tungstenite::connect_async(browser)
            .await
            .expect_err("browser origin rejected");
        assert_eq!(websocket_http_status(browser), StatusCode::FORBIDDEN);

        let mut valid = url.into_client_request().expect("request");
        valid.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}").parse().expect("authorization"),
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(valid)
            .await
            .expect("valid token accepted");
        socket.close(None).await.expect("close");

        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            let parent = fs::metadata(harness.server.token_path.parent().expect("parent"))
                .expect("parent metadata");
            let file = fs::metadata(&harness.server.token_path).expect("token metadata");
            assert_eq!(parent.permissions().mode() & 0o777, 0o700);
            assert_eq!(file.permissions().mode() & 0o777, 0o600);
            assert_eq!(file.uid(), rustix::process::getuid().as_raw());
        }

        harness.server.shutdown();
        running.wait().await;
    }

    #[tokio::test]
    async fn websocket_handshake_create_and_subscribe() {
        let harness = harness();
        let running = harness
            .server
            .clone()
            .serve(0)
            .await
            .expect("serve localhost");
        let url = format!("ws://{}/ws", running.address());
        let token = load_or_create_token(&harness.server.token_path).expect("websocket token");
        let mut request = url.into_client_request().expect("websocket request");
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}").parse().expect("authorization"),
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("connect websocket");

        async fn ws_rpc<T: Serialize>(
            socket: &mut tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            id: i64,
            method: &str,
            params: T,
        ) -> Value {
            let request = Request::new(
                JsonRpcId::Number(id),
                method,
                Some(serde_json::to_value(params).expect("params")),
            );
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::to_string(&request).expect("JSON").into(),
                ))
                .await
                .expect("send");
            loop {
                let message = socket
                    .next()
                    .await
                    .expect("socket open")
                    .expect("websocket message");
                if let tokio_tungstenite::tungstenite::Message::Text(frame) = message {
                    let value: Value = serde_json::from_str(&frame).expect("response JSON");
                    if value.get("id") == Some(&json!(id)) {
                        assert!(value.get("error").is_none(), "rpc error: {value}");
                        return value["result"].clone();
                    }
                }
            }
        }

        assert_eq!(
            ws_rpc(
                &mut socket,
                1,
                "handshake",
                ClientHello {
                    protocol_version: ProtocolVersion::current()
                }
            )
            .await,
            json!({ "protocol_version": PROTOCOL_VERSION })
        );
        let created = ws_rpc(
            &mut socket,
            2,
            "session.create",
            json!({
                "cwd": harness._directory.path(), "profile": "primary"
            }),
        )
        .await;
        let subscribed = ws_rpc(
            &mut socket,
            3,
            "events.subscribe",
            json!({
                "session_id": created["session"]["id"]
            }),
        )
        .await;
        assert!(!subscribed["events"].as_array().expect("events").is_empty());

        socket.close(None).await.expect("close websocket");
        harness.server.shutdown();
        running.wait().await;
    }

    #[tokio::test]
    async fn shutdown_closes_an_idle_websocket_before_waiting() {
        let harness = harness();
        let running = harness
            .server
            .clone()
            .serve(0)
            .await
            .expect("serve localhost");
        let url = format!("ws://{}/ws", running.address());
        let token = load_or_create_token(&harness.server.token_path).expect("websocket token");
        let mut request = url.into_client_request().expect("websocket request");
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}").parse().expect("authorization"),
        );
        let (_socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("connect websocket");

        harness.server.shutdown();
        tokio::time::timeout(Duration::from_secs(1), running.wait())
            .await
            .expect("listener stopped with idle connection");
    }
}
