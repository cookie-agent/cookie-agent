//! Machines as the engine serves them: each runtime's machine, the
//! `machines.*` changes, and the assignment of runtimes as they connect
//! (`docs/site/specs/memory.md` section 3).

use std::sync::Arc;

use cookie_agent_protocol::{
    MachineInfo, MachineName, MachineRuntime, RememberedRuntime, RuntimeId, RuntimeLifetime,
    runtime_protocol::RuntimeMachine,
};

use super::{Engine, EngineError};
use crate::machines::{Assignment, MachineError, MachineRegistry, ObservedRuntime, RecordKind};

/// A runtime the daemon holds a connection to now.
struct ConnectedRuntime {
    runtime_id: RuntimeId,
    reported: Option<RuntimeMachine>,
    platform: Option<String>,
}

impl Engine {
    /// The runtimes connected now, with the machine and platform they
    /// reported.
    fn connected_runtimes(&self) -> Vec<ConnectedRuntime> {
        self.inner
            .runtimes
            .templates()
            .into_iter()
            .filter(|template| template.listing.connected)
            .filter_map(|template| {
                Some(ConnectedRuntime {
                    runtime_id: RuntimeId::new(template.runtime_id).ok()?,
                    reported: template.listing.machine,
                    platform: template.listing.platform,
                })
            })
            .collect()
    }

    /// The machine of a connected runtime, created when no machine claims
    /// it; `None` when the registry cannot be written.
    fn resolve_machine(
        &self,
        runtime_id: &RuntimeId,
        reported: Option<&RuntimeMachine>,
    ) -> Option<Assignment> {
        self.inner
            .machines
            .resolve(runtime_id, reported)
            .inspect_err(|error| {
                eprintln!("cookie-agent: runtime `{runtime_id}` has no machine: {error}");
            })
            .ok()
    }

    /// The machine `runtime_id` belongs to: the one it is assigned to while
    /// it is connected, else the one it is pinned to or the daemon last saw
    /// it on (`docs/site/specs/memory.md` section 16.8). `None` for an
    /// unknown runtime, or one the daemon cannot place yet.
    #[must_use]
    pub fn runtime_machine(&self, runtime_id: &str) -> Option<MachineName> {
        let runtime = RuntimeId::new(runtime_id).ok()?;
        let (connected, reported) = self
            .inner
            .runtimes
            .template_listing(runtime_id)
            .map_or((false, None), |listing| {
                (listing.connected, listing.machine)
            });
        let found = if connected {
            self.resolve_machine(&runtime, reported.as_ref())
        } else {
            self.inner
                .machines
                .lookup_offline(&runtime, reported.as_ref())
        };
        found.map(|assignment| assignment.machine)
    }

    /// What the registry remembers of `runtime_id` from when it last
    /// connected: its platform and home, for a runtime this process has
    /// not seen connected.
    #[must_use]
    pub fn remembered_runtime(&self, runtime_id: &str) -> Option<RememberedRuntimeFacts> {
        let runtime = RuntimeId::new(runtime_id).ok()?;
        self.inner
            .machines
            .record(&runtime)
            .map(|record| RememberedRuntimeFacts {
                platform: record.platform,
                home: record.home,
                last_seen: record.last_seen,
            })
    }

    /// Every machine of the registry, in name order, with the connected
    /// runtimes assigned to it and the runtimes it remembers that are not
    /// connected.
    #[must_use]
    pub fn machines(&self) -> Vec<MachineInfo> {
        let connected = self
            .connected_runtimes()
            .into_iter()
            .filter_map(|runtime| {
                let assignment =
                    self.resolve_machine(&runtime.runtime_id, runtime.reported.as_ref())?;
                Some((assignment, runtime))
            })
            .collect::<Vec<_>>();
        let registry = self.inner.machines.registry();
        let remembered = registry
            .records
            .iter()
            .filter(|record| {
                !connected
                    .iter()
                    .any(|(_, runtime)| runtime.runtime_id == record.runtime)
            })
            .filter_map(|record| {
                let assignment =
                    crate::machines::offline_assignment(&registry, &record.runtime, None)?;
                Some((assignment, record))
            })
            .collect::<Vec<_>>();
        registry
            .machines
            .into_iter()
            .map(|machine| {
                let runtimes = connected
                    .iter()
                    .filter(|(assignment, _)| assignment.machine == machine.name)
                    .map(|(assignment, runtime)| MachineRuntime {
                        runtime_id: runtime.runtime_id.clone(),
                        assigned_by: assignment.by,
                        platform: runtime.platform.clone(),
                    })
                    .collect::<Vec<_>>();
                let remembered = remembered
                    .iter()
                    .filter(|(assignment, _)| assignment.machine == machine.name)
                    .map(|(assignment, record)| RememberedRuntime {
                        runtime_id: record.runtime.clone(),
                        assigned_by: assignment.by,
                        platform: record.platform.clone(),
                        home: record.home.clone(),
                        last_seen: record.last_seen,
                    })
                    .collect::<Vec<_>>();
                MachineInfo {
                    remembered,
                    memories: self
                        .inner
                        .machines
                        .memory()
                        .machine_memory_count(&machine.name) as u64,
                    platform: shared_platform(runtimes.iter().map(|runtime| &runtime.platform)),
                    name: machine.name,
                    ids: machine.ids,
                    pinned: machine.runtimes,
                    connected: runtimes,
                }
            })
            .collect()
    }

    /// Signals every change to the machines.
    #[must_use]
    pub fn subscribe_machine_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.machines.subscribe()
    }

    /// Renames `machine` to `name`, moving its memory folder.
    pub async fn rename_machine(
        &self,
        machine: MachineName,
        name: MachineName,
    ) -> Result<(), EngineError> {
        self.change_machines(move |machines| machines.rename(&machine, &name))
            .await
    }

    /// Pins `runtime` to `machine`, created when the name is new.
    pub async fn assign_machine(
        &self,
        runtime: RuntimeId,
        machine: MachineName,
    ) -> Result<(), EngineError> {
        self.change_machines(move |machines| machines.assign(&runtime, &machine))
            .await
    }

    /// Removes `runtime`'s pin.
    pub async fn unassign_machine(&self, runtime: RuntimeId) -> Result<(), EngineError> {
        self.change_machines(move |machines| machines.unassign(&runtime))
            .await
    }

    /// Merges `from` into `into`, memories included.
    pub async fn merge_machines(
        &self,
        from: MachineName,
        into: MachineName,
    ) -> Result<(), EngineError> {
        self.change_machines(move |machines| machines.merge(&from, &into))
            .await
    }

    /// Drops the record of `runtime`, which must not be connected.
    pub async fn forget_runtime_record(&self, runtime: RuntimeId) -> Result<(), EngineError> {
        let connected = self
            .connected_runtimes()
            .iter()
            .any(|known| known.runtime_id == runtime);
        self.change_machines(move |machines| machines.forget_runtime(&runtime, connected))
            .await
    }

    /// Drops the records of configured runtimes that `config` no longer
    /// configures (`docs/site/specs/memory.md` section 16.8).
    pub(crate) async fn drop_unconfigured_runtime_records(
        &self,
        config: &cookie_agent_config::LoadedConfiguration,
    ) {
        let configured = config
            .runtimes
            .iter()
            .filter(|(_, template)| template.daemon_starts())
            .map(|(runtime, _)| runtime.as_str().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        let machines = Arc::clone(&self.inner.machines);
        let dropped = tokio::task::spawn_blocking(move || {
            machines.drop_records(|record| {
                record.kind == RecordKind::Spawned && !configured.contains(record.runtime.as_str())
            })
        })
        .await;
        if let Ok(Err(error)) = dropped {
            eprintln!("cookie-agent: runtime records not updated: {error}");
        }
    }

    /// Removes `machine`, which must have no connected runtimes and no
    /// memories.
    pub async fn forget_machine(&self, machine: MachineName) -> Result<(), EngineError> {
        let connected = self
            .connected_runtimes()
            .into_iter()
            .map(|runtime| (runtime.runtime_id, runtime.reported))
            .collect::<Vec<_>>();
        self.change_machines(move |machines| machines.forget(&machine, &connected))
            .await
    }

    /// Runs a registry change off the async workers: it waits for the
    /// memory lock and moves folders. A change re-places every connected
    /// runtime, so one it left without a machine gets a new one.
    async fn change_machines(
        &self,
        change: impl FnOnce(&MachineRegistry) -> Result<(), MachineError> + Send + 'static,
    ) -> Result<(), EngineError> {
        let machines = Arc::clone(&self.inner.machines);
        tokio::task::spawn_blocking(move || change(&machines))
            .await
            .map_err(|error| {
                MachineError::Store(format!("the change did not finish: {error}"))
            })??;
        self.assign_connected_runtimes();
        Ok(())
    }

    /// Places every connected runtime on a machine, writing the machines
    /// the registry does not have yet, and records where each spawned or
    /// dial-in runtime runs (`docs/site/specs/memory.md` section 16.8).
    pub(crate) fn assign_connected_runtimes(&self) {
        for runtime in self.connected_runtimes() {
            self.resolve_machine(&runtime.runtime_id, runtime.reported.as_ref());
        }
        let observed = self
            .inner
            .runtimes
            .templates()
            .into_iter()
            .filter_map(|template| {
                let kind = match template.lifetime {
                    RuntimeLifetime::Builtin => return None,
                    RuntimeLifetime::DialIn => RecordKind::DialIn,
                    RuntimeLifetime::Spawned => RecordKind::Spawned,
                };
                Some(ObservedRuntime {
                    runtime: RuntimeId::new(template.runtime_id).ok()?,
                    kind,
                    connected: template.listing.connected,
                    reported: template.listing.machine,
                    platform: template.listing.platform,
                    home: template.listing.home,
                })
            })
            .collect::<Vec<_>>();
        if let Err(error) = self
            .inner
            .machines
            .observe(&observed, jiff::Timestamp::now())
        {
            eprintln!("cookie-agent: runtime records not updated: {error}");
        }
    }

    /// Places the built-in runtimes now and every runtime as it connects,
    /// after dropping the records of templates the configuration no longer
    /// has.
    pub(super) fn install_machine_assignment(&self) {
        let configured = self
            .inner
            .config
            .load()
            .runtimes
            .keys()
            .map(|runtime| runtime.as_str().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        if let Err(error) = self.inner.machines.drop_records(|record| {
            record.kind == RecordKind::Spawned && !configured.contains(record.runtime.as_str())
        }) {
            eprintln!("cookie-agent: runtime records not updated: {error}");
        }
        self.assign_connected_runtimes();
        let Some(runtime) = self.inner.runtime.clone() else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        let mut changes = self.inner.runtimes.subscribe();
        changes.mark_unchanged();
        self.spawn_admission_task(&runtime, async move {
            while changes.changed().await.is_ok() {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let engine = Engine { inner };
                let _ =
                    tokio::task::spawn_blocking(move || engine.assign_connected_runtimes()).await;
            }
        });
    }
}

/// What the registry remembers of a runtime from when it last connected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RememberedRuntimeFacts {
    pub platform: Option<String>,
    pub home: Option<String>,
    pub last_seen: jiff::Timestamp,
}

/// The platform every runtime with one reports, when they agree.
fn shared_platform<'a>(platforms: impl Iterator<Item = &'a Option<String>>) -> Option<String> {
    let mut shared: Option<&String> = None;
    for platform in platforms.flatten() {
        match shared {
            Some(known) if known != platform => return None,
            _ => shared = Some(platform),
        }
    }
    shared.cloned()
}
