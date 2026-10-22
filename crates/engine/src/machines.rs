//! The machine registry (`docs/site/specs/memory.md` section 3): the
//! computers runtimes run on, kept in `<data_root>/machines.json`.
//!
//! A runtime's machine is never stored with the runtime: it follows from
//! the registry and what the runtime reported whenever it is asked for
//! ([`assignment`]), so a change to the registry moves every connected
//! runtime it affects at once. Only a runtime that no machine claims makes
//! the registry grow, by one new machine written at once.
//!
//! The registry also remembers where each runtime ran (section 16.8): the
//! machine ID, platform, and home a spawned or dial-in runtime reported
//! when it last connected, and when the daemon last saw it. A runtime that
//! is not connected is placed by that record, so it keeps its machine while
//! it is offline and across restarts.
//!
//! Every change holds the memory lock ([`crate::memory::MemoryLock`]) while
//! it rereads the file, rewrites it, and moves machine memory folders, so
//! two daemons sharing a data root, or a memory write, never race it.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_protocol::{
    MachineAssignment, MachineName, RuntimeId, runtime_protocol::RuntimeMachine,
};
use serde::{Deserialize, Serialize};

use crate::memory::{FolderMoves, MemoryError, MemoryLock, MemoryRoot};

#[cfg(test)]
mod tests;

/// The registry's file in the data root.
pub(crate) const REGISTRY_FILE: &str = "machines.json";
/// The registry format this build writes.
const REGISTRY_VERSION: u32 = 2;
/// The format before runtime records, which this build reads with none.
const RECORDLESS_VERSION: u32 = 1;
/// The name of a machine whose host name leaves nothing usable.
const UNNAMED_HOST: &str = "machine";

/// `machines.json`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    version: u32,
    machines: Vec<Machine>,
    /// What the registry remembers of each runtime it saw connected, in
    /// runtime ID order. Version 1 has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    remembered: Vec<RuntimeRecord>,
}

/// The registry's contents: the machines, in name order, and the runtime
/// records, in runtime ID order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Registry {
    pub(crate) machines: Vec<Machine>,
    pub(crate) records: Vec<RuntimeRecord>,
}

impl Registry {
    fn sort(&mut self) {
        self.machines
            .sort_by(|left, right| left.name.cmp(&right.name));
        self.records
            .sort_by(|left, right| left.runtime.cmp(&right.runtime));
    }

    pub(crate) fn record(&self, runtime: &RuntimeId) -> Option<&RuntimeRecord> {
        self.records
            .iter()
            .find(|record| record.runtime == *runtime)
    }
}

/// How the daemon reached a runtime it remembers. A configured template's
/// record goes when the template leaves the configuration; a dial-in's
/// stays until it is forgotten.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordKind {
    Spawned,
    DialIn,
}

/// What the registry remembers of a runtime it saw connected.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeRecord {
    pub(crate) runtime: RuntimeId,
    pub(crate) kind: RecordKind,
    /// The machine ID it reported; absent when it reported none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) machine_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) home: Option<String>,
    /// When the daemon last saw it connect or disconnect.
    pub(crate) last_seen: jiff::Timestamp,
}

impl RuntimeRecord {
    /// Whether `other` records the same report, whenever it was seen.
    fn same_report(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.machine_id == other.machine_id
            && self.platform == other.platform
            && self.home == other.home
    }

    /// The machine the record says the runtime reported.
    pub(crate) fn reported(&self) -> Option<RuntimeMachine> {
        self.machine_id.clone().map(|id| RuntimeMachine {
            id,
            host_name: None,
        })
    }
}

/// A runtime as the daemon sees it now, for [`MachineRegistry::observe`].
#[derive(Clone, Debug)]
pub(crate) struct ObservedRuntime {
    pub(crate) runtime: RuntimeId,
    pub(crate) kind: RecordKind,
    pub(crate) connected: bool,
    pub(crate) reported: Option<RuntimeMachine>,
    pub(crate) platform: Option<String>,
    pub(crate) home: Option<String>,
}

/// One machine of the registry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Machine {
    pub(crate) name: MachineName,
    /// The machine IDs the machine stands for.
    pub(crate) ids: Vec<String>,
    /// Runtime IDs pinned to the machine whatever they report.
    pub(crate) runtimes: Vec<RuntimeId>,
}

/// The machine a runtime belongs to, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Assignment {
    pub(crate) machine: MachineName,
    pub(crate) by: MachineAssignment,
}

/// Why a machine change was refused.
#[derive(Debug, thiserror::Error)]
pub enum MachineError {
    #[error("there is no machine named `{0}`")]
    NotFound(MachineName),
    #[error("the machine name `{0}` is taken")]
    NameTaken(MachineName),
    #[error("machine `{0}` still has connected runtimes or memories")]
    InUse(MachineName),
    #[error("both `{from}` and `{into}` have a memory file named `{name}`")]
    MemoryConflict {
        from: MachineName,
        into: MachineName,
        name: String,
    },
    #[error("machine `{0}` cannot be merged into itself")]
    MergeSelf(MachineName),
    #[error("runtime `{0}` is not pinned to a machine")]
    NotPinned(RuntimeId),
    #[error("the daemon remembers no machine for runtime `{0}`")]
    NotRemembered(RuntimeId),
    #[error("runtime `{0}` is connected")]
    StillConnected(RuntimeId),
    /// Reading or writing the registry or a memory folder failed.
    #[error("{0}")]
    Store(String),
}

impl From<MemoryError> for MachineError {
    fn from(error: MemoryError) -> Self {
        Self::Store(error.to_string())
    }
}

/// Rule 1 and 2 of the assignment order: the machine pinning `runtime`,
/// else the machine standing for the machine ID it reported.
pub(crate) fn assignment(
    machines: &[Machine],
    runtime: &RuntimeId,
    reported: Option<&RuntimeMachine>,
) -> Option<Assignment> {
    if let Some(machine) = machines
        .iter()
        .find(|machine| machine.runtimes.contains(runtime))
    {
        return Some(Assignment {
            machine: machine.name.clone(),
            by: MachineAssignment::Pinned,
        });
    }
    let reported = reported?;
    machines
        .iter()
        .find(|machine| machine.ids.contains(&reported.id))
        .map(|machine| Assignment {
            machine: machine.name.clone(),
            by: MachineAssignment::MachineId,
        })
}

/// The machine a runtime that is not connected belongs to: its pin, else
/// the machine standing for the machine ID it last reported (`last`, this
/// process's memory of it, else its record), as `last_seen`.
pub(crate) fn offline_assignment(
    registry: &Registry,
    runtime: &RuntimeId,
    last: Option<&RuntimeMachine>,
) -> Option<Assignment> {
    let recorded = registry.record(runtime).and_then(RuntimeRecord::reported);
    let found = assignment(&registry.machines, runtime, last.or(recorded.as_ref()))?;
    Some(match found.by {
        MachineAssignment::Pinned => found,
        _ => Assignment {
            by: MachineAssignment::LastSeen,
            ..found
        },
    })
}

/// Rule 3 and 4: the machine to create for a runtime no machine claims. A
/// reported machine ID gets a machine of its own, named after the host
/// name; otherwise the machine pins the runtime ID and is named after it.
fn new_machine(
    machines: &[Machine],
    runtime: &RuntimeId,
    reported: Option<&RuntimeMachine>,
) -> Machine {
    match reported {
        Some(reported) => Machine {
            name: unique_name(
                machines,
                &host_machine_name(reported.host_name.as_deref().unwrap_or_default()),
            ),
            ids: vec![reported.id.clone()],
            runtimes: Vec::new(),
        },
        None => Machine {
            name: unique_name(machines, runtime.as_str()),
            ids: Vec::new(),
            runtimes: vec![runtime.clone()],
        },
    }
}

/// A machine name for a host name: its first label, lowercased, with every
/// run of characters outside `[a-z0-9_-]` replaced by one `-`, edges
/// trimmed, `host-` in front when it does not start with a letter, and at
/// most 32 bytes. `machine` when nothing usable is left.
pub(crate) fn host_machine_name(host: &str) -> String {
    let label = host.split('.').next().unwrap_or_default();
    let mut name = String::new();
    for character in label.chars() {
        let character = character.to_ascii_lowercase();
        if character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '_'
            || character == '-'
        {
            name.push(character);
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_matches(['-', '_']);
    let mut name = match name {
        "" => UNNAMED_HOST.to_owned(),
        name if name.starts_with(|character: char| character.is_ascii_lowercase()) => {
            name.to_owned()
        }
        name => format!("host-{name}"),
    };
    truncate_name(&mut name, 32);
    name
}

/// Shortens `name` to at most `bytes`, without a trailing `-` or `_`.
fn truncate_name(name: &mut String, bytes: usize) {
    name.truncate(bytes);
    let kept = name.trim_end_matches(['-', '_']).len();
    name.truncate(kept);
}

/// `base`, or `base-2`, `base-3`, … when a machine has the name; the base
/// is shortened so the name keeps 32 bytes. `base` must be a valid name.
fn unique_name(machines: &[Machine], base: &str) -> MachineName {
    let taken = |candidate: &str| {
        machines
            .iter()
            .any(|machine| machine.name.as_str() == candidate)
    };
    if !taken(base) {
        return MachineName::new(base).expect("a sanitized machine name");
    }
    (2_u32..)
        .map(|suffix| {
            let suffix = format!("-{suffix}");
            let mut stem = base.to_owned();
            truncate_name(&mut stem, 32 - suffix.len());
            format!("{stem}{suffix}")
        })
        .find(|candidate| !taken(candidate))
        .and_then(|candidate| MachineName::new(candidate).ok())
        .expect("a free machine name")
}

/// Checks what [`RegistryFile`] cannot say in its types: one format, unique
/// names, and each machine ID and runtime ID on at most one machine.
fn validate(file: &RegistryFile) -> Result<(), String> {
    match file.version {
        REGISTRY_VERSION => {}
        RECORDLESS_VERSION if file.remembered.is_empty() => {}
        RECORDLESS_VERSION => {
            return Err(format!(
                "version {RECORDLESS_VERSION} has no `remembered` runtimes"
            ));
        }
        version => {
            return Err(format!(
                "version {version} is not a registry version this build reads ({RECORDLESS_VERSION} or {REGISTRY_VERSION})"
            ));
        }
    }
    let mut remembered = BTreeSet::new();
    for record in &file.remembered {
        if !remembered.insert(record.runtime.as_str()) {
            return Err(format!("runtime `{}` is remembered twice", record.runtime));
        }
        let valid = record.reported().is_none_or(|reported| reported.is_valid());
        if !valid {
            return Err(format!(
                "runtime `{}` has a malformed machine ID",
                record.runtime
            ));
        }
    }
    let (mut names, mut ids, mut runtimes) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for machine in &file.machines {
        if !names.insert(machine.name.as_str()) {
            return Err(format!("machine `{}` appears twice", machine.name));
        }
        for id in &machine.ids {
            let valid = RuntimeMachine {
                id: id.clone(),
                host_name: None,
            }
            .is_valid();
            if !valid {
                return Err(format!(
                    "machine `{}` has a malformed machine ID",
                    machine.name
                ));
            }
            if !ids.insert(id.as_str()) {
                return Err(format!("machine ID `{id}` is on two machines"));
            }
        }
        for runtime in &machine.runtimes {
            if !runtimes.insert(runtime.as_str()) {
                return Err(format!("runtime `{runtime}` is pinned to two machines"));
            }
        }
    }
    Ok(())
}

/// The daemon's machine registry.
pub(crate) struct MachineRegistry {
    path: PathBuf,
    memory: MemoryRoot,
    /// The registry as last read or written.
    state: Mutex<Registry>,
    /// The runtimes [`Self::observe`] last saw connected, so it notices a
    /// runtime connecting or going offline.
    online: Mutex<BTreeSet<RuntimeId>>,
    /// Serializes changes within this process; the memory lock serializes
    /// processes.
    mutation: Mutex<()>,
    /// Bumped whenever the machines or the records change.
    changes: tokio::sync::watch::Sender<u64>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl MachineRegistry {
    /// Reads `<data_dir>/machines.json`; a registry that does not exist yet
    /// has no machines. A file that does not parse or breaks the registry's
    /// rules fails.
    pub(crate) fn open(data_dir: &Path) -> Result<Self, MachineError> {
        let path = data_dir.join(REGISTRY_FILE);
        let state = read(&path)?;
        Ok(Self {
            path,
            memory: MemoryRoot::new(data_dir),
            state: Mutex::new(state),
            online: Mutex::default(),
            mutation: Mutex::new(()),
            changes: tokio::sync::watch::channel(0).0,
        })
    }

    pub(crate) fn memory(&self) -> &MemoryRoot {
        &self.memory
    }

    /// Every machine, in name order.
    #[cfg(test)]
    pub(crate) fn machines(&self) -> Vec<Machine> {
        lock(&self.state).machines.clone()
    }

    /// The machines and the runtime records.
    pub(crate) fn registry(&self) -> Registry {
        lock(&self.state).clone()
    }

    /// What the registry remembers of `runtime`.
    pub(crate) fn record(&self, runtime: &RuntimeId) -> Option<RuntimeRecord> {
        lock(&self.state).record(runtime).cloned()
    }

    /// Signals every change to the machines.
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// The machine `runtime` belongs to by a pin or by the machine ID it
    /// reported, without creating one.
    pub(crate) fn lookup(
        &self,
        runtime: &RuntimeId,
        reported: Option<&RuntimeMachine>,
    ) -> Option<Assignment> {
        assignment(&lock(&self.state).machines, runtime, reported)
    }

    /// The machine a `runtime` that is not connected belongs to: by its pin,
    /// else by the machine ID it last reported, `last` when this process
    /// saw it and its record otherwise.
    pub(crate) fn lookup_offline(
        &self,
        runtime: &RuntimeId,
        last: Option<&RuntimeMachine>,
    ) -> Option<Assignment> {
        offline_assignment(&lock(&self.state), runtime, last)
    }

    /// The machine a connected `runtime` belongs to, in the order of section
    /// 3.2; a new machine is written to the registry at once when no
    /// machine claims it.
    pub(crate) fn resolve(
        &self,
        runtime: &RuntimeId,
        reported: Option<&RuntimeMachine>,
    ) -> Result<Assignment, MachineError> {
        if let Some(found) = self.lookup(runtime, reported) {
            return Ok(found);
        }
        self.mutate(|registry, _| {
            let machines = &mut registry.machines;
            if let Some(found) = assignment(machines, runtime, reported) {
                return Ok((found, FolderMoves::default()));
            }
            let machine = new_machine(machines, runtime, reported);
            let found = Assignment {
                machine: machine.name.clone(),
                by: if reported.is_some() {
                    MachineAssignment::MachineId
                } else {
                    MachineAssignment::Pinned
                },
            };
            machines.push(machine);
            Ok((found, FolderMoves::default()))
        })
    }

    /// Records what each connected runtime of `runtimes` reports, stamped
    /// `now` when it has just connected or reports something new, and
    /// stamps `now` on the record of each one that went offline since the
    /// last call. Writes the registry only when a record changes.
    pub(crate) fn observe(
        &self,
        runtimes: &[ObservedRuntime],
        now: jiff::Timestamp,
    ) -> Result<(), MachineError> {
        let mut online = lock(&self.online);
        let changes = {
            let state = lock(&self.state);
            observed_changes(&state, &online, runtimes, now)
        };
        if !changes.is_empty() {
            self.mutate(|registry, _| {
                for change in &changes {
                    match change {
                        RecordChange::Write(record) => {
                            registry
                                .records
                                .retain(|known| known.runtime != record.runtime);
                            registry.records.push(record.clone());
                        }
                        RecordChange::Seen(runtime) => {
                            if let Some(record) = registry
                                .records
                                .iter_mut()
                                .find(|record| record.runtime == *runtime)
                            {
                                record.last_seen = now;
                            }
                        }
                    }
                }
                Ok(((), FolderMoves::default()))
            })?;
        }
        *online = runtimes
            .iter()
            .filter(|runtime| runtime.connected)
            .map(|runtime| runtime.runtime.clone())
            .collect();
        Ok(())
    }

    /// Drops `runtime`'s record. `connected` refuses: the runtime is
    /// recorded again whenever it connects.
    pub(crate) fn forget_runtime(
        &self,
        runtime: &RuntimeId,
        connected: bool,
    ) -> Result<(), MachineError> {
        if connected {
            return Err(MachineError::StillConnected(runtime.clone()));
        }
        self.mutate(|registry, _| {
            let before = registry.records.len();
            registry.records.retain(|record| record.runtime != *runtime);
            if registry.records.len() == before {
                return Err(MachineError::NotRemembered(runtime.clone()));
            }
            Ok(((), FolderMoves::default()))
        })
    }

    /// Drops every record `drop` selects.
    pub(crate) fn drop_records(
        &self,
        drop: impl Fn(&RuntimeRecord) -> bool,
    ) -> Result<(), MachineError> {
        if !lock(&self.state).records.iter().any(&drop) {
            return Ok(());
        }
        self.mutate(|registry, _| {
            registry.records.retain(|record| !drop(record));
            Ok(((), FolderMoves::default()))
        })
    }

    /// Renames `machine` to `name` and moves its memory folder with it.
    pub(crate) fn rename(
        &self,
        machine: &MachineName,
        name: &MachineName,
    ) -> Result<(), MachineError> {
        self.mutate(|registry, lock| {
            let machines = &mut registry.machines;
            let index = position(machines, machine)?;
            if machine == name {
                return Ok(((), FolderMoves::default()));
            }
            if machines.iter().any(|other| other.name == *name) {
                return Err(MachineError::NameTaken(name.clone()));
            }
            let moves = self
                .memory
                .rename_machine_folder(lock, machine, name)
                .map_err(|error| match error {
                    MemoryError::Exists(_) => MachineError::NameTaken(name.clone()),
                    error => error.into(),
                })?;
            machines[index].name = name.clone();
            Ok(((), moves))
        })
    }

    /// Pins `runtime` to `machine`, created when the name is new, and
    /// removes its pin from any other machine.
    pub(crate) fn assign(
        &self,
        runtime: &RuntimeId,
        machine: &MachineName,
    ) -> Result<(), MachineError> {
        self.mutate(|registry, _| {
            let machines = &mut registry.machines;
            for other in machines.iter_mut() {
                other.runtimes.retain(|pinned| pinned != runtime);
            }
            match machines.iter_mut().find(|other| other.name == *machine) {
                Some(target) => target.runtimes.push(runtime.clone()),
                None => machines.push(Machine {
                    name: machine.clone(),
                    ids: Vec::new(),
                    runtimes: vec![runtime.clone()],
                }),
            }
            Ok(((), FolderMoves::default()))
        })
    }

    /// Removes `runtime`'s pin, so that it falls back to the machine ID it
    /// reports.
    pub(crate) fn unassign(&self, runtime: &RuntimeId) -> Result<(), MachineError> {
        self.mutate(|registry, _| {
            let machine = registry
                .machines
                .iter_mut()
                .find(|machine| machine.runtimes.contains(runtime))
                .ok_or_else(|| MachineError::NotPinned(runtime.clone()))?;
            machine.runtimes.retain(|pinned| pinned != runtime);
            Ok(((), FolderMoves::default()))
        })
    }

    /// Moves `from`'s machine IDs, pins, and memories to `into` and removes
    /// `from`. A memory file name both folders have fails the merge. The
    /// runtimes remembered on `from` follow its machine IDs.
    pub(crate) fn merge(&self, from: &MachineName, into: &MachineName) -> Result<(), MachineError> {
        self.mutate(|registry, lock| {
            let machines = &mut registry.machines;
            if from == into {
                return Err(MachineError::MergeSelf(from.clone()));
            }
            let source = position(machines, from)?;
            position(machines, into)?;
            let moves = self
                .memory
                .merge_machine_folders(lock, from, into)
                .map_err(|error| match error {
                    MemoryError::Conflict(name) => MachineError::MemoryConflict {
                        from: from.clone(),
                        into: into.clone(),
                        name,
                    },
                    error => error.into(),
                })?;
            let merged = machines.remove(source);
            let target = machines
                .iter_mut()
                .find(|machine| machine.name == *into)
                .expect("the target machine exists");
            target.ids.extend(merged.ids);
            target.runtimes.extend(merged.runtimes);
            Ok(((), moves))
        })
    }

    /// Removes `machine` when none of the `connected` runtimes (with what
    /// they reported) belongs to it and its memory folder is empty or
    /// missing; an empty folder goes with it, and so do the records of the
    /// runtimes remembered on it.
    pub(crate) fn forget(
        &self,
        machine: &MachineName,
        connected: &[(RuntimeId, Option<RuntimeMachine>)],
    ) -> Result<(), MachineError> {
        self.mutate(|registry, lock| {
            let index = position(&registry.machines, machine)?;
            let in_use = connected.iter().any(|(runtime, reported)| {
                assignment(&registry.machines, runtime, reported.as_ref())
                    .is_some_and(|found| found.machine == *machine)
            });
            if in_use || self.memory.machine_folder_has_entries(machine)? {
                return Err(MachineError::InUse(machine.clone()));
            }
            let remembered = registry
                .records
                .iter()
                .filter(|record| {
                    offline_assignment(registry, &record.runtime, None)
                        .is_some_and(|found| found.machine == *machine)
                })
                .map(|record| record.runtime.clone())
                .collect::<Vec<_>>();
            let moves = self.memory.remove_empty_machine_folder(lock, machine)?;
            registry
                .records
                .retain(|record| !remembered.contains(&record.runtime));
            registry.machines.remove(index);
            Ok(((), moves))
        })
    }

    /// Runs `change` on the registry as it is on disk, under the memory
    /// lock, and writes the result when it changed anything. When writing
    /// fails, the folder moves `change` made are undone.
    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut Registry, &MemoryLock) -> Result<(T, FolderMoves), MachineError>,
    ) -> Result<T, MachineError> {
        let _mutation = lock(&self.mutation);
        let memory_lock = self.memory.lock()?;
        // Another daemon sharing the data root may have changed it.
        let current = read(&self.path)?;
        let mut registry = current.clone();
        let (value, moves) = change(&mut registry, &memory_lock)?;
        registry.sort();
        if registry != current {
            let file = RegistryFile {
                version: REGISTRY_VERSION,
                machines: registry.machines.clone(),
                remembered: registry.records.clone(),
            };
            let written = validate(&file)
                .map_err(MachineError::Store)
                .and_then(|()| write(&self.path, &file));
            if let Err(error) = written {
                moves.undo();
                return Err(error);
            }
        }
        drop(memory_lock);
        let mut cached = lock(&self.state);
        if *cached != registry {
            *cached = registry;
            drop(cached);
            self.changes.send_modify(|revision| *revision += 1);
        }
        Ok(value)
    }
}

/// One record change [`MachineRegistry::observe`] makes.
#[derive(Clone, Debug)]
enum RecordChange {
    /// Write this record, replacing the runtime's.
    Write(RuntimeRecord),
    /// Stamp the runtime's record as seen now.
    Seen(RuntimeId),
}

/// What [`MachineRegistry::observe`] changes: a record for each connected
/// runtime that was not `online` before or reports something new, and a
/// stamp for each `online` runtime that is no longer connected.
fn observed_changes(
    registry: &Registry,
    online: &BTreeSet<RuntimeId>,
    runtimes: &[ObservedRuntime],
    now: jiff::Timestamp,
) -> Vec<RecordChange> {
    let mut changes = Vec::new();
    for runtime in runtimes {
        let known = registry.record(&runtime.runtime);
        if runtime.connected {
            let record = RuntimeRecord {
                runtime: runtime.runtime.clone(),
                kind: runtime.kind,
                machine_id: runtime
                    .reported
                    .as_ref()
                    .map(|reported| reported.id.clone()),
                platform: runtime.platform.clone(),
                home: runtime.home.clone(),
                last_seen: now,
            };
            let unchanged = online.contains(&runtime.runtime)
                && known.is_some_and(|known| known.same_report(&record));
            if !unchanged {
                changes.push(RecordChange::Write(record));
            }
        } else if online.contains(&runtime.runtime) && known.is_some() {
            changes.push(RecordChange::Seen(runtime.runtime.clone()));
        }
    }
    changes
}

fn position(machines: &[Machine], name: &MachineName) -> Result<usize, MachineError> {
    machines
        .iter()
        .position(|machine| machine.name == *name)
        .ok_or_else(|| MachineError::NotFound(name.clone()))
}

/// The registry in `path`, sorted; empty when it does not exist.
fn read(path: &Path) -> Result<Registry, MachineError> {
    let bytes = match cookie_agent_models::secure_store::read_replaced(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Registry::default()),
        Err(error) => {
            return Err(MachineError::Store(format!("{}: {error}", path.display())));
        }
    };
    let file: RegistryFile = serde_json::from_slice(&bytes)
        .map_err(|error| MachineError::Store(format!("{}: {error}", path.display())))?;
    validate(&file)
        .map_err(|reason| MachineError::Store(format!("{}: {reason}", path.display())))?;
    let mut registry = Registry {
        machines: file.machines,
        records: file.remembered,
    };
    registry.sort();
    Ok(registry)
}

/// Writes `file` to `path` through a flushed temporary file in the same
/// folder, renamed over it.
fn write(path: &Path, file: &RegistryFile) -> Result<(), MachineError> {
    let failed = |error: io::Error| MachineError::Store(format!("{}: {error}", path.display()));
    let directory = path
        .parent()
        .ok_or_else(|| MachineError::Store(format!("{} has no parent folder", path.display())))?;
    fs::create_dir_all(directory).map_err(failed)?;
    let mut bytes = serde_json::to_vec_pretty(file).expect("the registry serializes");
    bytes.push(b'\n');
    let temporary = directory.join(format!(".{REGISTRY_FILE}.{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let mut output = fs::File::create(&temporary)?;
        io::Write::write_all(&mut output, &bytes)?;
        output.sync_all()?;
        drop(output);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(failed)
}
