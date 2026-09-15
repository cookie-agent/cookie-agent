//! Session directories, projections, and rebuildable metadata caches.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(unix)]
use std::{
    fs::OpenOptions,
    os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
};

use cookie_agent_protocol::{
    AgentId, AgentSnapshot, ChildSummary, ClientRenameId, ClientRunId, EventPayload, RunId,
    RunSelection, SessionId, SessionMeta, SessionOrigin, SessionPermissionOverlay,
    SessionRenameRecord, SessionStatus, SessionTitle, SessionTitleChange, SessionTree, ToolCallId,
    Usage, UsageRollup,
};
use thiserror::Error;
use uuid::Uuid;

use crate::events::{EventLog, EventLogError, fsync_directory};
use crate::ownership::{
    HeldLock, SessionOwnership, WriteAuthority, WriteCapability, owner_lock_path, try_acquire,
};

pub(crate) const PROJECT_CWD_FILE: &str = "cwd";
/// v2 session-store root under the data root (`~/.cookie-agent/sessions`).
pub(crate) const SESSIONS_ROOT_DIR: &str = "sessions";
/// Marker file recording the on-disk layout version of a work-dir store.
pub(crate) const LAYOUT_MARKER_FILE: &str = "layout.json";
/// Session metadata cache file name in the v2 layout.
pub(crate) const SESSION_META_FILE: &str = "metadata";
/// Pre-v2 session metadata cache name, still read for one release.
pub(crate) const LEGACY_SESSION_META_FILE: &str = "meta.json";
/// Per-root directory holding delegated child sessions.
pub(crate) const SUBAGENTS_DIR: &str = "subagents";
/// Persisted child-summary cache inside [`SUBAGENTS_DIR`].
pub(crate) const SUBAGENT_INDEX_FILE: &str = "index.json";
/// Current `subagents/index.json` schema version.
const SUBAGENT_INDEX_VERSION: u32 = 1;
/// Event log file name (unchanged across layouts).
pub(crate) const EVENTS_FILE: &str = "events.jsonl";
/// Layout version written by this build.
pub(crate) const LAYOUT_VERSION: u32 = 2;

/// Where a session lives relative to its work-dir store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionLocation {
    /// `sessions/<workdirkey>/<id>/`
    Root,
    /// `sessions/<workdirkey>/<root>/subagents/<id>/`
    Child { root: SessionId },
}

impl SessionLocation {
    #[allow(dead_code)] // consumed by the lazy-tree passes (P2)
    fn root_of(self) -> Option<SessionId> {
        match self {
            Self::Root => None,
            Self::Child { root } => Some(root),
        }
    }
}

/// Which on-disk layout a store opens. Production uses [`LayoutChoice::PreferV2`];
/// tests exercise the flat v1 layout explicitly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // every variant is constructed from P1 onwards
enum LayoutChoice {
    /// v2 (`sessions/<workdirkey>/`) unless a legacy project is still on disk.
    PreferV2,
    /// Always the legacy flat `projects/<hash>/sessions/` layout.
    #[allow(dead_code)] // becomes the default until P1
    ForceFlat,
    /// Always v2, even with a legacy project on disk (used by migration tests).
    #[allow(dead_code)]
    ForceV2,
}

/// Lazy-tree state for one root session (§3.1 of the storage spec).
#[derive(Debug, Default)]
pub(crate) struct TreeState {
    /// The one-time bulk child pass has completed for this root.
    #[allow(dead_code)] // consumed by the lazy-tree passes (P2)
    loaded: bool,
    /// Direct children keyed by parent session, covering the whole tree. Edges
    /// come from child `origin` metadata, not from directory nesting: every
    /// descendant of a root is filed one level under `<root>/subagents/`.
    children: HashMap<SessionId, Vec<SessionId>>,
    /// Terminal run statuses observed per child, persisted into `index.json`.
    terminal_runs: HashMap<SessionId, BTreeMap<String, SessionStatus>>,
    /// Restart-stable tree grants folded in by the bulk load (4.3), retained so
    /// grant rebuilds stay O(cached data) instead of O(logs).
    grants: Vec<cookie_agent_protocol::TreeApprovalGrant>,
    /// Children whose logs carry goal-producer state (4.4).
    producer_sessions: Vec<SessionId>,
}

/// Products of a completed [`SessionStore::load_tree`] pass that the engine
/// folds into its singletons (delegation registry, approvals, producers).
#[derive(Debug)]
pub(crate) struct TreeLoadProducts {
    pub(crate) root: SessionId,
    /// `(parent_session_id, run_id, payload)` delegation records held in child logs.
    pub(crate) delegations: Vec<(
        SessionId,
        Option<cookie_agent_protocol::RunId>,
        EventPayload,
    )>,
    /// Restart-stable tree approval grants held in child logs.
    pub(crate) grants: Vec<cookie_agent_protocol::TreeApprovalGrant>,
    /// Frozen model bindings referenced by child logs (4.2), validated with the
    /// same acceptance list the startup pass applies to root logs.
    pub(crate) bindings: Vec<(SessionId, cookie_agent_protocol::FrozenModelBinding)>,
    /// Children whose logs carry goal-producer state needing reconciliation.
    pub(crate) producer_sessions: Vec<SessionId>,
    /// Every child summary the pass produced, for usage and listing caches.
    pub(crate) summaries: Vec<SessionSummary>,
}

impl TreeLoadProducts {
    fn for_root(root: SessionId) -> Self {
        Self {
            root,
            delegations: Vec::new(),
            grants: Vec::new(),
            bindings: Vec::new(),
            producer_sessions: Vec::new(),
            summaries: Vec::new(),
        }
    }
}

/// Observer slot with a hand-written `Debug` (the payload is a `dyn` trait).
#[derive(Default)]
struct ObserverSlot(Mutex<Option<Arc<dyn TreeLoadObserver>>>);

impl std::fmt::Debug for ObserverSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObserverSlot")
            .finish_non_exhaustive()
    }
}

/// Callback the engine installs so store-side tree loads reach engine singletons
/// even when the load was triggered from inside the store. A failing observer
/// fails the load, so the access that triggered it fails closed.
pub(crate) trait TreeLoadObserver: Send + Sync {
    fn tree_loaded(&self, products: TreeLoadProducts) -> Result<(), crate::runtime::EngineError>;
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Event(#[from] EventLogError),
    #[error("session IO failure at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid session metadata at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("session {0} not found")]
    Missing(SessionId),
    #[error("session {0} is owned by another cookie process")]
    SessionLocked(SessionId),
    #[error("session store is closed")]
    StoreClosed,
    #[error("sequence {through_seq} is not a valid event in session {session_id}")]
    InvalidSequence {
        session_id: SessionId,
        through_seq: u64,
    },
    #[error("invalid fork title: {0}")]
    InvalidForkTitle(String),
    /// A lazy tree load completed but the engine rejected its products (a child
    /// log referencing a manifest this runtime cannot serve, for example).
    /// `impl From<SessionError> for EngineError` unwraps it back to its type.
    #[error("tree load rejected: {0}")]
    TreeRejected(Box<crate::runtime::EngineError>),
    /// A v1 to v2 store migration (§6) refused to start or could not be verified.
    /// A refused migration leaves the legacy store untouched; an unfinished one
    /// leaves `.migrating` behind so the next open resumes where this one stopped.
    #[error("session store migration: {0}")]
    Migration(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunProjection {
    pub id: RunId,
    pub client_run_id: ClientRunId,
    pub input: String,
    pub selection: RunSelection,
    pub agent: AgentSnapshot,
    pub status: SessionStatus,
    pub final_text: Option<String>,
    pub pending_calls: HashMap<ToolCallId, String>,
}

#[derive(Clone, Debug)]
pub struct SessionProjection {
    pub meta: SessionMeta,
    pub creation_agent: AgentSnapshot,
    pub status: SessionStatus,
    pub usage: Option<Usage>,
    pub usage_rollup: UsageRollup,
    pub agent_usage: BTreeMap<AgentId, UsageRollup>,
    pub runs: HashMap<RunId, RunProjection>,
    pub rename_records: HashMap<cookie_agent_protocol::ClientRenameId, SessionRenameRecord>,
    pub permission_overlay: SessionPermissionOverlay,
    pub log: Arc<EventLog>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct SessionSummary {
    pub meta: SessionMeta,
    pub usage: Option<Usage>,
    pub usage_rollup: UsageRollup,
    pub agent_usage: BTreeMap<AgentId, UsageRollup>,
}

/// One entry of a root's `subagents/index.json` child-summary cache (§3.4).
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct IndexedChild {
    summary: SessionSummary,
    /// Run ids that reached a terminal status, for the delegation registry.
    terminal_runs: BTreeMap<String, SessionStatus>,
}

/// Persisted child-summary cache (`subagents/index.json`, §3.4). A missing or
/// corrupt file only means "children unknown until tree load".
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct SubagentIndex {
    version: u32,
    children: Vec<IndexedChild>,
}

impl SessionProjection {
    #[must_use]
    pub fn metadata(&self) -> SessionMeta {
        self.meta.clone()
    }
}

#[derive(Debug, Default)]
struct SessionResidency {
    resident: HashMap<SessionId, SessionProjection>,
    evicted: HashMap<SessionId, SessionSummary>,
}

impl SessionResidency {
    fn known_ids(&self) -> Vec<SessionId> {
        self.resident
            .keys()
            .chain(self.evicted.keys())
            .copied()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }
}

#[derive(Debug)]
enum StoreOwnership {
    PendingPublish {
        authority: WriteAuthority,
    },
    Adopting {
        _lock: HeldLock,
        authority: WriteAuthority,
    },
    Owned {
        _lock: HeldLock,
        authority: WriteAuthority,
    },
    Foreign,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteOpen {
    AlreadyOwned,
    Adopting,
}

#[cfg(test)]
#[derive(Debug)]
struct EvictionTransitionHook {
    reached: Mutex<Option<tokio::sync::oneshot::Sender<SessionId>>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
#[derive(Debug)]
struct PublishHook {
    reached: std::sync::mpsc::Sender<SessionId>,
    release: std::sync::mpsc::Receiver<()>,
}

#[derive(Debug)]
pub struct SessionStore {
    data_root: PathBuf,
    /// Directory that root session dirs live in: `sessions/<workdirkey>/` for the
    /// v2 layout, `projects/<hash>/sessions/` while a legacy store is still flat.
    workdir_dir: PathBuf,
    /// Directory holding project-level files (`cwd`, the layout marker, the grant
    /// journal, runtime revisions and artifacts): `sessions/<workdirkey>/` under
    /// v2, `projects/<hash>/` under the flat layout.
    project_dir: PathBuf,
    /// Legacy flat layout: every session (root or child) is a direct child of
    /// [`Self::workdir_dir`] and root-only discovery is replaced by a full scan.
    flat_layout: bool,
    cwd: PathBuf,
    /// id -> where it lives on disk. Roots are inserted at discovery; children
    /// lazily (tree load, index cache, or direct-address locate).
    locations: Mutex<HashMap<SessionId, SessionLocation>>,
    /// root id -> state of the lazy tree load.
    trees: Mutex<HashMap<SessionId, TreeState>>,
    /// Per-root coalescing guards for concurrent `load_tree` triggers.
    #[allow(dead_code)] // wired up by the lazy-tree passes (P2)
    tree_locks: Mutex<HashMap<SessionId, Arc<Mutex<()>>>>,
    /// Engine hook applied after each completed tree load.
    tree_observer: ObserverSlot,
    residency: Mutex<SessionResidency>,
    ownership: Mutex<HashMap<SessionId, StoreOwnership>>,
    adoption_locks: Mutex<HashMap<SessionId, Arc<Mutex<()>>>>,
    mutation: Mutex<()>,
    closed: AtomicBool,
    #[cfg(test)]
    eviction_transition_hook: Mutex<Option<EvictionTransitionHook>>,
    #[cfg(test)]
    publish_hook: Mutex<Option<PublishHook>>,
    #[cfg(test)]
    tree_load_count: Mutex<HashMap<SessionId, usize>>,
}

impl SessionStore {
    /// Legacy v1 project directory. Retained for migration detection only.
    pub fn project_dir(data_root: &Path, cwd: &Path) -> PathBuf {
        data_root.join("projects").join(Self::project_hash(cwd))
    }

    /// The `<16-hex-hash>` component shared by the v1 project dir and the v2
    /// work-dir key. Bit-identical to the historical hash.
    fn project_hash(cwd: &Path) -> String {
        let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned());
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        canonical.to_string_lossy().hash(&mut hash);
        format!("{:016x}", hash.finish())
    }

    /// v2 work-dir key: `<16-hex-hash>-<sanitized-basename>` (§1.1). The hash
    /// prefix is the only component ever matched against; the suffix is
    /// cosmetic and computed once at directory creation.
    pub(crate) fn workdir_key(cwd: &Path) -> String {
        let hash = Self::project_hash(cwd);
        let suffix = workdir_key_suffix(cwd);
        if suffix.is_empty() {
            hash
        } else {
            format!("{hash}-{suffix}")
        }
    }

    /// Locate the v2 work-dir for `cwd`, matching an existing directory by hash
    /// prefix (a stale suffix after a cwd rename is harmless), else reserve the
    /// freshly derived name.
    pub(crate) fn resolve_workdir_dir(data_root: &Path, cwd: &Path) -> PathBuf {
        let hash = Self::project_hash(cwd);
        let sessions_root = data_root.join(SESSIONS_ROOT_DIR);
        if let Ok(entries) = fs::read_dir(&sessions_root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !entry.path().is_dir() {
                    continue;
                }
                if name == hash || name.starts_with(&format!("{hash}-")) {
                    return entry.path();
                }
            }
        }
        sessions_root.join(Self::workdir_key(cwd))
    }

    pub fn open(data_root: &Path, cwd: &Path) -> Result<Arc<Self>, SessionError> {
        Self::open_with_layout(data_root, cwd, LayoutChoice::PreferV2)
    }

    #[cfg(test)]
    #[allow(dead_code)] // layout-pinned fixtures land in P1/P4
    pub(crate) fn open_flat_for_test(
        data_root: &Path,
        cwd: &Path,
    ) -> Result<Arc<Self>, SessionError> {
        Self::open_with_layout(data_root, cwd, LayoutChoice::ForceFlat)
    }

    #[cfg(test)]
    #[allow(dead_code)] // layout-pinned fixtures land in P1/P4
    pub(crate) fn open_v2_for_test(
        data_root: &Path,
        cwd: &Path,
    ) -> Result<Arc<Self>, SessionError> {
        Self::open_with_layout(data_root, cwd, LayoutChoice::ForceV2)
    }

    fn open_with_layout(
        data_root: &Path,
        cwd: &Path,
        layout: LayoutChoice,
    ) -> Result<Arc<Self>, SessionError> {
        if matches!(layout, LayoutChoice::PreferV2) {
            // Blocking v1 -> v2 migration (§6.2): the store never serves a
            // half-migrated layout because this gate runs before construction.
            crate::migration::run_if_needed(data_root, cwd, &crate::migration::stderr_progress)?;
        }
        let v2_dir = Self::resolve_workdir_dir(data_root, cwd);
        let legacy_root = Self::project_dir(data_root, cwd);
        let legacy_dir = legacy_root.join("sessions");
        // Migration is what promotes a legacy store; until it runs the store
        // keeps serving the flat v1 layout it already has on disk.
        let flat_layout = match layout {
            LayoutChoice::PreferV2 => {
                !v2_dir.join(LAYOUT_MARKER_FILE).is_file() && legacy_dir.is_dir()
            }
            LayoutChoice::ForceFlat => true,
            LayoutChoice::ForceV2 => false,
        };
        let (workdir_dir, project_dir) = if flat_layout {
            (legacy_dir, legacy_root)
        } else {
            (v2_dir.clone(), v2_dir.clone())
        };
        #[cfg(unix)]
        create_unix_session_directory_all(&workdir_dir)?;
        #[cfg(windows)]
        for path in [
            data_root.join(if flat_layout {
                "projects"
            } else {
                SESSIONS_ROOT_DIR
            }),
            project_dir.clone(),
            workdir_dir.clone(),
        ] {
            create_windows_session_directory(&path)?;
        }
        if !flat_layout {
            write_layout_marker_if_absent(&project_dir)?;
        }
        write_project_cwd(&project_dir, cwd)?;
        let store = Arc::new(Self {
            data_root: data_root.to_owned(),
            workdir_dir: workdir_dir.clone(),
            project_dir,
            flat_layout,
            cwd: cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned()),
            locations: Mutex::new(HashMap::new()),
            trees: Mutex::new(HashMap::new()),
            tree_locks: Mutex::new(HashMap::new()),
            tree_observer: ObserverSlot::default(),
            residency: Mutex::new(SessionResidency::default()),
            ownership: Mutex::new(HashMap::new()),
            adoption_locks: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
            closed: AtomicBool::new(false),
            #[cfg(test)]
            eviction_transition_hook: Mutex::new(None),
            #[cfg(test)]
            publish_hook: Mutex::new(None),
            #[cfg(test)]
            tree_load_count: Mutex::new(HashMap::new()),
        });
        store.refresh_discovered();
        Ok(store)
    }

    /// The data root this store was opened against (migration detection).
    #[must_use]
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    #[must_use]
    pub fn is_flat_layout(&self) -> bool {
        self.flat_layout
    }

    /// Path of `id`'s session directory for a known placement.
    fn path_for(&self, location: SessionLocation, id: SessionId) -> PathBuf {
        if self.flat_layout {
            return self.workdir_dir.join(id.to_string());
        }
        match location {
            SessionLocation::Root => self.workdir_dir.join(id.to_string()),
            SessionLocation::Child { root } => self
                .workdir_dir
                .join(root.to_string())
                .join(SUBAGENTS_DIR)
                .join(id.to_string()),
        }
    }

    #[must_use]
    fn cached_location(&self, id: SessionId) -> Option<SessionLocation> {
        self.locations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&id)
            .copied()
    }

    fn record_location(&self, id: SessionId, location: SessionLocation) {
        self.locations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, location);
    }

    /// Root a session belongs to (`None` for a root session itself). Unknown
    ///
    /// (consumed by the lazy-tree passes; wired up in P2)
    /// children are located on disk first so the answer is layout-accurate.
    #[allow(dead_code)] // wired up by the lazy-tree passes (P2)
    pub(crate) fn root_of(&self, id: SessionId) -> Result<SessionId, SessionError> {
        self.resolve_dir(id)?;
        match self.cached_location(id) {
            Some(SessionLocation::Child { root }) => Ok(root),
            _ => Ok(id),
        }
    }

    /// Fallible replacement for the historical flat `session_dir()` (§2.2).
    pub(crate) fn resolve_dir(&self, id: SessionId) -> Result<PathBuf, SessionError> {
        if let Some(location) = self.cached_location(id) {
            return Ok(self.path_for(location, id));
        }
        let root_dir = self.workdir_dir.join(id.to_string());
        if root_dir.is_dir() {
            self.record_location(id, SessionLocation::Root);
            return Ok(root_dir);
        }
        if !self.flat_layout
            && let Some(root) = self.locate_child(id)
        {
            self.record_location(id, SessionLocation::Child { root });
            return Ok(self.path_for(SessionLocation::Child { root }, id));
        }
        Err(SessionError::Missing(id))
    }

    /// Direct-address locate for an unknown child: stat each root's
    /// `subagents/<id>/metadata` (or the legacy `meta.json`). O(#roots) stats,
    /// paid once per unknown child before `locations` caches the answer.
    fn locate_child(&self, id: SessionId) -> Option<SessionId> {
        for root in self.root_dir_ids() {
            let dir = self
                .workdir_dir
                .join(root.to_string())
                .join(SUBAGENTS_DIR)
                .join(id.to_string());
            if dir.join(SESSION_META_FILE).exists() || dir.join(LEGACY_SESSION_META_FILE).exists() {
                return Some(root);
            }
        }
        None
    }

    /// Session-id-named directories directly inside the work dir. Under the v2
    /// layout these are roots *by construction*; the flat layout filters by the
    /// cached origin instead.
    fn root_dir_ids(&self) -> Vec<SessionId> {
        let mut ids = Vec::new();
        let Ok(entries) = fs::read_dir(&self.workdir_dir) else {
            return ids;
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let Ok(id) = entry.file_name().to_string_lossy().parse::<SessionId>() else {
                continue;
            };
            ids.push(id);
        }
        ids
    }

    /// Single source of truth for where a *new* session is created (§2.2).
    fn placement_for(&self, origin: &SessionOrigin) -> SessionLocation {
        if self.flat_layout {
            return SessionLocation::Root;
        }
        match origin {
            SessionOrigin::Root => SessionLocation::Root,
            SessionOrigin::Delegated {
                root_session_id, ..
            } => SessionLocation::Child {
                root: *root_session_id,
            },
        }
    }

    /// Origin carried by a creation payload, which decides placement.
    fn creation_origin(creation: &EventPayload) -> Option<SessionOrigin> {
        match creation {
            EventPayload::SessionCreated { origin, .. } => Some(origin.clone()),
            _ => None,
        }
    }

    /// Directory a session's metadata cache lives in, plus its placement.
    fn dir_for_placement(&self, location: SessionLocation, id: SessionId) -> PathBuf {
        self.path_for(location, id)
    }

    /// Directory a new session directory is renamed into (must share the
    /// filesystem with its temporary sibling).
    fn publish_parent_for(&self, location: SessionLocation) -> Result<PathBuf, SessionError> {
        match location {
            SessionLocation::Root => Ok(self.workdir_dir.clone()),
            SessionLocation::Child { root } => self.ensure_subagents_dir(root),
        }
    }

    /// Ensure a root's `subagents/` directory exists, returning it.
    fn ensure_subagents_dir(&self, root: SessionId) -> Result<PathBuf, SessionError> {
        let dir = self.workdir_dir.join(root.to_string()).join(SUBAGENTS_DIR);
        if !dir.exists() {
            #[cfg(unix)]
            create_unix_session_directory_all(&dir)?;
            #[cfg(windows)]
            create_windows_session_directory(&dir)?;
        }
        Ok(dir)
    }

    pub fn create(
        &self,
        session_id: SessionId,
        origin: cookie_agent_protocol::EventOrigin,
        creation: EventPayload,
    ) -> Result<Arc<EventLog>, SessionError> {
        self.create_with_status(session_id, origin, creation)
            .map(|(log, _)| log)
    }

    /// Creates a session atomically and reports whether this caller won creation.
    pub fn create_with_status(
        &self,
        session_id: SessionId,
        origin: cookie_agent_protocol::EventOrigin,
        creation: EventPayload,
    ) -> Result<(Arc<EventLog>, bool), SessionError> {
        let _mutation = self.lock_mutation();
        self.ensure_open()?;
        if let Some(existing) = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .get(&session_id)
            .cloned()
        {
            return Ok((existing.log, false));
        }
        let creation_origin = Self::creation_origin(&creation).unwrap_or(SessionOrigin::Root);
        let location = self.placement_for(&creation_origin);
        self.record_location(session_id, location);
        let final_dir = self.dir_for_placement(location, session_id);
        if final_dir.exists() {
            return Err(SessionError::SessionLocked(session_id));
        }
        let authority = WriteAuthority::new();
        let log = EventLog::create_buffered_owned(
            final_dir.join(EVENTS_FILE),
            session_id,
            origin,
            creation,
            authority.capability(),
        )?;
        let result = projection(log.clone())?;
        self.ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id, StoreOwnership::PendingPublish { authority });
        {
            let mut residency = self
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            residency.resident.insert(session_id, result);
            residency.evicted.remove(&session_id);
        }
        self.note_placed_child(&location, &creation_origin, session_id);
        Ok((log, true))
    }

    pub fn get(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        if let Some(session) = self.get_resident(id) {
            return Ok(session);
        }
        self.ensure_tree_for(id)?;
        if self.is_owned(id) {
            return self.reopen_owned(id);
        }
        self.open_snapshot(id, true)
    }

    /// Read a session without completing its tree first.
    ///
    /// Startup passes need a session's own log for bookkeeping; going through
    /// [`Self::get`] would put child reads back on the startup path (§4.1.1).
    pub(crate) fn get_log_only(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        if let Some(session) = self.get_resident(id) {
            return Ok(session);
        }
        if self.is_owned(id) {
            return self.reopen_owned(id);
        }
        self.open_snapshot(id, true)
    }

    #[must_use]
    pub fn get_resident(&self, id: SessionId) -> Option<SessionProjection> {
        self.residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .get(&id)
            .cloned()
    }

    /// Inspects recovery controls without paging an owned dormant session back in.
    pub(crate) fn recovery_event_snapshot(
        &self,
        id: SessionId,
    ) -> Result<Arc<[cookie_agent_protocol::StoredEvent]>, SessionError> {
        if let Some(session) = self.get_resident(id) {
            return Ok(session.log.event_snapshot());
        }
        Ok(EventLog::open_read_only(self.resolve_dir(id)?.join(EVENTS_FILE), id)?.event_snapshot())
    }

    /// Arc clone of the resident log plus its persistence flag, without
    /// cloning the whole projection. Used by the append hot path, which only
    /// needs the log and the `first_user_message` gate.
    fn resident_log(&self, id: SessionId) -> Result<(Arc<EventLog>, bool), SessionError> {
        let log = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .get(&id)
            .map(|session| session.log.clone());
        match log {
            Some(log) => {
                let persisted = log.is_persisted();
                Ok((log, persisted))
            }
            None => {
                let session = self.get(id)?;
                let persisted = session.log.is_persisted();
                Ok((session.log, persisted))
            }
        }
    }

    fn reopen_owned(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        let _mutation = self.lock_mutation();
        if let Some(session) = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .get(&id)
            .cloned()
        {
            return Ok(session);
        }
        let session_dir = self.resolve_dir(id)?;
        if !session_dir.is_dir() {
            return Err(SessionError::Missing(id));
        }
        let capability = self.write_capability(id, false)?;
        let log = EventLog::open_owned(session_dir.join(EVENTS_FILE), id, capability)?;
        let reopened = projection(log)?;
        let mut residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        residency.resident.insert(id, reopened.clone());
        residency.evicted.remove(&id);
        Ok(reopened)
    }

    /// Reads and folds one session log without making it resident.
    ///
    /// `direct` marks a read the caller asked for by session id (a `get` or a
    /// direct-address child access). Child logs are otherwise only legal inside
    /// [`Self::load_tree`], and a debug build asserts that invariant so hidden
    /// child loads show up in CI instead of in startup profiles (§3.2.3, §3.3).
    fn open_snapshot(
        &self,
        id: SessionId,
        direct: bool,
    ) -> Result<SessionProjection, SessionError> {
        debug_assert!(
            direct || TreeLoadReads::active() || !self.is_filed_child(id),
            "child log {id} opened outside a tree load"
        );
        let session_dir = self.resolve_dir(id)?;
        if !session_dir.is_dir() {
            return Err(SessionError::Missing(id));
        }
        let snapshot = projection(EventLog::open_read_only(session_dir.join(EVENTS_FILE), id)?)?;
        self.residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .evicted
            .insert(id, summary_from_projection(&snapshot));
        Ok(snapshot)
    }

    /// Takes the store's durable-mutation lock, noting the holder on this
    /// thread. Paths that already hold it must not start a lazy tree load: the
    /// load's install phase needs the same lock, and waiting for another thread's
    /// tree load while holding it inverts the two (append, create, fork and
    /// `begin_write_locked` all reach `get` from inside a mutation).
    fn lock_mutation(&self) -> MutationGuard<'_> {
        if MUTATION_DEPTH.with(|depth| depth.get()) > 0 {
            return MutationGuard { locked: None };
        }
        let guard = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        MUTATION_DEPTH.with(|depth| depth.set(depth.get() + 1));
        MutationGuard {
            locked: Some(guard),
        }
    }

    #[must_use]
    fn mutation_held() -> bool {
        MUTATION_DEPTH.with(|depth| depth.get() > 0)
    }

    pub(crate) fn begin_write(&self, id: SessionId) -> Result<WriteOpen, SessionError> {
        // The tree has to be complete before a session in it starts writing, and
        // the load cannot run under `mutation` (it installs its own).
        self.ensure_tree_for(id)?;
        let _mutation = self.lock_mutation();
        self.ensure_open()?;
        self.begin_write_locked(id)
    }

    #[cfg(test)]
    pub(crate) fn open_for_write(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        let adoption_lock = self.adoption_lock(id);
        let _adoption = adoption_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.begin_write(id)? == WriteOpen::Adopting {
            self.commit_adoption(id)?;
        }
        self.get(id)
    }

    fn begin_write_locked(&self, id: SessionId) -> Result<WriteOpen, SessionError> {
        if self.is_owned(id) {
            if let Some(session) = self.get_resident(id) {
                drop(session);
                return Ok(WriteOpen::AlreadyOwned);
            }
            let session_dir = self.resolve_dir(id)?;
            let capability = self.write_capability(id, false)?;
            let reopened = projection(EventLog::open_owned(
                session_dir.join(EVENTS_FILE),
                id,
                capability,
            )?)?;
            let mut residency = self
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            residency.resident.insert(id, reopened.clone());
            residency.evicted.remove(&id);
            return Ok(WriteOpen::AlreadyOwned);
        }
        let session_dir = self.resolve_dir(id)?;
        if !session_dir.is_dir() {
            return Err(SessionError::Missing(id));
        }
        let lock = match try_acquire(&session_dir) {
            Ok(SessionOwnership::Owned(lock)) => lock,
            Ok(SessionOwnership::Foreign) => {
                self.ownership
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id, StoreOwnership::Foreign);
                return Err(SessionError::SessionLocked(id));
            }
            Err(error) => {
                eprintln!("session {id} ownership classification failed: {error}");
                self.ownership
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id, StoreOwnership::Foreign);
                return Err(SessionError::SessionLocked(id));
            }
        };
        let authority = WriteAuthority::new();
        let opened =
            EventLog::open_owned(session_dir.join(EVENTS_FILE), id, authority.capability());
        let projection = match opened.and_then(|log| {
            projection(log).map_err(|error| match error {
                SessionError::Event(error) => error,
                _ => unreachable!("projection only returns event-log errors"),
            })
        }) {
            Ok(projection) => projection,
            Err(error) => {
                eprintln!("session {id} adoption failed closed: {error}");
                return Err(SessionError::SessionLocked(id));
            }
        };
        self.ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                id,
                StoreOwnership::Adopting {
                    _lock: lock,
                    authority,
                },
            );
        let mut residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        residency.resident.insert(id, projection.clone());
        residency.evicted.remove(&id);
        Ok(WriteOpen::Adopting)
    }

    pub(crate) fn commit_adoption(&self, id: SessionId) -> Result<(), SessionError> {
        self.ensure_open()?;
        let mut ownership = self
            .ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = ownership
            .remove(&id)
            .ok_or(SessionError::SessionLocked(id))?;
        match state {
            StoreOwnership::Adopting { _lock, authority } => {
                ownership.insert(id, StoreOwnership::Owned { _lock, authority });
                Ok(())
            }
            state => {
                ownership.insert(id, state);
                Err(SessionError::SessionLocked(id))
            }
        }
    }

    pub(crate) fn rollback_adoption(&self, id: SessionId) {
        let projection = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .remove(&id);
        if let Some(projection) = projection {
            let _ = projection.log.suspend_writer();
            self.residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .evicted
                .insert(id, summary_from_projection(&projection));
        }
        let removed = self
            .ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        debug_assert!(
            matches!(removed, Some(StoreOwnership::Adopting { .. }))
                || self.closed.load(Ordering::Acquire)
        );
    }

    pub(crate) fn adoption_lock(&self, id: SessionId) -> Arc<Mutex<()>> {
        self.adoption_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn write_capability(
        &self,
        id: SessionId,
        allow_adopting: bool,
    ) -> Result<WriteCapability, SessionError> {
        let ownership = self
            .ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match ownership.get(&id) {
            Some(
                StoreOwnership::PendingPublish { authority }
                | StoreOwnership::Owned { authority, .. },
            ) => Ok(authority.capability()),
            Some(StoreOwnership::Adopting { authority, .. }) if allow_adopting => {
                Ok(authority.capability())
            }
            _ => Err(SessionError::SessionLocked(id)),
        }
    }

    #[must_use]
    pub fn is_owned(&self, id: SessionId) -> bool {
        matches!(
            self.ownership
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&id),
            Some(StoreOwnership::PendingPublish { .. } | StoreOwnership::Owned { .. })
        )
    }

    pub fn evict(&self, id: SessionId) -> Result<bool, SessionError> {
        let _mutation = self.lock_mutation();
        // Persisted child caches are refreshed after the residency guard drops.
        let parent_root = self.parent_root_of(id);
        let evicted = self.evict_locked(id)?;
        if evicted && let Some(root) = parent_root {
            self.persist_subagent_index(root);
        }
        Ok(evicted)
    }

    /// The root a session is filed under, from the location cache only (never
    /// resolves paths, so it is safe to call while store locks are held).
    fn parent_root_of(&self, id: SessionId) -> Option<SessionId> {
        match self.cached_location(id) {
            Some(SessionLocation::Child { root }) => Some(root),
            Some(SessionLocation::Root) | None => {
                if self.flat_layout {
                    return None;
                }
                match self.cached_origin(id) {
                    Some(SessionOrigin::Delegated {
                        root_session_id, ..
                    }) => Some(root_session_id),
                    _ => None,
                }
            }
        }
    }

    fn evict_locked(&self, id: SessionId) -> Result<bool, SessionError> {
        let mut residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(session) = residency.resident.get(&id) else {
            return Ok(false);
        };
        if !session.log.is_persisted() {
            return Ok(false);
        }
        // Stream records may be published before their grouped sync. Flush before
        // removing the resident projection so eviction never outruns durability.
        session.log.flush()?;
        let summary = SessionSummary {
            meta: session.meta.clone(),
            usage: session.usage.clone(),
            usage_rollup: session.usage_rollup.clone(),
            agent_usage: session.agent_usage.clone(),
        };
        residency.evicted.insert(id, summary);
        #[cfg(test)]
        if let Some(hook) = self
            .eviction_transition_hook
            .lock()
            .expect("eviction transition hook lock poisoned")
            .take()
        {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("eviction transition reached lock poisoned")
                .take()
            {
                let _ = reached.send(id);
            }
            let _ = hook
                .release
                .lock()
                .expect("eviction transition release lock poisoned")
                .recv();
        }
        residency.resident.remove(&id);
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn install_eviction_transition_hook_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<SessionId>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        *self
            .eviction_transition_hook
            .lock()
            .expect("eviction transition hook lock poisoned") = Some(EvictionTransitionHook {
            reached: Mutex::new(Some(reached)),
            release: Mutex::new(release_receiver),
        });
        (receiver, release)
    }

    #[cfg(test)]
    fn install_publish_hook_for_test(
        &self,
    ) -> (
        std::sync::mpsc::Receiver<SessionId>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached, reached_receiver) = std::sync::mpsc::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        *self
            .publish_hook
            .lock()
            .expect("publish hook lock poisoned") = Some(PublishHook {
            reached,
            release: release_receiver,
        });
        (reached_receiver, release)
    }

    pub fn append(
        &self,
        id: SessionId,
        run: Option<RunId>,
        origin: cookie_agent_protocol::EventOrigin,
        event: EventPayload,
    ) -> Result<cookie_agent_protocol::StoredEvent, SessionError> {
        self.append_with_mode(id, run, origin, event, false)
    }

    pub(crate) fn append_recovery(
        &self,
        id: SessionId,
        run: Option<RunId>,
        origin: cookie_agent_protocol::EventOrigin,
        event: EventPayload,
    ) -> Result<cookie_agent_protocol::StoredEvent, SessionError> {
        self.append_with_mode(id, run, origin, event, true)
    }

    fn append_with_mode(
        &self,
        id: SessionId,
        run: Option<RunId>,
        origin: cookie_agent_protocol::EventOrigin,
        event: EventPayload,
        recovery: bool,
    ) -> Result<cookie_agent_protocol::StoredEvent, SessionError> {
        let _mutation = self.lock_mutation();
        self.ensure_open()?;
        let capability = self.write_capability(id, recovery)?;
        let (log, was_persisted) = self.resident_log(id)?;
        let first_user_message = !was_persisted
            && matches!(
                event,
                EventPayload::UserInputAdmitted { .. }
                    | EventPayload::GoalActivated { .. }
                    | EventPayload::ProducerMessageAccepted { .. }
                    | EventPayload::UserInputSubmitted { .. }
                    | EventPayload::DelegatedContextSeeded { .. }
                    | EventPayload::SessionPermissionOverlaySet { .. }
                    | EventPayload::SkillLoaded { .. }
                    | EventPayload::AgentMdLoaded { .. }
            );
        let envelope = log.append_owned(&capability, run, origin, event)?;
        // Fold-ignored payloads (the per-token TextDelta/ReasoningDelta and
        // per-chunk ToolCallProgress hot path) only advance the metadata tip;
        // update the resident projection in place instead of re-folding the
        // whole log. The resident is updated before write_cache — on a cache
        // write failure the resident stays consistent with the log while the
        // on-disk discovery cache lags, which is strictly better than the
        // previous resident-behind-log window.
        let incremental_meta = if fold_consumed(&envelope.payload) {
            None
        } else {
            let mut residency = self
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if residency.resident.contains_key(&id) {
                residency.evicted.remove(&id);
                let resident = residency.resident.get_mut(&id).expect("checked above");
                resident.meta.last_event_seq = envelope.seq;
                resident.meta.last_activity = envelope.timestamp;
                Some(resident.meta.clone())
            } else {
                None
            }
        };
        if let Some(meta) = incremental_meta {
            if first_user_message {
                let projection = self.get(id)?;
                self.persist_buffered(id, &projection)?;
            } else if log.is_persisted() {
                write_cache(&self.meta_cache_path(id)?, &meta)?;
            }
        } else {
            let rebuilt = projection(log.clone())?;
            if first_user_message {
                self.persist_buffered(id, &rebuilt)?;
            } else if log.is_persisted() {
                write_cache(&self.meta_cache_path(id)?, &rebuilt.meta)?;
            }
            {
                let mut residency = self
                    .residency
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                residency.resident.insert(id, rebuilt);
                residency.evicted.remove(&id);
            }
            // The publish that just happened changed the child's durable summary.
            if !was_persisted && let Some(root) = self.parent_root_of(id) {
                self.persist_subagent_index(root);
            }
        }
        // Run-terminal events are rare and are the only thing the delegation
        // registry needs from a child log, so cache them per tree (§4.1.3).
        if let Some(terminal) = terminal_run_of(run, &envelope.payload) {
            self.record_terminal_run(id, terminal.0, terminal.1);
        }
        #[cfg(test)]
        {
            let reference = projection_fold(log).expect("reference fold");
            let resident = self.get_resident(id).expect("resident after append");
            assert_projection_equivalent(&resident, &reference);
        }
        Ok(envelope)
    }

    pub fn fork(
        &self,
        source_id: SessionId,
        through_seq: u64,
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<SessionId, SessionError> {
        let _mutation = self.lock_mutation();
        self.ensure_open()?;
        let source = self.get(source_id)?;
        if !source.log.is_persisted() {
            return Err(SessionError::InvalidSequence {
                session_id: source_id,
                through_seq,
            });
        }
        source.log.suspend_writer()?;
        let source_events = source.log.all_events();
        let prefix = source_events
            .iter()
            .filter(|event| event.seq <= through_seq)
            .cloned()
            .collect::<Vec<_>>();
        if through_seq == 0
            || through_seq > source_events.last().map_or(0, |event| event.seq)
            || !cookie_agent_protocol::visible_events(&prefix)
                .iter()
                .any(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
        {
            return Err(SessionError::InvalidSequence {
                session_id: source_id,
                through_seq,
            });
        }

        let session_id = SessionId::new_v7();
        // A forked origin is copied verbatim from the source's `SessionCreated`
        // event, so a fork of a child stays inside the same tree.
        let location = self.placement_for(&source.meta.origin);
        self.record_location(session_id, location);
        let destination_parent = self.publish_parent_for(location)?;
        let final_dir = self.dir_for_placement(location, session_id);
        let temporary =
            destination_parent.join(format!(".{session_id}.{}.tmp", SessionId::new_v7()));
        #[cfg(unix)]
        fs::create_dir(&temporary).map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        #[cfg(windows)]
        create_windows_session_directory(&temporary)?;
        #[cfg(unix)]
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700)).map_err(|source| {
            SessionError::Io {
                path: temporary.clone(),
                source,
            }
        })?;
        let authority = WriteAuthority::new();
        let capability = authority.capability();
        let result = (|| {
            let log_path = temporary.join("events.jsonl");
            #[cfg(windows)]
            create_windows_session_file(&log_path)?;
            for event in source_events
                .iter()
                .filter(|event| event.seq <= through_seq)
            {
                let mut copied = event.clone();
                copied.session_id = session_id;
                crate::events::append_copied_event_jsonl(&log_path, &copied)?;
            }
            #[cfg(unix)]
            fs::set_permissions(&log_path, fs::Permissions::from_mode(0o600)).map_err(
                |source| SessionError::Io {
                    path: log_path.clone(),
                    source,
                },
            )?;
            let log = EventLog::open_owned(log_path, session_id, capability.clone())?;
            log.append_owned(
                &capability,
                None,
                origin,
                EventPayload::SessionReverted { through_seq },
            )?;
            let prefix_projection = projection(log.clone())?;
            let title = fork_title(prefix_projection.meta.title.as_ref())?;
            log.append_owned(
                &capability,
                None,
                cookie_agent_protocol::EventOrigin::new("user")
                    .expect("static event origin is valid"),
                EventPayload::SessionTitleCommitted {
                    change: SessionTitleChange::UserSet {
                        title,
                        client_rename_id: ClientRenameId::new(format!("fork-{session_id}"))
                            .expect("fork rename ID is bounded"),
                    },
                    input_through_seq: through_seq,
                },
            )?;
            log.suspend_writer()?;
            let fork_projection = projection(log)?;
            write_cache(
                &temporary.join(self.meta_write_name()),
                &fork_projection.meta,
            )?;
            #[cfg(unix)]
            let lock_session_dir = &temporary;
            #[cfg(windows)]
            let lock_session_dir = &final_dir;
            let lock = match try_acquire(lock_session_dir).map_err(|source| SessionError::Io {
                path: owner_lock_path(lock_session_dir),
                source,
            })? {
                SessionOwnership::Owned(lock) => lock,
                SessionOwnership::Foreign => return Err(SessionError::SessionLocked(session_id)),
            };
            #[cfg(test)]
            if let Some(hook) = self
                .publish_hook
                .lock()
                .expect("publish hook lock poisoned")
                .take()
            {
                let _ = hook.reached.send(session_id);
                let _ = hook.release.recv();
            }
            fsync_directory(&temporary)?;
            publish_prepared_dir(&temporary, &final_dir, session_id)?;
            fsync_directory(&destination_parent)?;
            self.ownership
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    session_id,
                    StoreOwnership::Owned {
                        _lock: lock,
                        authority,
                    },
                );
            let log = EventLog::open_owned(final_dir.join("events.jsonl"), session_id, capability)?;
            let fork_projection = projection(log)?;
            let fork_origin = fork_projection.meta.origin.clone();
            self.residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .resident
                .insert(session_id, fork_projection);
            self.note_placed_child(&location, &fork_origin, session_id);
            Ok(session_id)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
        }
        result
    }

    fn persist_buffered(
        &self,
        session_id: SessionId,
        projection: &SessionProjection,
    ) -> Result<(), SessionError> {
        self.write_capability(session_id, false)?;
        let location = self.placement_for(&projection.meta.origin);
        self.record_location(session_id, location);
        self.note_placed_child(&location, &projection.meta.origin, session_id);
        let destination_parent = self.publish_parent_for(location)?;
        let final_dir = self.dir_for_placement(location, session_id);
        let temporary =
            destination_parent.join(format!(".{session_id}.{}.tmp", SessionId::new_v7()));
        #[cfg(unix)]
        create_unix_session_directory_all(&temporary)?;
        #[cfg(windows)]
        create_windows_session_directory(&temporary)?;
        let result = (|| {
            let log_path = temporary.join("events.jsonl");
            #[cfg(windows)]
            create_windows_session_file(&log_path)?;
            for event in projection.log.all_events() {
                crate::events::append_jsonl(&log_path, &event)?;
            }
            write_cache(&temporary.join(self.meta_write_name()), &projection.meta)?;
            #[cfg(unix)]
            let lock_session_dir = &temporary;
            #[cfg(windows)]
            let lock_session_dir = &final_dir;
            let lock = match try_acquire(lock_session_dir).map_err(|source| SessionError::Io {
                path: owner_lock_path(lock_session_dir),
                source,
            })? {
                SessionOwnership::Owned(lock) => lock,
                SessionOwnership::Foreign => return Err(SessionError::SessionLocked(session_id)),
            };
            #[cfg(test)]
            if let Some(hook) = self
                .publish_hook
                .lock()
                .expect("publish hook lock poisoned")
                .take()
            {
                let _ = hook.reached.send(session_id);
                let _ = hook.release.recv();
            }
            fsync_directory(&temporary)?;
            publish_prepared_dir(&temporary, &final_dir, session_id)?;
            fsync_directory(&destination_parent)?;
            let mut ownership = self
                .ownership
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let state = ownership
                .remove(&session_id)
                .ok_or(SessionError::SessionLocked(session_id))?;
            match state {
                StoreOwnership::PendingPublish { authority } => {
                    ownership.insert(
                        session_id,
                        StoreOwnership::Owned {
                            _lock: lock,
                            authority,
                        },
                    );
                }
                state => {
                    ownership.insert(session_id, state);
                    return Err(SessionError::SessionLocked(session_id));
                }
            }
            Ok::<(), SessionError>(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
            return result;
        }
        projection.log.mark_persisted();
        Ok(())
    }

    pub(crate) fn persist_buffered_session(&self, id: SessionId) -> Result<(), SessionError> {
        let _mutation = self.lock_mutation();
        self.ensure_open()?;
        let projection = self.get(id)?;
        if !projection.log.is_persisted() {
            self.persist_buffered(id, &projection)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn all(&self) -> Vec<SessionProjection> {
        self.residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .values()
            .cloned()
            .collect()
    }

    /// Every snapshot known to the store. Superseded by [`Self::root_snapshots`]
    /// and [`Self::tree_snapshots`]; retained until the last caller decides.
    pub fn all_snapshots(&self) -> Vec<SessionProjection> {
        self.refresh_discovered();
        let ids = {
            let residency = self
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            residency
                .resident
                .keys()
                .chain(residency.evicted.keys())
                .copied()
                .collect::<HashSet<_>>()
        };
        ids.into_iter()
            .filter_map(|id| match self.get(id) {
                Ok(session) => Some(session),
                Err(error) => {
                    eprintln!("session {id} snapshot skipped: {error}");
                    None
                }
            })
            .collect()
    }
    /// Snapshots of the sessions that are roots of their own tree. This is the
    /// startup pass shape: never touches delegated child logs.
    pub fn root_snapshots(&self) -> Vec<SessionProjection> {
        self.refresh_discovered();
        let ids = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .known_ids()
            .into_iter()
            .filter(|id| self.is_root_id(*id))
            .collect::<Vec<_>>();

        self.snapshots_for(ids)
    }

    /// Snapshots of `root` plus every session delegated from it.
    pub fn tree_snapshots(&self, root: SessionId) -> Vec<SessionProjection> {
        self.refresh_discovered();
        let mut ids = vec![root];
        ids.extend(
            self.residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .known_ids()
                .into_iter()
                .filter(|id| *id != root && self.is_tree_member(root, *id)),
        );
        self.snapshots_for(ids)
    }

    fn snapshots_for(&self, ids: Vec<SessionId>) -> Vec<SessionProjection> {
        ids.into_iter()
            .filter_map(|id| match self.read_snapshot(id) {
                Ok(session) => Some(session),
                Err(error) => {
                    eprintln!("session {id} snapshot skipped: {error}");
                    None
                }
            })
            .collect()
    }

    /// Read-only snapshot access that never triggers a lazy tree load. Startup
    /// passes and the delegation registry rebuild use this so a cold root stays
    /// cold until something actually resumes it.
    /// The root whose tree must be complete before serving `id` (§3.2).
    pub(crate) fn ensure_tree_for(&self, id: SessionId) -> Result<(), SessionError> {
        if self.flat_layout {
            return Ok(());
        }
        if Self::mutation_held() {
            // Reached from inside a write path: its entry point (`begin_write`,
            // the engine, or an earlier read) already completed this tree.
            return Ok(());
        }
        if self.resolve_dir(id).is_err() {
            // Unknown sessions stay unknown: the caller reports the miss, and a
            // later create can still place them.
            return Ok(());
        }
        let root = match self.cached_location(id) {
            Some(SessionLocation::Child { root }) => root,
            _ => id,
        };
        self.load_tree(root)
    }

    /// Whether the child tree of `root` was already bulk-loaded in this process.
    #[must_use]
    pub(crate) fn is_tree_loaded(&self, root: SessionId) -> bool {
        self.trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&root)
            .is_some_and(|state| state.loaded)
    }

    #[cfg(test)]
    fn note_tree_load(&self, root: SessionId) {
        *self
            .tree_load_count
            .lock()
            .expect("tree load count lock poisoned")
            .entry(root)
            .or_default() += 1;
    }

    /// How many times a root's child tree was bulk-loaded in this process.
    #[cfg(test)]
    pub(crate) fn tree_load_count(&self, root: SessionId) -> usize {
        self.tree_load_count
            .lock()
            .expect("tree load count lock poisoned")
            .get(&root)
            .copied()
            .unwrap_or(0)
    }

    fn tree_guard(&self, root: SessionId) -> Arc<Mutex<()>> {
        let mut locks = self
            .tree_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            locks
                .entry(root)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Reads and folds every child log of `root` exactly once per process
    /// (§3.3), harvesting what the engine singletons need — delegation records,
    /// restart-stable grants, producer state, manifest bindings and summaries —
    /// and leaving no child resident. Idempotent; concurrent triggers coalesce
    /// per root.
    pub(crate) fn load_tree(&self, root: SessionId) -> Result<(), SessionError> {
        if self.flat_layout || self.is_tree_loaded(root) {
            return Ok(());
        }
        let guard = self.tree_guard(root);
        let _loading = guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.is_tree_loaded(root) {
            return Ok(());
        }
        self.ensure_open()?;
        #[cfg(test)]
        self.note_tree_load(root);
        let mut products = TreeLoadProducts::for_root(root);
        let mut tree_grants = Vec::new();
        let mut edges = HashMap::<SessionId, Vec<SessionId>>::new();
        let mut terminal_runs = HashMap::<SessionId, BTreeMap<String, SessionStatus>>::new();
        {
            // Read phase: no store lock is held, so the pass cannot block appends
            // and cannot re-enter the store's own write paths.
            let _reads = TreeLoadReads::begin();
            for child in self.child_dir_ids(root) {
                let projection = self.open_snapshot(child, false)?;
                let events = projection.log.event_snapshot();
                products
                    .summaries
                    .push(summary_from_projection(&projection));
                for envelope in events.iter() {
                    match &envelope.payload {
                        EventPayload::SessionCreated { creation_agent, .. } => {
                            products.bindings.extend(
                                creation_agent
                                    .fallback_chain
                                    .iter()
                                    .cloned()
                                    .map(|binding| (projection.meta.session_id, binding)),
                            )
                        }
                        EventPayload::RunStarted {
                            selected_suffix, ..
                        } => products.bindings.extend(
                            selected_suffix
                                .iter()
                                .cloned()
                                .map(|binding| (projection.meta.session_id, binding)),
                        ),
                        EventPayload::TreeApprovalGrantCommitted { grant } => {
                            // The visible-grant rebuild needs every grant; only
                            // the approval store filters to restart-stable ones.
                            if restart_stable_grant(grant) {
                                products.grants.push(grant.clone());
                            }
                            tree_grants.push(grant.clone());
                        }
                        payload => {
                            if !projection.log.delegation_event_tainted(envelope)
                                && crate::delegation_events::is_delegation_payload(payload)
                            {
                                products.delegations.push((
                                    projection.meta.session_id,
                                    envelope.run_id,
                                    payload.clone(),
                                ));
                            }
                        }
                    }
                }
                if crate::runtime::producers::producer_state_pending(&events) {
                    products.producer_sessions.push(projection.meta.session_id);
                }
                let parent = match projection.meta.origin {
                    SessionOrigin::Delegated {
                        parent_session_id, ..
                    } => parent_session_id,
                    _ => root,
                };
                edges
                    .entry(parent)
                    .or_default()
                    .push(projection.meta.session_id);
                let runs = projection
                    .runs
                    .iter()
                    .filter(|(_, run)| is_terminal_status(run.status))
                    .map(|(run_id, run)| (run_id.to_string(), run.status))
                    .collect::<BTreeMap<_, _>>();
                if !runs.is_empty() {
                    terminal_runs.insert(projection.meta.session_id, runs);
                }
            }
        }
        // Install phase: serialized with creates and appends so a concurrent
        // child cannot be overwritten by the pass's snapshot of it.
        {
            let _mutation = self.lock_mutation();
            {
                let mut residency = self
                    .residency
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                for summary in &products.summaries {
                    let id = summary.meta.session_id;
                    if residency.resident.contains_key(&id) {
                        continue;
                    }
                    residency.evicted.insert(id, summary.clone());
                }
            }
            let mut trees = self
                .trees
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let state = trees.entry(root).or_default();
            for (parent, children) in edges {
                let slot = state.children.entry(parent).or_default();
                for child in children {
                    if !slot.contains(&child) {
                        slot.push(child);
                    }
                }
            }
            for (child, runs) in terminal_runs {
                let slot = state.terminal_runs.entry(child).or_default();
                for (run_id, status) in runs {
                    slot.entry(run_id).or_insert(status);
                }
            }
            state.grants = tree_grants;
            state.producer_sessions = products.producer_sessions.clone();
            state.loaded = true;
        }
        self.persist_subagent_index(root);
        // Observer runs with no store lock held: it calls back into the store.
        self.notify_tree_loaded(products)
    }

    /// Grants installed by tree loads, so a rebuild stays O(cached data) (§4.3).
    pub(crate) fn loaded_tree_grants(&self) -> Vec<cookie_agent_protocol::TreeApprovalGrant> {
        self.trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|state| state.loaded)
            .flat_map(|state| state.grants.clone())
            .collect()
    }

    /// Sessions whose logs can carry goal-producer state without a hidden child
    /// log read: every root, plus children already known to need reconciliation.
    pub(crate) fn producer_scan_sessions(&self) -> Vec<SessionId> {
        self.refresh_discovered();
        let mut ids = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .known_ids()
            .into_iter()
            .filter(|id| self.is_root_id(*id))
            .collect::<Vec<_>>();
        let loaded = self
            .trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for state in loaded.values() {
            if state.loaded {
                ids.extend(state.producer_sessions.iter().copied());
            }
        }
        ids.sort_by_key(|id| id.to_string());
        ids.dedup();
        ids
    }

    /// Whether a session is filed inside another session's tree.
    fn is_filed_child(&self, id: SessionId) -> bool {
        matches!(
            self.cached_location(id),
            Some(SessionLocation::Child { .. })
        )
    }

    /// Existence check that never opens an event log: the location cache and, for
    /// unknown ids, a directory probe.
    pub(crate) fn session_exists(&self, id: SessionId) -> bool {
        self.resolve_dir(id).is_ok()
    }

    fn notify_tree_loaded(&self, products: TreeLoadProducts) -> Result<(), SessionError> {
        let observer = self
            .tree_observer
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(observer) = observer else {
            return Ok(());
        };
        observer
            .tree_loaded(products)
            .map_err(|error| SessionError::TreeRejected(Box::new(error)))
    }

    /// Installs the engine hook that receives tree load products (§3.3).
    pub(crate) fn set_tree_load_observer(&self, observer: Arc<dyn TreeLoadObserver>) {
        *self
            .tree_observer
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(observer);
    }

    pub(crate) fn read_snapshot(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        if let Some(session) = self.get_resident(id) {
            return Ok(session);
        }
        self.open_snapshot(id, false)
    }

    fn is_root_id(&self, id: SessionId) -> bool {
        if self.flat_layout {
            return matches!(self.cached_origin(id), Some(SessionOrigin::Root) | None);
        }
        !matches!(
            self.cached_location(id),
            Some(SessionLocation::Child { .. })
        )
    }

    fn is_tree_member(&self, root: SessionId, id: SessionId) -> bool {
        if !self.flat_layout {
            return matches!(
                self.cached_location(id),
                Some(SessionLocation::Child { root: parent }) if parent == root
            );
        }
        matches!(
            self.cached_origin(id),
            Some(SessionOrigin::Delegated {
                root_session_id,
                ..
            }) if root_session_id == root
        )
    }

    fn cached_origin(&self, id: SessionId) -> Option<SessionOrigin> {
        let residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = residency.resident.get(&id) {
            return Some(session.meta.origin.clone());
        }
        residency
            .evicted
            .get(&id)
            .map(|summary| summary.meta.origin.clone())
    }

    #[must_use]
    pub fn all_summaries(&self) -> Vec<SessionSummary> {
        self.refresh_discovered();
        let residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut summaries = residency.evicted.clone();
        summaries.extend(residency.resident.iter().map(|(session_id, session)| {
            (
                *session_id,
                SessionSummary {
                    meta: session.meta.clone(),
                    usage: session.usage.clone(),
                    usage_rollup: session.usage_rollup.clone(),
                    agent_usage: session.agent_usage.clone(),
                },
            )
        }));
        summaries.into_values().collect()
    }

    pub fn summary(&self, id: SessionId) -> Result<SessionSummary, SessionError> {
        {
            let residency = self
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(session) = residency.resident.get(&id) {
                return Ok(summary_from_projection(session));
            }
            if let Some(summary) = residency.evicted.get(&id) {
                return Ok(summary.clone());
            }
        }
        self.get(id)
            .map(|session| summary_from_projection(&session))
    }

    /// Layout-aware discovery refresh. Metadata caches only — never an event log.
    fn refresh_discovered(&self) {
        if self.flat_layout {
            self.refresh_discovered_flat();
        } else {
            self.refresh_discovered_roots();
        }
    }

    /// Whether `id` already has a residency entry (resident or evicted).
    fn is_known(&self, id: SessionId) -> bool {
        let residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        residency.resident.contains_key(&id) || residency.evicted.contains_key(&id)
    }

    /// Installs an evicted summary without ever displacing fresher in-memory
    /// state (resident projections or an already-cached summary).
    fn cache_summary(&self, id: SessionId, summary: SessionSummary) {
        let mut residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if residency.resident.contains_key(&id) {
            return;
        }
        residency.evicted.entry(id).or_insert(summary);
    }

    /// v2 root-only discovery (§2.3): root `metadata` caches plus each root's
    /// tiny `subagents/index.json`. Directory placement encodes root-ness, so
    /// no `origin` check (and no child event log) is needed.
    fn refresh_discovered_roots(&self) {
        let roots = self.root_dir_ids();
        for root in roots {
            let dir = self.workdir_dir.join(root.to_string());
            if !self.is_known(root) {
                // Invalid entries stay uncached so later discovery retries them
                // and repeats the diagnostic (today's semantics).
                match read_cache(&meta_path(&dir), &dir.join(EVENTS_FILE)) {
                    Ok(meta) if meta.session_id == root => {
                        self.cache_summary(
                            root,
                            SessionSummary {
                                meta,
                                usage: None,
                                usage_rollup: UsageRollup::default(),
                                agent_usage: BTreeMap::new(),
                            },
                        );
                    }
                    Ok(_) => eprintln!("session {root} metadata ID does not match its directory"),
                    Err(error) => {
                        eprintln!("session {root} metadata skipped: {error}");
                        continue;
                    }
                }
            }
            self.record_location(root, SessionLocation::Root);
            self.seed_tree_from_index(root);
        }
    }

    fn refresh_discovered_flat(&self) {
        let entries = match fs::read_dir(&self.workdir_dir) {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("session discovery failed: {error}");
                return;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            if !entry.path().is_dir() {
                continue;
            }
            let Ok(id) = entry.file_name().to_string_lossy().parse::<SessionId>() else {
                continue;
            };
            let known = {
                let residency = self
                    .residency
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                residency.resident.contains_key(&id) || residency.evicted.contains_key(&id)
            };
            if known {
                continue;
            }
            // Invalid entries stay uncached so later discovery retries them and repeats the
            // diagnostic after callers have had a chance to repair the files.
            match read_cache(&meta_path(&entry.path()), &entry.path().join(EVENTS_FILE)) {
                Ok(meta) if meta.session_id == id => {
                    self.residency
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .evicted
                        .entry(id)
                        .or_insert(SessionSummary {
                            meta,
                            usage: None,
                            usage_rollup: UsageRollup::default(),
                            agent_usage: BTreeMap::new(),
                        });
                }
                Ok(_) => eprintln!("session {id} metadata ID does not match its directory"),
                Err(error) => eprintln!("session {id} metadata skipped: {error}"),
            }
        }
    }

    /// Seeded child ids/terminal-run cache from a root's persisted
    /// `subagents/index.json` (never from a child log).
    fn seed_tree_from_index(&self, root: SessionId) {
        let Some(index) = self.read_subagent_index(root) else {
            return;
        };
        let mut edges = Vec::new();
        let mut terminal_runs = HashMap::new();
        for child in index.children {
            let id = child.summary.meta.session_id;
            if !self.is_known(id) {
                self.cache_summary(id, child.summary.clone());
            }
            self.record_location(id, SessionLocation::Child { root });
            let parent = match child.summary.meta.origin {
                SessionOrigin::Delegated {
                    parent_session_id, ..
                } => parent_session_id,
                _ => root,
            };
            edges.push((parent, id));
            if !child.terminal_runs.is_empty() {
                terminal_runs.insert(id, child.terminal_runs);
            }
        }
        let mut trees = self
            .trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = trees.entry(root).or_default();
        if state.loaded {
            return;
        }
        state.terminal_runs = terminal_runs;
        for (parent, id) in edges {
            let siblings = state.children.entry(parent).or_default();
            if !siblings.contains(&id) {
                siblings.push(id);
            }
        }
    }

    /// Where the descendants of `id` live (one flat level, whatever the depth).
    fn subagents_dir(&self, id: SessionId) -> PathBuf {
        self.session_dir(id).join(SUBAGENTS_DIR)
    }

    fn subagent_index_path(&self, root: SessionId) -> PathBuf {
        self.subagents_dir(root).join(SUBAGENT_INDEX_FILE)
    }

    fn read_subagent_index(&self, root: SessionId) -> Option<SubagentIndex> {
        let bytes = fs::read(self.subagent_index_path(root)).ok()?;
        let index = serde_json::from_slice::<SubagentIndex>(&bytes).ok()?;
        (index.version == SUBAGENT_INDEX_VERSION).then_some(index)
    }

    /// Every descendant filed under `root`, at any depth, without opening a log:
    /// the seeded tree state plus the location cache. All descendants share one
    /// directory level, so depth comes from `origin`, not from nesting.
    fn tree_members(&self, root: SessionId) -> Vec<SessionId> {
        let mut members = self
            .trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&root)
            .map(|state| {
                state
                    .children
                    .values()
                    .flatten()
                    .copied()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let located = {
            let locations = self
                .locations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            locations
                .iter()
                .filter(|(_, location)| {
                    matches!(location, SessionLocation::Child { root: parent } if *parent == root)
                })
                .map(|(id, _)| *id)
                .collect::<Vec<_>>()
        };
        members.extend(located);
        members.sort_by_key(|id| id.to_string());
        members.dedup();
        members
    }

    /// Direct children of `parent` inside tree `root`: recorded edges plus every
    /// member whose origin names `parent`.
    fn direct_children(&self, root: SessionId, parent: SessionId) -> Vec<SessionId> {
        let mut children = self
            .trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&root)
            .and_then(|state| state.children.get(&parent).cloned())
            .unwrap_or_default();
        for id in self.tree_members(root) {
            let filed_under = match self.cached_origin(id) {
                Some(SessionOrigin::Delegated {
                    parent_session_id, ..
                }) => parent_session_id,
                _ => root,
            };
            if filed_under == parent && !children.contains(&id) {
                children.push(id);
            }
        }
        children.sort_by_key(|id| id.to_string());
        children.dedup();
        children
    }

    /// Files a newly created session under its root: tree edge + index refresh.
    fn note_placed_child(&self, location: &SessionLocation, origin: &SessionOrigin, id: SessionId) {
        let SessionLocation::Child { root } = location else {
            return;
        };
        let parent = match origin {
            SessionOrigin::Delegated {
                parent_session_id, ..
            } => *parent_session_id,
            _ => *root,
        };
        self.record_child_edge(*root, parent, id);
        self.persist_subagent_index(*root);
    }

    /// Records the `parent -> child` edge plus the child's placement, and the
    /// root's index cache is refreshed by the caller.
    fn record_child_edge(&self, root: SessionId, parent: SessionId, child: SessionId) {
        let mut trees = self
            .trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let siblings = trees
            .entry(root)
            .or_default()
            .children
            .entry(parent)
            .or_default();
        if !siblings.contains(&child) {
            siblings.push(child);
        }
    }

    /// Descendant session ids filed under `parent`, from the directory listing
    /// only (non-uuid entries such as `index.json` are skipped).
    fn child_dir_ids(&self, parent: SessionId) -> Vec<SessionId> {
        let Ok(entries) = fs::read_dir(self.subagents_dir(parent)) else {
            return Vec::new();
        };
        let mut ids = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .parse::<SessionId>()
                    .ok()
            })
            .collect::<Vec<_>>();
        ids.sort_by_key(|id| id.to_string());
        ids
    }

    /// Residency-only summary lookup (never opens a log).
    fn cached_summary(&self, id: SessionId) -> Option<SessionSummary> {
        let residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = residency.resident.get(&id) {
            return Some(summary_from_projection(session));
        }
        residency.evicted.get(&id).cloned()
    }

    /// Rebuilds and atomically rewrites a root's child-summary cache. Whole-file
    /// rewrite on purpose (§3.4); failures are reported, never fatal, because
    /// the cache is rebuilt from logs on the next tree load.
    fn persist_subagent_index(&self, root: SessionId) {
        if self.flat_layout {
            return;
        }
        let cached = self
            .trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&root)
            .map(|state| state.terminal_runs.clone())
            .unwrap_or_default();
        let children = self
            .tree_members(root)
            .into_iter()
            .filter_map(|id| {
                let summary = self.cached_summary(id)?;
                Some(IndexedChild {
                    summary,
                    terminal_runs: cached.get(&id).cloned().unwrap_or_default(),
                })
            })
            .collect::<Vec<_>>();
        let path = self.subagent_index_path(root);
        if children.is_empty() && !path.is_file() {
            return;
        }
        let index = SubagentIndex {
            version: SUBAGENT_INDEX_VERSION,
            children,
        };
        if let Err(error) = write_index_json(&path, &index) {
            eprintln!("subagent index refresh skipped for {root}: {error}");
        }
    }

    /// Remembers a terminal run status for the delegation registry cache, then
    /// refreshes the owning root's index.
    fn record_terminal_run(&self, id: SessionId, run_id: RunId, status: SessionStatus) {
        if self.flat_layout {
            return;
        }
        let Ok(root) = self.root_of(id) else {
            return;
        };
        if root == id {
            return;
        }
        let parent = match self.cached_origin(id) {
            Some(SessionOrigin::Delegated {
                parent_session_id, ..
            }) => parent_session_id,
            _ => root,
        };
        let refreshed = {
            let mut trees = self
                .trees
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let state = trees.entry(root).or_default();
            let siblings = state.children.entry(parent).or_default();
            if !siblings.contains(&id) {
                siblings.push(id);
            }
            state
                .terminal_runs
                .entry(id)
                .or_default()
                .insert(run_id.to_string(), status)
                .is_none()
        };
        if refreshed {
            self.persist_subagent_index(root);
        }
    }

    /// Terminal run statuses for a child, served from the index/tree cache
    /// without paging the child log (§4.1.3).
    #[allow(dead_code)] // consumed by the delegation registry rebuild (P2)
    pub(crate) fn terminal_run_status(
        &self,
        id: SessionId,
        run_id: RunId,
    ) -> Option<SessionStatus> {
        let root = self.root_of(id).ok()?;
        let trees = self
            .trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        trees
            .get(&root)?
            .terminal_runs
            .get(&id)?
            .get(&run_id.to_string())
            .copied()
    }

    pub fn session_usage(
        &self,
        id: SessionId,
        pricing: &cookie_agent_config::PricingConfig,
        catalog: &BTreeMap<
            cookie_agent_protocol::ModelKey,
            cookie_agent_models::catalog::CatalogModelCost,
        >,
    ) -> Result<cookie_agent_protocol::SessionUsageResult, SessionError> {
        let usage = self.summary(id)?.usage_rollup;
        Ok(cookie_agent_protocol::SessionUsageResult {
            session_id: id,
            usage: crate::usage::with_pricing(usage, pricing, catalog),
        })
    }

    #[must_use]
    pub fn is_resident(&self, id: SessionId) -> bool {
        self.residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .contains_key(&id)
    }

    #[must_use]
    pub fn resident_subagent_count(&self) -> usize {
        self.residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .values()
            .filter(|session| matches!(session.meta.origin, SessionOrigin::Delegated { .. }))
            .count()
    }
    /// Directory holding project-level files (artifacts, the grant journal,
    /// runtime revisions, `cwd` and `layout.json`).
    #[must_use]
    pub fn workdir_dir_path(&self) -> &Path {
        &self.project_dir
    }

    #[must_use]
    pub fn project_dir_path(&self) -> &Path {
        &self.project_dir
    }
    #[must_use]
    pub(crate) fn sessions_dir_path(&self) -> &Path {
        &self.workdir_dir
    }
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    #[must_use]
    pub fn session_dir(&self, id: SessionId) -> PathBuf {
        self.cached_location(id)
            .map(|location| self.path_for(location, id))
            .unwrap_or_else(|| self.workdir_dir.join(id.to_string()))
    }

    /// Metadata cache path to *read* (handles the v1/v2 name fallback).
    pub(crate) fn meta_cache_path(&self, id: SessionId) -> Result<PathBuf, SessionError> {
        Ok(meta_path(&self.resolve_dir(id)?))
    }

    /// Metadata cache file name this layout writes.
    #[must_use]
    fn meta_write_name(&self) -> &'static str {
        if self.flat_layout {
            LEGACY_SESSION_META_FILE
        } else {
            SESSION_META_FILE
        }
    }

    pub fn is_persisted(&self, id: SessionId) -> Result<bool, SessionError> {
        Ok(self.get(id)?.log.is_persisted())
    }

    /// Every direct child of `parent`, resolved from *placement* rather than by
    /// scanning every resident projection: seeded tree state + location cache,
    /// then a `subagents/` directory scan that adopts children no cache
    /// mentions from their `metadata` cache alone (§3.4). No child event log is
    /// read here — children of a cold tree report their persisted status.
    pub fn children(&self, parent: SessionId) -> Vec<ChildSummary> {
        // Listing a tree is a use of it: make sure it is complete first (§3.2.2).
        let _ = self.ensure_tree_for(parent);
        self.child_ids(parent)
            .into_iter()
            .filter_map(|id| self.child_summary(id))
            .collect()
    }

    /// Direct child ids of `parent`, resolved from placement: seeded tree state
    /// and location cache first, then a scan of the tree directory that adopts
    /// filed children no cache mentions from their `metadata` alone.
    fn child_ids(&self, parent: SessionId) -> Vec<SessionId> {
        self.refresh_discovered();
        let root = match self.cached_location(parent) {
            Some(SessionLocation::Child { root }) => root,
            _ => parent,
        };
        let mut children = self.direct_children(root, parent);
        if self.flat_layout {
            // The flat layout has no placement to read: `origin` is the only
            // parent pointer, so fall back to resident projections (as before).
            for session in self.all() {
                if let SessionOrigin::Delegated {
                    parent_session_id, ..
                } = session.meta.origin
                    && parent_session_id == parent
                    && !children.contains(&session.meta.session_id)
                {
                    children.push(session.meta.session_id);
                }
            }
            return children;
        }
        for id in self.child_dir_ids(root) {
            if children.contains(&id) {
                continue;
            }
            if self.adopt_filed_child(root, id) == Some(parent) {
                children.push(id);
            }
        }
        children.sort_by_key(|id| id.to_string());
        children
    }

    /// Adopts a child that is present on disk but missing from the caches (e.g.
    /// a crash between the directory publish and the `index.json` refresh), and
    /// reports the parent its origin names.
    fn adopt_filed_child(&self, root: SessionId, id: SessionId) -> Option<SessionId> {
        let dir = self.path_for(SessionLocation::Child { root }, id);
        let Ok(meta) = read_cache(&meta_path(&dir), &dir.join(EVENTS_FILE)) else {
            return None;
        };
        if meta.session_id != id {
            eprintln!("session {id} metadata ID does not match its directory");
            return None;
        }
        let parent = match meta.origin {
            SessionOrigin::Delegated {
                parent_session_id, ..
            } => parent_session_id,
            _ => root,
        };
        self.record_location(id, SessionLocation::Child { root });
        self.cache_summary(
            id,
            SessionSummary {
                meta,
                usage: None,
                usage_rollup: UsageRollup::default(),
                agent_usage: BTreeMap::new(),
            },
        );
        self.record_child_edge(root, parent, id);
        self.persist_subagent_index(root);
        Some(parent)
    }

    /// Metadata for a tree member without rebuilding its log.
    fn summary_meta(&self, id: SessionId) -> Result<SessionMeta, SessionError> {
        if let Some(session) = self.get_resident(id) {
            return Ok(session.meta);
        }
        if let Some(summary) = self.cached_summary(id) {
            return Ok(summary.meta);
        }
        if !self.flat_layout {
            // v2 tree assembly must never page a child log in implicitly.
            return Err(SessionError::Missing(id));
        }
        Ok(self.get(id)?.meta)
    }

    /// Listing view of one child, preferring the live projection for resident
    /// sessions and the summary cache otherwise.
    fn child_summary(&self, id: SessionId) -> Option<ChildSummary> {
        if let Some(session) = self.get_resident(id) {
            return Some(ChildSummary {
                session_id: session.meta.session_id,
                agent: session.meta.creation_selection.agent.clone(),
                title: session.meta.title.clone(),
                title_updated_seq: session.meta.title_updated_seq,
                status: session.status,
                usage: session.usage,
            });
        }
        let summary = self.cached_summary(id)?;
        Some(ChildSummary {
            session_id: summary.meta.session_id,
            agent: summary.meta.creation_selection.agent.clone(),
            title: summary.meta.title.clone(),
            title_updated_seq: summary.meta.title_updated_seq,
            status: summary.meta.status,
            usage: summary.usage,
        })
    }

    pub fn tree(&self, id: SessionId) -> Result<SessionTree, SessionError> {
        self.ensure_tree_for(id)?;
        // Assembly walks placement edges, so it costs one metadata cache read per
        // tree member instead of a rebuild of every resident session's log.
        let root = self.get(id)?;
        let mut metadata = HashMap::new();
        let mut children = HashMap::<SessionId, Vec<SessionId>>::new();
        metadata.insert(id, root.metadata());
        let mut queue = self.child_ids(id);
        if !queue.is_empty() {
            children.insert(id, queue.clone());
        }
        while let Some(current) = queue.pop() {
            if metadata.contains_key(&current) {
                continue;
            }
            metadata.insert(current, self.summary_meta(current)?);
            let descendants = self.child_ids(current);
            if !descendants.is_empty() {
                children.insert(current, descendants.clone());
                queue.extend(descendants);
            }
        }

        fn build_tree(
            id: SessionId,
            metadata: &HashMap<SessionId, SessionMeta>,
            children: &HashMap<SessionId, Vec<SessionId>>,
        ) -> Result<SessionTree, SessionError> {
            Ok(SessionTree {
                session: metadata
                    .get(&id)
                    .cloned()
                    .ok_or(SessionError::Missing(id))?,
                children: children
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .map(|child| build_tree(*child, metadata, children))
                    .collect::<Result<Vec<_>, _>>()?,
            })
        }

        build_tree(id, &metadata, &children)
    }

    pub(crate) fn release_ownership(&self) {
        self.closed.store(true, Ordering::Release);
        let _mutation = self.lock_mutation();
        let residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for session in residency.resident.values() {
            let _ = session.log.suspend_writer();
        }
        self.ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    fn ensure_open(&self) -> Result<(), SessionError> {
        if self.closed.load(Ordering::Acquire) {
            Err(SessionError::StoreClosed)
        } else {
            Ok(())
        }
    }
}

impl Drop for SessionStore {
    fn drop(&mut self) {
        self.release_ownership();
    }
}

/// Whether the projection fold consumes this payload. Fold-ignored payloads
/// (the streaming hot path: TextDelta/ReasoningDelta per token,
/// ToolCallProgress per output chunk) only advance the metadata tip and are
/// applied incrementally by `append_with_mode`; consumed payloads trigger a
/// full rebuild.
///
/// Direction is deliberate: consumed variants are an explicit whitelist so a
/// future `EventPayload` variant defaults to rebuild (safe-slow), never to
/// incremental (wrong). Keep this in sync with the fold body below.
fn fold_consumed(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::SessionCreated { .. }
            | EventPayload::SessionReverted { .. }
            | EventPayload::SessionPermissionOverlaySet { .. }
            | EventPayload::SessionTitleCommitted { .. }
            | EventPayload::DelegateChildTerminated { .. }
            | EventPayload::RunStarted { .. }
            | EventPayload::UserInputSubmitted { .. }
            | EventPayload::RunCompleted { .. }
            | EventPayload::RunFailed { .. }
            | EventPayload::RunCancelled { .. }
            | EventPayload::RunInterrupted { .. }
            | EventPayload::ToolCallStarted { .. }
            | EventPayload::ToolCallTerminated { .. }
            | EventPayload::ModelTurnCommitted { .. }
            | EventPayload::ModelUsageRecorded { .. }
            | EventPayload::InternalAgentUsageRecorded { .. }
    )
}

#[cfg(test)]
thread_local! {
    static PROJECTION_FOLDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn projection_fold_count() -> u64 {
    PROJECTION_FOLDS.with(std::cell::Cell::get)
}

/// Asserts that two projections of the same log are field-identical. The log
/// itself is compared by identity/tip rather than by folding the events.
#[cfg(test)]
fn assert_projection_equivalent(actual: &SessionProjection, expected: &SessionProjection) {
    assert_eq!(actual.meta, expected.meta, "meta");
    assert_eq!(
        actual.creation_agent, expected.creation_agent,
        "creation_agent"
    );
    assert_eq!(actual.status, expected.status, "status");
    assert_eq!(actual.usage, expected.usage, "usage");
    assert_eq!(actual.usage_rollup, expected.usage_rollup, "usage_rollup");
    assert_eq!(actual.agent_usage, expected.agent_usage, "agent_usage");
    assert_eq!(actual.runs, expected.runs, "runs");
    assert_eq!(
        actual.rename_records, expected.rename_records,
        "rename_records"
    );
    assert_eq!(
        actual.permission_overlay, expected.permission_overlay,
        "permission_overlay"
    );
    let logs_match = Arc::ptr_eq(&actual.log, &expected.log)
        || (actual.log.physical_tip_seq() == expected.log.physical_tip_seq()
            && actual.log.event_snapshot().len() == expected.log.event_snapshot().len());
    assert!(logs_match, "log tip/length");
}

pub(crate) fn projection(log: Arc<EventLog>) -> Result<SessionProjection, SessionError> {
    #[cfg(test)]
    PROJECTION_FOLDS.with(|count| count.set(count.get() + 1));
    projection_fold(log)
}

fn projection_fold(log: Arc<EventLog>) -> Result<SessionProjection, SessionError> {
    let events = log.event_snapshot();
    let physical_tip = log.last_event().expect("creation checked by EventLog");
    let (
        origin,
        cwd_identity,
        creation_selection,
        creation_agent,
        runtime_revision,
        catalog_revision,
        provider_state_revision,
        model_revision,
        agent_revision,
        recipe_registry_revision,
        manifest_revision,
    ) = match &events
        .first()
        .expect("creation checked by EventLog")
        .payload
    {
        EventPayload::SessionCreated {
            origin,
            cwd_identity,
            creation_selection,
            creation_agent,
            runtime_revision,
            catalog_revision,
            provider_state_revision,
            model_revision,
            agent_revision,
            recipe_registry_revision,
            manifest_revision,
        } => (
            origin.clone(),
            cwd_identity.clone(),
            creation_selection.clone(),
            creation_agent.as_ref().clone(),
            runtime_revision.clone(),
            catalog_revision.clone(),
            provider_state_revision.clone(),
            model_revision.clone(),
            agent_revision.clone(),
            recipe_registry_revision.clone(),
            manifest_revision.clone(),
        ),
        _ => unreachable!(),
    };
    let mut meta = SessionMeta {
        session_id: events[0].session_id,
        origin,
        cwd_identity,
        creation_selection,
        runtime_revision,
        catalog_revision,
        provider_state_revision,
        model_revision,
        agent_revision,
        recipe_registry_revision,
        manifest_revision,
        title: None,
        title_updated_seq: 0,
        last_event_seq: log.physical_tip_seq(),
        last_activity: physical_tip.timestamp,
        status: SessionStatus::Idle,
        skipped_events: log
            .diagnostics()
            .iter()
            .filter(|diagnostic| diagnostic.skipped)
            .map(|diagnostic| cookie_agent_protocol::SkippedEvent {
                seq: diagnostic.seq,
                reason: diagnostic.reason.clone(),
            })
            .collect(),
    };
    let mut runs = HashMap::<RunId, RunProjection>::new();
    let mut status = SessionStatus::Idle;
    let mut usage = None;
    let mut usage_rollup = UsageRollup::default();
    let mut agent_usage = BTreeMap::<AgentId, UsageRollup>::new();
    let mut rename_records = HashMap::new();
    let mut permission_overlay = SessionPermissionOverlay::default();
    let mut automatic_title = None;
    let mut delegated_title = None;
    let mut user_title: Option<Option<cookie_agent_protocol::SessionTitle>> = None;
    let recorded_usage_turns = events
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::ModelUsageRecorded { model_turn_seq, .. } => Some(model_turn_seq),
            _ => None,
        })
        .collect::<HashSet<_>>();
    for envelope in events.iter() {
        if let EventPayload::SessionPermissionOverlaySet { overlay } = &envelope.payload {
            permission_overlay = overlay.clone();
        }
        if let EventPayload::SessionTitleCommitted { change, .. } = &envelope.payload {
            match change {
                SessionTitleChange::UserSet { title, .. } => {
                    user_title = Some(Some(title.clone()));
                }
                SessionTitleChange::UserClear { .. } => user_title = Some(None),
                SessionTitleChange::UserReset { .. } => user_title = None,
                SessionTitleChange::DelegatedSet { title, .. } => {
                    delegated_title = Some(title.clone());
                }
                SessionTitleChange::InternalAgentSet { title, .. }
                | SessionTitleChange::FallbackSet { title } => {
                    automatic_title = Some(title.clone());
                }
            }
            meta.title = user_title
                .clone()
                .unwrap_or_else(|| delegated_title.clone().or_else(|| automatic_title.clone()));
            meta.title_updated_seq = envelope.seq;
            if let Some(record) = change.user_rename_record() {
                rename_records.insert(record.client_rename_id.clone(), record);
            }
        }
        if let EventPayload::DelegateChildTerminated {
            status: terminal, ..
        } = &envelope.payload
        {
            status = *terminal;
            continue;
        }
        if matches!(envelope.payload, EventPayload::SessionReverted { .. }) {
            status = SessionStatus::Idle;
            for run in runs.values_mut() {
                if run.status == SessionStatus::Running {
                    run.status = SessionStatus::Interrupted;
                    run.pending_calls.clear();
                }
            }
            continue;
        }
        let Some(run_id) = envelope.run_id else {
            continue;
        };
        match &envelope.payload {
            EventPayload::RunStarted {
                client_run_id,
                selection,
                agent,
                ..
            } => {
                status = SessionStatus::Running;
                runs.insert(
                    run_id,
                    RunProjection {
                        id: run_id,
                        client_run_id: client_run_id.clone(),
                        input: String::new(),
                        selection: selection.clone(),
                        agent: agent.as_ref().clone(),
                        status: SessionStatus::Running,
                        final_text: None,
                        pending_calls: HashMap::new(),
                    },
                );
            }
            // User input is prompt history, not a lifecycle transition.
            EventPayload::UserInputSubmitted { input } => {
                if let Some(run) = runs.get_mut(&run_id)
                    && run.input.is_empty()
                {
                    run.input = input.clone();
                }
            }
            EventPayload::UserInputApplied { .. } => {}
            EventPayload::RunCompleted { final_text } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Completed;
                    run.final_text = final_text.clone();
                    status = SessionStatus::Completed;
                }
            }
            EventPayload::RunFailed { .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Failed;
                    status = SessionStatus::Failed;
                }
            }
            EventPayload::RunCancelled { .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Cancelled;
                    status = SessionStatus::Cancelled;
                }
            }
            EventPayload::RunInterrupted { .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Interrupted;
                    status = SessionStatus::Interrupted;
                }
            }
            EventPayload::ToolCallStarted { start } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    let tool = turns_tool_name(&events, &start.owner).unwrap_or_default();
                    run.pending_calls.insert(start.tool_call_id, tool);
                }
            }
            EventPayload::ToolCallTerminated { termination } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.pending_calls.remove(&termination.tool_call_id);
                }
            }
            EventPayload::ModelTurnCommitted {
                model_turn_seq,
                resolved_model,
                turn,
                ..
            } => {
                let reported = &turn.usage;
                let total = usage.get_or_insert_with(Usage::default);
                add_usage(&mut total.input_tokens, reported.input_tokens);
                add_usage(
                    &mut total.input_tokens_no_cache,
                    reported.input_tokens_no_cache,
                );
                add_usage(
                    &mut total.input_tokens_cache_read,
                    reported.input_tokens_cache_read,
                );
                add_usage(
                    &mut total.input_tokens_cache_write,
                    reported.input_tokens_cache_write,
                );
                add_usage(&mut total.output_tokens, reported.output_tokens);
                add_usage(&mut total.output_tokens_text, reported.output_tokens_text);
                add_usage(
                    &mut total.output_tokens_reasoning,
                    reported.output_tokens_reasoning,
                );
                if !recorded_usage_turns.contains(model_turn_seq) {
                    crate::usage::record_stamped(&mut usage_rollup, resolved_model, reported, None);
                    if let Some(agent) = runs.get(&run_id).map(|run| run.agent.agent.clone()) {
                        crate::usage::record_stamped(
                            agent_usage.entry(agent).or_default(),
                            resolved_model,
                            reported,
                            None,
                        );
                    }
                }
            }
            EventPayload::ModelUsageRecorded {
                agent_id,
                resolved_model,
                usage: reported,
                estimated_cost_pico_usd,
                ..
            } => {
                crate::usage::record_stamped(
                    &mut usage_rollup,
                    resolved_model,
                    reported,
                    *estimated_cost_pico_usd,
                );
                crate::usage::record_stamped(
                    agent_usage.entry(agent_id.clone()).or_default(),
                    resolved_model,
                    reported,
                    *estimated_cost_pico_usd,
                );
            }
            EventPayload::InternalAgentUsageRecorded {
                agent_id,
                resolved_model,
                usage: reported,
                estimated_cost_pico_usd,
                ..
            } => {
                crate::usage::record_stamped(
                    &mut usage_rollup,
                    resolved_model,
                    reported,
                    *estimated_cost_pico_usd,
                );
                crate::usage::record_stamped(
                    agent_usage.entry(agent_id.clone()).or_default(),
                    resolved_model,
                    reported,
                    *estimated_cost_pico_usd,
                );
            }
            _ => {}
        }
    }
    meta.status = status;
    Ok(SessionProjection {
        meta,
        creation_agent,
        status,
        usage,
        usage_rollup,
        agent_usage,
        runs,
        rename_records,
        permission_overlay,
        log,
    })
}

/// Moves a fully prepared session directory into its published location.
///
/// A root's directory can already exist as a placement scaffold: a child created
/// while the root was still buffered is filed under `<root>/subagents/`, which
/// brings `<root>` into being before the root itself publishes. Such a scaffold
/// holds nothing but that directory, so the prepared files are merged into it;
/// anything else present means the location is genuinely taken.
fn publish_prepared_dir(
    temporary: &Path,
    final_dir: &Path,
    session_id: SessionId,
) -> Result<(), SessionError> {
    if !final_dir.exists() {
        return fs::rename(temporary, final_dir).map_err(|source| SessionError::Io {
            path: final_dir.to_owned(),
            source,
        });
    }
    let scaffold = fs::read_dir(final_dir)
        .map_err(|source| SessionError::Io {
            path: final_dir.to_owned(),
            source,
        })?
        .filter_map(|entry| entry.ok())
        .all(|entry| entry.file_name() == SUBAGENTS_DIR && entry.path().is_dir());
    if !scaffold {
        return Err(SessionError::SessionLocked(session_id));
    }
    let entries = fs::read_dir(temporary)
        .map_err(|source| SessionError::Io {
            path: temporary.to_owned(),
            source,
        })?
        .filter_map(|entry| entry.ok())
        .collect::<Vec<_>>();
    for entry in entries {
        fs::rename(entry.path(), final_dir.join(entry.file_name())).map_err(|source| {
            SessionError::Io {
                path: final_dir.join(entry.file_name()),
                source,
            }
        })?;
    }
    fs::remove_dir(temporary).map_err(|source| SessionError::Io {
        path: temporary.to_owned(),
        source,
    })
}

/// Only restart-stable grants are folded into the approval store (§4.3): a grant
/// whose binding cannot survive a restart must not outlive the process that
/// earned it.
pub(crate) fn restart_stable_grant(grant: &cookie_agent_protocol::TreeApprovalGrant) -> bool {
    !grant.resources.is_empty()
        && grant.resources.iter().all(|resource| {
            resource.binding_lifetime
                == cookie_agent_protocol::PreparedBindingLifetime::RestartStable
        })
}

/// Whether a run status ends a run.
fn is_terminal_status(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Completed
            | SessionStatus::Failed
            | SessionStatus::Interrupted
            | SessionStatus::Cancelled
    )
}

thread_local! {
    /// Nesting depth of this thread's `SessionStore::lock_mutation` guards.
    static MUTATION_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// RAII holder for the store's durable-mutation lock. `None` means this thread
/// already holds it further up the stack, so the guard is a no-op.
struct MutationGuard<'a> {
    locked: Option<std::sync::MutexGuard<'a, ()>>,
}

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        if self.locked.is_some() {
            MUTATION_DEPTH.with(|depth| depth.set(depth.get() - 1));
        }
    }
}

thread_local! {
    /// Depth of the enclosing `load_tree` read phase on this thread.
    static TREE_LOAD_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Marks a scope in which reading child event logs is legal (§3.3).
struct TreeLoadReads;

impl TreeLoadReads {
    fn begin() -> Self {
        TREE_LOAD_READS.with(|depth| depth.set(depth.get() + 1));
        Self
    }

    fn active() -> bool {
        TREE_LOAD_READS.with(|depth| depth.get() > 0)
    }
}

impl Drop for TreeLoadReads {
    fn drop(&mut self) {
        TREE_LOAD_READS.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

fn terminal_run_of(run: Option<RunId>, payload: &EventPayload) -> Option<(RunId, SessionStatus)> {
    let status = match payload {
        EventPayload::RunCompleted { .. } => SessionStatus::Completed,
        EventPayload::RunFailed { .. } => SessionStatus::Failed,
        EventPayload::RunCancelled { .. } => SessionStatus::Cancelled,
        EventPayload::RunInterrupted { .. } => SessionStatus::Interrupted,
        _ => return None,
    };
    Some((run?, status))
}

fn summary_from_projection(session: &SessionProjection) -> SessionSummary {
    SessionSummary {
        meta: session.meta.clone(),
        usage: session.usage.clone(),
        usage_rollup: session.usage_rollup.clone(),
        agent_usage: session.agent_usage.clone(),
    }
}

fn fork_title(title: Option<&SessionTitle>) -> Result<SessionTitle, SessionError> {
    const SUFFIX: &str = " (fork)";
    let base = title.map_or("Untitled", SessionTitle::as_str);
    let max_base = SessionTitle::MAX_BYTES.saturating_sub(SUFFIX.len());
    let mut boundary = base.len().min(max_base);
    while !base.is_char_boundary(boundary) {
        boundary -= 1;
    }
    SessionTitle::new(format!("{}{SUFFIX}", &base[..boundary]))
        .map_err(|error| SessionError::InvalidForkTitle(error.to_string()))
}

fn add_usage(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or_default().saturating_add(value));
    }
}

fn turns_tool_name(
    events: &[cookie_agent_protocol::StoredEvent],
    owner: &cookie_agent_protocol::AssistantToolCallRef,
) -> Option<String> {
    events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::ModelTurnCommitted {
            model_turn_seq,
            turn,
            ..
        } if *model_turn_seq == owner.model_turn_seq => {
            match turn.content.get(owner.content_index as usize) {
                Some(cookie_agent_protocol::PersistedAssistantPart::ToolCall { name, .. }) => {
                    Some(name.as_str().to_owned())
                }
                _ => None,
            }
        }
        _ => None,
    })
}

/// Lowercased, `[a-z0-9._-]`-only, repeat-collapsed, 32-char-truncated basename
/// of the canonical cwd (§1.1). Empty results fall back to a hash-only key.
fn workdir_key_suffix(cwd: &Path) -> String {
    fn trim(value: &str) -> String {
        value
            .trim_matches(|character| character == '-' || character == '_')
            .to_owned()
    }

    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned());
    let base = canonical
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut sanitized = String::with_capacity(base.len());
    for character in base.chars() {
        let mapped = if character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '_' | '-')
        {
            character
        } else {
            '-'
        };
        if mapped == '-' && sanitized.ends_with('-') {
            continue;
        }
        sanitized.push(mapped);
    }
    trim(&trim(&sanitized).chars().take(32).collect::<String>())
}

fn write_layout_marker_if_absent(workdir_dir: &Path) -> Result<(), SessionError> {
    let path = workdir_dir.join(LAYOUT_MARKER_FILE);
    if path.exists() {
        return Ok(());
    }
    let marker = serde_json::json!({ "version": LAYOUT_VERSION });
    let temporary = workdir_dir.join(format!(".{LAYOUT_MARKER_FILE}.{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<(), SessionError> {
        #[cfg(unix)]
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|source| SessionError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            serde_json::to_writer_pretty(&mut file, &marker).map_err(|source| {
                SessionError::Json {
                    path: temporary.clone(),
                    source,
                }
            })?;
            file.sync_all().map_err(|source| SessionError::Io {
                path: temporary.clone(),
                source,
            })?;
            drop(file);
            fs::rename(&temporary, &path).map_err(|source| SessionError::Io {
                path: path.clone(),
                source,
            })?;
            fsync_directory(workdir_dir)?;
        }
        #[cfg(windows)]
        {
            let mut file =
                cookie_agent_models::secure_store::create_windows_private_file(&temporary)
                    .map_err(|source| SessionError::Io {
                        path: temporary.clone(),
                        source,
                    })?;
            serde_json::to_writer_pretty(&mut file, &marker).map_err(|source| {
                SessionError::Json {
                    path: temporary.clone(),
                    source,
                }
            })?;
            file.sync_all().map_err(|source| SessionError::Io {
                path: temporary.clone(),
                source,
            })?;
            drop(file);
            replace_windows_path_with_retry(&temporary, &path).map_err(|source| {
                SessionError::Io {
                    path: path.clone(),
                    source,
                }
            })?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Atomically rewrites a small JSON cache (temp file + rename + parent fsync),
/// mirroring [`write_cache`]'s durability discipline.
fn write_index_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), SessionError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| SessionError::Json {
        path: path.to_owned(),
        source,
    })?;
    let parent = path.parent().ok_or_else(|| SessionError::Io {
        path: path.to_owned(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session cache has no parent",
        ),
    })?;
    #[cfg(unix)]
    create_unix_session_directory_all(parent)?;
    #[cfg(windows)]
    create_windows_session_directory(parent)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "index".to_owned());
    let temporary = parent.join(format!(".{name}.{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<(), SessionError> {
        #[cfg(unix)]
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|source| SessionError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|source| SessionError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            drop(file);
            fs::rename(&temporary, path).map_err(|source| SessionError::Io {
                path: path.to_owned(),
                source,
            })?;
            fsync_directory(parent)?;
        }
        #[cfg(windows)]
        {
            let mut file =
                cookie_agent_models::secure_store::create_windows_private_file(&temporary)
                    .map_err(|source| SessionError::Io {
                        path: temporary.clone(),
                        source,
                    })?;
            file.write_all(&bytes).map_err(|source| SessionError::Io {
                path: temporary.clone(),
                source,
            })?;
            file.sync_all().map_err(|source| SessionError::Io {
                path: temporary.clone(),
                source,
            })?;
            drop(file);
            replace_windows_path_with_retry(&temporary, path).map_err(|source| {
                SessionError::Io {
                    path: path.to_owned(),
                    source,
                }
            })?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Session metadata cache path, preferring the v2 name and falling back to the
/// pre-v2 `meta.json` for one release.
pub(crate) fn meta_path(session_dir: &Path) -> PathBuf {
    let current = session_dir.join(SESSION_META_FILE);
    if current.exists() {
        return current;
    }
    let legacy = session_dir.join(LEGACY_SESSION_META_FILE);
    if legacy.exists() {
        return legacy;
    }
    current
}

fn write_cache(path: &Path, cache: &SessionMeta) -> Result<(), SessionError> {
    let persisted = serde_json::to_value(cache).map_err(|source| SessionError::Json {
        path: path.to_owned(),
        source,
    })?;
    let bytes = serde_json::to_vec_pretty(&persisted).map_err(|source| SessionError::Json {
        path: path.to_owned(),
        source,
    })?;
    let parent = path.parent().expect("session cache has a parent");
    let temporary = parent.join(format!(".meta.json.{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<(), SessionError> {
        #[cfg(unix)]
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|source| SessionError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|source| SessionError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            drop(file);
            fs::rename(&temporary, path).map_err(|source| SessionError::Io {
                path: path.to_owned(),
                source,
            })?;
            fsync_directory(parent)?;
        }
        #[cfg(windows)]
        {
            let mut file =
                cookie_agent_models::secure_store::create_windows_private_file(&temporary)
                    .map_err(|source| SessionError::Io {
                        path: temporary.clone(),
                        source,
                    })?;
            file.write_all(&bytes).map_err(|source| SessionError::Io {
                path: temporary.clone(),
                source,
            })?;
            file.sync_all().map_err(|source| SessionError::Io {
                path: temporary.clone(),
                source,
            })?;
            drop(file);
            replace_windows_path_with_retry(&temporary, path).map_err(|source| {
                SessionError::Io {
                    path: path.to_owned(),
                    source,
                }
            })?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn replace_windows_path_with_retry(source: &Path, target: &Path) -> std::io::Result<()> {
    const ATTEMPTS: usize = 50;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(25);

    for attempt in 0..ATTEMPTS {
        match cookie_agent_models::secure_store::replace_windows_path(source, target) {
            Ok(()) => return Ok(()),
            Err(error) if attempt + 1 < ATTEMPTS && windows_replace_is_contended(&error) => {
                std::thread::sleep(BACKOFF);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("path replacement attempts are nonzero")
}

#[cfg(windows)]
fn windows_replace_is_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(error.raw_os_error(), Some(5 | 32))
}

fn read_cache(path: &Path, events_path: &Path) -> Result<SessionMeta, SessionError> {
    let bytes = fs::read(path).map_err(|source| SessionError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut value = serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|source| {
        SessionError::Json {
            path: path.to_owned(),
            source,
        }
    })?;
    if value.get("last_activity").is_none() {
        let modified = fs::metadata(events_path)
            .and_then(|metadata| metadata.modified())
            .map_err(|source| SessionError::Io {
                path: events_path.to_owned(),
                source,
            })?;
        let timestamp =
            jiff::Timestamp::try_from(modified).unwrap_or_else(|_| jiff::Timestamp::now());
        value
            .as_object_mut()
            .ok_or_else(|| SessionError::Json {
                path: path.to_owned(),
                source: serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "session metadata is not an object",
                )),
            })?
            .insert(
                "last_activity".into(),
                serde_json::to_value(timestamp).expect("timestamp serializes"),
            );
    }
    serde_json::from_value(value).map_err(|source| SessionError::Json {
        path: path.to_owned(),
        source,
    })
}

#[cfg(unix)]
fn write_project_cwd(project_dir: &Path, cwd: &Path) -> Result<(), SessionError> {
    let Ok(canonical) = cwd.canonicalize() else {
        return Ok(());
    };
    let bytes = canonical.as_os_str().as_bytes();
    let path = project_dir.join(PROJECT_CWD_FILE);
    if project_cwd_is_current(&path, bytes) {
        return Ok(());
    }

    let temporary = project_dir.join(format!(".{PROJECT_CWD_FILE}.{}.tmp", Uuid::now_v7()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &path)?;
        fs::File::open(project_dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|source| SessionError::Io { path, source })
}

#[cfg(windows)]
fn write_project_cwd(project_dir: &Path, cwd: &Path) -> Result<(), SessionError> {
    let Ok(canonical) = cwd.canonicalize() else {
        return Ok(());
    };
    let bytes = canonical.as_os_str().as_encoded_bytes();
    let path = project_dir.join(PROJECT_CWD_FILE);
    if project_cwd_is_current(&path, bytes) {
        return Ok(());
    }
    let temporary = project_dir.join(format!(".{PROJECT_CWD_FILE}.{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<(), SessionError> {
        let mut file = cookie_agent_models::secure_store::create_windows_private_file(&temporary)
            .map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        file.write_all(bytes).map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        drop(file);
        replace_windows_path_with_retry(&temporary, &path).map_err(|source| SessionError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn project_cwd_is_current(path: &Path, expected: &[u8]) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes == expected)
}

#[cfg(windows)]
fn project_cwd_is_current(path: &Path, expected: &[u8]) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes == expected)
}

#[cfg(unix)]
pub(crate) fn create_unix_session_directory_all(path: &Path) -> Result<(), SessionError> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path).map_err(|source| SessionError::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(windows)]
pub(crate) fn create_windows_session_directory(path: &Path) -> Result<(), SessionError> {
    cookie_agent_models::secure_store::SecureDirectory::open(path)
        .map(|_| ())
        .map_err(|error| match error {
            cookie_agent_models::secure_store::SecureStoreError::Io(source) => SessionError::Io {
                path: path.to_owned(),
                source,
            },
            error => SessionError::Io {
                path: path.to_owned(),
                source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, error),
            },
        })
}

#[cfg(windows)]
fn create_windows_session_file(path: &Path) -> Result<(), SessionError> {
    cookie_agent_models::secure_store::create_windows_private_file(path)
        .map(drop)
        .map_err(|source| SessionError::Io {
            path: path.to_owned(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::Path,
        sync::{Arc, mpsc},
        thread,
    };

    #[cfg(unix)]
    use std::{
        ffi::OsString,
        os::unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
        },
    };

    use cookie_agent_config::{ModelPricing, PicoUsdPerMillion, PricingConfig};
    use cookie_agent_protocol::{
        AgentId, AgentMode, AgentRevision, AttemptId, CatalogRevision, ClientRunId, EventPayload,
        InternalAgentBackend, InternalAgentFailure, InternalAgentInvocationId, InternalAgentKind,
        InternalAgentRunId, InvocationId, ModelFinishReason, ModelRevision, PersistedModelTurn,
        ProviderStateRevision, RecipeRegistryRevision, RunId, RuntimeRevision, SafeCode,
        SafeDisplayText, SafeErrorMessage, SafeInternalAgentCall, SafeInternalAgentResult,
        SessionId, SessionOrigin, SessionPermissionOverlay, SessionTitle, SessionTitleChange,
        Sha256Digest, ToolCallId, Usage,
    };

    use crate::ownership::owner_lock_path;

    use super::{
        LAYOUT_MARKER_FILE, PROJECT_CWD_FILE, SESSION_META_FILE, SESSIONS_ROOT_DIR,
        SUBAGENT_INDEX_FILE, SUBAGENT_INDEX_VERSION, SUBAGENTS_DIR, SessionError, SessionStore,
        meta_path, projection,
    };

    /// The v2 work-dir store for `cwd` (what a freshly opened store creates).
    fn workdir_dir(data_root: &Path, cwd: &Path) -> std::path::PathBuf {
        data_root
            .join(SESSIONS_ROOT_DIR)
            .join(SessionStore::workdir_key(cwd))
    }

    #[cfg(unix)]
    fn cwd_file(data_root: &Path, cwd: &Path) -> std::path::PathBuf {
        workdir_dir(data_root, cwd).join(PROJECT_CWD_FILE)
    }

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temporary root");
        #[cfg(unix)]
        {
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("private temporary root");
        }
        #[cfg(windows)]
        {
            fs::remove_dir(directory.path()).expect("remove ordinary temp directory");
            cookie_agent_models::secure_store::SecureDirectory::open(directory.path())
                .expect("private temporary root");
        }
        directory
    }

    fn create_private_test_dir_all(path: &Path) {
        #[cfg(unix)]
        {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(path).expect("private test directory");
        }
        #[cfg(windows)]
        cookie_agent_models::secure_store::create_windows_private_dir_all(path)
            .expect("private test directory");
    }

    fn write_private_test_file(path: &Path, contents: impl AsRef<[u8]>) {
        #[cfg(unix)]
        {
            use std::io::Write as _;

            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .expect("private test file");
            file.write_all(contents.as_ref())
                .expect("write private test file");
        }
        #[cfg(windows)]
        {
            use std::io::Write as _;

            let mut file = cookie_agent_models::secure_store::create_windows_private_file(path)
                .expect("private test file");
            file.write_all(contents.as_ref())
                .expect("write private test file");
        }
    }

    fn persist_test_session(store: &SessionStore) -> SessionId {
        persist_test_session_with_origin(store, SessionOrigin::Root)
    }

    /// Creates and durably publishes a session carrying `origin`. Delegated
    /// origins land under the root's `subagents/` directory (§2.2).
    fn persist_test_session_with_origin(store: &SessionStore, origin: SessionOrigin) -> SessionId {
        let session_id = SessionId::new_v7();
        let agent = crate::test_support::agent_snapshot("test", AgentMode::Primary);
        let selection = crate::test_support::run_selection("test");
        let binding = agent.fallback_chain[0].clone();
        let revision = |label: char| format!("sha256:{}", label.to_string().repeat(64));
        let runtime_revision = RuntimeRevision::new(revision('1')).unwrap();
        let catalog_revision = CatalogRevision::new(revision('2')).unwrap();
        let provider_state_revision = ProviderStateRevision::new(revision('3')).unwrap();
        let model_revision = ModelRevision::new(revision('4')).unwrap();
        let agent_revision = AgentRevision::new(revision('5')).unwrap();
        let recipe_registry_revision = RecipeRegistryRevision::new(revision('6')).unwrap();
        store
            .create(
                session_id,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionCreated {
                    origin,
                    cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test")
                        .unwrap(),
                    creation_selection: selection.clone(),
                    creation_agent: Box::new(agent.clone()),
                    runtime_revision: runtime_revision.clone(),
                    catalog_revision: catalog_revision.clone(),
                    provider_state_revision: provider_state_revision.clone(),
                    model_revision: model_revision.clone(),
                    agent_revision: agent_revision.clone(),
                    recipe_registry_revision: recipe_registry_revision.clone(),
                    manifest_revision: binding.manifest_revision.clone(),
                },
            )
            .unwrap();
        let run_id = RunId::new_v7();
        store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::RunStarted {
                    client_run_id: ClientRunId::new("private-session-test").unwrap(),
                    selection,
                    agent: Box::new(agent),
                    runtime_revision,
                    catalog_revision,
                    provider_state_revision,
                    model_revision,
                    agent_revision,
                    recipe_registry_revision,
                    manifest_revision: binding.manifest_revision.clone(),
                    selected_suffix: vec![binding],
                    internal_agents: Vec::new(),
                    input_through_seq: 1,
                },
            )
            .unwrap();
        store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::UserInputSubmitted {
                    input: "persist me".into(),
                },
            )
            .unwrap();
        session_id
    }

    fn create_buffered_test_session(store: &SessionStore) -> SessionId {
        let session_id = SessionId::new_v7();
        let agent = crate::test_support::agent_snapshot("test", AgentMode::Primary);
        let selection = crate::test_support::run_selection("test");
        let binding = agent.fallback_chain[0].clone();
        let revision = |label: char| format!("sha256:{}", label.to_string().repeat(64));
        store
            .create(
                session_id,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionCreated {
                    origin: SessionOrigin::Root,
                    cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test")
                        .unwrap(),
                    creation_selection: selection,
                    creation_agent: Box::new(agent),
                    runtime_revision: RuntimeRevision::new(revision('1')).unwrap(),
                    catalog_revision: CatalogRevision::new(revision('2')).unwrap(),
                    provider_state_revision: ProviderStateRevision::new(revision('3')).unwrap(),
                    model_revision: ModelRevision::new(revision('4')).unwrap(),
                    agent_revision: AgentRevision::new(revision('5')).unwrap(),
                    recipe_registry_revision: RecipeRegistryRevision::new(revision('6')).unwrap(),
                    manifest_revision: binding.manifest_revision,
                },
            )
            .expect("create buffered session");
        session_id
    }

    #[test]
    fn ownership_is_acquired_on_write_open_and_released_with_the_store() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        assert!(owner_lock_path(&owner.session_dir(session_id)).is_file());
        let stale_log = owner.get(session_id).expect("owned projection").log;
        let (authorized, release_append) = stale_log.install_append_authorization_hook_for_test();
        let appending = thread::spawn(move || {
            stale_log.append(
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionReverted { through_seq: 1 },
            )
        });
        authorized
            .recv()
            .expect("append passed initial authorization");

        assert_eq!(Arc::strong_count(&owner), 1);
        drop(owner);
        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        observer
            .open_for_write(session_id)
            .expect("adopt after owner drops");
        assert!(observer.is_owned(session_id));
        release_append.send(()).expect("release stale append");
        assert!(matches!(
            appending.join().expect("stale append thread"),
            Err(crate::events::EventLogError::ReadOnly(_))
        ));
    }

    #[test]
    fn ownership_release_does_not_wait_for_store_drop() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        let stale_log = owner.get(session_id).expect("owned projection").log;

        owner.release_ownership();
        owner.release_ownership();

        assert!(!owner.is_owned(session_id));
        assert!(matches!(
            owner.append(
                session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionReverted { through_seq: 1 },
            ),
            Err(SessionError::StoreClosed)
        ));
        assert!(matches!(
            stale_log.append(
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionReverted { through_seq: 1 },
            ),
            Err(crate::events::EventLogError::ReadOnly(_))
        ));
        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        observer
            .open_for_write(session_id)
            .expect("adopt after explicit ownership release");
    }

    #[test]
    fn ownership_release_waits_for_an_append_that_already_won_serialization() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        let log = owner.get(session_id).expect("owned projection").log;
        let (authorized, release_append) = log.install_append_authorization_hook_for_test();
        let append_store = Arc::clone(&owner);
        let appending = thread::spawn(move || {
            append_store.append(
                session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionReverted { through_seq: 1 },
            )
        });
        authorized.recv().expect("append passed authorization");

        let release_store = Arc::clone(&owner);
        let (released, release_observed) = mpsc::channel();
        let releasing = thread::spawn(move || {
            release_store.release_ownership();
            released.send(()).expect("report ownership release");
        });
        assert!(matches!(
            release_observed.recv_timeout(std::time::Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        assert!(matches!(
            observer.open_for_write(session_id),
            Err(SessionError::SessionLocked(id)) if id == session_id
        ));

        release_append.send(()).expect("release append");
        appending
            .join()
            .expect("append thread")
            .expect("append wins");
        release_observed
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("ownership releases after append");
        releasing.join().expect("release thread");
        observer
            .open_for_write(session_id)
            .expect("adopt after append and release");
    }

    #[test]
    fn eviction_retains_ownership_and_reopens_for_the_owner() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        assert!(owner.evict(session_id).expect("evict owned session"));
        assert!(!owner.is_resident(session_id));

        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        let snapshot = observer.get(session_id).expect("read-only snapshot");
        assert!(matches!(
            snapshot.log.append(
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionReverted { through_seq: 1 },
            ),
            Err(crate::events::EventLogError::ReadOnly(_))
        ));
        assert!(matches!(
            observer.open_for_write(session_id),
            Err(SessionError::SessionLocked(id)) if id == session_id
        ));
        owner
            .open_for_write(session_id)
            .expect("owner reopens after eviction");
    }

    #[test]
    fn foreign_snapshot_preserves_torn_tail_until_owned_adoption() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        let event_path = owner.session_dir(session_id).join("events.jsonl");
        let mut bytes = fs::read(&event_path).expect("read event log");
        bytes.extend_from_slice(b"{\"torn\"");
        fs::write(&event_path, &bytes).expect("write torn tail");

        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        observer.get(session_id).expect("read foreign snapshot");
        assert_eq!(fs::read(&event_path).expect("tail remains"), bytes);
        assert!(matches!(
            observer.open_for_write(session_id),
            Err(SessionError::SessionLocked(id)) if id == session_id
        ));

        drop(owner);
        observer.open_for_write(session_id).expect("adopt torn log");
        assert_ne!(fs::read(&event_path).expect("tail truncated"), bytes);
        assert!(
            fs::read(&event_path)
                .expect("read repaired log")
                .ends_with(b"\n")
        );
    }

    #[test]
    fn failed_adoption_is_unobservable_and_retryable() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        assert_eq!(Arc::strong_count(&owner), 1);
        drop(owner);

        let first = SessionStore::open(&data, &cwd).expect("first adopter");
        assert_eq!(
            first.begin_write(session_id).unwrap(),
            super::WriteOpen::Adopting
        );
        assert!(!first.is_owned(session_id));
        assert!(matches!(
            first.append(
                session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionReverted { through_seq: 1 },
            ),
            Err(SessionError::SessionLocked(id)) if id == session_id
        ));
        first.rollback_adoption(session_id);

        let second = SessionStore::open(&data, &cwd).expect("second adopter");
        assert_eq!(
            second.begin_write(session_id).unwrap(),
            super::WriteOpen::Adopting
        );
        second.commit_adoption(session_id).expect("commit retry");
        assert!(second.is_owned(session_id));
    }

    #[test]
    fn concurrent_adoption_has_one_winner() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        drop(owner);
        let stores = [
            SessionStore::open(&data, &cwd).expect("first contender"),
            SessionStore::open(&data, &cwd).expect("second contender"),
        ];
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let threads = stores.clone().map(|store| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                match store.begin_write(session_id) {
                    Ok(super::WriteOpen::Adopting) => {
                        store.commit_adoption(session_id).expect("commit winner");
                        true
                    }
                    Err(SessionError::SessionLocked(id)) if id == session_id => false,
                    result => panic!("unexpected adoption result: {result:?}"),
                }
            })
        });
        barrier.wait();
        let winners = threads
            .into_iter()
            .map(|thread| usize::from(thread.join().expect("adoption contender")))
            .sum::<usize>();
        assert_eq!(winners, 1);
    }

    #[test]
    fn buffered_publish_is_locked_before_the_directory_becomes_visible() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let creator = SessionStore::open(&data, &cwd).expect("creator store");
        let session_id = create_buffered_test_session(&creator);
        let (reached, release) = creator.install_publish_hook_for_test();
        let publishing = {
            let creator = Arc::clone(&creator);
            thread::spawn(move || creator.persist_buffered_session(session_id))
        };
        assert_eq!(
            reached.recv().expect("publisher acquired ownership lock"),
            session_id
        );

        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        assert!(
            matches!(observer.get(session_id), Err(SessionError::Missing(id)) if id == session_id)
        );
        release.send(()).expect("release publisher");
        publishing
            .join()
            .expect("publisher thread")
            .expect("publish session");
        assert!(matches!(
            observer.open_for_write(session_id),
            Err(SessionError::SessionLocked(id)) if id == session_id
        ));
    }

    #[test]
    fn fork_publish_is_locked_before_the_directory_becomes_visible() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let creator = SessionStore::open(&data, &cwd).expect("creator store");
        let source_id = persist_test_session(&creator);
        let through_seq = creator
            .get(source_id)
            .expect("source projection")
            .log
            .events()
            .into_iter()
            .find(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
            .expect("source user input")
            .seq;
        let (reached, release) = creator.install_publish_hook_for_test();
        let publishing = {
            let creator = Arc::clone(&creator);
            thread::spawn(move || {
                creator.fork(
                    source_id,
                    through_seq,
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                )
            })
        };
        let fork_id = reached.recv().expect("fork acquired ownership lock");

        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        assert!(matches!(observer.get(fork_id), Err(SessionError::Missing(id)) if id == fork_id));
        release.send(()).expect("release fork publisher");
        assert_eq!(
            publishing
                .join()
                .expect("fork publisher thread")
                .expect("publish fork"),
            fork_id
        );
        assert!(matches!(
            observer.open_for_write(fork_id),
            Err(SessionError::SessionLocked(id)) if id == fork_id
        ));
    }

    #[test]
    fn metadata_cache_reads_never_observe_partial_replacements() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).expect("session store");
        let session_id = persist_test_session(&store);
        let session_dir = store.session_dir(session_id);
        let cache_path = meta_path(&session_dir);
        let event_path = session_dir.join("events.jsonl");
        let meta = store.get(session_id).expect("projection").meta;
        let writer = thread::spawn({
            let cache_path = cache_path.clone();
            let meta = meta.clone();
            move || {
                for _ in 0..100 {
                    super::write_cache(&cache_path, &meta).expect("replace metadata cache");
                }
            }
        });
        for _ in 0..100 {
            let read = super::read_cache(&cache_path, &event_path).expect("read complete cache");
            assert_eq!(read.session_id, session_id);
        }
        writer.join().expect("metadata writer");
    }

    #[test]
    fn discovery_does_not_reread_known_evicted_metadata() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        drop(owner);

        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        let cached = observer.summary(session_id).expect("cached summary");
        assert!(!observer.is_resident(session_id));
        let mut replacement = cached.meta.clone();
        replacement.title = Some(
            cookie_agent_protocol::SessionTitle::new("changed on disk").expect("replacement title"),
        );
        super::write_cache(
            &observer
                .meta_cache_path(session_id)
                .expect("metadata cache path"),
            &replacement,
        )
        .expect("replace metadata cache");

        let rediscovered = observer
            .all_summaries()
            .into_iter()
            .find(|summary| summary.meta.session_id == session_id)
            .expect("rediscovered summary");
        assert_eq!(rediscovered.meta.title, cached.meta.title);
    }

    /// Builds a delegated origin filed under `root`, nested below `parent`.
    fn delegated_origin(root: SessionId, parent: SessionId, depth: u32) -> SessionOrigin {
        SessionOrigin::Delegated {
            root_session_id: root,
            parent_session_id: parent,
            parent_run_id: RunId::new_v7(),
            parent_tool_call_id: ToolCallId::new_v7(),
            invocation_id: InvocationId::new_v7(),
            depth,
        }
    }

    fn test_user_input_seq(store: &SessionStore, id: SessionId) -> u64 {
        store
            .get(id)
            .expect("session")
            .log
            .events()
            .into_iter()
            .find(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
            .expect("user input event")
            .seq
    }

    /// §8.2 #4: startup discovery is root-only. With every child event log made
    /// unreadable, a reopened store still lists the whole tree because it reads
    /// root `metadata` caches plus each root's `subagents/index.json`.
    #[cfg(unix)]
    #[test]
    fn startup_discovery_never_reads_child_logs() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&store);
        let child = persist_test_session_with_origin(&store, delegated_origin(root, root, 1));
        let grandchild = persist_test_session_with_origin(&store, delegated_origin(root, child, 2));

        // Both descendants sit one level under the root, whatever their depth.
        let root_dir = store.session_dir(root);
        let child_dir = store.session_dir(child);
        let grandchild_dir = store.session_dir(grandchild);
        assert_eq!(
            child_dir,
            root_dir.join(SUBAGENTS_DIR).join(child.to_string())
        );
        assert_eq!(
            grandchild_dir,
            root_dir.join(SUBAGENTS_DIR).join(grandchild.to_string())
        );
        let expected = store.all_summaries();
        assert_eq!(expected.len(), 3);
        drop(store);

        for directory in [&child_dir, &grandchild_dir] {
            fs::set_permissions(
                directory.join("events.jsonl"),
                fs::Permissions::from_mode(0o000),
            )
            .expect("unreadable child log");
        }

        let observer = SessionStore::open(&data, &cwd).expect("cold observer store");
        let discovered = observer.all_summaries();
        assert_eq!(
            discovered.len(),
            3,
            "index.json pre-populates child summaries"
        );
        for summary in &expected {
            let found = discovered
                .iter()
                .find(|found| found.meta.session_id == summary.meta.session_id)
                .expect("discovered summary");
            assert_eq!(found.meta, summary.meta);
        }
        assert!(!observer.is_resident(child));
        assert_eq!(
            observer
                .children(root)
                .into_iter()
                .map(|child| child.session_id)
                .collect::<Vec<_>>(),
            vec![child]
        );
        assert_eq!(
            observer
                .children(child)
                .into_iter()
                .map(|child| child.session_id)
                .collect::<Vec<_>>(),
            vec![grandchild]
        );
        assert!(
            !observer.is_tree_loaded(root),
            "an unreadable child must not report a loaded tree"
        );
        assert_eq!(
            observer.root_snapshots().len(),
            1,
            "only the root log is read"
        );

        // A child log is only needed where the tree is actually assembled, and
        // there an unreadable child fails closed (§3.2.2).
        assert!(observer.tree(root).is_err());
        for directory in [&child_dir, &grandchild_dir] {
            fs::set_permissions(
                directory.join("events.jsonl"),
                fs::Permissions::from_mode(0o600),
            )
            .expect("readable child log");
        }
        let tree = observer.tree(root).expect("tree after a lazy load");
        assert_eq!(tree.session.session_id, root);
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].session.session_id, child);
        assert_eq!(tree.children[0].children[0].session.session_id, grandchild);
    }

    /// §8.2 #5: opening a root reads each child log exactly once, leaves the
    /// children evicted, and never re-reads them for later queries.
    #[cfg(unix)]
    #[test]
    fn tree_load_reads_each_child_once_and_leaves_them_evicted() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        let first = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        let second = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        drop(owner);

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        assert!(
            !store.is_tree_loaded(root),
            "a cold store has no loaded trees"
        );
        store.get(root).expect("open the root");
        assert!(store.is_tree_loaded(root), "opening a root loads its tree");
        assert_eq!(store.tree_load_count(root), 1);
        for child in [first, second] {
            assert!(!store.is_resident(child), "children stay out of residency");
            assert!(store.session_exists(child));
        }
        assert_eq!(store.children(root).len(), 2);

        // Child logs go unreadable: everything the tree offers is already
        // cached, so further queries keep working and no second load happens.
        for child in [first, second] {
            fs::set_permissions(
                store.session_dir(child).join("events.jsonl"),
                fs::Permissions::from_mode(0o000),
            )
            .expect("unreadable child log");
        }
        assert_eq!(store.children(root).len(), 2);
        assert_eq!(store.tree(root).expect("cached tree").children.len(), 2);
        assert_eq!(store.get(root).expect("root again").meta.session_id, root);
        assert_eq!(store.tree_load_count(root), 1, "the pass runs once");
    }

    /// §8.2 #6: addressing a child directly locates it by directory, loads its
    /// root's tree first, then serves the child.
    #[test]
    fn direct_address_child_loads_its_tree_first() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        drop(owner);

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        // Force placement discovery instead of serving from the summary cache.
        let index = store
            .session_dir(root)
            .join(SUBAGENTS_DIR)
            .join(SUBAGENT_INDEX_FILE);
        fs::remove_file(&index).expect("remove child index");
        assert!(!store.is_tree_loaded(root));

        let projection = store.get(child).expect("direct child access");
        assert_eq!(projection.meta.session_id, child);
        assert!(
            store.is_tree_loaded(root),
            "the child's tree completed first"
        );
        assert_eq!(store.tree_load_count(root), 1);
        assert!(!store.is_resident(child));
        assert!(index.is_file(), "the load rebuilt the child summary cache");
    }

    /// §8.2 #11: a fork inherits the source's tree, so fork-of-root is published
    /// into the work dir and fork-of-child into the same root's `subagents/`.
    #[test]
    fn fork_placement_follows_the_source_tree() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).expect("session store");
        let root = persist_test_session(&store);
        let child = persist_test_session_with_origin(&store, delegated_origin(root, root, 1));
        let origin = cookie_agent_protocol::EventOrigin::new("client:test").unwrap();

        let root_fork = store
            .fork(root, test_user_input_seq(&store, root), origin.clone())
            .expect("fork of root");
        let child_fork = store
            .fork(child, test_user_input_seq(&store, child), origin)
            .expect("fork of child");

        let root_dir = store.session_dir(root);
        assert_eq!(
            store.session_dir(root_fork).parent(),
            Some(store.workdir_dir.as_path())
        );
        assert_eq!(
            store.session_dir(child_fork).parent(),
            Some(root_dir.join(SUBAGENTS_DIR).as_path())
        );
        assert!(matches!(
            store
                .get(child_fork)
                .expect("forked child")
                .meta
                .origin,
            SessionOrigin::Delegated {
                root_session_id,
                parent_session_id,
                ..
            } if root_session_id == root && parent_session_id == root
        ));
        let index = store.tree_members(root);
        assert!(index.contains(&child));
        assert!(index.contains(&child_fork));
        assert!(
            !store
                .children(root)
                .into_iter()
                .any(|listed| listed.session_id == root_fork)
        );
        assert!(root_dir.join(SUBAGENTS_DIR).join("index.json").is_file());
    }

    /// §8.2 #12: `subagents/index.json` is a cache. Corrupt or missing content
    /// never fails a startup, and placement discovery rebuilds it.
    #[test]
    fn subagent_index_corruption_is_rebuilt_not_fatal() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&store);
        let child = persist_test_session_with_origin(&store, delegated_origin(root, root, 1));
        let index_path = store
            .session_dir(root)
            .join(SUBAGENTS_DIR)
            .join(SUBAGENT_INDEX_FILE);
        assert!(index_path.is_file(), "index written on child creation");
        drop(store);

        for corruption in ["not json at all", "{\"version\": 99, \"children\": []}"] {
            fs::write(&index_path, corruption).expect("corrupt index");
            let observer = SessionStore::open(&data, &cwd).expect("cold open with corrupt index");
            let listed = observer
                .children(root)
                .into_iter()
                .map(|child| child.session_id)
                .collect::<Vec<_>>();
            // The directory scan re-adopts the filed child and rebuilds the cache.
            assert!(listed.contains(&child), "placement rescans filed children");
            let rebuilt: serde_json::Value =
                serde_json::from_slice(&fs::read(&index_path).expect("rebuilt index"))
                    .expect("valid index json");
            assert_eq!(
                rebuilt["version"],
                serde_json::json!(SUBAGENT_INDEX_VERSION)
            );
            assert_eq!(rebuilt["children"].as_array().expect("children").len(), 1);
            drop(observer);
        }

        fs::remove_file(&index_path).expect("remove index");
        let observer = SessionStore::open(&data, &cwd).expect("cold open with missing index");
        assert!(
            observer
                .children(root)
                .into_iter()
                .any(|listed| listed.session_id == child)
        );
    }

    fn append_pending_test_delta(
        store: &SessionStore,
        session_id: SessionId,
        text: &str,
    ) -> (
        Arc<crate::events::EventLog>,
        cookie_agent_protocol::StoredEvent,
    ) {
        let projection = store.get(session_id).expect("session projection");
        let (run_id, resolved_model, prompt_fingerprint) = projection
            .log
            .events()
            .iter()
            .find_map(|event| match &event.payload {
                EventPayload::RunStarted {
                    agent,
                    selected_suffix,
                    ..
                } => Some((
                    event.run_id.expect("run id"),
                    crate::model_history::wire_model(
                        selected_suffix.first().expect("selected model"),
                    ),
                    agent.prompt_fingerprint.clone(),
                )),
                _ => None,
            })
            .expect("run event");
        let attempt_id = AttemptId::new_v7();
        store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::ModelAttemptStarted {
                    attempt_id,
                    attempt_ordinal: 1,
                    fallback_index: 0,
                    retry_ordinal: 0,
                    resolved_model,
                    prompt_fingerprint,
                },
            )
            .expect("start attempt");
        let log = store.get(session_id).expect("session projection").log;
        log.pause_background_sync_for_test();
        let delta = store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::TextDelta {
                    attempt_id,
                    text: text.into(),
                },
            )
            .expect("append buffered delta");
        (log, delta)
    }

    #[test]
    fn eviction_waits_for_pending_stream_records_to_sync() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let store = SessionStore::open(&temporary.path().join("data"), &cwd).unwrap();
        let session_id = persist_test_session(&store);
        let (log, _) = append_pending_test_delta(&store, session_id, "durable before eviction");
        let (sync_reached, release_sync) = log.install_sync_hook_for_test();
        let (eviction_done, eviction_result) = mpsc::channel();
        let evicting = {
            let store = store.clone();
            thread::spawn(move || {
                eviction_done
                    .send(store.evict(session_id))
                    .expect("report eviction result");
            })
        };

        sync_reached.recv().expect("eviction reached pending sync");
        assert!(matches!(
            eviction_result.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release_sync.send(()).expect("release eviction sync");
        assert!(
            eviction_result
                .recv()
                .expect("receive eviction result")
                .expect("evict session")
        );
        evicting.join().expect("eviction thread");

        let durable = crate::events::load_jsonl::<cookie_agent_protocol::StoredEvent>(
            &store.session_dir(session_id).join("events.jsonl"),
        )
        .expect("read evicted event log");
        assert!(durable.iter().any(|event| matches!(
            &event.payload,
            EventPayload::TextDelta { text, .. } if text == "durable before eviction"
        )));
    }

    #[test]
    fn fork_flushes_pending_source_records_before_copying() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let store = SessionStore::open(&temporary.path().join("data"), &cwd).unwrap();
        let session_id = persist_test_session(&store);
        let (log, delta) = append_pending_test_delta(&store, session_id, "copied after sync");
        assert!(log.writer_is_open_for_test());
        let delta_seq = delta.seq;
        let run_id = delta.run_id.expect("delta run id");
        let EventPayload::TextDelta { attempt_id, .. } = delta.payload else {
            panic!("pending event is a text delta")
        };
        let (sync_reached, release_sync) = log.install_sync_hook_for_test();
        let (fork_done, fork_result) = mpsc::channel();
        let forking = {
            let store = store.clone();
            thread::spawn(move || {
                fork_done
                    .send(store.fork(
                        session_id,
                        delta_seq,
                        cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
                    ))
                    .expect("report fork result");
            })
        };

        sync_reached.recv().expect("fork reached source sync");
        assert!(matches!(
            fork_result.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release_sync.send(()).expect("release fork sync");
        let fork_id = fork_result
            .recv()
            .expect("receive fork result")
            .expect("fork session");
        forking.join().expect("fork thread");
        assert!(!log.writer_is_open_for_test());

        let copied = store.get(fork_id).expect("fork projection").log.events();
        assert!(copied.iter().any(|event| matches!(
            &event.payload,
            EventPayload::TextDelta { text, .. } if text == "copied after sync"
        )));
        store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::ReasoningDelta {
                    attempt_id,
                    text: "reopened after fork".into(),
                },
            )
            .expect("append after suspended fork source");
        assert!(log.writer_is_open_for_test());
        log.flush().expect("flush reopened source writer");
    }

    #[cfg(unix)]
    #[test]
    fn unix_buffered_session_is_private_at_creation() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).unwrap();
        let session_id = persist_test_session(&store);
        let project = store.project_dir_path();
        let session = store.session_dir(session_id);

        for path in [
            data.clone(),
            data.join(SESSIONS_ROOT_DIR),
            project.to_owned(),
            session.clone(),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for path in [
            project.join(PROJECT_CWD_FILE),
            project.join(LAYOUT_MARKER_FILE),
            session.join("events.jsonl"),
            session.join(SESSION_META_FILE),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_session_reuses_preexisting_loose_modes() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).unwrap();
        let session_id = persist_test_session(&store);
        let project = store.project_dir_path().to_owned();
        let session = store.session_dir(session_id);
        let directories = [
            data.clone(),
            data.join(SESSIONS_ROOT_DIR),
            project.clone(),
            session.clone(),
        ];
        let files = [
            project.join(PROJECT_CWD_FILE),
            project.join(LAYOUT_MARKER_FILE),
            session.join("events.jsonl"),
            session.join(SESSION_META_FILE),
        ];
        for path in &directories {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for path in &files {
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        drop(store);

        let reopened = SessionStore::open(&data, &cwd).unwrap();
        reopened.get(session_id).expect("loose existing session");
        for path in directories {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        for path in files {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o644
            );
        }
    }

    // Requires Unix symlink semantics and exact raw path bytes.
    #[cfg(unix)]
    #[test]
    fn canonical_aliases_share_the_existing_project_and_record_canonical_bytes() {
        let temp = tempfile::tempdir().expect("temp");
        let real = temp.path().join("real");
        let alias = temp.path().join("alias");
        let data = temp.path().join("data");
        fs::create_dir(&real).expect("real cwd");
        symlink(&real, &alias).expect("alias");

        let real_store = SessionStore::open(&data, &real).expect("real store");
        let alias_store = SessionStore::open(&data, &alias).expect("alias store");
        assert_eq!(
            real_store.project_dir_path(),
            alias_store.project_dir_path()
        );
        assert_eq!(
            fs::read(cwd_file(&data, &alias)).expect("cwd bytes"),
            real.canonicalize()
                .expect("canonical")
                .as_os_str()
                .as_bytes()
        );
    }

    // Requires constructing and comparing non-UTF8 Unix path bytes.
    #[cfg(unix)]
    #[test]
    fn non_utf8_cwd_round_trips_exact_bytes() {
        let temp = tempfile::tempdir().expect("temp");
        let cwd = temp
            .path()
            .join(OsString::from_vec(b"project-\xfe\xff".to_vec()));
        let data = temp.path().join("data");
        fs::create_dir(&cwd).expect("cwd");

        SessionStore::open(&data, &cwd).expect("store");
        assert_eq!(
            fs::read(cwd_file(&data, &cwd)).expect("cwd bytes"),
            cwd.canonicalize()
                .expect("canonical")
                .as_os_str()
                .as_bytes()
        );
    }

    // Existing state is reused without permission repair.
    #[cfg(unix)]
    #[test]
    fn cwd_file_is_private_at_creation_and_loose_mode_is_reused() {
        let temp = tempfile::tempdir().expect("temp");
        let data = temp.path().join("data");
        SessionStore::open(&data, temp.path()).expect("store");
        let path = cwd_file(&data, temp.path());
        assert_eq!(
            fs::metadata(&path).expect("metadata").mode() & 0o7777,
            0o600
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("loosen mode");
        SessionStore::open(&data, temp.path()).expect("reopen");
        assert_eq!(fs::metadata(path).expect("metadata").mode() & 0o7777, 0o644);
    }

    // Verifies atomic replacement using Unix inode identity and mode bits.
    #[cfg(unix)]
    #[test]
    fn stale_cwd_file_is_replaced_atomically() {
        let temp = tempfile::tempdir().expect("temp");
        let data = temp.path().join("data");
        SessionStore::open(&data, temp.path()).expect("store");
        let path = cwd_file(&data, temp.path());
        fs::write(&path, b"stale project path").expect("stale file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        let stale_inode = fs::metadata(&path).expect("stale metadata").ino();

        SessionStore::open(&data, temp.path()).expect("refresh");
        assert_ne!(
            fs::metadata(&path).expect("new metadata").ino(),
            stale_inode
        );
        assert_eq!(
            fs::read(&path).expect("cwd bytes"),
            temp.path()
                .canonicalize()
                .expect("canonical")
                .as_os_str()
                .as_bytes()
        );
        let project = path.parent().expect("project");
        assert!(
            fs::read_dir(project)
                .expect("project entries")
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(".cwd."))
        );
    }

    // Verifies retention using Unix inode identity.
    #[cfg(unix)]
    #[test]
    fn correct_cwd_file_is_retained_on_reopen() {
        let temp = tempfile::tempdir().expect("temp");
        let data = temp.path().join("data");
        SessionStore::open(&data, temp.path()).expect("store");
        let path = cwd_file(&data, temp.path());
        let inode = fs::metadata(&path).expect("metadata").ino();

        SessionStore::open(&data, temp.path()).expect("reopen");
        assert_eq!(fs::metadata(path).expect("metadata").ino(), inode);
    }

    /// An empty legacy project folder is promoted by the migration gate (§6.3);
    /// unrelated files in it are left where they are and the folder keeps a
    /// pointer for builds that still read the flat layout.
    #[test]
    fn empty_legacy_project_promotes_to_the_v2_work_dir() {
        let temp = private_tempdir();
        let data = temp.path().join("data");
        let project = SessionStore::project_dir(&data, temp.path());
        assert_eq!(
            project.file_name().expect("hash").to_string_lossy().len(),
            16
        );
        create_private_test_dir_all(&project.join("sessions"));
        fs::write(project.join("sentinel"), b"keep").expect("sentinel");
        assert!(!project.join(PROJECT_CWD_FILE).exists());

        let store = SessionStore::open(&data, temp.path()).expect("migrate existing project");
        assert!(!store.is_flat_layout());
        assert_eq!(
            fs::read(project.join("sentinel")).expect("sentinel"),
            b"keep"
        );
        assert!(!project.join("sessions").is_dir(), "emptied legacy store");
        assert!(project.join(".migrated").is_file(), "migration marker");
        assert!(project.join("MIGRATED").is_file(), "pointer for old builds");
        let workdir = store.workdir_dir_path();
        assert!(workdir.join(LAYOUT_MARKER_FILE).is_file(), "layout marker");
        assert!(workdir.join(PROJECT_CWD_FILE).is_file(), "cwd file");
    }

    #[test]
    fn replayed_stamps_keep_footer_and_session_cost_equal_across_pricing_changes() {
        let temp = private_tempdir();
        let path = temp.path().join("events.jsonl");
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let attempt_id = AttemptId::new_v7();
        let agent = crate::test_support::agent_snapshot("test", AgentMode::Primary);
        let selection = crate::test_support::run_selection("test");
        let binding = agent.fallback_chain[0].clone();
        let resolved_model = crate::model_history::wire_model(&binding);
        let model_key = resolved_model.selection.model.clone();
        let runtime_revision = RuntimeRevision::new(format!("sha256:{}", "1".repeat(64))).unwrap();
        let catalog_revision = CatalogRevision::new(format!("sha256:{}", "2".repeat(64))).unwrap();
        let provider_state_revision =
            ProviderStateRevision::new(format!("sha256:{}", "3".repeat(64))).unwrap();
        let model_revision = ModelRevision::new(format!("sha256:{}", "4".repeat(64))).unwrap();
        let agent_revision = AgentRevision::new(format!("sha256:{}", "5".repeat(64))).unwrap();
        let recipe_registry_revision =
            RecipeRegistryRevision::new(format!("sha256:{}", "6".repeat(64))).unwrap();
        let log = crate::events::EventLog::create(
            path.clone(),
            session_id,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionCreated {
                origin: SessionOrigin::Root,
                cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test").unwrap(),
                creation_selection: selection.clone(),
                creation_agent: Box::new(agent.clone()),
                runtime_revision: runtime_revision.clone(),
                catalog_revision: catalog_revision.clone(),
                provider_state_revision: provider_state_revision.clone(),
                model_revision: model_revision.clone(),
                agent_revision: agent_revision.clone(),
                recipe_registry_revision: recipe_registry_revision.clone(),
                manifest_revision: binding.manifest_revision.clone(),
            },
        )
        .unwrap();
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::RunStarted {
                client_run_id: ClientRunId::new("usage-replay").unwrap(),
                selection,
                agent: Box::new(agent.clone()),
                runtime_revision,
                catalog_revision,
                provider_state_revision,
                model_revision,
                agent_revision,
                recipe_registry_revision,
                manifest_revision: binding.manifest_revision.clone(),
                selected_suffix: vec![binding],
                internal_agents: Vec::new(),
                input_through_seq: 1,
            },
        )
        .unwrap();
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::UserInputSubmitted {
                input: "question".into(),
            },
        )
        .unwrap();
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved_model.clone(),
                prompt_fingerprint: agent.prompt_fingerprint.clone(),
            },
        )
        .unwrap();
        let usage = Usage {
            input_tokens: Some(120),
            input_tokens_no_cache: Some(70),
            input_tokens_cache_read: Some(40),
            input_tokens_cache_write: Some(10),
            output_tokens: Some(30),
            ..Usage::default()
        };
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::ModelTurnCommitted {
                attempt_id,
                model_turn_seq: 1,
                resolved_model: resolved_model.clone(),
                input_through_seq: 1,
                turn: PersistedModelTurn {
                    content: Vec::new(),
                    provider_options: BTreeMap::new(),
                    finish_reason: ModelFinishReason::Stop,
                    usage: usage.clone(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        )
        .unwrap();
        let usage_event = log
            .append(
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::ModelUsageRecorded {
                    model_turn_seq: 1,
                    agent_id: agent.agent.clone(),
                    resolved_model,
                    usage,
                    estimated_cost_pico_usd: Some(123_456_789_000),
                },
            )
            .unwrap();
        let through_seq = usage_event.seq;
        drop(log);

        let source_json = fs::read_to_string(&path).unwrap();
        let rewrite = |session_id: SessionId, stamp: Option<u64>| {
            source_json
                .lines()
                .map(|line| {
                    let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
                    value["session_id"] = serde_json::json!(session_id);
                    if value["payload"]["type"] == "model_usage_recorded" {
                        let payload = value["payload"].as_object_mut().unwrap();
                        payload.insert(
                            "estimated_cost_pico_usd".into(),
                            serde_json::to_value(stamp).unwrap(),
                        );
                    }
                    serde_json::to_string(&value).unwrap()
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        };
        let reopened = crate::events::EventLog::open(path, session_id).unwrap();
        // The TUI footer reducer sums these same durable pico-USD stamps.
        let footer_pico_usd = reopened
            .events()
            .iter()
            .filter_map(|event| match event.payload {
                EventPayload::ModelUsageRecorded {
                    estimated_cost_pico_usd,
                    ..
                } => estimated_cost_pico_usd,
                _ => None,
            })
            .sum::<u64>();
        let rebuilt = projection(reopened).unwrap();
        assert_eq!(rebuilt.usage_rollup.request_count, 1);
        assert_eq!(rebuilt.usage_rollup.input_tokens, 120);
        assert_eq!(rebuilt.usage_rollup.output_tokens, 30);
        assert_eq!(rebuilt.usage_rollup.cache_read_tokens, 40);
        assert_eq!(rebuilt.usage_rollup.cache_write_tokens, 10);
        assert_eq!(rebuilt.agent_usage[&agent.agent].request_count, 1);
        let changed_pricing = PricingConfig {
            models: BTreeMap::from([(
                model_key.clone(),
                ModelPricing {
                    input_per_million_usd: Some(
                        PicoUsdPerMillion::from_decimal_str("999").unwrap(),
                    ),
                    output_per_million_usd: Some(
                        PicoUsdPerMillion::from_decimal_str("999").unwrap(),
                    ),
                    ..ModelPricing::default()
                },
            )]),
        };
        let expected = Some(footer_pico_usd as f64 / 1_000_000_000_000.0);
        assert_eq!(
            crate::usage::with_pricing(
                rebuilt.usage_rollup.clone(),
                &PricingConfig::default(),
                &BTreeMap::new(),
            )
            .estimated_cost_usd,
            expected
        );

        let cwd = temp.path().join("fork-cwd");
        let data = temp.path().join("fork-data");
        create_private_test_dir_all(&cwd);
        let seed = SessionStore::open(&data, &cwd).unwrap();
        let sessions_dir = seed.sessions_dir_path().to_owned();
        drop(seed);
        let stamped_source = SessionId::new_v7();
        let unpriced_source = SessionId::new_v7();
        for (source_id, contents) in [
            (
                stamped_source,
                rewrite(stamped_source, Some(123_456_789_000)),
            ),
            (unpriced_source, rewrite(unpriced_source, None)),
        ] {
            let directory = sessions_dir.join(source_id.to_string());
            create_private_test_dir_all(&directory);
            write_private_test_file(&directory.join("events.jsonl"), contents);
        }
        let store = SessionStore::open(&data, &cwd).unwrap();
        let stamped_fork = store
            .fork(
                stamped_source,
                through_seq,
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .unwrap();
        let unpriced_fork = store
            .fork(
                unpriced_source,
                through_seq,
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .unwrap();
        assert_eq!(
            crate::usage::with_pricing(
                store.get(stamped_fork).unwrap().usage_rollup,
                &changed_pricing,
                &BTreeMap::new(),
            )
            .estimated_cost_usd,
            expected
        );
        assert_eq!(
            crate::usage::with_pricing(
                store.get(unpriced_fork).unwrap().usage_rollup,
                &changed_pricing,
                &BTreeMap::new(),
            )
            .estimated_cost_usd,
            None
        );
        assert_eq!(
            crate::usage::with_pricing(rebuilt.usage_rollup, &changed_pricing, &BTreeMap::new(),)
                .estimated_cost_usd,
            expected
        );
    }

    #[test]
    fn internal_usage_is_once_per_fallback_phase_and_all_kinds_survive_reopen() {
        let temp = private_tempdir();
        let path = temp.path().join("internal-usage-events.jsonl");
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let owner = crate::test_support::agent_snapshot("test", AgentMode::Primary);
        let selection = crate::test_support::run_selection("test");
        let binding = owner.fallback_chain[0].clone();
        let resolved_model = crate::model_history::wire_model(&binding);
        let fallback_model = crate::model_history::wire_model(
            &crate::test_support::model_binding_named("fallback-one"),
        );
        let revision = |value: char| format!("sha256:{}", value.to_string().repeat(64));
        let runtime_revision = RuntimeRevision::new(revision('1')).unwrap();
        let catalog_revision = CatalogRevision::new(revision('2')).unwrap();
        let provider_state_revision = ProviderStateRevision::new(revision('3')).unwrap();
        let model_revision = ModelRevision::new(revision('4')).unwrap();
        let agent_revision = AgentRevision::new(revision('5')).unwrap();
        let recipe_registry_revision = RecipeRegistryRevision::new(revision('6')).unwrap();
        let log = crate::events::EventLog::create(
            path.clone(),
            session_id,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionCreated {
                origin: SessionOrigin::Root,
                cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test").unwrap(),
                creation_selection: selection.clone(),
                creation_agent: Box::new(owner.clone()),
                runtime_revision: runtime_revision.clone(),
                catalog_revision: catalog_revision.clone(),
                provider_state_revision: provider_state_revision.clone(),
                model_revision: model_revision.clone(),
                agent_revision: agent_revision.clone(),
                recipe_registry_revision: recipe_registry_revision.clone(),
                manifest_revision: binding.manifest_revision.clone(),
            },
        )
        .unwrap();
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::RunStarted {
                client_run_id: ClientRunId::new("internal-usage-replay").unwrap(),
                selection,
                agent: Box::new(owner),
                runtime_revision,
                catalog_revision,
                provider_state_revision,
                model_revision,
                agent_revision,
                recipe_registry_revision,
                manifest_revision: binding.manifest_revision.clone(),
                selected_suffix: vec![binding],
                internal_agents: Vec::new(),
                input_through_seq: 1,
            },
        )
        .unwrap();

        let kinds = [
            (
                InternalAgentKind::Approval,
                cookie_agent_config::BUILT_IN_APPROVAL_AGENT_ID,
            ),
            (
                InternalAgentKind::ContextCompaction,
                cookie_agent_config::BUILT_IN_COMPACTION_AGENT_ID,
            ),
            (
                InternalAgentKind::SessionTitle,
                cookie_agent_config::BUILT_IN_TITLE_AGENT_ID,
            ),
        ];
        for (index, (kind, agent_name)) in kinds.into_iter().enumerate() {
            let invocation_id = InternalAgentInvocationId::new_v7();
            let internal_run_id = InternalAgentRunId::new_v7();
            let agent_id = AgentId::new(agent_name).unwrap();
            log.append(
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::InternalAgentStarted {
                    invocation_id,
                    internal_run_id,
                    kind,
                    backend: InternalAgentBackend::Model {
                        resolved_model: resolved_model.clone(),
                    },
                    call: SafeInternalAgentCall {
                        name: SafeCode::new("internal").unwrap(),
                        input_summary: SafeDisplayText::new("bounded input").unwrap(),
                        input_digest: Sha256Digest::of_bytes(b"input"),
                    },
                },
            )
            .unwrap();
            let usage = Usage {
                input_tokens: Some(100 + index as u64),
                input_tokens_cache_read: Some(0),
                output_tokens: Some(10),
                output_tokens_reasoning: Some(0),
                ..Usage::default()
            };
            log.append(
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::InternalAgentUsageRecorded {
                    internal_run_id,
                    kind,
                    agent_id: agent_id.clone(),
                    resolved_model: resolved_model.clone(),
                    usage: usage.clone(),
                    estimated_cost_pico_usd: None,
                },
            )
            .unwrap();
            if index == 0 {
                assert!(
                    log.append(
                        Some(run_id),
                        cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                        EventPayload::InternalAgentUsageRecorded {
                            internal_run_id,
                            kind,
                            agent_id: agent_id.clone(),
                            resolved_model: resolved_model.clone(),
                            usage: usage.clone(),
                            estimated_cost_pico_usd: None,
                        },
                    )
                    .is_err()
                );
                let failure = || InternalAgentFailure {
                    code: SafeCode::new("fallback").unwrap(),
                    message: SafeErrorMessage::new("test fallback").unwrap(),
                    retryable: true,
                    model_error: None,
                };
                log.append(
                    Some(run_id),
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                    EventPayload::InternalAgentFallback {
                        invocation_id,
                        internal_run_id,
                        kind,
                        from: InternalAgentBackend::Model {
                            resolved_model: resolved_model.clone(),
                        },
                        to: InternalAgentBackend::Model {
                            resolved_model: fallback_model.clone(),
                        },
                        failure: failure(),
                        attempts: 1,
                    },
                )
                .unwrap();
                log.append(
                    Some(run_id),
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                    EventPayload::InternalAgentUsageRecorded {
                        internal_run_id,
                        kind,
                        agent_id: agent_id.clone(),
                        resolved_model: fallback_model.clone(),
                        usage: usage.clone(),
                        estimated_cost_pico_usd: None,
                    },
                )
                .unwrap();
                log.append(
                    Some(run_id),
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                    EventPayload::InternalAgentFallback {
                        invocation_id,
                        internal_run_id,
                        kind,
                        from: InternalAgentBackend::Model {
                            resolved_model: fallback_model.clone(),
                        },
                        to: InternalAgentBackend::Model {
                            resolved_model: resolved_model.clone(),
                        },
                        failure: failure(),
                        attempts: 2,
                    },
                )
                .unwrap();
                log.append(
                    Some(run_id),
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                    EventPayload::InternalAgentUsageRecorded {
                        internal_run_id,
                        kind,
                        agent_id: agent_id.clone(),
                        resolved_model: resolved_model.clone(),
                        usage,
                        estimated_cost_pico_usd: None,
                    },
                )
                .unwrap();
            }
            log.append(
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::InternalAgentCompleted {
                    invocation_id,
                    internal_run_id,
                    kind,
                    result: SafeInternalAgentResult {
                        output_summary: SafeDisplayText::new("validated output").unwrap(),
                        output_digest: Sha256Digest::of_bytes(b"output"),
                    },
                },
            )
            .unwrap();
        }
        drop(log);

        let raw = fs::read_to_string(&path).unwrap();
        assert_eq!(
            raw.lines()
                .filter(|line| {
                    let value: serde_json::Value = serde_json::from_str(line).unwrap();
                    value["payload"]["type"] == "internal_agent_usage_recorded"
                        && value["payload"]["estimated_cost_pico_usd"].is_null()
                })
                .count(),
            5
        );
        let reopened = crate::events::EventLog::open(path, session_id).unwrap();
        let rebuilt = projection(reopened).unwrap();
        assert_eq!(rebuilt.usage_rollup.request_count, 5);
        assert_eq!(rebuilt.usage_rollup.input_tokens, 503);
        for (kind, agent_name) in kinds {
            assert_eq!(
                rebuilt.agent_usage[&AgentId::new(agent_name).unwrap()].request_count,
                if kind == InternalAgentKind::Approval {
                    3
                } else {
                    1
                }
            );
        }
        let rate = PicoUsdPerMillion::from_decimal_str("1").unwrap();
        let pricing = PricingConfig {
            models: BTreeMap::from([
                (
                    resolved_model.selection.model,
                    ModelPricing {
                        input_per_million_usd: Some(rate),
                        output_per_million_usd: Some(rate),
                        ..ModelPricing::default()
                    },
                ),
                (
                    fallback_model.selection.model,
                    ModelPricing {
                        input_per_million_usd: Some(rate),
                        output_per_million_usd: Some(rate),
                        ..ModelPricing::default()
                    },
                ),
            ]),
        };
        assert_eq!(
            crate::usage::with_pricing(rebuilt.usage_rollup, &pricing, &BTreeMap::new())
                .estimated_cost_usd,
            None
        );
    }

    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut value = self.0;
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            self.0 = value;
            value
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn fuzz_origin() -> cookie_agent_protocol::EventOrigin {
        cookie_agent_protocol::EventOrigin::new("engine:fuzz").expect("fuzz origin")
    }

    /// Extracts the run id, resolved model, agent id, prompt fingerprint, and a
    /// reusable RunStarted payload from a session created by
    /// `persist_test_session`.
    fn fuzz_scaffolding(
        store: &SessionStore,
        session_id: SessionId,
    ) -> (
        RunId,
        cookie_agent_protocol::ResolvedModelRef,
        AgentId,
        Sha256Digest,
        EventPayload,
    ) {
        let projection = store.get(session_id).expect("session projection");
        projection
            .log
            .event_snapshot()
            .iter()
            .find_map(|event| match &event.payload {
                EventPayload::RunStarted {
                    agent,
                    selected_suffix,
                    ..
                } => Some((
                    event.run_id.expect("run id"),
                    crate::model_history::wire_model(selected_suffix.first().expect("model")),
                    agent.agent.clone(),
                    agent.prompt_fingerprint.clone(),
                    event.payload.clone(),
                )),
                _ => None,
            })
            .expect("run started event")
    }

    fn fuzz_tool_owner(turn_seq: u64, label: &str) -> cookie_agent_protocol::AssistantToolCallRef {
        cookie_agent_protocol::AssistantToolCallRef {
            model_turn_seq: turn_seq,
            content_index: 0,
            model_call_id: cookie_agent_protocol::ModelCallId::new(label).expect("model call id"),
            provider_item_id: None,
        }
    }

    #[test]
    fn fold_ignored_appends_do_not_rebuild_projection() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let store = SessionStore::open(&temporary.path().join("data"), &cwd).unwrap();
        let session_id = persist_test_session(&store);
        let (run_id, resolved_model, _, prompt_fingerprint, _) =
            fuzz_scaffolding(&store, session_id);
        let attempt_id = AttemptId::new_v7();
        store
            .append(
                session_id,
                Some(run_id),
                fuzz_origin(),
                EventPayload::ModelAttemptStarted {
                    attempt_id,
                    attempt_ordinal: 1,
                    fallback_index: 0,
                    retry_ordinal: 0,
                    resolved_model,
                    prompt_fingerprint,
                },
            )
            .expect("start attempt");
        let before = super::projection_fold_count();
        for text in ["delta one", "delta two"] {
            store
                .append(
                    session_id,
                    Some(run_id),
                    fuzz_origin(),
                    EventPayload::TextDelta {
                        attempt_id,
                        text: text.into(),
                    },
                )
                .expect("text delta");
        }
        store
            .append(
                session_id,
                Some(run_id),
                fuzz_origin(),
                EventPayload::ReasoningDelta {
                    attempt_id,
                    text: "thinking".into(),
                },
            )
            .expect("reasoning delta");
        assert_eq!(
            super::projection_fold_count(),
            before,
            "fold-ignored appends must not rebuild the projection"
        );
        store
            .append(
                session_id,
                Some(run_id),
                fuzz_origin(),
                EventPayload::RunCompleted { final_text: None },
            )
            .expect("complete run");
        assert_eq!(
            super::projection_fold_count(),
            before + 1,
            "fold-consumed payloads rebuild exactly once"
        );
    }

    #[test]
    fn incremental_projection_matches_full_fold_across_random_event_streams() {
        for seed in [7_u64, 42, 0x5EED_5EED, 999_331] {
            run_projection_fuzz(seed, 400);
        }
    }

    /// Drives a pseudo-random event stream through a real SessionStore. The
    /// `append_with_mode` test assertion re-folds the log after every append
    /// and compares it against the resident projection, so each step is a
    /// differential check; this test additionally pins that full folds happen
    /// exactly on fold-consumed payloads.
    fn run_projection_fuzz(seed: u64, steps: usize) {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let store = SessionStore::open(&temporary.path().join("data"), &cwd).unwrap();
        let session_id = persist_test_session(&store);
        let (mut run_id, resolved_model, agent_id, prompt_fingerprint, run_started) =
            fuzz_scaffolding(&store, session_id);
        let mut attempt_id = AttemptId::new_v7();
        store
            .append(
                session_id,
                Some(run_id),
                fuzz_origin(),
                EventPayload::ModelAttemptStarted {
                    attempt_id,
                    attempt_ordinal: 1,
                    fallback_index: 0,
                    retry_ordinal: 0,
                    resolved_model: resolved_model.clone(),
                    prompt_fingerprint: prompt_fingerprint.clone(),
                },
            )
            .expect("start attempt");

        let mut rng = XorShift(seed | 1);
        let mut next_turn_seq = 1_u64;
        let mut next_attempt_ordinal = 2_u32;
        let mut callable_owners: Vec<cookie_agent_protocol::AssistantToolCallRef> = Vec::new();
        let mut open_tools: Vec<(ToolCallId, cookie_agent_protocol::AssistantToolCallRef)> =
            Vec::new();
        let mut consumed_appends = 0_u64;
        let folds_before = super::projection_fold_count();
        let append = |store: &SessionStore,
                      run: Option<RunId>,
                      payload: EventPayload,
                      consumed: &mut u64| {
            if super::fold_consumed(&payload) {
                *consumed += 1;
            }
            store
                .append(session_id, run, fuzz_origin(), payload)
                .expect("fuzz append");
        };

        for step in 0..steps {
            let roll = rng.below(100);
            match roll {
                // ~45%: streaming text deltas (fold-ignored).
                0..=44 => append(
                    &store,
                    Some(run_id),
                    EventPayload::TextDelta {
                        attempt_id,
                        text: format!("delta-{seed}-{step}"),
                    },
                    &mut consumed_appends,
                ),
                // ~15%: reasoning deltas (fold-ignored).
                45..=59 => append(
                    &store,
                    Some(run_id),
                    EventPayload::ReasoningDelta {
                        attempt_id,
                        text: format!("reasoning-{seed}-{step}"),
                    },
                    &mut consumed_appends,
                ),
                // ~15%: tool progress on an open call (fold-ignored).
                60..=74 => {
                    if let Some((tool_call_id, _)) = open_tools.first() {
                        append(
                            &store,
                            Some(run_id),
                            EventPayload::ToolCallProgress {
                                tool_call_id: *tool_call_id,
                                message: SafeDisplayText::new("progress").expect("progress"),
                                display: None,
                            },
                            &mut consumed_appends,
                        );
                    } else {
                        append(
                            &store,
                            Some(run_id),
                            EventPayload::TextDelta {
                                attempt_id,
                                text: format!("fallback-{seed}-{step}"),
                            },
                            &mut consumed_appends,
                        );
                    }
                }
                // ~5%: committed model turn carrying one tool-call part, plus
                // its usage record (consumed). The part gives later tool starts
                // a valid owner.
                75..=79 => {
                    let turn_seq = next_turn_seq;
                    next_turn_seq += 1;
                    let owner = fuzz_tool_owner(turn_seq, &format!("fuzz-mc-{turn_seq}"));
                    append(
                        &store,
                        Some(run_id),
                        EventPayload::ModelTurnCommitted {
                            attempt_id,
                            model_turn_seq: turn_seq,
                            resolved_model: resolved_model.clone(),
                            input_through_seq: 1,
                            turn: PersistedModelTurn {
                                content: vec![
                                    cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                                        id: owner.model_call_id.clone(),
                                        provider_item_id: None,
                                        name: SafeCode::new("fuzz_tool").expect("tool name"),
                                        input: serde_json::json!({}),
                                        raw_input: None,
                                        metadata: None,
                                    },
                                ],
                                provider_options: BTreeMap::new(),
                                finish_reason: ModelFinishReason::ToolCalls,
                                usage: Usage::default(),
                                response_metadata: BTreeMap::new(),
                                provider_metadata: BTreeMap::new(),
                                native_replay: None,
                            },
                            warnings: Vec::new(),
                        },
                        &mut consumed_appends,
                    );
                    callable_owners.push(owner);
                    append(
                        &store,
                        Some(run_id),
                        EventPayload::ModelUsageRecorded {
                            model_turn_seq: turn_seq,
                            agent_id: agent_id.clone(),
                            resolved_model: resolved_model.clone(),
                            usage: Usage::default(),
                            estimated_cost_pico_usd: None,
                        },
                        &mut consumed_appends,
                    );
                    // A committed turn is terminal for its attempt; stream the
                    // next deltas under a fresh attempt.
                    attempt_id = AttemptId::new_v7();
                    store
                        .append(
                            session_id,
                            Some(run_id),
                            fuzz_origin(),
                            EventPayload::ModelAttemptStarted {
                                attempt_id,
                                attempt_ordinal: next_attempt_ordinal,
                                fallback_index: 0,
                                retry_ordinal: 0,
                                resolved_model: resolved_model.clone(),
                                prompt_fingerprint: prompt_fingerprint.clone(),
                            },
                        )
                        .expect("start attempt");
                    next_attempt_ordinal += 1;
                }
                // ~5%: tool call start against a committed tool-call owner
                // (consumed).
                80..=84 => {
                    if let Some(owner) = callable_owners.pop() {
                        let tool_call_id = ToolCallId::new_v7();
                        append(
                            &store,
                            Some(run_id),
                            EventPayload::ToolCallStarted {
                                start: cookie_agent_protocol::ToolCallStart {
                                    output: Default::default(),
                                    tool_call_id,
                                    owner: owner.clone(),
                                    presentation: cookie_agent_protocol::ToolCallPresentation {
                                        title: SafeDisplayText::new("fuzz tool").expect("title"),
                                        primary_argument: None,
                                    },
                                    operation_fingerprint: serde_json::from_value(
                                        serde_json::json!({
                                            "digest": Sha256Digest::of_bytes(b"fuzz operation")
                                        }),
                                    )
                                    .expect("operation fingerprint"),
                                },
                            },
                            &mut consumed_appends,
                        );
                        open_tools.push((tool_call_id, owner));
                    } else {
                        append(
                            &store,
                            Some(run_id),
                            EventPayload::TextDelta {
                                attempt_id,
                                text: format!("pre-tool-{seed}-{step}"),
                            },
                            &mut consumed_appends,
                        );
                    }
                }
                // ~5%: tool call termination (consumed).
                85..=89 => {
                    if let Some((tool_call_id, owner)) = open_tools.pop() {
                        append(
                            &store,
                            Some(run_id),
                            EventPayload::ToolCallTerminated {
                                termination: cookie_agent_protocol::ToolCallTermination {
                                    tool_call_id,
                                    owner,
                                    outcome: cookie_agent_protocol::ToolTerminationOutcome::Failed,
                                    result: None,
                                    error: Some(cookie_agent_protocol::SafeToolError {
                                        code: SafeCode::new("fuzz_failed").expect("code"),
                                        message: SafeErrorMessage::new("fuzz failed")
                                            .expect("message"),
                                    }),
                                },
                            },
                            &mut consumed_appends,
                        );
                    } else {
                        append(
                            &store,
                            Some(run_id),
                            EventPayload::ReasoningDelta {
                                attempt_id,
                                text: format!("no-tool-{seed}-{step}"),
                            },
                            &mut consumed_appends,
                        );
                    }
                }
                // ~3%: titles, overlays, user input (all consumed).
                90..=92 => match rng.below(3) {
                    0 => append(
                        &store,
                        None,
                        EventPayload::SessionTitleCommitted {
                            change: SessionTitleChange::UserSet {
                                title: SessionTitle::new(format!("fuzz title {step}"))
                                    .expect("title"),
                                client_rename_id: cookie_agent_protocol::ClientRenameId::new(
                                    format!("fuzz-rename-{seed}-{step}"),
                                )
                                .expect("rename id"),
                            },
                            input_through_seq: 1,
                        },
                        &mut consumed_appends,
                    ),
                    1 => append(
                        &store,
                        None,
                        EventPayload::SessionPermissionOverlaySet {
                            overlay: SessionPermissionOverlay::default(),
                        },
                        &mut consumed_appends,
                    ),
                    _ => append(
                        &store,
                        Some(run_id),
                        EventPayload::UserInputSubmitted {
                            input: format!("follow-up {step}"),
                        },
                        &mut consumed_appends,
                    ),
                },
                // ~2%: complete the run and start a fresh one (consumed).
                // Tool owners are per-run state; model turn sequences are
                // session-global and stay contiguous across runs and reverts.
                93..=94 => {
                    append(
                        &store,
                        Some(run_id),
                        EventPayload::RunCompleted { final_text: None },
                        &mut consumed_appends,
                    );
                    run_id = RunId::new_v7();
                    attempt_id = AttemptId::new_v7();
                    next_attempt_ordinal = 2;
                    callable_owners.clear();
                    open_tools.clear();
                    append(
                        &store,
                        Some(run_id),
                        run_started.clone(),
                        &mut consumed_appends,
                    );
                    store
                        .append(
                            session_id,
                            Some(run_id),
                            fuzz_origin(),
                            EventPayload::ModelAttemptStarted {
                                attempt_id,
                                attempt_ordinal: 1,
                                fallback_index: 0,
                                retry_ordinal: 0,
                                resolved_model: resolved_model.clone(),
                                prompt_fingerprint: prompt_fingerprint.clone(),
                            },
                        )
                        .expect("start attempt");
                }
                // ~2%: revert to the creation event (consumed). Hidden turns
                // invalidate any owners committed before the revert.
                95..=96 => {
                    callable_owners.clear();
                    open_tools.clear();
                    append(
                        &store,
                        None,
                        EventPayload::SessionReverted { through_seq: 1 },
                        &mut consumed_appends,
                    );
                }
                // ~3%: user input on the current run (consumed).
                _ => append(
                    &store,
                    Some(run_id),
                    EventPayload::UserInputSubmitted {
                        input: format!("input {seed}-{step}"),
                    },
                    &mut consumed_appends,
                ),
            }
        }

        assert_eq!(
            super::projection_fold_count() - folds_before,
            consumed_appends,
            "seed {seed}: full folds happen exactly on fold-consumed payloads"
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use cookie_agent_protocol::{
        AgentMode, AgentRevision, CatalogRevision, ClientRunId, CwdIdentity, EventPayload,
        ModelRevision, ProviderStateRevision, RecipeRegistryRevision, RunId, RuntimeRevision,
        SessionId, SessionOrigin,
    };

    use crate::ownership::owner_lock_path;

    use super::{PROJECT_CWD_FILE, SessionStore};

    fn revision(label: char) -> String {
        format!("sha256:{}", label.to_string().repeat(64))
    }

    #[test]
    fn windows_session_store_applies_private_acls_before_use() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let cwd = temporary.path().join("workspace");
        std::fs::create_dir(&cwd).expect("workspace");
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).unwrap_or_else(|error| {
            panic!("Windows session store open failed for data={data:?}, cwd={cwd:?}: {error:?}")
        });
        let project = store.project_dir_path();
        for path in [
            project.to_owned(),
            project.join("sessions"),
            project.join(PROJECT_CWD_FILE),
        ] {
            cookie_agent_models::secure_store::verify_windows_private_creation(&path)
                .unwrap_or_else(|error| {
                    panic!("private ACL validation failed for {path:?}: {error:?}")
                });
        }
    }

    #[test]
    fn windows_session_store_uses_preexisting_untrusted_project_acl() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let cwd = temporary.path().join("workspace");
        std::fs::create_dir(&cwd).expect("workspace");
        let data = temporary.path().join("data");
        let project = SessionStore::project_dir(&data, &cwd);
        std::fs::create_dir_all(project.join("sessions")).expect("ordinary project");
        SessionStore::open(&data, &cwd).expect("ordinary existing project");
    }

    #[test]
    fn windows_buffered_session_persists_private_files_before_writing() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let cwd = temporary.path().join("workspace");
        std::fs::create_dir(&cwd).expect("workspace");
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).expect("session store");
        let session_id = SessionId::new_v7();
        let agent = crate::test_support::agent_snapshot("test", AgentMode::Primary);
        let selection = crate::test_support::run_selection("test");
        let binding = agent.fallback_chain[0].clone();
        let runtime_revision = RuntimeRevision::new(revision('1')).unwrap();
        let catalog_revision = CatalogRevision::new(revision('2')).unwrap();
        let provider_state_revision = ProviderStateRevision::new(revision('3')).unwrap();
        let model_revision = ModelRevision::new(revision('4')).unwrap();
        let agent_revision = AgentRevision::new(revision('5')).unwrap();
        let recipe_registry_revision = RecipeRegistryRevision::new(revision('6')).unwrap();
        store
            .create(
                session_id,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionCreated {
                    origin: SessionOrigin::Root,
                    cwd_identity: CwdIdentity::new("workspace:test").unwrap(),
                    creation_selection: selection.clone(),
                    creation_agent: Box::new(agent.clone()),
                    runtime_revision: runtime_revision.clone(),
                    catalog_revision: catalog_revision.clone(),
                    provider_state_revision: provider_state_revision.clone(),
                    model_revision: model_revision.clone(),
                    agent_revision: agent_revision.clone(),
                    recipe_registry_revision: recipe_registry_revision.clone(),
                    manifest_revision: binding.manifest_revision.clone(),
                },
            )
            .expect("buffered session");
        let run_id = RunId::new_v7();
        store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::RunStarted {
                    client_run_id: ClientRunId::new("windows-buffered-session").unwrap(),
                    selection,
                    agent: Box::new(agent),
                    runtime_revision,
                    catalog_revision,
                    provider_state_revision,
                    model_revision,
                    agent_revision,
                    recipe_registry_revision,
                    manifest_revision: binding.manifest_revision.clone(),
                    selected_suffix: vec![binding],
                    internal_agents: Vec::new(),
                    input_through_seq: 1,
                },
            )
            .expect("start buffered run");
        store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::UserInputSubmitted {
                    input: "persist me".into(),
                },
            )
            .expect("persist first input");

        let session_dir = store
            .project_dir_path()
            .join("sessions")
            .join(session_id.to_string());
        for path in [
            session_dir.clone(),
            session_dir.join("events.jsonl"),
            session_dir.join("meta.json"),
            owner_lock_path(&session_dir),
        ] {
            cookie_agent_models::secure_store::verify_windows_private_creation(&path)
                .unwrap_or_else(|error| {
                    panic!("private ACL validation failed for {path:?}: {error}")
                });
        }
    }
}
