//! `session.compact` on a session whose agent is an ACP agent
//! (`docs/site/specs/acp-agents.md` section 15, compaction): cookie's own
//! compaction needs a model, which an ACP run has none of, so the request
//! goes to the agent as its `compact` command when it offers one, and is
//! refused otherwise. Automatic compaction is the model loop's and never
//! runs for an ACP run.

use std::time::Duration;

use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload as Event, RunOutcome, RunStartParams,
    SessionCompactResult, SessionId, SessionStatus,
};

use super::super::{Engine, EngineError};

/// How often a forwarded compaction checks whether its run has ended.
const POLL: Duration = Duration::from_millis(50);

impl Engine {
    /// The compaction of `session` when its agent is an ACP agent: the
    /// agent's `compact` command as a run of its own (`/compact <focus>`),
    /// waited for. `None` when the agent is not an ACP agent.
    pub(crate) async fn compact_acp_session(
        &self,
        session: SessionId,
        focus: Option<&str>,
        origin: EventOrigin,
    ) -> Result<Option<SessionCompactResult>, EngineError> {
        let Some(offers) = self.session_acp_offers(session)? else {
            return Ok(None);
        };
        let projection = self.inner.store.get(session)?;
        let offers_compact = projection
            .log
            .event_snapshot()
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                Event::AcpSessionUpdated { update } => update.commands.as_ref(),
                _ => None,
            })
            .is_some_and(|commands| commands.iter().any(|command| command.name == "compact"));
        if !offers_compact {
            return Err(EngineError::AcpUnsupported(format!(
                "`{}` does not offer compaction",
                offers.id
            )));
        }
        if projection.status == SessionStatus::Running {
            return Err(EngineError::SessionRunning(session));
        }
        let text = match focus.map(str::trim).filter(|focus| !focus.is_empty()) {
            Some(focus) => format!("/compact {focus}"),
            None => "/compact".to_owned(),
        };
        let run = self
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session,
                    client_run_id: ClientRunId::new(format!("compact:{}", SessionId::new_v7()))
                        .expect("a compaction's run key is bounded"),
                    selection: self.session_model_selection(session)?,
                    content: text.as_str().into(),
                },
                origin,
            )
            .await?
            .run_id;
        loop {
            let outcome = self
                .inner
                .store
                .get(session)?
                .log
                .event_snapshot()
                .iter()
                .rev()
                .find_map(|event| match &event.payload {
                    Event::RunFinished {
                        outcome: cookie_agent_protocol::Open::Known(outcome),
                    } if event.run_id == Some(run) => Some(outcome.clone()),
                    _ => None,
                });
            match outcome {
                None => tokio::time::sleep(POLL).await,
                Some(RunOutcome::Completed { .. }) => {
                    return Ok(Some(SessionCompactResult {
                        compacted: true,
                        cancellation_reason: None,
                    }));
                }
                Some(RunOutcome::Failed { error, .. }) => {
                    return Err(EngineError::Acp(error.as_str().to_owned()));
                }
                Some(_) => {
                    return Ok(Some(SessionCompactResult {
                        compacted: false,
                        cancellation_reason: Some("compaction cancelled".to_owned()),
                    }));
                }
            }
        }
    }
}
