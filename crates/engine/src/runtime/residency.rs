use std::time::Duration;

use cookie_agent_protocol::{EventPayload as Event, SessionId, SessionOrigin, SessionStatus};
use jiff::Timestamp;

#[cfg(test)]
use super::PagingRaceHook;
use super::helpers::root_id;
use super::{Engine, EngineError, SessionCommand, delegation::DelegationState};

const JANITOR_INTERVAL: Duration = Duration::from_secs(60);
const ARTIFACT_GC_GRACE: Duration = Duration::from_secs(60 * 60);

impl Engine {
    pub(super) fn start_subagent_janitor(&self) {
        let Some(runtime) = self.inner.runtime.clone() else {
            return;
        };
        let inner = std::sync::Arc::downgrade(&self.inner);
        let task = runtime.spawn(async move {
            loop {
                tokio::time::sleep(JANITOR_INTERVAL).await;
                let Some(inner) = inner.upgrade() else {
                    break;
                };
                let engine = Engine { inner };
                if engine
                    .inner
                    .admission_tasks_closing
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    break;
                }
                if let Err(error) = engine.evict_idle_subagents().await {
                    eprintln!("subagent session janitor failed: {error}");
                }
                if let Err(error) = engine.collect_artifacts(ARTIFACT_GC_GRACE) {
                    eprintln!("artifact janitor failed: {error}");
                }
            }
        });
        *self
            .inner
            .janitor_task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(task);
    }

    pub(super) async fn evict_idle_subagents(&self) -> Result<Vec<SessionId>, EngineError> {
        let cap = self.inner.config.runtime.delegation.max_resident_subagents;
        let idle_after = self.inner.config.runtime.delegation.idle_eviction_after;
        self.evict_idle_subagents_with(cap, idle_after).await
    }

    fn collect_artifacts(
        &self,
        grace: Duration,
    ) -> Result<super::artifacts::ArtifactGcReport, EngineError> {
        Ok(self
            .inner
            .artifacts
            .collect_garbage(self.inner.store.sessions_dir_path(), grace)?)
    }

    #[cfg(test)]
    pub(crate) async fn evict_idle_subagents_for_test(
        &self,
        cap: usize,
        idle_after: Duration,
    ) -> Result<Vec<SessionId>, EngineError> {
        self.evict_idle_subagents_with(cap, idle_after).await
    }

    #[cfg(test)]
    pub(crate) fn set_delegation_queued_for_test(&self, session_id: SessionId, queued: bool) {
        if let Some(record) = self
            .inner
            .delegations_by_session
            .lock()
            .expect("delegation registry lock poisoned")
            .get_mut(&session_id)
        {
            record.state = if queued {
                DelegationState::Queued
            } else {
                DelegationState::Finished(SessionStatus::Completed)
            };
        }
    }

    #[cfg(test)]
    pub(crate) fn set_notification_sent_for_test(&self, session_id: SessionId, sent: bool) {
        if let Some(record) = self
            .inner
            .delegations_by_session
            .lock()
            .expect("delegation registry lock poisoned")
            .get_mut(&session_id)
        {
            record.notification_sent = sent;
        }
    }

    #[cfg(test)]
    pub(crate) fn subagent_eviction_eligible_for_test(
        &self,
        session_id: SessionId,
        idle_after: Duration,
    ) -> bool {
        self.inner
            .store
            .get_resident(session_id)
            .is_some_and(|session| {
                self.subagent_eviction_ended_at(&session, Timestamp::now(), idle_after)
                    .is_some()
            })
    }

    #[cfg(test)]
    pub(crate) fn delegation_finished_for_test(&self, session_id: SessionId) -> bool {
        self.inner
            .delegations_by_session
            .lock()
            .expect("delegation registry lock poisoned")
            .get(&session_id)
            .is_some_and(|record| matches!(record.state, DelegationState::Finished(_)))
    }

    #[cfg(test)]
    pub(crate) fn delegation_state_for_test(&self, session_id: SessionId) -> String {
        self.inner
            .delegations_by_session
            .lock()
            .expect("delegation registry lock poisoned")
            .get(&session_id)
            .map_or_else(|| "missing".into(), |record| format!("{:?}", record.state))
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn install_janitor_before_barrier_hook_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .janitor_before_barrier_hook
            .lock()
            .expect("janitor barrier hook lock poisoned") =
            Some(std::sync::Arc::new(PagingRaceHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: std::sync::Arc::clone(&release),
            }));
        (receiver, release)
    }

    async fn evict_idle_subagents_with(
        &self,
        cap: usize,
        idle_after: Duration,
    ) -> Result<Vec<SessionId>, EngineError> {
        let _delegation_admission = self.inner.delegation_admission.lock().await;
        let resident_count = self.inner.store.resident_subagent_count();
        if resident_count <= cap {
            return Ok(Vec::new());
        }
        let now = Timestamp::now();
        let mut candidates = self
            .inner
            .store
            .all()
            .into_iter()
            .filter_map(|session| {
                let ended_at = self.subagent_eviction_ended_at(&session, now, idle_after)?;
                Some((ended_at, session.meta.session_id))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(ended_at, session_id)| (*ended_at, *session_id));

        let mut evicted = Vec::new();
        for (_, session_id) in candidates {
            if self.inner.store.resident_subagent_count() <= cap {
                break;
            }
            let _residency = self.inner.residency_mutation.lock().await;
            let Some(session) = self.inner.store.get_resident(session_id) else {
                continue;
            };
            if self
                .subagent_eviction_ended_at(&session, Timestamp::now(), idle_after)
                .is_none()
            {
                continue;
            }
            #[cfg(test)]
            let hook = self
                .inner
                .janitor_before_barrier_hook
                .lock()
                .expect("janitor barrier hook lock poisoned")
                .take();
            #[cfg(test)]
            if let Some(hook) = hook {
                if let Some(reached) = hook
                    .reached
                    .lock()
                    .expect("janitor barrier reached lock poisoned")
                    .take()
                {
                    let _ = reached.send(());
                }
                hook.release.notified().await;
            }
            let actor = {
                self.inner
                    .actors
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&session_id)
                    .cloned()
            };
            if let Some(actor) = actor {
                let (reply, receiver) = tokio::sync::oneshot::channel();
                actor
                    .send(SessionCommand::EvictionBarrier { reply })
                    .await
                    .map_err(|_| EngineError::ActorStopped)?;
                receiver.await.map_err(|_| EngineError::ActorStopped)??;
            }
            let Some(session) = self.inner.store.get_resident(session_id) else {
                continue;
            };
            if self
                .subagent_eviction_ended_at(&session, Timestamp::now(), idle_after)
                .is_none()
            {
                continue;
            }
            let last_event_seq = session.meta.last_event_seq;
            self.inner
                .actors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&session_id);
            if self.inner.store.evict(session_id)? {
                self.notify_evicted_subscribers(session_id, last_event_seq)
                    .await;
                self.clear_evicted_session_caches(session_id, &session.meta.origin);
                evicted.push(session_id);
            }
        }
        Ok(evicted)
    }

    fn subagent_eviction_ended_at(
        &self,
        session: &crate::session::SessionProjection,
        now: Timestamp,
        idle_after: Duration,
    ) -> Option<Timestamp> {
        if !matches!(session.meta.origin, SessionOrigin::Delegated { .. })
            || session.runs.is_empty()
            || session
                .runs
                .values()
                .any(|run| matches!(run.status, SessionStatus::Running | SessionStatus::Idle))
            || self
                .inner
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .values()
                .any(|active| active.session == session.meta.session_id)
            || self
                .inner
                .delegation_queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&session.meta.session_id)
            || self
                .inner
                .compaction_in_progress
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&session.meta.session_id)
            || self
                .inner
                .pending_approvals
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .keys()
                .any(|(session_id, _)| *session_id == session.meta.session_id)
            || has_runless_pending_inputs(&session.log.event_snapshot())
            || self.producers_pin_session(session.meta.session_id)
        {
            return None;
        }
        if let Some(record) = self
            .inner
            .delegations_by_session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session.meta.session_id)
            .copied()
            && (!matches!(record.state, DelegationState::Finished(_))
                || record.background && !record.notification_sent)
        {
            return None;
        }
        let ended_at = session
            .log
            .event_snapshot()
            .iter()
            .filter(|event| {
                event.run_id.is_some()
                    && matches!(
                        event.payload,
                        Event::RunCompleted { .. }
                            | Event::RunFailed { .. }
                            | Event::RunCancelled { .. }
                            | Event::RunInterrupted { .. }
                    )
            })
            .map(|event| event.timestamp)
            .max()?;
        let idle = Duration::try_from(now.duration_since(ended_at)).ok()?;
        (idle > idle_after).then_some(ended_at)
    }

    async fn notify_evicted_subscribers(&self, session_id: SessionId, last_event_seq: u64) {
        let subscribers = self
            .inner
            .subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session_id)
            .unwrap_or_default();
        for subscriber in subscribers {
            let _ = subscriber
                .sender
                .send(cookie_agent_protocol::EventSubscriptionMessage::Gap {
                    session_id,
                    last_delivered_seq: last_event_seq,
                })
                .await;
        }
    }

    fn clear_evicted_session_caches(&self, session_id: SessionId, origin: &SessionOrigin) {
        self.inner
            .producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session_id);
        // Permission modes are tree-root keyed. Paging out a child must retain
        // the shared mode; an evicted root no longer owns runtime-only state.
        if root_id(origin, session_id) == session_id {
            self.inner
                .permission_modes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&session_id);
        }
        self.inner
            .context_token_estimators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session_id);
        self.inner
            .compaction_in_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session_id);
        self.inner
            .compaction_deferred
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session_id);
    }
}

pub(super) fn has_runless_pending_inputs(events: &[cookie_agent_protocol::StoredEvent]) -> bool {
    let boundary = events
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.payload,
                Event::RunCompleted { .. }
                    | Event::RunFailed { .. }
                    | Event::RunCancelled { .. }
                    | Event::RunInterrupted { .. }
            )
        })
        .map_or(0, |event| event.seq);
    let mut pending = 0_usize;
    for event in events
        .iter()
        .filter(|event| event.seq > boundary && event.run_id.is_none())
    {
        match event.payload {
            Event::UserInputAdmitted { .. } => pending += 1,
            Event::UserInputRecalled { .. } => pending = pending.saturating_sub(1),
            _ => {}
        }
    }
    pending != 0
}
