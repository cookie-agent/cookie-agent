use std::{
    collections::{BTreeMap, HashMap, HashSet},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use cookie_agent_config::PluginConfig;
use cookie_agent_protocol::{
    ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, EXTENSION_PROTOCOL_VERSION,
    ErrorResponse, ExtensionInitializeResult, ExtensionPingParams, ExtensionPingResult,
    ExtensionToolCallParams, ExtensionToolCallResult, ExtensionToolDeclaration, JsonRpcError,
    JsonRpcId, JsonRpcVersion, Notification, PLUGIN_PING_METHOD, PLUGIN_TOOLS_CALL_METHOD,
    PermissionAction, PreparedApprovalResource, PreparedBindingLifetime,
    PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
    PreparedResourceIdentity, Request, Response, SafeDisplayText, Sha256Digest, SuccessResponse,
    extension_initialize_request, extension_shutdown_notification,
};
use futures_util::StreamExt as _;
use oven_sdk::JsonSchema;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use serde_json::Value;
use tokio::{
    io::AsyncWriteExt as _,
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::codec::{FramedRead, LinesCodec};

use crate::tool_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec,
};

const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginState {
    Disabled,
    Disconnected,
    Connecting,
    Connected,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct PluginStatus {
    pub plugin: String,
    pub state: PluginState,
    pub reason: Option<String>,
    pub tools: Vec<String>,
}

struct PluginRuntime {
    name: String,
    config: PluginConfig,
    status: Mutex<PluginStatus>,
    control: Mutex<Option<mpsc::Sender<Control>>>,
    declarations: Mutex<Vec<ExtensionToolDeclaration>>,
    forced_failure: Mutex<Option<String>>,
    forced_failure_notify: Notify,
    ready: Arc<Notify>,
    mcp: Arc<crate::McpRegistry>,
}

enum Control {
    Ping(oneshot::Sender<Result<(), String>>),
    ToolCall {
        params: ExtensionToolCallParams,
        reply: oneshot::Sender<Result<ExtensionToolCallResult, String>>,
    },
    Shutdown,
}

enum ReaderEvent {
    Message(String),
    Failed(String),
    Eof,
}

enum PendingRequest {
    Initialize {
        deadline: Instant,
    },
    Ping {
        deadline: Instant,
        reply: oneshot::Sender<Result<(), String>>,
    },
    ToolCall {
        deadline: Instant,
        reply: oneshot::Sender<Result<ExtensionToolCallResult, String>>,
    },
}

impl PendingRequest {
    fn deadline(&self) -> Instant {
        match self {
            Self::Initialize { deadline }
            | Self::Ping { deadline, .. }
            | Self::ToolCall { deadline, .. } => *deadline,
        }
    }
}

struct PluginRegistryInner {
    plugins: BTreeMap<String, Arc<PluginRuntime>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    ready: Arc<Notify>,
}

#[derive(Clone)]
pub struct PluginRegistry {
    inner: Arc<PluginRegistryInner>,
}

impl std::fmt::Debug for PluginRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginRegistry")
            .finish_non_exhaustive()
    }
}

impl PluginRegistry {
    pub(crate) fn new(
        plugins: BTreeMap<String, PluginConfig>,
        mcp: Arc<crate::McpRegistry>,
    ) -> Self {
        let ready = Arc::new(Notify::new());
        let plugins = plugins
            .into_iter()
            .map(|(name, config)| {
                let state = if config.enabled {
                    PluginState::Disconnected
                } else {
                    PluginState::Disabled
                };
                let runtime = Arc::new(PluginRuntime {
                    name: name.clone(),
                    config,
                    status: Mutex::new(PluginStatus {
                        plugin: name.clone(),
                        state,
                        reason: None,
                        tools: Vec::new(),
                    }),
                    control: Mutex::new(None),
                    declarations: Mutex::new(Vec::new()),
                    forced_failure: Mutex::new(None),
                    forced_failure_notify: Notify::new(),
                    ready: Arc::clone(&ready),
                    mcp: Arc::clone(&mcp),
                });
                (name, runtime)
            })
            .collect();
        let inner = Arc::new(PluginRegistryInner {
            plugins,
            tasks: Mutex::new(Vec::new()),
            ready,
        });
        let weak = Arc::downgrade(&inner);
        mcp.set_plugin_collision_handler(Arc::new(move |plugin, tool, replacement| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let Some(runtime) = inner.plugins.get(plugin) else {
                return;
            };
            if let Some(replacement) = replacement {
                let mut status = runtime
                    .status
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                status.tools.retain(|name| name != tool);
                status.reason = Some(format!(
                    "tool `{tool}` was replaced by later plugin `{replacement}`"
                ));
                return;
            }
            *runtime
                .forced_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(format!(
                "plugin `{plugin}` tool `{tool}` was preempted by MCP"
            ));
            runtime.forced_failure_notify.notify_one();
        }));
        Self { inner }
    }

    pub(crate) fn start_eager(&self, runtime: &tokio::runtime::Handle) {
        for plugin in self
            .inner
            .plugins
            .values()
            .filter(|plugin| plugin.config.enabled)
        {
            let plugin = Arc::clone(plugin);
            let (control, receiver) = mpsc::channel(8);
            *plugin
                .control
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(control);
            let task = runtime.spawn(async move {
                plugin.run(receiver).await;
            });
            self.inner
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(task);
        }
    }

    pub(crate) async fn await_eager_ready(&self) {
        loop {
            let notified = self.inner.ready.notified();
            let ready = self.inner.plugins.values().all(|plugin| {
                !plugin.config.enabled
                    || matches!(
                        plugin
                            .status
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .state,
                        PluginState::Connected | PluginState::Failed
                    )
            });
            if ready {
                return;
            }
            notified.await;
        }
    }

    #[must_use]
    pub fn statuses(&self) -> Vec<PluginStatus> {
        self.inner
            .plugins
            .values()
            .map(|plugin| {
                plugin
                    .status
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            })
            .collect()
    }

    pub async fn ping(&self, name: &str) -> Result<(), String> {
        let plugin = self
            .inner
            .plugins
            .get(name)
            .ok_or_else(|| format!("unknown plugin `{name}`"))?;
        let sender = plugin
            .control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| format!("plugin `{name}` is not running"))?;
        let (reply, receive) = oneshot::channel();
        sender
            .send(Control::Ping(reply))
            .await
            .map_err(|_| format!("plugin `{name}` is not running"))?;
        receive
            .await
            .map_err(|_| format!("plugin `{name}` stopped during ping"))?
    }

    fn resolve_tool(
        &self,
        tool: &str,
    ) -> Result<(Arc<PluginRuntime>, ExtensionToolDeclaration), ToolError> {
        for runtime in self.inner.plugins.values() {
            if !runtime.mcp.plugin_owns_tool(&runtime.name, tool) {
                continue;
            }
            let declaration = runtime
                .declarations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .find(|declaration| declaration.name == tool)
                .cloned();
            if let Some(declaration) = declaration {
                return Ok((Arc::clone(runtime), declaration));
            }
        }
        Err(ToolError::execution(format!(
            "plugin tool `{tool}` is no longer available"
        )))
    }

    async fn call_tool(
        &self,
        plugin: &str,
        params: ExtensionToolCallParams,
    ) -> Result<ExtensionToolCallResult, ToolError> {
        let (runtime, _) = self.resolve_tool(&params.tool)?;
        if runtime.name != plugin {
            return Err(ToolError::operation_changed(
                "plugin tool ownership changed after approval",
            ));
        }
        let sender = runtime
            .control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| ToolError::execution(format!("plugin `{plugin}` is not running")))?;
        let (reply, receive) = oneshot::channel();
        sender
            .send(Control::ToolCall { params, reply })
            .await
            .map_err(|_| ToolError::execution(format!("plugin `{plugin}` is not running")))?;
        receive
            .await
            .map_err(|_| {
                ToolError::execution(format!("plugin `{plugin}` stopped during tool call"))
            })?
            .map_err(ToolError::execution)
    }

    pub async fn shutdown(&self) {
        let task_timeout = self
            .inner
            .plugins
            .values()
            .map(|plugin| plugin.config.shutdown_grace_ms)
            .max()
            .unwrap_or(0)
            .saturating_add(6_000);
        let senders = self
            .inner
            .plugins
            .values()
            .filter_map(|plugin| {
                plugin
                    .control
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            })
            .collect::<Vec<_>>();
        for sender in senders {
            let _ = sender.send(Control::Shutdown).await;
        }
        let tasks = self
            .inner
            .tasks
            .lock()
            .map(|mut tasks| tasks.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for mut task in tasks {
            if tokio::time::timeout(Duration::from_millis(task_timeout), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

impl PluginRuntime {
    async fn run(self: Arc<Self>, receiver: mpsc::Receiver<Control>) {
        self.set_status(PluginState::Connecting, None, Vec::new());
        let result = self.spawn_and_supervise(receiver).await;
        *self
            .control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.declarations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.mcp.release_plugin_tools(&self.name);
        match result {
            Ok(()) => self.set_status(PluginState::Disconnected, None, Vec::new()),
            Err(reason) => self.set_status(PluginState::Failed, Some(reason), Vec::new()),
        }
    }

    async fn spawn_and_supervise(&self, receiver: mpsc::Receiver<Control>) -> Result<(), String> {
        let command = self
            .config
            .command
            .as_ref()
            .expect("enabled plugins are validated");
        let mut process = tokio::process::Command::new(command);
        process
            .args(&self.config.args)
            .env_clear()
            .envs(&self.config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(cwd) = &self.config.cwd {
            process.current_dir(cwd);
        }
        let mut wrapped = CommandWrap::from(process);
        #[cfg(unix)]
        wrapped.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        wrapped.wrap(JobObject);
        wrapped.wrap(KillOnDrop);
        let mut child = wrapped
            .spawn()
            .map_err(|error| format!("spawn failure: {error}"))?;
        let result = self.supervise_spawned(&mut child, receiver).await;
        let cleanup = terminate_and_reap(&mut child).await;
        match (result, cleanup) {
            (Err(reason), Err(cleanup)) => Err(format!("{reason}; cleanup failed: {cleanup}")),
            (result, Ok(())) => result,
            (Ok(()), Err(cleanup)) => Err(format!("cleanup failed: {cleanup}")),
        }
    }

    async fn supervise_spawned(
        &self,
        child: &mut Box<dyn ChildWrapper>,
        receiver: mpsc::Receiver<Control>,
    ) -> Result<(), String> {
        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| "spawn failure: child stdin unavailable".to_owned())?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| "spawn failure: child stdout unavailable".to_owned())?;
        let (reader_task, inbound) = spawn_reader(stdout);
        let result = self.run_host_loop(child, stdin, receiver, inbound).await;
        reader_task.abort();
        let _ = reader_task.await;
        result
    }

    async fn run_host_loop(
        &self,
        child: &mut Box<dyn ChildWrapper>,
        mut stdin: tokio::process::ChildStdin,
        mut control: mpsc::Receiver<Control>,
        mut inbound: mpsc::Receiver<ReaderEvent>,
    ) -> Result<(), String> {
        write_json(
            &mut stdin,
            &extension_initialize_request(env!("CARGO_PKG_VERSION")),
        )
        .await?;
        let mut pending = HashMap::from([(
            1_i64,
            PendingRequest::Initialize {
                deadline: Instant::now() + Duration::from_millis(self.config.startup_timeout_ms),
            },
        )]);
        let mut next_request_id = 2_i64;
        let mut connected = false;

        loop {
            if let Some(reason) = self
                .forced_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                return Err(reason);
            }
            let deadline = pending.values().map(PendingRequest::deadline).min();
            tokio::select! {
                status = child.wait() => {
                    return Err(match status {
                        Ok(status) => format!("process exited with {status}"),
                        Err(error) => format!("process wait failed: {error}"),
                    });
                }
                event = inbound.recv() => match event {
                    Some(ReaderEvent::Message(message)) => {
                        self.dispatch_message(
                            &message,
                            &mut stdin,
                            &mut pending,
                            &mut connected,
                        ).await?;
                    }
                    Some(ReaderEvent::Failed(reason)) => return Err(reason),
                    Some(ReaderEvent::Eof) | None => return Err("plugin stdout reached EOF".into()),
                },
                command = control.recv() => match command {
                    Some(Control::Ping(reply)) if connected => {
                        let id = next_request_id;
                        next_request_id = next_request_id.checked_add(1)
                            .ok_or_else(|| "plugin request id space exhausted".to_owned())?;
                        let request = Request::new(
                            JsonRpcId::Number(id),
                            PLUGIN_PING_METHOD,
                            Some(serde_json::to_value(ExtensionPingParams {})
                                .expect("ping params serialize")),
                        );
                        write_json(&mut stdin, &request).await?;
                        pending.insert(id, PendingRequest::Ping {
                            deadline: Instant::now()
                                + Duration::from_millis(self.config.interception_timeout_ms),
                            reply,
                        });
                    }
                    Some(Control::Ping(reply)) => {
                        let _ = reply.send(Err(format!("plugin `{}` is not connected", self.name)));
                    }
                    Some(Control::ToolCall { params, reply }) if connected => {
                        let id = next_request_id;
                        next_request_id = next_request_id.checked_add(1)
                            .ok_or_else(|| "plugin request id space exhausted".to_owned())?;
                        let request = Request::new(
                            JsonRpcId::Number(id),
                            PLUGIN_TOOLS_CALL_METHOD,
                            Some(serde_json::to_value(params)
                                .expect("plugin tool call params serialize")),
                        );
                        write_json(&mut stdin, &request).await?;
                        pending.insert(id, PendingRequest::ToolCall {
                            deadline: Instant::now()
                                + Duration::from_millis(self.config.tool_timeout_ms),
                            reply,
                        });
                    }
                    Some(Control::ToolCall { reply, .. }) => {
                        let _ = reply.send(Err(format!("plugin `{}` is not connected", self.name)));
                    }
                    Some(Control::Shutdown) | None => {
                        return graceful_shutdown(
                            child,
                            &mut stdin,
                            self.config.shutdown_grace_ms,
                        ).await;
                    }
                },
                () = self.forced_failure_notify.notified() => {}
                () = wait_for_deadline(deadline) => {
                    let now = Instant::now();
                    let expired = pending
                        .iter()
                        .filter_map(|(id, request)| (request.deadline() <= now).then_some(*id))
                        .collect::<Vec<_>>();
                    for id in expired {
                        match pending.remove(&id).expect("expired pending request exists") {
                            PendingRequest::Initialize { .. } => return Err("handshake timeout".into()),
                            PendingRequest::Ping { reply, .. } => {
                                let _ = reply.send(Err("plugin ping timed out".into()));
                            }
                            PendingRequest::ToolCall { reply, .. } => {
                                let _ = reply.send(Err("plugin tool call timed out".into()));
                            }
                        }
                    }
                }
            }
        }
    }

    async fn dispatch_message(
        &self,
        message: &str,
        stdin: &mut tokio::process::ChildStdin,
        pending: &mut HashMap<i64, PendingRequest>,
        connected: &mut bool,
    ) -> Result<(), String> {
        let value: Value = serde_json::from_str(message)
            .map_err(|error| format!("malformed plugin message: {error}"))?;
        if value.get("method").is_some() {
            if value.get("id").is_some() {
                let request: Request = serde_json::from_value(value)
                    .map_err(|error| format!("malformed plugin request: {error}"))?;
                let response = ErrorResponse {
                    jsonrpc: JsonRpcVersion::current(),
                    id: request.id,
                    error: JsonRpcError {
                        code: -32601,
                        message: "plugin method is reserved but not supported by this engine stage"
                            .into(),
                        data: None,
                    },
                };
                write_json(stdin, &response).await?;
            } else {
                serde_json::from_value::<Notification>(value)
                    .map_err(|error| format!("malformed plugin notification: {error}"))?;
            }
            return Ok(());
        }

        let response: Response = serde_json::from_value(value)
            .map_err(|error| format!("malformed plugin response: {error}"))?;
        let id = match &response {
            Response::Success(response) => &response.id,
            Response::Error(response) => &response.id,
        };
        let JsonRpcId::Number(id) = id else {
            return Ok(());
        };
        let Some(request) = pending.remove(id) else {
            return Ok(());
        };
        match request {
            PendingRequest::Initialize { .. } => {
                let initialize = parse_initialize(response, &self.name)?;
                validate_tools(&initialize)?;
                let names = initialize
                    .tools
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect::<Vec<_>>();
                self.mcp
                    .claim_plugin_tools(&self.name, &names)
                    .map_err(|error| error.to_string())?;
                *self
                    .declarations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = initialize.tools;
                self.set_status(PluginState::Connected, None, names);
                *connected = true;
                Ok(())
            }
            PendingRequest::Ping { reply, .. } => {
                let result = parse_ping(response);
                let _ = reply.send(result);
                Ok(())
            }
            PendingRequest::ToolCall { reply, .. } => {
                let result = parse_tool_call(response);
                let _ = reply.send(result);
                Ok(())
            }
        }
    }

    fn set_status(&self, state: PluginState, reason: Option<String>, tools: Vec<String>) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        status.state = state;
        status.reason = reason;
        status.tools = tools;
        drop(status);
        self.ready.notify_waiters();
    }
}

fn spawn_reader(
    stdout: tokio::process::ChildStdout,
) -> (JoinHandle<()>, mpsc::Receiver<ReaderEvent>) {
    let (sender, receiver) = mpsc::channel(32);
    let task = tokio::spawn(async move {
        let mut frames = FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_FRAME_BYTES));
        while let Some(frame) = frames.next().await {
            let event = match frame {
                Ok(message) => ReaderEvent::Message(message),
                Err(error) => ReaderEvent::Failed(format!(
                    "plugin stdout frame is invalid UTF-8 or exceeds {MAX_FRAME_BYTES} bytes: {error}"
                )),
            };
            let failed = matches!(event, ReaderEvent::Failed(_));
            if sender.send(event).await.is_err() || failed {
                return;
            }
        }
        let _ = sender.send(ReaderEvent::Eof).await;
    });
    (task, receiver)
}

fn parse_initialize(
    response: Response,
    expected_name: &str,
) -> Result<ExtensionInitializeResult, String> {
    let success = match response {
        Response::Success(success) => success,
        Response::Error(error) => {
            return Err(format!("handshake rejected: {}", error.error.message));
        }
    };
    let found_version = success
        .result
        .get("protocol_version")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "malformed handshake result: protocol_version must be a string".to_owned()
        })?;
    if found_version != EXTENSION_PROTOCOL_VERSION {
        return Err(format!(
            "version mismatch: expected {EXTENSION_PROTOCOL_VERSION}, found {found_version}"
        ));
    }
    let result: ExtensionInitializeResult = serde_json::from_value(success.result)
        .map_err(|error| format!("malformed handshake result: {error}"))?;
    if result.name != expected_name {
        return Err(format!(
            "plugin name mismatch: configured `{expected_name}`, reported `{}`",
            result.name
        ));
    }
    Ok(result)
}

fn parse_ping(response: Response) -> Result<(), String> {
    match response {
        Response::Success(SuccessResponse { result, .. }) => {
            serde_json::from_value::<ExtensionPingResult>(result)
                .map_err(|error| format!("malformed plugin ping result: {error}"))?;
            Ok(())
        }
        Response::Error(error) => Err(format!("plugin ping rejected: {}", error.error.message)),
    }
}

fn parse_tool_call(response: Response) -> Result<ExtensionToolCallResult, String> {
    match response {
        Response::Success(SuccessResponse { result, .. }) => serde_json::from_value(result)
            .map_err(|error| format!("malformed plugin tool call result: {error}")),
        Response::Error(error) => Err(format!(
            "plugin tool call rejected: {}",
            error.error.message
        )),
    }
}

#[async_trait]
impl ToolProvider for PluginRegistry {
    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let mut tools = Vec::new();
        for runtime in self.inner.plugins.values() {
            let declarations = runtime
                .declarations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            tools.extend(
                declarations
                    .into_iter()
                    .filter(|tool| runtime.mcp.plugin_owns_tool(&runtime.name, &tool.name))
                    .map(|tool| ToolSpec {
                        name: tool.name,
                        permission_name: format!("plugin:{}", tool.permission_name),
                        description: tool.description,
                        parameters: tool.parameters,
                    }),
            );
        }
        Ok(tools)
    }

    fn get_permission_name(_tool_name: &str) -> Result<&'static str, ToolError> {
        Ok("plugin")
    }

    fn get_permission_resource(
        &self,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let (_, declaration) = self.resolve_tool(tool_name)?;
        let primary = primary_resource(&declaration, arguments)?;
        Ok((
            "plugin",
            Some(permission_resource(
                &declaration.permission_name,
                primary.as_deref(),
            )),
        ))
    }

    fn get_display_argument(&self, name: &str, arguments: &Value) -> Result<String, ToolError> {
        let (_, declaration) = self.resolve_tool(name)?;
        Ok(primary_resource(&declaration, arguments)?.unwrap_or_else(|| name.to_owned()))
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let (runtime, declaration) = self.resolve_tool(&call.name)?;
        if !call.arguments.is_object() {
            return Err(ToolError::execution(
                "plugin tool arguments must be an object",
            ));
        }
        let normalized = call.arguments.clone();
        let argument_bytes = serde_json::to_vec(&normalized)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let primary = primary_resource(&declaration, &normalized)?;
        let permission_label =
            permission_resource(&declaration.permission_name, primary.as_deref());
        let resource = PreparedApprovalResource {
            capability: PermissionAction::Plugin,
            canonical: PreparedResourceIdentity::new(format!(
                "plugin-tool:{}",
                Sha256Digest::of_bytes(permission_label.as_bytes())
            ))
            .map_err(|error| ToolError::execution(error.to_string()))?,
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                permission_label.as_bytes(),
            ),
            binding_lifetime: PreparedBindingLifetime::RestartStable,
            boundary: ApprovalBoundary::Exact,
            source: ApprovalResourceSource::PrimaryOperation,
        };
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(&argument_bytes),
            vec![ApprovalCapability {
                action: PermissionAction::Plugin,
                operation: PreparedCapabilityOperation::new(format!(
                    "{}:call",
                    declaration.permission_name
                ))
                .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![resource],
            Sha256Digest::of_bytes(ctx.workspace_root.to_string_lossy().as_bytes()),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            normalized,
            None,
            Box::new(PluginExecutor {
                registry: self.clone(),
                plugin: runtime.name.clone(),
                declaration,
                session: ctx.session,
                call,
                resource: primary,
                timeout: Duration::from_millis(runtime.config.tool_timeout_ms),
            }),
        )
    }
}

struct PluginExecutor {
    registry: PluginRegistry,
    plugin: String,
    declaration: ExtensionToolDeclaration,
    session: cookie_agent_protocol::SessionId,
    call: ToolCall,
    resource: Option<String>,
    timeout: Duration,
}

#[async_trait]
impl PreparedExecutor for PluginExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        let (runtime, declaration) = self.registry.resolve_tool(&self.call.name)?;
        if runtime.name != self.plugin || declaration != self.declaration {
            return Err(ToolError::operation_changed(
                "plugin tool changed after approval",
            ));
        }
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_protocol::PersistedToolResult, ToolError> {
        self.revalidate().await?;
        let params = ExtensionToolCallParams {
            tool: self.call.name.clone(),
            session_id: self.session,
            invocation_id: self.call.id,
            arguments: self.call.arguments.clone(),
            resource: self.resource.clone(),
            cancellation_token: Some(self.call.id.to_string()),
        };
        let call = self.registry.call_tool(&self.plugin, params);
        tokio::pin!(call);
        let result = tokio::select! {
            result = &mut call => result,
            () = context.cancellation.cancelled() => {
                Err(ToolError::execution("plugin tool call cancelled"))
            }
            () = tokio::time::sleep(self.timeout) => {
                Err(ToolError::execution("plugin tool call timed out"))
            }
        }?;
        Ok(cookie_agent_protocol::PersistedToolResult {
            title: safe_title(&self.call.name),
            output: result.content.clone(),
            metadata: serde_json::json!({
                "plugin": {
                    "content": result.content,
                    "is_error": result.is_error,
                }
            }),
            truncation: None,
            attachments: Vec::new(),
        })
    }
}

fn primary_resource(
    declaration: &ExtensionToolDeclaration,
    arguments: &Value,
) -> Result<Option<String>, ToolError> {
    let Some(parameter) = &declaration.primary_resource_param else {
        return Ok(None);
    };
    let value = arguments
        .as_object()
        .and_then(|arguments| arguments.get(parameter));
    value
        .map(|value| match value {
            Value::String(value) => Ok(value.clone()),
            value => serde_json::to_string(value)
                .map_err(|error| ToolError::execution(error.to_string())),
        })
        .transpose()
}

fn permission_resource(permission_name: &str, primary: Option<&str>) -> String {
    primary.map_or_else(
        || permission_name.to_owned(),
        |primary| format!("{permission_name} {primary}"),
    )
}

fn safe_title(value: &str) -> SafeDisplayText {
    let mut title = String::new();
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if title.len() + character.len_utf8() > SafeDisplayText::MAX_BYTES {
            break;
        }
        title.push(character);
    }
    SafeDisplayText::new(title).expect("sanitized plugin title")
}

fn validate_tools(initialize: &ExtensionInitializeResult) -> Result<(), String> {
    let mut names = HashSet::new();
    for tool in &initialize.tools {
        if !is_snake_case(&tool.name) || !names.insert(&tool.name) {
            return Err(format!(
                "plugin declared invalid or duplicate tool name `{}`",
                tool.name
            ));
        }
        if tool.description.is_empty() || !is_snake_case(&tool.permission_name) {
            return Err(format!("plugin tool `{}` is malformed", tool.name));
        }
        let schema = JsonSchema::new(tool.parameters.clone()).map_err(|error| {
            format!(
                "plugin tool `{}` has invalid JSON Schema: {error}",
                tool.name
            )
        })?;
        jsonschema::draft202012::meta::validate(schema.as_value()).map_err(|error| {
            format!(
                "plugin tool `{}` has invalid JSON Schema: {error}",
                tool.name
            )
        })?;
        let properties = schema
            .as_value()
            .as_object()
            .and_then(|schema| schema.get("properties"))
            .and_then(Value::as_object);
        if let Some(parameter) = &tool.primary_resource_param
            && !properties.is_some_and(|properties| properties.contains_key(parameter))
        {
            return Err(format!(
                "plugin tool `{}` primary_resource_param `{parameter}` is not declared in properties",
                tool.name
            ));
        }
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

async fn write_json(
    stdin: &mut tokio::process::ChildStdin,
    message: &impl serde::Serialize,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(message).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    stdin
        .write_all(&bytes)
        .await
        .map_err(|error| format!("plugin write failed: {error}"))?;
    stdin
        .flush()
        .await
        .map_err(|error| format!("plugin flush failed: {error}"))
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn graceful_shutdown(
    child: &mut Box<dyn ChildWrapper>,
    stdin: &mut tokio::process::ChildStdin,
    grace_ms: u64,
) -> Result<(), String> {
    let notification: Notification = extension_shutdown_notification();
    let _ = write_json(stdin, &notification).await;
    let _ = stdin.shutdown().await;
    match tokio::time::timeout(Duration::from_millis(grace_ms), child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("plugin wait failed during shutdown: {error}")),
        Err(_) => Err("plugin shutdown grace period expired".into()),
    }
}

async fn terminate_and_reap(child: &mut Box<dyn ChildWrapper>) -> Result<(), String> {
    let status_error = match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => None,
        Err(error) => Some(error),
    };
    let kill_error = child.start_kill().err();
    let wait = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .map_err(|_| "timed out waiting to reap plugin".to_owned())?
        .map_err(|error| format!("plugin reap failed: {error}"));
    if let Some(error) = status_error {
        return Err(format!("plugin status check failed before reap: {error}"));
    }
    if let Some(error) = kill_error {
        return Err(format!("plugin termination failed: {error}"));
    }
    wait.map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc, time::Duration};

    use cookie_agent_config::PluginConfig;
    use cookie_agent_protocol::{
        AgentId, CancellationCapability, Modality, ModelCapabilities, ReplayCapability, RunId,
        SessionId, ToolCallId,
    };
    use tokio_util::sync::CancellationToken;

    use super::{PluginRegistry, PluginState};
    use crate::{
        ArtifactStore,
        events::OutputHub,
        tool_api::{
            ProgressSink, ToolCall, ToolExecutionContext, ToolPreparationContext, ToolProvider,
            TurnAgentContext,
        },
    };

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_plugin.py");
    const DECLARATION: &str = r#"[{"name":"fixture_echo","description":"Echo","parameters":{"type":"object","properties":{"text":{"type":"string"},"path":{"type":"string"}}},"permission_name":"fixture_echo","primary_resource_param":"path"}]"#;

    struct Harness {
        directory: tempfile::TempDir,
        registry: PluginRegistry,
    }

    async fn harness(extra_env: &[(&str, &str)], timeout_ms: u64) -> Harness {
        let directory = tempfile::tempdir().expect("plugin test directory");
        let mcp = Arc::new(
            crate::McpRegistry::new(BTreeMap::new(), directory.path().join("oauth.json"))
                .expect("MCP registry"),
        );
        let mut env = BTreeMap::from([
            ("FIXTURE_NAME".into(), "fixture".into()),
            ("FIXTURE_TOOLS".into(), DECLARATION.into()),
        ]);
        env.extend(
            extra_env
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        let registry = PluginRegistry::new(
            BTreeMap::from([(
                "fixture".into(),
                PluginConfig {
                    command: Some("python3".into()),
                    args: vec![FIXTURE.into()],
                    env,
                    cwd: None,
                    enabled: true,
                    interception_timeout_ms: 2_000,
                    startup_timeout_ms: 10_000,
                    shutdown_grace_ms: 3_000,
                    tool_timeout_ms: timeout_ms,
                },
            )]),
            mcp,
        );
        registry.start_eager(&tokio::runtime::Handle::current());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if registry
                    .statuses()
                    .iter()
                    .any(|status| status.state == PluginState::Connected)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("plugin connected");
        Harness {
            directory,
            registry,
        }
    }

    fn turn_context() -> Arc<TurnAgentContext> {
        Arc::new(TurnAgentContext {
            agent: AgentId::new("test").expect("agent ID"),
            capabilities: ModelCapabilities {
                input: [Modality::Text].into_iter().collect(),
                output: [Modality::Text].into_iter().collect(),
                context_tokens: 8_192,
                output_tokens: 2_048,
                tool_calling: true,
                parallel_tool_calls: true,
                structured_output: false,
                reasoning: false,
                temperature: true,
                top_p: true,
                seed: false,
                native_replay: ReplayCapability::Optional,
                cancellation: CancellationCapability::LocalOnly,
                media: BTreeMap::new(),
            },
        })
    }

    async fn prepared(harness: &Harness) -> crate::PreparedTool {
        harness
            .registry
            .prepare(
                ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: harness.directory.path().into(),
                    workspace_root: harness.directory.path().into(),
                    turn_context: turn_context(),
                },
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "fixture_echo".into(),
                    arguments: serde_json::json!({"text":"hello", "path":"src/lib.rs"}),
                },
            )
            .await
            .expect("prepare plugin call")
    }

    async fn execute(
        harness: &Harness,
        prepared: crate::PreparedTool,
        cancellation: CancellationToken,
    ) -> Result<cookie_agent_protocol::PersistedToolResult, crate::ToolError> {
        let call_id = ToolCallId::new_v7();
        let executor = prepared
            .executor
            .lock()
            .await
            .take()
            .expect("prepared executor");
        let (progress, _receiver) = tokio::sync::mpsc::channel(1);
        executor
            .execute(ToolExecutionContext {
                session: SessionId::new_v7(),
                run: RunId::new_v7(),
                progress: ProgressSink::new(progress, OutputHub::new(call_id, 1024)),
                cancellation,
                stdin: None,
                turn_context: turn_context(),
                artifacts: ArtifactStore::open(harness.directory.path().join("artifacts"))
                    .expect("artifact store"),
            })
            .await
    }

    #[tokio::test]
    async fn plugin_executor_maps_success_and_error_results() {
        let success = harness(&[], 1_000).await;
        let arguments = serde_json::json!({"text":"hello", "path":"src/lib.rs"});
        assert_eq!(
            success
                .registry
                .get_display_argument("fixture_echo", &arguments)
                .expect("display argument"),
            "src/lib.rs"
        );
        assert_eq!(
            success
                .registry
                .get_permission_resource("fixture_echo", &arguments)
                .expect("permission resource"),
            ("plugin", Some("fixture_echo src/lib.rs".into()))
        );
        let result = execute(&success, prepared(&success).await, CancellationToken::new())
            .await
            .expect("plugin result");
        assert_eq!(result.output, "hello");
        assert_eq!(result.metadata["plugin"]["is_error"], false);
        success.registry.shutdown().await;

        let error = harness(&[("FIXTURE_TOOL_ERROR", "1")], 1_000).await;
        let result = execute(&error, prepared(&error).await, CancellationToken::new())
            .await
            .expect("plugin error result");
        assert_eq!(result.metadata["plugin"]["is_error"], true);
        error.registry.shutdown().await;
    }

    #[tokio::test]
    async fn plugin_executor_maps_rpc_error_timeout_and_cancellation() {
        let rpc = harness(&[("FIXTURE_TOOL_RPC_ERROR", "1")], 1_000).await;
        let error = execute(&rpc, prepared(&rpc).await, CancellationToken::new())
            .await
            .expect_err("RPC error");
        assert!(error.to_string().contains("fixture tool RPC error"));
        rpc.registry.shutdown().await;

        let slow = harness(&[("FIXTURE_TOOL_DELAY_MS", "200")], 20).await;
        let error = execute(&slow, prepared(&slow).await, CancellationToken::new())
            .await
            .expect_err("timeout");
        assert!(error.to_string().contains("timed out"));
        slow.registry.shutdown().await;

        let cancelled = harness(&[("FIXTURE_TOOL_DELAY_MS", "200")], 1_000).await;
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = execute(&cancelled, prepared(&cancelled).await, cancellation)
            .await
            .expect_err("cancellation");
        assert!(error.to_string().contains("cancelled"));
        cancelled.registry.shutdown().await;
    }

    #[tokio::test]
    async fn crash_during_call_invalidates_prepared_tool_and_listing() {
        let harness = harness(&[("FIXTURE_CRASH_DURING_TOOL", "1")], 1_000).await;
        let stale = prepared(&harness).await;
        let error = execute(&harness, prepared(&harness).await, CancellationToken::new())
            .await
            .expect_err("crash");
        assert!(error.to_string().contains("stopped during tool call"));
        tokio::time::timeout(Duration::from_secs(1), async {
            while harness.registry.statuses()[0].state != PluginState::Failed {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("failed state");
        assert!(
            harness
                .registry
                .tools_for_session(&crate::SessionToolContext {
                    session: SessionId::new_v7(),
                })
                .expect("tool listing")
                .is_empty()
        );
        let executor = stale.executor.lock().await.take().expect("stale executor");
        assert!(executor.revalidate().await.is_err());
        harness.registry.shutdown().await;
    }
}
