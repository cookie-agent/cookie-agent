//! Durable goal and producer-message projection.

use std::collections::{HashMap, HashSet};

use cookie_agent_protocol::{
    EventPayload, GoalId, GoalReminderIdentity, GoalReminderKind, GoalState, GoalStatus,
    ProducerDeliveryMode, ProducerIdempotencyKey, ProducerMessageId, ProducerOwner, RunId,
    RunSelection, SafeDisplayText, StoredEvent,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProducerMessageRecord {
    pub message_id: ProducerMessageId,
    pub producer_owner: ProducerOwner,
    pub mode: ProducerDeliveryMode,
    pub idempotency_key: ProducerIdempotencyKey,
    pub description: SafeDisplayText,
    pub body: String,
    pub reminder: Option<GoalReminderIdentity>,
    /// `send_message` hop depth stamped by the guard. `None` means the message
    /// carries no hop metadata: either a non-Agent producer or mail accepted
    /// before hop counting existed, which inheritance treats as hop `0`.
    pub agent_hop: Option<u32>,
    pub accepted_seq: u64,
    pub admission: Option<(RunId, u64)>,
    pub claims: HashSet<u64>,
    pub consumed: bool,
    pub discarded: bool,
    pub discarded_seq: Option<u64>,
    pub consumption_recorded: bool,
    pub consumed_run: Option<RunId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProducerClaimRecord {
    pub run_id: RunId,
    pub message_ids: Vec<ProducerMessageId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct GoalProducerProjection {
    pub goal: Option<GoalState>,
    pub selection: Option<RunSelection>,
    pub messages: Vec<ProducerMessageRecord>,
    pub claims: HashMap<u64, ProducerClaimRecord>,
    pub invalid: Vec<(u64, String)>,
}

impl GoalProducerProjection {
    pub(crate) fn next_reminder_kind(&self, goal_id: GoalId) -> GoalReminderKind {
        // Acceptance and claims are retractable; only committed input coverage introduces a goal.
        if self.messages.iter().any(|message| {
            message.consumed
                && message
                    .reminder
                    .is_some_and(|reminder| reminder.goal_id == goal_id)
        }) {
            GoalReminderKind::Continuation
        } else {
            GoalReminderKind::Started
        }
    }

    /// Hop depth a `send_message` from `run` must carry: one more than the
    /// deepest Agent-owned mail this session's log shows `run` has seen, or `0`
    /// when the run has seen no agent mail.
    ///
    /// The basis is durable observation — admission, a live claim, or committed
    /// consumption — not payload heuristics, so a chain cannot be reset by
    /// releasing a claim or by restarting the process. A message with no hop
    /// metadata is a pre-guard send and counts as depth `0`.
    ///
    /// This over-approximates on purpose: any send from a run that consumed
    /// hop-`N` mail inherits `N + 1`, even when the model treats the send as an
    /// unrelated topic. The guard's bound, not per-message linkage, is the
    /// control operators tune.
    #[must_use]
    pub(crate) fn inherited_agent_hop(&self, run: RunId) -> u32 {
        self.messages
            .iter()
            .filter(|message| {
                matches!(message.producer_owner, ProducerOwner::Agent { .. })
                    && (message.consumed_run == Some(run)
                        || message
                            .admission
                            .is_some_and(|(admitted_run, _)| admitted_run == run)
                        || self.claims.values().any(|claim| {
                            claim.run_id == run && claim.message_ids.contains(&message.message_id)
                        }))
            })
            .map(|message| message.agent_hop.unwrap_or(0))
            .max()
            .map_or(0, |deepest| deepest.saturating_add(1))
    }

    #[must_use]
    pub(crate) fn from_events(events: &[StoredEvent]) -> Self {
        let mut projection = Self::default();
        let mut goal_revisions = HashMap::<cookie_agent_protocol::GoalId, u64>::new();
        let mut message_indexes = HashMap::<ProducerMessageId, usize>::new();
        let mut dedup_indexes = HashMap::<(ProducerOwner, ProducerIdempotencyKey), usize>::new();
        let mut terminal_runs = HashSet::<RunId>::new();

        for event in events {
            match &event.payload {
                EventPayload::GoalActivated {
                    goal_id,
                    objective,
                    revision,
                    selection,
                } => {
                    let replaceable = projection
                        .goal
                        .as_ref()
                        .is_none_or(|goal| is_terminal(goal.status));
                    let distinct = projection
                        .goal
                        .as_ref()
                        .is_none_or(|goal| goal.goal_id != *goal_id);
                    let unseen = !goal_revisions.contains_key(goal_id);
                    if objective.trim().is_empty() || !replaceable || !distinct || !unseen {
                        projection.reject(event.seq, "invalid goal activation");
                        continue;
                    }
                    projection.goal = Some(GoalState {
                        goal_id: *goal_id,
                        objective: objective.clone(),
                        status: GoalStatus::Active,
                        items: Vec::new(),
                        revision: *revision,
                    });
                    projection.selection = selection.clone();
                    goal_revisions.insert(*goal_id, *revision);
                }
                EventPayload::GoalChecklistRevised {
                    goal_id,
                    items,
                    revision,
                } => {
                    let valid_items = items.iter().all(|item| !item.description.trim().is_empty());
                    let valid = valid_items
                        && projection.goal.as_ref().is_some_and(|goal| {
                            goal.goal_id == *goal_id
                                && !is_terminal(goal.status)
                                && *revision > goal.revision
                        });
                    if !valid {
                        projection.reject(event.seq, "invalid goal checklist revision");
                        continue;
                    }
                    let goal = projection.goal.as_mut().expect("validated current goal");
                    goal.items = items.clone();
                    goal.revision = *revision;
                    goal_revisions.insert(*goal_id, *revision);
                }
                EventPayload::GoalLifecycleChanged {
                    goal_id,
                    status,
                    revision,
                    selection,
                } => {
                    let valid = projection.goal.as_ref().is_some_and(|goal| {
                        goal.goal_id == *goal_id
                            && *revision > goal.revision
                            && valid_lifecycle_change(goal, *status)
                            && (selection.is_none() || *status == GoalStatus::Active)
                    });
                    if !valid {
                        projection.reject(event.seq, "invalid goal lifecycle transition");
                        continue;
                    }
                    let goal = projection.goal.as_mut().expect("validated current goal");
                    goal.status = *status;
                    goal.revision = *revision;
                    if let Some(selection) = selection {
                        projection.selection = Some(selection.clone());
                    }
                    goal_revisions.insert(*goal_id, *revision);
                }
                EventPayload::ProducerMessageAccepted {
                    message_id,
                    producer_owner,
                    mode,
                    idempotency_key,
                    description,
                    body,
                    reminder,
                    agent_hop,
                } => {
                    if !valid_reminder_owner(producer_owner, reminder.as_ref()) {
                        projection.reject(event.seq, "invalid producer reminder ownership");
                        continue;
                    }
                    if !valid_hop_owner(producer_owner, *agent_hop) {
                        projection.reject(event.seq, "invalid producer hop ownership");
                        continue;
                    }
                    let dedup_key = (producer_owner.clone(), idempotency_key.clone());
                    if let Some(index) = dedup_indexes.get(&dedup_key).copied() {
                        let prior = &projection.messages[index];
                        // `agent_hop` is guard metadata, not payload: a replayed
                        // acceptance for the same key stays idempotent even when
                        // the stamped depth differs from the stored record.
                        let exact = prior.message_id == *message_id
                            && prior.mode == *mode
                            && prior.description == *description
                            && prior.body == *body
                            && prior.reminder == *reminder;
                        if exact {
                            continue;
                        }
                        projection.reject(event.seq, "conflicting producer idempotency key");
                        continue;
                    }
                    if message_indexes.contains_key(message_id) {
                        projection.reject(event.seq, "conflicting producer message id");
                        continue;
                    }
                    let index = projection.messages.len();
                    projection.messages.push(ProducerMessageRecord {
                        message_id: *message_id,
                        producer_owner: producer_owner.clone(),
                        mode: *mode,
                        idempotency_key: idempotency_key.clone(),
                        description: description.clone(),
                        body: body.clone(),
                        reminder: *reminder,
                        agent_hop: *agent_hop,
                        accepted_seq: event.seq,
                        admission: None,
                        claims: HashSet::new(),
                        consumed: false,
                        discarded: false,
                        discarded_seq: None,
                        consumption_recorded: false,
                        consumed_run: None,
                    });
                    message_indexes.insert(*message_id, index);
                    dedup_indexes.insert(dedup_key, index);
                }
                EventPayload::ProducerMessageAdmitted { message_id } => {
                    let Some(run_id) = event.run_id else {
                        projection.reject(event.seq, "producer admission is missing run id");
                        continue;
                    };
                    if terminal_runs.contains(&run_id) {
                        projection.reject(event.seq, "producer admission targets a terminal run");
                        continue;
                    }
                    let Some(index) = message_indexes.get(message_id).copied() else {
                        projection.reject(event.seq, "producer admission has no accepted message");
                        continue;
                    };
                    let message = &projection.messages[index];
                    let replaceable = message.admission.is_some_and(|(prior_run, _)| {
                        prior_run != run_id && terminal_runs.contains(&prior_run)
                    });
                    if message.consumed
                        || message.discarded_seq.is_some()
                        || (message.admission.is_some() && !replaceable)
                    {
                        projection.reject(event.seq, "invalid producer admission");
                        continue;
                    }
                    projection.messages[index].admission = Some((run_id, event.seq));
                }
                EventPayload::ProducerMessagesClaimed { message_ids } => {
                    let Some(run_id) = event.run_id else {
                        projection.reject(event.seq, "producer claim is missing run id");
                        continue;
                    };
                    let unique = message_ids.iter().copied().collect::<HashSet<_>>();
                    let indexes = message_ids
                        .iter()
                        .filter_map(|message_id| message_indexes.get(message_id).copied())
                        .collect::<Vec<_>>();
                    let valid = !message_ids.is_empty()
                        && unique.len() == message_ids.len()
                        && indexes.len() == message_ids.len()
                        && !projection.claims.contains_key(&event.seq)
                        && indexes.iter().all(|index| {
                            let message = &projection.messages[*index];
                            message.accepted_seq < event.seq
                                && !message.consumed
                                && message.discarded_seq.is_none()
                                && message.admission.is_some_and(
                                    |(admission_run, admission_seq)| {
                                        admission_run == run_id && admission_seq < event.seq
                                    },
                                )
                        });
                    if !valid {
                        projection.reject(event.seq, "invalid producer claim");
                        continue;
                    }
                    for index in indexes {
                        projection.messages[index].claims.insert(event.seq);
                    }
                    projection.claims.insert(
                        event.seq,
                        ProducerClaimRecord {
                            run_id,
                            message_ids: message_ids.clone(),
                        },
                    );
                }
                EventPayload::ProducerMessagesReleased { claim_seq } => {
                    let Some(run_id) = event.run_id else {
                        projection.reject(event.seq, "producer release is missing run id");
                        continue;
                    };
                    let Some(claim) = projection.claims.get(claim_seq) else {
                        projection.reject(event.seq, "producer release has no claim");
                        continue;
                    };
                    if *claim_seq == 0 || claim.run_id != run_id {
                        projection.reject(event.seq, "producer release ownership is invalid");
                        continue;
                    }
                    let message_ids = claim.message_ids.clone();
                    for message_id in &message_ids {
                        if let Some(index) = message_indexes.get(message_id) {
                            projection.messages[*index].claims.remove(claim_seq);
                        }
                    }
                    projection.claims.remove(claim_seq);
                }
                EventPayload::ModelTurnCommitted {
                    input_through_seq, ..
                } => {
                    let Some(run_id) = event.run_id else { continue };
                    for message in &mut projection.messages {
                        if !message.consumed
                            && message
                                .admission
                                .is_some_and(|(admission_run, admission_seq)| {
                                    admission_run == run_id && admission_seq <= *input_through_seq
                                })
                            && message
                                .discarded_seq
                                .is_none_or(|discarded_seq| discarded_seq > *input_through_seq)
                        {
                            message.discarded = false;
                            message.discarded_seq = None;
                            message.consumed = true;
                            message.consumed_run = Some(run_id);
                        }
                    }
                }
                EventPayload::ProducerMessageConsumed { message_id, run_id } => {
                    let Some(index) = message_indexes.get(message_id).copied() else {
                        projection
                            .reject(event.seq, "producer consumption has no accepted message");
                        continue;
                    };
                    let message = &projection.messages[index];
                    if event.run_id != Some(*run_id)
                        || message.admission.map(|(run, _)| run) != Some(*run_id)
                        || message.consumed_run != Some(*run_id)
                        || message.consumption_recorded
                    {
                        projection.reject(event.seq, "invalid producer consumption marker");
                        continue;
                    }
                    projection.messages[index].consumption_recorded = true;
                }
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder,
                    producer_owner,
                } => {
                    let Some(index) = message_indexes.get(message_id).copied() else {
                        projection.reject(event.seq, "producer discard has no accepted message");
                        continue;
                    };
                    let message = &projection.messages[index];
                    let identity_matches = producer_owner
                        .as_ref()
                        .is_some_and(|owner| owner == &message.producer_owner)
                        && reminder
                            .as_ref()
                            .is_none_or(|identity| message.reminder.as_ref() == Some(identity));
                    let legacy_matches = producer_owner.is_none()
                        && reminder
                            .as_ref()
                            .is_some_and(|identity| message.reminder.as_ref() == Some(identity));
                    if (!identity_matches && !legacy_matches)
                        || message.consumed
                        || !message.claims.is_empty()
                    {
                        projection.reject(event.seq, "invalid producer message discard");
                        continue;
                    }
                    if message.discarded_seq.is_none() {
                        projection.messages[index].discarded = true;
                        projection.messages[index].discarded_seq = Some(event.seq);
                    }
                }
                EventPayload::RunCompleted { .. }
                | EventPayload::RunFailed { .. }
                | EventPayload::RunCancelled { .. }
                | EventPayload::RunInterrupted { .. } => {
                    if let Some(run_id) = event.run_id {
                        terminal_runs.insert(run_id);
                    }
                }
                _ => {}
            }
        }
        projection
    }

    fn reject(&mut self, seq: u64, reason: &str) {
        self.invalid.push((seq, reason.to_owned()));
    }
}

fn is_terminal(status: GoalStatus) -> bool {
    matches!(status, GoalStatus::Completed | GoalStatus::Cancelled)
}

fn valid_lifecycle_change(goal: &GoalState, status: GoalStatus) -> bool {
    match (goal.status, status) {
        (GoalStatus::Active, GoalStatus::Paused | GoalStatus::Cancelled)
        | (GoalStatus::Paused, GoalStatus::Active | GoalStatus::Cancelled) => true,
        (GoalStatus::Active | GoalStatus::Paused, GoalStatus::Completed) => {
            !goal.items.is_empty() && goal.items.iter().all(|item| item.finished)
        }
        _ => false,
    }
}

fn valid_reminder_owner(owner: &ProducerOwner, reminder: Option<&GoalReminderIdentity>) -> bool {
    if matches!(owner, ProducerOwner::Plugin { plugin } if plugin.trim().is_empty()) {
        return false;
    }
    match (owner, reminder) {
        (ProducerOwner::Goal { goal_id }, Some(reminder)) => *goal_id == reminder.goal_id,
        (ProducerOwner::Goal { .. }, None) | (_, Some(_)) => false,
        (_, None) => true,
    }
}

/// Hop metadata belongs exclusively to agent mail. A malformed durable event
/// that stamps it onto plugin, goal, or delegation mail is quarantined so it
/// cannot inflate an agent chain it never joined.
const fn valid_hop_owner(owner: &ProducerOwner, agent_hop: Option<u32>) -> bool {
    agent_hop.is_none() || matches!(owner, ProducerOwner::Agent { .. })
}

#[cfg(test)]
mod tests;
