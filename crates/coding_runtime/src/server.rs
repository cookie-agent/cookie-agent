//! The coding runtime's end of the runtime protocol.
//!
//! [`serve`] answers one daemon over a frame transport: it initializes a
//! coding runtime for its workspace (tools, project skills), answers
//! `runtime/context` with the workspace's `AGENTS.md`, and prepares and
//! executes calls. It keeps each prepared call under its prepared ID until
//! the daemon executes or releases it, runs executions concurrently, sends
//! their progress back with `tool/progress`, and runs background jobs
//! locally, reporting them with `job/*`. It hosts the template's MCP servers (`docs/site/specs/runtimes.md`
//! section 3): it connects them before it answers `runtime/initialize`,
//! declares their tools next to its built-in ones, reconnects dropped ones
//! with the shared reconnect policy, and reports their status with
//! `mcp/changed` and a changed tool list with `runtime/tools_changed`. It
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
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use base64::Engine as _;
use cookie_agent_config::{LoadedMcpServer, McpOAuthConfig, McpServerConfig, McpServerSource};
use cookie_agent_mcp::{McpRegistry, McpRegistryOptions, McpServerFailure, McpServerStatus};
use cookie_agent_protocol::{SessionId, Transport};
use cookie_agent_runtime_api::{
    BackgroundJob, BackgroundJobs, BackgroundLauncher, BackgroundTask, CodingToolsFactory,
    ProgressCapture, ProgressSink, RuntimeConnection, RuntimeConnector, RuntimeInfo,
    SessionToolContext, ToolArtifacts, ToolCall, ToolError, ToolExecutionContext, ToolOrigin,
    ToolPreparationContext, ToolProgress, ToolProvider, WorkspaceContext,
    context::{AGENT_MD_MAX_BYTES, load_agent_md},
    protocol::{
        self, ContextParams, ContextResult, Empty, ExecuteOutcome, ExecuteParams, ExecuteResult,
        InitializeParams, InitializeResult, InlineMedia, JobCompletedParams, JobProgressParams,
        JobRef, JobStartedParams, McpChangedParams, McpFailureReport, McpReconnectParams,
        McpServerReport, McpServerSettings, Outcome, PrepareOutcome, PrepareParams, PrepareResult,
        PreparedCall, PreparedRef, PromptSectionDeclaration, RUNTIME_PROTOCOL_VERSION,
        RuntimeSettings, RuntimeSkills, RuntimeTools, SkillDeclaration, SkillDiscoveryDeclaration,
        ToolDeclaration, ToolProgressParams, WireToolError, error_code, method,
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
    /// The workspace when `runtime/initialize` names none, such as the
    /// `--workspace` argument of `cookie runtime coding`.
    pub workspace: Option<PathBuf>,
    /// Builds the runtime's tools for its workspace.
    pub tools: CodingToolsFactory,
    /// How dropped MCP servers are reconnected; the shared default when
    /// absent. Tests that cannot wait for the production backoff set it.
    pub mcp_reconnect: Option<ReconnectPolicy>,
}

impl Default for ServeOptions {
    /// No default workspace and the [`crate::BuiltinTools`].
    fn default() -> Self {
        Self {
            workspace: None,
            tools: builtin_tools(),
            mcp_reconnect: None,
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
    async fn connect(&self, _workspace: Option<&Path>) -> Result<RuntimeConnection, String> {
        let (daemon, runtime) = cookie_agent_protocol::in_process_pair(64);
        tokio::spawn(serve(Box::new(runtime), self.options.clone()));
        Ok(RuntimeConnection::new(Box::new(daemon)))
    }
}

/// A connector that runs a coding runtime with the [`crate::BuiltinTools`]
/// in this process. The daemon names the workspace in
/// `runtime/initialize`.
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
/// The daemon names the workspace in `runtime/initialize`, so
/// `options.workspace` should be `None`.
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

/// Serves one daemon over `transport` until it sends `runtime/shutdown` or
/// the transport ends. Calls and background jobs still running are
/// cancelled when it returns.
pub async fn serve(transport: Box<dyn Transport>, options: ServeOptions) {
    let (peer, mut incoming) = Peer::start(transport);
    let server = Arc::new(Server {
        peer: peer.clone(),
        options,
        initializing: AtomicBool::new(false),
        state: OnceLock::new(),
        prepared: Mutex::default(),
        executions: Mutex::default(),
        jobs: Mutex::default(),
        next_prepared: AtomicU64::new(1),
        next_job: AtomicU64::new(1),
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
    lock(&server.jobs).clear();
    if let Some(mcp) = server.state.get().and_then(|state| state.mcp.clone()) {
        mcp.shutdown().await;
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
    /// Set by the first `runtime/initialize`, which may wait for MCP
    /// servers before it sets `state`.
    initializing: AtomicBool,
    state: OnceLock<Arc<Initialized>>,
    /// Prepared calls by prepared ID, until executed or released.
    prepared: Mutex<HashMap<u64, cookie_agent_runtime_api::PreparedTool>>,
    /// Executing calls by prepared ID.
    executions: Mutex<HashMap<u64, Execution>>,
    /// Running background jobs by job ID.
    jobs: Mutex<HashMap<u64, CancellationToken>>,
    next_prepared: AtomicU64,
    next_job: AtomicU64,
    /// Cancelled when the server stops; every call and job runs under a
    /// child of it.
    stop: CancellationToken,
}

struct Initialized {
    info: Arc<RuntimeInfo>,
    settings: RuntimeSettings,
    /// The built-in providers, then the MCP registry when the runtime hosts
    /// MCP servers. Names are unique across them: the registry fails a
    /// server whose tool would take a built-in name.
    providers: Vec<Arc<dyn ToolProvider>>,
    /// The template's MCP servers, when it has any.
    mcp: Option<McpRegistry>,
}

impl Initialized {
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
    RpcError::remote(error_code::UNKNOWN_HANDLE, format!("unknown {what} {id}"))
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
            return match self.initialize(&request).await {
                Ok((result, watch)) => {
                    reply(request, Ok(result)).await;
                    if let Some(watch) = watch {
                        tokio::spawn(Arc::clone(&self).report_mcp(watch));
                    }
                }
                Err(error) => reply::<InitializeResult>(request, Err(error)).await,
            };
        }
        let Some(state) = self.state.get().cloned() else {
            if let Some(admitted) = admitted {
                lock(&self.executions).remove(&admitted.params.prepared_id);
            }
            let _ = request
                .fail(RpcError::remote(
                    error_code::NOT_INITIALIZED,
                    format!("`{method}` before `{}`", method::INITIALIZE),
                ))
                .await;
            return;
        };
        match method.as_str() {
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
                let result = self.prepare(&state, &request).await;
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
                        RpcError::remote(-32603, "the execution was not admitted")
                    })),
                };
                reply(request, result).await;
            }
            method::MCP_RECONNECT => {
                let result = reconnect_mcp(&state, &request).await;
                reply(request, result).await;
            }
            other => {
                let message = format!("unknown method `{other}`");
                let _ = request
                    .fail(RpcError::remote(error_code::METHOD_NOT_FOUND, message))
                    .await;
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
            method::JOB_KILL => {
                if let Ok(JobRef { job }) = notification.params()
                    && let Some(kill) = lock(&self.jobs).remove(&job)
                {
                    kill.cancel();
                }
            }
            // Notifications this runtime does not know are ignored.
            _ => {}
        }
    }

    /// Initializes the runtime: its workspace, tools, and skills, and its
    /// MCP servers, each of which has finished its first connection attempt
    /// when this returns. Returns the reply and, when the runtime hosts MCP
    /// servers, what [`Self::report_mcp`] watches once the reply is sent.
    async fn initialize(
        &self,
        request: &IncomingRequest,
    ) -> Result<(InitializeResult, Option<McpWatch>), RpcError> {
        let params: InitializeParams = request.params()?;
        if params.version != RUNTIME_PROTOCOL_VERSION {
            return Err(RpcError::remote(
                error_code::VERSION_MISMATCH,
                format!(
                    "runtime protocol version mismatch: the daemon speaks version {}, this \
                     runtime speaks version {RUNTIME_PROTOCOL_VERSION}",
                    params.version
                ),
            ));
        }
        let rejected = |message: String| RpcError::remote(error_code::INITIALIZE_REJECTED, message);
        if self.initializing.swap(true, Ordering::AcqRel) {
            return Err(rejected("the runtime is already initialized".into()));
        }
        let result = self.initialize_once(params).await.map_err(rejected);
        if result.is_err() {
            self.initializing.store(false, Ordering::Release);
        }
        result
    }

    async fn initialize_once(
        &self,
        params: InitializeParams,
    ) -> Result<(InitializeResult, Option<McpWatch>), String> {
        cookie_agent_protocol::RuntimeId::new(params.runtime_id.as_str())
            .map_err(|_| format!("invalid runtime ID `{}`", params.runtime_id))?;
        let mcp_servers = mcp_servers(&params.settings.mcp_servers)?;
        if params.settings.agent_md.max_bytes > AGENT_MD_MAX_BYTES {
            return Err(format!("agent_md.max_bytes exceeds {AGENT_MD_MAX_BYTES}"));
        }
        let requested = params
            .workspace
            .map(|workspace| expand_home(Path::new(&workspace)));
        let path = match (requested, &self.options.workspace) {
            (Some(requested), Some(own)) => {
                let same = requested
                    .canonicalize()
                    .ok()
                    .zip(own.canonicalize().ok())
                    .is_some_and(|(requested, own)| requested == own);
                if !same {
                    return Err(format!(
                        "the daemon asks for workspace {}, but this runtime serves {}",
                        requested.display(),
                        own.display()
                    ));
                }
                requested
            }
            (Some(requested), None) => requested,
            (None, Some(own)) => own.clone(),
            (None, None) => return Err("no workspace was given".into()),
        };
        let workspace = WorkspaceContext::new(&path)
            .map_err(|error| format!("workspace {} is unavailable: {error}", path.display()))?;
        let skills = cookie_agent_config::load_project_skills(workspace.root())
            .map_err(|error| format!("project skills: {error}"))?;
        let mut providers = (self.options.tools)(&workspace);
        let (mcp, watch) = if mcp_servers.is_empty() {
            (None, None)
        } else {
            let registry = McpRegistry::new(mcp_servers, McpRegistryOptions::default())
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
        let info = Arc::new(RuntimeInfo::coding(&params.runtime_id, workspace));
        let result = InitializeResult {
            version: RUNTIME_PROTOCOL_VERSION,
            kind: info.kind().clone(),
            attachment_name: info.attachment_name().to_owned(),
            workspace: info
                .workspace()
                .map(|workspace| workspace.root().to_string_lossy().into_owned()),
            platform: Some(std::env::consts::OS.to_owned()),
            tools,
            skills: wire_skills(&skills),
            mcp_servers,
        };
        let initialized = Arc::new(Initialized {
            info,
            settings: params.settings,
            providers,
            mcp,
        });
        if self.state.set(initialized).is_err() {
            return Err("the runtime is already initialized".into());
        }
        Ok((result, watch))
    }

    /// Reports the MCP servers' status and the tool list to the daemon as
    /// they change, until the runtime stops.
    async fn report_mcp(self: Arc<Self>, mut watch: McpWatch) {
        let Some(state) = self.state.get().cloned() else {
            return;
        };
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
            if let Ok(tools) = declared_tools(&state.providers)
                && tools != watch.declared
            {
                watch.declared = tools;
                if self
                    .peer
                    .notify(method::TOOLS_CHANGED, &watch.declared)
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
        if let Some(preset) = &params.preset {
            cookie_agent_protocol::AgentId::new(preset.as_str()).map_err(|_| {
                RpcError::remote(
                    error_code::INVALID_PARAMS,
                    format!("invalid agent preset `{preset}`"),
                )
            })?;
        }
        let workspace = state
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
        .map_err(|error| RpcError::remote(-32603, format!("AGENTS.md lookup failed: {error}")))?
        .map_err(|error| RpcError::Remote {
            code: error_code::CONTEXT_FAILED,
            message: error.to_string(),
            data: Some(serde_json::json!({
                "path": error.path.to_string_lossy(),
            })),
        })?;
        Ok(ContextResult {
            agent_md: context.entries,
            skipped: context.skipped,
        })
    }

    async fn prepare(
        &self,
        state: &Initialized,
        request: &IncomingRequest,
    ) -> Result<PrepareResult, RpcError> {
        let params: PrepareParams = request.params()?;
        if params.calls.len() > protocol::MAX_PREPARE_BATCH {
            return Err(RpcError::remote(
                error_code::INVALID_PARAMS,
                format!(
                    "at most {} calls are prepared at once",
                    protocol::MAX_PREPARE_BATCH
                ),
            ));
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
            let permission = protocol::permission_action(permission_name).ok_or_else(|| {
                ToolError::execution(format!(
                    "tool `{}` has permission `{permission_name}`, which is not a permission \
                     action",
                    call.name
                ))
            })?;
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
                launched_jobs: Vec::new(),
            });
        };
        let artifacts = Arc::new(InlineArtifacts::default());
        let launched = Arc::new(Mutex::new(Vec::new()));
        let context = ToolExecutionContext {
            session: params.session,
            run: params.run,
            progress: RemoteProgress::sink(self.peer.clone(), ProgressTarget::Call(prepared_id)),
            cancellation: cancel,
            turn_context: Arc::new(params.turn.into()),
            artifacts: artifacts.clone(),
            background: params.background.then(|| {
                Arc::new(RemoteJobs {
                    server: Arc::downgrade(self),
                    prepared_id,
                    launched: Arc::clone(&launched),
                }) as Arc<dyn BackgroundJobs>
            }),
        };
        let result = AssertUnwindSafe(executor.execute(context))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(ToolError::execution("tool execution panicked")));
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
        let launched_jobs = std::mem::take(&mut *lock(&launched));
        Ok(ExecuteResult {
            outcome,
            media: artifacts.take(),
            launched_jobs,
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
async fn reconnect_mcp(
    state: &Initialized,
    request: &IncomingRequest,
) -> Result<Outcome, RpcError> {
    let params: McpReconnectParams = request.params()?;
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

#[derive(Clone, Copy, Debug)]
enum ProgressTarget {
    Call(u64),
    Job(u64),
}

/// Sends a call's or job's progress to the daemon and waits until the
/// daemon has captured it, so output stays in order and capture limits
/// reach the tool.
#[derive(Debug)]
struct RemoteProgress {
    peer: Peer,
    target: ProgressTarget,
    /// Keeps the sink's display channel open. Display deltas travel with
    /// the progress itself, so nothing is ever sent on it.
    _display: mpsc::Receiver<ToolProgress>,
}

impl RemoteProgress {
    fn sink(peer: Peer, target: ProgressTarget) -> ProgressSink {
        let (sender, display) = mpsc::channel(1);
        ProgressSink::with_capture(
            sender,
            Arc::new(Self {
                peer,
                target,
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
        let outcome: Result<Outcome, RpcError> = match self.target {
            ProgressTarget::Call(prepared_id) => {
                self.peer
                    .request(
                        method::TOOL_PROGRESS,
                        &ToolProgressParams {
                            prepared_id,
                            progress: progress.into(),
                        },
                    )
                    .await
            }
            ProgressTarget::Job(job) => {
                self.peer
                    .request(
                        method::JOB_PROGRESS,
                        &JobProgressParams {
                            job,
                            progress: progress.into(),
                        },
                    )
                    .await
            }
        };
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

/// Lets an executing call reserve background jobs: the daemon reserves its
/// side with `job/started`, and the job runs here.
#[derive(Debug)]
struct RemoteJobs {
    server: Weak<Server>,
    prepared_id: u64,
    launched: Arc<Mutex<Vec<u64>>>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Server").finish_non_exhaustive()
    }
}

#[async_trait]
impl BackgroundJobs for RemoteJobs {
    async fn reserve(&self, _session: SessionId) -> Result<BackgroundJob, ToolError> {
        let server = self
            .server
            .upgrade()
            .ok_or_else(|| ToolError::execution("the runtime is shutting down"))?;
        let job = server.next_job.fetch_add(1, Ordering::Relaxed);
        let outcome: Outcome = server
            .peer
            .request(
                method::JOB_STARTED,
                &JobStartedParams {
                    prepared_id: self.prepared_id,
                    job,
                },
            )
            .await
            .map_err(|error| ToolError::runtime_unavailable(error.to_string()))?;
        outcome.into_result()?;
        Ok(BackgroundJob::new(
            RemoteProgress::sink(server.peer.clone(), ProgressTarget::Job(job)),
            Box::new(RemoteLauncher {
                server: Weak::clone(&self.server),
                job,
                launched: Arc::clone(&self.launched),
            }),
        ))
    }
}

struct RemoteLauncher {
    server: Weak<Server>,
    job: u64,
    launched: Arc<Mutex<Vec<u64>>>,
}

impl BackgroundLauncher for RemoteLauncher {
    fn launch(self: Box<Self>, task: BackgroundTask) -> Result<(), ToolError> {
        let server = self
            .server
            .upgrade()
            .ok_or_else(|| ToolError::execution("the runtime is shutting down"))?;
        let job = self.job;
        let kill = server.stop.child_token();
        lock(&server.jobs).insert(job, kill.clone());
        lock(&self.launched).push(job);
        let peer = server.peer.clone();
        let server = Weak::clone(&self.server);
        tokio::spawn(async move {
            let report = tokio::select! {
                report = task => Some(report),
                () = kill.cancelled() => None,
            };
            if let Some(server) = server.upgrade() {
                lock(&server.jobs).remove(&job);
            }
            if let Some(report) = report {
                let _ = peer
                    .notify(
                        method::JOB_COMPLETED,
                        &JobCompletedParams {
                            job,
                            report: report.into(),
                        },
                    )
                    .await;
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod mcp_tests;
#[cfg(test)]
mod tests;
