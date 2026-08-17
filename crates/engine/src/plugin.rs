use std::{
    collections::{BTreeMap, HashMap, HashSet},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use cookie_agent_config::PluginConfig;
use cookie_agent_protocol::{
    EXTENSION_PROTOCOL_VERSION, ErrorResponse, ExtensionInitializeResult, ExtensionPingParams,
    ExtensionPingResult, JsonRpcError, JsonRpcId, JsonRpcVersion, Notification, PLUGIN_PING_METHOD,
    Request, Response, SuccessResponse, extension_initialize_request,
    extension_shutdown_notification,
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
    forced_failure: Mutex<Option<String>>,
    forced_failure_notify: Notify,
    mcp: Arc<crate::McpRegistry>,
}

enum Control {
    Ping(oneshot::Sender<Result<(), String>>),
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
}

impl PendingRequest {
    fn deadline(&self) -> Instant {
        match self {
            Self::Initialize { deadline } | Self::Ping { deadline, .. } => *deadline,
        }
    }
}

struct PluginRegistryInner {
    plugins: BTreeMap<String, Arc<PluginRuntime>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
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
                    forced_failure: Mutex::new(None),
                    forced_failure_notify: Notify::new(),
                    mcp: Arc::clone(&mcp),
                });
                (name, runtime)
            })
            .collect();
        let inner = Arc::new(PluginRegistryInner {
            plugins,
            tasks: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&inner);
        mcp.set_plugin_collision_handler(Arc::new(move |plugin, tool| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let Some(runtime) = inner.plugins.get(plugin) else {
                return;
            };
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
                self.set_status(PluginState::Connected, None, names);
                *connected = true;
                Ok(())
            }
            PendingRequest::Ping { reply, .. } => {
                let result = parse_ping(response);
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
