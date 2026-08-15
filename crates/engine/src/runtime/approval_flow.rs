use cookie_agent_protocol::{
    ApprovalConstraints, ApprovalDecisionSource, ApprovalEvaluation, ApprovalFinalDecision,
    ApprovalFinalOutcome, ApprovalId, ApprovalInternalDecision, ApprovalInternalDecisionKind,
    ApprovalReasonCode, ApprovalRequest, ApprovalTrigger, PermissionMode,
    PreparedOperationIdentity, RunId, StoredEvent,
};
use serde_json::Value;

use super::{
    ActiveRun, ApprovalEvaluationTransition, ApprovalOutcome, ApprovalTerminal, ApprovalToolInput,
    Engine, EngineError, Event, FrozenInternalAgentPolicy, InternalAgentExecution,
    InternalAgentHistoryInput, ModelApprovalInput, SessionCommand,
    approval_projection::doom_loop_repetitions, helpers::root_id,
    internal_agents::parse_internal_approval,
};
use crate::tool_api::{PreparedExecutorCell, UNSCOPED_PERMISSION_RESOURCE_DISPLAY};
use cookie_agent_protocol::InternalAgentKind;

pub(super) const APPROVAL_USER_REQUEST_PREFIX: &str = "Evaluate only the current approval request. Return strict JSON only: {\"decision\":\"allow\"|\"deny\"|\"ask\"}.\n\n<latest_user_request>\n";
pub(super) const APPROVAL_USER_REQUEST_SUFFIX: &str = "\n</latest_user_request>";
pub(super) const APPROVAL_TOOL_CALL_PREFIX: &str = "\n\n<tool_call>\n";
pub(super) const APPROVAL_TOOL_CALL_SUFFIX: &str = "\n</tool_call>";
pub(super) const APPROVAL_NO_USER_MESSAGE: &str = "[no user message]";

impl Engine {
    pub(super) async fn request_model_approval(
        &self,
        active: &ActiveRun,
        run: RunId,
        input: ModelApprovalInput<'_>,
    ) -> Result<ApprovalOutcome, EngineError> {
        let approval_policy = self.active_internal_policy(active, InternalAgentKind::Approval)?;
        let request = approval_request_for_operation(
            ApprovalTrigger::ModelToolApproval,
            input.operation.clone(),
            input
                .operation
                .resources()
                .iter()
                .zip(input.policy_labels)
                .map(|(resource, label)| cookie_agent_protocol::DecisionTrace {
                    action: resource.capability,
                    normalized_resource: label
                        .clone()
                        .unwrap_or_else(|| UNSCOPED_PERMISSION_RESOURCE_DISPLAY.to_owned()),
                    candidates: Vec::new(),
                    effect: cookie_agent_protocol::PermissionEffect::Ask,
                    precedence_reason: input
                        .message
                        .clone()
                        .unwrap_or_else(|| "model requested tool approval".into()),
                })
                .collect(),
            false,
            approval_expiry(approval_policy.limits.timeout_ms),
        );
        self.await_user_approval(active, run, request, input.executor, false, input.tool)
            .await
    }

    pub(super) async fn await_user_approval(
        &self,
        active: &ActiveRun,
        run: RunId,
        request: ApprovalRequest,
        executor: PreparedExecutorCell,
        allow_prior_grant: bool,
        tool: ApprovalToolInput<'_>,
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
                self.evaluate_stateless_approval(active, run, &session, &approval_policy, tool)
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

    async fn evaluate_stateless_approval(
        &self,
        active: &ActiveRun,
        run: RunId,
        session: &crate::session::SessionProjection,
        policy: &FrozenInternalAgentPolicy,
        tool: ApprovalToolInput<'_>,
    ) -> ApprovalInternalDecisionKind {
        let events = session.log.events();
        let prompt = approval_stateless_input(tool, latest_user_message(&events, run));
        let history =
            approval_stateless_history(policy.agent.composed_prompt.clone(), prompt.clone());
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
        match result {
            Ok(result) => {
                parse_internal_approval(&result.text).unwrap_or(ApprovalInternalDecisionKind::Ask)
            }
            Err(_) => ApprovalInternalDecisionKind::Ask,
        }
    }
}

fn approval_stateless_input(tool: ApprovalToolInput<'_>, latest_user: Option<&str>) -> String {
    let tool_call = serde_json::json!({
        "name": tool.name,
        "normalized_parameters": canonical_approval_parameters(tool.normalized_parameters),
    });
    format!(
        "{APPROVAL_USER_REQUEST_PREFIX}{}{APPROVAL_USER_REQUEST_SUFFIX}{APPROVAL_TOOL_CALL_PREFIX}{}{APPROVAL_TOOL_CALL_SUFFIX}",
        latest_user.unwrap_or(APPROVAL_NO_USER_MESSAGE),
        serde_json::to_string(&tool_call).expect("safe approval tool call serializes")
    )
}

fn canonical_approval_parameters(value: &Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.iter().map(canonical_approval_parameters).collect())
        }
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), canonical_approval_parameters(&values[key])))
                    .collect(),
            )
        }
        _ => value.clone(),
    }
}

fn latest_user_message(events: &[StoredEvent], run: RunId) -> Option<&str> {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            Event::UserInputSubmitted { input } if event.run_id == Some(run) => {
                Some(input.as_str())
            }
            _ => None,
        })
        .or_else(|| {
            events.iter().rev().find_map(|event| match &event.payload {
                Event::UserInputSubmitted { input } => Some(input.as_str()),
                _ => None,
            })
        })
}

fn approval_stateless_history(system_prompt: String, input: String) -> Vec<oven_sdk::HistoryTurn> {
    vec![
        oven_sdk::HistoryTurn::system(oven_sdk::SystemMessage::new(vec![
            oven_sdk::SystemPart::Text(oven_sdk::TextPart::new(system_prompt)),
        ])),
        oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(vec![oven_sdk::InputPart::Text(
            oven_sdk::TextPart::new(input),
        )])),
    ]
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
    use oven_sdk::Request as ModelRequest;

    use super::{
        APPROVAL_NO_USER_MESSAGE, APPROVAL_TOOL_CALL_PREFIX, APPROVAL_TOOL_CALL_SUFFIX,
        APPROVAL_USER_REQUEST_PREFIX, APPROVAL_USER_REQUEST_SUFFIX, ApprovalToolInput,
        approval_stateless_history, approval_stateless_input,
    };

    #[test]
    fn approval_framing_string_is_frozen() {
        assert_eq!(
            APPROVAL_USER_REQUEST_PREFIX,
            "Evaluate only the current approval request. Return strict JSON only: {\"decision\":\"allow\"|\"deny\"|\"ask\"}.\n\n<latest_user_request>\n"
        );
        assert_eq!(APPROVAL_USER_REQUEST_SUFFIX, "\n</latest_user_request>");
        assert_eq!(APPROVAL_TOOL_CALL_PREFIX, "\n\n<tool_call>\n");
        assert_eq!(APPROVAL_TOOL_CALL_SUFFIX, "\n</tool_call>");
        assert_eq!(APPROVAL_NO_USER_MESSAGE, "[no user message]");
    }

    #[test]
    fn approval_request_prefix_is_stable_and_tool_parameters_are_last() {
        let (runtime, binding) = crate::test_support::model_runtime_and_binding();
        let model = runtime.resolve(&binding.selection).expect("resolved model");
        let first = approval_stateless_input(
            ApprovalToolInput {
                name: "write",
                normalized_parameters: &serde_json::json!({"filePath":"a"}),
            },
            Some("make the change"),
        );
        let second = approval_stateless_input(
            ApprovalToolInput {
                name: "write",
                normalized_parameters: &serde_json::json!({"filePath":"b"}),
            },
            Some("make the change"),
        );
        let prefix =
            format!("{APPROVAL_USER_REQUEST_PREFIX}make the change{APPROVAL_USER_REQUEST_SUFFIX}");
        assert!(first.starts_with(&prefix));
        assert!(second.starts_with(&prefix));
        assert_ne!(first, second);
        assert!(first.ends_with(APPROVAL_TOOL_CALL_SUFFIX));
        let first_request = model.prepare_request(ModelRequest::new(approval_stateless_history(
            "system".into(),
            first,
        )));
        let second_request = model.prepare_request(ModelRequest::new(approval_stateless_history(
            "system".into(),
            second,
        )));
        let first_serialized = serde_json::to_string(&first_request).unwrap();
        let second_serialized = serde_json::to_string(&second_request).unwrap();
        let first_params = first_serialized
            .find("\\\"normalized_parameters\\\":{\\\"filePath\\\":\\\"a\\\"}")
            .expect("first prepared params tail");
        let second_params = second_serialized
            .find("\\\"normalized_parameters\\\":{\\\"filePath\\\":\\\"b\\\"}")
            .expect("second prepared params tail");
        assert_eq!(first_params, second_params);
        assert_eq!(
            &first_serialized[..first_params],
            &second_serialized[..second_params]
        );
        assert_ne!(
            first_serialized.as_bytes()[first_params..],
            second_serialized.as_bytes()[second_params..]
        );
    }

    #[test]
    fn stateless_approval_does_not_accumulate_prior_decisions() {
        let make = || {
            approval_stateless_history(
                "system".into(),
                approval_stateless_input(
                    ApprovalToolInput {
                        name: "write",
                        normalized_parameters: &serde_json::json!({"filePath":"same"}),
                    },
                    Some("same user request"),
                ),
            )
        };
        let first = make();
        for _ in 0..5 {
            assert_eq!(
                serde_json::to_vec(&make()).unwrap(),
                serde_json::to_vec(&first).unwrap()
            );
        }
    }

    #[test]
    fn approval_without_user_message_uses_fixed_fallback() {
        let input = approval_stateless_input(
            ApprovalToolInput {
                name: "delegate_subagent",
                normalized_parameters: &serde_json::json!({"agent_type":"worker"}),
            },
            None,
        );
        assert!(input.contains(&format!(
            "{APPROVAL_USER_REQUEST_PREFIX}{APPROVAL_NO_USER_MESSAGE}{APPROVAL_USER_REQUEST_SUFFIX}"
        )));
    }

    #[test]
    fn approval_parameters_are_canonicalized_recursively() {
        let first = approval_stateless_input(
            ApprovalToolInput {
                name: "write",
                normalized_parameters: &serde_json::json!({"z":1,"nested":{"b":2,"a":1}}),
            },
            Some("request"),
        );
        let second = approval_stateless_input(
            ApprovalToolInput {
                name: "write",
                normalized_parameters: &serde_json::json!({"nested":{"a":1,"b":2},"z":1}),
            },
            Some("request"),
        );
        assert_eq!(first, second);
    }
}
