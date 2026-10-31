use std::{collections::HashSet, sync::Arc, time::Duration};

use cookie_agent_protocol::{EventPayload as Event, SessionId, SessionOrigin, SessionStatus};
use jiff::Timestamp;

use crate::runtime::helpers::root_id;
use crate::runtime::{Engine, EngineError, SessionCommand, delegation::DelegationState};

const JANITOR_INTERVAL: Duration = Duration::from_secs(60);

impl Engine {
    pub(in crate::runtime) fn start_subagent_janitor(&self) {
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
                    .delegation
                    .admission_tasks_closing
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    break;
                }
                if let Err(error) = engine.evict_idle_subagents().await {
                    eprintln!("subagent session janitor failed: {error}");
                }
                // How long a root session tree sits unused before the janitor
                // pages it out. Viewing it again, or any command, reloads it.
                let idle_after = engine
                    .inner
                    .config
                    .load()
                    .runtime
                    .server
                    .session_idle_eviction_after;
                if let Err(error) = engine.evict_idle_roots_with(idle_after).await {
                    eprintln!("root session janitor failed: {error}");
                }
                let sweeper = engine.clone();
                if let Err(error) = crate::runtime::blocking_io::run(move || {
                    sweeper.sweep_upload_leases(std::time::Instant::now());
                })
                .await
                {
                    eprintln!("upload lease sweep failed: {error}");
                }
            }
        });
        *self
            .inner
            .janitor_task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(task);
    }

    pub(in crate::runtime) async fn evict_idle_subagents(
        &self,
    ) -> Result<Vec<SessionId>, EngineError> {
        let cap = self
            .inner
            .config
            .load()
            .runtime
            .delegation
            .max_resident_subagents;
        let idle_after = self
            .inner
            .config
            .load()
            .runtime
            .delegation
            .idle_eviction_after;
        self.evict_idle_subagents_with(cap, idle_after).await
    }

    pub(in crate::runtime) async fn evict_idle_subagents_with(
        &self,
        cap: usize,
        idle_after: Duration,
    ) -> Result<Vec<SessionId>, EngineError> {
        let delegation_admission = self.inner.delegation.admission.lock().await;
        // The cap applies to each tree on its own: one tree's activity never
        // pages out another tree's children (tree-local C10).
        let over_cap = self
            .inner
            .store
            .resident_subagent_counts()
            .into_iter()
            .filter(|(_, count)| *count > cap)
            .map(|(root, _)| root)
            .collect::<std::collections::HashSet<_>>();
        if over_cap.is_empty() {
            return Ok(Vec::new());
        }
        let now = Timestamp::now();
        let mut candidates = self
            .inner
            .store
            .all()
            .into_iter()
            .filter_map(|session| {
                let SessionOrigin::Delegated {
                    root_session_id, ..
                } = session.meta.origin
                else {
                    return None;
                };
                if !over_cap.contains(&root_session_id) {
                    return None;
                }
                let ended_at = self.subagent_eviction_ended_at(&session, now, idle_after)?;
                Some((ended_at, root_session_id, session.meta.session_id))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(ended_at, _, session_id)| (*ended_at, *session_id));

        let mut evicted = Vec::new();
        let mut closing = Vec::new();
        for (_, root, session_id) in candidates {
            if self
                .inner
                .store
                .resident_subagent_counts()
                .get(&root)
                .is_none_or(|count| *count <= cap)
            {
                continue;
            }
            let _residency = self.inner.sessions.residency_mutation.lock().await;
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
                .test_hooks
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
            self.drain_actor(session_id).await?;
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
                .sessions
                .actors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&session_id);
            if self.inner.store.evict(session_id)? {
                self.inner
                    .store
                    .notify_evicted_subscribers(session_id, last_event_seq);
                self.clear_evicted_session_caches(session_id);
                evicted.push(session_id);
                closing.extend(self.take_acp_session_to_close(session_id));
            }
        }
        drop(delegation_admission);
        self.finish_evicted_acp_sessions(closing).await;
        Ok(evicted)
    }

    pub(in crate::runtime) fn subagent_eviction_ended_at(
        &self,
        session: &crate::session::SessionProjection,
        now: Timestamp,
        idle_after: Duration,
    ) -> Option<Timestamp> {
        if !matches!(session.meta.origin, SessionOrigin::Delegated { .. })
            || session.runs.is_empty()
            || self.session_has_pending_work(session)
        {
            return None;
        }
        if let Some(record) = self
            .inner
            .delegation
            .by_session
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
                event.run_id.is_some() && matches!(event.payload, Event::RunFinished { .. })
            })
            .map(|event| event.timestamp)
            .max()?;
        let idle = Duration::try_from(now.duration_since(ended_at)).ok()?;
        (idle > idle_after).then_some(ended_at)
    }

    /// Whether anything still needs `session` in memory: a live or queued
    /// run, compaction, an approval, unadmitted input, or producer work
    /// about to happen. A producer that only holds a registration does not
    /// pin it (`Engine::producers_pin_session`).
    fn session_has_pending_work(&self, session: &crate::session::SessionProjection) -> bool {
        let session_id = session.meta.session_id;
        session.status == SessionStatus::Running
            || session
                .runs
                .values()
                .any(|run| matches!(run.status, SessionStatus::Running | SessionStatus::Idle))
            || self
                .inner
                .sessions
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .values()
                .any(|active| active.session == session_id)
            || self
                .inner
                .delegation
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&session_id)
            || self
                .inner
                .compaction
                .in_progress
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&session_id)
            || self
                .inner
                .approvals
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .keys()
                .any(|(pending, _)| *pending == session_id)
            || has_runless_pending_inputs(&session.log.event_snapshot())
            || self.producers_pin_session(session_id)
            || self.acp_background_running(session_id)
    }

    /// Pages out root session trees nobody has used for `idle_after`: no
    /// event, no live tail, and no pending work anywhere in the tree. A tree
    /// goes whole or not at all, children first, so an evicted root never
    /// leaves resident children behind it. Its runtime-only settings (the
    /// permission mode, context calibration) survive for when it reloads.
    pub(in crate::runtime) async fn evict_idle_roots_with(
        &self,
        idle_after: Duration,
    ) -> Result<Vec<SessionId>, EngineError> {
        let delegation_admission = self.inner.delegation.admission.lock().await;
        let now = Timestamp::now();
        let resident = self.inner.store.all();
        let candidates = resident
            .iter()
            .filter(|session| self.root_tree_idle(session, &resident, now, idle_after))
            .map(|session| session.meta.session_id)
            .collect::<Vec<_>>();
        let mut evicted = Vec::new();
        let mut closing = Vec::new();
        for root in candidates {
            let _residency = self.inner.sessions.residency_mutation.lock().await;
            let still_idle = |engine: &Self| {
                let resident = engine.inner.store.all();
                resident.iter().any(|session| {
                    session.meta.session_id == root
                        && engine.root_tree_idle(session, &resident, Timestamp::now(), idle_after)
                })
            };
            if !still_idle(self) {
                continue;
            }
            // Drain each tree member's actor so nothing it already accepted
            // is cut off, then re-check: the drained work may have used it.
            let members = self
                .inner
                .store
                .all()
                .into_iter()
                .filter(|session| root_id(&session.meta.origin, session.meta.session_id) == root)
                .map(|session| session.meta.session_id)
                .collect::<Vec<_>>();
            for member in &members {
                self.drain_actor(*member).await?;
            }
            if !still_idle(self) {
                continue;
            }
            // Children first, root last.
            let mut ordered = members.clone();
            ordered.sort_by_key(|member| *member == root);
            for member in ordered {
                let Some(session) = self.inner.store.get_resident(member) else {
                    continue;
                };
                let last_event_seq = session.meta.last_event_seq;
                self.inner
                    .sessions
                    .actors
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&member);
                if !self.inner.store.evict(member)? {
                    // A log never persisted (an empty session) stays put.
                    continue;
                }
                self.inner
                    .store
                    .notify_evicted_subscribers(member, last_event_seq);
                if member == root {
                    self.retain_registrations_on_eviction(member);
                    self.inner
                        .sessions
                        .producer_projections
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&member);
                    self.inner
                        .deferral_views
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&member);
                } else {
                    self.clear_evicted_session_caches(member);
                }
                evicted.push(member);
                closing.extend(self.take_acp_session_to_close(member));
            }
            if members
                .iter()
                .all(|member| !self.inner.store.is_resident(*member))
            {
                self.forget_evicted_tree(root, &members);
            }
        }
        drop(delegation_admission);
        self.finish_evicted_acp_sessions(closing).await;
        Ok(evicted)
    }

    /// Closes the ACP sessions of the sessions an eviction paged out, taken
    /// as it paged them out. An evicted session's ACP session ends as a
    /// deleted one's does; its next turn resumes or loads it, or starts a
    /// new one, once this one closed.
    async fn finish_evicted_acp_sessions(
        &self,
        closing: Vec<crate::runtime::acp::runner::ClosingAcpSession>,
    ) {
        #[cfg(test)]
        let hook = self
            .inner
            .test_hooks
            .acp_eviction_close_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        #[cfg(test)]
        if let Some(hook) = hook {
            if let Some(reached) = hook
                .reached
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
        for closing in closing {
            self.finish_acp_session_close(closing).await;
        }
    }

    /// Drops what the engine keeps for the sessions of a tree paged out
    /// whole. What the tree's load handed over (its delegation records and
    /// the registry built from them) goes with the tree's load, so the next
    /// use loads the tree again and hands it over anew.
    fn forget_evicted_tree(&self, root: SessionId, members: &[SessionId]) {
        let members = members.iter().copied().collect::<HashSet<_>>();
        self.inner.store.unload_tree(root, || {
            self.inner.delegation_events.forget_sessions(&members);
            self.inner
                .delegation
                .by_session
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|child, record| {
                    !members.contains(child) && !members.contains(&record.parent_session_id)
                });
        });
        self.inner.memory_seen.forget_sessions(&members);
        self.inner
            .sessions
            .forks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(source, _), _| !members.contains(source));
        // A steer holding its session's order keeps it.
        self.inner
            .sessions
            .steer_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|session, order| !members.contains(session) || Arc::strong_count(order) > 1);
    }

    /// Whether `session` is a root whose whole resident tree has been unused
    /// for `idle_after`.
    fn root_tree_idle(
        &self,
        session: &crate::session::SessionProjection,
        resident: &[Arc<crate::session::SessionProjection>],
        now: Timestamp,
        idle_after: Duration,
    ) -> bool {
        if !matches!(session.meta.origin, SessionOrigin::Root) {
            return false;
        }
        let root = session.meta.session_id;
        let unused = |member: &crate::session::SessionProjection| {
            // Cheap checks first: the pending-work check scans the log.
            member.log.last_event().is_some_and(|event| {
                Duration::try_from(now.duration_since(event.timestamp))
                    .is_ok_and(|idle| idle > idle_after)
            }) && !self.inner.store.has_live_tail(member.meta.session_id)
                && !self.session_has_pending_work(member)
        };
        unused(session)
            && resident
                .iter()
                .filter(|member| {
                    matches!(
                        member.meta.origin,
                        SessionOrigin::Delegated { root_session_id, .. } if root_session_id == root
                    )
                })
                .all(|child| {
                    unused(child)
                        && self
                            .subagent_eviction_ended_at(child, now, idle_after)
                            .is_some()
                })
    }

    /// Waits until `session`'s actor has run everything queued before now.
    async fn drain_actor(&self, session: SessionId) -> Result<(), EngineError> {
        let actor = self
            .inner
            .sessions
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session)
            .cloned();
        if let Some(actor) = actor {
            let (barrier, receiver) = SessionCommand::call(false, |_| async { Ok(()) });
            actor
                .send(barrier)
                .await
                .map_err(|_| EngineError::ActorStopped)?;
            receiver.await.map_err(|_| EngineError::ActorStopped)??;
        }
        Ok(())
    }

    fn clear_evicted_session_caches(&self, session_id: SessionId) {
        self.retain_registrations_on_eviction(session_id);
        self.inner
            .sessions
            .producer_projections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session_id);
        self.inner
            .compaction
            .context_token_estimators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session_id);
        self.inner
            .compaction
            .in_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session_id);
        self.inner
            .compaction
            .deferred
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session_id);
        self.inner
            .deferral_views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session_id);
        self.inner
            .memory_seen
            .forget_sessions(&HashSet::from([session_id]));
        let mut steer_order = self
            .inner
            .sessions
            .steer_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if steer_order
            .get(&session_id)
            .is_some_and(|order| Arc::strong_count(order) == 1)
        {
            steer_order.remove(&session_id);
        }
    }
}

pub(in crate::runtime) fn has_runless_pending_inputs(
    events: &[Arc<cookie_agent_protocol::StoredEvent>],
) -> bool {
    // Back from the end to the latest run's finish: an input admitted
    // outside a run since then is pending unless a later record recalled it.
    let mut recalled = Vec::new();
    for event in events
        .iter()
        .rev()
        .take_while(|event| !matches!(event.payload, Event::RunFinished { .. }))
        .filter(|event| event.run_id.is_none())
    {
        match &event.payload {
            Event::UserInputAdmitted { .. } if !recalled.contains(&event.seq) => return true,
            Event::UserInputRecalled { user_input_seq, .. } => recalled.push(*user_input_seq),
            _ => {}
        }
    }
    false
}
