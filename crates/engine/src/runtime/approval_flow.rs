use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{
    ApprovalConstraints, ApprovalDecisionSource, ApprovalEvaluation, ApprovalFinalDecision,
    ApprovalFinalOutcome, ApprovalId, ApprovalInternalDecision, ApprovalInternalDecisionKind,
    ApprovalReasonCode, ApprovalRequest, ApprovalTrigger, PermissionMode,
    PreparedOperationIdentity, RunId, SessionId, StoredEvent,
};
use serde_json::Value;
use tokio::sync::oneshot;

use super::{
    ActiveRun, Engine, EngineError, Event, FrozenInternalAgentPolicy, InternalAgentExecution,
    InternalAgentHistoryInput,
    approval_projection::doom_loop_repetitions,
    helpers::{root_id, truncate_utf8},
    internal_agents::parse_internal_approval,
};
use crate::permissions::ApprovalStore;
use crate::tool_api::{PreparedExecutorCell, UNSCOPED_PERMISSION_RESOURCE_DISPLAY};
use cookie_agent_protocol::InternalAgentKind;

pub(super) const APPROVAL_USER_REQUEST_PREFIX: &str = "Evaluate only the current approval request. Return strict JSON only: {\"decision\":\"allow\"|\"deny\"|\"ask\"}.\n\n<latest_user_request>\n";
pub(super) const APPROVAL_USER_REQUEST_SUFFIX: &str = "\n</latest_user_request>";
pub(super) const APPROVAL_PRIOR_DECISIONS_PREFIX: &str = "\n\n<prior_decisions>\nMost recent finalized approvals for the same permission action in this session tree, oldest first. User decisions show the user's intent; approval reviewer decisions are earlier automated verdicts and may be wrong.\n";
pub(super) const APPROVAL_PRIOR_DECISIONS_SUFFIX: &str = "</prior_decisions>";
pub(super) const APPROVAL_NO_PRIOR_DECISIONS: &str = "[none]\n";
pub(super) const APPROVAL_TOOL_CALL_PREFIX: &str = "\n\n<tool_call>\n";
pub(super) const APPROVAL_TOOL_CALL_SUFFIX: &str = "\n</tool_call>";
pub(super) const APPROVAL_NO_USER_MESSAGE: &str = "[no user message]";
/// Prior decisions shown to the approval reviewer, newest kept.
const APPROVAL_PRIOR_DECISION_LIMIT: usize = 5;
/// Byte cap for each resource or feedback string in a prior-decision line.
const APPROVAL_PRIOR_DECISION_TEXT_MAX: usize = 256;

#[derive(Clone, Debug)]
pub(crate) struct ApprovalOutcome {
    pub(crate) approved: bool,
    pub(crate) feedback: Option<String>,
}

pub(crate) struct PendingApproval {
    pub(crate) sender: oneshot::Sender<ApprovalOutcome>,
    pub(crate) executor: PreparedExecutorCell,
    pub(crate) permission_overlay_epoch: u64,
}

#[derive(Clone, Copy)]
pub(crate) enum PreparedApprovalInvalidation {
    OperationChanged,
    PreparedCapabilityLost,
}

#[derive(Clone, Copy)]
pub(crate) enum ApprovalTerminal {
    Cancelled,
    Expired,
}

pub(crate) enum ApprovalEvaluationTransition {
    Resolved(ApprovalOutcome),
    Escalated(oneshot::Receiver<ApprovalOutcome>),
}

pub(crate) struct ApprovalToolInput<'a> {
    pub(crate) name: &'a str,
    pub(crate) normalized_parameters: &'a Value,
}

/// One finalized approval the reviewer sees as context for a new request.
#[derive(Debug, Eq, PartialEq)]
struct PriorApprovalDecision {
    timestamp: jiff::Timestamp,
    operations: Vec<String>,
    resources: Vec<String>,
    approved: bool,
    source: ApprovalDecisionSource,
    feedback: Option<String>,
}

pub(crate) struct ModelApprovalInput<'a> {
    pub(crate) operation: &'a PreparedOperationIdentity,
    pub(crate) policy_labels: &'a [Option<String>],
    pub(crate) executor: PreparedExecutorCell,
    pub(crate) message: Option<String>,
    pub(crate) tool: ApprovalToolInput<'a>,
}

/// Approval runtime state owned by [`super::Inner`].
#[derive(Default)]
pub(crate) struct ApprovalRuntimeState {
    pub(crate) store: ApprovalStore,
    pub(crate) pending: Mutex<HashMap<(SessionId, ApprovalId), PendingApproval>>,
    /// Runtime-only permission modes keyed by delegation-tree root.
    pub(crate) permission_modes: Mutex<HashMap<SessionId, PermissionMode>>,
    pub(crate) permission_overlay_mutation: tokio::sync::Mutex<()>,
}

impl Engine {
    pub(super) async fn request_model_approval(
        &self,
        active: &ActiveRun,
        run: RunId,
        input: ModelApprovalInput<'_>,
    ) -> Result<ApprovalOutcome, EngineError> {
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
            // The user-facing approval window comes from `[approval].timeout_ms`.
            // The internal approval agent's own `limits.timeout_ms` is only the
            // classifier model-call budget (see `run_internal_history_agent`).
            approval_expiry(self.inner.config.runtime.approval.timeout_ms),
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
            super::event_origin("engine:approvals"),
            Event::ApprovalRequested {
                request: request.clone(),
            },
        )
        .await?;

        let repetitions = doom_loop_repetitions(
            &self.inner.store.log(active.session)?.event_snapshot(),
            run,
            request.operation_fingerprint(),
        );
        if repetitions >= 4 {
            self.append(
                active.session,
                Some(run),
                super::event_origin("engine:approvals"),
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
                super::event_origin("engine:approvals"),
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
            && let Some(grant) = self
                .inner
                .approvals
                .store
                .matching(root, request.operation())
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
                super::event_origin("engine:approvals"),
                Event::ApprovalEvaluated {
                    approval_id,
                    decision,
                },
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                super::event_origin("engine:approvals"),
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
                super::event_origin("engine:approvals"),
                Event::ApprovalEvaluated {
                    approval_id,
                    decision,
                },
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                super::event_origin("engine:approvals"),
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
            .approvals
            .permission_modes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&root)
            .copied()
            .unwrap_or_default();
        if permission_mode == PermissionMode::Yolo {
            self.append(
                active.session,
                Some(run),
                super::event_origin("engine:approvals"),
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
                super::event_origin("engine:approvals"),
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
            PermissionMode::AutoApprove
            | PermissionMode::AutoApproveN
            | PermissionMode::AutoApproveY => {
                #[cfg(test)]
                let hook = {
                    self.inner
                        .test_hooks
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
                self.evaluate_stateless_approval(
                    active,
                    run,
                    &session,
                    &approval_policy,
                    &request,
                    tool,
                )
                .await
            }
            PermissionMode::Yolo => unreachable!("yolo approvals resolve before prompting"),
        };
        let session = active.session;
        let cancelled = active.cancellation.is_cancelled();
        let evaluated = (request.clone(), executor.clone());
        let transition = self
            .on_actor(session, move |engine| {
                let (request, executor) = evaluated;
                engine.approval_evaluation_complete_direct(
                    session,
                    run,
                    request,
                    executor,
                    (permission_mode, internal_kind),
                    cancelled,
                )
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
                let finalized = self.on_actor(session, move |engine| {
                    engine.approval_terminal_direct(session, run, approval_id, ApprovalTerminal::Cancelled)
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
                let finalized = self.on_actor(session, move |engine| {
                    engine.approval_terminal_direct(session, run, approval_id, ApprovalTerminal::Expired)
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
        request: &ApprovalRequest,
        tool: ApprovalToolInput<'_>,
    ) -> ApprovalInternalDecisionKind {
        let events = session.log.event_snapshot();
        let actions = request
            .operation()
            .capabilities()
            .iter()
            .map(|capability| capability.action)
            .collect::<Vec<_>>();
        let tree_events = self
            .inner
            .store
            .resident_tree_logs(root_id(&session.meta.origin, active.session))
            .iter()
            .map(|log| log.event_snapshot())
            .collect::<Vec<_>>();
        let prior =
            prior_approval_decisions(tree_events.iter().map(|events| events.as_slice()), &actions);
        let prompt = approval_stateless_input(tool, latest_user_message(&events, run), &prior);
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

fn approval_stateless_input(
    tool: ApprovalToolInput<'_>,
    latest_user: Option<&str>,
    prior: &[PriorApprovalDecision],
) -> String {
    let tool_call = serde_json::json!({
        "name": tool.name,
        "normalized_parameters": canonical_approval_parameters(tool.normalized_parameters),
    });
    format!(
        "{APPROVAL_USER_REQUEST_PREFIX}{}{APPROVAL_USER_REQUEST_SUFFIX}{APPROVAL_PRIOR_DECISIONS_PREFIX}{}{APPROVAL_PRIOR_DECISIONS_SUFFIX}{APPROVAL_TOOL_CALL_PREFIX}{}{APPROVAL_TOOL_CALL_SUFFIX}",
        latest_user.unwrap_or(APPROVAL_NO_USER_MESSAGE),
        render_prior_decisions(prior),
        serde_json::to_string(&tool_call).expect("safe approval tool call serializes")
    )
}

fn render_prior_decisions(prior: &[PriorApprovalDecision]) -> String {
    if prior.is_empty() {
        return APPROVAL_NO_PRIOR_DECISIONS.to_owned();
    }
    prior
        .iter()
        .map(|decision| {
            let outcome = if decision.approved {
                "approved"
            } else {
                "rejected"
            };
            let decider = match decision.source {
                ApprovalDecisionSource::User => "user",
                ApprovalDecisionSource::TreeGrant => "user (approve all)",
                _ => "approval reviewer",
            };
            let feedback = decision
                .feedback
                .as_deref()
                .map(|feedback| format!(" with feedback {feedback:?}"))
                .unwrap_or_default();
            format!(
                "- {} {}: {outcome} by {decider}{feedback}\n",
                decision.operations.join(","),
                decision.resources.join(", "),
            )
        })
        .collect()
}

/// The newest [`APPROVAL_PRIOR_DECISION_LIMIT`] approvals across `logs` that
/// share a permission action with the current request and were decided by the
/// user, a user tree grant, or the approval reviewer. Oldest first.
fn prior_approval_decisions<'a>(
    logs: impl IntoIterator<Item = &'a [Arc<StoredEvent>]>,
    actions: &[cookie_agent_protocol::PermissionAction],
) -> Vec<PriorApprovalDecision> {
    let mut prior = Vec::new();
    for events in logs {
        // Finalization follows its request, so a reverse scan meets it first.
        let mut finalized = HashMap::new();
        let mut found = 0;
        for event in events.iter().rev() {
            match &event.payload {
                Event::ApprovalFinalized {
                    approval_id,
                    decision,
                } if matches!(
                    decision.source,
                    ApprovalDecisionSource::User
                        | ApprovalDecisionSource::TreeGrant
                        | ApprovalDecisionSource::InternalAgent
                ) && matches!(
                    decision.outcome,
                    ApprovalFinalOutcome::Approved | ApprovalFinalOutcome::Rejected
                ) =>
                {
                    finalized.insert(*approval_id, decision);
                }
                Event::ApprovalRequested { request } => {
                    let Some(decision) = finalized.remove(&request.approval_id()) else {
                        continue;
                    };
                    let capabilities = request.operation().capabilities();
                    if !capabilities
                        .iter()
                        .any(|capability| actions.contains(&capability.action))
                    {
                        continue;
                    }
                    prior.push(PriorApprovalDecision {
                        timestamp: event.timestamp,
                        operations: capabilities
                            .iter()
                            .map(|capability| capability.operation.as_str().to_owned())
                            .collect(),
                        resources: request
                            .evaluations()
                            .iter()
                            .map(|evaluation| {
                                truncate_utf8(
                                    &evaluation.trace.normalized_resource,
                                    APPROVAL_PRIOR_DECISION_TEXT_MAX,
                                )
                            })
                            .collect(),
                        approved: decision.outcome == ApprovalFinalOutcome::Approved,
                        source: decision.source,
                        feedback: decision.feedback.as_ref().map(|feedback| {
                            truncate_utf8(
                                feedback.message.as_str(),
                                APPROVAL_PRIOR_DECISION_TEXT_MAX,
                            )
                        }),
                    });
                    found += 1;
                    if found == APPROVAL_PRIOR_DECISION_LIMIT {
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    prior.sort_by_key(|decision| decision.timestamp);
    let excess = prior.len().saturating_sub(APPROVAL_PRIOR_DECISION_LIMIT);
    prior.drain(..excess);
    prior
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

fn latest_user_message(events: &[Arc<StoredEvent>], run: RunId) -> Option<&str> {
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
    request.evaluations().to_vec()
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

    use std::sync::Arc;

    use cookie_agent_protocol::{
        ApprovalBoundary, ApprovalCapability, ApprovalConstraints, ApprovalDecisionSource,
        ApprovalEvaluation, ApprovalFeedback, ApprovalFinalDecision, ApprovalFinalOutcome,
        ApprovalId, ApprovalReasonCode, ApprovalRequest, ApprovalResourceSource, ApprovalTrigger,
        DecisionTrace, PermissionAction, PermissionEffect, PreparedApprovalResource,
        PreparedBindingLifetime, PreparedCapabilityOperation, PreparedOperationIdentity,
        PreparedResourceDigest, PreparedResourceIdentity, SafeErrorMessage, SessionId,
        Sha256Digest, StoredEvent,
    };

    use super::{
        APPROVAL_NO_PRIOR_DECISIONS, APPROVAL_NO_USER_MESSAGE, APPROVAL_PRIOR_DECISIONS_PREFIX,
        APPROVAL_PRIOR_DECISIONS_SUFFIX, APPROVAL_TOOL_CALL_PREFIX, APPROVAL_TOOL_CALL_SUFFIX,
        APPROVAL_USER_REQUEST_PREFIX, APPROVAL_USER_REQUEST_SUFFIX, ApprovalToolInput, Event,
        PriorApprovalDecision, approval_stateless_history, approval_stateless_input,
        prior_approval_decisions, render_prior_decisions,
    };

    fn stored(seq: u64, payload: Event) -> Arc<StoredEvent> {
        Arc::new(StoredEvent {
            engine_version: None,
            origin: None,
            session_id: SessionId::new_v7(),
            run_id: None,
            seq,
            timestamp: jiff::Timestamp::from_second(i64::try_from(seq).unwrap()).unwrap(),
            payload,
        })
    }

    fn requested(
        seq: u64,
        approval_id: ApprovalId,
        action: PermissionAction,
        resource: &str,
    ) -> Arc<StoredEvent> {
        let binding_digest =
            PreparedResourceDigest::from_canonical_binding_bytes(resource.as_bytes());
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(resource.as_bytes()),
            vec![ApprovalCapability {
                action,
                operation: PreparedCapabilityOperation::new("op:test").unwrap(),
            }],
            vec![PreparedApprovalResource {
                capability: action,
                canonical: PreparedResourceIdentity::new("file:test").unwrap(),
                binding_digest: binding_digest.clone(),
                binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"context"),
        )
        .unwrap();
        let request = ApprovalRequest::new(
            approval_id,
            1,
            ApprovalTrigger::PermissionPolicy,
            operation,
            vec![ApprovalEvaluation {
                resource_digest: binding_digest,
                effect: PermissionEffect::Ask,
                trace: DecisionTrace {
                    action,
                    normalized_resource: resource.to_owned(),
                    candidates: Vec::new(),
                    effect: PermissionEffect::Ask,
                    precedence_reason: "test".to_owned(),
                },
            }],
            ApprovalConstraints {
                allow_once: true,
                allow_tree_grant: false,
                cancellable: true,
                expires_at: None,
            },
        )
        .unwrap();
        stored(seq, Event::ApprovalRequested { request })
    }

    fn finalized(
        seq: u64,
        approval_id: ApprovalId,
        outcome: ApprovalFinalOutcome,
        source: ApprovalDecisionSource,
        feedback: Option<&str>,
    ) -> Arc<StoredEvent> {
        stored(
            seq,
            Event::ApprovalFinalized {
                approval_id,
                decision: ApprovalFinalDecision {
                    outcome,
                    source,
                    reason_code: ApprovalReasonCode::InternalAgentAllowed,
                    feedback: feedback.map(|message| ApprovalFeedback {
                        message: SafeErrorMessage::new(message).unwrap(),
                    }),
                    tree_grant_id: None,
                },
            },
        )
    }

    fn decided(
        seq: u64,
        action: PermissionAction,
        resource: &str,
        outcome: ApprovalFinalOutcome,
        source: ApprovalDecisionSource,
    ) -> [Arc<StoredEvent>; 2] {
        let id = ApprovalId::new_v7();
        [
            requested(seq, id, action, resource),
            finalized(seq + 1, id, outcome, source, None),
        ]
    }

    #[test]
    fn prior_decisions_keep_the_newest_five_of_the_same_action_across_logs() {
        let mut root = Vec::new();
        for index in 0..4 {
            root.extend(decided(
                10 + index * 10,
                PermissionAction::Write,
                &format!("/root/{index}"),
                ApprovalFinalOutcome::Approved,
                ApprovalDecisionSource::InternalAgent,
            ));
        }
        root.extend(decided(
            100,
            PermissionAction::Bash,
            "git push",
            ApprovalFinalOutcome::Approved,
            ApprovalDecisionSource::User,
        ));
        let mut child = Vec::new();
        child.extend(decided(
            15,
            PermissionAction::Write,
            "/child/early",
            ApprovalFinalOutcome::Rejected,
            ApprovalDecisionSource::User,
        ));
        child.extend(decided(
            55,
            PermissionAction::Write,
            "/child/late",
            ApprovalFinalOutcome::Rejected,
            ApprovalDecisionSource::InternalAgent,
        ));

        let prior = prior_approval_decisions(
            [root.as_slice(), child.as_slice()],
            &[PermissionAction::Write],
        );

        let resources = prior
            .iter()
            .map(|decision| decision.resources[0].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            resources,
            [
                "/child/early",
                "/root/1",
                "/root/2",
                "/root/3",
                "/child/late"
            ]
        );
    }

    #[test]
    fn prior_decisions_skip_policy_cancelled_and_pending_approvals() {
        let pending = ApprovalId::new_v7();
        let mut events = Vec::new();
        events.extend(decided(
            10,
            PermissionAction::Write,
            "/policy",
            ApprovalFinalOutcome::Rejected,
            ApprovalDecisionSource::Policy,
        ));
        events.extend(decided(
            20,
            PermissionAction::Write,
            "/cancelled",
            ApprovalFinalOutcome::Cancelled,
            ApprovalDecisionSource::User,
        ));
        events.extend(decided(
            30,
            PermissionAction::Write,
            "/kept",
            ApprovalFinalOutcome::Approved,
            ApprovalDecisionSource::TreeGrant,
        ));
        events.push(requested(40, pending, PermissionAction::Write, "/pending"));

        let prior = prior_approval_decisions([events.as_slice()], &[PermissionAction::Write]);

        assert_eq!(prior.len(), 1);
        assert_eq!(prior[0].resources, ["/kept"]);
    }

    #[test]
    fn prior_decisions_carry_user_rejection_feedback() {
        let id = ApprovalId::new_v7();
        let events = [
            requested(10, id, PermissionAction::Write, "/etc/hosts"),
            finalized(
                11,
                id,
                ApprovalFinalOutcome::Rejected,
                ApprovalDecisionSource::User,
                Some("never touch system files"),
            ),
        ];

        let prior = prior_approval_decisions([events.as_slice()], &[PermissionAction::Write]);

        assert_eq!(
            render_prior_decisions(&prior),
            "- op:test /etc/hosts: rejected by user with feedback \"never touch system files\"\n"
        );
    }

    #[test]
    fn prior_decisions_render_each_decider_and_none_when_empty() {
        let decision = |approved, source| PriorApprovalDecision {
            timestamp: jiff::Timestamp::UNIX_EPOCH,
            operations: vec!["write:replace".to_owned()],
            resources: vec!["/tmp/a.py".to_owned()],
            approved,
            source,
            feedback: None,
        };
        assert_eq!(render_prior_decisions(&[]), APPROVAL_NO_PRIOR_DECISIONS);
        assert_eq!(
            render_prior_decisions(&[
                decision(true, ApprovalDecisionSource::InternalAgent),
                decision(false, ApprovalDecisionSource::User),
                decision(true, ApprovalDecisionSource::TreeGrant),
            ]),
            "- write:replace /tmp/a.py: approved by approval reviewer\n\
             - write:replace /tmp/a.py: rejected by user\n\
             - write:replace /tmp/a.py: approved by user (approve all)\n"
        );
    }

    #[test]
    fn approval_framing_string_is_frozen() {
        assert_eq!(
            APPROVAL_USER_REQUEST_PREFIX,
            "Evaluate only the current approval request. Return strict JSON only: {\"decision\":\"allow\"|\"deny\"|\"ask\"}.\n\n<latest_user_request>\n"
        );
        assert_eq!(APPROVAL_USER_REQUEST_SUFFIX, "\n</latest_user_request>");
        assert_eq!(
            APPROVAL_PRIOR_DECISIONS_PREFIX,
            "\n\n<prior_decisions>\nMost recent finalized approvals for the same permission action in this session tree, oldest first. User decisions show the user's intent; approval reviewer decisions are earlier automated verdicts and may be wrong.\n"
        );
        assert_eq!(APPROVAL_PRIOR_DECISIONS_SUFFIX, "</prior_decisions>");
        assert_eq!(APPROVAL_NO_PRIOR_DECISIONS, "[none]\n");
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
            &[],
        );
        let second = approval_stateless_input(
            ApprovalToolInput {
                name: "write",
                normalized_parameters: &serde_json::json!({"filePath":"b"}),
            },
            Some("make the change"),
            &[],
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
    fn approval_input_is_deterministic_for_identical_context() {
        let make = || {
            approval_stateless_history(
                "system".into(),
                approval_stateless_input(
                    ApprovalToolInput {
                        name: "write",
                        normalized_parameters: &serde_json::json!({"filePath":"same"}),
                    },
                    Some("same user request"),
                    &[],
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
            &[],
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
            &[],
        );
        let second = approval_stateless_input(
            ApprovalToolInput {
                name: "write",
                normalized_parameters: &serde_json::json!({"nested":{"a":1,"b":2},"z":1}),
            },
            Some("request"),
            &[],
        );
        assert_eq!(first, second);
    }

    #[test]
    fn approval_expiry_uses_the_requested_window() {
        let before = jiff::Timestamp::now();
        let expires_at = super::approval_expiry(120_000).expect("expiry");
        let remaining = expires_at.duration_since(before).unsigned_abs();
        assert!(remaining >= std::time::Duration::from_secs(119));
        assert!(remaining <= std::time::Duration::from_secs(121));
    }

    #[test]
    fn approval_expiry_wait_tracks_the_remaining_window() {
        let wait = super::approval_expiry_wait(super::approval_expiry(120_000));
        assert!(wait >= std::time::Duration::from_secs(119));
        assert!(wait <= std::time::Duration::from_secs(121));
    }
}
