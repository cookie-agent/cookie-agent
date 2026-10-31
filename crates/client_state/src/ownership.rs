//! Whether this connection owns a session it opens. The daemon answers
//! `session.resume` for a session another cookie process holds with
//! `session_owned_elsewhere`; such a session is shown as a read-only
//! snapshot, which no live tail keeps current.

use cookie_agent_protocol::{SessionId, SessionMeta, SessionResumeParams, SessionResumeResult};
use cookie_agent_protocol_transport::{Client, ClientError};

/// What resuming a session said about who owns it.
pub enum SessionOwnershipOutcome {
    /// This connection's daemon owns it now.
    Owned(Box<SessionMeta>),
    /// Another cookie process owns it: a read-only snapshot.
    Foreign,
    /// Resuming failed for another reason; the session stays read-only.
    Failed(String),
}

impl SessionOwnershipOutcome {
    /// Classifies a `session.resume` result.
    pub fn of_resume(result: Result<SessionResumeResult, ClientError>) -> Self {
        match result {
            Ok(result) => Self::Owned(Box::new(result.session)),
            Err(error) if session_owned_by_another_process(&error) => Self::Foreign,
            Err(error) => Self::Failed(error.to_string()),
        }
    }
}

/// Resumes `session_id` and classifies who owns it.
pub async fn classify_session(client: &Client, session_id: SessionId) -> SessionOwnershipOutcome {
    SessionOwnershipOutcome::of_resume(
        client
            .resume_session(SessionResumeParams { session_id })
            .await,
    )
}

/// Whether the daemon refused because another cookie process owns the
/// session.
pub fn session_owned_by_another_process(error: &ClientError) -> bool {
    error.is(&cookie_agent_protocol::errors::SESSION_OWNED_ELSEWHERE)
}
