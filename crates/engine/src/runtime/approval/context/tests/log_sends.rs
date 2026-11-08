//! What the reviewer's own log sends for the reviews of a session's log,
//! driven as the approval flow drives it, against today's render of the whole
//! log (`docs/site/specs/forever-sessions.md` section 5.5).

use std::{collections::HashMap, sync::Arc};

use cookie_agent_protocol::{
    ApprovalDecisionSource, ApprovalId, ApprovalInternalDecisionKind, ApprovalReasonCode,
    ApprovalRequest, InternalAgentKind, Open, ReviewerTurn, StoredEvent,
};

use crate::runtime::{
    Event,
    approval::context::{
        Bounds, CallFacts, Conversation, Inherited, Verdict,
        thread::{self, Review},
    },
};

/// The verdict each review of `events` gave and where its evaluated step is,
/// as the session's log records them: a completed approval agent between the
/// call's request and its evaluated step.
pub(in crate::runtime) fn verdicts(
    events: &[Arc<StoredEvent>],
) -> HashMap<ApprovalId, (Verdict, u64)> {
    let mut verdicts = HashMap::new();
    let mut pending = None::<ApprovalId>;
    let mut reviewed = std::collections::HashSet::new();
    for event in events {
        match &event.payload {
            Event::ApprovalRequested { request } => pending = Some(request.approval_id()),
            Event::InternalAgentFinished { kind, outcome, .. }
                if *kind == InternalAgentKind::Approval
                    && matches!(
                        outcome.known(),
                        Some(cookie_agent_protocol::InternalAgentOutcome::Completed { .. })
                    ) =>
            {
                if let Some(approval) = pending.take() {
                    reviewed.insert(approval);
                }
            }
            Event::ApprovalStep {
                approval_id,
                step: Open::Known(cookie_agent_protocol::ApprovalStep::Evaluated { decision }),
            } => {
                if pending == Some(*approval_id) {
                    pending = None;
                }
                let decided = match decision.reason_code {
                    ApprovalReasonCode::InternalAgentAllowed => {
                        Some(ApprovalInternalDecisionKind::Allow)
                    }
                    ApprovalReasonCode::InternalAgentDenied => {
                        Some(ApprovalInternalDecisionKind::Deny)
                    }
                    ApprovalReasonCode::Escalated => Some(ApprovalInternalDecisionKind::Ask),
                    _ => None,
                };
                if let Some(decision_kind) = decided.filter(|_| {
                    reviewed.contains(approval_id)
                        && decision.source == ApprovalDecisionSource::InternalAgent
                }) {
                    verdicts.insert(
                        *approval_id,
                        (
                            Verdict {
                                decision: decision_kind,
                                reason: decision.rationale.clone(),
                            },
                            event.seq,
                        ),
                    );
                }
            }
            _ => {}
        }
    }
    verdicts
}

/// What `events` records of the call `request` is for: its start and the
/// arguments of the committed turn it names.
fn call_of(events: &[Arc<StoredEvent>], request: &ApprovalRequest) -> Option<CallFacts> {
    let position = events.iter().position(|event| {
        matches!(&event.payload, Event::ToolCallStarted { start }
            if start.tool_call_id == request.tool_call_id())
    })?;
    let Event::ToolCallStarted { start } = &events[position].payload else {
        return None;
    };
    Some(thread::call_facts(
        &events[position],
        thread::turn_arguments(&events[..position], &start.owner),
    ))
}

/// A turn as the reviewer's log holds it: written as JSON and read back.
fn as_stored(turn: ReviewerTurn) -> ReviewerTurn {
    serde_json::from_value(serde_json::to_value(&turn).expect("a turn serializes"))
        .expect("a turn reads back")
}

/// What the reviewer's log sends for each review of `events` that a review
/// with a verdict precedes or that is `current`, in order, and the turns it
/// holds after each verdict: every review with a verdict records its turn, as
/// the flow records it once the session's log evaluated the verdict, and
/// reads only the session's log after the previous turn's cut.
#[allow(clippy::type_complexity)]
pub(in crate::runtime) fn sent_all(
    events: &[Arc<StoredEvent>],
    inherited: Option<&Inherited>,
    current: Option<ApprovalId>,
    bounds: Bounds,
) -> (Vec<(ApprovalId, Conversation)>, Vec<Vec<ReviewerTurn>>) {
    let verdicts = verdicts(events);
    let mut thread = Vec::<ReviewerTurn>::new();
    let mut unrecorded = None::<ReviewerTurn>;
    let mut threads = Vec::new();
    let mut conversations = Vec::new();
    for (position, event) in events.iter().enumerate() {
        let Event::ApprovalRequested { request } = &event.payload else {
            continue;
        };
        let id = request.approval_id();
        let verdict = verdicts.get(&id);
        // A review with no verdict is still sent, and a log's first turn that
        // was not recorded is what the next one reads on from.
        let failed = Some(id) != current && verdict.is_none();
        let before = &events[..=position];
        let turns = |owner: &cookie_agent_protocol::AssistantToolCallRef| {
            thread::turn_arguments(before, owner)
        };
        let call = call_of(before, request);
        let review = Review {
            thread: &thread,
            unrecorded: unrecorded.as_ref(),
            inherited: if thread.is_empty() { inherited } else { None },
            request,
            cut: event.seq,
            call: call.as_ref(),
            turns: Some(&turns),
            bounds,
        };
        let from = thread
            .last()
            .or(unrecorded.as_ref())
            .map_or(0, |last| last.cut_seq);
        let prepared = review.prepare(
            before
                .iter()
                .filter(|event| event.seq > from)
                .cloned()
                .collect::<Vec<_>>(),
        );
        if failed {
            if thread.is_empty() {
                unrecorded = prepared.into_unrecorded();
            }
            continue;
        }
        conversations.push((id, prepared.conversation.clone()));
        let Some((verdict, evaluated)) = verdict.filter(|_| Some(id) != current) else {
            break;
        };
        unrecorded = None;
        let turn = as_stored(prepared.record(verdict, *evaluated));
        if turn.epoch.is_some() {
            thread.clear();
        }
        thread.push(turn);
        threads.push(thread.clone());
    }
    (conversations, threads)
}

/// What the reviewer's log sends for `current`, after the reviews before it.
pub(in crate::runtime) fn sent(
    events: &[Arc<StoredEvent>],
    inherited: Option<&Inherited>,
    current: ApprovalId,
    bounds: Bounds,
) -> Option<Conversation> {
    let (conversations, _) = sent_all(events, inherited, Some(current), bounds);
    conversations
        .into_iter()
        .find_map(|(id, conversation)| (id == current).then_some(conversation))
}

/// Asserts that the reviewer's log sends `rendered`, today's render for
/// `current`, and that each turn it held was what it sends after.
pub(in crate::runtime) fn assert_log_sends_as_rendered(
    events: &[Arc<StoredEvent>],
    inherited: Option<&Inherited>,
    current: ApprovalId,
    bounds: Bounds,
    rendered: &Conversation,
) {
    let conversation = sent(events, inherited, current, bounds).expect("the log holds the request");
    assert_eq!(
        &conversation, rendered,
        "the reviewer's log sends the render"
    );
}
