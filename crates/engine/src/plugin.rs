use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use cookie_agent_config::PluginConfig;
use cookie_agent_protocol::{
    ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, EXTENSION_PROTOCOL_VERSION,
    ErrorResponse, ExtensionBusEventParams, ExtensionEmitParams, ExtensionEmitResultParams,
    ExtensionEmitStatus, ExtensionEventParams, ExtensionInitializeResult,
    ExtensionInterceptionHook, ExtensionPingParams, ExtensionPingResult,
    ExtensionPluginCapabilities, ExtensionToolCallParams, ExtensionToolCallResult,
    ExtensionToolDeclaration, JsonRpcError, JsonRpcId, JsonRpcVersion, Notification,
    PLUGIN_BUS_EVENT_METHOD, PLUGIN_EMIT_METHOD, PLUGIN_EMIT_RESULT_METHOD, PLUGIN_EVENT_METHOD,
    PLUGIN_PING_METHOD, PLUGIN_TOOLS_CALL_METHOD, PermissionAction, PreparedApprovalResource,
    PreparedBindingLifetime, PreparedCapabilityOperation, PreparedOperationIdentity,
    PreparedResourceDigest, PreparedResourceIdentity, Request, Response, SafeDisplayText,
    Sha256Digest, SuccessResponse, extension_initialize_request, extension_shutdown_notification,
};
#[cfg(test)]
use cookie_agent_protocol::{
    ExtensionAgentBeforeStartParams, ExtensionAgentBeforeStartResult,
    ExtensionSessionBeforeCompactParams, ExtensionSessionBeforeCompactResult,
    ExtensionToolAfterResultParams, ExtensionToolAfterResultResult, ExtensionToolBeforeCallParams,
    ExtensionToolBeforeCallResult, PLUGIN_INTERCEPT_AGENT_BEFORE_START_METHOD,
    PLUGIN_INTERCEPT_SESSION_BEFORE_COMPACT_METHOD, PLUGIN_INTERCEPT_TOOL_AFTER_RESULT_METHOD,
    PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD,
};
use futures_util::StreamExt as _;
use indexmap::IndexMap;
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
pub(crate) const MAX_PLUGIN_EVENT_PAYLOAD_BYTES: usize = 256 * 1024;
pub(crate) const MAX_PLUGIN_EVENT_NAME_CHARS: usize = 128;
pub(crate) const MAX_PLUGIN_EVENT_TOTAL_BYTES: usize = 272 * 1024;
const PLUGIN_OUTBOUND_CAPACITY: usize = 1024;
const PLUGIN_NOTIFICATION_CAPACITY: usize = 1024;
const PLUGIN_CONTEXT_CAPACITY: usize = 1024;
const PLUGIN_SPENT_CONTEXT_CAPACITY: usize = 256;
const PLUGIN_NOTIFICATION_CONTEXT_LIFETIME: Duration = Duration::from_secs(5);
const PLUGIN_SPENT_CONTEXT_LIFETIME: Duration = Duration::from_secs(30);
pub(crate) const PLUGIN_EVENTS_PER_SECOND: u32 = 40;
pub(crate) const PLUGIN_BYTES_PER_MINUTE: usize = 4 * 1024 * 1024;

#[derive(Debug)]
struct PublishQuota {
    second_started: Instant,
    minute_started: Instant,
    events_this_second: u32,
    bytes_this_minute: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginEmitRequest {
    pub plugin: String,
    pub session_id: cookie_agent_protocol::SessionId,
    pub context: PluginEmitContext,
    pub name: String,
    pub payload: Value,
    pub publish_bus: bool,
    pub publish_session_events: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum PluginEmitContext {
    Granted,
    Rejected {
        diagnostic_session_id: Option<cookie_agent_protocol::SessionId>,
        reason: String,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct PluginEmitOutcome {
    pub bus: ExtensionEmitStatus,
    pub durable: ExtensionEmitStatus,
    pub reason: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginDeliveryDrop {
    pub plugin: String,
    pub class: PluginDeliveryClass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PluginDeliveryClass {
    Chunk,
    Ordinary,
    Terminal,
}

pub(crate) type PluginEmitHandler = Arc<
    dyn Fn(PluginEmitRequest) -> Pin<Box<dyn Future<Output = PluginEmitOutcome> + Send>>
        + Send
        + Sync,
>;

#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    PluginEvent {
        session_id: cookie_agent_protocol::SessionId,
        plugin: String,
        name: String,
        payload: Value,
    },
}

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
    pub dropped_events: u64,
}

struct PluginRuntime {
    name: String,
    config: PluginConfig,
    status: Mutex<PluginStatus>,
    control: Mutex<Option<mpsc::Sender<Control>>>,
    notifications: Mutex<Arc<PluginNotificationQueue>>,
    declarations: Mutex<Vec<ExtensionToolDeclaration>>,
    capabilities: Mutex<Option<ExtensionPluginCapabilities>>,
    dropped_events: AtomicU64,
    forced_failure: Mutex<Option<String>>,
    forced_failure_notify: Notify,
    ready: Arc<Notify>,
    mcp: Arc<crate::McpRegistry>,
    emit_handler: Arc<Mutex<Option<PluginEmitHandler>>>,
    contexts: Mutex<PluginContexts>,
    publish_quotas: Mutex<HashMap<cookie_agent_protocol::SessionId, PublishQuota>>,
}

#[derive(Debug, Default)]
struct PluginContexts {
    active: VecDeque<PluginContextGrant>,
    spent: VecDeque<PluginSpentContext>,
}

#[derive(Debug)]
struct PluginContextGrant {
    id: String,
    session_id: cookie_agent_protocol::SessionId,
    expires_at: Instant,
}

#[derive(Debug)]
struct PluginSpentContext {
    id: String,
    session_id: cookie_agent_protocol::SessionId,
    expires_at: Instant,
}

struct PluginContextLease {
    runtime: Arc<PluginRuntime>,
    context_id: String,
    session_id: cookie_agent_protocol::SessionId,
}

impl Drop for PluginContextLease {
    fn drop(&mut self) {
        self.runtime
            .revoke_context(&self.context_id, self.session_id);
    }
}

enum Control {
    Ping(oneshot::Sender<Result<(), String>>),
    ToolCall {
        params: ExtensionToolCallParams,
        context_lifetime: Duration,
        reply: oneshot::Sender<Result<ExtensionToolCallResult, String>>,
    },
    Notify {
        notification: Notification,
        session_id: cookie_agent_protocol::SessionId,
        context_id: String,
        context_lifetime: Duration,
    },
    Intercept {
        method: &'static str,
        params: Value,
        session_id: Option<cookie_agent_protocol::SessionId>,
        context_id: Option<String>,
        context_lifetime: Option<Duration>,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Shutdown,
}

struct QueuedPluginNotification {
    control: Control,
    class: PluginDeliveryClass,
}

struct PluginNotificationQueue {
    capacity: usize,
    queued: Mutex<VecDeque<QueuedPluginNotification>>,
    notify: Notify,
}

impl PluginNotificationQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            queued: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        }
    }

    fn push(&self, control: Control, class: PluginDeliveryClass) -> Option<PluginDeliveryClass> {
        let mut queued = self
            .queued
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dropped = if queued.len() < self.capacity {
            None
        } else {
            let index = match class {
                PluginDeliveryClass::Chunk => queued
                    .iter()
                    .position(|item| item.class == PluginDeliveryClass::Chunk),
                PluginDeliveryClass::Ordinary => queued
                    .iter()
                    .position(|item| item.class == PluginDeliveryClass::Chunk)
                    .or_else(|| {
                        queued
                            .iter()
                            .position(|item| item.class == PluginDeliveryClass::Ordinary)
                    }),
                PluginDeliveryClass::Terminal => queued
                    .iter()
                    .position(|item| item.class == PluginDeliveryClass::Chunk)
                    .or_else(|| {
                        queued
                            .iter()
                            .position(|item| item.class == PluginDeliveryClass::Ordinary)
                    })
                    .or_else(|| {
                        queued
                            .iter()
                            .position(|item| item.class == PluginDeliveryClass::Terminal)
                    }),
            };
            let Some(index) = index else {
                return Some(class);
            };
            queued.remove(index).map(|item| item.class)
        };
        queued.push_back(QueuedPluginNotification { control, class });
        drop(queued);
        self.notify.notify_one();
        dropped
    }

    #[cfg(test)]
    fn pop(&self) -> Option<Control> {
        self.queued
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .map(|item| item.control)
    }

    async fn notified(&self) {
        self.notify.notified().await;
    }

    fn drain(&self) -> VecDeque<Control> {
        std::mem::take(
            &mut *self
                .queued
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
        .into_iter()
        .map(|item| item.control)
        .collect()
    }

    fn clear(&self) {
        self.queued
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
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
    Intercept {
        deadline: Instant,
        reply: oneshot::Sender<Result<Value, String>>,
    },
}

impl PendingRequest {
    fn deadline(&self) -> Instant {
        match self {
            Self::Initialize { deadline }
            | Self::Ping { deadline, .. }
            | Self::ToolCall { deadline, .. }
            | Self::Intercept { deadline, .. } => *deadline,
        }
    }
}

struct PluginRegistryInner {
    plugins: IndexMap<String, Arc<PluginRuntime>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    ready: Arc<Notify>,
    emit_handler: Arc<Mutex<Option<PluginEmitHandler>>>,
}

fn record_plugin_delivery_drop(
    runtime: &PluginRuntime,
    drops: &mut Vec<PluginDeliveryDrop>,
    class: PluginDeliveryClass,
) {
    let count = runtime.dropped_events.fetch_add(1, Ordering::AcqRel) + 1;
    runtime
        .status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .dropped_events = count;
    drops.push(PluginDeliveryDrop {
        plugin: runtime.name.clone(),
        class,
    });
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

pub(crate) fn plugin_event_origin(name: &str) -> cookie_agent_protocol::EventOrigin {
    cookie_agent_protocol::EventOrigin::new(format!("plugin:{name}")).unwrap_or_else(|_| {
        cookie_agent_protocol::EventOrigin::new(format!(
            "plugin:{}",
            cookie_agent_protocol::Sha256Digest::of_bytes(name.as_bytes())
        ))
        .expect("SHA-256 digest is a valid event origin slug")
    })
}

impl PluginRegistry {
    pub(crate) fn new(
        plugins: IndexMap<String, PluginConfig>,
        mcp: Arc<crate::McpRegistry>,
    ) -> Self {
        let ready = Arc::new(Notify::new());
        let emit_handler = Arc::new(Mutex::new(None));
        let plugins: IndexMap<String, Arc<PluginRuntime>> = plugins
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
                        dropped_events: 0,
                    }),
                    control: Mutex::new(None),
                    notifications: Mutex::new(Arc::new(PluginNotificationQueue::new(
                        PLUGIN_NOTIFICATION_CAPACITY,
                    ))),
                    declarations: Mutex::new(Vec::new()),
                    capabilities: Mutex::new(None),
                    dropped_events: AtomicU64::new(0),
                    forced_failure: Mutex::new(None),
                    forced_failure_notify: Notify::new(),
                    ready: Arc::clone(&ready),
                    mcp: Arc::clone(&mcp),
                    emit_handler: Arc::clone(&emit_handler),
                    contexts: Mutex::new(PluginContexts::default()),
                    publish_quotas: Mutex::new(HashMap::new()),
                });
                (name, runtime)
            })
            .collect();
        let inner = Arc::new(PluginRegistryInner {
            plugins,
            tasks: Mutex::new(Vec::new()),
            ready,
            emit_handler,
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
            let (control, receiver) = mpsc::channel(PLUGIN_OUTBOUND_CAPACITY);
            let notifications = plugin
                .notifications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            *plugin
                .control
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(control);
            let task = runtime.spawn(async move {
                plugin.run(receiver, notifications).await;
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

    pub(crate) fn set_emit_handler(&self, handler: PluginEmitHandler) {
        *self
            .inner
            .emit_handler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handler);
    }

    pub(crate) fn check_publish_quota(
        &self,
        plugin: &str,
        session_id: cookie_agent_protocol::SessionId,
        event_bytes: usize,
    ) -> Result<(), &'static str> {
        let runtime = self
            .inner
            .plugins
            .get(plugin)
            .ok_or("plugin is not registered")?;
        let now = Instant::now();
        let mut quotas = runtime
            .publish_quotas
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let quota = quotas.entry(session_id).or_insert(PublishQuota {
            second_started: now,
            minute_started: now,
            events_this_second: 0,
            bytes_this_minute: 0,
        });
        if now.duration_since(quota.second_started) >= Duration::from_secs(1) {
            quota.second_started = now;
            quota.events_this_second = 0;
        }
        if now.duration_since(quota.minute_started) >= Duration::from_secs(60) {
            quota.minute_started = now;
            quota.bytes_this_minute = 0;
        }
        if quota.events_this_second >= PLUGIN_EVENTS_PER_SECOND {
            return Err("plugin event rate exceeds 40 events per second");
        }
        if quota.bytes_this_minute.saturating_add(event_bytes) > PLUGIN_BYTES_PER_MINUTE {
            return Err("plugin event volume exceeds 4 MiB per minute");
        }
        quota.events_this_second += 1;
        quota.bytes_this_minute += event_bytes;
        Ok(())
    }

    pub(crate) fn note_offender_diagnostic(&self, plugin: &str, reason: String) {
        if let Some(runtime) = self.inner.plugins.get(plugin) {
            runtime
                .status
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .reason = Some(reason);
        }
    }

    pub(crate) fn stream_session_event(
        &self,
        event: &cookie_agent_protocol::StoredEvent,
        origin: Option<&cookie_agent_protocol::EventOrigin>,
    ) -> Vec<PluginDeliveryDrop> {
        let context_id = plugin_context_id();
        let source_plugin = origin.and_then(|origin| {
            self.inner
                .plugins
                .keys()
                .find(|name| plugin_event_origin(name) == *origin)
                .map(String::as_str)
        });
        let params = ExtensionEventParams {
            session_id: event.session_id,
            context_id: context_id.clone(),
            seq: event.seq,
            event: event.payload.clone(),
            timestamp: event.timestamp,
        };
        let notification = Notification::new(
            PLUGIN_EVENT_METHOD,
            Some(serde_json::to_value(params).expect("plugin event params serialize")),
        );
        let class = match &event.payload {
            cookie_agent_protocol::EventPayload::ToolCallProgress {
                output_chunk: Some(_),
                ..
            } => PluginDeliveryClass::Chunk,
            cookie_agent_protocol::EventPayload::ToolCallTerminated { .. } => {
                PluginDeliveryClass::Terminal
            }
            _ => PluginDeliveryClass::Ordinary,
        };
        self.stream_notification(
            notification,
            event.session_id,
            &context_id,
            source_plugin,
            class,
            |capabilities| capabilities.subscribe_events,
        )
    }

    pub(crate) fn stream_bus_event(
        &self,
        event: &ExtensionBusEventParams,
        source_plugin: Option<&str>,
    ) -> Vec<PluginDeliveryDrop> {
        let context_id = plugin_context_id();
        let mut event = event.clone();
        event.context_id = Some(context_id.clone());
        let notification = Notification::new(
            PLUGIN_BUS_EVENT_METHOD,
            Some(serde_json::to_value(&event).expect("plugin bus event params serialize")),
        );
        self.stream_notification(
            notification,
            event.session_id,
            &context_id,
            source_plugin,
            PluginDeliveryClass::Ordinary,
            |capabilities| capabilities.subscribe_bus,
        )
    }

    fn stream_notification(
        &self,
        notification: Notification,
        session_id: cookie_agent_protocol::SessionId,
        context_id: &str,
        source_plugin: Option<&str>,
        class: PluginDeliveryClass,
        subscribed: impl Fn(&ExtensionPluginCapabilities) -> bool,
    ) -> Vec<PluginDeliveryDrop> {
        let mut drops = Vec::new();
        for runtime in self.inner.plugins.values() {
            if source_plugin == Some(runtime.name.as_str()) {
                continue;
            }
            let enabled = runtime
                .capabilities
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .is_some_and(&subscribed);
            if !enabled {
                continue;
            }
            if runtime
                .control
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none()
            {
                continue;
            }
            let notifications = runtime
                .notifications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            if let Some(dropped) = notifications.push(
                Control::Notify {
                    notification: notification.clone(),
                    session_id,
                    context_id: context_id.to_owned(),
                    context_lifetime: PLUGIN_NOTIFICATION_CONTEXT_LIFETIME,
                },
                class,
            ) {
                record_plugin_delivery_drop(runtime, &mut drops, dropped);
            }
        }
        drops
    }

    pub(crate) fn interception_plugins(&self, hook: ExtensionInterceptionHook) -> Vec<String> {
        self.inner
            .plugins
            .values()
            .filter(|runtime| {
                runtime
                    .capabilities
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_ref()
                    .is_some_and(|capabilities| capabilities.intercept.contains(&hook))
            })
            .map(|runtime| runtime.name.clone())
            .collect()
    }

    pub(crate) async fn intercept_named<P, R>(
        &self,
        plugin: &str,
        method: &'static str,
        params: &P,
        session_id: Option<cookie_agent_protocol::SessionId>,
        context_id: Option<&str>,
    ) -> Result<R, String>
    where
        P: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let runtime = self
            .inner
            .plugins
            .get(plugin)
            .ok_or_else(|| format!("unknown plugin `{plugin}`"))?;
        let params = serde_json::to_value(params).expect("interception params serialize");
        self.intercept_runtime(runtime, method, params, session_id, context_id)
            .await
    }

    async fn intercept_runtime<R>(
        &self,
        runtime: &Arc<PluginRuntime>,
        method: &'static str,
        params: Value,
        session_id: Option<cookie_agent_protocol::SessionId>,
        context_id: Option<&str>,
    ) -> Result<R, String>
    where
        R: serde::de::DeserializeOwned,
    {
        let sender = runtime
            .control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| "plugin is not connected".to_owned())?;
        let (reply, receive) = oneshot::channel();
        let context_lifetime = context_id.map(|_| {
            Duration::from_millis(runtime.config.interception_timeout_ms.saturating_add(100))
        });
        sender
            .try_send(Control::Intercept {
                method,
                params,
                session_id,
                context_id: context_id.map(str::to_owned),
                context_lifetime,
                reply,
            })
            .map_err(|_| "plugin interception queue is full".to_owned())?;
        let _lease = match (session_id, context_id) {
            (Some(session_id), Some(context_id)) => Some(PluginContextLease {
                runtime: Arc::clone(runtime),
                context_id: context_id.to_owned(),
                session_id,
            }),
            _ => None,
        };
        match tokio::time::timeout(
            Duration::from_millis(runtime.config.interception_timeout_ms + 50),
            receive,
        )
        .await
        {
            Ok(Ok(Ok(value))) => serde_json::from_value(value)
                .map_err(|error| format!("malformed interception result: {error}")),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => Err("plugin crashed during interception".into()),
            Err(_) => Err("plugin interception timed out".into()),
        }
    }

    #[cfg(test)]
    pub(crate) async fn intercept_tool_before_call(
        &self,
        params: &ExtensionToolBeforeCallParams,
    ) -> Vec<(String, Result<ExtensionToolBeforeCallResult, String>)> {
        let mut results = Vec::new();
        for plugin in self.interception_plugins(ExtensionInterceptionHook::ToolBeforeCall) {
            let result = self
                .intercept_named::<_, ExtensionToolBeforeCallResult>(
                    &plugin,
                    PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD,
                    params,
                    Some(params.session_id),
                    Some(&params.context_id),
                )
                .await;
            let blocked = result.as_ref().is_ok_and(|result| {
                result.action == cookie_agent_protocol::ExtensionToolBeforeCallAction::Block
            });
            results.push((plugin, result));
            if blocked {
                break;
            }
        }
        results
    }

    #[cfg(test)]
    pub(crate) async fn intercept_tool_after_result(
        &self,
        params: &ExtensionToolAfterResultParams,
    ) -> Vec<(String, Result<ExtensionToolAfterResultResult, String>)> {
        let mut chained = params.clone();
        let mut results = Vec::new();
        for plugin in self.interception_plugins(ExtensionInterceptionHook::ToolAfterResult) {
            let result = self
                .intercept_named::<_, ExtensionToolAfterResultResult>(
                    &plugin,
                    PLUGIN_INTERCEPT_TOOL_AFTER_RESULT_METHOD,
                    &chained,
                    Some(params.session_id),
                    Some(&params.context_id),
                )
                .await;
            if let Ok(result) = &result
                && result.action == cookie_agent_protocol::ExtensionToolAfterResultAction::Replace
                && let Some(replacement) = &result.replacement_content
            {
                chained.result_content = replacement.clone();
            }
            results.push((plugin, result));
        }
        results
    }

    #[cfg(test)]
    pub(crate) async fn intercept_agent_before_start(
        &self,
        params: &ExtensionAgentBeforeStartParams,
    ) -> Vec<(String, Result<ExtensionAgentBeforeStartResult, String>)> {
        let mut chained = params.clone();
        let mut results = Vec::new();
        for plugin in self.interception_plugins(ExtensionInterceptionHook::AgentBeforeStart) {
            let result = self
                .intercept_named::<_, ExtensionAgentBeforeStartResult>(
                    &plugin,
                    PLUGIN_INTERCEPT_AGENT_BEFORE_START_METHOD,
                    &chained,
                    Some(params.session_id),
                    Some(&params.context_id),
                )
                .await;
            if let Ok(result) = &result {
                if let Some(replacement) = result
                    .replace_system_prompt
                    .as_ref()
                    .filter(|value| !value.is_empty())
                {
                    chained.prompt_context["system_prompt"] = Value::String(replacement.clone());
                }
                if let Some(addendum) = result
                    .append_to_system_prompt
                    .as_ref()
                    .or(result.addendum.as_ref())
                    .filter(|value| !value.is_empty())
                    && let Some(system_prompt) = chained
                        .prompt_context
                        .get("system_prompt")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                {
                    chained.prompt_context["system_prompt"] =
                        Value::String(format!("{system_prompt}\n{addendum}"));
                }
                if let Some(message) = &result.inject_message {
                    let messages = chained
                        .prompt_context
                        .as_object_mut()
                        .expect("prompt context is an object")
                        .entry("injected_messages")
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Some(messages) = messages.as_array_mut() {
                        messages.push(serde_json::to_value(message).expect("message serializes"));
                    }
                }
            }
            results.push((plugin, result));
        }
        results
    }

    #[cfg(test)]
    pub(crate) async fn intercept_session_before_compact(
        &self,
        params: &ExtensionSessionBeforeCompactParams,
    ) -> Vec<(String, Result<ExtensionSessionBeforeCompactResult, String>)> {
        let mut chained = params.clone();
        let mut results = Vec::new();
        for plugin in self.interception_plugins(ExtensionInterceptionHook::SessionBeforeCompact) {
            let result = self
                .intercept_named::<_, ExtensionSessionBeforeCompactResult>(
                    &plugin,
                    PLUGIN_INTERCEPT_SESSION_BEFORE_COMPACT_METHOD,
                    &chained,
                    Some(params.session_id),
                    Some(&params.context_id),
                )
                .await;
            if let Ok(result) = &result
                && let Some(addendum) = result.addendum.clone().filter(|value| !value.is_empty())
            {
                chained.additions.push(addendum);
            }
            if let Ok(result) = &result
                && let Some(instructions) = result.instructions_override.clone()
            {
                chained.instructions = Some(instructions);
            }
            results.push((plugin, result));
            if results
                .last()
                .is_some_and(|(_, result)| result.as_ref().is_ok_and(|result| result.cancel))
            {
                break;
            }
        }
        results
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
        let context_id = params.context_id.clone();
        let session_id = params.session_id;
        let context_lifetime =
            Duration::from_millis(runtime.config.tool_timeout_ms.saturating_add(100));
        sender
            .send(Control::ToolCall {
                params,
                context_lifetime,
                reply,
            })
            .await
            .map_err(|_| ToolError::execution(format!("plugin `{plugin}` is not running")))?;
        let _lease = PluginContextLease {
            runtime: Arc::clone(&runtime),
            context_id: context_id.clone(),
            session_id,
        };
        match receive.await {
            Ok(result) => result.map_err(ToolError::execution),
            Err(_) => Err(ToolError::execution(format!(
                "plugin `{plugin}` stopped during tool call"
            ))),
        }
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
            let _ = sender.try_send(Control::Shutdown);
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
    fn register_context(
        &self,
        context_id: &str,
        session_id: cookie_agent_protocol::SessionId,
        expires_at: Instant,
    ) {
        let now = Instant::now();
        let mut contexts = self
            .contexts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_plugin_contexts(&mut contexts, now);
        if contexts
            .spent
            .iter()
            .any(|context| context.id == context_id)
        {
            return;
        }
        if expires_at <= now {
            remember_spent_context(&mut contexts, context_id.to_owned(), session_id, now);
            return;
        }
        contexts.active.push_back(PluginContextGrant {
            id: context_id.to_owned(),
            session_id,
            expires_at,
        });
        if contexts.active.len() > PLUGIN_CONTEXT_CAPACITY
            && let Some(evicted) = contexts.active.pop_front()
        {
            remember_spent_context(&mut contexts, evicted.id, evicted.session_id, now);
        }
    }

    fn revoke_context(&self, context_id: &str, session_id: cookie_agent_protocol::SessionId) {
        let now = Instant::now();
        let mut contexts = self
            .contexts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_plugin_contexts(&mut contexts, now);
        if let Some(index) = contexts
            .active
            .iter()
            .position(|context| context.id == context_id)
            && let Some(context) = contexts.active.remove(index)
        {
            remember_spent_context(&mut contexts, context.id, context.session_id, now);
        } else if !contexts
            .spent
            .iter()
            .any(|context| context.id == context_id)
        {
            remember_spent_context(&mut contexts, context_id.to_owned(), session_id, now);
        }
    }

    fn consume_context(
        &self,
        context_id: &str,
        session_id: cookie_agent_protocol::SessionId,
    ) -> PluginEmitContext {
        let now = Instant::now();
        let mut contexts = self
            .contexts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_plugin_contexts(&mut contexts, now);
        if let Some(index) = contexts
            .active
            .iter()
            .position(|context| context.id == context_id)
            && let Some(context) = contexts.active.remove(index)
        {
            let granted = context.session_id == session_id;
            let diagnostic_session_id = context.session_id;
            remember_spent_context(&mut contexts, context.id, context.session_id, now);
            return if granted {
                PluginEmitContext::Granted
            } else {
                PluginEmitContext::Rejected {
                    diagnostic_session_id: Some(diagnostic_session_id),
                    reason: "session_id does not match the delivered context".into(),
                }
            };
        }
        if let Some(context) = contexts
            .spent
            .iter()
            .find(|context| context.id == context_id)
        {
            return PluginEmitContext::Rejected {
                diagnostic_session_id: Some(context.session_id),
                reason: "context_id was already consumed, expired, or revoked".into(),
            };
        }
        PluginEmitContext::Rejected {
            diagnostic_session_id: None,
            reason: "context_id is unknown to this plugin".into(),
        }
    }

    async fn run(
        self: Arc<Self>,
        receiver: mpsc::Receiver<Control>,
        notifications: Arc<PluginNotificationQueue>,
    ) {
        self.set_status(PluginState::Connecting, None, Vec::new());
        let result = self.spawn_and_supervise(receiver, &notifications).await;
        *self
            .control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        notifications.clear();
        *self
            .contexts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = PluginContexts::default();
        self.declarations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        *self
            .capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.mcp.release_plugin_tools(&self.name);
        match result {
            Ok(()) => self.set_status(PluginState::Disconnected, None, Vec::new()),
            Err(reason) => self.set_status(PluginState::Failed, Some(reason), Vec::new()),
        }
    }

    async fn spawn_and_supervise(
        &self,
        receiver: mpsc::Receiver<Control>,
        notifications: &Arc<PluginNotificationQueue>,
    ) -> Result<(), String> {
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
            .map_err(|error| format!("spawn failure for command `{command}`: {error}"))?;
        let result = self
            .supervise_spawned(&mut child, receiver, notifications)
            .await;
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
        notifications: &Arc<PluginNotificationQueue>,
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
        let result = self
            .run_host_loop(child, stdin, receiver, notifications, inbound)
            .await;
        reader_task.abort();
        let _ = reader_task.await;
        result
    }

    async fn run_host_loop(
        &self,
        child: &mut Box<dyn ChildWrapper>,
        mut stdin: tokio::process::ChildStdin,
        mut control: mpsc::Receiver<Control>,
        notifications: &Arc<PluginNotificationQueue>,
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
                () = notifications.notified() => {
                    for notification in notifications.drain() {
                        match notification {
                            Control::Notify { notification, session_id, context_id, context_lifetime } if connected => {
                                self.register_context(
                                    &context_id,
                                    session_id,
                                    Instant::now() + context_lifetime,
                                );
                                write_json(&mut stdin, &notification).await?;
                            }
                            Control::Notify { .. } => {}
                            _ => unreachable!("notification queue accepts only notifications"),
                        }
                    }
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
                    Some(Control::ToolCall { params, context_lifetime, reply }) if connected => {
                        self.register_context(
                            &params.context_id,
                            params.session_id,
                            Instant::now() + context_lifetime,
                        );
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
                    Some(Control::Notify { notification, session_id, context_id, context_lifetime }) if connected => {
                        self.register_context(
                            &context_id,
                            session_id,
                            Instant::now() + context_lifetime,
                        );
                        write_json(&mut stdin, &notification).await?;
                    }
                    Some(Control::Notify { .. }) => {}
                    Some(Control::Intercept { method, params, session_id, context_id, context_lifetime, reply }) if connected => {
                        if let (Some(session_id), Some(context_id), Some(context_lifetime)) =
                            (session_id, context_id.as_deref(), context_lifetime)
                        {
                            self.register_context(
                                context_id,
                                session_id,
                                Instant::now() + context_lifetime,
                            );
                        }
                        let id = next_request_id;
                        next_request_id = next_request_id.checked_add(1)
                            .ok_or_else(|| "plugin request id space exhausted".to_owned())?;
                        write_json(&mut stdin, &Request::new(
                            JsonRpcId::Number(id), method, Some(params),
                        )).await?;
                        pending.insert(id, PendingRequest::Intercept {
                            deadline: Instant::now()
                                + Duration::from_millis(self.config.interception_timeout_ms),
                            reply,
                        });
                    }
                    Some(Control::Intercept { reply, .. }) => {
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
                            PendingRequest::Intercept { reply, .. } => {
                                let _ = reply.send(Err("plugin interception timed out".into()));
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
                let notification = serde_json::from_value::<Notification>(value)
                    .map_err(|error| format!("malformed plugin notification: {error}"))?;
                if notification.method == PLUGIN_EMIT_METHOD {
                    let params: ExtensionEmitParams =
                        serde_json::from_value(notification.params.unwrap_or(Value::Null))
                            .map_err(|error| {
                                format!("malformed plugin emit notification: {error}")
                            })?;
                    let capabilities = self
                        .capabilities
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    let handler = self
                        .emit_handler()
                        .ok_or_else(|| "plugin event publisher is unavailable".to_owned())?;
                    let outcome = if let Some(capabilities) = capabilities {
                        let context = self.consume_context(&params.context_id, params.session_id);
                        handler(PluginEmitRequest {
                            plugin: self.name.clone(),
                            session_id: params.session_id,
                            context,
                            name: params.name.clone(),
                            payload: params.payload,
                            publish_bus: capabilities.publish_bus,
                            publish_session_events: capabilities.publish_session_events,
                        })
                        .await
                    } else {
                        PluginEmitOutcome {
                            bus: ExtensionEmitStatus::Rejected,
                            durable: ExtensionEmitStatus::Rejected,
                            reason: Some("plugin is not initialized".into()),
                        }
                    };
                    write_json(
                        stdin,
                        &Notification::new(
                            PLUGIN_EMIT_RESULT_METHOD,
                            Some(
                                serde_json::to_value(ExtensionEmitResultParams {
                                    name: params.name,
                                    bus: outcome.bus,
                                    durable: outcome.durable,
                                    reason: outcome.reason,
                                })
                                .expect("emit result params serialize"),
                            ),
                        ),
                    )
                    .await?;
                }
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
                *self
                    .capabilities
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(initialize.capabilities);
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
            PendingRequest::Intercept { reply, .. } => {
                let result = match response {
                    Response::Success(success) => Ok(success.result),
                    Response::Error(error) => Err(format!(
                        "plugin interception rejected: {}",
                        error.error.message
                    )),
                };
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

    fn emit_handler(&self) -> Option<PluginEmitHandler> {
        self.emit_handler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

fn prune_plugin_contexts(contexts: &mut PluginContexts, now: Instant) {
    let mut active = VecDeque::with_capacity(contexts.active.len());
    while let Some(context) = contexts.active.pop_front() {
        if context.expires_at <= now {
            let expired = context;
            remember_spent_context(contexts, expired.id, expired.session_id, now);
        } else {
            active.push_back(context);
        }
    }
    contexts.active = active;
    contexts.spent.retain(|context| context.expires_at > now);
}

fn remember_spent_context(
    contexts: &mut PluginContexts,
    id: String,
    session_id: cookie_agent_protocol::SessionId,
    now: Instant,
) {
    contexts.spent.push_back(PluginSpentContext {
        id,
        session_id,
        expires_at: now + PLUGIN_SPENT_CONTEXT_LIFETIME,
    });
    if contexts.spent.len() > PLUGIN_SPENT_CONTEXT_CAPACITY {
        contexts.spent.pop_front();
    }
}

pub(crate) fn plugin_context_id() -> String {
    uuid::Uuid::now_v7().to_string()
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
            context_id: plugin_context_id(),
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
        AgentId, AssistantToolCallRef, CancellationCapability, EventPayload,
        ExtensionAgentBeforeStartParams, ExtensionBusEventParams,
        ExtensionSessionBeforeCompactParams, ExtensionToolAfterResultAction,
        ExtensionToolAfterResultParams, ExtensionToolBeforeCallAction,
        ExtensionToolBeforeCallParams, Modality, ModelCallId, ModelCapabilities, Notification,
        PersistedToolResult, PluginDiagnosticKind, ReplayCapability, RunId, SafeDisplayText,
        SessionId, StoredEvent, ToolCallId, ToolCallTermination, ToolTerminationOutcome,
    };
    use indexmap::IndexMap;
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;

    use super::{
        Control, PluginDeliveryClass, PluginRegistry, PluginState, plugin_context_id,
        plugin_event_origin,
    };

    #[test]
    fn plugin_event_origins_preserve_valid_names_and_hash_legacy_names() {
        assert_eq!(plugin_event_origin("fixture").as_str(), "plugin:fixture");
        let legacy = plugin_event_origin("command_handler");
        assert_eq!(legacy.plugin_name().expect("plugin origin").len(), 64);
        assert_eq!(legacy, plugin_event_origin("command_handler"));
    }
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
    #[cfg(unix)]
    const PYTHON: &str = "python3";
    #[cfg(windows)]
    const PYTHON: &str = "python";

    #[cfg(unix)]
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
    #[cfg(windows)]
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

    struct Harness {
        directory: tempfile::TempDir,
        registry: PluginRegistry,
    }

    async fn harness(extra_env: &[(&str, &str)], timeout_ms: u64) -> Harness {
        let directory = tempfile::tempdir().expect("plugin test directory");
        let mcp = Arc::new(
            crate::McpRegistry::new(
                BTreeMap::new(),
                directory.path().join("private-oauth").join("oauth.json"),
            )
            .expect("MCP registry"),
        );
        let mut env = BTreeMap::from([
            ("FIXTURE_NAME".into(), "fixture".into()),
            ("FIXTURE_TOOLS".into(), DECLARATION.into()),
        ]);
        #[cfg(windows)]
        for name in ["PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "TEMP", "TMP"] {
            if let Ok(value) = std::env::var(name) {
                env.insert(name.to_owned(), value);
            }
        }
        env.extend(
            extra_env
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        let registry = PluginRegistry::new(
            BTreeMap::from([(
                "fixture".into(),
                PluginConfig {
                    command: Some(PYTHON.into()),
                    args: vec![FIXTURE.into()],
                    env,
                    cwd: None,
                    enabled: true,
                    interception_timeout_ms: 2_000,
                    startup_timeout_ms: 10_000,
                    shutdown_grace_ms: 3_000,
                    tool_timeout_ms: timeout_ms,
                },
            )])
            .into_iter()
            .collect(),
            mcp,
        );
        registry.start_eager(&tokio::runtime::Handle::current());
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, async {
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
        .await;
        if connected.is_err() {
            panic!(
                "plugin failed to connect with interpreter `{PYTHON}` and fixture `{FIXTURE}`; statuses: {:?}",
                registry.statuses()
            );
        }
        Harness {
            directory,
            registry,
        }
    }

    async fn multi_harness(plugins: &[(&str, &[(&str, &str)])]) -> Harness {
        let directory = tempfile::tempdir().expect("plugin test directory");
        let mcp = Arc::new(
            crate::McpRegistry::new(
                BTreeMap::new(),
                directory.path().join("private-oauth").join("oauth.json"),
            )
            .expect("MCP registry"),
        );
        let plugins: IndexMap<String, PluginConfig> = plugins
            .iter()
            .map(|(name, extra_env)| {
                let mut env = BTreeMap::from([
                    ("FIXTURE_NAME".into(), (*name).to_owned()),
                    ("FIXTURE_TOOLS".into(), "[]".into()),
                ]);
                env.extend(
                    extra_env
                        .iter()
                        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
                );
                #[cfg(windows)]
                for name in ["PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "TEMP", "TMP"] {
                    if let Ok(value) = std::env::var(name) {
                        env.insert(name.to_owned(), value);
                    }
                }
                let interception_timeout_ms = env
                    .remove("FIXTURE_HOST_INTERCEPTION_TIMEOUT_MS")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(2_000);
                (
                    (*name).to_owned(),
                    PluginConfig {
                        command: Some(PYTHON.into()),
                        args: vec![FIXTURE.into()],
                        env,
                        cwd: None,
                        enabled: true,
                        interception_timeout_ms,
                        startup_timeout_ms: 10_000,
                        shutdown_grace_ms: 3_000,
                        tool_timeout_ms: 1_000,
                    },
                )
            })
            .collect();
        let registry = PluginRegistry::new(plugins, mcp);
        registry.start_eager(&tokio::runtime::Handle::current());
        registry.await_eager_ready().await;
        assert!(
            registry
                .statuses()
                .iter()
                .all(|status| status.state == PluginState::Connected)
        );
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

    #[tokio::test]
    async fn streams_ordered_events_and_bus_without_self_echo() {
        let marker = tempfile::tempdir().expect("marker directory");
        let event_file = marker.path().join("events.jsonl");
        let bus_file = marker.path().join("bus.jsonl");
        let capabilities = r#"{"tools":true,"resources":false,"subscribe_events":true,"subscribe_bus":true,"publish_bus":false,"publish_session_events":false,"intercept":[]}"#;
        let harness = harness(
            &[
                ("FIXTURE_CAPABILITIES", capabilities),
                (
                    "FIXTURE_EVENT_FILE",
                    event_file.to_str().expect("event path"),
                ),
                (
                    "FIXTURE_BUS_EVENT_FILE",
                    bus_file.to_str().expect("bus path"),
                ),
            ],
            1_000,
        )
        .await;
        let session_id = SessionId::new_v7();
        for seq in [2, 3] {
            let event = StoredEvent {
                engine_version: None,
                origin: None,
                session_id,
                run_id: None,
                seq,
                timestamp: jiff::Timestamp::now(),
                payload: if seq == 2 {
                    EventPayload::ToolCallProgress {
                        tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
                        message: cookie_agent_protocol::SafeDisplayText::new("bash stdout")
                            .expect("message"),
                        output_chunk: Some(
                            cookie_agent_protocol::SafeDisplayText::new("partial output")
                                .expect("chunk"),
                        ),
                    }
                } else {
                    EventPayload::PluginDiagnostic {
                        plugin: "engine".into(),
                        kind: PluginDiagnosticKind::HookBlocked,
                        message: format!("event {seq}"),
                        count: 1,
                    }
                },
            };
            assert!(
                harness
                    .registry
                    .stream_session_event(&event, None)
                    .is_empty()
            );
        }
        assert!(
            harness
                .registry
                .stream_bus_event(
                    &ExtensionBusEventParams {
                        session_id,
                        context_id: None,
                        plugin: "other".into(),
                        name: "notice".into(),
                        payload: serde_json::json!({"ok": true}),
                    },
                    None,
                )
                .is_empty()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !event_file.exists()
                || std::fs::read_to_string(&event_file)
                    .map_or(0, |contents| contents.lines().count())
                    < 2
                || !bus_file.exists()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("streamed notifications");
        let records = std::fs::read_to_string(&event_file)
            .expect("event file")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("event JSON"))
            .collect::<Vec<_>>();
        let seqs = records
            .iter()
            .map(|record| record["seq"].clone())
            .collect::<Vec<_>>();
        assert_eq!(seqs, [serde_json::json!(2), serde_json::json!(3)]);
        assert_eq!(records[0]["event"]["output_chunk"], "partial output");

        let self_event = StoredEvent {
            engine_version: None,
            origin: Some(cookie_agent_protocol::EventOrigin::new("plugin:fixture").unwrap()),
            session_id,
            run_id: None,
            seq: 4,
            timestamp: jiff::Timestamp::now(),
            payload: EventPayload::PluginEventAdded {
                plugin: "fixture".into(),
                name: "self".into(),
                payload: Value::Null,
            },
        };
        harness
            .registry
            .stream_session_event(&self_event, self_event.origin.as_ref());
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            std::fs::read_to_string(event_file)
                .expect("event file")
                .lines()
                .count(),
            2
        );
        harness.registry.shutdown().await;
    }

    #[tokio::test]
    async fn dispatches_all_interception_hooks_and_fails_open_on_crash() {
        let capabilities = r#"{"tools":true,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["tool_before_call","tool_after_result","agent_before_start","session_before_compact"]}"#;
        let active = harness(
            &[
                ("FIXTURE_CAPABILITIES", capabilities),
                (
                    "FIXTURE_TOOL_BEFORE_RESULT",
                    r#"{"action":"allow","modified_arguments":{"text":"modified","path":"src/lib.rs"}}"#,
                ),
                (
                    "FIXTURE_TOOL_AFTER_RESULT",
                    r#"{"action":"replace","replacement_content":"replaced"}"#,
                ),
                (
                    "FIXTURE_AGENT_BEFORE_RESULT",
                    r#"{"addendum":"agent addendum"}"#,
                ),
                (
                    "FIXTURE_COMPACT_BEFORE_RESULT",
                    r#"{"addendum":"compact addendum"}"#,
                ),
            ],
            1_000,
        )
        .await;
        let session_id = SessionId::new_v7();
        let before = active
            .registry
            .intercept_tool_before_call(&ExtensionToolBeforeCallParams {
                session_id,
                context_id: plugin_context_id(),
                tool: "fixture_echo".into(),
                arguments: serde_json::json!({"text":"original","path":"src/lib.rs"}),
                permission_name: "fixture_echo".into(),
                resource: Some("src/lib.rs".into()),
            })
            .await;
        assert!(matches!(
            &before[0].1,
            Ok(result)
                if result.action == ExtensionToolBeforeCallAction::Allow
                    && result.modified_arguments.as_ref().is_some_and(|value| value["text"] == "modified")
        ));
        let after = active
            .registry
            .intercept_tool_after_result(&ExtensionToolAfterResultParams {
                session_id,
                context_id: plugin_context_id(),
                tool: "fixture_echo".into(),
                arguments: Value::Null,
                result_content: "original".into(),
                is_error: false,
            })
            .await;
        assert!(matches!(
            &after[0].1,
            Ok(result)
                if result.action == ExtensionToolAfterResultAction::Replace
                    && result.replacement_content.as_deref() == Some("replaced")
        ));
        assert_eq!(
            active
                .registry
                .intercept_agent_before_start(&ExtensionAgentBeforeStartParams {
                    session_id,
                    context_id: plugin_context_id(),
                    agent_path: "primary".into(),
                    prompt_context: Value::Null,
                })
                .await[0]
                .1
                .as_ref()
                .expect("agent interception")
                .addendum
                .as_deref(),
            Some("agent addendum")
        );
        assert_eq!(
            active
                .registry
                .intercept_session_before_compact(&ExtensionSessionBeforeCompactParams {
                    session_id,
                    context_id: plugin_context_id(),
                    checkpoint_id: "checkpoint".into(),
                    additions: Vec::new(),
                    instructions: None,
                })
                .await[0]
                .1
                .as_ref()
                .expect("compaction interception")
                .addendum
                .as_deref(),
            Some("compact addendum")
        );
        active.registry.shutdown().await;

        let crashed = harness(
            &[
                ("FIXTURE_CAPABILITIES", capabilities),
                ("FIXTURE_CRASH_DURING_INTERCEPT", "1"),
            ],
            1_000,
        )
        .await;
        let result = crashed
            .registry
            .intercept_tool_before_call(&ExtensionToolBeforeCallParams {
                session_id,
                context_id: plugin_context_id(),
                tool: "fixture_echo".into(),
                arguments: serde_json::json!({}),
                permission_name: "fixture_echo".into(),
                resource: None,
            })
            .await;
        assert!(
            result[0]
                .1
                .as_ref()
                .is_err_and(|error| error.contains("crashed"))
        );
        crashed.registry.shutdown().await;
    }

    #[tokio::test]
    async fn tool_before_interception_orders_and_block_short_circuits() {
        let marker = tempfile::tempdir().expect("marker directory");
        let second_file = marker.path().join("second.jsonl");
        let capabilities = r#"{"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["tool_before_call"]}"#;
        let first_env = [
            ("FIXTURE_CAPABILITIES", capabilities),
            (
                "FIXTURE_TOOL_BEFORE_RESULT",
                r#"{"action":"allow","modified_arguments":{"step":"first"}}"#,
            ),
        ];
        let second_path = second_file.to_str().expect("second path");
        let second_env = [
            ("FIXTURE_CAPABILITIES", capabilities),
            ("FIXTURE_INTERCEPT_FILE", second_path),
        ];
        let harness = multi_harness(&[("zeta", &first_env), ("alpha", &second_env)]).await;
        let params = ExtensionToolBeforeCallParams {
            session_id: SessionId::new_v7(),
            context_id: plugin_context_id(),
            tool: "example".into(),
            arguments: serde_json::json!({"step":"original"}),
            permission_name: "read".into(),
            resource: None,
        };
        let results = harness.registry.intercept_tool_before_call(&params).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "zeta");
        assert_eq!(results[1].0, "alpha");
        let second: Value = serde_json::from_str(
            std::fs::read_to_string(&second_file)
                .expect("second interception")
                .lines()
                .next()
                .expect("second line"),
        )
        .expect("second JSON");
        assert_eq!(second["params"]["arguments"]["step"], "original");
        harness.registry.shutdown().await;

        let blocked_file = marker.path().join("blocked-second.jsonl");
        let block_env = [
            ("FIXTURE_CAPABILITIES", capabilities),
            (
                "FIXTURE_TOOL_BEFORE_RESULT",
                r#"{"action":"block","reason":"blocked"}"#,
            ),
        ];
        let blocked_path = blocked_file.to_str().expect("blocked path");
        let untouched_env = [
            ("FIXTURE_CAPABILITIES", capabilities),
            ("FIXTURE_INTERCEPT_FILE", blocked_path),
        ];
        let blocked = multi_harness(&[("zeta", &block_env), ("alpha", &untouched_env)]).await;
        let results = blocked.registry.intercept_tool_before_call(&params).await;
        assert_eq!(results.len(), 1);
        assert!(matches!(
            &results[0].1,
            Ok(result) if result.action == ExtensionToolBeforeCallAction::Block
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!blocked_file.exists());
        blocked.registry.shutdown().await;
    }

    #[tokio::test]
    async fn interception_timeout_fails_open_and_remaining_hooks_continue() {
        let marker = tempfile::tempdir().expect("marker directory");
        let second_file = marker.path().join("second.jsonl");
        let capabilities = r#"{"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["tool_before_call"]}"#;
        let slow_env = [
            ("FIXTURE_CAPABILITIES", capabilities),
            ("FIXTURE_INTERCEPT_DELAY_MS", "200"),
            ("FIXTURE_HOST_INTERCEPTION_TIMEOUT_MS", "30"),
        ];
        let second_path = second_file.to_str().expect("second path");
        let steady_env = [
            ("FIXTURE_CAPABILITIES", capabilities),
            ("FIXTURE_INTERCEPT_FILE", second_path),
        ];
        let harness = multi_harness(&[("first", &slow_env), ("second", &steady_env)]).await;
        let results = harness
            .registry
            .intercept_tool_before_call(&ExtensionToolBeforeCallParams {
                session_id: SessionId::new_v7(),
                context_id: plugin_context_id(),
                tool: "example".into(),
                arguments: serde_json::json!({}),
                permission_name: "read".into(),
                resource: None,
            })
            .await;
        assert_eq!(results.len(), 2);
        assert!(
            results[0]
                .1
                .as_ref()
                .is_err_and(|error| error.contains("timed out"))
        );
        assert!(results[1].1.is_ok());
        assert!(second_file.exists());
        harness.registry.shutdown().await;
    }

    #[tokio::test]
    async fn result_agent_and_compaction_hooks_receive_accumulated_state() {
        let marker = tempfile::tempdir().expect("marker directory");
        let alpha_file = marker.path().join("alpha.jsonl");
        let capabilities = r#"{"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["tool_after_result","agent_before_start","session_before_compact"]}"#;
        let zeta_env = [
            ("FIXTURE_CAPABILITIES", capabilities),
            (
                "FIXTURE_TOOL_AFTER_RESULT",
                r#"{"action":"replace","replacement_content":"zeta result"}"#,
            ),
            (
                "FIXTURE_AGENT_BEFORE_RESULT",
                r#"{"addendum":"zeta agent"}"#,
            ),
            (
                "FIXTURE_COMPACT_BEFORE_RESULT",
                r#"{"addendum":"zeta compact"}"#,
            ),
        ];
        let alpha_path = alpha_file.to_str().expect("alpha path");
        let alpha_env = [
            ("FIXTURE_CAPABILITIES", capabilities),
            ("FIXTURE_INTERCEPT_FILE", alpha_path),
        ];
        let harness = multi_harness(&[("zeta", &zeta_env), ("alpha", &alpha_env)]).await;
        let session_id = SessionId::new_v7();
        harness
            .registry
            .intercept_tool_after_result(&ExtensionToolAfterResultParams {
                session_id,
                context_id: plugin_context_id(),
                tool: "example".into(),
                arguments: serde_json::json!({}),
                result_content: "original".into(),
                is_error: false,
            })
            .await;
        harness
            .registry
            .intercept_agent_before_start(&ExtensionAgentBeforeStartParams {
                session_id,
                context_id: plugin_context_id(),
                agent_path: "primary".into(),
                prompt_context: serde_json::json!({"system_prompt":"original prompt"}),
            })
            .await;
        harness
            .registry
            .intercept_session_before_compact(&ExtensionSessionBeforeCompactParams {
                session_id,
                context_id: plugin_context_id(),
                checkpoint_id: "checkpoint".into(),
                additions: Vec::new(),
                instructions: None,
            })
            .await;
        let records = std::fs::read_to_string(alpha_file)
            .expect("alpha records")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("record JSON"))
            .collect::<Vec<_>>();
        assert_eq!(records[0]["params"]["result_content"], "zeta result");
        assert_eq!(
            records[1]["params"]["prompt_context"]["system_prompt"],
            "original prompt\nzeta agent"
        );
        assert_eq!(records[2]["params"]["additions"][0], "zeta compact");
        harness.registry.shutdown().await;
    }

    #[tokio::test]
    async fn full_plugin_buffer_drops_without_blocking_and_counts_loss() {
        let capabilities = r#"{"tools":true,"resources":false,"subscribe_events":true,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":[]}"#;
        let harness = harness(&[("FIXTURE_CAPABILITIES", capabilities)], 1_000).await;
        let runtime = harness
            .registry
            .inner
            .plugins
            .get("fixture")
            .expect("fixture runtime");
        let original = runtime
            .notifications
            .lock()
            .expect("notification lock")
            .clone();
        *runtime.notifications.lock().expect("notification lock") =
            Arc::new(super::PluginNotificationQueue::new(1));
        let event = StoredEvent {
            engine_version: None,
            origin: None,
            session_id: SessionId::new_v7(),
            run_id: None,
            seq: 2,
            timestamp: jiff::Timestamp::now(),
            payload: EventPayload::ToolCallProgress {
                tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
                message: cookie_agent_protocol::SafeDisplayText::new("bash stdout")
                    .expect("message"),
                output_chunk: Some(
                    cookie_agent_protocol::SafeDisplayText::new("flood chunk").expect("chunk"),
                ),
            },
        };
        assert!(
            harness
                .registry
                .stream_session_event(&event, None)
                .is_empty()
        );
        let started = std::time::Instant::now();
        let drops = harness.registry.stream_session_event(&event, None);
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].plugin, "fixture");
        assert_eq!(
            runtime.contexts.lock().expect("contexts lock").active.len(),
            0,
            "queued or failed delivery registered a context grant before host dequeue"
        );
        assert_eq!(harness.registry.statuses()[0].dropped_events, 1);
        *runtime.notifications.lock().expect("notification lock") = original;
        harness.registry.shutdown().await;
    }

    #[tokio::test]
    async fn chunk_flood_evicts_by_priority_without_reordering_terminal() {
        let capabilities = r#"{"tools":true,"resources":false,"subscribe_events":true,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":[]}"#;
        let harness = harness(&[("FIXTURE_CAPABILITIES", capabilities)], 1_000).await;
        let runtime = harness
            .registry
            .inner
            .plugins
            .get("fixture")
            .expect("fixture runtime");
        let original = runtime
            .notifications
            .lock()
            .expect("notification lock")
            .clone();
        let notifications = Arc::new(super::PluginNotificationQueue::new(17));
        *runtime.notifications.lock().expect("notification lock") = Arc::clone(&notifications);
        let session_id = SessionId::new_v7();
        let call_id = ToolCallId::new_v7();
        let owner = AssistantToolCallRef {
            model_turn_seq: 1,
            content_index: 0,
            model_call_id: ModelCallId::new("plugin-flood-call").expect("model call id"),
            provider_item_id: None,
        };
        let event = |seq, payload| StoredEvent {
            engine_version: None,
            origin: None,
            session_id,
            run_id: None,
            seq,
            timestamp: jiff::Timestamp::now(),
            payload,
        };

        let chunk = |seq| {
            event(
                seq,
                EventPayload::ToolCallProgress {
                    tool_call_id: call_id,
                    message: SafeDisplayText::new("bash stdout").expect("message"),
                    output_chunk: Some(
                        SafeDisplayText::new(format!("chunk {seq}")).expect("chunk"),
                    ),
                },
            )
        };
        assert!(
            harness
                .registry
                .stream_session_event(&chunk(1), None)
                .is_empty()
        );
        for seq in 2..=17 {
            assert!(
                harness
                    .registry
                    .stream_session_event(
                        &event(
                            seq,
                            EventPayload::PluginDiagnostic {
                                plugin: "engine".into(),
                                kind: PluginDiagnosticKind::HookBlocked,
                                message: format!("ordinary {seq}"),
                                count: 1,
                            },
                        ),
                        None,
                    )
                    .is_empty()
            );
        }
        let first_overflow = harness.registry.stream_session_event(
            &event(
                18,
                EventPayload::PluginDiagnostic {
                    plugin: "engine".into(),
                    kind: PluginDiagnosticKind::HookBlocked,
                    message: "ordinary overflow".into(),
                    count: 1,
                },
            ),
            None,
        );
        assert_eq!(first_overflow.len(), 1);
        assert_eq!(first_overflow[0].class, PluginDeliveryClass::Chunk);
        let second_overflow = harness.registry.stream_session_event(
            &event(
                19,
                EventPayload::PluginDiagnostic {
                    plugin: "engine".into(),
                    kind: PluginDiagnosticKind::HookBlocked,
                    message: "second ordinary overflow".into(),
                    count: 1,
                },
            ),
            None,
        );
        assert_eq!(second_overflow.len(), 1);
        assert_eq!(second_overflow[0].class, PluginDeliveryClass::Ordinary);
        let terminal_drops = harness.registry.stream_session_event(
            &event(
                20,
                EventPayload::ToolCallTerminated {
                    termination: ToolCallTermination {
                        tool_call_id: call_id,
                        owner,
                        outcome: ToolTerminationOutcome::Completed,
                        result: Some(PersistedToolResult {
                            title: SafeDisplayText::new("Bash").expect("title"),
                            output: "done".into(),
                            metadata: Value::Null,
                            truncation: None,
                            attachments: Vec::new(),
                        }),
                        error: None,
                    },
                },
            ),
            None,
        );

        assert_eq!(terminal_drops.len(), 1);
        assert_eq!(terminal_drops[0].class, PluginDeliveryClass::Ordinary);
        assert_eq!(harness.registry.statuses()[0].dropped_events, 3);
        let mut delivered = Vec::new();
        while let Some(control) = notifications.pop() {
            let Control::Notify { notification, .. } = control else {
                panic!("event notification")
            };
            let params = notification.params.expect("params");
            delivered.push((
                params["seq"].as_u64().expect("sequence"),
                params["event"]["type"]
                    .as_str()
                    .expect("event type")
                    .to_owned(),
            ));
        }
        assert!(delivered.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(delivered.last(), Some(&(20, "tool_call_terminated".into())));

        *runtime.notifications.lock().expect("notification lock") = original;
        harness.registry.shutdown().await;
    }

    #[tokio::test]
    async fn expired_context_is_spent_and_keeps_only_its_known_session() {
        let capabilities = r#"{"tools":true,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":[]}"#;
        let harness = harness(&[("FIXTURE_CAPABILITIES", capabilities)], 1_000).await;
        let runtime = harness
            .registry
            .inner
            .plugins
            .get("fixture")
            .expect("fixture runtime");
        let session_id = SessionId::new_v7();
        runtime.register_context(
            "expiring-context",
            session_id,
            tokio::time::Instant::now() + Duration::from_millis(1),
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(matches!(
            runtime.consume_context("expiring-context", SessionId::new_v7()),
            super::PluginEmitContext::Rejected {
                diagnostic_session_id: Some(known_session),
                ..
            } if known_session == session_id
        ));
        harness.registry.shutdown().await;
    }

    #[tokio::test]
    async fn context_lifetime_starts_at_delivery_and_cancelled_queue_entry_stays_spent() {
        let harness = harness(&[], 1_000).await;
        let runtime = harness
            .registry
            .inner
            .plugins
            .get("fixture")
            .expect("fixture runtime");
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        let session_id = SessionId::new_v7();
        sender
            .try_send(Control::Notify {
                notification: Notification::new("plugin/test", None),
                session_id,
                context_id: "delayed".into(),
                context_lifetime: Duration::from_millis(5),
            })
            .expect("queue delayed context");
        tokio::time::sleep(Duration::from_millis(10)).await;
        let Control::Notify {
            context_id,
            context_lifetime,
            ..
        } = receiver.recv().await.expect("delayed control")
        else {
            panic!("expected notification control");
        };
        runtime.register_context(
            &context_id,
            session_id,
            tokio::time::Instant::now() + context_lifetime,
        );
        assert!(matches!(
            runtime.consume_context(&context_id, session_id),
            super::PluginEmitContext::Granted
        ));

        sender
            .try_send(Control::Notify {
                notification: Notification::new("plugin/test", None),
                session_id,
                context_id: "cancelled".into(),
                context_lifetime: Duration::from_secs(1),
            })
            .expect("queue cancelled context");
        runtime.revoke_context("cancelled", session_id);
        let Control::Notify {
            context_id,
            context_lifetime,
            ..
        } = receiver.recv().await.expect("cancelled control")
        else {
            panic!("expected notification control");
        };
        runtime.register_context(
            &context_id,
            session_id,
            tokio::time::Instant::now() + context_lifetime,
        );
        assert!(matches!(
            runtime.consume_context(&context_id, session_id),
            super::PluginEmitContext::Rejected {
                diagnostic_session_id: Some(known_session),
                ..
            } if known_session == session_id
        ));
        harness.registry.shutdown().await;
    }
}
