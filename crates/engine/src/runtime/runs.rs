use std::collections::HashSet;

use cookie_agent_protocol::{
    RunCancelResult, RunId, RunRecallSteerResult, RunStartParams, RunStartResult, RunSteerResult,
    RunToolStdinParams, RunToolStdinResult, SessionId, SessionStatus,
};

use super::{ActiveRun, Engine, EngineError, Event, SessionCommand, helpers::safe_error};

impl Engine {
    pub async fn start_run(&self, params: RunStartParams) -> Result<RunStartResult, EngineError> {
        let session = params.session_id;
        self.request(session, |reply| SessionCommand::Start {
            params,
            admission: None,
            reply,
        })
        .await
    }

    /// Synchronous setup/CLI wrapper. Do not call from a Tokio runtime.
    pub fn start_run_blocking(
        &self,
        params: RunStartParams,
    ) -> Result<RunStartResult, EngineError> {
        let session = params.session_id;
        self.request_blocking(session, |reply| SessionCommand::Start {
            params,
            admission: None,
            reply,
        })
    }

    pub async fn steer(&self, run_id: RunId, input: String) -> Result<RunSteerResult, EngineError> {
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        self.request(active.session, |reply| SessionCommand::Steer {
            run: run_id,
            input,
            reply,
        })
        .await
    }

    /// Synchronous setup/CLI wrapper. Do not call from a Tokio runtime.
    pub fn steer_blocking(
        &self,
        run_id: RunId,
        input: String,
    ) -> Result<RunSteerResult, EngineError> {
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        self.request_blocking(active.session, |reply| SessionCommand::Steer {
            run: run_id,
            input,
            reply,
        })
    }

    pub async fn recall_steer(&self, run_id: RunId) -> Result<RunRecallSteerResult, EngineError> {
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        self.request(active.session, |reply| SessionCommand::RecallSteer {
            run: run_id,
            reply,
        })
        .await
    }

    pub async fn cancel_run(&self, run_id: RunId) -> Result<RunCancelResult, EngineError> {
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        let result = self
            .request(active.session, |reply| SessionCommand::Cancel {
                run: run_id,
                reply,
            })
            .await?;
        let inflight_runs: Vec<_> = {
            let mut inflight = self
                .inner
                .inflight_delegations
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
                    .inflight_delegations
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
                        .active
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&child_run_id)
                        .cloned()
                };
                if let Some(child_active) = child_active {
                    child_active.cancellation.cancel();
                    let _ = self
                        .request(child_active.session, |reply| SessionCommand::Cancel {
                            run: child_run_id,
                            reply,
                        })
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
            .events()
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
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&params.run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(params.run_id))?;
        self.request(active.session, |reply| SessionCommand::Stdin {
            params,
            reply,
        })
        .await
    }
}
