use std::{borrow::Cow, collections::HashMap, sync::Mutex};

use cookie_agent_protocol::{
    ApprovalConstraints, ApprovalDecisionSource, ApprovalEvaluation, ApprovalFinalDecision,
    ApprovalFinalOutcome, ApprovalId, ApprovalInternalDecision, ApprovalInternalDecisionKind,
    ApprovalReasonCode, ApprovalRequest, ApprovalTrigger, PermissionMode,
    PreparedOperationIdentity, RunId, SessionId, SessionOrigin,
};
use tokio::sync::oneshot;

use crate::permissions::ApprovalStore;
use crate::permissions::UNSCOPED_PERMISSION_RESOURCE_DISPLAY;
use crate::runtime::{
    ActiveRun, Engine, EngineError, Event, FrozenInternalAgentPolicy, InternalAgentExecution,
    InternalAgentHistoryInput,
    approval::context as approval_context,
    approval::projection::doom_loop_repetitions,
    helpers::root_id,
    internal_agents::{expand_internal_prompt, internal_agent_output_limit},
};
use cookie_agent_protocol::InternalAgentKind;
use cookie_agent_runtime_api::PreparedExecutorCell;

#[derive(Clone, Debug)]
pub(crate) struct ApprovalOutcome {
    pub(crate) approved: bool,
    pub(crate) feedback: Option<String>,
    /// Set when the approval reviewer itself denied the call.
    pub(crate) reviewer_denial: Option<ReviewerDenial>,
}

/// The approval reviewer's denial of a call: the agent is told the reviewer
/// refused it, and why (approval reviewer spec section 7).
#[derive(Clone, Debug)]
pub(crate) struct ReviewerDenial {
    pub(crate) reason: Option<String>,
}

impl ApprovalOutcome {
    /// The tool failure the agent receives for this refused outcome.
    pub(crate) fn denial_message(self, source: &str, reason: &str) -> String {
        match self.reviewer_denial {
            Some(denial) => super::projection::denied_tool_failure(
                REVIEWER_DENIAL_SOURCE,
                denial
                    .reason
                    .unwrap_or_else(|| REVIEWER_DENIAL_REASON.to_owned()),
                None,
            ),
            None => super::projection::denied_tool_failure(source, reason, self.feedback),
        }
    }
}

/// The `source` of a tool denial the approval reviewer decided.
pub(crate) const REVIEWER_DENIAL_SOURCE: &str = "approval_reviewer";
/// Its `reason` when the reviewer gave none.
pub(crate) const REVIEWER_DENIAL_REASON: &str = "the approval reviewer denied this call";

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

/// What the approval reviewer made of a call, or that it never ran.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReviewOutcome {
    /// The permission mode sends the call to the user without a review.
    Unreviewed,
    /// The reviewer answered `allow`, `deny`, or `ask`.
    Verdict {
        decision: ApprovalInternalDecisionKind,
        reason: Option<String>,
    },
    /// The reviewer failed: an error, a timeout, an unparseable reply, or
    /// no model. No permission mode approves such a call for the user.
    Failed,
}

pub(crate) enum ApprovalEvaluationTransition {
    Resolved(ApprovalOutcome),
    /// The call waits for the user until `expires_at`.
    Escalated {
        receiver: oneshot::Receiver<ApprovalOutcome>,
        expires_at: Option<jiff::Timestamp>,
    },
}

/// The call an approval is for. The reviewer reads the rest of it from the
/// log (approval reviewer spec section 4.6).
pub(crate) struct ApprovalToolInput<'a> {
    pub(crate) tool_call_id: cookie_agent_protocol::ToolCallId,
    /// Layer or runtime that owns the tool: the request and any tree grant
    /// made from it belong to that runtime.
    pub(crate) origin: Option<&'a crate::ToolOrigin>,
}

pub(crate) struct ModelApprovalInput<'a> {
    pub(crate) operation: &'a PreparedOperationIdentity,
    pub(crate) policy_labels: &'a [Option<String>],
    pub(crate) executor: PreparedExecutorCell,
    pub(crate) message: Option<String>,
    pub(crate) tool: ApprovalToolInput<'a>,
}

/// Approval runtime state owned by [`crate::runtime::Inner`].
#[derive(Default)]
pub(crate) struct ApprovalRuntimeState {
    pub(crate) store: ApprovalStore,
    pub(crate) pending: Mutex<HashMap<(SessionId, ApprovalId), PendingApproval>>,
    pub(crate) permission_overlay_mutation: tokio::sync::Mutex<()>,
}

impl Engine {
    pub(in crate::runtime) async fn request_model_approval(
        &self,
        active: &ActiveRun,
        run: RunId,
        input: ModelApprovalInput<'_>,
    ) -> Result<ApprovalOutcome, EngineError> {
        let request = approval_request_for_operation(
            input.tool.tool_call_id,
            ApprovalTrigger::ModelToolApproval,
            input.operation.clone(),
            input
                .operation
                .resources()
                .iter()
                .zip(input.policy_labels)
                .map(|(resource, label)| cookie_agent_protocol::DecisionTrace {
                    action: resource.capability.clone(),
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
            // The user's window, `[approval].timeout_ms`, starts when the
            // call escalates to them; the reviewer's own `limits.timeout_ms`
            // bounds only its model call (see `run_internal_history_agent`).
            None,
        );
        self.await_user_approval(active, run, request, input.executor, false, input.tool)
            .await
    }

    pub(in crate::runtime) async fn await_user_approval(
        &self,
        active: &ActiveRun,
        run: RunId,
        request: ApprovalRequest,
        executor: PreparedExecutorCell,
        allow_prior_grant: bool,
        tool: ApprovalToolInput<'_>,
    ) -> Result<ApprovalOutcome, EngineError> {
        // The request, and any tree grant made from it, belongs to the
        // runtime of the tool it is for.
        let request = request.with_runtime(
            tool.origin
                .and_then(crate::ToolOrigin::runtime)
                .and_then(|runtime| cookie_agent_protocol::RuntimeId::new(runtime.id()).ok()),
        );
        let approval_id = request.approval_id();
        let session = self.inner.store.get(active.session)?;
        let root = root_id(&session.meta.origin, active.session);
        self.append(
            active.session,
            Some(run),
            crate::runtime::event_origin("engine:approvals"),
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
                crate::runtime::event_origin("engine:approvals"),
                Event::approval_step(
                    approval_id,
                    cookie_agent_protocol::ApprovalStep::DoomLoop {
                        operation_fingerprint: request.operation_fingerprint().clone(),
                        repetitions,
                    },
                ),
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                crate::runtime::event_origin("engine:approvals"),
                Event::approval_finalized(
                    approval_id,
                    cookie_agent_protocol::ApprovalFinalization::Decided {
                        decision: ApprovalFinalDecision {
                            outcome: ApprovalFinalOutcome::Rejected,
                            source: ApprovalDecisionSource::DoomLoopGuard,
                            reason_code: ApprovalReasonCode::DoomLoopDetected,
                            feedback: None,
                            tree_grant_id: None,
                        },
                    },
                ),
            )
            .await?;
            return Ok(ApprovalOutcome {
                approved: false,
                feedback: None,
                reviewer_denial: None,
            });
        }

        if allow_prior_grant
            && let Some(grant) =
                self.inner
                    .approvals
                    .store
                    .matching(root, request.operation(), request.runtime_id())
        {
            let decision = ApprovalInternalDecision {
                decision: ApprovalInternalDecisionKind::Allow,
                source: ApprovalDecisionSource::TreeGrant,
                reason_code: ApprovalReasonCode::TreeGrantMatched,
                evaluations: approval_evaluations(&request),
                rationale: None,
            };
            self.append(
                active.session,
                Some(run),
                crate::runtime::event_origin("engine:approvals"),
                Event::approval_step(
                    approval_id,
                    cookie_agent_protocol::ApprovalStep::Evaluated { decision },
                ),
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                crate::runtime::event_origin("engine:approvals"),
                Event::approval_finalized(
                    approval_id,
                    cookie_agent_protocol::ApprovalFinalization::Decided {
                        decision: ApprovalFinalDecision {
                            outcome: ApprovalFinalOutcome::Approved,
                            source: ApprovalDecisionSource::TreeGrant,
                            reason_code: ApprovalReasonCode::TreeGrantMatched,
                            feedback: None,
                            tree_grant_id: Some(grant.grant_id),
                        },
                    },
                ),
            )
            .await?;
            return Ok(ApprovalOutcome {
                approved: true,
                feedback: None,
                reviewer_denial: None,
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
                rationale: None,
            };
            self.append(
                active.session,
                Some(run),
                crate::runtime::event_origin("engine:approvals"),
                Event::approval_step(
                    approval_id,
                    cookie_agent_protocol::ApprovalStep::Evaluated { decision },
                ),
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                crate::runtime::event_origin("engine:approvals"),
                Event::approval_finalized(
                    approval_id,
                    cookie_agent_protocol::ApprovalFinalization::Decided {
                        decision: ApprovalFinalDecision {
                            outcome: ApprovalFinalOutcome::Rejected,
                            source: ApprovalDecisionSource::Policy,
                            reason_code: ApprovalReasonCode::PolicyDenied,
                            feedback: None,
                            tree_grant_id: None,
                        },
                    },
                ),
            )
            .await?;
            return Ok(ApprovalOutcome {
                approved: false,
                feedback: None,
                reviewer_denial: None,
            });
        }

        let permission_mode = self.permission_mode(root);
        if permission_mode == PermissionMode::Yolo {
            self.append(
                active.session,
                Some(run),
                crate::runtime::event_origin("engine:approvals"),
                Event::approval_step(
                    approval_id,
                    cookie_agent_protocol::ApprovalStep::Evaluated {
                        decision: ApprovalInternalDecision {
                            decision: ApprovalInternalDecisionKind::Allow,
                            source: ApprovalDecisionSource::Policy,
                            reason_code: ApprovalReasonCode::YoloApproved,
                            evaluations: approval_evaluations(&request),
                            rationale: None,
                        },
                    },
                ),
            )
            .await?;
            self.append(
                active.session,
                Some(run),
                crate::runtime::event_origin("engine:approvals"),
                Event::approval_finalized(
                    approval_id,
                    cookie_agent_protocol::ApprovalFinalization::Decided {
                        decision: ApprovalFinalDecision {
                            outcome: ApprovalFinalOutcome::Approved,
                            source: ApprovalDecisionSource::Policy,
                            reason_code: ApprovalReasonCode::YoloApproved,
                            feedback: None,
                            tree_grant_id: None,
                        },
                    },
                ),
            )
            .await?;
            return Ok(ApprovalOutcome {
                approved: true,
                feedback: None,
                reviewer_denial: None,
            });
        }

        let review = match permission_mode {
            PermissionMode::Ask => ReviewOutcome::Unreviewed,
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
                self.review_approval(active, run, &session, &approval_policy, &request)
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
                    (permission_mode, review),
                    cancelled,
                )
            })
            .await?;
        let (mut receiver, expires_at) = match transition {
            ApprovalEvaluationTransition::Resolved(outcome) => return Ok(outcome),
            ApprovalEvaluationTransition::Escalated {
                receiver,
                expires_at,
            } => (receiver, expires_at),
        };
        let expiry_wait = approval_expiry_wait(expires_at);
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
                        reviewer_denial: None,
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
                        reviewer_denial: None,
                    })
                } else {
                    receiver.await.map_err(|_| EngineError::ActorStopped)
                }
            }
        }
    }

    /// Asks the approval reviewer about `request`, the call under review,
    /// in the session's reviewer conversation (approval reviewer spec
    /// section 4).
    async fn review_approval(
        &self,
        active: &ActiveRun,
        run: RunId,
        session: &crate::session::SessionProjection,
        policy: &FrozenInternalAgentPolicy,
        request: &ApprovalRequest,
    ) -> ReviewOutcome {
        let Ok(log) = self.inner.store.log(active.session) else {
            return ReviewOutcome::Failed;
        };
        let events = log.event_snapshot();
        let inherited = self.reviewer_inheritance(session);
        // The official `approval` document's body is the whole prompt; the
        // output contract stays in the request block
        // (`approval_context::REQUEST_CONTRACT`).
        let system = expand_internal_prompt(policy, "");
        // The window of the reviewer's first model, the one a review runs
        // on unless it falls back (approval reviewer spec section 6).
        let bounds = approval_context::Bounds::new(
            policy
                .models
                .first()
                .and_then(|binding| binding.descriptor.capabilities.limits.context),
            policy
                .models
                .first()
                .and_then(|binding| internal_agent_output_limit(binding, policy)),
            &system,
        );
        let Some(conversation) =
            approval_context::render(&events, inherited.as_ref(), request.approval_id(), bounds)
        else {
            return ReviewOutcome::Failed;
        };
        let history = conversation.history(system);
        let result = self
            .run_internal_history_agent(
                active.session,
                Some(run),
                InternalAgentKind::Approval,
                policy,
                InternalAgentHistoryInput {
                    history,
                    summary_source: conversation.request,
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
            Ok(result) => approval_context::parse_verdict(&result.text).map_or(
                ReviewOutcome::Failed,
                |verdict| ReviewOutcome::Verdict {
                    decision: verdict.decision,
                    reason: verdict.reason,
                },
            ),
            Err(_) => ReviewOutcome::Failed,
        }
    }

    /// What a delegated session's reviewer conversation opens with: its
    /// root's user messages up to the delegation, then each ancestor's
    /// instructions from its parent, and the root's `AGENTS.md` (approval
    /// reviewer spec sections 4.3 and 4.5). `None` for a root.
    fn reviewer_inheritance(
        &self,
        session: &crate::session::SessionProjection,
    ) -> Option<approval_context::Inherited> {
        let mut origin = session.meta.origin.clone();
        let mut ancestors = Vec::new();
        while let SessionOrigin::Delegated {
            parent_session_id, ..
        } = origin
        {
            let parent = self.inner.store.get(parent_session_id).ok()?;
            origin = parent.meta.origin.clone();
            ancestors.push(parent);
        }
        let root = ancestors.pop()?;
        // The root's child on the way down: the delegation that started
        // this branch.
        let delegated_at = ancestors
            .last()
            .map_or(&session.log, |child| &child.log)
            .event_snapshot()
            .first()
            .map(|created| created.timestamp)?;
        let root_events = root.log.event_snapshot();
        let mut entries = approval_context::root_messages_until(&root_events, delegated_at);
        entries.extend(ancestors.iter().rev().filter_map(|ancestor| {
            approval_context::delegation_prompt(&ancestor.log.event_snapshot())
        }));
        Some(approval_context::Inherited {
            entries,
            agents_md: approval_context::agents_md_until(&root_events, delegated_at),
        })
    }
}

/// Untrusted `text` for the reviewer's tagged blocks: every `<` becomes
/// `&lt;`, so the text can neither close nor open a block (approval
/// reviewer spec section 4.7). The blocks' own tags are fixed strings.
pub(in crate::runtime) fn escape_untrusted(text: &str) -> Cow<'_, str> {
    if text.contains('<') {
        Cow::Owned(text.replace('<', "&lt;"))
    } else {
        Cow::Borrowed(text)
    }
}

pub(in crate::runtime) fn approval_evaluations(
    request: &ApprovalRequest,
) -> Vec<ApprovalEvaluation> {
    request.evaluations().to_vec()
}

pub(in crate::runtime) fn approval_constraints(request: &ApprovalRequest) -> ApprovalConstraints {
    request.constraints().clone()
}

pub(in crate::runtime) fn approval_request_for_operation(
    tool_call_id: cookie_agent_protocol::ToolCallId,
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
        tool_call_id,
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

/// When an approval escalated now expires: never without a `window`.
pub(in crate::runtime) fn approval_expiry(
    window: Option<std::time::Duration>,
) -> Option<jiff::Timestamp> {
    jiff::Timestamp::now().checked_add(window?).ok()
}

pub(in crate::runtime) fn approval_deadline_exhausted(expires_at: Option<jiff::Timestamp>) -> bool {
    expires_at.is_some_and(|expires_at| expires_at <= jiff::Timestamp::now())
}

pub(in crate::runtime) fn approval_expiry_wait(
    expires_at: Option<jiff::Timestamp>,
) -> std::time::Duration {
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
mod tests;
