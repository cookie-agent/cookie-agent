use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{Arc, Mutex},
};

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
    /// One review at a time per session, from reading the reviewer's log to
    /// recording its turn, so each turn follows the one before it.
    pub(crate) reviews: Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>,
    /// A session's first reviewer turn that was sent and not recorded, as its
    /// review failed, with how many cuts its log had then: the next review
    /// reads on from its cut rather than pass over the whole log again.
    pub(crate) unrecorded: Mutex<HashMap<SessionId, (u64, cookie_agent_protocol::ReviewerTurn)>>,
}

/// A review's turn, to record in the reviewer's log once the session's log
/// holds its verdict.
pub(crate) struct PendingReviewTurn {
    _review: tokio::sync::OwnedMutexGuard<()>,
    prepared: approval_context::thread::Prepared,
    /// The reviewer's log holds no turn yet: this one creates it.
    create: bool,
    /// How many cuts the session's log had when the review read it: a revert
    /// since leaves the turn out.
    cuts: u64,
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

        // Every approval of a session, an ACP agent's too.
        let repetitions = self
            .inner
            .store
            .log(active.session)?
            .state()
            .doom_loop_repetitions(run, request.operation_fingerprint());
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

        // An ACP agent's harness is its only gate that asks: what cookie's
        // rules leave open goes to the user, whatever the tree's mode
        // (`docs/site/specs/acp-permissions.md`).
        let permission_mode = if active.policy.acp_agent().is_some() {
            PermissionMode::Ask
        } else {
            self.permission_mode(root)
        };
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

        let (review, pending_turn) = match permission_mode {
            PermissionMode::Ask => (ReviewOutcome::Unreviewed, None),
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
        let evaluated = (request.clone(), executor.clone(), review.clone());
        let (transition, evaluated_seq) = self
            .on_actor(session, move |engine| {
                let (request, executor, review) = evaluated;
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
        if let Some(pending) = pending_turn {
            match (&review, evaluated_seq) {
                (ReviewOutcome::Verdict { decision, reason }, Some(evaluated_seq)) => {
                    self.record_review_turn(
                        session,
                        pending,
                        &approval_context::Verdict {
                            decision: *decision,
                            reason: reason.clone(),
                        },
                        evaluated_seq,
                    );
                }
                _ => self.keep_unrecorded_turn(session, pending),
            }
        }
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
    /// section 4), and what its log records once the verdict is evaluated.
    ///
    /// The conversation is the reviewer's own log
    /// (`docs/site/specs/forever-sessions.md` section 5.5): this reads the
    /// session's log only after the last turn's cut, and the whole of it only
    /// for a log's first turn, which a session an earlier build reviewed
    /// starts with.
    async fn review_approval(
        &self,
        active: &ActiveRun,
        run: RunId,
        session: &crate::session::SessionProjection,
        policy: &FrozenInternalAgentPolicy,
        request: &ApprovalRequest,
    ) -> (ReviewOutcome, Option<PendingReviewTurn>) {
        let gate = Arc::clone(
            self.inner
                .approvals
                .reviews
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(active.session)
                .or_default(),
        );
        let guard = gate.lock_owned().await;
        let Ok(prepared) = self.prepare_review(session, policy, request) else {
            return (ReviewOutcome::Failed, None);
        };
        let Some((prepared, create, cuts, system)) = prepared else {
            return (ReviewOutcome::Failed, None);
        };
        let history = prepared.conversation.history(system);
        let summary_source = prepared.conversation.request.clone();
        let result = self
            .run_internal_history_agent(
                active.session,
                Some(run),
                InternalAgentKind::Approval,
                policy,
                InternalAgentHistoryInput {
                    history,
                    summary_source,
                    tools: Vec::new(),
                    reject_non_text: true,
                },
                InternalAgentExecution {
                    cancellation: &active.cancellation,
                    actor_direct: false,
                },
            )
            .await;
        let pending = PendingReviewTurn {
            _review: guard,
            prepared,
            create,
            cuts,
        };
        let outcome = match result {
            Ok(result) => approval_context::parse_verdict(&result.text).map_or(
                ReviewOutcome::Failed,
                |verdict| ReviewOutcome::Verdict {
                    decision: verdict.decision,
                    reason: verdict.reason,
                },
            ),
            Err(_) => ReviewOutcome::Failed,
        };
        (outcome, Some(pending))
    }

    /// What the review of `request` sends, from the reviewer's log and the
    /// session's since the log's last turn, whether it creates the log, how
    /// many cuts the session's log had, and the system prompt. `None` when the
    /// session's log does not hold the request.
    #[allow(clippy::type_complexity)]
    fn prepare_review(
        &self,
        session: &crate::session::SessionProjection,
        policy: &FrozenInternalAgentPolicy,
        request: &ApprovalRequest,
    ) -> Result<Option<(approval_context::thread::Prepared, bool, u64, String)>, EngineError> {
        let session_id = session.meta.session_id;
        let log = self.inner.store.log(session_id)?;
        let cuts = log.cut_count();
        // The model's context holds the call under review, open, with its
        // committed turn, and the request just appended.
        let context = log.model_context_events()?;
        let Some(cut) = context.iter().rev().find_map(|event| match &event.payload {
            Event::ApprovalRequested { request: requested }
                if requested.approval_id() == request.approval_id() =>
            {
                Some(event.seq)
            }
            _ => None,
        }) else {
            return Ok(None);
        };
        let turns = |owner: &cookie_agent_protocol::AssistantToolCallRef| {
            approval_context::thread::turn_arguments(&context, owner)
        };
        let call = log.state().open_call(request.tool_call_id()).map(|start| {
            let arguments = match &start.payload {
                Event::ToolCallStarted { start } => turns(&start.owner),
                _ => None,
            };
            approval_context::thread::call_facts(start, arguments)
        });
        let thread = match self.inner.store.reviewer_log(session_id)? {
            Some(reviewer) => {
                approval_context::thread::thread_of(&reviewer.model_context_events()?)
            }
            None => Vec::new(),
        };
        // A first turn sent and not recorded since, of this log as it stands.
        let unrecorded = thread
            .is_empty()
            .then(|| {
                self.inner
                    .approvals
                    .unrecorded
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&session_id)
                    .filter(|(at, turn)| *at == cuts && turn.cut_seq < cut)
                    .map(|(_, turn)| turn.clone())
            })
            .flatten();
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
        // A log's first turn opens with what a delegated session inherits,
        // read once, and reads the session's log from its start.
        let inherited = if thread.is_empty() && unrecorded.is_none() {
            self.reviewer_inheritance(session)?
        } else {
            None
        };
        let review = approval_context::thread::Review {
            thread: &thread,
            unrecorded: unrecorded.as_ref(),
            inherited: inherited.as_ref(),
            request,
            cut,
            call: call.as_ref(),
            turns: Some(&turns),
            bounds,
        };
        // The delta after the last turn's cut, from the nearest point the log
        // can read from; a log's first turn passes over the whole log once.
        let from = thread
            .last()
            .or(unrecorded.as_ref())
            .map_or(1, |last| last.cut_seq.saturating_add(1));
        let read = log
            .history_range(from..=cut)?
            .collect::<Result<Vec<_>, _>>()?;
        #[cfg(test)]
        self.inner
            .test_hooks
            .reviewer_reads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((from, read.len()));
        let prepared = review.prepare(read);
        Ok(Some((prepared, thread.is_empty(), cuts, system)))
    }

    /// Keeps `pending`, a log's first turn whose review gave no verdict to
    /// record, for the next review to read on from.
    fn keep_unrecorded_turn(&self, session: SessionId, pending: PendingReviewTurn) {
        let PendingReviewTurn {
            _review,
            prepared,
            create,
            cuts,
        } = pending;
        if !create {
            return;
        }
        if let Some(turn) = prepared.into_unrecorded() {
            self.inner
                .approvals
                .unrecorded
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(session, (cuts, turn));
        }
    }

    /// Records `pending`, the turn a review sent, with `verdict`, which the
    /// session's log evaluated at `evaluated_seq`, in the reviewer's log. A
    /// revert of the session since the review read it leaves it out, and a
    /// failure only costs the next review a longer delta.
    fn record_review_turn(
        &self,
        session: SessionId,
        pending: PendingReviewTurn,
        verdict: &approval_context::Verdict,
        evaluated_seq: u64,
    ) {
        let PendingReviewTurn {
            _review,
            prepared,
            create,
            cuts,
        } = pending;
        if self
            .inner
            .store
            .log(session)
            .map_or(true, |log| log.cut_count() != cuts)
        {
            return;
        }
        self.inner
            .approvals
            .unrecorded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session);
        let payload = Event::ReviewerTurnRecorded {
            turn: Box::new(prepared.record(verdict, evaluated_seq)),
        };
        let recorded = if create {
            self.inner
                .store
                .create_reviewer_log(session, vec![payload])
                .map(drop)
        } else {
            self.inner.store.append_reviewer(session, payload).map(drop)
        };
        if let Err(error) = recorded {
            eprintln!(
                "cookie-agent: session {session}: the reviewer's turn was not recorded: {error}"
            );
        }
    }

    /// What a delegated session's reviewer conversation opens with: its
    /// root's user messages up to the delegation, then each ancestor's
    /// instructions from its parent, and the root's `AGENTS.md` (approval
    /// reviewer spec sections 4.3 and 4.5). `None` for a root.
    fn reviewer_inheritance(
        &self,
        session: &crate::session::SessionProjection,
    ) -> Result<Option<approval_context::Inherited>, EngineError> {
        let mut origin = session.meta.origin.clone();
        let mut ancestors = Vec::new();
        while let SessionOrigin::Delegated {
            parent_session_id, ..
        } = origin
        {
            let Ok(parent) = self.inner.store.get(parent_session_id) else {
                return Ok(None);
            };
            origin = parent.meta.origin.clone();
            ancestors.push(parent);
        }
        let Some(root) = ancestors.pop() else {
            return Ok(None);
        };
        // The root's child on the way down: the delegation that started
        // this branch.
        let Some(delegated_at) = ancestors
            .last()
            .map_or(&session.log, |child| &child.log)
            .creation()
            .map(|created| created.timestamp)
        else {
            return Ok(None);
        };
        // An ancestor can be an ACP agent's session, whose log is windowed: a
        // pass over each, not its model's context.
        let mut opening = approval_context::RootOpening::new(delegated_at);
        for event in root.log.history()? {
            opening.observe(event?);
        }
        let (mut entries, agents_md) = opening.finish();
        for ancestor in ancestors.iter().rev() {
            entries.extend(
                ancestor
                    .log
                    .history()?
                    .find_map_ok(approval_context::delegation_prompt)?,
            );
        }
        Ok(Some(approval_context::Inherited { entries, agents_md }))
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
