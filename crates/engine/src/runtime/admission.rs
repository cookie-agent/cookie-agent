use std::{
    future::Future,
    sync::{Arc, atomic::Ordering},
};

use cookie_agent_protocol::{
    AgentId, DelegateRequestPayload, DelegatedContextTurn, InvocationId, RunId, RunSelection,
    SessionId, SessionMeta, SessionOrigin, SessionStatus, SessionTitle, SessionTitleChange,
    Sha256Digest, ToolCallId,
};
use tokio::sync::oneshot;

use super::{
    Engine, EngineError, Event, Inner,
    delegation::{DelegationRecord, cancelled_delegate_result_with_reason},
    helpers::{invocation_id, session_depth},
};
use crate::policy::FrozenRunPolicy;

impl Engine {
    pub(crate) fn spawn_admission_task<F>(&self, runtime: &tokio::runtime::Handle, task: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let Ok(mut tasks) = self.inner.delegation.admission_tasks.lock() else {
            return false;
        };
        if self
            .inner
            .delegation
            .admission_tasks_closing
            .load(Ordering::Acquire)
        {
            return false;
        }
        tasks.retain(|task| !task.is_finished());
        tasks.push(runtime.spawn(task));
        true
    }

    pub(super) async fn spawn_admission_blocking<T, E, F>(&self, work: F) -> Result<T, EngineError>
    where
        T: Send + 'static,
        E: Into<EngineError> + Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        {
            let mut tasks = self
                .inner
                .delegation
                .admission_blocking_tasks
                .lock()
                .map_err(|_| EngineError::ActorStopped)?;
            if self
                .inner
                .delegation
                .admission_tasks_closing
                .load(Ordering::Acquire)
            {
                return Err(EngineError::ActorStopped);
            }
            tasks.retain(|task| !task.is_finished());
            #[cfg(test)]
            let hook = self
                .inner
                .test_hooks
                .admission_blocking_hook
                .lock()
                .expect("admission blocking hook lock poisoned")
                .take();
            tasks.push(tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                if let Some(hook) = hook {
                    let _ = hook.reached.send(());
                    let _ = hook.release.recv();
                }
                let _ = sender.send(work().map_err(Into::into));
            }));
        }
        receiver.await.map_err(|_| EngineError::ActorStopped)?
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create_child(
        &self,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        agent: &AgentId,
        child_policy: FrozenRunPolicy,
        cache_strategies: Vec<Option<cookie_agent_protocol::FrozenCacheStrategy>>,
        request_fingerprint: Sha256Digest,
        request: DelegateRequestPayload,
        admission: Option<(InvocationId, u64)>,
        attachments: Option<Vec<cookie_agent_protocol::RuntimeAttachment>>,
    ) -> Result<SessionMeta, EngineError> {
        let child_runtime = Arc::clone(&child_policy.runtime);
        let child_title = request.title.clone();
        let resume_session_id = request.resume_session_id;
        let seeded_context = request.seeded_context.clone();
        let parent = self.inner.store.get(parent_session_id)?;
        let pending_tool = parent
            .runs
            .get(&parent_run_id)
            .and_then(|run| run.pending_calls.get(&parent_tool_call_id));
        let staged_skill_fork = self
            .inner
            .skills_runtime
            .pending_forks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&parent_tool_call_id);
        let pending_skill_fork =
            pending_tool.is_some_and(|tool| tool == "skill") && staged_skill_fork;
        let direct_skill_fork = pending_tool.is_none()
            && staged_skill_fork
            && self
                .inner
                .skills_runtime
                .direct_calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&parent_tool_call_id);
        let cron = self.cron_delegation(parent_tool_call_id);
        let pending_cron_run =
            pending_tool.is_some_and(|tool| tool == "cron_run") && cron.is_some();
        if pending_tool.is_none_or(|tool| tool != "delegate_subagent")
            && !pending_cron_run
            && !pending_skill_fork
            && !direct_skill_fork
        {
            return Err(EngineError::ToolFailed(
                "delegate call is not pending".into(),
            ));
        }
        if self
            .terminal_parent_delegate(parent_session_id, parent_run_id, parent_tool_call_id)
            .await?
        {
            return Err(EngineError::ToolFailed(
                "delegate parent run is terminal".into(),
            ));
        }
        let parent_policy = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&parent_run_id)
            .map(|active| active.policy.clone())
            .ok_or_else(|| EngineError::ToolFailed("delegate parent run is not active".into()))?;
        let depth_ceiling = self
            .delegation_ceiling(&parent_policy, cron.is_some())
            .map_err(|_| EngineError::ToolFailed("delegate admission denied".into()))?;
        let depth = session_depth(&parent.meta.origin);
        let target_allowed = cron.is_some()
            || parent_policy
                .agent
                .delegation
                .as_ref()
                .is_some_and(|delegation| delegation.targets.contains(agent));
        if depth >= depth_ceiling || !target_allowed {
            return Err(EngineError::ToolFailed("delegate admission denied".into()));
        }
        let invocation_id = invocation_id(parent_session_id, parent_run_id, parent_tool_call_id);
        let delegation_events = self.inner.delegation_events.clone();
        let reserved_agent = child_policy.agent.clone();
        let reserved_suffix = child_policy.selected_suffix.clone();
        if reserved_suffix.is_empty() && child_policy.acp_agent().is_none() {
            return Err(EngineError::NoRunnableModel);
        }
        let reserved_revisions =
            crate::policy::runtime_revisions(&child_runtime, child_policy.manifest_revision());
        let entry = self
            .spawn_admission_blocking(move || {
                delegation_events.reserve(
                    invocation_id,
                    parent_session_id,
                    parent_run_id,
                    parent_tool_call_id,
                    reserved_agent,
                    reserved_revisions,
                    reserved_suffix,
                    cache_strategies,
                    request_fingerprint,
                    request,
                )
            })
            .await?;
        #[cfg(test)]
        let reservation_hook = self
            .inner
            .test_hooks
            .delegation_reservation_hook
            .lock()
            .expect("delegation reservation hook lock poisoned")
            .take();
        #[cfg(test)]
        if let Some(hook) = reservation_hook {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("delegation reservation reached lock poisoned")
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
        if admission.is_some_and(|(invocation_id, generation)| {
            !self.admission_generation_live(invocation_id, generation)
        }) {
            return Err(EngineError::ToolFailed(
                "delegate admission was abandoned".into(),
            ));
        }
        // The reservation may have completed while the parent was cancelled.
        // Never turn that durable reservation into a child after cancellation.
        if self
            .terminal_parent_delegate(parent_session_id, parent_run_id, parent_tool_call_id)
            .await?
        {
            return Err(EngineError::ToolFailed(
                "delegate parent run is terminal".into(),
            ));
        }
        if let Ok(existing) = self.inner.store.get(entry.reservation.child_session_id) {
            if resume_session_id.is_none() {
                // A redelivery after a crash may have missed the root's
                // record of the child's attachments.
                self.record_delegated_attachments(
                    delegated_root(&parent.meta),
                    existing.meta.session_id,
                    self.attachments_of(&existing),
                )
                .await?;
                self.ensure_delegated_context_seed(
                    existing.meta.session_id,
                    invocation_id,
                    seeded_context,
                )
                .await?;
                self.ensure_delegated_title(existing.meta.session_id, invocation_id, child_title)
                    .await?;
            }
            if !direct_skill_fork {
                self.ensure_parent_link(
                    parent_session_id,
                    parent_run_id,
                    parent_tool_call_id,
                    existing.meta.session_id,
                )
                .await?;
            }
            let delegation_events = self.inner.delegation_events.clone();
            self.spawn_admission_blocking(move || delegation_events.mark_started(invocation_id))
                .await?;
            return Ok(existing.metadata());
        }
        let (root, depth) = match parent.meta.origin {
            SessionOrigin::Delegated {
                root_session_id,
                depth,
                ..
            } => (root_session_id, depth + 1),
            _ => (parent_session_id, 1),
        };
        let origin = SessionOrigin::Delegated {
            root_session_id: root,
            parent_session_id,
            parent_run_id,
            parent_tool_call_id,
            invocation_id,
            depth,
        };
        let selection = RunSelection {
            agent: agent.clone(),
            model: child_policy.run_model_selection(),
            preset: child_policy.preset.clone(),
        };
        let child_session_id = entry.reservation.child_session_id;
        let short_id = {
            let existing = self.existing_tree_handles(root)?;
            super::handles::generate_handle(agent.as_str(), &existing, super::handles::random_hex)?
        };
        let creation = Event::SessionCreated {
            cron: cron
                .as_ref()
                .map(|cron| cookie_agent_protocol::SessionCron {
                    job_id: cron.job.clone(),
                    fired_at: cron.fired_at,
                }),
            history_format: cookie_agent_protocol::HISTORY_FORMAT,
            origin,
            short_id,
            cwd_identity: parent.meta.cwd_identity.clone(),
            creation_selection: selection,
            creation_agent: Box::new(child_policy.agent.clone()),
            revisions: crate::policy::runtime_revisions(
                &child_runtime,
                child_policy.manifest_revision(),
            ),
        };
        // The child starts with the capabilities its root named, or inherits
        // its parent's attachments, and records them itself so its log is
        // self-describing.
        let child_attachments = match attachments {
            Some(attachments) => attachments,
            None => self.inherited_attachments(&parent)?,
        };
        let mut attachments = super::attachments::initial_attachment_events(&child_attachments);
        // A cron job's run now starts with the job's rules as its overlay.
        if let Some(cron) = cron.as_ref().filter(|cron| !cron.rules.is_empty()) {
            attachments.push(Event::SessionPermissionOverlaySet {
                overlay: cookie_agent_protocol::SessionPermissionOverlay {
                    rules: cron.rules.clone(),
                },
            });
        }
        let store = self.inner.store.clone();
        self.spawn_admission_blocking(move || {
            store.create_with_initial(
                child_session_id,
                super::event_origin("engine:delegation"),
                creation,
                attachments,
            )
        })
        .await?;
        self.spawn_actor(child_session_id)?;
        self.record_delegated_attachments(root, child_session_id, child_attachments)
            .await?;
        self.ensure_delegated_context_seed(child_session_id, invocation_id, seeded_context)
            .await?;
        self.ensure_delegated_title(child_session_id, invocation_id, child_title)
            .await?;
        if entry.request.staged_skill.is_some() {
            self.inner
                .store
                .persist_buffered_session(child_session_id)?;
        }
        if admission.is_some_and(|(invocation_id, generation)| {
            !self.admission_generation_live(invocation_id, generation)
        }) {
            return Err(EngineError::ToolFailed(
                "delegate admission was abandoned".into(),
            ));
        }
        if !direct_skill_fork {
            self.ensure_parent_link(
                parent_session_id,
                parent_run_id,
                parent_tool_call_id,
                child_session_id,
            )
            .await?;
        }
        let delegation_events = self.inner.delegation_events.clone();
        self.spawn_admission_blocking(move || delegation_events.mark_started(invocation_id))
            .await?;
        Ok(self.inner.store.get(child_session_id)?.metadata())
    }

    async fn ensure_delegated_context_seed(
        &self,
        child_session_id: SessionId,
        invocation_id: InvocationId,
        turns: Vec<DelegatedContextTurn>,
    ) -> Result<(), EngineError> {
        if turns.is_empty() {
            return Ok(());
        }
        let projection = self.inner.store.get(child_session_id)?;
        let mut found = false;
        for event in projection.log.event_snapshot().iter() {
            if let Event::DelegatedContextSeeded {
                invocation_id: existing_invocation,
                turns: existing_turns,
            } = &event.payload
                && *existing_invocation == invocation_id
            {
                if existing_turns != &turns {
                    return Err(EngineError::ToolFailed(
                        "delegated child has conflicting inherited context".into(),
                    ));
                }
                found = true;
            }
        }
        if !found {
            self.append(
                child_session_id,
                None,
                super::event_origin("engine:delegation"),
                Event::DelegatedContextSeeded {
                    invocation_id,
                    turns,
                },
            )
            .await?;
        }
        Ok(())
    }

    pub(super) fn ensure_delegated_context_seed_blocking(
        &self,
        child_session_id: SessionId,
        invocation_id: InvocationId,
        turns: Vec<DelegatedContextTurn>,
    ) -> Result<(), EngineError> {
        if turns.is_empty() {
            return Ok(());
        }
        let projection = self.inner.store.get(child_session_id)?;
        let mut found = false;
        for event in projection.log.event_snapshot().iter() {
            if let Event::DelegatedContextSeeded {
                invocation_id: existing_invocation,
                turns: existing_turns,
            } = &event.payload
                && *existing_invocation == invocation_id
            {
                if existing_turns != &turns {
                    return Err(EngineError::ToolFailed(
                        "delegated child has conflicting inherited context".into(),
                    ));
                }
                found = true;
            }
        }
        if !found {
            self.append_recovery_direct(
                child_session_id,
                None,
                super::event_origin("engine:delegation"),
                Event::DelegatedContextSeeded {
                    invocation_id,
                    turns,
                },
            )?;
        }
        Ok(())
    }

    async fn ensure_delegated_title(
        &self,
        child_session_id: SessionId,
        invocation_id: InvocationId,
        title: SessionTitle,
    ) -> Result<(), EngineError> {
        let projection = self.inner.store.get(child_session_id)?;
        let mut has_matching_title = false;
        for event in projection.log.event_snapshot().iter() {
            if let Event::SessionTitleCommitted { change, .. } = &event.payload {
                match change {
                    SessionTitleChange::DelegatedSet {
                        title: existing,
                        invocation_id: existing_invocation,
                    } if *existing_invocation == invocation_id => {
                        if existing != &title {
                            return Err(EngineError::ToolFailed(
                                "delegated child has a conflicting origin title".into(),
                            ));
                        }
                        has_matching_title = true;
                    }
                    SessionTitleChange::UserSet { .. }
                    | SessionTitleChange::UserClear { .. }
                    | SessionTitleChange::UserReset { .. } => {}
                    _ => {
                        return Err(EngineError::ToolFailed(
                            "delegated child has a conflicting session title".into(),
                        ));
                    }
                }
            }
        }
        if !has_matching_title {
            self.append(
                child_session_id,
                None,
                super::event_origin("user"),
                Event::SessionTitleCommitted {
                    change: SessionTitleChange::DelegatedSet {
                        title,
                        invocation_id,
                    },
                    input_through_seq: 0,
                },
            )
            .await?;
        }
        Ok(())
    }

    pub(super) fn ensure_delegated_title_blocking(
        &self,
        child_session_id: SessionId,
        invocation_id: InvocationId,
        title: SessionTitle,
    ) -> Result<(), EngineError> {
        let projection = self.inner.store.get(child_session_id)?;
        let mut has_matching_title = false;
        for event in projection.log.event_snapshot().iter() {
            if let Event::SessionTitleCommitted { change, .. } = &event.payload {
                match change {
                    SessionTitleChange::DelegatedSet {
                        title: existing,
                        invocation_id: existing_invocation,
                    } if *existing_invocation == invocation_id => {
                        if existing != &title {
                            return Err(EngineError::ToolFailed(
                                "delegated child has a conflicting origin title".into(),
                            ));
                        }
                        has_matching_title = true;
                    }
                    SessionTitleChange::UserSet { .. }
                    | SessionTitleChange::UserClear { .. }
                    | SessionTitleChange::UserReset { .. } => {}
                    _ => {
                        return Err(EngineError::ToolFailed(
                            "delegated child has a conflicting session title".into(),
                        ));
                    }
                }
            }
        }
        if !has_matching_title {
            self.append_recovery_direct(
                child_session_id,
                None,
                super::event_origin("user"),
                Event::SessionTitleCommitted {
                    change: SessionTitleChange::DelegatedSet {
                        title,
                        invocation_id,
                    },
                    input_through_seq: 0,
                },
            )?;
        }
        Ok(())
    }

    pub(super) fn admission_generation_live(
        &self,
        invocation_id: InvocationId,
        generation: u64,
    ) -> bool {
        self.inner
            .delegation
            .inflight
            .lock()
            .ok()
            .and_then(|admissions| {
                admissions
                    .get(&invocation_id)
                    .and_then(|entries| entries.get(&generation))
                    .is_some_and(|admission| !admission.cancelled)
                    .then_some(())
            })
            .is_some()
    }

    #[cfg(test)]
    pub(crate) fn cancel_inflight_delegation_for_test(
        &self,
        invocation_id: InvocationId,
    ) -> Result<(), EngineError> {
        let mut admissions = self
            .inner
            .delegation
            .inflight
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let entries = admissions
            .get_mut(&invocation_id)
            .ok_or_else(|| EngineError::ToolFailed("delegate admission is not in flight".into()))?;
        for admission in entries.values_mut() {
            admission.cancelled = true;
        }
        Ok(())
    }

    pub(super) fn publish_admission_child(
        &self,
        invocation_id: InvocationId,
        generation: u64,
        child_session_id: SessionId,
    ) -> Result<(), EngineError> {
        let mut admissions = self
            .inner
            .delegation
            .inflight
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let admission = admissions
            .get_mut(&invocation_id)
            .and_then(|entries| entries.get_mut(&generation))
            .ok_or_else(|| EngineError::ToolFailed("delegate admission disappeared".into()))?;
        admission.child_session_id = Some(child_session_id);
        Ok(())
    }

    pub(super) fn publish_admission_run(
        &self,
        invocation_id: InvocationId,
        generation: u64,
        child_session_id: SessionId,
        child_run_id: RunId,
    ) -> Result<(), EngineError> {
        let mut admissions = self
            .inner
            .delegation
            .inflight
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let admission = admissions
            .get_mut(&invocation_id)
            .and_then(|entries| entries.get_mut(&generation))
            .ok_or_else(|| EngineError::ToolFailed("delegate admission disappeared".into()))?;
        admission.child_session_id = Some(child_session_id);
        admission.child_run_id = Some(child_run_id);
        admission.starting = false;
        Ok(())
    }

    /// Takes back `child_run_id` from an admission it was published to when
    /// the run never went live, so the admission reads as having no run.
    pub(super) fn retract_admission_run(
        &self,
        invocation_id: InvocationId,
        generation: u64,
        child_run_id: RunId,
    ) {
        if let Ok(mut admissions) = self.inner.delegation.inflight.lock()
            && let Some(admission) = admissions
                .get_mut(&invocation_id)
                .and_then(|entries| entries.get_mut(&generation))
                .filter(|admission| admission.child_run_id == Some(child_run_id))
        {
            admission.child_run_id = None;
        }
    }

    pub(super) fn publish_resume_admission_target(
        &self,
        invocation_id: InvocationId,
        generation: u64,
        child_session_id: SessionId,
        child_run_id: RunId,
        previous_record: DelegationRecord,
    ) -> Result<(), EngineError> {
        let mut admissions = self
            .inner
            .delegation
            .inflight
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let admission = admissions
            .get_mut(&invocation_id)
            .and_then(|entries| entries.get_mut(&generation))
            .filter(|admission| !admission.cancelled)
            .ok_or_else(|| EngineError::ToolFailed("delegate admission disappeared".into()))?;
        admission.child_session_id = Some(child_session_id);
        admission.child_run_id = Some(child_run_id);
        admission.resume_attach = true;
        admission.previous_record = Some(previous_record);
        admission.starting = false;
        Ok(())
    }

    pub(super) fn publish_resume_admission_seq(
        &self,
        invocation_id: InvocationId,
        generation: u64,
        admission_seq: u64,
    ) -> Result<(), EngineError> {
        let mut admissions = self
            .inner
            .delegation
            .inflight
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let admission = admissions
            .get_mut(&invocation_id)
            .and_then(|entries| entries.get_mut(&generation))
            .ok_or_else(|| EngineError::ToolFailed("delegate admission disappeared".into()))?;
        admission.resume_admission_seq = Some(admission_seq);
        Ok(())
    }

    pub(super) fn mark_admission_starting(
        &self,
        invocation_id: InvocationId,
        generation: u64,
    ) -> Result<(), EngineError> {
        let mut admissions = self
            .inner
            .delegation
            .inflight
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let admission = admissions
            .get_mut(&invocation_id)
            .and_then(|entries| entries.get_mut(&generation))
            .filter(|admission| !admission.cancelled)
            .ok_or_else(|| EngineError::ToolFailed("delegate admission disappeared".into()))?;
        admission.starting = true;
        Ok(())
    }

    pub(super) fn clear_admission_starting(&self, invocation_id: InvocationId, generation: u64) {
        if let Ok(mut admissions) = self.inner.delegation.inflight.lock()
            && let Some(admission) = admissions
                .get_mut(&invocation_id)
                .and_then(|entries| entries.get_mut(&generation))
        {
            admission.starting = false;
        }
    }

    /// Atomically revalidates the generation against the admission registry at
    /// the destructive cancellation point. A retry that enters first makes the
    /// all-abandoned predicate false; a retry that enters afterwards observes a
    /// sweep that was already linearized for the previous generation.
    pub(super) fn observe_abandoned_admission(
        &self,
        invocation_id: InvocationId,
        generation: u64,
    ) -> Result<Option<AbandonedAdmission>, EngineError> {
        let admissions = self
            .inner
            .delegation
            .inflight
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let Some(entries) = admissions.get(&invocation_id) else {
            return Ok(None);
        };
        let Some(admission) = entries.get(&generation) else {
            return Ok(None);
        };
        if !admission.cancelled {
            return Ok(None);
        }
        if admission.resume_attach && admission.resume_admission_seq.is_none() {
            return Ok(None);
        }
        if admission.starting && admission.child_run_id.is_none() {
            return Ok(None);
        }
        if entries.values().any(|entry| !entry.cancelled) {
            return Ok(None);
        }
        let (Some(parent_session_id), Some(parent_tool_call_id)) =
            (admission.parent_session_id, admission.parent_tool_call_id)
        else {
            return Ok(None);
        };
        let target = AbandonedAdmission {
            parent_session_id,
            parent_run_id: admission.parent_run_id,
            parent_tool_call_id,
            child_session_id: admission.child_session_id,
            child_run_id: admission.child_run_id,
            resume_attach: admission.resume_attach,
            resume_admission_seq: admission.resume_admission_seq,
            previous_record: admission.previous_record,
        };
        Ok(Some(target))
    }

    pub(super) fn cancel_abandoned_admission(
        &self,
        invocation_id: InvocationId,
        generation: u64,
        observed: AbandonedAdmission,
    ) -> Result<Option<AbandonedAdmission>, EngineError> {
        let mut admissions = self
            .inner
            .delegation
            .inflight
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let Some(entries) = admissions.get_mut(&invocation_id) else {
            return Ok(None);
        };
        let Some(admission) = entries.get(&generation).copied() else {
            return Ok(None);
        };
        if !admission.cancelled
            || (admission.resume_attach && admission.resume_admission_seq.is_none())
            || (admission.starting && admission.child_run_id.is_none())
            || entries.values().any(|entry| !entry.cancelled)
        {
            entries.remove(&generation);
            if entries.is_empty() {
                admissions.remove(&invocation_id);
            }
            return Ok(None);
        }
        let (Some(parent_session_id), Some(parent_tool_call_id)) =
            (admission.parent_session_id, admission.parent_tool_call_id)
        else {
            return Ok(None);
        };
        let target = AbandonedAdmission {
            parent_session_id,
            parent_run_id: admission.parent_run_id,
            parent_tool_call_id,
            child_session_id: admission.child_session_id,
            child_run_id: admission.child_run_id,
            resume_attach: admission.resume_attach,
            resume_admission_seq: admission.resume_admission_seq,
            previous_record: admission.previous_record,
        };
        if target != observed {
            return Ok(None);
        }
        if !target.resume_attach
            && let Some(run_id) = target.child_run_id
        {
            match self.cancel_run_durably(run_id, Some("delegate admission was abandoned".into())) {
                Ok(_) => {}
                Err(EngineError::MissingRun(missing)) => {
                    let recorded = target.child_session_id.and_then(|child| {
                        self.inner.store.get(child).ok().and_then(|projection| {
                            projection.runs.get(&run_id).map(|run| run.status)
                        })
                    });
                    match recorded {
                        // The run ended: nothing is left to cancel.
                        Some(status) if status != SessionStatus::Running => {}
                        // Published before its `RunStarted` is appended: the
                        // admission's start sees the cancellation once it
                        // returns and sweeps again, the run then live.
                        None => return Ok(None),
                        Some(_) => return Err(EngineError::MissingRun(missing)),
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Some(target))
    }

    /// Drives an abandoned admission from engine-owned shared state. Parent
    /// resolution is revalidated by the parent actor immediately before its
    /// durable append, so a concurrent retry cannot be resolved by a stale sweep.
    pub(super) async fn sweep_abandoned_admission(
        &self,
        invocation_id: InvocationId,
        generation: u64,
    ) -> Result<(), EngineError> {
        let observed = self.observe_abandoned_admission(invocation_id, generation)?;
        #[cfg(test)]
        let hook = self
            .inner
            .test_hooks
            .abandoned_sweep_hook
            .lock()
            .expect("abandoned sweep hook lock poisoned")
            .clone();
        #[cfg(test)]
        if let Some(observed) = observed
            && let Some(hook) = hook
        {
            let _ = hook.reached.send(());
            let _ = hook
                .captured
                .send(observed.child_run_id.into_iter().collect());
            hook.release.notified().await;
        }
        let Some(observed) = observed else {
            return Ok(());
        };
        let Some(target) = self.cancel_abandoned_admission(invocation_id, generation, observed)?
        else {
            return Ok(());
        };
        if target.resume_attach {
            self.rollback_running_resume(
                invocation_id,
                target
                    .child_session_id
                    .ok_or_else(|| EngineError::ToolFailed("resume child is missing".into()))?,
                target
                    .child_run_id
                    .ok_or_else(|| EngineError::ToolFailed("resume run is missing".into()))?,
                target.resume_admission_seq.ok_or_else(|| {
                    EngineError::ToolFailed("resume admission sequence is missing".into())
                })?,
                target.previous_record.ok_or_else(|| {
                    EngineError::ToolFailed("previous delegation owner is missing".into())
                })?,
                SessionStatus::Cancelled,
            )
            .await?;
        }
        let short_id = target
            .child_session_id
            .and_then(|id| self.session_short_id(id));
        let result = cancelled_delegate_result_with_reason(
            target.child_session_id,
            short_id.as_deref(),
            "delegate admission was abandoned",
        );
        let parent_session_id = target.parent_session_id;
        self.on_actor(parent_session_id, move |engine| {
            engine.resolve_abandoned_delegate_failure_if_pending_direct(
                invocation_id,
                generation,
                parent_session_id,
                target.parent_run_id,
                target.parent_tool_call_id,
                result,
            )
        })
        .await
        .map(|_| ())
    }
}

#[derive(Clone, Copy)]
pub(super) struct InflightDelegation {
    pub(super) parent_run_id: RunId,
    parent_session_id: Option<SessionId>,
    parent_tool_call_id: Option<ToolCallId>,
    child_session_id: Option<SessionId>,
    pub(super) child_run_id: Option<RunId>,
    starting: bool,
    pub(super) cancelled: bool,
    resume_attach: bool,
    resume_admission_seq: Option<u64>,
    previous_record: Option<DelegationRecord>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct AbandonedAdmission {
    parent_session_id: SessionId,
    parent_run_id: RunId,
    parent_tool_call_id: ToolCallId,
    child_session_id: Option<SessionId>,
    child_run_id: Option<RunId>,
    resume_attach: bool,
    resume_admission_seq: Option<u64>,
    previous_record: Option<DelegationRecord>,
}

/// Removes only its own admission generation. Concurrent redeliveries retain
/// independent entries until every caller has completed or unwound.
pub(super) struct AdmissionGuard {
    inner: Arc<Inner>,
    invocation_id: InvocationId,
    pub(super) generation: u64,
    completed: bool,
}

impl AdmissionGuard {
    pub(super) fn begin(
        inner: Arc<Inner>,
        invocation_id: InvocationId,
        parent_run_id: RunId,
    ) -> Self {
        let generation = inner
            .delegation
            .next_admission_generation
            .fetch_add(1, Ordering::Relaxed);
        let mut admissions = inner
            .delegation
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entries = admissions.entry(invocation_id).or_default();
        // Keep abandoned generations until their sweeper has observed them.
        // Removing them here would discard the child/run identity needed to
        // finish a pending cancellation.
        entries.insert(
            generation,
            InflightDelegation {
                parent_run_id,
                parent_session_id: None,
                parent_tool_call_id: None,
                child_session_id: None,
                child_run_id: None,
                starting: false,
                cancelled: false,
                resume_attach: false,
                resume_admission_seq: None,
                previous_record: None,
            },
        );
        drop(admissions);
        Self {
            inner,
            invocation_id,
            generation,
            completed: false,
        }
    }

    pub(super) fn complete(&mut self) {
        self.completed = true;
        self.remove();
    }

    pub(super) fn handoff(&mut self) {
        // The successful admission is now observed by DelegateAwait. Keep its
        // generation live until the child reaches a terminal state so a stale
        // concurrent redelivery cannot cancel the shared child.
        self.completed = true;
    }

    pub(super) fn set_parent(&self, parent_session_id: SessionId, parent_tool_call_id: ToolCallId) {
        if let Some(admission) = self
            .inner
            .delegation
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(&self.invocation_id)
            .and_then(|entries| entries.get_mut(&self.generation))
        {
            admission.parent_session_id = Some(parent_session_id);
            admission.parent_tool_call_id = Some(parent_tool_call_id);
        }
    }

    pub(super) fn remove(&self) {
        let mut admissions = self
            .inner
            .delegation
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entries) = admissions.get_mut(&self.invocation_id) {
            entries.remove(&self.generation);
            if entries.is_empty() {
                admissions.remove(&self.invocation_id);
            }
        }
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut admissions = self
            .inner
            .delegation
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let abandoned = if let Some(entries) = admissions.get_mut(&self.invocation_id)
            && let Some(admission) = entries.get_mut(&self.generation)
        {
            // An abandoned delegate_invoke has no handle for the caller to
            // cancel. Retain a cancellation gate so a concurrent creator
            // cannot start its child after this future is dropped.
            admission.cancelled = true;
            true
        } else {
            false
        };
        drop(admissions);
        if abandoned
            && let Some(runtime) = self
                .inner
                .runtime
                .clone()
                .or_else(|| tokio::runtime::Handle::try_current().ok())
        {
            let engine = Engine {
                inner: self.inner.clone(),
            };
            let sweep_engine = engine.clone();
            let invocation_id = self.invocation_id;
            let generation = self.generation;
            let _ = engine.spawn_admission_task(&runtime, async move {
                if let Err(error) = sweep_engine
                    .sweep_abandoned_admission(invocation_id, generation)
                    .await
                {
                    eprintln!("delegate admission sweep failed: {error}");
                }
            });
        }
    }
}

/// The root session of a child of `parent`.
fn delegated_root(parent: &SessionMeta) -> SessionId {
    match parent.origin {
        SessionOrigin::Delegated {
            root_session_id, ..
        } => root_session_id,
        _ => parent.session_id,
    }
}
