use std::collections::HashSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::{
    ApprovalStatus, EventOrigin, RunCancelResult, RunId, RunRecallSteerResult, RunStartParams,
    RunStartResult, RunSteerResult, RunToolStdinParams, RunToolStdinResult, SessionId,
    SessionStatus,
};

use super::{
    ActiveRun, ApprovalTerminal, Engine, EngineError, Event, SessionCommand, UserInputInterception,
    approval_projection::{approval_records, approval_run_id},
    helpers::safe_error,
    mailbox::pending_inputs,
};
use crate::tool_api::StdinWrite;

impl Engine {
    pub async fn start_run(
        &self,
        params: RunStartParams,
        origin: EventOrigin,
    ) -> Result<RunStartResult, EngineError> {
        let session = params.session_id;
        self.request(session, |reply| SessionCommand::Start {
            params,
            origin,
            admission: None,
            reply,
        })
        .await
    }

    pub async fn steer(
        &self,
        run_id: RunId,
        input: String,
        origin: EventOrigin,
    ) -> Result<RunSteerResult, EngineError> {
        let active = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        match self.intercept_user_input(active.session, input).await? {
            UserInputInterception::Accepted {
                input,
                original_input,
            } => {
                let session = active.session;
                self.on_actor(session, move |engine| {
                    engine.active_run_in(session, run_id)?;
                    if !engine.run_is_running(session, run_id)? {
                        return Ok(RunSteerResult {
                            accepted: false,
                            handled_reason: None,
                        });
                    }
                    if let Some(original_input) = original_input {
                        engine.append_direct(
                            session,
                            Some(run_id),
                            origin.clone(),
                            Event::UserInputTransformed {
                                original_input,
                                input: input.clone(),
                            },
                        )?;
                    }
                    engine.append_direct(
                        session,
                        Some(run_id),
                        origin,
                        Event::UserInputAdmitted { input },
                    )?;
                    engine.clear_skill_turn_state(session);
                    Ok(RunSteerResult {
                        accepted: true,
                        handled_reason: None,
                    })
                })
                .await
            }
            UserInputInterception::Handled { reason } => Ok(RunSteerResult {
                accepted: false,
                handled_reason: Some(reason),
            }),
        }
    }

    pub async fn recall_steer(&self, run_id: RunId) -> Result<RunRecallSteerResult, EngineError> {
        let active = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        let session = active.session;
        self.on_actor(session, move |engine| {
            engine.active_run_in(session, run_id)?;
            if !engine.run_is_running(session, run_id)? {
                return Ok(RunRecallSteerResult { recalled: None });
            }
            let recalled =
                pending_inputs(&engine.inner.store.log(session)?.event_snapshot(), run_id)
                    .pop()
                    .map(|pending| pending.input);
            if let Some(input) = &recalled {
                engine.append_direct(
                    session,
                    Some(run_id),
                    super::event_origin("user"),
                    Event::UserInputRecalled {
                        input: input.clone(),
                    },
                )?;
            }
            Ok(RunRecallSteerResult { recalled })
        })
        .await
    }

    pub async fn cancel_run(&self, run_id: RunId) -> Result<RunCancelResult, EngineError> {
        let active = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        let result = self.cancel_on_actor(active.session, run_id).await?;
        let inflight_runs: Vec<_> = {
            let mut inflight = self
                .inner
                .delegation
                .inflight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inflight
                .values_mut()
                .flat_map(|entries| entries.values_mut())
                .filter(|delegate| delegate.parent_run_id == run_id)
                .filter_map(|delegate| {
                    delegate.cancelled = true;
                    delegate.child_run_id
                })
                .collect()
        };
        let delegation_events = self.inner.delegation_events.clone();
        let children = self
            .spawn_admission_blocking(move || Ok::<_, EngineError>(delegation_events.entries()))
            .await?;
        let mut pending = vec![run_id];
        pending.extend(inflight_runs);
        let mut visited = HashSet::new();
        while let Some(parent_run_id) = pending.pop() {
            if !visited.insert(parent_run_id) {
                continue;
            }
            let inflight_children: Vec<_> = {
                let mut inflight = self
                    .inner
                    .delegation
                    .inflight
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                inflight
                    .values_mut()
                    .flat_map(|entries| entries.values_mut())
                    .filter(|delegate| delegate.parent_run_id == parent_run_id)
                    .filter_map(|delegate| {
                        delegate.cancelled = true;
                        delegate.child_run_id
                    })
                    .collect()
            };
            pending.extend(inflight_children);
            for child_run_id in children
                .iter()
                .filter(|entry| entry.reservation.parent_run_id == parent_run_id)
                .filter_map(|entry| entry.child_run_id)
            {
                pending.push(child_run_id);
                if child_run_id == run_id {
                    continue;
                }
                let child_active = {
                    self.inner
                        .sessions
                        .active
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&child_run_id)
                        .cloned()
                };
                if let Some(child_active) = child_active {
                    child_active.cancellation.cancel();
                    let _ = self
                        .cancel_on_actor(child_active.session, child_run_id)
                        .await;
                }
            }
        }
        Ok(result)
    }

    /// Cancels an active run and commits its terminal event under a per-run
    /// gate. The run loop observes the same gate, so concurrent cancellation
    /// paths cannot append two `RunCancelled` records.
    pub(super) fn cancel_run_durably(
        &self,
        run_id: RunId,
        reason: Option<String>,
    ) -> Result<bool, EngineError> {
        let active = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned();
        let Some(active) = active else {
            let session = self
                .inner
                .store
                .all()
                .into_iter()
                .find(|session| session.runs.contains_key(&run_id))
                .ok_or(EngineError::MissingRun(run_id))?;
            let mut committed = false;
            return self.commit_run_cancelled_with_retry(
                session.meta.session_id,
                run_id,
                reason,
                &mut committed,
            );
        };
        active.cancellation.cancel();
        active
            .stdin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let mut committed = active
            .cancelled_committed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.commit_run_cancelled_with_retry(active.session, run_id, reason, &mut committed)
    }

    pub(super) fn append_run_cancelled_once(
        &self,
        active: &ActiveRun,
        run_id: RunId,
        reason: Option<String>,
    ) -> Result<bool, EngineError> {
        let mut committed = active
            .cancelled_committed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.commit_run_cancelled_with_retry(active.session, run_id, reason, &mut committed)
    }

    pub(super) fn commit_run_cancelled_with_retry(
        &self,
        session: SessionId,
        run_id: RunId,
        reason: Option<String>,
        committed: &mut bool,
    ) -> Result<bool, EngineError> {
        let mut last_error = None;
        for _ in 0..3 {
            match self.commit_run_cancelled_once(session, run_id, reason.clone(), committed) {
                Ok(result) => return Ok(result),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.expect("cancellation retry attempts are nonempty"))
    }

    pub(super) fn commit_run_cancelled_once(
        &self,
        session: SessionId,
        run_id: RunId,
        reason: Option<String>,
        committed: &mut bool,
    ) -> Result<bool, EngineError> {
        if *committed {
            return Ok(false);
        }
        // `append_direct` can append to the log before a projection/cache
        // refresh fails. The event log is authoritative in that window.
        if self.run_cancelled_recorded(session, run_id)? {
            *committed = true;
            return Ok(false);
        }
        if self
            .inner
            .store
            .get(session)?
            .runs
            .get(&run_id)
            .is_none_or(|run| run.status != SessionStatus::Running)
        {
            return Ok(false);
        }
        match self.append_direct(
            session,
            Some(run_id),
            super::event_origin("engine:model-loop"),
            Event::RunCancelled {
                reason: reason.as_deref().map(safe_error),
            },
        ) {
            Ok(()) => {
                *committed = true;
                Ok(true)
            }
            Err(error) => {
                if self.run_cancelled_recorded(session, run_id)? {
                    *committed = true;
                    Ok(true)
                } else {
                    Err(error)
                }
            }
        }
    }

    pub(super) fn run_cancelled_recorded(
        &self,
        session: SessionId,
        run_id: RunId,
    ) -> Result<bool, EngineError> {
        Ok(self
            .inner
            .store
            .get(session)?
            .log
            .event_snapshot()
            .iter()
            .any(|event| {
                event.run_id == Some(run_id) && matches!(event.payload, Event::RunCancelled { .. })
            }))
    }

    pub async fn tool_stdin(
        &self,
        params: RunToolStdinParams,
    ) -> Result<RunToolStdinResult, EngineError> {
        let active = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&params.run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(params.run_id))?;
        let session = active.session;
        // Stdin forwards bytes to a running tool and appends only
        // `ToolStdinSubmitted`, which producer reconciliation never reads.
        self.on_actor_unreconciled(session, move |engine| {
            let active = engine.active_run_in(session, params.run_id)?;
            let data = params
                .data
                .map(|encoded| STANDARD.decode(encoded))
                .transpose()?
                .unwrap_or_default();
            let sender = active
                .stdin
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&params.call_id)
                .cloned()
                .ok_or(EngineError::StdinUnavailable)?;
            sender
                .try_send(StdinWrite {
                    data: data.clone(),
                    eof: params.eof,
                })
                .map_err(|_| EngineError::StdinUnavailable)?;
            if params.eof {
                active
                    .stdin
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&params.call_id);
            }
            engine.append_direct(
                session,
                Some(params.run_id),
                super::event_origin("engine:tool-execution"),
                Event::ToolStdinSubmitted {
                    tool_call_id: params.call_id,
                    byte_count: data.len() as u64,
                },
            )?;
            Ok(RunToolStdinResult { accepted: true })
        })
        .await
    }

    /// The active run `run` when it belongs to `session`.
    pub(super) fn active_run_in(
        &self,
        session: SessionId,
        run: RunId,
    ) -> Result<std::sync::Arc<ActiveRun>, EngineError> {
        self.inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run)
            .cloned()
            .filter(|active| active.session == session)
            .ok_or(EngineError::MissingRun(run))
    }

    /// Whether `session`'s projection still reports `run` as running.
    pub(super) fn run_is_running(
        &self,
        session: SessionId,
        run: RunId,
    ) -> Result<bool, EngineError> {
        Ok(self
            .inner
            .store
            .get(session)?
            .runs
            .get(&run)
            .is_some_and(|run| run.status == SessionStatus::Running))
    }

    /// Cancels `run` on `session`'s actor: trips its cancellation, drops its
    /// stdin channels, and cancels the approvals it still has pending.
    async fn cancel_on_actor(
        &self,
        session: SessionId,
        run: RunId,
    ) -> Result<RunCancelResult, EngineError> {
        self.on_actor(session, move |engine| {
            let active = engine.active_run_in(session, run)?;
            active.cancellation.cancel();
            active
                .stdin
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
            let events = engine.inner.store.log(session)?.event_snapshot();
            let pending = approval_records(session, &events)
                .into_values()
                .filter(|record| {
                    matches!(
                        record.status,
                        ApprovalStatus::Pending | ApprovalStatus::Escalated
                    ) && approval_run_id(&events, record.request.approval_id()) == Some(run)
                })
                .map(|record| record.request.approval_id())
                .collect::<Vec<_>>();
            for approval_id in pending {
                engine.approval_terminal_direct(
                    session,
                    run,
                    approval_id,
                    ApprovalTerminal::Cancelled,
                )?;
            }
            Ok(RunCancelResult { cancelled: true })
        })
        .await
    }
}
