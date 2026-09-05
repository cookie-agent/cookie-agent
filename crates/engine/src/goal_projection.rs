//! Durable goal and producer-message projection.

use std::collections::{HashMap, HashSet};

use cookie_agent_protocol::{
    EventPayload, GoalReminderIdentity, GoalState, GoalStatus, ProducerDeliveryMode,
    ProducerIdempotencyKey, ProducerMessageId, ProducerOwner, RunId, RunSelection, StoredEvent,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProducerMessageRecord {
    pub message_id: ProducerMessageId,
    pub producer_owner: ProducerOwner,
    pub mode: ProducerDeliveryMode,
    pub idempotency_key: ProducerIdempotencyKey,
    pub body: String,
    pub reminder: Option<GoalReminderIdentity>,
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
                    body,
                    reminder,
                } => {
                    if !valid_reminder_owner(producer_owner, reminder.as_ref()) {
                        projection.reject(event.seq, "invalid producer reminder ownership");
                        continue;
                    }
                    let dedup_key = (producer_owner.clone(), idempotency_key.clone());
                    if let Some(index) = dedup_indexes.get(&dedup_key).copied() {
                        let prior = &projection.messages[index];
                        let exact = prior.message_id == *message_id
                            && prior.mode == *mode
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
                        body: body.clone(),
                        reminder: *reminder,
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use cookie_agent_protocol::{
        EventPayload, GoalId, GoalItem, GoalReminderIdentity, GoalStatus, ModelFinishReason,
        PersistedModelTurn, ProducerDeliveryMode, ProducerIdempotencyKey, ProducerMessageId,
        ProducerOwner, RunId, SessionId, StoredEvent, Usage,
    };

    use super::GoalProducerProjection;

    fn event(seq: u64, run_id: Option<RunId>, payload: EventPayload) -> StoredEvent {
        StoredEvent {
            engine_version: None,
            origin: None,
            session_id: SessionId(uuid::Uuid::from_u128(1)),
            run_id,
            seq,
            timestamp: jiff::Timestamp::new(seq as i64, 0).unwrap(),
            payload,
        }
    }

    fn accepted(
        seq: u64,
        message_id: ProducerMessageId,
        owner: ProducerOwner,
        key: &str,
        body: &str,
        reminder: Option<GoalReminderIdentity>,
    ) -> StoredEvent {
        event(
            seq,
            None,
            EventPayload::ProducerMessageAccepted {
                message_id,
                producer_owner: owner,
                mode: ProducerDeliveryMode::Steer,
                idempotency_key: ProducerIdempotencyKey::new(key).unwrap(),
                body: body.into(),
                reminder,
            },
        )
    }

    #[test]
    fn goal_replay_quarantines_invalid_events_without_mutating_state() {
        let first = GoalId::new_v7();
        let replacement = GoalId::new_v7();
        let item = GoalItem {
            description: "Verify tests".into(),
            finished: true,
        };
        let events = vec![
            event(
                1,
                None,
                EventPayload::GoalActivated {
                    goal_id: first,
                    objective: "Ship".into(),
                    revision: 0,
                    selection: None,
                },
            ),
            event(
                2,
                None,
                EventPayload::GoalActivated {
                    goal_id: replacement,
                    objective: "Too soon".into(),
                    revision: 0,
                    selection: None,
                },
            ),
            event(
                3,
                None,
                EventPayload::GoalChecklistRevised {
                    goal_id: first,
                    items: vec![GoalItem {
                        description: "   ".into(),
                        finished: false,
                    }],
                    revision: 1,
                },
            ),
            event(
                4,
                None,
                EventPayload::GoalChecklistRevised {
                    goal_id: first,
                    items: vec![item.clone(), item],
                    revision: 1,
                },
            ),
            event(
                5,
                None,
                EventPayload::GoalLifecycleChanged {
                    goal_id: first,
                    status: GoalStatus::Completed,
                    revision: 2,
                    selection: None,
                },
            ),
            event(
                6,
                None,
                EventPayload::GoalChecklistRevised {
                    goal_id: first,
                    items: vec![],
                    revision: 3,
                },
            ),
            event(
                7,
                None,
                EventPayload::GoalActivated {
                    goal_id: replacement,
                    objective: "Next".into(),
                    revision: 0,
                    selection: None,
                },
            ),
        ];

        let projection = GoalProducerProjection::from_events(&events);
        let goal = projection.goal.unwrap();
        assert_eq!(goal.goal_id, replacement);
        assert_eq!(goal.objective, "Next");
        assert_eq!(goal.revision, 0);
        assert_eq!(
            projection
                .invalid
                .iter()
                .map(|entry| entry.0)
                .collect::<Vec<_>>(),
            vec![2, 3, 6]
        );
    }

    #[test]
    fn producer_replay_deduplicates_and_uses_commit_coverage_as_consumption() {
        let owner = ProducerOwner::Plugin {
            plugin: "jobs".into(),
        };
        let message_id = ProducerMessageId::new_v7();
        let run = RunId::new_v7();
        let events = vec![
            accepted(1, message_id, owner.clone(), "job:1", "result", None),
            accepted(
                2,
                ProducerMessageId::new_v7(),
                owner,
                "job:1",
                "changed",
                None,
            ),
            event(
                3,
                Some(run),
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            event(
                4,
                Some(run),
                EventPayload::ModelTurnCommitted {
                    attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                    model_turn_seq: 1,
                    resolved_model: crate::model_history::wire_model(
                        &crate::test_support::model_binding(),
                    ),
                    input_through_seq: 3,
                    turn: PersistedModelTurn {
                        content: vec![],
                        provider_options: BTreeMap::new(),
                        finish_reason: ModelFinishReason::Stop,
                        usage: Usage::default(),
                        response_metadata: BTreeMap::new(),
                        provider_metadata: BTreeMap::new(),
                        native_replay: None,
                    },
                    warnings: vec![],
                },
            ),
        ];

        let projection = GoalProducerProjection::from_events(&events);
        assert_eq!(projection.messages.len(), 1);
        assert_eq!(projection.messages[0].admission, Some((run, 3)));
        assert!(projection.messages[0].consumed);
        assert_eq!(projection.messages[0].consumed_run, Some(run));
        assert!(!projection.messages[0].consumption_recorded);
        assert_eq!(projection.invalid.len(), 1);
    }

    #[test]
    fn interrupted_admission_is_replaced_but_committed_admission_is_final() {
        let owner = ProducerOwner::Plugin {
            plugin: "jobs".into(),
        };
        let message_id = ProducerMessageId::new_v7();
        let first = RunId::new_v7();
        let retry = RunId::new_v7();
        let events = vec![
            accepted(1, message_id, owner, "job:2", "result", None),
            event(
                2,
                Some(first),
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            event(
                3,
                Some(first),
                EventPayload::RunInterrupted { reason: None },
            ),
            event(
                4,
                Some(retry),
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
        ];
        let projection = GoalProducerProjection::from_events(&events);
        assert_eq!(projection.messages[0].admission, Some((retry, 4)));
        assert!(!projection.messages[0].consumed);
    }

    #[test]
    fn legacy_goal_discard_matches_the_accepted_reminder() {
        let goal_id = GoalId::new_v7();
        let reminder = GoalReminderIdentity {
            goal_id,
            revision: 4,
        };
        let message_id = ProducerMessageId::new_v7();
        let events = vec![
            accepted(
                1,
                ProducerMessageId::new_v7(),
                ProducerOwner::Goal { goal_id },
                "bad",
                "body",
                None,
            ),
            accepted(
                2,
                message_id,
                ProducerOwner::Goal { goal_id },
                "good",
                "body",
                Some(reminder),
            ),
            event(
                3,
                None,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: Some(reminder),
                    producer_owner: None,
                },
            ),
        ];
        let projection = GoalProducerProjection::from_events(&events);
        assert_eq!(projection.messages.len(), 1);
        assert_eq!(projection.messages[0].discarded_seq, Some(3));
        assert_eq!(projection.invalid.len(), 1);
    }

    #[test]
    fn real_discard_requires_the_accepted_owner_and_is_idempotent() {
        let owner = ProducerOwner::Plugin {
            plugin: "jobs".into(),
        };
        let message_id = ProducerMessageId::new_v7();
        let events = vec![
            accepted(1, message_id, owner.clone(), "job:3", "result", None),
            event(
                2,
                None,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: None,
                    producer_owner: Some(ProducerOwner::Plugin {
                        plugin: "other".into(),
                    }),
                },
            ),
            event(
                3,
                None,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: None,
                    producer_owner: None,
                },
            ),
            event(
                4,
                None,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: None,
                    producer_owner: Some(owner.clone()),
                },
            ),
            event(
                5,
                None,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: None,
                    producer_owner: Some(owner),
                },
            ),
        ];

        let projection = GoalProducerProjection::from_events(&events);
        assert_eq!(projection.messages[0].discarded_seq, Some(4));
        assert_eq!(
            projection.invalid,
            vec![
                (2, "invalid producer message discard".into()),
                (3, "invalid producer message discard".into())
            ]
        );
    }

    #[test]
    fn claim_blocks_discard_until_matching_release() {
        let owner = ProducerOwner::Plugin {
            plugin: "jobs".into(),
        };
        let message_id = ProducerMessageId::new_v7();
        let run = RunId::new_v7();
        let events = vec![
            accepted(1, message_id, owner.clone(), "job:4", "result", None),
            event(
                2,
                Some(run),
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            event(
                3,
                Some(run),
                EventPayload::ProducerMessagesClaimed {
                    message_ids: vec![message_id],
                },
            ),
            event(
                4,
                None,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: None,
                    producer_owner: Some(owner.clone()),
                },
            ),
            event(
                5,
                Some(run),
                EventPayload::ProducerMessagesReleased { claim_seq: 3 },
            ),
            event(
                6,
                None,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: None,
                    producer_owner: Some(owner),
                },
            ),
        ];

        let projection = GoalProducerProjection::from_events(&events);
        assert!(projection.claims.is_empty());
        assert!(projection.messages[0].claims.is_empty());
        assert_eq!(projection.messages[0].discarded_seq, Some(6));
        assert_eq!(
            projection.invalid,
            vec![(4, "invalid producer message discard".into())]
        );
    }

    #[test]
    fn terminal_run_does_not_clear_recovered_claim_state() {
        let message_id = ProducerMessageId::new_v7();
        let run = RunId::new_v7();
        let events = vec![
            accepted(
                1,
                message_id,
                ProducerOwner::Plugin {
                    plugin: "jobs".into(),
                },
                "job:5",
                "result",
                None,
            ),
            event(
                2,
                Some(run),
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            event(
                3,
                Some(run),
                EventPayload::ProducerMessagesClaimed {
                    message_ids: vec![message_id],
                },
            ),
            event(4, Some(run), EventPayload::RunCancelled { reason: None }),
        ];

        let projection = GoalProducerProjection::from_events(&events);
        assert_eq!(projection.claims[&3].run_id, run);
        assert_eq!(projection.claims[&3].message_ids, vec![message_id]);
        assert_eq!(projection.messages[0].claims, HashSet::from([3]));
    }

    #[test]
    fn discard_before_commit_coverage_is_not_resurrected() {
        let owner = ProducerOwner::Plugin {
            plugin: "jobs".into(),
        };
        let message_id = ProducerMessageId::new_v7();
        let run = RunId::new_v7();
        let events = vec![
            accepted(1, message_id, owner.clone(), "job:6", "result", None),
            event(
                2,
                Some(run),
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            event(
                3,
                None,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: None,
                    producer_owner: Some(owner),
                },
            ),
            committed(4, run, 3),
        ];

        let projection = GoalProducerProjection::from_events(&events);
        assert!(!projection.messages[0].consumed);
        assert_eq!(projection.messages[0].discarded_seq, Some(3));
    }

    #[test]
    fn legacy_discard_after_covered_admission_is_consumed_and_cleared() {
        let goal_id = GoalId::new_v7();
        let reminder = GoalReminderIdentity {
            goal_id,
            revision: 1,
        };
        let message_id = ProducerMessageId::new_v7();
        let run = RunId::new_v7();
        let events = vec![
            accepted(
                1,
                message_id,
                ProducerOwner::Goal { goal_id },
                "goal:1",
                "reminder",
                Some(reminder),
            ),
            event(
                2,
                Some(run),
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            event(
                3,
                None,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: Some(reminder),
                    producer_owner: None,
                },
            ),
            committed(4, run, 2),
        ];

        let projection = GoalProducerProjection::from_events(&events);
        assert!(projection.messages[0].consumed);
        assert_eq!(projection.messages[0].consumed_run, Some(run));
        assert_eq!(projection.messages[0].discarded_seq, None);
    }

    fn committed(seq: u64, run: RunId, input_through_seq: u64) -> StoredEvent {
        event(
            seq,
            Some(run),
            EventPayload::ModelTurnCommitted {
                attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                model_turn_seq: 1,
                resolved_model: crate::model_history::wire_model(
                    &crate::test_support::model_binding(),
                ),
                input_through_seq,
                turn: PersistedModelTurn {
                    content: vec![],
                    provider_options: BTreeMap::new(),
                    finish_reason: ModelFinishReason::Stop,
                    usage: Usage::default(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: vec![],
            },
        )
    }
}
