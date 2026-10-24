//! The project registry (`docs/site/specs/projects.md` sections 3 and 5):
//! one folder per project, `<data_root>/projects/<id>/`, holding
//! `project.toml` (name, emoji, creation time, defaults) and an optional
//! `AGENTS.md`. A project's memories stay in `memory/projects/<name>/`,
//! whose name is the project's name.
//!
//! Every change holds the memory lock ([`crate::memory::MemoryLock`]) while
//! it rereads the project, checks its revision, rewrites it, and moves or
//! removes its memory folder, so two daemons sharing a data root, or a
//! memory write, never race it. Reads take no lock: every file is replaced
//! by renaming a flushed temporary file over it.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_protocol::{
    AgentId, AgentMdSkipped, CapabilityName, ProjectDefaults, ProjectId, ProjectName,
    ProjectRuntime, RuntimeId, SessionId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::memory::{FolderMoves, MemoryError, MemoryRoot, url::MemoryName};

#[cfg(test)]
mod tests;

/// The registry's folder in the data root.
pub(crate) const PROJECTS_DIR: &str = "projects";
/// A project's file in its folder.
const PROJECT_FILE: &str = "project.toml";
/// A project's `AGENTS.md` in its folder.
const AGENTS_FILE: &str = "AGENTS.md";
/// The most bytes an emoji may take.
const MAX_EMOJI_BYTES: usize = 32;
/// The most bytes a project's `AGENTS.md` may take: what a workspace's may
/// before it is skipped.
pub(crate) const MAX_AGENTS_MD_BYTES: usize = AgentMdSkipped::MAX_SKIP_BYTES as usize;

/// Why a project change was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("there is no project {0}")]
    NotFound(ProjectId),
    #[error("the project name `{0}` is taken")]
    NameTaken(ProjectName),
    #[error("project {project} changed since it was read; its current revision is {revision}")]
    Changed {
        project: ProjectId,
        revision: String,
    },
    #[error("the project's {field} was refused: {reason}")]
    Invalid { field: &'static str, reason: String },
    #[error("sessions of project {project} are running: {}", list(.sessions))]
    SessionsRunning {
        project: ProjectId,
        sessions: Vec<SessionId>,
    },
    #[error(
        "session {session_id} is a subagent of session {root_session_id} and belongs to its root's project"
    )]
    RootSessionOnly {
        session_id: SessionId,
        root_session_id: SessionId,
    },
    /// Reading or writing a project or its memory folder failed.
    #[error("{0}")]
    Store(String),
}

fn list(sessions: &[SessionId]) -> String {
    sessions
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl From<MemoryError> for ProjectError {
    fn from(error: MemoryError) -> Self {
        Self::Store(error.to_string())
    }
}

fn store_error(path: &Path) -> impl FnOnce(io::Error) -> ProjectError + '_ {
    move |error| ProjectError::Store(format!("{}: {error}", path.display()))
}

/// `project.toml`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    name: ProjectName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    emoji: Option<String>,
    created: jiff::Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    defaults: Option<DefaultsFile>,
}

/// `[defaults]`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DefaultsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<AgentId>,
    #[serde(default = "attach_global")]
    global: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    runtimes: Vec<RuntimeFile>,
}

const fn attach_global() -> bool {
    true
}

/// One entry of `[defaults] runtimes`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeFile {
    runtime_id: RuntimeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capability: Option<CapabilityName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
}

impl From<&ProjectDefaults> for DefaultsFile {
    fn from(defaults: &ProjectDefaults) -> Self {
        Self {
            preset: defaults.preset.clone(),
            agent: defaults.agent.clone(),
            global: defaults.global,
            runtimes: defaults
                .runtimes
                .iter()
                .map(|runtime| RuntimeFile {
                    runtime_id: runtime.runtime_id.clone(),
                    capability: runtime.capability.clone(),
                    workspace: runtime.workspace.clone(),
                })
                .collect(),
        }
    }
}

impl From<DefaultsFile> for ProjectDefaults {
    fn from(file: DefaultsFile) -> Self {
        Self {
            preset: file.preset,
            agent: file.agent,
            global: file.global,
            runtimes: file
                .runtimes
                .into_iter()
                .map(|runtime| ProjectRuntime {
                    runtime_id: runtime.runtime_id,
                    capability: runtime.capability,
                    workspace: runtime.workspace,
                })
                .collect(),
        }
    }
}

/// One project as found on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Project {
    pub(crate) id: ProjectId,
    pub(crate) name: ProjectName,
    pub(crate) emoji: Option<String>,
    pub(crate) created: jiff::Timestamp,
    pub(crate) defaults: Option<ProjectDefaults>,
    /// The first 16 hex digits of the SHA-256 of `project.toml`, and of
    /// `AGENTS.md` when there is one.
    pub(crate) revision: String,
    /// `AGENTS.md`, when there is one; invalid UTF-8 is replaced.
    pub(crate) agents_md: Option<String>,
}

impl Project {
    /// The project's name as a memory folder name.
    pub(crate) fn memory_name(&self) -> MemoryName {
        MemoryName::new(self.name.as_str()).expect("project names are memory names")
    }

    fn file(&self) -> ProjectFile {
        ProjectFile {
            name: self.name.clone(),
            emoji: self.emoji.clone(),
            created: self.created,
            defaults: self.defaults.as_ref().map(DefaultsFile::from),
        }
    }
}

/// The first 16 hex digits of the SHA-256 of `bytes`.
pub(crate) fn short_digest(bytes: &[u8]) -> String {
    hex16(&Sha256::digest(bytes))
}

fn hex16(digest: &[u8]) -> String {
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A project's revision: `project.toml`'s bytes, then `AGENTS.md`'s when
/// there is one.
fn revision(file: &[u8], agents_md: Option<&[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(file);
    if let Some(agents_md) = agents_md {
        hasher.update(b"\0AGENTS.md\0");
        hasher.update(agents_md);
    }
    hex16(&hasher.finalize())
}

/// Whether `emoji` is exactly one emoji (`docs/site/specs/projects.md`
/// section 3): one extended grapheme cluster of at most 32 bytes, holding a
/// character Unicode marks as an emoji, made only of emoji characters and
/// emoji components, and not ASCII, whitespace, or a control character.
pub(crate) fn validate_emoji(emoji: &str) -> Result<(), String> {
    use unicode_properties::UnicodeEmoji as _;
    use unicode_segmentation::UnicodeSegmentation as _;
    if emoji.is_empty() {
        return Err("it is empty".to_owned());
    }
    if emoji.len() > MAX_EMOJI_BYTES {
        return Err(format!(
            "it is {} bytes, over the {MAX_EMOJI_BYTES}-byte limit",
            emoji.len()
        ));
    }
    if emoji.is_ascii() {
        return Err("it is ASCII, not an emoji".to_owned());
    }
    if emoji
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("it holds whitespace or a control character".to_owned());
    }
    if emoji.graphemes(true).count() != 1 {
        return Err("it is more than one character".to_owned());
    }
    if !emoji.chars().any(|character| character.is_emoji_char())
        || !emoji
            .chars()
            .all(|character| character.is_emoji_char_or_emoji_component())
    {
        return Err("it is not an emoji".to_owned());
    }
    Ok(())
}

/// Checks what [`ProjectDefaults`] cannot say in its types: no runtime
/// listed twice and no empty workspace. The runtimes need not be connected,
/// so whether they have the capability is not checked.
pub(crate) fn validate_defaults(defaults: &ProjectDefaults) -> Result<(), ProjectError> {
    let invalid = |reason: String| ProjectError::Invalid {
        field: "defaults",
        reason,
    };
    let mut seen = BTreeSet::new();
    for runtime in &defaults.runtimes {
        if runtime
            .workspace
            .as_deref()
            .is_some_and(|workspace| workspace.trim().is_empty())
        {
            return Err(invalid(format!(
                "runtime `{}` has an empty workspace",
                runtime.runtime_id
            )));
        }
        let key = (
            runtime.runtime_id.as_str(),
            runtime.capability.as_ref().map(CapabilityName::as_str),
            runtime.workspace.as_deref(),
        );
        if !seen.insert(key) {
            return Err(invalid(format!(
                "runtime `{}` is listed twice with the same capability and workspace",
                runtime.runtime_id
            )));
        }
    }
    Ok(())
}

/// A change to one project: only what is `Some` changes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectChange {
    pub(crate) name: Option<ProjectName>,
    /// A new emoji, or `Some(None)` to clear it.
    pub(crate) emoji: Option<Option<String>>,
    /// New defaults, or `Some(None)` to clear them.
    pub(crate) defaults: Option<Option<ProjectDefaults>>,
    /// The `AGENTS.md` text; empty removes the file.
    pub(crate) agents_md: Option<String>,
}

/// The daemon's projects.
pub(crate) struct ProjectStore {
    dir: PathBuf,
    memory: MemoryRoot,
    /// Serializes changes within this process; the memory lock serializes
    /// processes.
    mutation: Mutex<()>,
    /// Bumped whenever a project changes through this store.
    changes: tokio::sync::watch::Sender<u64>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl ProjectStore {
    /// The projects of the data root `data_dir`, whose memory folder is
    /// `memory`.
    pub(crate) fn new(data_dir: &Path, memory: MemoryRoot) -> Self {
        Self {
            dir: data_dir.join(PROJECTS_DIR),
            memory,
            mutation: Mutex::new(()),
            changes: tokio::sync::watch::channel(0).0,
        }
    }

    /// Signals every project change this store makes; changes other
    /// processes make are not seen.
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn changed(&self) {
        self.changes
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    pub(crate) fn memory(&self) -> &MemoryRoot {
        &self.memory
    }

    fn project_dir(&self, id: &ProjectId) -> PathBuf {
        self.dir.join(id.as_str())
    }

    /// Every project that reads, in name order. A project folder that does
    /// not read is reported on standard error and left out.
    pub(crate) fn list(&self) -> Result<Vec<Project>, ProjectError> {
        let (projects, unreadable) = self.read_all()?;
        for (path, error) in unreadable {
            eprintln!("cookie-agent: project {} skipped: {error}", path.display());
        }
        Ok(projects)
    }

    /// The names of the projects, in name order.
    pub(crate) fn names(&self) -> Vec<String> {
        self.read_all().map_or_else(
            |_| Vec::new(),
            |(projects, _)| {
                projects
                    .into_iter()
                    .map(|project| project.name.into_string())
                    .collect()
            },
        )
    }

    /// The IDs of the projects there are: the folders with a project's name
    /// and a `project.toml`, which is not read. What a session's project is
    /// checked against whenever sessions are listed.
    pub(crate) fn ids(&self) -> Vec<ProjectId> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            // Folders being built or removed start with a dot.
            .filter_map(|entry| {
                ProjectId::new(entry.file_name().to_string_lossy().into_owned()).ok()
            })
            .filter(|id| self.exists(id))
            .collect()
    }

    /// Whether project `id` exists: its folder has a `project.toml`, which
    /// is not read.
    pub(crate) fn exists(&self, id: &ProjectId) -> bool {
        self.project_dir(id).join(PROJECT_FILE).is_file()
    }

    /// One project.
    pub(crate) fn get(&self, id: &ProjectId) -> Result<Project, ProjectError> {
        read_project(&self.project_dir(id), id)?.ok_or_else(|| ProjectError::NotFound(id.clone()))
    }

    /// Every project that reads, and the folders that do not with why.
    #[allow(clippy::type_complexity)]
    fn read_all(&self) -> Result<(Vec<Project>, Vec<(PathBuf, ProjectError)>), ProjectError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()));
            }
            Err(error) => return Err(store_error(&self.dir)(error)),
        };
        let (mut projects, mut unreadable) = (Vec::new(), Vec::new());
        for entry in entries {
            let entry = entry.map_err(store_error(&self.dir))?;
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            // Folders being built or removed start with a dot.
            let Ok(id) = ProjectId::new(entry.file_name().to_string_lossy().into_owned()) else {
                continue;
            };
            match read_project(&entry.path(), &id) {
                Ok(Some(project)) => projects.push(project),
                Ok(None) => {}
                Err(error) => unreadable.push((entry.path(), error)),
            }
        }
        projects.sort_by(|left, right| left.name.cmp(&right.name));
        Ok((projects, unreadable))
    }

    /// Creates a project named `name` and its empty overview. A memory
    /// folder of that name that belongs to no project becomes the new
    /// project's.
    pub(crate) fn create(
        &self,
        name: ProjectName,
        emoji: Option<String>,
        defaults: Option<ProjectDefaults>,
        overview_max_bytes: usize,
    ) -> Result<Project, ProjectError> {
        if let Some(emoji) = &emoji {
            validate_emoji(emoji).map_err(|reason| ProjectError::Invalid {
                field: "emoji",
                reason,
            })?;
        }
        if let Some(defaults) = &defaults {
            validate_defaults(defaults)?;
        }
        let _mutation = lock(&self.mutation);
        let memory_lock = self.memory.lock()?;
        let (projects, unreadable) = self.read_all()?;
        if projects.iter().any(|project| project.name == name) {
            return Err(ProjectError::NameTaken(name));
        }
        if !unreadable.is_empty() {
            // An unreadable project might have the name.
            return Err(ProjectError::Store(format!(
                "project {} does not read, so the name cannot be checked",
                unreadable[0].0.display()
            )));
        }
        let project = self.place_new(name, emoji, defaults)?;
        if let Err(error) =
            self.memory
                .ensure_overview(&memory_lock, &project.memory_name(), overview_max_bytes)
        {
            let _ = fs::remove_dir_all(self.project_dir(&project.id));
            return Err(error.into());
        }
        drop(memory_lock);
        self.changed();
        Ok(project)
    }

    /// Writes a new project's folder under a temporary name and renames it
    /// into place, so a crash leaves no half-made project.
    fn place_new(
        &self,
        name: ProjectName,
        emoji: Option<String>,
        defaults: Option<ProjectDefaults>,
    ) -> Result<Project, ProjectError> {
        let id = ProjectId::new(uuid::Uuid::now_v7().to_string()).expect("a UUID is a project ID");
        let mut project = Project {
            id,
            name,
            emoji,
            created: jiff::Timestamp::now(),
            defaults,
            revision: String::new(),
            agents_md: None,
        };
        fs::create_dir_all(&self.dir).map_err(store_error(&self.dir))?;
        let staging = self.dir.join(format!(".{}.tmp", project.id));
        let target = self.project_dir(&project.id);
        let placed = (|| {
            fs::create_dir(&staging).map_err(store_error(&staging))?;
            let bytes = encode(&project.file())?;
            write_atomic(&staging.join(PROJECT_FILE), &bytes)?;
            project.revision = revision(&bytes, None);
            fs::rename(&staging, &target).map_err(store_error(&target))?;
            sync_dir(&self.dir);
            Ok(())
        })();
        if placed.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        placed.map(|()| project)
    }

    /// Applies `change` to the project `id` at `expected`, its revision as
    /// last read. A rename moves the memory folder first, under the memory
    /// lock; when it fails, nothing changes, and when writing the project
    /// fails afterwards, the folder moves back.
    pub(crate) fn update(
        &self,
        id: &ProjectId,
        expected: &str,
        change: ProjectChange,
        overview_max_bytes: usize,
    ) -> Result<Project, ProjectError> {
        if let Some(Some(emoji)) = &change.emoji {
            validate_emoji(emoji).map_err(|reason| ProjectError::Invalid {
                field: "emoji",
                reason,
            })?;
        }
        if let Some(Some(defaults)) = &change.defaults {
            validate_defaults(defaults)?;
        }
        if let Some(text) = &change.agents_md
            && text.len() > MAX_AGENTS_MD_BYTES
        {
            return Err(ProjectError::Invalid {
                field: "agents_md",
                reason: format!(
                    "it is {} bytes, over the {} MiB limit",
                    text.len(),
                    MAX_AGENTS_MD_BYTES / (1024 * 1024)
                ),
            });
        }
        let _mutation = lock(&self.mutation);
        let memory_lock = self.memory.lock()?;
        let current = self.get(id)?;
        if current.revision != expected {
            return Err(ProjectError::Changed {
                project: id.clone(),
                revision: current.revision,
            });
        }
        let mut updated = current.clone();
        let mut moves = FolderMoves::default();
        if let Some(name) = change.name.filter(|name| *name != current.name) {
            let (projects, _) = self.read_all()?;
            if projects.iter().any(|project| project.name == name) {
                return Err(ProjectError::NameTaken(name));
            }
            updated.name = name;
            moves = self
                .memory
                .rename_project_folder(&memory_lock, &current.memory_name(), &updated.memory_name())
                .map_err(|error| match error {
                    MemoryError::Exists(_) => ProjectError::NameTaken(updated.name.clone()),
                    error => error.into(),
                })?;
        }
        if let Some(emoji) = change.emoji {
            updated.emoji = emoji;
        }
        if let Some(defaults) = change.defaults {
            updated.defaults = defaults;
        }
        if let Some(text) = change.agents_md {
            updated.agents_md = (!text.is_empty()).then_some(text);
        }
        let dir = self.project_dir(id);
        let written = (|| {
            let bytes = encode(&updated.file())?;
            write_atomic(&dir.join(PROJECT_FILE), &bytes)?;
            let agents_path = dir.join(AGENTS_FILE);
            match &updated.agents_md {
                Some(text) if updated.agents_md != current.agents_md => {
                    write_atomic(&agents_path, text.as_bytes())?;
                }
                None if current.agents_md.is_some() => match fs::remove_file(&agents_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(store_error(&agents_path)(error)),
                },
                _ => {}
            }
            Ok(bytes)
        })();
        let bytes = match written {
            Ok(bytes) => bytes,
            Err(error) => {
                // Put back what was there, best effort.
                if let Ok(bytes) = encode(&current.file()) {
                    let _ = write_atomic(&dir.join(PROJECT_FILE), &bytes);
                }
                moves.undo();
                return Err(error);
            }
        };
        updated.revision = revision(&bytes, updated.agents_md.as_deref().map(str::as_bytes));
        if let Err(error) =
            self.memory
                .ensure_overview(&memory_lock, &updated.memory_name(), overview_max_bytes)
        {
            eprintln!(
                "cookie-agent: the overview of project `{}`: {error}",
                updated.name
            );
        }
        drop(memory_lock);
        self.changed();
        Ok(updated)
    }

    /// Deletes the project `id` at `expected`: its whole memory folder,
    /// memory files or not, then its own folder. Returns how many memory
    /// files went. The caller deletes the project's sessions first.
    pub(crate) fn delete(&self, id: &ProjectId, expected: &str) -> Result<u64, ProjectError> {
        let _mutation = lock(&self.mutation);
        let memory_lock = self.memory.lock()?;
        let current = self.get(id)?;
        if current.revision != expected {
            return Err(ProjectError::Changed {
                project: id.clone(),
                revision: current.revision,
            });
        }
        let memories = self
            .memory
            .remove_project_folder(&memory_lock, &current.memory_name())?;
        // Renamed away first, so a removal that fails halfway leaves no
        // folder that reads as a project.
        let dir = self.project_dir(id);
        let doomed = self.dir.join(format!(".{id}.deleted"));
        fs::rename(&dir, &doomed).map_err(store_error(&dir))?;
        if let Err(error) = fs::remove_dir_all(&doomed) {
            eprintln!(
                "cookie-agent: the folder of deleted project {id} stays at {}: {error}",
                doomed.display()
            );
        }
        sync_dir(&self.dir);
        drop(memory_lock);
        self.changed();
        Ok(memories)
    }

    /// The upgrade to projects (`docs/site/specs/projects.md` section 3):
    /// every valid folder under `memory/projects/` that no project has
    /// becomes a project, with no defaults and no `AGENTS.md`, and every
    /// project's overview exists with the daemon's description. Idempotent;
    /// it adopts nothing while a project folder does not read, since that
    /// project could have the name. Returns the projects it created.
    pub(crate) fn adopt_memory_folders(
        &self,
        overview_max_bytes: usize,
    ) -> Result<Vec<Project>, ProjectError> {
        let _mutation = lock(&self.mutation);
        let memory_lock = self.memory.lock()?;
        let (mut projects, unreadable) = self.read_all()?;
        let mut adopted = Vec::new();
        if unreadable.is_empty() {
            let names = projects
                .iter()
                .map(|project| project.name.as_str().to_owned())
                .collect::<BTreeSet<_>>();
            for folder in self.memory.project_folders()? {
                if names.contains(folder.as_str()) {
                    continue;
                }
                let name =
                    ProjectName::new(folder.as_str()).expect("memory names are project names");
                let project = self.place_new(name, None, None)?;
                adopted.push(project.clone());
                projects.push(project);
            }
        } else {
            for (path, error) in &unreadable {
                eprintln!(
                    "cookie-agent: memory folders were not adopted as projects: project {} does not read: {error}",
                    path.display()
                );
            }
        }
        for project in &projects {
            if let Err(error) = self.memory.ensure_overview(
                &memory_lock,
                &project.memory_name(),
                overview_max_bytes,
            ) {
                eprintln!(
                    "cookie-agent: the overview of project `{}`: {error}",
                    project.name
                );
            }
        }
        drop(memory_lock);
        if !adopted.is_empty() {
            self.changed();
        }
        Ok(adopted)
    }
}

/// The project in `dir`, or `None` when it has no `project.toml` (a folder
/// that is being made or removed).
fn read_project(dir: &Path, id: &ProjectId) -> Result<Option<Project>, ProjectError> {
    let path = dir.join(PROJECT_FILE);
    let bytes = match cookie_agent_models::secure_store::read_replaced(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(store_error(&path)(error)),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| ProjectError::Store(format!("{}: {error}", path.display())))?;
    let file: ProjectFile = toml::from_str(text)
        .map_err(|error| ProjectError::Store(format!("{}: {error}", path.display())))?;
    if let Some(emoji) = &file.emoji {
        validate_emoji(emoji).map_err(|reason| {
            ProjectError::Store(format!(
                "{}: the emoji is invalid: {reason}",
                path.display()
            ))
        })?;
    }
    let agents_path = dir.join(AGENTS_FILE);
    let agents_md = match fs::read(&agents_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(store_error(&agents_path)(error)),
    };
    Ok(Some(Project {
        id: id.clone(),
        name: file.name,
        emoji: file.emoji,
        created: file.created,
        defaults: file.defaults.map(ProjectDefaults::from),
        revision: revision(&bytes, agents_md.as_deref()),
        agents_md: agents_md.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
    }))
}

fn encode(file: &ProjectFile) -> Result<Vec<u8>, ProjectError> {
    toml::to_string(file)
        .map(String::into_bytes)
        .map_err(|error| ProjectError::Store(format!("the project does not encode: {error}")))
}

/// Writes `bytes` to `path` through a flushed temporary file in the same
/// folder, renamed over it.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ProjectError> {
    let directory = path.parent().expect("project files have a folder");
    let file_name = path
        .file_name()
        .expect("project files have a name")
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
    result.map_err(store_error(path))?;
    sync_dir(directory);
    Ok(())
}

/// Flushes a folder's entries on Unix, best effort.
fn sync_dir(directory: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = fs::File::open(directory) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = directory;
}

/// What clients are told a project holds of `memory`: its memory files.
pub(crate) fn memory_count(memory: &MemoryRoot, project: &Project) -> u64 {
    memory.project_memory_count(&project.memory_name()) as u64
}
