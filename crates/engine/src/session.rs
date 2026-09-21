//! Session directories, projections, and rebuildable metadata caches.

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
    AgentId, AgentSnapshot, ChildSummary, ClientRenameId, ClientRunId, EventPayload,
    EventSubscriptionMessage, EventsSubscribeResult, RunId, RunSelection, SessionId, SessionMeta,
    SessionOrigin, SessionPermissionOverlay, SessionRenameRecord, SessionStatus, SessionTitle,
    SessionTitleChange, SessionTree, StoredEvent, ToolCallId, Usage, UsageRollup,
};
use thiserror::Error;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::events::{EventLog, EventLogError, fsync_directory};
use crate::ownership::{
    HeldLock, SessionOwnership, WriteAuthority, WriteCapability, owner_lock_path, try_acquire,
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
    #[allow(dead_code)] // consumed by the lazy-tree passes (P2)
    fn root_of(self) -> Option<SessionId> {
        match self {
            Self::Root => None,
            Self::Child { root } => Some(root),
        }
    }
}

/// Lazy-tree state for one root session (§3.1 of the storage spec).
#[derive(Debug, Default)]
pub(crate) struct TreeState {
    /// The one-time bulk child pass completed *and* the engine accepted its
    /// products. Publication order is
    /// `Unloaded -> Loading -> (products applied + index durable) -> Loaded`:
    /// a stale fold, a failed pass or a rejected observer never sets this.
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
    /// What the loaded children reported about their own role as delegation
    /// parents, harvested by the one fold so a nested registry rebuild never
    /// reopens a child log (4.1.3).
    parent_facts: HashMap<SessionId, ParentRunFacts>,
    /// Children whose `index.json` entry disagrees with that session's own
    /// authoritative `metadata` cache. The disagreement is the one thing a
    /// summary cache cannot explain to itself — the session moved after the
    /// index was written — so it marks the cached summary *pre-load data* that
    /// [`SessionStore::summary`] refuses to serve before the tree is complete
    /// (§3.4). Cleared by the bulk pass, which installs the fold's answer.
    stale_seeds: HashSet<SessionId>,
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
    pub(crate) producer_projections:
        Vec<(SessionId, crate::goal_projection::GoalProducerProjection)>,
    /// Every child summary the pass produced, for usage and listing caches.
    pub(crate) summaries: Vec<SessionSummary>,
    /// `artifact://sha256/...` digests the pass found in child logs, installed
    /// into the artifact router so a sweep of a loaded tree never reopens them
    /// (3.3(b), 5.2).
    pub(crate) artifact_refs: HashSet<String>,
    /// Per-child delegation-parent facts (4.1.3).
    pub(crate) parent_facts: HashMap<SessionId, ParentRunFacts>,
    /// The [`LogFingerprint`] of each folded child log, taken with the snapshots
    /// above. It is the freshness proof for `artifact_refs`: a sweep may reuse the
    /// harvested set only while every child still has the same resident tip *and*
    /// the same durable length (§3.3(b), §5.2).
    pub(crate) child_log_fingerprints: BTreeMap<SessionId, LogFingerprint>,
}

impl TreeLoadProducts {
    fn for_root(root: SessionId) -> Self {
        Self {
            root,
            delegations: Vec::new(),
            grants: Vec::new(),
            bindings: Vec::new(),
            producer_sessions: Vec::new(),
            producer_projections: Vec::new(),
            summaries: Vec::new(),
            artifact_refs: HashSet::new(),
            parent_facts: HashMap::new(),
            child_log_fingerprints: BTreeMap::new(),
        }
    }
}

/// What the delegation registry needs about a session acting as a delegation
/// *parent*, harvested from that parent's own log while it is folded (4.1.3).
/// Carrying these facts is what lets a nested registry rebuild run without
/// reopening a child log for `DelegateQueued`/`DelegateFinishedV2` lookups.
#[derive(Clone, Debug)]
pub(crate) struct ParentRunFacts {
    /// Tree root the parent belongs to (the parent itself when it is a root).
    pub(crate) root_session_id: SessionId,
    /// The parent is itself delegated, which suppresses its background slot.
    pub(crate) delegated: bool,
    /// `(parent run, child)` pairs from `DelegateQueued` records.
    pub(crate) queued: Vec<(cookie_agent_protocol::RunId, SessionId)>,
    /// `(invocation, child)` pairs from `DelegateFinishedV2` notifications.
    pub(crate) notified: Vec<(cookie_agent_protocol::InvocationId, SessionId)>,
    /// Delegations whose delivery a producer message already accepted.
    pub(crate) producer_accepted: Vec<cookie_agent_protocol::InvocationId>,
    /// Runs of this parent that reached `Interrupted`.
    pub(crate) interrupted_runs: Vec<cookie_agent_protocol::RunId>,
}

impl ParentRunFacts {
    /// Folds the facts out of one already-read projection. Never opens a log.
    fn from_projection(projection: &SessionProjection) -> Self {
        let (root_session_id, delegated) = match projection.meta.origin {
            SessionOrigin::Delegated {
                root_session_id, ..
            } => (root_session_id, true),
            _ => (projection.meta.session_id, false),
        };
        let mut facts = Self {
            root_session_id,
            delegated,
            queued: Vec::new(),
            notified: Vec::new(),
            producer_accepted: Vec::new(),
            interrupted_runs: Vec::new(),
        };
        for envelope in projection.log.event_snapshot().iter() {
            match &envelope.payload {
                EventPayload::DelegateQueued { session_id, .. } => {
                    if let Some(run_id) = envelope.run_id {
                        facts.queued.push((run_id, *session_id));
                    }
                }
                EventPayload::DelegateFinishedV2 {
                    invocation_id,
                    session_id,
                    ..
                } => facts.notified.push((*invocation_id, *session_id)),
                EventPayload::ProducerMessageAccepted {
                    producer_owner:
                        cookie_agent_protocol::ProducerOwner::Delegation { invocation_id },
                    ..
                } => facts.producer_accepted.push(*invocation_id),
                _ => {}
            }
        }
        facts.interrupted_runs = projection
            .runs
            .iter()
            .filter(|(_, run)| run.status == SessionStatus::Interrupted)
            .map(|(run_id, _)| *run_id)
            .collect();
        facts
    }
}

/// Where one root's lazy tree load stands (§3.1). The gate is what a concurrent
/// trigger waits on, so `Loaded` is only ever observed after the products of the
/// pass were applied.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum TreeLoadStatus {
    /// No pass has completed.
    #[default]
    Unloaded,
    /// A pass owns the root: either folding, or delivering products it installed.
    Loading,
    /// Installed and durable, but the engine rejected the products. Retryable
    /// without a second fold: the queued products are delivered again.
    Pending,
    /// Fold complete, index durable, products applied.
    Loaded,
}

/// Per-root load gate: a driver marker plus the waiters a completing load wakes.
/// Deliberately *not* a plain mutex — the observer callback must run with no
/// store lock held (D3) while concurrent triggers still cannot take the fast
/// path, so waiters park on a completion record instead of on the guard.
#[derive(Debug, Default)]
struct TreeGate {
    status: Mutex<TreeLoadStatus>,
    ready: Condvar,
}

impl TreeGate {
    #[cfg(test)]
    fn status(&self) -> TreeLoadStatus {
        *self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Publishes a new status and wakes every waiter.
    fn settle(&self, status: TreeLoadStatus) {
        *self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = status;
        self.ready.notify_all();
    }

    /// Claims the right to drive one load cycle, or `None` when the tree is
    /// already loaded.
    fn acquire(&self) -> Option<TreeLoadStatus> {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            match *status {
                TreeLoadStatus::Loaded => return None,
                TreeLoadStatus::Loading => {
                    status = self
                        .ready
                        .wait(status)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                _ => {
                    let from = *status;
                    *status = TreeLoadStatus::Loading;
                    drop(status);
                    return Some(from);
                }
            }
        }
    }
}

/// Completed loads waiting for the engine hook that consumes them (D5).
#[derive(Default)]
struct PendingLoads {
    /// Engine hook installed by [`SessionStore::set_tree_load_observer`].
    observer: Option<Arc<dyn TreeLoadObserver>>,
    /// Products of finished passes, keyed by root, each claimed exactly once.
    queued: HashMap<SessionId, Arc<TreeLoadProducts>>,
    /// Roots whose own driver thread is delivering their products right now, so
    /// a drain from another thread can never claim them a second time.
    driving: HashSet<SessionId>,
}

impl std::fmt::Debug for PendingLoads {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingLoads")
            .field("observer", &self.observer.is_some())
            .field("queued", &self.queued.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Callback the engine installs so store-side tree loads reach engine singletons
/// even when the load was triggered from inside the store. A failing observer
/// fails the load, so the access that triggered it fails closed, and its
/// products stay queued for the next attempt rather than being dropped.
pub(crate) trait TreeLoadObserver: Send + Sync {
    fn tree_loaded(
        &self,
        products: Arc<TreeLoadProducts>,
    ) -> Result<(), crate::runtime::EngineError>;
}

/// How many unlocked folds a load may lose to a concurrent writer before the
/// pass falls back to folding with `mutation` held (L1).
const TREE_LOAD_RACES: usize = 3;
/// How many driver turns one [`SessionStore::load_tree`] call allows itself:
/// a fold, plus a couple of handovers around a rejected load.
const TREE_LOAD_TURNS: usize = 4;

/// Outcome of trying to publish one fold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Install {
    /// The fold still matches what is on disk and in memory: installed.
    Published,
    /// A writer moved a child (or added one) underneath the read phase: the
    /// fold is stale and was *not* installed.
    Stale,
}

/// Before/after proof that one session log did not move while it was folded.
///
/// This is *the* fold-validity signal of the store, and it is deliberately not
/// just a durable byte count: a resident log holds its newest records in a
/// buffered writer, so the file can stay the same size while the log moves.
/// Anything else that wants to reuse data harvested by a fold (§3.3(b), §5.2)
/// has to be invalidated by the same pair, which is why this type is crate
/// visible rather than private to the load path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LogFingerprint {
    /// Tip of this process's resident log for the session (`None` when the
    /// session is not resident and the fold read the file itself).
    pub(crate) resident_tip: Option<u64>,
    /// Bytes of `events.jsonl` visible on disk.
    pub(crate) durable_len: u64,
}

/// One read phase of a tree load: what was harvested, plus the fingerprints and
/// the child listing that prove the harvest is still current when it is
/// installed.
struct TreeFold {
    products: TreeLoadProducts,
    /// Children the pass based itself on, in directory order.
    children: Vec<SessionId>,
    /// Log fingerprint taken before each child was folded.
    fingerprints: HashMap<SessionId, LogFingerprint>,
    /// Every grant the pass saw, restart-stable or not (§4.3).
    tree_grants: Vec<cookie_agent_protocol::TreeApprovalGrant>,
    edges: HashMap<SessionId, Vec<SessionId>>,
    terminal_runs: HashMap<SessionId, BTreeMap<String, SessionStatus>>,
}

impl TreeFold {
    fn for_root(root: SessionId) -> Self {
        Self {
            products: TreeLoadProducts::for_root(root),
            children: Vec::new(),
            fingerprints: HashMap::new(),
            tree_grants: Vec::new(),
            edges: HashMap::new(),
            terminal_runs: HashMap::new(),
        }
    }
}

/// RAII claim on one root's load gate. Publishing a status is deliberate;
/// anything that leaves the turn without one (an IO failure, a `?` return, a
/// panic) restores a *retryable* state and wakes the waiters, so a root can
/// never wedge in `Loading`.
struct TreeLoadDriver {
    gate: Arc<TreeGate>,
    root: SessionId,
    settled: bool,
}

impl TreeLoadDriver {
    fn new(gate: Arc<TreeGate>, root: SessionId) -> Self {
        TREE_LOAD_DRIVERS.with(|drivers| drivers.borrow_mut().push(root));
        Self {
            gate,
            root,
            settled: false,
        }
    }

    /// Publishes the outcome of this turn and wakes every waiter.
    fn settle(&mut self, status: TreeLoadStatus) {
        self.settled = true;
        self.gate.settle(status);
    }

    /// Completes a load: the durable install is already in place, so the store
    /// can be marked loaded before the gate releases the waiters.
    fn publish_loaded(&mut self, store: &SessionStore) {
        store.mark_tree_loaded(self.root);
        self.settle(TreeLoadStatus::Loaded);
    }
}

impl Drop for TreeLoadDriver {
    fn drop(&mut self) {
        TREE_LOAD_DRIVERS.with(|drivers| {
            let mut drivers = drivers.borrow_mut();
            if let Some(index) = drivers.iter().position(|id| *id == self.root) {
                drivers.remove(index);
            }
        });
        if !self.settled {
            self.gate.settle(TreeLoadStatus::Unloaded);
        }
    }
}

thread_local! {
    /// Roots this thread is loading, or is delivering the products of. Re-entering
    /// the store from an observer callback must never wait on its own gate.
    static TREE_LOAD_DRIVERS: std::cell::RefCell<Vec<SessionId>> = const {
        std::cell::RefCell::new(Vec::new())
    };
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

/// Test-only load hook. Hand-written `Debug`: the payload is a `dyn Fn`, and the
/// store derives `Debug`.
#[cfg(test)]
#[derive(Default)]
#[allow(clippy::type_complexity)]
struct TreeLoadReadHook(std::sync::Mutex<Option<Arc<dyn Fn(SessionId) + Send + Sync>>>);

#[cfg(test)]
impl TreeLoadReadHook {
    fn installed(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }
}

#[cfg(test)]
impl std::fmt::Debug for TreeLoadReadHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TreeLoadReadHook")
            .field("installed", &self.installed())
            .finish_non_exhaustive()
    }
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
    ownership: Mutex<HashMap<SessionId, StoreOwnership>>,
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
            ownership: Mutex::new(HashMap::new()),
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

    /// Root a session belongs to (`None` for a root session itself). Unknown
    ///
    /// (consumed by the lazy-tree passes; wired up in P2)
    /// children are located on disk first so the answer is disk-accurate.
    #[allow(dead_code)] // wired up by the lazy-tree passes (P2)
    pub(crate) fn root_of(&self, id: SessionId) -> Result<SessionId, SessionError> {
        self.resolve_dir(id)?;
        match self.cached_location(id) {
            Some(SessionLocation::Child { root }) => Ok(root),
            _ => Ok(id),
        }
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

    /// Test-only seam: fires at the end of a bulk load's read phase, right before
    /// the fold is verified and installed, i.e. exactly inside the window a stale
    /// publication would slip through (review L1).
    #[cfg(test)]
    pub(crate) fn install_tree_load_read_hook_for_test(
        &self,
        hook: impl Fn(SessionId) + Send + Sync + 'static,
    ) {
        *self
            .tree_load_read_hook
            .0
            .lock()
            .expect("hook lock poisoned") = Some(Arc::new(hook));
    }

    #[cfg(test)]
    fn run_tree_load_read_hook(&self, root: SessionId) {
        // Held across the call: the hook runs with no store lock, and a test that
        // appends from inside it must reach the same `mutation` mutex normally.
        let hook = self
            .tree_load_read_hook
            .0
            .lock()
            .expect("hook lock poisoned")
            .clone();
        if let Some(hook) = hook {
            hook(root);
        }
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
            write_cache(&temporary.join(SESSION_META_FILE), &fork_projection.meta)?;
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
            self.publish_prepared_dir(&temporary, &final_dir, session_id)?;
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
            self.publish_prepared_dir(&temporary, &final_dir, session_id)?;
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

    /// The root whose tree must be complete before serving `id` (§3.2).
    pub(crate) fn ensure_tree_for(&self, id: SessionId) -> Result<(), SessionError> {
        if self.mutation_held() {
            // Deadlock safety net, not a serving path: a caller that takes
            // `mutation` must complete the tree *before* it takes the lock
            // ([`Self::fork`], [`Self::persist_buffered_session`],
            // [`Self::begin_write`]). Reaching this branch means it did, and the
            // load's install phase needs the same lock it already holds.
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
        if TREE_LOAD_DRIVERS.with(|drivers| drivers.borrow().contains(&root)) {
            // This thread owns the load, or runs the observer callback it
            // triggered: the tree is being published right now, and waiting on
            // the gate here would deadlock the callback against its own load.
            return Ok(());
        }
        self.load_tree(root)
    }

    /// Cheap pre-check for a read that can be answered from memory *before* it
    /// reaches [`Self::get`] — a cached summary, a fork source, a buffered
    /// publish. Those paths would otherwise serve pre-load data for a tree whose
    /// bulk pass has not run (§3.2).
    ///
    /// `false` means "a load cannot be pending here", so the caller skips
    /// `ensure_tree_for` entirely and the common already-loaded path stays free:
    /// one placement probe and one tree-state probe, no filesystem access, and
    /// never a load started under `mutation` (a per-root gate guard or the
    /// mutation lock must not be held when a load begins).
    fn tree_load_pending_for(&self, id: SessionId) -> bool {
        if self.mutation_held() {
            return false;
        }
        let root = match self.cached_location(id) {
            // Placement is not cached: `ensure_tree_for` has to resolve the
            // directory to know what to load, so let it make that call.
            None => return true,
            Some(SessionLocation::Child { root }) => root,
            Some(SessionLocation::Root) => id,
        };
        !self.tree_is_loaded(root)
    }

    /// Whether `root`'s tree is complete: loaded, installed, and published.
    fn tree_is_loaded(&self, root: SessionId) -> bool {
        self.trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&root)
            .is_some_and(|state| state.loaded)
    }

    /// Whether the child tree of `root` was bulk-loaded *and applied* in this
    /// process. Set only after the engine accepted the load's products.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_tree_loaded(&self, root: SessionId) -> bool {
        self.tree_is_loaded(root)
    }

    /// The load gate of one root, created on first use.
    fn tree_gate(&self, root: SessionId) -> Arc<TreeGate> {
        let mut locks = self
            .tree_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            locks
                .entry(root)
                .or_insert_with(|| Arc::new(TreeGate::default())),
        )
    }

    /// Marks the tree complete. Always happens before the gate publishes, so a
    /// waiter woken by the completion record can never miss the installed state.
    fn mark_tree_loaded(&self, root: SessionId) {
        self.trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(root)
            .or_default()
            .loaded = true;
    }

    /// Reads and folds every child log of `root` exactly once per process
    /// (§3.3), harvesting what the engine singletons need — delegation records,
    /// restart-stable grants, producer state, manifest bindings, artifact
    /// references and summaries — and leaving no child resident. Idempotent;
    /// concurrent triggers coalesce per root, and a trigger that arrives while a
    /// load is in flight waits for its completion record instead of taking the
    /// fast path (§3.1).
    pub(crate) fn load_tree(&self, root: SessionId) -> Result<(), SessionError> {
        let gate = self.tree_gate(root);
        for _ in 0..TREE_LOAD_TURNS {
            // `acquire` joins an in-flight load rather than racing it, so only
            // one thread ever folds; `from` says whether products of a rejected
            // load are still waiting to be delivered.
            let Some(from) = gate.acquire() else {
                // The root reports loaded. That is a promise about the *store*
                // side of a pass; a pass that ran while no observer was
                // installed (or whose delivery a drain never reached) can still
                // be holding products back, and serving a tree read on top of
                // unapplied products is exactly what `Loaded` must never mean.
                return self.redeliver_unclaimed_load(root);
            };
            let mut driver = TreeLoadDriver::new(Arc::clone(&gate), root);
            let outcome = if from == TreeLoadStatus::Pending {
                self.deliver_queued_load(root, &mut driver)
            } else {
                self.run_tree_load(root, &mut driver).map(|()| true)
            };
            // Dropping `driver` publishes whatever the turn settled, so a
            // failure here always leaves the root retryable.
            match outcome? {
                true => return Ok(()),
                // A rejected load's products vanished (claimed elsewhere): the
                // tree still needs its pass.
                false => continue,
            }
        }
        Err(SessionError::TreeContended(root))
    }

    /// One full turn: fold, install, make the index durable, then hand the
    /// products to the engine. `Loaded` is published last, and only if all of it
    /// succeeded (§3.1, review L2).
    fn run_tree_load(
        &self,
        root: SessionId,
        driver: &mut TreeLoadDriver,
    ) -> Result<(), SessionError> {
        self.ensure_open()?;
        let products = self.fold_tree(root)?;
        // The cache rewrite stays inside the driver's claim: `Loaded` promises a
        // durable `index.json` that matches what was installed (§3.4).
        self.persist_subagent_index(root)?;
        self.publish_load_products(products, driver)
    }

    /// Runs the read/install loop. An unlocked fold is verified against the
    /// fingerprints taken with it and thrown away when a writer moved one of the
    /// snapshots underneath it (L1); a fold that keeps losing that race runs
    /// again with `mutation` held, where no writer can move at all.
    fn fold_tree(&self, root: SessionId) -> Result<TreeLoadProducts, SessionError> {
        for _ in 0..TREE_LOAD_RACES {
            let fold = self.harvest_tree(root)?;
            if self.install_tree_fold(root, &fold)? == Install::Published {
                return Ok(fold.products);
            }
        }
        let _mutation = self.lock_mutation();
        let fold = self.harvest_tree(root)?;
        if self.install_tree_fold(root, &fold)? != Install::Published {
            // Only a writer outside this process can move a log while `mutation`
            // is held, which the single-writer ownership model does not allow.
            return Err(SessionError::TreeContended(root));
        }
        Ok(fold.products)
    }

    /// The read phase of one bulk pass. No store lock is held, so the pass
    /// cannot block appends and cannot re-enter the store's write paths (D3).
    fn harvest_tree(&self, root: SessionId) -> Result<TreeFold, SessionError> {
        let mut fold = TreeFold::for_root(root);
        let _reads = TreeLoadReads::begin();
        fold.children = self.child_dir_ids(root);
        for child in fold.children.clone() {
            // Captured before the read: an equal fingerprint afterwards proves
            // nothing was appended to or synced into this log while it folded.
            let fingerprint = self.log_fingerprint(child);
            // A child this process already owns is folded from its resident
            // projection. That is the freshest view there is, and reopening the
            // file would fold bytes its writer has not synced yet.
            let projection = match self.get_resident(child) {
                Some(session) => session,
                None => self.open_snapshot(child, false)?,
            };
            let events = projection.log.event_snapshot();
            fold.products
                .summaries
                .push(summary_from_projection(&projection));
            fold.products
                .child_log_fingerprints
                .insert(child, fingerprint);
            fold.fingerprints.insert(child, fingerprint);
            crate::runtime::artifacts::collect_artifact_references_in_events(
                &events,
                &mut fold.products.artifact_refs,
            )
            .map_err(|source| SessionError::Io {
                path: self
                    .path_for(SessionLocation::Child { root }, child)
                    .join(EVENTS_FILE),
                source,
            })?;
            for envelope in events.iter() {
                match &envelope.payload {
                    EventPayload::SessionCreated { creation_agent, .. } => {
                        fold.products.bindings.extend(
                            creation_agent
                                .fallback_chain
                                .iter()
                                .cloned()
                                .map(|binding| (projection.meta.session_id, binding)),
                        )
                    }
                    EventPayload::RunStarted {
                        selected_suffix, ..
                    } => fold.products.bindings.extend(
                        selected_suffix
                            .iter()
                            .cloned()
                            .map(|binding| (projection.meta.session_id, binding)),
                    ),
                    EventPayload::TreeApprovalGrantCommitted { grant } => {
                        // The visible-grant rebuild needs every grant; only
                        // the approval store filters to restart-stable ones.
                        if restart_stable_grant(grant) {
                            fold.products.grants.push(grant.clone());
                        }
                        fold.tree_grants.push(grant.clone());
                    }
                    payload => {
                        if !projection.log.delegation_event_tainted(envelope)
                            && crate::delegation_events::is_delegation_payload(payload)
                        {
                            fold.products.delegations.push((
                                projection.meta.session_id,
                                envelope.run_id,
                                payload.clone(),
                            ));
                        }
                    }
                }
            }
            if crate::runtime::producers::producer_state_pending(&events) {
                fold.products
                    .producer_sessions
                    .push(projection.meta.session_id);
                fold.products.producer_projections.push((
                    projection.meta.session_id,
                    crate::goal_projection::GoalProducerProjection::from_events(&events),
                ));
            }
            // The registry needs these facts about the child *as a parent*;
            // carrying them here is what keeps a nested rebuild from folding the
            // same log a second time (§4.1.3).
            fold.products
                .parent_facts
                .insert(child, ParentRunFacts::from_projection(&projection));
            let parent = match projection.meta.origin {
                SessionOrigin::Delegated {
                    parent_session_id, ..
                } => parent_session_id,
                _ => root,
            };
            fold.edges
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
                fold.terminal_runs.insert(projection.meta.session_id, runs);
            }
        }
        #[cfg(test)]
        self.run_tree_load_read_hook(root);
        Ok(fold)
    }

    /// Fingerprint of one session log: this process's resident tip plus the
    /// durable byte length. Both only ever grow, so equal fingerprints before
    /// and after a fold prove the snapshot being installed is the current one.
    /// Crate visible because the artifact sweep reuses harvested data and must be
    /// invalidated by exactly this signal, not by a weaker one (§5.2).
    pub(crate) fn log_fingerprint(&self, id: SessionId) -> LogFingerprint {
        LogFingerprint {
            resident_tip: self
                .get_resident(id)
                .map(|session| session.log.physical_tip_seq()),
            durable_len: self
                .resolve_dir(id)
                .ok()
                .and_then(|directory| fs::metadata(directory.join(EVENTS_FILE)).ok())
                .map_or(0, |metadata| metadata.len()),
        }
    }

    /// Publishes a fold's in-memory state. Every snapshot is re-checked under
    /// `mutation` first, which is the same lock appends, creates and publishes
    /// take: a fold that raced a writer is dropped here and read again.
    /// Never sets `loaded` — that waits for the products (L1, L2).
    fn install_tree_fold(&self, root: SessionId, fold: &TreeFold) -> Result<Install, SessionError> {
        let _mutation = self.lock_mutation();
        self.ensure_open()?;
        if self.child_dir_ids(root) != fold.children {
            return Ok(Install::Stale);
        }
        for (child, fingerprint) in &fold.fingerprints {
            if &self.log_fingerprint(*child) != fingerprint {
                return Ok(Install::Stale);
            }
        }
        {
            let mut residency = self
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for summary in &fold.products.summaries {
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
        for (parent, children) in &fold.edges {
            let slot = state.children.entry(*parent).or_default();
            for child in children {
                if !slot.contains(child) {
                    slot.push(*child);
                }
            }
        }
        for (child, runs) in &fold.terminal_runs {
            let slot = state.terminal_runs.entry(*child).or_default();
            for (run_id, status) in runs {
                slot.entry(run_id.to_owned()).or_insert(*status);
            }
        }
        for (child, facts) in &fold.products.parent_facts {
            state.parent_facts.insert(*child, facts.clone());
        }
        // Every summary the seed was standing in for now has the fold's answer
        // behind it, so the staleness record is spent (§3.4).
        state.stale_seeds.clear();
        state.grants = fold.tree_grants.clone();
        state.producer_sessions = fold.products.producer_sessions.clone();
        Ok(Install::Published)
    }

    /// Queues a completed load and delivers it (§3.3(d)). The observer runs with
    /// no store lock and no gate guard held, so re-entering the store from the
    /// callback cannot depend on lock luck (review L4).
    fn publish_load_products(
        &self,
        products: TreeLoadProducts,
        driver: &mut TreeLoadDriver,
    ) -> Result<(), SessionError> {
        let root = products.root;
        let observer = {
            let mut pending = self
                .pending_loads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            pending.queued.insert(root, Arc::new(products));
            let Some(observer) = pending.observer.clone() else {
                // Nothing to deliver to *yet*. The products stay keyed by root
                // until an observer claims them, and the load itself is
                // complete (D5). They also stay *owed*: every later access to
                // this root funnels through `load_tree`, which hands them to the
                // hook before that access may serve the tree (§3.3, review F5).
                driver.publish_loaded(self);
                return Ok(());
            };
            // Claim it for this thread before releasing the queue, so a drain
            // from another thread can never apply the same products twice.
            pending.driving.insert(root);
            observer
        };
        let Some(claimed) = self.claim_queued_load(root) else {
            // Already claimed by this driver's own insert: nothing to apply.
            driver.publish_loaded(self);
            return Ok(());
        };
        self.deliver_products(root, &observer, claimed, driver)
    }

    /// Re-delivers the products of a load whose observer rejected them once.
    /// Returns `false` when nothing is queued, which tells the caller the tree
    /// needs its pass after all.
    fn deliver_queued_load(
        &self,
        root: SessionId,
        driver: &mut TreeLoadDriver,
    ) -> Result<bool, SessionError> {
        let (observer, claimed) = {
            let mut pending = self
                .pending_loads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(observer) = pending.observer.clone() else {
                driver.publish_loaded(self);
                return Ok(true);
            };
            let claimed = pending.queued.remove(&root);
            if claimed.is_some() {
                pending.driving.insert(root);
            }
            (observer, claimed)
        };
        let Some(products) = claimed else {
            // No driver marker is set, so settling `Unloaded` is enough to let
            // the caller take the fold turn.
            return Ok(false);
        };
        self.deliver_products(root, &observer, products, driver)?;
        Ok(true)
    }

    fn deliver_products(
        &self,
        root: SessionId,
        observer: &Arc<dyn TreeLoadObserver>,
        products: Arc<TreeLoadProducts>,
        driver: &mut TreeLoadDriver,
    ) -> Result<(), SessionError> {
        let result = observer.tree_loaded(Arc::clone(&products));
        let mut pending = self
            .pending_loads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.driving.remove(&root);
        match result {
            Ok(()) => {
                drop(pending);
                driver.publish_loaded(self);
                Ok(())
            }
            Err(error) => {
                // Retryable: the products stay queued and the gate leaves
                // `Pending`, so the next trigger delivers them again without a
                // second fold. The tree is *not* marked loaded (review L2).
                pending.queued.entry(root).or_insert(products);
                drop(pending);
                driver.settle(TreeLoadStatus::Pending);
                Err(SessionError::TreeRejected(Box::new(error)))
            }
        }
    }

    /// Atomically takes one root's queued products for delivery.
    fn claim_queued_load(&self, root: SessionId) -> Option<Arc<TreeLoadProducts>> {
        self.pending_loads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .queued
            .remove(&root)
    }

    /// Delivers products a completed load is still holding back for a root whose
    /// gate already reports `Loaded`.
    ///
    /// This is what keeps `Loaded` honest without an observer: a pass that
    /// finished with no hook installed (or whose queue a drain abandoned on an
    /// earlier root's failure) left the products unapplied, and no read of that
    /// tree may be served until the hook that arrived has applied them. The
    /// access that triggered it fails closed, and the products stay queued for
    /// the next one — the durable install stands, so no second fold is needed
    /// (§3.3, D5, review F5).
    fn redeliver_unclaimed_load(&self, root: SessionId) -> Result<(), SessionError> {
        let (observer, products) = {
            let mut pending = self
                .pending_loads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(observer) = pending.observer.clone() else {
                // Still no hook in this process: nothing can apply them yet, and
                // this is the store-only case the pass was designed for.
                return Ok(());
            };
            if pending.driving.contains(&root) {
                // Another thread is applying this root's products right now.
                return Ok(());
            }
            (observer, pending.queued.remove(&root))
        };
        let Some(products) = products else {
            return Ok(());
        };
        match observer.tree_loaded(Arc::clone(&products)) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.pending_loads
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .queued
                    .entry(root)
                    .or_insert(products);
                Err(SessionError::TreeRejected(Box::new(error)))
            }
        }
    }

    /// Delivers every completed load that is waiting for the engine hook,
    /// skipping the ones another thread's driver owns (D5). Each product set is
    /// claimed exactly once; a rejection leaves it queued for the next call.
    pub(crate) fn drain_pending_loads(&self) -> Result<(), SessionError> {
        let (observer, claimed) = {
            let mut pending = self
                .pending_loads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(observer) = pending.observer.clone() else {
                return Ok(());
            };
            let roots = pending
                .queued
                .keys()
                .filter(|root| !pending.driving.contains(root))
                .copied()
                .collect::<Vec<_>>();
            let mut claimed = Vec::with_capacity(roots.len());
            for root in roots {
                if let Some(products) = pending.queued.remove(&root) {
                    pending.driving.insert(root);
                    claimed.push((root, products));
                }
            }
            (observer, claimed)
        };
        for position in 0..claimed.len() {
            let (root, products) = &claimed[position];
            if let Err(error) = observer.tree_loaded(Arc::clone(products)) {
                let mut pending = self
                    .pending_loads
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // Everything claimed from here on went undelivered, so it all
                // goes back: dropping it would lose a completed load, and
                // leaving its `driving` marker set would wedge the root against
                // every later delivery pass (D5).
                for (root, products) in claimed[position..].iter() {
                    pending.driving.remove(root);
                    pending
                        .queued
                        .entry(*root)
                        .or_insert_with(|| Arc::clone(products));
                }
                return Err(SessionError::TreeRejected(Box::new(error)));
            }
            self.pending_loads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .driving
                .remove(root);
        }
        Ok(())
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

    /// Root sessions whose logs can carry goal-producer state. Loaded children
    /// are reconciled from the projections harvested by their tree load.
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

    /// What the delegation registry needs about `id` acting as a delegation
    /// parent, without reopening a child log: the resident projection first,
    /// then the facts the tree load folded, and only for a session the load
    /// cannot know about (a root, or a store that never files children) its own
    /// log (§4.1.3).
    ///
    /// `None` means "not knowable yet": `id` is a filed child whose tree has not
    /// been bulk-loaded, so its facts do not exist anywhere. Callers must treat
    /// the parent as unresolved — the log is *not* a legal read here (§3.3).
    pub(crate) fn parent_run_facts(
        &self,
        id: SessionId,
    ) -> Result<Option<ParentRunFacts>, SessionError> {
        if let Some(session) = self.get_resident(id) {
            return Ok(Some(ParentRunFacts::from_projection(&session)));
        }
        if let Some(facts) = self.cached_parent_facts(id) {
            return Ok(Some(facts));
        }
        // Resolving placement costs a directory probe, never a log read, and is
        // what tells a filed child from a root in a store that has not looked at
        // this session yet.
        if self.resolve_dir(id).is_ok() && self.is_filed_child(id) {
            // A child log is folded by exactly one thing: its tree's bulk pass.
            // That pass is the only producer of these facts, so arriving here
            // means the tree is still unloaded — and reading the log now would
            // fold it a second time, outside the coalesced pass and at a second,
            // differently-timed moment (§3.3, §4.1.3). The registry re-runs when
            // that tree's load delivers its products, and that is what resolves
            // this parent.
            return Ok(None);
        }
        let projection = self.get_log_only(id)?;
        Ok(Some(ParentRunFacts::from_projection(&projection)))
    }

    /// Facts the bulk pass harvested for `id`.
    ///
    /// Deliberately *not* gated on the tree being fully published: the pass
    /// installs these under `mutation` from a fold it verified against its own
    /// fingerprints, and the engine applying the other products (grants,
    /// delegation records) has no bearing on what this child's log said. Waiting
    /// for `loaded` would push a caller back onto a cold child log read, which is
    /// precisely what these facts exist to avoid (§4.1.3).
    fn cached_parent_facts(&self, id: SessionId) -> Option<ParentRunFacts> {
        self.trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .find_map(|state| state.parent_facts.get(&id).cloned())
    }

    /// Installs the engine hook that receives tree load products and delivers
    /// everything a completed load already queued (§3.3, D5).
    pub(crate) fn set_tree_load_observer(
        &self,
        observer: Arc<dyn TreeLoadObserver>,
    ) -> Result<(), SessionError> {
        self.pending_loads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .observer = Some(observer);
        self.drain_pending_loads()
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
                    agent_usage: session.agent_usage.clone(),
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

    /// Seeded child ids, summaries and the terminal-run cache from a root's
    /// persisted `subagents/index.json` (never from a child log).
    ///
    /// The file is a cache, so each entry is validated against the directory it
    /// claims before anything is trusted from it, and duplicates are ignored. A
    /// stale or corrupt entry is simply dropped: the directory scan or a later
    /// tree load recovers the child, and a bad index stays a non-event (§3.4,
    /// review L9). Seeded summaries carry `usage: None` — the derived per-session
    /// total is only ever served from a log that was actually folded (D8).
    fn seed_tree_from_index(&self, root: SessionId) {
        let Some(index) = self.read_subagent_index(root) else {
            return;
        };
        let mut edges = Vec::new();
        let mut terminal_runs = HashMap::new();
        let mut stale_seeds = Vec::new();
        let mut seen = HashSet::new();
        for child in index.children {
            let id = child.summary.meta.session_id;
            if !seen.insert(id) {
                eprintln!("subagent index for {root} names session {id} twice; ignored");
                continue;
            }
            if self.is_indexed_root(id) {
                eprintln!("subagent index for {root} names root session {id}; ignored");
                continue;
            }
            if self.cached_location(id) != Some(SessionLocation::Child { root }) {
                // Fresh placement: prove the directory exists and agrees first.
                let validated = self.validated_index_child(root, id);
                if validated.is_none() {
                    continue;
                }
                let authoritative = validated.expect("checked above");
                if authoritative != child.summary.meta {
                    // The entry disagrees with the session's own authoritative
                    // `metadata` cache: the child moved on after this index was
                    // written, so nothing it says about the child is current.
                    // The placement stays trusted (the directory proved it) and
                    // the entry keeps feeding the listings, but a read that
                    // *serves* this summary has to complete the tree first —
                    // that pass, not the cache, is what can answer for it now
                    // (§3.4, §3.2).
                    stale_seeds.push(id);
                }
                if !self.is_known(id) {
                    self.cache_summary(
                        id,
                        SessionSummary {
                            meta: authoritative,
                            usage: None,
                            usage_rollup: child.summary.usage_rollup.clone(),
                            agent_usage: child.summary.agent_usage.clone(),
                        },
                    );
                }
                self.record_location(id, SessionLocation::Child { root });
            }
            let parent = match self.cached_origin(id) {
                Some(SessionOrigin::Delegated {
                    parent_session_id, ..
                }) => parent_session_id,
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
        state.stale_seeds.extend(stale_seeds);
        for (parent, id) in edges {
            let siblings = state.children.entry(parent).or_default();
            if !siblings.contains(&id) {
                siblings.push(id);
            }
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

    /// Test-only: the load gate state of one root, as the store sees it.
    #[cfg(test)]
    pub(crate) fn tree_load_status(&self, root: SessionId) -> TreeLoadStatus {
        self.tree_gate(root).status()
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
                agent_usage: BTreeMap::new(),
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
        short_id,
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
            short_id,
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
            short_id.clone(),
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
        short_id,
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

/// Every entry of a session directory, or `None` when it is not a directory at
/// all. A per-entry failure is returned instead of skipped: guessing about a
/// listing is exactly what a publish decision must not do.
fn scaffold_listing(directory: &Path) -> Result<Option<Vec<fs::DirEntry>>, SessionError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SessionError::Io {
                path: directory.to_owned(),
                source,
            });
        }
    };
    let mut listing = Vec::new();
    for entry in entries {
        listing.push(entry.map_err(|source| SessionError::Io {
            path: directory.to_owned(),
            source,
        })?);
    }
    Ok(Some(listing))
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
    /// Which stores this thread holds a `lock_mutation` guard for. Keyed by store
    /// identity, so nesting two stores on one thread cannot make the second one
    /// skip its own mutex (review L15).
    static MUTATION_DEPTH: std::cell::RefCell<HashMap<usize, ()>> =
        std::cell::RefCell::new(HashMap::new());
}

/// RAII holder for the store's durable-mutation lock. `locked: None` means this
/// thread already holds *this store's* lock further up the stack, so the guard is
/// a no-op.
struct MutationGuard<'a> {
    locked: Option<std::sync::MutexGuard<'a, ()>>,
    store: Option<usize>,
}

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        let Some(store) = self.store else {
            return;
        };
        self.locked = None;
        MUTATION_DEPTH.with(|depths| {
            depths.borrow_mut().remove(&store);
        });
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

/// Session metadata cache path.
pub(crate) fn meta_path(session_dir: &Path) -> PathBuf {
    session_dir.join(SESSION_META_FILE)
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
    let temporary = parent.join(format!(".metadata.{}.tmp", Uuid::now_v7()));
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
pub(crate) fn replace_windows_path_with_retry(source: &Path, target: &Path) -> std::io::Result<()> {
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
fn write_workdir_cwd(workdir_dir: &Path, cwd: &Path) -> Result<(), SessionError> {
    let Ok(canonical) = cwd.canonicalize() else {
        return Ok(());
    };
    let bytes = canonical.as_os_str().as_bytes();
    let path = workdir_dir.join(WORKDIR_CWD_FILE);
    if workdir_cwd_is_current(&path, bytes) {
        return Ok(());
    }

    let temporary = workdir_dir.join(format!(".{WORKDIR_CWD_FILE}.{}.tmp", Uuid::now_v7()));
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
        fs::File::open(workdir_dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|source| SessionError::Io { path, source })
}

#[cfg(windows)]
fn write_workdir_cwd(workdir_dir: &Path, cwd: &Path) -> Result<(), SessionError> {
    let Ok(canonical) = cwd.canonicalize() else {
        return Ok(());
    };
    let bytes = canonical.as_os_str().as_encoded_bytes();
    let path = workdir_dir.join(WORKDIR_CWD_FILE);
    if workdir_cwd_is_current(&path, bytes) {
        return Ok(());
    }
    let temporary = workdir_dir.join(format!(".{WORKDIR_CWD_FILE}.{}.tmp", Uuid::now_v7()));
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
fn workdir_cwd_is_current(path: &Path, expected: &[u8]) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes == expected)
}

#[cfg(windows)]
fn workdir_cwd_is_current(path: &Path, expected: &[u8]) -> bool {
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
