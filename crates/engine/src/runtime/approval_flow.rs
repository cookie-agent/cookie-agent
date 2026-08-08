use super::*;

impl Engine {
    pub(super) async fn request_model_approval(
        &self,
        active: &ActiveRun,
        run: RunId,
        operation: &PreparedOperationIdentity,
        policy_labels: &[String],
        executor: PreparedExecutorCell,
        message: Option<String>,
    ) -> Result<ApprovalOutcome, EngineError> {
        let approval_policy = self.active_internal_policy(active, InternalAgentKind::Approval);
        let request = approval_request_for_operation(
            ApprovalTrigger::ModelToolApproval,
            operation.clone(),
            operation
                .resources()
                .iter()
                .zip(policy_labels)
                .map(|(resource, label)| cookie_agent_protocol::DecisionTrace {
                    action: resource.capability,
                    normalized_resource: label.clone(),
                    candidates: Vec::new(),
                    effect: cookie_agent_protocol::PermissionEffect::Ask,
                    precedence_reason: message
                        .clone()
                        .unwrap_or_else(|| "model requested tool approval".into()),
                })
                .collect(),
            false,
            approval_expiry(approval_policy.limits.timeout_ms),
        );
        self.await_user_approval(active, run, request, executor, false)
            .await
    }

    pub(super) async fn await_user_approval(
        &self,
        active: &ActiveRun,
        run: RunId,
        request: ApprovalRequest,
        executor: PreparedExecutorCell,
        allow_prior_grant: bool,
    ) -> Result<ApprovalOutcome, EngineError> {
        let approval_id = request.approval_id();
        let session = self.inner.store.get(active.session)?;
        let root = root_id(&session.meta.origin, active.session);
        self.append(
            active.session,
            Some(run),
            Event::ApprovalRequested {
                request: request.clone(),
            },
        )
        .await?;

        let repetitions = doom_loop_repetitions(
            &self.inner.store.get(active.session)?.log.events(),
            run,
            request.operation_fingerprint(),
        );
        if repetitions >= 4 {
            self.append(
                active.session,
                Some(run),
                Event::ApprovalDoomLoopDetected {
                    approval_id,
                    operation_fingerprint: request.operation_fingerprint().clone(),
                    repetitions,
                },
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                Event::ApprovalFinalized {
                    approval_id,
                    decision: ApprovalFinalDecision {
                        outcome: ApprovalFinalOutcome::Rejected,
                        source: ApprovalDecisionSource::DoomLoopGuard,
                        reason_code: ApprovalReasonCode::DoomLoopDetected,
                        feedback: None,
                        tree_grant_id: None,
                    },
                },
            )
            .await?;
            return Ok(ApprovalOutcome {
                approved: false,
                feedback: None,
            });
        }

        if allow_prior_grant
            && let Some(grant) = self.inner.approvals.matching(root, request.operation())
        {
            let decision = ApprovalInternalDecision {
                decision: ApprovalInternalDecisionKind::Allow,
                source: ApprovalDecisionSource::TreeGrant,
                reason_code: ApprovalReasonCode::TreeGrantMatched,
                evaluations: approval_evaluations(&request),
            };
            self.append(
                active.session,
                Some(run),
                Event::ApprovalEvaluated {
                    approval_id,
                    decision,
                },
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                Event::ApprovalFinalized {
                    approval_id,
                    decision: ApprovalFinalDecision {
                        outcome: ApprovalFinalOutcome::Approved,
                        source: ApprovalDecisionSource::TreeGrant,
                        reason_code: ApprovalReasonCode::TreeGrantMatched,
                        feedback: None,
                        tree_grant_id: Some(grant.grant_id),
                    },
                },
            )
            .await?;
            return Ok(ApprovalOutcome {
                approved: true,
                feedback: None,
            });
        }

        if approval_evaluations(&request)
            .iter()
            .any(|evaluation| evaluation.effect == cookie_agent_protocol::PermissionEffect::Deny)
        {
            let decision = ApprovalInternalDecision {
                decision: ApprovalInternalDecisionKind::Deny,
                source: ApprovalDecisionSource::Policy,
                reason_code: ApprovalReasonCode::PolicyDenied,
                evaluations: approval_evaluations(&request),
            };
            self.append(
                active.session,
                Some(run),
                Event::ApprovalEvaluated {
                    approval_id,
                    decision,
                },
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                Event::ApprovalFinalized {
                    approval_id,
                    decision: ApprovalFinalDecision {
                        outcome: ApprovalFinalOutcome::Rejected,
                        source: ApprovalDecisionSource::Policy,
                        reason_code: ApprovalReasonCode::PolicyDenied,
                        feedback: None,
                        tree_grant_id: None,
                    },
                },
            )
            .await?;
            return Ok(ApprovalOutcome {
                approved: false,
                feedback: None,
            });
        }

        let permission_mode = self
            .inner
            .permission_modes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&active.session)
            .copied()
            .unwrap_or_default();
        if permission_mode == PermissionMode::Yolo {
            self.append(
                active.session,
                Some(run),
                Event::ApprovalEvaluated {
                    approval_id,
                    decision: ApprovalInternalDecision {
                        decision: ApprovalInternalDecisionKind::Allow,
                        source: ApprovalDecisionSource::Policy,
                        reason_code: ApprovalReasonCode::YoloApproved,
                        evaluations: approval_evaluations(&request),
                    },
                },
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                Event::ApprovalFinalized {
                    approval_id,
                    decision: ApprovalFinalDecision {
                        outcome: ApprovalFinalOutcome::Approved,
                        source: ApprovalDecisionSource::Policy,
                        reason_code: ApprovalReasonCode::YoloApproved,
                        feedback: None,
                        tree_grant_id: None,
                    },
                },
            )
            .await?;
            return Ok(ApprovalOutcome {
                approved: true,
                feedback: None,
            });
        }

        let internal_kind = match permission_mode {
            PermissionMode::Ask => ApprovalInternalDecisionKind::Ask,
            PermissionMode::AutoApprove => {
                let safe_resources = approval_evaluations(&request)
                    .iter()
                    .map(|evaluation| evaluation.trace.normalized_resource.clone())
                    .collect::<Vec<_>>();
                let safe_operations = request
                    .operation()
                    .capabilities()
                    .iter()
                    .map(|capability| capability.operation.as_str())
                    .collect::<Vec<_>>();
                let prompt = serde_json::to_string(&serde_json::json!({
                    "instruction": "Return strict JSON only: {\"decision\":\"allow\"|\"deny\"|\"ask\"}.",
                    "cwd_identity": session.meta.cwd_identity,
                    "operations": safe_operations,
                    "resource_labels": safe_resources,
                }))
                .expect("safe approval prompt serializes");
                #[cfg(test)]
                let hook = {
                    self.inner
                        .approval_evaluation_hook
                        .lock()
                        .expect("approval evaluation hook lock poisoned")
                        .take()
                };
                #[cfg(test)]
                if let Some(hook) = hook {
                    if let Some(reached) = hook
                        .reached
                        .lock()
                        .expect("approval evaluation reached lock poisoned")
                        .take()
                    {
                        let _ = reached.send(());
                    }
                    hook.release.notified().await;
                }
                let approval_policy =
                    self.active_internal_policy(active, InternalAgentKind::Approval);
                match self
                    .run_internal_text_agent(
                        active.session,
                        Some(run),
                        InternalAgentKind::Approval,
                        &approval_policy,
                        prompt,
                        InternalAgentExecution {
                            cancellation: &active.cancellation,
                            actor_direct: false,
                        },
                    )
                    .await
                {
                    Ok(result) => parse_internal_approval(&result.text)
                        .unwrap_or(ApprovalInternalDecisionKind::Ask),
                    Err(_) => ApprovalInternalDecisionKind::Ask,
                }
            }
            PermissionMode::Yolo => unreachable!("yolo approvals resolve before prompting"),
        };
        let transition = self
            .request(active.session, |reply| {
                SessionCommand::ApprovalEvaluationComplete {
                    run,
                    request: request.clone(),
                    executor: executor.clone(),
                    decision: internal_kind,
                    cancelled: active.cancellation.is_cancelled(),
                    reply,
                }
            })
            .await?;
        let mut receiver = match transition {
            ApprovalEvaluationTransition::Resolved(outcome) => return Ok(outcome),
            ApprovalEvaluationTransition::Escalated(receiver) => receiver,
        };
        let expiry_wait = approval_expiry_wait(approval_constraints(&request).expires_at);
        tokio::select! {
            decision = &mut receiver => decision.map_err(|_| EngineError::ActorStopped),
            _ = active.cancellation.cancelled() => {
                let finalized = self.request(active.session, |reply| {
                    SessionCommand::ApprovalTerminal {
                        run,
                        approval_id,
                        terminal: ApprovalTerminal::Cancelled,
                        reply,
                    }
                }).await?;
                if finalized {
                    Ok(ApprovalOutcome {
                        approved: false,
                        feedback: Some("cancelled".into()),
                    })
                } else {
                    receiver.await.map_err(|_| EngineError::ActorStopped)
                }
            },
            _ = tokio::time::sleep(expiry_wait) => {
                let finalized = self.request(active.session, |reply| {
                    SessionCommand::ApprovalTerminal {
                        run,
                        approval_id,
                        terminal: ApprovalTerminal::Expired,
                        reply,
                    }
                }).await?;
                if finalized {
                    Ok(ApprovalOutcome {
                        approved: false,
                        feedback: Some("approval expired unattended".into()),
                    })
                } else {
                    receiver.await.map_err(|_| EngineError::ActorStopped)
                }
            }
        }
    }
}

pub(super) fn approval_evaluations(request: &ApprovalRequest) -> Vec<ApprovalEvaluation> {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| value.get("evaluations").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .expect("protocol approval request serializes evaluations")
}

pub(super) fn approval_constraints(request: &ApprovalRequest) -> ApprovalConstraints {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| value.get("constraints").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .expect("protocol approval request serializes constraints")
}

pub(super) fn approval_request_for_operation(
    trigger: ApprovalTrigger,
    operation: PreparedOperationIdentity,
    traces: Vec<cookie_agent_protocol::DecisionTrace>,
    allow_tree_grant: bool,
    expires_at: Option<jiff::Timestamp>,
) -> ApprovalRequest {
    let evaluations = operation
        .resources()
        .iter()
        .zip(traces)
        .map(|(resource, trace)| ApprovalEvaluation {
            resource_digest: resource.binding_digest.clone(),
            effect: trace.effect,
            trace,
        })
        .collect::<Vec<_>>();
    ApprovalRequest::new(
        ApprovalId::new_v7(),
        1,
        trigger,
        operation,
        evaluations,
        ApprovalConstraints {
            allow_once: true,
            allow_tree_grant,
            cancellable: true,
            expires_at,
        },
    )
    .expect("prepared approval request is complete")
}

pub(super) fn approval_expiry(timeout_ms: u64) -> Option<jiff::Timestamp> {
    jiff::Timestamp::now()
        .checked_add(std::time::Duration::from_millis(timeout_ms))
        .ok()
}

pub(super) fn approval_deadline_exhausted(expires_at: Option<jiff::Timestamp>) -> bool {
    expires_at.is_some_and(|expires_at| expires_at <= jiff::Timestamp::now())
}

pub(super) fn approval_expiry_wait(expires_at: Option<jiff::Timestamp>) -> std::time::Duration {
    let Some(expires_at) = expires_at else {
        return std::time::Duration::from_secs(100 * 365 * 24 * 60 * 60);
    };
    let now = jiff::Timestamp::now();
    if expires_at <= now {
        std::time::Duration::ZERO
    } else {
        expires_at.duration_since(now).unsigned_abs()
    }
}
