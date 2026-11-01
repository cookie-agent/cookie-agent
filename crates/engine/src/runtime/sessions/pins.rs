//! Pinned root sessions (`docs/site/specs/session-pins-and-filters.md`):
//! the user pins and unpins them with `session.pin.set`, and every listing
//! holds them whatever its window. The store keeps them in
//! `<data_root>/pinned.json`.

use cookie_agent_protocol::{SessionPinSetParams, SessionPinSetResult};

use crate::runtime::Engine;

impl Engine {
    /// `session.pin.set`: pins or unpins the root sessions it names, tells
    /// every client of those that changed, and says which it left.
    pub fn set_session_pinned(&self, params: SessionPinSetParams) -> SessionPinSetResult {
        let (roots, skipped) = self.root_sessions_among(params.session_ids);
        let changed = self.inner.store.set_pinned(&roots, params.pinned);
        for root in &changed {
            self.inner.store.announce_live_waiting(*root);
        }
        SessionPinSetResult { changed, skipped }
    }
}
