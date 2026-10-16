//! The daemon's end of the runtime protocol: runtime instances that run in
//! another task or process and are reached over a frame transport.
//!
//! A [`ProtocolTemplate`] starts an instance with its [`RuntimeConnector`]
//! and sends `runtime/initialize`. The [`RuntimeClient`] it gets back keeps
//! the connection: its [`RemoteToolProvider`] publishes the tools the runtime
//! declared and proxies preparation and execution, it routes `tool/progress`
//! into the call's capture, and it hands `tool/detachable` to the call's
//! detach point. When the connection is lost, calls in flight, backgrounded
//! ones included, fail with `runtime_unavailable`.
//! The client also keeps what the runtime reports about the MCP servers it
//! hosts (`mcp/changed`), forwards their changes and failures to the
//! [`McpHub`], and asks the runtime to reconnect them (`mcp/reconnect`).
//!
//! A runtime in a child process plugs in as another [`RuntimeConnector`]
//! (`super::process`).

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use base64::Engine as _;
use cookie_agent_config::{McpServerConfig, SkillDiscovery, SkillRegistry};
use cookie_agent_protocol::{SessionId, ToolCallId, ToolCallPresentation};
use cookie_agent_runtime_api::{
    DetachPoint, DiagnosticTail, PreparedDeclaration, PreparedExecutor, PreparedSerializationKey,
    PreparedTool, ProgressSink, PromptSection, RuntimeConnection, RuntimeConnector, RuntimeInfo,
    SessionToolContext, ToolArtifacts, ToolCall, ToolCompletion, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec, WorkspaceContext,
    context::AgentMdContext,
    protocol::{
        self, ContextParams, ContextResult, DetachableParams, Empty, ExecuteOutcome, ExecuteParams,
        ExecuteResult, InitializeParams, InitializeResult, InlineMedia, McpChangedParams,
        McpReconnectParams, McpServerReport, Outcome, PrepareOutcome, PrepareParams, PrepareResult,
        PreparedCall, PreparedRef, RUNTIME_PROTOCOL_VERSION, RuntimeSettings, RuntimeSkills,
        RuntimeTools, ToolProgressParams, WireToolCall, error_code, method,
    },
    rpc::{Incoming, IncomingNotification, IncomingRequest, Peer, RpcError},
};
use tokio::sync::Notify;

use super::{
    McpHub, McpServerFailure,
    process::ProcessConnector,
    registry::{RuntimeRegistry, RuntimeTemplate, StartedRuntime},
};
use crate::EngineError;

/// Failed preparations whose presentation the engine has not asked for yet.
const MAX_PENDING_PRESENTATIONS: usize = 256;

/// A template whose instances speak the runtime protocol.
pub(crate) struct ProtocolTemplate {
    runtime_id: String,
    takes_workspace: bool,
    /// The workspace an instance gets when the template takes none, such as
    /// the daemon's working directory for `local`.
    fixed_workspace: Option<PathBuf>,
    connector: Arc<dyn RuntimeConnector>,
    settings: RuntimeSettings,
    /// Bound on connecting and initializing; `None` for an in-process
    /// runtime.
    startup_timeout: Option<Duration>,
    /// How long a stopping instance gets to answer `runtime/shutdown`.
    shutdown_grace: Duration,
    /// Where the MCP servers instances host report to.
    mcp_hub: Option<Arc<McpHub>>,
}

/// How long the daemon waits for the in-process `local` runtime to answer
/// `runtime/shutdown`.
pub(crate) const LOCAL_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

impl ProtocolTemplate {
    /// A configured template: its instances run `connector` for the
    /// workspace a session asks for when `takes_workspace`, and must answer
    /// `runtime/initialize` within `startup_timeout`.
    pub(crate) fn new(
        runtime_id: &str,
        takes_workspace: bool,
        connector: Arc<dyn RuntimeConnector>,
        settings: RuntimeSettings,
        startup_timeout: Duration,
        shutdown_grace: Duration,
    ) -> Self {
        Self {
            runtime_id: runtime_id.to_owned(),
            takes_workspace,
            fixed_workspace: None,
            connector,
            settings,
            startup_timeout: Some(startup_timeout),
            shutdown_grace,
            mcp_hub: None,
        }
    }

    /// Reports the MCP servers its instances host to `hub`.
    pub(crate) fn with_mcp_hub(mut self, hub: Arc<McpHub>) -> Self {
        self.mcp_hub = Some(hub);
        self
    }

    /// The `local` coding runtime for `workspace`.
    pub(crate) fn local(
        connector: Arc<dyn RuntimeConnector>,
        workspace: &Path,
        settings: RuntimeSettings,
    ) -> Self {
        Self {
            runtime_id: cookie_agent_runtime_api::LOCAL_RUNTIME_ID.to_owned(),
            takes_workspace: false,
            fixed_workspace: Some(workspace.to_owned()),
            connector,
            settings,
            startup_timeout: None,
            shutdown_grace: LOCAL_SHUTDOWN_GRACE,
            mcp_hub: None,
        }
    }

    async fn connect_and_initialize(
        &self,
        workspace: Option<&Path>,
        diagnostics: &Mutex<Option<DiagnosticTail>>,
    ) -> Result<StartedRuntime, String> {
        let connection = self.connector.connect(workspace).await?;
        lock(diagnostics).clone_from(&connection.diagnostics);
        let requested = workspace.or(self.fixed_workspace.as_deref());
        let (client, result) = RuntimeClient::initialize(
            connection,
            &self.runtime_id,
            requested,
            &self.settings,
            self.shutdown_grace,
            self.mcp_hub.clone(),
        )
        .await?;
        started(client, result)
    }
}

#[async_trait]
impl RuntimeTemplate for ProtocolTemplate {
    fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    fn takes_workspace(&self) -> bool {
        self.takes_workspace
    }

    async fn start(&self, workspace: Option<&Path>) -> Result<StartedRuntime, String> {
        let diagnostics = Mutex::new(None);
        let attempt = self.connect_and_initialize(workspace, &diagnostics);
        let result = match self.startup_timeout {
            Some(timeout) => tokio::time::timeout(timeout, attempt)
                .await
                .unwrap_or_else(|_| {
                    Err(format!(
                        "the runtime did not start within {} ms",
                        timeout.as_millis()
                    ))
                }),
            None => attempt.await,
        };
        result.map_err(|reason| match lock(&diagnostics).as_ref() {
            Some(tail) => tail.annotate(&reason),
            None => reason,
        })
    }
}

/// The settings instances of `runtime_id` get from the daemon's
/// configuration, its template's MCP servers included.
pub(crate) fn runtime_settings(
    config: &cookie_agent_config::LoadedConfiguration,
    runtime_id: &str,
) -> RuntimeSettings {
    let limits = crate::preview::PreviewLimits::from(&config.runtime.tool_output);
    RuntimeSettings {
        tool_output: protocol::ToolOutputLimits {
            max_lines: limits.max_lines as u64,
            max_bytes: limits.max_bytes as u64,
            head_lines: limits.head_lines as u64,
            tail_lines: limits.tail_lines as u64,
        },
        agent_md: protocol::AgentMdLimits {
            max_bytes: cookie_agent_runtime_api::context::AGENT_MD_MAX_BYTES,
        },
        mcp_servers: config
            .runtimes
            .iter()
            .find(|(id, _)| id.as_str() == runtime_id)
            .map(|(_, template)| {
                template
                    .mcp_servers
                    .iter()
                    .map(|(name, server)| protocol::McpServerSettings {
                        name: name.clone(),
                        config: serde_json::to_value(server)
                            .expect("MCP server configuration serializes"),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Registers a protocol template per configured `[runtimes.<id>]` other
/// than `local`, each starting its instances with a [`ProcessConnector`].
pub(crate) fn register_configured_templates(
    registry: &RuntimeRegistry,
    config: &cookie_agent_config::LoadedConfiguration,
    mcp_hub: &Arc<McpHub>,
) -> Result<(), EngineError> {
    for (runtime_id, template) in &config.runtimes {
        if runtime_id.is_reserved() {
            continue;
        }
        registry.register_template(Arc::new(
            ProtocolTemplate::new(
                runtime_id.as_str(),
                template.takes_workspace(),
                Arc::new(ProcessConnector::new(
                    template.command.clone(),
                    template.env.clone(),
                    template.shutdown_grace(),
                )),
                runtime_settings(config, runtime_id.as_str()),
                template.startup_timeout(),
                template.shutdown_grace(),
            )
            .with_mcp_hub(Arc::clone(mcp_hub)),
        ))?;
    }
    Ok(())
}

/// One connection to a runtime instance. Dropping the last handle closes it.
#[derive(Clone)]
pub(crate) struct RuntimeClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    peer: Peer,
    runtime_id: String,
    /// What the runtime writes outside the protocol, such as its stderr.
    diagnostics: Option<DiagnosticTail>,
    /// How long [`RuntimeClient::shutdown`] waits for the runtime.
    shutdown_grace: Duration,
    /// `None` until the initialize reply or the first `tools_changed`.
    tools: Mutex<Option<Arc<DeclaredTools>>>,
    /// Executing calls by prepared ID: where their progress and detach
    /// point reach the host.
    executions: Mutex<HashMap<u64, ExecutionHost>>,
    /// Presentations of calls that failed to prepare, by call.
    presentations: Mutex<VecDeque<(ToolCallId, ToolCallPresentation)>>,
    /// The MCP servers the runtime hosts.
    mcp: Mutex<HostedMcp>,
    mcp_hub: Option<Arc<McpHub>>,
    /// `mcp/reconnect` retries in flight; a run waits for them.
    mcp_retries: AtomicUsize,
    mcp_settled: Notify,
}

/// What the daemon knows about the MCP servers a runtime hosts.
#[derive(Default)]
struct HostedMcp {
    /// The instance's workspace, once initialized.
    workspace: Option<String>,
    /// The template's definitions, by server name.
    definitions: BTreeMap<String, McpServerConfig>,
    /// Their last reported status; `None` before the initialize reply or
    /// the first report.
    servers: Option<Vec<McpServerReport>>,
}

/// One MCP server a runtime hosts, as `mcp.server.list` shows it.
#[derive(Clone, Debug)]
pub(crate) struct HostedMcpServer {
    pub(crate) runtime_id: String,
    pub(crate) workspace: Option<String>,
    pub(crate) definition: McpServerConfig,
    pub(crate) report: McpServerReport,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        self.peer.close();
    }
}

#[derive(Default)]
struct DeclaredTools {
    specs: Vec<ToolSpec>,
    prompt_sections: Vec<PromptSection>,
}

impl From<RuntimeTools> for DeclaredTools {
    fn from(tools: RuntimeTools) -> Self {
        Self {
            specs: tools
                .tools
                .into_iter()
                .map(protocol::ToolDeclaration::into_spec)
                .collect(),
            prompt_sections: tools
                .prompt_sections
                .into_iter()
                .map(PromptSection::from)
                .collect(),
        }
    }
}

struct ExecutionHost {
    progress: ProgressSink,
    detach: DetachPoint,
}

/// One `mcp/reconnect` retry in flight.
struct McpRetry(Arc<ClientInner>);

impl Drop for McpRetry {
    fn drop(&mut self) {
        if self.0.mcp_retries.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.mcp_settled.notify_waiters();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl RuntimeClient {
    /// Connects over `transport` and initializes the runtime. The error is
    /// a human-readable reason.
    pub(crate) async fn initialize(
        connection: RuntimeConnection,
        runtime_id: &str,
        workspace: Option<&Path>,
        settings: &RuntimeSettings,
        shutdown_grace: Duration,
        mcp_hub: Option<Arc<McpHub>>,
    ) -> Result<(Self, InitializeResult), String> {
        let (peer, incoming) = Peer::start(connection.transport);
        let definitions = settings
            .mcp_servers
            .iter()
            .filter_map(|server| {
                serde_json::from_value(server.config.clone())
                    .ok()
                    .map(|config| (server.name.clone(), config))
            })
            .collect();
        let inner = Arc::new(ClientInner {
            peer,
            runtime_id: runtime_id.to_owned(),
            diagnostics: connection.diagnostics,
            shutdown_grace,
            tools: Mutex::default(),
            executions: Mutex::default(),
            presentations: Mutex::default(),
            mcp: Mutex::new(HostedMcp {
                definitions,
                ..HostedMcp::default()
            }),
            mcp_hub,
            mcp_retries: AtomicUsize::new(0),
            mcp_settled: Notify::new(),
        });
        tokio::spawn(handle_incoming(Arc::downgrade(&inner), incoming));
        let result: InitializeResult = inner
            .peer
            .request(
                method::INITIALIZE,
                &InitializeParams {
                    version: RUNTIME_PROTOCOL_VERSION,
                    runtime_id: runtime_id.to_owned(),
                    workspace: workspace.map(|path| path.to_string_lossy().into_owned()),
                    settings: settings.clone(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        if result.version != RUNTIME_PROTOCOL_VERSION {
            return Err(format!(
                "runtime protocol version mismatch: the daemon speaks version \
                 {RUNTIME_PROTOCOL_VERSION}, the runtime speaks version {}",
                result.version
            ));
        }
        // A notification the runtime sent after its reply may already have
        // been handled; it is newer than the reply.
        lock(&inner.tools).get_or_insert_with(|| Arc::new(result.tools.clone().into()));
        let failed = {
            let mut mcp = lock(&inner.mcp);
            mcp.workspace.clone_from(&result.workspace);
            let servers = mcp
                .servers
                .get_or_insert_with(|| result.mcp_servers.clone());
            servers
                .iter()
                .filter(|server| server.state == protocol::McpServerState::Failed)
                .map(|server| {
                    (
                        server.name.clone(),
                        server.message.clone().unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
        };
        if let Some(hub) = &inner.mcp_hub {
            for (server, reason) in failed {
                hub.failed(inner.mcp_failure(server, reason));
            }
            if !result.mcp_servers.is_empty() {
                hub.changed();
            }
        }
        Ok((Self { inner }, result))
    }

    /// The MCP servers the runtime hosts, with their last reported status.
    pub(crate) fn mcp_servers(&self) -> Vec<HostedMcpServer> {
        let mcp = lock(&self.inner.mcp);
        mcp.servers
            .iter()
            .flatten()
            .filter_map(|report| {
                Some(HostedMcpServer {
                    runtime_id: self.inner.runtime_id.clone(),
                    workspace: mcp.workspace.clone(),
                    definition: mcp.definitions.get(&report.name)?.clone(),
                    report: report.clone(),
                })
            })
            .collect()
    }

    /// Asks the runtime to reconnect its MCP server `server` now and waits
    /// for the attempt.
    pub(crate) async fn reconnect_mcp(&self, server: &str) -> Result<(), EngineError> {
        let outcome: Outcome = self
            .inner
            .peer
            .request(
                method::MCP_RECONNECT,
                &McpReconnectParams {
                    server: Some(server.to_owned()),
                },
            )
            .await
            .map_err(|error| {
                EngineError::Mcp(format!("runtime `{}`: {error}", self.inner.runtime_id))
            })?;
        outcome
            .into_result()
            .map_err(|error| EngineError::Mcp(error.to_string()))
    }

    /// Asks the runtime to retry its failed MCP servers, as when a session
    /// that uses it is created or opened; [`Self::mcp_settled`] waits for
    /// the attempts.
    pub(crate) fn retry_failed_mcp(&self) {
        let any_failed = lock(&self.inner.mcp)
            .servers
            .iter()
            .flatten()
            .any(|server| server.state == protocol::McpServerState::Failed);
        if !any_failed {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.inner.mcp_retries.fetch_add(1, Ordering::AcqRel);
        let retry = McpRetry(Arc::clone(&self.inner));
        handle.spawn(async move {
            let _ = retry
                .0
                .peer
                .request::<_, Outcome>(method::MCP_RECONNECT, &McpReconnectParams::default())
                .await;
            drop(retry);
        });
    }

    /// Waits until every retry [`Self::retry_failed_mcp`] started has
    /// finished, or the connection ended.
    pub(crate) async fn mcp_settled(&self) {
        while self.inner.mcp_retries.load(Ordering::Acquire) != 0 {
            let notified = self.inner.mcp_settled.notified();
            if self.inner.mcp_retries.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }
    }

    /// Whether the connection has ended.
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.peer.is_closed()
    }

    /// Waits for the connection to end and returns why it did, followed by
    /// what the runtime last wrote outside the protocol.
    pub(crate) fn closed(&self) -> impl std::future::Future<Output = String> + Send + 'static {
        let peer = self.inner.peer.clone();
        let diagnostics = self.inner.diagnostics.clone();
        async move {
            let reason = peer.closed().await;
            match diagnostics {
                Some(tail) => tail.annotate(&reason),
                None => reason,
            }
        }
    }

    /// Asks the runtime to stop and waits up to its shutdown grace for the
    /// answer, then closes the connection; a runtime process that has not
    /// exited after another grace is killed.
    pub(crate) async fn shutdown(&self) {
        let _ = tokio::time::timeout(
            self.inner.shutdown_grace,
            self.inner
                .peer
                .request::<_, Empty>(method::SHUTDOWN, &Empty {}),
        )
        .await;
        self.inner.peer.close();
    }

    /// The runtime's `AGENTS.md` context for a run of `preset`.
    pub(crate) async fn agent_md(
        &self,
        preset: Option<&str>,
    ) -> Result<Option<AgentMdContext>, EngineError> {
        let reply: Result<ContextResult, RpcError> = self
            .inner
            .peer
            .request(
                method::CONTEXT,
                &ContextParams {
                    preset: preset.map(str::to_owned),
                },
            )
            .await;
        match reply {
            Ok(context) => Ok(Some(AgentMdContext {
                entries: context.agent_md,
                skipped: context.skipped,
            })),
            // A runtime that is gone publishes nothing, its context included.
            Err(RpcError::Closed(_)) => Ok(None),
            Err(RpcError::Remote {
                code: error_code::CONTEXT_FAILED,
                message,
                data,
            }) => match data
                .as_ref()
                .and_then(|data| data.get("path"))
                .and_then(serde_json::Value::as_str)
            {
                Some(path) => Err(EngineError::AgentMdIo {
                    path: PathBuf::from(path),
                    source: std::io::Error::other(message),
                }),
                None => Err(EngineError::Runtime(message)),
            },
            Err(error) => Err(EngineError::Runtime(format!(
                "runtime `{}` context: {error}",
                self.inner.runtime_id
            ))),
        }
    }
}

/// The instance an initialize reply describes.
fn started(client: RuntimeClient, result: InitializeResult) -> Result<StartedRuntime, String> {
    {
        let workspace = result
            .workspace
            .map(|root| WorkspaceContext::reported(PathBuf::from(root), result.platform));
        let attachment_name = cookie_agent_protocol::AttachmentName::new(
            result.attachment_name.as_str(),
        )
        .map_err(|_| {
            format!(
                "the runtime reported invalid attachment name `{}`",
                result.attachment_name
            )
        })?;
        let skills = skill_registry(result.skills)?;
        let info = RuntimeInfo::new(
            &client.inner.runtime_id,
            result.kind,
            attachment_name.as_str(),
            workspace,
        );
        Ok(StartedRuntime {
            info,
            providers: vec![Arc::new(RemoteToolProvider {
                client: client.clone(),
            })],
            skills,
            connection: Some(client),
        })
    }
}

/// The skills a runtime reported, parsed with the daemon's rules.
fn skill_registry(skills: RuntimeSkills) -> Result<SkillRegistry, String> {
    let documents = skills
        .skills
        .into_iter()
        .map(|skill| {
            cookie_agent_config::parse_skill_document(
                &skill.document,
                skill.source,
                Path::new(&skill.path),
                Path::new(&skill.base_dir),
                skill
                    .supporting_files
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("the runtime reported an invalid skill: {error}"))?;
    let discoveries = skills
        .discoveries
        .into_iter()
        .map(|discovery| SkillDiscovery {
            name: discovery.name,
            source: discovery.source,
            path: PathBuf::from(discovery.path),
            precedence_winner: discovery.precedence_winner,
        })
        .collect();
    SkillRegistry::from_parts(documents, discoveries).map_err(|error| error.to_string())
}

async fn handle_incoming(
    client: Weak<ClientInner>,
    mut incoming: tokio::sync::mpsc::Receiver<Incoming>,
) {
    while let Some(message) = incoming.recv().await {
        let Some(client) = client.upgrade() else {
            return;
        };
        match message {
            Incoming::Request(request) => {
                tokio::spawn(async move { client.handle_request(request).await });
            }
            Incoming::Notification(notification) => client.handle_notification(&notification),
        }
    }
    if let Some(client) = client.upgrade() {
        client.connection_lost();
    }
}

impl ClientInner {
    fn mcp_failure(&self, server: String, reason: String) -> McpServerFailure {
        McpServerFailure {
            runtime_id: self.runtime_id.clone(),
            workspace: lock(&self.mcp).workspace.clone(),
            server,
            reason,
        }
    }

    fn unavailable(&self, error: &RpcError) -> ToolError {
        match error {
            RpcError::Closed(reason) => ToolError::runtime_unavailable(format!(
                "runtime `{}` is unavailable: {reason}",
                self.runtime_id
            )),
            other => ToolError::execution(format!("runtime `{}`: {other}", self.runtime_id)),
        }
    }

    async fn handle_request(self: Arc<Self>, request: IncomingRequest) {
        let result = match request.method.as_str() {
            method::TOOL_PROGRESS => self.call_progress(&request).await,
            other => Err(RpcError::remote(
                error_code::METHOD_NOT_FOUND,
                format!("unknown method `{other}`"),
            )),
        };
        let _ = match result {
            Ok(outcome) => request.respond(&outcome).await,
            Err(error) => request.fail(error).await,
        };
    }

    fn handle_notification(&self, notification: &IncomingNotification) {
        match notification.method.as_str() {
            method::TOOLS_CHANGED => {
                if let Ok(tools) = notification.params::<RuntimeTools>() {
                    *lock(&self.tools) = Some(Arc::new(tools.into()));
                }
            }
            method::MCP_CHANGED => {
                if let Ok(McpChangedParams { servers, failures }) = notification.params() {
                    lock(&self.mcp).servers = Some(servers);
                    if let Some(hub) = &self.mcp_hub {
                        hub.changed();
                        for failure in failures {
                            hub.failed(self.mcp_failure(failure.server, failure.reason));
                        }
                    }
                }
            }
            method::TOOL_DETACHABLE => {
                if let Ok(DetachableParams { prepared_id, note }) = notification.params()
                    && let Some(host) = lock(&self.executions).get(&prepared_id)
                {
                    host.detach.reached(note);
                }
            }
            _ => {}
        }
    }

    async fn call_progress(&self, request: &IncomingRequest) -> Result<Outcome, RpcError> {
        let ToolProgressParams {
            prepared_id,
            progress,
        } = request.params()?;
        let sink = lock(&self.executions)
            .get(&prepared_id)
            .map(|host| host.progress.clone());
        let Some(sink) = sink else {
            return Ok(Outcome::of(Err(ToolError::ProgressSinkClosed)));
        };
        Ok(Outcome::of(sink.send(progress.into()).await))
    }

    /// The connection ended: calls fail through their pending replies.
    fn connection_lost(&self) {
        lock(&self.executions).clear();
    }

    fn remember_presentation(&self, call: ToolCallId, presentation: ToolCallPresentation) {
        let mut presentations = lock(&self.presentations);
        if presentations.len() == MAX_PENDING_PRESENTATIONS {
            presentations.pop_front();
        }
        presentations.push_back((call, presentation));
    }

    fn take_presentation(&self, call: ToolCallId) -> Option<ToolCallPresentation> {
        let mut presentations = lock(&self.presentations);
        let index = presentations.iter().position(|(id, _)| *id == call)?;
        presentations
            .remove(index)
            .map(|(_, presentation)| presentation)
    }

    async fn execute(
        self: &Arc<Self>,
        prepared_id: u64,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let ToolExecutionContext {
            session,
            run,
            progress,
            cancellation,
            turn_context,
            artifacts,
            detach,
        } = context;
        let params = ExecuteParams {
            prepared_id,
            session,
            run,
            turn: turn_context.as_ref().into(),
        };
        lock(&self.executions).insert(prepared_id, ExecutionHost { progress, detach });
        let mut guard = ExecutionGuard {
            client: self,
            prepared_id,
            finished: false,
        };
        let request = self
            .peer
            .request::<_, ExecuteResult>(method::TOOL_EXECUTE, &params);
        tokio::pin!(request);
        let mut cancel_sent = false;
        let reply = loop {
            tokio::select! {
                reply = &mut request => break reply,
                () = cancellation.cancelled(), if !cancel_sent => {
                    cancel_sent = true;
                    let _ = self.peer.notify(method::TOOL_CANCEL, &PreparedRef { prepared_id }).await;
                }
            }
        };
        guard.finished = true;
        drop(guard);
        let reply = reply.map_err(|error| self.unavailable(&error))?;
        let completion = match reply.outcome {
            ExecuteOutcome::Completed {
                output,
                result,
                failed,
            } => ToolCompletion {
                output,
                result,
                failed,
            },
            ExecuteOutcome::Failed { error } => return Err(error.into()),
        };
        retain_media(artifacts.as_ref(), session, reply.media, &completion)?;
        Ok(completion)
    }
}

/// Unregisters an execution; one dropped before its reply also cancels the
/// call in the runtime.
struct ExecutionGuard<'a> {
    client: &'a Arc<ClientInner>,
    prepared_id: u64,
    finished: bool,
}

impl Drop for ExecutionGuard<'_> {
    fn drop(&mut self) {
        lock(&self.client.executions).remove(&self.prepared_id);
        if !self.finished {
            self.client.peer.notify_detached(
                method::TOOL_CANCEL,
                &PreparedRef {
                    prepared_id: self.prepared_id,
                },
            );
        }
    }
}

/// Stores the media a reply carried in the session's artifact store; the
/// result's attachments must all be among it.
fn retain_media(
    artifacts: &dyn ToolArtifacts,
    session: SessionId,
    media: Vec<InlineMedia>,
    completion: &ToolCompletion,
) -> Result<(), ToolError> {
    let mut total = 0_u64;
    let mut retained = Vec::with_capacity(media.len());
    for blob in media {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&blob.data)
            .map_err(|error| {
                ToolError::execution(format!("invalid media from runtime: {error}"))
            })?;
        total += bytes.len() as u64;
        if total > protocol::MAX_INLINE_MEDIA_BYTES {
            return Err(ToolError::resource_limit(format!(
                "a runtime result carries at most {} bytes of media",
                protocol::MAX_INLINE_MEDIA_BYTES
            )));
        }
        if cookie_agent_protocol::Sha256Digest::of_bytes(&bytes).as_str() != blob.sha256 {
            return Err(ToolError::execution(
                "media from the runtime does not match its digest",
            ));
        }
        let (reference, _) = artifacts.retain(session, &bytes)?;
        if reference != protocol::artifact_reference(&blob.sha256) {
            return Err(ToolError::execution(
                "the artifact store gave runtime media an unexpected reference",
            ));
        }
        retained.push(reference);
    }
    if let Some(attachment) = completion
        .result
        .all_attachments()
        .find(|attachment| !retained.contains(&attachment.reference))
    {
        return Err(ToolError::execution(format!(
            "the runtime's result references media it did not send ({})",
            attachment.reference.uri
        )));
    }
    Ok(())
}

/// The tools a protocol runtime declared, as one provider. The sync
/// metadata methods are answered from the declarations and from what the
/// runtime reports when it prepares a call.
pub(crate) struct RemoteToolProvider {
    client: RuntimeClient,
}

impl RemoteToolProvider {
    fn prepared_tool(&self, call: PreparedCall) -> Result<PreparedTool, ToolError> {
        let serialization_key = call
            .serialization_key
            .as_deref()
            .map(|key| {
                protocol::hex_decode(key)
                    .map(PreparedSerializationKey::new)
                    .ok_or_else(|| ToolError::execution("invalid serialization key from runtime"))
            })
            .transpose()?;
        let executor = Box::new(RemoteExecutor {
            client: Arc::clone(&self.client.inner),
            prepared_id: call.prepared_id,
            executed: false,
        });
        Ok(PreparedTool::new(
            call.operation,
            call.normalized_arguments,
            serialization_key,
            executor,
        )?
        .with_reported_policy_labels(call.policy_labels)?
        .with_declaration(PreparedDeclaration {
            permission_name: call
                .permission
                .map(|permission| permission.as_str().to_owned()),
            permission_resource: call.permission_resource,
            presentation: call.presentation,
        }))
    }
}

#[async_trait]
impl ToolProvider for RemoteToolProvider {
    fn provider_id(&self) -> &'static str {
        "runtime.protocol"
    }

    fn prompt_sections(&self, _ctx: &SessionToolContext) -> Result<Vec<PromptSection>, ToolError> {
        Ok(lock(&self.client.inner.tools)
            .as_ref()
            .map(|tools| tools.prompt_sections.clone())
            .unwrap_or_default())
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(lock(&self.client.inner.tools)
            .as_ref()
            .map(|tools| tools.specs.clone())
            .unwrap_or_default())
    }

    fn get_permission_name(_tool_name: &str) -> Result<Option<&'static str>, ToolError>
    where
        Self: Sized,
    {
        Err(ToolError::execution(
            "a runtime tool's permission is declared by its runtime",
        ))
    }

    fn get_permission_resource(
        &self,
        _tool_name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        Err(ToolError::execution(
            "a runtime tool's permission resource is reported when it is prepared",
        ))
    }

    fn get_display_argument(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Err(ToolError::execution(
            "a runtime tool's display argument is reported when it is prepared",
        ))
    }

    fn presentation(&self, call: &ToolCall) -> ToolCallPresentation {
        self.client
            .inner
            .take_presentation(call.id)
            .unwrap_or_else(|| cookie_agent_runtime_api::tool_title_only(&call.name))
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        self.prepare_parallel(ctx, vec![call])
            .await
            .pop()
            .expect("one result per call")
    }

    async fn prepare_parallel(
        &self,
        ctx: ToolPreparationContext,
        calls: Vec<ToolCall>,
    ) -> Vec<Result<PreparedTool, ToolError>> {
        let client = &self.client.inner;
        let mut prepared = Vec::with_capacity(calls.len());
        let turn: protocol::WireTurnContext = ctx.turn_context.as_ref().into();
        for batch in calls.chunks(protocol::MAX_PREPARE_BATCH) {
            let reply: Result<PrepareResult, RpcError> = client
                .peer
                .request(
                    method::TOOL_PREPARE,
                    &PrepareParams {
                        session: ctx.session,
                        run: ctx.run,
                        turn: turn.clone(),
                        calls: batch
                            .iter()
                            .map(|call| WireToolCall {
                                id: call.id,
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            })
                            .collect(),
                    },
                )
                .await;
            let outcomes = match reply {
                Ok(reply) if reply.results.len() == batch.len() => reply.results,
                Ok(reply) => {
                    // Release whatever the runtime prepared before failing.
                    for outcome in reply.results {
                        if let PrepareOutcome::Prepared(call) = outcome {
                            client.peer.notify_detached(
                                method::TOOL_RELEASE,
                                &PreparedRef {
                                    prepared_id: call.prepared_id,
                                },
                            );
                        }
                    }
                    prepared.extend(batch.iter().map(|_| {
                        Err(ToolError::execution(
                            "the runtime answered a different number of calls",
                        ))
                    }));
                    continue;
                }
                Err(error) => {
                    prepared.extend(batch.iter().map(|_| Err(client.unavailable(&error))));
                    continue;
                }
            };
            for (call, outcome) in batch.iter().zip(outcomes) {
                prepared.push(match outcome {
                    PrepareOutcome::Prepared(declared) => self.prepared_tool(declared),
                    PrepareOutcome::Failed {
                        error,
                        presentation,
                    } => {
                        client.remember_presentation(call.id, presentation);
                        Err(error.into())
                    }
                });
            }
        }
        prepared
    }
}

/// Executes, revalidates, or releases one call the runtime prepared.
struct RemoteExecutor {
    client: Arc<ClientInner>,
    prepared_id: u64,
    executed: bool,
}

impl Drop for RemoteExecutor {
    fn drop(&mut self) {
        if !self.executed {
            self.client.peer.notify_detached(
                method::TOOL_RELEASE,
                &PreparedRef {
                    prepared_id: self.prepared_id,
                },
            );
        }
    }
}

#[async_trait]
impl PreparedExecutor for RemoteExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        let outcome: Outcome = self
            .client
            .peer
            .request(
                method::TOOL_REVALIDATE,
                &PreparedRef {
                    prepared_id: self.prepared_id,
                },
            )
            .await
            .map_err(|error| self.client.unavailable(&error))?;
        outcome.into_result()
    }

    async fn execute(
        mut self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        self.executed = true;
        let client = Arc::clone(&self.client);
        client.execute(self.prepared_id, context).await
    }
}

#[cfg(test)]
mod tests;
