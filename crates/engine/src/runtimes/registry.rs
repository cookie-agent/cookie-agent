//! Runtime templates and the capability instances opened from them.
//!
//! A [`RuntimeTemplate`] is a way to reach a runtime; its ID is the *runtime
//! ID*. A runtime offers capabilities, and one connection serves all of
//! them. The [`RuntimeRegistry`] opens at most one [`RuntimeInstance`] per
//! (runtime ID, capability, canonical workspace) and shares it with every
//! session attached to it. The `global` and `local` instances start with the
//! engine; any other instance opens when a session first needs it.
//!
//! Production registers the built-in `global` and `local` templates and one
//! protocol template per configured `[runtimes.<id>]`, whose instances run in
//! child processes; `local` runs behind the runtime protocol in-process.
//! Instances are keyed by the canonical workspace the runtime reports, never
//! by a path the daemon resolved itself, since a workspace may live on
//! another machine. An instance whose protocol connection ends is marked
//! failed, and the next session that needs it starts it again under a new
//! generation.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use cookie_agent_config::SkillRegistry;
use cookie_agent_protocol::{
    CapabilityName, RuntimeAttachment, RuntimeCapabilityInfo, RuntimeKind, RuntimeLifetime,
};
use cookie_agent_runtime_api::{RuntimeInfo, ToolOrigin, ToolProvider};

use tokio_util::sync::CancellationToken;

use super::{ReconnectOutcome, ReconnectPolicy};
use crate::EngineError;

/// A way to reach a runtime and open its capabilities: the built-in
/// `global` and `local` runtimes and the configured `[runtimes.<id>]`
/// templates.
#[async_trait]
pub trait RuntimeTemplate: Send + Sync {
    /// The runtime's ID.
    fn runtime_id(&self) -> &str;

    /// How the daemon reaches the runtime.
    fn lifetime(&self) -> RuntimeLifetime {
        RuntimeLifetime::Spawned
    }

    /// What the runtime offers, connecting it when it is not connected. The
    /// error is a human-readable reason.
    async fn capabilities(&self) -> Result<Vec<RuntimeCapabilityInfo>, String>;

    /// What clients see without connecting: whether a connection is up, and
    /// what the runtime offered when it last connected.
    fn listing(&self) -> RuntimeListingState;

    /// Why no session can use the runtime now, without trying: a runtime
    /// the daemon cannot start, such as one that dials in, that is not
    /// connected. `None` when it is connected or the daemon starts it.
    fn offline(&self) -> Option<String> {
        None
    }

    /// The workspace an instance of `capability` opens in when a session
    /// names none, such as the daemon's working directory for `local`.
    fn default_workspace(&self, _capability: &CapabilityName) -> Option<PathBuf> {
        None
    }

    /// Opens `capability` for `workspace`, which is `Some` exactly when the
    /// capability takes one and is the path a session asked for, exactly as
    /// given: the runtime resolves it and reports the canonical workspace.
    /// One connection serves every instance; a template whose connection
    /// dropped connects again. The error says whether the runtime could not
    /// be reached or did not open the capability.
    async fn start(
        &self,
        capability: &CapabilityName,
        workspace: Option<&Path>,
    ) -> Result<StartedRuntime, StartError>;

    /// Lets go of the template's connection and returns it, as the daemon
    /// stops or a client disconnects the runtime; the caller shuts it down.
    fn disconnect(&self) -> Option<RuntimeLink> {
        None
    }

    /// Takes the runtime's settings as the daemon's configuration has them
    /// now (`docs/site/specs/runtime-management.md` section 3.1): the next
    /// connection initializes with them, and a connected runtime gets them
    /// at once. The default ignores them: only a runtime behind the runtime
    /// protocol takes settings.
    fn update_settings(&self, _settings: &cookie_agent_runtime_api::protocol::RuntimeSettings) {}

    /// Every MCP server the connected runtime is configured with, as
    /// `mcp.server.list` shows them without a workspace: its runtime-scoped
    /// servers, live, and its workspace-scoped ones as configured. Empty
    /// while it is not connected.
    fn runtime_mcp_servers(&self) -> Vec<cookie_agent_protocol::McpServerInfo> {
        Vec::new()
    }

    /// Reconnects the connected runtime's runtime-scoped MCP server
    /// `server` now and waits for the attempt.
    async fn reconnect_runtime_mcp(&self, server: &str) -> Result<(), EngineError> {
        Err(EngineError::Mcp(format!(
            "runtime `{}` hosts no MCP server `{server}` of its own",
            self.runtime_id()
        )))
    }

    /// The ACP agents the runtime hosts and whether a session can use them
    /// now (`docs/site/specs/acp-agents.md` section 3). A runtime that
    /// hosts none answers an empty list.
    fn acp_agents(&self) -> RuntimeAcpAgents {
        RuntimeAcpAgents::default()
    }

    /// What the daemon knows of the connected runtime's shell environment
    /// (`docs/site/specs/shell-environment.md`); `None` while it is not
    /// connected or does not run behind the runtime protocol.
    fn shell_environment(&self) -> Option<RuntimeShellEnvironment> {
        None
    }

    /// Asks the connected runtime to capture the user's shell environment
    /// again, as after the configuration was reloaded.
    fn refresh_shell_environment(&self) {}

    /// The runtime's connection, connecting it when the daemon starts the
    /// runtime, for its ACP agents. Refused at once when the runtime dials
    /// in and is not connected.
    async fn agent_connection(&self) -> Result<RuntimeLink, String> {
        Err(format!(
            "runtime `{}` hosts no ACP agents",
            self.runtime_id()
        ))
    }
}

/// What the daemon knows of a connected runtime's shell environment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeShellEnvironment {
    /// The runtime's protocol version, from its initialize reply.
    pub protocol_version: Option<cookie_agent_protocol::SemVer>,
    /// The report of its latest capture, once it sent one.
    pub report: Option<cookie_agent_runtime_api::protocol::ShellEnvironmentReport>,
}

/// The ACP agents a runtime hosts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeAcpAgents {
    /// As the runtime last reported them, or as the daemon's configuration
    /// declares them for a runtime it starts that is not connected yet.
    pub agents: Vec<cookie_agent_runtime_api::protocol::AcpAgentReport>,
    /// Whether a session can use them now: the runtime is connected, or the
    /// daemon starts it on demand.
    pub available: bool,
}

/// A runtime's protocol connection, opaque outside the engine: what the
/// ACP agents it hosts use, and what a template lets go of as it
/// disconnects.
#[derive(Clone)]
pub struct RuntimeLink(pub(crate) super::RuntimeClient);

/// Why [`RuntimeTemplate::start`] failed, with a human-readable reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartError {
    /// The runtime could not be reached: connecting to it or initializing
    /// it failed, or its connection ended.
    Connect(String),
    /// The runtime is up but did not open the capability, such as for a
    /// workspace outside the directories it serves.
    Open(String),
}

impl StartError {
    /// The reason, without saying which step failed.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Connect(reason) | Self::Open(reason) => reason,
        }
    }
}

impl std::fmt::Display for StartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.reason())
    }
}

/// What [`RuntimeTemplate::listing`] reports.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeListingState {
    pub connected: bool,
    pub capabilities: Vec<RuntimeCapabilityInfo>,
    pub platform: Option<String>,
    /// The machine the runtime reported when it last connected; `None`
    /// when it reported none, or a malformed one.
    pub machine: Option<cookie_agent_protocol::runtime_protocol::RuntimeMachine>,
    /// The home directory the runtime reported when it last connected;
    /// `None` when it reported none, or a malformed one.
    pub home: Option<String>,
    /// Why the last attempt to connect failed, while it is not connected.
    pub message: Option<String>,
}

/// What an opened capability instance reports about itself.
pub struct StartedRuntime {
    /// Identity, capability, kind, attachment name, and canonical workspace.
    pub info: RuntimeInfo,
    /// Tool providers, in composition order.
    pub providers: Vec<Arc<dyn ToolProvider>>,
    /// Skills the instance contributes: user skills for the global runtime,
    /// project skills found for its workspace for a coding instance. Read
    /// once at start.
    pub skills: SkillRegistry,
    /// The runtime protocol connection of an instance that runs behind it;
    /// `None` for an in-process runtime.
    pub(crate) connection: Option<super::remote::InstanceConnection>,
    /// The runtime's own tools, outside any instance (its runtime-scoped
    /// MCP servers'), which sessions compose once per runtime.
    pub(crate) shared: Option<super::remote::SharedTools>,
}

/// Lifecycle state of a runtime instance as clients see it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeInstanceStatus {
    Starting,
    Ready,
    Reconnecting,
    Failed,
    Stopped,
}

/// A running runtime, shared by every session attached to it. A restart
/// after its connection dropped keeps the instance (and its identity) and
/// replaces what the runtime declared under a new generation.
pub(crate) struct RuntimeInstance {
    info: Arc<RuntimeInfo>,
    current: Mutex<CurrentStart>,
    /// Providers added in-process after the instance started; they survive
    /// restarts.
    added: Mutex<Vec<Arc<dyn ToolProvider>>>,
}

/// What the runtime declared at its current start.
struct CurrentStart {
    /// Distinguishes this start of the instance from any earlier one, so a
    /// tool published before a restart fails as changed.
    generation: u64,
    providers: Vec<Arc<dyn ToolProvider>>,
    skills: Arc<SkillRegistry>,
    connection: Option<super::remote::InstanceConnection>,
    shared: Option<super::remote::SharedTools>,
}

impl CurrentStart {
    fn new(started: StartedRuntime, generation: u64) -> Self {
        Self {
            generation,
            providers: started.providers,
            skills: Arc::new(started.skills),
            connection: started.connection,
            shared: started.shared,
        }
    }
}

impl RuntimeInstance {
    fn new(started: StartedRuntime, generation: u64) -> Self {
        let StartedRuntime {
            info,
            providers,
            skills,
            connection,
            shared,
        } = started;
        Self {
            info: Arc::new(info),
            current: Mutex::new(CurrentStart {
                generation,
                providers,
                skills: Arc::new(skills),
                connection,
                shared,
            }),
            added: Mutex::default(),
        }
    }

    fn current(&self) -> std::sync::MutexGuard<'_, CurrentStart> {
        self.current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn connection(&self) -> Option<super::remote::InstanceConnection> {
        self.current().connection.clone()
    }

    pub(crate) fn info(&self) -> &Arc<RuntimeInfo> {
        &self.info
    }

    pub(crate) fn generation(&self) -> u64 {
        self.current().generation
    }

    pub(crate) fn origin(&self) -> ToolOrigin {
        ToolOrigin::Runtime(Arc::clone(&self.info))
    }

    /// The instance's providers in composition order.
    #[cfg(test)]
    pub(crate) fn providers(&self) -> Vec<Arc<dyn ToolProvider>> {
        self.sources().1
    }

    /// The current generation together with the providers of that start, in
    /// composition order.
    pub(crate) fn sources(&self) -> (u64, Vec<Arc<dyn ToolProvider>>) {
        let current = self.current();
        let mut providers = current.providers.clone();
        let generation = current.generation;
        drop(current);
        providers.extend(
            self.added
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .cloned(),
        );
        (generation, providers)
    }

    /// The current generation together with the runtime's own tools, when
    /// it has any; every instance of the runtime carries the same ones.
    pub(crate) fn shared_source(&self) -> Option<(u64, super::remote::SharedTools)> {
        let current = self.current();
        current
            .shared
            .clone()
            .map(|shared| (current.generation, shared))
    }

    pub(crate) fn push_provider(&self, provider: Arc<dyn ToolProvider>) {
        self.added
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(provider);
    }

    pub(crate) fn skills(&self) -> Arc<SkillRegistry> {
        Arc::clone(&self.current().skills)
    }

    /// Replaces the skills the instance contributes, as the global runtime's
    /// are after the official skills changed.
    pub(crate) fn replace_skills(&self, skills: SkillRegistry) {
        self.current().skills = Arc::new(skills);
    }

    /// Whether `info` describes this instance: a restart must come back as
    /// the same runtime, capability, kind, attachment name, and workspace.
    fn same_identity(&self, info: &RuntimeInfo) -> bool {
        self.info.id() == info.id()
            && self.info.capability() == info.capability()
            && self.info.kind() == info.kind()
            && self.info.attachment_name() == info.attachment_name()
            && self.info.workspace().map(|workspace| workspace.root())
                == info.workspace().map(|workspace| workspace.root())
    }

    /// Takes over what a restart declared, under `generation`.
    fn restart(&self, started: StartedRuntime, generation: u64) {
        *self.current() = CurrentStart::new(started, generation);
    }

    /// The `AGENTS.md` context of a coding runtime for a run of `preset`;
    /// `None` for a runtime without a workspace.
    pub(crate) async fn agent_md(
        &self,
        preset: Option<&str>,
    ) -> Result<Option<cookie_agent_runtime_api::context::AgentMdContext>, EngineError> {
        if let Some(connection) = self.connection() {
            return connection.agent_md(preset).await;
        }
        let Some(workspace) = self.info.workspace() else {
            return Ok(None);
        };
        cookie_agent_runtime_api::context::load_agent_md(
            workspace.root(),
            preset,
            cookie_agent_runtime_api::context::AGENT_MD_MAX_BYTES,
        )
        .map(Some)
        .map_err(|error| EngineError::AgentMdIo {
            path: error.path,
            source: error.source,
        })
    }

    /// The attachment a session records for this instance under `name`.
    pub(crate) fn attachment(
        &self,
        name: cookie_agent_protocol::AttachmentName,
    ) -> Result<RuntimeAttachment, EngineError> {
        Ok(RuntimeAttachment {
            name,
            runtime_id: cookie_agent_protocol::RuntimeId::new(self.info.id()).map_err(|_| {
                EngineError::Runtime(format!("invalid runtime ID `{}`", self.info.id()))
            })?,
            capability: self.info.capability().clone(),
            kind: self.info.kind().clone(),
            workspace: self.workspace_string(),
        })
    }

    fn workspace_string(&self) -> Option<String> {
        self.info
            .workspace()
            .map(|workspace| workspace.root().to_string_lossy().into_owned())
    }

    /// Drops what the runtime declared and its protocol connection, as when
    /// it failed for good.
    fn drop_runtime(&self) -> Option<super::remote::RuntimeClient> {
        let mut current = self.current();
        current.providers.clear();
        current.shared = None;
        current
            .connection
            .take()
            .map(|connection| connection.client)
    }

    /// Drops every provider (they may hold the engine) and the protocol
    /// connection.
    fn clear(&self) -> Option<super::remote::RuntimeClient> {
        self.added
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.drop_runtime()
    }
}

/// How long a session that needs an instance whose connection was lost
/// waits for the daemon to bring it back, beyond which it is refused.
const RECONNECT_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Why `template`'s runtime could not be reached, from `reason`: a runtime
/// that dials in says it is not connected itself; the daemon failed to
/// start another.
fn start_failure(template: &dyn RuntimeTemplate, reason: &str) -> String {
    if template.lifetime() == RuntimeLifetime::DialIn {
        reason.to_owned()
    } else {
        format!(
            "runtime `{}` failed to start: {reason}",
            template.runtime_id()
        )
    }
}

/// The error of a start a client's disconnect overtook.
fn disconnected(runtime_id: &str) -> EngineError {
    EngineError::Runtime(format!(
        "runtime `{runtime_id}` was disconnected while it started"
    ))
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct InstanceKey {
    runtime_id: String,
    capability: String,
    workspace: Option<PathBuf>,
}

impl InstanceKey {
    fn of(info: &RuntimeInfo) -> Self {
        Self {
            runtime_id: info.id().to_owned(),
            capability: info.capability().as_str().to_owned(),
            workspace: info
                .workspace()
                .map(|workspace| workspace.root().to_owned()),
        }
    }
}

/// What was recorded for an instance before a start, as
/// [`RuntimeRegistry::begin_start`] found it.
struct Previous {
    /// Whether the registry had recorded the instance at all.
    recorded: bool,
    /// The instance that ran there before it failed, for the start to
    /// revive.
    instance: Option<Arc<RuntimeInstance>>,
}

/// Whether a failed start is recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Record {
    /// Only for an instance the registry already recorded: a request that
    /// is refused, such as an attach, leaves nothing behind.
    Known,
    /// Always, for an attachment a session records.
    Always,
}

#[derive(Clone)]
enum InstanceSlot {
    Starting,
    Ready(Arc<RuntimeInstance>),
    /// The connection dropped and the daemon is restarting the runtime; its
    /// tools stay published and calls fail fast.
    Reconnecting(Arc<RuntimeInstance>),
    /// Failed to start, or dropped and did not reconnect. `instance` is the
    /// instance that ran before, which a later start revives.
    Failed {
        reason: String,
        instance: Option<Arc<RuntimeInstance>>,
    },
    Stopped(Arc<RuntimeInstance>),
}

impl InstanceSlot {
    /// The instance whose tools sessions see: ready or reconnecting.
    fn published(&self) -> Option<&Arc<RuntimeInstance>> {
        match self {
            Self::Ready(instance) | Self::Reconnecting(instance) => Some(instance),
            Self::Starting | Self::Failed { .. } | Self::Stopped(_) => None,
        }
    }
}

/// What keeps a runtime busy, which a client cannot disconnect it during
/// ([`crate::Engine::runtime_activity`]).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeActivity {
    /// Active runs of sessions that attach the runtime.
    pub active_runs: u32,
    /// Background calls executing on the runtime.
    pub background_calls: u32,
}

impl RuntimeActivity {
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.active_runs == 0 && self.background_calls == 0
    }
}

impl std::fmt::Display for RuntimeActivity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = |count: u32, one: &str, many: &str| match count {
            1 => format!("1 {one}"),
            count => format!("{count} {many}"),
        };
        let parts = [
            (self.active_runs > 0).then(|| count(self.active_runs, "active run", "active runs")),
            (self.background_calls > 0)
                .then(|| count(self.background_calls, "background call", "background calls")),
        ];
        let parts = parts.into_iter().flatten().collect::<Vec<_>>();
        formatter.write_str(&parts.join(", "))
    }
}

/// A runtime instance that failed to start, or dropped and did not
/// reconnect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeFailure {
    pub runtime_id: String,
    pub capability: String,
    /// The workspace the instance was started for, when its capability
    /// takes one.
    pub workspace: Option<String>,
    pub reason: String,
}

/// A runtime as `runtimes.list` reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeTemplateSummary {
    pub runtime_id: String,
    pub lifetime: RuntimeLifetime,
    pub listing: RuntimeListingState,
}

/// An instance as `runtimes.list` reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeInstanceSummary {
    pub runtime_id: String,
    pub capability: String,
    /// The name the instance reports; absent until it has started.
    pub attachment_name: Option<String>,
    pub kind: Option<RuntimeKind>,
    pub workspace: Option<String>,
    pub platform: Option<String>,
    pub status: RuntimeInstanceStatus,
    /// Why the instance failed, when it did.
    pub message: Option<String>,
}

/// The base layer's providers, the registered templates, and the instances
/// started from them.
pub(crate) struct RuntimeRegistry {
    base: Mutex<Vec<Arc<dyn ToolProvider>>>,
    templates: Mutex<BTreeMap<String, Arc<dyn RuntimeTemplate>>>,
    /// The templates of runtimes that dialed in, also among `templates`.
    dial_ins: Mutex<BTreeMap<String, Arc<super::DialInTemplate>>>,
    slots: Mutex<BTreeMap<InstanceKey, InstanceSlot>>,
    /// Workspace spellings that resolved to another canonical workspace when
    /// an instance started for them, by (runtime ID, requested workspace).
    aliases: Mutex<BTreeMap<InstanceKey, PathBuf>>,
    /// Serializes instance starts so one instance never starts twice.
    start: tokio::sync::Mutex<()>,
    generations: AtomicU64,
    global: Arc<RuntimeInstance>,
    local: Arc<RuntimeInstance>,
    changes: tokio::sync::watch::Sender<u64>,
    /// Instance failures, for clients viewing attached sessions.
    failures: tokio::sync::broadcast::Sender<RuntimeFailure>,
    /// How a dropped instance is reconnected.
    reconnect: Mutex<ReconnectPolicy>,
    /// Cancelled when the registry is cleared, which ends reconnect loops.
    shutdown: CancellationToken,
    /// Composition diagnostics already reported, so each is reported once.
    reported: Mutex<std::collections::HashSet<String>>,
    /// Runtimes a client disconnected ([`Self::disconnect`]): opening a
    /// session leaves them down until something uses them.
    parked: Mutex<std::collections::HashSet<String>>,
    /// By runtime ID, cancelled when a client disconnects the runtime: a
    /// start or reconnect begun before then neither installs an instance
    /// nor keeps trying.
    stops: Mutex<BTreeMap<String, CancellationToken>>,
}

impl RuntimeRegistry {
    /// A registry whose `global` and `local` instances are already running.
    pub(crate) fn new(
        global_template: Arc<dyn RuntimeTemplate>,
        global: StartedRuntime,
        local_template: Arc<dyn RuntimeTemplate>,
        local: StartedRuntime,
    ) -> Self {
        let global = Arc::new(RuntimeInstance::new(global, 1));
        let local = Arc::new(RuntimeInstance::new(local, 2));
        let mut slots = BTreeMap::new();
        slots.insert(
            InstanceKey::of(&global.info),
            InstanceSlot::Ready(Arc::clone(&global)),
        );
        slots.insert(
            InstanceKey::of(&local.info),
            InstanceSlot::Ready(Arc::clone(&local)),
        );
        let mut templates = BTreeMap::new();
        templates.insert(global_template.runtime_id().to_owned(), global_template);
        templates.insert(local_template.runtime_id().to_owned(), local_template);
        Self {
            base: Mutex::default(),
            templates: Mutex::new(templates),
            dial_ins: Mutex::default(),
            slots: Mutex::new(slots),
            aliases: Mutex::default(),
            start: tokio::sync::Mutex::new(()),
            generations: AtomicU64::new(3),
            global,
            local,
            changes: tokio::sync::watch::channel(0).0,
            failures: tokio::sync::broadcast::channel(64).0,
            reconnect: Mutex::new(ReconnectPolicy::default()),
            shutdown: CancellationToken::new(),
            reported: Mutex::default(),
            parked: Mutex::default(),
            stops: Mutex::default(),
        }
    }

    /// Reports each composition diagnostic once on stderr.
    pub(crate) fn report(&self, diagnostics: &[String]) {
        if diagnostics.is_empty() {
            return;
        }
        let mut reported = self
            .reported
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for diagnostic in diagnostics {
            if reported.insert(diagnostic.clone()) {
                eprintln!("cookie-agent: {diagnostic}");
            }
        }
    }

    /// Adds a template. Its runtime ID must not be registered yet.
    pub(crate) fn register_template(
        &self,
        template: Arc<dyn RuntimeTemplate>,
    ) -> Result<(), EngineError> {
        let runtime_id = template.runtime_id().to_owned();
        cookie_agent_protocol::RuntimeId::new(runtime_id.as_str())
            .map_err(|_| EngineError::Runtime(format!("invalid runtime ID `{runtime_id}`")))?;
        let mut templates = self
            .templates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if templates.contains_key(&runtime_id) {
            return Err(EngineError::Runtime(format!(
                "runtime `{runtime_id}` is already registered"
            )));
        }
        templates.insert(runtime_id, template);
        drop(templates);
        self.notify();
        Ok(())
    }

    /// The template of dial-in runtime `runtime_id`, created on its first
    /// connection. An ID a configured or built-in runtime uses cannot dial
    /// in.
    pub(crate) fn dial_in_template(
        &self,
        runtime_id: &str,
    ) -> Result<Arc<super::DialInTemplate>, super::DialInRefusal> {
        let id = cookie_agent_protocol::RuntimeId::new(runtime_id)
            .map_err(|_| super::DialInRefusal::InvalidId(runtime_id.to_owned()))?;
        if id.is_reserved() {
            return Err(super::DialInRefusal::Reserved(runtime_id.to_owned()));
        }
        let mut templates = self
            .templates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut dial_ins = self
            .dial_ins
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(template) = dial_ins.get(runtime_id) {
            return Ok(Arc::clone(template));
        }
        if templates.contains_key(runtime_id) {
            return Err(super::DialInRefusal::Configured(runtime_id.to_owned()));
        }
        let template = Arc::new(super::DialInTemplate::new(runtime_id));
        templates.insert(runtime_id.to_owned(), template.clone());
        dial_ins.insert(runtime_id.to_owned(), Arc::clone(&template));
        drop((templates, dial_ins));
        self.notify();
        Ok(template)
    }

    /// Reopens the failed instances of `runtime_id`, as when it dialed in
    /// again, and signals the change.
    pub(crate) fn revive(self: &Arc<Self>, runtime_id: &str) {
        let failed = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(key, slot)| {
                key.runtime_id == runtime_id && matches!(slot, InstanceSlot::Failed { .. })
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        self.notify();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        for key in failed {
            let registry = Arc::clone(self);
            handle.spawn(async move {
                let _ = registry
                    .ensure(
                        &key.runtime_id,
                        Some(key.capability.as_str()),
                        key.workspace.as_deref(),
                    )
                    .await;
            });
        }
    }

    /// Records why dial-in runtime `template`'s connection `client` ended,
    /// when it does, and signals the change, so listings show the runtime
    /// disconnected and why.
    pub(crate) fn watch_connection(
        self: &Arc<Self>,
        template: &Arc<super::DialInTemplate>,
        client: &super::RuntimeClient,
    ) {
        let closed = client.closed();
        let registry = Arc::downgrade(self);
        let template = Arc::downgrade(template);
        let epoch = client.epoch();
        let client = client.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let reason = closed.await;
                if let Some(template) = template.upgrade() {
                    template.disconnected(epoch, &reason, client.last_heard_at());
                }
                if let Some(registry) = registry.upgrade() {
                    registry.notify();
                }
            });
        }
    }

    pub(crate) fn push_base(&self, provider: Arc<dyn ToolProvider>) {
        self.base
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(provider);
    }

    pub(crate) fn base_providers(&self) -> Vec<Arc<dyn ToolProvider>> {
        self.base
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) const fn global(&self) -> &Arc<RuntimeInstance> {
        &self.global
    }

    pub(crate) const fn local(&self) -> &Arc<RuntimeInstance> {
        &self.local
    }

    /// The local coding runtime's workspace root.
    #[cfg(test)]
    pub(crate) fn local_workspace_root(&self) -> &Path {
        self.local
            .info
            .workspace()
            .expect("the local coding runtime has a workspace")
            .root()
    }

    /// Whether a template of `runtime_id` is registered.
    pub(crate) fn knows(&self, runtime_id: &str) -> bool {
        self.template(runtime_id).is_some()
    }

    fn template(&self, runtime_id: &str) -> Option<Arc<dyn RuntimeTemplate>> {
        self.templates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(runtime_id)
            .cloned()
    }

    /// The running instance a session attachment refers to, if it is up.
    pub(crate) fn instance(&self, attachment: &RuntimeAttachment) -> Option<Arc<RuntimeInstance>> {
        let key = InstanceKey {
            runtime_id: attachment.runtime_id.as_str().to_owned(),
            capability: attachment.capability.as_str().to_owned(),
            workspace: attachment.workspace.as_deref().map(PathBuf::from),
        };
        let slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slots
            .get(&key)
            .and_then(InstanceSlot::published)
            .map(Arc::clone)
    }

    /// Every instance whose tools sessions see: ready or reconnecting, the
    /// built-in ones included.
    pub(crate) fn published_instances(&self) -> Vec<Arc<RuntimeInstance>> {
        let mut instances = vec![Arc::clone(&self.global), Arc::clone(&self.local)];
        for instance in self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter_map(InstanceSlot::published)
        {
            if !instances.iter().any(|known| Arc::ptr_eq(known, instance)) {
                instances.push(Arc::clone(instance));
            }
        }
        instances
    }

    /// A running instance of `runtime_id` whose canonical workspace is
    /// `workspace`, if one is up.
    pub(crate) fn instance_at(
        &self,
        runtime_id: &str,
        workspace: Option<&str>,
    ) -> Option<Arc<RuntimeInstance>> {
        let workspace = workspace.map(PathBuf::from);
        self.slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|(key, _)| key.runtime_id == runtime_id && key.workspace == workspace)
            .and_then(|(_, slot)| slot.published())
            .map(Arc::clone)
    }

    /// The MCP servers that running instances host, in instance order.
    pub(crate) fn hosted_mcp_servers(&self) -> Vec<super::HostedMcpServer> {
        let instances = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter_map(InstanceSlot::published)
            .map(Arc::clone)
            .collect::<Vec<_>>();
        instances
            .iter()
            .filter_map(|instance| instance.connection())
            .flat_map(|connection| connection.mcp_servers())
            .collect()
    }

    /// The servers every connected runtime is configured with, as
    /// [`RuntimeTemplate::runtime_mcp_servers`] lists them, except a
    /// workspace-scoped one that a running instance of its runtime hosts,
    /// which that instance lists.
    pub(crate) fn runtime_mcp_servers(
        &self,
        hosted: &[super::HostedMcpServer],
    ) -> Vec<cookie_agent_protocol::McpServerInfo> {
        let templates = self
            .templates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        templates
            .iter()
            .flat_map(|template| template.runtime_mcp_servers())
            .filter(|server| {
                server.scope == Some(cookie_agent_protocol::McpServerScope::Runtime)
                    || !hosted.iter().any(|instance| {
                        instance.runtime_id == server.runtime_id.as_str()
                            && instance.report.name == server.name
                    })
            })
            .collect()
    }

    /// Reconnects runtime `runtime_id`'s own MCP server `server`.
    pub(crate) async fn reconnect_runtime_mcp(
        &self,
        runtime_id: &str,
        server: &str,
    ) -> Result<(), EngineError> {
        let template = self
            .template(runtime_id)
            .ok_or_else(|| EngineError::Mcp(format!("runtime `{runtime_id}` is not running")))?;
        template.reconnect_runtime_mcp(server).await
    }

    /// The running instance a request for `runtime_id` at `requested`
    /// refers to, if it is up. A template that takes no workspace has one
    /// instance; otherwise `requested` is the canonical workspace or a
    /// spelling of it that an earlier start resolved.
    fn live(&self, requested: &InstanceKey) -> Option<Arc<RuntimeInstance>> {
        let key = self.resolve(requested);
        let slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slots.iter().find_map(|(candidate, slot)| {
            slot.published()
                .filter(|_| {
                    candidate.runtime_id == key.runtime_id
                        && candidate.capability == key.capability
                        && (key.workspace.is_none() || candidate.workspace == key.workspace)
                })
                .map(Arc::clone)
        })
    }

    /// The canonical key a request resolved to when an instance started for
    /// it; the request itself otherwise.
    fn resolve(&self, requested: &InstanceKey) -> InstanceKey {
        self.aliases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(requested)
            .map_or_else(
                || requested.clone(),
                |workspace| InstanceKey {
                    runtime_id: requested.runtime_id.clone(),
                    capability: requested.capability.clone(),
                    workspace: Some(workspace.clone()),
                },
            )
    }

    /// The capability `requested` names, or the runtime's only capability
    /// when it names none, as `template` offers it: from its last
    /// connection when it has one, connecting it otherwise.
    /// The error is the reason connecting failed, or `None` when the
    /// runtime does not offer the capability.
    async fn capability(
        template: &dyn RuntimeTemplate,
        requested: Option<&str>,
    ) -> Result<RuntimeCapabilityInfo, (EngineError, Option<String>)> {
        let runtime_id = template.runtime_id();
        let known = template.listing().capabilities;
        let capabilities = if known.is_empty() {
            template.capabilities().await.map_err(|reason| {
                (
                    EngineError::Runtime(start_failure(template, &reason)),
                    Some(reason),
                )
            })?
        } else {
            known
        };
        let refused = |message: String| (EngineError::Runtime(message), None);
        let names = || {
            capabilities
                .iter()
                .map(|capability| capability.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        match requested {
            Some(name) => capabilities
                .iter()
                .find(|capability| capability.name.as_str() == name)
                .cloned()
                .ok_or_else(|| {
                    refused(format!(
                        "runtime `{runtime_id}` offers no capability `{name}`; it offers: {}",
                        names()
                    ))
                }),
            None => match capabilities.as_slice() {
                [only] => Ok(only.clone()),
                [] => Err(refused(format!(
                    "runtime `{runtime_id}` offers no capabilities"
                ))),
                _ => Err(refused(format!(
                    "runtime `{runtime_id}` offers several capabilities; choose one of: {}",
                    names()
                ))),
            },
        }
    }

    /// The instance of `capability` of `runtime_id` (the runtime's only
    /// capability when `None`) for `workspace`, opened when it is not
    /// running, as attaching it, creating a session with it, or retrying it
    /// does. `workspace` must be given exactly when the capability takes
    /// one, unless the template has a default workspace for it; an unknown
    /// runtime ID is an error.
    ///
    /// The workspace is passed to the runtime exactly as given: it may live
    /// on another machine, so the daemon never resolves it. The instance is
    /// keyed by the canonical workspace the runtime reports, so two spellings
    /// of one workspace share an instance.
    ///
    /// A request for an instance the registry has never recorded records
    /// nothing when it fails: no session uses it, so it does not show in
    /// `runtimes.list`. One that failed before stays failed with the new
    /// reason.
    pub(crate) async fn ensure(
        self: &Arc<Self>,
        runtime_id: &str,
        capability: Option<&str>,
        workspace: Option<&Path>,
    ) -> Result<Arc<RuntimeInstance>, EngineError> {
        self.open(runtime_id, capability, workspace, Record::Known)
            .await
    }

    /// [`Self::ensure`], recording a failed start as `record` says: always
    /// for an attachment a session records, so clients viewing the session
    /// see why it publishes no tools.
    async fn open(
        self: &Arc<Self>,
        runtime_id: &str,
        capability: Option<&str>,
        workspace: Option<&Path>,
        record: Record,
    ) -> Result<Arc<RuntimeInstance>, EngineError> {
        let template = self
            .template(runtime_id)
            .ok_or_else(|| EngineError::Runtime(format!("unknown runtime `{runtime_id}`")))?;
        // Using a runtime a client disconnected starts it again.
        self.unpark(runtime_id);
        let offered = match Self::capability(template.as_ref(), capability).await {
            Ok(offered) => offered,
            Err((error, Some(_))) => {
                // The runtime could not be reached: a session that names the
                // capability, as one reopening does, sees its instance fail.
                if let Some(capability) = capability {
                    let key = InstanceKey {
                        runtime_id: runtime_id.to_owned(),
                        capability: capability.to_owned(),
                        workspace: workspace.map(Path::to_owned),
                    };
                    let previous = self.begin_start(&key);
                    self.fail_start(key, error.to_string(), previous, record);
                }
                return Err(error);
            }
            Err((error, None)) => return Err(error),
        };
        let default_workspace = template.default_workspace(&offered.name);
        let workspace = match (offered.takes_workspace, workspace) {
            (true, Some(workspace)) if workspace.as_os_str().is_empty() => {
                return Err(EngineError::Runtime(format!(
                    "capability `{}` of runtime `{runtime_id}` requires a workspace",
                    offered.name
                )));
            }
            (true, Some(workspace)) => Some(workspace),
            (true, None) => Some(default_workspace.as_deref().ok_or_else(|| {
                EngineError::Runtime(format!(
                    "capability `{}` of runtime `{runtime_id}` requires a workspace",
                    offered.name
                ))
            })?),
            (false, Some(_)) => {
                return Err(EngineError::Runtime(format!(
                    "capability `{}` of runtime `{runtime_id}` takes no workspace",
                    offered.name
                )));
            }
            (false, None) => None,
        };
        let requested = InstanceKey {
            runtime_id: runtime_id.to_owned(),
            capability: offered.name.as_str().to_owned(),
            workspace: workspace.map(Path::to_owned),
        };
        if let Some(instance) = self.live(&requested) {
            return self.reuse(template.as_ref(), instance).await;
        }
        let _start = self.start.lock().await;
        if let Some(instance) = self.live(&requested) {
            drop(_start);
            return self.reuse(template.as_ref(), instance).await;
        }
        let pending = self.resolve(&requested);
        let stop = self.stop_token(runtime_id);
        let previous = self.begin_start(&pending);
        let started = template.start(&offered.name, workspace).await;
        if stop.is_cancelled() {
            self.abandon_start(&pending, previous);
            return Err(disconnected(runtime_id));
        }
        match started {
            Ok(started)
                if started.info.id() == runtime_id
                    && started.info.capability() == &offered.name =>
            {
                match self.install(&requested, &pending, started, previous.instance, &stop) {
                    Some(instance) => Ok(instance),
                    None => Err(disconnected(runtime_id)),
                }
            }
            Ok(started) => {
                let message = format!(
                    "runtime `{runtime_id}` opened `{}` of runtime `{}` for capability `{}`",
                    started.info.capability(),
                    started.info.id(),
                    offered.name
                );
                self.fail_start(pending, message.clone(), previous, record);
                Err(EngineError::Runtime(message))
            }
            Err(error) => {
                let message = match &error {
                    StartError::Connect(reason) => start_failure(template.as_ref(), reason),
                    StartError::Open(reason) => match workspace {
                        Some(workspace) => format!(
                            "runtime `{runtime_id}` could not open `{}` for workspace `{}`: \
                             {reason}",
                            offered.name,
                            workspace.display()
                        ),
                        None => format!(
                            "runtime `{runtime_id}` could not open `{}`: {reason}",
                            offered.name
                        ),
                    },
                };
                self.fail_start(pending, message.clone(), previous, record);
                Err(EngineError::Runtime(message))
            }
        }
    }

    /// `instance`, which a session is about to use, when its connection is
    /// live; an instance is never handed out over a dead connection. One
    /// whose connection ended, or does not answer a liveness check, is
    /// being reconnected: a runtime that cannot be reached without trying
    /// (a dial-in runtime that is not connected) is refused at once, and
    /// the daemon waits for one it starts to come back, up to
    /// [`RECONNECT_WAIT`].
    async fn reuse(
        &self,
        template: &dyn RuntimeTemplate,
        instance: Arc<RuntimeInstance>,
    ) -> Result<Arc<RuntimeInstance>, EngineError> {
        let key = InstanceKey::of(&instance.info);
        let ready = matches!(
            self.slot(&key),
            Some(InstanceSlot::Ready(current)) if Arc::ptr_eq(&current, &instance)
        );
        if ready {
            let live = match instance.connection() {
                Some(connection) => connection.client.verify_live().await.is_ok(),
                None => true,
            };
            if live {
                return Ok(instance);
            }
        }
        if let Some(offline) = template.offline() {
            return Err(EngineError::Runtime(offline));
        }
        self.settle(&key).await
    }

    /// Waits for the instance at `key`, whose connection was lost, to come
    /// back: ready over a live connection, or failed.
    async fn settle(&self, key: &InstanceKey) -> Result<Arc<RuntimeInstance>, EngineError> {
        let mut changes = self.changes.subscribe();
        let wait = async {
            loop {
                match self.slot(key) {
                    Some(InstanceSlot::Ready(instance))
                        if instance
                            .connection()
                            .is_none_or(|connection| !connection.is_closed()) =>
                    {
                        return Ok(instance);
                    }
                    Some(InstanceSlot::Failed { reason, .. }) => {
                        return Err(EngineError::Runtime(reason));
                    }
                    None | Some(InstanceSlot::Stopped(_)) => {
                        return Err(EngineError::Runtime(format!(
                            "runtime `{}` stopped",
                            key.runtime_id
                        )));
                    }
                    Some(
                        InstanceSlot::Starting
                        | InstanceSlot::Ready(_)
                        | InstanceSlot::Reconnecting(_),
                    ) => {}
                }
                if changes.changed().await.is_err() {
                    return Err(EngineError::Runtime(format!(
                        "runtime `{}` stopped",
                        key.runtime_id
                    )));
                }
            }
        };
        tokio::time::timeout(RECONNECT_WAIT, wait)
            .await
            .unwrap_or_else(|_| {
                Err(EngineError::Runtime(format!(
                    "runtime `{}` lost its connection and did not come back within {}",
                    key.runtime_id,
                    cookie_agent_protocol::diagnostics::duration(RECONNECT_WAIT)
                )))
            })
    }

    /// What is recorded at `key` now.
    fn slot(&self, key: &InstanceKey) -> Option<InstanceSlot> {
        self.slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned()
    }

    /// Marks `key` starting and returns what was recorded there before.
    fn begin_start(&self, key: &InstanceKey) -> Previous {
        let previous = {
            let mut slots = self
                .slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match slots.insert(key.clone(), InstanceSlot::Starting) {
                Some(InstanceSlot::Failed { instance, .. }) => Previous {
                    recorded: true,
                    instance,
                },
                Some(_) => Previous {
                    recorded: true,
                    instance: None,
                },
                None => Previous {
                    recorded: false,
                    instance: None,
                },
            }
        };
        self.notify();
        previous
    }

    /// The token [`Self::disconnect`] cancels for `runtime_id`.
    fn stop_token(&self, runtime_id: &str) -> CancellationToken {
        self.stops
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(runtime_id.to_owned())
            .or_insert_with(|| self.shutdown.child_token())
            .clone()
    }

    /// Puts back what was recorded at `pending` before a start a client's
    /// disconnect overtook: the instance that ran there, stopped, or
    /// nothing.
    fn abandon_start(&self, pending: &InstanceKey, previous: Previous) {
        {
            let mut slots = self
                .slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(InstanceSlot::Starting) = slots.get(pending) {
                match previous.instance {
                    Some(instance) => {
                        slots.insert(pending.clone(), InstanceSlot::Stopped(instance));
                    }
                    None => {
                        slots.remove(pending);
                    }
                }
            }
        }
        self.notify();
    }

    /// Records a started instance under the canonical workspace it reported,
    /// or returns the running instance already there. An instance that ran
    /// there before and failed is revived under a new generation. Nothing is
    /// recorded once `stop` is cancelled: a client disconnected the runtime
    /// while it started.
    fn install(
        self: &Arc<Self>,
        requested: &InstanceKey,
        pending: &InstanceKey,
        started: StartedRuntime,
        previous: Option<Arc<RuntimeInstance>>,
        stop: &CancellationToken,
    ) -> Option<Arc<RuntimeInstance>> {
        let key = InstanceKey::of(&started.info);
        if let (Some(_), Some(canonical)) = (&requested.workspace, &key.workspace)
            && &key != requested
        {
            self.aliases
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(requested.clone(), canonical.clone());
        }
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Checked under the slots lock, which a disconnect takes after
        // cancelling: either the disconnect finds the instance installed and
        // stops it, or the instance is not installed.
        if stop.is_cancelled() {
            drop(slots);
            self.abandon_start(
                pending,
                Previous {
                    recorded: true,
                    instance: previous,
                },
            );
            return None;
        }
        let mut revive = None;
        if let Some(InstanceSlot::Starting) = slots.get(pending) {
            match previous {
                Some(instance) if pending != &key => {
                    // The workspace now resolves elsewhere; the instance that
                    // ran for the old one stays failed there.
                    let reason = format!(
                        "runtime `{}` now resolves to another workspace",
                        key.runtime_id
                    );
                    slots.insert(
                        pending.clone(),
                        InstanceSlot::Failed {
                            reason,
                            instance: Some(instance),
                        },
                    );
                }
                previous => {
                    revive = previous;
                    slots.remove(pending);
                }
            }
        }
        if let Some(existing) = slots.get(&key).and_then(InstanceSlot::published) {
            // Another spelling of a workspace that already has an instance;
            // the runtime's connection is shared, so it stays up.
            let existing = Arc::clone(existing);
            drop(slots);
            self.notify();
            return Some(existing);
        }
        let failed = revive.or_else(|| match slots.get(&key) {
            Some(InstanceSlot::Failed {
                instance: Some(instance),
                ..
            }) => Some(Arc::clone(instance)),
            _ => None,
        });
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        let instance = match failed.filter(|instance| instance.same_identity(&started.info)) {
            Some(instance) => {
                instance.restart(started, generation);
                instance
            }
            None => Arc::new(RuntimeInstance::new(started, generation)),
        };
        slots.insert(key, InstanceSlot::Ready(Arc::clone(&instance)));
        drop(slots);
        self.watch(&instance);
        self.notify();
        Some(instance)
    }

    /// The instance a recorded attachment refers to, started when it is not
    /// running, with the same rules as attaching it.
    pub(crate) async fn ensure_attachment(
        self: &Arc<Self>,
        attachment: &RuntimeAttachment,
    ) -> Result<Arc<RuntimeInstance>, EngineError> {
        if let Some(instance) = self.instance(attachment) {
            return Ok(instance);
        }
        self.open(
            attachment.runtime_id.as_str(),
            Some(attachment.capability.as_str()),
            attachment.workspace.as_deref().map(Path::new),
            Record::Always,
        )
        .await
    }

    /// Watches the built-in instances' connections; see [`Self::watch`].
    pub(crate) fn watch_builtin(self: &Arc<Self>) {
        self.watch(&self.global);
        self.watch(&self.local);
    }

    /// Reconnects `instance` when its current runtime protocol connection
    /// ends while it is ready (see [`Self::reconnect_lost`]).
    fn watch(self: &Arc<Self>, instance: &Arc<RuntimeInstance>) {
        let (generation, Some(connection)) = ({
            let current = instance.current();
            (current.generation, current.connection.clone())
        }) else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let closed = connection.client.closed();
        drop(connection);
        let registry = Arc::downgrade(self);
        let instance = Arc::downgrade(instance);
        handle.spawn(async move {
            let reason = closed.await;
            if let (Some(registry), Some(instance)) = (registry.upgrade(), instance.upgrade()) {
                registry.reconnect_lost(instance, generation, reason).await;
            }
        });
    }

    /// The connection of `instance` at `generation` ended. The instance
    /// becomes `reconnecting`: its tools stay published and calls fail fast
    /// with `runtime_unavailable` while the template restarts it with the
    /// reconnect policy. A restart revives the instance under a new
    /// generation, so its tools are listed again; when every attempt fails,
    /// its tools are dropped, it becomes `failed`, and clients are told.
    async fn reconnect_lost(
        self: Arc<Self>,
        instance: Arc<RuntimeInstance>,
        generation: u64,
        reason: String,
    ) {
        let key = InstanceKey::of(&instance.info);
        // Taken before the slot says reconnecting, so a disconnect that
        // stops the instance also ends these attempts.
        let stop = self.stop_token(&key.runtime_id);
        {
            let mut slots = self
                .slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match slots.get(&key) {
                Some(InstanceSlot::Ready(current))
                    if Arc::ptr_eq(current, &instance) && instance.generation() == generation => {}
                _ => return,
            }
            slots.insert(
                key.clone(),
                InstanceSlot::Reconnecting(Arc::clone(&instance)),
            );
        }
        self.notify();
        let template = self.template(&key.runtime_id);
        // A runtime that dialed in went away on its own; one the daemon
        // runs exited.
        let message = match &template {
            Some(template) if template.lifetime() == RuntimeLifetime::DialIn => {
                format!("runtime `{}` disconnected: {reason}", key.runtime_id)
            }
            _ => format!("runtime `{}` exited: {reason}", key.runtime_id),
        };
        let Some(template) = template else {
            self.fail_lost(&key, &instance, message);
            return;
        };
        let workspace = key.workspace.clone();
        let capability = instance.info.capability().clone();
        let policy = self
            .reconnect
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let outcome = policy
            .run(&stop, |_| {
                let template = Arc::clone(&template);
                let workspace = workspace.clone();
                let capability = capability.clone();
                let instance = Arc::clone(&instance);
                async move {
                    let started = template
                        .start(&capability, workspace.as_deref())
                        .await
                        .map_err(|error| error.to_string())?;
                    if instance.same_identity(&started.info) {
                        return Ok(started);
                    }
                    Err(format!(
                        "the runtime restarted as `{}`",
                        started.info.attachment_name()
                    ))
                }
            })
            .await;
        match outcome {
            ReconnectOutcome::Reconnected(started) => {
                let restarted = {
                    let mut slots = self
                        .slots
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    match slots.get(&key) {
                        Some(InstanceSlot::Reconnecting(current))
                            if Arc::ptr_eq(current, &instance) =>
                        {
                            let generation = self.generations.fetch_add(1, Ordering::Relaxed);
                            instance.restart(started, generation);
                            slots.insert(key, InstanceSlot::Ready(Arc::clone(&instance)));
                            None
                        }
                        _ => Some(started),
                    }
                };
                // Stopped meanwhile, the reopened instance is not needed;
                // the runtime's connection is shared, so it stays up.
                if restarted.is_none() {
                    self.watch(&instance);
                    self.notify();
                }
            }
            ReconnectOutcome::Exhausted {
                attempts,
                last_error,
            } => {
                let message =
                    format!("{message}; reconnecting failed {attempts} times, last: {last_error}");
                self.fail_lost(&key, &instance, message);
            }
            ReconnectOutcome::Cancelled => {}
        }
    }

    /// Records that a reconnecting instance failed for good: its tools are
    /// dropped and clients are told.
    fn fail_lost(&self, key: &InstanceKey, instance: &Arc<RuntimeInstance>, reason: String) {
        {
            let mut slots = self
                .slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match slots.get(key) {
                Some(InstanceSlot::Reconnecting(current)) if Arc::ptr_eq(current, instance) => {}
                _ => return,
            }
            slots.insert(
                key.clone(),
                InstanceSlot::Failed {
                    reason: reason.clone(),
                    instance: Some(Arc::clone(instance)),
                },
            );
        }
        drop(instance.drop_runtime());
        let _ = self.failures.send(RuntimeFailure {
            runtime_id: key.runtime_id.clone(),
            capability: key.capability.clone(),
            workspace: key
                .workspace
                .as_ref()
                .map(|workspace| workspace.to_string_lossy().into_owned()),
            reason,
        });
        self.notify();
    }

    /// Replaces the reconnect policy, for tests that cannot wait for the
    /// production backoff.
    #[cfg(test)]
    pub(crate) fn set_reconnect_policy(&self, policy: ReconnectPolicy) {
        *self
            .reconnect
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = policy;
    }

    /// Records a failed start and reports it to clients. An instance the
    /// registry had not recorded before is forgotten instead when `record`
    /// is [`Record::Known`]: nothing uses it. The instance that ran there
    /// before, if any, is kept for a later start to revive.
    fn fail_start(&self, key: InstanceKey, reason: String, previous: Previous, record: Record) {
        if record == Record::Known && !previous.recorded {
            self.slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&key);
            self.notify();
            return;
        }
        let failure = RuntimeFailure {
            runtime_id: key.runtime_id.clone(),
            capability: key.capability.clone(),
            workspace: key
                .workspace
                .as_ref()
                .map(|workspace| workspace.to_string_lossy().into_owned()),
            reason: reason.clone(),
        };
        self.set_slot(
            key,
            InstanceSlot::Failed {
                reason,
                instance: previous.instance,
            },
        );
        let _ = self.failures.send(failure);
    }

    /// Instance failures as they happen.
    pub(crate) fn subscribe_failures(&self) -> tokio::sync::broadcast::Receiver<RuntimeFailure> {
        self.failures.subscribe()
    }

    fn set_slot(&self, key: InstanceKey, slot: InstanceSlot) {
        self.slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, slot);
        self.notify();
    }

    fn notify(&self) {
        self.changes.send_modify(|revision| *revision += 1);
    }

    /// Signals every template or instance status change.
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub(crate) fn templates(&self) -> Vec<RuntimeTemplateSummary> {
        self.templates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|template| RuntimeTemplateSummary {
                runtime_id: template.runtime_id().to_owned(),
                lifetime: template.lifetime(),
                listing: template.listing(),
            })
            .collect()
    }

    /// Gives every runtime its settings from `config`, as applied now: a
    /// connected runtime that takes them gets the ones that changed.
    pub(crate) fn update_settings(&self, config: &cookie_agent_config::LoadedConfiguration) {
        for template in self.every_template() {
            template.update_settings(&super::runtime_settings(config, template.runtime_id()));
        }
    }

    /// Every registered runtime, dial-in ones included.
    fn every_template(&self) -> Vec<Arc<dyn RuntimeTemplate>> {
        let mut templates = self
            .templates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        templates.extend(
            self.dial_ins
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .values()
                .map(|template| Arc::clone(template) as Arc<dyn RuntimeTemplate>),
        );
        templates
    }

    /// What the daemon knows of the shell environment of every connected
    /// runtime, by runtime ID.
    pub(crate) fn shell_environments(&self) -> BTreeMap<String, RuntimeShellEnvironment> {
        self.every_template()
            .into_iter()
            .filter_map(|template| {
                let shell_environment = template.shell_environment()?;
                Some((template.runtime_id().to_owned(), shell_environment))
            })
            .collect()
    }

    /// Asks every connected runtime to capture the user's shell
    /// environment again.
    pub(crate) fn refresh_shell_environments(&self) {
        for template in self.every_template() {
            template.refresh_shell_environment();
        }
    }

    /// The ACP agents of every registered runtime, by runtime ID.
    pub(crate) fn acp_agents(&self) -> BTreeMap<String, RuntimeAcpAgents> {
        let templates = self
            .templates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        templates
            .into_iter()
            .map(|template| (template.runtime_id().to_owned(), template.acp_agents()))
            .filter(|(_, agents)| !agents.agents.is_empty())
            .collect()
    }

    /// Runtime `runtime_id`'s connection, for its ACP agents.
    pub(crate) async fn agent_connection(&self, runtime_id: &str) -> Result<RuntimeLink, String> {
        let template = self
            .template(runtime_id)
            .ok_or_else(|| format!("runtime `{runtime_id}` is not registered"))?;
        self.unpark(runtime_id);
        template.agent_connection().await
    }

    /// Whether a client disconnected `runtime_id` and nothing has used it
    /// since.
    pub(crate) fn is_parked(&self, runtime_id: &str) -> bool {
        self.parked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(runtime_id)
    }

    fn unpark(&self, runtime_id: &str) {
        self.parked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(runtime_id);
    }

    /// A runtime the daemon starts from a template, which clients may
    /// connect and disconnect by hand; the error says why `runtime_id` is
    /// not one.
    fn startable(&self, runtime_id: &str) -> Result<Arc<dyn RuntimeTemplate>, EngineError> {
        let template = self
            .template(runtime_id)
            .ok_or_else(|| EngineError::Runtime(format!("unknown runtime `{runtime_id}`")))?;
        match template.lifetime() {
            RuntimeLifetime::Spawned => Ok(template),
            RuntimeLifetime::Builtin => Err(EngineError::Runtime(format!(
                "runtime `{runtime_id}` is built into the daemon and is always connected"
            ))),
            RuntimeLifetime::DialIn => Err(EngineError::Runtime(format!(
                "runtime `{runtime_id}` dials in to the daemon: it connects and disconnects on its own"
            ))),
        }
    }

    /// Connects template runtime `runtime_id` without opening an instance,
    /// as `runtimes.connect` does: its process starts and answers
    /// `runtime/initialize`. A connected runtime is left as it is.
    pub(crate) async fn connect(&self, runtime_id: &str) -> Result<(), EngineError> {
        let template = self.startable(runtime_id)?;
        self.unpark(runtime_id);
        let result = template.capabilities().await;
        self.notify();
        result.map(drop).map_err(EngineError::Runtime)
    }

    /// Disconnects template runtime `runtime_id`, as `runtimes.disconnect`
    /// does: its instances stop, with no reconnect, and opening a session
    /// leaves it down until a run, attachment, new session, or delegation
    /// uses it. Returns the connections for the caller to shut down.
    pub(crate) fn disconnect(
        &self,
        runtime_id: &str,
    ) -> Result<Vec<super::remote::RuntimeClient>, EngineError> {
        let template = self.startable(runtime_id)?;
        self.parked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(runtime_id.to_owned());
        // Ends reconnect loops and keeps starts in flight from installing,
        // before the slots are stopped (see `install`).
        if let Some(stop) = self
            .stops
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(runtime_id)
        {
            stop.cancel();
        }
        let mut connections = Vec::new();
        {
            let mut slots = self
                .slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for (key, slot) in slots.iter_mut() {
                if key.runtime_id != runtime_id {
                    continue;
                }
                match slot {
                    InstanceSlot::Ready(instance) | InstanceSlot::Reconnecting(instance) => {
                        connections.extend(instance.clear());
                        *slot = InstanceSlot::Stopped(Arc::clone(instance));
                    }
                    InstanceSlot::Failed {
                        instance: Some(instance),
                        ..
                    } => {
                        connections.extend(instance.clear());
                        *slot = InstanceSlot::Stopped(Arc::clone(instance));
                    }
                    InstanceSlot::Starting
                    | InstanceSlot::Failed { instance: None, .. }
                    | InstanceSlot::Stopped(_) => {}
                }
            }
            // Instances that never opened have nothing to show.
            slots.retain(|key, slot| {
                key.runtime_id != runtime_id || !matches!(slot, InstanceSlot::Failed { .. })
            });
        }
        connections.extend(template.disconnect().map(|connection| connection.0));
        let mut epochs = std::collections::HashSet::new();
        connections.retain(|connection| epochs.insert(connection.epoch()));
        self.notify();
        Ok(connections)
    }

    /// How `runtimes.list` reports `instance`.
    pub(crate) fn summary(&self, instance: &RuntimeInstance) -> Option<RuntimeInstanceSummary> {
        let key = InstanceKey::of(&instance.info);
        let workspace = key
            .workspace
            .as_ref()
            .map(|workspace| workspace.to_string_lossy().into_owned());
        self.instances().into_iter().find(|summary| {
            summary.runtime_id == key.runtime_id
                && summary.capability == key.capability
                && summary.workspace == workspace
        })
    }

    pub(crate) fn instances(&self) -> Vec<RuntimeInstanceSummary> {
        self.slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(key, slot)| {
                let (instance, status, message) = match slot {
                    InstanceSlot::Starting => (None, RuntimeInstanceStatus::Starting, None),
                    InstanceSlot::Ready(instance) => {
                        (Some(instance), RuntimeInstanceStatus::Ready, None)
                    }
                    InstanceSlot::Reconnecting(instance) => {
                        (Some(instance), RuntimeInstanceStatus::Reconnecting, None)
                    }
                    InstanceSlot::Failed { reason, instance } => (
                        instance.as_ref(),
                        RuntimeInstanceStatus::Failed,
                        Some(reason.clone()),
                    ),
                    InstanceSlot::Stopped(instance) => {
                        (Some(instance), RuntimeInstanceStatus::Stopped, None)
                    }
                };
                RuntimeInstanceSummary {
                    runtime_id: key.runtime_id.clone(),
                    capability: key.capability.clone(),
                    attachment_name: instance
                        .map(|instance| instance.info.attachment_name().to_owned()),
                    kind: instance.map(|instance| instance.info.kind().clone()),
                    workspace: key
                        .workspace
                        .as_ref()
                        .map(|workspace| workspace.to_string_lossy().into_owned()),
                    platform: instance.and_then(|instance| {
                        instance
                            .info
                            .workspace()
                            .and_then(|workspace| workspace.platform().map(str::to_owned))
                    }),
                    status,
                    message,
                }
            })
            .collect()
    }

    /// Drops every provider (they may hold the engine) and marks the
    /// instances stopped. Returns the runtime protocol connections, for the
    /// caller to shut down; dropping them closes them.
    pub(crate) fn clear(&self) -> Vec<super::remote::RuntimeClient> {
        self.shutdown.cancel();
        self.base
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let mut connections = Vec::new();
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for slot in slots.values_mut() {
            match slot {
                InstanceSlot::Ready(instance) | InstanceSlot::Reconnecting(instance) => {
                    connections.extend(instance.clear());
                    *slot = InstanceSlot::Stopped(Arc::clone(instance));
                }
                InstanceSlot::Failed {
                    instance: Some(instance),
                    ..
                } => connections.extend(instance.clear()),
                InstanceSlot::Starting
                | InstanceSlot::Failed { instance: None, .. }
                | InstanceSlot::Stopped(_) => {}
            }
        }
        drop(slots);
        connections.extend(self.global.clear());
        connections.extend(self.local.clear());
        for template in self
            .templates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
        {
            connections.extend(template.disconnect().map(|connection| connection.0));
        }
        // Instances of one runtime share its connection: shut each down once.
        let mut epochs = std::collections::HashSet::new();
        connections.retain(|connection| epochs.insert(connection.epoch()));
        self.notify();
        connections
    }
}
