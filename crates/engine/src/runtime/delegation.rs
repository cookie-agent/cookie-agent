use std::sync::{Arc, atomic::Ordering};

use cookie_agent_protocol::{
    DelegateRequestPayload, DelegatedContextRole, DelegatedContextTurn, InvocationId,
    PersistedToolResult as ToolResult, RunId, RunStartParams, SafeToolError, SessionId,
    SessionOrigin, SessionStatus, ToolCallId, ToolCallTermination, ToolTerminationOutcome,
};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{
    Engine, EngineError, Event, SessionCommand,
    admission::AdmissionGuard,
    helpers::{invocation_id, safe_code, safe_display, safe_error, session_depth},
};
use crate::{
    EngineHistoryView,
    delegation_api::{DelegateAwait, DelegateHandle, DelegateInvocation},
    delegation_events::{self, DelegationEventError},
    policy::{self, FrozenRunPolicy, freeze_delegated_agent_policy, resolve_agent},
    session,
};

impl Engine {
    pub(crate) fn validate_resume_target(
        &self,
        parent_session_id: SessionId,
        resume_session_id: SessionId,
        agent_type: &cookie_agent_protocol::AgentId,
    ) -> Result<session::SessionProjection, EngineError> {
        if resume_session_id == parent_session_id {
            return Err(EngineError::MissingTool(
                "resume_session_id cannot reference the delegating session itself".into(),
            ));
        }
        let mut cursor = parent_session_id;
        let mut visited = std::collections::HashSet::new();
        while visited.insert(cursor) {
            let current = self.inner.store.get(cursor)?;
            let SessionOrigin::Delegated {
                parent_session_id, ..
            } = current.meta.origin
            else {
                break;
            };
            if parent_session_id == resume_session_id {
                return Err(EngineError::MissingTool(
                    "resume_session_id cannot reference an ancestor session".into(),
                ));
            }
            cursor = parent_session_id;
        }
        let resumed = self.inner.store.get(resume_session_id).map_err(|_| {
            EngineError::MissingTool(format!("resume session {resume_session_id} was not found"))
        })?;
        let direct_child = matches!(
            resumed.meta.origin,
            SessionOrigin::Delegated {
                parent_session_id: owner,
                ..
            } if owner == parent_session_id
        ) && self.inner.delegation_events.entries().iter().any(|entry| {
            entry.reservation.parent_session_id == parent_session_id
                && entry.reservation.child_session_id == resume_session_id
        });
        if !direct_child {
            return Err(EngineError::MissingTool(
                "resume session is not a prior direct child of the delegating parent".into(),
            ));
        }
        if &resumed.meta.creation_selection.agent != agent_type {
            return Err(EngineError::MissingTool(
                "resume session agent does not match agent_type".into(),
            ));
        }
        if let Some(record) = self
            .inner
            .delegations_by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .get(&resume_session_id)
            .copied()
            && matches!(
                record.state,
                DelegationState::Queued | DelegationState::Starting
            )
        {
            return Err(EngineError::MissingTool(format!(
                "resume session already has in-flight delegation {} in {} state",
                record.invocation_id,
                if record.state == DelegationState::Queued {
                    "queued"
                } else {
                    "starting"
                }
            )));
        }
        Ok(resumed)
    }

    async fn delegated_context_seed(
        &self,
        parent_session_id: SessionId,
    ) -> Result<Vec<DelegatedContextTurn>, EngineError> {
        let history = self
            .get_history(parent_session_id, EngineHistoryView::Assembled)
            .await?;
        Ok(context_seed_from_history(history))
    }

    /// Serializes the durable parent backlink per invocation. Every admission
    /// path re-checks under this barrier; only the first appends it.
    pub(super) async fn ensure_parent_link(
        &self,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        child_session_id: SessionId,
    ) -> Result<(), EngineError> {
        self.request(parent_session_id, |reply| {
            SessionCommand::EnsureToolCallLinked {
                run: parent_run_id,
                tool_call_id: parent_tool_call_id,
                child_session_id,
                reply,
            }
        })
        .await
    }

    pub(crate) fn ensure_parent_link_blocking(
        &self,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        child_session_id: SessionId,
    ) -> Result<(), EngineError> {
        let ensure = || {
            self.request_blocking(parent_session_id, |reply| {
                SessionCommand::EnsureToolCallLinked {
                    run: parent_run_id,
                    tool_call_id: parent_tool_call_id,
                    child_session_id,
                    reply,
                }
            })
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|scope| {
                scope
                    .spawn(ensure)
                    .join()
                    .expect("ensure-link helper thread panicked")
            })
        } else {
            ensure()
        }
    }

    pub(super) async fn terminal_parent_delegate(
        &self,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
    ) -> Result<bool, EngineError> {
        let parent = self.inner.store.get(parent_session_id)?;
        let Some(run) = parent.runs.get(&parent_run_id) else {
            return Ok(true);
        };
        if !matches!(
            run.status,
            SessionStatus::Cancelled | SessionStatus::Failed | SessionStatus::Completed
        ) {
            return Ok(false);
        }
        if run
            .pending_calls
            .get(&parent_tool_call_id)
            .is_some_and(|tool| tool == "delegate_subagent")
        {
            let result =
                cancelled_delegate_result_with_reason(None, "parent run was already terminal");
            self.append(
                parent_session_id,
                Some(parent_run_id),
                Event::ToolCallTerminated {
                    termination: ToolCallTermination {
                        tool_call_id: parent_tool_call_id,
                        owner: self.tool_call_owner(
                            parent_session_id,
                            parent_run_id,
                            parent_tool_call_id,
                        )?,
                        outcome: ToolTerminationOutcome::Cancelled,
                        result: Some(result),
                        error: Some(SafeToolError {
                            code: safe_code("parent_terminal"),
                            message: safe_error("parent run was already terminal"),
                        }),
                    },
                },
            )
            .await?;
        }
        Ok(true)
    }

    /// Admits a delegate invocation, creates/attaches its child, and starts the
    /// invocation-derived child run exactly once.
    pub async fn delegate_invoke(
        &self,
        invocation: DelegateInvocation,
    ) -> Result<DelegateHandle, EngineError> {
        let invocation_id = invocation_id(
            invocation.parent_session_id,
            invocation.parent_run_id,
            invocation.parent_tool_call_id,
        );
        let mut admission =
            AdmissionGuard::begin(self.inner.clone(), invocation_id, invocation.parent_run_id);
        admission.set_parent(invocation.parent_session_id, invocation.parent_tool_call_id);
        // The admission task, rather than this observer, owns the durable start
        // confirmation and therefore survives a dropped caller future.
        let (reply, receiver) = oneshot::channel();
        let engine = self.clone();
        let generation = admission.generation;
        let Some(runtime) = tokio::runtime::Handle::try_current().ok() else {
            return Err(EngineError::ActorStopped);
        };
        if !self.spawn_admission_task(&runtime, async move {
            let result = engine
                .delegate_invoke_admitted(invocation, invocation_id, generation)
                .await;
            let _ = reply.send(result);
        }) {
            return Err(EngineError::ActorStopped);
        }
        let result = receiver.await.map_err(|_| EngineError::ActorStopped)?;
        if result.is_ok() {
            admission.handoff();
        } else {
            admission.complete();
        }
        result
    }

    pub(super) async fn delegate_invoke_admitted(
        &self,
        invocation: DelegateInvocation,
        invocation_id: InvocationId,
        generation: u64,
    ) -> Result<DelegateHandle, EngineError> {
        let admission_guard = self.inner.delegation_admission.lock().await;
        if invocation
            .prompt
            .starts_with(super::skills::RESERVED_STAGED_SKILL_PREFIX)
        {
            return Err(EngineError::MissingTool(
                "delegate prompt uses a reserved staged-skill prefix".into(),
            ));
        }
        let staged_skill = self
            .inner
            .pending_skill_forks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&invocation.parent_tool_call_id)
            .map(super::skills::PreparedSkillInvocation::staged_payload);
        if invocation.resume_session_id.is_some() && invocation.inherit_context {
            return Err(EngineError::MissingTool(
                "resume_session_id and inherit_context cannot both be set".into(),
            ));
        }
        if let Some(existing) = self.delegation_event_get(invocation_id).await?
            && existing.child_run_id.is_some()
        {
            validate_redelivery_mode(existing.request.background, invocation.background)?;
            let request_matches = existing.reservation.parent_session_id
                == invocation.parent_session_id
                && existing.reservation.parent_run_id == invocation.parent_run_id
                && existing.reservation.parent_tool_call_id == invocation.parent_tool_call_id
                && existing.child_agent.agent == invocation.agent_type
                && existing.request.description == invocation.description
                && existing.request.prompt == invocation.prompt
                && existing.request.resume_session_id == invocation.resume_session_id
                && existing.request.inherit_context == invocation.inherit_context
                && existing.request.staged_skill == staged_skill;
            if !request_matches {
                return Err(EngineError::MissingTool(
                    "delegate redelivery does not match its durable reservation".into(),
                ));
            }
            let registry_owns_invocation = self
                .inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?
                .get(&existing.reservation.child_session_id)
                .is_some_and(|record| record.invocation_id == invocation_id);
            if existing.run_attached || registry_owns_invocation {
                return Ok(DelegateHandle {
                    invocation_id,
                    child_session_id: existing.reservation.child_session_id,
                    child_run_id: existing.child_run_id,
                });
            }
        }
        let parent = self.inner.store.get(invocation.parent_session_id)?;
        if parent
            .runs
            .get(&invocation.parent_run_id)
            .is_some_and(|run| run.status == SessionStatus::Interrupted)
            && self.delegation_event_get(invocation_id).await?.is_none()
        {
            return Err(EngineError::MissingTool(
                "delegate parent run is interrupted; use recovery".into(),
            ));
        }
        let active_parent = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&invocation.parent_run_id)
            .cloned()
            .ok_or_else(|| EngineError::MissingTool("delegate parent run is not active".into()))?;
        let parent_policy = Arc::clone(&active_parent.policy);
        let parent_delegation = parent_policy
            .agent
            .delegation
            .as_ref()
            .ok_or_else(|| EngineError::MissingTool("delegation is disabled".into()))?;
        if !parent_delegation.targets.contains(&invocation.agent_type)
            || session_depth(&parent.meta.origin) >= parent_delegation.effective_depth_ceiling
        {
            return Err(EngineError::MissingTool(
                "delegate target or depth is not allowed".into(),
            ));
        }
        let resume_child = invocation
            .resume_session_id
            .map(|resume_session_id| {
                self.validate_resume_target(
                    invocation.parent_session_id,
                    resume_session_id,
                    &invocation.agent_type,
                )
            })
            .transpose()?;
        if resume_child.as_ref().is_some_and(|child| {
            child.meta.creation_selection.preset.as_deref() != parent_policy.registry.preset()
        }) {
            return Err(EngineError::MissingTool(
                "resume target belongs to a different agent preset".into(),
            ));
        }
        let child_agent = resolve_agent(&parent_policy.registry, &invocation.agent_type)?;
        if !child_agent.document.frontmatter.enabled
            || !matches!(
                child_agent.document.frontmatter.mode,
                cookie_agent_config::AgentMode::Subagent | cookie_agent_config::AgentMode::All
            )
        {
            return Err(EngineError::DisabledAgent(invocation.agent_type));
        }
        let fallback_index = active_parent.fallback_index.load(Ordering::Acquire) as usize;
        let inherited = parent_policy.active_suffix(fallback_index);
        let child_selection = child_agent
            .resolved_fallback
            .iter()
            .filter_map(|fallback| match fallback {
                crate::runtime_snapshot::ResolvedAgentFallback::Selection(selection) => {
                    Some(selection)
                }
                crate::runtime_snapshot::ResolvedAgentFallback::ParentModel => None,
            })
            .find(|selection| {
                parent_policy
                    .runtime
                    .models
                    .model(&selection.model)
                    .is_some_and(|model| {
                        model.model.status
                            == cookie_agent_models::compiler::CompiledModelStatus::Available
                    })
            })
            .cloned()
            .or_else(|| {
                if child_agent.resolved_fallback.is_empty() {
                    inherited.first().map(|binding| binding.selection.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| EngineError::MissingTool("delegate child has no model suffix".into()))?;
        let child_policy = freeze_delegated_child_policy(
            child_agent,
            &parent_policy,
            &child_selection,
            inherited,
            parent_delegation.effective_depth_ceiling,
            policy::ResultLimits {
                tool_output_max_lines: self.inner.config.runtime.tool_output.max_lines,
                tool_output_max_bytes: self.inner.config.runtime.tool_output.max_bytes,
            },
        )?;
        let seeded_context = if invocation.inherit_context {
            self.delegated_context_seed(invocation.parent_session_id)
                .await?
        } else {
            Vec::new()
        };
        let proposed_title = super::titles::delegated_title(
            &invocation.description,
            self.inner.config.runtime.session_title.max_chars,
        )?;
        let title = resume_child
            .as_ref()
            .and_then(|child| child.meta.title.clone())
            .unwrap_or(proposed_title);
        let request = DelegateRequestPayload {
            description: invocation.description,
            prompt: invocation.prompt,
            title,
            resume_session_id: invocation.resume_session_id,
            inherit_context: invocation.inherit_context,
            seeded_context,
            background: invocation.background,
            staged_skill,
        };
        let fingerprint = delegation_events::delegation_request_fingerprint(
            &child_policy.agent,
            &child_policy.selected_suffix,
            &request,
        )
        .map_err(|()| EngineError::RuntimeCompileFailed)?;

        let root_session_id = match parent.meta.origin {
            SessionOrigin::Delegated {
                root_session_id, ..
            } => root_session_id,
            _ => parent.meta.session_id,
        };
        let resumed_running = if resume_child
            .as_ref()
            .is_some_and(|child| child.status == SessionStatus::Running)
        {
            let record = self
                .inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?
                .get(
                    &resume_child
                        .as_ref()
                        .expect("checked resume child")
                        .meta
                        .session_id,
                )
                .copied()
                .ok_or_else(|| {
                    EngineError::MissingTool(
                        "running resume session has no active delegation record".into(),
                    )
                })?;
            let run_id = record.child_run_id.ok_or_else(|| {
                EngineError::MissingTool("running resume session has no active run".into())
            })?;
            Some((run_id, record))
        } else {
            None
        };
        let fresh_counts_slot =
            invocation.background && !matches!(parent.meta.origin, SessionOrigin::Delegated { .. });
        let counts_slot = resumed_running
            .map(|(_, record)| record.counts_slot)
            .unwrap_or(fresh_counts_slot);
        let queued = resumed_running.is_none()
            && fresh_counts_slot
            && self.background_slot_unavailable(root_session_id)?;
        if queued && self.background_queue_full(root_session_id)? {
            return Err(EngineError::MissingTool(format!(
                "delegate admission denied: background queue is full for concurrency limit {}",
                self.inner
                    .config
                    .runtime
                    .delegation
                    .max_concurrency
                    .unwrap_or(4)
            )));
        }
        let child = match self
            .create_child(
                invocation.parent_session_id,
                invocation.parent_run_id,
                invocation.parent_tool_call_id,
                &invocation.agent_type,
                child_policy,
                fingerprint,
                request,
                Some((invocation_id, generation)),
            )
            .await
        {
            Ok(child) => child,
            Err(error) => {
                if is_delegation_event_append_failure(&error) {
                    let result =
                        delegate_failure_result(None, "delegate reservation event append failed");
                    self.resolve_delegate_failure_if_pending(
                        invocation.parent_session_id,
                        invocation.parent_run_id,
                        invocation.parent_tool_call_id,
                        result,
                    )
                    .await?;
                }
                return Err(error);
            }
        };
        self.transfer_pending_skill_fork(invocation.parent_tool_call_id, child.session_id);
        self.publish_admission_child(invocation_id, generation, child.session_id)?;
        let entry = self
            .delegation_event_get(invocation_id)
            .await?
            .ok_or_else(|| EngineError::MissingTool("delegate reservation disappeared".into()))?;
        #[cfg(test)]
        let skill_fork_hook = if entry.request.staged_skill.is_some() {
            self.inner
                .skill_fork_reservation_hook
                .lock()
                .expect("skill fork reservation hook lock poisoned")
                .take()
        } else {
            None
        };
        #[cfg(test)]
        if let Some(hook) = skill_fork_hook {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("skill fork reservation reached lock poisoned")
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
        if resume_child.is_some() {
            self.inner
                .delegation_queue
                .lock()
                .map_err(|_| EngineError::ActorStopped)?
                .retain(|session_id| *session_id != child.session_id);
        }
        if let Some((child_run_id, previous_record)) = resumed_running {
            if entry.child_run_id == Some(child_run_id)
                && (entry.run_attached || previous_record.invocation_id == invocation_id)
            {
                return Ok(DelegateHandle {
                    invocation_id,
                    child_session_id: child.session_id,
                    child_run_id: Some(child_run_id),
                });
            }
            let delegation_events = self.inner.delegation_events.clone();
            self.spawn_admission_blocking(move || {
                delegation_events.mark_run_attached(invocation_id, child_run_id)
            })
            .await?;
            #[cfg(test)]
            self.wait_for_resume_test_hook(&self.inner.resume_attachment_hook)
                .await;
            if let Err(error) = self.publish_resume_admission_target(
                invocation_id,
                generation,
                child.session_id,
                child_run_id,
                previous_record,
            ) {
                let delegation_events = self.inner.delegation_events.clone();
                self.spawn_admission_blocking(move || {
                    delegation_events.mark_finished(invocation_id, SessionStatus::Cancelled)
                })
                .await?;
                return Err(error);
            }
            let handle = DelegateHandle {
                invocation_id,
                child_session_id: child.session_id,
                child_run_id: Some(child_run_id),
            };
            let monitor_release = if invocation.background || counts_slot {
                match self.spawn_background_monitor_gated(handle) {
                    Ok(release) => Some(release),
                    Err(error) => {
                        let delegation_events = self.inner.delegation_events.clone();
                        self.spawn_admission_blocking(move || {
                            delegation_events.mark_finished(invocation_id, SessionStatus::Cancelled)
                        })
                        .await?;
                        return Err(error);
                    }
                }
            } else {
                None
            };
            #[cfg(test)]
            if let Some(hook) = {
                self.inner
                    .resume_admission_hook
                    .lock()
                    .expect("resume admission hook lock poisoned")
                    .clone()
            } {
                if let Some(reached) = hook
                    .reached
                    .lock()
                    .expect("resume admission reached lock poisoned")
                    .take()
                {
                    let _ = reached.send(());
                }
                hook.release.notified().await;
            }
            let admission = match self
                .request(child.session_id, |reply| {
                    SessionCommand::AdmitDelegatedResume {
                        run: child_run_id,
                        input: entry.request.prompt.clone(),
                        reply,
                    }
                })
                .await
            {
                Ok(admission) => admission,
                Err(error) => {
                    let delegation_events = self.inner.delegation_events.clone();
                    self.spawn_admission_blocking(move || {
                        delegation_events.mark_finished(invocation_id, SessionStatus::Cancelled)
                    })
                    .await?;
                    return Err(error);
                }
            };
            if !admission.accepted {
                let delegation_events = self.inner.delegation_events.clone();
                self.spawn_admission_blocking(move || {
                    delegation_events.mark_finished(invocation_id, SessionStatus::Cancelled)
                })
                .await?;
                return Err(EngineError::MissingTool(
                    "resume session stopped before its prompt was admitted".into(),
                ));
            }
            let Some(admission_seq) = admission.admission_seq else {
                let delegation_events = self.inner.delegation_events.clone();
                self.spawn_admission_blocking(move || {
                    delegation_events.mark_finished(invocation_id, SessionStatus::Cancelled)
                })
                .await?;
                let _ = self.cancel_run_durably(
                    child_run_id,
                    Some("resume admission sequence was not published".into()),
                );
                return Err(EngineError::MissingTool(
                    "resume admission sequence is missing".into(),
                ));
            };
            if let Err(error) =
                self.publish_resume_admission_seq(invocation_id, generation, admission_seq)
            {
                drop(monitor_release);
                self.rollback_running_resume(
                    invocation_id,
                    child.session_id,
                    child_run_id,
                    admission_seq,
                    previous_record,
                    SessionStatus::Cancelled,
                )
                .await?;
                return Err(error);
            }
            #[cfg(test)]
            self.wait_for_resume_test_hook(&self.inner.resume_rollback_hook)
                .await;
            if !self.admission_generation_live(invocation_id, generation) {
                drop(monitor_release);
                self.rollback_running_resume(
                    invocation_id,
                    child.session_id,
                    child_run_id,
                    admission_seq,
                    previous_record,
                    SessionStatus::Cancelled,
                )
                .await?;
                return Err(EngineError::MissingTool(
                    "delegate admission was abandoned".into(),
                ));
            }
            let registry_failed = {
                match self.inner.delegations_by_session.lock() {
                    Ok(mut records) => {
                        records.insert(
                            child.session_id,
                            DelegationRecord {
                                parent_session_id: invocation.parent_session_id,
                                parent_run_id: invocation.parent_run_id,
                                parent_tool_call_id: invocation.parent_tool_call_id,
                                invocation_id,
                                root_session_id,
                                child_run_id: Some(child_run_id),
                                state: DelegationState::Running,
                                background: invocation.background,
                                counts_slot,
                                notification_sent: false,
                            },
                        );
                        false
                    }
                    Err(_) => true,
                }
            };
            if registry_failed {
                drop(monitor_release);
                self.rollback_running_resume(
                    invocation_id,
                    child.session_id,
                    child_run_id,
                    admission_seq,
                    previous_record,
                    SessionStatus::Cancelled,
                )
                .await?;
                return Err(EngineError::ActorStopped);
            }
            if let Some(release) = monitor_release
                && release.send(()).is_err()
            {
                self.rollback_running_resume(
                    invocation_id,
                    child.session_id,
                    child_run_id,
                    admission_seq,
                    previous_record,
                    SessionStatus::Cancelled,
                )
                .await?;
                return Err(EngineError::ActorStopped);
            }
            if !self.admission_generation_live(invocation_id, generation) {
                self.rollback_running_resume(
                    invocation_id,
                    child.session_id,
                    child_run_id,
                    admission_seq,
                    previous_record,
                    SessionStatus::Cancelled,
                )
                .await?;
                return Err(EngineError::MissingTool(
                    "delegate admission was abandoned".into(),
                ));
            }
            return Ok(handle);
        }
        self.inner
            .delegations_by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .insert(
                child.session_id,
                DelegationRecord {
                    parent_session_id: invocation.parent_session_id,
                    parent_run_id: invocation.parent_run_id,
                    parent_tool_call_id: invocation.parent_tool_call_id,
                    invocation_id,
                    root_session_id,
                    child_run_id: None,
                    state: if queued {
                        DelegationState::Queued
                    } else {
                        DelegationState::Starting
                    },
                    background: invocation.background,
                    counts_slot,
                    notification_sent: false,
                },
            );
        if queued {
            let position = {
                let mut queue = self
                    .inner
                    .delegation_queue
                    .lock()
                    .map_err(|_| EngineError::ActorStopped)?;
                queue.push_back(child.session_id);
                u32::try_from(queue.len()).ok()
            };
            if let Err(error) = self
                .append(
                    invocation.parent_session_id,
                    Some(invocation.parent_run_id),
                    Event::DelegateQueued {
                        session_id: child.session_id,
                        position,
                    },
                )
                .await
            {
                self.inner
                    .delegation_queue
                    .lock()
                    .map_err(|_| EngineError::ActorStopped)?
                    .retain(|session_id| *session_id != child.session_id);
                let delegation_events = self.inner.delegation_events.clone();
                let _ = self
                    .spawn_admission_blocking(move || {
                        delegation_events.mark_finished(invocation_id, SessionStatus::Failed)
                    })
                    .await;
                let terminalized = self
                    .terminalize_child_without_run(
                        child.session_id,
                        SessionStatus::Failed,
                        "delegate queue event append failed",
                    )
                    .await;
                if let Some(record) = self
                    .inner
                    .delegations_by_session
                    .lock()
                    .map_err(|_| EngineError::ActorStopped)?
                    .get_mut(&child.session_id)
                {
                    record.state = DelegationState::Finished(SessionStatus::Failed);
                }
                drop(admission_guard);
                if terminalized.is_ok() {
                    self.finish_background_or_retry(child.session_id, invocation_id)
                        .await;
                } else {
                    let _ = self.start_queued_delegation(root_session_id).await;
                }
                return Err(error);
            }
            return Ok(DelegateHandle {
                invocation_id,
                child_session_id: child.session_id,
                child_run_id: None,
            });
        }
        let child_run_id = match self
            .ensure_delegate_run(&entry, Some((invocation_id, generation)))
            .await
        {
            Ok(run_id) => run_id,
            Err(error) => {
                if is_delegation_event_append_failure(&error) {
                    let result = delegate_failure_result(
                        Some(entry.reservation.child_session_id),
                        "delegate run event confirmation failed",
                    );
                    let _ = self
                        .resolve_delegate_failure_if_pending(
                            entry.reservation.parent_session_id,
                            entry.reservation.parent_run_id,
                            entry.reservation.parent_tool_call_id,
                            result,
                        )
                        .await;
                }
                let delegation_events = self.inner.delegation_events.clone();
                let _ = self
                    .spawn_admission_blocking(move || {
                        delegation_events.mark_finished(invocation_id, SessionStatus::Failed)
                    })
                    .await;
                let terminalized = self
                    .terminalize_child_without_run(
                        child.session_id,
                        SessionStatus::Failed,
                        "delegate child startup failed",
                    )
                    .await;
                if let Some(record) = self
                    .inner
                    .delegations_by_session
                    .lock()
                    .map_err(|_| EngineError::ActorStopped)?
                    .get_mut(&child.session_id)
                {
                    record.state = DelegationState::Finished(SessionStatus::Failed);
                }
                drop(admission_guard);
                if invocation.background {
                    if terminalized.is_ok() {
                        self.finish_background_or_retry(child.session_id, invocation_id)
                            .await;
                    } else {
                        let _ = self.start_queued_delegation(root_session_id).await;
                    }
                }
                return Err(error);
            }
        };
        if let Some(record) = self
            .inner
            .delegations_by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .get_mut(&child.session_id)
        {
            record.child_run_id = Some(child_run_id);
            record.state = DelegationState::Running;
        }
        let handle = DelegateHandle {
            invocation_id,
            child_session_id: child.session_id,
            child_run_id: Some(child_run_id),
        };
        if invocation.background
            && let Err(error) = self.spawn_background_monitor(handle)
        {
            let _ = self.cancel_run_durably(
                child_run_id,
                Some("background delegate monitor failed to start".into()),
            );
            drop(admission_guard);
            self.finish_background_or_retry(child.session_id, invocation_id)
                .await;
            return Err(error);
        }
        Ok(handle)
    }

    /// Waits for a child terminal state and returns the bounded model-visible
    /// delegate result. Cancellation is represented as structured JSON.
    pub fn await_delegate(&self, handle: DelegateHandle) -> DelegateAwait {
        let engine = self.clone();
        DelegateAwait {
            future: Box::pin(async move { engine.await_delegate_inner(handle).await }),
            engine: self.clone(),
            runtime: self.inner.runtime.clone(),
            handle,
            completed: false,
        }
    }

    pub(super) async fn await_delegate_inner(
        &self,
        handle: DelegateHandle,
    ) -> Result<ToolResult, EngineError> {
        loop {
            let child = match self.inner.store.get(handle.child_session_id) {
                Ok(child) => child,
                Err(_) => {
                    return Ok(delegate_failure_result(
                        Some(handle.child_session_id),
                        "child session is missing",
                    ));
                }
            };
            let status = handle
                .child_run_id
                .and_then(|run_id| child.runs.get(&run_id).map(|run| run.status))
                .unwrap_or(child.status);
            match status {
                SessionStatus::Running | SessionStatus::Idle => {
                    let active = {
                        self.inner
                            .active
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .get(&handle.child_run_id.expect("running delegate has a run"))
                            .cloned()
                    };
                    if let Some(active) = active {
                        // Event-driven terminal-state wakeups are post-MVP; this
                        // bounded cancellation-aware wait keeps the MVP responsive.
                        tokio::select! {
                            () = active.cancellation.cancelled() => {},
                            () = tokio::time::sleep(std::time::Duration::from_millis(25)) => {},
                        }
                    } else {
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    }
                }
                SessionStatus::Completed
                | SessionStatus::Cancelled
                | SessionStatus::Failed
                | SessionStatus::Interrupted => {
                    let delegation_events = self.inner.delegation_events.clone();
                    self.spawn_admission_blocking(move || {
                        delegation_events.mark_finished(handle.invocation_id, status)
                    })
                    .await?;
                    let result = terminal_delegate_result(&child, handle.child_run_id, status);
                    let mut records = self
                        .inner
                        .delegations_by_session
                        .lock()
                        .map_err(|_| EngineError::ActorStopped)?;
                    if let Some(record) = records
                        .get_mut(&handle.child_session_id)
                        .filter(|record| record.invocation_id == handle.invocation_id)
                    {
                        record.state = DelegationState::Finished(status);
                    }
                    drop(records);
                    self.clear_delegate_admissions(handle.invocation_id);
                    return Ok(result);
                }
            }
        }
    }

    /// Cancels the child run and returns the structured delegate cancellation
    /// result used by the parent tool invocation.
    pub async fn cancel_delegate(&self, handle: DelegateHandle) -> Result<ToolResult, EngineError> {
        let child = match self.inner.store.get(handle.child_session_id) {
            Ok(child) => child,
            Err(_) => {
                return Ok(delegate_failure_result(
                    Some(handle.child_session_id),
                    "child session is missing",
                ));
            }
        };
        if !matches!(child.status, SessionStatus::Running | SessionStatus::Idle) {
            return self.await_delegate(handle).await;
        }
        let Some(child_run_id) = handle.child_run_id else {
            return Ok(cancelled_delegate_result(handle.child_session_id, None));
        };
        let _ = self.cancel_run(child_run_id).await;
        self.await_delegate(handle).await
    }

    pub(super) async fn delegation_event_get(
        &self,
        invocation_id: InvocationId,
    ) -> Result<Option<delegation_events::DelegationEntry>, EngineError> {
        let delegation_events = self.inner.delegation_events.clone();
        self.spawn_admission_blocking(move || {
            Ok::<_, EngineError>(delegation_events.get(invocation_id))
        })
        .await
    }

    pub(super) fn clear_delegate_admissions(&self, invocation_id: InvocationId) {
        self.inner
            .inflight_delegations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&invocation_id);
    }

    pub(super) async fn resolve_delegate_failure_if_pending(
        &self,
        session_id: SessionId,
        run_id: RunId,
        tool_call_id: ToolCallId,
        result: ToolResult,
    ) -> Result<bool, EngineError> {
        self.request(session_id, |reply| {
            SessionCommand::ResolveDelegateFailureIfPending {
                run: run_id,
                tool_call_id,
                result,
                reply,
            }
        })
        .await
    }

    pub(super) fn resolve_delegate_failure_if_pending_direct(
        &self,
        session_id: SessionId,
        run_id: RunId,
        tool_call_id: ToolCallId,
        result: ToolResult,
    ) -> Result<bool, EngineError> {
        let pending = self
            .inner
            .store
            .get(session_id)?
            .runs
            .get(&run_id)
            .is_some_and(|run| {
                run.pending_calls
                    .get(&tool_call_id)
                    .is_some_and(|tool| tool == "delegate_subagent")
            });
        if !pending {
            return Ok(false);
        }
        self.append_direct(
            session_id,
            Some(run_id),
            Event::ToolCallTerminated {
                termination: ToolCallTermination {
                    tool_call_id,
                    owner: self.tool_call_owner(session_id, run_id, tool_call_id)?,
                    outcome: ToolTerminationOutcome::Failed,
                    result: Some(result),
                    error: Some(SafeToolError {
                        code: safe_code("delegate_failed"),
                        message: safe_error("delegated run failed"),
                    }),
                },
            },
        )?;
        Ok(true)
    }

    pub(super) fn resolve_abandoned_delegate_failure_if_pending_direct(
        &self,
        invocation_id: InvocationId,
        generation: u64,
        session_id: SessionId,
        run_id: RunId,
        tool_call_id: ToolCallId,
        result: ToolResult,
    ) -> Result<bool, EngineError> {
        let mut admissions = self
            .inner
            .inflight_delegations
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let still_abandoned = admissions.get(&invocation_id).is_some_and(|entries| {
            entries
                .get(&generation)
                .is_some_and(|entry| entry.cancelled)
                && entries.values().all(|entry| entry.cancelled)
        });
        if !still_abandoned {
            return Ok(false);
        }
        let resolved = self.resolve_delegate_failure_if_pending_direct(
            session_id,
            run_id,
            tool_call_id,
            result,
        )?;
        admissions.remove(&invocation_id);
        Ok(resolved)
    }

    pub(super) async fn ensure_delegate_run(
        &self,
        entry: &delegation_events::DelegationEntry,
        admission: Option<(InvocationId, u64)>,
    ) -> Result<RunId, EngineError> {
        if entry.terminal_status.is_some() {
            return Err(EngineError::MissingTool(
                "delegate invocation was cancelled before startup".into(),
            ));
        }
        #[cfg(test)]
        if self
            .inner
            .delegate_start_failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                (remaining > 0).then(|| remaining - 1)
            })
            .is_ok()
        {
            self.inner.delegate_start_failure_observed.notify_one();
            return Err(EngineError::MissingTool(
                "injected delegate startup failure".into(),
            ));
        }
        if admission.is_some_and(|(invocation_id, generation)| {
            !self.admission_generation_live(invocation_id, generation)
        }) {
            return Err(EngineError::MissingTool(
                "delegate cancelled before child start".into(),
            ));
        }
        let child = self.inner.store.get(entry.reservation.child_session_id)?;
        if let Some(staged_skill) = &entry.request.staged_skill {
            self.stage_child_skill_from_event(entry.reservation.child_session_id, staged_skill);
        }
        if let Some((invocation_id, generation)) = admission {
            self.mark_admission_starting(invocation_id, generation)?;
        }
        if let Some(run_id) = entry.child_run_id
            && child.runs.contains_key(&run_id)
        {
            if let Some((invocation_id, generation)) = admission {
                self.publish_admission_run(
                    invocation_id,
                    generation,
                    entry.reservation.child_session_id,
                    run_id,
                )?;
                if !self.admission_generation_live(invocation_id, generation) {
                    self.sweep_abandoned_admission(invocation_id, generation)
                        .await?;
                    return Err(EngineError::MissingTool(
                        "delegate cancelled before child start".into(),
                    ));
                }
            }
            return Ok(run_id);
        }
        let client_run_id = delegate_client_run_id(entry.reservation.invocation_id);
        let existing_run = child
            .runs
            .values()
            .find(|run| run.client_run_id == client_run_id)
            .map(|run| run.id);
        let run_id = match existing_run {
            Some(run_id) => run_id,
            None => match self
                .request(entry.reservation.child_session_id, |reply| {
                    SessionCommand::Start {
                        params: RunStartParams {
                            session_id: entry.reservation.child_session_id,
                            client_run_id: client_run_id.clone(),
                            selection: child.meta.creation_selection.clone(),
                            input: render_delegate_input(&entry.request),
                        },
                        admission,
                        reply,
                    }
                })
                .await
            {
                Ok(started) => started.run_id,
                Err(error) => {
                    if let Some((invocation_id, generation)) = admission {
                        self.clear_admission_starting(invocation_id, generation);
                        if !self.admission_generation_live(invocation_id, generation) {
                            self.sweep_abandoned_admission(invocation_id, generation)
                                .await?;
                        }
                    }
                    return Err(error);
                }
            },
        };
        let cancelled = if let Some((invocation_id, generation)) = admission {
            // The Start actor published a newly-created run before delivering
            // its reply. Existing local runs reach the same state here.
            self.publish_admission_run(
                invocation_id,
                generation,
                entry.reservation.child_session_id,
                run_id,
            )?;
            !self.admission_generation_live(invocation_id, generation)
        } else {
            false
        };
        // The actor-owned confirmation must run even after its observer drops:
        // the start reply may already have created the child run.
        #[cfg(test)]
        let confirmation_hook = self
            .inner
            .admission_confirmation_hook
            .lock()
            .expect("admission confirmation hook lock poisoned")
            .clone();
        #[cfg(test)]
        if let Some(hook) = confirmation_hook {
            let _ = hook.reached.send(());
            hook.release.wait().await;
        }
        let delegation_events = self.inner.delegation_events.clone();
        let invocation_id = entry.reservation.invocation_id;
        let confirmation = self
            .spawn_admission_blocking(move || {
                delegation_events.mark_run_started(invocation_id, run_id)
            })
            .await;
        if let Err(error) = confirmation {
            // The child already has an active run, so terminally cancel it before
            // the caller resolves the parent through its actor.
            let _ = self.cancel_run_durably(
                run_id,
                Some("delegate run event confirmation failed".into()),
            );
            return Err(error);
        }
        if cancelled {
            if let Some((invocation_id, generation)) = admission {
                self.sweep_abandoned_admission(invocation_id, generation)
                    .await?;
            }
            return Err(EngineError::MissingTool(
                "delegate cancelled during child start".into(),
            ));
        }
        Ok(run_id)
    }

    fn background_slot_unavailable(&self, root_session_id: SessionId) -> Result<bool, EngineError> {
        let Some(limit) = self.inner.config.runtime.delegation.max_concurrency else {
            return Ok(false);
        };
        let running = self
            .inner
            .delegations_by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .values()
            .filter(|record| {
                record.root_session_id == root_session_id
                    && record.counts_slot
                    && record.state == DelegationState::Running
            })
            .count() as u32;
        Ok(running >= limit)
    }

    fn background_queue_full(&self, root_session_id: SessionId) -> Result<bool, EngineError> {
        let Some(limit) = self.inner.config.runtime.delegation.max_concurrency else {
            return Ok(false);
        };
        let queued = self
            .inner
            .delegations_by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .values()
            .filter(|record| {
                record.root_session_id == root_session_id && record.state == DelegationState::Queued
            })
            .count() as u32;
        Ok(background_queue_limit_reached(limit, queued))
    }

    fn spawn_background_monitor(&self, handle: DelegateHandle) -> Result<(), EngineError> {
        let runtime = self
            .inner
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
            .ok_or(EngineError::ActorStopped)?;
        let engine = self.clone();
        let monitor = engine.clone();
        if !self.spawn_admission_task(&runtime, async move {
            let _ = monitor.await_delegate_inner(handle).await;
            loop {
                match monitor
                    .finish_background_delegate(handle.child_session_id, handle.invocation_id)
                    .await
                {
                    Ok(()) => break,
                    Err(error) => {
                        if monitor
                            .inner
                            .admission_tasks_closing
                            .load(Ordering::Acquire)
                        {
                            break;
                        }
                        eprintln!("background delegate completion retrying: {error}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }) {
            return Err(EngineError::ActorStopped);
        }
        Ok(())
    }

    fn spawn_background_monitor_gated(
        &self,
        handle: DelegateHandle,
    ) -> Result<tokio::sync::oneshot::Sender<()>, EngineError> {
        #[cfg(test)]
        if self
            .inner
            .resume_monitor_failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                (remaining > 0).then(|| remaining - 1)
            })
            .is_ok()
        {
            return Err(EngineError::ActorStopped);
        }
        let runtime = self
            .inner
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
            .ok_or(EngineError::ActorStopped)?;
        let (release, admitted) = tokio::sync::oneshot::channel();
        let engine = self.clone();
        let monitor = engine.clone();
        if !self.spawn_admission_task(&runtime, async move {
            if admitted.await.is_err() {
                return;
            }
            let _ = monitor.await_delegate_inner(handle).await;
            loop {
                match monitor
                    .finish_background_delegate(handle.child_session_id, handle.invocation_id)
                    .await
                {
                    Ok(()) => break,
                    Err(error) => {
                        if monitor
                            .inner
                            .admission_tasks_closing
                            .load(Ordering::Acquire)
                        {
                            break;
                        }
                        eprintln!("background delegate completion retrying: {error}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }) {
            return Err(EngineError::ActorStopped);
        }
        Ok(release)
    }

    pub(super) async fn rollback_running_resume(
        &self,
        invocation_id: InvocationId,
        child_session_id: SessionId,
        child_run_id: RunId,
        admission_seq: u64,
        previous_record: DelegationRecord,
        status: SessionStatus,
    ) -> Result<(), EngineError> {
        let recalled = self
            .request(child_session_id, |reply| {
                SessionCommand::RecallDelegatedResume {
                    run: child_run_id,
                    admission_seq,
                    reply,
                }
            })
            .await
            .unwrap_or(false);
        let delegation_events = self.inner.delegation_events.clone();
        self.spawn_admission_blocking(move || {
            delegation_events.mark_finished(invocation_id, status)
        })
        .await?;
        if let Ok(mut records) = self.inner.delegations_by_session.lock()
            && records
                .get(&child_session_id)
                .is_some_and(|record| record.invocation_id == invocation_id)
        {
            records.insert(child_session_id, previous_record);
        }
        if !recalled {
            let child = self.inner.store.get(child_session_id)?;
            if child
                .runs
                .get(&child_run_id)
                .is_some_and(|run| run.status == SessionStatus::Running)
            {
                let _ = self.cancel_run_durably(
                    child_run_id,
                    Some("abandoned resume prompt could not be recalled".into()),
                );
            }
        }
        Ok(())
    }

    async fn finish_background_or_retry(
        &self,
        child_session_id: SessionId,
        invocation_id: InvocationId,
    ) {
        if self
            .finish_background_delegate(child_session_id, invocation_id)
            .await
            .is_err()
        {
            let _ = self.spawn_background_finish_retry(child_session_id, invocation_id);
        }
    }

    fn spawn_background_finish_retry(
        &self,
        child_session_id: SessionId,
        invocation_id: InvocationId,
    ) -> Result<(), EngineError> {
        let runtime = self
            .inner
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
            .ok_or(EngineError::ActorStopped)?;
        let engine = self.clone();
        let retry = engine.clone();
        if !self.spawn_admission_task(&runtime, async move {
            loop {
                match retry
                    .finish_background_delegate(child_session_id, invocation_id)
                    .await
                {
                    Ok(()) => break,
                    Err(error) => {
                        if retry.inner.admission_tasks_closing.load(Ordering::Acquire) {
                            break;
                        }
                        eprintln!("background delegate completion retrying: {error}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }) {
            return Err(EngineError::ActorStopped);
        }
        Ok(())
    }

    async fn finish_background_delegate(
        &self,
        child_session_id: SessionId,
        invocation_id: InvocationId,
    ) -> Result<(), EngineError> {
        let child = self.inner.store.get(child_session_id)?;
        let entry = self
            .delegation_event_get(invocation_id)
            .await?
            .ok_or_else(|| {
                EngineError::MissingTool("delegate reservation event is missing".into())
            })?;
        if entry.reservation.child_session_id != child_session_id {
            return Err(EngineError::MissingTool(
                "delegate reservation child does not match completion monitor".into(),
            ));
        }
        let parent_session_id = entry.reservation.parent_session_id;
        let parent_run_id = entry.reservation.parent_run_id;
        let parent = self.inner.store.get(parent_session_id)?;
        let root_session_id = match parent.meta.origin {
            SessionOrigin::Delegated {
                root_session_id, ..
            } => root_session_id,
            _ => parent_session_id,
        };
        let exact_status = if let Some(status) = entry.terminal_status {
            status
        } else if let Some(child_run_id) = entry.child_run_id {
            child
                .runs
                .get(&child_run_id)
                .map(|run| run.status)
                .ok_or_else(|| EngineError::MissingTool("delegate child run is missing".into()))?
        } else {
            child.status
        };
        if !matches!(
            exact_status,
            SessionStatus::Completed
                | SessionStatus::Failed
                | SessionStatus::Interrupted
                | SessionStatus::Cancelled
        ) {
            return Err(EngineError::MissingTool(
                "delegate child run is not terminal".into(),
            ));
        }
        let (owns_registry, registry_background, already_sent) = {
            let mut records = self
                .inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?;
            if let Some(record) = records
                .get_mut(&child_session_id)
                .filter(|record| record.invocation_id == invocation_id)
            {
                let already_sent = record.notification_sent;
                record.state = DelegationState::Finished(exact_status);
                (true, record.background, already_sent)
            } else {
                (false, false, false)
            }
        };
        let background_return = parent.log.events().iter().any(|event| {
            matches!(
                &event.payload,
                Event::ToolCallTerminated { termination }
                    if termination.tool_call_id == entry.reservation.parent_tool_call_id
                        && termination.result.as_ref().is_some_and(|result| {
                            result.metadata.get("session_id")
                                == Some(&serde_json::json!(child_session_id))
                        })
            )
        });
        let background = registry_background || background_return;
        let already_logged = background
            && parent.log.events().iter().any(|event| {
                matches!(
                    event.payload,
                    Event::DelegateFinishedV2 {
                        invocation_id: logged_invocation,
                        session_id,
                        ..
                    } if logged_invocation == invocation_id && session_id == child_session_id
                )
            });
        if owns_registry
            && already_logged
            && let Some(record) = self
                .inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?
                .get_mut(&child_session_id)
        {
            record.notification_sent = true;
        }
        let notification_result = if background && !already_sent && !already_logged {
            let teaser = if entry.terminal_status.is_some() && entry.child_run_id.is_none() {
                DelegateTeaser {
                    session_id: child_session_id,
                    status: SessionStatus::Cancelled,
                    preview: String::new(),
                    total_lines: 0,
                }
            } else {
                delegate_teaser(&child, child_session_id, exact_status, entry.child_run_id)
            };
            let result = self
                .append(
                    parent_session_id,
                    Some(parent_run_id),
                    Event::DelegateFinishedV2 {
                        invocation_id,
                        session_id: child_session_id,
                        status: exact_status,
                        preview: teaser.preview,
                        total_lines: teaser.total_lines,
                    },
                )
                .await;
            if result.is_ok()
                && owns_registry
                && let Some(record) = self
                    .inner
                    .delegations_by_session
                    .lock()
                    .map_err(|_| EngineError::ActorStopped)?
                    .get_mut(&child_session_id)
            {
                record.notification_sent = true;
            }
            result
        } else {
            Ok(())
        };
        let drain_result = if owns_registry {
            self.start_queued_delegation(root_session_id).await
        } else {
            Ok(())
        };
        let completion = notification_result.and(drain_result);
        if completion.is_ok()
            && let Err(error) = self.evict_idle_subagents().await
        {
            eprintln!("subagent session completion eviction failed: {error}");
        }
        completion
    }

    async fn terminalize_child_without_run(
        &self,
        child_session_id: SessionId,
        status: SessionStatus,
        reason: &str,
    ) -> Result<(), EngineError> {
        #[cfg(test)]
        if self
            .inner
            .delegate_terminal_append_failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                (remaining > 0).then(|| remaining - 1)
            })
            .is_ok()
        {
            return Err(EngineError::MissingTool(
                "injected delegate terminal append failure".into(),
            ));
        }
        let child = self.inner.store.get(child_session_id)?;
        if child.status == SessionStatus::Idle {
            self.append(
                child_session_id,
                None,
                Event::DelegateChildTerminated {
                    status,
                    reason: Some(safe_error(reason)),
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn void_runless_pending_inputs(
        &self,
        child_session_id: SessionId,
    ) -> Result<(), EngineError> {
        let events = self.inner.store.get(child_session_id)?.log.events();
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
        let mut pending = std::collections::VecDeque::new();
        for event in events
            .iter()
            .filter(|event| event.seq > boundary && event.run_id.is_none())
        {
            match &event.payload {
                Event::UserInputAdmitted { input } => pending.push_back(input.clone()),
                Event::UserInputRecalled { .. } => {
                    pending.pop_back();
                }
                _ => {}
            }
        }
        for input in pending.into_iter().rev() {
            self.append(child_session_id, None, Event::UserInputRecalled { input })
                .await?;
        }
        Ok(())
    }

    pub(super) fn void_runless_pending_inputs_blocking(
        &self,
        child_session_id: SessionId,
    ) -> Result<(), EngineError> {
        let events = self.inner.store.get(child_session_id)?.log.events();
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
        let mut pending = std::collections::VecDeque::new();
        for event in events
            .iter()
            .filter(|event| event.seq > boundary && event.run_id.is_none())
        {
            match &event.payload {
                Event::UserInputAdmitted { input } => pending.push_back(input.clone()),
                Event::UserInputRecalled { .. } => {
                    pending.pop_back();
                }
                _ => {}
            }
        }
        for input in pending.into_iter().rev() {
            self.append_blocking(child_session_id, None, Event::UserInputRecalled { input })?;
        }
        Ok(())
    }

    async fn start_queued_delegation(&self, root_session_id: SessionId) -> Result<(), EngineError> {
        let admission_guard = self.inner.delegation_admission.lock().await;
        if self.background_slot_unavailable(root_session_id)? {
            return Ok(());
        }
        let child_session_id = {
            let records = self
                .inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?;
            let queue = self
                .inner
                .delegation_queue
                .lock()
                .map_err(|_| EngineError::ActorStopped)?;
            let position = queue.iter().position(|session_id| {
                records.get(session_id).is_some_and(|record| {
                    record.root_session_id == root_session_id
                        && record.state == DelegationState::Queued
                })
            });
            position.and_then(|position| queue.get(position).copied())
        };
        let Some(child_session_id) = child_session_id else {
            return Ok(());
        };
        let invocation_id = self
            .inner
            .delegations_by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .get(&child_session_id)
            .map(|record| record.invocation_id)
            .ok_or_else(|| {
                EngineError::MissingTool("queued subagent registry entry is missing".into())
            })?;
        let entry = self
            .delegation_event_get(invocation_id)
            .await?
            .ok_or_else(|| {
                EngineError::MissingTool("queued subagent reservation is missing".into())
            })?;
        let child_run_id = self.ensure_delegate_run(&entry, None).await?;
        let removed = {
            let mut records = self
                .inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?;
            let record = records.get_mut(&child_session_id).ok_or_else(|| {
                EngineError::MissingTool("queued subagent registry entry is missing".into())
            })?;
            record.child_run_id = Some(child_run_id);
            record.state = DelegationState::Running;
            let mut queue = self
                .inner
                .delegation_queue
                .lock()
                .map_err(|_| EngineError::ActorStopped)?;
            queue
                .iter()
                .position(|session_id| *session_id == child_session_id)
                .is_some_and(|position| queue.remove(position).is_some())
        };
        if !removed {
            let error =
                EngineError::MissingTool("queued subagent disappeared during startup".into());
            let _ = self.cancel_run_durably(
                child_run_id,
                Some("queued subagent disappeared during startup".into()),
            );
            drop(admission_guard);
            let _ = self.spawn_background_finish_retry(child_session_id, invocation_id);
            return Err(error);
        }
        let handle = DelegateHandle {
            invocation_id,
            child_session_id,
            child_run_id: Some(child_run_id),
        };
        if let Err(error) = self.spawn_background_monitor(handle) {
            let _ = self.cancel_run_durably(
                child_run_id,
                Some("background delegate monitor failed to start".into()),
            );
            drop(admission_guard);
            let _ = self.spawn_background_finish_retry(child_session_id, invocation_id);
            return Err(error);
        }
        Ok(())
    }

    pub fn subagent_agent_type(
        &self,
        caller_session_id: SessionId,
        child_session_id: SessionId,
    ) -> Result<cookie_agent_protocol::AgentId, EngineError> {
        let child = self.inner.store.get(child_session_id)?;
        match child.meta.origin {
            SessionOrigin::Delegated {
                parent_session_id, ..
            } if parent_session_id == caller_session_id => {
                Ok(child.meta.creation_selection.agent.clone())
            }
            _ => Err(EngineError::MissingTool(
                "subagent session is not owned by the caller".into(),
            )),
        }
    }

    async fn ensure_subagent_owned(
        &self,
        caller_session_id: SessionId,
        child_session_id: SessionId,
    ) -> Result<DelegateHandle, EngineError> {
        self.subagent_agent_type(caller_session_id, child_session_id)?;
        let child = self.inner.store.get(child_session_id)?;
        let SessionOrigin::Delegated {
            parent_run_id,
            parent_tool_call_id,
            invocation_id,
            ..
        } = child.meta.origin
        else {
            unreachable!("ownership check requires a delegated origin");
        };
        let registry_record = self
            .inner
            .delegations_by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .get(&child_session_id)
            .copied();
        if registry_record.is_some_and(|record| record.parent_session_id != caller_session_id) {
            return Err(EngineError::MissingTool(
                "subagent registry ownership does not match its parent link".into(),
            ));
        }
        let (parent_run_id, parent_tool_call_id, invocation_id) = registry_record.map_or(
            (parent_run_id, parent_tool_call_id, invocation_id),
            |record| {
                (
                    record.parent_run_id,
                    record.parent_tool_call_id,
                    record.invocation_id,
                )
            },
        );
        self.ensure_parent_link(
            caller_session_id,
            parent_run_id,
            parent_tool_call_id,
            child_session_id,
        )
        .await?;
        let child_run_id = registry_record
            .and_then(|record| record.child_run_id)
            .or_else(|| {
                self.inner
                    .delegation_events
                    .get(invocation_id)
                    .and_then(|entry| entry.child_run_id)
            });
        Ok(DelegateHandle {
            invocation_id,
            child_session_id,
            child_run_id,
        })
    }

    pub async fn get_subagent_result(
        &self,
        caller_session_id: SessionId,
        child_session_id: SessionId,
        wait: bool,
        offset: u32,
        limit: u32,
        cancellation: CancellationToken,
    ) -> Result<ToolResult, EngineError> {
        let handle = self
            .ensure_subagent_owned(caller_session_id, child_session_id)
            .await?;
        loop {
            let child = self.inner.store.get(child_session_id)?;
            let record = self
                .inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?
                .get(&child_session_id)
                .copied();
            let queued = record.is_some_and(|record| record.state == DelegationState::Queued);
            let pending_terminal = record.and_then(|record| match record.state {
                DelegationState::Finished(status) if record.child_run_id.is_none() => Some(status),
                _ => None,
            });
            let terminal = pending_terminal.is_some()
                || (!queued
                    && matches!(
                        child.status,
                        SessionStatus::Completed
                            | SessionStatus::Failed
                            | SessionStatus::Interrupted
                            | SessionStatus::Cancelled
                    ));
            if terminal || !wait {
                let status = if queued {
                    "queued"
                } else if let Some(status) = pending_terminal {
                    session_status_name(status)
                } else {
                    session_status_name(child.status)
                };
                let text = if terminal && pending_terminal.is_none() {
                    delegate_final_text(&child, handle.child_run_id)
                } else {
                    ""
                };
                return Ok(paginated_subagent_result(status, text, offset, limit));
            }
            tokio::select! {
                () = cancellation.cancelled() => {
                    return Err(EngineError::MissingTool("subagent result wait cancelled".into()));
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
            }
        }
    }

    pub async fn steer_subagent(
        &self,
        caller_session_id: SessionId,
        child_session_id: SessionId,
        message: String,
    ) -> Result<ToolResult, EngineError> {
        if message.trim().is_empty() {
            return Err(EngineError::MissingTool(
                "subagent steer message must not be empty".into(),
            ));
        }
        let handle = self
            .ensure_subagent_owned(caller_session_id, child_session_id)
            .await?;
        let admission_guard = self.inner.delegation_admission.lock().await;
        let record = self
            .inner
            .delegations_by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .get(&child_session_id)
            .copied()
            .ok_or_else(|| EngineError::MissingTool("subagent registry entry is missing".into()))?;
        let child = self.inner.store.get(child_session_id)?;
        match record.state {
            DelegationState::Queued => {
                self.append(
                    child_session_id,
                    None,
                    Event::UserInputAdmitted { input: message },
                )
                .await?;
                drop(admission_guard);
                Ok(steered_delegate_result(child_session_id, "queued"))
            }
            DelegationState::Starting | DelegationState::Running => {
                if matches!(
                    child.status,
                    SessionStatus::Completed
                        | SessionStatus::Failed
                        | SessionStatus::Interrupted
                        | SessionStatus::Cancelled
                ) {
                    return Err(EngineError::MissingTool(format!(
                        "subagent is terminal ({}) and cannot be steered",
                        session_status_name(child.status)
                    )));
                }
                let child_run_id =
                    record.child_run_id.or(handle.child_run_id).ok_or_else(|| {
                        EngineError::MissingTool("subagent has not started a run".into())
                    })?;
                drop(admission_guard);
                let result = self
                    .request(child_session_id, |reply| SessionCommand::Steer {
                        run: child_run_id,
                        input: message,
                        original_input: None,
                        reply,
                    })
                    .await?;
                if !result.accepted {
                    return Err(EngineError::MissingTool(
                        "subagent is no longer running".into(),
                    ));
                }
                Ok(steered_delegate_result(child_session_id, "running"))
            }
            DelegationState::Finished(status) => Err(EngineError::MissingTool(format!(
                "subagent is terminal ({}) and cannot be steered",
                session_status_name(status)
            ))),
        }
    }

    pub async fn cancel_subagent(
        &self,
        caller_session_id: SessionId,
        child_session_id: SessionId,
        reason: Option<String>,
    ) -> Result<ToolResult, EngineError> {
        let handle = self
            .ensure_subagent_owned(caller_session_id, child_session_id)
            .await?;
        let admission_guard = self.inner.delegation_admission.lock().await;
        let record = self
            .inner
            .delegations_by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .get(&child_session_id)
            .copied()
            .ok_or_else(|| EngineError::MissingTool("subagent registry entry is missing".into()))?;
        if record.state == DelegationState::Queued {
            self.inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?
                .get(&child_session_id)
                .filter(|record| record.state == DelegationState::Queued)
                .map(|_| ())
                .ok_or_else(|| {
                    EngineError::MissingTool("subagent is not queued for cancellation".into())
                })?;
            let cancellation_reason = reason
                .as_deref()
                .unwrap_or("queued subagent cancelled before startup");
            let delegation_events = self.inner.delegation_events.clone();
            self.spawn_admission_blocking(move || {
                delegation_events.mark_finished(record.invocation_id, SessionStatus::Cancelled)
            })
            .await?;
            self.void_runless_pending_inputs(child_session_id).await?;
            self.terminalize_child_without_run(
                child_session_id,
                SessionStatus::Cancelled,
                cancellation_reason,
            )
            .await?;
            self.inner
                .delegation_queue
                .lock()
                .map_err(|_| EngineError::ActorStopped)?
                .retain(|session_id| *session_id != child_session_id);
            if let Some(record) = self
                .inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?
                .get_mut(&child_session_id)
            {
                record.state = DelegationState::Finished(SessionStatus::Cancelled);
            }
            drop(admission_guard);
            self.finish_background_or_retry(child_session_id, record.invocation_id)
                .await;
            return Ok(cancelled_delegate_result(child_session_id, None));
        }
        let child_run_id = record
            .child_run_id
            .or(handle.child_run_id)
            .or_else(|| {
                self.inner
                    .delegation_events
                    .get(handle.invocation_id)
                    .and_then(|entry| entry.child_run_id)
            })
            .ok_or_else(|| EngineError::MissingTool("subagent has not started a run".into()))?;
        drop(admission_guard);
        let child = self.inner.store.get(child_session_id)?;
        if matches!(child.status, SessionStatus::Running | SessionStatus::Idle) {
            self.cancel_run_durably(child_run_id, reason)?;
        }
        loop {
            let child = self.inner.store.get(child_session_id)?;
            if !matches!(child.status, SessionStatus::Running | SessionStatus::Idle) {
                return Ok(cancelled_delegate_result(
                    child_session_id,
                    Some(delegate_final_text(&child, Some(child_run_id)).to_owned()),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[cfg(test)]
    pub(crate) fn delegation_queue_contains(
        &self,
        child_session_id: SessionId,
    ) -> Result<bool, EngineError> {
        Ok(self
            .inner
            .delegation_queue
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .contains(&child_session_id))
    }

    #[cfg(test)]
    pub(crate) fn delegation_registry_snapshot(
        &self,
        child_session_id: SessionId,
    ) -> Result<(InvocationId, Option<SessionStatus>, bool), EngineError> {
        self.inner
            .delegations_by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .get(&child_session_id)
            .map(|record| {
                (
                    record.invocation_id,
                    match record.state {
                        DelegationState::Finished(status) => Some(status),
                        _ => None,
                    },
                    record.counts_slot,
                )
            })
            .ok_or_else(|| EngineError::MissingTool("subagent registry entry is missing".into()))
    }

    #[cfg(test)]
    pub(crate) fn install_resume_admission_hook(
        &self,
    ) -> (tokio::sync::oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .resume_admission_hook
            .lock()
            .expect("resume admission hook lock poisoned") =
            Some(Arc::new(super::ResumeAdmissionHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: Arc::clone(&release),
            }));
        (receiver, release)
    }

    #[cfg(test)]
    pub(crate) fn install_skill_fork_reservation_hook(
        &self,
    ) -> (oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        let (reached, receiver) = oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .skill_fork_reservation_hook
            .lock()
            .expect("skill fork reservation hook lock poisoned") =
            Some(Arc::new(super::PagingRaceHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: Arc::clone(&release),
            }));
        (receiver, release)
    }

    #[cfg(test)]
    pub(crate) fn install_delegation_reservation_hook(
        &self,
    ) -> (oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        let (reached, receiver) = oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .delegation_reservation_hook
            .lock()
            .expect("delegation reservation hook lock poisoned") =
            Some(Arc::new(super::PagingRaceHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: Arc::clone(&release),
            }));
        (receiver, release)
    }

    #[cfg(test)]
    pub(crate) fn install_resume_attachment_hook(
        &self,
    ) -> (tokio::sync::oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        self.install_resume_test_hook(&self.inner.resume_attachment_hook)
    }

    #[cfg(test)]
    pub(crate) fn install_resume_rollback_hook(
        &self,
    ) -> (tokio::sync::oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        self.install_resume_test_hook(&self.inner.resume_rollback_hook)
    }

    #[cfg(test)]
    fn install_resume_test_hook(
        &self,
        slot: &std::sync::Mutex<Option<Arc<super::ResumeAdmissionHook>>>,
    ) -> (tokio::sync::oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        *slot.lock().expect("resume test hook lock poisoned") =
            Some(Arc::new(super::ResumeAdmissionHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: Arc::clone(&release),
            }));
        (receiver, release)
    }

    #[cfg(test)]
    async fn wait_for_resume_test_hook(
        &self,
        slot: &std::sync::Mutex<Option<Arc<super::ResumeAdmissionHook>>>,
    ) {
        let hook = slot.lock().expect("resume test hook lock poisoned").clone();
        if let Some(hook) = hook {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("resume test hook reached lock poisoned")
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_delegation_slot_ownership(
        &self,
        child_session_id: SessionId,
        background: bool,
        counts_slot: bool,
    ) -> Result<(), EngineError> {
        let mut records = self
            .inner
            .delegations_by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let record = records
            .get_mut(&child_session_id)
            .ok_or_else(|| EngineError::MissingTool("subagent registry entry is missing".into()))?;
        record.background = background;
        record.counts_slot = counts_slot;
        Ok(())
    }

    pub(super) fn rebuild_delegation_registry(
        &self,
        entries: &[delegation_events::DelegationEntry],
    ) -> Result<(), EngineError> {
        let mut queued_roots = Vec::new();
        for entry in entries {
            let Ok(child) = self.inner.store.get(entry.reservation.child_session_id) else {
                continue;
            };
            let parent = self.inner.store.get(entry.reservation.parent_session_id)?;
            let background = parent.log.events().iter().any(|event| {
                matches!(
                    &event.payload,
                    Event::ToolCallTerminated { termination }
                        if termination.tool_call_id == entry.reservation.parent_tool_call_id
                            && termination.result.as_ref().is_some_and(|result| {
                                result.metadata.get("session_id")
                                    == Some(&serde_json::json!(child.meta.session_id))
                            })
                )
            });
            let notification_sent = parent.log.events().iter().any(|event| {
                matches!(
                    event.payload,
                    Event::DelegateFinishedV2 {
                        invocation_id,
                        session_id,
                        ..
                    } if invocation_id == entry.reservation.invocation_id
                        && session_id == child.meta.session_id
                )
            });
            let exact_status = entry
                .terminal_status
                .or_else(|| {
                    entry
                        .child_run_id
                        .and_then(|run_id| child.runs.get(&run_id).map(|run| run.status))
                })
                .or_else(|| {
                    (entry.request.resume_session_id.is_none()
                        && matches!(
                            child.status,
                            SessionStatus::Failed | SessionStatus::Cancelled
                        ))
                    .then_some(child.status)
                });
            if background
                && !notification_sent
                && let Some(status) = exact_status
                && matches!(
                    status,
                    SessionStatus::Completed
                        | SessionStatus::Failed
                        | SessionStatus::Interrupted
                        | SessionStatus::Cancelled
                )
            {
                let teaser = if entry.terminal_status.is_some() && entry.child_run_id.is_none() {
                    DelegateTeaser {
                        session_id: child.meta.session_id,
                        status,
                        preview: String::new(),
                        total_lines: 0,
                    }
                } else {
                    delegate_teaser(&child, child.meta.session_id, status, entry.child_run_id)
                };
                self.append_blocking(
                    entry.reservation.parent_session_id,
                    Some(entry.reservation.parent_run_id),
                    Event::DelegateFinishedV2 {
                        invocation_id: entry.reservation.invocation_id,
                        session_id: child.meta.session_id,
                        status,
                        preview: teaser.preview,
                        total_lines: teaser.total_lines,
                    },
                )?;
            }
        }
        let mut latest = std::collections::HashMap::new();
        for entry in entries {
            latest.insert(entry.reservation.child_session_id, entry);
        }
        for entry in latest.into_values() {
            let child = match self.inner.store.get(entry.reservation.child_session_id) {
                Ok(child) => child,
                Err(_) => continue,
            };
            let parent = self.inner.store.get(entry.reservation.parent_session_id)?;
            let root_session_id = match parent.meta.origin {
                SessionOrigin::Delegated {
                    root_session_id, ..
                } => root_session_id,
                _ => parent.meta.session_id,
            };
            let queued_event = parent.log.events().iter().any(|event| {
                matches!(
                    event.payload,
                    Event::DelegateQueued { session_id, .. }
                        if event.run_id == Some(entry.reservation.parent_run_id)
                            && session_id == child.meta.session_id
                )
            });
            let notification_sent = parent.log.events().iter().any(|event| {
                matches!(
                    event.payload,
                    Event::DelegateFinishedV2 {
                        invocation_id,
                        session_id,
                        ..
                    } if invocation_id == entry.reservation.invocation_id
                        && session_id == child.meta.session_id
                )
            });
            let background_return = parent.log.events().iter().any(|event| {
                matches!(
                    &event.payload,
                    Event::ToolCallTerminated { termination }
                        if termination.tool_call_id == entry.reservation.parent_tool_call_id
                            && termination.result.as_ref().is_some_and(|result| {
                                result.metadata.get("session_id")
                                    == Some(&serde_json::json!(child.meta.session_id))
                            })
                )
            });
            let background = queued_event || background_return;
            let exact_run_status = entry
                .child_run_id
                .and_then(|run_id| child.runs.get(&run_id).map(|run| run.status));
            let state = if let Some(status) = entry.terminal_status {
                DelegationState::Finished(status)
            } else if entry.child_run_id.is_none() && queued_event {
                DelegationState::Queued
            } else if exact_run_status.is_some_and(|status| {
                matches!(
                    status,
                    SessionStatus::Completed
                        | SessionStatus::Failed
                        | SessionStatus::Interrupted
                        | SessionStatus::Cancelled
                )
            }) {
                DelegationState::Finished(exact_run_status.expect("checked terminal status"))
            } else {
                DelegationState::Running
            };
            let counts_slot =
                background && !matches!(parent.meta.origin, SessionOrigin::Delegated { .. });
            self.inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?
                .insert(
                    child.meta.session_id,
                    DelegationRecord {
                        parent_session_id: entry.reservation.parent_session_id,
                        parent_run_id: entry.reservation.parent_run_id,
                        parent_tool_call_id: entry.reservation.parent_tool_call_id,
                        invocation_id: entry.reservation.invocation_id,
                        root_session_id,
                        child_run_id: entry.child_run_id,
                        state,
                        background,
                        counts_slot,
                        notification_sent,
                    },
                );
            if state == DelegationState::Queued {
                self.inner
                    .delegation_queue
                    .lock()
                    .map_err(|_| EngineError::ActorStopped)?
                    .push_back(child.meta.session_id);
                queued_roots.push(root_session_id);
            } else if background
                && let DelegationState::Finished(status) = state
                && !notification_sent
            {
                let teaser = if entry.terminal_status.is_some() && entry.child_run_id.is_none() {
                    DelegateTeaser {
                        session_id: child.meta.session_id,
                        status,
                        preview: String::new(),
                        total_lines: 0,
                    }
                } else {
                    delegate_teaser(&child, child.meta.session_id, status, entry.child_run_id)
                };
                self.append_blocking(
                    entry.reservation.parent_session_id,
                    Some(entry.reservation.parent_run_id),
                    Event::DelegateFinishedV2 {
                        invocation_id: entry.reservation.invocation_id,
                        session_id: child.meta.session_id,
                        status,
                        preview: teaser.preview,
                        total_lines: teaser.total_lines,
                    },
                )?;
            }
        }
        queued_roots.sort_unstable();
        queued_roots.dedup();
        if let Some(runtime) = self.inner.runtime.clone() {
            for root_session_id in queued_roots {
                let engine = self.clone();
                runtime.spawn(async move {
                    if let Err(error) = engine.start_queued_delegation(root_session_id).await {
                        eprintln!("queued delegate recovery failed: {error}");
                    }
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DelegationState {
    Queued,
    Starting,
    Running,
    Finished(SessionStatus),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DelegationRecord {
    pub(super) parent_session_id: SessionId,
    pub(super) parent_run_id: RunId,
    pub(super) parent_tool_call_id: ToolCallId,
    pub(super) invocation_id: InvocationId,
    pub(super) root_session_id: SessionId,
    pub(super) child_run_id: Option<RunId>,
    pub(super) state: DelegationState,
    pub(super) background: bool,
    pub(super) counts_slot: bool,
    pub(super) notification_sent: bool,
}

pub(crate) fn freeze_delegated_child_policy(
    child_agent: &crate::runtime_snapshot::ResolvedAgent,
    parent_policy: &FrozenRunPolicy,
    child_selection: &cookie_agent_protocol::ModelSelection,
    inherited_suffix: &[cookie_agent_protocol::FrozenModelBinding],
    inherited_depth_ceiling: u32,
    result_limits: policy::ResultLimits,
) -> Result<FrozenRunPolicy, EngineError> {
    freeze_delegated_agent_policy(
        child_agent,
        Arc::clone(&parent_policy.registry),
        Arc::clone(&parent_policy.runtime),
        child_selection,
        inherited_suffix,
        inherited_depth_ceiling,
        policy::FreezeOptions {
            result_limits,
            prompt_cache_strategy: parent_policy.prompt_cache_strategy.clone(),
        },
    )
}

pub(super) fn delegate_client_run_id(
    invocation_id: InvocationId,
) -> cookie_agent_protocol::ClientRunId {
    cookie_agent_protocol::ClientRunId::new(format!("delegate:{invocation_id}"))
        .expect("bounded delegate client run id")
}

pub(super) fn render_delegate_input(request: &DelegateRequestPayload) -> String {
    request.prompt.clone()
}

const DELEGATED_CONTEXT_MAX_BYTES: usize = 64 * 1024;

fn context_seed_from_history(history: Vec<oven_sdk::HistoryTurn>) -> Vec<DelegatedContextTurn> {
    let mut turns = history
        .into_iter()
        .filter_map(|turn| match turn {
            oven_sdk::HistoryTurn::User(message) => {
                let text = message
                    .content
                    .into_iter()
                    .filter_map(|part| match part {
                        oven_sdk::InputPart::Text(text) => Some(text.text),
                        _ => None,
                    })
                    .collect::<String>();
                (!text.is_empty()).then_some(DelegatedContextTurn {
                    role: DelegatedContextRole::User,
                    text,
                })
            }
            oven_sdk::HistoryTurn::Assistant(turn) => {
                let text = turn
                    .message
                    .content
                    .into_iter()
                    .filter_map(|part| match part {
                        oven_sdk::AssistantPart::Text(text) => Some(text.text),
                        _ => None,
                    })
                    .collect::<String>();
                (!text.is_empty()).then_some(DelegatedContextTurn {
                    role: DelegatedContextRole::Assistant,
                    text,
                })
            }
            oven_sdk::HistoryTurn::System(_) | oven_sdk::HistoryTurn::Tool(_) => None,
        })
        .collect::<Vec<_>>();
    let mut excess = turns
        .iter()
        .map(|turn| turn.text.len())
        .sum::<usize>()
        .saturating_sub(DELEGATED_CONTEXT_MAX_BYTES);
    while excess > 0 && !turns.is_empty() {
        if turns[0].text.len() <= excess {
            excess -= turns.remove(0).text.len();
            continue;
        }
        let mut boundary = excess;
        while !turns[0].text.is_char_boundary(boundary) {
            boundary += 1;
        }
        turns[0].text.drain(..boundary);
        excess = 0;
    }
    turns
}

struct DelegateTeaser {
    session_id: SessionId,
    status: SessionStatus,
    preview: String,
    total_lines: u64,
}

fn delegate_teaser(
    child: &session::SessionProjection,
    child_session_id: SessionId,
    status: SessionStatus,
    child_run_id: Option<RunId>,
) -> DelegateTeaser {
    let text = delegate_final_text(child, child_run_id);
    DelegateTeaser {
        session_id: child_session_id,
        status,
        preview: preview_text(text),
        total_lines: text.lines().count() as u64,
    }
}

fn delegate_final_text(child: &session::SessionProjection, child_run_id: Option<RunId>) -> &str {
    child_run_id
        .and_then(|run_id| child.runs.get(&run_id))
        .and_then(|run| run.final_text.as_deref())
        .unwrap_or("")
}

fn preview_text(text: &str) -> String {
    let first_lines = text.lines().take(20).collect::<Vec<_>>().join("\n");
    if first_lines.len() <= 2048 {
        return first_lines;
    }
    let mut end = 2048;
    while !first_lines.is_char_boundary(end) {
        end -= 1;
    }
    first_lines[..end].to_owned()
}

const fn session_status_name(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Idle => "running",
        SessionStatus::Running => "running",
        SessionStatus::Completed => "completed",
        SessionStatus::Failed => "failed",
        SessionStatus::Cancelled => "cancelled",
        SessionStatus::Interrupted => "interrupted",
    }
}

const fn background_queue_limit_reached(limit: u32, queued: u32) -> bool {
    queued >= limit.saturating_mul(4)
}

fn paginated_subagent_result(status: &str, text: &str, offset: u32, limit: u32) -> ToolResult {
    let lines = text.lines().collect::<Vec<_>>();
    let mut output = format!("<status>{status}</status>\n<content>\n");
    for (index, line) in lines
        .iter()
        .enumerate()
        .skip(offset as usize)
        .take(limit as usize)
    {
        output.push_str(&format!("{}: {line}\n", index + 1));
    }
    output.push_str("</content>");
    ToolResult {
        title: safe_display("Read subagent result"),
        output,
        metadata: serde_json::json!({
            "offset": offset,
            "limit": limit,
            "total_lines": lines.len(),
        }),
        truncation: None,
        attachments: Vec::new(),
    }
}

fn steered_delegate_result(child_session_id: SessionId, status: &str) -> ToolResult {
    structured_delegate_result(
        "Subagent steered",
        serde_json::json!({
            "session_id": child_session_id,
            "status": status,
        }),
    )
}

pub(crate) fn completed_delegate_result(
    child: &session::SessionProjection,
    child_run_id: Option<RunId>,
) -> ToolResult {
    terminal_delegate_result(child, child_run_id, SessionStatus::Completed)
}

fn terminal_delegate_result(
    child: &session::SessionProjection,
    child_run_id: Option<RunId>,
    status: SessionStatus,
) -> ToolResult {
    let teaser = delegate_teaser(child, child.meta.session_id, status, child_run_id);
    let metadata = serde_json::json!({
        "session_id": teaser.session_id,
        "status": session_status_name(teaser.status),
        "preview": teaser.preview,
        "total_lines": teaser.total_lines,
    });
    structured_delegate_result("Subagent finished", metadata)
}

pub(super) fn structured_delegate_result(title: &str, metadata: Value) -> ToolResult {
    ToolResult {
        title: safe_display(title),
        output: metadata.to_string(),
        metadata,
        truncation: None,
        attachments: Vec::new(),
    }
}

pub(super) fn cancelled_delegate_result(
    child_session_id: SessionId,
    partial_report: Option<String>,
) -> ToolResult {
    structured_delegate_result(
        "Delegate cancelled",
        serde_json::json!({
            "status": "cancelled",
            "child_session_id": child_session_id,
            "partial_report": partial_report,
        }),
    )
}

pub(super) fn cancelled_delegate_result_with_reason(
    child_session_id: Option<SessionId>,
    reason: &str,
) -> ToolResult {
    structured_delegate_result(
        "Delegate cancelled",
        serde_json::json!({
            "status": "cancelled",
            "child_session_id": child_session_id,
            "reason": reason,
        }),
    )
}

pub(super) fn delegate_failure_result(
    child_session_id: Option<SessionId>,
    reason: &str,
) -> ToolResult {
    structured_delegate_result(
        "Delegate failed",
        serde_json::json!({
            "status": "failed",
            "child_session_id": child_session_id,
            "reason": reason,
        }),
    )
}

fn validate_redelivery_mode(
    durable_background: bool,
    redelivery_background: bool,
) -> Result<(), EngineError> {
    if durable_background != redelivery_background {
        let durable = if durable_background {
            "background"
        } else {
            "foreground"
        };
        let redelivery = if redelivery_background {
            "background"
        } else {
            "foreground"
        };
        return Err(EngineError::MissingTool(format!(
            "delegate redelivery execution mode conflict: durable invocation is {durable}, redelivery requested {redelivery}"
        )));
    }
    Ok(())
}

pub(super) fn is_delegation_event_append_failure(error: &EngineError) -> bool {
    matches!(
        error,
        EngineError::DelegationEvents(DelegationEventError::Session(
            crate::session::SessionError::Event(_)
        )) | EngineError::ActorStopped
    )
}

#[cfg(test)]
mod concurrency_tests {
    use super::{
        DELEGATED_CONTEXT_MAX_BYTES, background_queue_limit_reached, context_seed_from_history,
        paginated_subagent_result, preview_text, validate_redelivery_mode,
    };

    #[test]
    fn background_queue_rejects_only_at_four_times_concurrency() {
        assert!(!background_queue_limit_reached(4, 15));
        assert!(background_queue_limit_reached(4, 16));
        assert!(background_queue_limit_reached(4, 17));
    }

    #[test]
    fn redelivery_execution_mode_must_match_in_both_directions() {
        assert!(validate_redelivery_mode(false, false).is_ok());
        assert!(validate_redelivery_mode(true, true).is_ok());
        let foreground_to_background = validate_redelivery_mode(false, true)
            .expect_err("foreground to background must fail")
            .to_string();
        assert!(foreground_to_background.contains("durable invocation is foreground"));
        assert!(foreground_to_background.contains("requested background"));
        let background_to_foreground = validate_redelivery_mode(true, false)
            .expect_err("background to foreground must fail")
            .to_string();
        assert!(background_to_foreground.contains("durable invocation is background"));
        assert!(background_to_foreground.contains("requested foreground"));
    }

    #[test]
    fn preview_is_line_and_utf8_bounded() {
        let text = (0..30)
            .map(|index| format!("{index}: {}", "é".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = preview_text(&text);
        assert!(preview.lines().count() <= 20);
        assert!(preview.len() <= 2048);
        assert!(std::str::from_utf8(preview.as_bytes()).is_ok());
    }

    #[test]
    fn result_pages_match_read_line_numbering_and_empty_running_shape() {
        let page = paginated_subagent_result("completed", "one\ntwo\nthree", 1, 1);
        assert_eq!(
            page.output,
            "<status>completed</status>\n<content>\n2: two\n</content>"
        );
        assert_eq!(page.metadata["total_lines"], 3);

        let running = paginated_subagent_result("running", "", 0, 20);
        assert_eq!(
            running.output,
            "<status>running</status>\n<content>\n</content>"
        );
        assert_eq!(running.metadata["total_lines"], 0);
    }

    #[test]
    fn inherited_context_is_text_only_and_truncates_oldest_bytes_first() {
        let history = vec![
            oven_sdk::HistoryTurn::system(oven_sdk::SystemMessage::new(vec![
                oven_sdk::SystemPart::Text(oven_sdk::TextPart::new("private system prompt")),
            ])),
            oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(vec![
                oven_sdk::InputPart::Text(oven_sdk::TextPart::new("old-user".repeat(125))),
            ])),
            oven_sdk::HistoryTurn::tool(oven_sdk::ToolMessage::new(vec![
                oven_sdk::ToolResultPart::new(
                    "call",
                    oven_sdk::ToolContent::Text("secret tool output".into()),
                ),
            ])),
            oven_sdk::HistoryTurn::assistant(oven_sdk::CompletedTurn::new(
                oven_sdk::AssistantMessage::new(vec![
                    oven_sdk::AssistantPart::Text(oven_sdk::TextPart::new("n".repeat(70_000))),
                    oven_sdk::AssistantPart::ToolResult(oven_sdk::ToolResultPart::new(
                        "hosted",
                        oven_sdk::ToolContent::Text("hosted output".into()),
                    )),
                ]),
                oven_sdk::Finish::new(oven_sdk::Usage::default(), oven_sdk::FinishReason::Stop),
            )),
        ];
        let seed = context_seed_from_history(history);
        assert_eq!(seed.len(), 1);
        assert_eq!(
            seed[0].role,
            cookie_agent_protocol::DelegatedContextRole::Assistant
        );
        assert_eq!(seed[0].text.len(), DELEGATED_CONTEXT_MAX_BYTES);
        assert!(!seed[0].text.contains("old-user"));
        assert!(!seed[0].text.contains("tool output"));
    }
}
