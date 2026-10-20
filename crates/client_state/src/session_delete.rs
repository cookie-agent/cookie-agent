//! Deleting a session tree (`session.delete`, client protocol 1.5): whether
//! the daemon can, the call from the session the user confirmed, and what a
//! client shows for its outcome. Every client asks the user first.

use cookie_agent_protocol::{
    Client, ClientError, SemVer, SessionDeleteParams, SessionId, SessionMeta,
};

use crate::StateStore;

/// The client protocol minor that added `session.delete`.
pub const SESSION_DELETE_MINOR: u32 = 5;

/// What a client shows when the daemon predates `session.delete`.
pub const DAEMON_TOO_OLD_TO_DELETE: &str = "The daemon is too old to delete sessions.";

/// Whether a daemon whose handshake reported `version` can delete sessions.
/// Before the handshake nothing is known, so nothing is sent.
pub fn daemon_can_delete_sessions(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| {
        version
            .shared_minor(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
            .is_some_and(|minor| minor >= SESSION_DELETE_MINOR)
    })
}

/// How a delete ended.
#[derive(Debug)]
pub enum SessionDeleteOutcome {
    /// The daemon deleted these sessions, the root first.
    Deleted(Vec<SessionId>),
    /// The daemon is older than 1.5; nothing was sent, or it did not know
    /// the method.
    DaemonTooOld,
    /// The daemon refused or failed; its message, hint, and detail say why.
    Failed(ClientError),
}

/// The params that delete `session`'s tree, guarded by the last event the
/// user saw when confirming.
pub fn delete_params(session: &SessionMeta) -> SessionDeleteParams {
    SessionDeleteParams {
        session_id: session.session_id,
        expected_last_seq: session.last_event_seq,
    }
}

/// Deletes the tree of the root session the user confirmed, as `session`
/// showed it.
pub async fn delete_session(client: &Client, session: &SessionMeta) -> SessionDeleteOutcome {
    if !daemon_can_delete_sessions(client.daemon_version()) {
        return SessionDeleteOutcome::DaemonTooOld;
    }
    SessionDeleteOutcome::of(client.delete_session(delete_params(session)).await)
}

impl SessionDeleteOutcome {
    /// Classifies a `session.delete` result.
    pub fn of(result: Result<cookie_agent_protocol::SessionDeleteResult, ClientError>) -> Self {
        match result {
            Ok(result) => Self::Deleted(result.deleted),
            Err(ClientError::DaemonTooOld { .. }) => Self::DaemonTooOld,
            Err(error) => Self::Failed(error),
        }
    }
}

impl StateStore {
    /// Drops everything this store holds for a session the daemon removed.
    pub fn forget_session(&mut self, session_id: SessionId) {
        self.sessions.remove(&session_id);
        self.physical_events.remove(&session_id);
        self.quarantined_sessions.remove(&session_id);
        self.replays.remove(&session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_daemon_of_minor_five_or_later_deletes() {
        assert!(!daemon_can_delete_sessions(None));
        assert!(!daemon_can_delete_sessions(Some(SemVer::new(1, 4, 9))));
        assert!(daemon_can_delete_sessions(Some(SemVer::new(1, 5, 0))));
        assert!(daemon_can_delete_sessions(Some(SemVer::new(1, 9, 0))));
        assert!(!daemon_can_delete_sessions(Some(SemVer::new(2, 5, 0))));
    }

    #[test]
    fn an_unknown_method_reads_as_a_daemon_too_old() {
        assert!(matches!(
            SessionDeleteOutcome::of(Err(ClientError::DaemonTooOld {
                method: "session.delete".into()
            })),
            SessionDeleteOutcome::DaemonTooOld
        ));
    }
}
