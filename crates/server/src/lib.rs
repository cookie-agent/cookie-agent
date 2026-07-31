//! Transport-neutral JSON-RPC server for the CookieCode engine.

use std::{
    collections::{BTreeSet, HashMap},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

#[cfg(test)]
use std::sync::Mutex;

use async_trait::async_trait;
use axum::{
    Router,
    extract::{State, WebSocketUpgrade},
    response::Response,
    routing::get,
};
use cookiecode_config::Config;
use cookiecode_engine::{Engine, EngineError, events::OutputMessage};
use cookiecode_protocol::{
    AgentListParams, AgentListResult, AgentType, ApprovalRespondParams, ClientHello, ErrorResponse,
    Event, EventSubscriptionMessage, EventsSubscribeParams, JsonRpcError, JsonRpcId,
    ModelDescriptor, ModelRef, Notification, OutputSnapshotEnvelope, OutputStream,
    PROTOCOL_VERSION, ProviderListModelsParams, ProviderListModelsResult, Response as RpcResponse,
    RunCancelParams, RunStartConflict, RunStartConflictCode, RunStartParams, RunSteerParams,
    RunToolStdinParams, ServerHello, SessionChildrenParams, SessionChildrenResult,
    SessionCreateParams, SessionCreateResult, SessionGetParams, SessionGetResult,
    SessionListParams, SessionListResult, SessionResumeParams, SessionResumeResult,
    SessionTreeParams, SessionTreeResult, SuccessResponse,
};
use cookiecode_providers::Provider;
use futures_util::StreamExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
    time::{Duration, sleep},
};
use tokio_util::sync::CancellationToken;

const OUTBOUND_QUEUE_CAPACITY: usize = 512;

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
#[derive(Clone, Debug, PartialEq)]
pub enum MessageFrame {
    Text(String),
    Value(Value),
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

/// The provider instances installed in the process. Model discovery is based
/// on configured profile references because the provider trait is streaming
/// only and deliberately has no remote discovery API.
pub type ProviderRegistry = HashMap<String, Arc<dyn Provider>>;

/// Transport-facing composition of the engine, immutable configuration, and
/// installed provider registry.
#[derive(Clone)]
pub struct Server {
    engine: Engine,
    config: Config,
    providers: ProviderRegistry,
    shutdown: CancellationToken,
    #[cfg(test)]
    connection_observer: Arc<Mutex<Option<mpsc::Sender<CancellationToken>>>>,
}

impl Server {
    #[must_use]
    pub fn new(engine: Engine, config: Config, providers: ProviderRegistry) -> Self {
        Self {
            engine,
            config,
            providers,
            shutdown: CancellationToken::new(),
            #[cfg(test)]
            connection_observer: Arc::new(Mutex::new(None)),
        }
    }

    /// Builds the WebSocket router. The listener is deliberately created by
    /// [`Self::serve`] so it can enforce the localhost-only binding policy.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/ws", get(websocket_upgrade))
            .with_state(self)
    }

    /// Starts the default localhost-only WebSocket transport. `port = 0`
    /// requests an ephemeral localhost port, which is useful to embedders and
    /// integration tests.
    pub async fn serve(self: Arc<Self>, port: u16) -> Result<RunningServer, ServerError> {
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            .await
            .map_err(ServerError::Listen)?;
        let address = listener.local_addr().map_err(ServerError::Listen)?;
        let shutdown = self.shutdown.clone();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, self.router())
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
                                notifications.clone(),
                                &connection_shutdown,
                            ).await;
                            let response = match result {
                                Ok(RouteResult::Handshake) => success_response(id, &ServerHello { protocol_version: PROTOCOL_VERSION })?,
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

    async fn route_after_handshake(
        &self,
        handshaken: &mut bool,
        method: &str,
        params: Option<Value>,
        notifications: mpsc::Sender<Value>,
        shutdown: &CancellationToken,
    ) -> Result<RouteResult, RpcFault> {
        let result = if !*handshaken && method != "handshake" {
            Err(RpcFault::handshake_required())
        } else {
            self.route(method, params, notifications, shutdown).await
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
        notifications: mpsc::Sender<Value>,
        shutdown: &CancellationToken,
    ) -> Result<RouteResult, RpcFault> {
        match method {
            "handshake" => {
                let hello: ClientHello = decode_params(params)?;
                if hello.protocol_version != PROTOCOL_VERSION {
                    return Err(RpcFault::unsupported_version(hello.protocol_version));
                }
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
                value(
                    self.engine
                        .approval_respond(
                            request.session_id,
                            request.approval_id,
                            request.decision,
                            request.scope,
                            request.feedback,
                        )
                        .await
                        .map_err(engine_fault)?,
                )
            }
            "provider.list_models" => {
                let request: ProviderListModelsParams = params_or_default(params)?;
                value(self.list_models(request))
            }
            "agent.list" => {
                let request: AgentListParams = params_or_default(params)?;
                if matches!(
                    request.agent_type,
                    Some(AgentType::SubAgent | AgentType::Internal)
                ) {
                    return Err(RpcFault::invalid_params(
                        "only `primary` and `all` agents are client-invocable",
                    ));
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

    fn list_models(&self, request: ProviderListModelsParams) -> ProviderListModelsResult {
        let mut models = BTreeSet::new();
        for profile in self.config.agents.values() {
            for model in &profile.models {
                if self.providers.contains_key(&model.provider)
                    && request
                        .provider
                        .as_ref()
                        .is_none_or(|provider| provider == &model.provider)
                {
                    models.insert((model.provider.clone(), model.model.clone()));
                }
            }
        }
        ProviderListModelsResult {
            models: models
                .into_iter()
                .map(|(provider, model)| ModelDescriptor {
                    model: ModelRef { provider, model },
                    display_name: None,
                })
                .collect(),
        }
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
                            cookiecode_protocol::EventEnvelope {
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
        call_id: cookiecode_protocol::ToolCallId,
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

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("could not bind localhost websocket listener: {0}")]
    Listen(#[source] std::io::Error),
}

async fn websocket_upgrade(
    State(server): State<Arc<Server>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade.on_upgrade(move |socket| async move {
        let _ = server.serve_stream(WebSocketStream { socket }).await;
    })
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
    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: "invalid params",
            data: Some(json!({ "detail": message.into() })),
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
    fn unsupported_version(version: u32) -> Self {
        Self {
            code: -32002,
            message: "unsupported protocol version",
            data: Some(json!({ "protocol_version": version, "supported": PROTOCOL_VERSION })),
        }
    }
    fn engine(message: String) -> Self {
        Self {
            code: -32000,
            message: "engine error",
            data: Some(json!({ "detail": message })),
        }
    }
    fn internal(message: String) -> Self {
        Self {
            code: -32603,
            message: "internal error",
            data: Some(json!({ "detail": message })),
        }
    }
}

fn engine_fault(error: EngineError) -> RpcFault {
    RpcFault::engine(error.to_string())
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
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(RpcFault::invalid_request("jsonrpc must be `2.0`"));
    }
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty())
        .ok_or_else(|| RpcFault::invalid_request("method must be a non-empty string"))?
        .to_owned();
    let params = object.get("params").cloned();
    match object.get("id") {
        Some(id) => Ok(Incoming::Request {
            id: serde_json::from_value(id.clone()).map_err(|_| {
                RpcFault::invalid_request("id must be null, a string, or an integer")
            })?,
            method,
            params,
        }),
        None => Ok(Incoming::Notification { method, params }),
    }
}

fn decode_params<T: DeserializeOwned>(params: Option<Value>) -> Result<T, RpcFault> {
    serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|error| RpcFault::invalid_params(error.to_string()))
}

fn params_or_default<T>(params: Option<Value>) -> Result<T, RpcFault>
where
    T: DeserializeOwned + Default,
{
    match params {
        Some(params) => serde_json::from_value(params)
            .map_err(|error| RpcFault::invalid_params(error.to_string())),
        None => Ok(T::default()),
    }
}

fn value<T: Serialize>(value: T) -> Result<RouteResult, RpcFault> {
    serde_json::to_value(value)
        .map(RouteResult::Value)
        .map_err(|error| RpcFault::internal(error.to_string()))
}

fn success_response<T: Serialize>(id: JsonRpcId, result: &T) -> Result<Value, TransportError> {
    serde_json::to_value(RpcResponse::Success(SuccessResponse {
        jsonrpc: "2.0".into(),
        id,
        result: serde_json::to_value(result).map_err(|_| TransportError::Closed)?,
    }))
    .map_err(|_| TransportError::Closed)
}

fn error_response(id: Option<JsonRpcId>, fault: RpcFault) -> Result<Value, TransportError> {
    match id {
        Some(id) => serde_json::to_value(RpcResponse::Error(ErrorResponse {
            jsonrpc: "2.0".into(),
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
    snapshot: cookiecode_protocol::OutputSnapshot,
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
        collections::{BTreeMap, HashMap},
        sync::Arc,
    };

    use async_trait::async_trait;
    use cookiecode_config::{AgentProfile, Config, ModelConfig, ProviderConfig, ProviderType};
    use cookiecode_engine::EngineOptions;
    use cookiecode_protocol::{
        ActionKind, ApprovalResource, DecisionTrace, Effect, MatchedPermissionRule, Request,
    };
    use cookiecode_providers::{
        ModelId, NormalizedEvent, ProviderCapabilities, ProviderError, ProviderRequest,
    };
    use futures_util::{SinkExt, StreamExt, stream};
    use serde::Serialize;

    use super::*;

    struct ScriptedFakeProvider;

    struct ErroringStream {
        release: Arc<tokio::sync::Notify>,
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

    #[async_trait]
    impl Provider for ScriptedFakeProvider {
        fn capabilities(&self, _: &ModelId) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            // Keeping the scripted turn open lets the test steer it reliably.
            Ok(stream::pending().boxed())
        }
    }

    struct Harness {
        _directory: tempfile::TempDir,
        engine: Engine,
        server: Arc<Server>,
    }

    fn harness() -> Harness {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut config = Config::default();
        config.providers.insert(
            "fake".into(),
            ProviderConfig {
                kind: ProviderType::OpenAi,
                api_key_env: None,
                base_url: None,
                api: None,
            },
        );
        config.agents = BTreeMap::from([(
            "primary".into(),
            AgentProfile {
                r#type: cookiecode_config::AgentType::Primary,
                models: vec![ModelConfig {
                    provider: "fake".into(),
                    model: "scripted".into(),
                }],
                ..AgentProfile::default()
            },
        )]);
        let provider: Arc<dyn Provider> = Arc::new(ScriptedFakeProvider);
        let providers = HashMap::from([("fake".into(), provider.clone())]);
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: config.clone(),
            providers: providers.clone(),
            tools: Vec::new(),
        })
        .expect("open engine");
        Harness {
            _directory: directory,
            server: Arc::new(Server::new(engine.clone(), config, providers)),
            engine,
        }
    }

    async fn rpc<S: MessageStream, T: Serialize>(
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
                assert!(value.get("error").is_none(), "rpc error: {value}");
                return value["result"].clone();
            }
        }
    }

    fn frame_value(frame: MessageFrame) -> Value {
        match frame {
            MessageFrame::Value(value) => value,
            MessageFrame::Text(text) => serde_json::from_str(&text).expect("JSON frame"),
        }
    }

    async fn handshake<S: MessageStream>(stream: &mut S) {
        let result = rpc(
            stream,
            1,
            "handshake",
            ClientHello {
                protocol_version: PROTOCOL_VERSION,
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
    async fn unsupported_handshake_version_returns_a_stable_error() {
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
        assert_eq!(response["error"]["code"], -32002);
        assert_eq!(response["error"]["data"]["supported"], PROTOCOL_VERSION);

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
            cookiecode_protocol::OutputSnapshot {
                call_id: cookiecode_protocol::ToolCallId::new_v7(),
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
        let call_id = cookiecode_protocol::ToolCallId::new_v7();
        let (output_tx, output_rx) = mpsc::channel(2);
        output_tx
            .send(OutputMessage::Gap(cookiecode_protocol::OutputGap {
                call_id,
                stream: OutputStream::Stdout,
                next_offset: 3,
            }))
            .await
            .expect("queue eviction gap");
        output_tx
            .send(OutputMessage::Delta(cookiecode_protocol::OutputDelta {
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
            cookiecode_protocol::OutputSnapshot {
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
        let call_id = cookiecode_protocol::ToolCallId::new_v7();
        let (output_tx, output_rx) = mpsc::channel(1);
        output_tx
            .send(OutputMessage::Delta(cookiecode_protocol::OutputDelta {
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
            cookiecode_protocol::OutputSnapshot {
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
    async fn approval_respond_routes_to_its_session() {
        let harness = harness();
        let session = harness
            .engine
            .create_session(harness._directory.path(), "primary")
            .expect("session");
        harness
            .engine
            .append(
                session.id,
                None,
                Event::ApprovalRequested {
                    approval_id: "approval-1".into(),
                    action: ActionKind::Bash,
                    resource: "git status".into(),
                    suggested_pattern: "git status".into(),
                    resources: vec![ApprovalResource {
                        action: ActionKind::Bash,
                        resource: "git status".into(),
                        suggested_pattern: "git status".into(),
                    }],
                    decision_trace: DecisionTrace {
                        action: ActionKind::Bash,
                        normalized_resource: "git status".into(),
                        candidates: vec![MatchedPermissionRule {
                            rule_id: None,
                            source_layer: "test".into(),
                            effect: Effect::Ask,
                            hard: false,
                        }],
                        effect: Effect::Ask,
                        precedence_reason: "test".into(),
                    },
                },
            )
            .await
            .expect("approval request");
        let (mut client, server_stream) = in_process_pair(4);
        let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
        handshake(&mut client).await;
        let result = rpc(
            &mut client,
            2,
            "approval.respond",
            json!({
                "session_id": session.id, "approval_id": "approval-1", "decision": "once"
            }),
        )
        .await;
        assert_eq!(
            result,
            json!({ "approval_id": "approval-1", "decision": "once" })
        );

        drop(client);
        task.await.expect("server task").expect("stream result");
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
        let (mut socket, _) = tokio_tungstenite::connect_async(url)
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
                    protocol_version: PROTOCOL_VERSION
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
        let (_socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("connect websocket");

        harness.server.shutdown();
        tokio::time::timeout(Duration::from_secs(1), running.wait())
            .await
            .expect("listener stopped with idle connection");
    }
}
