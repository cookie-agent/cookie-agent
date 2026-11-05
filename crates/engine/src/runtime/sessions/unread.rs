//! Unread root sessions (`docs/site/specs/unread-sessions.md`): marked by
//! the daemon when a push signal for a tree comes while nobody is present
//! on it, read again once a client is, and set by the user with
//! `session.unread.set`. The store keeps them in `<data_root>/unread.json`.

use cookie_agent_protocol::{
    SessionId, SessionOrigin, SessionUnreadSetParams, SessionUnreadSetResult, SessionUnreadSkip,
    SessionUnreadSkipReason,
};

use crate::runtime::Engine;

impl Engine {
    /// Marks root session `root` unread: a push signal for its tree came
    /// while nobody was present on it.
    pub fn mark_unread(&self, root: SessionId) {
        self.set_unread_announced(&[root], true);
    }

    /// Marks the root of `session`'s tree read: a client is present on it.
    pub fn mark_tree_read(&self, session: SessionId) {
        let root = self.root_session_of(session);
        self.set_unread_announced(&[root], false);
    }

    /// `session.unread.set`: marks the root sessions it names, and says
    /// which it left.
    pub fn set_session_unread(&self, params: SessionUnreadSetParams) -> SessionUnreadSetResult {
        let (roots, skipped) = self.root_sessions_among(params.session_ids);
        SessionUnreadSetResult {
            changed: self.set_unread_announced(&roots, params.unread),
            skipped,
        }
    }

    /// The root sessions among `session_ids`, and why each other one is
    /// left: the user marks roots only (`session.unread.set`,
    /// `session.pin.set`).
    pub(super) fn root_sessions_among(
        &self,
        session_ids: Vec<SessionId>,
    ) -> (Vec<SessionId>, Vec<SessionUnreadSkip>) {
        let mut roots = Vec::new();
        let mut skipped = Vec::new();
        for session_id in session_ids {
            match self.session_meta(session_id).map(|meta| meta.origin) {
                Ok(SessionOrigin::Delegated {
                    root_session_id, ..
                }) => skipped.push(SessionUnreadSkip {
                    session_id,
                    reason: SessionUnreadSkipReason::NotARootSession,
                    root_session_id: Some(root_session_id),
                }),
                Ok(_) => roots.push(session_id),
                Err(_) => skipped.push(SessionUnreadSkip {
                    session_id,
                    reason: SessionUnreadSkipReason::SessionNotFound,
                    root_session_id: None,
                }),
            }
        }
        (roots, skipped)
    }

    /// Marks `roots` and tells every client of those that changed.
    fn set_unread_announced(&self, roots: &[SessionId], unread: bool) -> Vec<SessionId> {
        let changed = self.inner.store.set_unread(roots, unread);
        for root in &changed {
            self.inner.store.announce_live_waiting(*root);
        }
        changed
    }
}
