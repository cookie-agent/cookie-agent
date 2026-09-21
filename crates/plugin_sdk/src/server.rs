use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::Duration,
};

use cookie_agent_protocol::{
    ErrorResponse, ExtensionAgentBeforeStartParams, ExtensionAgentBeforeStartResult,
    ExtensionAllowBlockResult, ExtensionBusEventParams, ExtensionEmitParams,
    ExtensionEmitResultParams, ExtensionEmitStatus, ExtensionEventParams,
    ExtensionInitializeParams, ExtensionInitializeResult, ExtensionInterceptionHook,
    ExtensionMessageEndParams, ExtensionMessageEndResult, ExtensionModelBeforeRequestParams,
    ExtensionModelBeforeRequestResult, ExtensionModelBeforeSelectParams, ExtensionPingParams,
    ExtensionPingResult, ExtensionPluginCapabilities, ExtensionProducerDiscardParams,
    ExtensionProducerDiscardResult, ExtensionProducerRegisterParams,
    ExtensionProducerRegisterResult, ExtensionProducerSendParams, ExtensionProducerSendResult,
    ExtensionProducerUnregisterParams, ExtensionProducerUnregisterResult, ExtensionProtocolVersion,
    ExtensionProviderAfterResponseParams, ExtensionProviderAfterResponseResult,
    ExtensionProviderBeforeHeadersParams, ExtensionProviderBeforeHeadersResult,
    ExtensionProviderBeforeRequestParams, ExtensionProviderBeforeRequestResult,
    ExtensionRecoveryCompleteParams, ExtensionRecoveryCompleteResult, ExtensionRecoveryOutcome,
    ExtensionRecoveryStartParams, ExtensionSessionBeforeCompactParams,
    ExtensionSessionBeforeCompactResult, ExtensionSessionBeforeForkParams,
    ExtensionSessionBeforeRevertParams, ExtensionSessionBeforeRevertResult,
    ExtensionShutdownParams, ExtensionToolAfterResultParams, ExtensionToolAfterResultResult,
    ExtensionToolBeforeCallParams, ExtensionToolBeforeCallResult, ExtensionToolCallParams,
    ExtensionToolCallResult, ExtensionUserBeforeInputParams, ExtensionUserBeforeInputResult,
    JsonRpcError, JsonRpcId, JsonRpcVersion, Notification, PLUGIN_BUS_EVENT_METHOD,
    PLUGIN_EMIT_METHOD, PLUGIN_EMIT_RESULT_METHOD, PLUGIN_EVENT_METHOD, PLUGIN_INITIALIZE_METHOD,
    PLUGIN_INTERCEPT_AGENT_BEFORE_START_METHOD, PLUGIN_INTERCEPT_MESSAGE_END_METHOD,
    PLUGIN_INTERCEPT_MODEL_BEFORE_REQUEST_METHOD, PLUGIN_INTERCEPT_MODEL_BEFORE_SELECT_METHOD,
    PLUGIN_INTERCEPT_PROVIDER_AFTER_RESPONSE_METHOD,
    PLUGIN_INTERCEPT_PROVIDER_BEFORE_HEADERS_METHOD,
    PLUGIN_INTERCEPT_PROVIDER_BEFORE_REQUEST_METHOD,
    PLUGIN_INTERCEPT_SESSION_BEFORE_COMPACT_METHOD, PLUGIN_INTERCEPT_SESSION_BEFORE_FORK_METHOD,
    PLUGIN_INTERCEPT_SESSION_BEFORE_REVERT_METHOD, PLUGIN_INTERCEPT_TOOL_AFTER_RESULT_METHOD,
    PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD, PLUGIN_INTERCEPT_USER_BEFORE_INPUT_METHOD,
    PLUGIN_PING_METHOD, PLUGIN_PRODUCER_DISCARD_METHOD, PLUGIN_PRODUCER_REGISTER_METHOD,
    PLUGIN_PRODUCER_SEND_METHOD, PLUGIN_PRODUCER_UNREGISTER_METHOD,
    PLUGIN_RECOVERY_COMPLETE_METHOD, PLUGIN_RECOVERY_START_METHOD, PLUGIN_SHUTDOWN_METHOD,
    PLUGIN_TOOLS_CALL_METHOD, ProducerDeliveryMode, ProducerId, ProducerIdempotencyKey,
    ProducerMessageId, Request, Response, SafeErrorMessage, SessionId, SuccessResponse,
};
use serde::Serialize;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

use crate::{
    PluginError, ToolDecl, ToolFailure, ToolOutput,
    framing::{MAX_FRAME_BYTES, read_frame},
};

// The engine grants five seconds from wire delivery. Keep a one-second client-side safety margin.
const NOTIFICATION_CONTEXT_LIFETIME: Duration = Duration::from_secs(4);
const INTERNAL_ERROR: i32 = -32603;
const INVALID_PARAMS: i32 = -32602;
const METHOD_NOT_FOUND: i32 = -32601;
const SERVER_BUSY: i32 = -32000;
const MAX_CONCURRENT_HANDLERS: usize = 64;
const MAX_PENDING_REQUESTS: usize = 128;

type HandlerFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
type ToolHandler = Arc<
    dyn Fn(PluginContext, ExtensionToolCallParams) -> HandlerFuture<Result<ToolOutput, ToolFailure>>
        + Send
        + Sync,
>;
type ToolBeforeHandler = Arc<
    dyn Fn(
            PluginContext,
            ExtensionToolBeforeCallParams,
        ) -> HandlerFuture<ExtensionToolBeforeCallResult>
        + Send
        + Sync,
>;
type ToolAfterHandler = Arc<
    dyn Fn(
            PluginContext,
            ExtensionToolAfterResultParams,
        ) -> HandlerFuture<ExtensionToolAfterResultResult>
        + Send
        + Sync,
>;
type AgentBeforeHandler = Arc<
    dyn Fn(
            PluginContext,
            ExtensionAgentBeforeStartParams,
        ) -> HandlerFuture<ExtensionAgentBeforeStartResult>
        + Send
        + Sync,
>;
type CompactHandler = Arc<
    dyn Fn(
            PluginContext,
            ExtensionSessionBeforeCompactParams,
        ) -> HandlerFuture<ExtensionSessionBeforeCompactResult>
        + Send
        + Sync,
>;
type UserBeforeInputHandler = Arc<
    dyn Fn(
            PluginContext,
            ExtensionUserBeforeInputParams,
        ) -> HandlerFuture<ExtensionUserBeforeInputResult>
        + Send
        + Sync,
>;
type ModelBeforeRequestHandler = Arc<
    dyn Fn(
            PluginContext,
            ExtensionModelBeforeRequestParams,
        ) -> HandlerFuture<ExtensionModelBeforeRequestResult>
        + Send
        + Sync,
>;
type ProviderBeforeHeadersHandler = Arc<
    dyn Fn(
            PluginContext,
            ExtensionProviderBeforeHeadersParams,
        ) -> HandlerFuture<ExtensionProviderBeforeHeadersResult>
        + Send
        + Sync,
>;
type ProviderBeforeRequestHandler = Arc<
    dyn Fn(
            PluginContext,
            ExtensionProviderBeforeRequestParams,
        ) -> HandlerFuture<ExtensionProviderBeforeRequestResult>
        + Send
        + Sync,
>;
type ProviderAfterResponseHandler = Arc<
    dyn Fn(
            PluginContext,
            ExtensionProviderAfterResponseParams,
        ) -> HandlerFuture<ExtensionProviderAfterResponseResult>
        + Send
        + Sync,
>;
type MessageEndHandler = Arc<
    dyn Fn(PluginContext, ExtensionMessageEndParams) -> HandlerFuture<ExtensionMessageEndResult>
        + Send
        + Sync,
>;
type ModelBeforeSelectHandler = Arc<
    dyn Fn(
            PluginContext,
            ExtensionModelBeforeSelectParams,
        ) -> HandlerFuture<ExtensionAllowBlockResult>
        + Send
        + Sync,
>;
type SessionBeforeForkHandler = Arc<
    dyn Fn(
            PluginContext,
            ExtensionSessionBeforeForkParams,
        ) -> HandlerFuture<ExtensionAllowBlockResult>
        + Send
        + Sync,
>;
type SessionBeforeRevertHandler = Arc<
    dyn Fn(
            PluginContext,
            ExtensionSessionBeforeRevertParams,
        ) -> HandlerFuture<ExtensionSessionBeforeRevertResult>
        + Send
        + Sync,
>;
type EventHandler =
    Arc<dyn Fn(PluginContext, ExtensionEventParams) -> HandlerFuture<()> + Send + Sync>;
type BusHandler =
    Arc<dyn Fn(PluginContext, ExtensionBusEventParams) -> HandlerFuture<()> + Send + Sync>;
type RecoveryHandler = Arc<dyn Fn(PluginContext) -> HandlerFuture<RecoveryResult> + Send + Sync>;

#[derive(Clone)]
struct RegisteredTool {
    declaration: ToolDecl,
    handler: ToolHandler,
}

#[derive(Clone, Default)]
struct Handlers {
    tools: Vec<RegisteredTool>,
    tool_before: Option<ToolBeforeHandler>,
    tool_after: Option<ToolAfterHandler>,
    agent_before: Option<AgentBeforeHandler>,
    compact: Option<CompactHandler>,
    user_before_input: Option<UserBeforeInputHandler>,
    model_before_request: Option<ModelBeforeRequestHandler>,
    provider_before_headers: Option<ProviderBeforeHeadersHandler>,
    provider_before_request: Option<ProviderBeforeRequestHandler>,
    provider_after_response: Option<ProviderAfterResponseHandler>,
    message_end: Option<MessageEndHandler>,
    model_before_select: Option<ModelBeforeSelectHandler>,
    session_before_fork: Option<SessionBeforeForkHandler>,
    session_before_revert: Option<SessionBeforeRevertHandler>,
    event: Option<EventHandler>,
    bus: Option<BusHandler>,
    recovery: Option<RecoveryHandler>,
    publish_bus: bool,
    producers: bool,
}

impl Handlers {
    fn capabilities(&self) -> ExtensionPluginCapabilities {
        let mut intercept = Vec::new();
        if self.tool_before.is_some() {
            intercept.push(ExtensionInterceptionHook::ToolBeforeCall);
        }
        if self.tool_after.is_some() {
            intercept.push(ExtensionInterceptionHook::ToolAfterResult);
        }
        if self.agent_before.is_some() {
            intercept.push(ExtensionInterceptionHook::AgentBeforeStart);
        }
        if self.compact.is_some() {
            intercept.push(ExtensionInterceptionHook::SessionBeforeCompact);
        }
        if self.user_before_input.is_some() {
            intercept.push(ExtensionInterceptionHook::UserBeforeInput);
        }
        if self.model_before_request.is_some() {
            intercept.push(ExtensionInterceptionHook::ModelBeforeRequest);
        }
        if self.provider_before_headers.is_some() {
            intercept.push(ExtensionInterceptionHook::ProviderBeforeHeaders);
        }
        if self.provider_before_request.is_some() {
            intercept.push(ExtensionInterceptionHook::ProviderBeforeRequest);
        }
        if self.provider_after_response.is_some() {
            intercept.push(ExtensionInterceptionHook::ProviderAfterResponse);
        }
        if self.message_end.is_some() {
            intercept.push(ExtensionInterceptionHook::MessageEnd);
        }
        if self.model_before_select.is_some() {
            intercept.push(ExtensionInterceptionHook::ModelBeforeSelect);
        }
        if self.session_before_fork.is_some() {
            intercept.push(ExtensionInterceptionHook::SessionBeforeFork);
        }
        if self.session_before_revert.is_some() {
            intercept.push(ExtensionInterceptionHook::SessionBeforeRevert);
        }
        ExtensionPluginCapabilities {
            producer_messaging: self.producers,
            tools: !self.tools.is_empty(),
            resources: false,
            subscribe_events: self.event.is_some(),
            subscribe_bus: self.bus.is_some(),
            publish_bus: self.publish_bus,
            publish_session_events: false,
            intercept,
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
    /// Starts a builder for a plugin with the reported name and version.
    #[must_use]
    pub fn builder(name: impl Into<String>, version: impl Into<String>) -> PluginServerBuilder {
        PluginServerBuilder {
            name: name.into(),
            version: version.into(),
            handlers: Handlers::default(),
            error: None,
        }
    }

    /// Runs this plugin on standard input and output until shutdown or EOF.
    ///
    /// Incoming calls are dispatched concurrently. Ordering across calls and notifications is not
    /// guaranteed; the engine serializes interception chains where ordering is required.
    pub async fn run_stdio(self) -> Result<(), PluginError> {
        self.run_io(tokio::io::stdin(), tokio::io::stdout()).await
    }

    async fn run_io<R, W>(self, reader: R, writer: W) -> Result<(), PluginError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending_emits = Arc::new(Mutex::new(HashMap::new()));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));
        let (outbound, outbound_rx) = mpsc::channel(128);
        let state = Arc::new(ContextState {
            grants: Mutex::new(HashMap::new()),
            pending_emits: Arc::clone(&pending_emits),
            pending_requests: Arc::clone(&pending_requests),
            outbound,
            publishing: PublishingCapabilities {
                bus: self.handlers.publish_bus,
            },
            producers: self.handlers.producers,
            request_ids: AtomicI64::new(1),
            pending_slots: Arc::new(Semaphore::new(MAX_PENDING_REQUESTS)),
            connected: AtomicBool::new(true),
        });
        let context = PluginContext { state, grant: None };
        let mut writer_task = tokio::spawn(writer_loop(
            writer,
            outbound_rx,
            Arc::clone(&pending_emits),
            Arc::clone(&pending_requests),
        ));
        let (inbound_tx, mut inbound_rx) = mpsc::channel(32);
        let reader_task = tokio::spawn(reader_loop(reader, inbound_tx));
        let handler_slots = Arc::new(Semaphore::new(MAX_CONCURRENT_HANDLERS));
        let recovery_running = Arc::new(AtomicBool::new(false));

        let mut dispatch_tasks: Vec<JoinHandle<()>> = Vec::new();
        let (result, writer_finished) = loop {
            tokio::select! {
                inbound = inbound_rx.recv() => match inbound {
                    Some(Ok(frame)) => {
                        dispatch_tasks.retain(|task| !task.is_finished());
                        match self.dispatch_message(
                            &frame.message,
                            frame.received_at,
                            &context,
                            &mut dispatch_tasks,
                            &handler_slots,
                            &recovery_running,
                        ).await {
                            Ok(Dispatch::Continue) => {}
                            Ok(Dispatch::Shutdown) => break (Ok(()), false),
                            Err(error) => break (Err(error), false),
                        }
                    }
                    Some(Err(error)) => break (Err(error), false),
                    None => break (Ok(()), false),
                },
                writer_result = writer_task_finished(&mut writer_task) => {
                    let result = match writer_result {
                        Ok(()) => Err(PluginError::TransportClosed),
                        Err(error) => Err(error),
                    };
                    break (result, true);
                }
            }
        };

        context.state.connected.store(false, Ordering::Release);
        reader_task.abort();
        let _ = reader_task.await;
        if !writer_finished {
            writer_task.abort();
            let _ = writer_task.await;
        }
        fail_pending_emits(&pending_emits);
        fail_pending_requests(&pending_requests);
        for task in dispatch_tasks {
            task.abort();
        }
        result
    }

    async fn dispatch_message(
        &self,
        message: &str,
        received_at: Instant,
        context: &PluginContext,
        tasks: &mut Vec<JoinHandle<()>>,
        handler_slots: &Arc<Semaphore>,
        recovery_running: &Arc<AtomicBool>,
    ) -> Result<Dispatch, PluginError> {
        let value: Value = serde_json::from_str(message)?;
        if value.get("method").is_none() {
            let response: Response = serde_json::from_value(value).map_err(|error| {
                PluginError::Protocol(format!("malformed engine response: {error}"))
            })?;
            resolve_request(&context.state.pending_requests, response)?;
            return Ok(Dispatch::Continue);
        }
        if value.get("id").is_some() {
            let request: Request = serde_json::from_value(value).map_err(|error| {
                PluginError::Protocol(format!("malformed engine request: {error}"))
            })?;
            self.dispatch_request(request, context, tasks, handler_slots)
                .await?;
            return Ok(Dispatch::Continue);
        }

        let notification: Notification = serde_json::from_value(value).map_err(|error| {
            PluginError::Protocol(format!("malformed engine notification: {error}"))
        })?;
        self.dispatch_notification(
            notification,
            received_at,
            context,
            tasks,
            handler_slots,
            recovery_running,
        )
        .await
    }

    async fn dispatch_request(
        &self,
        request: Request,
        context: &PluginContext,
        tasks: &mut Vec<JoinHandle<()>>,
        handler_slots: &Arc<Semaphore>,
    ) -> Result<(), PluginError> {
        match request.method.as_str() {
            PLUGIN_INITIALIZE_METHOD => {
                let _: ExtensionInitializeParams = parse_params(&request)?;
                let result = ExtensionInitializeResult {
                    protocol_version: ExtensionProtocolVersion::current(),
                    name: self.name.clone(),
                    version: self.version.clone(),
                    capabilities: self.handlers.capabilities(),
                    tools: self
                        .handlers
                        .tools
                        .iter()
                        .map(|tool| tool.declaration.clone())
                        .collect(),
                };
                send_success(context, request.id, result).await?;
            }
            PLUGIN_PING_METHOD => {
                let _: ExtensionPingParams = parse_params(&request)?;
                send_success(context, request.id, ExtensionPingResult {}).await?;
            }
            PLUGIN_TOOLS_CALL_METHOD => {
                let Some(permit) = try_handler_slot(context, &request, handler_slots).await? else {
                    return Ok(());
                };
                let params: ExtensionToolCallParams = match parse_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        send_error(context, request.id, INVALID_PARAMS, error.to_string()).await?;
                        return Ok(());
                    }
                };
                let Some(tool) = self
                    .handlers
                    .tools
                    .iter()
                    .find(|tool| tool.declaration.name == params.tool)
                    .cloned()
                else {
                    send_error(
                        context,
                        request.id,
                        METHOD_NOT_FOUND,
                        format!("unknown plugin tool `{}`", params.tool),
                    )
                    .await?;
                    return Ok(());
                };
                let plugin_context =
                    context.register_request(params.session_id, params.context_id.clone());
                tasks.push(tokio::spawn(async move {
                    let _permit = permit;
                    let handler_context = plugin_context.clone();
                    let handler_params = params.clone();
                    let result =
                        isolate(
                            async move { (tool.handler)(handler_context, handler_params).await },
                        )
                        .await;
                    plugin_context.revoke(&params.context_id);
                    match result {
                        Ok(Ok(output)) => {
                            let _ = send_success(
                                &plugin_context,
                                request.id,
                                ExtensionToolCallResult::from(output),
                            )
                            .await;
                        }
                        Ok(Err(error)) => {
                            let _ =
                                send_rpc_error(&plugin_context, request.id, error.into_rpc()).await;
                        }
                        Err(()) => {
                            let _ = send_error(
                                &plugin_context,
                                request.id,
                                INTERNAL_ERROR,
                                "plugin tool handler panicked",
                            )
                            .await;
                        }
                    }
                }));
            }
            PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD => {
                let handler = self.handlers.tool_before.clone();
                dispatch_intercept::<ExtensionToolBeforeCallParams, _, _, _>(
                    request,
                    context,
                    handler,
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            PLUGIN_INTERCEPT_TOOL_AFTER_RESULT_METHOD => {
                let handler = self.handlers.tool_after.clone();
                dispatch_intercept::<ExtensionToolAfterResultParams, _, _, _>(
                    request,
                    context,
                    handler,
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            PLUGIN_INTERCEPT_AGENT_BEFORE_START_METHOD => {
                let handler = self.handlers.agent_before.clone();
                dispatch_intercept::<ExtensionAgentBeforeStartParams, _, _, _>(
                    request,
                    context,
                    handler,
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            PLUGIN_INTERCEPT_SESSION_BEFORE_COMPACT_METHOD => {
                let handler = self.handlers.compact.clone();
                dispatch_intercept::<ExtensionSessionBeforeCompactParams, _, _, _>(
                    request,
                    context,
                    handler,
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            PLUGIN_INTERCEPT_USER_BEFORE_INPUT_METHOD => {
                dispatch_intercept(
                    request,
                    context,
                    self.handlers.user_before_input.clone(),
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            PLUGIN_INTERCEPT_MODEL_BEFORE_REQUEST_METHOD => {
                dispatch_intercept(
                    request,
                    context,
                    self.handlers.model_before_request.clone(),
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            PLUGIN_INTERCEPT_PROVIDER_BEFORE_HEADERS_METHOD => {
                dispatch_intercept(
                    request,
                    context,
                    self.handlers.provider_before_headers.clone(),
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            PLUGIN_INTERCEPT_PROVIDER_BEFORE_REQUEST_METHOD => {
                dispatch_intercept(
                    request,
                    context,
                    self.handlers.provider_before_request.clone(),
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            PLUGIN_INTERCEPT_PROVIDER_AFTER_RESPONSE_METHOD => {
                dispatch_intercept(
                    request,
                    context,
                    self.handlers.provider_after_response.clone(),
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            PLUGIN_INTERCEPT_MESSAGE_END_METHOD => {
                dispatch_intercept(
                    request,
                    context,
                    self.handlers.message_end.clone(),
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            PLUGIN_INTERCEPT_MODEL_BEFORE_SELECT_METHOD => {
                dispatch_intercept(
                    request,
                    context,
                    self.handlers.model_before_select.clone(),
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            PLUGIN_INTERCEPT_SESSION_BEFORE_FORK_METHOD => {
                dispatch_intercept(
                    request,
                    context,
                    self.handlers.session_before_fork.clone(),
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            PLUGIN_INTERCEPT_SESSION_BEFORE_REVERT_METHOD => {
                dispatch_intercept(
                    request,
                    context,
                    self.handlers.session_before_revert.clone(),
                    tasks,
                    handler_slots,
                    |handler, ctx, params| handler(ctx, params),
                    |value| value,
                )
                .await?;
            }
            _ => {
                send_error(
                    context,
                    request.id,
                    METHOD_NOT_FOUND,
                    "plugin method is not supported",
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn dispatch_notification(
        &self,
        notification: Notification,
        received_at: Instant,
        context: &PluginContext,
        tasks: &mut Vec<JoinHandle<()>>,
        handler_slots: &Arc<Semaphore>,
        recovery_running: &Arc<AtomicBool>,
    ) -> Result<Dispatch, PluginError> {
        match notification.method.as_str() {
            PLUGIN_SHUTDOWN_METHOD => {
                let _: ExtensionShutdownParams = parse_notification_params(&notification)?;
                Ok(Dispatch::Shutdown)
            }
            PLUGIN_EVENT_METHOD => {
                let params: ExtensionEventParams = parse_notification_params(&notification)?;
                if let Some(handler) = self.handlers.event.clone() {
                    let Ok(permit) = Arc::clone(handler_slots).try_acquire_owned() else {
                        eprintln!("cookie-agent plugin event handler capacity exhausted");
                        return Ok(Dispatch::Continue);
                    };
                    let plugin_context = context.register_notification(
                        params.session_id,
                        params.context_id.clone(),
                        received_at,
                    );
                    tasks.push(tokio::spawn(async move {
                        let _permit = permit;
                        if isolate(async move { handler(plugin_context, params).await })
                            .await
                            .is_err()
                        {
                            eprintln!("cookie-agent plugin event handler panicked");
                        }
                    }));
                }
                Ok(Dispatch::Continue)
            }
            PLUGIN_BUS_EVENT_METHOD => {
                let params: ExtensionBusEventParams = parse_notification_params(&notification)?;
                if let Some(handler) = self.handlers.bus.clone() {
                    let Ok(permit) = Arc::clone(handler_slots).try_acquire_owned() else {
                        eprintln!("cookie-agent plugin bus handler capacity exhausted");
                        return Ok(Dispatch::Continue);
                    };
                    let plugin_context = params.context_id.as_ref().map_or_else(
                        || context.clone(),
                        |context_id| {
                            context.register_notification(
                                params.session_id,
                                context_id.clone(),
                                received_at,
                            )
                        },
                    );
                    tasks.push(tokio::spawn(async move {
                        let _permit = permit;
                        if isolate(async move { handler(plugin_context, params).await })
                            .await
                            .is_err()
                        {
                            eprintln!("cookie-agent plugin bus handler panicked");
                        }
                    }));
                }
                Ok(Dispatch::Continue)
            }
            PLUGIN_EMIT_RESULT_METHOD => {
                let result: ExtensionEmitResultParams = parse_notification_params(&notification)?;
                resolve_emit(&context.state.pending_emits, result);
                Ok(Dispatch::Continue)
            }
            PLUGIN_RECOVERY_START_METHOD => {
                let _: ExtensionRecoveryStartParams = parse_notification_params(&notification)?;
                let handler = self.handlers.recovery.clone();
                if recovery_running.swap(true, Ordering::AcqRel) {
                    return Err(PluginError::Protocol(
                        "engine started recovery while recovery was already running".into(),
                    ));
                }
                let plugin_context = context.clone();
                let recovery_running = Arc::clone(recovery_running);
                tasks.push(tokio::spawn(async move {
                    let outcome = match handler {
                        Some(handler) => {
                            let handler_context = plugin_context.clone();
                            match isolate(async move { handler(handler_context).await }).await {
                                Ok(Ok(())) => ExtensionRecoveryOutcome::Ready,
                                Ok(Err(error)) => ExtensionRecoveryOutcome::Failed {
                                    message: error.message,
                                },
                                Err(()) => ExtensionRecoveryOutcome::Failed {
                                    message: SafeErrorMessage::new(
                                        "plugin recovery handler panicked",
                                    )
                                    .expect("static recovery message is valid"),
                                },
                            }
                        }
                        None => ExtensionRecoveryOutcome::Ready,
                    };
                    if let Err(error) = context_request::<_, ExtensionRecoveryCompleteResult>(
                        &plugin_context,
                        PLUGIN_RECOVERY_COMPLETE_METHOD,
                        ExtensionRecoveryCompleteParams { outcome },
                    )
                    .await
                    {
                        eprintln!("cookie-agent plugin recovery completion failed: {error}");
                    }
                    recovery_running.store(false, Ordering::Release);
                }));
                Ok(Dispatch::Continue)
            }
            _ => Ok(Dispatch::Continue),
        }
    }
}

/// Builder used to register plugin handlers and derive capabilities.
pub struct PluginServerBuilder {
    name: String,
    version: String,
    handlers: Handlers,
    error: Option<PluginError>,
}

impl PluginServerBuilder {
    /// Registers a declared tool and its asynchronous handler.
    #[must_use]
    pub fn tool<F, Fut>(mut self, declaration: ToolDecl, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionToolCallParams) -> Fut + Send + Sync + 'static,
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
            handler: Arc::new(move |context, request| Box::pin(handler(context, request))),
        });
        self
    }

    /// Registers the `tool_before_call` interception hook.
    #[must_use]
    pub fn tool_before_call<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionToolBeforeCallParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExtensionToolBeforeCallResult> + Send + 'static,
    {
        self.handlers.tool_before = Some(Arc::new(move |context, request| {
            Box::pin(handler(context, request))
        }));
        self
    }

    /// Registers the `tool_after_result` interception hook.
    #[must_use]
    pub fn tool_after_result<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionToolAfterResultParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExtensionToolAfterResultResult> + Send + 'static,
    {
        self.handlers.tool_after = Some(Arc::new(move |context, request| {
            Box::pin(handler(context, request))
        }));
        self
    }

    /// Registers the `agent_before_start` interception hook.
    #[must_use]
    pub fn agent_before_start<F, Fut, R>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionAgentBeforeStartParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: Into<ExtensionAgentBeforeStartResult> + Send + 'static,
    {
        self.handlers.agent_before = Some(Arc::new(move |context, request| {
            let future = handler(context, request);
            Box::pin(async move { future.await.into() })
        }));
        self
    }

    /// Registers the `session_before_compact` interception hook.
    #[must_use]
    pub fn session_before_compact<F, Fut, R>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionSessionBeforeCompactParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: Into<ExtensionSessionBeforeCompactResult> + Send + 'static,
    {
        self.handlers.compact = Some(Arc::new(move |context, request| {
            let future = handler(context, request);
            Box::pin(async move { future.await.into() })
        }));
        self
    }

    /// Registers the `user_before_input` interception hook.
    #[must_use]
    pub fn user_before_input<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionUserBeforeInputParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExtensionUserBeforeInputResult> + Send + 'static,
    {
        self.handlers.user_before_input = Some(Arc::new(move |context, request| {
            Box::pin(handler(context, request))
        }));
        self
    }
    /// Registers the `model_before_request` interception hook.
    #[must_use]
    pub fn model_before_request<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionModelBeforeRequestParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExtensionModelBeforeRequestResult> + Send + 'static,
    {
        self.handlers.model_before_request = Some(Arc::new(move |context, request| {
            Box::pin(handler(context, request))
        }));
        self
    }
    /// Registers the `provider_before_headers` interception hook.
    #[must_use]
    pub fn provider_before_headers<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionProviderBeforeHeadersParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExtensionProviderBeforeHeadersResult> + Send + 'static,
    {
        self.handlers.provider_before_headers = Some(Arc::new(move |context, request| {
            Box::pin(handler(context, request))
        }));
        self
    }
    /// Registers the `provider_before_request` interception hook.
    #[must_use]
    pub fn provider_before_request<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionProviderBeforeRequestParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExtensionProviderBeforeRequestResult> + Send + 'static,
    {
        self.handlers.provider_before_request = Some(Arc::new(move |context, request| {
            Box::pin(handler(context, request))
        }));
        self
    }
    /// Registers the observe-only `provider_after_response` interception hook.
    #[must_use]
    pub fn provider_after_response<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionProviderAfterResponseParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExtensionProviderAfterResponseResult> + Send + 'static,
    {
        self.handlers.provider_after_response = Some(Arc::new(move |context, request| {
            Box::pin(handler(context, request))
        }));
        self
    }
    /// Registers the `message_end` interception hook.
    #[must_use]
    pub fn message_end<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionMessageEndParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExtensionMessageEndResult> + Send + 'static,
    {
        self.handlers.message_end = Some(Arc::new(move |context, request| {
            Box::pin(handler(context, request))
        }));
        self
    }
    /// Registers the `model_before_select` interception hook.
    #[must_use]
    pub fn model_before_select<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionModelBeforeSelectParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExtensionAllowBlockResult> + Send + 'static,
    {
        self.handlers.model_before_select = Some(Arc::new(move |context, request| {
            Box::pin(handler(context, request))
        }));
        self
    }
    /// Registers the `session_before_fork` interception hook.
    #[must_use]
    pub fn session_before_fork<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionSessionBeforeForkParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExtensionAllowBlockResult> + Send + 'static,
    {
        self.handlers.session_before_fork = Some(Arc::new(move |context, request| {
            Box::pin(handler(context, request))
        }));
        self
    }
    /// Registers the `session_before_revert` interception hook.
    #[must_use]
    pub fn session_before_revert<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(PluginContext, ExtensionSessionBeforeRevertParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExtensionSessionBeforeRevertResult> + Send + 'static,
    {
        self.handlers.session_before_revert = Some(Arc::new(move |context, request| {
            Box::pin(handler(context, request))
        }));
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
    /// The SDK sends `plugin/recovery/complete` only after this callback returns. When producer
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
}

/// Context passed to plugin handlers for publishing correlated events.
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
        self.emit(session_id, name.into(), payload).await
    }

    async fn emit(
        &self,
        session_id: SessionId,
        name: String,
        payload: Value,
    ) -> Result<ExtensionEmitStatus, PluginError> {
        if !self.state.publishing.bus {
            return Err(PluginError::PublishingNotEnabled("bus"));
        }
        let context_id = self.consume(session_id)?;
        let params = ExtensionEmitParams {
            session_id,
            context_id,
            name: name.clone(),
            payload,
        };
        let value = serde_json::to_value(Notification::new(
            PLUGIN_EMIT_METHOD,
            Some(serde_json::to_value(params)?),
        ))?;
        let (reply, receive) = oneshot::channel();
        self.state
            .outbound
            .send(Outbound::Emit { value, name, reply })
            .await
            .map_err(|_| PluginError::TransportClosed)?;
        let result = receive.await.map_err(|_| PluginError::TransportClosed)??;
        Ok(result.bus)
    }

    /// Registers a long-lived producer for one session.
    ///
    /// The returned handle has no `Drop` cleanup. Call [`ProducerHandle::unregister`] explicitly,
    /// including when no messages were sent.
    pub async fn register_producer(
        &self,
        session_id: SessionId,
    ) -> Result<ProducerHandle, PluginError> {
        if !self.state.producers {
            return Err(PluginError::ProducerMessagingNotEnabled);
        }
        let result: ExtensionProducerRegisterResult = context_request(
            self,
            PLUGIN_PRODUCER_REGISTER_METHOD,
            ExtensionProducerRegisterParams { session_id },
        )
        .await?;
        Ok(ProducerHandle {
            context: self.clone(),
            session_id,
            producer_id: result.producer_id,
        })
    }

    /// Discards this plugin's waiting message by session and durable receipt.
    ///
    /// No producer registration is required, including after reconnect. The engine
    /// checks stable ownership and rejects consumed or currently claimed messages.
    /// A durable actor claim removes a message from waiting before request preparation
    /// and hooks. Discard rejects until release, even before network activity; the
    /// claim does not prove provider receipt or execution. Release after failed
    /// preparation or cancellation may return an unconsumed message to waiting;
    /// consumed messages cannot return. Rejection makes no exactly-once execution
    /// or external-effects guarantee. An already-discarded owned message succeeds.
    /// This never unregisters a producer.
    pub async fn discard_producer_message(
        &self,
        session_id: SessionId,
        message_id: ProducerMessageId,
    ) -> Result<(), PluginError> {
        let _: ExtensionProducerDiscardResult = context_request(
            self,
            PLUGIN_PRODUCER_DISCARD_METHOD,
            ExtensionProducerDiscardParams {
                session_id,
                message_id,
            },
        )
        .await?;
        Ok(())
    }

    fn register_notification(
        &self,
        session_id: SessionId,
        context_id: String,
        received_at: Instant,
    ) -> Self {
        self.register(
            session_id,
            context_id,
            GrantExpiry::At(received_at + NOTIFICATION_CONTEXT_LIFETIME),
        )
    }

    fn register_request(&self, session_id: SessionId, context_id: String) -> Self {
        self.register(session_id, context_id, GrantExpiry::Request)
    }

    fn register(&self, session_id: SessionId, context_id: String, expiry: GrantExpiry) -> Self {
        let now = Instant::now();
        let mut grants = self
            .state
            .grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let mut grants = self
            .state
            .grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(grant) = grants.get(&scope.context_id) else {
            return Err(PluginError::ContextUnavailable(session_id));
        };
        if grant.session_id != session_id
            || matches!(grant.expiry, GrantExpiry::At(deadline) if deadline <= Instant::now())
        {
            grants.remove(&scope.context_id);
            return Err(PluginError::ContextUnavailable(session_id));
        }
        grants.remove(&scope.context_id);
        Ok(scope.context_id.clone())
    }

    fn revoke(&self, context_id: &str) {
        self.state
            .grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(context_id);
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
        let description = cookie_agent_protocol::SafeDisplayText::new(description)
            .map_err(|error| PluginError::Protocol(format!("invalid description: {error}")))?;
        if description.as_str().trim().is_empty() {
            return Err(PluginError::Protocol(
                "description must not be blank".into(),
            ));
        }
        let result: ExtensionProducerSendResult = context_request(
            &self.context,
            PLUGIN_PRODUCER_SEND_METHOD,
            ExtensionProducerSendParams {
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

    /// Discards an owned waiting message in this handle's session.
    ///
    /// Still usable after unregister: message ownership is independent of this
    /// registration. A durable actor claim removes the message from waiting before
    /// request preparation and hooks, so discard rejects while claimed, not just
    /// after network dispatch. See [`PluginContext::discard_producer_message`] for
    /// claim release and consumption semantics.
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
        let _: ExtensionProducerUnregisterResult = context_request(
            &self.context,
            PLUGIN_PRODUCER_UNREGISTER_METHOD,
            ExtensionProducerUnregisterParams {
                session_id: self.session_id,
                producer_id: self.producer_id,
            },
        )
        .await?;
        Ok(())
    }
}

struct ContextState {
    grants: Mutex<HashMap<String, Grant>>,
    pending_emits: PendingEmits,
    pending_requests: PendingRequests,
    outbound: mpsc::Sender<Outbound>,
    publishing: PublishingCapabilities,
    producers: bool,
    request_ids: AtomicI64,
    pending_slots: Arc<Semaphore>,
    connected: AtomicBool,
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

#[derive(Clone, Copy)]
struct PublishingCapabilities {
    bus: bool,
}

enum GrantExpiry {
    At(Instant),
    Request,
}

type EmitReply = oneshot::Sender<Result<ExtensionEmitResultParams, PluginError>>;
type PendingEmits = Arc<Mutex<HashMap<String, VecDeque<EmitReply>>>>;
type RequestReply = oneshot::Sender<Result<Value, PluginError>>;

struct PendingRequest {
    reply: RequestReply,
    _permit: OwnedSemaphorePermit,
}

type PendingRequests = Arc<Mutex<HashMap<i64, PendingRequest>>>;

enum Outbound {
    Message(Value),
    Emit {
        value: Value,
        name: String,
        reply: EmitReply,
    },
    Request {
        value: Value,
        id: i64,
        pending: PendingRequest,
    },
}

enum Dispatch {
    Continue,
    Shutdown,
}

struct InboundFrame {
    message: String,
    received_at: Instant,
}

trait InterceptParams: serde::de::DeserializeOwned + Send + 'static {
    fn session_id(&self) -> SessionId;
    fn context_id(&self) -> &str;
}

macro_rules! intercept_params {
    ($($type:ty),+ $(,)?) => {
        $(impl InterceptParams for $type {
            fn session_id(&self) -> SessionId { self.session_id }
            fn context_id(&self) -> &str { &self.context_id }
        })+
    };
}

intercept_params!(
    ExtensionToolBeforeCallParams,
    ExtensionToolAfterResultParams,
    ExtensionAgentBeforeStartParams,
    ExtensionSessionBeforeCompactParams,
    ExtensionUserBeforeInputParams,
    ExtensionModelBeforeRequestParams,
    ExtensionProviderBeforeHeadersParams,
    ExtensionProviderBeforeRequestParams,
    ExtensionProviderAfterResponseParams,
    ExtensionMessageEndParams,
    ExtensionModelBeforeSelectParams,
    ExtensionSessionBeforeForkParams,
    ExtensionSessionBeforeRevertParams,
);

async fn dispatch_intercept<P, H, O, R>(
    request: Request,
    context: &PluginContext,
    handler: Option<H>,
    tasks: &mut Vec<JoinHandle<()>>,
    handler_slots: &Arc<Semaphore>,
    call: impl FnOnce(H, PluginContext, P) -> HandlerFuture<O> + Send + 'static,
    convert: impl FnOnce(O) -> R + Send + 'static,
) -> Result<(), PluginError>
where
    P: InterceptParams,
    H: Send + 'static,
    O: Send + 'static,
    R: Serialize + Send + 'static,
{
    let Some(handler) = handler else {
        send_error(
            context,
            request.id,
            METHOD_NOT_FOUND,
            "interception hook is not registered",
        )
        .await?;
        return Ok(());
    };
    let Some(permit) = try_handler_slot(context, &request, handler_slots).await? else {
        return Ok(());
    };
    let params: P = match parse_params(&request) {
        Ok(params) => params,
        Err(error) => {
            send_error(context, request.id, INVALID_PARAMS, error.to_string()).await?;
            return Ok(());
        }
    };
    let session_id = params.session_id();
    let context_id = params.context_id().to_owned();
    let plugin_context = context.register_request(session_id, context_id.clone());
    tasks.push(tokio::spawn(async move {
        let _permit = permit;
        let handler_context = plugin_context.clone();
        let result = isolate(async move { call(handler, handler_context, params).await }).await;
        plugin_context.revoke(&context_id);
        match result {
            Ok(result) => {
                let _ = send_success(&plugin_context, request.id, convert(result)).await;
            }
            Err(()) => {
                let _ = send_error(
                    &plugin_context,
                    request.id,
                    INTERNAL_ERROR,
                    "plugin interception handler panicked",
                )
                .await;
            }
        }
    }));
    Ok(())
}

async fn try_handler_slot(
    context: &PluginContext,
    request: &Request,
    slots: &Arc<Semaphore>,
) -> Result<Option<OwnedSemaphorePermit>, PluginError> {
    match Arc::clone(slots).try_acquire_owned() {
        Ok(permit) => Ok(Some(permit)),
        Err(_) => {
            send_error(
                context,
                request.id.clone(),
                SERVER_BUSY,
                "plugin handler capacity exhausted",
            )
            .await?;
            Ok(None)
        }
    }
}

async fn isolate<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> Result<T, ()> {
    tokio::spawn(future).await.map_err(|_| ())
}

fn parse_params<T: serde::de::DeserializeOwned>(request: &Request) -> Result<T, PluginError> {
    serde_json::from_value(request.params.clone().unwrap_or(Value::Null)).map_err(PluginError::from)
}

fn parse_notification_params<T: serde::de::DeserializeOwned>(
    notification: &Notification,
) -> Result<T, PluginError> {
    serde_json::from_value(notification.params.clone().unwrap_or(Value::Null))
        .map_err(PluginError::from)
}

async fn send_success(
    context: &PluginContext,
    id: JsonRpcId,
    result: impl Serialize,
) -> Result<(), PluginError> {
    let response = SuccessResponse {
        jsonrpc: JsonRpcVersion::current(),
        id,
        result: serde_json::to_value(result)?,
    };
    send_value(context, serde_json::to_value(response)?).await
}

async fn send_error(
    context: &PluginContext,
    id: JsonRpcId,
    code: i32,
    message: impl Into<String>,
) -> Result<(), PluginError> {
    send_rpc_error(
        context,
        id,
        JsonRpcError {
            code,
            message: message.into(),
            data: None,
        },
    )
    .await
}

async fn send_rpc_error(
    context: &PluginContext,
    id: JsonRpcId,
    error: JsonRpcError,
) -> Result<(), PluginError> {
    let response = ErrorResponse {
        jsonrpc: JsonRpcVersion::current(),
        id,
        error,
    };
    send_value(context, serde_json::to_value(response)?).await
}

async fn send_value(context: &PluginContext, value: Value) -> Result<(), PluginError> {
    context
        .state
        .outbound
        .send(Outbound::Message(value))
        .await
        .map_err(|_| PluginError::TransportClosed)
}

async fn context_request<P, R>(
    context: &PluginContext,
    method: &'static str,
    params: P,
) -> Result<R, PluginError>
where
    P: Serialize,
    R: serde::de::DeserializeOwned,
{
    if !context.state.producers {
        return Err(PluginError::ProducerMessagingNotEnabled);
    }
    if !context.state.connected.load(Ordering::Acquire) {
        return Err(PluginError::TransportClosed);
    }
    let permit = Arc::clone(&context.state.pending_slots)
        .try_acquire_owned()
        .map_err(|_| PluginError::TooManyPendingRequests)?;
    let id = context.state.request_ids.fetch_add(1, Ordering::Relaxed);
    let request = Request::new(
        JsonRpcId::Number(id),
        method,
        Some(serde_json::to_value(params)?),
    );
    let (reply, receive) = oneshot::channel();
    context
        .state
        .outbound
        .send(Outbound::Request {
            value: serde_json::to_value(request)?,
            id,
            pending: PendingRequest {
                reply,
                _permit: permit,
            },
        })
        .await
        .map_err(|_| PluginError::TransportClosed)?;
    let value = receive.await.map_err(|_| PluginError::TransportClosed)??;
    serde_json::from_value(value)
        .map_err(|error| PluginError::Protocol(format!("invalid result for `{method}`: {error}")))
}

async fn reader_loop<R>(reader: R, sender: mpsc::Sender<Result<InboundFrame, PluginError>>)
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    loop {
        match read_frame(&mut reader).await {
            Ok(Some(frame)) => {
                let frame = InboundFrame {
                    message: frame,
                    received_at: Instant::now(),
                };
                if sender.send(Ok(frame)).await.is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                return;
            }
        }
    }
}

async fn writer_loop<W>(
    mut writer: W,
    mut receiver: mpsc::Receiver<Outbound>,
    pending_emits: PendingEmits,
    pending_requests: PendingRequests,
) -> Result<(), PluginError>
where
    W: AsyncWrite + Unpin,
{
    while let Some(outbound) = receiver.recv().await {
        let (value, emit, request) = match outbound {
            Outbound::Message(value) => (value, None, None),
            Outbound::Emit { value, name, reply } => (value, Some((name, reply)), None),
            Outbound::Request { value, id, pending } => (value, None, Some((id, pending))),
        };
        let bytes = serde_json::to_vec(&value)?;
        if bytes.len() > MAX_FRAME_BYTES {
            if let Some((_, reply)) = emit {
                let _ = reply.send(Err(PluginError::Protocol(format!(
                    "plugin frame exceeds {MAX_FRAME_BYTES} bytes"
                ))));
                continue;
            }
            if let Some((_, pending)) = request {
                let _ = pending.reply.send(Err(PluginError::Protocol(format!(
                    "plugin frame exceeds {MAX_FRAME_BYTES} bytes"
                ))));
                continue;
            }
            return Err(PluginError::Protocol(format!(
                "plugin frame exceeds {MAX_FRAME_BYTES} bytes"
            )));
        }
        if let Some((name, reply)) = emit {
            pending_emits
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(name)
                .or_default()
                .push_back(reply);
        }
        if let Some((id, request)) = request {
            pending_requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id, request);
        }
        writer.write_all(&bytes).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}

async fn writer_task_finished(
    task: &mut JoinHandle<Result<(), PluginError>>,
) -> Result<(), PluginError> {
    match task.await {
        Ok(result) => result,
        Err(_) => Err(PluginError::TransportClosed),
    }
}

fn resolve_emit(pending: &PendingEmits, result: ExtensionEmitResultParams) {
    let reply = {
        let mut pending = pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let reply = pending.get_mut(&result.name).and_then(VecDeque::pop_front);
        if pending.get(&result.name).is_some_and(VecDeque::is_empty) {
            pending.remove(&result.name);
        }
        reply
    };
    if let Some(reply) = reply {
        let _ = reply.send(Ok(result));
    }
}

fn fail_pending_emits(pending: &PendingEmits) {
    let replies = pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain()
        .flat_map(|(_, replies)| replies)
        .collect::<Vec<_>>();
    for reply in replies {
        let _ = reply.send(Err(PluginError::TransportClosed));
    }
}

fn resolve_request(pending: &PendingRequests, response: Response) -> Result<(), PluginError> {
    let (id, result) = match response {
        Response::Success(response) => (response.id, Ok(response.result)),
        Response::Error(response) => (
            response.id,
            Err(PluginError::EngineRequest {
                code: response.error.code,
                message: response.error.message,
                data: response.error.data,
            }),
        ),
    };
    let JsonRpcId::Number(id) = id else {
        return Err(PluginError::Protocol(
            "engine response used a non-numeric plugin request ID".into(),
        ));
    };
    let request = pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&id)
        .ok_or_else(|| PluginError::Protocol(format!("engine response has unknown ID {id}")))?;
    let _ = request.reply.send(result);
    Ok(())
}

fn fail_pending_requests(pending: &PendingRequests) {
    let requests = pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain()
        .map(|(_, request)| request)
        .collect::<Vec<_>>();
    for request in requests {
        let _ = request.reply.send(Err(PluginError::TransportClosed));
    }
}

fn validate_tool(tool: &ToolDecl, duplicate: bool) -> Result<(), PluginError> {
    if duplicate {
        return Err(PluginError::InvalidTool(format!(
            "duplicate tool name `{}`",
            tool.name
        )));
    }
    if !is_snake_case(&tool.name) {
        return Err(PluginError::InvalidTool(format!(
            "tool name `{}` must be snake_case",
            tool.name
        )));
    }
    if tool.description.is_empty() {
        return Err(PluginError::InvalidTool(format!(
            "tool `{}` description must not be empty",
            tool.name
        )));
    }
    if !is_snake_case(&tool.permission_name) {
        return Err(PluginError::InvalidTool(format!(
            "tool `{}` permission_name must be snake_case",
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
    if let Some(primary) = &tool.primary_resource_param
        && !tool
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| properties.contains_key(primary))
    {
        return Err(PluginError::InvalidTool(format!(
            "tool `{}` primary_resource_param `{primary}` is not declared in properties",
            tool.name
        )));
    }
    Ok(())
}

fn is_snake_case(value: &str) -> bool {
    !value.is_empty()
        && value.split('_').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
        && value.as_bytes()[0].is_ascii_lowercase()
}

#[cfg(test)]
mod tests;
