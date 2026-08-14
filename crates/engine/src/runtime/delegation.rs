use std::sync::{Arc, atomic::Ordering};

use cookie_agent_protocol::{
    InvocationId, PersistedToolResult as ToolResult, RunId, RunStartParams, SafeToolError,
    SessionId, SessionOrigin, SessionStatus, Sha256Digest, ToolCallId, ToolCallTermination,
    ToolTerminationOutcome,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{
    Engine, EngineError, Event, SessionCommand,
    admission::AdmissionGuard,
    helpers::{invocation_id, safe_code, safe_display, safe_error, session_depth},
};
use crate::{
    delegation_api::{DelegateAwait, DelegateHandle, DelegateInvocation},
    journal::{self, JournalError},
    policy::{self, FrozenRunPolicy, freeze_delegated_agent_policy, resolve_agent},
    session,
};

impl Engine {
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
        let parent = self.inner.store.get(invocation.parent_session_id)?;
        if parent
            .runs
            .get(&invocation.parent_run_id)
            .is_some_and(|run| run.status == SessionStatus::Interrupted)
            && self.journal_get(invocation_id).await?.is_none()
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
        let fingerprint_payload = serde_json::to_vec(&(
            &invocation.agent_type,
            &invocation.description,
            &invocation.prompt,
            &child_policy.agent,
            &child_policy.selected_suffix,
        ))
        .map_err(|_| EngineError::RuntimeCompileFailed)?;
        let fingerprint = Sha256Digest::new(format!("{:x}", Sha256::digest(&fingerprint_payload)))
            .map_err(|_| EngineError::RuntimeCompileFailed)?;
        let title = super::titles::delegated_title(
            &invocation.description,
            self.inner.config.runtime.session_title.max_chars,
        )?;
        let request = journal::DelegateRequestPayload {
            description: invocation.description,
            prompt: invocation.prompt,
            title,
        };

        let root_session_id = match parent.meta.origin {
            SessionOrigin::Delegated {
                root_session_id, ..
            } => root_session_id,
            _ => parent.meta.session_id,
        };
        let counts_slot =
            invocation.background && !matches!(parent.meta.origin, SessionOrigin::Delegated { .. });
        let queued = counts_slot && self.background_slot_unavailable(root_session_id)?;
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
                if is_journal_append_failure(&error) {
                    let result = delegate_failure_result(None, "delegate journal append failed");
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
        self.publish_admission_child(invocation_id, generation, child.session_id)?;
        let entry = self
            .journal_get(invocation_id)
            .await?
            .ok_or_else(|| EngineError::MissingTool("delegate reservation disappeared".into()))?;
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
                    self.finish_background_or_retry(child.session_id).await;
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
                if is_journal_append_failure(&error) {
                    let result = delegate_failure_result(
                        Some(entry.reservation.child_session_id),
                        "delegate journal run confirmation failed",
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
                    record.state = DelegationState::Finished(
                        terminalized
                            .as_ref()
                            .ok()
                            .and_then(|_| self.inner.store.get(child.session_id).ok())
                            .map_or(SessionStatus::Failed, |child| child.status),
                    );
                }
                drop(admission_guard);
                if invocation.background {
                    if terminalized.is_ok() {
                        self.finish_background_or_retry(child.session_id).await;
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
            self.finish_background_or_retry(child.session_id).await;
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
            match child.status {
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
                SessionStatus::Completed => {
                    let result = terminal_delegate_result(
                        &child,
                        handle.child_run_id,
                        SessionStatus::Completed,
                    );
                    self.clear_delegate_admissions(handle.invocation_id);
                    return Ok(result);
                }
                SessionStatus::Cancelled => {
                    let result = terminal_delegate_result(
                        &child,
                        handle.child_run_id,
                        SessionStatus::Cancelled,
                    );
                    self.clear_delegate_admissions(handle.invocation_id);
                    return Ok(result);
                }
                SessionStatus::Failed | SessionStatus::Interrupted => {
                    let result =
                        terminal_delegate_result(&child, handle.child_run_id, child.status);
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

    pub(super) async fn journal_get(
        &self,
        invocation_id: InvocationId,
    ) -> Result<Option<journal::JournalEntry>, EngineError> {
        let journal = self.inner.journal.clone();
        self.spawn_admission_blocking(move || Ok::<_, EngineError>(journal.get(invocation_id)))
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
        entry: &journal::JournalEntry,
        admission: Option<(InvocationId, u64)>,
    ) -> Result<RunId, EngineError> {
        #[cfg(test)]
        if self
            .inner
            .delegate_start_failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                (remaining > 0).then(|| remaining - 1)
            })
            .is_ok()
        {
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
        let journal = self.inner.journal.clone();
        let invocation_id = entry.reservation.invocation_id;
        let confirmation = self
            .spawn_admission_blocking(move || journal.mark_run_started(invocation_id, run_id))
            .await;
        if let Err(error) = confirmation {
            // A failed confirmation may have poisoned the sole journal writer.
            // The child already has an active run, so terminally cancel it before
            // the caller resolves the parent through its actor.
            let _ = self.cancel_run_durably(
                run_id,
                Some("delegate journal run confirmation failed".into()),
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
                    .finish_background_delegate(handle.child_session_id)
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

    async fn finish_background_or_retry(&self, child_session_id: SessionId) {
        if self
            .finish_background_delegate(child_session_id)
            .await
            .is_err()
        {
            let _ = self.spawn_background_finish_retry(child_session_id);
        }
    }

    fn spawn_background_finish_retry(
        &self,
        child_session_id: SessionId,
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
                match retry.finish_background_delegate(child_session_id).await {
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
    ) -> Result<(), EngineError> {
        let child = self.inner.store.get(child_session_id)?;
        let (parent_session_id, parent_run_id, root_session_id, already_sent) = {
            let mut records = self
                .inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?;
            let record = records.get_mut(&child_session_id).ok_or_else(|| {
                EngineError::MissingTool("subagent registry entry is missing".into())
            })?;
            if !record.background {
                return Err(EngineError::MissingTool(
                    "foreground subagent cannot emit a background notification".into(),
                ));
            }
            let already_sent = record.notification_sent;
            record.state = DelegationState::Finished(child.status);
            (
                record.parent_session_id,
                record.parent_run_id,
                record.root_session_id,
                already_sent,
            )
        };
        let already_logged = !already_sent
            && self
                .inner
                .store
                .get(parent_session_id)?
                .log
                .events()
                .iter()
                .any(|event| {
                    matches!(
                        event.payload,
                        Event::DelegateFinished { session_id, .. }
                            if session_id == child_session_id
                    )
                });
        if already_logged
            && let Some(record) = self
                .inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?
                .get_mut(&child_session_id)
        {
            record.notification_sent = true;
        }
        let notification_result = if !already_sent && !already_logged {
            let child_run_id = self
                .inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?
                .get(&child_session_id)
                .and_then(|record| record.child_run_id);
            let teaser = delegate_teaser(&child, child_session_id, child.status, child_run_id);
            let result = self
                .append(
                    parent_session_id,
                    Some(parent_run_id),
                    Event::DelegateFinished {
                        session_id: child_session_id,
                        status: child.status,
                        preview: teaser.preview,
                        total_lines: teaser.total_lines,
                    },
                )
                .await;
            if result.is_ok()
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
        let drain_result = self.start_queued_delegation(root_session_id).await;
        notification_result.and(drain_result)
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
        let entry = self.journal_get(invocation_id).await?.ok_or_else(|| {
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
            let _ = self.spawn_background_finish_retry(child_session_id);
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
            let _ = self.spawn_background_finish_retry(child_session_id);
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
        self.ensure_parent_link(
            caller_session_id,
            parent_run_id,
            parent_tool_call_id,
            child_session_id,
        )
        .await?;
        let registry_record = self
            .inner
            .delegations_by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .get(&child_session_id)
            .copied();
        if let Some(record) = registry_record
            && (record.parent_session_id != caller_session_id
                || record.parent_run_id != parent_run_id
                || record.parent_tool_call_id != parent_tool_call_id)
        {
            return Err(EngineError::MissingTool(
                "subagent registry ownership does not match its parent link".into(),
            ));
        }
        let child_run_id = registry_record
            .and_then(|record| record.child_run_id)
            .or_else(|| {
                self.inner
                    .journal
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
            let queued = self
                .inner
                .delegations_by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?
                .get(&child_session_id)
                .is_some_and(|record| record.state == DelegationState::Queued);
            let terminal = matches!(
                child.status,
                SessionStatus::Completed
                    | SessionStatus::Failed
                    | SessionStatus::Interrupted
                    | SessionStatus::Cancelled
            );
            if terminal || !wait {
                let status = if queued {
                    "queued"
                } else {
                    session_status_name(child.status)
                };
                let text = if terminal {
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
            self.finish_background_or_retry(child_session_id).await;
            return Ok(cancelled_delegate_result(child_session_id, None));
        }
        let child_run_id = record
            .child_run_id
            .or(handle.child_run_id)
            .or_else(|| {
                self.inner
                    .journal
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

    pub(super) fn rebuild_delegation_registry(
        &self,
        entries: &[journal::JournalEntry],
    ) -> Result<(), EngineError> {
        let mut queued_roots = Vec::new();
        for entry in entries {
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
                        if session_id == child.meta.session_id
                )
            });
            let notification_sent = parent.log.events().iter().any(|event| {
                matches!(
                    event.payload,
                    Event::DelegateFinished { session_id, .. }
                        if session_id == child.meta.session_id
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
            let terminal = matches!(
                child.status,
                SessionStatus::Completed
                    | SessionStatus::Failed
                    | SessionStatus::Interrupted
                    | SessionStatus::Cancelled
            );
            let state = if terminal {
                DelegationState::Finished(child.status)
            } else if entry.child_run_id.is_none() && queued_event {
                DelegationState::Queued
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
            } else if background && terminal && !notification_sent {
                let teaser = delegate_teaser(
                    &child,
                    child.meta.session_id,
                    child.status,
                    entry.child_run_id,
                );
                self.append_blocking(
                    entry.reservation.parent_session_id,
                    Some(entry.reservation.parent_run_id),
                    Event::DelegateFinished {
                        session_id: child.meta.session_id,
                        status: child.status,
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

#[derive(Clone, Copy, Debug)]
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
        result_limits,
    )
}

pub(super) fn delegate_client_run_id(
    invocation_id: InvocationId,
) -> cookie_agent_protocol::ClientRunId {
    cookie_agent_protocol::ClientRunId::new(format!("delegate:{invocation_id}"))
        .expect("bounded delegate client run id")
}

pub(super) fn render_delegate_input(request: &journal::DelegateRequestPayload) -> String {
    request.prompt.clone()
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

pub(super) fn is_journal_append_failure(error: &EngineError) -> bool {
    matches!(
        error,
        EngineError::Journal(
            JournalError::Event(_) | JournalError::Poisoned | JournalError::Stopped
        ) | EngineError::ActorStopped
    )
}

#[cfg(test)]
mod concurrency_tests {
    use super::{background_queue_limit_reached, paginated_subagent_result, preview_text};

    #[test]
    fn background_queue_rejects_only_at_four_times_concurrency() {
        assert!(!background_queue_limit_reached(4, 15));
        assert!(background_queue_limit_reached(4, 16));
        assert!(background_queue_limit_reached(4, 17));
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
}
