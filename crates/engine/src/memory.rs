//! The memory folder, `<data_root>/memory` (`docs/site/specs/memory.md`
//! section 4): its advisory lock, its first-start defaults, the machine
//! memory folders that renaming and merging machines move, and the memory
//! store: URLs ([`url`]), the file format and revisions ([`file`]), reads,
//! listings, and changes ([`store`]), and search ([`search`]).
//!
//! Every change to memory or to the machine registry holds [`MemoryLock`],
//! because two plain `cookie` processes are two daemons sharing one data
//! root. Functions that change folders take the held lock as proof.

// The model and client surfaces of later phases call the store; until then
// only its tests and the first start do.
#[allow(dead_code)]
pub(crate) mod file;
#[allow(dead_code)]
pub(crate) mod search;
#[allow(dead_code)]
pub(crate) mod store;
#[allow(dead_code)]
pub(crate) mod url;

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use cookie_agent_protocol::MachineName;

/// The memory folder's name in the data root.
pub(crate) const MEMORY_DIR: &str = "memory";
/// The lock file inside the memory folder.
pub(crate) const LOCK_FILE: &str = ".lock";
/// The folder of user categories inside the memory folder.
pub(crate) const USER_DIR: &str = "user";
/// The folder of agent memories inside the memory folder.
pub(crate) const AGENTS_DIR: &str = "agents";
/// The folder of machine memories inside the memory folder.
pub(crate) const MACHINES_DIR: &str = "machines";
/// The folder of projects inside the memory folder.
pub(crate) const PROJECTS_DIR: &str = "projects";
/// A memory file's extension.
pub(crate) const MEMORY_EXTENSION: &str = "md";

/// How long taking the lock waits for another process.
const LOCK_BUDGET: Duration = Duration::from_secs(5);

/// Why the memory folder could not be locked or changed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MemoryError {
    #[error("another cookie process holds the memory lock {}", .0.display())]
    Contention(PathBuf),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The target folder already exists.
    #[error("{} already exists", .0.display())]
    Exists(PathBuf),
    /// Both folders hold a file of this name.
    #[error("both memory folders hold `{0}`")]
    Conflict(String),
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> MemoryError + '_ {
    move |source| MemoryError::Io {
        path: path.to_owned(),
        source,
    }
}

/// The memory folder of one data root.
#[derive(Clone, Debug)]
pub(crate) struct MemoryRoot {
    root: PathBuf,
}

/// The held memory lock; dropping it releases the lock.
#[derive(Debug)]
pub(crate) struct MemoryLock {
    /// The locked file; `None` while the memory folder does not exist.
    file: Option<fs::File>,
}

impl Drop for MemoryLock {
    fn drop(&mut self) {
        if let Some(file) = &self.file {
            let _ = cookie_agent_models::secure_store::unlock(file);
        }
    }
}

/// Folder moves done under the lock, undone in reverse when a later step
/// of the same change fails.
#[derive(Debug, Default)]
#[must_use = "undo the moves when the change fails"]
pub(crate) struct FolderMoves {
    renamed: Vec<(PathBuf, PathBuf)>,
    created: Vec<PathBuf>,
    removed: Vec<PathBuf>,
}

impl FolderMoves {
    /// Puts every moved file and folder back, best effort.
    pub(crate) fn undo(self) {
        for path in self.removed.iter().rev() {
            let _ = fs::create_dir(path);
        }
        for (from, to) in self.renamed.iter().rev() {
            let _ = fs::rename(to, from);
        }
        for path in self.created.iter().rev() {
            let _ = fs::remove_dir(path);
        }
    }
}

/// The user categories a first start creates (section 4.5): file name,
/// description, and keywords; their bodies are empty.
const DEFAULT_CATEGORIES: [(&str, &str, &[&str]); 4] = [
    (
        "profile",
        "Who the user is: role, expertise, languages, and the machines and accounts they use",
        &["user", "role", "background", "expertise"],
    ),
    (
        "preferences",
        "How the user wants work done: style, tools, formats, tone, and things to always or never do",
        &["prefer", "style", "always", "never", "format"],
    ),
    (
        "feedback",
        "Corrections and confirmed approaches from the user, each with why it matters",
        &["feedback", "correction", "mistake", "approach"],
    ),
    (
        "references",
        "Where to find things: URLs, dashboards, documentation, related repositories",
        &["link", "url", "docs", "dashboard", "repo"],
    ),
];

impl MemoryRoot {
    /// The memory folder of the data root `data_dir`.
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join(MEMORY_DIR),
        }
    }

    /// The first start of section 4.5: when the memory folder does not
    /// exist, creates it with the four layer folders and the default user
    /// categories, and returns true. A memory folder that exists, whatever
    /// it holds, is never touched.
    ///
    /// The folder is built under a temporary name in the data root and
    /// renamed into place, so a crash leaves no half-made memory folder
    /// that later starts would take as the user's, and a second daemon
    /// starting at the same moment keeps the first one's folder.
    pub(crate) fn create_defaults_if_absent(&self) -> Result<bool, MemoryError> {
        if fs::symlink_metadata(&self.root).is_ok() {
            return Ok(false);
        }
        let data_dir = self.root.parent().expect("the memory folder has a parent");
        fs::create_dir_all(data_dir).map_err(io_error(data_dir))?;
        let staging = data_dir.join(format!(".{MEMORY_DIR}.{}.tmp", uuid::Uuid::now_v7()));
        let built = (|| {
            for layer in [USER_DIR, AGENTS_DIR, MACHINES_DIR, PROJECTS_DIR] {
                let path = staging.join(layer);
                fs::create_dir_all(&path).map_err(io_error(&path))?;
            }
            for (category, description, keywords) in DEFAULT_CATEGORIES {
                let document = file::MemoryDocument::new(
                    description,
                    keywords
                        .iter()
                        .map(|keyword| (*keyword).to_owned())
                        .collect(),
                    "",
                )
                .expect("the default categories are valid");
                let path = staging
                    .join(USER_DIR)
                    .join(format!("{category}.{MEMORY_EXTENSION}"));
                fs::write(&path, document.render()).map_err(io_error(&path))?;
            }
            Ok(())
        })();
        let placed = built.and_then(|()| match fs::rename(&staging, &self.root) {
            Ok(()) => Ok(true),
            // Another daemon created the folder first.
            Err(_) if fs::symlink_metadata(&self.root).is_ok() => Ok(false),
            Err(source) => Err(MemoryError::Io {
                path: self.root.clone(),
                source,
            }),
        });
        if !matches!(placed, Ok(true)) {
            let _ = fs::remove_dir_all(&staging);
        }
        placed
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.root
    }

    /// `memory/machines/<machine>/`.
    pub(crate) fn machine_dir(&self, machine: &MachineName) -> PathBuf {
        self.root.join(MACHINES_DIR).join(machine.as_str())
    }

    /// Takes the advisory lock on `memory/.lock`, waiting a few seconds for
    /// another process. While the memory folder does not exist there is
    /// nothing a change could race with, and the lock creates nothing, so
    /// that the first start still finds no memory folder: the returned lock
    /// then holds no file.
    pub(crate) fn lock(&self) -> Result<MemoryLock, MemoryError> {
        if !self.root.is_dir() {
            return Ok(MemoryLock { file: None });
        }
        let path = self.root.join(LOCK_FILE);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_error(&path))?;
        if !cookie_agent_models::secure_store::lock_within_file(&file, LOCK_BUDGET)
            .map_err(io_error(&path))?
        {
            return Err(MemoryError::Contention(path));
        }
        Ok(MemoryLock { file: Some(file) })
    }

    /// The memory files (`*.md`) directly in a machine's folder; none when
    /// it has no folder.
    pub(crate) fn machine_memory_count(&self, machine: &MachineName) -> usize {
        fs::read_dir(self.machine_dir(machine)).map_or(0, |entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .filter(|entry| {
                    Path::new(&entry.file_name())
                        .extension()
                        .is_some_and(|extension| extension == MEMORY_EXTENSION)
                })
                .count()
        })
    }

    /// Whether a machine's folder holds anything at all; false when it has
    /// no folder.
    pub(crate) fn machine_folder_has_entries(
        &self,
        machine: &MachineName,
    ) -> Result<bool, MemoryError> {
        let path = self.machine_dir(machine);
        match fs::read_dir(&path) {
            Ok(mut entries) => Ok(entries.next().is_some()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(MemoryError::Io { path, source }),
        }
    }

    /// Moves `memory/machines/<from>/` to `<to>/`. Nothing moves when
    /// `from` has no folder; a `to` folder that already exists fails.
    pub(crate) fn rename_machine_folder(
        &self,
        _lock: &MemoryLock,
        from: &MachineName,
        to: &MachineName,
    ) -> Result<FolderMoves, MemoryError> {
        let (source, target) = (self.machine_dir(from), self.machine_dir(to));
        if fs::symlink_metadata(&target).is_ok() {
            return Err(MemoryError::Exists(target));
        }
        let mut moves = FolderMoves::default();
        if fs::symlink_metadata(&source).is_err() {
            return Ok(moves);
        }
        fs::rename(&source, &target).map_err(io_error(&source))?;
        moves.renamed.push((source, target));
        Ok(moves)
    }

    /// Moves every file of `memory/machines/<from>/` into `<into>/` and
    /// removes the emptied folder. Fails, moving nothing, when a name exists
    /// in both; a partial move is undone.
    pub(crate) fn merge_machine_folders(
        &self,
        lock: &MemoryLock,
        from: &MachineName,
        into: &MachineName,
    ) -> Result<FolderMoves, MemoryError> {
        let (source, target) = (self.machine_dir(from), self.machine_dir(into));
        if fs::symlink_metadata(&source).is_err() {
            return Ok(FolderMoves::default());
        }
        if fs::symlink_metadata(&target).is_err() {
            return self.rename_machine_folder(lock, from, into);
        }
        let mut names = fs::read_dir(&source)
            .map_err(io_error(&source))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(io_error(&source))?;
        names.sort();
        if let Some(clash) = names
            .iter()
            .find(|name| fs::symlink_metadata(target.join(name)).is_ok())
        {
            return Err(MemoryError::Conflict(clash.to_string_lossy().into_owned()));
        }
        let mut moves = FolderMoves::default();
        let result = (|| {
            for name in &names {
                let (from, to) = (source.join(name), target.join(name));
                fs::rename(&from, &to).map_err(io_error(&from))?;
                moves.renamed.push((from, to));
            }
            fs::remove_dir(&source).map_err(io_error(&source))?;
            moves.removed.push(source.clone());
            Ok(())
        })();
        match result {
            Ok(()) => Ok(moves),
            Err(error) => {
                moves.undo();
                Err(error)
            }
        }
    }

    /// Removes a machine's empty folder, if it has one.
    pub(crate) fn remove_empty_machine_folder(
        &self,
        _lock: &MemoryLock,
        machine: &MachineName,
    ) -> Result<FolderMoves, MemoryError> {
        let path = self.machine_dir(machine);
        let mut moves = FolderMoves::default();
        match fs::remove_dir(&path) {
            Ok(()) => {
                moves.removed.push(path);
                Ok(moves)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(moves),
            Err(source) => Err(MemoryError::Io { path, source }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(value: &str) -> MachineName {
        MachineName::new(value).unwrap()
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn the_lock_creates_no_memory_folder_and_locks_one_that_exists() {
        let data = tempfile::tempdir().unwrap();
        let memory = MemoryRoot::new(data.path());
        let lock = memory.lock().unwrap();
        assert!(lock.file.is_none());
        assert!(!memory.path().exists());
        drop(lock);

        fs::create_dir(memory.path()).unwrap();
        let held = memory.lock().unwrap();
        assert!(memory.path().join(LOCK_FILE).is_file());
        let other = fs::File::open(memory.path().join(LOCK_FILE)).unwrap();
        assert!(!cookie_agent_models::secure_store::try_lock_once(&other).unwrap());
        drop(held);
        assert!(cookie_agent_models::secure_store::try_lock_once(&other).unwrap());
    }

    #[test]
    fn renaming_moves_the_folder_and_refuses_an_existing_target() {
        let data = tempfile::tempdir().unwrap();
        let memory = MemoryRoot::new(data.path());
        let lock = memory.lock().unwrap();
        // No folder yet: nothing moves.
        let moves = memory
            .rename_machine_folder(&lock, &name("laptop"), &name("desk"))
            .unwrap();
        assert!(moves.renamed.is_empty());

        write(&memory.machine_dir(&name("laptop")).join("gpu.md"), "x");
        write(&memory.machine_dir(&name("taken")).join("a.md"), "y");
        assert!(matches!(
            memory.rename_machine_folder(&lock, &name("laptop"), &name("taken")),
            Err(MemoryError::Exists(_))
        ));
        assert!(memory.machine_dir(&name("laptop")).join("gpu.md").is_file());

        let moves = memory
            .rename_machine_folder(&lock, &name("laptop"), &name("desk"))
            .unwrap();
        assert!(memory.machine_dir(&name("desk")).join("gpu.md").is_file());
        assert!(!memory.machine_dir(&name("laptop")).exists());
        moves.undo();
        assert!(memory.machine_dir(&name("laptop")).join("gpu.md").is_file());
        assert!(!memory.machine_dir(&name("desk")).exists());
    }

    #[test]
    fn merging_moves_every_file_unless_a_name_is_in_both() {
        let data = tempfile::tempdir().unwrap();
        let memory = MemoryRoot::new(data.path());
        let lock = memory.lock().unwrap();
        let (from, into) = (name("box"), name("labpc"));
        write(&memory.machine_dir(&from).join("scratch.md"), "from");
        write(&memory.machine_dir(&from).join("gpu.md"), "from");
        write(&memory.machine_dir(&into).join("gpu.md"), "into");
        assert!(matches!(
            memory.merge_machine_folders(&lock, &from, &into),
            Err(MemoryError::Conflict(clash)) if clash == "gpu.md"
        ));
        assert_eq!(memory.machine_memory_count(&from), 2);
        assert_eq!(memory.machine_memory_count(&into), 1);

        fs::remove_file(memory.machine_dir(&from).join("gpu.md")).unwrap();
        let moves = memory.merge_machine_folders(&lock, &from, &into).unwrap();
        assert!(!memory.machine_dir(&from).exists());
        assert_eq!(memory.machine_memory_count(&into), 2);
        assert_eq!(
            fs::read_to_string(memory.machine_dir(&into).join("gpu.md")).unwrap(),
            "into"
        );
        moves.undo();
        assert_eq!(memory.machine_memory_count(&from), 1);
        assert_eq!(memory.machine_memory_count(&into), 1);
    }

    #[test]
    fn merging_into_a_machine_without_a_folder_moves_the_folder() {
        let data = tempfile::tempdir().unwrap();
        let memory = MemoryRoot::new(data.path());
        let lock = memory.lock().unwrap();
        write(&memory.machine_dir(&name("box")).join("a.md"), "a");
        let _moves = memory
            .merge_machine_folders(&lock, &name("box"), &name("labpc"))
            .unwrap();
        assert!(memory.machine_dir(&name("labpc")).join("a.md").is_file());
        assert!(!memory.machine_dir(&name("box")).exists());
    }

    #[test]
    fn only_markdown_files_count_as_memories() {
        let data = tempfile::tempdir().unwrap();
        let memory = MemoryRoot::new(data.path());
        let machine = name("labpc");
        assert_eq!(memory.machine_memory_count(&machine), 0);
        assert!(!memory.machine_folder_has_entries(&machine).unwrap());
        write(&memory.machine_dir(&machine).join("notes.txt"), "x");
        assert_eq!(memory.machine_memory_count(&machine), 0);
        assert!(memory.machine_folder_has_entries(&machine).unwrap());
        write(&memory.machine_dir(&machine).join("a.md"), "x");
        fs::create_dir(memory.machine_dir(&machine).join("nested.md")).unwrap();
        assert_eq!(memory.machine_memory_count(&machine), 1);
    }
}
