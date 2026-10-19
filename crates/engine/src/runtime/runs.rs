use std::sync::Arc;

use cookie_agent_protocol::{
    ApprovalStatus, EventOrigin, RunCancelResult, RunId, RunStartParams, RunStartResult,
    RunSteerResult, SessionId, SessionStatus, ToolCallId, UserContent,
};

use super::{
    ActiveRun, ApprovalTerminal, Engine, EngineError, Event, SessionCommand, UserInputInterception,
    approval_projection::{approval_records, approval_run_id},
    helpers::safe_error,
};

impl Engine {
    pub async fn start_run(
        &self,
        params: RunStartParams,
        origin: EventOrigin,
    ) -> Result<RunStartResult, EngineError> {
        let session = params.session_id;
        self.ensure_appendable(session)?;
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
        content: UserContent,
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
        // Interception can take a while; holding the session's steer order
        // across it admits back-to-back steers in the order they arrived.
        let order = Arc::clone(
            self.inner
                .sessions
                .steer_order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(active.session)
                .or_default(),
        );
        let _in_order = order.lock().await;
        match self.intercept_user_input(active.session, content).await? {
            UserInputInterception::Accepted { content, original } => {
                let session = active.session;
                // A steer reaches the run's own model; a fallback it moved
                // to gets stand-ins for what it does not accept.
                let model = &active.policy.selected_suffix[0].selection.model;
                let _input_artifacts = self
                    .admit_input_media(
                        session,
                        &content,
                        model,
                        &super::uploads::catalog_input_media(&active.policy.runtime, model),
                    )
                    .await?;
                self.on_actor(session, move |engine| {
                    engine.active_run_in(session, run_id)?;
                    if !engine.run_is_running(session, run_id)? {
                        return Ok(RunSteerResult {
                            accepted: false,
                            handled_reason: None,
                            user_input_seq: None,
                        });
                    }
                    if let Some(original_content) = original {
                        engine.append_direct(
                            session,
                            Some(run_id),
                            origin.clone(),
                            Event::UserInputTransformed {
                                original_content,
                                content: content.clone(),
                            },
                        )?;
                    }
                    engine.append_direct(
                        session,
                        Some(run_id),
                        origin,
                        Event::UserInputAdmitted { content },
                    )?;
                    let user_input_seq = engine.inner.store.get(session)?.meta.last_event_seq;
                    engine.clear_skill_turn_state(session);
                    Ok(RunSteerResult {
                        accepted: true,
                        handled_reason: None,
                        user_input_seq: Some(user_input_seq),
                    })
                })
                .await
            }
            UserInputInterception::Handled { reason } => Ok(RunSteerResult {
                accepted: false,
                handled_reason: Some(reason),
                user_input_seq: None,
            }),
        }
    }

    /// Cancels `run_id`. `origin` is who asked: it is recorded on the
    /// `RunCancelled` event. Delegated children keep running: only their own
    /// session's cancellation or `stop_background` stops them
    /// (`docs/site/specs/background-tools.md` section 7.4). A delegate
    /// admission the run still has in flight is abandoned, since no child
    /// exists for it yet.
    pub async fn cancel_run(
        &self,
        run_id: RunId,
        origin: EventOrigin,
    ) -> Result<RunCancelResult, EngineError> {
        let active = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        let result = self.cancel_on_actor(active.session, run_id, origin).await?;
        for delegate in self
            .inner
            .delegation
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values_mut()
            .flat_map(|entries| entries.values_mut())
            .filter(|delegate| delegate.parent_run_id == run_id && delegate.child_run_id.is_none())
        {
            delegate.cancelled = true;
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
                super::event_origin("engine:model-loop"),
                &mut committed,
            );
        };
        active.cancellation.cancel();
        let origin = active.cancel_origin();
        let mut committed = active
            .cancelled_committed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.commit_run_cancelled_with_retry(active.session, run_id, reason, origin, &mut committed)
    }

    pub(super) fn append_run_cancelled_once(
        &self,
        active: &ActiveRun,
        run_id: RunId,
        reason: Option<String>,
    ) -> Result<bool, EngineError> {
        let origin = active.cancel_origin();
        let mut committed = active
            .cancelled_committed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.commit_run_cancelled_with_retry(active.session, run_id, reason, origin, &mut committed)
    }

    pub(super) fn commit_run_cancelled_with_retry(
        &self,
        session: SessionId,
        run_id: RunId,
        reason: Option<String>,
        origin: EventOrigin,
        committed: &mut bool,
    ) -> Result<bool, EngineError> {
        let mut last_error = None;
        for _ in 0..3 {
            match self.commit_run_cancelled_once(
                session,
                run_id,
                reason.clone(),
                origin.clone(),
                committed,
            ) {
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
        origin: EventOrigin,
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
            origin,
            Event::run_finished(cookie_agent_protocol::RunOutcome::Cancelled {
                reason: reason.as_deref().map(safe_error),
            }),
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
                event.run_id == Some(run_id)
                    && matches!(
                        event.payload,
                        Event::RunFinished {
                            outcome: cookie_agent_protocol::Open::Known(
                                cookie_agent_protocol::RunOutcome::Cancelled { .. }
                            )
                        }
                    )
            }))
    }

    /// Asks the executing call `call` of `run` to move to the background, as
    /// the user; it detaches once it reaches its detach point. False when the
    /// call is not an executing call of a background-capable tool.
    pub fn request_detach(&self, run: RunId, call: ToolCallId) -> bool {
        let active = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run)
            .cloned();
        let request = active.and_then(|active| {
            active
                .detach_requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&call)
                .cloned()
        });
        request.is_some_and(|request| {
            request.notify_one();
            true
        })
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

    /// Cancels `run` on `session`'s actor: trips its cancellation and cancels
    /// the approvals it still has pending.
    async fn cancel_on_actor(
        &self,
        session: SessionId,
        run: RunId,
        origin: EventOrigin,
    ) -> Result<RunCancelResult, EngineError> {
        self.on_actor(session, move |engine| {
            let active = engine.active_run_in(session, run)?;
            active
                .cancelled_by
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get_or_insert(origin);
            active.cancellation.cancel();
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
