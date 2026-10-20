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

    /// Lets go of the template's connection as the daemon stops; the
    /// daemon shuts it down through the instances that use it.
    fn disconnect(&self) {}
}

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
}

impl CurrentStart {
    fn new(started: StartedRuntime, generation: u64) -> Self {
        Self {
            generation,
            providers: started.providers,
            skills: Arc::new(started.skills),
            connection: started.connection,
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
        } = started;
        Self {
            info: Arc::new(info),
            current: Mutex::new(CurrentStart {
                generation,
                providers,
                skills: Arc::new(skills),
                connection,
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

    pub(crate) fn push_provider(&self, provider: Arc<dyn ToolProvider>) {
        self.added
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(provider);
    }

    pub(crate) fn skills(&self) -> Arc<SkillRegistry> {
        Arc::clone(&self.current().skills)
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
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let reason = closed.await;
                if let Some(template) = template.upgrade() {
                    template.disconnected(epoch, &reason);
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
                    EngineError::Runtime(format!(
                        "runtime `{runtime_id}` failed to start: {reason}"
                    )),
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
            return Ok(instance);
        }
        let _start = self.start.lock().await;
        if let Some(instance) = self.live(&requested) {
            return Ok(instance);
        }
        let pending = self.resolve(&requested);
        let previous = self.begin_start(&pending);
        match template.start(&offered.name, workspace).await {
            Ok(started)
                if started.info.id() == runtime_id
                    && started.info.capability() == &offered.name =>
            {
                Ok(self.install(&requested, &pending, started, previous.instance))
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
                    StartError::Connect(reason) => {
                        format!("runtime `{runtime_id}` failed to start: {reason}")
                    }
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

    /// Records a started instance under the canonical workspace it reported,
    /// or returns the running instance already there. An instance that ran
    /// there before and failed is revived under a new generation.
    fn install(
        self: &Arc<Self>,
        requested: &InstanceKey,
        pending: &InstanceKey,
        started: StartedRuntime,
        previous: Option<Arc<RuntimeInstance>>,
    ) -> Arc<RuntimeInstance> {
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
            return existing;
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
        instance
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
            .run(&self.shutdown, |_| {
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
            template.disconnect();
        }
        // Instances of one runtime share its connection: shut each down once.
        let mut epochs = std::collections::HashSet::new();
        connections.retain(|connection| epochs.insert(connection.epoch()));
        self.notify();
        connections
    }
}
