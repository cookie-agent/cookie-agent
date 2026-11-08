//! Today's render of the reviewer conversation from the whole session log,
//! kept as the test-only reference the reviewer's own log is compared with
//! (`docs/site/specs/forever-sessions.md` section 5.5): before the
//! reviewer had a log, every escalated approval rendered its conversation
//! with [`render`], a pass over the session's log.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::Arc,
};

use cookie_agent_protocol::{
    ApprovalDecisionSource, ApprovalFinalOutcome, ApprovalId, ApprovalInternalDecisionKind,
    ApprovalReasonCode, ApprovalRequest, InternalAgentKind, Open, RunId, StoredEvent, ToolCallId,
    ToolTerminationOutcome,
};
use serde_json::Value;

use super::{
    Attachments, Bounds, CARRIED_VERDICTS, CallFacts, Conversation, Inherited, KEPT_FIRST_MESSAGES,
    MESSAGE_TOKEN_CAP, READ_ONLY_TOOLS, Since, TRAJECTORY_TOKEN_CAP, Verdict, agents_md_before,
    agents_md_text, cap_tokens, estimate_tokens, has_authority, input_label, producer_label,
    request_block, source_label, summarize_arguments, timestamp_label,
};
use crate::runtime::{Event, approval::flow::escape_untrusted};

/// The request block of `approval_id` as the log records it.
fn block(facts: &LogFacts, approval_id: ApprovalId, since: Since) -> String {
    let request = facts.requests.get(&approval_id).map(|(_, request)| request);
    request_block(
        request,
        request.and_then(|request| facts.calls.get(&request.tool_call_id())),
        &facts.attachments,
        since,
    )
}

/// An entry of the conversation at its position in the log.
struct Positioned {
    seq: u64,
    text: String,
    message: bool,
}

/// A review the log records: its call, and its verdict once the reviewer
/// gave one.
struct Review {
    approval_id: ApprovalId,
    cut: u64,
    verdict: Option<Verdict>,
}

/// The facts the renderer gathers from one session's log in a single pass.
#[derive(Default)]
struct LogFacts {
    entries: Vec<Positioned>,
    reviews: Vec<Review>,
    requests: HashMap<ApprovalId, (u64, ApprovalRequest)>,
    calls: HashMap<ToolCallId, CallFacts>,
    attachments: Attachments,
    agents_md: Vec<(u64, Option<RunId>, String)>,
}

/// The session's reviewer conversation, up to and including the review of
/// `current` (sections 4.1 and 6). `inherited` is a delegated session's
/// opening; a root's instructions come from its own log.
///
/// The conversation is an epoch: it starts over, from a fresh render of the
/// log within the budgets, when the next review's turn would not fit the
/// window. Epoch boundaries follow from the log and `bounds` alone, so a
/// restart reproduces them.
pub(in crate::runtime) fn render(
    events: &[Arc<StoredEvent>],
    inherited: Option<&Inherited>,
    current: ApprovalId,
    bounds: Bounds,
) -> Option<Conversation> {
    let facts = gather(events, current);
    let current_cut = facts.requests.get(&current)?.0;
    let completed = facts
        .reviews
        .iter()
        .filter(|review| review.cut < current_cut)
        .filter_map(|review| {
            review
                .verdict
                .as_ref()
                .map(|verdict| (review.approval_id, review.cut, verdict))
        })
        .collect::<Vec<_>>();
    let first_cut = completed.first().map_or(current_cut, |(_, cut, _)| *cut);
    let agents_md = match inherited {
        Some(inherited) => inherited.agents_md.clone(),
        None => agents_md_before(&facts.agents_md, first_cut),
    };
    let opening = Opening {
        inherited,
        agents_md: agents_md.as_deref(),
    };
    let mut reviews = Vec::<(String, String)>::new();
    let mut tokens = bounds.system_tokens;
    let mut previous = 0;
    let mut epochs = 0;
    let steps = completed
        .iter()
        .map(|(approval_id, cut, verdict)| (*approval_id, *cut, Some(*verdict)))
        .chain(std::iter::once((current, current_cut, None)));
    for (index, (approval_id, cut, verdict)) in steps.enumerate() {
        let carried = || {
            completed[index.saturating_sub(CARRIED_VERDICTS)..index]
                .iter()
                .map(|(approval_id, cut, verdict)| {
                    (*cut, carried_verdict(&facts, *approval_id, verdict))
                })
                .collect::<Vec<_>>()
        };
        let mut text = if reviews.is_empty() {
            fresh_turn(
                &facts,
                cut,
                approval_id,
                opening,
                &carried(),
                bounds,
                epochs > 0,
            )
        } else {
            delta_turn(&facts, (previous, cut), approval_id)
        };
        if !reviews.is_empty() && tokens + estimate_tokens(&text) > bounds.window {
            reviews.clear();
            epochs += 1;
            tokens = bounds.system_tokens;
            text = fresh_turn(&facts, cut, approval_id, opening, &carried(), bounds, true);
        }
        tokens += estimate_tokens(&text);
        previous = cut;
        match verdict {
            Some(verdict) => {
                let verdict = verdict.render();
                tokens += estimate_tokens(&verdict);
                reviews.push((text, verdict));
            }
            None => {
                return Some(Conversation {
                    reviews,
                    request: text,
                });
            }
        }
    }
    None
}

/// What opens the first turn of every epoch.
#[derive(Clone, Copy)]
struct Opening<'a> {
    inherited: Option<&'a Inherited>,
    agents_md: Option<&'a str>,
}

/// An earlier verdict as a new epoch carries it: a trajectory entry.
fn carried_verdict(facts: &LogFacts, approval_id: ApprovalId, verdict: &Verdict) -> String {
    let name = facts
        .requests
        .get(&approval_id)
        .and_then(|(_, request)| facts.calls.get(&request.tool_call_id()))
        .and_then(|call| call.name.as_deref())
        .unwrap_or("a tool");
    let mut text = format!("[your earlier verdict] on {name}: {}", verdict.render());
    text = escape_untrusted(&cap_tokens(&text, TRAJECTORY_TOKEN_CAP)).into_owned();
    text
}

/// The first turn of an epoch: the opening, then every entry up to `cut`
/// within the budgets, the carried verdicts among them, then the request
/// block (section 6).
fn fresh_turn(
    facts: &LogFacts,
    cut: u64,
    approval_id: ApprovalId,
    opening: Opening<'_>,
    carried: &[(u64, String)],
    bounds: Bounds,
    started_over: bool,
) -> String {
    // Inherited messages come first, at position zero.
    let inherited = opening
        .inherited
        .map(|inherited| inherited.entries.as_slice())
        .unwrap_or_default();
    let mut messages = inherited
        .iter()
        .map(|text| (0, text.as_str()))
        .chain(
            facts
                .entries
                .iter()
                .filter(|entry| entry.message && entry.seq <= cut)
                .map(|entry| (entry.seq, entry.text.as_str())),
        )
        .collect::<Vec<_>>();
    let omitted_messages = budget_messages(&mut messages, bounds.messages());
    let mut trajectory = facts
        .entries
        .iter()
        .filter(|entry| !entry.message && entry.seq <= cut)
        .map(|entry| (entry.seq, entry.text.as_str()))
        .chain(carried.iter().map(|(seq, text)| (*seq, text.as_str())))
        .collect::<Vec<_>>();
    trajectory.sort_by_key(|(seq, _)| *seq);
    let omitted_calls = budget_trajectory(&mut trajectory, bounds.trajectory());

    let mut text = String::from("<context>\n");
    let mut wrote = false;
    let mut push = |text: &mut String, entry: &str| {
        text.push_str(entry);
        text.push_str("\n\n");
        wrote = true;
    };
    if let Some(agents_md) = opening.agents_md {
        push(&mut text, &format!("[project instructions]\n{agents_md}"));
    }
    // Messages and trajectory interleave by position; an omission marker
    // stands where it dropped entries.
    let mut merged = messages
        .iter()
        .map(|(seq, entry)| (*seq, Cow::Borrowed(*entry)))
        .chain(
            trajectory
                .iter()
                .map(|(seq, entry)| (*seq, Cow::Borrowed(*entry))),
        )
        .collect::<Vec<_>>();
    if let Some((at, count)) = omitted_messages {
        merged.push((
            at,
            Cow::Owned(format!("[… {count} earlier messages omitted …]")),
        ));
    }
    if let Some((at, count)) = omitted_calls {
        merged.push((
            at,
            Cow::Owned(format!("[… {count} earlier trajectory entries omitted …]")),
        ));
    }
    merged.sort_by_key(|(seq, _)| *seq);
    for (_, entry) in &merged {
        push(&mut text, entry);
    }
    if wrote {
        text.pop();
    } else {
        text.push_str("[nothing new]\n");
    }
    text.push_str("</context>\n\n");
    text.push_str(&block(
        facts,
        approval_id,
        if started_over {
            Since::StartedOver
        } else {
            Since::FirstReview
        },
    ));
    text
}

/// Keeps `messages` within `budget`: the first [`KEPT_FIRST_MESSAGES`]
/// messages with the user's authority, the first instructions from a
/// parent agent, and the newest messages, dropping a
/// block from the middle down to half the budget, so it happens rarely.
/// Returns where the omission marker goes and how many it stands for.
fn budget_messages(messages: &mut Vec<(u64, &str)>, budget: usize) -> Option<(u64, usize)> {
    let total = messages
        .iter()
        .map(|(_, text)| estimate_tokens(text))
        .sum::<usize>();
    if total <= budget {
        return None;
    }
    let target = budget / 2;
    let mut keep = vec![false; messages.len()];
    let mut used = 0;
    let mut first = 0;
    let mut instructions = false;
    for (index, (_, text)) in messages.iter().enumerate() {
        // A delegated session's own instructions are its task too: the
        // first of them stays beside the root's first messages.
        let task = if has_authority(text) && first < KEPT_FIRST_MESSAGES {
            first += 1;
            true
        } else if !instructions && text.starts_with("[instructions from the parent agent]") {
            instructions = true;
            true
        } else {
            false
        };
        if task {
            keep[index] = true;
            used += estimate_tokens(text);
        }
    }
    let last_first = keep.iter().rposition(|kept| *kept);
    for index in (0..messages.len()).rev() {
        if keep[index] || last_first.is_some_and(|last| index < last) {
            continue;
        }
        let cost = estimate_tokens(messages[index].1);
        if used + cost > target {
            break;
        }
        keep[index] = true;
        used += cost;
    }
    let dropped = keep.iter().filter(|kept| !**kept).count();
    let at = messages
        .iter()
        .zip(&keep)
        .find(|(_, kept)| !**kept)
        .map_or(0, |((seq, _), _)| *seq);
    let mut kept = keep.into_iter();
    messages.retain(|_| kept.next().unwrap_or(false));
    (dropped > 0).then_some((at, dropped))
}

/// Keeps `trajectory` within `budget` by dropping its oldest half, as
/// often as it takes. Returns where the omission marker goes and how many
/// entries it stands for.
fn budget_trajectory(trajectory: &mut Vec<(u64, &str)>, budget: usize) -> Option<(u64, usize)> {
    let mut dropped = 0;
    let mut at = 0;
    while trajectory
        .iter()
        .map(|(_, text)| estimate_tokens(text))
        .sum::<usize>()
        > budget
        && !trajectory.is_empty()
    {
        let half = trajectory.len().div_ceil(2);
        at = trajectory[half - 1].0;
        trajectory.drain(..half);
        dropped += half;
    }
    (dropped > 0).then_some((at, dropped))
}

/// A later turn of an epoch: the entries after `range.0` up to `range.1`,
/// then the request block of `approval_id`.
fn delta_turn(facts: &LogFacts, range: (u64, u64), approval_id: ApprovalId) -> String {
    let delta = facts
        .entries
        .iter()
        .filter(|entry| entry.seq > range.0 && entry.seq <= range.1)
        .collect::<Vec<_>>();
    let mut text = String::from("<context_delta>\n");
    for entry in &delta {
        text.push_str(&entry.text);
        text.push_str("\n\n");
    }
    if delta.is_empty() {
        text.push_str("[nothing new]\n");
    } else {
        text.pop();
    }
    text.push_str("</context_delta>\n\n");
    let messages = delta.iter().filter(|entry| entry.message).count();
    text.push_str(&block(facts, approval_id, Since::Delta(messages)));
    text
}

/// One pass over `events`: every entry with its position, every review
/// with its verdict, and what the request blocks need. `current` is the
/// approval under review, which never has a verdict yet.
fn gather(events: &[Arc<StoredEvent>], current: ApprovalId) -> LogFacts {
    let mut facts = LogFacts::default();
    let mut run_ordinals = HashMap::<RunId, usize>::new();
    let mut runs_with_input = HashSet::<RunId>::new();
    let mut turn_calls = HashMap::<(u64, u32), (String, Value)>::new();
    let mut producer_bodies = HashMap::new();
    let mut pending_review = None::<ApprovalId>;
    let mut reviewed = HashSet::<ApprovalId>::new();
    for event in events {
        // Runs count in the order they first appear, which is their
        // `run_started`.
        if let Some(run) = event.run_id {
            let next = run_ordinals.len() + 1;
            run_ordinals.entry(run).or_insert(next);
        }
        match &event.payload {
            Event::RuntimeAttached {
                name,
                runtime_id,
                kind,
                workspace,
                ..
            } => {
                facts.attachments.insert(
                    name.as_str().to_owned(),
                    (
                        runtime_id.as_str().to_owned(),
                        kind.as_str().to_owned(),
                        workspace.clone(),
                    ),
                );
            }
            Event::AgentMdLoaded { .. } => {
                if let Some(text) = agents_md_text(event) {
                    facts.agents_md.push((event.seq, event.run_id, text));
                }
            }
            Event::UserInputSubmitted { content } => {
                let origin = event.origin.as_ref().map(|origin| origin.as_str());
                let when = run_label(event.run_id, &run_ordinals, &mut runs_with_input);
                facts.entries.push(Positioned {
                    seq: event.seq,
                    text: format!(
                        "{} {}{when}\n{}",
                        input_label(origin),
                        timestamp_label(event.timestamp),
                        escape_untrusted(&cap_tokens(&content.as_plain_text(), MESSAGE_TOKEN_CAP))
                    ),
                    message: true,
                });
            }
            Event::ProducerMessageAccepted {
                message_id,
                producer_owner,
                body,
                ..
            } => {
                producer_bodies.insert(*message_id, (producer_owner.clone(), body.clone()));
            }
            Event::ProducerMessageConsumed { message_id, run_id } => {
                if let Some((owner, body)) = producer_bodies.get(message_id)
                    && let Some(label) = producer_label(owner)
                {
                    let when = run_label(Some(*run_id), &run_ordinals, &mut runs_with_input);
                    facts.entries.push(Positioned {
                        seq: event.seq,
                        text: format!(
                            "{label} {}{when}\n{}",
                            timestamp_label(event.timestamp),
                            escape_untrusted(&cap_tokens(&body.as_plain_text(), MESSAGE_TOKEN_CAP))
                        ),
                        message: true,
                    });
                }
            }
            Event::ModelTurnCommitted {
                model_turn_seq,
                turn,
                ..
            } => {
                for (index, part) in turn.content.iter().enumerate() {
                    if let cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                        name,
                        input,
                        ..
                    } = part
                    {
                        turn_calls.insert(
                            (*model_turn_seq, u32::try_from(index).unwrap_or(u32::MAX)),
                            (name.as_str().to_owned(), input.clone()),
                        );
                    }
                }
            }
            Event::ToolCallStarted { start } => {
                let committed =
                    turn_calls.get(&(start.owner.model_turn_seq, start.owner.content_index));
                facts.calls.insert(
                    start.tool_call_id,
                    CallFacts {
                        name: Some(start.presentation.tool.name.clone()),
                        source: start
                            .presentation
                            .tool
                            .source
                            .as_ref()
                            .and_then(|source| source.known().cloned()),
                        arguments: committed.map(|(_, input)| input.clone()),
                    },
                );
            }
            Event::ToolCallTerminated { termination } => {
                if let Some(call) = facts.calls.get(&termination.tool_call_id)
                    && let Some(text) = trajectory_entry(call, termination)
                {
                    facts.entries.push(Positioned {
                        seq: event.seq,
                        text,
                        message: false,
                    });
                }
            }
            Event::ApprovalRequested { request } => {
                facts
                    .requests
                    .insert(request.approval_id(), (event.seq, request.clone()));
                if request.approval_id() != current {
                    pending_review = Some(request.approval_id());
                }
            }
            Event::InternalAgentFinished { kind, outcome, .. } => {
                if *kind == InternalAgentKind::Approval
                    && matches!(
                        outcome.known(),
                        Some(cookie_agent_protocol::InternalAgentOutcome::Completed { .. })
                    )
                    && let Some(approval_id) = pending_review.take()
                {
                    reviewed.insert(approval_id);
                }
            }
            Event::ApprovalStep {
                approval_id,
                step: Open::Known(cookie_agent_protocol::ApprovalStep::Evaluated { decision }),
            } => {
                if pending_review == Some(*approval_id) {
                    pending_review = None;
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
                let verdict = decided
                    .filter(|_| {
                        reviewed.contains(approval_id)
                            && decision.source == ApprovalDecisionSource::InternalAgent
                    })
                    .map(|decision_kind| Verdict {
                        decision: decision_kind,
                        reason: decision.rationale.clone(),
                    });
                if let Some((cut, _)) = facts.requests.get(approval_id) {
                    facts.reviews.push(Review {
                        approval_id: *approval_id,
                        cut: *cut,
                        verdict,
                    });
                }
            }
            Event::ApprovalFinalized {
                outcome:
                    Open::Known(cookie_agent_protocol::ApprovalFinalization::Decided { decision }),
                ..
            } if decision.source == ApprovalDecisionSource::User => {
                let text = match decision.outcome {
                    ApprovalFinalOutcome::Approved => Some("[user decision] approved".to_owned()),
                    ApprovalFinalOutcome::Rejected => Some(match &decision.feedback {
                        Some(feedback) => format!(
                            "[user decision] rejected: {}",
                            escape_untrusted(&cap_tokens(
                                feedback.message.as_str(),
                                TRAJECTORY_TOKEN_CAP
                            ))
                        ),
                        None => "[user decision] rejected".to_owned(),
                    }),
                    ApprovalFinalOutcome::Cancelled | ApprovalFinalOutcome::Expired => None,
                };
                if let Some(text) = text {
                    facts.entries.push(Positioned {
                        seq: event.seq,
                        text,
                        message: false,
                    });
                }
            }
            _ => {}
        }
    }
    facts
}

/// A finished call as a trajectory entry, or `None` for a read-only one
/// (section 4.4).
fn trajectory_entry(
    call: &CallFacts,
    termination: &cookie_agent_protocol::ToolCallTermination,
) -> Option<String> {
    let name = call.name.as_deref().unwrap_or("unknown");
    if READ_ONLY_TOOLS.contains(&name) {
        return None;
    }
    let denied = termination.error.as_ref().is_some_and(|error| {
        serde_json::from_str::<Value>(error.message.as_str())
            .ok()
            .and_then(|value| value.get("kind").and_then(Value::as_str).map(str::to_owned))
            .is_some_and(|kind| kind == "tool_denied")
    });
    let outcome = match termination.outcome {
        _ if denied => "denied",
        ToolTerminationOutcome::Completed => "completed",
        ToolTerminationOutcome::Failed => "failed",
        ToolTerminationOutcome::Cancelled | ToolTerminationOutcome::Interrupted => "cancelled",
    };
    let summary = call
        .arguments
        .as_ref()
        .map(|arguments| summarize_arguments(name, arguments))
        .unwrap_or_default();
    let mut text = format!(
        "[tool call] {name}{}: {outcome}",
        source_label(call.source.as_ref())
    );
    if !summary.is_empty() {
        text.push('\n');
        text.push_str(&summary);
    }
    Some(escape_untrusted(&cap_tokens(&text, TRAJECTORY_TOKEN_CAP)).into_owned())
}

/// When an input arrived: `, starts run N` for a run's first input, else
/// `, during run N`.
fn run_label(
    run: Option<RunId>,
    ordinals: &HashMap<RunId, usize>,
    with_input: &mut HashSet<RunId>,
) -> String {
    let Some(run) = run else {
        return String::new();
    };
    let ordinal = ordinals.get(&run).copied().unwrap_or(0);
    if with_input.insert(run) {
        format!(", starts run {ordinal}")
    } else {
        format!(", during run {ordinal}")
    }
}
