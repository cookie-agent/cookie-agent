//! Reading, listing, and changing memory files (`docs/site/specs/memory.md`
//! sections 4 and 8).
//!
//! Reads never take the lock: every write replaces a file by renaming a
//! flushed temporary file over it, so a reader sees the old or the new file
//! whole. On Windows that rename can leave the name briefly unresolvable,
//! so reads go through `secure_store::read_replaced` and its kin. Writes, edits, and deletes take [`super::MemoryLock`] for their
//! whole check-and-change, and may wait for it up to a few seconds, so
//! async callers run them on a blocking thread.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use cookie_agent_identity::AgentId;
use cookie_agent_models::secure_store::{read_replaced, while_replacing as replaced};
use cookie_agent_protocol::MachineName;

use super::{
    MEMORY_EXTENSION, MemoryError, MemoryRoot,
    agents::{AgentMemory, with_frontmatter},
    file::{InvalidMemory, MemoryDocument, MemoryRevision, too_large},
    url::{MemoryLayer, MemoryName, MemoryUrl},
};

/// What the store is told about the owners of a URL: the agent of an
/// `agent://` URL and whether the project of a `project://` URL exists.
/// Asked while the memory lock is held.
pub(crate) trait MemoryOwners {
    /// What the store knows about `agent`.
    fn agent(&self, agent: &AgentId) -> AgentMemory;

    /// `None` when `project` is a project; otherwise the names of the
    /// projects there are, in name order.
    fn unknown_project(&self, project: &MemoryName) -> Option<Vec<String>> {
        let _ = project;
        None
    }
}

/// A function of the agent alone knows every project.
impl<F: Fn(&AgentId) -> AgentMemory> MemoryOwners for F {
    fn agent(&self, agent: &AgentId) -> AgentMemory {
        self(agent)
    }
}

/// Why a write to a project that does not exist is refused
/// (`docs/site/specs/projects.md` section 3).
fn unknown_project(url: &MemoryUrl, project: &MemoryName, projects: &[String]) -> StoreError {
    let existing = if projects.is_empty() {
        "There are no projects yet".to_owned()
    } else {
        format!("The projects are {}", projects.join(", "))
    };
    StoreError::Refused(format!(
        "{url} names `{project}`, which is no project. {existing}; projects are created from a client, not by writing memory"
    ))
}

/// Who asks for a change. Model tools may not delete `user://` or
/// `agent://` files, whose bodies they clear instead
/// ([`MemoryRoot::clear_body`]), nor create `user://` files; the user,
/// through a client, may (sections 8 and 10).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Origin {
    Model,
    /// The client surface (`memory.*`).
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

/// What a write of an `agent://` file kept in place of what it was given
/// (section 16.7): the daemon owns an agent file's description and its two
/// keywords, and an agent file the daemon does not keep keeps the
/// description it has.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct KeptMetadata {
    /// The description the file keeps, when the text gave another.
    pub(crate) description: Option<String>,
    /// The daemon's keywords the text left out, added back.
    pub(crate) keywords: Vec<String>,
}

impl KeptMetadata {
    /// What was kept, in a sentence or two for the writer of `url`.
    pub(crate) fn describe(&self, url: &MemoryUrl) -> String {
        let mut parts = Vec::new();
        if let Some(description) = &self.description {
            parts.push(format!(
                "The description of {url} is the daemon's and cannot be changed, so it stays \"{description}\"."
            ));
        }
        if !self.keywords.is_empty() {
            let keywords = self
                .keywords
                .iter()
                .map(|keyword| format!("`{keyword}`"))
                .collect::<Vec<_>>()
                .join(" and ");
            parts.push(format!(
                "Its keywords always include {keywords}, so they were added back; other keywords are kept."
            ));
        }
        parts.join(" ")
    }
}

/// A successful write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Written {
    pub(crate) revision: MemoryRevision,
    /// What an `agent://` write kept of the file's frontmatter; `None` when
    /// the text was written as given.
    pub(crate) kept: Option<KeptMetadata>,
}

/// What [`MemoryRoot::clear_body`] did, with the file's revision after it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ClearedBody {
    /// The body was removed; the frontmatter stays as it was.
    Cleared(MemoryRevision),
    /// The body was already empty, so nothing was written.
    AlreadyEmpty(MemoryRevision),
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
    Agent(AgentId),
    Machine(MachineName),
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

pub(super) fn io_error(path: &Path) -> impl FnOnce(io::Error) -> MemoryError + '_ {
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
pub(super) fn read_bytes(path: &Path) -> Result<Option<(Vec<u8>, fs::Metadata)>, MemoryError> {
    let metadata = match replaced(path, || fs::symlink_metadata(path)) {
        Ok(metadata) => metadata,
        Err(error) if not_found(&error) => return Ok(None),
        Err(source) => return Err(io_error(path)(source)),
    };
    if !metadata.is_file() {
        return Ok(Some((Vec::new(), metadata)));
    }
    let bytes = read_replaced(path).map_err(io_error(path))?;
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
    let metadata = replaced(path, || fs::metadata(path)).map_err(io_error(path))?;
    if metadata.len() > max_file_bytes as u64 {
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        let mut file = replaced(path, || fs::File::open(path)).map_err(io_error(path))?;
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
    let bytes = read_replaced(path).map_err(io_error(path))?;
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
pub(super) fn read_dir_sorted(path: &Path) -> Result<Vec<(String, fs::FileType)>, MemoryError> {
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
    pub(super) fn file_path(&self, url: &MemoryUrl) -> PathBuf {
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
    /// an `agent://` file needs `agents` to know its agent. An `agent://`
    /// file keeps the frontmatter the daemon owns (section 16.7): the
    /// daemon's description and keywords for an agent it keeps a file for,
    /// and the description the file has otherwise; the text's own
    /// frontmatter is replaced when it differs, its body kept as given.
    /// A project's overview keeps the daemon's description the same way.
    /// A write to a project that does not exist is refused; the first
    /// memory of a machine creates its folder.
    pub(crate) fn write(
        &self,
        url: &MemoryUrl,
        text: &str,
        expected: Option<&MemoryRevision>,
        origin: Origin,
        owners: impl MemoryOwners,
        max_file_bytes: usize,
    ) -> Result<Written, StoreError> {
        let invalid = |reason| StoreError::Invalid {
            url: url.clone(),
            reason,
        };
        let document = MemoryDocument::parse(text.as_bytes(), max_file_bytes).map_err(invalid)?;
        let lock = self.lock()?;
        if let MemoryUrl::Project { project, .. } = url
            && let Some(projects) = owners.unknown_project(project)
        {
            return Err(unknown_project(url, project, &projects));
        }
        let path = self.file_path(url);
        let existing = read_bytes(&path)?.map(|(bytes, _)| bytes);
        let current = existing.as_deref().map(MemoryRevision::of);
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
            (None, None) if matches!(url, MemoryUrl::User { .. }) && origin == Origin::Model => {
                return Err(StoreError::Refused(format!(
                    "{url} does not exist, and only the user adds user categories"
                )));
            }
            (Some(_), Some(_)) | (None, None) => {}
        }
        let mut text = std::borrow::Cow::Borrowed(text);
        let mut kept = None;
        let target = match url {
            MemoryUrl::Agent { agent } => {
                let status = owners.agent(agent);
                if existing.is_none() && !status.exists() {
                    return Err(StoreError::Refused(format!(
                        "{url} names no agent in the agent registry"
                    )));
                }
                match &status {
                    AgentMemory::Managed(spec) => {
                        Some(spec.apply(agent, &document).map_err(invalid)?)
                    }
                    AgentMemory::Registered | AgentMemory::Unknown => existing
                        .as_deref()
                        .and_then(|bytes| MemoryDocument::parse(bytes, max_file_bytes).ok())
                        .map(|current| {
                            MemoryDocument::new(
                                current.description(),
                                document.keywords().to_vec(),
                                document.body(),
                            )
                        })
                        .transpose()
                        .map_err(invalid)?,
                }
            }
            MemoryUrl::Project { project, .. } if super::projects::is_overview(url) => {
                Some(super::projects::managed_overview(project, &document).map_err(invalid)?)
            }
            _ => None,
        };
        if let Some(target) = target.filter(|target| *target != document) {
            let added = target
                .keywords()
                .iter()
                .filter(|keyword| {
                    !document
                        .keywords()
                        .iter()
                        .any(|given| given.to_lowercase() == keyword.to_lowercase())
                })
                .cloned()
                .collect::<Vec<_>>();
            let metadata = KeptMetadata {
                description: (target.description() != document.description())
                    .then(|| target.description().to_owned()),
                keywords: added,
            };
            let replaced = with_frontmatter(&text, &target).ok_or_else(|| {
                invalid(InvalidMemory("the frontmatter was not found".to_owned()))
            })?;
            if replaced.len() > max_file_bytes {
                return Err(invalid(too_large(replaced.len(), max_file_bytes)));
            }
            text = std::borrow::Cow::Owned(replaced);
            kept = (metadata != KeptMetadata::default()).then_some(metadata);
        }
        let revision = write_file(&path, text.as_bytes())?;
        drop(lock);
        self.changed();
        Ok(Written { revision, kept })
    }

    /// Replaces the one occurrence of `old` in the body of a memory at
    /// revision `expected` with `new`. Fails, changing nothing, when `old`
    /// occurs zero or several times or the result is not a valid memory.
    /// The file is written in the form [`MemoryDocument::render`] gives,
    /// with the daemon's description and keywords for an agent file the
    /// daemon keeps (section 16.7).
    pub(crate) fn edit(
        &self,
        url: &MemoryUrl,
        old: &str,
        new: &str,
        expected: &MemoryRevision,
        owners: impl MemoryOwners,
        max_file_bytes: usize,
    ) -> Result<MemoryRevision, StoreError> {
        let lock = self.lock()?;
        if let MemoryUrl::Project { project, .. } = url
            && let Some(projects) = owners.unknown_project(project)
        {
            return Err(unknown_project(url, project, &projects));
        }
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
        let mut edited = document
            .with_body(document.body().replacen(old, new, 1))
            .map_err(invalid)?;
        if let MemoryUrl::Agent { agent } = url
            && let AgentMemory::Managed(spec) = owners.agent(agent)
        {
            edited = spec.apply(agent, &edited).map_err(invalid)?;
        }
        if let MemoryUrl::Project { project, .. } = url
            && super::projects::is_overview(url)
        {
            edited = super::projects::managed_overview(project, &edited).map_err(invalid)?;
        }
        let text = edited.render();
        if text.len() > max_file_bytes {
            return Err(invalid(too_large(text.len(), max_file_bytes)));
        }
        let revision = write_file(&path, text.as_bytes())?;
        drop(lock);
        self.changed();
        Ok(revision)
    }

    /// Removes the body of a valid memory at revision `expected`, keeping
    /// the file and its frontmatter byte for byte. This is what a model
    /// tool's delete of a `user://` or `agent://` memory does
    /// (section 16.6). A body that is already empty is left alone, and
    /// nothing is written or signalled.
    pub(crate) fn clear_body(
        &self,
        url: &MemoryUrl,
        expected: &MemoryRevision,
        max_file_bytes: usize,
    ) -> Result<ClearedBody, StoreError> {
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
        MemoryDocument::parse(&bytes, max_file_bytes).map_err(invalid)?;
        let frontmatter = std::str::from_utf8(&bytes)
            .ok()
            .and_then(MemoryDocument::frontmatter_len)
            .ok_or_else(|| invalid(InvalidMemory("the frontmatter was not found".to_owned())))?;
        if frontmatter == bytes.len() {
            return Ok(ClearedBody::AlreadyEmpty(current));
        }
        let revision = write_file(&path, &bytes[..frontmatter])?;
        drop(lock);
        self.changed();
        Ok(ClearedBody::Cleared(revision))
    }

    /// Deletes a memory at revision `expected`. Deleting a `user://` or
    /// `agent://` file is refused for [`Origin::Model`], whose deletes of
    /// those clear the body instead ([`Self::clear_body`]). A project's
    /// folder stays when its last memory goes: it belongs to the project.
    pub(crate) fn delete(
        &self,
        url: &MemoryUrl,
        expected: &MemoryRevision,
        origin: Origin,
    ) -> Result<(), StoreError> {
        if origin == Origin::Model && matches!(url.layer(), MemoryLayer::User | MemoryLayer::Agent)
        {
            return Err(StoreError::Refused(format!(
                "{url} can only be deleted by the user; a model tool clears its body instead"
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
        drop(lock);
        self.changed();
        Ok(())
    }
}

/// Writes `bytes` to `path` through a flushed temporary file in the same
/// folder renamed over it, creating the folder first. Returns the new
/// revision.
pub(super) fn write_file(path: &Path, bytes: &[u8]) -> Result<MemoryRevision, MemoryError> {
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
