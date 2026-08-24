use std::collections::HashMap;

use cookie_agent_protocol::{
    ApprovalDecisionSource, ApprovalFinalDecision, ApprovalFinalOutcome, ApprovalReasonCode,
    ApprovalStatus, SafeToolError, SessionId, SessionStatus, ToolCallTermination,
    ToolTerminationOutcome,
};

use super::{
    Engine, EngineError, Event, ToolCallFailureCode, ToolFailure,
    approval_projection::{approval_records, approval_run_id},
    delegation::{
        cancelled_delegate_result, cancelled_delegate_result_with_reason,
        completed_delegate_result, delegate_failure_result, is_delegation_event_append_failure,
    },
    helpers::{invocation_id, safe_code, safe_error},
};
use crate::delegation_api::DelegateHandle;

impl Engine {
    pub(super) fn reconcile(&self) -> Result<(), EngineError> {
        // Every active run from a previous process is terminally interrupted.
        for session in self.inner.store.all() {
            let mut internal = HashMap::new();
            for event in session.log.events() {
                match event.payload {
                    Event::InternalAgentStarted {
                        invocation_id,
                        internal_run_id,
                        kind,
                        ..
                    } => {
                        internal.insert((invocation_id, internal_run_id), (kind, event.run_id));
                    }
                    Event::InternalAgentCompleted {
                        invocation_id,
                        internal_run_id,
                        ..
                    }
                    | Event::InternalAgentFailed {
                        invocation_id,
                        internal_run_id,
                        ..
                    }
                    | Event::InternalAgentCancelled {
                        invocation_id,
                        internal_run_id,
                        ..
                    }
                    | Event::InternalAgentInterrupted {
                        invocation_id,
                        internal_run_id,
                        ..
                    } => {
                        internal.remove(&(invocation_id, internal_run_id));
                    }
                    _ => {}
                }
            }
            for ((invocation_id, internal_run_id), (kind, parent_run)) in internal {
                self.append_blocking(
                    session.meta.session_id,
                    parent_run,
                    super::event_origin("engine:recovery"),
                    Event::InternalAgentInterrupted {
                        invocation_id,
                        internal_run_id,
                        kind,
                        reason: Some(safe_error("daemon restart")),
                    },
                )?;
            }
            for run in session
                .runs
                .values()
                .filter(|run| run.status == SessionStatus::Running)
            {
                self.append_blocking(
                    session.meta.session_id,
                    Some(run.id),
                    super::event_origin("engine:recovery"),
                    Event::RunInterrupted {
                        reason: Some(safe_error("daemon restart")),
                    },
                )?;
            }
            for run in session.runs.values() {
                for (tool_call_id, tool) in &run.pending_calls {
                    if tool == "delegate_subagent" {
                        continue;
                    }
                    let failure = restart_tool_failure();
                    self.append_blocking(
                        session.meta.session_id,
                        Some(run.id),
                        super::event_origin("engine:recovery"),
                        Event::ToolCallTerminated {
                            termination: ToolCallTermination {
                                tool_call_id: *tool_call_id,
                                owner: self.tool_call_owner(
                                    session.meta.session_id,
                                    run.id,
                                    *tool_call_id,
                                )?,
                                outcome: ToolTerminationOutcome::Interrupted,
                                result: None,
                                error: Some(SafeToolError {
                                    code: failure.code.safe_code(),
                                    message: safe_error(&failure.message),
                                }),
                            },
                        },
                    )?;
                }
            }
            for record in approval_records(session.meta.session_id, &session.log.events())
                .into_values()
                .filter(|record| {
                    matches!(
                        record.status,
                        ApprovalStatus::Pending | ApprovalStatus::Escalated
                    )
                })
            {
                let approval_run =
                    approval_run_id(&session.log.events(), record.request.approval_id())
                        .ok_or(EngineError::ApprovalConflict)?;
                self.append_blocking(
                    session.meta.session_id,
                    Some(approval_run),
                    super::event_origin("engine:recovery"),
                    Event::ApprovalCancelled {
                        approval_id: record.request.approval_id(),
                        reason_code: ApprovalReasonCode::PreparedCapabilityLost,
                    },
                )?;
                self.append_blocking(
                    session.meta.session_id,
                    Some(approval_run),
                    super::event_origin("engine:recovery"),
                    Event::ApprovalFinalized {
                        approval_id: record.request.approval_id(),
                        decision: ApprovalFinalDecision {
                            outcome: ApprovalFinalOutcome::Cancelled,
                            source: ApprovalDecisionSource::System,
                            reason_code: ApprovalReasonCode::PreparedCapabilityLost,
                            feedback: None,
                            tree_grant_id: None,
                        },
                    },
                )?;
            }
        }
        let mut delegation_entries = self.inner.delegation_events.entries();
        let mut latest_invocations = std::collections::HashMap::new();
        for entry in &delegation_entries {
            latest_invocations.insert(
                entry.reservation.child_session_id,
                entry.reservation.invocation_id,
            );
        }
        for entry in &mut delegation_entries {
            if self
                .inner
                .store
                .get(entry.reservation.child_session_id)
                .is_err()
            {
                if entry.terminal_status.is_none() {
                    let reason = safe_error("child_missing: child session was never created");
                    self.inner.delegation_events.mark_finished_with_reason(
                        entry.reservation.invocation_id,
                        SessionStatus::Failed,
                        Some(reason.clone()),
                    )?;
                    entry.terminal_status = Some(SessionStatus::Failed);
                    entry.terminal_reason = Some(reason);
                }
                let parent = self.inner.store.get(entry.reservation.parent_session_id)?;
                let pending = parent
                    .runs
                    .get(&entry.reservation.parent_run_id)
                    .and_then(|run| {
                        run.pending_calls
                            .get(&entry.reservation.parent_tool_call_id)
                    })
                    .is_some_and(|tool| tool == "delegate_subagent");
                if pending {
                    self.terminate_tool_direct(
                        entry.reservation.parent_session_id,
                        entry.reservation.parent_run_id,
                        entry.reservation.parent_tool_call_id,
                        ToolTerminationOutcome::Failed,
                        Some(delegate_failure_result(
                            Some(entry.reservation.child_session_id),
                            "delegate child session was never created",
                        )),
                        Some(SafeToolError {
                            code: safe_code("child_missing"),
                            message: safe_error("delegate child session was never created"),
                        }),
                    )?;
                }
                continue;
            }
            if self
                .inner
                .store
                .get(entry.reservation.child_session_id)
                .is_ok()
            {
                if entry.terminal_status.is_none() {
                    let child = self.inner.store.get(entry.reservation.child_session_id)?;
                    let recovered_status = entry
                        .child_run_id
                        .and_then(|run_id| child.runs.get(&run_id).map(|run| run.status))
                        .or_else(|| {
                            matches!(
                                child.status,
                                SessionStatus::Completed
                                    | SessionStatus::Failed
                                    | SessionStatus::Cancelled
                                    | SessionStatus::Interrupted
                            )
                            .then_some(child.status)
                        });
                    if let Some(status) = recovered_status.filter(|status| {
                        matches!(
                            status,
                            SessionStatus::Completed
                                | SessionStatus::Failed
                                | SessionStatus::Cancelled
                                | SessionStatus::Interrupted
                        )
                    }) {
                        self.inner
                            .delegation_events
                            .mark_finished(entry.reservation.invocation_id, status)?;
                        entry.terminal_status = Some(status);
                    }
                }
                if entry.request.resume_session_id.is_none() {
                    self.ensure_delegated_context_seed_blocking(
                        entry.reservation.child_session_id,
                        entry.reservation.invocation_id,
                        entry.request.seeded_context.clone(),
                    )?;
                    self.ensure_delegated_title_blocking(
                        entry.reservation.child_session_id,
                        entry.reservation.invocation_id,
                        entry.request.title.clone(),
                    )?;
                }
                let parent = self.inner.store.get(entry.reservation.parent_session_id)?;
                let parent_cancelled = parent
                    .runs
                    .get(&entry.reservation.parent_run_id)
                    .is_some_and(|run| run.status == SessionStatus::Cancelled)
                    && !parent.log.events().iter().any(|event| {
                        matches!(
                            &event.payload,
                            Event::ToolCallTerminated { termination }
                                if termination.tool_call_id
                                    == entry.reservation.parent_tool_call_id
                        )
                    });
                if parent_cancelled {
                    if let Some(child_run_id) = entry.child_run_id {
                        let child = self.inner.store.get(entry.reservation.child_session_id)?;
                        if child.runs.get(&child_run_id).is_some_and(|run| {
                            matches!(
                                run.status,
                                SessionStatus::Running | SessionStatus::Interrupted
                            )
                        }) {
                            self.append_blocking(
                                entry.reservation.child_session_id,
                                Some(child_run_id),
                                super::event_origin("engine:recovery"),
                                Event::RunCancelled {
                                    reason: Some(safe_error("parent delegate run was cancelled")),
                                },
                            )?;
                        }
                    } else if entry.terminal_status.is_none()
                        && latest_invocations.get(&entry.reservation.child_session_id)
                            == Some(&entry.reservation.invocation_id)
                    {
                        self.inner.delegation_events.mark_finished(
                            entry.reservation.invocation_id,
                            SessionStatus::Cancelled,
                        )?;
                        entry.terminal_status = Some(SessionStatus::Cancelled);
                        self.void_runless_pending_inputs_blocking(
                            entry.reservation.child_session_id,
                        )?;
                        let child = self.inner.store.get(entry.reservation.child_session_id)?;
                        if child.status == SessionStatus::Idle {
                            self.append_blocking(
                                entry.reservation.child_session_id,
                                None,
                                super::event_origin("engine:recovery"),
                                Event::DelegateChildTerminated {
                                    status: SessionStatus::Cancelled,
                                    reason: Some(safe_error("parent delegate run was cancelled")),
                                },
                            )?;
                        }
                    }
                }
                self.ensure_parent_link_blocking(
                    entry.reservation.parent_session_id,
                    entry.reservation.parent_run_id,
                    entry.reservation.parent_tool_call_id,
                    entry.reservation.child_session_id,
                )?;
                if !entry.started {
                    self.inner
                        .delegation_events
                        .mark_started(entry.reservation.invocation_id)?;
                }
            }
        }
        self.rebuild_delegation_registry(&delegation_entries)?;
        Ok(())
    }

    pub(super) async fn resolve_interrupted_direct(
        &self,
        session_id: SessionId,
    ) -> Result<(), EngineError> {
        let session = self.inner.store.get(session_id)?;
        let approval_records = approval_records(session_id, &session.log.events());
        for record in approval_records.values().filter(|record| {
            matches!(
                record.status,
                ApprovalStatus::Pending | ApprovalStatus::Escalated
            )
        }) {
            let Some(run_id) = approval_run_id(&session.log.events(), record.request.approval_id())
            else {
                continue;
            };
            let decision = restart_approval_decision();
            self.append_direct(
                session_id,
                Some(run_id),
                super::event_origin("engine:recovery"),
                Event::ApprovalCancelled {
                    approval_id: record.request.approval_id(),
                    reason_code: ApprovalReasonCode::PreparedCapabilityLost,
                },
            )?;
            self.append_direct(
                session_id,
                Some(run_id),
                super::event_origin("engine:recovery"),
                Event::ApprovalFinalized {
                    approval_id: record.request.approval_id(),
                    decision,
                },
            )?;
        }
        for run in session.runs.values().filter(|run| {
            matches!(
                run.status,
                SessionStatus::Interrupted | SessionStatus::Cancelled
            )
        }) {
            for (call, tool) in &run.pending_calls {
                if tool == "delegate_subagent" {
                    let recovery_key = (session_id, run.id, *call);
                    if self
                        .inner
                        .recovery_waiters
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .contains(&recovery_key)
                    {
                        continue;
                    }
                    let invocation = invocation_id(session_id, run.id, *call);
                    let Some(entry) = self.delegation_event_get(invocation).await? else {
                        self.terminate_tool_direct(
                            session_id,
                            run.id,
                            *call,
                            ToolTerminationOutcome::Interrupted,
                            Some(delegate_failure_result(
                                None,
                                "delegate interrupted by daemon restart: no durable reservation",
                            )),
                            Some(SafeToolError {
                                code: safe_code("restart_interrupted"),
                                message: safe_error(
                                    "delegate reservation is missing after restart",
                                ),
                            }),
                        )?;
                        continue;
                    };
                    let child_id = entry.reservation.child_session_id;
                    if run.status == SessionStatus::Cancelled {
                        let result = cancelled_delegate_result_with_reason(
                            Some(child_id),
                            "parent delegate run was cancelled",
                        );
                        self.terminate_tool_direct(
                            session_id,
                            run.id,
                            *call,
                            ToolTerminationOutcome::Cancelled,
                            Some(result),
                            Some(SafeToolError {
                                code: safe_code("parent_cancelled"),
                                message: safe_error("parent delegate run was cancelled"),
                            }),
                        )?;
                        continue;
                    }
                    let child = match self.inner.store.get(child_id) {
                        Ok(child) => child,
                        Err(_) => {
                            self.inner.delegation_events.mark_finished_with_reason(
                                invocation,
                                SessionStatus::Failed,
                                Some(safe_error(
                                    "child_missing: delegate child session is missing",
                                )),
                            )?;
                            self.terminate_tool_direct(
                                session_id,
                                run.id,
                                *call,
                                ToolTerminationOutcome::Failed,
                                Some(delegate_failure_result(
                                    Some(child_id),
                                    "delegate child session is missing",
                                )),
                                Some(SafeToolError {
                                    code: safe_code("child_missing"),
                                    message: safe_error("delegate child session is missing"),
                                }),
                            )?;
                            continue;
                        }
                    };
                    if child.status == SessionStatus::Completed {
                        self.inner
                            .delegation_events
                            .mark_finished(invocation, SessionStatus::Completed)?;
                        let result = completed_delegate_result(&child, entry.child_run_id);
                        self.terminate_tool_direct(
                            session_id,
                            run.id,
                            *call,
                            ToolTerminationOutcome::Completed,
                            Some(result),
                            None,
                        )?;
                    } else if child.status == SessionStatus::Cancelled {
                        self.inner
                            .delegation_events
                            .mark_finished(invocation, SessionStatus::Cancelled)?;
                        let result = cancelled_delegate_result(child_id, None);
                        self.terminate_tool_direct(
                            session_id,
                            run.id,
                            *call,
                            ToolTerminationOutcome::Cancelled,
                            Some(result),
                            Some(SafeToolError {
                                code: safe_code("child_cancelled"),
                                message: safe_error("delegate child was cancelled"),
                            }),
                        )?;
                    } else if entry.child_run_id.is_none() {
                        self.inner
                            .recovery_waiters
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .insert(recovery_key);
                        let child_run_id = match self.ensure_delegate_run(&entry, None).await {
                            Ok(run_id) => run_id,
                            Err(error) => {
                                self.inner
                                    .recovery_waiters
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .remove(&recovery_key);
                                if is_delegation_event_append_failure(&error) {
                                    let _ = self.resolve_delegate_failure_if_pending_direct(
                                        session_id,
                                        run.id,
                                        *call,
                                        delegate_failure_result(
                                            Some(child_id),
                                            "delegate run event confirmation failed",
                                        ),
                                    );
                                }
                                return Err(error);
                            }
                        };
                        let engine = self.clone();
                        let parent_run_id = run.id;
                        let tool_call_id = *call;
                        tokio::spawn(async move {
                            let result = engine
                                .await_delegate(DelegateHandle {
                                    invocation_id: entry.reservation.invocation_id,
                                    child_session_id: child_id,
                                    child_run_id: Some(child_run_id),
                                })
                                .await;
                            if let Ok(result) = result {
                                let _ = engine
                                    .submit_tool_result(
                                        session_id,
                                        parent_run_id,
                                        tool_call_id,
                                        Ok(result),
                                    )
                                    .await;
                            }
                            engine
                                .inner
                                .recovery_waiters
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .remove(&(session_id, parent_run_id, tool_call_id));
                        });
                    } else {
                        self.inner
                            .delegation_events
                            .mark_finished(invocation, SessionStatus::Interrupted)?;
                        self.terminate_tool_direct(
                            session_id,
                            run.id,
                            *call,
                            ToolTerminationOutcome::Interrupted,
                            Some(delegate_failure_result(
                                Some(child_id),
                                "delegate child interrupted by daemon restart",
                            )),
                            Some(SafeToolError {
                                code: safe_code("child_interrupted"),
                                message: safe_error("delegate child interrupted by daemon restart"),
                            }),
                        )?;
                    }
                } else {
                    let failure = restart_tool_failure();
                    self.terminate_tool_direct(
                        session_id,
                        run.id,
                        *call,
                        ToolTerminationOutcome::Interrupted,
                        None,
                        Some(SafeToolError {
                            code: failure.code.safe_code(),
                            message: safe_error(&failure.message),
                        }),
                    )?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn rebuild_approvals(&self) {
        for session in self.inner.store.all() {
            for envelope in session.log.events() {
                if let Event::TreeApprovalGrantCommitted { grant } = envelope.payload
                    && !grant.resources.is_empty()
                    && grant.resources.iter().all(|resource| {
                        resource.binding_lifetime
                            == cookie_agent_protocol::PreparedBindingLifetime::RestartStable
                    })
                {
                    self.inner.approvals.grant(grant);
                }
            }
        }
        self.inner
            .approvals
            .invalidate_grants(&self.inner.grant_journal.invalidated_ids());
    }
}

pub(crate) fn restart_approval_decision() -> ApprovalFinalDecision {
    ApprovalFinalDecision {
        outcome: ApprovalFinalOutcome::Cancelled,
        source: ApprovalDecisionSource::System,
        reason_code: ApprovalReasonCode::PreparedCapabilityLost,
        feedback: None,
        tree_grant_id: None,
    }
}

pub(crate) fn restart_tool_failure() -> ToolFailure {
    ToolFailure {
        code: ToolCallFailureCode::PreparedCapabilityLost,
        message: "prepared capability lost during daemon restart".into(),
    }
}
