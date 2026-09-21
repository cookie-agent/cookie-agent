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
            description: cookie_agent_protocol::SafeDisplayText::new("Producer result").unwrap(),
            body: body.into(),
            reminder,
            agent_hop: None,
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
        kind: Default::default(),
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
        kind: Default::default(),
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

#[test]
fn reminder_kind_requires_committed_goal_input_not_acceptance_claims_or_controls() {
    use cookie_agent_protocol::GoalReminderKind::{Continuation, Started};

    let goal_id = GoalId::new_v7();
    let run = RunId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let control_id = ProducerMessageId::new_v7();
    let mut events = vec![
        accepted(
            1,
            control_id,
            ProducerOwner::GoalControl { goal_id },
            "pause",
            "paused",
            None,
        ),
        event(
            2,
            Some(run),
            EventPayload::ProducerMessageAdmitted {
                message_id: control_id,
            },
        ),
        committed(3, run, 2),
        accepted(
            4,
            message_id,
            ProducerOwner::Goal { goal_id },
            "first",
            "Goal started",
            Some(GoalReminderIdentity {
                goal_id,
                revision: 0,
                kind: Started,
            }),
        ),
        event(
            5,
            Some(run),
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
        event(
            6,
            Some(run),
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![message_id],
            },
        ),
    ];
    let projection = GoalProducerProjection::from_events(&events);
    assert!(projection.invalid.is_empty());
    assert!(projection.messages[0].consumed);
    assert_eq!(projection.next_reminder_kind(goal_id), Started);

    events.push(committed(7, run, 2));
    assert_eq!(
        GoalProducerProjection::from_events(&events).next_reminder_kind(goal_id),
        Started
    );
    events.push(committed(8, run, 6));
    let projection = GoalProducerProjection::from_events(&events);
    assert!(projection.invalid.is_empty());
    assert!(!projection.messages[1].consumption_recorded);
    assert_eq!(projection.next_reminder_kind(goal_id), Continuation);
    assert_eq!(projection.next_reminder_kind(GoalId::new_v7()), Started);
}

#[test]
fn agent_hop_round_trips_and_invalid_owner_is_quarantined() {
    let message_id = ProducerMessageId::new_v7();
    let run = RunId::new_v7();
    let owner = ProducerOwner::Agent {
        session_id: SessionId::new_v7(),
    };
    let mut accepted_event = accepted(1, message_id, owner, "hop", "mail", None);
    if let EventPayload::ProducerMessageAccepted { agent_hop, .. } = &mut accepted_event.payload {
        *agent_hop = Some(2);
    }
    let projection = GoalProducerProjection::from_events(&[accepted_event]);
    assert_eq!(projection.messages[0].agent_hop, Some(2));
    let mut invalid = accepted(
        2,
        ProducerMessageId::new_v7(),
        ProducerOwner::Plugin { plugin: "x".into() },
        "bad",
        "mail",
        None,
    );
    if let EventPayload::ProducerMessageAccepted { agent_hop, .. } = &mut invalid.payload {
        *agent_hop = Some(1);
    }
    assert_eq!(
        GoalProducerProjection::from_events(&[invalid])
            .invalid
            .len(),
        1
    );
    assert_eq!(projection.inherited_agent_hop(run), 0);
}

#[test]
fn agent_hop_increments_only_for_the_run_that_observed_agent_mail() {
    let agent = ProducerOwner::Agent {
        session_id: SessionId::new_v7(),
    };
    let other_agent = ProducerOwner::Agent {
        session_id: SessionId::new_v7(),
    };
    let observed = RunId::new_v7();
    let unrelated = RunId::new_v7();
    let deep = ProducerMessageId::new_v7();
    let legacy = ProducerMessageId::new_v7();
    let unobserved = ProducerMessageId::new_v7();
    let tool_result = ProducerMessageId::new_v7();
    let projection = GoalProducerProjection::from_events(&[
        accepted_hop(1, deep, &agent, "deep", "chain mail", Some(2)),
        admitted(2, observed, deep),
        // Mail accepted before hop counting existed inherits from depth 0.
        accepted_hop(3, legacy, &other_agent, "legacy", "older mail", None),
        admitted(4, unrelated, legacy),
        // A third agent's deep chain is not this session's observation.
        accepted_hop(5, unobserved, &agent, "unseen", "never observed", Some(9)),
        // Non-agent producers cannot join an agent chain, hop metadata or not.
        accepted(
            6,
            tool_result,
            ProducerOwner::Plugin {
                plugin: "mcp".into(),
            },
            "plugin",
            "tool result",
            None,
        ),
        admitted(7, observed, tool_result),
    ]);
    // Depth is the deepest agent mail the run durably saw, plus one hop.
    assert_eq!(projection.inherited_agent_hop(observed), 3);
    assert_eq!(projection.inherited_agent_hop(unrelated), 1);
    // A run that observed nothing sends at the head of a chain.
    assert_eq!(projection.inherited_agent_hop(RunId::new_v7()), 0);
    assert!(projection.invalid.is_empty());
}

#[test]
fn agent_hop_counts_a_live_claim_after_its_admission_moves_to_a_new_run() {
    let agent = ProducerOwner::Agent {
        session_id: SessionId::new_v7(),
    };
    let first = RunId::new_v7();
    let replacement = RunId::new_v7();
    let deep = ProducerMessageId::new_v7();
    let projection = GoalProducerProjection::from_events(&[
        accepted_hop(1, deep, &agent, "queued", "queued mail", Some(5)),
        admitted(2, first, deep),
        claimed(3, first, &[deep]),
        event(
            4,
            Some(first),
            EventPayload::RunCompleted { final_text: None },
        ),
        // The unreleased claim survives even though the admission now
        // belongs to the replacement run.
        admitted(5, replacement, deep),
    ]);
    assert!(projection.invalid.is_empty());
    assert_eq!(projection.messages[0].admission, Some((replacement, 5)));
    // Claim-only observation still deepens the claiming run's chain.
    assert_eq!(projection.inherited_agent_hop(first), 6);
    assert_eq!(projection.inherited_agent_hop(replacement), 6);
    assert_eq!(projection.inherited_agent_hop(RunId::new_v7()), 0);
}

#[test]
fn agent_hop_survives_claim_release_and_counts_committed_consumption() {
    let agent = ProducerOwner::Agent {
        session_id: SessionId::new_v7(),
    };
    let run = RunId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let mut events = vec![
        accepted_hop(1, message_id, &agent, "steered", "steered mail", Some(3)),
        admitted(2, run, message_id),
        claimed(3, run, &[message_id]),
    ];
    let claimed_only = GoalProducerProjection::from_events(&events);
    assert_eq!(claimed_only.inherited_agent_hop(run), 4);
    // Releasing the claim does not reset the chain: the run durably saw the
    // mail it released.
    events.push(released(4, run, 3));
    let released = GoalProducerProjection::from_events(&events);
    assert!(released.invalid.is_empty());
    assert!(released.claims.is_empty());
    assert_eq!(released.inherited_agent_hop(run), 4);
    // Committed input coverage is the durable consumption signal.
    let committed_projection = GoalProducerProjection::from_events(&[
        accepted_hop(1, message_id, &agent, "read", "consumed mail", Some(3)),
        admitted(2, run, message_id),
        committed(3, run, 2),
    ]);
    assert!(committed_projection.invalid.is_empty());
    assert!(committed_projection.messages[0].consumed);
    assert_eq!(committed_projection.messages[0].consumed_run, Some(run));
    assert_eq!(committed_projection.inherited_agent_hop(run), 4);
    assert_eq!(committed_projection.inherited_agent_hop(RunId::new_v7()), 0);
}

#[test]
fn agent_hop_ignores_discarded_mail_the_recipient_never_read() {
    let agent = ProducerOwner::Agent {
        session_id: SessionId::new_v7(),
    };
    let run = RunId::new_v7();
    let discarded = ProducerMessageId::new_v7();
    let projection = GoalProducerProjection::from_events(&[
        accepted_hop(1, discarded, &agent, "gone", "purged mail", Some(5)),
        discarded_event(2, discarded, &agent),
        accepted_hop(
            3,
            ProducerMessageId::new_v7(),
            &agent,
            "live",
            "unread mail",
            Some(1),
        ),
    ]);
    assert!(projection.invalid.is_empty());
    assert!(projection.messages[0].discarded);
    // Neither an accepted-but-unread chain nor a purged one deepens it.
    assert_eq!(projection.inherited_agent_hop(run), 0);
}

fn accepted_hop(
    seq: u64,
    message_id: ProducerMessageId,
    owner: &ProducerOwner,
    key: &str,
    body: &str,
    agent_hop: Option<u32>,
) -> StoredEvent {
    event(
        seq,
        None,
        EventPayload::ProducerMessageAccepted {
            message_id,
            producer_owner: owner.clone(),
            mode: ProducerDeliveryMode::Steer,
            idempotency_key: ProducerIdempotencyKey::new(key).unwrap(),
            description: cookie_agent_protocol::SafeDisplayText::new("Producer result").unwrap(),
            body: body.into(),
            reminder: None,
            agent_hop,
        },
    )
}

fn admitted(seq: u64, run: RunId, message_id: ProducerMessageId) -> StoredEvent {
    event(
        seq,
        Some(run),
        EventPayload::ProducerMessageAdmitted { message_id },
    )
}

fn claimed(seq: u64, run: RunId, message_ids: &[ProducerMessageId]) -> StoredEvent {
    event(
        seq,
        Some(run),
        EventPayload::ProducerMessagesClaimed {
            message_ids: message_ids.to_vec(),
        },
    )
}

fn released(seq: u64, run: RunId, claim_seq: u64) -> StoredEvent {
    event(
        seq,
        Some(run),
        EventPayload::ProducerMessagesReleased { claim_seq },
    )
}

fn discarded_event(seq: u64, message_id: ProducerMessageId, owner: &ProducerOwner) -> StoredEvent {
    event(
        seq,
        None,
        EventPayload::ProducerMessageDiscarded {
            message_id,
            reminder: None,
            producer_owner: Some(owner.clone()),
        },
    )
}

fn committed(seq: u64, run: RunId, input_through_seq: u64) -> StoredEvent {
    event(
        seq,
        Some(run),
        EventPayload::ModelTurnCommitted {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            model_turn_seq: 1,
            resolved_model: crate::model_history::wire_model(&crate::test_support::model_binding()),
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
