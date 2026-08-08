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
        let approval_policy = self.active_internal_policy(active, InternalAgentKind::Approval)?;
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
                    approval_session_increment_count: 0,
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
                    approval_session_increment_count: 0,
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
                    approval_session_increment_count: 0,
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

        let (internal_kind, approval_session_increment_count) = match permission_mode {
            PermissionMode::Ask => (ApprovalInternalDecisionKind::Ask, 0),
            PermissionMode::AutoApprove => {
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
                    self.active_internal_policy(active, InternalAgentKind::Approval)?;
                self.evaluate_with_approval_conversation(
                    active,
                    run,
                    &session,
                    &request,
                    &approval_policy,
                )
                .await
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
                    approval_session_increment_count,
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

    async fn evaluate_with_approval_conversation(
        &self,
        active: &ActiveRun,
        run: RunId,
        session: &crate::session::SessionProjection,
        request: &ApprovalRequest,
        policy: &FrozenInternalAgentPolicy,
    ) -> (ApprovalInternalDecisionKind, u64) {
        let conversation = {
            let mut conversations = self
                .inner
                .approval_conversations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(conversations.entry(active.session).or_insert_with(|| {
                Arc::new(tokio::sync::Mutex::new(ApprovalConversation::default()))
            }))
        };
        let mut conversation = tokio::select! {
            conversation = conversation.lock() => conversation,
            _ = active.cancellation.cancelled() => {
                return (ApprovalInternalDecisionKind::Ask, 0);
            }
        };
        let events = session.log.events();
        let input_through_seq = events.last().map_or(0, |event| event.seq);
        let prompt = approval_conversation_increment(
            session,
            request,
            conversation.increment_count == 0,
            conversation.input_through_seq,
            &events,
        );
        let max_input_bytes = usize::try_from(policy.limits.max_input_tokens)
            .unwrap_or(usize::MAX)
            .saturating_mul(4);
        let prompt = truncate_utf8(&prompt, max_input_bytes);
        let increment_count = conversation.push_user(prompt.clone(), input_through_seq);
        let system_prompt = format!(
            "{}\nReturn strict JSON only: {{\"decision\":\"allow\"|\"deny\"|\"ask\"}}.",
            policy.agent.composed_prompt
        );
        let history = loop {
            let history = conversation.history(system_prompt.clone());
            let within_limit = internal_history_tokens(&history, &[])
                .is_ok_and(|tokens| tokens <= policy.limits.max_input_tokens);
            if within_limit {
                break history;
            }
            if !conversation.trim_oldest_increment() {
                return (ApprovalInternalDecisionKind::Ask, increment_count);
            }
        };
        let result = self
            .run_internal_history_agent(
                active.session,
                Some(run),
                InternalAgentKind::Approval,
                policy,
                InternalAgentHistoryInput {
                    history,
                    summary_source: prompt,
                    tools: Vec::new(),
                    reject_non_text: true,
                },
                InternalAgentExecution {
                    cancellation: &active.cancellation,
                    actor_direct: false,
                },
            )
            .await;
        let decision = match result {
            Ok(result) => {
                conversation.set_latest_assistant(result.text.clone());
                parse_internal_approval(&result.text).unwrap_or(ApprovalInternalDecisionKind::Ask)
            }
            Err(_) => ApprovalInternalDecisionKind::Ask,
        };
        (decision, increment_count)
    }

    #[cfg(test)]
    pub(crate) async fn approval_conversation_snapshot(
        &self,
        session: SessionId,
    ) -> Option<Vec<(String, Option<String>)>> {
        let conversation = {
            let conversations = self
                .inner
                .approval_conversations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(conversations.get(&session)?)
        };
        let conversation = conversation.lock().await;
        Some(
            conversation
                .increments
                .iter()
                .map(|increment| (increment.user.clone(), increment.assistant.clone()))
                .collect(),
        )
    }
}

fn approval_conversation_increment(
    session: &crate::session::SessionProjection,
    request: &ApprovalRequest,
    first: bool,
    after_seq: u64,
    events: &[StoredEvent],
) -> String {
    let resource_labels = approval_evaluations(request)
        .iter()
        .map(|evaluation| evaluation.trace.normalized_resource.clone())
        .collect::<Vec<_>>();
    let operations = request
        .operation()
        .capabilities()
        .iter()
        .map(|capability| capability.operation.as_str())
        .collect::<Vec<_>>();
    let value = if first {
        serde_json::json!({
            "instruction": "Return strict JSON only: {\"decision\":\"allow\"|\"deny\"|\"ask\"}.",
            "cwd_identity": session.meta.cwd_identity,
            "operations": operations,
            "resource_labels": resource_labels,
        })
    } else {
        let source = if matches!(session.meta.origin, SessionOrigin::Delegated { .. }) {
            "delegate"
        } else {
            "user"
        };
        let intervening_messages = events
            .iter()
            .filter(|event| event.seq > after_seq)
            .filter_map(|event| match &event.payload {
                Event::UserInputSubmitted { input } => Some(serde_json::json!({
                    "source": source,
                    "content": input,
                })),
                _ => None,
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "intervening_messages": intervening_messages,
            "operations": operations,
            "resource_labels": resource_labels,
        })
    };
    serde_json::to_string(&value).expect("safe approval increment serializes")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_conversation_retains_twenty_increments_and_counts_omitted_messages() {
        let mut conversation = ApprovalConversation::default();
        for index in 1..=22 {
            assert_eq!(
                conversation.push_user(format!("request {index}"), index),
                index
            );
            conversation.set_latest_assistant(format!("decision {index}"));
        }

        assert_eq!(conversation.increment_count, 22);
        assert_eq!(conversation.increments.len(), 20);
        assert_eq!(conversation.omitted_messages, 4);
        assert_eq!(conversation.increments.front().unwrap().user, "request 3");
        assert_eq!(conversation.history("system".into()).len(), 42);
    }

    #[test]
    fn approval_history_can_trim_below_the_twenty_increment_cap() {
        let mut conversation = ApprovalConversation::default();
        for index in 1..=3 {
            conversation.push_user(format!("request {index}"), index);
            conversation.set_latest_assistant(format!("decision {index}"));
        }
        assert!(conversation.trim_oldest_increment());
        assert_eq!(conversation.increments.len(), 2);
        assert_eq!(conversation.omitted_messages, 2);
        assert_eq!(conversation.increments.front().unwrap().user, "request 2");
    }
}
