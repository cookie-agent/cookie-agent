use std::{
    collections::HashSet,
    sync::{Arc, atomic::Ordering},
};

use cookie_agent_protocol::{
    InvocationId, PersistedToolResult as ToolResult, RunId, RunStartParams, SafeToolError,
    SessionId, SessionOrigin, SessionStatus, Sha256Digest, ToolCallId, ToolCallTermination,
    ToolTerminationOutcome,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use super::{
    Engine, EngineError, Event, SessionCommand,
    admission::AdmissionGuard,
    artifacts::ArtifactStore,
    helpers::{invocation_id, safe_code, safe_display, safe_error, session_depth},
    tool_execution::bound_tool_result,
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
            .is_some_and(|tool| tool == "delegate")
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
        let _admission = self.inner.delegation_admission.lock().await;
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
        enforce_delegation_concurrency(self, &parent)?;
        if !parent_delegation.targets.contains(&invocation.agent)
            || session_depth(&parent.meta.origin) >= parent_delegation.effective_depth_ceiling
        {
            return Err(EngineError::MissingTool(
                "delegate target or depth is not allowed".into(),
            ));
        }
        let child_agent = resolve_agent(&parent_policy.registry, &invocation.agent)?;
        if !child_agent.document.frontmatter.enabled
            || !matches!(
                child_agent.document.frontmatter.mode,
                cookie_agent_config::AgentMode::Subagent | cookie_agent_config::AgentMode::All
            )
        {
            return Err(EngineError::DisabledAgent(invocation.agent));
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
            &invocation.agent,
            &invocation.task,
            &invocation.context,
            &invocation.success_criteria,
            &invocation.expected_output,
            &child_policy.agent,
            &child_policy.selected_suffix,
        ))
        .map_err(|_| EngineError::RuntimeCompileFailed)?;
        let fingerprint = Sha256Digest::new(format!("{:x}", Sha256::digest(&fingerprint_payload)))
            .map_err(|_| EngineError::RuntimeCompileFailed)?;
        let request = journal::DelegateRequestPayload {
            task: invocation.task,
            context: invocation.context,
            success_criteria: invocation.success_criteria,
            expected_output: invocation.expected_output,
        };
        let child = match self
            .create_child(
                invocation.parent_session_id,
                invocation.parent_run_id,
                invocation.parent_tool_call_id,
                &invocation.agent,
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
                    self.resolve_delegate_failure_if_pending(
                        entry.reservation.parent_session_id,
                        entry.reservation.parent_run_id,
                        entry.reservation.parent_tool_call_id,
                        result,
                    )
                    .await?;
                }
                return Err(error);
            }
        };
        Ok(DelegateHandle {
            invocation_id,
            child_session_id: child.session_id,
            child_run_id,
        })
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
                            .get(&handle.child_run_id)
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
                    let result = completed_delegate_result(
                        &child,
                        Some(handle.child_run_id),
                        &self.inner.artifacts,
                    )?;
                    self.clear_delegate_admissions(handle.invocation_id);
                    return Ok(result);
                }
                SessionStatus::Cancelled => {
                    let result = cancelled_delegate_result(handle.child_session_id, None);
                    self.clear_delegate_admissions(handle.invocation_id);
                    return Ok(result);
                }
                SessionStatus::Failed | SessionStatus::Interrupted => {
                    let result = delegate_failure_result(
                        Some(handle.child_session_id),
                        child
                            .runs
                            .get(&handle.child_run_id)
                            .and_then(|run| run.final_text.as_deref())
                            .unwrap_or("child run failed or was interrupted"),
                    );
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
        let _ = self.cancel_run(handle.child_run_id).await;
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
                    .is_some_and(|tool| tool == "delegate")
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

pub(super) fn enforce_delegation_concurrency(
    engine: &Engine,
    parent: &session::SessionProjection,
) -> Result<(), EngineError> {
    let Some(limit) = engine.inner.config.runtime.delegation.max_concurrency else {
        return Ok(());
    };
    let root = match parent.meta.origin {
        SessionOrigin::Delegated {
            root_session_id, ..
        } => root_session_id,
        _ => parent.meta.session_id,
    };
    let active_sessions = engine
        .inner
        .active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .map(|active| active.session)
        .collect::<HashSet<_>>();
    let active_delegated = active_sessions
        .into_iter()
        .filter(|session_id| {
            engine.inner.store.get(*session_id).is_ok_and(|session| {
                matches!(
                    session.meta.origin,
                    SessionOrigin::Delegated { root_session_id, .. } if root_session_id == root
                )
            })
        })
        .count() as u32;
    if delegation_concurrency_limit_reached(limit, active_delegated) {
        return Err(EngineError::MissingTool(format!(
            "delegate admission denied: active delegated-session concurrency limit {limit} reached"
        )));
    }
    Ok(())
}

const fn delegation_concurrency_limit_reached(limit: u32, active_delegated: u32) -> bool {
    active_delegated >= limit
}

pub(super) fn delegate_client_run_id(
    invocation_id: InvocationId,
) -> cookie_agent_protocol::ClientRunId {
    cookie_agent_protocol::ClientRunId::new(format!("delegate:{invocation_id}"))
        .expect("bounded delegate client run id")
}

pub(super) fn render_delegate_input(request: &journal::DelegateRequestPayload) -> String {
    // Stable, provider-neutral child prompt rendering retained in the journal.
    // JSON preserves arbitrary structured context and expected-output details.
    format!(
        "Task:\n{}\n\nContext:\n{}\n\nSuccess criteria:\n{}\n\nExpected output:\n{}",
        request.task,
        serde_json::to_string(&request.context).expect("delegate context serializes"),
        serde_json::to_string(&request.success_criteria).expect("success criteria serialize"),
        serde_json::to_string(&request.expected_output).expect("expected output serializes"),
    )
}

pub(crate) fn completed_delegate_result(
    child: &session::SessionProjection,
    child_run_id: Option<RunId>,
    artifacts: &ArtifactStore,
) -> std::io::Result<ToolResult> {
    let output = child_run_id
        .and_then(|child_run_id| child.runs.get(&child_run_id))
        .and_then(|run| run.final_text.clone())
        .unwrap_or_else(|| "child completed without a final report".into());
    bound_tool_result(
        ToolResult {
            title: safe_display("Delegate report"),
            output,
            metadata: serde_json::json!({
                "status": "completed",
                "child_session_id": child.meta.session_id,
            }),
            truncation: None,
            attachments: Vec::new(),
        },
        "delegate",
        ToolCallId::new_v7(),
        artifacts,
        usize::MAX,
        32 * 1024,
    )
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
    use super::delegation_concurrency_limit_reached;

    #[test]
    fn max_concurrency_denies_at_the_configured_active_run_limit() {
        assert!(!delegation_concurrency_limit_reached(2, 1));
        assert!(delegation_concurrency_limit_reached(2, 2));
        assert!(delegation_concurrency_limit_reached(2, 3));
    }
}
