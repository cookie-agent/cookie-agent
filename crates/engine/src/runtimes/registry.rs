//! Runtime templates and the instances started from them.
//!
//! A [`RuntimeTemplate`] is a way to start runtimes; its ID is the *runtime
//! ID*. The [`RuntimeRegistry`] starts at most one [`RuntimeInstance`] per
//! (runtime ID, canonical workspace) and shares it with every session attached
//! to it. The `global` and `local` instances start with the engine; any other
//! instance starts when a session first needs it.
//!
//! Production registers only the built-in `global` and `local` templates.
//! Configured templates (`[runtimes.<id>]`, phase 4) and the runtime protocol
//! client (phase 3) plug in here as further [`RuntimeTemplate`]
//! implementations.

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

use crate::EngineError;

/// A way to start runtime instances: the built-in `global` and `local`
/// runtimes today, configured `[runtimes.<id>]` templates later.
#[async_trait]
pub trait RuntimeTemplate: Send + Sync {
    /// The runtime ID instances of this template carry.
    fn runtime_id(&self) -> &str;

    /// Whether starting an instance takes a workspace. Attaching such a
    /// template requires one; attaching any other template rejects one.
    fn takes_workspace(&self) -> bool;

    /// Starts one instance. `workspace` is `Some` exactly when
    /// [`Self::takes_workspace`] is true. The error is a human-readable
    /// reason.
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

/// A running runtime, shared by every session attached to it.
pub(crate) struct RuntimeInstance {
    info: Arc<RuntimeInfo>,
    /// Distinguishes this start of the instance from any earlier one, so a
    /// tool published before a restart fails as changed.
    generation: u64,
    providers: Mutex<Vec<Arc<dyn ToolProvider>>>,
    skills: Arc<SkillRegistry>,
}

impl RuntimeInstance {
    fn new(started: StartedRuntime, generation: u64) -> Self {
        Self {
            info: Arc::new(started.info),
            generation,
            providers: Mutex::new(started.providers),
            skills: Arc::new(started.skills),
        }
    }

    pub(crate) fn info(&self) -> &Arc<RuntimeInfo> {
        &self.info
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn origin(&self) -> ToolOrigin {
        ToolOrigin::Runtime(Arc::clone(&self.info))
    }

    /// The instance's providers in composition order.
    pub(crate) fn providers(&self) -> Vec<Arc<dyn ToolProvider>> {
        self.providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn push_provider(&self, provider: Arc<dyn ToolProvider>) {
        self.providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(provider);
    }

    pub(crate) fn skills(&self) -> &Arc<SkillRegistry> {
        &self.skills
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
            kind: self.info.kind(),
            workspace: self.workspace_string(),
        })
    }

    fn workspace_string(&self) -> Option<String> {
        self.info
            .workspace()
            .map(|workspace| workspace.root().to_string_lossy().into_owned())
    }

    fn clear(&self) {
        self.providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
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
    Failed(String),
    Stopped(Arc<RuntimeInstance>),
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
    /// Serializes instance starts so one instance never starts twice.
    start: tokio::sync::Mutex<()>,
    generations: AtomicU64,
    global: Arc<RuntimeInstance>,
    local: Arc<RuntimeInstance>,
    changes: tokio::sync::watch::Sender<u64>,
    /// Instance failures, for clients viewing attached sessions.
    failures: tokio::sync::broadcast::Sender<RuntimeFailure>,
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
            start: tokio::sync::Mutex::new(()),
            generations: AtomicU64::new(3),
            global,
            local,
            changes: tokio::sync::watch::channel(0).0,
            failures: tokio::sync::broadcast::channel(64).0,
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
        match slots.get(&key) {
            Some(InstanceSlot::Ready(instance)) => Some(Arc::clone(instance)),
            _ => None,
        }
    }

    /// The running instance for `runtime_id` at `workspace`, if it is up.
    fn ready(&self, runtime_id: &str, workspace: Option<&Path>) -> Option<Arc<RuntimeInstance>> {
        let slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slots.iter().find_map(|(key, slot)| match slot {
            InstanceSlot::Ready(instance)
                if key.runtime_id == runtime_id
                    && workspace
                        .is_none_or(|workspace| key.workspace.as_deref() == Some(workspace)) =>
            {
                Some(Arc::clone(instance))
            }
            _ => None,
        })
    }

    /// The instance of `runtime_id` for `workspace`, started when it is not
    /// running. `workspace` must be given exactly when the template takes
    /// one; an unknown runtime ID is an error.
    pub(crate) async fn ensure(
        &self,
        runtime_id: &str,
        workspace: Option<&Path>,
    ) -> Result<Arc<RuntimeInstance>, EngineError> {
        let template = self
            .template(runtime_id)
            .ok_or_else(|| EngineError::Runtime(format!("unknown runtime `{runtime_id}`")))?;
        let workspace = match (template.takes_workspace(), workspace) {
            (true, None) => {
                return Err(EngineError::Runtime(format!(
                    "runtime `{runtime_id}` requires a workspace"
                )));
            }
            (false, Some(_)) => {
                return Err(EngineError::Runtime(format!(
                    "runtime `{runtime_id}` takes no workspace"
                )));
            }
            (true, Some(workspace)) => Some(workspace.canonicalize().map_err(|error| {
                EngineError::Runtime(format!(
                    "workspace {} for runtime `{runtime_id}` is unavailable: {error}",
                    workspace.display()
                ))
            })?),
            (false, None) => None,
        };
        if let Some(instance) = self.ready(runtime_id, workspace.as_deref()) {
            return Ok(instance);
        }
        let _start = self.start.lock().await;
        if let Some(instance) = self.ready(runtime_id, workspace.as_deref()) {
            return Ok(instance);
        }
        let pending = InstanceKey {
            runtime_id: runtime_id.to_owned(),
            workspace: workspace.clone(),
        };
        self.set_slot(pending.clone(), InstanceSlot::Starting);
        match template.start(workspace.as_deref()).await {
            Ok(started) if started.info.id() == runtime_id => {
                let generation = self.generations.fetch_add(1, Ordering::Relaxed);
                let instance = Arc::new(RuntimeInstance::new(started, generation));
                let key = InstanceKey::of(&instance.info);
                {
                    let mut slots = self
                        .slots
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    slots.remove(&pending);
                    slots.insert(key, InstanceSlot::Ready(Arc::clone(&instance)));
                }
                self.notify();
                Ok(instance)
            }
            Ok(started) => {
                let message = format!(
                    "runtime `{runtime_id}` reported runtime ID `{}`",
                    started.info.id()
                );
                self.fail_start(pending, message.clone());
                Err(EngineError::Runtime(message))
            }
            Err(reason) => {
                let message = format!("runtime `{runtime_id}` failed to start: {reason}");
                self.fail_start(pending, message.clone());
                Err(EngineError::Runtime(message))
            }
        }
    }

    /// The instance a recorded attachment refers to, started when it is not
    /// running, with the same rules as attaching it.
    pub(crate) async fn ensure_attachment(
        &self,
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

    /// Records a failed start and reports it to clients.
    fn fail_start(&self, key: InstanceKey, reason: String) {
        let failure = RuntimeFailure {
            runtime_id: key.runtime_id.clone(),
            workspace: key
                .workspace
                .as_ref()
                .map(|workspace| workspace.to_string_lossy().into_owned()),
            reason: reason.clone(),
        };
        self.set_slot(key, InstanceSlot::Failed(reason));
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
                    InstanceSlot::Failed(message) => {
                        (None, RuntimeInstanceStatus::Failed, Some(message.clone()))
                    }
                    InstanceSlot::Stopped(instance) => {
                        (Some(instance), RuntimeInstanceStatus::Stopped, None)
                    }
                };
                RuntimeInstanceSummary {
                    runtime_id: key.runtime_id.clone(),
                    attachment_name: instance
                        .map(|instance| instance.info.attachment_name().to_owned()),
                    kind: instance.map(|instance| instance.info.kind()),
                    workspace: key
                        .workspace
                        .as_ref()
                        .map(|workspace| workspace.to_string_lossy().into_owned()),
                    platform: instance.and_then(|instance| {
                        instance
                            .info
                            .workspace()
                            .map(|workspace| workspace.platform().to_owned())
                    }),
                    status,
                    message,
                }
            })
            .collect()
    }

    /// Drops every provider (they may hold the engine) and marks the
    /// instances stopped.
    pub(crate) fn clear(&self) {
        self.base
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for slot in slots.values_mut() {
            if let InstanceSlot::Ready(instance) = slot {
                instance.clear();
                *slot = InstanceSlot::Stopped(Arc::clone(instance));
            }
        }
        drop(slots);
        self.global.clear();
        self.local.clear();
        self.notify();
    }
}
