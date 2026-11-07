//! The scans the folds of [`super::LogState`] replaced, kept as their
//! references: every fold is compared with its scan at every event of a log
//! (`docs/site/specs/forever-sessions.md` section 7). Each store append and
//! revert compares in tests, as the reference projection fold does, and
//! [`assert_every_event`] goes through a log whole.
//!
//! What differs is named and never shown, so a real log's contents stay out of
//! any output.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use cookie_agent_protocol::{
    AcpCommand, AcpContextUsage, AcpDetached, AcpOptionChoice, AcpSessionRecord, AgentId,
    ApprovalId, ApprovalRecord, ApprovalStatus, ApprovalStep, AttachmentName, ClientResponseId,
    DelegationStage, EventPayload as Event, InternalAgentInvocationId, InternalAgentRunId,
    InvocationId, Open, OperationFingerprint, ProducerOwner, QueuedInputs, ResolvedModelRef, RunId,
    RunOutcome, SessionId, SessionModelState, SessionTitle, SessionTitleChange, Sha256Digest,
    StoredEvent, ToolCallId, ToolTerminationOutcome, TreeApprovalGrant, UserContent,
};
use jiff::Timestamp;

use super::{
    DelegatedTitle, HarvestedDelegation, LatestRequest, LogState, OpenApproval, OpenInternalAgent,
    RunStreak, TakenInput, UserDecision, apply_approval_event, approval_event_id, pending_record,
    seed_digest,
};
use crate::goal_projection::{GoalProducerProjection, goal_tools_offered};

type Events = [Arc<StoredEvent>];

/// The agent's latest ACP session and what left it behind (`lifecycle::plan`).
#[must_use]
pub fn acp_agent<'a>(
    events: &'a Events,
    agent: &AgentId,
) -> Option<(&'a AcpSessionRecord, Vec<&'a AcpDetached>)> {
    let mut latest: Option<&AcpSessionRecord> = None;
    let mut detached: Vec<&AcpDetached> = Vec::new();
    for event in events {
        match &event.payload {
            Event::AcpSessionStarted { record } if &record.agent == agent => {
                latest = Some(record);
                detached.clear();
            }
            Event::AcpSessionUpdated { update } => {
                if let (Some(record), Some(marker)) = (latest, &update.detached)
                    && marker.acp_session_id == record.acp_session_id
                {
                    detached.push(marker);
                }
            }
            _ => {}
        }
    }
    latest.map(|record| (record, detached))
}

/// `options::recorded_option_values`.
#[must_use]
pub fn recorded_option_values(
    events: &Events,
    agent: &AgentId,
    runtime_agent: &AgentId,
) -> Vec<(String, String)> {
    let mut started = false;
    for event in events.iter().rev() {
        match &event.payload {
            Event::AcpSessionStarted { record } if !started => {
                started = true;
                if &record.agent != agent || &record.runtime_agent != runtime_agent {
                    return Vec::new();
                }
            }
            Event::AcpSessionStarted { record }
                if &record.agent != agent || &record.runtime_agent != runtime_agent =>
            {
                return Vec::new();
            }
            Event::AcpSessionUpdated { update } if started => {
                if let Some(options) = &update.options {
                    return options
                        .iter()
                        .map(|option| (option.id.clone(), option.current_value.clone()))
                        .collect();
                }
            }
            _ => {}
        }
    }
    Vec::new()
}

/// `options::recorded_starting_options`.
#[must_use]
pub fn recorded_starting_options(events: &Events) -> Vec<AcpOptionChoice> {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            Event::AcpSessionUpdated { update } => update.starting_options.clone(),
            _ => None,
        })
        .unwrap_or_default()
}

/// What `compact_acp_session` looked for its `compact` command in.
#[must_use]
pub fn latest_commands(events: &Events) -> Option<&[AcpCommand]> {
    events.iter().rev().find_map(|event| match &event.payload {
        Event::AcpSessionUpdated { update } => update.commands.as_deref(),
        _ => None,
    })
}

/// `stored_acp_context`.
#[must_use]
pub fn stored_acp_context(events: &Events) -> Option<AcpContextUsage> {
    events.iter().rev().find_map(|event| match &event.payload {
        Event::AcpSessionUpdated { update } => update.context,
        _ => None,
    })
}

/// The latest `prompt_context` recorded while `agent`'s session was the log's
/// latest, since a session of that agent last started afresh.
#[must_use]
pub fn agent_prompt_context(events: &Events, agent: &AgentId) -> Option<Vec<String>> {
    let mut latest = None;
    let mut context = None;
    for event in events {
        match &event.payload {
            Event::AcpSessionStarted { record } => {
                latest = Some(&record.agent);
                if &record.agent == agent && !super::continues_a_session(record.how) {
                    context = None;
                }
            }
            Event::AcpSessionUpdated { update } if latest == Some(agent) => {
                if let Some(recorded) = &update.prompt_context {
                    context = Some(recorded.clone());
                }
            }
            _ => {}
        }
    }
    context
}

/// The ACP sessions `events` record, each once as first recorded, but those a
/// fork left behind: `lifecycle::own_records`.
#[must_use]
pub fn own_records(events: &Events) -> Vec<AcpSessionRecord> {
    let inherited = events
        .iter()
        .filter_map(|event| match &event.payload {
            Event::AcpSessionUpdated { update } => update
                .detached
                .as_ref()
                .filter(|marker| marker.reason != cookie_agent_protocol::AcpDetachReason::Reverted)
                .map(|marker| marker.acp_session_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut records: Vec<AcpSessionRecord> = Vec::new();
    for event in events {
        if let Event::AcpSessionStarted { record } = &event.payload
            && !inherited.contains(record.acp_session_id.as_str())
            && !records
                .iter()
                .any(|known| known.acp_session_id == record.acp_session_id)
        {
            records.push(record.clone());
        }
    }
    records
}

/// The internal agents that started and did not finish, as
/// `reconcile_session_with` found them.
#[must_use]
pub fn open_internal_agents(
    events: &Events,
) -> HashMap<(InternalAgentInvocationId, InternalAgentRunId), OpenInternalAgent> {
    let mut internal = HashMap::new();
    for event in events {
        match &event.payload {
            Event::InternalAgentStarted {
                invocation_id,
                internal_run_id,
                kind,
                ..
            } => {
                internal.insert(
                    (*invocation_id, *internal_run_id),
                    OpenInternalAgent {
                        kind: *kind,
                        run: event.run_id,
                    },
                );
            }
            Event::InternalAgentFinished {
                invocation_id,
                internal_run_id,
                ..
            } => {
                internal.remove(&(*invocation_id, *internal_run_id));
            }
            _ => {}
        }
    }
    internal
}

/// `approval_projection::doom_loop_repetitions`: approval requests for
/// `fingerprint` in `run_id` since the last reset, user input or a different
/// operation completing, scanning back from the tip.
#[must_use]
pub fn doom_loop_repetitions(
    events: &Events,
    run_id: RunId,
    fingerprint: &OperationFingerprint,
) -> u32 {
    let mut repetitions = 0_u32;
    // A termination precedes nothing it depends on in reverse: its start, met
    // later, decides whether it was a reset. Each candidate keeps the count of
    // requests that followed it.
    let mut candidates = HashMap::<ToolCallId, u32>::new();
    let mut reset_at = None::<u32>;
    for event in events
        .iter()
        .rev()
        .filter(|event| event.run_id == Some(run_id))
    {
        match &event.payload {
            Event::UserInputSubmitted { .. }
            | Event::UserInputApplied { .. }
            | Event::RunStarted { .. } => {
                if matches!(
                    event.payload,
                    Event::UserInputSubmitted { .. } | Event::UserInputApplied { .. }
                ) {
                    reset_at = Some(reset_at.map_or(repetitions, |at| at.min(repetitions)));
                }
                break;
            }
            Event::ApprovalRequested { request }
                if request.operation_fingerprint() == fingerprint =>
            {
                repetitions = repetitions.saturating_add(1);
            }
            Event::ToolCallTerminated { termination }
                if termination.outcome == ToolTerminationOutcome::Completed =>
            {
                candidates
                    .entry(termination.tool_call_id)
                    .or_insert(repetitions);
            }
            Event::ToolCallStarted { start } => {
                if let Some(at) = candidates.remove(&start.tool_call_id)
                    && &start.operation_fingerprint != fingerprint
                {
                    reset_at = Some(reset_at.map_or(at, |reset| reset.min(at)));
                }
            }
            _ => {}
        }
        // Older resets can only count more requests than the latest one.
        if let Some(at) = reset_at
            && candidates.values().all(|candidate| *candidate >= at)
        {
            break;
        }
    }
    reset_at.unwrap_or(repetitions)
}

/// How `after_runs` read the latest runs, scanning back from the tip.
#[must_use]
pub fn run_streak(events: &Events) -> RunStreak {
    let mut streak = RunStreak::default();
    for event in events.iter().rev() {
        match &event.payload {
            Event::AutomaticRunsHeld { .. } => {
                streak.held = true;
                break;
            }
            Event::AutomaticRunsResumed {} | Event::UserInputSubmitted { .. } => break,
            Event::RunFinished { outcome } => match outcome {
                Open::Known(RunOutcome::Cancelled { .. })
                    if streak.failures == 0
                        && event.origin.as_ref().is_some_and(super::stopped_by_user) =>
                {
                    streak.stopped = true;
                    break;
                }
                Open::Known(RunOutcome::Failed { error, .. }) => {
                    streak.failures += 1;
                    streak.last_error.get_or_insert_with(|| error.clone());
                }
                Open::Known(RunOutcome::Interrupted { .. }) => {}
                _ => break,
            },
            _ => {}
        }
    }
    streak
}

/// `title_regeneration_target`.
#[must_use]
pub fn latest_turn(events: &Events) -> Option<(RunId, u64)> {
    events.iter().rev().find_map(|event| match &event.payload {
        Event::ModelTurnCommitted {
            input_through_seq, ..
        } => event.run_id.map(|run| (run, *input_through_seq)),
        _ => None,
    })
}

/// `active_fallback_index`, of whichever run tried a model last.
#[must_use]
pub fn latest_fallback(events: &Events) -> Option<(RunId, usize)> {
    events.iter().rev().find_map(|event| {
        let run = event.run_id?;
        match &event.payload {
            Event::ModelAttemptStarted { fallback_index, .. } => {
                Some((run, *fallback_index as usize))
            }
            Event::ModelFallback {
                to_fallback_index, ..
            } => Some((run, *to_fallback_index as usize)),
            _ => None,
        }
    })
}

/// `cache_expiry::latest_request`, without the bound on how far back it looked
/// for when the request went out.
#[must_use]
pub fn latest_request(events: &Events) -> Option<LatestRequest> {
    let mut checkpoint_after = false;
    let mut found = None;
    let mut attempt = None;
    for event in events.iter().rev() {
        match (&event.payload, &found) {
            (Event::ContextCheckpointCommitted { .. }, None) => checkpoint_after = true,
            (
                Event::ModelUsageRecorded {
                    model_turn_seq,
                    resolved_model,
                    usage,
                    cache_ttl,
                    ..
                },
                None,
            ) => {
                if checkpoint_after {
                    return None;
                }
                let stamp = (*cache_ttl)?;
                let tokens = usage
                    .input_tokens
                    .unwrap_or(0)
                    .saturating_add(usage.output_tokens.unwrap_or(0));
                found = Some((
                    *model_turn_seq,
                    LatestRequest {
                        usage_seq: event.seq,
                        sent_at: event.timestamp,
                        stamp,
                        tokens,
                        model: resolved_model.clone(),
                    },
                ));
            }
            (
                Event::ModelTurnCommitted {
                    model_turn_seq,
                    attempt_id,
                    ..
                },
                Some((turn, _)),
            ) if model_turn_seq == turn && attempt.is_none() => attempt = Some(*attempt_id),
            (Event::ModelRequestPrepared { attempt_id, .. }, Some(_))
            | (Event::ModelAttemptStarted { attempt_id, .. }, Some(_))
                if Some(*attempt_id) == attempt =>
            {
                let (_, mut request) = found.take()?;
                request.sent_at = event.timestamp;
                return Some(request);
            }
            _ => {}
        }
    }
    found.map(|(_, request)| request)
}

/// `Engine::input_since`, before the prompt blocks: the input the session took
/// after `boundary`.
#[must_use]
pub fn input_since<'a>(
    events: &'a Events,
    producers: &'a GoalProducerProjection,
    boundary: u64,
) -> Vec<TakenInput<'a>> {
    let admitted = producers
        .messages
        .iter()
        .filter(|message| !message.discarded)
        .filter_map(|message| {
            message.admission.map(|(_, seq)| {
                let wake = matches!(message.producer_owner, ProducerOwner::AcpAgent { .. });
                (seq, (message.message_id, &message.body, wake))
            })
        })
        .collect::<HashMap<_, _>>();
    let mut submitted: HashMap<u64, &UserContent> = HashMap::new();
    let mut taken = Vec::new();
    for event in events {
        match &event.payload {
            Event::UserInputSubmitted { content } => {
                submitted.insert(event.seq, content);
            }
            Event::UserInputApplied { user_input_seq } if event.seq > boundary => {
                if let Some(content) = submitted.get(user_input_seq) {
                    taken.push(TakenInput::User {
                        user_input_seq: *user_input_seq,
                        content,
                    });
                }
            }
            Event::ProducerMessageAdmitted { .. } if event.seq > boundary => {
                if let Some((message_id, body, wake)) = admitted.get(&event.seq) {
                    taken.push(TakenInput::Producer {
                        message_id: *message_id,
                        body,
                        wake: *wake,
                    });
                }
            }
            _ => {}
        }
    }
    taken
}

/// `woke_since`.
#[must_use]
pub fn woke_since(producers: &GoalProducerProjection, boundary: u64) -> bool {
    producers.messages.iter().any(|message| {
        !message.discarded
            && matches!(message.producer_owner, ProducerOwner::AcpAgent { .. })
            && message.admission.is_some_and(|(_, seq)| seq > boundary)
    })
}

/// What `producer_input_waiting` looked for first.
#[must_use]
pub fn producer_message_unadmitted(producers: &GoalProducerProjection) -> bool {
    producers
        .messages
        .iter()
        .any(|message| !message.discarded && message.admission.is_none())
}

/// The boundary `acp_prompt` read: `input_through_seq` of the last committed
/// turn.
#[must_use]
pub fn input_boundary(events: &Events) -> u64 {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            Event::ModelTurnCommitted {
                input_through_seq, ..
            } => Some(*input_through_seq),
            _ => None,
        })
        .unwrap_or(0)
}

fn committed_input_boundaries(events: &Events) -> impl Iterator<Item = u64> + '_ {
    events.iter().filter_map(|event| match &event.payload {
        Event::ModelTurnCommitted {
            input_through_seq, ..
        } => Some(*input_through_seq),
        _ => None,
    })
}

fn highest_input_boundary(events: &Events) -> u64 {
    committed_input_boundaries(events).max().unwrap_or(0)
}

/// Whether a committed turn covered less input than one before it. No run
/// does: `input_through_seq` only moves forward. The fold lets go of what a
/// turn covers, so on a log where this holds it would not send that input
/// again as the scan did.
#[must_use]
pub fn input_boundary_fell(events: &Events) -> bool {
    let mut highest = 0;
    committed_input_boundaries(events).any(|boundary| {
        let fell = boundary < highest;
        highest = highest.max(boundary);
        fell
    })
}

fn run_start_index(events: &Events, run: RunId) -> Option<usize> {
    events.iter().rposition(|event| {
        event.run_id == Some(run) && matches!(event.payload, Event::RunStarted { .. })
    })
}

/// `mailbox::pending_inputs`: the sequence numbers of the inputs admitted for
/// `run` that it has yet to submit.
#[must_use]
pub fn pending_inputs(events: &Events, run: RunId) -> Vec<u64> {
    let mut pending = std::collections::VecDeque::new();
    let mut initial_input_submitted = false;
    let start = run_start_index(events, run);
    let run_start_seq = start.map(|start| events[start].seq);
    let previous_terminal = start.and_then(|start| {
        events[..start]
            .iter()
            .rposition(|event| matches!(event.payload, Event::RunFinished { .. }))
    });
    let previous_terminal_seq = previous_terminal.map_or(0, |previous| events[previous].seq);
    for event in events[previous_terminal.unwrap_or(0)..]
        .iter()
        .filter(|event| {
            event.run_id == Some(run)
                || (run_start_seq.is_some_and(|run_start_seq| event.seq < run_start_seq)
                    && event.seq > previous_terminal_seq
                    && event.run_id.is_none()
                    && matches!(
                        event.payload,
                        Event::UserInputAdmitted { .. } | Event::UserInputRecalled { .. }
                    ))
        })
    {
        match &event.payload {
            Event::ProducerMessageAdmitted { .. } => initial_input_submitted = true,
            Event::UserInputAdmitted { .. } => pending.push_back(event.seq),
            Event::UserInputRecalled { user_input_seq, .. } => {
                if let Some(position) = pending.iter().position(|seq| seq == user_input_seq) {
                    pending.remove(position);
                }
            }
            Event::UserInputSubmitted { .. } => {
                if initial_input_submitted {
                    pending.pop_front();
                } else {
                    initial_input_submitted = true;
                }
            }
            Event::RunFinished { .. } => pending.clear(),
            _ => {}
        }
    }
    pending.into_iter().collect()
}

/// `residency::has_runless_pending_inputs`.
#[must_use]
pub fn has_runless_pending_inputs(events: &Events) -> bool {
    let mut recalled = Vec::new();
    for event in events
        .iter()
        .rev()
        .take_while(|event| !matches!(event.payload, Event::RunFinished { .. }))
        .filter(|event| event.run_id.is_none())
    {
        match &event.payload {
            Event::UserInputAdmitted { .. } if !recalled.contains(&event.seq) => return true,
            Event::UserInputRecalled { user_input_seq, .. } => recalled.push(*user_input_seq),
            _ => {}
        }
    }
    false
}

/// What `void_runless_pending_inputs` gathered, by sequence number.
#[must_use]
pub fn runless_pending_inputs(events: &Events) -> Vec<u64> {
    let boundary = events
        .iter()
        .rev()
        .find(|event| matches!(event.payload, Event::RunFinished { .. }))
        .map_or(0, |event| event.seq);
    let mut pending = Vec::new();
    for event in events
        .iter()
        .filter(|event| event.seq > boundary && event.run_id.is_none())
    {
        match &event.payload {
            Event::UserInputAdmitted { .. } => pending.push(event.seq),
            Event::UserInputRecalled { user_input_seq, .. } => {
                pending.retain(|seq| seq != user_input_seq);
            }
            _ => {}
        }
    }
    pending
}

/// What `PromotePendingInputs` gathered: `run`'s submissions nothing applied.
#[must_use]
pub fn unapplied_submissions(events: &Events, run: RunId) -> Vec<u64> {
    let run_events = &events[run_start_index(events, run).unwrap_or(0)..];
    let applied: HashSet<u64> = run_events
        .iter()
        .filter_map(|event| match &event.payload {
            Event::UserInputApplied { user_input_seq } if event.run_id == Some(run) => {
                Some(*user_input_seq)
            }
            _ => None,
        })
        .collect();
    run_events
        .iter()
        .filter_map(|event| match &event.payload {
            Event::UserInputSubmitted { .. }
                if event.run_id == Some(run) && !applied.contains(&event.seq) =>
            {
                Some(event.seq)
            }
            _ => None,
        })
        .collect()
}

/// The latest `run_started` any run recorded, as the callers looked for it
/// (`run_agent_prompt`, `acp_compaction`, `acp_machines_note`).
#[must_use]
pub fn latest_run_started(events: &Events) -> Option<&Arc<StoredEvent>> {
    events
        .iter()
        .rev()
        .find(|event| event.run_id.is_some() && matches!(event.payload, Event::RunStarted { .. }))
}

/// What `acp_machines_note` decided from.
#[must_use]
pub fn machines_changed_before(events: &Events, run: RunId) -> Option<bool> {
    let started = events.iter().rposition(|event| {
        event.run_id == Some(run) && matches!(event.payload, Event::RunStarted { .. })
    })?;
    let previous_end = events[..started]
        .iter()
        .rposition(|event| matches!(event.payload, Event::RunFinished { .. }))?;
    Some(events[previous_end..started].iter().any(|event| {
        matches!(
            event.payload,
            Event::RuntimeAttached { .. } | Event::RuntimeDetached { .. }
        )
    }))
}

/// What `add_attached_context` read.
#[must_use]
pub fn attached_since_run_end(events: &Events) -> Option<HashSet<AttachmentName>> {
    let ended = events.iter().rev().find_map(|event| {
        matches!(event.payload, Event::RunFinished { .. }).then_some(event.seq)
    })?;
    Some(
        events
            .iter()
            .filter(|event| event.seq > ended)
            .filter_map(|event| match &event.payload {
                Event::RuntimeAttached { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect(),
    )
}

/// What `goal_tools_offered` decided from, for an ACP agent's root run.
#[must_use]
pub fn goal_open_at_run_start(events: &Events, run: RunId) -> bool {
    let start = run_start_index(events, run).unwrap_or(0);
    goal_tools_offered(true, true, &events[..start])
}

/// `run_cancelled_recorded`, for every run at once.
#[must_use]
pub fn cancelled_runs(events: &Events) -> HashSet<RunId> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                Event::RunFinished {
                    outcome: Open::Known(RunOutcome::Cancelled { .. })
                }
            )
        })
        .filter_map(|event| event.run_id)
        .collect()
}

/// What `run_failure_event` looked for.
#[must_use]
pub fn last_attempt_model(events: &Events, run: RunId) -> Option<&ResolvedModelRef> {
    events.iter().rev().find_map(|event| {
        if event.run_id != Some(run) {
            return None;
        }
        match &event.payload {
            Event::ModelAttemptStarted { resolved_model, .. } => Some(resolved_model),
            _ => None,
        }
    })
}

/// What `root_tree_idle` read.
#[must_use]
pub fn last_run_end(events: &Events) -> Option<Timestamp> {
    events
        .iter()
        .filter(|event| {
            event.run_id.is_some() && matches!(event.payload, Event::RunFinished { .. })
        })
        .map(|event| event.timestamp)
        .max()
}

/// The `tool_call_started` of every call no termination recorded the end of,
/// each the latest of its ID (`tool_call_owner`, `signal_approval_needs_you`).
#[must_use]
pub fn open_calls(events: &Events) -> HashMap<ToolCallId, u64> {
    let mut open = HashMap::new();
    for event in events {
        match &event.payload {
            Event::ToolCallStarted { start } => {
                open.insert(start.tool_call_id, event.seq);
            }
            Event::ToolCallTerminated { termination } => {
                open.remove(&termination.tool_call_id);
            }
            _ => {}
        }
    }
    open
}

/// `approval_records`, whatever their status.
#[must_use]
pub fn approval_records(
    session_id: SessionId,
    events: &Events,
) -> HashMap<ApprovalId, ApprovalRecord> {
    let mut records = HashMap::<ApprovalId, ApprovalRecord>::new();
    for envelope in events {
        if let Event::ApprovalRequested { request } = &envelope.payload {
            records.insert(request.approval_id(), pending_record(session_id, request));
        } else if let Some(approval_id) = approval_event_id(&envelope.payload)
            && let Some(record) = records.get_mut(&approval_id)
        {
            apply_approval_event(record, &envelope.payload);
        }
    }
    records
}

/// `approval_run_id`.
#[must_use]
pub fn approval_run_id(events: &Events, approval_id: ApprovalId) -> Option<RunId> {
    events.iter().find_map(|event| match &event.payload {
        Event::ApprovalRequested { request } if request.approval_id() == approval_id => {
            event.run_id
        }
        _ => None,
    })
}

fn decision_of(event: &StoredEvent) -> Option<(&ClientResponseId, UserDecision)> {
    match &event.payload {
        Event::ApprovalStep {
            approval_id,
            step:
                Open::Known(ApprovalStep::UserDecision {
                    client_response_id,
                    decision,
                    feedback,
                    agent_option,
                }),
        } => Some((
            client_response_id,
            UserDecision {
                approval_id: *approval_id,
                decision: *decision,
                feedback: feedback.clone(),
                agent_option: agent_option.clone(),
            },
        )),
        _ => None,
    }
}

/// What `approval_respond_direct` looked `response` up in: the first decision
/// under it.
#[must_use]
pub fn response(events: &Events, response: &ClientResponseId) -> Option<UserDecision> {
    events.iter().find_map(|event| {
        decision_of(event)
            .filter(|(recorded, _)| *recorded == response)
            .map(|(_, decision)| decision)
    })
}

/// What `user_answer` read: the latest decision on the approval.
#[must_use]
pub fn user_decision(events: &Events, approval_id: ApprovalId) -> Option<UserDecision> {
    events.iter().rev().find_map(|event| {
        decision_of(event)
            .filter(|(_, decision)| decision.approval_id == approval_id)
            .map(|(_, decision)| decision)
    })
}

/// `permission_overlay_epoch`.
#[must_use]
pub fn permission_overlay_epoch(events: &Events) -> u64 {
    events
        .iter()
        .rev()
        .find(|event| matches!(event.payload, Event::SessionPermissionOverlaySet { .. }))
        .map_or(0, |event| event.seq)
}

/// What `ensure_delegated_title` decided.
#[must_use]
pub fn delegated_title(
    events: &Events,
    invocation_id: InvocationId,
    title: &SessionTitle,
) -> DelegatedTitle {
    let mut has_matching_title = false;
    for event in events {
        if let Event::SessionTitleCommitted { change, .. } = &event.payload {
            match change {
                SessionTitleChange::DelegatedSet {
                    title: existing,
                    invocation_id: existing_invocation,
                } if *existing_invocation == invocation_id => {
                    if existing != title {
                        return DelegatedTitle::OtherOrigin;
                    }
                    has_matching_title = true;
                }
                SessionTitleChange::UserSet { .. }
                | SessionTitleChange::UserClear { .. }
                | SessionTitleChange::UserReset { .. } => {}
                _ => return DelegatedTitle::OtherTitle,
            }
        }
    }
    if has_matching_title {
        DelegatedTitle::Same
    } else {
        DelegatedTitle::Absent
    }
}

/// The "once" facts a pass over the log finds: seeds, skills, links, accepted
/// delegation completions, and what each delegated title was.
#[derive(Default)]
struct OnceScan<'a> {
    seeds: HashMap<InvocationId, Vec<Sha256Digest>>,
    titles: Vec<(InvocationId, &'a SessionTitle)>,
    skills: HashSet<(String, Sha256Digest)>,
    links: HashSet<(ToolCallId, SessionId)>,
    delegations_accepted: HashSet<InvocationId>,
}

fn once_scan(events: &Events) -> OnceScan<'_> {
    let mut scan = OnceScan::default();
    for event in events {
        match &event.payload {
            Event::DelegatedContextSeeded {
                invocation_id,
                turns,
            } => scan
                .seeds
                .entry(*invocation_id)
                .or_default()
                .push(seed_digest(turns)),
            Event::SessionTitleCommitted {
                change:
                    SessionTitleChange::DelegatedSet {
                        title,
                        invocation_id,
                    },
                ..
            } => scan.titles.push((*invocation_id, title)),
            Event::SkillLoaded {
                name,
                rendered_body,
                ..
            } => {
                scan.skills.insert((
                    name.clone(),
                    Sha256Digest::of_bytes(rendered_body.as_bytes()),
                ));
            }
            Event::ToolCallLinked {
                tool_call_id,
                child_session_id,
            } => {
                scan.links.insert((*tool_call_id, *child_session_id));
            }
            Event::ProducerMessageAccepted {
                producer_owner: ProducerOwner::Delegation { invocation_id },
                ..
            } => {
                scan.delegations_accepted.insert(*invocation_id);
            }
            _ => {}
        }
    }
    scan
}

/// The folds of `state` that differ from their scans over `events`, the log
/// `state` went through.
#[must_use]
pub fn differences(state: &LogState, events: &Events) -> Vec<&'static str> {
    let mut found = Vec::new();
    let mut check = |name: &'static str, same: bool| {
        if !same {
            found.push(name);
        }
    };
    let Some(tip) = events.last().map(|event| event.seq) else {
        return found;
    };

    // Per agent, and for one no record names.
    let mut agents = vec![AgentId::new("reference-stranger").expect("a valid agent ID")];
    let mut declarations = Vec::new();
    for event in events {
        if let Event::AcpSessionStarted { record } = &event.payload {
            if !agents.contains(&record.agent) {
                agents.push(record.agent.clone());
            }
            if !declarations.contains(&record.runtime_agent) {
                declarations.push(record.runtime_agent.clone());
            }
        }
    }
    for agent in &agents {
        let known = state.acp_agent(agent);
        let scanned = acp_agent(events, agent);
        check(
            "acp session per agent",
            match (known, &scanned) {
                (None, None) => true,
                (Some(known), Some((record, detached))) => {
                    known.record == **record && known.detached.iter().eq(detached.iter().copied())
                }
                _ => false,
            },
        );
        check(
            "prompt context per agent",
            known
                .and_then(|known| known.prompt_context.as_deref())
                .cloned()
                == agent_prompt_context(events, agent),
        );
        for declaration in &declarations {
            check(
                "recorded option values",
                state.recorded_option_values(agent, declaration)
                    == recorded_option_values(events, agent, declaration),
            );
        }
    }
    check(
        "own ACP sessions",
        state.acp_own_records().cloned().collect::<Vec<_>>() == own_records(events),
    );
    check(
        "starting options",
        state.acp_starting_options() == recorded_starting_options(events),
    );
    check("commands", state.acp_commands() == latest_commands(events));
    check(
        "context use",
        state.acp_context() == stored_acp_context(events),
    );

    // Latest values.
    check(
        "model state",
        *state.model() == SessionModelState::from_events(events),
    );
    check("queue", *state.queue() == QueuedInputs::from_events(events));
    let producers = GoalProducerProjection::from_events(events);
    check("producers", state.producers().agrees_with_whole(&producers));
    check(
        "unadmitted producer message",
        state.producer_message_unadmitted() == producer_message_unadmitted(&producers),
    );
    check(
        "permission overlay epoch",
        state.permission_overlay_epoch() == permission_overlay_epoch(events),
    );

    // Input since a boundary at or after every committed turn's. An input a
    // turn covered is let go (`input_boundary_fell` tells of a log where that
    // matters).
    check(
        "input boundary",
        state.input_boundary() == input_boundary(events),
    );
    let boundary = highest_input_boundary(events);
    for after in [
        boundary,
        boundary + 1,
        boundary + tip.saturating_sub(boundary) / 2,
        tip.saturating_sub(1).max(boundary),
        tip.max(boundary),
    ] {
        check(
            "input since",
            state.inputs_since(after) == input_since(events, &producers, after),
        );
        check(
            "woke since",
            state.woke_since(after) == woke_since(&producers, after),
        );
    }

    // The current run.
    let started = latest_run_started(events);
    check(
        "latest run started",
        state.latest_run_started().map(|event| event.seq) == started.map(|event| event.seq),
    );
    if let Some(run) = started.and_then(|event| event.run_id) {
        let finished = events.iter().any(|event| {
            event.run_id == Some(run) && matches!(event.payload, Event::RunFinished { .. })
        });
        check(
            "pending inputs",
            state
                .pending_inputs(run)
                .iter()
                .map(|event| event.seq)
                .eq(pending_inputs(events, run)),
        );
        if !finished {
            check(
                "unapplied submissions",
                state
                    .unapplied_submissions(run)
                    .eq(unapplied_submissions(events, run)),
            );
        }
        check(
            "machines changed before the run",
            state.machines_changed_before(run) == machines_changed_before(events, run),
        );
        check(
            "goal open at the run's start",
            state.goal_open_at_run_start() == goal_open_at_run_start(events, run),
        );
        check(
            "last attempt",
            state.last_attempt_model(run) == last_attempt_model(events, run),
        );
    }
    check(
        "runless pending inputs",
        state
            .runless_pending_inputs()
            .iter()
            .map(|event| event.seq)
            .eq(runless_pending_inputs(events)),
    );
    check(
        "has runless pending inputs",
        state.runless_pending_inputs().is_empty() != has_runless_pending_inputs(events),
    );
    check(
        "attached since the run's end",
        state.attached_since_run_end().cloned() == attached_since_run_end(events),
    );
    check(
        "cancelled runs",
        state.run.cancelled == cancelled_runs(events),
    );
    check(
        "last run's end",
        state.last_run_end() == last_run_end(events),
    );

    // Open calls.
    let open = open_calls(events);
    check(
        "open calls",
        state.calls.len() == open.len()
            && open
                .iter()
                .all(|(call, seq)| state.open_call(*call).map(|event| event.seq) == Some(*seq)),
    );

    // What the latest run's approvals and the runs before it come to.
    check("run streak", *state.run_streak() == run_streak(events));
    check(
        "open internal agents",
        state.internal_agents == open_internal_agents(events),
    );
    check("latest turn", state.latest_turn() == latest_turn(events));
    check(
        "latest fallback",
        state.latest_fallback() == latest_fallback(events),
    );
    check(
        "latest request",
        state.latest_request().cloned() == latest_request(events),
    );
    if let Some(run) = state
        .latest_run_started()
        .and_then(|started| started.run_id)
    {
        for event in events {
            if let Event::ApprovalRequested { request } = &event.payload {
                let fingerprint = request.operation_fingerprint();
                check(
                    "doom loop repetitions",
                    state.doom_loop_repetitions(run, fingerprint)
                        == doom_loop_repetitions(events, run, fingerprint),
                );
            }
        }
    }

    // Approvals.
    let session = events[0].session_id;
    let open = approval_records(session, events)
        .into_iter()
        .filter(|(_, record)| {
            matches!(
                record.status,
                ApprovalStatus::Pending | ApprovalStatus::Escalated
            )
        })
        .map(|(id, record)| {
            let run = approval_run_id(events, id);
            (id, OpenApproval { record, run })
        })
        .collect::<HashMap<_, _>>();
    check("open approvals", state.approvals.open == open);
    if let Some(settled) = &state.approvals.settled {
        let id = settled.record.request.approval_id();
        check(
            "the approval settled last",
            approval_records(session, events).get(&id) == Some(&settled.record)
                && approval_run_id(events, id) == settled.run,
        );
    }
    let mut responses = HashSet::new();
    let mut approvals = HashSet::new();
    for event in events {
        if let Some((id, decision)) = decision_of(event) {
            responses.insert(id);
            approvals.insert(decision.approval_id);
        }
    }
    check(
        "responses",
        state.approvals.responses.len() == responses.len()
            && responses
                .iter()
                .all(|id| state.response(id).map(AsRef::as_ref) == response(events, id).as_ref()),
    );
    check(
        "user decisions",
        state.approvals.decisions.len() == approvals.len()
            && approvals.iter().all(|id| {
                state.user_decision(*id).map(AsRef::as_ref) == user_decision(events, *id).as_ref()
            }),
    );

    // "Once" facts.
    let once = once_scan(events);
    check("seeds", state.once.seeds == once.seeds);
    check("skills", state.once.skills == once.skills);
    check("links", state.once.links == once.links);
    check(
        "delegation completions",
        state.once.delegations_accepted == once.delegations_accepted,
    );
    for (invocation, title) in &once.titles {
        check(
            "delegated title",
            state.delegated_title(*invocation, title)
                == delegated_title(events, *invocation, title),
        );
    }

    let inputs = events
        .iter()
        .filter(|event| matches!(event.payload, Event::UserInputSubmitted { .. }));
    check(
        "opening inputs",
        state
            .opening_inputs()
            .iter()
            .map(|event| event.seq)
            .eq(inputs
                .clone()
                .take(super::OPENING_INPUTS)
                .map(|event| event.seq)),
    );
    if let Some(run) = events
        .iter()
        .rev()
        .find(|event| matches!(event.payload, Event::RunStarted { .. }))
        .and_then(|started| started.run_id)
    {
        let mut inputs = inputs;
        check(
            "first input of the latest run",
            state
                .first_input_of_run(run)
                .map(|input| input.map(|input| input.seq))
                == Some(
                    inputs
                        .find(|event| event.run_id == Some(run))
                        .map(|event| event.seq),
                ),
        );
    }

    let (grants, delegations, queued, accepted) = harvest(events);
    let harvested = state.harvest();
    check("the harvest's grants", harvested.grants() == grants);
    check(
        "the harvest's delegation records",
        harvested.delegations() == delegations,
    );
    check(
        "the harvest's queued delegations",
        harvested.queued() == queued,
    );
    check(
        "the harvest's accepted delegations",
        harvested.producer_accepted() == accepted,
    );

    found
}

/// What [`harvest`] reads: the grants, the delegation records, the queued
/// delegations, and those a producer message accepted.
type Harvest = (
    Vec<TreeApprovalGrant>,
    Vec<HarvestedDelegation>,
    Vec<(RunId, SessionId)>,
    Vec<InvocationId>,
);

/// `TreeFold::harvest_records` and `ParentFactsFold`.
#[must_use]
pub fn harvest(events: &Events) -> Harvest {
    let mut revisions = cookie_agent_protocol::RevisionFold::default();
    let (mut grants, mut delegations, mut queued, mut accepted) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut started = HashMap::new();
    for event in events {
        revisions.observe(&event.payload);
        match &event.payload {
            Event::DelegationProgressed {
                invocation_id,
                stage: Open::Known(DelegationStage::Started { child_session_id }),
            } => {
                started.insert(*invocation_id, *child_session_id);
            }
            Event::DelegationProgressed {
                invocation_id,
                stage: Open::Known(DelegationStage::Queued { .. }),
            } => {
                if let (Some(run), Some(child)) = (event.run_id, started.get(invocation_id)) {
                    queued.push((run, *child));
                }
            }
            Event::ProducerMessageAccepted {
                producer_owner: ProducerOwner::Delegation { invocation_id },
                ..
            } => accepted.push(*invocation_id),
            _ => {}
        }
        match &event.payload {
            Event::TreeApprovalGrantCommitted { grant } => grants.push(grant.clone()),
            payload if crate::delegation_events::is_delegation_payload(payload) => {
                delegations.push(HarvestedDelegation {
                    seq: event.seq,
                    run: event.run_id,
                    payload: crate::delegation_events::harvested(payload, &revisions),
                });
            }
            _ => {}
        }
    }
    (grants, delegations, queued, accepted)
}

/// Panics, naming the folds and the record, when `state` differs from what
/// the scans read in `events`.
pub fn assert_matches_scans(state: &LogState, events: &Events) {
    let found = differences(state, events);
    assert!(
        found.is_empty(),
        "folds differ from their scans at record {:?}: {found:?}",
        events.last().map(|event| event.seq),
    );
}

/// Folds `events` from the start, comparing every fold with its scan at every
/// event. Names the folds that differed, each with the first record it did.
#[must_use]
pub fn differences_at_every_event(events: &Events) -> Vec<(&'static str, u64)> {
    let mut state = LogState::default();
    let mut found: Vec<(&'static str, u64)> = Vec::new();
    for (index, event) in events.iter().enumerate() {
        state.observe(event);
        for name in differences(&state, &events[..=index]) {
            if !found.iter().any(|(known, _)| *known == name) {
                found.push((name, event.seq));
            }
        }
    }
    if input_boundary_fell(events) {
        found.push(("input boundary fell", 0));
    }
    found
}

/// Panics unless every fold matches its scan at every event of `events`.
pub fn assert_every_event(events: &Events) {
    let found = differences_at_every_event(events);
    assert!(
        found.is_empty(),
        "folds differ from their scans (fold, first record): {found:?}"
    );
}
