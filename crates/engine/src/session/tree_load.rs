//! Delegation-tree loading: gates, folds, harvests, and product delivery.

use super::*;

/// Lazy-tree state for one root session (§3.1 of the storage spec).
#[derive(Debug, Default)]
pub(crate) struct TreeState {
    /// The one-time bulk child pass completed *and* the engine accepted its
    /// products. Publication order is
    /// `Unloaded -> Loading -> (products applied + index durable) -> Loaded`:
    /// a stale fold, a failed pass or a rejected observer never sets this.
    pub(super) loaded: bool,
    /// Direct children keyed by parent session, covering the whole tree. Edges
    /// come from child `origin` metadata, not from directory nesting: every
    /// descendant of a root is filed one level under `<root>/subagents/`.
    pub(super) children: HashMap<SessionId, Vec<SessionId>>,
    /// Terminal run statuses observed per child, persisted into `index.json`.
    pub(super) terminal_runs: HashMap<SessionId, BTreeMap<String, SessionStatus>>,
    /// Restart-stable tree grants folded in by the bulk load (4.3), retained so
    /// grant rebuilds stay O(cached data) instead of O(logs).
    pub(super) grants: Vec<cookie_agent_protocol::TreeApprovalGrant>,
    /// Children whose logs carry goal-producer state (4.4).
    pub(super) producer_sessions: Vec<SessionId>,
    /// What the loaded children reported about their own role as delegation
    /// parents, harvested by the one fold so a nested registry rebuild never
    /// reopens a child log (4.1.3).
    pub(super) parent_facts: HashMap<SessionId, ParentRunFacts>,
    /// Children whose `index.json` entry disagrees with that session's own
    /// authoritative `metadata` cache. The disagreement is the one thing a
    /// summary cache cannot explain to itself — the session moved after the
    /// index was written — so it marks the cached summary *pre-load data* that
    /// [`SessionStore::summary`] refuses to serve before the tree is complete
    /// (§3.4). Cleared by the bulk pass, which installs the fold's answer.
    pub(super) stale_seeds: HashSet<SessionId>,
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
    pub(super) fn for_root(root: SessionId) -> Self {
        Self {
            root,
            delegations: Vec::new(),
            grants: Vec::new(),
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
    pub(super) fn from_projection(projection: &SessionProjection) -> Self {
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
pub(super) struct TreeGate {
    pub(super) status: Mutex<TreeLoadStatus>,
    pub(super) ready: Condvar,
}

impl TreeGate {
    #[cfg(test)]
    pub(super) fn status(&self) -> TreeLoadStatus {
        *self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Publishes a new status and wakes every waiter.
    pub(super) fn settle(&self, status: TreeLoadStatus) {
        *self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = status;
        self.ready.notify_all();
    }

    /// Claims the right to drive one load cycle, or `None` when the tree is
    /// already loaded.
    pub(super) fn acquire(&self) -> Option<TreeLoadStatus> {
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
pub(super) struct PendingLoads {
    /// Engine hook installed by [`SessionStore::set_tree_load_observer`].
    pub(super) observer: Option<Arc<dyn TreeLoadObserver>>,
    /// Products of finished passes, keyed by root, each claimed exactly once.
    pub(super) queued: HashMap<SessionId, Arc<TreeLoadProducts>>,
    /// Roots whose own driver thread is delivering their products right now, so
    /// a drain from another thread can never claim them a second time.
    pub(super) driving: HashSet<SessionId>,
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
pub(super) const TREE_LOAD_RACES: usize = 3;

/// How many driver turns one [`SessionStore::load_tree`] call allows itself:
/// a fold, plus a couple of handovers around a rejected load.
pub(super) const TREE_LOAD_TURNS: usize = 4;

/// Outcome of trying to publish one fold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Install {
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
pub(super) struct TreeFold {
    pub(super) products: TreeLoadProducts,
    /// Children the pass based itself on, in directory order.
    pub(super) children: Vec<SessionId>,
    /// Log fingerprint taken before each child was folded.
    pub(super) fingerprints: HashMap<SessionId, LogFingerprint>,
    /// Every grant the pass saw, restart-stable or not (§4.3).
    pub(super) tree_grants: Vec<cookie_agent_protocol::TreeApprovalGrant>,
    pub(super) edges: HashMap<SessionId, Vec<SessionId>>,
    pub(super) terminal_runs: HashMap<SessionId, BTreeMap<String, SessionStatus>>,
}

impl TreeFold {
    pub(super) fn for_root(root: SessionId) -> Self {
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
pub(super) struct TreeLoadDriver {
    pub(super) gate: Arc<TreeGate>,
    pub(super) root: SessionId,
    pub(super) settled: bool,
}

impl TreeLoadDriver {
    pub(super) fn new(gate: Arc<TreeGate>, root: SessionId) -> Self {
        TREE_LOAD_DRIVERS.with(|drivers| drivers.borrow_mut().push(root));
        Self {
            gate,
            root,
            settled: false,
        }
    }

    /// Publishes the outcome of this turn and wakes every waiter.
    pub(super) fn settle(&mut self, status: TreeLoadStatus) {
        self.settled = true;
        self.gate.settle(status);
    }

    /// Completes a load: the durable install is already in place, so the store
    /// can be marked loaded before the gate releases the waiters.
    pub(super) fn publish_loaded(&mut self, store: &SessionStore) {
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

#[cfg(test)]
#[derive(Debug)]
pub(super) struct EvictionTransitionHook {
    pub(super) reached: Mutex<Option<tokio::sync::oneshot::Sender<SessionId>>>,
    pub(super) release: Mutex<std::sync::mpsc::Receiver<()>>,
}

/// Test-only load hook. Hand-written `Debug`: the payload is a `dyn Fn`, and the
/// store derives `Debug`.
#[cfg(test)]
#[derive(Default)]
#[allow(clippy::type_complexity)]
pub(super) struct TreeLoadReadHook(std::sync::Mutex<Option<Arc<dyn Fn(SessionId) + Send + Sync>>>);

#[cfg(test)]
impl TreeLoadReadHook {
    pub(super) fn installed(&self) -> bool {
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
pub(super) struct PublishHook {
    pub(super) reached: std::sync::mpsc::Sender<SessionId>,
    pub(super) release: std::sync::mpsc::Receiver<()>,
}

/// Marks a scope in which reading child event logs is legal (§3.3).
pub(super) struct TreeLoadReads;

impl TreeLoadReads {
    pub(super) fn begin() -> Self {
        TREE_LOAD_READS.with(|depth| depth.set(depth.get() + 1));
        Self
    }

    pub(super) fn active() -> bool {
        TREE_LOAD_READS.with(|depth| depth.get() > 0)
    }
}

impl Drop for TreeLoadReads {
    fn drop(&mut self) {
        TREE_LOAD_READS.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

thread_local! {
    /// Roots this thread is loading, or is delivering the products of. Re-entering
    /// the store from an observer callback must never wait on its own gate.
    pub(super) static TREE_LOAD_DRIVERS: std::cell::RefCell<Vec<SessionId>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

thread_local! {
    /// Depth of the enclosing `load_tree` read phase on this thread.
    pub(super) static TREE_LOAD_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl SessionStore {
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
    pub(super) fn run_tree_load_read_hook(&self, root: SessionId) {
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
    pub(super) fn install_publish_hook_for_test(
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
    pub(super) fn tree_load_pending_for(&self, id: SessionId) -> bool {
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
    pub(super) fn tree_is_loaded(&self, root: SessionId) -> bool {
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
    pub(super) fn tree_gate(&self, root: SessionId) -> Arc<TreeGate> {
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
    pub(super) fn mark_tree_loaded(&self, root: SessionId) {
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
    pub(super) fn run_tree_load(
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
    pub(super) fn fold_tree(&self, root: SessionId) -> Result<TreeLoadProducts, SessionError> {
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
    pub(super) fn harvest_tree(&self, root: SessionId) -> Result<TreeFold, SessionError> {
        let mut fold = TreeFold::for_root(root);
        let _reads = TreeLoadReads::begin();
        // Ownership is the tree's: the root's lock is the only one this build
        // keeps, so the load pass is where an older build's per-child locks go.
        self.remove_legacy_child_locks(root);
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
    pub(super) fn install_tree_fold(
        &self,
        root: SessionId,
        fold: &TreeFold,
    ) -> Result<Install, SessionError> {
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
    pub(super) fn publish_load_products(
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
    pub(super) fn deliver_queued_load(
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

    pub(super) fn deliver_products(
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
    pub(super) fn claim_queued_load(&self, root: SessionId) -> Option<Arc<TreeLoadProducts>> {
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
    pub(super) fn redeliver_unclaimed_load(&self, root: SessionId) -> Result<(), SessionError> {
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
    pub(super) fn cached_parent_facts(&self, id: SessionId) -> Option<ParentRunFacts> {
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

    /// Seeded child ids, summaries and the terminal-run cache from a root's
    /// persisted `subagents/index.json` (never from a child log).
    ///
    /// The file is a cache, so each entry is validated against the directory it
    /// claims before anything is trusted from it, and duplicates are ignored. A
    /// stale or corrupt entry is simply dropped: the directory scan or a later
    /// tree load recovers the child, and a bad index stays a non-event (§3.4,
    /// review L9). Seeded summaries carry `usage: None` — the derived per-session
    /// total is only ever served from a log that was actually folded (D8).
    pub(super) fn seed_tree_from_index(&self, root: SessionId) {
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

    /// Test-only: the load gate state of one root, as the store sees it.
    #[cfg(test)]
    pub(crate) fn tree_load_status(&self, root: SessionId) -> TreeLoadStatus {
        self.tree_gate(root).status()
    }
}
