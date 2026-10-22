//! Cookie Agent's MCP client: the servers one runtime hosts, as a
//! [`ToolProvider`].
//!
//! An [`McpRegistry`] connects every enabled server eagerly, lists its tools
//! as `<server>_<tool>`, marks their descriptions and results as untrusted
//! server output, retains result media as attachments, and follows each
//! server through its states ([`McpServerState`]). A connected server that
//! drops is reconnected with the shared
//! [`ReconnectPolicy`]:
//! its tools stay published and calls fail fast with `runtime_unavailable`
//! meanwhile, and when every attempt fails its tools are dropped and the
//! failure is broadcast (`docs/site/specs/runtimes.md` section 6.4). Names
//! are unique inside one registry: a server whose generated name collides
//! with a reserved name (the hosting runtime's other tools) or with another
//! server's tool fails.
//!
//! The daemon's global runtime hosts the top-level `[mcp]` servers with one
//! registry, and each coding runtime hosts its template's servers with
//! another. OAuth for remote servers, with its credential store, exists only
//! with the `oauth` feature, which only the daemon enables; a registry opened
//! without a credential store connects every server without OAuth.

#![allow(deprecated)]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use base64::Engine as _;
use cookie_agent_config::LoadedMcpServer;
use cookie_agent_protocol::{
    DisplayBlock, DisplayDelta, Open, PersistedToolResult as ToolResult, PreparedPresentation,
    SafeDisplayText, ToolAttachment, ToolCallId, ToolEmittedContent, ToolEmittedMessage,
    ToolEmittedMessageRole, ToolHint,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, ProgressSink, SessionToolContext, ToolCall, ToolCompletion,
    ToolConcurrency, ToolError, ToolExecutionContext, ToolPreparationContext, ToolProgress,
    ToolProvider, ToolSpec,
    reconnect::{ReconnectOutcome, ReconnectPolicy},
};
use process_wrap::tokio::CommandWrap;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt, Peer,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, ContentBlock,
        CreateMessageRequestParams, CreateMessageResult, ElicitRequestParams, ElicitResult,
        ErrorCode, ErrorData, ListRootsResult, ProgressNotificationParam, ProtocolVersion,
        ResourceContents, ServerResult, Tool,
    },
    service::{
        NotificationContext, PeerRequestOptions, RequestContext, RoleClient, RunningService,
        RxJsonRpcMessage, TxJsonRpcMessage,
    },
    transport::{StreamableHttpClientTransport, TokioChildProcess, Transport},
};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "oauth")]
mod oauth;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const TOOL_LIST_DEBOUNCE: Duration = Duration::from_millis(750);
/// How long a call that stops waiting may spend telling its server so.
const MCP_CANCEL_NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    Disabled,
    Disconnected,
    Connecting,
    Connected,
    /// The connection dropped and is being re-established; the server's
    /// tools stay published and calls fail fast with `runtime_unavailable`.
    Reconnecting,
    NeedsAuth,
    Failed,
}

/// An MCP server that failed to connect, or dropped and did not reconnect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerFailure {
    pub server: String,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct McpServerStatus {
    pub server: String,
    pub state: McpServerState,
    pub message: Option<String>,
    pub tools: Vec<String>,
    pub auth_in_progress: bool,
}

#[derive(Clone)]
struct CachedTool {
    raw_name: String,
    spec: ToolSpec,
}

type ClientService = RunningService<RoleClient, McpClientHandler>;

struct ServerRuntime {
    name: String,
    sanitized_name: String,
    loaded: LoadedMcpServer,
    status: Mutex<McpServerStatus>,
    tools: Mutex<Vec<CachedTool>>,
    list_generation: AtomicU64,
    publication: Mutex<()>,
    tool_refresh: Mutex<ToolRefreshDebounce>,
    connect_lock: AsyncMutex<()>,
    service: AsyncMutex<Option<ClientService>>,
    superseded: CancellationToken,
    registry: Weak<RegistryInner>,
    progress: AsyncMutex<HashMap<ToolCallId, ProgressSink>>,
    /// OAuth state of a remote server.
    #[cfg(feature = "oauth")]
    auth: oauth::ServerAuth,
    /// The installed connection, watched for drops.
    connection: Mutex<Option<LiveConnection>>,
}

/// One installed connection of a server. `lost` fires when its transport
/// ends; `retired` when it is closed on purpose.
struct LiveConnection {
    id: u64,
    lost: CancellationToken,
    retired: CancellationToken,
}

impl Drop for LiveConnection {
    fn drop(&mut self) {
        self.retired.cancel();
    }
}

/// A transport that signals `lost` when its peer stops sending: a stdio
/// process exit, a closed pipe, or an HTTP session the transport gave up on.
struct WatchedTransport<T> {
    inner: T,
    lost: CancellationToken,
}

impl<T: Transport<RoleClient>> Transport<RoleClient> for WatchedTransport<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        let lost = self.lost.clone();
        let receive = self.inner.receive();
        async move {
            let message = receive.await;
            if message.is_none() {
                lost.cancel();
            }
            message
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

struct ToolRefreshDebounce {
    peer: Option<Peer<RoleClient>>,
    cancellation: CancellationToken,
    notify: Arc<Notify>,
    worker: Option<tokio::task::JoinHandle<()>>,
    #[cfg(test)]
    armed: Arc<AtomicU64>,
}

impl Default for ToolRefreshDebounce {
    fn default() -> Self {
        Self {
            peer: None,
            cancellation: CancellationToken::new(),
            notify: Arc::new(Notify::new()),
            worker: None,
            #[cfg(test)]
            armed: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl ToolRefreshDebounce {
    fn cancel(&mut self) {
        self.peer = None;
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

impl Drop for ServerRuntime {
    fn drop(&mut self) {
        self.tool_refresh
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cancel();
    }
}

struct RegistryInner {
    servers: Mutex<BTreeMap<String, Arc<ServerRuntime>>>,
    reserved_names: Mutex<HashSet<String>>,
    claimed_names: Mutex<HashMap<String, String>>,
    /// The daemon's OAuth credential store; without one no server uses
    /// OAuth.
    #[cfg(feature = "oauth")]
    oauth_credentials: Option<oauth::OAuthCredentialFile>,
    /// Told about every change and failure, besides the subscribers.
    observer: Option<Arc<dyn McpObserver>>,
    shutdown: CancellationToken,
    connection_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    active_connects: AtomicUsize,
    connects_idle: Notify,
    eager_pending: AtomicUsize,
    eager_ready: Notify,
    /// Bumped on every server configuration or status change.
    changes: tokio::sync::watch::Sender<u64>,
    /// Connection failures, for clients viewing affected sessions.
    failures: tokio::sync::broadcast::Sender<McpServerFailure>,
    /// How a dropped connection is re-established.
    reconnect: Mutex<ReconnectPolicy>,
    /// Source of [`LiveConnection::id`].
    connection_ids: AtomicU64,
    /// [`McpRegistryOptions::working_dir`].
    working_dir: Option<std::path::PathBuf>,
}

impl RegistryInner {
    fn changed(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        if let Some(observer) = &self.observer {
            observer.changed();
        }
    }

    fn failed(&self, failure: McpServerFailure) {
        if let Some(observer) = &self.observer {
            observer.failed(&failure);
        }
        let _ = self.failures.send(failure);
    }
}

/// Learns about a registry's changes as they happen, synchronously, such as
/// the daemon merging the servers of every runtime into one stream.
pub trait McpObserver: Send + Sync {
    /// A server's configuration or status changed.
    fn changed(&self);
    /// A server failed to connect, or dropped and did not reconnect.
    fn failed(&self, failure: &McpServerFailure);
}

/// How [`McpRegistry::new`] opens a registry.
#[derive(Default)]
pub struct McpRegistryOptions {
    /// The OAuth credential store (`mcp-oauth.json`). Without one, remote
    /// servers connect without OAuth.
    #[cfg(feature = "oauth")]
    pub oauth_store: Option<std::path::PathBuf>,
    pub observer: Option<Arc<dyn McpObserver>>,
    /// Where a stdio server runs when its configuration names no `cwd`,
    /// and what a relative `cwd` resolves against: the hosting instance's
    /// workspace, or a home directory. Without one, a server with no `cwd`
    /// inherits this process's working directory.
    pub working_dir: Option<std::path::PathBuf>,
}

#[derive(Clone)]
pub struct McpRegistry {
    inner: Arc<RegistryInner>,
}

impl std::fmt::Debug for McpRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpRegistry")
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct McpClientHandler {
    server: Weak<ServerRuntime>,
}

struct ActiveConnect(Arc<RegistryInner>);

impl ActiveConnect {
    fn new(registry: Arc<RegistryInner>) -> Self {
        registry.active_connects.fetch_add(1, Ordering::AcqRel);
        Self(registry)
    }
}

impl Drop for ActiveConnect {
    fn drop(&mut self) {
        if self.0.active_connects.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.connects_idle.notify_waiters();
        }
    }
}

struct EagerConnect(Arc<RegistryInner>);

impl Drop for EagerConnect {
    fn drop(&mut self) {
        if self.0.eager_pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.eager_ready.notify_waiters();
        }
    }
}

#[derive(Clone)]
struct ChildCleanup(Arc<ChildCleanupInner>);

struct ChildCleanupInner {
    done: AtomicBool,
    notify: Notify,
}

impl ChildCleanup {
    fn new() -> Self {
        Self(Arc::new(ChildCleanupInner {
            done: AtomicBool::new(false),
            notify: Notify::new(),
        }))
    }

    fn finish(&self) {
        self.0.done.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }

    async fn wait(&self) {
        while !self.0.done.load(Ordering::Acquire) {
            let notified = self.0.notify.notified();
            if self.0.done.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
    }
}

struct ReapingChildTransport {
    inner: Option<TokioChildProcess>,
    cleanup: ChildCleanup,
}

impl ReapingChildTransport {
    fn new(inner: TokioChildProcess, cleanup: ChildCleanup) -> Self {
        Self {
            inner: Some(inner),
            cleanup,
        }
    }
}

impl Transport<RoleClient> for ReapingChildTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner
            .as_mut()
            .expect("open child transport")
            .send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.inner.as_mut().expect("open child transport").receive()
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let inner = self.inner.take();
        let cleanup = self.cleanup.clone();
        async move {
            let result = match inner {
                Some(mut inner) => inner.graceful_shutdown().await,
                None => Ok(()),
            };
            cleanup.finish();
            result
        }
    }
}

impl Drop for ReapingChildTransport {
    fn drop(&mut self) {
        let Some(mut inner) = self.inner.take() else {
            return;
        };
        let cleanup = self.cleanup.clone();
        tokio::spawn(async move {
            let _ = inner.graceful_shutdown().await;
            cleanup.finish();
        });
    }
}

impl ClientHandler for McpClientHandler {
    fn create_message(
        &self,
        _params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<CreateMessageResult, ErrorData>> + Send + '_ {
        std::future::ready(Err(unsupported_server_request("sampling/createMessage")))
    }

    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ListRootsResult, ErrorData>> + Send + '_ {
        std::future::ready(Err(unsupported_server_request("roots/list")))
    }

    fn create_elicitation(
        &self,
        _request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ElicitResult, ErrorData>> + Send + '_ {
        std::future::ready(Err(unsupported_server_request("elicitation/create")))
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let Some(server) = self.server.upgrade() else {
            return;
        };
        let message = params.message.unwrap_or_else(|| match params.total {
            Some(total) => format!("MCP progress: {} / {total}", params.progress),
            None => format!("MCP progress: {}", params.progress),
        });
        let sinks = server
            .progress
            .lock()
            .await
            .iter()
            .map(|(id, sink)| (*id, sink.clone()))
            .collect::<Vec<_>>();
        for (tool_call_id, sink) in sinks {
            let _ = sink
                .send(ToolProgress {
                    output: Vec::new(),
                    tool_call_id,
                    message: message.clone(),
                    display: Some(DisplayDelta::Append {
                        index: 0,
                        text: safe_title(&message).to_string(),
                        stream: None,
                    }),
                })
                .await;
        }
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        let Some(server) = self.server.upgrade() else {
            return;
        };
        server.schedule_tool_refresh(context.peer);
    }
}

fn unsupported_server_request(method: &str) -> ErrorData {
    ErrorData::new(
        ErrorCode::METHOD_NOT_FOUND,
        format!("unsupported MCP server request `{method}`"),
        None,
    )
}

/// A stdio server's working directory: `cwd` when absolute, `cwd` under
/// `base` when relative, else `base`.
fn resolve_working_dir(
    cwd: Option<&str>,
    base: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    match (cwd.map(std::path::Path::new), base) {
        (Some(cwd), Some(base)) if cwd.is_relative() => Some(base.join(cwd)),
        (Some(cwd), _) => Some(cwd.to_owned()),
        (None, base) => base.map(std::path::Path::to_owned),
    }
}

impl McpRegistry {
    /// A registry for `servers`; [`Self::start_eager`] connects them.
    /// Server names must stay distinct after sanitization.
    pub fn new(
        servers: BTreeMap<String, LoadedMcpServer>,
        options: McpRegistryOptions,
    ) -> Result<Self, ToolError> {
        #[cfg(feature = "oauth")]
        let oauth_credentials = options
            .oauth_store
            .map(oauth::OAuthCredentialFile::open)
            .transpose()?;
        let inner = Arc::new(RegistryInner {
            servers: Mutex::new(BTreeMap::new()),
            // Names of the hosting runtime's other tools; collisions with
            // other runtimes and the base layer are resolved by prefixing.
            reserved_names: Mutex::new(HashSet::new()),
            claimed_names: Mutex::new(HashMap::new()),
            #[cfg(feature = "oauth")]
            oauth_credentials,
            observer: options.observer,
            shutdown: CancellationToken::new(),
            connection_tasks: Mutex::new(Vec::new()),
            active_connects: AtomicUsize::new(0),
            connects_idle: Notify::new(),
            eager_pending: AtomicUsize::new(0),
            eager_ready: Notify::new(),
            changes: tokio::sync::watch::channel(0).0,
            failures: tokio::sync::broadcast::channel(64).0,
            reconnect: Mutex::new(ReconnectPolicy::default()),
            connection_ids: AtomicU64::new(1),
            working_dir: options.working_dir,
        });
        let mut sanitized_servers = HashMap::new();
        let mut runtimes = BTreeMap::new();
        for (name, loaded) in servers {
            let sanitized_name = sanitize_name(&name);
            if let Some(existing) = sanitized_servers.insert(sanitized_name.clone(), name.clone()) {
                return Err(ToolError::execution(format!(
                    "MCP server `{name}` collides with server `{existing}` after name sanitization"
                )));
            }
            let state = if loaded.config.enabled {
                McpServerState::Disconnected
            } else {
                McpServerState::Disabled
            };
            let runtime =
                ServerRuntime::new(name.clone(), sanitized_name, loaded, state, None, &inner);
            runtimes.insert(name, runtime);
        }
        *inner.servers.lock().unwrap_or_else(|p| p.into_inner()) = runtimes;
        Ok(Self { inner })
    }

    /// Reserves the names of `provider`'s tools, the hosting runtime's
    /// other tools: an MCP server that generates one of them fails.
    pub fn reserve_provider(&self, provider: &dyn ToolProvider) -> Result<(), ToolError> {
        let Ok(tools) = provider.tools_for_session(&SessionToolContext::new(
            cookie_agent_protocol::SessionId::new_v7(),
        )) else {
            // Session-dependent providers are checked when they publish tools.
            return Ok(());
        };
        let mut reserved = self
            .inner
            .reserved_names
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let claimed = self
            .inner
            .claimed_names
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for tool in tools {
            if let Some(server) = claimed.get(&tool.name) {
                return Err(ToolError::execution(format!(
                    "MCP server `{server}` generated tool name `{}` which collides with a registered tool",
                    tool.name
                )));
            }
            reserved.insert(tool.name);
        }
        Ok(())
    }

    /// Connects every enabled server that has not connected yet, on
    /// `runtime`. [`Self::await_eager_ready`] waits for these attempts.
    pub fn start_eager(&self, runtime: &tokio::runtime::Handle) {
        let servers = self
            .servers()
            .into_iter()
            .filter(|server| {
                server.loaded.config.enabled
                    && server.current_state() == McpServerState::Disconnected
            })
            .collect::<Vec<_>>();
        self.inner
            .eager_pending
            .fetch_add(servers.len(), Ordering::AcqRel);
        for server in servers {
            let registry = Arc::clone(&self.inner);
            let task_registry = Arc::clone(&registry);
            let eager = EagerConnect(Arc::clone(&task_registry));
            let task = runtime.spawn(async move {
                let _eager = eager;
                if let Err(error) = server.connect().await
                    && !task_registry.shutdown.is_cancelled()
                {
                    server.fail(error.message());
                }
            });
            registry
                .connection_tasks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(task);
        }
    }

    /// Retries every enabled server whose last connection attempt failed, as
    /// when a session that uses them is created or opened. The retries count
    /// as startup connections: the next run waits for them to finish.
    pub fn retry_failed(&self, runtime: &tokio::runtime::Handle) {
        for server in self.servers() {
            if !server.loaded.config.enabled || server.current_state() != McpServerState::Failed {
                continue;
            }
            // Visible at once, so a client that looks right after the
            // session opened sees the attempt rather than the old failure.
            server.set_status(McpServerState::Connecting, None);
            self.inner.eager_pending.fetch_add(1, Ordering::AcqRel);
            let readiness = EagerConnect(Arc::clone(&self.inner));
            let _ = self.spawn_connection(server, runtime, Some(readiness));
        }
    }

    /// Connection failures as they happen.
    #[must_use]
    pub fn subscribe_failures(&self) -> tokio::sync::broadcast::Receiver<McpServerFailure> {
        self.inner.failures.subscribe()
    }

    /// Replaces how dropped connections are re-established, for tests that
    /// cannot wait for the production backoff.
    pub fn set_reconnect_policy(&self, policy: ReconnectPolicy) {
        *self
            .inner
            .reconnect
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = policy;
    }

    /// Waits until every connection attempt [`Self::start_eager`] and
    /// [`Self::retry_failed`] started has finished, successfully or not.
    pub async fn await_eager_ready(&self) {
        while self.inner.eager_pending.load(Ordering::Acquire) != 0 {
            let notified = self.inner.eager_ready.notified();
            if self.inner.eager_pending.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }
    }

    /// Stops every server and waits for connection attempts to end.
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        for server in self.servers() {
            server.cancel_tool_refresh();
        }
        let tasks = self
            .inner
            .connection_tasks
            .lock()
            .map(|mut tasks| tasks.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for mut task in tasks {
            if tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        for server in self.servers() {
            let mut service = server.service.lock().await;
            if let Some(service) = service.as_mut() {
                let _ = service.close_with_timeout(Duration::from_secs(4)).await;
            }
            *service = None;
        }
        while self.inner.active_connects.load(Ordering::Acquire) != 0 {
            let notified = self.inner.connects_idle.notified();
            if self.inner.active_connects.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }
    }

    /// A counter bumped on every server configuration or status change.
    #[must_use]
    pub fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }

    /// The status of every server, by name.
    #[must_use]
    pub fn statuses(&self) -> Vec<McpServerStatus> {
        self.servers()
            .into_iter()
            .map(|server| {
                server
                    .status
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
            })
            .collect()
    }

    /// Adds `name`, or replaces its definition, and connects it when it is
    /// enabled.
    pub async fn upsert_server(
        &self,
        name: String,
        loaded: LoadedMcpServer,
    ) -> Result<(), ToolError> {
        let sanitized_name = sanitize_name(&name);
        let previous = {
            let servers = self
                .inner
                .servers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(existing) = servers
                .values()
                .find(|server| server.name != name && server.sanitized_name == sanitized_name)
            {
                return Err(ToolError::execution(format!(
                    "MCP server `{name}` collides with server `{}` after name sanitization",
                    existing.name
                )));
            }
            servers.get(&name).cloned()
        };
        #[cfg(feature = "oauth")]
        let (previous_auth, needs_auth_after_edit) = self
            .replace_credentials(previous.as_ref(), &name, &loaded)
            .await?;
        #[cfg(not(feature = "oauth"))]
        let needs_auth_after_edit = {
            if let Some(previous) = &previous {
                previous.supersede();
            }
            false
        };
        let state = if !loaded.config.enabled {
            McpServerState::Disabled
        } else if needs_auth_after_edit {
            McpServerState::NeedsAuth
        } else {
            McpServerState::Disconnected
        };
        let message = (state == McpServerState::NeedsAuth).then(|| {
            "OAuth credentials invalidated by MCP server configuration change; authenticate to connect".into()
        });
        let runtime = ServerRuntime::new(
            name.clone(),
            sanitized_name,
            loaded,
            state,
            message,
            &self.inner,
        );
        {
            let mut servers = self
                .inner
                .servers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            servers.insert(name, Arc::clone(&runtime));
        }
        #[cfg(feature = "oauth")]
        drop(previous_auth);
        if let Some(previous) = previous
            && let Some(mut service) = previous.service.lock().await.take()
        {
            let _ = service.close_with_timeout(Duration::from_secs(4)).await;
        }
        if runtime.loaded.config.enabled && runtime.current_state() == McpServerState::Disconnected
        {
            let handle = tokio::runtime::Handle::try_current()
                .map_err(|_| ToolError::execution("MCP runtime is unavailable"))?;
            self.spawn_connection(runtime, &handle, None)?;
        }
        self.inner.changed();
        Ok(())
    }

    /// Removes `name`, closing its connection (and forgetting its OAuth
    /// credential).
    pub async fn remove_server(&self, name: &str) -> Result<(), ToolError> {
        let server = self.server(name)?;
        #[cfg(feature = "oauth")]
        let _auth = self.forget_credentials(&server).await?;
        server.supersede();
        let removed = {
            let mut servers = self
                .inner
                .servers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let removed = servers
                .get(name)
                .cloned()
                .ok_or_else(|| ToolError::execution(format!("unknown MCP server `{name}`")))?;
            servers.remove(name);
            removed
        };
        if let Some(mut service) = removed.service.lock().await.take() {
            let _ = service.close_with_timeout(Duration::from_secs(4)).await;
        }
        #[cfg(feature = "oauth")]
        removed.end_auth_flow();
        self.inner.changed();
        Ok(())
    }

    /// Closes `name`'s connection and connects it again now, waiting for
    /// the attempt.
    pub async fn reconnect_server(&self, name: &str) -> Result<(), ToolError> {
        let server = self.server(name)?;
        if !server.loaded.config.enabled {
            return Err(ToolError::execution(format!(
                "MCP server `{name}` is disabled"
            )));
        }
        server.cancel_tool_refresh();
        server.retire_connection();
        server.clear_claims();
        server
            .tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        if let Some(mut service) = server.service.lock().await.take() {
            let _ = service.close_with_timeout(Duration::from_secs(4)).await;
        }
        server.set_status(McpServerState::Disconnected, None);
        match server.connect().await {
            Ok(()) => Ok(()),
            Err(error) => {
                server.fail(error.message());
                Err(error)
            }
        }
    }

    fn spawn_connection(
        &self,
        server: Arc<ServerRuntime>,
        runtime: &tokio::runtime::Handle,
        readiness: Option<EagerConnect>,
    ) -> Result<(), ToolError> {
        let registry = Arc::clone(&self.inner);
        let mut tasks = registry
            .connection_tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if registry.shutdown.is_cancelled() {
            return Err(ToolError::execution("MCP registry is shutting down"));
        }
        let task_registry = Arc::clone(&registry);
        let task = runtime.spawn(async move {
            let _readiness = readiness;
            if let Err(error) = server.connect().await
                && !task_registry.shutdown.is_cancelled()
            {
                server.fail(error.message());
            }
        });
        tasks.push(task);
        Ok(())
    }

    fn servers(&self) -> Vec<Arc<ServerRuntime>> {
        self.inner
            .servers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect()
    }

    fn server(&self, name: &str) -> Result<Arc<ServerRuntime>, ToolError> {
        self.inner
            .servers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::execution(format!("unknown MCP server `{name}`")))
    }

    fn resolve_tool(&self, tool_name: &str) -> Result<(Arc<ServerRuntime>, String), ToolError> {
        for server in self.servers() {
            if let Some(raw) = server.raw_tool_name(tool_name) {
                return Ok((server, raw));
            }
        }
        Err(ToolError::execution(format!(
            "unknown MCP tool `{tool_name}`"
        )))
    }
}

impl ServerRuntime {
    fn new(
        name: String,
        sanitized_name: String,
        loaded: LoadedMcpServer,
        state: McpServerState,
        message: Option<String>,
        registry: &Arc<RegistryInner>,
    ) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(McpServerStatus {
                server: name.clone(),
                state,
                message,
                tools: Vec::new(),
                auth_in_progress: false,
            }),
            name,
            sanitized_name,
            loaded,
            tools: Mutex::new(Vec::new()),
            list_generation: AtomicU64::new(0),
            publication: Mutex::new(()),
            tool_refresh: Mutex::new(ToolRefreshDebounce::default()),
            connect_lock: AsyncMutex::new(()),
            service: AsyncMutex::new(None),
            superseded: CancellationToken::new(),
            registry: Arc::downgrade(registry),
            progress: AsyncMutex::new(HashMap::new()),
            #[cfg(feature = "oauth")]
            auth: oauth::ServerAuth::default(),
            connection: Mutex::new(None),
        })
    }

    /// Whether the server authenticates with OAuth: a remote server whose
    /// OAuth is not disabled or overridden by a static `Authorization`
    /// header, in a registry with a credential store.
    #[cfg(feature = "oauth")]
    fn oauth_enabled(&self) -> bool {
        self.registry
            .upgrade()
            .is_some_and(|registry| registry.oauth_credentials.is_some())
            && oauth::oauth_enabled_config(&self.loaded.config)
    }

    /// Marks the server `needs_auth` when `error` is an OAuth authorization
    /// failure of an OAuth server, and returns the error to report then.
    fn service_auth_failure(&self, error: &rmcp::service::ServiceError) -> Option<ToolError> {
        #[cfg(feature = "oauth")]
        if self.oauth_enabled() && oauth::service_error_requires_auth(error) {
            self.mark_needs_auth(oauth::auth_challenge(error), error);
            return Some(self.needs_auth_error());
        }
        let _ = error;
        None
    }

    /// [`Self::service_auth_failure`] for a failed MCP initialization.
    fn initialize_auth_failure(
        &self,
        error: &rmcp::service::ClientInitializeError,
    ) -> Option<ToolError> {
        #[cfg(feature = "oauth")]
        if self.oauth_enabled() && oauth::initialize_error_requires_auth(error) {
            self.mark_needs_auth(error.auth_challenge().map(str::to_owned), error);
            return Some(self.needs_auth_error());
        }
        let _ = error;
        None
    }

    #[cfg(feature = "oauth")]
    fn needs_auth_error(&self) -> ToolError {
        ToolError::execution(format!(
            "MCP server `{}` requires OAuth authorization",
            self.name
        ))
    }

    /// Where the server's process starts: its configured `cwd`, resolved
    /// against the registry's working directory when relative, else that
    /// working directory; `None` inherits this process's.
    fn working_dir(&self) -> Option<std::path::PathBuf> {
        let base = self
            .registry
            .upgrade()
            .and_then(|registry| registry.working_dir.clone());
        resolve_working_dir(self.loaded.config.cwd.as_deref(), base.as_deref())
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.loaded.config.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS))
    }

    fn current_state(&self) -> McpServerState {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state
            .clone()
    }

    fn set_status(&self, state: McpServerState, message: Option<String>) {
        {
            let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
            status.state = state;
            status.message = message;
            status.tools = self
                .tools
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .map(|tool| tool.spec.name.clone())
                .collect();
        }
        self.notify_changed();
    }

    fn notify_changed(&self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.changed();
        }
    }

    fn fail(&self, message: String) {
        let message = cookie_agent_protocol::diagnostics::sanitize(&message, 4096);
        if self.superseded.is_cancelled() {
            return;
        }
        if self.current_state() == McpServerState::NeedsAuth {
            return;
        }
        self.clear_claims();
        self.tools.lock().unwrap_or_else(|p| p.into_inner()).clear();
        self.set_status(McpServerState::Failed, Some(message.clone()));
        if let Some(registry) = self.registry.upgrade() {
            registry.failed(McpServerFailure {
                server: self.name.clone(),
                reason: message,
            });
        }
    }

    /// Forgets the installed connection without treating its end as a drop.
    fn retire_connection(&self) {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }

    /// Reports that the installed connection's transport is gone.
    fn signal_lost(&self) {
        if let Some(connection) = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            connection.lost.cancel();
        }
    }

    /// Installs `lost` as the current connection's drop signal and spawns
    /// the task that reconnects when it fires.
    fn watch_connection(self: &Arc<Self>, registry: &Arc<RegistryInner>, lost: CancellationToken) {
        let id = registry.connection_ids.fetch_add(1, Ordering::Relaxed);
        let retired = CancellationToken::new();
        *self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(LiveConnection {
            id,
            lost: lost.clone(),
            retired: retired.clone(),
        });
        let server = Arc::downgrade(self);
        let shutdown = registry.shutdown.clone();
        let superseded = self.superseded.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                () = lost.cancelled() => {}
                () = retired.cancelled() => return,
                () = shutdown.cancelled() => return,
                () = superseded.cancelled() => return,
            }
            if let Some(server) = server.upgrade() {
                server.recover(id).await;
            }
        });
        let mut tasks = registry
            .connection_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    /// Bridges the drop of connection `id`: the server's tools stay
    /// published while [`ReconnectPolicy`] re-establishes it, and are
    /// dropped when every attempt fails.
    async fn recover(self: Arc<Self>, id: u64) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        {
            let mut connection = self
                .connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if connection.as_ref().map(|connection| connection.id) != Some(id) {
                return;
            }
            *connection = None;
        }
        if registry.shutdown.is_cancelled() || self.superseded.is_cancelled() {
            return;
        }
        if let Some(mut dead) = self.service.lock().await.take() {
            let _ = dead.close_with_timeout(Duration::from_secs(1)).await;
        }
        self.cancel_tool_refresh_task();
        self.set_status(
            McpServerState::Reconnecting,
            Some("connection lost; reconnecting".into()),
        );
        let policy = registry
            .reconnect
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let outcome = tokio::select! {
            outcome = policy.run(&registry.shutdown, |attempt| {
                let server = Arc::clone(&self);
                async move {
                    // A manual reconnect, edit, or auth challenge took over.
                    if server.current_state() != McpServerState::Reconnecting {
                        return Ok(());
                    }
                    server.set_status(
                        McpServerState::Reconnecting,
                        Some(format!("connection lost; reconnect attempt {attempt}")),
                    );
                    server.connect().await.map_err(|error| error.message())
                }
            }) => outcome,
            () = self.superseded.cancelled() => ReconnectOutcome::Cancelled,
        };
        if let ReconnectOutcome::Exhausted {
            attempts,
            last_error,
        } = outcome
            && self.current_state() == McpServerState::Reconnecting
        {
            self.fail(format!(
                "MCP server `{}` disconnected and did not reconnect after {attempts} attempts: {last_error}",
                self.name
            ));
        }
    }

    fn supersede(&self) {
        self.superseded.cancel();
        self.cancel_tool_refresh_task();
        #[cfg(feature = "oauth")]
        self.end_auth_flow();
        self.next_list_generation();
        self.clear_claims();
    }

    fn clear_claims(&self) {
        if let Some(registry) = self.registry.upgrade() {
            registry
                .claimed_names
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|_, owner| owner != &self.name);
        }
    }

    fn raw_tool_name(&self, generated: &str) -> Option<String> {
        self.tools
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|tool| tool.spec.name == generated)
            .map(|tool| tool.raw_name.clone())
    }

    async fn connect(self: &Arc<Self>) -> Result<(), ToolError> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| ToolError::execution("MCP registry is unavailable"))?;
        if registry.shutdown.is_cancelled() {
            return Err(ToolError::execution("MCP registry is shutting down"));
        }
        if self.superseded.is_cancelled() {
            return Err(self.superseded_error());
        }
        let _active = ActiveConnect::new(Arc::clone(&registry));
        let _connect = tokio::select! {
            lock = self.connect_lock.lock() => lock,
            () = registry.shutdown.cancelled() => {
                return Err(ToolError::execution("MCP registry is shutting down"));
            }
            () = self.superseded.cancelled() => {
                return Err(self.superseded_error());
            }
        };
        if registry.shutdown.is_cancelled() {
            return Err(ToolError::execution("MCP registry is shutting down"));
        }
        if self.superseded.is_cancelled() {
            return Err(self.superseded_error());
        }
        match self.current_state() {
            McpServerState::Connected => return Ok(()),
            McpServerState::Disabled => {
                return Err(ToolError::execution(format!(
                    "MCP server `{}` is disabled",
                    self.name
                )));
            }
            McpServerState::NeedsAuth => {
                return Err(ToolError::execution(format!(
                    "MCP server `{}` requires OAuth authorization",
                    self.name
                )));
            }
            McpServerState::Disconnected
            | McpServerState::Connecting
            | McpServerState::Reconnecting
            | McpServerState::Failed => {}
        }
        self.reset_tool_refresh();
        // A reconnect keeps its state, so the tools stay published.
        if self.current_state() != McpServerState::Reconnecting {
            self.set_status(McpServerState::Connecting, None);
        }
        let lost = CancellationToken::new();
        let handler = McpClientHandler {
            server: Arc::downgrade(self),
        };
        let lifecycle = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        };
        let timeout = self.timeout();
        let mut service = if let Some(command) = &self.loaded.config.command {
            let mut process = tokio::process::Command::new(command);
            process
                .args(&self.loaded.config.args)
                .envs(&self.loaded.config.env);
            if let Some(cwd) = self.working_dir() {
                process.current_dir(cwd);
            }
            let mut wrapped = CommandWrap::from(process);
            #[cfg(unix)]
            wrapped.wrap(ProcessGroup::leader());
            #[cfg(windows)]
            wrapped.wrap(JobObject);
            let transport = TokioChildProcess::new(wrapped).map_err(|error| {
                ToolError::execution(format!(
                    "MCP server `{}` failed to spawn command `{command}`: {error}",
                    self.name,
                ))
            })?;
            let cleanup = ChildCleanup::new();
            let transport = WatchedTransport {
                inner: ReapingChildTransport::new(transport, cleanup.clone()),
                lost: lost.clone(),
            };
            let outcome = tokio::select! {
                result = tokio::time::timeout(timeout, handler.serve_with_lifecycle(transport, lifecycle)) => Some(result),
                () = registry.shutdown.cancelled() => None,
                () = self.superseded.cancelled() => None,
            };
            match outcome {
                Some(Ok(Ok(service))) => service,
                Some(Ok(Err(error))) => {
                    cleanup.wait().await;
                    return Err(ToolError::execution(format!(
                        "MCP server `{}` connect failed: {error}",
                        self.name
                    )));
                }
                Some(Err(_)) => {
                    cleanup.wait().await;
                    return Err(ToolError::execution(format!(
                        "MCP server `{}` connect timed out",
                        self.name
                    )));
                }
                None => {
                    cleanup.wait().await;
                    return if self.superseded.is_cancelled() {
                        Err(self.superseded_error())
                    } else {
                        Err(ToolError::execution("MCP registry is shutting down"))
                    };
                }
            }
        } else {
            let url = self
                .loaded
                .config
                .url
                .as_ref()
                .expect("validated MCP transport");
            let mut headers = HashMap::new();
            for (name, value) in &self.loaded.config.headers {
                headers.insert(
                    http::HeaderName::try_from(name)
                        .map_err(|error| ToolError::execution(error.to_string()))?,
                    http::HeaderValue::try_from(value)
                        .map_err(|error| ToolError::execution(error.to_string()))?,
                );
            }
            let config = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.clone())
                .custom_headers(headers);
            macro_rules! serve {
                ($transport:expr) => {
                    tokio::select! {
                        result = tokio::time::timeout(timeout, handler.serve_with_lifecycle($transport, lifecycle)) => result,
                        () = registry.shutdown.cancelled() => {
                            return Err(ToolError::execution("MCP registry is shutting down"));
                        }
                        () = self.superseded.cancelled() => {
                            return Err(self.superseded_error());
                        }
                    }
                };
            }
            #[cfg(feature = "oauth")]
            let result = if self.oauth_enabled() {
                serve!(WatchedTransport {
                    inner: self.oauth_transport(config).await?,
                    lost: lost.clone(),
                })
            } else {
                serve!(WatchedTransport {
                    inner: StreamableHttpClientTransport::from_config(config),
                    lost: lost.clone(),
                })
            };
            #[cfg(not(feature = "oauth"))]
            let result = serve!(WatchedTransport {
                inner: StreamableHttpClientTransport::from_config(config),
                lost: lost.clone(),
            });
            match result {
                Err(_) => {
                    return Err(ToolError::execution(format!(
                        "MCP server `{}` connect timed out",
                        self.name
                    )));
                }
                Ok(Err(error)) => {
                    if let Some(error) = self.initialize_auth_failure(&error) {
                        return Err(error);
                    }
                    return Err(ToolError::execution(format!(
                        "MCP server `{}` connect failed: {error}",
                        self.name
                    )));
                }
                Ok(Ok(service)) => service,
            }
        };
        let generation = self.next_list_generation();
        let tools = tokio::select! {
            result = tokio::time::timeout(timeout, service.peer().list_all_tools()) => result,
            () = registry.shutdown.cancelled() => {
                let _ = service.close_with_timeout(Duration::from_secs(4)).await;
                return Err(ToolError::execution("MCP registry is shutting down"));
            }
            () = self.superseded.cancelled() => {
                let _ = service.close_with_timeout(Duration::from_secs(4)).await;
                return Err(self.superseded_error());
            }
        }
        .map_err(|_| {
            ToolError::execution(format!("MCP server `{}` tools/list timed out", self.name))
        })?;
        let tools = match tools {
            Ok(tools) => tools,
            Err(error) => {
                if let Some(error) = self.service_auth_failure(&error) {
                    let _ = service.close_with_timeout(Duration::from_secs(4)).await;
                    return Err(error);
                }
                return Err(ToolError::execution(format!(
                    "MCP server `{}` tools/list failed: {error}",
                    self.name
                )));
            }
        };
        self.publish_tools(generation, tools);
        if self.current_state() == McpServerState::Failed {
            return Err(ToolError::execution(
                self.status
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .message
                    .clone()
                    .unwrap_or_default(),
            ));
        }
        let mut installed = self.service.lock().await;
        if registry.shutdown.is_cancelled() || self.superseded.is_cancelled() {
            drop(installed);
            let _ = service.close_with_timeout(Duration::from_secs(4)).await;
            return if self.superseded.is_cancelled() {
                Err(self.superseded_error())
            } else {
                Err(ToolError::execution("MCP registry is shutting down"))
            };
        }
        *installed = Some(service);
        drop(installed);
        self.watch_connection(&registry, lost);
        self.set_status(McpServerState::Connected, None);
        Ok(())
    }

    fn next_list_generation(&self) -> u64 {
        let _publication = self.publication.lock().unwrap_or_else(|p| p.into_inner());
        self.list_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn schedule_tool_refresh(self: &Arc<Self>, peer: Peer<RoleClient>) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut refresh = self
            .tool_refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if refresh.cancellation.is_cancelled() {
            return;
        }
        refresh.peer = Some(peer);
        if refresh.worker.is_none() {
            let server = Arc::downgrade(self);
            let notify = Arc::clone(&refresh.notify);
            let cancellation = refresh.cancellation.clone();
            let shutdown = registry.shutdown.clone();
            let superseded = self.superseded.clone();
            #[cfg(test)]
            let armed = Arc::clone(&refresh.armed);
            refresh.worker = Some(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = notify.notified() => {}
                        () = cancellation.cancelled() => return,
                        () = shutdown.cancelled() => return,
                        () = superseded.cancelled() => return,
                    }
                    loop {
                        #[cfg(test)]
                        armed.fetch_add(1, Ordering::Release);
                        tokio::select! {
                            () = tokio::time::sleep(TOOL_LIST_DEBOUNCE) => break,
                            () = notify.notified() => {}
                            () = cancellation.cancelled() => return,
                            () = shutdown.cancelled() => return,
                            () = superseded.cancelled() => return,
                        }
                    }

                    let Some(server) = server.upgrade() else {
                        return;
                    };
                    if cancellation.is_cancelled() {
                        return;
                    }
                    let peer = {
                        let refresh = server
                            .tool_refresh
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if !Arc::ptr_eq(&refresh.notify, &notify) {
                            return;
                        }
                        refresh.peer.clone()
                    };
                    if let Some(peer) = peer
                        && !peer.is_transport_closed()
                    {
                        server.refresh_tools(peer, &cancellation, &shutdown).await;
                    }
                }
            }));
        }
        refresh.notify.notify_one();
    }

    async fn refresh_tools(
        &self,
        peer: Peer<RoleClient>,
        cancellation: &CancellationToken,
        shutdown: &CancellationToken,
    ) {
        let generation = self.next_list_generation();
        let timeout = self.timeout();
        let result = tokio::select! {
            result = tokio::time::timeout(timeout, peer.list_all_tools()) => result,
            () = cancellation.cancelled() => return,
            () = shutdown.cancelled() => return,
            () = self.superseded.cancelled() => return,
        };
        if cancellation.is_cancelled() || shutdown.is_cancelled() {
            return;
        }
        match result {
            Ok(Ok(tools)) => self.publish_tools(generation, tools),
            Ok(Err(error)) => {
                if self.service_auth_failure(&error).is_none() {
                    self.publish_refresh_failure(
                        generation,
                        format!("tools/list refresh failed: {error}"),
                    );
                }
            }
            Err(_) => {
                self.publish_refresh_failure(generation, "tools/list refresh timed out".into());
            }
        }
    }

    fn reset_tool_refresh(&self) {
        let mut refresh = self
            .tool_refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        refresh.cancel();
        *refresh = ToolRefreshDebounce::default();
    }

    fn cancel_tool_refresh_task(&self) {
        let mut refresh = self
            .tool_refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        refresh.cancel();
    }

    fn cancel_tool_refresh(&self) {
        self.cancel_tool_refresh_task();
        self.next_list_generation();
    }

    fn superseded_error(&self) -> ToolError {
        ToolError::execution(format!("MCP server `{}` was superseded", self.name))
    }

    fn publish_tools(&self, generation: u64, tools: Vec<Tool>) {
        self.publish_tools_before_commit(generation, tools, || {});
    }

    fn publish_refresh_failure(&self, generation: u64, message: String) {
        self.publish_refresh_failure_before_commit(generation, message, || {});
    }

    fn publish_refresh_failure_before_commit(
        &self,
        generation: u64,
        message: String,
        before_commit: impl FnOnce(),
    ) {
        before_commit();
        let _publication = self.publication.lock().unwrap_or_else(|p| p.into_inner());
        if self.list_generation.load(Ordering::Acquire) == generation {
            self.fail(message);
        }
    }

    fn publish_tools_before_commit(
        &self,
        generation: u64,
        tools: Vec<Tool>,
        before_commit: impl FnOnce(),
    ) {
        let converted = tools
            .into_iter()
            .map(|tool| convert_tool(&self.name, &self.sanitized_name, tool))
            .collect::<Result<Vec<_>, _>>();
        before_commit();
        let publication = self.publication.lock().unwrap_or_else(|p| p.into_inner());
        if self.superseded.is_cancelled()
            || self.list_generation.load(Ordering::Acquire) != generation
        {
            return;
        }
        let converted = match converted {
            Ok(tools) => tools,
            Err(error) => {
                self.fail(error.message());
                return;
            }
        };
        let unchanged = {
            let current = self
                .tools
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            current.len() == converted.len()
                && current.iter().zip(&converted).all(|(current, refreshed)| {
                    current.raw_name == refreshed.raw_name
                        && current.spec.name == refreshed.spec.name
                        && current.spec.description == refreshed.spec.description
                        && current.spec.parameters == refreshed.spec.parameters
                })
        };
        if unchanged {
            return;
        }
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let reserved = registry
            .reserved_names
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut claimed = registry
            .claimed_names
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        claimed.retain(|_, owner| owner != &self.name);
        let mut local = HashSet::new();
        for tool in &converted {
            if reserved.contains(&tool.spec.name) || !local.insert(tool.spec.name.clone()) {
                drop(claimed);
                drop(reserved);
                self.fail(format!(
                    "MCP server `{}` generated colliding tool name `{}`",
                    self.name, tool.spec.name
                ));
                return;
            }
            if claimed.contains_key(&tool.spec.name) {
                drop(claimed);
                drop(reserved);
                self.fail(format!(
                    "MCP server `{}` generated colliding tool name `{}`",
                    self.name, tool.spec.name
                ));
                return;
            }
        }
        for tool in &converted {
            claimed.insert(tool.spec.name.clone(), self.name.clone());
        }
        drop(claimed);
        drop(reserved);
        *self.tools.lock().unwrap_or_else(|p| p.into_inner()) = converted;
        drop(publication);
        if self.current_state() == McpServerState::Connected {
            self.set_status(McpServerState::Connected, None);
        }
    }
}

#[async_trait]
impl ToolProvider for McpRegistry {
    fn provider_id(&self) -> &'static str {
        "mcp"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(self
            .servers()
            .into_iter()
            .flat_map(|server| {
                server
                    .tools
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .iter()
                    .map(|tool| tool.spec.clone())
                    .collect::<Vec<_>>()
            })
            .collect())
    }

    fn get_permission_name(_tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        Ok(Some("mcp"))
    }

    fn get_permission_resource(
        &self,
        tool_name: &str,
        _arguments: &Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        let owned = self
            .servers()
            .into_iter()
            .any(|server| tool_name.starts_with(&format!("{}_", server.sanitized_name)));
        if !owned {
            return Err(ToolError::execution(format!(
                "unknown MCP tool `{tool_name}`"
            )));
        }
        Ok((Some("mcp"), Some(tool_name.to_owned())))
    }

    fn get_display_argument(&self, name: &str, _arguments: &Value) -> Result<String, ToolError> {
        Ok(name.to_owned())
    }

    fn presentation(&self, call: &ToolCall) -> PreparedPresentation {
        let presentation = cookie_agent_runtime_api::tool_presentation(&call.name, &call.name)
            .with_hint(ToolHint::GENERIC);
        match call.arguments.as_object() {
            Some(arguments) if !arguments.is_empty() => {
                presentation.with_arguments([DisplayBlock::Json {
                    value: call.arguments.clone(),
                }])
            }
            _ => presentation,
        }
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let (server, raw_name) = self.resolve_tool(&call.name)?;
        let arguments = call
            .arguments
            .as_object()
            .cloned()
            .ok_or_else(|| ToolError::execution("MCP tool arguments must be an object"))?;
        let normalized = Value::Object(arguments.clone());
        PreparedTool::declared(
            normalized,
            None,
            Box::new(McpExecutor {
                server,
                generated_name: call.name,
                raw_name,
                arguments,
                call_id: call.id,
            }),
        )
    }
}

struct McpExecutor {
    server: Arc<ServerRuntime>,
    generated_name: String,
    raw_name: String,
    arguments: Map<String, Value>,
    call_id: ToolCallId,
}

#[async_trait]
impl PreparedExecutor for McpExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        if self.server.raw_tool_name(&self.generated_name).as_deref() != Some(&self.raw_name) {
            return Err(ToolError::operation_changed(
                "MCP tool list changed after approval",
            ));
        }
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let result: Result<ToolResult, ToolError> = async move {
            self.revalidate().await?;
            let params = CallToolRequestParams::new(self.raw_name.clone())
                .with_arguments(self.arguments.clone());
            let timeout = self.server.timeout();
            if self.server.current_state() == McpServerState::Reconnecting {
                return Err(ToolError::runtime_unavailable(format!(
                    "MCP server `{}` is reconnecting",
                    self.server.name
                )));
            }
            // Another call may hold the connection; cancelling stops the wait.
            let service = tokio::select! {
                service = self.server.service.lock() => service,
                () = context.cancellation.cancelled() => {
                    return Err(ToolError::execution("MCP tool call cancelled"));
                }
            };
            let Some(service) = service.as_ref() else {
                return Err(ToolError::runtime_unavailable(format!(
                    "MCP server `{}` is not connected",
                    self.server.name
                )));
            };
            self.server
                .progress
                .lock()
                .await
                .insert(self.call_id, context.progress.clone());
            let result = self
                .call(service, params, timeout, &context.cancellation)
                .await;
            self.server.progress.lock().await.remove(&self.call_id);
            map_tool_result(&context, &self.generated_name, result?)
        }
        .await;
        result.map(|result| {
            let failed = result.metadata["mcp"]["is_error"] == true;
            let mut completion = ToolCompletion::single(result);
            completion.failed = failed;
            completion
        })
    }
}

impl McpExecutor {
    /// Sends the `tools/call` request and waits for its result. A call that
    /// is cancelled or times out tells the server with
    /// `notifications/cancelled` and stops waiting at once.
    async fn call(
        &self,
        service: &ClientService,
        params: CallToolRequestParams,
        timeout: Duration,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<CallToolResult, ToolError> {
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let sent = tokio::select! {
            sent = service.send_cancellable_request(request, PeerRequestOptions::no_options()) => sent,
            () = cancellation.cancelled() => return Err(ToolError::execution("MCP tool call cancelled")),
            () = &mut deadline => return Err(ToolError::execution("MCP tool call timed out")),
        };
        let mut handle = sent.map_err(|error| self.service_error(error))?;
        let (reply, reason) = tokio::select! {
            reply = &mut handle.rx => (Some(reply), None),
            () = cancellation.cancelled() => (None, Some("cancelled")),
            () = &mut deadline => (None, Some("timed out")),
        };
        let Some(reply) = reply else {
            let reason = reason.expect("a call stops waiting for a reason");
            // Telling the server is best effort and must not hold the call.
            let _ = tokio::time::timeout(
                MCP_CANCEL_NOTIFICATION_TIMEOUT,
                handle.cancel(Some(format!("tool call {reason}"))),
            )
            .await;
            return Err(ToolError::execution(format!("MCP tool call {reason}")));
        };
        let reply = reply
            .unwrap_or(Err(rmcp::ServiceError::TransportClosed))
            .map_err(|error| self.service_error(error))?;
        match reply {
            ServerResult::CallToolResult(result) => Ok(result),
            ServerResult::InputRequiredResult(_) => Err(ToolError::execution(
                "MCP server asked for input during the call, which is not supported",
            )),
            _ => Err(self.service_error(rmcp::ServiceError::UnexpectedResponse)),
        }
    }

    fn service_error(&self, error: rmcp::ServiceError) -> ToolError {
        match self.server.service_auth_failure(&error) {
            Some(error) => error,
            None if matches!(error, rmcp::ServiceError::TransportClosed) => {
                self.server.signal_lost();
                ToolError::runtime_unavailable(format!(
                    "MCP server `{}` disconnected during the call",
                    self.server.name
                ))
            }
            None => ToolError::execution(error.to_string()),
        }
    }
}

fn convert_tool(server: &str, sanitized_server: &str, tool: Tool) -> Result<CachedTool, ToolError> {
    let generated = format!("{}_{}", sanitized_server, sanitize_name(tool.name.as_ref()));
    let description = format!(
        "[{server}] {} (untrusted MCP output)",
        tool.description.as_deref().unwrap_or("MCP tool")
    );
    let mut schema = tool.input_schema.as_ref().clone();
    schema.insert("type".into(), Value::String("object".into()));
    schema.insert("additionalProperties".into(), Value::Bool(false));
    Ok(CachedTool {
        raw_name: tool.name.into_owned(),
        spec: ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: ToolConcurrency::Parallel,
            // External opt-out requires a future extension-protocol capability.
            result_truncation: Default::default(),
            name: generated,
            permission_name: Some("mcp".into()),
            description,
            parameters: Value::Object(schema),
        },
    })
}

fn map_tool_result(
    context: &ToolExecutionContext,
    tool_name: &str,
    result: rmcp::model::CallToolResult,
) -> Result<ToolResult, ToolError> {
    let mut output = Vec::new();
    // The retained attachment each placeholder in `output` stands for.
    let mut media = HashMap::new();
    let mut attachments = Vec::new();
    let mut emitted_attachments = Vec::new();
    for block in result.content {
        match block {
            ContentBlock::Text(text) => output.push(text.text),
            ContentBlock::Image(image) => {
                if !mcp_attachment_budget_remaining(&attachments, &emitted_attachments) {
                    output.push(
                        "[MCP image attachment omitted: combined attachment limit reached]".into(),
                    );
                    continue;
                }
                match retain_base64_attachment(context, image.mime_type, &image.data) {
                    Ok(retained) => {
                        let artifact = retained.attachment.artifact_ref();
                        if route_mcp_attachment(
                            retained,
                            &mut attachments,
                            &mut emitted_attachments,
                        ) {
                            output.push("[MCP image attachment]".into());
                            media.insert(output.len() - 1, artifact);
                        } else {
                            output.push(
                                "[MCP image attachment omitted: combined attachment limit reached]"
                                    .into(),
                            );
                        }
                    }
                    Err(McpAttachmentError::Invalid(error)) => {
                        output.push(format!("[Invalid MCP image content: {error}]"));
                    }
                    Err(McpAttachmentError::Gated(error)) => {
                        return Err(ToolError::Failed(error));
                    }
                }
            }
            ContentBlock::Audio(audio) => {
                if !mcp_attachment_budget_remaining(&attachments, &emitted_attachments) {
                    output.push(
                        "[MCP audio attachment omitted: combined attachment limit reached]".into(),
                    );
                    continue;
                }
                match retain_base64_attachment(context, audio.mime_type, &audio.data) {
                    Ok(retained) => {
                        let artifact = retained.attachment.artifact_ref();
                        if route_mcp_attachment(
                            retained,
                            &mut attachments,
                            &mut emitted_attachments,
                        ) {
                            output.push("[MCP audio attachment]".into());
                            media.insert(output.len() - 1, artifact);
                        } else {
                            output.push(
                                "[MCP audio attachment omitted: combined attachment limit reached]"
                                    .into(),
                            );
                        }
                    }
                    Err(McpAttachmentError::Invalid(error)) => {
                        output.push(format!("[Invalid MCP audio content: {error}]"));
                    }
                    Err(McpAttachmentError::Gated(error)) => {
                        return Err(ToolError::Failed(error));
                    }
                }
            }
            ContentBlock::Resource(resource) => match resource.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    output.push(format!("[{uri}]\n{text}"));
                }
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => {
                    if !mcp_attachment_budget_remaining(&attachments, &emitted_attachments) {
                        output.push(format!(
                            "[MCP embedded resource unavailable: {uri}: combined attachment limit reached]"
                        ));
                        continue;
                    }
                    match retain_base64_attachment(
                        context,
                        mime_type.unwrap_or_else(|| "application/octet-stream".into()),
                        &blob,
                    ) {
                        Ok(retained) => {
                            let artifact = retained.attachment.artifact_ref();
                            let routed = route_mcp_attachment(
                                retained,
                                &mut attachments,
                                &mut emitted_attachments,
                            );
                            output.push(if routed {
                                format!("[MCP embedded resource attachment: {uri}]")
                            } else {
                                format!(
                                    "[MCP embedded resource unavailable: {uri}: combined attachment limit reached]"
                                )
                            });
                            if routed {
                                media.insert(output.len() - 1, artifact);
                            }
                        }
                        Err(McpAttachmentError::Invalid(error)) => output.push(format!(
                            "[MCP embedded resource unavailable: {uri}: {error}]"
                        )),
                        Err(McpAttachmentError::Gated(error)) => {
                            return Err(ToolError::Failed(error));
                        }
                    }
                }
                _ => output.push("[Unsupported MCP embedded resource]".into()),
            },
            ContentBlock::ResourceLink(link) => {
                output.push(format!("[MCP resource link: {} ({})]", link.name, link.uri))
            }
            _ => output.push("[Unsupported MCP content block]".into()),
        }
    }
    let text_output = output.join("\n");
    let structured_content = result.structured_content.filter(|structured| {
        !output
            .iter()
            .chain(std::iter::once(&text_output))
            .any(|text| serde_json::from_str::<Value>(text).ok().as_ref() == Some(structured))
    });
    let mut metadata = serde_json::json!({
        "mcp": {
            "is_error": result.is_error.unwrap_or(false),
        }
    });
    let mut display = result_display(&output, &mut media);
    if let Some(structured) = structured_content {
        display.push(Open::Known(DisplayBlock::Json {
            value: structured.clone(),
        }));
        metadata["mcp"]["structured_content"] = structured;
    }
    let additional_messages = if emitted_attachments.is_empty() {
        Vec::new()
    } else {
        vec![
            ToolEmittedMessage::new(
                ToolEmittedMessageRole::User,
                emitted_attachments
                    .into_iter()
                    .map(ToolEmittedContent::File)
                    .collect(),
            )
            .map_err(|error| ToolError::execution(error.to_string()))?,
        ]
    };
    Ok(ToolResult {
        display: cookie_agent_protocol::fit_result_display(display),
        background: None,
        retained_output: None,
        title: safe_title(tool_name),
        output: text_output,
        metadata,
        attachments,
        additional_messages,
    })
}

/// What clients see of a result's `output` parts: consecutive text as one
/// `text` block, as the output joins it, and each retained attachment as the
/// `media` it is instead of its placeholder.
fn result_display(
    output: &[String],
    media: &mut HashMap<usize, cookie_agent_protocol::ArtifactRef>,
) -> Vec<Open<DisplayBlock>> {
    let mut display = Vec::new();
    for (index, part) in output.iter().enumerate() {
        if let Some(artifact) = media.remove(&index) {
            display.push(Open::Known(DisplayBlock::Media {
                artifact,
                alt: None,
            }));
            continue;
        }
        match display.last_mut() {
            Some(Open::Known(DisplayBlock::Text { text, .. })) => {
                text.push('\n');
                text.push_str(part);
            }
            _ => display.push(Open::Known(DisplayBlock::text(part.clone()))),
        }
    }
    display
}

#[derive(Debug)]
enum McpAttachmentError {
    /// Malformed or undecodable content; reported inline, the call still succeeds.
    Invalid(String),
    /// Rejected by the media gate; fails the tool result like `read` does.
    Gated(String),
}

#[derive(Debug)]
struct RetainedMcpAttachment {
    attachment: ToolAttachment,
    delivery: cookie_agent_runtime_api::media::AttachmentGate,
}

fn route_mcp_attachment(
    retained: RetainedMcpAttachment,
    attachments: &mut Vec<ToolAttachment>,
    emitted_attachments: &mut Vec<ToolAttachment>,
) -> bool {
    if !mcp_attachment_budget_remaining(attachments, emitted_attachments) {
        return false;
    }
    match retained.delivery {
        cookie_agent_runtime_api::media::AttachmentGate::AttachToolResult => {
            attachments.push(retained.attachment)
        }
        cookie_agent_runtime_api::media::AttachmentGate::DeliverViaUserTurn => {
            emitted_attachments.push(retained.attachment);
        }
        cookie_agent_runtime_api::media::AttachmentGate::RejectUnsupportedModel
        | cookie_agent_runtime_api::media::AttachmentGate::RejectUnsupportedFamily
        | cookie_agent_runtime_api::media::AttachmentGate::RejectTooLarge { .. } => {
            unreachable!("rejected MCP attachment gates return an error")
        }
    }
    true
}

fn mcp_attachment_budget_remaining(
    attachments: &[ToolAttachment],
    emitted_attachments: &[ToolAttachment],
) -> bool {
    attachments.len().saturating_add(emitted_attachments.len()) < ToolResult::MAX_ATTACHMENTS
}

fn retain_base64_attachment(
    context: &ToolExecutionContext,
    mime_type: String,
    data: &str,
) -> Result<RetainedMcpAttachment, McpAttachmentError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| McpAttachmentError::Invalid(error.to_string()))?;
    let mut delivery = cookie_agent_runtime_api::media::AttachmentGate::AttachToolResult;
    let sniffed =
        cookie_agent_runtime_api::media::approved_media_type(std::path::Path::new(""), &bytes)
            .map_err(|error| McpAttachmentError::Invalid(error.to_string()))?;
    let retained_mime = if mime_type.is_empty() || mime_type == "application/octet-stream" {
        sniffed.unwrap_or(mime_type.as_str())
    } else {
        mime_type.as_str()
    };
    let attachment = if let Some(mime) = sniffed {
        if cookie_agent_runtime_api::media::canonical_video_mime_type(mime)
            != cookie_agent_runtime_api::media::canonical_video_mime_type(retained_mime)
        {
            return Err(McpAttachmentError::Invalid(format!(
                "attachment MIME mismatch: declared {retained_mime}, validated {mime}"
            )));
        }
        let gate = cookie_agent_runtime_api::media::gate_attachment(
            &context.turn_context.input_media,
            mime,
            bytes.len() as u64,
        );
        if let Some(error) = cookie_agent_runtime_api::media::attachment_gate_error(
            gate,
            mime,
            &context.turn_context.model,
            &context.turn_context.adapter,
        ) {
            return Err(McpAttachmentError::Gated(error));
        }
        delivery = gate;
        context.retain_validated_attachment(retained_mime, None, &bytes)
    } else {
        context.retain_attachment(retained_mime, None, &bytes)
    }
    .map_err(|error| McpAttachmentError::Invalid(error.to_string()))?;
    Ok(RetainedMcpAttachment {
        attachment,
        delivery,
    })
}

fn sanitize_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
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
    SafeDisplayText::new(title).expect("sanitized MCP title")
}

#[cfg(test)]
mod tests;
