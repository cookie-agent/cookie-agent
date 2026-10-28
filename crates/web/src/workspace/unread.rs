//! Marking root sessions read or unread (`docs/site/specs/unread-sessions.md`):
//! the daemon keeps the state and announces each change, which the session
//! list shows; the page never clears it on its own.

use cookie_agent_protocol::SessionId;
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;

impl Workspace {
    /// Whether any root session is unread: the rail's Chat button says so.
    pub fn any_unread(self) -> bool {
        self.sessions
            .with(|sessions| sessions.iter().any(|session| session.unread))
    }

    /// Marks `sessions` unread or read; the status line says how many
    /// changed.
    pub fn set_unread(self, sessions: Vec<SessionId>, unread: bool) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client.set_session_unread(sessions, unread).await {
                Ok(result) => self
                    .status
                    .set(cookie_agent_client_state::unread::marked_message(
                        result.changed.len(),
                        unread,
                    )),
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }
}
