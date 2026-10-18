//! The daemon's end of the runtime protocol: runtimes that run in another
//! task or process and are reached over a frame transport.
//!
//! A [`ProtocolTemplate`] connects its runtime with its
//! [`RuntimeConnector`] and sends `runtime/initialize`, once: the
//! [`RuntimeClient`] it gets back keeps the connection, and every capability
//! instance opens over it (`capability/open`). Each instance's
//! [`RemoteToolProvider`] publishes the tools the runtime declared for it and
//! proxies preparation and execution, it routes `tool/progress`
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
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use base64::Engine as _;
use cookie_agent_config::{McpServerConfig, SkillDiscovery, SkillRegistry};
use cookie_agent_protocol::{
    CapabilityName, ProducerDiscardParams, ProducerRegisterParams, ProducerSendParams,
    ProducerSourceStatus, ProducerUnregisterParams, RecoveryCompleteParams, RecoveryOutcome,
    RuntimeCapabilityInfo, SessionId, ToolCallId, ToolCallPresentation, errors,
};
use cookie_agent_runtime_api::{
    DetachPoint, DiagnosticTail, PreparedDeclaration, PreparedExecutor, PreparedSerializationKey,
    PreparedTool, ProgressSink, PromptSection, RuntimeConnection, RuntimeConnector, RuntimeInfo,
    SessionToolContext, ToolArtifacts, ToolCall, ToolCompletion, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec, WorkspaceContext,
    context::AgentMdContext,
    protocol::{
        self, CapabilityOpenParams, CapabilityOpenResult, CapabilityToolsChangedParams,
        ContextParams, ContextResult, DetachableParams, Empty, ExecuteOutcome, ExecuteParams,
        ExecuteResult, InitializeParams, InitializeResult, InlineMedia, McpChangedParams,
        McpReconnectParams, McpServerReport, Outcome, OutcomeExt as _, PrepareOutcome,
        PrepareParams, PrepareResult, PreparedCall, PreparedRef, RuntimeSettings, RuntimeSkills,
        RuntimeTools, ToolDeclarationExt as _, ToolProgressParams, WireToolCall, method,
    },
    rpc::{Incoming, IncomingNotification, IncomingRequest, Peer, RpcError},
};
use tokio::sync::Notify;

use super::{
    McpHub, McpServerFailure,
    process::ProcessConnector,
    registry::{RuntimeListingState, RuntimeRegistry, RuntimeTemplate, StartedRuntime},
};
use crate::EngineError;

/// Failed preparations whose presentation the engine has not asked for yet.
const MAX_PENDING_PRESENTATIONS: usize = 256;

/// A runtime that speaks the runtime protocol: one connection, which every
/// capability instance of the runtime shares.
pub(crate) struct ProtocolTemplate {
    runtime_id: String,
    /// The workspace a `coding` instance gets when a session names none,
    /// such as the daemon's working directory for `local`.
    default_workspace: Option<PathBuf>,
    connector: Arc<dyn RuntimeConnector>,
    settings: RuntimeSettings,
    /// Bound on connecting and initializing, and on opening an instance;
    /// `None` for an in-process runtime.
    startup_timeout: Option<Duration>,
    /// How long a stopping runtime gets to answer `runtime/shutdown`.
    shutdown_grace: Duration,
    /// Where the MCP servers instances host report to.
    mcp_hub: Option<Arc<McpHub>>,
    lifetime: cookie_agent_protocol::RuntimeLifetime,
    /// Serializes connecting, so concurrent instances share one connection.
    connecting: tokio::sync::Mutex<()>,
    /// The current connection and what it reported.
    connection: Mutex<Option<RuntimeClient>>,
    /// What the runtime offered when it last connected.
    offered: Mutex<RuntimeListingState>,
}

/// How long the daemon waits for the in-process `local` runtime to answer
/// `runtime/shutdown`.
pub(crate) const LOCAL_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

impl ProtocolTemplate {
    /// A configured template: its runtime runs `connector` and must answer
    /// `runtime/initialize`, and each `capability/open`, within
    /// `startup_timeout`.
    pub(crate) fn new(
        runtime_id: &str,
        connector: Arc<dyn RuntimeConnector>,
        settings: RuntimeSettings,
        startup_timeout: Duration,
        shutdown_grace: Duration,
    ) -> Self {
        Self {
            runtime_id: runtime_id.to_owned(),
            default_workspace: None,
            connector,
            settings,
            startup_timeout: Some(startup_timeout),
            shutdown_grace,
            mcp_hub: None,
            lifetime: cookie_agent_protocol::RuntimeLifetime::Spawned,
            connecting: tokio::sync::Mutex::new(()),
            connection: Mutex::new(None),
            offered: Mutex::default(),
        }
    }

    /// Reports the MCP servers its instances host to `hub`.
    pub(crate) fn with_mcp_hub(mut self, hub: Arc<McpHub>) -> Self {
        self.mcp_hub = Some(hub);
        self
    }

    /// The `local` coding runtime, whose instances open in `workspace` when
    /// a session names none.
    pub(crate) fn local(
        connector: Arc<dyn RuntimeConnector>,
        workspace: &Path,
        settings: RuntimeSettings,
    ) -> Self {
        Self {
            default_workspace: Some(workspace.to_owned()),
            startup_timeout: None,
            shutdown_grace: LOCAL_SHUTDOWN_GRACE,
            lifetime: cookie_agent_protocol::RuntimeLifetime::Builtin,
            ..Self::new(
                cookie_agent_runtime_api::LOCAL_RUNTIME_ID,
                connector,
                settings,
                Duration::ZERO,
                LOCAL_SHUTDOWN_GRACE,
            )
        }
    }

    /// The runtime's live connection, connecting and initializing it when
    /// there is none or it ended.
    async fn connected(&self) -> Result<RuntimeClient, String> {
        let current = || {
            lock(&self.connection)
                .clone()
                .filter(|client| !client.is_closed())
        };
        if let Some(client) = current() {
            return Ok(client);
        }
        let _connecting = self.connecting.lock().await;
        if let Some(client) = current() {
            return Ok(client);
        }
        let diagnostics = Mutex::new(None);
        let attempt = async {
            let connection = self.connector.connect().await?;
            lock(&diagnostics).clone_from(&connection.diagnostics);
            RuntimeClient::initialize(
                connection,
                &self.runtime_id,
                &self.settings,
                self.shutdown_grace,
                self.mcp_hub.clone(),
            )
            .await
        };
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
        let (client, result) = result
            .map_err(|reason| match lock(&diagnostics).as_ref() {
                Some(tail) => tail.annotate(&reason),
                None => reason,
            })
            .inspect_err(|reason| lock(&self.offered).message = Some(reason.clone()))?;
        *lock(&self.offered) = RuntimeListingState {
            connected: true,
            capabilities: client.capabilities(),
            platform: result.platform,
            message: None,
        };
        *lock(&self.connection) = Some(client.clone());
        Ok(client)
    }
}

#[async_trait]
impl RuntimeTemplate for ProtocolTemplate {
    fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    fn lifetime(&self) -> cookie_agent_protocol::RuntimeLifetime {
        self.lifetime
    }

    async fn capabilities(&self) -> Result<Vec<RuntimeCapabilityInfo>, String> {
        Ok(self.connected().await?.capabilities())
    }

    fn listing(&self) -> RuntimeListingState {
        let mut listing = lock(&self.offered).clone();
        listing.connected = lock(&self.connection)
            .as_ref()
            .is_some_and(|client| !client.is_closed());
        if listing.connected {
            listing.message = None;
        }
        listing
    }

    fn default_workspace(&self, capability: &CapabilityName) -> Option<PathBuf> {
        (capability.as_str() == CapabilityName::CODING)
            .then(|| self.default_workspace.clone())
            .flatten()
    }

    async fn start(
        &self,
        capability: &CapabilityName,
        workspace: Option<&Path>,
    ) -> Result<StartedRuntime, String> {
        let client = self.connected().await?;
        let open = client.open(capability, workspace);
        let result = match self.startup_timeout {
            Some(timeout) => tokio::time::timeout(timeout, open)
                .await
                .unwrap_or_else(|_| {
                    Err(format!(
                        "the runtime did not open `{capability}` within {} ms",
                        timeout.as_millis()
                    ))
                }),
            None => open.await,
        };
        result.map_err(|reason| client.inner.annotate(&reason))
    }

    fn disconnect(&self) {
        lock(&self.connection).take();
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

/// What a plugin connection handles beyond the runtime protocol.
pub(crate) trait ClientExtension: Send + Sync {
    /// A notification the runtime protocol does not define, such as
    /// `plugin/emit`.
    fn notification(&self, client: RuntimeClient, notification: IncomingNotification);

    /// Something the daemon could not do with a message the plugin sent.
    fn diagnostic(&self, message: String);
}

/// Distinguishes every runtime connection of this process from every other,
/// so a producer registration made over one never passes for another's.
static NEXT_CONNECTION_EPOCH: AtomicU64 = AtomicU64::new(1);

/// One producer request a runtime sent: which connection, which method, and
/// its params as sent.
pub(crate) struct RuntimeProducerCall {
    /// The connection's producer source: `runtime:<runtime ID>` or
    /// `plugin:<name>`.
    pub(crate) source: cookie_agent_protocol::ProducerSource,
    pub(crate) epoch: u64,
    pub(crate) method: String,
    pub(crate) params: serde_json::Value,
}

/// The answer to a [`RuntimeProducerCall`]: its result, or why it failed.
pub(crate) type RuntimeProducerAnswer =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send>>;

/// Where a runtime connection's producer requests go: the engine, once it
/// opened producer messaging on the connection.
pub(crate) struct RuntimeProducerHost {
    /// Answers one producer operation.
    pub(crate) call: Box<dyn Fn(RuntimeProducerCall) -> RuntimeProducerAnswer + Send + Sync>,
    /// A connection's recovery state changed.
    pub(crate) changed: Box<dyn Fn() + Send + Sync>,
}

/// A connection's producer messaging.
#[derive(Default)]
struct ProducerState {
    /// The runtime declared `producer_messaging`.
    declared: bool,
    /// Set when the engine opens producer messaging on the connection.
    host: Option<Arc<RuntimeProducerHost>>,
    status: Option<ProducerSourceStatus>,
    outcome: Option<RecoveryOutcome>,
}

/// A plugin connection after its handshake.
pub(crate) struct PluginHandshake {
    pub(crate) client: RuntimeClient,
    pub(crate) result: cookie_agent_protocol::ExtensionInitializeResult,
    /// What the daemon ignored in the plugin's reply, one entry each.
    pub(crate) notes: Vec<String>,
}

/// Drops the entries of a plugin's initialize reply's `capabilities.intercept`
/// this build cannot use, before the reply is decoded: one it cannot read,
/// one naming a hook it does not dispatch, and one repeating a hook. Returns
/// a note for each (`docs/site/specs/compatibility-v1.md` section 9.1).
fn take_unusable_intercepts(reply: &mut serde_json::Value) -> Vec<String> {
    let Some(entries) = reply
        .pointer_mut("/capabilities/intercept")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    entries.retain(|entry| {
        let intercept = match serde_json::from_value::<cookie_agent_protocol::ExtensionIntercept>(
            entry.clone(),
        ) {
            Ok(intercept) => intercept,
            Err(error) => {
                notes.push(format!(
                    "the plugin declares an intercept entry this daemon cannot read ({error}); \
                         it is skipped"
                ));
                return false;
            }
        };
        let hook = intercept.hook.as_str();
        if cookie_agent_protocol::ExtensionInterceptionHook::from_name(hook).is_none() {
            notes.push(format!(
                "the plugin intercepts hook `{hook}`, which this daemon does not know; \
                 it is skipped"
            ));
            return false;
        }
        if !seen.insert(hook.to_owned()) {
            notes.push(format!(
                "the plugin intercepts hook `{hook}` more than once; only its first entry is used"
            ));
            return false;
        }
        true
    });
    notes
}

/// Drops the patterns of a plugin's initialize reply's
/// `capabilities.subscribe_events` this build cannot read, before the reply
/// is decoded, and returns a note for each.
fn take_unreadable_event_patterns(reply: &mut serde_json::Value) -> Vec<String> {
    let Some(patterns) = reply
        .pointer_mut("/capabilities/subscribe_events")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    patterns.retain(|pattern| {
        match serde_json::from_value::<cookie_agent_protocol::EventPattern>(pattern.clone()) {
            Ok(_) => true,
            Err(error) => {
                notes.push(format!(
                    "the plugin subscribes to events by {pattern}, which this daemon cannot read \
                     ({error}); it is skipped"
                ));
                false
            }
        }
    });
    notes
}

/// Reads a handshake reply: the peer's version first, from the raw JSON
/// (`docs/site/specs/compatibility-v1.md` section 3.2), then the rest.
fn read_handshake<T: serde::de::DeserializeOwned>(
    supported: cookie_agent_protocol::SemVer,
    reply: serde_json::Value,
) -> Result<T, String> {
    cookie_agent_protocol::negotiate_version(supported, &reply)
        .map_err(|refusal| refusal.to_string())?;
    serde_json::from_value(reply).map_err(|error| format!("invalid reply: {error}"))
}

/// One connection to a runtime. Dropping the last handle closes it.
#[derive(Clone)]
pub(crate) struct RuntimeClient {
    inner: Arc<ClientInner>,
}

/// One capability instance of a runtime: its connection, shared with the
/// runtime's other instances, and its handle on that connection.
#[derive(Clone)]
pub(crate) struct InstanceConnection {
    pub(crate) client: RuntimeClient,
    /// `None` for a plugin, whose connection has one implicit instance.
    pub(crate) instance: Option<String>,
}

impl InstanceConnection {
    /// The instance's `AGENTS.md` context for a run of `preset`.
    pub(crate) async fn agent_md(
        &self,
        preset: Option<&str>,
    ) -> Result<Option<AgentMdContext>, EngineError> {
        match &self.instance {
            Some(instance) => self.client.agent_md(instance, preset).await,
            None => Ok(None),
        }
    }

    /// The MCP servers the instance hosts, with their last reported status.
    pub(crate) fn mcp_servers(&self) -> Vec<HostedMcpServer> {
        self.instance
            .as_deref()
            .map(|instance| self.client.mcp_servers(instance))
            .unwrap_or_default()
    }

    /// Asks the runtime to reconnect the instance's MCP server `server` now
    /// and waits for the attempt.
    pub(crate) async fn reconnect_mcp(&self, server: &str) -> Result<(), EngineError> {
        let instance = self
            .instance
            .as_deref()
            .ok_or_else(|| EngineError::Mcp(format!("unknown MCP server `{server}`")))?;
        self.client.reconnect_mcp(instance, server).await
    }

    /// Asks the runtime to retry the instance's failed MCP servers.
    pub(crate) fn retry_failed_mcp(&self) {
        if let Some(instance) = &self.instance {
            self.client.retry_failed_mcp(instance);
        }
    }

    /// Waits for the MCP retries of the connection.
    pub(crate) async fn mcp_settled(&self) {
        self.client.mcp_settled().await;
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.client.epoch()
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    pub(crate) fn produces(&self) -> bool {
        self.client.produces()
    }

    pub(crate) fn producer_status(&self) -> Option<ProducerSourceStatus> {
        self.client.producer_status()
    }

    pub(crate) fn start_producers(&self, host: &Arc<RuntimeProducerHost>) -> bool {
        self.client.start_producers(host)
    }
}

struct ClientInner {
    peer: Peer,
    runtime_id: String,
    /// This connection's producer epoch; see [`NEXT_CONNECTION_EPOCH`].
    epoch: u64,
    /// The producer source of the connection, once initialized:
    /// `runtime:<runtime ID>`, or `plugin:<name>` for a plugin.
    producer_source: OnceLock<cookie_agent_protocol::ProducerSource>,
    /// Handles what the runtime protocol does not define, for a plugin.
    extension: OnceLock<Arc<dyn ClientExtension>>,
    producers: Mutex<ProducerState>,
    /// What the runtime writes outside the protocol, such as its stderr.
    diagnostics: Option<DiagnosticTail>,
    /// How long [`RuntimeClient::shutdown`] waits for the runtime.
    shutdown_grace: Duration,
    /// What the runtime offers, from its initialize reply.
    capabilities: OnceLock<Vec<RuntimeCapabilityInfo>>,
    /// The runtime's operating system, from its initialize reply.
    platform: Mutex<Option<String>>,
    /// Each instance's declared tools, by instance handle (`None` for a
    /// plugin's implicit instance): from its open reply or its latest
    /// `tools_changed`.
    tools: Mutex<HashMap<Option<String>, Arc<DeclaredTools>>>,
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

/// What the daemon knows about the MCP servers a runtime's instances host.
#[derive(Default)]
struct HostedMcp {
    /// The template's definitions, by server name.
    definitions: BTreeMap<String, McpServerConfig>,
    /// Each instance's servers, by instance handle.
    instances: BTreeMap<String, InstanceMcp>,
}

/// The MCP servers of one instance.
#[derive(Default)]
struct InstanceMcp {
    /// The instance's workspace.
    workspace: Option<String>,
    /// Their last reported status; `None` before the open reply or the
    /// first report.
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
            epoch: NEXT_CONNECTION_EPOCH.fetch_add(1, Ordering::Relaxed),
            producer_source: OnceLock::new(),
            extension: OnceLock::new(),
            producers: Mutex::default(),
            diagnostics: connection.diagnostics,
            shutdown_grace,
            capabilities: OnceLock::new(),
            platform: Mutex::default(),
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
        let reply: serde_json::Value = inner
            .peer
            .request(
                method::INITIALIZE,
                &InitializeParams {
                    protocol_version: cookie_agent_protocol::RUNTIME_PROTOCOL_VERSION,
                    engine_version: env!("CARGO_PKG_VERSION").to_owned(),
                    features: Vec::new(),
                    runtime_id: runtime_id.to_owned(),
                    settings: settings.clone(),
                },
            )
            .await
            .map_err(|error| format!("runtime `{runtime_id}`: {error}"))?;
        let result: InitializeResult =
            read_handshake(cookie_agent_protocol::RUNTIME_PROTOCOL_VERSION, reply)
                .map_err(|reason| format!("runtime `{runtime_id}`: {reason}"))?;
        // No runtime feature is defined yet.
        for feature in &result.features {
            eprintln!(
                "cookie-agent: runtime `{runtime_id}` declares feature `{feature}`, which this \
                 daemon does not know; ignoring it"
            );
        }
        if let Ok(source) = cookie_agent_protocol::ProducerSource::runtime(runtime_id) {
            let _ = inner.producer_source.set(source);
        }
        lock(&inner.producers).declared = result.producer_messaging;
        let mut capabilities = Vec::with_capacity(result.capabilities.len());
        for declared in &result.capabilities {
            if capabilities
                .iter()
                .any(|known: &RuntimeCapabilityInfo| known.name == declared.name)
            {
                return Err(format!(
                    "the runtime offers capability `{}` twice",
                    declared.name
                ));
            }
            capabilities.push(RuntimeCapabilityInfo {
                name: declared.name.clone(),
                kind: declared.kind.clone(),
                takes_workspace: declared.takes_workspace,
                description: declared.description.clone(),
            });
        }
        let _ = inner.capabilities.set(capabilities);
        lock(&inner.platform).clone_from(&result.platform);
        Ok((Self { inner }, result))
    }

    /// Connects a plugin over `connection` and runs its `plugin/initialize`
    /// handshake. The plugin's tools then run over the runtime protocol's
    /// `tool/*` messages and its producers as `plugin:<name>`; what only
    /// plugins send goes to `extension`. `producer_messaging` is whether its
    /// configuration lets it produce. Also returns a diagnostic for each
    /// entry of the reply this daemon does not know and ignored.
    pub(crate) async fn initialize_plugin(
        connection: RuntimeConnection,
        name: &str,
        producer_messaging: bool,
        interception_timeout: Duration,
        shutdown_grace: Duration,
        extension: Arc<dyn ClientExtension>,
    ) -> Result<PluginHandshake, String> {
        let (peer, incoming) = Peer::start(connection.transport);
        let source = cookie_agent_protocol::ProducerSource::plugin(name)
            .map_err(|error| format!("invalid plugin name: {error}"))?;
        let inner = Arc::new(ClientInner {
            peer,
            runtime_id: name.to_owned(),
            epoch: NEXT_CONNECTION_EPOCH.fetch_add(1, Ordering::Relaxed),
            producer_source: OnceLock::from(source),
            extension: OnceLock::from(extension),
            producers: Mutex::default(),
            diagnostics: connection.diagnostics,
            shutdown_grace,
            capabilities: OnceLock::from(Vec::new()),
            platform: Mutex::default(),
            tools: Mutex::default(),
            executions: Mutex::default(),
            presentations: Mutex::default(),
            mcp: Mutex::default(),
            mcp_hub: None,
            mcp_retries: AtomicUsize::new(0),
            mcp_settled: Notify::new(),
        });
        tokio::spawn(handle_incoming(Arc::downgrade(&inner), incoming));
        let client = Self { inner };
        let failed = |reason: &dyn std::fmt::Display| {
            client
                .inner
                .annotate(&format!("plugin `{name}`: handshake failed: {reason}"))
        };
        let reply: serde_json::Value = client
            .inner
            .peer
            .request(
                cookie_agent_protocol::PLUGIN_INITIALIZE_METHOD,
                &cookie_agent_protocol::ExtensionInitializeParams {
                    protocol_version: cookie_agent_protocol::PLUGIN_PROTOCOL_VERSION,
                    engine_version: env!("CARGO_PKG_VERSION").to_owned(),
                    features: producer_messaging
                        .then(|| cookie_agent_protocol::FEATURE_PRODUCER_MESSAGING.to_owned())
                        .into_iter()
                        .collect(),
                    interception_timeout_ms: u64::try_from(interception_timeout.as_millis())
                        .unwrap_or(u64::MAX),
                },
            )
            .await
            .map_err(|error| failed(&error))?;
        let mut reply = reply;
        let mut intercept_notes = take_unusable_intercepts(&mut reply);
        intercept_notes.extend(take_unreadable_event_patterns(&mut reply));
        let result: cookie_agent_protocol::ExtensionInitializeResult =
            read_handshake(cookie_agent_protocol::PLUGIN_PROTOCOL_VERSION, reply)
                .map_err(|reason| failed(&reason))?;
        // No plugin feature is defined yet.
        let notes = result
            .features
            .iter()
            .map(|feature| {
                format!(
                    "the plugin declares feature `{feature}`, which this daemon does not know; \
                     it is ignored"
                )
            })
            .chain(intercept_notes)
            .collect();
        if result.name != name {
            return Err(format!(
                "plugin name mismatch: configured `{name}`, reported `{}`",
                result.name
            ));
        }
        lock(&client.inner.tools)
            .entry(None)
            .or_insert_with(|| Arc::new(result.tools.clone().into()));
        lock(&client.inner.producers).declared =
            producer_messaging && result.capabilities.producer_messaging;
        Ok(PluginHandshake {
            client,
            result,
            notes,
        })
    }

    /// Sends request `method` with `params` and returns its raw result.
    pub(crate) async fn request_value<P: serde::Serialize + Sync>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<serde_json::Value, RpcError> {
        self.inner.peer.request(method, params).await
    }

    /// Sends notification `method` with `params` as they are.
    pub(crate) async fn notify_raw(
        &self,
        method: &str,
        params: &serde_json::value::RawValue,
    ) -> Result<(), RpcError> {
        self.inner.peer.notify(method, params).await
    }

    /// The tools a plugin declares, as one provider.
    pub(crate) fn tool_provider(&self) -> Arc<dyn ToolProvider> {
        Arc::new(RemoteToolProvider {
            client: self.clone(),
            instance: None,
        })
    }

    /// What the runtime offers.
    pub(crate) fn capabilities(&self) -> Vec<RuntimeCapabilityInfo> {
        self.inner.capabilities.get().cloned().unwrap_or_default()
    }

    /// Opens `capability` for `workspace` and returns the instance. The
    /// error is a human-readable reason.
    pub(crate) async fn open(
        &self,
        capability: &CapabilityName,
        workspace: Option<&Path>,
    ) -> Result<StartedRuntime, String> {
        let offered = self
            .capabilities()
            .into_iter()
            .find(|offered| &offered.name == capability)
            .ok_or_else(|| format!("the runtime offers no capability `{capability}`"))?;
        let result: CapabilityOpenResult = self
            .inner
            .peer
            .request(
                method::CAPABILITY_OPEN,
                &CapabilityOpenParams {
                    capability: capability.clone(),
                    workspace: workspace.map(|path| path.to_string_lossy().into_owned()),
                },
            )
            .await
            .map_err(|error| format!("{}: {error}", self.inner.peer_name()))?;
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
        let platform = self.platform();
        let workspace = result
            .workspace
            .clone()
            .map(|root| WorkspaceContext::reported(PathBuf::from(root), platform));
        // A notification the runtime sent after its reply may already have
        // been handled; it is newer than the reply.
        lock(&self.inner.tools)
            .entry(Some(result.instance.clone()))
            .or_insert_with(|| Arc::new(result.tools.into()));
        let failed = {
            let mut mcp = lock(&self.inner.mcp);
            let instance = mcp.instances.entry(result.instance.clone()).or_default();
            instance.workspace.clone_from(&result.workspace);
            instance
                .servers
                .get_or_insert_with(|| result.mcp_servers.clone())
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
        if let Some(hub) = &self.inner.mcp_hub {
            for (server, reason) in failed {
                hub.failed(self.inner.mcp_failure(&result.instance, server, reason));
            }
            if !result.mcp_servers.is_empty() {
                hub.changed();
            }
        }
        let info = RuntimeInfo::new(
            &self.inner.runtime_id,
            capability.clone(),
            offered.kind,
            attachment_name.as_str(),
            workspace,
        );
        Ok(StartedRuntime {
            info,
            providers: vec![Arc::new(RemoteToolProvider {
                client: self.clone(),
                instance: Some(result.instance.clone()),
            })],
            skills,
            connection: Some(InstanceConnection {
                client: self.clone(),
                instance: Some(result.instance),
            }),
        })
    }

    /// The operating system the runtime reported.
    fn platform(&self) -> Option<String> {
        lock(&self.inner.platform).clone()
    }

    /// What the process last wrote outside the protocol.
    pub(crate) fn diagnostics(&self) -> Option<String> {
        self.inner
            .diagnostics
            .as_ref()
            .and_then(DiagnosticTail::text)
    }

    /// Tells a plugin to stop (`plugin/shutdown`) and closes the connection
    /// once it has gone or its shutdown grace has passed; a process that has
    /// not exited after another grace is killed.
    pub(crate) async fn shutdown_plugin(&self) {
        let _ = self
            .inner
            .peer
            .notify(cookie_agent_protocol::PLUGIN_SHUTDOWN_METHOD, &Empty {})
            .await;
        let _ = tokio::time::timeout(self.inner.shutdown_grace, self.inner.peer.closed()).await;
        self.inner.peer.close();
    }

    /// This connection's producer epoch.
    pub(crate) fn epoch(&self) -> u64 {
        self.inner.epoch
    }

    /// The runtime declared producer messaging.
    pub(crate) fn produces(&self) -> bool {
        lock(&self.inner.producers).declared
    }

    /// The runtime's producer recovery state; `None` when it does not
    /// produce. A closed connection is `starting`: its work is unknown
    /// until the instance reconnects and recovers.
    pub(crate) fn producer_status(&self) -> Option<ProducerSourceStatus> {
        let state = lock(&self.inner.producers);
        if !state.declared {
            return None;
        }
        if self.inner.peer.is_closed() {
            return Some(ProducerSourceStatus::Starting);
        }
        Some(state.status.unwrap_or(ProducerSourceStatus::Starting))
    }

    /// Opens producer messaging on a connection whose runtime declared it:
    /// its requests go to `host` from now on, and the runtime is told to
    /// recover. Returns whether it opened now.
    pub(crate) fn start_producers(&self, host: &Arc<RuntimeProducerHost>) -> bool {
        {
            let mut state = lock(&self.inner.producers);
            if !state.declared || state.host.is_some() || self.inner.peer.is_closed() {
                return false;
            }
            state.host = Some(Arc::clone(host));
            state.status = Some(ProducerSourceStatus::Starting);
        }
        self.inner
            .peer
            .notify_detached(method::PRODUCER_RECOVERY_START, &Empty {});
        true
    }

    /// The MCP servers instance `instance` hosts, with their last reported
    /// status.
    fn mcp_servers(&self, instance: &str) -> Vec<HostedMcpServer> {
        let mcp = lock(&self.inner.mcp);
        let Some(hosted) = mcp.instances.get(instance) else {
            return Vec::new();
        };
        hosted
            .servers
            .iter()
            .flatten()
            .filter_map(|report| {
                Some(HostedMcpServer {
                    runtime_id: self.inner.runtime_id.clone(),
                    workspace: hosted.workspace.clone(),
                    definition: mcp.definitions.get(&report.name)?.clone(),
                    report: report.clone(),
                })
            })
            .collect()
    }

    /// Asks the runtime to reconnect MCP server `server` of instance
    /// `instance` now and waits for the attempt.
    async fn reconnect_mcp(&self, instance: &str, server: &str) -> Result<(), EngineError> {
        let outcome: Outcome = self
            .inner
            .peer
            .request(
                method::MCP_RECONNECT,
                &McpReconnectParams {
                    instance: instance.to_owned(),
                    server: Some(server.to_owned()),
                },
            )
            .await
            .map_err(|error| EngineError::Mcp(format!("{}: {error}", self.inner.peer_name())))?;
        outcome
            .into_result()
            .map_err(|error| EngineError::Mcp(error.to_string()))
    }

    /// Asks the runtime to retry the failed MCP servers of instance
    /// `instance`, as when a session that uses it is created or opened;
    /// [`Self::mcp_settled`] waits for the attempts.
    fn retry_failed_mcp(&self, instance: &str) {
        let any_failed = lock(&self.inner.mcp)
            .instances
            .get(instance)
            .and_then(|hosted| hosted.servers.as_ref())
            .into_iter()
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
        let params = McpReconnectParams {
            instance: instance.to_owned(),
            server: None,
        };
        handle.spawn(async move {
            let _ = retry
                .0
                .peer
                .request::<_, Outcome>(method::MCP_RECONNECT, &params)
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

    /// Instance `instance`'s `AGENTS.md` context for a run of `preset`.
    async fn agent_md(
        &self,
        instance: &str,
        preset: Option<&str>,
    ) -> Result<Option<AgentMdContext>, EngineError> {
        let reply: Result<ContextResult, RpcError> = self
            .inner
            .peer
            .request(
                method::CONTEXT,
                &ContextParams {
                    instance: instance.to_owned(),
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
            Err(error) if error.data_code() == Some(errors::CONTEXT_FAILED.code) => {
                let reason = format!("{}: {error}", self.inner.peer_name());
                match error.data_member("path") {
                    Some(path) => Err(EngineError::AgentMdIo {
                        path: PathBuf::from(path),
                        source: std::io::Error::other(reason),
                    }),
                    None => Err(EngineError::Runtime(reason)),
                }
            }
            Err(error) => Err(EngineError::Runtime(format!(
                "{} context: {error}",
                self.inner.peer_name()
            ))),
        }
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
            Incoming::Notification(notification) => {
                if !client.handle_notification(&notification)
                    && let Some(extension) = client.extension.get().cloned()
                {
                    extension.notification(
                        RuntimeClient {
                            inner: Arc::clone(&client),
                        },
                        notification,
                    );
                }
            }
        }
    }
    if let Some(client) = client.upgrade() {
        client.connection_lost();
    }
}

impl ClientInner {
    /// Reports something the daemon could not do with a message the peer
    /// sent: a plugin's reason shows it; a runtime's goes to the daemon's
    /// log.
    fn diagnostic(&self, message: String) {
        match self.extension.get() {
            Some(extension) => extension.diagnostic(message),
            None => eprintln!("cookie-agent: {}: {message}", self.peer_name()),
        }
    }

    /// How errors name the peer: `runtime `<id>`` or `plugin `<name>``.
    fn peer_name(&self) -> String {
        if self.extension.get().is_some() {
            format!("plugin `{}`", self.runtime_id)
        } else {
            format!("runtime `{}`", self.runtime_id)
        }
    }

    /// `reason` followed by what the process last wrote outside the protocol.
    fn annotate(&self, reason: &str) -> String {
        match &self.diagnostics {
            Some(tail) => tail.annotate(reason),
            None => reason.to_owned(),
        }
    }

    fn mcp_failure(&self, instance: &str, server: String, reason: String) -> McpServerFailure {
        McpServerFailure {
            runtime_id: self.runtime_id.clone(),
            workspace: lock(&self.mcp)
                .instances
                .get(instance)
                .and_then(|hosted| hosted.workspace.clone()),
            server,
            reason,
        }
    }

    fn unavailable(&self, error: &RpcError) -> ToolError {
        match error {
            RpcError::Closed(reason) => ToolError::runtime_unavailable(format!(
                "{} is unavailable: {reason}",
                self.peer_name()
            )),
            other => ToolError::execution(format!("{}: {other}", self.peer_name())),
        }
    }

    async fn handle_request(self: Arc<Self>, request: IncomingRequest) {
        let result = match request.method.as_str() {
            method::TOOL_PROGRESS => self.call_progress(&request).await.and_then(|outcome| {
                serde_json::to_value(outcome)
                    .map_err(|error| errors::INTERNAL.error().detail(error.to_string()).into())
            }),
            method::PRODUCER_REGISTER
            | method::PRODUCER_SEND
            | method::PRODUCER_UNREGISTER
            | method::PRODUCER_DISCARD => self.call_producer(&request).await,
            method::PRODUCER_RECOVERY_COMPLETE => self
                .complete_producer_recovery(&request)
                .map(|()| serde_json::json!({})),
            other => Err(errors::UNKNOWN_METHOD.error().field("method", other).into()),
        };
        let _ = match result {
            Ok(outcome) => request.respond(&outcome).await,
            Err(error) => request.fail(error).await,
        };
    }

    /// Handles a notification the runtime protocol defines; returns whether
    /// it was one. Declarations are information and read tolerantly;
    /// `tool/detachable` is a command and read strictly. What cannot be read,
    /// or comes from the wrong kind of peer, is reported and ignored.
    fn handle_notification(&self, notification: &IncomingNotification) -> bool {
        let plugin = self.extension.get().is_some();
        let ignored = |reason: &dyn std::fmt::Display| {
            self.diagnostic(format!("ignored `{}`: {reason}", notification.method));
        };
        match notification.method.as_str() {
            // A plugin's implicit instance.
            method::TOOLS_CHANGED if plugin => match notification.params::<RuntimeTools>() {
                Ok(tools) => {
                    lock(&self.tools).insert(None, Arc::new(tools.into()));
                }
                Err(error) => ignored(&error),
            },
            method::TOOLS_CHANGED => ignored(
                &"only a plugin sends it; a runtime reports an instance's tools with \
                  `capability/tools_changed`",
            ),
            method::CAPABILITY_TOOLS_CHANGED | method::MCP_CHANGED if plugin => {
                ignored(&"only a runtime sends it");
            }
            method::CAPABILITY_TOOLS_CHANGED => match notification.params() {
                Ok(CapabilityToolsChangedParams { instance, tools }) => {
                    lock(&self.tools).insert(Some(instance), Arc::new(tools.into()));
                }
                Err(error) => ignored(&error),
            },
            method::MCP_CHANGED => match notification.params() {
                Ok(McpChangedParams {
                    instance,
                    servers,
                    failures,
                }) => {
                    lock(&self.mcp)
                        .instances
                        .entry(instance.clone())
                        .or_default()
                        .servers = Some(servers);
                    if let Some(hub) = &self.mcp_hub {
                        hub.changed();
                        for failure in failures {
                            hub.failed(self.mcp_failure(&instance, failure.server, failure.reason));
                        }
                    }
                }
                Err(error) => ignored(&error),
            },
            method::TOOL_DETACHABLE => match notification.command::<DetachableParams>() {
                Ok(DetachableParams {
                    prepared_id,
                    note,
                    reports_itself,
                }) => {
                    if let Some(host) = lock(&self.executions).get(&prepared_id) {
                        host.detach
                            .reached_with(cookie_agent_runtime_api::Detachable {
                                note,
                                reports_itself,
                            });
                    }
                }
                Err(error) => ignored(&error),
            },
            _ => return false,
        }
        true
    }

    /// Hands a producer operation to the engine once it opened producer
    /// messaging on this connection.
    async fn call_producer(
        &self,
        request: &IncomingRequest,
    ) -> Result<serde_json::Value, RpcError> {
        let host = lock(&self.producers).host.clone().ok_or_else(|| {
            producer_rejected("producer messaging has not started on this connection")
        })?;
        let source = self
            .producer_source
            .get()
            .cloned()
            .ok_or_else(|| RpcError::from(errors::NOT_INITIALIZED.error()))?;
        // Commands: refused with the path of what their schema does not list.
        match request.method.as_str() {
            method::PRODUCER_REGISTER => drop(request.command::<ProducerRegisterParams>()?),
            method::PRODUCER_SEND => drop(request.command::<ProducerSendParams>()?),
            method::PRODUCER_UNREGISTER => drop(request.command::<ProducerUnregisterParams>()?),
            method::PRODUCER_DISCARD => drop(request.command::<ProducerDiscardParams>()?),
            _ => {}
        }
        (host.call)(RuntimeProducerCall {
            source,
            epoch: self.epoch,
            method: request.method.clone(),
            params: request.params()?,
        })
        .await
        .map_err(producer_rejected)
    }

    /// Records the runtime's recovery outcome. A repeat of the same outcome
    /// is accepted; a different one is not.
    fn complete_producer_recovery(&self, request: &IncomingRequest) -> Result<(), RpcError> {
        let RecoveryCompleteParams { outcome } = request.command()?;
        let host = {
            let mut state = lock(&self.producers);
            let Some(host) = state.host.clone() else {
                return Err(producer_rejected(
                    "producer recovery has not started on this connection",
                ));
            };
            if let Some(previous) = &state.outcome {
                return if *previous == outcome {
                    Ok(())
                } else {
                    Err(producer_rejected("recovery outcome already reported"))
                };
            }
            state.status = Some(match outcome {
                RecoveryOutcome::Ready => ProducerSourceStatus::Ready,
                RecoveryOutcome::Failed { .. } => ProducerSourceStatus::Failed,
            });
            state.outcome = Some(outcome);
            host
        };
        (host.changed)();
        Ok(())
    }

    async fn call_progress(&self, request: &IncomingRequest) -> Result<Outcome, RpcError> {
        let ToolProgressParams {
            prepared_id,
            progress,
        } = request.command()?;
        let sink = lock(&self.executions)
            .get(&prepared_id)
            .map(|host| host.progress.clone());
        let Some(sink) = sink else {
            return Ok(Outcome::of(Err(ToolError::ProgressSinkClosed)));
        };
        Ok(Outcome::of(sink.send(progress.into()).await))
    }

    /// The connection ended: calls fail through their pending replies, and
    /// its producer registrations no longer pass for live.
    fn connection_lost(&self) {
        lock(&self.executions).clear();
        let host = lock(&self.producers).host.clone();
        if let Some(host) = host {
            (host.changed)();
        }
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
            background,
            producers: _,
        } = context;
        let params = ExecuteParams {
            prepared_id,
            session,
            run,
            turn: turn_context.as_ref().into(),
            background,
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

/// The registry's `producer_rejected`, with `reason` as its detail.
fn producer_rejected(reason: impl Into<String>) -> RpcError {
    errors::PRODUCER_REJECTED.error().detail(reason).into()
}

/// Unregisters an execution; one dropped before its reply also cancels the
/// call in the runtime, unless the call reported itself at its detach point.
struct ExecutionGuard<'a> {
    client: &'a Arc<ClientInner>,
    prepared_id: u64,
    finished: bool,
}

impl Drop for ExecutionGuard<'_> {
    fn drop(&mut self) {
        let host = lock(&self.client.executions).remove(&self.prepared_id);
        // Work that reports itself outlives the call that waited for it.
        let reports_itself = host
            .and_then(|host| host.detach.current())
            .is_some_and(|detachable| detachable.reports_itself);
        if !self.finished && !reports_itself {
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
    /// The instance whose tools these are; `None` for a plugin.
    instance: Option<String>,
}

impl RemoteToolProvider {
    fn declared(&self) -> Option<Arc<DeclaredTools>> {
        lock(&self.client.inner.tools).get(&self.instance).cloned()
    }
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
        Ok(self
            .declared()
            .map(|tools| tools.prompt_sections.clone())
            .unwrap_or_default())
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(self
            .declared()
            .map(|tools| tools.specs.clone())
            .unwrap_or_default())
    }

    /// An MCP server's tool, by the instance's last MCP report.
    fn tool_source(&self, tool_name: &str) -> Option<cookie_agent_runtime_api::ProvidedToolSource> {
        let instance = self.instance.as_deref()?;
        let mcp = lock(&self.client.inner.mcp);
        mcp.instances
            .get(instance)?
            .servers
            .iter()
            .flatten()
            .find(|server| server.tools.iter().any(|tool| tool == tool_name))
            .map(|server| cookie_agent_runtime_api::ProvidedToolSource::Mcp {
                server: server.name.clone(),
            })
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
                        instance: self.instance.clone(),
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
