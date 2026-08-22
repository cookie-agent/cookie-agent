use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use cookie_agent_protocol::{
    ErrorResponse, ExtensionAgentBeforeStartParams, ExtensionAgentBeforeStartResult,
    ExtensionAllowBlockResult, ExtensionBusEventParams, ExtensionEmitParams,
    ExtensionEmitResultParams, ExtensionEmitStatus, ExtensionEventParams,
    ExtensionInitializeParams, ExtensionInitializeResult, ExtensionInterceptionHook,
    ExtensionMessageEndParams, ExtensionMessageEndResult, ExtensionModelBeforeRequestParams,
    ExtensionModelBeforeRequestResult, ExtensionModelBeforeSelectParams, ExtensionPingParams,
    ExtensionPingResult, ExtensionPluginCapabilities, ExtensionProtocolVersion,
    ExtensionProviderAfterResponseParams, ExtensionProviderAfterResponseResult,
    ExtensionProviderBeforeHeadersParams, ExtensionProviderBeforeHeadersResult,
    ExtensionProviderBeforeRequestParams, ExtensionProviderBeforeRequestResult,
    ExtensionSessionBeforeCompactParams, ExtensionSessionBeforeCompactResult,
    ExtensionSessionBeforeForkParams, ExtensionSessionBeforeRevertParams,
    ExtensionSessionBeforeRevertResult, ExtensionShutdownParams, ExtensionToolAfterResultParams,
    ExtensionToolAfterResultResult, ExtensionToolBeforeCallParams, ExtensionToolBeforeCallResult,
    ExtensionToolCallParams, ExtensionToolCallResult, ExtensionUserBeforeInputParams,
    ExtensionUserBeforeInputResult, JsonRpcError, JsonRpcId, JsonRpcVersion, Notification,
    PLUGIN_BUS_EVENT_METHOD, PLUGIN_EMIT_METHOD, PLUGIN_EMIT_RESULT_METHOD, PLUGIN_EVENT_METHOD,
    PLUGIN_INITIALIZE_METHOD, PLUGIN_INTERCEPT_AGENT_BEFORE_START_METHOD,
    PLUGIN_INTERCEPT_MESSAGE_END_METHOD, PLUGIN_INTERCEPT_MODEL_BEFORE_REQUEST_METHOD,
    PLUGIN_INTERCEPT_MODEL_BEFORE_SELECT_METHOD, PLUGIN_INTERCEPT_PROVIDER_AFTER_RESPONSE_METHOD,
    PLUGIN_INTERCEPT_PROVIDER_BEFORE_HEADERS_METHOD,
    PLUGIN_INTERCEPT_PROVIDER_BEFORE_REQUEST_METHOD,
    PLUGIN_INTERCEPT_SESSION_BEFORE_COMPACT_METHOD, PLUGIN_INTERCEPT_SESSION_BEFORE_FORK_METHOD,
    PLUGIN_INTERCEPT_SESSION_BEFORE_REVERT_METHOD, PLUGIN_INTERCEPT_TOOL_AFTER_RESULT_METHOD,
    PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD, PLUGIN_INTERCEPT_USER_BEFORE_INPUT_METHOD,
    PLUGIN_PING_METHOD, PLUGIN_SHUTDOWN_METHOD, PLUGIN_TOOLS_CALL_METHOD, Request, SessionId,
    SuccessResponse,
};
use serde::Serialize;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader},
    sync::{mpsc, oneshot},
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
    publish_bus: bool,
    publish_session_events: bool,
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
            tools: !self.tools.is_empty(),
            resources: false,
            subscribe_events: self.event.is_some(),
            subscribe_bus: self.bus.is_some(),
            publish_bus: self.publish_bus,
            publish_session_events: self.publish_session_events,
            intercept,
        }
    }
}

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
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (outbound, outbound_rx) = mpsc::channel(128);
        let state = Arc::new(ContextState {
            grants: Mutex::new(HashMap::new()),
            pending: Arc::clone(&pending),
            outbound,
            publishing: PublishingCapabilities {
                bus: self.handlers.publish_bus,
                session: self.handlers.publish_session_events,
            },
        });
        let context = PluginContext { state, grant: None };
        let mut writer_task = tokio::spawn(writer_loop(writer, outbound_rx, Arc::clone(&pending)));
        let (inbound_tx, mut inbound_rx) = mpsc::channel(32);
        let reader_task = tokio::spawn(reader_loop(reader, inbound_tx));

        let mut dispatch_tasks: Vec<JoinHandle<()>> = Vec::new();
        let result = loop {
            tokio::select! {
                inbound = inbound_rx.recv() => match inbound {
                    Some(Ok(frame)) => {
                        dispatch_tasks.retain(|task| !task.is_finished());
                        match self.dispatch_message(
                            &frame.message,
                            frame.received_at,
                            &context,
                            &mut dispatch_tasks,
                        ).await {
                            Ok(Dispatch::Continue) => {}
                            Ok(Dispatch::Shutdown) => break Ok(()),
                            Err(error) => break Err(error),
                        }
                    }
                    Some(Err(error)) => break Err(error),
                    None => break Ok(()),
                },
                writer_result = writer_task_finished(&mut writer_task) => {
                    break match writer_result {
                        Ok(()) => Err(PluginError::TransportClosed),
                        Err(error) => Err(error),
                    };
                }
            }
        };

        reader_task.abort();
        for task in dispatch_tasks {
            task.abort();
        }
        fail_pending(&pending);
        writer_task.abort();
        let _ = reader_task.await;
        let _ = writer_task.await;
        result
    }

    async fn dispatch_message(
        &self,
        message: &str,
        received_at: Instant,
        context: &PluginContext,
        tasks: &mut Vec<JoinHandle<()>>,
    ) -> Result<Dispatch, PluginError> {
        let value: Value = serde_json::from_str(message)?;
        if value.get("method").is_none() {
            return Ok(Dispatch::Continue);
        }
        if value.get("id").is_some() {
            let request: Request = serde_json::from_value(value).map_err(|error| {
                PluginError::Protocol(format!("malformed engine request: {error}"))
            })?;
            self.dispatch_request(request, context, tasks).await?;
            return Ok(Dispatch::Continue);
        }

        let notification: Notification = serde_json::from_value(value).map_err(|error| {
            PluginError::Protocol(format!("malformed engine notification: {error}"))
        })?;
        self.dispatch_notification(notification, received_at, context, tasks)
            .await
    }

    async fn dispatch_request(
        &self,
        request: Request,
        context: &PluginContext,
        tasks: &mut Vec<JoinHandle<()>>,
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
    ) -> Result<Dispatch, PluginError> {
        match notification.method.as_str() {
            PLUGIN_SHUTDOWN_METHOD => {
                let _: ExtensionShutdownParams = parse_notification_params(&notification)?;
                Ok(Dispatch::Shutdown)
            }
            PLUGIN_EVENT_METHOD => {
                let params: ExtensionEventParams = parse_notification_params(&notification)?;
                if let Some(handler) = self.handlers.event.clone() {
                    let plugin_context = context.register_notification(
                        params.session_id,
                        params.context_id.clone(),
                        received_at,
                    );
                    tasks.push(tokio::spawn(async move {
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
                resolve_emit(&context.state.pending, result);
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

    /// Enables non-durable bus publishing through [`PluginContext::emit_bus`].
    #[must_use]
    pub fn enable_bus_publishing(mut self) -> Self {
        self.handlers.publish_bus = true;
        self
    }

    /// Enables durable session publishing through [`PluginContext::emit_session`].
    #[must_use]
    pub fn enable_session_publishing(mut self) -> Self {
        self.handlers.publish_session_events = true;
        self
    }

    /// Validates registrations and creates the server.
    pub fn build(self) -> Result<PluginServer, PluginError> {
        if let Some(error) = self.error {
            return Err(error);
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
        self.emit(session_id, name.into(), payload, EmitTarget::Bus)
            .await
    }

    /// Publishes a durable session event using this handler's one-shot context grant.
    ///
    /// The server must be configured with
    /// [`PluginServerBuilder::enable_session_publishing`]. Notification grants expire locally
    /// after four seconds, one second before the engine's authoritative deadline.
    pub async fn emit_session(
        &self,
        session_id: SessionId,
        name: impl Into<String>,
        payload: Value,
    ) -> Result<ExtensionEmitStatus, PluginError> {
        self.emit(session_id, name.into(), payload, EmitTarget::Session)
            .await
    }

    async fn emit(
        &self,
        session_id: SessionId,
        name: String,
        payload: Value,
        target: EmitTarget,
    ) -> Result<ExtensionEmitStatus, PluginError> {
        if !self.state.publishing.enabled(target) {
            return Err(PluginError::PublishingNotEnabled(target.name()));
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
        Ok(match target {
            EmitTarget::Bus => result.bus,
            EmitTarget::Session => result.durable,
        })
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

struct ContextState {
    grants: Mutex<HashMap<String, Grant>>,
    pending: PendingEmits,
    outbound: mpsc::Sender<Outbound>,
    publishing: PublishingCapabilities,
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
    session: bool,
}

impl PublishingCapabilities {
    fn enabled(self, target: EmitTarget) -> bool {
        match target {
            EmitTarget::Bus => self.bus,
            EmitTarget::Session => self.session,
        }
    }
}

enum GrantExpiry {
    At(Instant),
    Request,
}

type EmitReply = oneshot::Sender<Result<ExtensionEmitResultParams, PluginError>>;
type PendingEmits = Arc<Mutex<HashMap<String, VecDeque<EmitReply>>>>;

enum Outbound {
    Message(Value),
    Emit {
        value: Value,
        name: String,
        reply: EmitReply,
    },
}

#[derive(Clone, Copy)]
enum EmitTarget {
    Bus,
    Session,
}

impl EmitTarget {
    const fn name(self) -> &'static str {
        match self {
            Self::Bus => "bus",
            Self::Session => "session",
        }
    }
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
    pending: PendingEmits,
) -> Result<(), PluginError>
where
    W: AsyncWrite + Unpin,
{
    while let Some(outbound) = receiver.recv().await {
        let (value, emit) = match outbound {
            Outbound::Message(value) => (value, None),
            Outbound::Emit { value, name, reply } => (value, Some((name, reply))),
        };
        let bytes = serde_json::to_vec(&value)?;
        if bytes.len() > MAX_FRAME_BYTES {
            if let Some((_, reply)) = emit {
                let _ = reply.send(Err(PluginError::Protocol(format!(
                    "plugin frame exceeds {MAX_FRAME_BYTES} bytes"
                ))));
                continue;
            }
            return Err(PluginError::Protocol(format!(
                "plugin frame exceeds {MAX_FRAME_BYTES} bytes"
            )));
        }
        if let Some((name, reply)) = emit {
            pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(name)
                .or_default()
                .push_back(reply);
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

fn fail_pending(pending: &PendingEmits) {
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
mod tests {
    use cookie_agent_protocol::{
        ExtensionEngineCapabilities, ExtensionToolAfterResultAction, ExtensionToolBeforeCallAction,
        ToolCallId, extension_initialize_request, extension_shutdown_notification,
    };
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    use super::*;
    use crate::{allow, replace};

    fn declaration() -> ToolDecl {
        ToolDecl {
            name: "echo".into(),
            description: "Echo text".into(),
            parameters: json!({
                "type": "object",
                "properties": {"text": {"type": "string"}}
            }),
            permission_name: "echo".into(),
            primary_resource_param: None,
        }
    }

    #[test]
    fn derives_capabilities_from_handlers() {
        let server = PluginServer::builder("echo", "0.1.0")
            .tool(declaration(), |_ctx, _request| async {
                Ok(ToolOutput::success("ok"))
            })
            .tool_before_call(|_ctx, _request| async { allow() })
            .tool_after_result(|_ctx, _request| async { replace("new") })
            .on_event(|_ctx, _event| async {})
            .on_bus_event(|_ctx, _event| async {})
            .enable_bus_publishing()
            .enable_session_publishing()
            .build()
            .unwrap();
        let capabilities = server.handlers.capabilities();
        assert!(capabilities.tools);
        assert!(capabilities.subscribe_events);
        assert!(capabilities.subscribe_bus);
        assert!(capabilities.publish_bus);
        assert!(capabilities.publish_session_events);
        assert_eq!(
            capabilities.intercept,
            [
                ExtensionInterceptionHook::ToolBeforeCall,
                ExtensionInterceptionHook::ToolAfterResult,
            ]
        );
    }

    #[test]
    fn publishing_capabilities_require_explicit_opt_in() {
        let server = PluginServer::builder("echo", "0.1.0")
            .tool(declaration(), |_ctx, _request| async {
                Ok(ToolOutput::success("ok"))
            })
            .build()
            .unwrap();
        let capabilities = server.handlers.capabilities();
        assert!(!capabilities.publish_bus);
        assert!(!capabilities.publish_session_events);
    }

    #[test]
    fn validates_schema_during_building() {
        let mut invalid = declaration();
        invalid.parameters = json!({"type": 42});
        let error = PluginServer::builder("echo", "0.1.0")
            .tool(invalid, |_ctx, _request| async {
                Ok(ToolOutput::success("unused"))
            })
            .build()
            .err()
            .unwrap();
        assert!(error.to_string().contains("JSON Schema"));
    }

    #[tokio::test]
    async fn loopback_handshake_tool_ping_and_shutdown() {
        let server = PluginServer::builder("echo", "0.1.0")
            .tool(declaration(), |_ctx, request| async move {
                Ok(ToolOutput::success(
                    request.arguments["text"].as_str().unwrap(),
                ))
            })
            .build()
            .unwrap();
        let (engine_side, plugin_side) = tokio::io::duplex(64 * 1024);
        let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
        let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
        let (engine_read, mut engine_write) = tokio::io::split(engine_side);
        let mut engine_read = BufReader::new(engine_read);

        write_wire(&mut engine_write, &extension_initialize_request("test")).await;
        let initialize = read_wire(&mut engine_read).await;
        assert_eq!(
            initialize["result"]["protocol_version"],
            crate::EXTENSION_PROTOCOL_VERSION
        );
        assert_eq!(initialize["result"]["name"], "echo");
        assert_eq!(initialize["result"]["capabilities"]["tools"], true);

        let session_id = SessionId::new_v7();
        let invocation_id = ToolCallId::new_v7();
        let call = Request::new(
            JsonRpcId::Number(2),
            PLUGIN_TOOLS_CALL_METHOD,
            Some(
                serde_json::to_value(ExtensionToolCallParams {
                    tool: "echo".into(),
                    session_id,
                    context_id: "context".into(),
                    invocation_id,
                    arguments: json!({"text": "hello"}),
                    resource: None,
                    cancellation_token: None,
                })
                .unwrap(),
            ),
        );
        write_wire(&mut engine_write, &call).await;
        let response = read_wire(&mut engine_read).await;
        assert_eq!(
            response["result"],
            json!({"content": "hello", "is_error": false})
        );

        let ping = Request::new(JsonRpcId::Number(3), PLUGIN_PING_METHOD, Some(json!({})));
        write_wire(&mut engine_write, &ping).await;
        assert_eq!(read_wire(&mut engine_read).await["result"], json!({}));

        write_wire(&mut engine_write, &extension_shutdown_notification()).await;
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_dispatches_every_hook() {
        let server = PluginServer::builder("hooks", "0.1.0")
            .agent_before_start(|_ctx, _| async { crate::append_system_prompt("agent") })
            .session_before_compact(|_ctx, _| async { crate::cancel_compaction("compact") })
            .user_before_input(|_ctx, _| async {
                ExtensionUserBeforeInputResult {
                    action: cookie_agent_protocol::ExtensionUserBeforeInputAction::Transform,
                    new_text: Some("changed".into()),
                    reason: None,
                }
            })
            .model_before_request(|_ctx, _| async {
                ExtensionModelBeforeRequestResult {
                    action: cookie_agent_protocol::ExtensionModelBeforeRequestAction::Keep,
                    messages: None,
                    params_adjustments: None,
                }
            })
            .provider_before_headers(|_ctx, _| async {
                ExtensionProviderBeforeHeadersResult {
                    set: [("x-test".into(), "yes".into())].into(),
                    delete: vec!["x-old".into()],
                }
            })
            .provider_before_request(|_ctx, _| async {
                ExtensionProviderBeforeRequestResult {
                    action: cookie_agent_protocol::ExtensionProviderBeforeRequestAction::Keep,
                    payload: None,
                }
            })
            .provider_after_response(|_ctx, _| async { ExtensionProviderAfterResponseResult {} })
            .message_end(|_ctx, _| async {
                ExtensionMessageEndResult {
                    action: cookie_agent_protocol::ExtensionMessageEndAction::Keep,
                    content: None,
                }
            })
            .model_before_select(|_ctx, _| async {
                ExtensionAllowBlockResult {
                    action: cookie_agent_protocol::ExtensionAllowBlockAction::Allow,
                    reason: None,
                }
            })
            .session_before_fork(|_ctx, _| async {
                ExtensionAllowBlockResult {
                    action: cookie_agent_protocol::ExtensionAllowBlockAction::Allow,
                    reason: None,
                }
            })
            .session_before_revert(|_ctx, _| async {
                ExtensionSessionBeforeRevertResult {
                    action: cookie_agent_protocol::ExtensionSessionBeforeRevertAction::Override,
                    reason: None,
                    instructions_override: Some("revert".into()),
                }
            })
            .build()
            .unwrap();
        assert_eq!(server.handlers.capabilities().intercept.len(), 11);

        let (engine_side, plugin_side) = tokio::io::duplex(256 * 1024);
        let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
        let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
        let (engine_read, mut engine_write) = tokio::io::split(engine_side);
        let mut engine_read = BufReader::new(engine_read);
        write_wire(&mut engine_write, &extension_initialize_request("test")).await;
        let initialize = read_wire(&mut engine_read).await;
        assert_eq!(
            initialize["result"]["capabilities"]["intercept"]
                .as_array()
                .unwrap()
                .len(),
            11
        );

        let session = SessionId::new_v7();
        let attempt = cookie_agent_protocol::AttemptId::new_v7();
        let model_selection = json!({"model": "custom.test/model", "variant": null});
        let resolved_model = json!({
            "selection": model_selection.clone(),
            "provider_id": "custom.test",
            "model_id": "model",
            "adapter_id": "openai-responses",
            "selection_fingerprint": "0".repeat(64),
        });
        let calls = [
            (
                PLUGIN_INTERCEPT_AGENT_BEFORE_START_METHOD,
                json!({"session_id":session,"context_id":"hook","agent_path":"test","prompt_context":{}}),
                json!("agent"),
            ),
            (
                PLUGIN_INTERCEPT_SESSION_BEFORE_COMPACT_METHOD,
                json!({"session_id":session,"context_id":"hook","checkpoint_id":"one","additions":[]}),
                json!(true),
            ),
            (
                PLUGIN_INTERCEPT_USER_BEFORE_INPUT_METHOD,
                json!({"session_id":session,"context_id":"hook","text":"old"}),
                json!("changed"),
            ),
            (
                PLUGIN_INTERCEPT_MODEL_BEFORE_REQUEST_METHOD,
                json!({"session_id":session,"context_id":"hook","attempt_id":attempt,"messages":[],"model":resolved_model,"params":{}}),
                json!("keep"),
            ),
            (
                PLUGIN_INTERCEPT_PROVIDER_BEFORE_HEADERS_METHOD,
                json!({"session_id":session,"context_id":"hook","attempt_id":attempt,"headers":{}}),
                json!("yes"),
            ),
            (
                PLUGIN_INTERCEPT_PROVIDER_BEFORE_REQUEST_METHOD,
                json!({"session_id":session,"context_id":"hook","attempt_id":attempt,"payload":{}}),
                json!("keep"),
            ),
            (
                PLUGIN_INTERCEPT_PROVIDER_AFTER_RESPONSE_METHOD,
                json!({"session_id":session,"context_id":"hook","attempt_id":attempt,"status":200,"headers":{}}),
                Value::Null,
            ),
            (
                PLUGIN_INTERCEPT_MESSAGE_END_METHOD,
                json!({"session_id":session,"context_id":"hook","attempt_id":attempt,"role":"assistant","content":[]}),
                json!("keep"),
            ),
            (
                PLUGIN_INTERCEPT_MODEL_BEFORE_SELECT_METHOD,
                json!({"session_id":session,"context_id":"hook","from":null,"to":model_selection,"source":"user"}),
                json!("allow"),
            ),
            (
                PLUGIN_INTERCEPT_SESSION_BEFORE_FORK_METHOD,
                json!({"session_id":session,"context_id":"hook","through_seq":1}),
                json!("allow"),
            ),
            (
                PLUGIN_INTERCEPT_SESSION_BEFORE_REVERT_METHOD,
                json!({"session_id":session,"context_id":"hook","through_seq":1}),
                json!("revert"),
            ),
        ];
        for (index, (method, params, expected)) in calls.into_iter().enumerate() {
            write_wire(
                &mut engine_write,
                &Request::new(JsonRpcId::Number(index as i64 + 2), method, Some(params)),
            )
            .await;
            let response = read_wire(&mut engine_read).await;
            let result = response["result"].clone();
            if expected.is_null() {
                assert_eq!(result, json!({}), "{method}");
            } else {
                assert!(
                    result.to_string().contains(&expected.to_string()),
                    "{method}: {response}"
                );
            }
        }
        write_wire(&mut engine_write, &extension_shutdown_notification()).await;
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_emit_uses_delivered_context_and_returns_selected_status() {
        let (status_tx, mut status_rx) = mpsc::channel(1);
        let server = PluginServer::builder("emitter", "0.1.0")
            .on_bus_event(move |context, event| {
                let status_tx = status_tx.clone();
                async move {
                    let status = context
                        .emit_bus(event.session_id, "echoed", event.payload)
                        .await
                        .unwrap();
                    status_tx.send(status).await.unwrap();
                }
            })
            .enable_bus_publishing()
            .build()
            .unwrap();
        let (engine_side, plugin_side) = tokio::io::duplex(64 * 1024);
        let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
        let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
        let (engine_read, mut engine_write) = tokio::io::split(engine_side);
        let mut engine_read = BufReader::new(engine_read);

        write_wire(&mut engine_write, &extension_initialize_request("test")).await;
        let _ = read_wire(&mut engine_read).await;
        let session_id = SessionId::new_v7();
        write_wire(
            &mut engine_write,
            &Notification::new(
                PLUGIN_BUS_EVENT_METHOD,
                Some(
                    serde_json::to_value(ExtensionBusEventParams {
                        session_id,
                        context_id: Some("emit-context".into()),
                        plugin: "source".into(),
                        name: "incoming".into(),
                        payload: json!({"value": 1}),
                    })
                    .unwrap(),
                ),
            ),
        )
        .await;
        let emit = read_wire(&mut engine_read).await;
        assert_eq!(emit["method"], PLUGIN_EMIT_METHOD);
        assert_eq!(emit["params"]["session_id"], session_id.to_string());
        assert_eq!(emit["params"]["context_id"], "emit-context");
        assert_eq!(emit["params"]["name"], "echoed");
        write_wire(
            &mut engine_write,
            &Notification::new(
                PLUGIN_EMIT_RESULT_METHOD,
                Some(json!({
                    "name": "echoed",
                    "bus": "published",
                    "durable": "rejected",
                    "reason": "durable publishing disabled"
                })),
            ),
        )
        .await;
        assert_eq!(status_rx.recv().await, Some(ExtensionEmitStatus::Published));

        write_wire(&mut engine_write, &extension_shutdown_notification()).await;
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn concurrent_same_session_calls_emit_with_their_own_contexts() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let server = PluginServer::builder("concurrent", "0.1.0")
            .tool(declaration(), move |context, request| {
                let barrier = Arc::clone(&barrier);
                async move {
                    barrier.wait().await;
                    let name = request.arguments["text"].as_str().unwrap().to_owned();
                    let status = context
                        .emit_bus(request.session_id, name.clone(), json!({"call": name}))
                        .await
                        .unwrap();
                    assert_eq!(status, ExtensionEmitStatus::Published);
                    Ok(ToolOutput::success(name))
                }
            })
            .enable_bus_publishing()
            .build()
            .unwrap();
        let (engine_side, plugin_side) = tokio::io::duplex(64 * 1024);
        let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
        let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
        let (engine_read, mut engine_write) = tokio::io::split(engine_side);
        let mut engine_read = BufReader::new(engine_read);

        write_wire(&mut engine_write, &extension_initialize_request("test")).await;
        let _ = read_wire(&mut engine_read).await;
        let session_id = SessionId::new_v7();
        write_wire(
            &mut engine_write,
            &tool_call_request(2, session_id, "context-first", "first"),
        )
        .await;
        write_wire(
            &mut engine_write,
            &tool_call_request(3, session_id, "context-second", "second"),
        )
        .await;

        let emits = [
            read_wire(&mut engine_read).await,
            read_wire(&mut engine_read).await,
        ];
        for emit in &emits {
            let name = emit["params"]["name"].as_str().unwrap();
            let expected_context = match name {
                "first" => "context-first",
                "second" => "context-second",
                other => panic!("unexpected emit name {other}"),
            };
            assert_eq!(emit["params"]["session_id"], session_id.to_string());
            assert_eq!(emit["params"]["context_id"], expected_context);
        }
        for emit in &emits {
            write_wire(
                &mut engine_write,
                &Notification::new(
                    PLUGIN_EMIT_RESULT_METHOD,
                    Some(json!({
                        "name": emit["params"]["name"],
                        "bus": "published",
                        "durable": "rejected"
                    })),
                ),
            )
            .await;
        }

        let responses = [
            read_wire(&mut engine_read).await,
            read_wire(&mut engine_read).await,
        ];
        let mut outputs = responses
            .iter()
            .map(|response| response["result"]["content"].as_str().unwrap())
            .collect::<Vec<_>>();
        outputs.sort_unstable();
        assert_eq!(outputs, ["first", "second"]);

        write_wire(&mut engine_write, &extension_shutdown_notification()).await;
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn emit_grants_are_one_shot_expire_and_reject_unknown_sessions() {
        let (outbound, mut receiver) = mpsc::channel(4);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let context = PluginContext {
            state: Arc::new(ContextState {
                grants: Mutex::new(HashMap::new()),
                pending,
                outbound,
                publishing: PublishingCapabilities {
                    bus: true,
                    session: true,
                },
            }),
            grant: None,
        };
        let session = SessionId::new_v7();
        let unknown = SessionId::new_v7();
        let first = context.register_notification(session, "first".into(), Instant::now());
        assert!(matches!(
            first.consume(unknown),
            Err(PluginError::ContextUnavailable(id)) if id == unknown
        ));
        assert_eq!(first.consume(session).unwrap(), "first");
        assert!(matches!(
            first.consume(session),
            Err(PluginError::ContextUnavailable(id)) if id == session
        ));

        let received_at = Instant::now();
        tokio::time::advance(NOTIFICATION_CONTEXT_LIFETIME).await;
        let expiring = context.register_notification(session, "expiring".into(), received_at);
        assert!(matches!(
            expiring.consume(session),
            Err(PluginError::ContextUnavailable(id)) if id == session
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn emit_requires_target_capability_without_consuming_the_grant() {
        let (outbound, mut receiver) = mpsc::channel(1);
        let context = PluginContext {
            state: Arc::new(ContextState {
                grants: Mutex::new(HashMap::new()),
                pending: Arc::new(Mutex::new(HashMap::new())),
                outbound,
                publishing: PublishingCapabilities {
                    bus: false,
                    session: false,
                },
            }),
            grant: None,
        };
        let session = SessionId::new_v7();
        let scoped = context.register_request(session, "gated".into());
        assert!(matches!(
            scoped.emit_bus(session, "event", json!({})).await,
            Err(PluginError::PublishingNotEnabled("bus"))
        ));
        assert_eq!(scoped.consume(session).unwrap(), "gated");
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn synchronously_panicking_tool_and_intercept_handlers_return_internal_errors() {
        let server = PluginServer::builder("panic", "0.1.0")
            .tool(declaration(), synchronously_panicking_tool)
            .tool_before_call(synchronously_panicking_intercept)
            .build()
            .unwrap();
        let (engine_side, plugin_side) = tokio::io::duplex(64 * 1024);
        let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
        let server_task = tokio::spawn(server.run_io(plugin_read, plugin_write));
        let (engine_read, mut engine_write) = tokio::io::split(engine_side);
        let mut engine_read = BufReader::new(engine_read);
        write_wire(&mut engine_write, &extension_initialize_request("test")).await;
        let _ = read_wire(&mut engine_read).await;
        let call = Request::new(
            JsonRpcId::Number(2),
            PLUGIN_TOOLS_CALL_METHOD,
            Some(json!({
                "tool": "echo",
                "session_id": SessionId::new_v7(),
                "context_id": "panic-context",
                "invocation_id": ToolCallId::new_v7(),
                "arguments": {},
                "resource": null
            })),
        );
        write_wire(&mut engine_write, &call).await;
        let response = read_wire(&mut engine_read).await;
        assert_eq!(response["error"]["code"], INTERNAL_ERROR);

        let intercept = Request::new(
            JsonRpcId::Number(3),
            PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD,
            Some(json!({
                "session_id": SessionId::new_v7(),
                "context_id": "intercept-panic-context",
                "tool": "echo",
                "arguments": {},
                "permission_name": "echo",
                "resource": null
            })),
        );
        write_wire(&mut engine_write, &intercept).await;
        let response = read_wire(&mut engine_read).await;
        assert_eq!(response["error"]["code"], INTERNAL_ERROR);
        write_wire(&mut engine_write, &extension_shutdown_notification()).await;
        server_task.await.unwrap().unwrap();
    }

    fn synchronously_panicking_tool(
        _context: PluginContext,
        _request: ExtensionToolCallParams,
    ) -> std::future::Ready<Result<ToolOutput, ToolFailure>> {
        panic!("synchronous tool panic")
    }

    fn synchronously_panicking_intercept(
        _context: PluginContext,
        _request: ExtensionToolBeforeCallParams,
    ) -> std::future::Ready<ExtensionToolBeforeCallResult> {
        panic!("synchronous intercept panic")
    }

    #[test]
    fn helper_results_have_protocol_actions() {
        assert_eq!(allow().action, ExtensionToolBeforeCallAction::Allow);
        assert_eq!(
            replace("new").action,
            ExtensionToolAfterResultAction::Replace
        );
    }

    async fn write_wire<W: AsyncWrite + Unpin>(writer: &mut W, value: &impl Serialize) {
        let mut bytes = serde_json::to_vec(value).unwrap();
        bytes.push(b'\n');
        writer.write_all(&bytes).await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn read_wire<R: tokio::io::AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn tool_call_request(id: i64, session_id: SessionId, context_id: &str, text: &str) -> Request {
        Request::new(
            JsonRpcId::Number(id),
            PLUGIN_TOOLS_CALL_METHOD,
            Some(
                serde_json::to_value(ExtensionToolCallParams {
                    tool: "echo".into(),
                    session_id,
                    context_id: context_id.into(),
                    invocation_id: ToolCallId::new_v7(),
                    arguments: json!({"text": text}),
                    resource: None,
                    cancellation_token: None,
                })
                .unwrap(),
            ),
        )
    }

    #[allow(dead_code)]
    fn initialize_params() -> ExtensionInitializeParams {
        ExtensionInitializeParams {
            protocol_version: ExtensionProtocolVersion::current(),
            engine_version: "test".into(),
            capabilities: ExtensionEngineCapabilities {
                ping: true,
                shutdown: true,
                tools: true,
                event_streaming: true,
                event_publishing: true,
                interception: true,
            },
        }
    }
}
