//! `session.delete`: deleting a root session's whole tree from disk.

use std::collections::HashSet;

use cookie_agent_protocol::SessionId;

use crate::runtime::{Engine, EngineError};
use crate::session::SessionError;

/// The sessions a tree deletion deleted, and the ACP sessions each owned.
type DeletedTree = (
    Vec<SessionId>,
    Vec<(SessionId, Vec<cookie_agent_protocol::AcpSessionRecord>)>,
);

impl Engine {
    /// Deletes the tree rooted at `session_id` (events, metadata, artifacts,
    /// and every delegated session) and returns the deleted sessions, the
    /// root first. Every connection is told through `sessions.changed`.
    ///
    /// Refused for a delegated session ([`EngineError::NotARootSession`]),
    /// while a session of the tree is busy
    /// ([`EngineError::SessionTreeRunning`]), while another process holds
    /// the tree ([`EngineError::SessionOwnedByAnotherProcess`]), and when
    /// the root's last event is no longer `expected_last_seq`
    /// ([`EngineError::StaleDelete`]). A refusal changes nothing.
    pub async fn delete_session(
        &self,
        session_id: SessionId,
        expected_last_seq: u64,
    ) -> Result<Vec<SessionId>, EngineError> {
        self.ensure_not_shutting_down()?;
        let root = self.inner.store.root_of(session_id)?;
        if root != session_id {
            return Err(EngineError::NotARootSession {
                session_id,
                root_session_id: root,
            });
        }
        let deleted = if self.inner.store.holds_tree(root) {
            // On the root's actor, after every request already queued for
            // it. Nothing reconciles afterwards: there is nothing left.
            self.on_actor_unreconciled(root, move |engine| {
                engine.delete_tree_direct(root, expected_last_seq)
            })
            .await?
        } else {
            // A tree this process does not hold is not adopted: adoption
            // would repair it, appending events, before it is deleted.
            let engine = self.clone();
            crate::blocking::off_worker(move || engine.delete_tree_direct(root, expected_last_seq))?
        };
        let (deleted, acp_sessions) = deleted;
        self.forget_deleted_tree(root, &deleted);
        // The agents' own transcripts go with the sessions: each session's
        // ACP sessions are closed and deleted where the agent can.
        for (session, records) in acp_sessions {
            self.delete_acp_sessions(session, records).await;
        }
        for session in &deleted {
            self.close_acp_session(*session).await;
        }
        Ok(deleted)
    }

    /// Deletes the tree rooted at `root`; returns the deleted sessions and
    /// the ACP sessions each of them owned.
    fn delete_tree_direct(
        &self,
        root: SessionId,
        expected_last_seq: u64,
    ) -> Result<DeletedTree, EngineError> {
        // Serializes with an adoption of the tree, which would otherwise be
        // caught halfway.
        let adoption_lock = self.inner.store.adoption_lock(root);
        let _adoption = adoption_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let deletion = self
            .inner
            .store
            .begin_tree_deletion(root)
            .map_err(|error| match error {
                SessionError::SessionLocked(_) => EngineError::SessionOwnedByAnotherProcess(root),
                SessionError::TreeBusy(_) => EngineError::SessionTreeRunning { root, busy: root },
                error => error.into(),
            })?;
        // A running tree is refused as running even when the caller's guard
        // is stale too: a streaming run moves the root past what was listed.
        let members = deletion.members().iter().copied().collect::<HashSet<_>>();
        if deletion.was_held()
            && let Some(busy) = self.busy_member(&members).or(deletion.running_member())
        {
            return Err(EngineError::SessionTreeRunning { root, busy });
        }
        let current_last_seq = deletion.root_last_seq();
        if current_last_seq != expected_last_seq {
            return Err(EngineError::StaleDelete {
                session_id: root,
                current_last_seq,
            });
        }
        // Nothing of the tree may keep running or keep a file open in it.
        self.kill_deleted_background_jobs(&members);
        self.inner.uploads.forget_tree(root);
        self.inner.artifacts.forget_tree(root);
        let acp_sessions = deletion
            .members()
            .iter()
            .filter_map(|member| {
                let projection = self.inner.store.get(*member).ok()?;
                let records =
                    crate::runtime::acp::lifecycle::own_records(&projection.log.event_snapshot());
                (!records.is_empty()).then_some((*member, records))
            })
            .collect();
        Ok((deletion.commit()?, acp_sessions))
    }

    /// Whether a session of the tree rooted at `root` has an active run, a
    /// compaction, or a place in the delegation queue, as a delete would
    /// find it.
    pub(crate) fn tree_busy(&self, root: SessionId) -> bool {
        let root_of = |session: &SessionId| self.inner.store.root_of(*session).unwrap_or(*session);
        let busy =
            |sessions: Vec<SessionId>| sessions.iter().any(|session| root_of(session) == root);
        let running = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|run| run.session)
            .collect::<Vec<_>>();
        let compacting = self
            .inner
            .compaction
            .in_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let queued = self
            .inner
            .delegation
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .copied()
            .collect::<Vec<_>>();
        busy(running) || busy(compacting) || busy(queued)
    }

    /// A session of `members` with an active run, a compaction or run
    /// start holding it, or a place in the delegation queue.
    fn busy_member(&self, members: &HashSet<SessionId>) -> Option<SessionId> {
        let running = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|run| run.session)
            .find(|session| members.contains(session));
        running
            .or_else(|| {
                self.inner
                    .compaction
                    .in_progress
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .copied()
                    .find(|session| members.contains(session))
            })
            .or_else(|| {
                self.inner
                    .delegation
                    .queue
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .copied()
                    .find(|session| members.contains(session))
            })
    }

    /// Forgets what the engine keeps for a deleted tree's sessions. The
    /// actors go too; requests still queued on them find no session.
    fn forget_deleted_tree(&self, root: SessionId, deleted: &[SessionId]) {
        let deleted = deleted.iter().copied().collect::<HashSet<_>>();
        self.forget_deleted_sessions(&deleted);
        self.inner
            .approvals
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(session, _), _| !deleted.contains(session));
        self.inner.grant_journals.forget(root);
        self.rebuild_visible_tree_grants();
        self.inner
            .sessions
            .forks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(source, _), (_, fork)| !deleted.contains(source) && !deleted.contains(fork));
        self.inner
            .sessions
            .steer_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|session, _| !deleted.contains(session));
        self.inner
            .compaction
            .manual
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|session, _| !deleted.contains(session));
    }
}
