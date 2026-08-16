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
use cookie_agent_config::{LoadedMcpServer, McpServerConfig, McpServerSource};
use cookie_agent_protocol::{
    ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, PermissionAction,
    PersistedToolResult as ToolResult, PreparedApprovalResource, PreparedBindingLifetime,
    PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
    PreparedResourceIdentity, SafeDisplayText, Sha256Digest, ToolAttachment, ToolCallId,
};
use process_wrap::tokio::CommandWrap;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt,
    model::{
        CallToolRequestParams, ContentBlock, CreateMessageRequestParams, CreateMessageResult,
        ElicitRequestParams, ElicitResult, ErrorCode, ErrorData, ListRootsResult,
        ProgressNotificationParam, ProtocolVersion, ResourceContents, Tool,
    },
    service::{
        NotificationContext, RequestContext, RoleClient, RunningService, RxJsonRpcMessage,
        TxJsonRpcMessage,
    },
    transport::{StreamableHttpClientTransport, TokioChildProcess, Transport},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProgress, ToolProvider, ToolSpec,
};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    Disabled,
    PendingApproval,
    Disconnected,
    Connecting,
    Connected,
    Failed,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct McpServerStatus {
    pub server: String,
    pub state: McpServerState,
    pub message: Option<String>,
    pub tools: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct McpApprovalRequest {
    pub server: String,
    pub connection: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct McpNameTrustGrant {
    server: String,
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
    connect_lock: AsyncMutex<()>,
    service: AsyncMutex<Option<ClientService>>,
    superseded: CancellationToken,
    registry: Weak<RegistryInner>,
    progress: AsyncMutex<HashMap<ToolCallId, crate::ProgressSink>>,
}

struct RegistryInner {
    servers: Mutex<BTreeMap<String, Arc<ServerRuntime>>>,
    reserved_names: Mutex<HashSet<String>>,
    claimed_names: Mutex<HashMap<String, String>>,
    trusted_names: Mutex<HashSet<String>>,
    trust_path: std::path::PathBuf,
    shutdown: CancellationToken,
    connection_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    active_connects: AtomicUsize,
    connects_idle: Notify,
    eager_pending: AtomicUsize,
    eager_ready: Notify,
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
                    tool_call_id,
                    message: message.clone(),
                })
                .await;
        }
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        let Some(server) = self.server.upgrade() else {
            return;
        };
        let peer = context.peer;
        tokio::spawn(async move {
            let generation = server.next_list_generation();
            let timeout = server.timeout();
            let result = tokio::time::timeout(timeout, peer.list_all_tools()).await;
            match result {
                Ok(Ok(tools)) => server.publish_tools(generation, tools),
                Ok(Err(error)) => server.publish_refresh_failure(
                    generation,
                    format!("tools/list refresh failed: {error}"),
                ),
                Err(_) => server
                    .publish_refresh_failure(generation, "tools/list refresh timed out".into()),
            }
        });
    }
}

fn unsupported_server_request(method: &str) -> ErrorData {
    ErrorData::new(
        ErrorCode::METHOD_NOT_FOUND,
        format!("unsupported MCP server request `{method}`"),
        None,
    )
}

fn load_trust_records(path: &std::path::Path) -> Result<Vec<McpNameTrustGrant>, ToolError> {
    crate::events::load_jsonl(path).map_err(|error| {
        ToolError::execution(format!(
            "invalid MCP trust store `{}`; fix the records or remove the file to reset project MCP approvals: {error}",
            path.display()
        ))
    })
}

impl McpRegistry {
    pub(crate) fn new(
        servers: BTreeMap<String, LoadedMcpServer>,
        trust_path: std::path::PathBuf,
    ) -> Result<Self, ToolError> {
        let mut trusted_names = HashSet::new();
        if trust_path.exists() {
            let grants = load_trust_records(&trust_path)?;
            for grant in grants {
                trusted_names.insert(grant.server);
            }
        }
        let inner = Arc::new(RegistryInner {
            servers: Mutex::new(BTreeMap::new()),
            reserved_names: Mutex::new(HashSet::from([
                "read".into(),
                "write".into(),
                "edit".into(),
                "bash".into(),
                "delegate_subagent".into(),
                "get_subagent_result".into(),
                "steer_subagent".into(),
                "cancel_subagent".into(),
            ])),
            claimed_names: Mutex::new(HashMap::new()),
            trusted_names: Mutex::new(trusted_names),
            trust_path,
            shutdown: CancellationToken::new(),
            connection_tasks: Mutex::new(Vec::new()),
            active_connects: AtomicUsize::new(0),
            connects_idle: Notify::new(),
            eager_pending: AtomicUsize::new(0),
            eager_ready: Notify::new(),
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
            let trusted_by_name = inner
                .trusted_names
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(&name);
            let state = if !loaded.config.enabled {
                McpServerState::Disabled
            } else if loaded.source == McpServerSource::WorkspaceFile && !trusted_by_name {
                McpServerState::PendingApproval
            } else {
                McpServerState::Disconnected
            };
            let runtime = Arc::new(ServerRuntime {
                name: name.clone(),
                sanitized_name,
                loaded,
                status: Mutex::new(McpServerStatus {
                    server: name.clone(),
                    state,
                    message: None,
                    tools: Vec::new(),
                }),
                tools: Mutex::new(Vec::new()),
                list_generation: AtomicU64::new(0),
                publication: Mutex::new(()),
                connect_lock: AsyncMutex::new(()),
                service: AsyncMutex::new(None),
                superseded: CancellationToken::new(),
                registry: Arc::downgrade(&inner),
                progress: AsyncMutex::new(HashMap::new()),
            });
            runtimes.insert(name, runtime);
        }
        *inner.servers.lock().unwrap_or_else(|p| p.into_inner()) = runtimes;
        Ok(Self { inner })
    }

    pub(crate) fn reserve_provider(&self, provider: &dyn ToolProvider) -> Result<(), ToolError> {
        let Ok(tools) = provider.tools_for_session(&SessionToolContext {
            session: cookie_agent_protocol::SessionId::new_v7(),
        }) else {
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

    pub(crate) fn start_eager(&self, runtime: &tokio::runtime::Handle) {
        let servers = self
            .servers()
            .into_iter()
            .filter(|server| {
                server.loaded.config.enabled
                    && !server.loaded.config.lazy
                    && server.current_state() == McpServerState::Disconnected
            })
            .collect::<Vec<_>>();
        self.inner
            .eager_pending
            .store(servers.len(), Ordering::Release);
        for server in servers {
            let registry = Arc::clone(&self.inner);
            let task_registry = Arc::clone(&registry);
            let eager = EagerConnect(Arc::clone(&task_registry));
            let task = runtime.spawn(async move {
                let _eager = eager;
                if let Err(error) = server.connect().await
                    && !task_registry.shutdown.is_cancelled()
                {
                    server.fail(error.to_string());
                }
            });
            registry
                .connection_tasks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(task);
        }
    }

    pub(crate) async fn await_eager_ready(&self) {
        while self.inner.eager_pending.load(Ordering::Acquire) != 0 {
            let notified = self.inner.eager_ready.notified();
            if self.inner.eager_pending.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
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

    #[must_use]
    pub fn pending_approvals(&self) -> Vec<McpApprovalRequest> {
        self.servers()
            .into_iter()
            .filter(|server| server.current_state() == McpServerState::PendingApproval)
            .map(|server| McpApprovalRequest {
                server: server.name.clone(),
                connection: approval_display(&server.loaded.config),
            })
            .collect()
    }

    pub fn approve_project_server(&self, server_name: &str) -> Result<(), ToolError> {
        let server = self.server(server_name)?;
        if server.loaded.source != McpServerSource::WorkspaceFile {
            return Err(ToolError::execution(format!(
                "MCP server `{server_name}` is not project-scoped"
            )));
        }
        if server.current_state() != McpServerState::PendingApproval {
            return Err(ToolError::execution(format!(
                "MCP server `{server_name}` is not pending approval"
            )));
        }
        let grant = McpNameTrustGrant {
            server: server_name.to_owned(),
        };
        crate::events::append_jsonl(&self.inner.trust_path, &grant)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        self.inner
            .trusted_names
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(server_name.to_owned());
        server.set_status(McpServerState::Disconnected, None);
        if !server.loaded.config.lazy
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            self.inner.eager_pending.fetch_add(1, Ordering::AcqRel);
            let readiness = EagerConnect(Arc::clone(&self.inner));
            self.spawn_connection(server, &handle, Some(readiness))?;
        }
        Ok(())
    }

    pub fn reject_project_server(&self, server_name: &str) -> Result<(), ToolError> {
        let server = self.server(server_name)?;
        if server.loaded.source != McpServerSource::WorkspaceFile {
            return Err(ToolError::execution(format!(
                "MCP server `{server_name}` is not project-scoped"
            )));
        }
        if server.current_state() != McpServerState::PendingApproval {
            return Err(ToolError::execution(format!(
                "MCP server `{server_name}` is not pending approval"
            )));
        }
        server.set_status(
            McpServerState::Rejected,
            Some("project approval rejected".into()),
        );
        Ok(())
    }

    pub(crate) async fn upsert_server(
        &self,
        name: String,
        loaded: LoadedMcpServer,
    ) -> Result<(), ToolError> {
        let sanitized_name = sanitize_name(&name);
        {
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
        }
        let trusted_by_name = self
            .inner
            .trusted_names
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&name);
        let state = if !loaded.config.enabled {
            McpServerState::Disabled
        } else if loaded.source == McpServerSource::WorkspaceFile && !trusted_by_name {
            McpServerState::PendingApproval
        } else {
            McpServerState::Disconnected
        };
        let runtime = Arc::new(ServerRuntime {
            name: name.clone(),
            sanitized_name,
            loaded,
            status: Mutex::new(McpServerStatus {
                server: name.clone(),
                state,
                message: None,
                tools: Vec::new(),
            }),
            tools: Mutex::new(Vec::new()),
            list_generation: AtomicU64::new(0),
            publication: Mutex::new(()),
            connect_lock: AsyncMutex::new(()),
            service: AsyncMutex::new(None),
            superseded: CancellationToken::new(),
            registry: Arc::downgrade(&self.inner),
            progress: AsyncMutex::new(HashMap::new()),
        });
        let previous = {
            let mut servers = self
                .inner
                .servers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = servers.get(&name).cloned();
            if let Some(previous) = &previous {
                previous.supersede();
            }
            servers.insert(name, Arc::clone(&runtime));
            previous
        };
        if let Some(previous) = previous
            && let Some(mut service) = previous.service.lock().await.take()
        {
            let _ = service.close_with_timeout(Duration::from_secs(4)).await;
        }
        if runtime.loaded.config.enabled
            && !runtime.loaded.config.lazy
            && runtime.current_state() == McpServerState::Disconnected
        {
            let handle = tokio::runtime::Handle::try_current()
                .map_err(|_| ToolError::execution("MCP runtime is unavailable"))?;
            self.spawn_connection(runtime, &handle, None)?;
        }
        Ok(())
    }

    pub(crate) async fn remove_server(&self, name: &str) -> Result<(), ToolError> {
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
            removed.supersede();
            servers.remove(name);
            removed
        };
        if let Some(mut service) = removed.service.lock().await.take() {
            let _ = service.close_with_timeout(Duration::from_secs(4)).await;
        }
        Ok(())
    }

    pub(crate) async fn reconnect_server(&self, name: &str) -> Result<(), ToolError> {
        let server = self.server(name)?;
        if !server.loaded.config.enabled {
            return Err(ToolError::execution(format!(
                "MCP server `{name}` is disabled"
            )));
        }
        if server.current_state() == McpServerState::PendingApproval {
            return Err(ToolError::execution(format!(
                "MCP server `{name}` is pending project approval"
            )));
        }
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
                server.fail(error.to_string());
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
                server.fail(error.to_string());
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

    async fn resolve_tool(
        &self,
        tool_name: &str,
    ) -> Result<(Arc<ServerRuntime>, String), ToolError> {
        for server in self.servers() {
            if let Some(raw) = server.raw_tool_name(tool_name) {
                return Ok((server, raw));
            }
        }
        for server in self.servers() {
            if server.loaded.config.lazy
                && tool_name.starts_with(&format!("{}_", server.sanitized_name))
            {
                server.connect().await?;
                if let Some(raw) = server.raw_tool_name(tool_name) {
                    return Ok((server, raw));
                }
            }
        }
        Err(ToolError::execution(format!(
            "unknown MCP tool `{tool_name}`"
        )))
    }
}

impl ServerRuntime {
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

    fn fail(&self, message: String) {
        if self.superseded.is_cancelled() {
            return;
        }
        self.clear_claims();
        self.tools.lock().unwrap_or_else(|p| p.into_inner()).clear();
        self.set_status(McpServerState::Failed, Some(message));
    }

    fn supersede(&self) {
        self.superseded.cancel();
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
            McpServerState::PendingApproval => {
                return Err(ToolError::execution(format!(
                    "MCP server `{}` is pending project approval",
                    self.name
                )));
            }
            McpServerState::Rejected => {
                return Err(ToolError::execution(format!(
                    "MCP server `{}` project approval was rejected",
                    self.name
                )));
            }
            McpServerState::Disconnected | McpServerState::Connecting | McpServerState::Failed => {}
        }
        self.set_status(McpServerState::Connecting, None);
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
            if let Some(cwd) = &self.loaded.config.cwd {
                process.current_dir(cwd);
            }
            let mut wrapped = CommandWrap::from(process);
            #[cfg(unix)]
            wrapped.wrap(ProcessGroup::leader());
            #[cfg(windows)]
            wrapped.wrap(JobObject);
            let transport = TokioChildProcess::new(wrapped).map_err(|error| {
                ToolError::execution(format!(
                    "MCP server `{}` failed to spawn: {error}",
                    self.name
                ))
            })?;
            let cleanup = ChildCleanup::new();
            let transport = ReapingChildTransport::new(transport, cleanup.clone());
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
            let transport = StreamableHttpClientTransport::from_config(config);
            tokio::select! {
                result = tokio::time::timeout(timeout, handler.serve_with_lifecycle(transport, lifecycle)) => result,
                () = registry.shutdown.cancelled() => {
                    return Err(ToolError::execution("MCP registry is shutting down"));
                }
                () = self.superseded.cancelled() => {
                    return Err(self.superseded_error());
                }
            }
                .map_err(|_| {
                    ToolError::execution(format!("MCP server `{}` connect timed out", self.name))
                })?
                .map_err(|error| {
                    ToolError::execution(format!(
                        "MCP server `{}` connect failed: {error}",
                        self.name
                    ))
                })?
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
        })?
        .map_err(|error| {
            ToolError::execution(format!(
                "MCP server `{}` tools/list failed: {error}",
                self.name
            ))
        })?;
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
        self.set_status(McpServerState::Connected, None);
        Ok(())
    }

    fn next_list_generation(&self) -> u64 {
        let _publication = self.publication.lock().unwrap_or_else(|p| p.into_inner());
        self.list_generation.fetch_add(1, Ordering::AcqRel) + 1
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
                self.fail(error.to_string());
                return;
            }
        };
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
            if reserved.contains(&tool.spec.name)
                || !local.insert(tool.spec.name.clone())
                || claimed.contains_key(&tool.spec.name)
            {
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

    fn get_permission_name(_tool_name: &str) -> Result<&'static str, ToolError> {
        Ok("mcp")
    }

    fn permission_for_unlisted_tool(
        &self,
        tool_name: &str,
    ) -> Result<Option<&'static str>, ToolError> {
        Ok(self
            .servers()
            .into_iter()
            .any(|server| tool_name.starts_with(&format!("{}_", server.sanitized_name)))
            .then_some("mcp"))
    }

    fn get_permission_resource(
        &self,
        tool_name: &str,
        _arguments: &Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let owned = self
            .servers()
            .into_iter()
            .any(|server| tool_name.starts_with(&format!("{}_", server.sanitized_name)));
        if !owned {
            return Err(ToolError::execution(format!(
                "unknown MCP tool `{tool_name}`"
            )));
        }
        Ok(("mcp", Some(tool_name.to_owned())))
    }

    fn get_display_argument(&self, name: &str, _arguments: &Value) -> Result<String, ToolError> {
        Ok(name.to_owned())
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let (server, raw_name) = self.resolve_tool(&call.name).await?;
        let arguments = call
            .arguments
            .as_object()
            .cloned()
            .ok_or_else(|| ToolError::execution("MCP tool arguments must be an object"))?;
        let normalized = Value::Object(arguments.clone());
        let argument_bytes = serde_json::to_vec(&normalized)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let label = call.name.clone();
        let resource = PreparedApprovalResource {
            capability: PermissionAction::Mcp,
            canonical: PreparedResourceIdentity::new(format!(
                "mcp-tool:{}",
                Sha256Digest::of_bytes(label.as_bytes())
            ))
            .map_err(|error| ToolError::execution(error.to_string()))?,
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(label.as_bytes()),
            binding_lifetime: PreparedBindingLifetime::RestartStable,
            boundary: ApprovalBoundary::Exact,
            source: ApprovalResourceSource::PrimaryOperation,
        };
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(&argument_bytes),
            vec![ApprovalCapability {
                action: PermissionAction::Mcp,
                operation: PreparedCapabilityOperation::new(format!("{}:call", call.name))
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
    ) -> Result<ToolResult, ToolError> {
        self.revalidate().await?;
        let params = CallToolRequestParams::new(self.raw_name.clone())
            .with_arguments(self.arguments.clone());
        let timeout = self.server.timeout();
        let service = self.server.service.lock().await;
        let Some(service) = service.as_ref() else {
            return Err(ToolError::execution("MCP server is not connected"));
        };
        self.server
            .progress
            .lock()
            .await
            .insert(self.call_id, context.progress.clone());
        let call = service.call_tool(params);
        tokio::pin!(call);
        let result = tokio::select! {
            result = &mut call => result.map_err(|error| ToolError::execution(error.to_string())),
            _ = context.cancellation.cancelled() => Err(ToolError::execution("MCP tool call cancelled")),
            _ = tokio::time::sleep(timeout) => Err(ToolError::execution("MCP tool call timed out")),
        };
        self.server.progress.lock().await.remove(&self.call_id);
        map_tool_result(&context, &self.generated_name, result?)
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
            name: generated,
            permission_name: "mcp".into(),
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
    let raw_content = serde_json::to_value(&result.content)
        .map_err(|error| ToolError::execution(error.to_string()))?;
    let mut output = Vec::new();
    let mut attachments = Vec::new();
    for block in result.content {
        match block {
            ContentBlock::Text(text) => output.push(text.text),
            ContentBlock::Image(image) => {
                match retain_base64_attachment(context, image.mime_type, &image.data) {
                    Ok(attachment) => {
                        attachments.push(attachment);
                        output.push("[MCP image attachment]".into());
                    }
                    Err(error) => output.push(format!("[Invalid MCP image content: {error}]")),
                }
            }
            ContentBlock::Audio(audio) => {
                match retain_base64_attachment(context, audio.mime_type, &audio.data) {
                    Ok(attachment) => {
                        attachments.push(attachment);
                        output.push("[MCP audio attachment]".into());
                    }
                    Err(error) => output.push(format!("[Invalid MCP audio content: {error}]")),
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
                    match retain_base64_attachment(
                        context,
                        mime_type.unwrap_or_else(|| "application/octet-stream".into()),
                        &blob,
                    ) {
                        Ok(attachment) => {
                            attachments.push(attachment);
                            output.push(format!("[MCP embedded resource attachment: {uri}]"));
                        }
                        Err(error) => output.push(format!(
                            "[MCP embedded resource unavailable: {uri}: {error}]"
                        )),
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
    let metadata = serde_json::json!({
        "mcp": {
            "is_error": result.is_error.unwrap_or(false),
            "structured_content": result.structured_content,
            "content": raw_content,
        }
    });
    Ok(ToolResult {
        title: safe_title(tool_name),
        output: output.join("\n"),
        metadata,
        truncation: None,
        attachments,
    })
}

fn retain_base64_attachment(
    context: &ToolExecutionContext,
    mime_type: String,
    data: &str,
) -> Result<ToolAttachment, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| error.to_string())?;
    context
        .retain_attachment(mime_type, None, &bytes)
        .map_err(|error| error.to_string())
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

fn approval_display(config: &McpServerConfig) -> String {
    if let Some(command) = &config.command {
        let mut value = std::iter::once(command.as_str())
            .chain(config.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(cwd) = &config.cwd {
            value.push_str(&format!("\ncwd: {cwd}"));
        }
        for (name, env) in &config.env {
            value.push_str(&format!("\nenv {name}={env}"));
        }
        value
    } else {
        let mut value = config.url.clone().unwrap_or_default();
        for (name, header) in &config.headers {
            value.push_str(&format!("\n{name}: {header}"));
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, atomic::Ordering},
        time::Duration,
    };

    use cookie_agent_config::{LoadedMcpServer, McpServerConfig, McpServerSource};
    use cookie_agent_protocol::{
        AgentId, CancellationCapability, Modality, ModelCapabilities, ReplayCapability, RunId,
        SessionId, ToolCallId,
    };
    use rmcp::model::Tool;
    use serde_json::{Map, Value, json};
    use tokio_util::sync::CancellationToken;

    use crate::{
        ProgressSink, SessionToolContext, ToolCall, ToolExecutionContext, ToolPreparationContext,
        ToolProvider as _, TurnAgentContext, events::OutputHub, runtime::ArtifactStore,
    };

    use super::{McpRegistry, McpServerState, convert_tool, sanitize_name};

    fn fixture_config(lazy: bool) -> McpServerConfig {
        McpServerConfig {
            command: Some("python3".into()),
            args: vec![format!(
                "{}/tests/fixtures/mcp_server.py",
                env!("CARGO_MANIFEST_DIR")
            )],
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            lazy,
            timeout_ms: Some(5_000),
        }
    }

    fn registry(directory: &tempfile::TempDir, source: McpServerSource, lazy: bool) -> McpRegistry {
        McpRegistry::new(
            BTreeMap::from([(
                "fixture".into(),
                LoadedMcpServer {
                    source,
                    config: fixture_config(lazy),
                },
            )]),
            directory.path().join("trust.jsonl"),
        )
        .expect("MCP registry")
    }

    fn turn_context() -> Arc<TurnAgentContext> {
        Arc::new(TurnAgentContext {
            agent: AgentId::new("test").expect("agent"),
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

    async fn execute(
        registry: &McpRegistry,
        directory: &tempfile::TempDir,
        name: &str,
        arguments: Value,
    ) -> cookie_agent_protocol::PersistedToolResult {
        let call_id = ToolCallId::new_v7();
        let prepared = registry
            .prepare(
                ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: directory.path().into(),
                    workspace_root: directory.path().into(),
                    turn_context: turn_context(),
                },
                ToolCall {
                    id: call_id,
                    name: name.into(),
                    arguments,
                },
            )
            .await
            .expect("prepare MCP call");
        let executor = prepared
            .executor
            .lock()
            .await
            .take()
            .expect("prepared executor");
        executor.revalidate().await.expect("revalidate MCP call");
        let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(8);
        executor
            .execute(ToolExecutionContext {
                session: SessionId::new_v7(),
                run: RunId::new_v7(),
                progress: ProgressSink::new(progress_tx, OutputHub::new(call_id, 1024)),
                cancellation: CancellationToken::new(),
                stdin: None,
                turn_context: turn_context(),
                artifacts: ArtifactStore::open(directory.path().join("artifacts"))
                    .expect("artifact store"),
            })
            .await
            .expect("execute MCP call")
    }

    #[test]
    fn names_are_sanitized_without_prefix() {
        assert_eq!(sanitize_name("git hub"), "git_hub");
        assert_eq!(sanitize_name("search/repos"), "search_repos");
    }

    #[test]
    fn tool_specs_force_closed_object_schema_and_keep_defs() {
        let tool = Tool::new(
            "search/repos",
            "Search repositories.",
            Arc::new(Map::from_iter([
                ("$defs".into(), json!({"Query":{"type":"string"}})),
                (
                    "properties".into(),
                    json!({"query":{"$ref":"#/$defs/Query"}}),
                ),
            ])),
        );
        let converted = convert_tool("git hub", "git_hub", tool).expect("tool conversion");
        assert_eq!(converted.spec.name, "git_hub_search_repos");
        assert_eq!(converted.spec.permission_name, "mcp");
        assert_eq!(converted.spec.parameters["type"], "object");
        assert_eq!(converted.spec.parameters["additionalProperties"], false);
        assert!(converted.spec.parameters.get("$defs").is_some());
        assert!(converted.spec.description.contains("untrusted MCP output"));
    }

    #[tokio::test]
    async fn stdio_lists_calls_and_refreshes_tools() {
        let directory = tempfile::tempdir().expect("tempdir");
        let registry = registry(&directory, McpServerSource::UserFile, false);
        let server = registry.server("fixture").expect("fixture server");
        server.connect().await.expect("connect fixture");
        let specs = registry
            .tools_for_session(&SessionToolContext {
                session: SessionId::new_v7(),
            })
            .expect("MCP tools");
        assert_eq!(specs.len(), 2);
        assert!(specs.iter().any(|tool| tool.name == "fixture_echo_text"));

        let result = execute(
            &registry,
            &directory,
            "fixture_echo_text",
            json!({"text":"refresh"}),
        )
        .await;
        assert_eq!(result.output, "refresh");
        assert_eq!(result.metadata["mcp"]["is_error"], false);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let names = registry
                    .tools_for_session(&SessionToolContext {
                        session: SessionId::new_v7(),
                    })
                    .expect("refreshed tools")
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect::<Vec<_>>();
                if names.contains(&"fixture_new_tool".to_owned()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("tools/list_changed refresh");

        let error = execute(&registry, &directory, "fixture_fail", json!({})).await;
        assert_eq!(error.output, "fixture failure");
        assert_eq!(error.metadata["mcp"]["is_error"], true);
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn lazy_server_connects_on_first_named_use() {
        let directory = tempfile::tempdir().expect("tempdir");
        let registry = registry(&directory, McpServerSource::UserFile, true);
        assert!(
            registry
                .tools_for_session(&SessionToolContext {
                    session: SessionId::new_v7()
                })
                .expect("initial tools")
                .is_empty()
        );
        let result = execute(
            &registry,
            &directory,
            "fixture_echo_text",
            json!({"text":"lazy"}),
        )
        .await;
        assert_eq!(result.output, "lazy");
        assert_eq!(registry.statuses()[0].state, McpServerState::Connected);
        registry.shutdown().await;
    }

    #[test]
    fn stale_generation_created_during_publish_cannot_overwrite_newer_tools() {
        let directory = tempfile::tempdir().expect("tempdir");
        let registry = registry(&directory, McpServerSource::UserFile, true);
        let server = registry.server("fixture").expect("server");
        server.list_generation.store(1, Ordering::Release);
        server.publish_tools_before_commit(
            1,
            vec![Tool::new("old", "old", Arc::new(Map::new()))],
            || assert_eq!(server.next_list_generation(), 2),
        );
        assert!(server.tools.lock().expect("tools").is_empty());
        server.publish_tools(2, vec![Tool::new("new", "new", Arc::new(Map::new()))]);
        assert_eq!(
            server.tools.lock().expect("tools")[0].spec.name,
            "fixture_new"
        );
    }

    #[test]
    fn stale_refresh_failure_cannot_invalidate_newer_publication() {
        let directory = tempfile::tempdir().expect("tempdir");
        let registry = registry(&directory, McpServerSource::UserFile, true);
        let server = registry.server("fixture").expect("server");
        server.set_status(McpServerState::Connected, None);
        server.list_generation.store(1, Ordering::Release);
        server.publish_refresh_failure_before_commit(1, "old refresh failed".into(), || {
            let newer = server.next_list_generation();
            assert_eq!(newer, 2);
            server.publish_tools(newer, vec![Tool::new("new", "new", Arc::new(Map::new()))]);
        });
        assert_eq!(
            server.tools.lock().expect("tools")[0].spec.name,
            "fixture_new"
        );
        let status = registry.statuses().remove(0);
        assert_eq!(status.state, McpServerState::Connected);
        assert_eq!(status.message, None);
        assert_eq!(status.tools, ["fixture_new"]);
    }

    #[test]
    fn collisions_fail_the_named_server() {
        let directory = tempfile::tempdir().expect("tempdir");
        let registry = registry(&directory, McpServerSource::UserFile, true);
        let server = registry.server("fixture").expect("server");
        registry
            .inner
            .reserved_names
            .lock()
            .expect("reserved names")
            .insert("fixture_read".into());
        server.list_generation.store(1, Ordering::Release);
        server.publish_tools(1, vec![Tool::new("read", "read", Arc::new(Map::new()))]);
        let status = registry.statuses().remove(0);
        assert_eq!(status.state, McpServerState::Failed);
        assert!(
            status
                .message
                .expect("collision message")
                .contains("fixture")
        );
    }

    #[tokio::test]
    async fn eager_readiness_waits_for_initial_tool_listing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut config = fixture_config(false);
        config
            .env
            .insert("MCP_FIXTURE_LIST_DELAY_MS".into(), "100".into());
        let registry = McpRegistry::new(
            BTreeMap::from([(
                "fixture".into(),
                LoadedMcpServer {
                    source: McpServerSource::UserFile,
                    config,
                },
            )]),
            directory.path().join("trust.jsonl"),
        )
        .expect("registry");
        registry.start_eager(&tokio::runtime::Handle::current());
        assert!(
            registry
                .tools_for_session(&SessionToolContext {
                    session: SessionId::new_v7(),
                })
                .expect("tools before readiness")
                .is_empty()
        );
        registry.await_eager_ready().await;
        let tools = registry
            .tools_for_session(&SessionToolContext {
                session: SessionId::new_v7(),
            })
            .expect("tools after readiness");
        assert_eq!(tools.len(), 2);
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_aborts_inflight_connect_and_blocks_late_installation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut config = fixture_config(false);
        config
            .env
            .insert("MCP_FIXTURE_LIST_DELAY_MS".into(), "10000".into());
        let pid_file = directory.path().join("fixture.pid");
        config.env.insert(
            "MCP_FIXTURE_PID_FILE".into(),
            pid_file.to_string_lossy().into_owned(),
        );
        let registry = McpRegistry::new(
            BTreeMap::from([(
                "fixture".into(),
                LoadedMcpServer {
                    source: McpServerSource::UserFile,
                    config,
                },
            )]),
            directory.path().join("trust.jsonl"),
        )
        .expect("registry");
        let server = registry.server("fixture").expect("server");
        registry.start_eager(&tokio::runtime::Handle::current());
        tokio::time::timeout(Duration::from_secs(2), async {
            while server.current_state() != McpServerState::Connecting {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("connection started");
        tokio::time::timeout(Duration::from_secs(2), async {
            while std::fs::read_to_string(&pid_file)
                .map(|pid| pid.trim().is_empty())
                .unwrap_or(true)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fixture process started");
        let fixture_pid = std::fs::read_to_string(&pid_file)
            .expect("fixture PID")
            .trim()
            .to_owned();
        registry.shutdown().await;
        assert!(server.service.lock().await.is_none());
        assert_ne!(server.current_state(), McpServerState::Connected);
        assert_eq!(registry.inner.active_connects.load(Ordering::Acquire), 0);
        #[cfg(target_os = "linux")]
        let reaped = tokio::time::timeout(Duration::from_secs(2), async {
            let process = format!("/proc/{fixture_pid}");
            while std::path::Path::new(&process).exists() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        #[cfg(target_os = "linux")]
        assert!(
            reaped.is_ok(),
            "fixture process was not reaped: {}",
            std::fs::read_to_string(format!("/proc/{fixture_pid}/stat"))
                .unwrap_or_else(|error| error.to_string())
        );
        #[cfg(not(target_os = "linux"))]
        let _ = fixture_pid;
    }

    #[tokio::test]
    async fn replacing_an_inflight_connect_cannot_publish_stale_tools() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut delayed = fixture_config(false);
        delayed
            .env
            .insert("MCP_FIXTURE_LIST_DELAY_MS".into(), "10000".into());
        let registry = McpRegistry::new(
            BTreeMap::from([(
                "fixture".into(),
                LoadedMcpServer {
                    source: McpServerSource::UserFile,
                    config: delayed,
                },
            )]),
            directory.path().join("trust.jsonl"),
        )
        .expect("registry");
        let old = registry.server("fixture").expect("old server");
        registry.start_eager(&tokio::runtime::Handle::current());
        tokio::time::timeout(Duration::from_secs(2), async {
            while old.current_state() != McpServerState::Connecting {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("old connection started");

        registry
            .upsert_server(
                "fixture".into(),
                LoadedMcpServer {
                    source: McpServerSource::Runtime,
                    config: fixture_config(true),
                },
            )
            .await
            .expect("replace server");
        let replacement = registry.server("fixture").expect("replacement server");
        replacement.connect().await.expect("connect replacement");
        tokio::time::timeout(Duration::from_secs(2), async {
            while registry.inner.active_connects.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("old connection cancelled");

        assert!(old.tools.lock().expect("old tools").is_empty());
        assert_eq!(replacement.current_state(), McpServerState::Connected);
        assert_eq!(registry.statuses()[0].tools.len(), 2);
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn reconnect_failure_transitions_server_to_failed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut config = fixture_config(true);
        config.command = Some(
            directory
                .path()
                .join("missing-server")
                .display()
                .to_string(),
        );
        config.args.clear();
        let registry = McpRegistry::new(
            BTreeMap::from([(
                "fixture".into(),
                LoadedMcpServer {
                    source: McpServerSource::UserFile,
                    config,
                },
            )]),
            directory.path().join("trust.jsonl"),
        )
        .expect("registry");

        registry
            .reconnect_server("fixture")
            .await
            .expect_err("reconnect must fail");

        let status = registry.statuses().remove(0);
        assert_eq!(status.state, McpServerState::Failed);
        assert!(
            status
                .message
                .is_some_and(|message| message.contains("failed"))
        );
        registry.shutdown().await;
    }

    #[test]
    fn sanitized_server_name_collisions_are_rejected_at_startup() {
        let directory = tempfile::tempdir().expect("tempdir");
        let servers = BTreeMap::from([
            (
                "git hub".into(),
                LoadedMcpServer {
                    source: McpServerSource::UserFile,
                    config: fixture_config(true),
                },
            ),
            (
                "git/hub".into(),
                LoadedMcpServer {
                    source: McpServerSource::UserFile,
                    config: fixture_config(true),
                },
            ),
        ]);
        let error = McpRegistry::new(servers, directory.path().join("trust.jsonl"))
            .expect_err("sanitized server collision");
        assert!(error.to_string().contains("git/hub"));
        assert!(error.to_string().contains("git hub"));
    }

    #[test]
    fn project_approval_is_keyed_by_name_and_rejection_stays_blocked() {
        let directory = tempfile::tempdir().expect("tempdir");
        let project_registry = registry(&directory, McpServerSource::WorkspaceFile, true);
        assert_eq!(
            project_registry.statuses()[0].state,
            McpServerState::PendingApproval
        );
        let runtime_registry = registry(&directory, McpServerSource::Runtime, true);
        assert_eq!(
            runtime_registry.statuses()[0].state,
            McpServerState::Disconnected
        );
        let approval = project_registry.pending_approvals().remove(0);
        assert!(approval.connection.contains("mcp_server.py"));
        project_registry
            .approve_project_server("fixture")
            .expect("approve project MCP server");
        assert_eq!(
            project_registry.statuses()[0].state,
            McpServerState::Disconnected
        );

        let reopened = registry(&directory, McpServerSource::WorkspaceFile, true);
        assert_eq!(reopened.statuses()[0].state, McpServerState::Disconnected);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("trust.jsonl")).expect("trust file"),
            "{\"server\":\"fixture\"}\n"
        );

        let mut changed = fixture_config(true);
        changed.args.push("--changed".into());
        let changed = McpRegistry::new(
            BTreeMap::from([(
                "fixture".into(),
                LoadedMcpServer {
                    source: McpServerSource::WorkspaceFile,
                    config: changed,
                },
            )]),
            directory.path().join("trust.jsonl"),
        )
        .expect("changed registry");
        assert_eq!(changed.statuses()[0].state, McpServerState::Disconnected);

        let renamed = McpRegistry::new(
            BTreeMap::from([(
                "renamed".into(),
                LoadedMcpServer {
                    source: McpServerSource::WorkspaceFile,
                    config: fixture_config(true),
                },
            )]),
            directory.path().join("trust.jsonl"),
        )
        .expect("renamed registry");
        assert_eq!(renamed.statuses()[0].state, McpServerState::PendingApproval);
        renamed
            .reject_project_server("renamed")
            .expect("reject renamed server");
        assert_eq!(renamed.statuses()[0].state, McpServerState::Rejected);

        let reoffered = McpRegistry::new(
            BTreeMap::from([(
                "renamed".into(),
                LoadedMcpServer {
                    source: McpServerSource::WorkspaceFile,
                    config: fixture_config(true),
                },
            )]),
            directory.path().join("trust.jsonl"),
        )
        .expect("reoffered registry");
        assert_eq!(
            reoffered.statuses()[0].state,
            McpServerState::PendingApproval
        );
        reoffered
            .approve_project_server("renamed")
            .expect("reapprove renamed server");
        assert_eq!(reoffered.statuses()[0].state, McpServerState::Disconnected);
    }

    #[test]
    fn incompatible_trust_record_fails_with_reset_remediation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let trust_path = directory.path().join("trust.jsonl");
        std::fs::write(
            &trust_path,
            "{\"server\":\"fixture\",\"digest\":\"legacy\"}\n",
        )
        .expect("incompatible trust store");
        let error = McpRegistry::new(
            BTreeMap::from([(
                "fixture".into(),
                LoadedMcpServer {
                    source: McpServerSource::WorkspaceFile,
                    config: fixture_config(true),
                },
            )]),
            trust_path.clone(),
        )
        .expect_err("incompatible trust records must fail startup");
        let message = error.to_string();
        assert!(message.contains(&trust_path.display().to_string()));
        assert!(message.contains("fix the records or remove the file"));
        assert!(message.contains("reset project MCP approvals"));
    }
}
