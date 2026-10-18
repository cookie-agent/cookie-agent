//! The coding runtime's end of the runtime protocol.
//!
//! [`serve`] answers one daemon over a frame transport. It offers one
//! capability, `coding`, and opens an instance of it for every workspace the
//! daemon asks for (`capability/open`): the workspace's tools and project
//! skills. It answers `runtime/context` with an instance's `AGENTS.md`, and
//! prepares and executes calls of every instance concurrently. It keeps each prepared call under its prepared ID until
//! the daemon executes or releases it, runs executions concurrently, sends
//! their progress back with `tool/progress`, and tells the daemon with
//! `tool/detachable` when a call reaches its detach point. Each instance hosts the template's MCP servers in its workspace (`docs/site/specs/runtimes.md`
//! section 3): it connects them before it answers `capability/open`,
//! declares their tools next to its built-in ones, reconnects dropped ones
//! with the shared reconnect policy, and reports their status with
//! `mcp/changed` and a changed tool list with `capability/tools_changed`. It
//! returns on `runtime/shutdown` or when the transport ends, cancelling what
//! still runs and stopping its MCP servers.
//!
//! [`in_process_connector`] runs it on a task of the daemon's own process
//! over an in-memory frame pair; that is how the daemon's `local` coding
//! runtime runs.

use std::{
    collections::{BTreeMap, HashMap},
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use base64::Engine as _;
use cookie_agent_config::{LoadedMcpServer, McpOAuthConfig, McpServerConfig, McpServerSource};
use cookie_agent_mcp::{McpRegistry, McpRegistryOptions, McpServerFailure, McpServerStatus};
use cookie_agent_protocol::{RUNTIME_PROTOCOL_VERSION, SessionId, Transport, errors};
use cookie_agent_runtime_api::{
    CodingToolsFactory, DetachPoint, ProgressCapture, ProgressSink, RuntimeConnection,
    RuntimeConnector, RuntimeInfo, RuntimeKind, SessionToolContext, ToolArtifacts, ToolCall,
    ToolError, ToolExecutionContext, ToolOrigin, ToolPreparationContext, ToolProgress,
    ToolProvider, WorkspaceContext,
    context::{AGENT_MD_MAX_BYTES, load_agent_md},
    protocol::{
        self, CapabilityDeclaration, CapabilityOpenParams, CapabilityOpenResult,
        CapabilityToolsChangedParams, ContextParams, ContextResult, DetachableParams, Empty,
        ExecuteOutcome, ExecuteParams, ExecuteResult, InitializeParams, InitializeResult,
        InlineMedia, McpChangedParams, McpFailureReport, McpReconnectParams, McpServerReport,
        McpServerSettings, Outcome, OutcomeExt as _, PrepareOutcome, PrepareParams, PrepareResult,
        PreparedCall, PreparedRef, PromptSectionDeclaration, RuntimeSettings, RuntimeSkills,
        RuntimeTools, SkillDeclaration, SkillDiscoveryDeclaration, ToolDeclaration,
        ToolDeclarationExt as _, ToolProgressParams, WireToolError, method,
    },
    reconnect::ReconnectPolicy,
    rpc::{Incoming, IncomingNotification, IncomingRequest, Peer, RpcError},
};
use futures_util::FutureExt as _;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How [`serve`] builds its runtime.
#[derive(Clone)]
pub struct ServeOptions {
    /// The directories `coding` instances may work in: a workspace must lie
    /// under one of them, such as the `--root` arguments of
    /// `cookie runtime serve`. Any workspace when empty.
    pub roots: Vec<PathBuf>,
    /// Builds the runtime's tools for its workspace.
    pub tools: CodingToolsFactory,
    /// How dropped MCP servers are reconnected; the shared default when
    /// absent. Tests that cannot wait for the production backoff set it.
    pub mcp_reconnect: Option<ReconnectPolicy>,
    /// MCP servers of the runtime's own machine, hosted when the daemon
    /// sends none, as for a runtime that dials in.
    pub mcp_servers: Vec<McpServerSettings>,
    /// Declare producer messaging: the runtime's tools get a
    /// [`ToolExecutionContext::producers`] channel to the sessions attached
    /// to the runtime. The runtime tracks no external work of its own, so
    /// it reports its recovery ready as soon as the daemon starts it.
    pub producer_messaging: bool,
    /// Stops serving when cancelled, as `runtime/shutdown` does: calls still
    /// running are cancelled and the MCP servers shut down.
    pub stop: CancellationToken,
}

impl Default for ServeOptions {
    /// Any workspace and the [`crate::BuiltinTools`].
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            mcp_servers: Vec::new(),
            tools: builtin_tools(),
            mcp_reconnect: None,
            producer_messaging: false,
            stop: CancellationToken::new(),
        }
    }
}

fn builtin_tools() -> CodingToolsFactory {
    Arc::new(|workspace| vec![Arc::new(crate::BuiltinTools::new(workspace))])
}

/// Starts a coding runtime on a task of this process for each connection,
/// over an in-memory frame pair.
struct InProcessConnector {
    options: ServeOptions,
}

#[async_trait]
impl RuntimeConnector for InProcessConnector {
    async fn connect(&self) -> Result<RuntimeConnection, String> {
        let (daemon, runtime) = cookie_agent_protocol::in_process_pair(64);
        tokio::spawn(serve(Box::new(runtime), self.options.clone()));
        Ok(RuntimeConnection::new(Box::new(daemon)))
    }
}

/// A connector that runs a coding runtime with the [`crate::BuiltinTools`]
/// in this process, for any workspace.
#[must_use]
pub fn in_process_connector() -> Arc<dyn RuntimeConnector> {
    in_process_connector_with(builtin_tools())
}

/// A connector that runs a coding runtime with the tools `tools` builds, in
/// this process.
#[must_use]
pub fn in_process_connector_with(tools: CodingToolsFactory) -> Arc<dyn RuntimeConnector> {
    in_process_connector_with_options(ServeOptions {
        tools,
        ..ServeOptions::default()
    })
}

/// A connector that runs a coding runtime with `options` in this process.
#[must_use]
pub fn in_process_connector_with_options(options: ServeOptions) -> Arc<dyn RuntimeConnector> {
    Arc::new(InProcessConnector { options })
}

/// `path` with a leading `~` (alone, or followed by a separator) replaced by
/// this machine's home directory; any other path unchanged.
#[must_use]
pub fn expand_home(path: &Path) -> PathBuf {
    let mut components = path.components();
    match components.next() {
        Some(std::path::Component::Normal(first)) if first == "~" => {
            match cookie_agent_protocol::paths::home_dir() {
                Ok(home) => home.join(components.as_path()),
                Err(_) => path.to_owned(),
            }
        }
        _ => path.to_owned(),
    }
}

/// Serves one daemon over this process's standard input and output, one
/// JSON-RPC message per line, until it sends `runtime/shutdown` or standard
/// input ends. Nothing else may write to standard output meanwhile.
pub async fn serve_stdio(options: ServeOptions) {
    serve(
        Box::new(cookie_agent_runtime_api::rpc::LineTransport::new(
            tokio::io::stdin(),
            tokio::io::stdout(),
        )),
        options,
    )
    .await;
}

/// Serves one daemon over `transport` until it sends `runtime/shutdown`,
/// the transport ends, or [`ServeOptions::stop`] is cancelled. Calls still
/// running are cancelled when it returns.
pub async fn serve(transport: Box<dyn Transport>, options: ServeOptions) {
    let (peer, mut incoming) = Peer::start(transport);
    let stop = options.stop.child_token();
    let server = Arc::new(Server {
        peer: peer.clone(),
        options,
        initializing: AtomicBool::new(false),
        state: OnceLock::new(),
        instances: Mutex::default(),
        opening: tokio::sync::Mutex::new(()),
        next_instance: AtomicU64::new(1),
        prepared: Mutex::default(),
        executions: Mutex::default(),
        next_prepared: AtomicU64::new(1),
        stop,
    });
    loop {
        tokio::select! {
            () = server.stop.cancelled() => break,
            message = incoming.recv() => match message {
                None => break,
                Some(Incoming::Request(request)) => {
                    // An execution is registered in arrival order, so a
                    // `tool/cancel` sent after it finds it.
                    let admitted = server.admit_execution(&request);
                    let server = Arc::clone(&server);
                    tokio::spawn(async move { server.handle_request(request, admitted).await });
                }
                Some(Incoming::Notification(notification)) => {
                    server.handle_notification(&notification);
                }
            },
        }
    }
    server.stop.cancel();
    lock(&server.prepared).clear();
    let instances = lock(&server.instances).drain().collect::<Vec<_>>();
    for (_, instance) in instances {
        if let Some(mcp) = &instance.mcp {
            mcp.shutdown().await;
        }
    }
    peer.close();
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Server {
    peer: Peer,
    options: ServeOptions,
    /// Set by the first `runtime/initialize`.
    initializing: AtomicBool,
    state: OnceLock<Arc<Initialized>>,
    /// Open capability instances by handle.
    instances: Mutex<HashMap<String, Arc<Instance>>>,
    /// Serializes `capability/open`, so one workspace opens once.
    opening: tokio::sync::Mutex<()>,
    next_instance: AtomicU64,
    /// Prepared calls by prepared ID, until executed or released.
    prepared: Mutex<HashMap<u64, cookie_agent_runtime_api::PreparedTool>>,
    /// Executing calls by prepared ID.
    executions: Mutex<HashMap<u64, Execution>>,
    next_prepared: AtomicU64,
    /// Cancelled when the server stops; every call runs under a child of it.
    stop: CancellationToken,
}

/// What `runtime/initialize` set.
struct Initialized {
    runtime_id: String,
    settings: RuntimeSettings,
    /// The template's MCP servers, which every instance hosts.
    mcp_servers: BTreeMap<String, LoadedMcpServer>,
}

/// One open `coding` instance.
struct Instance {
    id: String,
    info: Arc<RuntimeInfo>,
    /// The built-in providers, then the MCP registry when the instance hosts
    /// MCP servers. Names are unique across them: the registry fails a
    /// server whose tool would take a built-in name.
    providers: Vec<Arc<dyn ToolProvider>>,
    /// The template's MCP servers, when it has any.
    mcp: Option<McpRegistry>,
    /// The result `capability/open` answered, for a repeated open.
    opened: CapabilityOpenResult,
}

impl Instance {
    /// The provider that declares `name` now.
    fn route(&self, name: &str) -> Option<usize> {
        let context = SessionToolContext::new(SessionId::new_v7());
        self.providers.iter().position(|provider| {
            provider
                .tools_for_session(&context)
                .is_ok_and(|tools| tools.iter().any(|tool| tool.name == name))
        })
    }
}

struct Execution {
    cancel: CancellationToken,
}

/// An execute request registered before its handler runs.
struct AdmittedExecution {
    params: ExecuteParams,
    cancel: CancellationToken,
}

fn unknown_handle(what: &str, id: u64) -> RpcError {
    errors::UNKNOWN_HANDLE
        .error()
        .detail(format!("unknown {what} {id}"))
        .into()
}

impl Server {
    /// Registers the execution an execute request starts.
    fn admit_execution(&self, request: &IncomingRequest) -> Option<AdmittedExecution> {
        if request.method != method::TOOL_EXECUTE {
            return None;
        }
        let params: ExecuteParams = request.params().ok()?;
        let cancel = self.stop.child_token();
        lock(&self.executions).insert(
            params.prepared_id,
            Execution {
                cancel: cancel.clone(),
            },
        );
        Some(AdmittedExecution { params, cancel })
    }

    async fn handle_request(
        self: Arc<Self>,
        request: IncomingRequest,
        admitted: Option<AdmittedExecution>,
    ) {
        let method = request.method.clone();
        if method == method::INITIALIZE {
            let result = self.initialize(&request);
            return reply(request, result).await;
        }
        let Some(state) = self.state.get().cloned() else {
            if let Some(admitted) = admitted {
                lock(&self.executions).remove(&admitted.params.prepared_id);
            }
            let _ = request
                .fail(
                    errors::NOT_INITIALIZED
                        .error()
                        .detail(format!("`{method}` before `{}`", method::INITIALIZE))
                        .into(),
                )
                .await;
            return;
        };
        match method.as_str() {
            method::CAPABILITY_OPEN => match Arc::clone(&self).open(&state, &request).await {
                Ok((result, watch)) => {
                    reply(request, Ok(result)).await;
                    if let Some((instance, watch)) = watch {
                        tokio::spawn(Arc::clone(&self).report_mcp(instance, watch));
                    }
                }
                Err(error) => reply::<CapabilityOpenResult>(request, Err(error)).await,
            },
            method::CONTEXT => {
                let result = self.context(&state, &request).await;
                reply(request, result).await;
            }
            method::SHUTDOWN => {
                let result = request.params::<Empty>();
                let stop = result.is_ok();
                reply(request, result).await;
                if stop {
                    self.stop.cancel();
                }
            }
            method::TOOL_PREPARE => {
                let result = self.prepare(&request).await;
                reply(request, result).await;
            }
            method::TOOL_REVALIDATE => {
                let result = self.revalidate(&request).await;
                reply(request, result).await;
            }
            method::TOOL_EXECUTE => {
                let result = match admitted {
                    Some(admitted) => {
                        let prepared_id = admitted.params.prepared_id;
                        let result = self.execute(admitted).await;
                        lock(&self.executions).remove(&prepared_id);
                        result
                    }
                    // Only a request whose params do not parse is not
                    // admitted.
                    None => Err(request.params::<ExecuteParams>().err().unwrap_or_else(|| {
                        errors::INTERNAL
                            .error()
                            .detail("the execution was not admitted")
                            .into()
                    })),
                };
                reply(request, result).await;
            }
            method::MCP_RECONNECT => {
                let result = self.reconnect_mcp(&request).await;
                reply(request, result).await;
            }
            other => {
                let error = errors::UNKNOWN_METHOD.error().field("method", other);
                let _ = request.fail(error.into()).await;
            }
        }
    }

    fn handle_notification(&self, notification: &IncomingNotification) {
        match notification.method.as_str() {
            method::TOOL_RELEASE => {
                if let Ok(PreparedRef { prepared_id }) = notification.params() {
                    lock(&self.prepared).remove(&prepared_id);
                }
            }
            method::TOOL_CANCEL => {
                if let Ok(PreparedRef { prepared_id }) = notification.params()
                    && let Some(execution) = lock(&self.executions).get(&prepared_id)
                {
                    execution.cancel.cancel();
                }
            }
            method::PRODUCER_RECOVERY_START if self.options.producer_messaging => {
                let peer = self.peer.clone();
                tokio::spawn(async move {
                    let _ = peer
                        .request::<_, cookie_agent_protocol::RecoveryCompleteResult>(
                            method::PRODUCER_RECOVERY_COMPLETE,
                            &cookie_agent_protocol::RecoveryCompleteParams {
                                outcome: cookie_agent_protocol::RecoveryOutcome::Ready,
                            },
                        )
                        .await;
                });
            }
            // Notifications this runtime does not know are ignored.
            _ => {}
        }
    }

    /// The open instance `id` names.
    fn instance(&self, id: Option<&str>) -> Result<Arc<Instance>, RpcError> {
        let id = id.ok_or_else(|| {
            RpcError::from(
                errors::BAD_PARAMS
                    .error()
                    .field("field", "instance")
                    .detail("the request names no instance"),
            )
        })?;
        lock(&self.instances).get(id).cloned().ok_or_else(|| {
            errors::UNKNOWN_HANDLE
                .error()
                .detail(format!("unknown instance `{id}`"))
                .into()
        })
    }

    /// Initializes the runtime: checks the version, before anything else of
    /// the request is read, and the settings, and reports the `coding`
    /// capability. Workspaces open later, with `capability/open`.
    fn initialize(&self, request: &IncomingRequest) -> Result<InitializeResult, RpcError> {
        let raw: serde_json::Value = request.params()?;
        cookie_agent_protocol::negotiate_version(RUNTIME_PROTOCOL_VERSION, &raw)
            .map_err(|refusal| RpcError::from(refusal.to_rpc_error()))?;
        let params: InitializeParams = request.params()?;
        if self.initializing.swap(true, Ordering::AcqRel) {
            return Err(errors::ALREADY_INITIALIZED.error().into());
        }
        let result = self
            .initialize_once(params)
            .map_err(|reason| errors::INITIALIZE_REJECTED.error().detail(reason).into());
        if result.is_err() {
            self.initializing.store(false, Ordering::Release);
        }
        result
    }

    fn initialize_once(&self, params: InitializeParams) -> Result<InitializeResult, String> {
        cookie_agent_protocol::RuntimeId::new(params.runtime_id.as_str())
            .map_err(|_| format!("invalid runtime ID `{}`", params.runtime_id))?;
        let mcp_servers = if params.settings.mcp_servers.is_empty() {
            mcp_servers(&self.options.mcp_servers)?
        } else {
            mcp_servers(&params.settings.mcp_servers)?
        };
        if params.settings.agent_md.max_bytes > AGENT_MD_MAX_BYTES {
            return Err(format!("agent_md.max_bytes exceeds {AGENT_MD_MAX_BYTES}"));
        }
        let initialized = Arc::new(Initialized {
            runtime_id: params.runtime_id,
            settings: params.settings,
            mcp_servers,
        });
        if self.state.set(initialized).is_err() {
            return Err("the runtime is already initialized".into());
        }
        Ok(InitializeResult {
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            features: Vec::new(),
            platform: Some(std::env::consts::OS.to_owned()),
            capabilities: vec![CapabilityDeclaration {
                name: cookie_agent_protocol::CapabilityName::coding(),
                kind: RuntimeKind::new(cookie_agent_runtime_api::CODING_RUNTIME_KIND)
                    .expect("`coding` is a valid kind"),
                takes_workspace: true,
                description: "Read, write, edit, and run commands in a workspace".into(),
            }],
            producer_messaging: self.options.producer_messaging,
        })
    }

    /// `capability/open`: the `coding` instance for a workspace, opened when
    /// it is not open yet. A new instance's MCP servers have finished their
    /// first connection attempt when this returns. Returns the reply and,
    /// for a new instance that hosts MCP servers, what
    /// [`Self::report_mcp`] watches once the reply is sent.
    async fn open(
        self: Arc<Self>,
        state: &Initialized,
        request: &IncomingRequest,
    ) -> Result<(CapabilityOpenResult, Option<(Arc<Instance>, McpWatch)>), RpcError> {
        let params: CapabilityOpenParams = request.params()?;
        let (capability, asked) = (params.capability.clone(), params.workspace.clone());
        let rejected = |reason: String| -> RpcError {
            errors::CAPABILITY_OPEN_FAILED
                .error()
                .field("capability", capability.as_str())
                .field("workspace", asked.as_deref())
                .detail(reason)
                .into()
        };
        if params.capability.as_str() != cookie_agent_protocol::CapabilityName::CODING {
            return Err(rejected(format!(
                "this runtime offers no capability `{}`",
                params.capability
            )));
        }
        let requested = params
            .workspace
            .map(|workspace| expand_home(Path::new(&workspace)))
            .ok_or_else(|| rejected("`coding` requires a workspace".into()))?;
        let workspace = WorkspaceContext::new(&requested).map_err(|error| {
            rejected(format!(
                "workspace {} is unavailable: {error}",
                requested.display()
            ))
        })?;
        if !self.options.roots.is_empty()
            && !self.options.roots.iter().any(|root| {
                root.canonicalize()
                    .is_ok_and(|root| workspace.root().starts_with(root))
            })
        {
            return Err(rejected(format!(
                "workspace {} is outside the directories this runtime serves",
                workspace.root().display()
            )));
        }
        let _opening = self.opening.lock().await;
        let existing = lock(&self.instances)
            .values()
            .find(|instance| {
                instance
                    .info
                    .workspace()
                    .is_some_and(|open| open.root() == workspace.root())
            })
            .cloned();
        if let Some(instance) = existing {
            return Ok((instance.opened.clone(), None));
        }
        let (instance, watch) = self
            .open_instance(state, workspace)
            .await
            .map_err(rejected)?;
        lock(&self.instances).insert(instance.id.clone(), Arc::clone(&instance));
        Ok((
            instance.opened.clone(),
            watch.map(|watch| (instance, watch)),
        ))
    }

    async fn open_instance(
        &self,
        state: &Initialized,
        workspace: WorkspaceContext,
    ) -> Result<(Arc<Instance>, Option<McpWatch>), String> {
        let skills = cookie_agent_config::load_project_skills(workspace.root())
            .map_err(|error| format!("project skills: {error}"))?;
        let mut providers = (self.options.tools)(&workspace);
        let (mcp, watch) = if state.mcp_servers.is_empty() {
            (None, None)
        } else {
            let registry =
                McpRegistry::new(state.mcp_servers.clone(), McpRegistryOptions::default())
                    .map_err(|error| error.to_string())?;
            for provider in &providers {
                registry
                    .reserve_provider(provider.as_ref())
                    .map_err(|error| error.to_string())?;
            }
            if let Some(policy) = &self.options.mcp_reconnect {
                registry.set_reconnect_policy(policy.clone());
            }
            registry.start_eager(&tokio::runtime::Handle::current());
            tokio::select! {
                () = registry.await_eager_ready() => {}
                () = self.stop.cancelled() => {
                    registry.shutdown().await;
                    return Err("the runtime stopped while its MCP servers connected".into());
                }
            }
            // Subscribed before the reply: a failure from now on reaches
            // the daemon once it has the reply, one before is in it.
            let watch = McpWatch {
                registry: registry.clone(),
                changes: registry.subscribe_changes(),
                failures: registry.subscribe_failures(),
                declared: RuntimeTools::default(),
                reported: Vec::new(),
            };
            providers.push(Arc::new(registry.clone()));
            (Some(registry), Some(watch))
        };
        let tools = declared_tools(&providers)?;
        let mcp_servers = mcp.as_ref().map(mcp_reports).unwrap_or_default();
        let watch = watch.map(|watch| McpWatch {
            declared: tools.clone(),
            reported: mcp_servers.clone(),
            ..watch
        });
        let info = Arc::new(RuntimeInfo::coding(&state.runtime_id, workspace));
        let id = format!(
            "coding_{}",
            self.next_instance.fetch_add(1, Ordering::Relaxed)
        );
        let opened = CapabilityOpenResult {
            instance: id.clone(),
            attachment_name: info.attachment_name().to_owned(),
            workspace: info
                .workspace()
                .map(|workspace| workspace.root().to_string_lossy().into_owned()),
            tools,
            skills: wire_skills(&skills),
            mcp_servers,
        };
        Ok((
            Arc::new(Instance {
                id,
                info,
                providers,
                mcp,
                opened,
            }),
            watch,
        ))
    }

    /// Reports an instance's MCP servers' status and tool list to the
    /// daemon as they change, until the runtime stops.
    async fn report_mcp(self: Arc<Self>, instance: Arc<Instance>, mut watch: McpWatch) {
        loop {
            let failure = tokio::select! {
                () = self.stop.cancelled() => return,
                changed = watch.changes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    None
                }
                failure = watch.failures.recv() => match failure {
                    Ok(failure) => Some(failure),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => None,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
            };
            if let Ok(tools) = declared_tools(&instance.providers)
                && tools != watch.declared
            {
                watch.declared = tools;
                if self
                    .peer
                    .notify(
                        method::CAPABILITY_TOOLS_CHANGED,
                        &CapabilityToolsChangedParams {
                            instance: instance.id.clone(),
                            tools: watch.declared.clone(),
                        },
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
            let servers = mcp_reports(&watch.registry);
            if servers == watch.reported && failure.is_none() {
                continue;
            }
            watch.reported = servers;
            let params = McpChangedParams {
                instance: instance.id.clone(),
                servers: watch.reported.clone(),
                failures: failure
                    .into_iter()
                    .map(|McpServerFailure { server, reason }| McpFailureReport { server, reason })
                    .collect(),
            };
            if self
                .peer
                .notify(method::MCP_CHANGED, &params)
                .await
                .is_err()
            {
                return;
            }
        }
    }

    async fn context(
        &self,
        state: &Initialized,
        request: &IncomingRequest,
    ) -> Result<ContextResult, RpcError> {
        let params: ContextParams = request.params()?;
        let instance = self.instance(Some(&params.instance))?;
        if let Some(preset) = &params.preset {
            cookie_agent_protocol::AgentId::new(preset.as_str()).map_err(|_| {
                RpcError::from(
                    errors::BAD_PARAMS
                        .error()
                        .field("field", "preset")
                        .detail(format!("invalid agent preset `{preset}`")),
                )
            })?;
        }
        let workspace = instance
            .info
            .workspace()
            .expect("a coding runtime has a workspace")
            .root()
            .to_owned();
        let max_bytes = state.settings.agent_md.max_bytes;
        let context = tokio::task::spawn_blocking(move || {
            load_agent_md(&workspace, params.preset.as_deref(), max_bytes)
        })
        .await
        .map_err(|error| {
            RpcError::from(
                errors::INTERNAL
                    .error()
                    .detail(format!("AGENTS.md lookup failed: {error}")),
            )
        })?
        .map_err(|error| {
            RpcError::from(
                errors::CONTEXT_FAILED
                    .error()
                    .field("path", error.path.to_string_lossy())
                    .detail(error.to_string()),
            )
        })?;
        Ok(ContextResult {
            agent_md: context.entries,
            skipped: context.skipped,
        })
    }

    async fn prepare(&self, request: &IncomingRequest) -> Result<PrepareResult, RpcError> {
        let params: PrepareParams = request.params()?;
        let state = self.instance(params.instance.as_deref())?;
        if params.calls.len() > protocol::MAX_PREPARE_BATCH {
            return Err(errors::BAD_PARAMS
                .error()
                .field("field", "calls")
                .detail(format!(
                    "at most {} calls are prepared at once",
                    protocol::MAX_PREPARE_BATCH
                ))
                .into());
        }
        let context = ToolPreparationContext {
            session: params.session,
            run: params.run,
            origin: ToolOrigin::Runtime(Arc::clone(&state.info)),
            turn_context: Arc::new(params.turn.into()),
        };
        let calls: Vec<ToolCall> = params
            .calls
            .into_iter()
            .map(|call| ToolCall {
                id: call.id,
                name: call.name,
                arguments: call.arguments,
            })
            .collect();
        // Calls grouped by provider, so one provider prepares its calls of
        // the turn together and same-target calls chain.
        let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut outcomes: Vec<Option<PrepareOutcome>> = (0..calls.len()).map(|_| None).collect();
        for (index, call) in calls.iter().enumerate() {
            match state.route(&call.name) {
                Some(provider) => match groups.iter_mut().find(|(group, _)| *group == provider) {
                    Some((_, indexes)) => indexes.push(index),
                    None => groups.push((provider, vec![index])),
                },
                None => {
                    outcomes[index] = Some(PrepareOutcome::Failed {
                        error: WireToolError::from(&ToolError::execution(format!(
                            "tool `{}` is unavailable",
                            call.name
                        ))),
                        presentation: cookie_agent_runtime_api::tool_title_only(&call.name),
                    });
                }
            }
        }
        for (provider_index, indexes) in groups {
            let provider = &state.providers[provider_index];
            let group = indexes.iter().map(|index| calls[*index].clone()).collect();
            let results = provider.prepare_parallel(context.clone(), group).await;
            for (index, result) in indexes.into_iter().zip(results) {
                outcomes[index] =
                    Some(self.prepared_outcome(provider.as_ref(), &calls[index], result));
            }
        }
        Ok(PrepareResult {
            results: outcomes
                .into_iter()
                .map(|outcome| outcome.expect("every call has an outcome"))
                .collect(),
        })
    }

    fn prepared_outcome(
        &self,
        provider: &dyn ToolProvider,
        call: &ToolCall,
        result: Result<cookie_agent_runtime_api::PreparedTool, ToolError>,
    ) -> PrepareOutcome {
        let presentation = provider.presentation(call);
        let prepared = result.and_then(|prepared| {
            let (permission_name, resource) =
                provider.get_permission_resource(&call.name, prepared.normalized_arguments())?;
            let permission = permission_name
                .map(|name| {
                    protocol::permission_action(name).ok_or_else(|| {
                        ToolError::execution(format!(
                            "tool `{}` has permission `{name}`, which is not a permission action",
                            call.name
                        ))
                    })
                })
                .transpose()?;
            Ok((prepared, permission, resource))
        });
        match prepared {
            Ok((prepared, permission, permission_resource)) => {
                let prepared_id = self.next_prepared.fetch_add(1, Ordering::Relaxed);
                let declaration = PreparedCall {
                    prepared_id,
                    operation: prepared.operation().clone(),
                    policy_labels: prepared.policy_labels().to_vec(),
                    normalized_arguments: prepared.normalized_arguments().clone(),
                    permission,
                    permission_resource,
                    presentation,
                    serialization_key: prepared
                        .serialization_key()
                        .map(|key| protocol::hex_encode(key.as_bytes())),
                };
                lock(&self.prepared).insert(prepared_id, prepared);
                PrepareOutcome::Prepared(declaration)
            }
            Err(error) => PrepareOutcome::Failed {
                error: WireToolError::from(&error),
                presentation,
            },
        }
    }

    async fn revalidate(&self, request: &IncomingRequest) -> Result<Outcome, RpcError> {
        let PreparedRef { prepared_id } = request.params()?;
        let executor = lock(&self.prepared)
            .get(&prepared_id)
            .map(|prepared| Arc::clone(prepared.executor()))
            .ok_or_else(|| unknown_handle("prepared call", prepared_id))?;
        let executor = executor.lock().await;
        let result = match executor.as_ref() {
            Some(executor) => executor.revalidate().await,
            None => Err(ToolError::execution(
                "prepared executor capability was already consumed",
            )),
        };
        Ok(Outcome::of(result))
    }

    async fn execute(
        self: &Arc<Self>,
        admitted: AdmittedExecution,
    ) -> Result<ExecuteResult, RpcError> {
        let AdmittedExecution { params, cancel } = admitted;
        let prepared_id = params.prepared_id;
        let prepared = lock(&self.prepared)
            .remove(&prepared_id)
            .ok_or_else(|| unknown_handle("prepared call", prepared_id))?;
        let Some(executor) = prepared.executor().lock().await.take() else {
            return Ok(ExecuteResult {
                outcome: ExecuteOutcome::Failed {
                    error: WireToolError::from(&ToolError::execution(
                        "prepared executor capability was already consumed",
                    )),
                },
                media: Vec::new(),
            });
        };
        let artifacts = Arc::new(InlineArtifacts::default());
        let (detach, mut detach_watch) = DetachPoint::channel();
        // The daemon decides whether to detach; it only needs to hear when
        // the call may be.
        let peer = self.peer.clone();
        let announce = tokio::spawn(async move {
            let detachable = detach_watch.reached().await;
            let _ = peer
                .notify(
                    method::TOOL_DETACHABLE,
                    &DetachableParams {
                        prepared_id,
                        note: detachable.note,
                        reports_itself: detachable.reports_itself,
                    },
                )
                .await;
        });
        let context = ToolExecutionContext {
            session: params.session,
            run: params.run,
            progress: RemoteProgress::sink(self.peer.clone(), prepared_id),
            cancellation: cancel,
            turn_context: Arc::new(params.turn.into()),
            artifacts: artifacts.clone(),
            detach,
            background: params.background,
            producers: self.options.producer_messaging.then(|| {
                Arc::new(PeerProducers {
                    peer: self.peer.clone(),
                }) as Arc<dyn cookie_agent_runtime_api::ProducerChannel>
            }),
        };
        let result = AssertUnwindSafe(executor.execute(context))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(ToolError::execution("tool execution panicked")));
        announce.abort();
        let outcome = match result {
            Ok(completion) => ExecuteOutcome::Completed {
                output: completion.output,
                result: completion.result,
                failed: completion.failed,
            },
            Err(error) => ExecuteOutcome::Failed {
                error: WireToolError::from(&error),
            },
        };
        Ok(ExecuteResult {
            outcome,
            media: artifacts.take(),
        })
    }
}

async fn reply<T: Serialize>(request: IncomingRequest, result: Result<T, RpcError>) {
    let _ = match result {
        Ok(result) => request.respond(&result).await,
        Err(error) => request.fail(error).await,
    };
}

/// What [`Server::report_mcp`] watches.
struct McpWatch {
    registry: McpRegistry,
    changes: tokio::sync::watch::Receiver<u64>,
    failures: tokio::sync::broadcast::Receiver<McpServerFailure>,
    /// The tool list and server status the daemon last got.
    declared: RuntimeTools,
    reported: Vec<McpServerReport>,
}

/// The template's MCP servers, parsed and checked like the daemon's
/// `[mcp.servers.<name>]` tables. OAuth stays with the daemon's global
/// runtime, so a server that enables it is refused.
fn mcp_servers(
    settings: &[McpServerSettings],
) -> Result<BTreeMap<String, LoadedMcpServer>, String> {
    let mut servers = BTreeMap::new();
    for server in settings {
        let config: McpServerConfig = serde_json::from_value(server.config.clone())
            .map_err(|error| format!("MCP server `{}`: {error}", server.name))?;
        config
            .validate(&server.name)
            .map_err(|error| format!("MCP server `{}`: {error}", server.name))?;
        if matches!(
            config.oauth,
            McpOAuthConfig::Enabled | McpOAuthConfig::Settings(_)
        ) {
            return Err(format!(
                "MCP server `{}` enables OAuth, which only the daemon's global runtime supports",
                server.name
            ));
        }
        let loaded = LoadedMcpServer {
            source: McpServerSource::UserFile,
            config,
        };
        if servers.insert(server.name.clone(), loaded).is_some() {
            return Err(format!("MCP server `{}` is listed twice", server.name));
        }
    }
    Ok(servers)
}

/// The status of every server `registry` hosts, as the protocol reports it.
fn mcp_reports(registry: &McpRegistry) -> Vec<McpServerReport> {
    registry.statuses().into_iter().map(mcp_report).collect()
}

fn mcp_report(status: McpServerStatus) -> McpServerReport {
    use cookie_agent_mcp::McpServerState as State;
    use protocol::McpServerState as Wire;
    McpServerReport {
        name: status.server,
        state: match status.state {
            State::Disabled => Wire::Disabled,
            State::Disconnected => Wire::Disconnected,
            State::Connecting => Wire::Connecting,
            State::Connected => Wire::Connected,
            State::Reconnecting => Wire::Reconnecting,
            // A runtime's servers never use OAuth.
            State::NeedsAuth | State::Failed => Wire::Failed,
        },
        message: status.message,
        tools: status.tools,
    }
}

/// `mcp/reconnect`: reconnects one server now, or retries every failed one,
/// and answers once the attempts have finished.
impl Server {
    async fn reconnect_mcp(&self, request: &IncomingRequest) -> Result<Outcome, RpcError> {
        let params: McpReconnectParams = request.params()?;
        let state = self.instance(Some(&params.instance))?;
        let Some(registry) = &state.mcp else {
            return Ok(Outcome::of(match params.server {
                Some(server) => Err(ToolError::execution(format!(
                    "unknown MCP server `{server}`"
                ))),
                None => Ok(()),
            }));
        };
        Ok(Outcome::of(match params.server {
            Some(server) => registry.reconnect_server(&server).await,
            None => {
                registry.retry_failed(&tokio::runtime::Handle::current());
                registry.await_eager_ready().await;
                Ok(())
            }
        }))
    }
}

/// The tool declarations of `providers`, whose names must be unique.
fn declared_tools(providers: &[Arc<dyn ToolProvider>]) -> Result<RuntimeTools, String> {
    let context = SessionToolContext::new(SessionId::new_v7());
    let mut tools = RuntimeTools::default();
    let mut names = std::collections::HashSet::new();
    for provider in providers {
        for spec in provider
            .tools_for_session(&context)
            .map_err(|error| format!("listing tools: {error}"))?
        {
            if !names.insert(spec.name.clone()) {
                return Err(format!("tool `{}` is declared twice", spec.name));
            }
            tools.tools.push(ToolDeclaration::from_spec(&spec)?);
        }
        tools.prompt_sections.extend(
            provider
                .prompt_sections(&context)
                .map_err(|error| format!("prompt sections: {error}"))?
                .into_iter()
                .map(PromptSectionDeclaration::from),
        );
    }
    Ok(tools)
}

fn wire_skills(registry: &cookie_agent_config::SkillRegistry) -> RuntimeSkills {
    let path = |path: &std::path::Path| path.to_string_lossy().into_owned();
    RuntimeSkills {
        skills: registry
            .skills()
            .map(|(_, skill)| SkillDeclaration {
                document: skill.document.clone(),
                source: skill.source,
                path: path(&skill.path),
                base_dir: path(&skill.base_dir),
                supporting_files: skill
                    .supporting_files
                    .iter()
                    .map(|file| path(file))
                    .collect(),
            })
            .collect(),
        discoveries: registry
            .discoveries()
            .iter()
            .map(|discovery| SkillDiscoveryDeclaration {
                name: discovery.name.clone(),
                source: discovery.source,
                path: path(&discovery.path),
                precedence_winner: discovery.precedence_winner,
            })
            .collect(),
    }
}

/// Sends a call's progress to the daemon and waits until the
/// daemon has captured it, so output stays in order and capture limits
/// reach the tool.
#[derive(Debug)]
struct RemoteProgress {
    peer: Peer,
    prepared_id: u64,
    /// Keeps the sink's display channel open. Display deltas travel with
    /// the progress itself, so nothing is ever sent on it.
    _display: mpsc::Receiver<ToolProgress>,
}

impl RemoteProgress {
    fn sink(peer: Peer, prepared_id: u64) -> ProgressSink {
        let (sender, display) = mpsc::channel(1);
        ProgressSink::with_capture(
            sender,
            Arc::new(Self {
                peer,
                prepared_id,
                _display: display,
            }),
        )
    }
}

#[async_trait]
impl ProgressCapture for RemoteProgress {
    async fn emit(
        &self,
        progress: ToolProgress,
        display: Option<mpsc::OwnedPermit<ToolProgress>>,
    ) -> Result<(), ToolError> {
        drop(display);
        let outcome: Result<Outcome, RpcError> = self
            .peer
            .request(
                method::TOOL_PROGRESS,
                &ToolProgressParams {
                    prepared_id: self.prepared_id,
                    progress: progress.into(),
                },
            )
            .await;
        outcome
            .map_err(|_| ToolError::ProgressSinkClosed)?
            .into_result()
    }
}

/// Attachment bytes a call retains, sent inline with its result.
#[derive(Debug, Default)]
struct InlineArtifacts {
    blobs: Mutex<RetainedBlobs>,
}

#[derive(Debug, Default)]
struct RetainedBlobs {
    /// Content by lowercase hex SHA-256, in retention order.
    blobs: Vec<(String, Vec<u8>)>,
    total: u64,
}

impl InlineArtifacts {
    fn take(&self) -> Vec<InlineMedia> {
        std::mem::take(&mut *lock(&self.blobs))
            .blobs
            .into_iter()
            .map(|(sha256, bytes)| InlineMedia {
                sha256,
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
            .collect()
    }
}

#[async_trait]
impl ToolArtifacts for InlineArtifacts {
    fn retain(
        &self,
        _session: SessionId,
        content: &[u8],
    ) -> Result<(cookie_agent_protocol::ArtifactReference, String), ToolError> {
        let digest = cookie_agent_protocol::Sha256Digest::of_bytes(content)
            .as_str()
            .to_owned();
        let mut retained = lock(&self.blobs);
        if !retained.blobs.iter().any(|(sha256, _)| *sha256 == digest) {
            let total = retained.total + content.len() as u64;
            if total > protocol::MAX_INLINE_MEDIA_BYTES {
                return Err(ToolError::resource_limit(format!(
                    "a result carries at most {} bytes of media",
                    protocol::MAX_INLINE_MEDIA_BYTES
                )));
            }
            retained.total = total;
            retained.blobs.push((digest.clone(), content.to_vec()));
        }
        Ok((protocol::artifact_reference(&digest), digest))
    }

    async fn read_page(
        self: Arc<Self>,
        _session: SessionId,
        _target: cookie_agent_protocol::ArtifactReadTarget,
        _offset: u64,
        _limit: u64,
    ) -> Result<cookie_agent_runtime_api::ArtifactReadPage, ToolError> {
        Err(ToolError::execution(
            "artifact reads are unavailable inside a runtime; use read_artifact",
        ))
    }
}

impl std::fmt::Debug for Server {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Server").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod mcp_tests;
#[cfg(test)]
mod tests;

/// A tool's producer channel: `producer/*` requests to the daemon over the
/// runtime's connection.
#[derive(Debug)]
struct PeerProducers {
    peer: Peer,
}

impl PeerProducers {
    async fn request<P: Serialize + Sync, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, ToolError> {
        self.peer
            .request(method, params)
            .await
            .map_err(|error| ToolError::execution(format!("{method}: {error}")))
    }
}

#[async_trait]
impl cookie_agent_runtime_api::ProducerChannel for PeerProducers {
    async fn register(
        &self,
        session: SessionId,
        name: cookie_agent_protocol::ProducerName,
    ) -> Result<cookie_agent_protocol::ProducerId, ToolError> {
        let result: cookie_agent_protocol::ProducerRegisterResult = self
            .request(
                method::PRODUCER_REGISTER,
                &cookie_agent_protocol::ProducerRegisterParams {
                    session_id: session,
                    name,
                },
            )
            .await?;
        Ok(result.producer_id)
    }

    async fn send(
        &self,
        session: SessionId,
        producer: cookie_agent_protocol::ProducerId,
        message: cookie_agent_runtime_api::ProducerMessage,
    ) -> Result<cookie_agent_protocol::ProducerMessageId, ToolError> {
        let result: cookie_agent_protocol::ProducerSendResult = self
            .request(
                method::PRODUCER_SEND,
                &cookie_agent_protocol::ProducerSendParams {
                    session_id: session,
                    producer_id: producer,
                    mode: message.mode,
                    idempotency_key: message.idempotency_key,
                    description: message.description,
                    body: message.body,
                },
            )
            .await?;
        Ok(result.message_id)
    }

    async fn unregister(
        &self,
        session: SessionId,
        producer: cookie_agent_protocol::ProducerId,
    ) -> Result<(), ToolError> {
        let _: cookie_agent_protocol::ProducerUnregisterResult = self
            .request(
                method::PRODUCER_UNREGISTER,
                &cookie_agent_protocol::ProducerUnregisterParams {
                    session_id: session,
                    producer_id: producer,
                },
            )
            .await?;
        Ok(())
    }

    async fn discard(
        &self,
        session: SessionId,
        message: cookie_agent_protocol::ProducerMessageId,
    ) -> Result<(), ToolError> {
        let _: cookie_agent_protocol::ProducerDiscardResult = self
            .request(
                method::PRODUCER_DISCARD,
                &cookie_agent_protocol::ProducerDiscardParams {
                    session_id: session,
                    message_id: message,
                },
            )
            .await?;
        Ok(())
    }
}
