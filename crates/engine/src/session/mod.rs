//! Session directories, projections, and rebuildable metadata caches.

mod cache;
mod fold;
mod tree_load;
mod workdir;

pub(crate) use cache::meta_path;
#[cfg(windows)]
pub(crate) use cache::replace_windows_path_with_retry;
use cache::*;
pub(crate) use fold::projection;
pub(crate) use fold::restart_stable_grant;
use fold::*;
pub(crate) use tree_load::LogFingerprint;
pub(crate) use tree_load::TreeLoadObserver;
pub(crate) use tree_load::TreeLoadProducts;
pub(crate) use tree_load::TreeState;
use tree_load::*;
#[cfg(unix)]
pub(crate) use workdir::create_unix_session_directory_all;
#[cfg(windows)]
pub(crate) use workdir::create_windows_session_directory;
use workdir::*;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
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
    AgentSnapshot, ChildSummary, ClientRenameId, ClientRunId, EventPayload,
    EventSubscriptionMessage, EventsSubscribeResult, RunId, RunSelection, SessionId, SessionMeta,
    SessionOrigin, SessionPermissionOverlay, SessionRenameRecord, SessionStatus, SessionTitle,
    SessionTitleChange, SessionTree, StoredEvent, ToolCallId, Usage, UsageRollup,
};
use thiserror::Error;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::events::{EventLog, EventLogError, fsync_directory};
use crate::ownership::{
    HeldLock, OWNER_LOCK_FILE, OWNER_LOCK_SUFFIX, SessionOwnership, WriteAuthority,
    WriteCapability, owner_lock_path, try_acquire,
};

pub(crate) const WORKDIR_CWD_FILE: &str = "cwd";
/// v2 session-store root under the data root (`~/.cookie-agent/sessions`).
pub(crate) const SESSIONS_ROOT_DIR: &str = "sessions";
/// Marker file recording the on-disk layout version of a work-dir store.
pub(crate) const LAYOUT_MARKER_FILE: &str = "layout.json";
/// Session metadata cache file name.
pub(crate) const SESSION_META_FILE: &str = "metadata";
/// Per-root directory holding delegated child sessions.
pub(crate) const SUBAGENTS_DIR: &str = "subagents";
/// Persisted child-summary cache inside [`SUBAGENTS_DIR`].
pub(crate) const SUBAGENT_INDEX_FILE: &str = "index.json";
/// Current `subagents/index.json` schema version.
const SUBAGENT_INDEX_VERSION: u32 = 1;
const PERSISTED_SUBSCRIBER_QUEUE_CAPACITY: usize = 256;
/// Event log file name.
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
    fn root_of(self) -> Option<SessionId> {
        match self {
            Self::Root => None,
            Self::Child { root } => Some(root),
        }
    }
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
    /// The tree of a root kept losing its read/install window to concurrent
    /// writers, or the load was retaken more times than the driver budget allows.
    #[error("tree load for session {0} keeps racing concurrent writers")]
    TreeContended(SessionId),
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

/// Ownership of one root session tree. Exactly one lock and one
/// [`WriteAuthority`] exist per tree; every log in the tree writes under that
/// authority, so dropping the tree's entry invalidates all of them at once.
#[derive(Debug)]
enum TreeOwnership {
    /// The root was created in this process and has not been published yet, so
    /// there is no directory to lock. Children may already be writing.
    PendingPublish { authority: WriteAuthority },
    /// The tree lock is held for an adoption that has not been committed yet.
    Adopting {
        _lock: HeldLock,
        authority: WriteAuthority,
    },
    Owned {
        _lock: HeldLock,
        authority: WriteAuthority,
    },
    /// Another process holds this tree's lock (or it could not be classified).
    Foreign,
}

impl TreeOwnership {
    /// The authority every log in the tree writes under, when this process
    /// holds the tree at all.
    fn authority(&self) -> Option<&WriteAuthority> {
        match self {
            Self::PendingPublish { authority }
            | Self::Adopting { authority, .. }
            | Self::Owned { authority, .. } => Some(authority),
            Self::Foreign => None,
        }
    }

    /// Whether the tree is held *and settled*: an uncommitted adoption is not
    /// yet a writable tree.
    fn is_settled(&self) -> bool {
        matches!(self, Self::PendingPublish { .. } | Self::Owned { .. })
    }
}

/// The store's ownership bookkeeping: one entry per root tree, plus the
/// per-session record of which sessions this process may write. A session is
/// writable only when its tree is held here *and* the session was created in
/// this process or adopted (reconciled and committed) in it.
#[derive(Debug, Default)]
struct OwnershipState {
    /// root session id -> the tree's ownership state.
    trees: HashMap<SessionId, TreeOwnership>,
    /// session id -> its root, for sessions created or adopted in this process.
    writable: HashMap<SessionId, SessionId>,
    /// session id -> its root, for adoptions that have not been committed.
    adopting: HashMap<SessionId, SessionId>,
}

impl OwnershipState {
    /// Capability for a write to `id`. `allow_adopting` admits a session whose
    /// adoption is still in flight — the reconciliation window, which is the
    /// only time an uncommitted session may append.
    fn capability(
        &self,
        id: SessionId,
        allow_adopting: bool,
    ) -> Result<WriteCapability, SessionError> {
        let root = match self.writable.get(&id) {
            Some(root) => *root,
            None if allow_adopting => *self
                .adopting
                .get(&id)
                .ok_or(SessionError::SessionLocked(id))?,
            None => return Err(SessionError::SessionLocked(id)),
        };
        let tree = self
            .trees
            .get(&root)
            .ok_or(SessionError::SessionLocked(id))?;
        if !allow_adopting && !tree.is_settled() {
            return Err(SessionError::SessionLocked(id));
        }
        tree.authority()
            .map(WriteAuthority::capability)
            .ok_or(SessionError::SessionLocked(id))
    }

    /// Whether any session still depends on the tree's lock. Used to decide
    /// whether a rolled-back adoption releases it.
    fn tree_is_referenced(&self, root: SessionId) -> bool {
        self.writable.values().any(|owner| *owner == root)
            || self.adopting.values().any(|owner| *owner == root)
    }
}

/// How a fork relates to the tree it publishes into.
#[derive(Debug)]
enum ForkTree {
    /// A forked root opens a tree of its own, locked at publication.
    NewRoot(WriteAuthority),
    /// A forked child joins its root's tree, carrying the lock when this fork
    /// is what took it.
    Joined(Option<(HeldLock, WriteAuthority)>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteOpen {
    AlreadyOwned,
    Adopting,
}

#[derive(Debug)]
pub struct SessionStore {
    data_root: PathBuf,
    /// The work-dir store: `sessions/<workdirkey>/`. Root session dirs live
    /// directly inside it, alongside the work-dir files (`cwd`, the layout
    /// marker, the grant journal, runtime revisions and artifacts).
    workdir_dir: PathBuf,
    cwd: PathBuf,
    /// id -> where it lives on disk. Roots are inserted at discovery; children
    /// lazily (tree load, index cache, or direct-address locate).
    locations: Mutex<HashMap<SessionId, SessionLocation>>,
    /// root id -> state of the lazy tree load.
    trees: Mutex<HashMap<SessionId, TreeState>>,
    /// Per-root load gates: who is driving the pass, and what waiters block on.
    tree_locks: Mutex<HashMap<SessionId, Arc<TreeGate>>>,
    /// Completed tree loads waiting for the engine hook that consumes them (D5).
    pending_loads: Mutex<PendingLoads>,
    residency: Mutex<SessionResidency>,
    /// Tree-scoped ownership: one lock per root, plus per-session writability.
    ownership: Mutex<OwnershipState>,
    /// Per-*tree* adoption gates, keyed by root: adopting two sessions of one
    /// tree concurrently would race on the single tree lock.
    adoption_locks: Mutex<HashMap<SessionId, Arc<Mutex<()>>>>,
    /// Serializes publication of one session directory, so the scaffold check in
    /// [`Self::publish_prepared_dir`] stays true until its entries have moved.
    publish_locks: Mutex<HashMap<SessionId, Arc<Mutex<()>>>>,
    mutation: Mutex<()>,
    subscribers: Mutex<HashMap<SessionId, Vec<mpsc::Sender<EventSubscriptionMessage>>>>,
    closed: AtomicBool,
    #[cfg(test)]
    eviction_transition_hook: Mutex<Option<EvictionTransitionHook>>,
    #[cfg(test)]
    publish_hook: Mutex<Option<PublishHook>>,
    /// Test-only: how many times each session's `events.jsonl` was opened and
    /// folded by this store (§8.2 #5). Counts log opens, not load attempts.
    #[cfg(test)]
    log_opens: Mutex<HashMap<SessionId, usize>>,
    /// Test-only: fires once at the end of a load read phase, before the fold is
    /// verified and installed, so a test can race a write into that window (L1).
    #[cfg(test)]
    tree_load_read_hook: TreeLoadReadHook,
}

impl SessionStore {
    /// The `<16-hex-hash>` component of the v2 work-dir key.
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
        let v2_dir = Self::resolve_workdir_dir(data_root, cwd);
        #[cfg(unix)]
        create_unix_session_directory_all(&v2_dir)?;
        #[cfg(windows)]
        for path in [data_root.join(SESSIONS_ROOT_DIR), v2_dir.clone()] {
            create_windows_session_directory(&path)?;
        }
        write_layout_marker_if_absent(&v2_dir)?;
        write_workdir_cwd(&v2_dir, cwd)?;
        let store = Arc::new(Self {
            data_root: data_root.to_owned(),
            workdir_dir: v2_dir,
            cwd: cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned()),
            locations: Mutex::new(HashMap::new()),
            trees: Mutex::new(HashMap::new()),
            tree_locks: Mutex::new(HashMap::new()),
            pending_loads: Mutex::new(PendingLoads::default()),
            residency: Mutex::new(SessionResidency::default()),
            ownership: Mutex::new(OwnershipState::default()),
            adoption_locks: Mutex::new(HashMap::new()),
            publish_locks: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
            subscribers: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            #[cfg(test)]
            eviction_transition_hook: Mutex::new(None),
            #[cfg(test)]
            publish_hook: Mutex::new(None),
            #[cfg(test)]
            log_opens: Mutex::new(HashMap::new()),
            #[cfg(test)]
            tree_load_read_hook: TreeLoadReadHook::default(),
        });
        store.refresh_discovered();
        Ok(store)
    }

    /// The data root this store was opened against.
    #[must_use]
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    /// Path of `id`'s session directory for a known placement.
    fn path_for(&self, location: SessionLocation, id: SessionId) -> PathBuf {
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

    /// Root of the tree `id` belongs to (`id` itself for a root session).
    /// Unknown children are located on disk first, so the answer is
    /// disk-accurate — it decides which `owner.lock` guards the session.
    pub(crate) fn root_of(&self, id: SessionId) -> Result<SessionId, SessionError> {
        self.resolve_dir(id)?;
        Ok(self
            .cached_location(id)
            .and_then(SessionLocation::root_of)
            .unwrap_or(id))
    }

    /// Directory whose `owner.lock` guards the tree rooted at `root`.
    fn tree_root_dir(&self, root: SessionId) -> PathBuf {
        self.path_for(SessionLocation::Root, root)
    }

    /// Resolves the on-disk directory of `id` by placement (§2.2).
    pub(crate) fn resolve_dir(&self, id: SessionId) -> Result<PathBuf, SessionError> {
        if let Some(location) = self.cached_location(id) {
            return Ok(self.path_for(location, id));
        }
        let root_dir = self.workdir_dir.join(id.to_string());
        if root_dir.is_dir() {
            self.record_location(id, SessionLocation::Root);
            return Ok(root_dir);
        }
        if let Some(root) = self.locate_child(id) {
            self.record_location(id, SessionLocation::Child { root });
            return Ok(self.path_for(SessionLocation::Child { root }, id));
        }
        Err(SessionError::Missing(id))
    }

    /// Direct-address locate for an unknown child: stat each root's
    /// `subagents/<id>/metadata`. O(#roots) stats, paid once per unknown child
    /// before `locations` caches the answer.
    fn locate_child(&self, id: SessionId) -> Option<SessionId> {
        for root in self.root_dir_ids() {
            let dir = self
                .workdir_dir
                .join(root.to_string())
                .join(SUBAGENTS_DIR)
                .join(id.to_string());
            if dir.join(SESSION_META_FILE).exists() {
                return Some(root);
            }
        }
        None
    }

    /// Session-id-named directories directly inside the work dir. These are
    /// roots *by construction*.
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

    /// Moves a fully prepared session directory into its published location.
    ///
    /// A root's directory can already exist as a placement scaffold: a child
    /// created while the root was still buffered is filed under `<root>/subagents/`
    /// which brings `<root>` into being before the root itself publishes. Such a
    /// scaffold holds *nothing but* that directory, so the prepared files are
    /// merged into it. Anything else present — another file, a foreign directory,
    /// or an empty directory that is not a scaffold at all — means the location is
    /// genuinely taken and the publish fails closed instead of replacing bytes
    /// another writer owns (D6, review L11).
    ///
    /// The whole decision runs under this session's publish gate and is
    /// revalidated there, so a second publisher of the same id cannot interleave
    /// between the check and the moves; a name that appeared in the destination in
    /// between is rejected rather than renamed over.
    fn publish_prepared_dir(
        &self,
        temporary: &Path,
        final_dir: &Path,
        session_id: SessionId,
    ) -> Result<(), SessionError> {
        let gate = self.publish_gate(session_id);
        let _publishing = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !final_dir.exists() {
            return fs::rename(temporary, final_dir).map_err(|source| SessionError::Io {
                path: final_dir.to_owned(),
                source,
            });
        }
        let existing = scaffold_listing(final_dir)?.unwrap_or_default();
        let merged_scaffold = match existing.as_slice() {
            [entry] => entry.file_name() == SUBAGENTS_DIR && entry.path().is_dir(),
            _ => false,
        };
        if !final_dir.is_dir() || !merged_scaffold {
            return Err(SessionError::SessionLocked(session_id));
        }
        let prepared = scaffold_listing(temporary)?.ok_or(SessionError::Io {
            path: temporary.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "prepared session directory vanished",
            ),
        })?;
        for entry in &prepared {
            let destination = final_dir.join(entry.file_name());
            // Never rename over a name another writer created in the meantime.
            if destination.symlink_metadata().is_ok() {
                return Err(SessionError::SessionLocked(session_id));
            }
            fs::rename(entry.path(), &destination).map_err(|source| SessionError::Io {
                path: destination,
                source,
            })?;
        }
        fs::remove_dir(temporary).map_err(|source| SessionError::Io {
            path: temporary.to_owned(),
            source,
        })?;
        // The entries that changed live in `final_dir` itself, so that is the
        // directory to sync before the session becomes visible: syncing the
        // parent again would prove nothing about the merged files.
        fsync_directory(final_dir)?;
        Ok(())
    }

    /// Process-serial gate for one session's publication.
    fn publish_gate(&self, session_id: SessionId) -> Arc<Mutex<()>> {
        let mut gates = self
            .publish_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            gates
                .entry(session_id)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
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
        // A new root opens its own tree, unlocked until it publishes. A child
        // joins its root's tree and writes under that tree's authority, so the
        // tree has to be held here before the child's log exists at all.
        let root = location.root_of().unwrap_or(session_id);
        let (capability, opened_tree) = match location {
            SessionLocation::Root => {
                let authority = WriteAuthority::new();
                (authority.capability(), Some(authority))
            }
            SessionLocation::Child { .. } => (self.tree_capability(root, session_id)?, None),
        };
        let log = EventLog::create_buffered_owned(
            final_dir.join(EVENTS_FILE),
            session_id,
            origin,
            creation,
            capability,
        )?;
        let result = projection(log.clone())?;
        {
            let mut ownership = self
                .ownership
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(authority) = opened_tree {
                ownership
                    .trees
                    .insert(root, TreeOwnership::PendingPublish { authority });
            }
            ownership.writable.insert(session_id, root);
        }
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
        // Completing the tree comes first, *before* the resident fast path: a
        // session created or adopted in this process is resident from a moment
        // when its root's products did not exist yet, and serving it from the
        // cache would hide the load that has to run (review L5).
        self.ensure_tree_for(id)?;
        if let Some(session) = self.get_resident(id) {
            return Ok(session);
        }
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
        self.note_log_open(id);
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
        self.note_log_open(id);
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
        let store = self.store_key();
        if MUTATION_DEPTH.with(|depths| depths.borrow().contains_key(&store)) {
            return MutationGuard {
                locked: None,
                store: None,
            };
        }
        let guard = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        MUTATION_DEPTH.with(|depths| depths.borrow_mut().insert(store, ()));
        MutationGuard {
            locked: Some(guard),
            store: Some(store),
        }
    }

    /// Identity of this store for the per-store reentrancy counter. Unique while
    /// the store is alive, which is exactly the window any of its guards span.
    #[must_use]
    fn store_key(&self) -> usize {
        std::ptr::from_ref(self).cast::<()>() as usize
    }

    #[must_use]
    fn mutation_held(&self) -> bool {
        let store = self.store_key();
        MUTATION_DEPTH.with(|depths| depths.borrow().contains_key(&store))
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

    /// Capability of a tree this process already holds and has settled, for a
    /// session that is about to write inside it. This is how a child joins its
    /// root's authority instead of minting one of its own.
    fn tree_capability(
        &self,
        root: SessionId,
        id: SessionId,
    ) -> Result<WriteCapability, SessionError> {
        let ownership = self
            .ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match ownership.trees.get(&root) {
            Some(tree) if tree.is_settled() => Ok(tree
                .authority()
                .expect("a settled tree has an authority")
                .capability()),
            _ => Err(SessionError::SessionLocked(id)),
        }
    }

    /// Makes sure this process holds the tree rooted at `root`, taking its lock
    /// when nobody holds it yet.
    ///
    /// Returns the capability every log in the tree writes under, plus the
    /// freshly taken lock when this call is the one that took it. The caller
    /// installs that lock only once the write it was taken for has actually
    /// succeeded, so a failure drops it and leaves the tree free.
    ///
    /// A cached `Foreign` classification is re-checked rather than believed: the
    /// owner may have exited since, exactly as the per-session classification
    /// was retried before.
    #[allow(clippy::type_complexity)]
    fn hold_tree(
        &self,
        root: SessionId,
        id: SessionId,
    ) -> Result<(WriteCapability, Option<(HeldLock, WriteAuthority)>), SessionError> {
        {
            let ownership = self
                .ownership
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match ownership.trees.get(&root) {
                Some(TreeOwnership::Foreign) | None => {}
                Some(tree) => {
                    return Ok((
                        tree.authority()
                            .expect("a held tree has an authority")
                            .capability(),
                        None,
                    ));
                }
            }
        }
        let root_dir = self.tree_root_dir(root);
        let lock = match try_acquire(&root_dir) {
            Ok(SessionOwnership::Owned(lock)) => lock,
            Ok(SessionOwnership::Foreign) => {
                self.mark_tree_foreign(root);
                return Err(SessionError::SessionLocked(id));
            }
            Err(error) => {
                eprintln!("session tree {root} ownership classification failed: {error}");
                self.mark_tree_foreign(root);
                return Err(SessionError::SessionLocked(id));
            }
        };
        let authority = WriteAuthority::new();
        Ok((authority.capability(), Some((lock, authority))))
    }

    /// Records that another process owns this tree. The record is per root, so
    /// it answers for every session in the tree at once: none of them is
    /// writable, and each reports [`SessionError::SessionLocked`]. A later
    /// acquisition still restats the lock, because the owner may have exited.
    fn mark_tree_foreign(&self, root: SessionId) {
        self.ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .trees
            .insert(root, TreeOwnership::Foreign);
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
        // Ownership is the tree's, so the lock that has to be free is the
        // root's — including when the session being adopted is a child. The
        // root itself is *not* reconciled here; it reconciles when it is first
        // written, through its own adoption.
        let root = self.root_of(id)?;
        let (capability, acquired) = self.hold_tree(root, id)?;
        let opened = EventLog::open_owned(session_dir.join(EVENTS_FILE), id, capability);
        let projection = match opened.and_then(|log| {
            projection(log).map_err(|error| match error {
                SessionError::Event(error) => error,
                _ => unreachable!("projection only returns event-log errors"),
            })
        }) {
            Ok(projection) => projection,
            Err(error) => {
                // `acquired` drops here: a tree this call locked is released
                // again, so the failed adoption leaves nothing behind.
                eprintln!("session {id} adoption failed closed: {error}");
                return Err(SessionError::SessionLocked(id));
            }
        };
        {
            let mut ownership = self
                .ownership
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some((lock, authority)) = acquired {
                ownership.trees.insert(
                    root,
                    TreeOwnership::Adopting {
                        _lock: lock,
                        authority,
                    },
                );
            }
            ownership.adopting.insert(id, root);
        }
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
        let root = *ownership
            .adopting
            .get(&id)
            .ok_or(SessionError::SessionLocked(id))?;
        let tree = ownership
            .trees
            .remove(&root)
            .ok_or(SessionError::SessionLocked(id))?;
        match tree {
            // The first committed adoption settles the tree; later ones join a
            // tree that is already owned or still pending its root's publish.
            TreeOwnership::Adopting { _lock, authority } => {
                ownership
                    .trees
                    .insert(root, TreeOwnership::Owned { _lock, authority });
            }
            TreeOwnership::Foreign => {
                ownership.trees.insert(root, TreeOwnership::Foreign);
                return Err(SessionError::SessionLocked(id));
            }
            settled => {
                ownership.trees.insert(root, settled);
            }
        }
        ownership.adopting.remove(&id);
        ownership.writable.insert(id, root);
        Ok(())
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
        let mut ownership = self
            .ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let removed = ownership.adopting.remove(&id);
        // A tree locked *for* this adoption and never committed by anything
        // else goes back to unowned, so another process can adopt it.
        if let Some(root) = removed
            && matches!(
                ownership.trees.get(&root),
                Some(TreeOwnership::Adopting { .. })
            )
            && !ownership.tree_is_referenced(root)
        {
            ownership.trees.remove(&root);
        }
        debug_assert!(removed.is_some() || self.closed.load(Ordering::Acquire));
    }

    /// Adoption gate for `id`'s *tree*. One tree lock means one adoption at a
    /// time in a tree; an id that cannot be located keeps a gate of its own,
    /// because its `begin_write` fails before it reaches any lock.
    pub(crate) fn adoption_lock(&self, id: SessionId) -> Arc<Mutex<()>> {
        let root = self.root_of(id).unwrap_or(id);
        self.adoption_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(root)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn write_capability(
        &self,
        id: SessionId,
        allow_adopting: bool,
    ) -> Result<WriteCapability, SessionError> {
        self.ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .capability(id, allow_adopting)
    }

    /// Whether this process may write `id`: its tree is held and settled here,
    /// and `id` itself was created or adopted in this process. A foreign root
    /// therefore answers `false` for every session in its tree.
    #[must_use]
    pub fn is_owned(&self, id: SessionId) -> bool {
        let ownership = self
            .ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ownership.writable.get(&id).is_some_and(|root| {
            ownership
                .trees
                .get(root)
                .is_some_and(TreeOwnership::is_settled)
        })
    }

    pub fn evict(&self, id: SessionId) -> Result<bool, SessionError> {
        let _mutation = self.lock_mutation();
        // Persisted child caches are refreshed after the residency guard drops.
        let parent_root = self.parent_root_of(id);
        let evicted = self.evict_locked(id)?;
        if evicted && let Some(root) = parent_root {
            self.persist_subagent_index(root)?;
        }
        Ok(evicted)
    }

    /// The root a session is filed under, from the location cache only (never
    /// resolves paths, so it is safe to call while store locks are held).
    fn parent_root_of(&self, id: SessionId) -> Option<SessionId> {
        match self.cached_location(id) {
            Some(SessionLocation::Child { root }) => Some(root),
            Some(SessionLocation::Root) | None => match self.cached_origin(id) {
                Some(SessionOrigin::Delegated {
                    root_session_id, ..
                }) => Some(root_session_id),
                _ => None,
            },
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
        // §3.2: the tree must be complete before a session writes into it, and
        // the load cannot run under `mutation` (it installs its own), so it runs
        // here instead. Cheap once the tree is Loaded.
        self.ensure_tree_for(id)?;
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
                self.persist_subagent_index(root)?;
            }
        }
        // Run-terminal events are rare and are the only thing the delegation
        // registry needs from a child log, so cache them per tree (§4.1.3).
        if let Some(terminal) = terminal_run_of(run, &envelope.payload) {
            self.record_terminal_run(id, terminal.0, terminal.1)?;
        }
        #[cfg(test)]
        {
            let reference = projection_fold(log).expect("reference fold");
            let resident = self.get_resident(id).expect("resident after append");
            assert_projection_equivalent(&resident, &reference);
        }
        self.publish_stored_event(&envelope);
        Ok(envelope)
    }

    pub(crate) fn subscribe_events(
        &self,
        session: SessionId,
        cursor: Option<u64>,
    ) -> Result<
        (
            EventsSubscribeResult,
            mpsc::Receiver<EventSubscriptionMessage>,
        ),
        SessionError,
    > {
        self.ensure_tree_for(session)?;
        // Snapshot and registration share the append lock with actor writes and
        // direct journal writes, closing the snapshot-to-live handoff gap.
        let _mutation = self.lock_mutation();
        let events = self
            .get(session)?
            .log
            .all_events()
            .into_iter()
            .filter(|event| cursor.is_none_or(|cursor| event.seq > cursor))
            .collect();
        let (sender, receiver) = mpsc::channel(PERSISTED_SUBSCRIBER_QUEUE_CAPACITY);
        self.subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(session)
            .or_default()
            .push(sender);
        Ok((EventsSubscribeResult { events }, receiver))
    }

    fn publish_stored_event(&self, envelope: &StoredEvent) {
        // Called under mutation after projection update, preserving append order.
        self.subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(envelope.session_id)
            .or_default()
            .retain(|sender| {
                // Reserve the final slot for a gap so a slow reader can replay.
                let is_gap = sender.capacity() <= 1;
                let message = if is_gap {
                    EventSubscriptionMessage::Gap {
                        session_id: envelope.session_id,
                        last_delivered_seq: envelope.seq.saturating_sub(1),
                    }
                } else {
                    EventSubscriptionMessage::Event {
                        event: Box::new(envelope.clone()),
                    }
                };
                sender.try_send(message).is_ok() && !is_gap
            });
    }

    pub(crate) fn notify_evicted_subscribers(&self, session_id: SessionId, last_event_seq: u64) {
        let subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&session_id)
            .unwrap_or_default();
        for sender in subscribers {
            // publish_stored_event always leaves a slot for this final gap.
            let _ = sender.try_send(EventSubscriptionMessage::Gap {
                session_id,
                last_delivered_seq: last_event_seq,
            });
        }
    }

    pub fn fork(
        &self,
        source_id: SessionId,
        through_seq: u64,
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<SessionId, SessionError> {
        // A fork reads its source's tree and files the new session into it, so
        // the tree has to be complete *before* `mutation` is taken: a load
        // installs under that lock and must never start while it is held.
        // Forking a cold child otherwise files it into an unflattened tree.
        if self.tree_load_pending_for(source_id) {
            self.ensure_tree_for(source_id)?;
        }
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
        let root = location.root_of().unwrap_or(session_id);
        // A forked root opens a tree of its own, locked when it publishes. A
        // forked child is prepared and published *inside* its root's directory,
        // so the root's lock has to be this process's before a byte is written
        // there — including the temporary directory below. Nobody holding it is
        // the cold-store fork: take it now, the way an adoption through a child
        // would.
        let (capability, fork_tree) = match location {
            SessionLocation::Root => {
                let authority = WriteAuthority::new();
                (authority.capability(), ForkTree::NewRoot(authority))
            }
            SessionLocation::Child { .. } => {
                let (capability, acquired) = self.hold_tree(root, session_id)?;
                (capability, ForkTree::Joined(acquired))
            }
        };
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
            write_cache(&temporary.join(SESSION_META_FILE), &fork_projection.meta)?;
            // Only a root opens a tree, and only a root takes a lock: on unix
            // inside the temporary directory that is about to become the root,
            // on Windows from the sidecar path derived from the final one. A
            // forked child publishes into a tree this process already holds.
            #[cfg(unix)]
            let lock_root_dir = &temporary;
            #[cfg(windows)]
            let lock_root_dir = &final_dir;
            let published_tree = match fork_tree {
                ForkTree::NewRoot(authority) => {
                    let lock =
                        match try_acquire(lock_root_dir).map_err(|source| SessionError::Io {
                            path: owner_lock_path(lock_root_dir),
                            source,
                        })? {
                            SessionOwnership::Owned(lock) => lock,
                            SessionOwnership::Foreign => {
                                return Err(SessionError::SessionLocked(session_id));
                            }
                        };
                    Some(TreeOwnership::Owned {
                        _lock: lock,
                        authority,
                    })
                }
                ForkTree::Joined(Some((lock, authority))) => Some(TreeOwnership::Owned {
                    _lock: lock,
                    authority,
                }),
                ForkTree::Joined(None) => None,
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
            self.publish_prepared_dir(&temporary, &final_dir, session_id)?;
            fsync_directory(&destination_parent)?;
            {
                let mut ownership = self
                    .ownership
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match published_tree {
                    Some(tree) => {
                        ownership.trees.insert(root, tree);
                    }
                    None => match ownership.trees.get(&root) {
                        Some(tree) if tree.is_settled() => {}
                        _ => return Err(SessionError::SessionLocked(session_id)),
                    },
                }
                ownership.writable.insert(session_id, root);
            }
            let log = EventLog::open_owned(final_dir.join("events.jsonl"), session_id, capability)?;
            let fork_projection = projection(log)?;
            let fork_origin = fork_projection.meta.origin.clone();
            self.residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .resident
                .insert(session_id, fork_projection);
            self.note_placed_child(&location, &fork_origin, session_id);
            if let SessionLocation::Child { root } = location {
                self.persist_subagent_index(root)?;
            }
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
        // Only a root's publication takes a lock; a child publishes into a tree
        // this process already holds, which `write_capability` just proved.
        let root = location.root_of().unwrap_or(session_id);
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
            write_cache(&temporary.join(SESSION_META_FILE), &projection.meta)?;
            #[cfg(unix)]
            let lock_root_dir = &temporary;
            #[cfg(windows)]
            let lock_root_dir = &final_dir;
            let lock = match location {
                SessionLocation::Root => {
                    match try_acquire(lock_root_dir).map_err(|source| SessionError::Io {
                        path: owner_lock_path(lock_root_dir),
                        source,
                    })? {
                        SessionOwnership::Owned(lock) => Some(lock),
                        SessionOwnership::Foreign => {
                            return Err(SessionError::SessionLocked(session_id));
                        }
                    }
                }
                SessionLocation::Child { .. } => None,
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
            self.publish_prepared_dir(&temporary, &final_dir, session_id)?;
            fsync_directory(&destination_parent)?;
            let mut ownership = self
                .ownership
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match lock {
                // The root's own publication settles its tree, carrying the
                // authority its children are already writing under.
                Some(lock) => {
                    let state = ownership
                        .trees
                        .remove(&root)
                        .ok_or(SessionError::SessionLocked(session_id))?;
                    match state {
                        TreeOwnership::PendingPublish { authority } => {
                            ownership.trees.insert(
                                root,
                                TreeOwnership::Owned {
                                    _lock: lock,
                                    authority,
                                },
                            );
                        }
                        state => {
                            ownership.trees.insert(root, state);
                            return Err(SessionError::SessionLocked(session_id));
                        }
                    }
                }
                None => match ownership.trees.get(&root) {
                    Some(tree) if tree.is_settled() => {}
                    _ => return Err(SessionError::SessionLocked(session_id)),
                },
            }
            Ok::<(), SessionError>(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
            return result;
        }
        projection.log.mark_persisted();
        // The child directory is durable now, so the cache may name it (§3.4).
        if let SessionLocation::Child { root } = location {
            self.persist_subagent_index(root)?;
        }
        Ok(())
    }

    pub(crate) fn persist_buffered_session(&self, id: SessionId) -> Result<(), SessionError> {
        // Publishing a buffered session makes it a member of its tree, so the
        // tree has to be flat before `mutation` is taken — a cold root would
        // otherwise stay unflattened with a child filed into it, and a load
        // cannot run under the lock its install phase needs (§3.2).
        if self.tree_load_pending_for(id) {
            self.ensure_tree_for(id)?;
        }
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

    /// Snapshots of the sessions that are roots of their own tree. This is the
    /// startup pass shape: never touches delegated child logs and never triggers
    /// a lazy tree load (§3.2, §4).
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

        self.startup_snapshots_for(ids)
    }

    /// Startup bookkeeping reads only: root summaries, the delegation registry,
    /// the approval rebuild and the producer install (§4). A child read here
    /// would land outside the coalesced bulk pass, which is why the use-path
    /// APIs go through [`Self::ensure_tree_for`] instead.
    fn startup_snapshots_for(&self, ids: Vec<SessionId>) -> Vec<SessionProjection> {
        ids.into_iter()
            .filter_map(|id| match self.startup_snapshot(id) {
                Ok(session) => Some(session),
                Err(error) => {
                    eprintln!("session {id} snapshot skipped: {error}");
                    None
                }
            })
            .collect()
    }

    /// Reads one session *without* completing its tree.
    ///
    /// Legal for startup bookkeeping and for direct-address access the caller
    /// has already proven; anything else wants [`Self::get`], which refuses to
    /// serve a session whose tree is still incomplete.
    fn startup_snapshot(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        if let Some(session) = self.get_resident(id) {
            return Ok(session);
        }
        self.open_snapshot(id, false)
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

    fn is_root_id(&self, id: SessionId) -> bool {
        !matches!(
            self.cached_location(id),
            Some(SessionLocation::Child { .. })
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
                },
            )
        }));
        summaries.into_values().collect()
    }

    /// One session's summary: a resident projection, a cached summary the store
    /// can prove is current, or — after completing the owning root's tree — the
    /// answer the bulk pass installed (§2.4, §3.2).
    ///
    /// A summary that cannot be proven current is *pre-load* data: the persisted
    /// `subagents/index.json` is a cache, and the one thing that proves a cached
    /// entry has fallen behind is a disagreement with the session's own
    /// authoritative `metadata` file. Serving it anyway is how a usage query ends
    /// up reporting an incomplete tree (`session_usage` and
    /// `Engine::session_tree_usage` both answer from this method), so the tree is
    /// completed first. The hot path stays lock-light: an already-loaded tree
    /// costs one placement probe and one tree-state probe, no filesystem access,
    /// and a caller holding `mutation` never starts a load here (§3.2, review F2).
    pub fn summary(&self, id: SessionId) -> Result<SessionSummary, SessionError> {
        if self.summary_cache_is_current(id)
            && let Some(summary) = self.cached_summary(id)
        {
            return Ok(summary);
        }
        self.ensure_tree_for(id)?;
        if self.summary_cache_is_current(id)
            && let Some(summary) = self.cached_summary(id)
        {
            return Ok(summary);
        }
        self.get(id)
            .map(|session| summary_from_projection(&session))
    }

    /// Whether the in-memory summary of `id` may be served *without* completing
    /// its tree: either the pass has run, so the cache holds what it installed, or
    /// the seed it holds still agrees with the session's authoritative `metadata`
    /// cache (§2.3, §3.4).
    fn summary_cache_is_current(&self, id: SessionId) -> bool {
        let root = match self.cached_location(id) {
            Some(SessionLocation::Child { root }) => root,
            // A root — or a session whose placement this store has not resolved
            // yet, in which case the tree in question can only be its own.
            Some(SessionLocation::Root) | None => id,
        };
        let trees = self
            .trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match trees.get(&root) {
            // Nothing was ever recorded about this tree, so nothing cached in it
            // has been proven against a log.
            None => false,
            Some(state) => state.loaded || !state.stale_seeds.contains(&id),
        }
    }

    /// Root discovery refresh. Metadata caches only — never an event log.
    fn refresh_discovered(&self) {
        self.refresh_discovered_roots();
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
        for root in roots.iter().copied() {
            let dir = self.workdir_dir.join(root.to_string());
            if !self.is_known(root) {
                // Invalid entries stay uncached so later discovery retries them
                // and repeats the diagnostic (today's semantics).
                match read_cache(&meta_path(&dir), &dir.join(EVENTS_FILE)) {
                    Ok(meta) if meta.session_id == root => {
                        // Root logs are part of startup discovery. Folding them
                        // here initializes usage while retaining the root-only
                        // invariant: child logs are never opened at startup.
                        match self.startup_snapshot(root) {
                            Ok(snapshot) => {
                                self.cache_summary(root, summary_from_projection(&snapshot))
                            }
                            Err(error) => eprintln!("session {root} usage skipped: {error}"),
                        }
                    }
                    Ok(_) => eprintln!("session {root} metadata ID does not match its directory"),
                    // A root directory without a `metadata` cache is a transient
                    // discovery state, not a fault. It is what discovery sees
                    // while a bare `<root>/subagents/` scaffold waits for its
                    // root to publish, while `publish_prepared_dir` merges a
                    // prepared directory into that scaffold, and, on filesystems
                    // without an atomic superseding replace, while the cache
                    // itself is rewritten. Skipping silently leaves the root
                    // uncached, so the next discovery retries it.
                    Err(SessionError::Io { source, .. })
                        if source.kind() == std::io::ErrorKind::NotFound =>
                    {
                        continue;
                    }
                    Err(error) => {
                        eprintln!("session {root} metadata skipped: {error}");
                        continue;
                    }
                }
            }
            self.record_location(root, SessionLocation::Root);
        }
        // Only once every root's placement is known: an `index.json` naming
        // another root must be rejected as the foreign entry it is (review L9).
        for root in roots {
            self.seed_tree_from_index(root);
        }
    }

    /// Where the descendants of a *root* `id` live (one flat level, whatever the
    /// depth). Descendants are always filed under the root's own directory, so
    /// this needs no placement lookup.
    fn subagents_dir(&self, root: SessionId) -> PathBuf {
        self.path_for(SessionLocation::Root, root)
            .join(SUBAGENTS_DIR)
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

    /// Files a newly created session under its root: tree edge plus placement,
    /// in memory only. The durable `index.json` rewrite waits until the child
    /// directory is actually published (`persist_buffered`, `fork`), because an
    /// index naming a session that was never published is exactly the stale entry
    /// startup then has to reject (review L10, §3.4).
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

    /// Sweeps the per-child ownership locks an older build wrote.
    ///
    /// Ownership is the tree's, so the only lock this build writes or reads is
    /// the root's. Anything matching a legacy child layout directly under
    /// `subagents/` — `<child>/owner.lock` on unix, `<child-id>.owner.lock` as
    /// a Windows sidecar — is dead weight. The sweep is best-effort: a file
    /// another process still holds open simply stays, and is never consulted.
    pub(super) fn remove_legacy_child_locks(&self, root: SessionId) {
        let Ok(entries) = fs::read_dir(self.subagents_dir(root)) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir() {
                if name.parse::<SessionId>().is_ok() {
                    let _ = fs::remove_file(entry.path().join(OWNER_LOCK_FILE));
                }
            } else if name
                .strip_suffix(OWNER_LOCK_SUFFIX)
                .is_some_and(|id| id.parse::<SessionId>().is_ok())
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Residency-only summary lookup (never opens a log, never completes a tree).
    ///
    /// This is the store's *non-triggering* summary read: `children`, the durable
    /// index rewrite and the listings want the cache exactly as it stands. A
    /// caller serving a session has to go through [`Self::summary`], which proves
    /// the cache current first (§3.2).
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

    /// Whether `id`'s child directory under `root` exists, i.e. the session has
    /// been published. A buffered child is in memory only and must stay out of
    /// the durable cache.
    fn is_published_child(&self, root: SessionId, id: SessionId) -> bool {
        self.path_for(SessionLocation::Child { root }, id).is_dir()
    }

    /// Rebuilds and atomically rewrites a root's child-summary cache. Whole-file
    /// rewrite on purpose (§3.4). Required refreshes are part of the durability
    /// contract, so callers must handle failures.
    fn persist_subagent_index(&self, root: SessionId) -> Result<(), SessionError> {
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
                // Only a published child may be named: the cache is rewritten
                // from inside tree loads and appends, and an entry for a session
                // whose directory does not exist yet is exactly the crash-window
                // staleness startup then has to reject (§3.4, review L10).
                if !self.is_published_child(root, id) {
                    return None;
                }
                let summary = self.cached_summary(id)?;
                Some(IndexedChild {
                    summary,
                    terminal_runs: cached.get(&id).cloned().unwrap_or_default(),
                })
            })
            .collect::<Vec<_>>();
        let path = self.subagent_index_path(root);
        if children.is_empty() && !path.is_file() {
            return Ok(());
        }
        let index = SubagentIndex {
            version: SUBAGENT_INDEX_VERSION,
            children,
        };
        write_index_json(&path, &index)
    }

    /// Remembers a terminal run status for the delegation registry cache, then
    /// refreshes the owning root's index.
    fn record_terminal_run(
        &self,
        id: SessionId,
        run_id: RunId,
        status: SessionStatus,
    ) -> Result<(), SessionError> {
        let Ok(root) = self.root_of(id) else {
            return Ok(());
        };
        if root == id {
            return Ok(());
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
            self.persist_subagent_index(root)?;
        }
        Ok(())
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
        // Terminal run records are the source of truth once the tree is complete
        // and every run has closed (§3.4.1, review L14); before that the durable
        // store simply has no answer, and the fold's stamps are reported so the
        // number never reads lower than the transcript footer.
        //
        // `summary` is what makes that "once" real: it completes the owning root's
        // tree before serving, so a cost question about an unloaded tree is
        // answered from the bulk pass instead of from the pre-load cache (§3.2).
        let mut usage = self.summary(id)?.usage_rollup;
        if usage.request_count == 0 {
            // A summary that arrived without usage — a session whose metadata cache
            // had to be rebuilt, or one that was seeded from discovery alone — is
            // not an answer to "what did this session cost": fold its log, which is
            // the same stamped accounting the transcript footer shows.
            usage = summary_from_projection(&self.get(id)?).usage_rollup;
        }
        Ok(cookie_agent_protocol::SessionUsageResult {
            session_id: id,
            usage: crate::usage::with_pricing(usage, pricing, catalog),
        })
    }

    /// Counts one `events.jsonl` open+fold of `id` (§8.2 #5 counts reads, not
    /// load attempts, so a hidden second fold cannot hide behind a cached flag).
    fn note_log_open(&self, id: SessionId) {
        #[cfg(test)]
        {
            *self
                .log_opens
                .lock()
                .expect("log open counter lock poisoned")
                .entry(id)
                .or_insert(0) += 1;
        }
        #[cfg(not(test))]
        let _ = id;
    }

    /// How many times this store opened and folded `id`'s `events.jsonl`.
    #[cfg(test)]
    pub(crate) fn log_open_count(&self, id: SessionId) -> usize {
        *self
            .log_opens
            .lock()
            .expect("log open counter lock poisoned")
            .get(&id)
            .unwrap_or(&0)
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

    /// Directory holding the work-dir files (artifacts, the grant journal,
    /// runtime revisions, `cwd` and `layout.json`).
    #[must_use]
    pub fn workdir_dir_path(&self) -> &Path {
        &self.workdir_dir
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Best-effort directory for `id`, *without* locating anything: an unknown id
    /// is guessed as `workdir_dir/<id>`, which is structurally wrong for a v2
    /// child. Production paths must use [`Self::resolve_dir`]; this stays
    /// available to tests, which know where they put their own fixtures.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn session_dir(&self, id: SessionId) -> PathBuf {
        self.cached_location(id)
            .map(|location| self.path_for(location, id))
            .unwrap_or_else(|| self.workdir_dir.join(id.to_string()))
    }

    /// Metadata cache path to *read*.
    pub(crate) fn meta_cache_path(&self, id: SessionId) -> Result<PathBuf, SessionError> {
        Ok(meta_path(&self.resolve_dir(id)?))
    }

    pub fn is_persisted(&self, id: SessionId) -> Result<bool, SessionError> {
        Ok(self.get(id)?.log.is_persisted())
    }

    /// Every direct child of `parent`, resolved from *placement* rather than by
    /// scanning every resident projection: seeded tree state + location cache,
    /// then a `subagents/` directory scan that adopts children no cache
    /// mentions from their `metadata` cache alone (§3.4). No child event log is
    /// read here — children of a cold tree report their persisted status.
    ///
    /// Fails when the tree itself could not be completed: a listing built from
    /// the pre-load cache would silently report a stale tree, so the caller gets
    /// the load error instead (review L14). A missing or stale `index.json` is
    /// not such a failure — the directory scan recovers it (§3.4).
    pub fn children(&self, parent: SessionId) -> Result<Vec<ChildSummary>, SessionError> {
        // Listing a tree is a use of it: make sure it is complete first (§3.2.2).
        self.ensure_tree_for(parent)?;
        Ok(self
            .child_ids(parent)?
            .into_iter()
            .filter_map(|id| self.child_summary(id))
            .collect())
    }

    /// Direct child ids of `parent`, resolved from placement: seeded tree state
    /// and location cache first, then a scan of the tree directory that adopts
    /// filed children no cache mentions from their `metadata` alone.
    fn child_ids(&self, parent: SessionId) -> Result<Vec<SessionId>, SessionError> {
        self.refresh_discovered();
        let root = match self.cached_location(parent) {
            Some(SessionLocation::Child { root }) => root,
            _ => parent,
        };
        let mut children = self.direct_children(root, parent);
        for id in self.child_dir_ids(root) {
            if children.contains(&id) {
                continue;
            }
            if self.adopt_filed_child(root, id)? == Some(parent) {
                children.push(id);
            }
        }
        children.sort_by_key(|id| id.to_string());
        Ok(children)
    }

    /// Whether `id` is a root of this store — by cached placement or by the
    /// top-level directory it lives in. A root can never be another tree's child,
    /// and trusting an index entry that says otherwise would give a real root a
    /// wrong-layout path (review L9).
    fn is_indexed_root(&self, id: SessionId) -> bool {
        matches!(self.cached_location(id), Some(SessionLocation::Root))
            || self.workdir_dir.join(id.to_string()).is_dir()
    }

    /// Checks one `index.json` entry against the directory it claims: the child
    /// directory must exist, must not be a root of this store, and its own
    /// metadata cache must parse, name that directory, and place it inside
    /// `root`. `None` means "ignore the entry" — never an error (§3.4).
    fn validated_index_child(&self, root: SessionId, id: SessionId) -> Option<SessionMeta> {
        let dir = self.path_for(SessionLocation::Child { root }, id);
        if !dir.is_dir() {
            eprintln!("subagent index for {root} names {id}, which has no directory");
            return None;
        }
        if self.workdir_dir.join(id.to_string()).is_dir() {
            eprintln!("subagent index for {root} names root session {id}; ignored");
            return None;
        }
        let Ok(meta) = read_cache(&meta_path(&dir), &dir.join(EVENTS_FILE)) else {
            eprintln!("subagent index for {root}: child {id} metadata is unreadable");
            return None;
        };
        if meta.session_id != id {
            eprintln!("session {id} metadata ID does not match its directory");
            return None;
        }
        match meta.origin {
            SessionOrigin::Delegated {
                root_session_id, ..
            } if root_session_id == root => {}
            _ => {
                eprintln!("subagent index for {root} names {id}, which is not in its tree");
                return None;
            }
        }
        Some(meta)
    }

    /// Adopts a child that is present on disk but missing from the caches (e.g.
    /// a crash between the directory publish and the `index.json` refresh), and
    /// reports the parent its origin names.
    fn adopt_filed_child(
        &self,
        root: SessionId,
        id: SessionId,
    ) -> Result<Option<SessionId>, SessionError> {
        let dir = self.path_for(SessionLocation::Child { root }, id);
        let Ok(meta) = read_cache(&meta_path(&dir), &dir.join(EVENTS_FILE)) else {
            return Ok(None);
        };
        if meta.session_id != id {
            eprintln!("session {id} metadata ID does not match its directory");
            return Ok(None);
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
            },
        );
        self.record_child_edge(root, parent, id);
        self.persist_subagent_index(root)?;
        Ok(Some(parent))
    }

    /// Metadata for a tree member without rebuilding its log.
    fn summary_meta(&self, id: SessionId) -> Result<SessionMeta, SessionError> {
        if let Some(session) = self.get_resident(id) {
            return Ok(session.meta);
        }
        if let Some(summary) = self.cached_summary(id) {
            return Ok(summary.meta);
        }
        // Tree assembly must never page a child log in implicitly.
        Err(SessionError::Missing(id))
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
        let mut queue = self.child_ids(id)?;
        if !queue.is_empty() {
            children.insert(id, queue.clone());
        }
        while let Some(current) = queue.pop() {
            if metadata.contains_key(&current) {
                continue;
            }
            metadata.insert(current, self.summary_meta(current)?);
            let descendants = self.child_ids(current)?;
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
        // Dropping every tree entry drops every tree lock and every tree
        // authority, which invalidates all of their logs at once.
        let mut ownership = self
            .ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ownership.trees.clear();
        ownership.writable.clear();
        ownership.adopting.clear();
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

#[cfg(test)]
mod tests;

#[cfg(all(test, windows))]
mod windows_tests {
    use cookie_agent_protocol::{
        AgentMode, AgentRevision, CatalogRevision, ClientRunId, CwdIdentity, EventPayload,
        ModelRevision, ProviderStateRevision, RecipeRegistryRevision, RunId, RuntimeRevision,
        SessionId, SessionOrigin,
    };

    use crate::ownership::owner_lock_path;

    use super::{SESSION_META_FILE, SESSIONS_ROOT_DIR, SessionStore, WORKDIR_CWD_FILE};

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
        let workdir = store.workdir_dir_path();
        let sessions_root = workdir.parent().expect("sessions root");
        for path in [
            sessions_root.to_owned(),
            workdir.to_owned(),
            workdir.join(WORKDIR_CWD_FILE),
        ] {
            cookie_agent_models::secure_store::verify_windows_private_creation(&path)
                .unwrap_or_else(|error| {
                    panic!("private ACL validation failed for {path:?}: {error:?}")
                });
        }
    }

    #[test]
    fn windows_session_store_uses_preexisting_untrusted_workdir_acl() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let cwd = temporary.path().join("workspace");
        std::fs::create_dir(&cwd).expect("workspace");
        let data = temporary.path().join("data");
        let workdir = data
            .join(SESSIONS_ROOT_DIR)
            .join(SessionStore::workdir_key(&cwd));
        std::fs::create_dir_all(&workdir).expect("ordinary workdir");
        SessionStore::open(&data, &cwd).expect("ordinary existing workdir");
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
                    short_id: None,
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

        let session_dir = store.workdir_dir_path().join(session_id.to_string());
        for path in [
            session_dir.clone(),
            session_dir.join("events.jsonl"),
            session_dir.join(SESSION_META_FILE),
            owner_lock_path(&session_dir),
        ] {
            cookie_agent_models::secure_store::verify_windows_private_creation(&path)
                .unwrap_or_else(|error| {
                    panic!("private ACL validation failed for {path:?}: {error}")
                });
        }
    }
}
