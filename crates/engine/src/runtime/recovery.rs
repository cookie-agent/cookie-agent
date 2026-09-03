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
    pub(super) fn reconcile_session(&self, session_id: SessionId) -> Result<(), EngineError> {
        let session = self.inner.store.get(session_id)?;
        let events = session.log.event_snapshot();
        let interrupted_runs = session
            .runs
            .values()
            .filter(|run| run.status == SessionStatus::Running)
            .map(|run| run.id)
            .collect::<Vec<_>>();
        {
            let mut internal = HashMap::new();
            for event in events.iter() {
                match &event.payload {
                    Event::InternalAgentStarted {
                        invocation_id,
                        internal_run_id,
                        kind,
                        ..
                    } => {
                        internal.insert((*invocation_id, *internal_run_id), (*kind, event.run_id));
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
                        internal.remove(&(*invocation_id, *internal_run_id));
                    }
                    _ => {}
                }
            }
            for ((invocation_id, internal_run_id), (kind, parent_run)) in internal {
                self.append_recovery_direct(
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
                self.append_recovery_direct(
                    session.meta.session_id,
                    Some(run.id),
                    super::event_origin("engine:recovery"),
                    Event::RunInterrupted {
                        reason: Some(safe_error("daemon restart")),
                    },
                )?;
            }
            #[cfg(test)]
            if self
                .inner
                .adoption_reconcile_failures
                .fetch_update(
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
            {
                return Err(EngineError::ActorStopped);
            }
            for run in session.runs.values() {
                for (tool_call_id, tool) in &run.pending_calls {
                    if tool == "delegate_subagent" {
                        continue;
                    }
                    let failure = restart_tool_failure();
                    self.append_recovery_direct(
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
            for record in approval_records(session.meta.session_id, &events)
                .into_values()
                .filter(|record| {
                    matches!(
                        record.status,
                        ApprovalStatus::Pending | ApprovalStatus::Escalated
                    )
                })
            {
                let approval_run = approval_run_id(&events, record.request.approval_id())
                    .ok_or(EngineError::ApprovalConflict)?;
                self.append_recovery_direct(
                    session.meta.session_id,
                    Some(approval_run),
                    super::event_origin("engine:recovery"),
                    Event::ApprovalCancelled {
                        approval_id: record.request.approval_id(),
                        reason_code: ApprovalReasonCode::PreparedCapabilityLost,
                    },
                )?;
                self.append_recovery_direct(
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
        for entry in self.inner.delegation_events.entries() {
            if entry.reservation.child_session_id == session_id
                && entry.request.resume_session_id.is_none()
            {
                self.ensure_delegated_context_seed_blocking(
                    session_id,
                    entry.reservation.invocation_id,
                    entry.request.seeded_context,
                )?;
                self.ensure_delegated_title_blocking(
                    session_id,
                    entry.reservation.invocation_id,
                    entry.request.title,
                )?;
            }
        }
        self.interrupt_delegation_accounting(session_id, &interrupted_runs)?;
        Ok(())
    }

    pub(super) fn recover_missing_delegation_children(
        &self,
        parent_session_id: SessionId,
    ) -> Result<(), EngineError> {
        for entry in self.inner.delegation_events.entries() {
            if entry.reservation.parent_session_id != parent_session_id
                || entry.terminal_status.is_some()
                || self
                    .inner
                    .store
                    .get(entry.reservation.child_session_id)
                    .is_ok()
            {
                continue;
            }
            let reason = safe_error("child_missing: child session was never created");
            let parent = self.inner.store.get(parent_session_id)?;
            let pending = parent
                .runs
                .get(&entry.reservation.parent_run_id)
                .and_then(|run| {
                    run.pending_calls
                        .get(&entry.reservation.parent_tool_call_id)
                })
                .is_some_and(|tool| tool == "delegate_subagent");
            self.inner.delegation_events.mark_finished_with_reason(
                entry.reservation.invocation_id,
                SessionStatus::Failed,
                Some(reason),
            )?;
            if pending {
                self.terminate_tool_direct(
                    parent_session_id,
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
        }
        Ok(())
    }

    pub(super) async fn resolve_interrupted_direct(
        &self,
        session_id: SessionId,
    ) -> Result<(), EngineError> {
        let session = self.inner.store.get(session_id)?;
        let events = session.log.event_snapshot();
        let approval_records = approval_records(session_id, &events);
        for record in approval_records.values().filter(|record| {
            matches!(
                record.status,
                ApprovalStatus::Pending | ApprovalStatus::Escalated
            )
        }) {
            let Some(run_id) = approval_run_id(&events, record.request.approval_id()) else {
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
        for session in self.inner.store.all_snapshots() {
            for envelope in session.log.event_snapshot().iter() {
                if let Event::TreeApprovalGrantCommitted { grant } = &envelope.payload
                    && !grant.resources.is_empty()
                    && grant.resources.iter().all(|resource| {
                        resource.binding_lifetime
                            == cookie_agent_protocol::PreparedBindingLifetime::RestartStable
                    })
                {
                    self.inner.approvals.grant(grant.clone());
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
