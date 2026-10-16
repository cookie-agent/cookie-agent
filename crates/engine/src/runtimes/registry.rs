//! Runtime templates and the instances started from them.
//!
//! A [`RuntimeTemplate`] is a way to start runtimes; its ID is the *runtime
//! ID*. The [`RuntimeRegistry`] starts at most one [`RuntimeInstance`] per
//! (runtime ID, canonical workspace) and shares it with every session attached
//! to it. The `global` and `local` instances start with the engine; any other
//! instance starts when a session first needs it.
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
use cookie_agent_protocol::{RuntimeAttachment, RuntimeKind};
use cookie_agent_runtime_api::{RuntimeInfo, ToolOrigin, ToolProvider};

use tokio_util::sync::CancellationToken;

use super::{ReconnectOutcome, ReconnectPolicy};
use crate::EngineError;

/// A way to start runtime instances: the built-in `global` and `local`
/// runtimes and the configured `[runtimes.<id>]` templates.
#[async_trait]
pub trait RuntimeTemplate: Send + Sync {
    /// The runtime ID instances of this template carry.
    fn runtime_id(&self) -> &str;

    /// Whether starting an instance takes a workspace. Attaching such a
    /// template requires one; attaching any other template rejects one.
    fn takes_workspace(&self) -> bool;

    /// Starts one instance. `workspace` is `Some` exactly when
    /// [`Self::takes_workspace`] is true, and is the path a session asked
    /// for, exactly as given: the runtime resolves it and reports the
    /// canonical workspace. The error is a human-readable reason.
    async fn start(&self, workspace: Option<&Path>) -> Result<StartedRuntime, String>;
}

/// What a started runtime reports about itself.
pub struct StartedRuntime {
    /// Identity, kind, attachment name, and canonical workspace.
    pub info: RuntimeInfo,
    /// Tool providers, in composition order.
    pub providers: Vec<Arc<dyn ToolProvider>>,
    /// Skills the runtime contributes: user skills for the global runtime,
    /// project skills found for its workspace for a coding runtime. Read once
    /// at start.
    pub skills: SkillRegistry,
    /// The runtime protocol connection of an instance that runs behind it;
    /// `None` for an in-process runtime.
    pub(crate) connection: Option<super::remote::RuntimeClient>,
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
    connection: Option<super::remote::RuntimeClient>,
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

    pub(crate) fn connection(&self) -> Option<super::remote::RuntimeClient> {
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
    /// the same runtime, kind, attachment name, and workspace.
    fn same_identity(&self, info: &RuntimeInfo) -> bool {
        self.info.id() == info.id()
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
            kind: self.info.kind().clone(),
            workspace: self.workspace_string(),
        })
    }

    fn workspace_string(&self) -> Option<String> {
        self.info
            .workspace()
            .map(|workspace| workspace.root().to_string_lossy().into_owned())
    }

    /// Drops what the runtime declared and its protocol connection, which
    /// closes it, as when it failed for good.
    fn drop_runtime(&self) -> Option<super::remote::RuntimeClient> {
        let mut current = self.current();
        current.providers.clear();
        current.connection.take()
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
    workspace: Option<PathBuf>,
}

impl InstanceKey {
    fn of(info: &RuntimeInfo) -> Self {
        Self {
            runtime_id: info.id().to_owned(),
            workspace: info
                .workspace()
                .map(|workspace| workspace.root().to_owned()),
        }
    }
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
    /// The workspace the instance was started for, when its template takes
    /// one.
    pub workspace: Option<String>,
    pub reason: String,
}

/// A template as `runtimes.list` reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeTemplateSummary {
    pub runtime_id: String,
    pub takes_workspace: bool,
}

/// An instance as `runtimes.list` reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeInstanceSummary {
    pub runtime_id: String,
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

    /// The running instance of `runtime_id` whose canonical workspace is
    /// `workspace`, if it is up.
    pub(crate) fn instance_at(
        &self,
        runtime_id: &str,
        workspace: Option<&str>,
    ) -> Option<Arc<RuntimeInstance>> {
        let key = InstanceKey {
            runtime_id: runtime_id.to_owned(),
            workspace: workspace.map(PathBuf::from),
        };
        self.slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .and_then(InstanceSlot::published)
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
                    workspace: Some(workspace.clone()),
                },
            )
    }

    /// The instance of `runtime_id` for `workspace`, started when it is not
    /// running. `workspace` must be given exactly when the template takes
    /// one; an unknown runtime ID is an error.
    ///
    /// The workspace is passed to the runtime exactly as given: it may live
    /// on another machine, so the daemon never resolves it. The instance is
    /// keyed by the canonical workspace the runtime reports, so two spellings
    /// of one workspace share an instance; a second start that resolves to a
    /// running instance is stopped again.
    pub(crate) async fn ensure(
        self: &Arc<Self>,
        runtime_id: &str,
        workspace: Option<&Path>,
    ) -> Result<Arc<RuntimeInstance>, EngineError> {
        let template = self
            .template(runtime_id)
            .ok_or_else(|| EngineError::Runtime(format!("unknown runtime `{runtime_id}`")))?;
        match (template.takes_workspace(), workspace) {
            (true, None) => {
                return Err(EngineError::Runtime(format!(
                    "runtime `{runtime_id}` requires a workspace"
                )));
            }
            (true, Some(workspace)) if workspace.as_os_str().is_empty() => {
                return Err(EngineError::Runtime(format!(
                    "runtime `{runtime_id}` requires a workspace"
                )));
            }
            (false, Some(_)) => {
                return Err(EngineError::Runtime(format!(
                    "runtime `{runtime_id}` takes no workspace"
                )));
            }
            _ => {}
        }
        let requested = InstanceKey {
            runtime_id: runtime_id.to_owned(),
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
        match template.start(workspace).await {
            Ok(started) if started.info.id() == runtime_id => {
                Ok(self.install(&requested, &pending, started, previous))
            }
            Ok(started) => {
                let message = format!(
                    "runtime `{runtime_id}` reported runtime ID `{}`",
                    started.info.id()
                );
                self.fail_start(pending, message.clone(), previous);
                Err(EngineError::Runtime(message))
            }
            Err(reason) => {
                let message = format!("runtime `{runtime_id}` failed to start: {reason}");
                self.fail_start(pending, message.clone(), previous);
                Err(EngineError::Runtime(message))
            }
        }
    }

    /// Marks `key` starting and returns the instance that ran there before
    /// it failed, if any, for the start to revive.
    fn begin_start(&self, key: &InstanceKey) -> Option<Arc<RuntimeInstance>> {
        let previous = {
            let mut slots = self
                .slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match slots.insert(key.clone(), InstanceSlot::Starting) {
                Some(InstanceSlot::Failed { instance, .. }) => instance,
                _ => None,
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
            // Another spelling of a workspace that already has an instance.
            let existing = Arc::clone(existing);
            drop(slots);
            if let Some(connection) = started.connection {
                tokio::spawn(async move { connection.shutdown().await });
            }
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
        let takes_workspace = self
            .template(attachment.runtime_id.as_str())
            .is_some_and(|template| template.takes_workspace());
        let workspace = takes_workspace
            .then(|| attachment.workspace.as_deref().map(Path::new))
            .flatten();
        self.ensure(attachment.runtime_id.as_str(), workspace).await
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
        let closed = connection.closed();
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
        let message = format!("runtime `{}` exited: {reason}", key.runtime_id);
        let Some(template) = self.template(&key.runtime_id) else {
            self.fail_lost(&key, &instance, message);
            return;
        };
        let workspace = key.workspace.clone().filter(|_| template.takes_workspace());
        let policy = self
            .reconnect
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let outcome = policy
            .run(&self.shutdown, |_| {
                let template = Arc::clone(&template);
                let workspace = workspace.clone();
                let instance = Arc::clone(&instance);
                async move {
                    let started = template.start(workspace.as_deref()).await?;
                    if instance.same_identity(&started.info) {
                        return Ok(started);
                    }
                    let restarted = format!(
                        "the runtime restarted as `{}`",
                        started.info.attachment_name()
                    );
                    if let Some(connection) = started.connection {
                        connection.shutdown().await;
                    }
                    Err(restarted)
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
                match restarted {
                    // Stopped meanwhile: the new connection is not needed.
                    Some(started) => {
                        if let Some(connection) = started.connection {
                            connection.shutdown().await;
                        }
                    }
                    None => {
                        self.watch(&instance);
                        self.notify();
                    }
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

    /// Records a failed start and reports it to clients. `previous` is the
    /// instance that ran there before, which a later start still revives.
    fn fail_start(&self, key: InstanceKey, reason: String, previous: Option<Arc<RuntimeInstance>>) {
        let failure = RuntimeFailure {
            runtime_id: key.runtime_id.clone(),
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
                instance: previous,
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
                takes_workspace: template.takes_workspace(),
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
        self.instances()
            .into_iter()
            .find(|summary| summary.runtime_id == key.runtime_id && summary.workspace == workspace)
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
        self.notify();
        connections
    }
}
