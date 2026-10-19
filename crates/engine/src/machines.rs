//! The machine registry (`docs/site/specs/memory.md` section 3): the
//! computers runtimes run on, kept in `<data_root>/machines.json`.
//!
//! A runtime's machine is never stored with the runtime: it follows from
//! the registry and what the runtime reported whenever it is asked for
//! ([`assignment`]), so a change to the registry moves every connected
//! runtime it affects at once. Only a runtime that no machine claims makes
//! the registry grow, by one new machine written at once.
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
/// The registry format this build reads and writes.
const REGISTRY_VERSION: u32 = 1;
/// The name of a machine whose host name leaves nothing usable.
const UNNAMED_HOST: &str = "machine";

/// `machines.json`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    version: u32,
    machines: Vec<Machine>,
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
    if file.version != REGISTRY_VERSION {
        return Err(format!(
            "version {} is not the registry version {REGISTRY_VERSION} this build reads",
            file.version
        ));
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
    /// The machines as last read or written, in name order.
    machines: Mutex<Vec<Machine>>,
    /// Serializes changes within this process; the memory lock serializes
    /// processes.
    mutation: Mutex<()>,
    /// Bumped whenever the machines change.
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
        let machines = read(&path)?;
        Ok(Self {
            path,
            memory: MemoryRoot::new(data_dir),
            machines: Mutex::new(machines),
            mutation: Mutex::new(()),
            changes: tokio::sync::watch::channel(0).0,
        })
    }

    pub(crate) fn memory(&self) -> &MemoryRoot {
        &self.memory
    }

    /// Every machine, in name order.
    pub(crate) fn machines(&self) -> Vec<Machine> {
        lock(&self.machines).clone()
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
        assignment(&lock(&self.machines), runtime, reported)
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
        self.mutate(|machines, _| {
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

    /// Renames `machine` to `name` and moves its memory folder with it.
    pub(crate) fn rename(
        &self,
        machine: &MachineName,
        name: &MachineName,
    ) -> Result<(), MachineError> {
        self.mutate(|machines, lock| {
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
        self.mutate(|machines, _| {
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
        self.mutate(|machines, _| {
            let machine = machines
                .iter_mut()
                .find(|machine| machine.runtimes.contains(runtime))
                .ok_or_else(|| MachineError::NotPinned(runtime.clone()))?;
            machine.runtimes.retain(|pinned| pinned != runtime);
            Ok(((), FolderMoves::default()))
        })
    }

    /// Moves `from`'s machine IDs, pins, and memories to `into` and removes
    /// `from`. A memory file name both folders have fails the merge.
    pub(crate) fn merge(&self, from: &MachineName, into: &MachineName) -> Result<(), MachineError> {
        self.mutate(|machines, lock| {
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
    /// missing; an empty folder goes with it.
    pub(crate) fn forget(
        &self,
        machine: &MachineName,
        connected: &[(RuntimeId, Option<RuntimeMachine>)],
    ) -> Result<(), MachineError> {
        self.mutate(|machines, lock| {
            let index = position(machines, machine)?;
            let in_use = connected.iter().any(|(runtime, reported)| {
                assignment(machines, runtime, reported.as_ref())
                    .is_some_and(|found| found.machine == *machine)
            });
            if in_use || self.memory.machine_folder_has_entries(machine)? {
                return Err(MachineError::InUse(machine.clone()));
            }
            let moves = self.memory.remove_empty_machine_folder(lock, machine)?;
            machines.remove(index);
            Ok(((), moves))
        })
    }

    /// Runs `change` on the registry as it is on disk, under the memory
    /// lock, and writes the result when it changed anything. When writing
    /// fails, the folder moves `change` made are undone.
    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut Vec<Machine>, &MemoryLock) -> Result<(T, FolderMoves), MachineError>,
    ) -> Result<T, MachineError> {
        let _mutation = lock(&self.mutation);
        let memory_lock = self.memory.lock()?;
        // Another daemon sharing the data root may have changed it.
        let current = read(&self.path)?;
        let mut machines = current.clone();
        let (value, moves) = change(&mut machines, &memory_lock)?;
        machines.sort_by(|left, right| left.name.cmp(&right.name));
        let file = RegistryFile {
            version: REGISTRY_VERSION,
            machines,
        };
        if file.machines != current {
            let written = validate(&file)
                .map_err(MachineError::Store)
                .and_then(|()| write(&self.path, &file));
            if let Err(error) = written {
                moves.undo();
                return Err(error);
            }
        }
        drop(memory_lock);
        let mut cached = lock(&self.machines);
        if *cached != file.machines {
            *cached = file.machines;
            drop(cached);
            self.changes.send_modify(|revision| *revision += 1);
        }
        Ok(value)
    }
}

fn position(machines: &[Machine], name: &MachineName) -> Result<usize, MachineError> {
    machines
        .iter()
        .position(|machine| machine.name == *name)
        .ok_or_else(|| MachineError::NotFound(name.clone()))
}

/// The machines in `path`, in name order; none when it does not exist.
fn read(path: &Path) -> Result<Vec<Machine>, MachineError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(MachineError::Store(format!("{}: {error}", path.display())));
        }
    };
    let mut file: RegistryFile = serde_json::from_slice(&bytes)
        .map_err(|error| MachineError::Store(format!("{}: {error}", path.display())))?;
    validate(&file)
        .map_err(|reason| MachineError::Store(format!("{}: {reason}", path.display())))?;
    file.machines
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(file.machines)
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
