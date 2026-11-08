//! `session.compact` on a session whose agent is an ACP agent
//! (`docs/site/specs/acp-agents.md` section 15, compaction): cookie's own
//! compaction needs a model, which an ACP run has none of, so the request
//! goes to the agent as its `compact` command when it offers one, and is
//! refused otherwise. Automatic compaction is the model loop's and never
//! runs for an ACP run.

use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload as Event, RunOutcome, RunStartParams,
    SessionCompactResult, SessionId, SessionStatus,
};

use super::super::{Engine, EngineError};

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
            .state()
            .acp_commands()
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
        #[cfg(test)]
        self.wait_for_acp_compaction_start_hook().await;
        // Watched before the run starts, so its end cannot slip past.
        let mut changes = self.watch_subagent(session);
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
        // The run's records start at its `run_started`, which is all that is
        // read of the log: a revert before the run started reuses sequences
        // below the tip as it was, so this is read once it has. A run that
        // already gave way to another is looked for in the whole log.
        let start = self
            .inner
            .store
            .log(session)?
            .state()
            .run_started(run)
            .map(|event| event.seq);
        loop {
            // Woken by every change of the session: the log is read only
            // once the projection has the run ended.
            let projection = self.inner.store.get(session)?;
            let ended = projection
                .runs
                .get(&run)
                .is_none_or(|run| run.status != SessionStatus::Running);
            // A queued input can have started another run, so this one's end may
            // be older than the latest turn.
            let outcome = if ended {
                let records = match start {
                    Some(start) => projection.log.history_range(start..)?,
                    None => projection.log.history()?,
                };
                let (mut started, mut outcome) = (false, None);
                for event in records {
                    let event = event?;
                    if event.run_id != Some(run) {
                        continue;
                    }
                    match &event.payload {
                        Event::RunStarted { .. } => started = true,
                        Event::RunFinished {
                            outcome: cookie_agent_protocol::Open::Known(finished),
                        } => outcome = Some(finished.clone()),
                        _ => {}
                    }
                }
                // A revert since cut the run away: it ended with nothing to show.
                if !started {
                    return Ok(Some(SessionCompactResult {
                        compacted: false,
                        cancellation_reason: Some("compaction reverted".to_owned()),
                    }));
                }
                outcome
            } else {
                None
            };
            match outcome {
                // A run's end signals the session's subagent waits.
                None => super::super::delegation::subagent_changed_or_recheck(&mut changes).await,
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
