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
            pending_loads: Mutex::new(PendingLoads::default()),
            residency: Mutex::new(SessionResidency::default()),
            ownership: Mutex::new(HashMap::new()),
            adoption_locks: Mutex::new(HashMap::new()),
            publish_locks: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
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
        Ok(envelope)
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
        if self.flat_layout {
            return Ok(());
        }
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
        if self.flat_layout || self.mutation_held() {
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
        if self.flat_layout {
            return Ok(());
        }
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
        if self.flat_layout {
            return matches!(self.cached_origin(id), Some(SessionOrigin::Root) | None);
        }
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
    /// cache. Flat-layout stores have no lazy tree to complete, so their caches
    /// are the answer by construction (§2.3, §3.4).
    fn summary_cache_is_current(&self, id: SessionId) -> bool {
        if self.flat_layout {
            return true;
        }
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
        if self.flat_layout {
            return Ok(());
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
        if self.flat_layout {
            return Ok(());
        }
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
            return Ok(children);
        }
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
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
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
        EVENTS_FILE, IndexedChild, LAYOUT_MARKER_FILE, PROJECT_CWD_FILE, SESSION_META_FILE,
        SESSIONS_ROOT_DIR, SUBAGENT_INDEX_FILE, SUBAGENT_INDEX_VERSION, SUBAGENTS_DIR,
        SessionError, SessionStore, SessionSummary, SubagentIndex, TREE_LOAD_RACES,
        TreeLoadObserver, TreeLoadProducts, TreeLoadStatus, meta_path, projection,
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
        // (a) Opening the store, and every startup bookkeeping pass it runs, reads
        // no child event log at all: the counts are of `events.jsonl` opens, not
        // of load attempts, so nothing can hide behind a cached flag.
        for child in [child, grandchild] {
            assert_eq!(observer.log_open_count(child), 0, "startup read {child}");
        }
        assert_eq!(observer.root_snapshots().len(), 1, "only the root log");
        assert_eq!(observer.all_summaries().len(), 3, "summaries from caches");
        for child in [child, grandchild] {
            assert_eq!(
                observer.log_open_count(child),
                0,
                "startup passes read no child log"
            );
        }
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
        // Listing *is* a tree use, so it triggers the load and reports the
        // failure instead of serving the pre-load cache (review L14).
        assert!(observer.children(root).is_err());
        assert!(observer.children(child).is_err());
        assert!(
            !observer.is_tree_loaded(root),
            "an unreadable child must not report a loaded tree"
        );
        assert!(
            observer.log_open_count(child) > 0,
            "a listing that cannot complete the tree attempts the child log"
        );
        assert_eq!(
            observer.root_snapshots().len(),
            1,
            "the root log is read by the startup pass only"
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
        assert_eq!(
            observer.log_open_count(grandchild),
            1,
            "the first completed fold reads each child exactly once"
        );
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
        for child in [first, second] {
            assert_eq!(store.log_open_count(child), 0, "opening reads no child");
        }
        store.get(root).expect("open the root");
        assert!(store.is_tree_loaded(root), "opening a root loads its tree");
        for child in [first, second] {
            // (b) exactly one fold per child, counted at the log itself.
            assert_eq!(store.log_open_count(child), 1, "one fold of {child}");
            assert!(!store.is_resident(child), "children stay out of residency");
            assert!(store.session_exists(child));
        }
        assert_eq!(store.children(root).expect("children").len(), 2);

        // Child logs go unreadable: everything the tree offers is already
        // cached, so further queries keep working and no second load happens.
        for child in [first, second] {
            fs::set_permissions(
                store.session_dir(child).join("events.jsonl"),
                fs::Permissions::from_mode(0o000),
            )
            .expect("unreadable child log");
        }
        assert_eq!(store.children(root).expect("children").len(), 2);
        assert_eq!(store.tree(root).expect("cached tree").children.len(), 2);
        assert_eq!(store.get(root).expect("root again").meta.session_id, root);
        assert_eq!(store.tree(root).expect("tree again").children.len(), 2);
        // (c) further uses of the loaded tree open no child log at all: were the
        // logs still readable this would be indistinguishable from a re-read.
        for child in [first, second] {
            assert_eq!(
                store.log_open_count(child),
                1,
                "the pass runs once for {child}"
            );
        }
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
        assert_eq!(
            store.log_open_count(child),
            2,
            "one bulk fold, plus the read that serves the requested child itself"
        );
        assert!(!store.is_resident(child));
        assert!(index.is_file(), "the load rebuilt the child summary cache");
    }

    /// Review F2: `summary` and `fork` are the two paths that can answer a child
    /// from memory alone — a cached/seeded summary, or a placement copy — and
    /// both of those answers are exactly what a bulk pass also produces. Neither
    /// may be served, nor a fork filed, before the tree is complete.
    #[test]
    fn cached_summary_and_fork_do_not_bypass_the_tree_load() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        let mut seeded = owner.summary(child).expect("child summary");
        // Content the durable index can carry and a fold cannot invent.
        seeded.meta.title =
            Some(SessionTitle::new("seeded by the durable index").expect("test title"));
        drop(owner);

        fs::write(
            workdir_dir(&data, &cwd)
                .join(root.to_string())
                .join(SUBAGENTS_DIR)
                .join(SUBAGENT_INDEX_FILE),
            serde_json::to_vec(&SubagentIndex {
                version: SUBAGENT_INDEX_VERSION,
                children: vec![IndexedChild {
                    summary: seeded,
                    terminal_runs: BTreeMap::new(),
                }],
            })
            .expect("encode index"),
        )
        .expect("forge the index");

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        assert!(
            !store.is_tree_loaded(root),
            "a cold store has completed nothing"
        );
        let served = store.summary(child).expect("child summary");
        assert_ne!(
            served.meta.title.as_ref().map(SessionTitle::as_str),
            Some("seeded by the durable index"),
            "the seeded summary is pre-load data and must not be what a load produced"
        );
        assert!(
            store.is_tree_loaded(root),
            "finishing the tree came before the memory-only answer"
        );
        assert_eq!(
            store.log_open_count(child),
            1,
            "and it cost the one read+fold §3.3 allows, not a second read"
        );

        // Same shape from a second cold store: forking a directly-addressed child
        // must not file it into a tree that was never flattened.
        let verifier = SessionStore::open(&data, &cwd).expect("verifier store");
        assert!(!verifier.is_tree_loaded(root));
        let fork_id = verifier
            .fork(child, test_user_input_seq(&verifier, child), test_origin())
            .expect("fork the child");
        assert!(
            verifier.is_tree_loaded(root),
            "the fork completed its source's tree before taking `mutation`"
        );
        assert!(
            verifier.tree_members(root).contains(&fork_id),
            "and filed the result into that tree"
        );
    }

    /// Observer recording what each completed load delivered, so a test can see
    /// the products the engine would have folded in — and can reject them.
    #[derive(Default)]
    struct CapturingObserver {
        deliveries: Mutex<Vec<(SessionId, Vec<String>, usize)>>,
        reject: AtomicBool,
    }

    impl TreeLoadObserver for CapturingObserver {
        fn tree_loaded(
            &self,
            products: Arc<TreeLoadProducts>,
        ) -> Result<(), crate::runtime::EngineError> {
            if self.reject.load(Ordering::SeqCst) {
                return Err(crate::runtime::EngineError::ActorStopped);
            }
            let mut references = products.artifact_refs.iter().cloned().collect::<Vec<_>>();
            references.sort();
            self.deliveries.lock().expect("observer lock").push((
                products.root,
                references,
                products.grants.len(),
            ));
            Ok(())
        }
    }

    fn test_origin() -> cookie_agent_protocol::EventOrigin {
        cookie_agent_protocol::EventOrigin::new("engine:test").expect("static origin is valid")
    }

    /// One child's `events.jsonl` open count, for tests that create children in a
    /// loop.
    fn child_log_opens(store: &SessionStore, child: SessionId) -> usize {
        store.log_open_count(child)
    }

    /// (d) Concurrent triggers on one root coalesce: one fold per child and one
    /// delivery of the products, no matter how many threads asked.
    #[test]
    fn concurrent_tree_load_triggers_fold_each_child_once() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        let children = (0..4)
            .map(|_| persist_test_session_with_origin(&owner, delegated_origin(root, root, 1)))
            .collect::<Vec<_>>();
        drop(owner);

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        let observer = Arc::new(CapturingObserver::default());
        store
            .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
            .expect("install observer");
        let threads = (0..8)
            .map(|_| {
                let store = Arc::clone(&store);
                thread::spawn(move || store.load_tree(root).expect("load the tree"))
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().expect("loader thread");
        }
        for child in &children {
            assert_eq!(
                child_log_opens(&store, *child),
                1,
                "eight triggers folded {child} exactly once"
            );
        }
        assert_eq!(
            observer.deliveries.lock().expect("observer lock").len(),
            1,
            "one completed load delivers its products once"
        );
    }

    /// (e) The install check is real: every load pass verifies its fold against the
    /// per-child fingerprints it read, and a fold that still agrees is published on
    /// the first install — never retried. A verification that spuriously reported
    /// staleness (a fingerprint derived from anything the fold itself perturbs)
    /// would burn the retry budget here. Review L1.
    #[test]
    fn tree_load_verifies_its_fold_and_publishes_without_retrying() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        drop(owner);

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        let observer = Arc::new(CapturingObserver::default());
        store
            .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
            .expect("install observer");
        let passes = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&passes);
        store.install_tree_load_read_hook_for_test(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        });

        store.get(root).expect("the uncontended load publishes");
        assert_eq!(
            passes.load(Ordering::SeqCst),
            1,
            "one read pass: the fold verified clean and was installed"
        );
        assert_eq!(store.log_open_count(child), 1, "the child was folded once");
        assert_eq!(
            observer.deliveries.lock().expect("observer lock").len(),
            1,
            "one delivery, in the completion path the install owns"
        );
    }

    /// Observer that keeps whole product sets, so a test can assert on *what* a
    /// fold published instead of only on how many times the pass ran.
    #[derive(Default)]
    struct ProductsObserver {
        deliveries: Mutex<Vec<Arc<TreeLoadProducts>>>,
    }

    impl TreeLoadObserver for ProductsObserver {
        fn tree_loaded(
            &self,
            products: Arc<TreeLoadProducts>,
        ) -> Result<(), crate::runtime::EngineError> {
            self.deliveries
                .lock()
                .expect("observer lock")
                .push(products);
            Ok(())
        }
    }

    /// A committed user title: the cheapest append whose effect is visible in the
    /// summary a load publishes, and one that a fold-ignored payload would not be.
    fn append_title(store: &SessionStore, id: SessionId, title: &str) {
        store
            .append(
                id,
                None,
                test_origin(),
                EventPayload::SessionTitleCommitted {
                    change: SessionTitleChange::UserSet {
                        title: SessionTitle::new(title).expect("test title"),
                        client_rename_id: cookie_agent_protocol::ClientRenameId::new(format!(
                            "rename-{}",
                            SessionId::new_v7()
                        ))
                        .expect("test rename id"),
                    },
                    input_through_seq: 1,
                },
            )
            .expect("commit the title");
    }

    /// (e′) Review L1, gate F1: the per-child fingerprints are load-bearing, not
    /// decorative. A real append through the store's own write path, landing in
    /// the window between the read phase and the install, makes that fold stale.
    /// The pass re-reads, publishes the fresh fold, and the stale one never
    /// marks the tree loaded.
    #[test]
    fn stale_child_append_causes_refold() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        drop(owner);

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        let observer = Arc::new(ProductsObserver::default());
        store
            .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
            .expect("install observer");

        let passes = Arc::new(AtomicUsize::new(0));
        // What the load state looked like when the re-fold began.
        let loaded_at_refold = Arc::new(AtomicBool::new(true));
        let driver = Arc::downgrade(&store);
        let counted = Arc::clone(&passes);
        let recorded = Arc::clone(&loaded_at_refold);
        store.install_tree_load_read_hook_for_test(move |root| {
            let Some(store) = driver.upgrade() else {
                return;
            };
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                // Move the log this fold already fingerprinted, for real.
                store.open_for_write(child).expect("adopt the child");
                append_title(&store, child, "appended during the read phase");
                // Keep the appended record in the log's writer: the retry must be
                // attributable to this append alone, not to a background sync.
                store
                    .get_resident(child)
                    .expect("resident child")
                    .log
                    .pause_background_sync_for_test();
            } else {
                recorded.store(store.is_tree_loaded(root), Ordering::SeqCst);
            }
        });

        store.get(root).expect("the retried load publishes");
        assert_eq!(
            passes.load(Ordering::SeqCst),
            2,
            "one stale fold, then the re-fold that installed"
        );
        assert!(
            !loaded_at_refold.load(Ordering::SeqCst),
            "`loaded` was never published from the fold that lost the race"
        );
        assert!(store.is_tree_loaded(root), "the fresh fold completed");
        let deliveries = observer.deliveries.lock().expect("observer lock").clone();
        assert_eq!(deliveries.len(), 1, "one publish, from the fresh fold");
        let published = deliveries[0]
            .summaries
            .iter()
            .find(|summary| summary.meta.session_id == child)
            .expect("the child was published");
        assert_eq!(
            published.meta.title.as_ref().map(SessionTitle::as_str),
            Some("appended during the read phase"),
            "the published state contains the event the stale fold missed"
        );
        assert_eq!(
            child_log_opens(&store, child),
            1,
            "the retry folded the child's resident projection, not a second read of its log"
        );
    }

    /// (e″) Review L1, gate F1: when every unlocked fold loses the race the pass
    /// falls back to folding with `mutation` held, and a fold that is still
    /// unprovable there is *reported*, never published: nothing is installed,
    /// nothing is delivered, and the root stays retryable.
    ///
    /// How this is forced, honestly: the writer is the test hook, which appends
    /// from the load's own thread and so re-enters `mutation`. That is the only
    /// deterministic way to move a log under that lock — an in-process writer on
    /// another thread blocks on it, so in production the condition this branch
    /// guards against is a writer outside the process. The test therefore pins
    /// the fallback's fail-closed behaviour, not a claim about which writer
    /// caused it.
    #[test]
    fn a_fold_that_always_loses_the_race_reports_the_tree_contended() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        drop(owner);

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        let observer = Arc::new(ProductsObserver::default());
        store
            .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
            .expect("install observer");

        let passes = Arc::new(AtomicUsize::new(0));
        let driver = Arc::downgrade(&store);
        let counted = Arc::clone(&passes);
        store.install_tree_load_read_hook_for_test(move |_| {
            let Some(store) = driver.upgrade() else {
                return;
            };
            if !store.is_owned(child) {
                store.open_for_write(child).expect("adopt the child");
                store
                    .get_resident(child)
                    .expect("resident child")
                    .log
                    .pause_background_sync_for_test();
            }
            // Append after every read phase, so no fold's fingerprints survive
            // to its install — including the one taken with `mutation` held.
            append_title(
                &store,
                child,
                &format!("racing append {}", SessionId::new_v7()),
            );
            counted.fetch_add(1, Ordering::SeqCst);
        });

        let error = store
            .load_tree(root)
            .expect_err("a fold that cannot be proven must not publish");
        assert!(
            matches!(error, SessionError::TreeContended(contended) if contended == root),
            "the budget-exhausted pass reports the tree, got {error:?}"
        );
        assert_eq!(
            passes.load(Ordering::SeqCst),
            TREE_LOAD_RACES + 1,
            "every unlocked fold was retried, then one pass with `mutation` held"
        );
        assert!(
            !store.is_tree_loaded(root),
            "an unprovable fold installs nothing, and never marks the tree loaded"
        );
        assert_eq!(
            store.tree_load_status(root),
            TreeLoadStatus::Unloaded,
            "the root stays joinable and retryable after the failure"
        );
        assert!(
            observer
                .deliveries
                .lock()
                .expect("observer lock")
                .is_empty(),
            "no products were delivered from an unproven fold"
        );
    }

    /// Review L2 + L3: an observer rejection leaves the tree retryable with its
    /// products still queued, so the next trigger applies them without a second
    /// fold, and each product set is claimed exactly once.
    #[test]
    fn rejected_tree_load_stays_retryable_and_redelivers_its_products() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        drop(owner);

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        let observer = Arc::new(CapturingObserver::default());
        observer.reject.store(true, Ordering::SeqCst);
        store
            .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
            .expect("install observer");
        assert!(
            matches!(store.load_tree(root), Err(SessionError::TreeRejected(_))),
            "a rejected load fails the access that triggered it"
        );
        assert!(
            !store.is_tree_loaded(root),
            "a rejected load never marks the tree loaded"
        );
        assert_eq!(store.tree_load_status(root), TreeLoadStatus::Pending);

        observer.reject.store(false, Ordering::SeqCst);
        store.load_tree(root).expect("the retry delivers");
        assert!(store.is_tree_loaded(root));
        assert_eq!(child_log_opens(&store, child), 1, "no second fold");
        assert_eq!(
            observer.deliveries.lock().expect("observer lock").len(),
            1,
            "the queued products were claimed exactly once"
        );
    }

    /// Review L3 / D5: a load that finished before the engine installed its hook
    /// is not lost — installing the observer delivers everything queued.
    #[test]
    fn loads_completed_without_an_observer_are_delivered_when_it_arrives() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        drop(owner);

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        store
            .load_tree(root)
            .expect("a load can complete with no observer installed");
        assert!(store.is_tree_loaded(root), "the store-side pass completed");
        let observer = Arc::new(CapturingObserver::default());
        store
            .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
            .expect("install observer");
        assert_eq!(
            observer.deliveries.lock().expect("observer lock").clone(),
            vec![(root, vec![], 0)],
            "the queued products reached the late observer once"
        );
        assert_eq!(
            child_log_opens(&store, child),
            1,
            "and were never re-folded"
        );
    }

    /// Review F5: `Loaded` may not be observable while a load's products are
    /// still unapplied. A pass that completed with no hook installed leaves them
    /// *owed*, so an access on that tree is refused until the hook that arrives
    /// later has taken them — and it takes them from the queued fold, never from
    /// a second one.
    #[test]
    fn a_loaded_tree_owes_its_products_until_the_hook_has_taken_them() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        drop(owner);

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        store
            .load_tree(root)
            .expect("a store-side pass completes with no hook installed");
        assert!(
            store.is_tree_loaded(root),
            "the durable install is in place"
        );
        assert!(
            store
                .pending_loads
                .lock()
                .expect("pending load lock")
                .queued
                .contains_key(&root),
            "and its products were never applied"
        );

        // The hook arrives and refuses what it was handed: the tree is loaded,
        // the products are not applied.
        let observer = Arc::new(CapturingObserver::default());
        observer.reject.store(true, Ordering::SeqCst);
        assert!(
            matches!(
                store.set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>),
                Err(SessionError::TreeRejected(_))
            ),
            "a refused delivery fails the install that triggered it"
        );
        assert_eq!(store.tree_load_status(root), TreeLoadStatus::Loaded);
        assert!(
            observer
                .deliveries
                .lock()
                .expect("observer lock")
                .is_empty(),
            "nothing has been applied yet"
        );

        // Serving this tree would answer from state the engine never received.
        assert!(
            matches!(store.get(root), Err(SessionError::TreeRejected(_)),),
            "an access on a loaded-but-unapplied tree fails closed"
        );
        assert!(
            observer
                .deliveries
                .lock()
                .expect("observer lock")
                .is_empty(),
            "the refused delivery applied nothing"
        );

        observer.reject.store(false, Ordering::SeqCst);
        let projection = store
            .get(root)
            .expect("the owed products are delivered, then served");
        assert_eq!(projection.meta.session_id, root);
        assert_eq!(
            observer.deliveries.lock().expect("observer lock").len(),
            1,
            "the queued products reached the hook exactly once"
        );
        assert!(
            !store
                .pending_loads
                .lock()
                .expect("pending load lock")
                .queued
                .contains_key(&root),
            "and nothing is owed any more"
        );
        assert_eq!(
            child_log_opens(&store, child),
            1,
            "delivering them never re-folded the child (§3.3)"
        );
    }

    /// Observer that answers `parent_run_facts` the way the delegation registry
    /// does — from inside a load's delivery — and records what that cost.
    struct FactsAtDelivery {
        store: std::sync::Weak<SessionStore>,
        parent: SessionId,
        /// `(child log opens so far, facts resolved)` at delivery time.
        seen: Mutex<Vec<(usize, bool)>>,
    }

    impl TreeLoadObserver for FactsAtDelivery {
        fn tree_loaded(
            &self,
            _products: Arc<TreeLoadProducts>,
        ) -> Result<(), crate::runtime::EngineError> {
            let store = self
                .store
                .upgrade()
                .expect("the store outlives its own observer");
            let resolved = store
                .parent_run_facts(self.parent)
                .expect("registry lookup")
                .is_some();
            self.seen
                .lock()
                .expect("delivery probe lock")
                .push((store.log_open_count(self.parent), resolved));
            Ok(())
        }
    }

    /// §4.1.3, review F3: a delegation-parent's facts can never be paid for with
    /// a cold child log read. Only that tree's bulk pass folds child logs, and it
    /// carries the facts with it — including into the delivery where the registry
    /// is rebuilt, which is where a second, differently-timed fold used to happen.
    #[test]
    fn parent_facts_of_an_unloaded_child_never_open_its_log() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        // `child` is itself a delegation parent: the registry asks it for facts.
        let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        let grandchild = persist_test_session_with_origin(&owner, delegated_origin(root, child, 2));
        drop(owner);

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        assert_eq!(store.log_open_count(child), 0, "a cold store read nothing");
        assert!(
            store.parent_run_facts(child).expect("resolvable").is_none(),
            "facts for an unloaded tree are reported as not knowable yet"
        );
        assert_eq!(
            store.log_open_count(child),
            0,
            "and answering that way opened no child log (§3.3)"
        );

        let observer = Arc::new(FactsAtDelivery {
            store: Arc::downgrade(&store),
            parent: child,
            seen: Mutex::new(Vec::new()),
        });
        store
            .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
            .expect("install observer");
        store.load_tree(root).expect("the tree load runs");

        let seen = observer.seen.lock().expect("delivery probe lock").clone();
        assert_eq!(
            seen,
            vec![(1, true)],
            "delivery resolved the facts from the fold it already made"
        );
        for descendant in [child, grandchild] {
            assert_eq!(
                store.log_open_count(descendant),
                1,
                "{descendant} was folded exactly once by the bulk pass (§3.3)"
            );
        }
        assert!(
            store
                .parent_run_facts(child)
                .expect("facts after the load")
                .is_some(),
            "and they stay resolvable afterwards"
        );
        assert_eq!(
            store.log_open_count(child),
            1,
            "resolving them opened nothing"
        );
        // The root is the one parent the pass cannot know about: its own log is
        // still the answer, and that read is legal.
        assert!(
            store.parent_run_facts(root).expect("root facts").is_some(),
            "a root's facts still fall back to the root's log"
        );
    }

    /// Review L9: a hostile `subagents/index.json` is dropped, not trusted — the
    /// duplicate, a child with no directory, and another root filed as a child
    /// none of them become live, and none of them move a real root's placement.
    #[test]
    fn subagent_index_entries_are_validated_before_they_are_trusted() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        let other_root = persist_test_session(&owner);
        let child_summary = owner.summary(child).expect("child summary");
        let other_summary = owner.summary(other_root).expect("other root summary");
        drop(owner);

        let phantom = SessionId::new_v7();
        let entry = |summary: SessionSummary| IndexedChild {
            summary,
            terminal_runs: BTreeMap::new(),
        };
        let forged = SubagentIndex {
            version: SUBAGENT_INDEX_VERSION,
            children: vec![
                entry(child_summary.clone()),
                entry(child_summary.clone()),
                entry(SessionSummary {
                    meta: cookie_agent_protocol::SessionMeta {
                        session_id: phantom,
                        ..child_summary.meta.clone()
                    },
                    usage: None,
                    usage_rollup: Default::default(),
                    agent_usage: BTreeMap::new(),
                }),
                entry(other_summary),
            ],
        };
        let index_path = workdir_dir(&data, &cwd)
            .join(root.to_string())
            .join(SUBAGENTS_DIR)
            .join(SUBAGENT_INDEX_FILE);
        fs::write(
            &index_path,
            serde_json::to_vec(&forged).expect("encode forged index"),
        )
        .expect("forge the index");

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        assert_eq!(
            store
                .children(root)
                .expect("children")
                .into_iter()
                .map(|listed| listed.session_id)
                .collect::<Vec<_>>(),
            vec![child],
            "duplicates, phantoms and foreign roots are ignored"
        );
        assert!(
            !store.session_exists(phantom),
            "an index cannot make a nonexistent session live"
        );
        assert_eq!(
            store.root_of(other_root).expect("placement"),
            other_root,
            "an index entry cannot re-home a real root under another tree"
        );
        assert_eq!(store.root_of(child).expect("placement"), root);
    }

    /// Review L10: the durable index names a child only once that child's
    /// directory is published, so a crash before the publish leaves no entry
    /// pointing at a directory that does not exist.
    #[test]
    fn the_durable_index_names_only_published_children() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).expect("store");
        let root = persist_test_session(&store);
        let child = create_buffered_child(&store, root);
        let index_path = store
            .session_dir(root)
            .join(SUBAGENTS_DIR)
            .join(SUBAGENT_INDEX_FILE);
        let child_dir = workdir_dir(&data, &cwd)
            .join(root.to_string())
            .join(SUBAGENTS_DIR)
            .join(child.to_string());
        assert!(!child_dir.exists(), "a buffered child has no directory");

        let indexed = |store: &SessionStore| {
            store
                .read_subagent_index(root)
                .map(|index| {
                    index
                        .children
                        .into_iter()
                        .map(|entry| entry.summary.meta.session_id)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            indexed(&store),
            Vec::new(),
            "an unpublished child is not named by the durable index"
        );
        // A tree load in that window must not smuggle it into the cache either.
        store.children(root).expect("listing the root tree");
        assert_eq!(
            indexed(&store),
            Vec::new(),
            "the load's cache rewrite skips children with no directory"
        );

        store
            .persist_buffered_session(child)
            .expect("publish the child");
        assert!(child_dir.is_dir(), "the child directory is published");
        assert_eq!(
            indexed(&store),
            vec![child],
            "the published child lands in the durable index"
        );
        assert!(index_path.is_file());
    }

    /// Creates a delegated child that stays buffered (published nowhere).
    fn create_buffered_child(store: &SessionStore, root: SessionId) -> SessionId {
        let session_id = SessionId::new_v7();
        store
            .create(
                session_id,
                test_origin(),
                EventPayload::SessionCreated {
                    origin: delegated_origin(root, root, 1),
                    cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test")
                        .expect("static identity is valid"),
                    creation_selection: crate::test_support::run_selection("test"),
                    creation_agent: Box::new(crate::test_support::agent_snapshot(
                        "test",
                        AgentMode::Primary,
                    )),
                    runtime_revision: RuntimeRevision::new(format!("sha256:{}", "1".repeat(64)))
                        .expect("static revision is valid"),
                    catalog_revision: CatalogRevision::new(format!("sha256:{}", "2".repeat(64)))
                        .expect("static revision is valid"),
                    provider_state_revision: ProviderStateRevision::new(format!(
                        "sha256:{}",
                        "3".repeat(64)
                    ))
                    .expect("static revision is valid"),
                    model_revision: ModelRevision::new(format!("sha256:{}", "4".repeat(64)))
                        .expect("static revision is valid"),
                    agent_revision: AgentRevision::new(format!("sha256:{}", "5".repeat(64)))
                        .expect("static revision is valid"),
                    recipe_registry_revision: RecipeRegistryRevision::new(format!(
                        "sha256:{}",
                        "6".repeat(64)
                    ))
                    .expect("static revision is valid"),
                    manifest_revision: cookie_agent_protocol::ModelSnapshotRevision::new(format!(
                        "sha256:{}",
                        "7".repeat(64)
                    ))
                    .expect("static revision is valid"),
                },
            )
            .expect("create a buffered child");
        session_id
    }

    /// Review L11 / D6: publishing merges onto a directory that is *exactly* the
    /// `subagents` scaffold. Anything else — an empty directory, or a name the
    /// prepared files would have to overwrite — fails closed instead of replacing
    /// bytes another writer owns.
    #[test]
    fn publishing_refuses_a_destination_that_is_not_the_scaffold() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).expect("store");
        let session_id = create_buffered_test_session(&store);
        let final_dir = store.session_dir(session_id);

        // An empty directory is not a scaffold: `.all()` on an empty listing must
        // not be read as "only the scaffold lives here".
        fs::create_dir_all(&final_dir).expect("pre-create the destination");
        assert!(
            matches!(
                store.persist_buffered_session(session_id),
                Err(SessionError::SessionLocked(id)) if id == session_id
            ),
            "an empty directory is not a scaffold"
        );
        fs::remove_dir_all(&final_dir).expect("clear the destination");

        // A foreign file beside the scaffold means somebody else owns it, and the
        // prepared `events.jsonl` must not be renamed over it.
        fs::create_dir_all(final_dir.join(SUBAGENTS_DIR)).expect("scaffold");
        write_private_test_file(&final_dir.join(EVENTS_FILE), "someone else's log");
        assert!(
            matches!(
                store.persist_buffered_session(session_id),
                Err(SessionError::SessionLocked(id)) if id == session_id
            ),
            "a destination collision is rejected, never renamed over"
        );
        assert_eq!(
            fs::read_to_string(final_dir.join(EVENTS_FILE)).expect("foreign log"),
            "someone else's log",
            "the rejected publish left the foreign bytes alone"
        );
    }

    /// D6, the case the scaffold exists for: a root whose child was filed while
    /// the root was still buffered merges its prepared files into that scaffold.
    #[test]
    fn publishing_merges_onto_a_bare_subagents_scaffold() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).expect("store");
        let root = create_buffered_test_session(&store);
        // Filing a child under an unpublished root brings the scaffold into being.
        let child = persist_test_session_with_origin(&store, delegated_origin(root, root, 1));
        let root_dir = store.session_dir(root);
        assert!(
            root_dir.join(SUBAGENTS_DIR).is_dir(),
            "the child created the root scaffold"
        );
        assert!(
            !root_dir.join(EVENTS_FILE).is_file(),
            "the root itself is not published yet"
        );

        store.persist_buffered_session(root).expect("publish root");
        assert!(
            root_dir.join(EVENTS_FILE).is_file(),
            "the merged log is durable"
        );
        assert!(
            root_dir.join(SESSION_META_FILE).is_file(),
            "the merged metadata cache is durable"
        );
        assert!(
            root_dir
                .join(SUBAGENTS_DIR)
                .join(child.to_string())
                .is_dir(),
            "the scaffold child survived the merge"
        );
        assert_eq!(store.children(root).expect("children").len(), 1);
        assert_eq!(store.root_of(child).expect("placement"), root);
    }

    /// D8 / review L12: seeding from `subagents/index.json` drops the derived
    /// per-session `usage` while keeping the durable rollups.
    #[test]
    fn seeded_child_summaries_drop_derived_usage() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let root = persist_test_session(&owner);
        let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
        let mut summary = owner.summary(child).expect("child summary");
        summary.usage = Some(Usage {
            input_tokens: Some(4_321),
            ..Default::default()
        });
        summary.usage_rollup.input_tokens = 99;
        drop(owner);

        fs::write(
            workdir_dir(&data, &cwd)
                .join(root.to_string())
                .join(SUBAGENTS_DIR)
                .join(SUBAGENT_INDEX_FILE),
            serde_json::to_vec(&SubagentIndex {
                version: SUBAGENT_INDEX_VERSION,
                children: vec![IndexedChild {
                    summary,
                    terminal_runs: BTreeMap::new(),
                }],
            })
            .expect("encode index"),
        )
        .expect("forge the index");

        let store = SessionStore::open(&data, &cwd).expect("cold store");
        let seeded = store.summary(child).expect("seeded summary");
        assert!(
            seeded.usage.is_none(),
            "a cold child must not serve derived usage"
        );
        assert_eq!(
            seeded.usage_rollup.input_tokens, 99,
            "the durable rollup is still served"
        );
        assert_eq!(
            store
                .children(root)
                .expect("children")
                .into_iter()
                .next()
                .expect("listed child")
                .usage,
            None,
            "and neither does the listing"
        );
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
                .expect("children")
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
                .expect("children")
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
                .expect("children")
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
