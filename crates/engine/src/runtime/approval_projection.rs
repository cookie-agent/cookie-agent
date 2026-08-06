use super::*;

impl Engine {
    pub(super) fn approval_evaluation_complete_direct(
        &self,
        session: SessionId,
        run: RunId,
        request: ApprovalRequest,
        executor: PreparedExecutorCell,
        decision: ApprovalInternalDecisionKind,
        cancelled: bool,
    ) -> Result<ApprovalEvaluationTransition, EngineError> {
        let approval_id = request.approval_id();
        let events = self.inner.store.get(session)?.log.events();
        let Some(record) = approval_records(session, &events).remove(&approval_id) else {
            return Err(EngineError::ApprovalNotPending {
                session_id: session,
                approval_id,
            });
        };
        if record.status != ApprovalStatus::Pending
            || approval_run_id(&events, approval_id) != Some(run)
        {
            return Err(EngineError::ApprovalNotPending {
                session_id: session,
                approval_id,
            });
        }
        if cancelled {
            self.approval_terminal_direct(session, run, approval_id, ApprovalTerminal::Cancelled)?;
            return Ok(ApprovalEvaluationTransition::Resolved(ApprovalOutcome {
                approved: false,
                feedback: Some("cancelled".into()),
            }));
        }
        if approval_deadline_exhausted(approval_constraints(&request).expires_at) {
            self.approval_terminal_direct(session, run, approval_id, ApprovalTerminal::Expired)?;
            return Ok(ApprovalEvaluationTransition::Resolved(ApprovalOutcome {
                approved: false,
                feedback: Some("approval expired unattended".into()),
            }));
        }

        let source = ApprovalDecisionSource::InternalAgent;
        let (decision, reason_code) = match decision {
            ApprovalInternalDecisionKind::Allow => (
                ApprovalInternalDecisionKind::Allow,
                ApprovalReasonCode::InternalAgentAllowed,
            ),
            ApprovalInternalDecisionKind::Deny => (
                ApprovalInternalDecisionKind::Deny,
                ApprovalReasonCode::InternalAgentDenied,
            ),
            ApprovalInternalDecisionKind::Ask | ApprovalInternalDecisionKind::Escalate => (
                ApprovalInternalDecisionKind::Escalate,
                ApprovalReasonCode::Escalated,
            ),
        };
        self.append_direct(
            session,
            Some(run),
            Event::ApprovalEvaluated {
                approval_id,
                decision: ApprovalInternalDecision {
                    decision,
                    source,
                    reason_code,
                    evaluations: approval_evaluations(&request),
                },
            },
        )?;
        if matches!(
            decision,
            ApprovalInternalDecisionKind::Allow | ApprovalInternalDecisionKind::Deny
        ) {
            let approved = decision == ApprovalInternalDecisionKind::Allow;
            self.append_direct(
                session,
                Some(run),
                Event::ApprovalFinalized {
                    approval_id,
                    decision: ApprovalFinalDecision {
                        outcome: if approved {
                            ApprovalFinalOutcome::Approved
                        } else {
                            ApprovalFinalOutcome::Rejected
                        },
                        source,
                        reason_code,
                        feedback: None,
                        tree_grant_id: None,
                    },
                },
            )?;
            return Ok(ApprovalEvaluationTransition::Resolved(ApprovalOutcome {
                approved,
                feedback: None,
            }));
        }

        self.append_direct(
            session,
            Some(run),
            Event::ApprovalEscalated {
                approval_id,
                reason_code: ApprovalReasonCode::Escalated,
            },
        )?;
        let (sender, receiver) = oneshot::channel();
        let replaced = self
            .inner
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert((session, approval_id), PendingApproval { sender, executor });
        if replaced.is_some() {
            return Err(EngineError::ApprovalConflict);
        }
        Ok(ApprovalEvaluationTransition::Escalated(receiver))
    }

    pub(super) fn approval_respond_direct(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<ApprovalRespondResult, EngineError> {
        let projection = self.inner.store.get(params.session_id)?;
        let events = projection.log.events();
        if let Some((recorded_approval_id, recorded_decision, recorded_feedback)) =
            events.iter().find_map(|event| match &event.payload {
                Event::ApprovalUserDecisionRecorded {
                    approval_id,
                    client_response_id,
                    decision,
                    feedback,
                } if client_response_id == &params.client_response_id => {
                    Some((*approval_id, *decision, feedback.clone()))
                }
                _ => None,
            })
        {
            let Some(request) =
                approval_records(params.session_id, &events).remove(&recorded_approval_id)
            else {
                return Err(approval_response_failure(
                    &params,
                    ApprovalRespondErrorCode::ApprovalNotFound,
                    None,
                ));
            };
            if recorded_approval_id != params.approval_id
                || recorded_decision != params.decision
                || recorded_feedback != params.feedback
                || approval_request_revision(&request.request) != params.request_revision
                || request.request.operation_fingerprint() != &params.operation_fingerprint
            {
                return Err(approval_response_failure(
                    &params,
                    ApprovalRespondErrorCode::IdempotencyConflict,
                    Some(&request),
                ));
            }
            return Ok(ApprovalRespondResult {
                client_response_id: params.client_response_id,
                approval: request,
            });
        }

        let mut records = approval_records(params.session_id, &events);
        let Some(record) = records.remove(&params.approval_id) else {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalNotFound,
                None,
            ));
        };
        if !matches!(
            record.status,
            ApprovalStatus::Pending | ApprovalStatus::Escalated
        ) {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalNotPending,
                Some(&record),
            ));
        }
        let run_id =
            approval_run_id(&events, params.approval_id).ok_or(EngineError::ApprovalConflict)?;
        if approval_deadline_exhausted(approval_constraints(&record.request).expires_at) {
            self.approval_terminal_direct(
                params.session_id,
                run_id,
                params.approval_id,
                ApprovalTerminal::Expired,
            )?;
            let current = approval_records(
                params.session_id,
                &self.inner.store.get(params.session_id)?.log.events(),
            )
            .remove(&params.approval_id);
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalNotPending,
                current.as_ref(),
            ));
        }
        if record.status != ApprovalStatus::Escalated {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalNotPending,
                Some(&record),
            ));
        }
        if approval_request_revision(&record.request) != params.request_revision {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalRevisionConflict,
                Some(&record),
            ));
        }
        if record.request.operation_fingerprint() != &params.operation_fingerprint {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::OperationFingerprintMismatch,
                Some(&record),
            ));
        }
        let allowed = match params.decision {
            ApprovalUserDecision::ApproveOnce => approval_constraints(&record.request).allow_once,
            ApprovalUserDecision::ApproveTree => {
                approval_constraints(&record.request).allow_tree_grant
            }
            ApprovalUserDecision::Reject => true,
            ApprovalUserDecision::Cancel => approval_constraints(&record.request).cancellable,
        };
        if !allowed {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::DecisionNotAllowed,
                Some(&record),
            ));
        }
        self.append_direct(
            params.session_id,
            Some(run_id),
            Event::ApprovalUserDecisionRecorded {
                approval_id: params.approval_id,
                client_response_id: params.client_response_id.clone(),
                decision: params.decision,
                feedback: params.feedback.clone(),
            },
        )?;
        let root = root_id(&projection.meta.origin, params.session_id);
        let grant = if params.decision == ApprovalUserDecision::ApproveTree {
            let grant = TreeApprovalGrant {
                grant_id: cookie_agent_protocol::TreeApprovalGrantId::new_v7(),
                root_session_id: root,
                approval_id: params.approval_id,
                operation_fingerprint: record.request.operation_fingerprint().clone(),
                capabilities: record.request.operation().capabilities().to_vec(),
                resources: record.request.operation().resources().to_vec(),
                created_at: jiff::Timestamp::now(),
            };
            grant
                .validate()
                .map_err(|_| EngineError::ApprovalConflict)?;
            Some(grant)
        } else {
            None
        };
        if let Some(grant) = &grant {
            self.append_direct(
                params.session_id,
                Some(run_id),
                Event::TreeApprovalGrantCommitted {
                    grant: grant.clone(),
                },
            )?;
            self.inner.approvals.grant(grant.clone());
        }
        let (outcome, reason_code, approved) = match params.decision {
            ApprovalUserDecision::ApproveOnce => (
                ApprovalFinalOutcome::Approved,
                ApprovalReasonCode::UserApprovedOnce,
                true,
            ),
            ApprovalUserDecision::ApproveTree => (
                ApprovalFinalOutcome::Approved,
                ApprovalReasonCode::UserApprovedTree,
                true,
            ),
            ApprovalUserDecision::Reject => (
                ApprovalFinalOutcome::Rejected,
                ApprovalReasonCode::UserRejected,
                false,
            ),
            ApprovalUserDecision::Cancel => (
                ApprovalFinalOutcome::Cancelled,
                ApprovalReasonCode::UserCancelled,
                false,
            ),
        };
        self.append_direct(
            params.session_id,
            Some(run_id),
            Event::ApprovalFinalized {
                approval_id: params.approval_id,
                decision: ApprovalFinalDecision {
                    outcome,
                    source: ApprovalDecisionSource::User,
                    reason_code,
                    feedback: params.feedback.clone(),
                    tree_grant_id: grant.as_ref().map(|grant| grant.grant_id),
                },
            },
        )?;
        if let Some(pending) = self
            .inner
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(params.session_id, params.approval_id))
        {
            let _ = pending.sender.send(ApprovalOutcome {
                approved,
                feedback: params
                    .feedback
                    .as_ref()
                    .map(|feedback| feedback.message.to_string()),
            });
        }
        let events = self.inner.store.get(params.session_id)?.log.events();
        let approval = approval_records(params.session_id, &events)
            .remove(&params.approval_id)
            .ok_or(EngineError::ApprovalConflict)?;
        Ok(ApprovalRespondResult {
            client_response_id: params.client_response_id,
            approval,
        })
    }

    pub(super) fn approval_capability_invalid_direct(
        &self,
        params: ApprovalRespondParams,
        invalidation: PreparedApprovalInvalidation,
    ) -> Result<ApprovalRespondResult, EngineError> {
        let projection = self.inner.store.get(params.session_id)?;
        let events = projection.log.events();
        if events.iter().any(|event| {
            matches!(
                &event.payload,
                Event::ApprovalUserDecisionRecorded { client_response_id, .. }
                    if client_response_id == &params.client_response_id
            )
        }) {
            return self.approval_respond_direct(params);
        }
        let mut records = approval_records(params.session_id, &events);
        let Some(record) = records.remove(&params.approval_id) else {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalNotFound,
                None,
            ));
        };
        if record.status != ApprovalStatus::Escalated {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalNotPending,
                Some(&record),
            ));
        }
        if approval_request_revision(&record.request) != params.request_revision {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::ApprovalRevisionConflict,
                Some(&record),
            ));
        }
        if record.request.operation_fingerprint() != &params.operation_fingerprint {
            return Err(approval_response_failure(
                &params,
                ApprovalRespondErrorCode::OperationFingerprintMismatch,
                Some(&record),
            ));
        }
        let run_id =
            approval_run_id(&events, params.approval_id).ok_or(EngineError::ApprovalConflict)?;
        let reason_code = match invalidation {
            PreparedApprovalInvalidation::OperationChanged => ApprovalReasonCode::OperationChanged,
            PreparedApprovalInvalidation::PreparedCapabilityLost => {
                ApprovalReasonCode::PreparedCapabilityLost
            }
        };
        self.append_direct(
            params.session_id,
            Some(run_id),
            Event::ApprovalCancelled {
                approval_id: params.approval_id,
                reason_code,
            },
        )?;
        self.append_direct(
            params.session_id,
            Some(run_id),
            Event::ApprovalFinalized {
                approval_id: params.approval_id,
                decision: ApprovalFinalDecision {
                    outcome: ApprovalFinalOutcome::Cancelled,
                    source: ApprovalDecisionSource::System,
                    reason_code,
                    feedback: None,
                    tree_grant_id: None,
                },
            },
        )?;
        if let Some(pending) = self
            .inner
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(params.session_id, params.approval_id))
        {
            let _ = pending.sender.send(ApprovalOutcome {
                approved: false,
                feedback: Some(match invalidation {
                    PreparedApprovalInvalidation::OperationChanged => {
                        "prepared operation changed before approval response".into()
                    }
                    PreparedApprovalInvalidation::PreparedCapabilityLost => {
                        "prepared capability was lost before approval response".into()
                    }
                }),
            });
        }
        let current = approval_records(
            params.session_id,
            &self.inner.store.get(params.session_id)?.log.events(),
        )
        .remove(&params.approval_id);
        Err(approval_response_failure(
            &params,
            ApprovalRespondErrorCode::OperationChanged,
            current.as_ref(),
        ))
    }

    pub(super) fn approval_terminal_direct(
        &self,
        session: SessionId,
        run: RunId,
        approval_id: ApprovalId,
        terminal: ApprovalTerminal,
    ) -> Result<bool, EngineError> {
        let events = self.inner.store.get(session)?.log.events();
        let Some(record) = approval_records(session, &events).remove(&approval_id) else {
            return Ok(false);
        };
        if !matches!(
            record.status,
            ApprovalStatus::Pending | ApprovalStatus::Escalated
        ) || approval_run_id(&events, approval_id) != Some(run)
        {
            return Ok(false);
        }
        let (reason_code, outcome, final_reason) = match terminal {
            ApprovalTerminal::Cancelled => (
                ApprovalReasonCode::RequestCancelled,
                ApprovalFinalOutcome::Cancelled,
                ApprovalReasonCode::RequestCancelled,
            ),
            ApprovalTerminal::Expired => (
                ApprovalReasonCode::ApprovalExpired,
                ApprovalFinalOutcome::Expired,
                ApprovalReasonCode::ApprovalExpired,
            ),
        };
        self.append_direct(
            session,
            Some(run),
            Event::ApprovalCancelled {
                approval_id,
                reason_code,
            },
        )?;
        self.append_direct(
            session,
            Some(run),
            Event::ApprovalFinalized {
                approval_id,
                decision: ApprovalFinalDecision {
                    outcome,
                    source: ApprovalDecisionSource::System,
                    reason_code: final_reason,
                    feedback: None,
                    tree_grant_id: None,
                },
            },
        )?;
        if let Some(pending) = self
            .inner
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(session, approval_id))
        {
            let _ = pending.sender.send(ApprovalOutcome {
                approved: false,
                feedback: Some(match terminal {
                    ApprovalTerminal::Cancelled => "cancelled".into(),
                    ApprovalTerminal::Expired => "approval expired unattended".into(),
                }),
            });
        }
        Ok(true)
    }
}

pub(super) fn approval_response_failure(
    params: &ApprovalRespondParams,
    code: ApprovalRespondErrorCode,
    current: Option<&ApprovalRecord>,
) -> EngineError {
    EngineError::ApprovalResponse(Box::new(ApprovalRespondFailure {
        code,
        session_id: params.session_id,
        approval_id: params.approval_id,
        client_response_id: params.client_response_id.to_string(),
        current_status: current.map(|record| record.status),
        current_revision: current.map(|record| approval_request_revision(&record.request)),
        current_expires_at: current
            .and_then(|record| approval_constraints(&record.request).expires_at),
        current_operation_fingerprint: current
            .map(|record| record.request.operation_fingerprint().clone()),
    }))
}

pub(super) fn approval_request_revision(request: &ApprovalRequest) -> u64 {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| value.get("revision").and_then(Value::as_u64))
        .expect("protocol approval request always serializes its revision")
}

pub(crate) fn approval_records(
    session_id: SessionId,
    events: &[StoredEvent],
) -> HashMap<ApprovalId, ApprovalRecord> {
    let mut records = HashMap::<ApprovalId, ApprovalRecord>::new();
    for envelope in events {
        match &envelope.payload {
            Event::ApprovalRequested { request } => {
                records.insert(
                    request.approval_id(),
                    ApprovalRecord {
                        session_id,
                        request: request.clone(),
                        status: ApprovalStatus::Pending,
                        internal_decision: None,
                        user_decision: None,
                        final_decision: None,
                    },
                );
            }
            Event::ApprovalEvaluated {
                approval_id,
                decision,
            } => {
                if let Some(record) = records.get_mut(approval_id) {
                    record.internal_decision = Some(decision.clone());
                }
            }
            Event::ApprovalEscalated { approval_id, .. } => {
                if let Some(record) = records.get_mut(approval_id) {
                    record.status = ApprovalStatus::Escalated;
                }
            }
            Event::ApprovalUserDecisionRecorded {
                approval_id,
                decision,
                ..
            } => {
                if let Some(record) = records.get_mut(approval_id) {
                    record.user_decision = Some(*decision);
                }
            }
            Event::ApprovalFinalized {
                approval_id,
                decision,
            } => {
                if let Some(record) = records.get_mut(approval_id) {
                    record.status = match decision.outcome {
                        ApprovalFinalOutcome::Approved => ApprovalStatus::Approved,
                        ApprovalFinalOutcome::Rejected => ApprovalStatus::Rejected,
                        ApprovalFinalOutcome::Cancelled => ApprovalStatus::Cancelled,
                        ApprovalFinalOutcome::Expired => ApprovalStatus::Expired,
                    };
                    record.final_decision = Some(decision.clone());
                }
            }
            Event::ApprovalCancelled { approval_id, .. } => {
                if let Some(record) = records.get_mut(approval_id) {
                    record.status = ApprovalStatus::Cancelled;
                }
            }
            _ => {}
        }
    }
    records
}

pub(super) fn approval_run_id(events: &[StoredEvent], approval_id: ApprovalId) -> Option<RunId> {
    events.iter().find_map(|event| match &event.payload {
        Event::ApprovalRequested { request } if request.approval_id() == approval_id => {
            event.run_id
        }
        _ => None,
    })
}

pub(crate) fn doom_loop_repetitions(
    events: &[StoredEvent],
    run_id: RunId,
    fingerprint: &OperationFingerprint,
) -> u32 {
    let mut starts = HashMap::<ToolCallId, OperationFingerprint>::new();
    let mut repetitions = 0_u32;
    for event in events.iter().filter(|event| event.run_id == Some(run_id)) {
        match &event.payload {
            Event::UserInputSubmitted { .. } | Event::UserInputApplied { .. } => {
                repetitions = 0;
            }
            Event::ApprovalRequested { request }
                if request.operation_fingerprint() == fingerprint =>
            {
                repetitions = repetitions.saturating_add(1);
            }
            Event::ToolCallStarted { start } => {
                starts.insert(start.tool_call_id, start.operation_fingerprint.clone());
            }
            Event::ToolCallTerminated { termination }
                if termination.outcome == ToolTerminationOutcome::Completed
                    && starts
                        .get(&termination.tool_call_id)
                        .is_some_and(|completed| completed != fingerprint) =>
            {
                repetitions = 0;
            }
            _ => {}
        }
    }
    repetitions
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeniedToolFailure {
    kind: String,
    source: ApprovalDecisionSource,
    reason: String,
    feedback: Option<String>,
}

pub(super) fn denied_tool_failure(
    source: ApprovalDecisionSource,
    reason: impl Into<String>,
    feedback: Option<String>,
) -> String {
    serde_json::to_string(&DeniedToolFailure {
        kind: "tool_denied".into(),
        source,
        reason: reason.into(),
        feedback,
    })
    .expect("denied tool failure serializes")
}
