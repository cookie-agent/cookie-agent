//! Deleting a whole session tree from disk (`session.delete`).
//!
//! A deletion renames the root directory to a tombstone in the work dir,
//! which is the commit point, forgets the tree, and then removes the
//! tombstone. A crash between the rename and the removal leaves only the
//! tombstone, which the next [`SessionStore::open`] removes.

use std::{collections::HashSet, fs, path::Path};

use cookie_agent_protocol::{SessionId, SessionOrigin, SessionStatus};
use uuid::Uuid;

use super::{
    MutationGuard, SESSION_META_FILE, SessionError, SessionLocation, SessionStore, TreeOwnership,
};
use crate::events::fsync_directory;
use crate::ownership::{HeldLock, SessionOwnership, try_acquire};

/// Name prefix of a tree directory a deletion moved aside.
pub(crate) const TOMBSTONE_PREFIX: &str = ".deleted-";

/// A tree deletion that has passed its ownership checks and holds the
/// store's exclusive mutation lock until it commits or is dropped. It lives
/// on the thread that began it.
pub(crate) struct TreeDeletion<'a> {
    store: &'a SessionStore,
    mutation: MutationGuard<'a>,
    root: SessionId,
    /// Whether this process held the tree before the deletion began.
    held: bool,
    /// The tree lock this deletion took for a tree nobody held.
    taken_lock: Option<HeldLock>,
    /// The tree's sessions, the root first.
    members: Vec<SessionId>,
    root_last_seq: u64,
}

impl TreeDeletion<'_> {
    /// Every session of the tree, the root first.
    pub(crate) fn members(&self) -> &[SessionId] {
        &self.members
    }

    /// The root's last event sequence as the session list shows it.
    pub(crate) fn root_last_seq(&self) -> u64 {
        self.root_last_seq
    }

    /// Whether this process held the tree before the deletion: only then can
    /// a session of it be running here. A tree whose lock was free has no
    /// live run anywhere, whatever status a crashed writer left behind.
    pub(crate) fn was_held(&self) -> bool {
        self.held
    }

    /// The first session of the tree whose resident projection is running.
    pub(crate) fn running_member(&self) -> Option<SessionId> {
        let residency = self
            .store
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.members.iter().copied().find(|id| {
            residency
                .resident
                .get(id)
                .is_some_and(|session| session.status == SessionStatus::Running)
        })
    }

    /// Moves the tree aside, forgets it, tells every connection, and removes
    /// it from disk. A failure before the move changes nothing on disk.
    pub(crate) fn commit(self) -> Result<Vec<SessionId>, SessionError> {
        let Self {
            store,
            mutation,
            root,
            held: _,
            taken_lock,
            members,
            root_last_seq: _,
        } = self;
        let root_dir = store.tree_root_dir(root);
        // No handle may stay open inside the directory: Windows refuses to
        // rename a directory with an open file.
        {
            let residency = store
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for id in &members {
                if let Some(session) = residency.resident.get(id) {
                    session.log.suspend_writer();
                }
            }
        }
        store
            .read_only_logs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0
            .retain(|(cached, _, _)| !members.contains(cached));
        let tombstone = if root_dir.exists() {
            let tombstone = store
                .workdir_dir
                .join(format!("{TOMBSTONE_PREFIX}{root}-{}", Uuid::now_v7()));
            rename_directory(&root_dir, &tombstone).map_err(|source| SessionError::Io {
                path: root_dir.clone(),
                source,
            })?;
            // The rename is the commit point; make it durable before anyone
            // is told the tree is gone.
            if let Err(error) = fsync_directory(&store.workdir_dir) {
                eprintln!("session tree {root} deleted, but syncing the work dir failed: {error}");
            }
            Some(tombstone)
        } else {
            None
        };
        store.forget_tree(root, &members);
        drop(taken_lock);
        #[cfg(windows)]
        {
            // The Windows lock is a sidecar beside the tree, released above.
            let _ = fs::remove_file(crate::ownership::owner_lock_path(&root_dir));
        }
        for id in &members {
            store.notify_index_change(
                cookie_agent_protocol::SessionsChangedNotification::Removed { session_id: *id },
            );
        }
        drop(mutation);
        if let Some(tombstone) = tombstone {
            remove_tombstone(&tombstone);
        }
        Ok(members)
    }
}

impl SessionStore {
    /// Whether this process holds the tree rooted at `root`, settled.
    pub(crate) fn holds_tree(&self, root: SessionId) -> bool {
        self.ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .trees
            .get(&root)
            .is_some_and(TreeOwnership::is_settled)
    }

    /// Begins deleting the tree rooted at `root`: takes the exclusive
    /// mutation lock and makes sure this process holds the tree, taking its
    /// lock when nobody does. Another process's lock is
    /// [`SessionError::SessionLocked`]; an adoption in flight is
    /// [`SessionError::TreeBusy`].
    pub(crate) fn begin_tree_deletion(
        &self,
        root: SessionId,
    ) -> Result<TreeDeletion<'_>, SessionError> {
        let mutation = self.lock_mutation();
        self.ensure_open()?;
        let root_dir = self.tree_root_dir(root);
        let state = self
            .ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .trees
            .get(&root)
            .map(|tree| match tree {
                TreeOwnership::PendingPublish { .. } | TreeOwnership::Owned { .. } => Some(true),
                TreeOwnership::Adopting { .. } => None,
                TreeOwnership::Foreign => Some(false),
            });
        let (held, taken_lock) = match state {
            Some(Some(true)) => (true, None),
            Some(None) => return Err(SessionError::TreeBusy(root)),
            Some(Some(false)) | None => {
                if !root_dir.is_dir() {
                    return Err(SessionError::Missing(root));
                }
                match try_acquire(&root_dir) {
                    Ok(SessionOwnership::Owned(lock)) => (false, Some(lock)),
                    Ok(SessionOwnership::Foreign) => {
                        self.mark_tree_foreign(root);
                        return Err(SessionError::SessionLocked(root));
                    }
                    Err(source) if !root_dir.is_dir() => {
                        drop(source);
                        return Err(SessionError::Missing(root));
                    }
                    Err(source) => {
                        return Err(SessionError::Io {
                            path: root_dir,
                            source,
                        });
                    }
                }
            }
        };
        let root_last_seq = self.root_last_seq(root, &root_dir)?;
        Ok(TreeDeletion {
            store: self,
            mutation,
            root,
            held,
            taken_lock,
            members: self.tree_session_ids(root),
            root_last_seq,
        })
    }

    /// The root's `last_event_seq` from the same source a listing reads:
    /// its resident projection, its evicted summary, or its `metadata`.
    fn root_last_seq(&self, root: SessionId, root_dir: &Path) -> Result<u64, SessionError> {
        {
            let residency = self
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(session) = residency.resident.get(&root) {
                return Ok(session.meta.last_event_seq);
            }
            if let Some(summary) = residency.evicted.get(&root) {
                return Ok(summary.meta.last_event_seq);
            }
        }
        if !root_dir.join(SESSION_META_FILE).exists() {
            return Err(SessionError::Missing(root));
        }
        Ok(self.read_meta(root, root_dir)?.last_event_seq)
    }

    /// Every session filed in the tree rooted at `root`, the root first:
    /// what this process knows of the tree, every child directory on disk,
    /// and resident children that are not published yet.
    fn tree_session_ids(&self, root: SessionId) -> Vec<SessionId> {
        let mut members = vec![root];
        let mut seen = HashSet::from([root]);
        let resident = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .values()
            .filter(|session| {
                matches!(
                    &session.meta.origin,
                    SessionOrigin::Delegated { root_session_id, .. } if *root_session_id == root
                )
            })
            .map(|session| session.meta.session_id)
            .collect::<Vec<_>>();
        for id in self
            .tree_members(root)
            .into_iter()
            .chain(self.child_dir_ids(root))
            .chain(resident)
        {
            if seen.insert(id) {
                members.push(id);
            }
        }
        members
    }

    /// Drops everything the store holds for the tree's sessions. The tree's
    /// ownership entry goes too, which releases its lock and invalidates
    /// every log's write capability at once.
    fn forget_tree(&self, root: SessionId, members: &[SessionId]) {
        let deleted = members.iter().copied().collect::<HashSet<_>>();
        {
            let mut residency = self
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for id in members {
                residency.resident.remove(id);
                residency.evicted.remove(id);
                residency.stale_meta_caches.remove(id);
            }
        }
        self.trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&root);
        self.pending_loads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .queued
            .remove(&root);
        self.locations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|id, location| {
                !deleted.contains(id)
                    && !matches!(location, SessionLocation::Child { root: of } if *of == root)
            });
        {
            let mut ownership = self
                .ownership
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            ownership.trees.remove(&root);
            ownership
                .writable
                .retain(|id, of| !deleted.contains(id) && *of != root);
            ownership
                .adopting
                .retain(|id, of| !deleted.contains(id) && *of != root);
        }
        // Dropping a session's senders ends every live tail of it.
        self.subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|id, _| !deleted.contains(id));
        for gates in [
            &self.adoption_locks,
            &self.publish_locks,
            &self.tree_publish_locks,
            &self.index_locks,
        ] {
            gates
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|id, _| !deleted.contains(id));
        }
        self.tree_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&root);
    }
}

/// Removes every tombstone a deletion left in `workdir_dir`, as when a
/// daemon stopped between moving a tree aside and removing it.
pub(super) fn sweep_tombstones(workdir_dir: &Path) {
    let Ok(entries) = fs::read_dir(workdir_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(TOMBSTONE_PREFIX)
        {
            remove_tombstone(&entry.path());
        }
    }
}

/// Removes a tombstone and everything in it. A failure is reported and
/// leaves it for the next start's sweep.
fn remove_tombstone(path: &Path) {
    let removed = if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match removed {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => eprintln!(
            "deleted session tree at {} could not be removed: {error}",
            path.display()
        ),
    }
}

/// Renames a tree directory. On Windows a scanner or indexer can hold a file
/// of it open for a moment, so a denied rename is retried briefly.
fn rename_directory(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        const ATTEMPTS: usize = 50;
        const BACKOFF: std::time::Duration = std::time::Duration::from_millis(25);
        for attempt in 0..ATTEMPTS {
            match fs::rename(from, to) {
                Ok(()) => return Ok(()),
                Err(error)
                    if attempt + 1 < ATTEMPTS
                        && super::cache::windows_replace_is_contended(&error) =>
                {
                    std::thread::sleep(BACKOFF);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("rename attempts are nonzero")
    }
    #[cfg(not(windows))]
    fs::rename(from, to)
}
