//! Reading, listing, and changing memory files (`docs/site/specs/memory.md`
//! sections 4 and 8).
//!
//! Reads never take the lock: every write replaces a file by renaming a
//! flushed temporary file over it, so a reader sees the old or the new file
//! whole. Writes, edits, and deletes take [`super::MemoryLock`] for their
//! whole check-and-change, and may wait for it up to a few seconds, so
//! async callers run them on a blocking thread.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use cookie_agent_identity::AgentId;
use cookie_agent_protocol::MachineName;

use super::{
    MEMORY_EXTENSION, MemoryError, MemoryRoot,
    file::{InvalidMemory, MemoryDocument, MemoryRevision, too_large},
    url::{MemoryLayer, MemoryName, MemoryUrl},
};

/// Who asks for a change. Model tools may not delete `user://` or
/// `agent://` files nor create `user://` files; the user, through a
/// client, may (sections 8 and 10).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Origin {
    Model,
    // The client surface (`memory.*`) asks for the user.
    #[allow(dead_code)]
    User,
}

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error("{url} does not exist")]
    NotFound { url: MemoryUrl },
    #[error("{url} already exists at revision {revision}; send that revision to replace it")]
    Exists {
        url: MemoryUrl,
        revision: MemoryRevision,
    },
    #[error("{url} changed since it was read; its current revision is {current}")]
    RevisionMismatch {
        url: MemoryUrl,
        current: MemoryRevision,
    },
    #[error("{url} is not a valid memory: {reason}")]
    Invalid {
        url: MemoryUrl,
        reason: InvalidMemory,
    },
    #[error("{0}")]
    Refused(String),
    #[error("`{old}` does not occur in the body of {url}")]
    NoOccurrence { url: MemoryUrl, old: String },
    #[error(
        "`{old}` occurs {count} times in the body of {url}; include more context so it occurs once"
    )]
    SeveralOccurrences {
        url: MemoryUrl,
        old: String,
        count: usize,
    },
}

/// A memory file as found on disk.
#[derive(Clone, Debug)]
pub(crate) struct MemoryEntry {
    pub(crate) url: MemoryUrl,
    pub(crate) revision: MemoryRevision,
    /// The file's size in bytes, which `memory.list` reports.
    #[allow(dead_code)]
    pub(crate) size: u64,
    pub(crate) modified: SystemTime,
    /// The parsed memory, or why the file is invalid.
    pub(crate) document: Result<MemoryDocument, InvalidMemory>,
}

/// One memory file read whole.
#[derive(Clone, Debug)]
pub(crate) struct MemoryRead {
    pub(crate) entry: MemoryEntry,
    /// The file's text; invalid UTF-8 is replaced.
    pub(crate) text: String,
}

/// Something in the memory folder that is not a memory file of the layout:
/// a badly named file, a file without the `.md` extension, a folder where a
/// file belongs, or anything nested deeper.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IgnoredEntry {
    /// The path relative to the memory folder, with `/` separators.
    pub(crate) path: String,
    pub(crate) reason: &'static str,
}

/// Which part of memory to list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ListScope {
    All,
    Layer(MemoryLayer),
    // `memory.list { agent }` lists one agent's file.
    #[allow(dead_code)]
    Agent(AgentId),
    Machine(MachineName),
    // `memory.list { project }` lists one project.
    #[allow(dead_code)]
    Project(MemoryName),
}

/// What [`MemoryRoot::list`] found.
#[derive(Clone, Debug, Default)]
pub(crate) struct MemoryListing {
    /// Memory files, valid or not, in layer order and then by URL.
    pub(crate) entries: Vec<MemoryEntry>,
    /// Entries that are not memory files, by path.
    pub(crate) ignored: Vec<IgnoredEntry>,
}

const NOT_MARKDOWN: &str = "not a `.md` file";
const BAD_NAME: &str = "the name breaks the naming rule";
const BAD_AGENT: &str = "the name is not an agent ID";
const BAD_MACHINE: &str = "the folder name is not a machine name";
const NESTED: &str = "nested deeper than the memory layout";
const NOT_FILE: &str = "not a regular file";
const NOT_FOLDER: &str = "not a folder";
const UNKNOWN_TOP: &str = "not part of the memory layout";

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> MemoryError + '_ {
    move |source| MemoryError::Io {
        path: path.to_owned(),
        source,
    }
}

fn not_found(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// Reads a whole file, or `None` when nothing is at `path`. Something other
/// than a regular file, a symbolic link included, reads as no bytes and
/// parses as invalid.
fn read_bytes(path: &Path) -> Result<Option<(Vec<u8>, fs::Metadata)>, MemoryError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if not_found(&error) => return Ok(None),
        Err(source) => return Err(io_error(path)(source)),
    };
    if !metadata.is_file() {
        return Ok(Some((Vec::new(), metadata)));
    }
    let bytes = fs::read(path).map_err(io_error(path))?;
    Ok(Some((bytes, metadata)))
}

fn entry(
    url: MemoryUrl,
    bytes: &[u8],
    metadata: &fs::Metadata,
    max_file_bytes: usize,
) -> MemoryEntry {
    let document = if metadata.is_file() {
        MemoryDocument::parse(bytes, max_file_bytes)
    } else {
        Err(InvalidMemory(NOT_FILE.to_owned()))
    };
    MemoryEntry {
        url,
        revision: MemoryRevision::of(bytes),
        size: bytes.len() as u64,
        modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        document,
    }
}

/// Reads a memory file for listing and search. A file over the limit is not
/// read whole: it is invalid, and its revision is hashed while streaming.
fn load(path: &Path, url: MemoryUrl, max_file_bytes: usize) -> Result<MemoryEntry, MemoryError> {
    let metadata = fs::metadata(path).map_err(io_error(path))?;
    if metadata.len() > max_file_bytes as u64 {
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        let mut file = fs::File::open(path).map_err(io_error(path))?;
        let size = io::copy(&mut file, &mut hasher).map_err(io_error(path))?;
        return Ok(MemoryEntry {
            url,
            revision: MemoryRevision::from_digest(hasher.finalize().as_slice()),
            size,
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            document: Err(too_large(
                usize::try_from(size).unwrap_or(usize::MAX),
                max_file_bytes,
            )),
        });
    }
    let bytes = fs::read(path).map_err(io_error(path))?;
    Ok(entry(url, &bytes, &metadata, max_file_bytes))
}

/// The stem of a `<name>.md` file name, or why it is not a memory file.
fn memory_stem(file_name: &str) -> Result<&str, &'static str> {
    file_name
        .strip_suffix(&format!(".{MEMORY_EXTENSION}"))
        .ok_or(NOT_MARKDOWN)
}

fn relative(path: &[&str]) -> String {
    path.join("/")
}

/// The sorted entries of a folder that exists; none when it does not.
fn read_dir_sorted(path: &Path) -> Result<Vec<(String, fs::FileType)>, MemoryError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if not_found(&error) => return Ok(Vec::new()),
        Err(source) => return Err(io_error(path)(source)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_error(path))?;
        let kind = entry.file_type().map_err(io_error(path))?;
        found.push((entry.file_name().to_string_lossy().into_owned(), kind));
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(found)
}

impl MemoryRoot {
    fn file_path(&self, url: &MemoryUrl) -> PathBuf {
        self.root.join(url.relative_path())
    }

    /// Reads one memory whole: its text, revision, and parsed content.
    pub(crate) fn read(
        &self,
        url: &MemoryUrl,
        max_file_bytes: usize,
    ) -> Result<MemoryRead, StoreError> {
        let path = self.file_path(url);
        let (bytes, metadata) =
            read_bytes(&path)?.ok_or_else(|| StoreError::NotFound { url: url.clone() })?;
        Ok(MemoryRead {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            entry: entry(url.clone(), &bytes, &metadata, max_file_bytes),
        })
    }

    /// Every memory file in `scope`, valid or not, and what else lies in
    /// its folders.
    pub(crate) fn list(
        &self,
        scope: &ListScope,
        max_file_bytes: usize,
    ) -> Result<MemoryListing, MemoryError> {
        let mut listing = MemoryListing::default();
        let wants = |layer: MemoryLayer| match scope {
            ListScope::All => true,
            ListScope::Layer(only) => *only == layer,
            ListScope::Agent(_) => layer == MemoryLayer::Agent,
            ListScope::Machine(_) => layer == MemoryLayer::Machine,
            ListScope::Project(_) => layer == MemoryLayer::Project,
        };
        if *scope == ListScope::All {
            self.list_top(&mut listing)?;
        }
        if wants(MemoryLayer::User) {
            self.list_flat(MemoryLayer::User, &mut listing, max_file_bytes)?;
        }
        if wants(MemoryLayer::Agent) {
            self.list_flat(MemoryLayer::Agent, &mut listing, max_file_bytes)?;
        }
        for layer in [MemoryLayer::Machine, MemoryLayer::Project] {
            if wants(layer) {
                let only = match scope {
                    ListScope::Machine(machine) => Some(machine.as_str()),
                    ListScope::Project(project) => Some(project.as_str()),
                    _ => None,
                };
                self.list_grouped(layer, only, &mut listing, max_file_bytes)?;
            }
        }
        if let ListScope::Agent(agent) = scope {
            listing.entries.retain(
                |entry| matches!(&entry.url, MemoryUrl::Agent { agent: found } if found == agent),
            );
            let path = format!("{}/{agent}.{MEMORY_EXTENSION}", MemoryLayer::Agent.dir());
            listing.ignored.retain(|ignored| ignored.path == path);
        }
        Ok(listing)
    }

    /// Entries directly in the memory folder that are not part of the
    /// layout.
    fn list_top(&self, listing: &mut MemoryListing) -> Result<(), MemoryError> {
        for (name, kind) in read_dir_sorted(&self.root)? {
            if MemoryLayer::ALL.iter().any(|layer| layer.dir() == name) {
                if !kind.is_dir() {
                    listing.ignored.push(IgnoredEntry {
                        path: name,
                        reason: NOT_FOLDER,
                    });
                }
            } else if name != super::LOCK_FILE {
                listing.ignored.push(IgnoredEntry {
                    path: name,
                    reason: UNKNOWN_TOP,
                });
            }
        }
        Ok(())
    }

    /// `user/<category>.md` or `agents/<agent>.md`.
    fn list_flat(
        &self,
        layer: MemoryLayer,
        listing: &mut MemoryListing,
        max_file_bytes: usize,
    ) -> Result<(), MemoryError> {
        let dir = self.root.join(layer.dir());
        for (file_name, kind) in read_dir_sorted(&dir)? {
            let path = relative(&[layer.dir(), &file_name]);
            let url = memory_stem(&file_name).and_then(|stem| match layer {
                MemoryLayer::User => MemoryName::new(stem)
                    .map(|category| MemoryUrl::User { category })
                    .ok_or(BAD_NAME),
                _ => MemoryUrl::parse(&format!("agent://{stem}")).map_err(|_| BAD_AGENT),
            });
            self.push(
                listing,
                path,
                &kind,
                url,
                &dir.join(&file_name),
                max_file_bytes,
            )?;
        }
        Ok(())
    }

    /// `machines/<machine>/<name>.md` or `projects/<project>/<name>.md`.
    fn list_grouped(
        &self,
        layer: MemoryLayer,
        only: Option<&str>,
        listing: &mut MemoryListing,
        max_file_bytes: usize,
    ) -> Result<(), MemoryError> {
        let dir = self.root.join(layer.dir());
        for (group, kind) in read_dir_sorted(&dir)? {
            if only.is_some_and(|only| only != group) {
                continue;
            }
            let group_path = relative(&[layer.dir(), &group]);
            if !kind.is_dir() {
                listing.ignored.push(IgnoredEntry {
                    path: group_path,
                    reason: NOT_FOLDER,
                });
                continue;
            }
            let valid_group = match layer {
                MemoryLayer::Machine => MachineName::new(group.as_str()).is_ok(),
                _ => MemoryName::new(group.as_str()).is_some(),
            };
            if !valid_group {
                listing.ignored.push(IgnoredEntry {
                    path: group_path,
                    reason: if layer == MemoryLayer::Machine {
                        BAD_MACHINE
                    } else {
                        BAD_NAME
                    },
                });
                continue;
            }
            let group_dir = dir.join(&group);
            for (file_name, kind) in read_dir_sorted(&group_dir)? {
                let path = relative(&[layer.dir(), &group, &file_name]);
                let url = memory_stem(&file_name).and_then(|stem| {
                    MemoryUrl::parse(&format!("{layer}://{group}/{stem}")).map_err(|_| BAD_NAME)
                });
                self.push(
                    listing,
                    path,
                    &kind,
                    url,
                    &group_dir.join(&file_name),
                    max_file_bytes,
                )?;
            }
        }
        Ok(())
    }

    fn push(
        &self,
        listing: &mut MemoryListing,
        path: String,
        kind: &fs::FileType,
        url: Result<MemoryUrl, &'static str>,
        file: &Path,
        max_file_bytes: usize,
    ) -> Result<(), MemoryError> {
        if kind.is_dir() {
            listing.ignored.push(IgnoredEntry {
                path,
                reason: NESTED,
            });
            return Ok(());
        }
        if !kind.is_file() {
            listing.ignored.push(IgnoredEntry {
                path,
                reason: NOT_FILE,
            });
            return Ok(());
        }
        match url {
            Ok(url) => match load(file, url, max_file_bytes) {
                Ok(entry) => listing.entries.push(entry),
                // Removed since the folder was read.
                Err(MemoryError::Io { source, .. }) if not_found(&source) => {}
                Err(error) => return Err(error),
            },
            Err(reason) => listing.ignored.push(IgnoredEntry { path, reason }),
        }
        Ok(())
    }

    /// Creates or replaces one memory from its whole file text, which must
    /// be a valid memory within `max_file_bytes`. Without `expected` the
    /// file must not exist; with it, the file must exist at that revision.
    /// Creating a `user://` file is refused for [`Origin::Model`]; creating
    /// an `agent://` file needs `agent_exists` to accept its agent ID. The
    /// first memory of a project or machine creates its folder. Returns the
    /// new revision.
    pub(crate) fn write(
        &self,
        url: &MemoryUrl,
        text: &str,
        expected: Option<&MemoryRevision>,
        origin: Origin,
        agent_exists: impl FnOnce(&AgentId) -> bool,
        max_file_bytes: usize,
    ) -> Result<MemoryRevision, StoreError> {
        MemoryDocument::parse(text.as_bytes(), max_file_bytes).map_err(|reason| {
            StoreError::Invalid {
                url: url.clone(),
                reason,
            }
        })?;
        let lock = self.lock()?;
        let path = self.file_path(url);
        let current = read_bytes(&path)?.map(|(bytes, _)| MemoryRevision::of(&bytes));
        match (current, expected) {
            (Some(revision), None) => {
                return Err(StoreError::Exists {
                    url: url.clone(),
                    revision,
                });
            }
            (None, Some(_)) => return Err(StoreError::NotFound { url: url.clone() }),
            (Some(current), Some(expected)) if current != *expected => {
                return Err(StoreError::RevisionMismatch {
                    url: url.clone(),
                    current,
                });
            }
            (Some(_), Some(_)) => {}
            (None, None) => match url {
                MemoryUrl::User { .. } if origin == Origin::Model => {
                    return Err(StoreError::Refused(format!(
                        "{url} does not exist, and only the user adds user categories"
                    )));
                }
                MemoryUrl::Agent { agent } if !agent_exists(agent) => {
                    return Err(StoreError::Refused(format!(
                        "{url} names no agent in the agent registry"
                    )));
                }
                _ => {}
            },
        }
        let revision = write_file(&path, text.as_bytes())?;
        drop(lock);
        Ok(revision)
    }

    /// Replaces the one occurrence of `old` in the body of a memory at
    /// revision `expected` with `new`. Fails, changing nothing, when `old`
    /// occurs zero or several times or the result is not a valid memory.
    /// The file is written in the form [`MemoryDocument::render`] gives.
    pub(crate) fn edit(
        &self,
        url: &MemoryUrl,
        old: &str,
        new: &str,
        expected: &MemoryRevision,
        max_file_bytes: usize,
    ) -> Result<MemoryRevision, StoreError> {
        let lock = self.lock()?;
        let path = self.file_path(url);
        let (bytes, _) =
            read_bytes(&path)?.ok_or_else(|| StoreError::NotFound { url: url.clone() })?;
        let current = MemoryRevision::of(&bytes);
        if current != *expected {
            return Err(StoreError::RevisionMismatch {
                url: url.clone(),
                current,
            });
        }
        let invalid = |reason| StoreError::Invalid {
            url: url.clone(),
            reason,
        };
        let document = MemoryDocument::parse(&bytes, max_file_bytes).map_err(invalid)?;
        let count = if old.is_empty() {
            0
        } else {
            document.body().matches(old).count()
        };
        match count {
            0 => {
                return Err(StoreError::NoOccurrence {
                    url: url.clone(),
                    old: old.to_owned(),
                });
            }
            1 => {}
            count => {
                return Err(StoreError::SeveralOccurrences {
                    url: url.clone(),
                    old: old.to_owned(),
                    count,
                });
            }
        }
        let edited = document
            .with_body(document.body().replacen(old, new, 1))
            .map_err(invalid)?;
        let text = edited.render();
        if text.len() > max_file_bytes {
            return Err(invalid(too_large(text.len(), max_file_bytes)));
        }
        let revision = write_file(&path, text.as_bytes())?;
        drop(lock);
        Ok(revision)
    }

    /// Deletes a memory at revision `expected`. Deleting a `user://` or
    /// `agent://` file is refused for [`Origin::Model`]. Deleting the last
    /// memory of a project removes the project's folder.
    pub(crate) fn delete(
        &self,
        url: &MemoryUrl,
        expected: &MemoryRevision,
        origin: Origin,
    ) -> Result<(), StoreError> {
        if origin == Origin::Model && matches!(url.layer(), MemoryLayer::User | MemoryLayer::Agent)
        {
            return Err(StoreError::Refused(format!(
                "{url} can only be deleted by the user; only machine:// and project:// memories can be deleted here"
            )));
        }
        let lock = self.lock()?;
        let path = self.file_path(url);
        let (bytes, _) =
            read_bytes(&path)?.ok_or_else(|| StoreError::NotFound { url: url.clone() })?;
        let current = MemoryRevision::of(&bytes);
        if current != *expected {
            return Err(StoreError::RevisionMismatch {
                url: url.clone(),
                current,
            });
        }
        fs::remove_file(&path).map_err(io_error(&path))?;
        if url.layer() == MemoryLayer::Project {
            let folder = self.root.join(url.relative_dir());
            // Fails, keeping the folder, while anything is left in it.
            let _ = fs::remove_dir(&folder);
        }
        drop(lock);
        Ok(())
    }
}

/// Writes `bytes` to `path` through a flushed temporary file in the same
/// folder renamed over it, creating the folder first. Returns the new
/// revision.
fn write_file(path: &Path, bytes: &[u8]) -> Result<MemoryRevision, MemoryError> {
    let directory = path.parent().expect("memory files have a folder");
    fs::create_dir_all(directory).map_err(io_error(directory))?;
    let file_name = path
        .file_name()
        .expect("memory files have a name")
        .to_string_lossy();
    let temporary = directory.join(format!(".{file_name}.{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let mut output = fs::File::create(&temporary)?;
        io::Write::write_all(&mut output, bytes)?;
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
    result.map_err(io_error(path))?;
    Ok(MemoryRevision::of(bytes))
}

#[cfg(test)]
mod tests;
