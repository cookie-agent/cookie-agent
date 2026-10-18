//! The plugin host (`docs/site/specs/plugins-v1.md`).
//!
//! A plugin is a trusted, local, daemon-wide extension in a child process.
//! Each configured plugin gets a [`PluginRuntime`] that starts the process
//! with the runtime stack's [`ProcessConnector`] (its own process group,
//! stderr captured), connects a [`RuntimeClient`] with the `plugin/initialize`
//! handshake, and restarts it with the [`ReconnectPolicy`] when it drops.
//!
//! The runtime client carries everything a plugin shares with a runtime: its
//! tools run over the runtime protocol's `tool/*` messages, and its producers
//! over `producer/*` as `plugin:<name>`. This module adds what only plugins
//! do:
//!
//! - **hooks**: [`PluginRegistry::intercept_named`] calls one plugin's hook
//!   with a fresh one-shot context grant and its interception timeout;
//! - **event and bus streams**: a bounded per-plugin queue that evicts by
//!   priority (output chunks first, terminal events last) and a writer that
//!   sends it with a context grant per notification;
//! - **emits**: `plugin/emit` checks the context grant and the plugin's
//!   publishing quota, then hands the event to the engine.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use cookie_agent_config::PluginConfig;
use cookie_agent_protocol::{
    ExtensionBusEventParams, ExtensionEmitParams, ExtensionEmitResultParams, ExtensionEmitStatus,
    ExtensionHookOnError, ExtensionInterceptionHook, ExtensionPluginCapabilities,
    PLUGIN_BUS_EVENT_METHOD, PLUGIN_EMIT_METHOD, PLUGIN_EMIT_RESULT_METHOD, PLUGIN_EVENT_METHOD,
    PluginInfo, PluginState, ProducerSourceState, ProducerSourceStatus, ToolCallPresentation,
};
use cookie_agent_runtime_api::{
    PreparedTool, PromptSection, RuntimeConnector as _, SessionToolContext, ToolCall, ToolError,
    ToolPreparationContext, ToolProvider, ToolSpec,
    reconnect::{ReconnectOutcome, ReconnectPolicy},
    rpc::IncomingNotification,
};
use indexmap::IndexMap;
use serde_json::{Value, value::RawValue};
use tokio::{
    sync::{Notify, watch},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use crate::runtimes::{ClientExtension, ProcessConnector, RuntimeClient};

pub(crate) mod hook_views;

pub(crate) const MAX_PLUGIN_EVENT_PAYLOAD_BYTES: usize = 256 * 1024;
pub(crate) const MAX_PLUGIN_EVENT_NAME_CHARS: usize = 128;
pub(crate) const MAX_PLUGIN_EVENT_TOTAL_BYTES: usize = 272 * 1024;
/// The provenance ID every plugin's tool provider shares.
pub(crate) const PLUGIN_PROVIDER_ID: &str = "plugin";
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

/// Why a hook produced no result. Each failure is recorded as a plugin
/// diagnostic of its [`Self::kind`]; the plugin's `on_error` for the hook
/// decides whether the operation goes on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InterceptError {
    /// No answer within the plugin's `interception_timeout_ms`.
    Timeout,
    /// The plugin is not connected, or its connection ended during the hook.
    Disconnected,
    /// The plugin answered with an error.
    Rejected(String),
    /// The plugin's answer does not match the hook's result.
    Malformed(String),
}

impl InterceptError {
    /// The diagnostic this failure is recorded as.
    pub(crate) fn kind(&self) -> cookie_agent_protocol::PluginDiagnosticKind {
        use cookie_agent_protocol::PluginDiagnosticKind;
        match self {
            Self::Timeout => PluginDiagnosticKind::InterceptionTimeout,
            Self::Disconnected => PluginDiagnosticKind::InterceptionCrash,
            Self::Rejected(_) | Self::Malformed(_) => PluginDiagnosticKind::InterceptionRejected,
        }
    }

    /// What went wrong, as the end of a sentence about the hook: "the hook
    /// timed out".
    pub(crate) fn reason(&self) -> String {
        match self {
            Self::Timeout => "timed out".into(),
            Self::Disconnected => "could not run because the plugin is not connected".into(),
            Self::Rejected(message) => format!("answered with an error: {message}"),
            Self::Malformed(message) => format!("answered with a malformed result: {message}"),
        }
    }
}

/// A plugin that intercepts a hook, and what its failure means.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HookPlugin {
    pub(crate) name: String,
    pub(crate) on_error: ExtensionHookOnError,
}

/// A hook call that failed for a plugin whose `on_error` for the hook is
/// `block`: the operation the hook guards is blocked or refused.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error(
    "plugin `{plugin}` could not check this: its `{hook}` hook {reason}, and the plugin blocks what it cannot check"
)]
pub struct PluginHookFailure {
    pub plugin: String,
    pub hook: ExtensionInterceptionHook,
    pub reason: String,
}

impl std::fmt::Display for InterceptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("plugin interception timed out"),
            Self::Disconnected => formatter.write_str("plugin is not connected"),
            Self::Rejected(message) => write!(formatter, "plugin interception rejected: {message}"),
            Self::Malformed(message) => {
                write!(formatter, "malformed interception result: {message}")
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PluginEmitRequest {
    pub plugin: String,
    pub session_id: cookie_agent_protocol::SessionId,
    pub context: PluginEmitContext,
    pub name: String,
    pub payload: Value,
    pub publish_bus: bool,
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

/// A notification waiting for a plugin, with the context grant it carries.
struct QueuedNotification {
    method: &'static str,
    /// The serialized params, shared by every plugin it goes to.
    params: Arc<RawValue>,
    session_id: cookie_agent_protocol::SessionId,
    context_id: String,
    class: PluginDeliveryClass,
}

/// A plugin's bounded outbound notification queue. When it is full, a new
/// notification evicts the oldest one of the lowest priority it outranks.
struct PluginNotificationQueue {
    capacity: usize,
    queued: Mutex<VecDeque<QueuedNotification>>,
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

    /// Queues `item`; returns the class of the notification dropped to make
    /// room, or the item's own when nothing it outranks is queued.
    fn push(&self, item: QueuedNotification) -> Option<PluginDeliveryClass> {
        let mut queued = lock(&self.queued);
        let dropped = if queued.len() < self.capacity {
            None
        } else {
            let position = |class| queued.iter().position(|queued| queued.class == class);
            let index = match item.class {
                PluginDeliveryClass::Chunk => position(PluginDeliveryClass::Chunk),
                PluginDeliveryClass::Ordinary => position(PluginDeliveryClass::Chunk)
                    .or_else(|| position(PluginDeliveryClass::Ordinary)),
                PluginDeliveryClass::Terminal => position(PluginDeliveryClass::Chunk)
                    .or_else(|| position(PluginDeliveryClass::Ordinary))
                    .or_else(|| position(PluginDeliveryClass::Terminal)),
            };
            let Some(index) = index else {
                return Some(item.class);
            };
            queued.remove(index).map(|removed| removed.class)
        };
        queued.push_back(item);
        drop(queued);
        self.notify.notify_one();
        dropped
    }

    fn pop(&self) -> Option<QueuedNotification> {
        lock(&self.queued).pop_front()
    }

    fn clear(&self) {
        lock(&self.queued).clear();
    }
}

#[derive(Debug, Default)]
struct PluginContexts {
    active: VecDeque<PluginContextGrant>,
    spent: VecDeque<PluginContextGrant>,
}

/// One-shot authority to emit for a session, granted with a hook or a
/// streamed notification.
#[derive(Debug)]
struct PluginContextGrant {
    id: String,
    session_id: cookie_agent_protocol::SessionId,
    expires_at: Instant,
}

/// Revokes a hook's context grant once the hook has answered.
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

/// The current connection of a plugin and what it declared.
struct Connected {
    client: RuntimeClient,
    capabilities: ExtensionPluginCapabilities,
    /// What the daemon ignored in its handshake, shown as the plugin's
    /// reason once it is connected.
    notes: Vec<String>,
}

/// One configured plugin.
struct PluginRuntime {
    name: String,
    config: PluginConfig,
    state: Mutex<PluginState>,
    reason: Mutex<Option<String>>,
    connection: Mutex<Option<Connected>>,
    notifications: PluginNotificationQueue,
    dropped_events: AtomicU64,
    contexts: Mutex<PluginContexts>,
    publish_quotas: Mutex<HashMap<cookie_agent_protocol::SessionId, PublishQuota>>,
    /// Set once the first start attempt has finished, connected or not.
    first_attempt_done: AtomicBool,
    /// Cancelled to restart the plugin; replaced with every connection.
    restart: Mutex<CancellationToken>,
    registry: Weak<PluginRegistryInner>,
}

struct PluginRegistryInner {
    plugins: IndexMap<String, Arc<PluginRuntime>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    ready: Notify,
    emit_handler: Mutex<Option<PluginEmitHandler>>,
    changes: watch::Sender<u64>,
    shutdown: CancellationToken,
    reconnect: Mutex<ReconnectPolicy>,
}

impl PluginRegistryInner {
    fn changed(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}

/// Every configured plugin, in the order their hooks run.
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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl PluginRegistry {
    pub(crate) fn new(plugins: IndexMap<String, PluginConfig>) -> Self {
        let (changes, _) = watch::channel(0);
        let inner = Arc::new_cyclic(|registry: &Weak<PluginRegistryInner>| {
            let plugins = plugins
                .into_iter()
                .map(|(name, config)| {
                    let state = if config.enabled {
                        PluginState::Starting
                    } else {
                        PluginState::Disabled
                    };
                    let runtime = Arc::new(PluginRuntime {
                        name: name.clone(),
                        first_attempt_done: AtomicBool::new(!config.enabled),
                        config,
                        state: Mutex::new(state),
                        reason: Mutex::new(None),
                        connection: Mutex::new(None),
                        notifications: PluginNotificationQueue::new(PLUGIN_NOTIFICATION_CAPACITY),
                        dropped_events: AtomicU64::new(0),
                        contexts: Mutex::new(PluginContexts::default()),
                        publish_quotas: Mutex::new(HashMap::new()),
                        restart: Mutex::new(CancellationToken::new()),
                        registry: registry.clone(),
                    });
                    (name, runtime)
                })
                .collect();
            PluginRegistryInner {
                plugins,
                tasks: Mutex::new(Vec::new()),
                ready: Notify::new(),
                emit_handler: Mutex::new(None),
                changes,
                shutdown: CancellationToken::new(),
                reconnect: Mutex::new(ReconnectPolicy::default()),
            }
        });
        Self { inner }
    }

    /// Starts every enabled plugin on `runtime`.
    pub(crate) fn start_eager(&self, runtime: &tokio::runtime::Handle) {
        for plugin in self.inner.plugins.values() {
            if plugin.config.enabled {
                let plugin = Arc::clone(plugin);
                let task = runtime.spawn(async move { plugin.supervise().await });
                lock(&self.inner.tasks).push(task);
            }
        }
    }

    /// How a dropped plugin is restarted; tests shorten it.
    #[cfg(test)]
    pub(crate) fn set_reconnect_policy(&self, policy: ReconnectPolicy) {
        *lock(&self.inner.reconnect) = policy;
    }

    /// Waits until every enabled plugin's first start attempt has finished,
    /// connected or failed.
    pub(crate) async fn await_eager_ready(&self) {
        loop {
            let notified = self.inner.ready.notified();
            if self
                .inner
                .plugins
                .values()
                .all(|plugin| plugin.first_attempt_done.load(Ordering::Acquire))
            {
                return;
            }
            notified.await;
        }
    }

    /// Every configured plugin as clients see it, in hook order.
    #[must_use]
    pub fn statuses(&self) -> Vec<PluginInfo> {
        self.inner
            .plugins
            .values()
            .map(|plugin| plugin.info())
            .collect()
    }

    /// Restarts plugin `name` now.
    pub(crate) fn restart(&self, name: &str) -> Result<(), String> {
        let plugin = self
            .inner
            .plugins
            .get(name)
            .ok_or_else(|| format!("unknown plugin `{name}`"))?;
        if !plugin.config.enabled {
            return Err(format!("plugin `{name}` is disabled"));
        }
        lock(&plugin.restart).cancel();
        Ok(())
    }

    /// Plugin status changes and producer connection changes, coalesced.
    pub(crate) fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }

    pub(crate) fn set_emit_handler(&self, handler: PluginEmitHandler) {
        *lock(&self.inner.emit_handler) = Some(handler);
    }

    /// The tool provider of each enabled plugin, in hook order. Each composes
    /// under the plugin's name and publishes the tools of its current
    /// connection.
    pub(crate) fn tool_providers(&self) -> Vec<Arc<dyn ToolProvider>> {
        self.inner
            .plugins
            .values()
            .filter(|plugin| plugin.config.enabled)
            .map(|plugin| {
                Arc::new(PluginToolProvider {
                    plugin: Arc::clone(plugin),
                }) as Arc<dyn ToolProvider>
            })
            .collect()
    }

    /// The live connection of every connected plugin.
    pub(crate) fn connections(&self) -> Vec<RuntimeClient> {
        self.inner
            .plugins
            .values()
            .filter_map(|plugin| plugin.client())
            .collect()
    }

    /// Whether `epoch` is the live connection of plugin `name`, and it
    /// produces.
    pub(crate) fn producer_connection_is_current(&self, name: &str, epoch: u64) -> bool {
        self.inner
            .plugins
            .get(name)
            .and_then(|plugin| plugin.client())
            .is_some_and(|client| {
                client.epoch() == epoch && !client.is_closed() && client.produces()
            })
    }

    /// The recovery state of every plugin configured for producer
    /// messaging. One that is not connected is `starting`, or `failed` once
    /// it stopped retrying; failed or disabled external work is unknown,
    /// never proof of completion.
    #[must_use]
    pub fn producer_recovery_states(&self) -> Vec<ProducerSourceState> {
        self.inner
            .plugins
            .values()
            .filter(|plugin| plugin.config.producer_messaging)
            .filter_map(|plugin| {
                let status = if !plugin.config.enabled {
                    ProducerSourceStatus::Disabled
                } else if let Some(client) = plugin.client() {
                    client
                        .producer_status()
                        .unwrap_or(ProducerSourceStatus::Disabled)
                } else if *lock(&plugin.state) == PluginState::Failed {
                    ProducerSourceStatus::Failed
                } else {
                    ProducerSourceStatus::Starting
                };
                Some(ProducerSourceState {
                    source: cookie_agent_protocol::ProducerSource::plugin(&plugin.name).ok()?,
                    status,
                })
            })
            .collect()
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
        let mut quotas = lock(&runtime.publish_quotas);
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

    /// Records `reason` as what plugin `plugin` last could not do.
    pub(crate) fn note_offender_diagnostic(&self, plugin: &str, reason: String) {
        if let Some(runtime) = self.inner.plugins.get(plugin) {
            *lock(&runtime.reason) = Some(reason);
            self.inner.changed();
        }
    }

    pub(crate) fn stream_session_event(
        &self,
        event: &cookie_agent_protocol::StoredEvent,
    ) -> Vec<PluginDeliveryDrop> {
        self.stream_event_payload(event.session_id, event.seq, &event.payload, event.timestamp)
    }

    /// Streams live-only output to event subscribers. It carries the
    /// sequence of the durable event it follows; it has none of its own.
    pub(crate) fn stream_transient_event(
        &self,
        event: &cookie_agent_protocol::TransientEvent,
    ) -> Vec<PluginDeliveryDrop> {
        self.stream_event_payload(
            event.session_id,
            event.after_seq,
            &event.payload,
            event.timestamp,
        )
    }

    fn stream_event_payload(
        &self,
        session_id: cookie_agent_protocol::SessionId,
        seq: u64,
        payload: &cookie_agent_protocol::EventPayload,
        timestamp: jiff::Timestamp,
    ) -> Vec<PluginDeliveryDrop> {
        let class = match payload {
            cookie_agent_protocol::EventPayload::ToolCallProgress {
                display: Some(_), ..
            } => PluginDeliveryClass::Chunk,
            cookie_agent_protocol::EventPayload::ToolCallTerminated { .. } => {
                PluginDeliveryClass::Terminal
            }
            _ => PluginDeliveryClass::Ordinary,
        };
        // Serialized once, when a plugin subscribes to any event; its type,
        // which the patterns select by, is read back from that JSON.
        let encoded = std::cell::OnceCell::new();
        let encoded = || encoded.get_or_init(|| EncodedEvent::of(payload));
        self.stream_notification(
            session_id,
            None,
            PLUGIN_EVENT_METHOD,
            class,
            |capabilities| {
                !capabilities.subscribe_events.is_empty() && {
                    let tag = &encoded().tag;
                    capabilities
                        .subscribe_events
                        .iter()
                        .any(|pattern| pattern.matches(tag))
                }
            },
            |context_id| {
                raw_params(&EventParamsRef {
                    session_id,
                    context_id,
                    seq,
                    event: &encoded().json,
                    timestamp,
                })
            },
        )
    }

    /// Streams a bus event to every bus subscriber except its source.
    pub(crate) fn stream_bus_event(
        &self,
        event: &ExtensionBusEventParams,
        source_plugin: Option<&str>,
    ) -> Vec<PluginDeliveryDrop> {
        self.stream_notification(
            event.session_id,
            source_plugin,
            PLUGIN_BUS_EVENT_METHOD,
            PluginDeliveryClass::Ordinary,
            |capabilities| capabilities.subscribe_bus,
            |context_id| {
                raw_params(&BusEventParamsRef {
                    session_id: event.session_id,
                    context_id,
                    plugin: &event.plugin,
                    name: &event.name,
                    payload: &event.payload,
                })
            },
        )
    }

    /// Queues one notification for every connected plugin `subscribed`
    /// selects, except its source. The params and their context are built
    /// only once a plugin wants them, and then shared by all of them.
    fn stream_notification(
        &self,
        session_id: cookie_agent_protocol::SessionId,
        source_plugin: Option<&str>,
        method: &'static str,
        class: PluginDeliveryClass,
        subscribed: impl Fn(&ExtensionPluginCapabilities) -> bool,
        build: impl FnOnce(&str) -> Arc<RawValue>,
    ) -> Vec<PluginDeliveryDrop> {
        let mut drops = Vec::new();
        let mut build = Some(build);
        let mut built: Option<(String, Arc<RawValue>)> = None;
        for runtime in self.inner.plugins.values() {
            if source_plugin == Some(runtime.name.as_str()) {
                continue;
            }
            let wanted = lock(&runtime.connection)
                .as_ref()
                .is_some_and(|connected| subscribed(&connected.capabilities));
            if !wanted {
                continue;
            }
            let (context_id, params) = built.get_or_insert_with(|| {
                let context_id = plugin_context_id();
                let build = build.take().expect("the notification is built once");
                let params = build(&context_id);
                (context_id, params)
            });
            if let Some(dropped) = runtime.notifications.push(QueuedNotification {
                method,
                params: Arc::clone(params),
                session_id,
                context_id: context_id.clone(),
                class,
            }) {
                if runtime.dropped_events.fetch_add(1, Ordering::AcqRel) == 0 {
                    self.inner.changed();
                }
                drops.push(PluginDeliveryDrop {
                    plugin: runtime.name.clone(),
                    class: dropped,
                });
            }
        }
        drops
    }

    /// The connected plugins that intercept `hook`, in hook order.
    pub(crate) fn interception_plugins(&self, hook: ExtensionInterceptionHook) -> Vec<HookPlugin> {
        self.inner
            .plugins
            .values()
            .filter_map(|runtime| {
                let on_error = lock(&runtime.connection)
                    .as_ref()?
                    .capabilities
                    .intercept
                    .iter()
                    .find(|intercept| intercept.hook.as_str() == hook.name())?
                    .on_error;
                Some(HookPlugin {
                    name: runtime.name.clone(),
                    on_error,
                })
            })
            .collect()
    }

    /// Runs hook `method` of `plugin` with `params`. When `context_id` is
    /// given, the call gets its own fresh context grant, written over the
    /// params' `context_id`, so a plugin that handles several hooks of one
    /// model attempt can emit from each.
    pub(crate) async fn intercept_named<P, R>(
        &self,
        plugin: &str,
        method: &'static str,
        params: &P,
        session_id: Option<cookie_agent_protocol::SessionId>,
        context_id: Option<&str>,
    ) -> Result<R, InterceptError>
    where
        P: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let runtime = self
            .inner
            .plugins
            .get(plugin)
            .ok_or(InterceptError::Disconnected)?;
        let client = runtime.client().ok_or(InterceptError::Disconnected)?;
        let mut params = serde_json::to_value(params).expect("interception params serialize");
        let timeout = Duration::from_millis(runtime.config.interception_timeout_ms);
        let _lease = match (session_id, context_id) {
            (Some(session_id), Some(_)) => {
                let context_id = plugin_context_id();
                if let Some(fields) = params.as_object_mut() {
                    fields.insert("context_id".into(), Value::String(context_id.clone()));
                }
                runtime.register_context(
                    &context_id,
                    session_id,
                    Instant::now() + timeout + Duration::from_millis(100),
                );
                Some(PluginContextLease {
                    runtime: Arc::clone(runtime),
                    context_id,
                    session_id,
                })
            }
            _ => None,
        };
        let value = match tokio::time::timeout(timeout, client.request_value(method, &params)).await
        {
            Ok(Ok(value)) => value,
            Ok(Err(cookie_agent_runtime_api::rpc::RpcError::Closed(_))) => {
                return Err(InterceptError::Disconnected);
            }
            Ok(Err(error)) => return Err(InterceptError::Rejected(error.to_string())),
            Err(_) => return Err(InterceptError::Timeout),
        };
        serde_json::from_value(value).map_err(|error| InterceptError::Malformed(error.to_string()))
    }

    /// Stops every plugin: each gets `plugin/shutdown` and its shutdown
    /// grace before its process group is killed.
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let task_timeout = self
            .inner
            .plugins
            .values()
            .map(|plugin| plugin.config.shutdown_grace_ms)
            .max()
            .unwrap_or(0)
            .saturating_mul(2)
            .saturating_add(1_000);
        let tasks = std::mem::take(&mut *lock(&self.inner.tasks));
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

/// How one connection of a plugin ended.
enum Ended {
    Shutdown,
    Restart,
    Dropped(String),
}

impl PluginRuntime {
    fn registry(&self) -> Option<Arc<PluginRegistryInner>> {
        self.registry.upgrade()
    }

    fn client(&self) -> Option<RuntimeClient> {
        lock(&self.connection)
            .as_ref()
            .map(|connected| connected.client.clone())
    }

    fn info(&self) -> PluginInfo {
        let connection = lock(&self.connection);
        let capabilities = connection.as_ref().map(|connected| &connected.capabilities);
        PluginInfo {
            name: self.name.clone(),
            state: *lock(&self.state),
            reason: lock(&self.reason).clone(),
            tools: connection
                .as_ref()
                .and_then(|connected| {
                    connected
                        .client
                        .tool_provider()
                        .tools_for_session(&SessionToolContext::new(
                            cookie_agent_protocol::SessionId::new_v7(),
                        ))
                        .ok()
                })
                .unwrap_or_default()
                .into_iter()
                .map(|spec| spec.name)
                .collect(),
            hooks: capabilities.map_or_else(Vec::new, |capabilities| {
                capabilities
                    .intercept
                    .iter()
                    .map(|intercept| intercept.hook.clone())
                    .collect()
            }),
            subscribe_events: capabilities.map_or_else(Vec::new, |capabilities| {
                capabilities.subscribe_events.clone()
            }),
            subscribe_bus: capabilities.is_some_and(|capabilities| capabilities.subscribe_bus),
            publish_bus: capabilities.is_some_and(|capabilities| capabilities.publish_bus),
            producer_messaging: connection
                .as_ref()
                .is_some_and(|connected| connected.client.produces()),
            dropped_events: self.dropped_events.load(Ordering::Acquire),
            stderr: connection
                .as_ref()
                .and_then(|connected| connected.client.diagnostics()),
        }
    }

    fn set_state(&self, state: PluginState, reason: Option<String>) {
        *lock(&self.state) = state;
        if reason.is_some() || state == PluginState::Connected {
            *lock(&self.reason) = reason;
        }
        if let Some(registry) = self.registry() {
            registry.changed();
        }
    }

    fn finish_first_attempt(&self) {
        if !self.first_attempt_done.swap(true, Ordering::AcqRel)
            && let Some(registry) = self.registry()
        {
            registry.ready.notify_waiters();
        }
    }

    /// Starts the plugin, serves it, and restarts it when it drops, until
    /// the daemon stops.
    async fn supervise(self: Arc<Self>) {
        let Some(registry) = self.registry() else {
            return;
        };
        let shutdown = registry.shutdown.clone();
        drop(registry);
        let mut connection = self.connect().await;
        self.finish_first_attempt();
        loop {
            let restart = shutdown.child_token();
            *lock(&self.restart) = restart.clone();
            let reason = match connection {
                Ok(connected) => match self.serve(connected, &restart).await {
                    Ended::Shutdown => return,
                    Ended::Restart => {
                        self.set_state(PluginState::Starting, None);
                        connection = self.connect().await;
                        continue;
                    }
                    Ended::Dropped(reason) => reason,
                },
                Err(reason) => reason,
            };
            if shutdown.is_cancelled() {
                self.set_state(PluginState::Stopped, None);
                return;
            }
            self.set_state(PluginState::Reconnecting, Some(reason));
            let policy = self
                .registry()
                .map(|registry| lock(&registry.reconnect).clone())
                .unwrap_or_default();
            let attempt = Arc::clone(&self);
            let outcome = policy
                .run(&restart, move |_| {
                    let attempt = Arc::clone(&attempt);
                    async move { attempt.connect().await }
                })
                .await;
            connection = match outcome {
                ReconnectOutcome::Reconnected(connected) => Ok(connected),
                ReconnectOutcome::Exhausted { last_error, .. } => {
                    self.set_state(PluginState::Failed, Some(last_error));
                    tokio::select! {
                        () = restart.cancelled() => {}
                        () = shutdown.cancelled() => {}
                    }
                    if shutdown.is_cancelled() {
                        self.set_state(PluginState::Stopped, None);
                        return;
                    }
                    self.set_state(PluginState::Starting, None);
                    self.connect().await
                }
                ReconnectOutcome::Cancelled => {
                    if shutdown.is_cancelled() {
                        self.set_state(PluginState::Stopped, None);
                        return;
                    }
                    self.set_state(PluginState::Starting, None);
                    self.connect().await
                }
            };
        }
    }

    /// Starts the process and runs the handshake. The caller owns the
    /// state: a failed attempt inside a reconnect stays `reconnecting`.
    async fn connect(self: &Arc<Self>) -> Result<Connected, String> {
        let grace = Duration::from_millis(self.config.shutdown_grace_ms);
        let mut connector =
            ProcessConnector::new(self.config.command.clone(), self.config.env.clone(), grace);
        if let Some(cwd) = &self.config.cwd {
            connector = connector.with_cwd(PathBuf::from(cwd));
        }
        let startup = Duration::from_millis(self.config.startup_timeout_ms);
        let extension = Arc::new(PluginExtension {
            plugin: Arc::downgrade(self),
        });
        let name = self.name.clone();
        let producer_messaging = self.config.producer_messaging;
        let interception_timeout = Duration::from_millis(self.config.interception_timeout_ms);
        let attempt = async move {
            let connection = connector.connect().await?;
            RuntimeClient::initialize_plugin(
                connection,
                &name,
                producer_messaging,
                interception_timeout,
                grace,
                extension,
            )
            .await
        };
        let result = tokio::time::timeout(startup, attempt)
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "the plugin did not start within {} ms",
                    startup.as_millis()
                ))
            });
        result.map(|handshake| Connected {
            client: handshake.client,
            capabilities: handshake.result.capabilities,
            notes: handshake.notes,
        })
    }

    /// Serves one connection until it drops, the plugin is restarted, or the
    /// daemon stops.
    async fn serve(self: &Arc<Self>, connected: Connected, restart: &CancellationToken) -> Ended {
        let client = connected.client.clone();
        let notes = (!connected.notes.is_empty()).then(|| connected.notes.join("; "));
        *lock(&self.connection) = Some(connected);
        self.set_state(PluginState::Connected, notes);
        self.finish_first_attempt();
        let writer = tokio::spawn(Arc::clone(self).write_notifications(client.clone()));
        let ended = tokio::select! {
            reason = client.closed() => Ended::Dropped(reason),
            () = restart.cancelled() => {
                let shutdown = self
                    .registry()
                    .is_none_or(|registry| registry.shutdown.is_cancelled());
                client.shutdown_plugin().await;
                if shutdown { Ended::Shutdown } else { Ended::Restart }
            }
        };
        writer.abort();
        *lock(&self.connection) = None;
        self.notifications.clear();
        if matches!(ended, Ended::Shutdown) {
            self.set_state(PluginState::Stopped, None);
        } else if let Some(registry) = self.registry() {
            registry.changed();
        }
        ended
    }

    /// Sends queued notifications, each with its context grant.
    async fn write_notifications(self: Arc<Self>, client: RuntimeClient) {
        loop {
            let notified = self.notifications.notify.notified();
            while let Some(item) = self.notifications.pop() {
                self.register_context(
                    &item.context_id,
                    item.session_id,
                    Instant::now() + PLUGIN_NOTIFICATION_CONTEXT_LIFETIME,
                );
                if client.notify_raw(item.method, &item.params).await.is_err() {
                    return;
                }
            }
            notified.await;
        }
    }

    fn register_context(
        &self,
        context_id: &str,
        session_id: cookie_agent_protocol::SessionId,
        expires_at: Instant,
    ) {
        let now = Instant::now();
        let mut contexts = lock(&self.contexts);
        prune_plugin_contexts(&mut contexts, now);
        if contexts
            .spent
            .iter()
            .any(|context| context.id == context_id)
        {
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
        let mut contexts = lock(&self.contexts);
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
        let mut contexts = lock(&self.contexts);
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

    /// Handles `plugin/emit`: checks the context grant, hands the event to
    /// the engine, and answers with `plugin/emit_result`.
    async fn emit(self: Arc<Self>, client: RuntimeClient, params: ExtensionEmitParams) {
        let handler = self
            .registry()
            .and_then(|registry| lock(&registry.emit_handler).clone());
        let publish_bus = lock(&self.connection)
            .as_ref()
            .is_some_and(|connected| connected.capabilities.publish_bus);
        let outcome = match handler {
            Some(handler) => {
                let context = self.consume_context(&params.context_id, params.session_id);
                handler(PluginEmitRequest {
                    plugin: self.name.clone(),
                    session_id: params.session_id,
                    context,
                    name: params.name.clone(),
                    payload: params.payload,
                    publish_bus,
                })
                .await
            }
            None => PluginEmitOutcome {
                bus: ExtensionEmitStatus::Rejected,
                reason: Some("plugin event publisher is unavailable".into()),
            },
        };
        let result = ExtensionEmitResultParams {
            name: params.name,
            bus: outcome.bus,
            reason: outcome.reason,
        };
        let _ = client
            .notify_raw(PLUGIN_EMIT_RESULT_METHOD, &raw_params(&result))
            .await;
    }
}

/// What a plugin connection handles beyond the runtime protocol.
struct PluginExtension {
    plugin: Weak<PluginRuntime>,
}

impl ClientExtension for PluginExtension {
    /// `plugin/emit` is a command, read strictly; other notifications the
    /// daemon does not know are ignored.
    fn notification(&self, client: RuntimeClient, notification: IncomingNotification) {
        if notification.method != PLUGIN_EMIT_METHOD {
            return;
        }
        let Some(plugin) = self.plugin.upgrade() else {
            return;
        };
        match notification.command::<ExtensionEmitParams>() {
            Ok(params) => {
                tokio::spawn(plugin.emit(client, params));
            }
            Err(error) => self.diagnostic(format!("malformed emit: {error}")),
        }
    }

    fn diagnostic(&self, message: String) {
        let Some(plugin) = self.plugin.upgrade() else {
            return;
        };
        if let Some(registry) = plugin.registry() {
            PluginRegistry { inner: registry }.note_offender_diagnostic(&plugin.name, message);
        }
    }
}

/// One plugin's tools: those its current connection declares, composed
/// under the plugin's name.
struct PluginToolProvider {
    plugin: Arc<PluginRuntime>,
}

impl PluginToolProvider {
    fn current(&self) -> Result<Arc<dyn ToolProvider>, ToolError> {
        self.plugin
            .client()
            .map(|client| client.tool_provider())
            .ok_or_else(|| {
                ToolError::runtime_unavailable(format!(
                    "plugin `{}` is not connected",
                    self.plugin.name
                ))
            })
    }
}

#[async_trait]
impl ToolProvider for PluginToolProvider {
    fn provider_id(&self) -> &'static str {
        PLUGIN_PROVIDER_ID
    }

    fn prompt_sections(&self, ctx: &SessionToolContext) -> Result<Vec<PromptSection>, ToolError> {
        self.current()
            .map_or_else(|_| Ok(Vec::new()), |provider| provider.prompt_sections(ctx))
    }

    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        self.current().map_or_else(
            |_| Ok(Vec::new()),
            |provider| provider.tools_for_session(ctx),
        )
    }

    fn composition_source(&self) -> Option<String> {
        Some(self.plugin.name.clone())
    }

    fn tool_source(
        &self,
        _tool_name: &str,
    ) -> Option<cookie_agent_runtime_api::ProvidedToolSource> {
        Some(cookie_agent_runtime_api::ProvidedToolSource::Plugin {
            plugin: self.plugin.name.clone(),
        })
    }

    fn get_permission_name(_tool_name: &str) -> Result<Option<&'static str>, ToolError>
    where
        Self: Sized,
    {
        Err(ToolError::execution(
            "a plugin tool's permission is declared by its plugin",
        ))
    }

    fn get_permission_resource(
        &self,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        self.current()?
            .get_permission_resource(tool_name, arguments)
    }

    fn get_display_argument(&self, name: &str, arguments: &Value) -> Result<String, ToolError> {
        self.current()?.get_display_argument(name, arguments)
    }

    fn presentation(&self, call: &ToolCall) -> ToolCallPresentation {
        match self.current() {
            Ok(provider) => provider.presentation(call),
            Err(_) => cookie_agent_runtime_api::tool_title_only(&call.name),
        }
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        self.current()?.prepare(ctx, call).await
    }

    async fn prepare_parallel(
        &self,
        ctx: ToolPreparationContext,
        calls: Vec<ToolCall>,
    ) -> Vec<Result<PreparedTool, ToolError>> {
        match self.current() {
            Ok(provider) => provider.prepare_parallel(ctx, calls).await,
            Err(_) => calls
                .iter()
                .map(|_| {
                    Err(ToolError::runtime_unavailable(format!(
                        "plugin `{}` is not connected",
                        self.plugin.name
                    )))
                })
                .collect(),
        }
    }
}

fn prune_plugin_contexts(contexts: &mut PluginContexts, now: Instant) {
    let mut active = VecDeque::with_capacity(contexts.active.len());
    while let Some(context) = contexts.active.pop_front() {
        if context.expires_at <= now {
            remember_spent_context(contexts, context.id, context.session_id, now);
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
    contexts.spent.push_back(PluginContextGrant {
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

fn raw_params(params: &impl serde::Serialize) -> Arc<RawValue> {
    Arc::from(
        serde_json::value::to_raw_value(params).expect("plugin notification params serialize"),
    )
}

/// [`cookie_agent_protocol::ExtensionEventParams`] borrowing the event it
/// carries, so one event is serialized once for every plugin.
#[derive(serde::Serialize)]
struct EventParamsRef<'a> {
    session_id: cookie_agent_protocol::SessionId,
    context_id: &'a str,
    seq: u64,
    event: &'a RawValue,
    timestamp: jiff::Timestamp,
}

/// An event payload's JSON and its type.
struct EncodedEvent {
    json: Box<RawValue>,
    tag: String,
}

impl EncodedEvent {
    fn of(payload: &cookie_agent_protocol::EventPayload) -> Self {
        #[derive(serde::Deserialize)]
        struct Tagged<'a> {
            #[serde(rename = "type", borrow)]
            tag: std::borrow::Cow<'a, str>,
        }
        let json = serde_json::value::to_raw_value(payload).expect("event payloads serialize");
        let tag = serde_json::from_str::<Tagged<'_>>(json.get())
            .map(|tagged| tagged.tag.into_owned())
            .unwrap_or_default();
        Self { json, tag }
    }
}

/// [`ExtensionBusEventParams`] with its context, borrowing the rest.
#[derive(serde::Serialize)]
struct BusEventParamsRef<'a> {
    session_id: cookie_agent_protocol::SessionId,
    context_id: &'a str,
    plugin: &'a str,
    name: &'a str,
    payload: &'a Value,
}

#[cfg(test)]
mod tests;
