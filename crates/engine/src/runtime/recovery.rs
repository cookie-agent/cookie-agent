use std::collections::{HashMap, HashSet};

use cookie_agent_protocol::{
    ApprovalDecisionSource, ApprovalFinalDecision, ApprovalFinalOutcome, ApprovalReasonCode,
    ApprovalStatus, SafeToolError, SessionId, SessionOrigin, SessionStatus, ToolCallTermination,
    ToolTerminationOutcome,
};

use super::{
    Engine, EngineError, Event, ToolCallFailureCode, ToolFailure,
    approval_projection::{approval_records, approval_run_id},
    delegation::{
        cancelled_delegate_result, cancelled_delegate_result_with_reason,
        completed_delegate_result, delegate_failure_result, is_journal_append_failure,
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
                    Event::ApprovalCancelled {
                        approval_id: record.request.approval_id(),
                        reason_code: ApprovalReasonCode::PreparedCapabilityLost,
                    },
                )?;
                self.append_blocking(
                    session.meta.session_id,
                    Some(approval_run),
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
        let mut journal_entries = self.inner.journal.entries();
        let mut latest_invocations = std::collections::HashMap::new();
        for entry in &journal_entries {
            latest_invocations.insert(
                entry.reservation.child_session_id,
                entry.reservation.invocation_id,
            );
        }
        for entry in &mut journal_entries {
            if self
                .inner
                .store
                .get(entry.reservation.child_session_id)
                .is_ok()
            {
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
                                Event::RunCancelled {
                                    reason: Some(safe_error("parent delegate run was cancelled")),
                                },
                            )?;
                        }
                    } else if entry.terminal_status.is_none()
                        && latest_invocations.get(&entry.reservation.child_session_id)
                            == Some(&entry.reservation.invocation_id)
                    {
                        self.inner.journal.mark_terminated(
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
                if !entry.linked {
                    self.inner
                        .journal
                        .mark_linked(entry.reservation.invocation_id)?;
                }
            }
        }
        let known_invocations: HashSet<_> = journal_entries
            .iter()
            .map(|entry| entry.reservation.invocation_id)
            .collect();
        for session in self.inner.store.all() {
            if let SessionOrigin::Delegated { invocation_id, .. } = session.meta.origin
                && !known_invocations.contains(&invocation_id)
            {
                // A valid delegated directory without a durable reservation is
                // foreign/orphaned. Preserve it for inspection but never attach it.
                if !session.runs.is_empty() {
                    for run in session
                        .runs
                        .values()
                        .filter(|run| run.status != SessionStatus::Interrupted)
                    {
                        self.append_blocking(
                            session.meta.session_id,
                            Some(run.id),
                            Event::RunInterrupted {
                                reason: Some(safe_error(
                                    "orphaned delegated session without journal reservation",
                                )),
                            },
                        )?;
                    }
                }
            }
        }
        self.rebuild_delegation_registry(&journal_entries)?;
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
                Event::ApprovalCancelled {
                    approval_id: record.request.approval_id(),
                    reason_code: ApprovalReasonCode::PreparedCapabilityLost,
                },
            )?;
            self.append_direct(
                session_id,
                Some(run_id),
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
                    let Some(entry) = self.journal_get(invocation).await? else {
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
                                if is_journal_append_failure(&error) {
                                    let _ = self.resolve_delegate_failure_if_pending_direct(
                                        session_id,
                                        run.id,
                                        *call,
                                        delegate_failure_result(
                                            Some(child_id),
                                            "delegate journal run confirmation failed",
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
