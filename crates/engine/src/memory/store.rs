//! Reading, listing, and changing memory files (`docs/site/specs/memory.md`
//! sections 4 and 8, as section 16.9 amends them: stamps, size limits per
//! kind of memory, model writes without revisions, and the similar-memory
//! check).
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

use jiff::Timestamp;

use cookie_agent_identity::AgentId;
use cookie_agent_models::secure_store::{read_replaced, while_replacing as replaced};
use cookie_agent_protocol::MachineName;

use super::{
    MEMORY_EXTENSION, MemoryError, MemoryRoot,
    agents::{AgentMemory, with_frontmatter},
    file::{InvalidMemory, MAX_FILE_BYTES, MemoryDocument, MemoryRevision, to_second, too_large},
    format,
    limits::MemoryLimits,
    similar::{NearMemory, Similarity},
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

/// How a write checks the memory it replaces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteMode<'a> {
    /// A client's `memory.write` (section 10): without a revision the
    /// memory must not exist; with one, it must exist at that revision.
    Client(Option<&'a MemoryRevision>),
    /// A model's `memory_write` (section 16.9.8): replacing a memory needs
    /// `seen`, the revision the session last saw of it, to be its current
    /// one. A new memory is first compared with the memories around it
    /// (section 16.9.5); `new` says the writer already looked, so only an
    /// identical body refuses it.
    Model {
        seen: Option<&'a MemoryRevision>,
        new: bool,
    },
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
    /// An edit's text occurs `count` times in the body, other than once.
    #[error(
        "{}",
        cookie_agent_runtime_api::text_tools::occurrence_error(old, *count, &format!("the body of {url}"), false)
    )]
    Occurrences {
        url: MemoryUrl,
        old: String,
        count: usize,
    },
    /// A model's replacing write of a memory its session has not seen as
    /// it is now. The message leaves the memory's content out: following
    /// a write, it would read as what the write left.
    #[error(
        "{url} was not written: this memory changed since you saw it, or you have not read it. Read it with memory_read, then write it again, or change it with memory_edit."
    )]
    Unseen { url: MemoryUrl },
    /// The change would leave the memory over its limit. The message
    /// leaves the memory's content out: following a write, it would read
    /// as what the write left.
    #[error("{}", too_large_message(url, *size, *max, *current))]
    TooLarge {
        url: MemoryUrl,
        size: usize,
        max: usize,
        /// The memory's current size in bytes, when it exists.
        current: Option<usize>,
    },
    /// A new memory looks like memories that exist.
    #[error("{}", similar_message(url, near))]
    Similar {
        url: MemoryUrl,
        near: Vec<NearMemory>,
    },
    /// A new memory has the body of one that exists.
    #[error("{url} was not created: {existing} already holds exactly this body")]
    Identical { url: MemoryUrl, existing: MemoryUrl },
}

/// Fails unless `old`, not empty, occurs exactly once in `body`, the body
/// of the memory at `url`.
fn occurs_once(url: &MemoryUrl, body: &str, old: &str) -> Result<(), StoreError> {
    let count = if old.is_empty() {
        0
    } else {
        body.matches(old).count()
    };
    if count == 1 {
        return Ok(());
    }
    Err(StoreError::Occurrences {
        url: url.clone(),
        old: old.to_owned(),
        count,
    })
}

fn too_large_message(url: &MemoryUrl, size: usize, max: usize, current: Option<usize>) -> String {
    let (size_text, max_text) = (format::size(size as u64), format::size(max as u64));
    match current {
        None => format!(
            "{url} was not created: it would be {size_text}, over its limit of {max_text}. Make it shorter."
        ),
        Some(current) if current > max => format!(
            "{url} was not written: it is {}, already over its limit of {max_text}, and only a change that makes it smaller is accepted until it fits; this one would leave it at {size_text}. Condense it first: read it with memory_read, then write a shorter version.",
            format::size(current as u64)
        ),
        Some(_) => format!(
            "{url} was not written: it would be {size_text}, over its limit of {max_text}. Condense it first: read it with memory_read, then merge or drop what it holds."
        ),
    }
}

fn similar_message(url: &MemoryUrl, near: &[NearMemory]) -> String {
    let listed = near
        .iter()
        .map(|near| format!("- {} — {}", near.url, near.description))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{url} was not created: memories like it already exist:\n{listed}\nIf one of these covers it, edit that instead. If it is new, write it again with `new: true` and a one-line `reason`."
    )
}

/// A memory file as found on disk.
#[derive(Clone, Debug)]
pub(crate) struct MemoryEntry {
    pub(crate) url: MemoryUrl,
    pub(crate) revision: MemoryRevision,
    /// The file's size in bytes, which `memory.list` reports.
    pub(crate) size: u64,
    /// When the memory was created and last modified: its stamps, or the
    /// file's modification time for a file without them (section 16.9.6).
    pub(crate) created: Timestamp,
    pub(crate) modified: Timestamp,
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
    /// The write created the memory.
    pub(crate) created: bool,
    /// Memories near a new one (section 16.9.5), for the writer to check.
    pub(crate) near: Vec<NearMemory>,
    /// The file the write replaced, as it was; `None` for a new memory.
    pub(crate) replaced: Option<String>,
    /// The file as the write left it.
    pub(crate) text: String,
}

/// A successful edit: the file before and after it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Edited {
    pub(crate) revision: MemoryRevision,
    pub(crate) before: String,
    pub(crate) after: String,
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

/// The modification time of the file `metadata` describes, to the second.
fn modified_time(metadata: &fs::Metadata) -> Timestamp {
    let time = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    to_second(Timestamp::try_from(time).unwrap_or(Timestamp::UNIX_EPOCH))
}

fn entry(url: MemoryUrl, bytes: &[u8], metadata: &fs::Metadata) -> MemoryEntry {
    let document = if metadata.is_file() {
        MemoryDocument::parse(bytes)
    } else {
        Err(InvalidMemory(NOT_FILE.to_owned()))
    };
    let file_time = modified_time(metadata);
    let stamps = document.as_ref().ok();
    MemoryEntry {
        url,
        revision: MemoryRevision::of(bytes),
        size: bytes.len() as u64,
        created: stamps
            .and_then(MemoryDocument::created)
            .unwrap_or(file_time),
        modified: stamps
            .and_then(MemoryDocument::modified)
            .unwrap_or(file_time),
        document,
    }
}

/// Reads a memory file for listing and search. A file larger than any
/// memory may be is not read whole: it is invalid, and its revision is
/// hashed while streaming.
fn load(path: &Path, url: MemoryUrl) -> Result<MemoryEntry, MemoryError> {
    let metadata = replaced(path, || fs::metadata(path)).map_err(io_error(path))?;
    if metadata.len() > MAX_FILE_BYTES as u64 {
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        let mut file = replaced(path, || fs::File::open(path)).map_err(io_error(path))?;
        let size = io::copy(&mut file, &mut hasher).map_err(io_error(path))?;
        let file_time = modified_time(&metadata);
        return Ok(MemoryEntry {
            url,
            revision: MemoryRevision::from_digest(hasher.finalize().as_slice()),
            size,
            created: file_time,
            modified: file_time,
            document: Err(too_large(usize::try_from(size).unwrap_or(usize::MAX))),
        });
    }
    let bytes = read_replaced(path).map_err(io_error(path))?;
    Ok(entry(url, &bytes, &metadata))
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
    pub(crate) fn read(&self, url: &MemoryUrl) -> Result<MemoryRead, StoreError> {
        let path = self.file_path(url);
        let (bytes, metadata) =
            read_bytes(&path)?.ok_or_else(|| StoreError::NotFound { url: url.clone() })?;
        Ok(MemoryRead {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            entry: entry(url.clone(), &bytes, &metadata),
        })
    }

    /// Every memory file in `scope`, valid or not, and what else lies in
    /// its folders.
    pub(crate) fn list(&self, scope: &ListScope) -> Result<MemoryListing, MemoryError> {
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
            self.list_flat(MemoryLayer::User, &mut listing)?;
        }
        if wants(MemoryLayer::Agent) {
            self.list_flat(MemoryLayer::Agent, &mut listing)?;
        }
        for layer in [MemoryLayer::Machine, MemoryLayer::Project] {
            if wants(layer) {
                let only = match scope {
                    ListScope::Machine(machine) => Some(machine.as_str()),
                    ListScope::Project(project) => Some(project.as_str()),
                    _ => None,
                };
                self.list_grouped(layer, only, &mut listing)?;
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
            } else if name != super::LOCK_FILE && name != super::READS_FILE {
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
            self.push(listing, path, &kind, url, &dir.join(&file_name))?;
        }
        Ok(())
    }

    /// `machines/<machine>/<name>.md` or `projects/<project>/<name>.md`.
    fn list_grouped(
        &self,
        layer: MemoryLayer,
        only: Option<&str>,
        listing: &mut MemoryListing,
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
                self.push(listing, path, &kind, url, &group_dir.join(&file_name))?;
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
            Ok(url) => match load(file, url) {
                Ok(entry) => listing.entries.push(entry),
                // Removed since the folder was read.
                Err(MemoryError::Io { source, .. }) if not_found(&source) => {}
                Err(error) => return Err(error),
            },
            Err(reason) => listing.ignored.push(IgnoredEntry { path, reason }),
        }
        Ok(())
    }

    /// The valid memories in the same place as `url` (section 16.9.5): the
    /// same project, the same machine, the user categories, or the agent's
    /// file; `url` itself left out.
    fn neighbours(&self, url: &MemoryUrl) -> Result<Vec<(MemoryUrl, MemoryDocument)>, MemoryError> {
        let scope = match url {
            MemoryUrl::User { .. } => ListScope::Layer(MemoryLayer::User),
            MemoryUrl::Agent { agent } => ListScope::Agent(agent.clone()),
            MemoryUrl::Machine { machine, .. } => ListScope::Machine(machine.clone()),
            MemoryUrl::Project { project, .. } => ListScope::Project(project.clone()),
        };
        Ok(self
            .list(&scope)?
            .entries
            .into_iter()
            .filter(|entry| entry.url != *url)
            .filter_map(|entry| Some((entry.url, entry.document.ok()?)))
            .collect())
    }

    /// Creates or replaces one memory from its whole file text, which must
    /// be a valid memory. `mode` says how the memory it replaces is checked
    /// ([`WriteMode`]). The daemon stamps the file (section 16.9.6): a new
    /// memory is created now, a replaced one keeps when it was created,
    /// and either is modified now; stamps the text gives are ignored. The
    /// result must fit the memory's limit in `limits`, unless the memory
    /// is already over it and the write makes it smaller (section 16.9.7).
    /// Creating an `agent://` file needs `owners` to know its agent. An
    /// `agent://` file keeps the frontmatter the daemon owns (section
    /// 16.7): the daemon's description and keywords for an agent it keeps a
    /// file for, and the description the file has otherwise; a project's
    /// overview keeps the daemon's description the same way. The body is
    /// kept as given and the frontmatter rendered anew. A write to a
    /// project that does not exist is refused; the first memory of a
    /// machine creates its folder.
    pub(crate) fn write(
        &self,
        url: &MemoryUrl,
        text: &str,
        mode: WriteMode<'_>,
        owners: impl MemoryOwners,
        limits: &MemoryLimits,
        now: Timestamp,
    ) -> Result<Written, StoreError> {
        let invalid = |reason| StoreError::Invalid {
            url: url.clone(),
            reason,
        };
        let document = MemoryDocument::parse(text.as_bytes()).map_err(invalid)?;
        let lock = self.lock()?;
        if let MemoryUrl::Project { project, .. } = url
            && let Some(projects) = owners.unknown_project(project)
        {
            return Err(unknown_project(url, project, &projects));
        }
        let path = self.file_path(url);
        let existing = read_bytes(&path)?;
        let current = existing
            .as_ref()
            .map(|(bytes, _)| MemoryRevision::of(bytes));
        let existing_text = existing
            .as_ref()
            .map(|(bytes, _)| String::from_utf8_lossy(bytes).into_owned());
        let mut near = Vec::new();
        match (mode, current) {
            (WriteMode::Client(None), Some(revision)) => {
                return Err(StoreError::Exists {
                    url: url.clone(),
                    revision,
                });
            }
            (WriteMode::Client(Some(_)), None) => {
                return Err(StoreError::NotFound { url: url.clone() });
            }
            (WriteMode::Client(Some(expected)), Some(current)) if current != *expected => {
                return Err(StoreError::RevisionMismatch {
                    url: url.clone(),
                    current,
                });
            }
            (WriteMode::Model { seen, .. }, Some(current)) if seen != Some(&current) => {
                return Err(StoreError::Unseen { url: url.clone() });
            }
            (WriteMode::Model { new, .. }, None) => {
                match super::similar::compare(&document, &self.neighbours(url)?) {
                    Similarity::Identical(existing) => {
                        return Err(StoreError::Identical {
                            url: url.clone(),
                            existing,
                        });
                    }
                    Similarity::Strong(found) if !new => {
                        return Err(StoreError::Similar {
                            url: url.clone(),
                            near: found,
                        });
                    }
                    Similarity::Strong(found) | Similarity::Weak(found) => near = found,
                    Similarity::None => {}
                }
            }
            (WriteMode::Client(_) | WriteMode::Model { .. }, _) => {}
        }
        let existing_document = existing
            .as_ref()
            .and_then(|(bytes, _)| MemoryDocument::parse(bytes).ok());
        let mut target = None;
        match url {
            MemoryUrl::Agent { agent } => {
                let status = owners.agent(agent);
                if existing.is_none() && !status.exists() {
                    return Err(StoreError::Refused(format!(
                        "{url} names no agent in the agent registry"
                    )));
                }
                target = match &status {
                    AgentMemory::Managed(spec) => {
                        Some(spec.apply(agent, &document).map_err(invalid)?)
                    }
                    AgentMemory::Registered | AgentMemory::Unknown => existing_document
                        .as_ref()
                        .map(|current| {
                            MemoryDocument::new(
                                current.description(),
                                document.keywords().to_vec(),
                                document.body(),
                            )
                        })
                        .transpose()
                        .map_err(invalid)?,
                };
            }
            MemoryUrl::Project { project, .. } if super::projects::is_overview(url) => {
                target =
                    Some(super::projects::managed_overview(project, &document).map_err(invalid)?);
            }
            _ => {}
        }
        let kept = target.as_ref().and_then(|target| {
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
            (metadata != KeptMetadata::default()).then_some(metadata)
        });
        let created = existing.as_ref().map_or(now, |(_, metadata)| {
            existing_document
                .as_ref()
                .and_then(MemoryDocument::created)
                .unwrap_or_else(|| modified_time(metadata))
        });
        let stamped = target
            .unwrap_or_else(|| document.clone())
            .with_stamps(Some(created), Some(now));
        let written = with_frontmatter(text, &stamped)
            .ok_or_else(|| invalid(InvalidMemory("the frontmatter was not found".to_owned())))?;
        check_size(url, &written, existing_text.as_deref(), limits)?;
        let revision = write_file(&path, written.as_bytes())?;
        drop(lock);
        self.changed();
        Ok(Written {
            revision,
            kept,
            created: existing.is_none(),
            near,
            replaced: existing_text,
            text: written,
        })
    }

    /// Replaces the one occurrence of `old` in the body of a memory with
    /// `new`, whatever else changed in it since the caller saw it (section
    /// 16.9.8). Fails, changing nothing, when `old` occurs zero or several
    /// times, the result is not a valid memory, or it does not fit the
    /// memory's limit ([`MemoryRoot::write`]). The file is written in the
    /// form [`MemoryDocument::render`] gives, stamped as modified now, with
    /// the daemon's description and keywords for an agent file the daemon
    /// keeps (section 16.7).
    pub(crate) fn edit(
        &self,
        url: &MemoryUrl,
        old: &str,
        new: &str,
        owners: impl MemoryOwners,
        limits: &MemoryLimits,
        now: Timestamp,
    ) -> Result<Edited, StoreError> {
        let lock = self.lock()?;
        if let MemoryUrl::Project { project, .. } = url
            && let Some(projects) = owners.unknown_project(project)
        {
            return Err(unknown_project(url, project, &projects));
        }
        let path = self.file_path(url);
        let (bytes, metadata) =
            read_bytes(&path)?.ok_or_else(|| StoreError::NotFound { url: url.clone() })?;
        let invalid = |reason| StoreError::Invalid {
            url: url.clone(),
            reason,
        };
        let document = MemoryDocument::parse(&bytes).map_err(invalid)?;
        occurs_once(url, document.body(), old)?;
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
        let created = document
            .created()
            .unwrap_or_else(|| modified_time(&metadata));
        let text = edited.with_stamps(Some(created), Some(now)).render();
        let before = String::from_utf8_lossy(&bytes).into_owned();
        check_size(url, &text, Some(&before), limits)?;
        let revision = write_file(&path, text.as_bytes())?;
        drop(lock);
        self.changed();
        Ok(Edited {
            revision,
            before,
            after: text,
        })
    }

    /// Whether an edit of the memory at `url` replacing `old` would apply
    /// now: the memory exists, is valid, and `old` occurs once in its body.
    /// Changes nothing; the edit checks again when it runs
    /// ([`MemoryRoot::edit`]).
    pub(crate) fn check_edit(&self, url: &MemoryUrl, old: &str) -> Result<(), StoreError> {
        let read = self.read(url)?;
        let document =
            MemoryDocument::parse(read.text.as_bytes()).map_err(|reason| StoreError::Invalid {
                url: url.clone(),
                reason,
            })?;
        occurs_once(url, document.body(), old)
    }

    /// Deletes a memory, at revision `expected` when one is given (a
    /// client's delete), and returns the text it held. A model's delete
    /// deletes, whatever the layer (section 16.9.11). A project's folder
    /// stays when its last memory goes: it belongs to the project.
    pub(crate) fn delete(
        &self,
        url: &MemoryUrl,
        expected: Option<&MemoryRevision>,
    ) -> Result<String, StoreError> {
        let lock = self.lock()?;
        let path = self.file_path(url);
        let (bytes, _) =
            read_bytes(&path)?.ok_or_else(|| StoreError::NotFound { url: url.clone() })?;
        let current = MemoryRevision::of(&bytes);
        if let Some(expected) = expected
            && current != *expected
        {
            return Err(StoreError::RevisionMismatch {
                url: url.clone(),
                current,
            });
        }
        fs::remove_file(&path).map_err(io_error(&path))?;
        drop(lock);
        self.changed();
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Stamps every valid memory file without stamps with its modification
    /// time, as `created` and `modified` (section 16.9.6), adding only the
    /// missing lines before the frontmatter's closing `---`, so the rest of
    /// the file stays as written. Run once at start; until a file is
    /// stamped, reads take its modification time instead. Returns how many
    /// files it stamped, and signals when it stamped any.
    pub(crate) fn stamp_unstamped(&self) -> Result<usize, MemoryError> {
        if !self.root.is_dir() {
            return Ok(0);
        }
        let lock = self.lock()?;
        let mut stamped = 0;
        for entry in self.list(&ListScope::All)?.entries {
            let Ok(document) = &entry.document else {
                continue;
            };
            if document.created().is_some() && document.modified().is_some() {
                continue;
            }
            let path = self.file_path(&entry.url);
            let Some((bytes, metadata)) = read_bytes(&path)? else {
                continue;
            };
            let Some(text) = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|text| add_stamps(text, modified_time(&metadata)))
            else {
                continue;
            };
            write_file(&path, text.as_bytes())?;
            stamped += 1;
        }
        drop(lock);
        if stamped > 0 {
            self.changed();
        }
        Ok(stamped)
    }
}

/// `text`, a valid memory file, with the stamps it lacks set to `time`,
/// added as lines just before its frontmatter's closing `---`. `None` when
/// it has both, or its frontmatter is not found.
fn add_stamps(text: &str, time: Timestamp) -> Option<String> {
    let document = MemoryDocument::parse(text.as_bytes()).ok()?;
    let frontmatter = MemoryDocument::frontmatter_len(text)?;
    let fence = text[..frontmatter].trim_end_matches(['\n', '\r']).len() - "---".len();
    let mut lines = String::new();
    for (key, stamp) in [
        ("created", document.created()),
        ("modified", document.modified()),
    ] {
        if stamp.is_none() {
            lines.push_str(&format!("{key}: {time}\n"));
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!("{}{lines}{}", &text[..fence], &text[fence..]))
}

/// Whether `text` may replace `current` (absent for a new memory) at
/// `url`: it fits the memory's limit, or the memory is over it and `text`
/// is smaller (section 16.9.7).
fn check_size(
    url: &MemoryUrl,
    text: &str,
    current: Option<&str>,
    limits: &MemoryLimits,
) -> Result<(), StoreError> {
    let max = limits.max_bytes(url);
    let shrinks_an_oversized = current.is_some_and(|current| current.len() > text.len());
    if text.len() <= max || shrinks_an_oversized && current.is_some_and(|c| c.len() > max) {
        return Ok(());
    }
    Err(StoreError::TooLarge {
        url: url.clone(),
        size: text.len(),
        max,
        current: current.map(str::len),
    })
}

/// Writes `bytes` to `path` through a flushed temporary file in the same
/// folder renamed over it, creating the folder first. Returns the new
/// revision.
pub(super) fn write_file(path: &Path, bytes: &[u8]) -> Result<MemoryRevision, MemoryError> {
    let revision = write_file_unsynced_folder(path, bytes)?;
    #[cfg(unix)]
    {
        let directory = path.parent().expect("memory files have a folder");
        fs::File::open(directory)
            .and_then(|directory| directory.sync_all())
            .map_err(io_error(path))?;
    }
    Ok(revision)
}

/// [`write_file`] without the fsync of the file's folder, for a caller that
/// writes several files of one folder and syncs it once after them.
pub(super) fn write_file_unsynced_folder(
    path: &Path,
    bytes: &[u8],
) -> Result<MemoryRevision, MemoryError> {
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
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(io_error(path))?;
    Ok(MemoryRevision::of(bytes))
}

#[cfg(test)]
mod tests;
