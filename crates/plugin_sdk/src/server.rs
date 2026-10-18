use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use cookie_agent_protocol::{
    ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, BoundedDisplayText,
    ExtensionAgentBeforeStartParams, ExtensionAgentBeforeStartResult, ExtensionAllowBlockResult,
    ExtensionBusEventParams, ExtensionEmitParams, ExtensionEmitResultParams, ExtensionEmitStatus,
    ExtensionEventParams, ExtensionInitializeParams, ExtensionInitializeResult,
    ExtensionInterceptionHook, ExtensionMessageEndParams, ExtensionMessageEndResult,
    ExtensionModelBeforeRequestParams, ExtensionModelBeforeRequestResult,
    ExtensionModelBeforeSelectParams, ExtensionPluginCapabilities, ExtensionProtocolVersion,
    ExtensionProviderAfterResponseParams, ExtensionProviderAfterResponseResult,
    ExtensionProviderBeforeRequestParams, ExtensionProviderBeforeRequestResult,
    ExtensionSessionBeforeCompactParams, ExtensionSessionBeforeCompactResult,
    ExtensionSessionBeforeForkParams, ExtensionSessionBeforeRevertParams,
    ExtensionSessionBeforeRevertResult, ExtensionToolAfterResultParams,
    ExtensionToolAfterResultResult, ExtensionToolBeforeCallParams, ExtensionToolBeforeCallResult,
    ExtensionUserBeforeInputParams, ExtensionUserBeforeInputResult, PLUGIN_BUS_EVENT_METHOD,
    PLUGIN_EMIT_METHOD, PLUGIN_EMIT_RESULT_METHOD, PLUGIN_EVENT_METHOD, PLUGIN_INITIALIZE_METHOD,
    PLUGIN_INTERCEPT_AGENT_BEFORE_START_METHOD, PLUGIN_INTERCEPT_MESSAGE_END_METHOD,
    PLUGIN_INTERCEPT_MODEL_BEFORE_REQUEST_METHOD, PLUGIN_INTERCEPT_MODEL_BEFORE_SELECT_METHOD,
    PLUGIN_INTERCEPT_PROVIDER_AFTER_RESPONSE_METHOD,
    PLUGIN_INTERCEPT_PROVIDER_BEFORE_REQUEST_METHOD,
    PLUGIN_INTERCEPT_SESSION_BEFORE_COMPACT_METHOD, PLUGIN_INTERCEPT_SESSION_BEFORE_FORK_METHOD,
    PLUGIN_INTERCEPT_SESSION_BEFORE_REVERT_METHOD, PLUGIN_INTERCEPT_TOOL_AFTER_RESULT_METHOD,
    PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD, PLUGIN_INTERCEPT_USER_BEFORE_INPUT_METHOD,
    PLUGIN_PRODUCER_DISCARD_METHOD, PLUGIN_PRODUCER_REGISTER_METHOD, PLUGIN_PRODUCER_SEND_METHOD,
    PLUGIN_PRODUCER_UNREGISTER_METHOD, PLUGIN_RECOVERY_COMPLETE_METHOD,
    PLUGIN_RECOVERY_START_METHOD, PLUGIN_SHUTDOWN_METHOD, PermissionAction, PersistedToolResult,
    PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
    PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity,
    ProducerDeliveryMode, ProducerDiscardParams, ProducerDiscardResult, ProducerId,
    ProducerIdempotencyKey, ProducerMessageId, ProducerName, ProducerRegisterParams,
    ProducerRegisterResult, ProducerSendParams, ProducerSendResult, ProducerUnregisterParams,
    ProducerUnregisterResult, RecoveryCompleteParams, RecoveryCompleteResult, RecoveryOutcome,
    RunId, SafeDisplayText, SafeErrorMessage, SessionId, Sha256Digest, ToolCallId,
    ToolCallPresentation, ToolCompletionOutput, ToolOutputChunk, Transport, errors,
    peer::{Incoming, IncomingNotification, IncomingRequest, LineTransport, Peer, RpcError},
    runtime_protocol::{
        DetachableParams, ExecuteOutcome, ExecuteParams, ExecuteResult, Outcome, PrepareOutcome,
        PrepareParams, PrepareResult, PreparedCall, PreparedRef, RuntimeTools, ToolErrorKind,
        ToolProgressParams, WireProgress, WireToolCall, WireToolError, method,
    },
};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{PluginError, ToolDecl, ToolFailure, ToolOutput};

const NOTIFICATION_CONTEXT_LIFETIME: Duration = Duration::from_secs(4);

type HandlerFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
type ToolHandler =
    Arc<dyn Fn(ToolContext, Value) -> HandlerFuture<Result<ToolOutput, ToolFailure>> + Send + Sync>;
type ResourceFn = Arc<dyn Fn(&Value) -> Option<String> + Send + Sync>;
/// Type-erased interception hook. It parses wire params into their typed form and reports the
/// correlation identity used to scope a context grant; the user handler is deferred until that
/// grant exists and yields the serialized hook result.
type InterceptHandler = Arc<dyn Fn(Value) -> Result<PreparedIntercept, PluginError> + Send + Sync>;

struct PreparedIntercept {
    session_id: SessionId,
    context_id: String,
    call: Box<dyn FnOnce(PluginContext) -> HandlerFuture<Value> + Send>,
}

type EventHandler =
    Arc<dyn Fn(PluginContext, ExtensionEventParams) -> HandlerFuture<()> + Send + Sync>;
type BusHandler =
    Arc<dyn Fn(PluginContext, ExtensionBusEventParams) -> HandlerFuture<()> + Send + Sync>;
type RecoveryHandler = Arc<dyn Fn(PluginContext) -> HandlerFuture<RecoveryResult> + Send + Sync>;

#[derive(Clone)]
struct RegisteredTool {
    declaration: ToolDecl,
    resource: Option<ResourceFn>,
    handler: ToolHandler,
}

#[derive(Clone, Default)]
struct Handlers {
    tools: Vec<RegisteredTool>,
    intercepts: HashMap<ExtensionInterceptionHook, InterceptHandler>,
    event: Option<EventHandler>,
    bus: Option<BusHandler>,
    recovery: Option<RecoveryHandler>,
    publish_bus: bool,
    producers: bool,
}

impl Handlers {
    fn capabilities(&self) -> ExtensionPluginCapabilities {
        ExtensionPluginCapabilities {
            producer_messaging: self.producers,
            subscribe_events: self.event.is_some(),
            subscribe_bus: self.bus.is_some(),
            publish_bus: self.publish_bus,
            intercept: INTERCEPTION_HOOKS
                .iter()
                .copied()
                .filter(|hook| self.intercepts.contains_key(hook))
                .collect(),
        }
    }

    fn tools(&self) -> RuntimeTools {
        RuntimeTools {
            tools: self
                .tools
                .iter()
                .map(|tool| tool.declaration.clone())
                .collect(),
            prompt_sections: Vec::new(),
        }
    }
}

/// Failure returned by a producer recovery callback.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct RecoveryFailure {
    message: SafeErrorMessage,
}

impl RecoveryFailure {
    /// Creates a recovery failure from a bounded, control-free message.
    pub fn new(message: impl Into<String>) -> Result<Self, PluginError> {
        let message = SafeErrorMessage::new(message.into())
            .map_err(|error| PluginError::Protocol(format!("invalid recovery failure: {error}")))?;
        Ok(Self { message })
    }
}

impl From<PluginError> for RecoveryFailure {
    fn from(error: PluginError) -> Self {
        let mut message = error.to_string().replace(char::is_control, " ");
        let mut end = message.len().min(SafeErrorMessage::MAX_BYTES);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        Self {
            message: SafeErrorMessage::new(message)
                .expect("sanitized plugin errors are valid recovery messages"),
        }
    }
}

/// Result returned by a producer recovery callback.
pub type RecoveryResult = Result<(), RecoveryFailure>;

/// A configured cookie-agent plugin server.
pub struct PluginServer {
    name: String,
    version: String,
    handlers: Arc<Handlers>,
}

impl PluginServer {
    /// Starts a builder for a plugin with the configured `name` and its own `version`.
    #[must_use]
    pub fn builder(name: impl Into<String>, version: impl Into<String>) -> PluginServerBuilder {
        PluginServerBuilder {
            name: name.into(),
            version: version.into(),
            handlers: Handlers::default(),
            error: None,
        }
    }

    /// Serves the engine over standard input and output, one JSON-RPC message per line,
    /// until it sends `plugin/shutdown` or standard input ends.
    pub async fn run_stdio(self) -> Result<(), PluginError> {
        self.run_transport(Box::new(LineTransport::new(
            tokio::io::stdin(),
            tokio::io::stdout(),
        )))
        .await
    }

    /// Serves the engine over `reader` and `writer`, one JSON-RPC message per line.
    pub async fn run_io<R, W>(self, reader: R, writer: W) -> Result<(), PluginError>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        self.run_transport(Box::new(LineTransport::new(reader, writer)))
            .await
    }

    /// Serves the engine over `transport`.
    pub async fn run_transport(self, transport: Box<dyn Transport>) -> Result<(), PluginError> {
        let (peer, mut incoming) = Peer::start(transport);
        let server = Arc::new(Server {
            name: self.name,
            version: self.version,
            handlers: self.handlers,
            context: Arc::new(ContextState {
                peer: peer.clone(),
                grants: Mutex::new(HashMap::new()),
                pending_emits: Mutex::new(HashMap::new()),
                publish_bus: AtomicBool::new(false),
                producers: AtomicBool::new(false),
            }),
            prepared: Mutex::new(HashMap::new()),
            executions: Mutex::new(HashMap::new()),
            next_prepared: AtomicU64::new(1),
            stop: CancellationToken::new(),
        });
        loop {
            tokio::select! {
                () = server.stop.cancelled() => break,
                message = incoming.recv() => match message {
                    None => break,
                    Some(Incoming::Request(request)) => {
                        // An execution is registered in arrival order, so a
                        // `tool/cancel` sent after it finds it.
                        let execution = server.admit_execution(&request);
                        let server = Arc::clone(&server);
                        tokio::spawn(async move { server.handle_request(request, execution).await });
                    }
                    Some(Incoming::Notification(notification)) => {
                        server.handle_notification(&notification);
                    }
                },
            }
        }
        server.stop.cancel();
        lock(&server.context.pending_emits).clear();
        peer.close();
        Ok(())
    }
}

/// Builds a [`PluginServer`].
pub struct PluginServerBuilder {
    name: String,
    version: String,
    handlers: Handlers,
    error: Option<PluginError>,
}

impl PluginServerBuilder {
    /// Registers a tool and its asynchronous handler. Its calls are checked
    /// under the permission its declaration names, with no resource.
    #[must_use]
    pub fn tool<F, Fut>(self, declaration: ToolDecl, handler: F) -> Self
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolFailure>> + Send + 'static,
    {
        self.register_tool(declaration, None, handler)
    }

    /// Registers a tool whose calls are checked, and approved, under the
    /// permission resource `resource` derives from their arguments, such as
    /// a path or a host.
    #[must_use]
    pub fn tool_with_resource<F, Fut, P>(
        self,
        declaration: ToolDecl,
        resource: P,
        handler: F,
    ) -> Self
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolFailure>> + Send + 'static,
        P: Fn(&Value) -> Option<String> + Send + Sync + 'static,
    {
        self.register_tool(declaration, Some(Arc::new(resource)), handler)
    }

    fn register_tool<F, Fut>(
        mut self,
        declaration: ToolDecl,
        resource: Option<ResourceFn>,
        handler: F,
    ) -> Self
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolFailure>> + Send + 'static,
    {
        if self.error.is_none()
            && let Err(error) = validate_tool(
                &declaration,
                self.handlers
                    .tools
                    .iter()
                    .any(|tool| tool.declaration.name == declaration.name),
            )
        {
            self.error = Some(error);
            return self;
        }
        self.handlers.tools.push(RegisteredTool {
            declaration,
            resource,
            handler: Arc::new(move |context, arguments| Box::pin(handler(context, arguments))),
        });
        self
    }

    fn intercept<P, F, Fut, R>(mut self, handler: F) -> Self
    where
        P: InterceptParams,
        F: Fn(PluginContext, P) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: Into<P::Result> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let erased: InterceptHandler = Arc::new(move |params| {
            let params: P = serde_json::from_value(params)?;
            let handler = Arc::clone(&handler);
            Ok(PreparedIntercept {
                session_id: params.session_id(),
                context_id: params.context_id().to_owned(),
                call: Box::new(move |context| {
                    let future = handler(context, params);
                    Box::pin(async move {
                        serde_json::to_value(future.await.into())
                            .expect("interception results serialize")
                    })
                }),
            })
        });
        self.handlers.intercepts.insert(P::HOOK, erased);
        self
    }

    /// Registers a session-event notification handler.
    #[must_use]
    pub fn on_event<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionEventParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.handlers.event = Some(Arc::new(move |context, event| {
            Box::pin(handler(context, event))
        }));
        self
    }

    /// Registers a bus-event notification handler.
    #[must_use]
    pub fn on_bus_event<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionBusEventParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.handlers.bus = Some(Arc::new(move |context, event| {
            Box::pin(handler(context, event))
        }));
        self
    }

    /// Enables producer messaging. Producer capability opt-in is off by default.
    #[must_use]
    pub fn enable_producers(mut self) -> Self {
        self.handlers.producers = true;
        self
    }

    /// Registers asynchronous startup recovery for producer-owned external work.
    ///
    /// The SDK sends `producer/recovery/complete` only after this callback returns. When producer
    /// messaging is enabled without a callback, recovery start reports ready asynchronously.
    /// Returning `Ok(())` reports ready; returning [`RecoveryFailure`] reports failed.
    #[must_use]
    pub fn on_recovery<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = RecoveryResult> + Send + 'static,
    {
        self.handlers.recovery = Some(Arc::new(move |context| Box::pin(handler(context))));
        self
    }

    /// Enables non-durable bus publishing through [`PluginContext::emit_bus`].
    #[must_use]
    pub fn enable_bus_publishing(mut self) -> Self {
        self.handlers.publish_bus = true;
        self
    }

    /// Validates registrations and creates the server.
    pub fn build(self) -> Result<PluginServer, PluginError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if !self.handlers.producers && self.handlers.recovery.is_some() {
            return Err(PluginError::Protocol(
                "on_recovery requires enable_producers".into(),
            ));
        }
        Ok(PluginServer {
            name: self.name,
            version: self.version,
            handlers: Arc::new(self.handlers),
        })
    }

    /// Builds and runs this plugin on standard input and output.
    pub async fn run_stdio(self) -> Result<(), PluginError> {
        self.build()?.run_stdio().await
    }

    /// The capabilities this plugin will declare.
    #[cfg(test)]
    pub(crate) fn capabilities(&self) -> ExtensionPluginCapabilities {
        self.handlers.capabilities()
    }
}

/// Context passed to plugin handlers for publishing correlated events and
/// producing messages.
#[derive(Clone)]
pub struct PluginContext {
    state: Arc<ContextState>,
    grant: Option<ScopedGrant>,
}

impl PluginContext {
    /// Publishes a non-durable bus event using this handler's one-shot context grant.
    ///
    /// The server must be configured with
    /// [`PluginServerBuilder::enable_bus_publishing`]. Notification grants expire locally after
    /// four seconds, one second before the engine's authoritative deadline.
    pub async fn emit_bus(
        &self,
        session_id: SessionId,
        name: impl Into<String>,
        payload: Value,
    ) -> Result<ExtensionEmitStatus, PluginError> {
        if !self.state.publish_bus.load(Ordering::Acquire) {
            return Err(PluginError::PublishingNotEnabled("bus"));
        }
        let name = name.into();
        let context_id = self.consume(session_id)?;
        let (reply, receive) = oneshot::channel();
        lock(&self.state.pending_emits)
            .entry(name.clone())
            .or_default()
            .push_back(reply);
        self.state
            .peer
            .notify(
                PLUGIN_EMIT_METHOD,
                &ExtensionEmitParams {
                    session_id,
                    context_id,
                    name,
                    payload,
                },
            )
            .await
            .map_err(|_| PluginError::TransportClosed)?;
        let result = receive.await.map_err(|_| PluginError::TransportClosed)?;
        Ok(result.bus)
    }

    /// Registers a long-lived producer for one session under the name
    /// `default`; see [`Self::register_named_producer`].
    ///
    /// The returned handle has no `Drop` cleanup. Call [`ProducerHandle::unregister`] explicitly,
    /// including when no messages were sent.
    pub async fn register_producer(
        &self,
        session_id: SessionId,
    ) -> Result<ProducerHandle, PluginError> {
        self.register_named_producer(session_id, ProducerName::default_name())
            .await
    }

    /// Registers a long-lived producer named `name` for one session. A
    /// plugin may run any number of named producers; each name keeps its own
    /// idempotency keys, and the session shows it as `plugin:<plugin> · <name>`.
    ///
    /// The returned handle has no `Drop` cleanup. Call [`ProducerHandle::unregister`] explicitly,
    /// including when no messages were sent.
    pub async fn register_named_producer(
        &self,
        session_id: SessionId,
        name: ProducerName,
    ) -> Result<ProducerHandle, PluginError> {
        if !self.state.producers.load(Ordering::Acquire) {
            return Err(PluginError::ProducerMessagingNotEnabled);
        }
        let result: ProducerRegisterResult = self
            .request(
                PLUGIN_PRODUCER_REGISTER_METHOD,
                &ProducerRegisterParams { session_id, name },
            )
            .await?;
        Ok(ProducerHandle {
            context: self.clone(),
            session_id,
            producer_id: result.producer_id,
        })
    }

    /// Discards a waiting message of this plugin, by session and durable receipt.
    ///
    /// No producer registration is required, including after reconnect, and any of the
    /// plugin's producer names may have sent it. The engine rejects consumed messages and
    /// messages a model request has claimed; an already-discarded message succeeds. This
    /// never unregisters a producer.
    pub async fn discard_producer_message(
        &self,
        session_id: SessionId,
        message_id: ProducerMessageId,
    ) -> Result<(), PluginError> {
        let _: ProducerDiscardResult = self
            .request(
                PLUGIN_PRODUCER_DISCARD_METHOD,
                &ProducerDiscardParams {
                    session_id,
                    message_id,
                },
            )
            .await?;
        Ok(())
    }

    async fn request<P, R>(&self, method: &str, params: &P) -> Result<R, PluginError>
    where
        P: Serialize + Sync,
        R: serde::de::DeserializeOwned,
    {
        self.state
            .peer
            .request(method, params)
            .await
            .map_err(engine_error)
    }

    fn with_grant(&self, session_id: SessionId, context_id: String, expiry: GrantExpiry) -> Self {
        let now = Instant::now();
        let mut grants = lock(&self.state.grants);
        grants.retain(
            |_, grant| !matches!(grant.expiry, GrantExpiry::At(deadline) if deadline <= now),
        );
        grants.insert(context_id.clone(), Grant { session_id, expiry });
        Self {
            state: Arc::clone(&self.state),
            grant: Some(ScopedGrant {
                session_id,
                context_id,
            }),
        }
    }

    fn consume(&self, session_id: SessionId) -> Result<String, PluginError> {
        let Some(scope) = &self.grant else {
            return Err(PluginError::ContextUnavailable(session_id));
        };
        if scope.session_id != session_id {
            return Err(PluginError::ContextUnavailable(session_id));
        }
        let Some(grant) = lock(&self.state.grants).remove(&scope.context_id) else {
            return Err(PluginError::ContextUnavailable(session_id));
        };
        if grant.session_id != session_id
            || matches!(grant.expiry, GrantExpiry::At(deadline) if deadline <= Instant::now())
        {
            return Err(PluginError::ContextUnavailable(session_id));
        }
        Ok(scope.context_id.clone())
    }

    fn revoke(&self, context_id: &str) {
        lock(&self.state.grants).remove(context_id);
    }
}

/// Context passed to a tool handler: the call, its progress and detach
/// point, its cancellation, and the plugin's producers.
#[derive(Clone)]
pub struct ToolContext {
    session_id: SessionId,
    run_id: RunId,
    tool_call_id: ToolCallId,
    background: bool,
    prepared_id: u64,
    cancellation: CancellationToken,
    plugin: PluginContext,
}

impl ToolContext {
    /// The session the call runs in.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// The run the call belongs to.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// The model's call ID.
    #[must_use]
    pub const fn tool_call_id(&self) -> ToolCallId {
        self.tool_call_id
    }

    /// The model asked to run this call in the background.
    #[must_use]
    pub const fn background(&self) -> bool {
        self.background
    }

    /// Whether the call is cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// Completes once the call is cancelled.
    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    /// Reports progress: a status `message` and, optionally, output text the
    /// session shows while the call runs.
    pub async fn progress(
        &self,
        message: impl Into<String>,
        output: Option<String>,
    ) -> Result<(), PluginError> {
        let outcome: Outcome = self
            .plugin
            .request(
                method::TOOL_PROGRESS,
                &ToolProgressParams {
                    prepared_id: self.prepared_id,
                    progress: WireProgress {
                        tool_call_id: self.tool_call_id,
                        message: message.into(),
                        display: None,
                        output: output
                            .map(|text| vec![ToolOutputChunk { stream: None, text }])
                            .unwrap_or_default(),
                    },
                },
            )
            .await?;
        match outcome.error {
            None => Ok(()),
            Some(error) => Err(PluginError::Protocol(error.message)),
        }
    }

    /// Tells the daemon this call may now run in the background; `note` is a
    /// line for the result the model gets then. A tool that declares
    /// `background` support calls this once its work is under way.
    pub async fn detachable(&self, note: Option<String>) -> Result<(), PluginError> {
        self.detach(note, false).await
    }

    /// Like [`Self::detachable`], for work that reports its own results
    /// through the plugin's producers once detached: the daemon then stops
    /// waiting for the call rather than reporting its result.
    pub async fn detachable_reporting_itself(
        &self,
        note: Option<String>,
    ) -> Result<(), PluginError> {
        self.detach(note, true).await
    }

    async fn detach(&self, note: Option<String>, reports_itself: bool) -> Result<(), PluginError> {
        self.plugin
            .state
            .peer
            .notify(
                method::TOOL_DETACHABLE,
                &DetachableParams {
                    prepared_id: self.prepared_id,
                    note,
                    reports_itself,
                },
            )
            .await
            .map_err(|_| PluginError::TransportClosed)
    }

    /// The plugin's context, for its producers.
    #[must_use]
    pub const fn plugin(&self) -> &PluginContext {
        &self.plugin
    }
}

/// A live producer registration bound to one session and plugin connection.
///
/// This handle intentionally performs no cleanup on drop. Explicit unregistration makes producer
/// lifecycle completion observable and avoids implying that asynchronous cleanup was delivered.
pub struct ProducerHandle {
    context: PluginContext,
    session_id: SessionId,
    producer_id: ProducerId,
}

impl ProducerHandle {
    /// Returns the runtime-only producer registration ID.
    #[must_use]
    pub const fn id(&self) -> ProducerId {
        self.producer_id
    }

    /// Returns the destination session fixed at registration.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Durably sends a producer message using the selected per-send delivery mode.
    pub async fn send(
        &self,
        message: impl Into<String>,
        description: impl Into<String>,
        mode: ProducerDeliveryMode,
        key: ProducerIdempotencyKey,
    ) -> Result<ProducerMessageId, PluginError> {
        let description = SafeDisplayText::new(description)
            .map_err(|error| PluginError::Protocol(format!("invalid description: {error}")))?;
        if description.as_str().trim().is_empty() {
            return Err(PluginError::Protocol(
                "description must not be blank".into(),
            ));
        }
        let result: ProducerSendResult = self
            .context
            .request(
                PLUGIN_PRODUCER_SEND_METHOD,
                &ProducerSendParams {
                    session_id: self.session_id,
                    producer_id: self.producer_id,
                    mode,
                    idempotency_key: key,
                    description,
                    body: message.into(),
                },
            )
            .await?;
        Ok(result.message_id)
    }

    /// Durably sends a message to the next safe model request.
    pub async fn steer(
        &self,
        message: impl Into<String>,
        description: impl Into<String>,
        key: ProducerIdempotencyKey,
    ) -> Result<ProducerMessageId, PluginError> {
        self.send(message, description, ProducerDeliveryMode::Steer, key)
            .await
    }

    /// Durably sends a message for a subsequent normal run.
    pub async fn queue(
        &self,
        message: impl Into<String>,
        description: impl Into<String>,
        key: ProducerIdempotencyKey,
    ) -> Result<ProducerMessageId, PluginError> {
        self.send(message, description, ProducerDeliveryMode::Queue, key)
            .await
    }

    /// Discards a waiting message of this plugin in this handle's session; see
    /// [`PluginContext::discard_producer_message`].
    pub async fn discard(&self, message_id: ProducerMessageId) -> Result<(), PluginError> {
        self.context
            .discard_producer_message(self.session_id, message_id)
            .await
    }

    /// Explicitly unregisters this producer. Zero-send unregistration is valid.
    ///
    /// Failures leave the handle available for retry. After success, the engine remains
    /// authoritative and rejects further sends on the closed registration. Discard
    /// operates on message receipts and remains available after unregister.
    pub async fn unregister(&self) -> Result<(), PluginError> {
        let _: ProducerUnregisterResult = self
            .context
            .request(
                PLUGIN_PRODUCER_UNREGISTER_METHOD,
                &ProducerUnregisterParams {
                    session_id: self.session_id,
                    producer_id: self.producer_id,
                },
            )
            .await?;
        Ok(())
    }
}

type EmitReply = oneshot::Sender<ExtensionEmitResultParams>;

struct ContextState {
    peer: Peer,
    grants: Mutex<HashMap<String, Grant>>,
    pending_emits: Mutex<HashMap<String, VecDeque<EmitReply>>>,
    /// The plugin declared bus publishing.
    publish_bus: AtomicBool,
    /// Producer messaging is open: the plugin declared it and the engine
    /// accepts it.
    producers: AtomicBool,
}

struct Grant {
    session_id: SessionId,
    expiry: GrantExpiry,
}

#[derive(Clone)]
struct ScopedGrant {
    session_id: SessionId,
    context_id: String,
}

enum GrantExpiry {
    At(Instant),
    Request,
}

/// A prepared call, kept until it is executed or released.
struct Prepared {
    tool: RegisteredTool,
    tool_call_id: ToolCallId,
    arguments: Value,
}

struct Server {
    name: String,
    version: String,
    handlers: Arc<Handlers>,
    context: Arc<ContextState>,
    prepared: Mutex<HashMap<u64, Prepared>>,
    /// Executing calls by prepared ID.
    executions: Mutex<HashMap<u64, CancellationToken>>,
    next_prepared: AtomicU64,
    stop: CancellationToken,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Server {
    fn context(&self) -> PluginContext {
        PluginContext {
            state: Arc::clone(&self.context),
            grant: None,
        }
    }

    /// Registers the execution an execute request starts.
    fn admit_execution(
        &self,
        request: &IncomingRequest,
    ) -> Option<(ExecuteParams, CancellationToken)> {
        if request.method != method::TOOL_EXECUTE {
            return None;
        }
        let params: ExecuteParams = request.params().ok()?;
        let cancel = self.stop.child_token();
        lock(&self.executions).insert(params.prepared_id, cancel.clone());
        Some((params, cancel))
    }

    async fn handle_request(
        self: Arc<Self>,
        request: IncomingRequest,
        execution: Option<(ExecuteParams, CancellationToken)>,
    ) {
        let result = match request.method.as_str() {
            PLUGIN_INITIALIZE_METHOD => self.initialize(&request).map(to_value),
            method::TOOL_PREPARE => self.prepare(&request).map(to_value),
            method::TOOL_REVALIDATE => request
                .params::<PreparedRef>()
                .map(|_| to_value(Outcome::default())),
            method::TOOL_EXECUTE => match execution {
                Some((params, cancel)) => {
                    let prepared_id = params.prepared_id;
                    let result = self.execute(params, cancel).await;
                    lock(&self.executions).remove(&prepared_id);
                    Ok(to_value(result))
                }
                None => Err(request
                    .params::<ExecuteParams>()
                    .err()
                    .unwrap_or_else(|| internal("the execution was not admitted"))),
            },
            other => match interception_hook(other) {
                Some(hook) => self.intercept(hook, &request).await,
                None => Err(errors::UNKNOWN_METHOD.error().field("method", other).into()),
            },
        };
        let _ = match result {
            Ok(value) => request.respond(&value).await,
            Err(error) => request.fail(error).await,
        };
    }

    fn initialize(&self, request: &IncomingRequest) -> Result<ExtensionInitializeResult, RpcError> {
        let params: ExtensionInitializeParams = request.params()?;
        let capabilities = self.handlers.capabilities();
        self.context
            .publish_bus
            .store(capabilities.publish_bus, Ordering::Release);
        self.context.producers.store(
            capabilities.producer_messaging && params.capabilities.producer_messaging,
            Ordering::Release,
        );
        Ok(ExtensionInitializeResult {
            protocol_version: ExtensionProtocolVersion::current(),
            name: self.name.clone(),
            version: self.version.clone(),
            capabilities,
            tools: self.handlers.tools(),
        })
    }

    fn prepare(&self, request: &IncomingRequest) -> Result<PrepareResult, RpcError> {
        let params: PrepareParams = request.params()?;
        let results = params
            .calls
            .into_iter()
            .map(|call| self.prepare_call(call))
            .collect();
        Ok(PrepareResult { results })
    }

    fn prepare_call(&self, call: WireToolCall) -> PrepareOutcome {
        let failed = |message: String, primary: Option<&str>| PrepareOutcome::Failed {
            error: WireToolError {
                kind: ToolErrorKind::Failed,
                message,
            },
            presentation: presentation(&call.name, primary),
        };
        let Some(tool) = self
            .handlers
            .tools
            .iter()
            .find(|tool| tool.declaration.name == call.name)
            .cloned()
        else {
            return failed(format!("unknown tool `{}`", call.name), None);
        };
        let resource = tool
            .resource
            .as_ref()
            .and_then(|resource| resource(&call.arguments));
        let operation = match operation_identity(
            &self.name,
            &tool.declaration,
            &call.arguments,
            resource.as_deref(),
        ) {
            Ok(operation) => operation,
            Err(message) => return failed(message, resource.as_deref()),
        };
        let prepared_id = self.next_prepared.fetch_add(1, Ordering::Relaxed);
        let permission = tool.declaration.permission.clone();
        let presentation = presentation(&call.name, resource.as_deref());
        lock(&self.prepared).insert(
            prepared_id,
            Prepared {
                tool,
                tool_call_id: call.id,
                arguments: call.arguments.clone(),
            },
        );
        PrepareOutcome::Prepared(PreparedCall {
            prepared_id,
            operation,
            policy_labels: vec![resource.clone()],
            normalized_arguments: call.arguments,
            permission_resource: permission.as_ref().and(resource),
            permission,
            presentation,
            serialization_key: None,
        })
    }

    async fn execute(&self, params: ExecuteParams, cancel: CancellationToken) -> ExecuteResult {
        let failed = |message: String| ExecuteResult {
            outcome: ExecuteOutcome::Failed {
                error: WireToolError {
                    kind: ToolErrorKind::Failed,
                    message,
                },
            },
            media: Vec::new(),
        };
        let Some(prepared) = lock(&self.prepared).remove(&params.prepared_id) else {
            return failed(format!("unknown prepared call {}", params.prepared_id));
        };
        let context = ToolContext {
            session_id: params.session,
            run_id: params.run,
            tool_call_id: prepared.tool_call_id,
            background: params.background,
            prepared_id: params.prepared_id,
            cancellation: cancel,
            plugin: self.context(),
        };
        let handler = Arc::clone(&prepared.tool.handler);
        match tokio::spawn(handler(context, prepared.arguments)).await {
            Ok(Ok(output)) => ExecuteResult {
                outcome: ExecuteOutcome::Completed {
                    result: PersistedToolResult {
                        title: safe_title(&prepared.tool.declaration.name),
                        output: String::new(),
                        display: cookie_agent_protocol::fit_result_display(
                            cookie_agent_protocol::text_display(output.text.clone()),
                        ),
                        background: None,
                        retained_output: None,
                        metadata: Value::Null,
                        attachments: Vec::new(),
                        additional_messages: Vec::new(),
                    },
                    output: ToolCompletionOutput::Single { text: output.text },
                    failed: output.failed,
                },
                media: Vec::new(),
            },
            Ok(Err(failure)) if failure.message.trim().is_empty() => failed(format!(
                "plugin tool `{}` failed without saying why",
                prepared.tool.declaration.name
            )),
            Ok(Err(failure)) => failed(failure.message),
            Err(_) => failed("plugin tool handler panicked".into()),
        }
    }

    async fn intercept(
        &self,
        hook: ExtensionInterceptionHook,
        request: &IncomingRequest,
    ) -> Result<Value, RpcError> {
        let Some(handler) = self.handlers.intercepts.get(&hook).cloned() else {
            return Err(errors::UNKNOWN_METHOD
                .error()
                .field("method", request.method.as_str())
                .detail("the plugin does not intercept this hook")
                .into());
        };
        let params: Value = request.params()?;
        let prepared = handler(params)
            .map_err(|error| errors::BAD_PARAMS.error().detail(error.to_string()))?;
        let context = self.context().with_grant(
            prepared.session_id,
            prepared.context_id.clone(),
            GrantExpiry::Request,
        );
        let joined = tokio::spawn((prepared.call)(context.clone())).await;
        context.revoke(&prepared.context_id);
        joined.map_err(|_| internal("plugin handler panicked"))
    }

    fn handle_notification(&self, notification: &IncomingNotification) {
        match notification.method.as_str() {
            PLUGIN_EVENT_METHOD => {
                let (Some(handler), Ok(params)) = (
                    self.handlers.event.clone(),
                    notification.params::<ExtensionEventParams>(),
                ) else {
                    return;
                };
                let context = self.context().with_grant(
                    params.session_id,
                    params.context_id.clone(),
                    GrantExpiry::At(Instant::now() + NOTIFICATION_CONTEXT_LIFETIME),
                );
                tokio::spawn(handler(context, params));
            }
            PLUGIN_BUS_EVENT_METHOD => {
                let (Some(handler), Ok(params)) = (
                    self.handlers.bus.clone(),
                    notification.params::<ExtensionBusEventParams>(),
                ) else {
                    return;
                };
                let context = match &params.context_id {
                    Some(context_id) => self.context().with_grant(
                        params.session_id,
                        context_id.clone(),
                        GrantExpiry::At(Instant::now() + NOTIFICATION_CONTEXT_LIFETIME),
                    ),
                    None => self.context(),
                };
                tokio::spawn(handler(context, params));
            }
            PLUGIN_EMIT_RESULT_METHOD => {
                if let Ok(result) = notification.params::<ExtensionEmitResultParams>() {
                    let reply = lock(&self.context.pending_emits)
                        .get_mut(&result.name)
                        .and_then(VecDeque::pop_front);
                    if let Some(reply) = reply {
                        let _ = reply.send(result);
                    }
                }
            }
            PLUGIN_RECOVERY_START_METHOD => {
                if !self.context.producers.load(Ordering::Acquire) {
                    return;
                }
                let recovery = self.handlers.recovery.clone();
                let context = self.context();
                tokio::spawn(async move {
                    let result = match recovery {
                        Some(recovery) => recovery(context.clone()).await,
                        None => Ok(()),
                    };
                    let outcome = match result {
                        Ok(()) => RecoveryOutcome::Ready,
                        Err(failure) => RecoveryOutcome::Failed {
                            message: failure.message,
                        },
                    };
                    let _: Result<RecoveryCompleteResult, _> = context
                        .request(
                            PLUGIN_RECOVERY_COMPLETE_METHOD,
                            &RecoveryCompleteParams { outcome },
                        )
                        .await;
                });
            }
            method::TOOL_RELEASE => {
                if let Ok(PreparedRef { prepared_id }) = notification.params() {
                    lock(&self.prepared).remove(&prepared_id);
                }
            }
            method::TOOL_CANCEL => {
                if let Ok(PreparedRef { prepared_id }) = notification.params()
                    && let Some(cancel) = lock(&self.executions).get(&prepared_id)
                {
                    cancel.cancel();
                }
            }
            PLUGIN_SHUTDOWN_METHOD => self.stop.cancel(),
            // Notifications this plugin does not know are ignored.
            _ => {}
        }
    }
}

/// The approval identity of a call: its arguments, the tool's permission
/// action (or one named after the tool when it needs none), and its
/// permission resource, or the tool's name when it has none.
fn operation_identity(
    plugin: &str,
    declaration: &ToolDecl,
    arguments: &Value,
    resource: Option<&str>,
) -> Result<PreparedOperationIdentity, String> {
    let action = declaration
        .permission
        .clone()
        .or_else(|| PermissionAction::new(declaration.name.as_str()).ok())
        .unwrap_or_else(|| PermissionAction::new("plugin_tool").expect("static action is valid"));
    let operation = PreparedCapabilityOperation::new(format!("{}:call", declaration.name))
        .map_err(|error| error.to_string())?;
    let label = resource.unwrap_or(declaration.name.as_str());
    let mut binding = Vec::from(plugin.as_bytes());
    binding.push(0);
    binding.extend_from_slice(label.as_bytes());
    let arguments = serde_json::to_vec(arguments).map_err(|error| error.to_string())?;
    PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(&arguments),
        vec![ApprovalCapability {
            action: action.clone(),
            operation,
        }],
        vec![PreparedApprovalResource {
            capability: action,
            canonical: PreparedResourceIdentity::new(format!(
                "plugin-tool:{}",
                Sha256Digest::of_bytes(&binding).as_str()
            ))
            .map_err(|error| error.to_string())?,
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(&binding),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: ApprovalBoundary::Exact,
            source: ApprovalResourceSource::PrimaryOperation,
        }],
        Sha256Digest::of_bytes(format!("plugin:{plugin}").as_bytes()),
    )
    .map_err(|error| error.to_string())
}

fn safe_title(name: &str) -> SafeDisplayText {
    SafeDisplayText::new(name.replace(char::is_control, " "))
        .unwrap_or_else(|_| SafeDisplayText::new("tool").expect("static title is valid"))
}

fn presentation(name: &str, primary: Option<&str>) -> ToolCallPresentation {
    ToolCallPresentation {
        title: safe_title(name),
        primary_argument: primary
            .map(|primary| truncate(primary.trim(), BoundedDisplayText::MAX_BYTES))
            .filter(|primary| !primary.is_empty())
            .and_then(|primary| BoundedDisplayText::new(primary).ok()),
    }
}

fn truncate(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// An `internal_error` whose detail says what failed. Every error the SDK
/// sends carries a registry code and a nonempty message
/// (`docs/site/specs/compatibility-v1.md` section 6).
fn internal(detail: &str) -> RpcError {
    errors::INTERNAL.error().detail(detail).into()
}

fn to_value(value: impl Serialize) -> Value {
    serde_json::to_value(value).expect("protocol results serialize")
}

fn engine_error(error: RpcError) -> PluginError {
    match error {
        RpcError::Closed(_) => PluginError::TransportClosed,
        RpcError::Remote {
            code,
            message,
            data,
        } => PluginError::EngineRequest {
            code,
            message,
            data,
        },
        other => PluginError::Protocol(other.to_string()),
    }
}

/// Typed parameters of one interception hook.
trait InterceptParams: serde::de::DeserializeOwned + Send + 'static {
    const HOOK: ExtensionInterceptionHook;
    type Result: Serialize + Send + 'static;

    fn session_id(&self) -> SessionId;
    fn context_id(&self) -> &str;
}

/// Declares every interception hook once: its builder method, wire method, params, and result.
/// Rows are in capability-advertisement order.
macro_rules! interception_hooks {
    ($(
        $(#[doc = $doc:literal])+
        $builder:ident: $hook:ident, $method:ident, $params:ty => $result:ty;
    )+) => {
        const INTERCEPTION_HOOKS: &[ExtensionInterceptionHook] =
            &[$(ExtensionInterceptionHook::$hook),+];

        fn interception_hook(method: &str) -> Option<ExtensionInterceptionHook> {
            match method {
                $($method => Some(ExtensionInterceptionHook::$hook),)+
                _ => None,
            }
        }

        $(impl InterceptParams for $params {
            const HOOK: ExtensionInterceptionHook = ExtensionInterceptionHook::$hook;
            type Result = $result;

            fn session_id(&self) -> SessionId {
                self.session_id
            }

            fn context_id(&self) -> &str {
                &self.context_id
            }
        })+

        impl PluginServerBuilder {
            $(
                $(#[doc = $doc])+
                #[must_use]
                pub fn $builder<F, Fut, R>(self, handler: F) -> Self
                where
                    F: Fn(PluginContext, $params) -> Fut + Send + Sync + 'static,
                    Fut: Future<Output = R> + Send + 'static,
                    R: Into<$result> + Send + 'static,
                {
                    self.intercept(handler)
                }
            )+
        }
    };
}

interception_hooks! {
    /// Registers the `tool_before_call` interception hook.
    tool_before_call: ToolBeforeCall, PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD,
        ExtensionToolBeforeCallParams => ExtensionToolBeforeCallResult;
    /// Registers the `tool_after_result` interception hook.
    tool_after_result: ToolAfterResult, PLUGIN_INTERCEPT_TOOL_AFTER_RESULT_METHOD,
        ExtensionToolAfterResultParams => ExtensionToolAfterResultResult;
    /// Registers the `agent_before_start` interception hook.
    agent_before_start: AgentBeforeStart, PLUGIN_INTERCEPT_AGENT_BEFORE_START_METHOD,
        ExtensionAgentBeforeStartParams => ExtensionAgentBeforeStartResult;
    /// Registers the `session_before_compact` interception hook.
    session_before_compact: SessionBeforeCompact, PLUGIN_INTERCEPT_SESSION_BEFORE_COMPACT_METHOD,
        ExtensionSessionBeforeCompactParams => ExtensionSessionBeforeCompactResult;
    /// Registers the `user_before_input` interception hook.
    user_before_input: UserBeforeInput, PLUGIN_INTERCEPT_USER_BEFORE_INPUT_METHOD,
        ExtensionUserBeforeInputParams => ExtensionUserBeforeInputResult;
    /// Registers the `model_before_request` interception hook.
    model_before_request: ModelBeforeRequest, PLUGIN_INTERCEPT_MODEL_BEFORE_REQUEST_METHOD,
        ExtensionModelBeforeRequestParams => ExtensionModelBeforeRequestResult;
    /// Registers the `provider_before_request` interception hook.
    provider_before_request: ProviderBeforeRequest, PLUGIN_INTERCEPT_PROVIDER_BEFORE_REQUEST_METHOD,
        ExtensionProviderBeforeRequestParams => ExtensionProviderBeforeRequestResult;
    /// Registers the observe-only `provider_after_response` interception hook.
    provider_after_response: ProviderAfterResponse, PLUGIN_INTERCEPT_PROVIDER_AFTER_RESPONSE_METHOD,
        ExtensionProviderAfterResponseParams => ExtensionProviderAfterResponseResult;
    /// Registers the `message_end` interception hook.
    message_end: MessageEnd, PLUGIN_INTERCEPT_MESSAGE_END_METHOD,
        ExtensionMessageEndParams => ExtensionMessageEndResult;
    /// Registers the `model_before_select` interception hook.
    model_before_select: ModelBeforeSelect, PLUGIN_INTERCEPT_MODEL_BEFORE_SELECT_METHOD,
        ExtensionModelBeforeSelectParams => ExtensionAllowBlockResult;
    /// Registers the `session_before_fork` interception hook.
    session_before_fork: SessionBeforeFork, PLUGIN_INTERCEPT_SESSION_BEFORE_FORK_METHOD,
        ExtensionSessionBeforeForkParams => ExtensionAllowBlockResult;
    /// Registers the `session_before_revert` interception hook.
    session_before_revert: SessionBeforeRevert, PLUGIN_INTERCEPT_SESSION_BEFORE_REVERT_METHOD,
        ExtensionSessionBeforeRevertParams => ExtensionSessionBeforeRevertResult;
}

fn validate_tool(tool: &ToolDecl, duplicate: bool) -> Result<(), PluginError> {
    if duplicate {
        return Err(PluginError::InvalidTool(format!(
            "duplicate tool name `{}`",
            tool.name
        )));
    }
    if tool.name.is_empty()
        || tool.name.len() > 64
        || !tool
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(PluginError::InvalidTool(format!(
            "tool name `{}` must be 1 to 64 ASCII letters, digits, `_`, or `-`",
            tool.name
        )));
    }
    if tool.description.trim().is_empty() {
        return Err(PluginError::InvalidTool(format!(
            "tool `{}` description must not be empty",
            tool.name
        )));
    }
    if tool.background_default && !tool.background {
        return Err(PluginError::InvalidTool(format!(
            "tool `{}` declares `background_default` without `background`",
            tool.name
        )));
    }
    if !tool.parameters.is_object() {
        return Err(PluginError::InvalidTool(format!(
            "tool `{}` parameters must be a JSON Schema object",
            tool.name
        )));
    }
    jsonschema::draft202012::meta::validate(&tool.parameters).map_err(|error| {
        PluginError::InvalidTool(format!(
            "tool `{}` has invalid JSON Schema: {error}",
            tool.name
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests;
