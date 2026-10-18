use crate::ui::transcript::*;

use cookie_agent_protocol::{
    AttemptId, EventPayload, GoalId, InvocationId, ProducerMessageId, RunId, SessionId, StoredEvent,
};

use jiff::Timestamp;

use crate::markdown::PlainHighlighter;

use crate::state::{SessionState, StateStore};

use super::support::*;

#[test]
fn producer_rows_collapse_to_summary_and_expand_to_model_body() {
    let session = SessionId::new_v7();
    let first = ProducerMessageId::new_v7();
    let second = ProducerMessageId::new_v7();
    let mut store = StateStore::default();
    for (seq, message_id) in [(1, first), (2, second)] {
        let mut event = producer_accepted(
            session,
            seq,
            message_id,
            ProducerOwner::external(
                cookie_agent_protocol::ProducerSource::plugin("build").unwrap(),
                cookie_agent_protocol::ProducerName::default_name(),
            ),
            ProducerDeliveryMode::Queue,
            &format!("ACTUAL MODEL BODY\n{}", "bounded content\n".repeat(1000)),
            None,
        );
        let EventPayload::ProducerMessageAccepted { description, .. } = &mut event.payload else {
            unreachable!()
        };
        *description =
            cookie_agent_protocol::SafeDisplayText::new("Build completed: parser").unwrap();
        assert!(store.apply_event(event));
    }
    let state = store.sessions.get_mut(&session).unwrap();
    for item in &mut state.transcript {
        let TranscriptItem::ProducerMessage {
            status, summary, ..
        } = item
        else {
            unreachable!()
        };
        assert_eq!(summary.as_deref(), Some("Build completed: parser"));
        *status = ProducerMessageStatus::Consumed;
    }
    let mut cache = LayoutCache::default();
    let mut expanded = HashSet::new();
    let render = |cache: &mut LayoutCache, expanded: &HashSet<BlockId>, clock| {
        ensure_cached_transcript_layout(
            cache,
            session,
            state,
            None,
            Some(expanded),
            80,
            &Theme::default(),
            &PlainHighlighter,
            crate::state::EventLevel::Info,
            clock,
            &crate::state::ModelDisplayNames::default(),
        )
    };
    render(&mut cache, &expanded, 0);
    assert_eq!(cache.item_layout_passes, 2);
    assert_eq!(
        snapshot_lines(&cache.layout.lines.to_vec()),
        "◇ ▸ Build completed: parser\n\n◇ ▸ Build completed: parser"
    );
    for (index, message_id) in [first, second].into_iter().enumerate() {
        assert_eq!(
            item_block_ids(&state.transcript[index]),
            [BlockId::ProducerMessage(message_id)]
        );
    }
    expanded.insert(BlockId::ProducerMessage(first));
    render(&mut cache, &expanded, 1);
    assert_eq!(
        cache.item_layout_passes, 3,
        "only the toggled producer is laid out again"
    );
    let text = snapshot_lines(&cache.layout.lines.to_vec());
    assert!(text.starts_with(
        "◇ ▾ Build completed: parser\n· plugin:build · default · queue · consumed\n· ACTUAL MODEL BODY"
    ));
    assert_eq!(text.matches("ACTUAL MODEL BODY").count(), 1);
    let region = cache
        .layout
        .regions
        .iter()
        .find(|region| region.id == BlockId::ProducerMessage(first))
        .unwrap();
    assert_eq!(region.header_lines, Some(1));
    assert!(
        block_hit(
            *region,
            &cache.layout.lines.to_vec(),
            Rect::new(0, 0, 80, 20),
            region.start_line + 1,
        )
        .unwrap()
        .hover_rect
        .is_none()
    );
    assert!(text.ends_with("◇ ▸ Build completed: parser"));
    assert!(cache.layout.lines.len() <= MAX_EXPANDED_BODY_LINES + 6);
    assert!(text.len() < MAX_EXPANDED_BODY_BYTES);
    assert!(render(&mut cache, &expanded, 2));
    assert_eq!(
        cache.item_layout_passes, 3,
        "settled rows ignore the animation clock"
    );
    for width in [1, 3, 7, 8, 20, 80] {
        for blocks in [None, Some(&expanded)] {
            let layout = transcript_layout(state, blocks, width);
            assert!(
                layout
                    .lines
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
            if blocks.is_none() {
                assert_eq!(layout.lines.len(), 3, "collapsed rows never wrap");
            }
        }
    }
    let owner = ProducerOwner::Goal {
        goal_id: GoalId::new_v7(),
    };
    assert_eq!(
        producer_summary(
            &owner,
            ProducerDeliveryMode::Steer,
            Some("  Goal started: parser\nwork  ")
        ),
        "Goal started: parser work"
    );
    assert_eq!(
        producer_summary(&owner, ProducerDeliveryMode::Steer, Some("")),
        "goal controller · steer"
    );
}

#[test]
fn goal_and_producer_rows_fit_narrow_viewports() {
    let goal = test_goal(
        GoalStatus::Active,
        vec![goal_item(
            "A checklist description that must wrap safely",
            false,
        )],
    );
    let mut state = SessionState {
        goal: Some(goal.clone()),
        ..SessionState::default()
    };
    state.transcript = vec![
        TranscriptItem::Goal {
            id: 1,
            seq: 1,
            activation: false,
            goal,
        },
        TranscriptItem::ProducerMessage {
            summary: None,
            id: 2,
            seq: 2,
            accepted_at: Timestamp::now(),
            message_id: ProducerMessageId::new_v7(),
            producer_owner: ProducerOwner::external(
                cookie_agent_protocol::ProducerSource::plugin("build-monitor").unwrap(),
                cookie_agent_protocol::ProducerName::default_name(),
            ),
            mode: ProducerDeliveryMode::Queue,
            body: "A producer body that wraps instead of widening the pane".to_owned(),
            reminder: None,
            status: crate::state::ProducerMessageStatus::Consumed,
        },
    ];
    for width in [1, 3, 7, 8, 20, 40, 80] {
        let layout = transcript_layout(&state, None, width);
        assert!(
            layout
                .lines
                .iter()
                .all(|line| line.width() <= usize::from(width)),
            "width {width}: {}",
            snapshot_lines(&layout.lines)
        );
        assert!(layout.user_regions.is_empty());
    }
}

#[test]
fn goal_control_messages_move_from_queue_to_transcript_when_consumed() {
    for body in [
        "Goal paused. Stop pursuing the objective.",
        "Goal cancelled. Stop pursuing the objective.",
    ] {
        let session_id = SessionId::new_v7();
        let message_id = ProducerMessageId::new_v7();
        let mut store = StateStore::default();
        let accepted = StoredEvent {
            engine_version: None,
            origin: None,
            session_id,
            run_id: None,
            seq: 1,
            timestamp: jiff::Timestamp::now(),
            payload: EventPayload::ProducerMessageAccepted {
                description: Default::default(),
                message_id,
                producer_owner: ProducerOwner::GoalControl {
                    goal_id: GoalId::new_v7(),
                },
                mode: ProducerDeliveryMode::Steer,
                idempotency_key: cookie_agent_protocol::ProducerIdempotencyKey::new("control")
                    .unwrap(),
                body: body.into(),
                reminder: None,
                agent_hop: None,
                retained_output: None,
            },
        };
        assert!(store.apply_event(accepted.clone()));
        for status in [
            crate::state::ProducerMessageStatus::Pending,
            crate::state::ProducerMessageStatus::Admitted,
        ] {
            if status == crate::state::ProducerMessageStatus::Admitted {
                assert!(store.apply_event(StoredEvent {
                    run_id: Some(RunId::new_v7()),
                    seq: 2,
                    payload: EventPayload::ProducerMessageAdmitted { message_id },
                    ..accepted.clone()
                }));
            }
            let state = &store.sessions[&session_id];
            let layout = transcript_layout_with_level(
                state,
                None,
                80,
                &Theme::default(),
                &PlainHighlighter,
                crate::state::EventLevel::Error,
            );
            let rendered = snapshot_lines(&layout.lines);
            assert!(rendered.is_empty(), "{status:?}: {rendered}");
            assert_eq!(state.transcript.len(), 1);
            assert!(layout.user_regions.is_empty());
        }
        let state = store.sessions.get_mut(&session_id).expect("session");
        let TranscriptItem::ProducerMessage { status, .. } = &mut state.transcript[0] else {
            panic!("producer transcript item")
        };
        *status = crate::state::ProducerMessageStatus::Consumed;
        let rendered = snapshot_lines(
            &transcript_layout_with_level(
                state,
                None,
                80,
                &Theme::default(),
                &PlainHighlighter,
                crate::state::EventLevel::Error,
            )
            .lines,
        );
        assert!(!rendered.contains(body), "{rendered}");
        assert!(rendered.contains("◇ ▸ goal control · steer"));
        let expanded = HashSet::from([BlockId::ProducerMessage(message_id)]);
        let rendered = snapshot_lines(&transcript_layout(state, Some(&expanded), 80).lines);
        assert_eq!(rendered.matches(body).count(), 1, "{rendered}");
        assert!(rendered.contains("· goal control · steer · consumed"));
        assert!(!rendered.contains("Continue"));
    }
}

#[test]
fn producer_lifecycle_renders_claimed_and_consumed_content_and_debug_discard_diagnostic() {
    let goal = test_goal(GoalStatus::Active, vec![goal_item("Keep going", false)]);
    for status in [
        crate::state::ProducerMessageStatus::Pending,
        crate::state::ProducerMessageStatus::Admitted,
        crate::state::ProducerMessageStatus::Claimed,
        crate::state::ProducerMessageStatus::Consumed,
        crate::state::ProducerMessageStatus::Discarded,
    ] {
        let state = SessionState {
            goal: Some(goal.clone()),
            transcript: vec![TranscriptItem::ProducerMessage {
                summary: Some(format!("GoalContinue: {}", goal.objective)),
                id: 1,
                seq: 9,
                accepted_at: Timestamp::now(),
                message_id: ProducerMessageId::new_v7(),
                producer_owner: ProducerOwner::Goal {
                    goal_id: goal.goal_id,
                },
                mode: ProducerDeliveryMode::Steer,
                body: "FULL REMINDER BODY RENDERS WHEN EXPANDED".to_owned(),
                reminder: Some(cookie_agent_protocol::GoalReminderIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                    kind: cookie_agent_protocol::GoalReminderKind::Continuation,
                }),
                status,
            }],
            ..SessionState::default()
        };
        let layout = transcript_layout_with_level(
            &state,
            None,
            60,
            &Theme::default(),
            &PlainHighlighter,
            crate::state::EventLevel::Info,
        );
        let rendered = snapshot_lines(&layout.lines);
        let label = match status {
            crate::state::ProducerMessageStatus::Claimed => Some("claimed"),
            crate::state::ProducerMessageStatus::Consumed => Some("consumed"),
            _ => None,
        };
        if let Some(label) = label {
            // Claimed already shows: the running request carries it, so it
            // must not wait for the turn to commit.
            assert_eq!(rendered.matches("◇ ▸ GoalContinue:").count(), 1);
            let expanded = HashSet::from([layout.regions[0].id]);
            let expanded = snapshot_lines(&transcript_layout(&state, Some(&expanded), 60).lines);
            assert!(expanded.contains(&format!("· goal controller · steer · {label}")));
            assert!(expanded.contains("FULL REMINDER BODY RENDERS WHEN EXPANDED"));
        } else {
            assert!(rendered.is_empty(), "{status:?}: {rendered}");
        }
        assert!(!rendered.contains("FULL REMINDER BODY"));
        assert!(layout.user_regions.is_empty());

        if status == crate::state::ProducerMessageStatus::Discarded {
            let debug = snapshot_lines(&transcript_layout(&state, None, 60).lines);
            assert!(debug.contains("producer message discarded"));
            assert!(debug.contains("goal controller · steer"));
            assert!(!debug.contains("FULL REMINDER BODY"));
            assert!(!debug.contains("Continue"));
        }
    }

    for producer_owner in [
        ProducerOwner::external(
            cookie_agent_protocol::ProducerSource::plugin("ci").unwrap(),
            cookie_agent_protocol::ProducerName::default_name(),
        ),
        ProducerOwner::Delegation {
            invocation_id: InvocationId::new_v7(),
        },
    ] {
        let message_id = ProducerMessageId::new_v7();
        let state = SessionState {
            transcript: vec![TranscriptItem::ProducerMessage {
                id: 1,
                seq: 1,
                accepted_at: Timestamp::now(),
                message_id,
                producer_owner,
                mode: ProducerDeliveryMode::Queue,
                body: "model payload".into(),
                summary: Some("Task completed".into()),
                reminder: None,
                status: ProducerMessageStatus::Consumed,
            }],
            ..SessionState::default()
        };
        let expanded = HashSet::from([BlockId::ProducerMessage(message_id)]);
        assert!(
            !snapshot_lines(&transcript_layout(&state, None, 60).lines).contains("model payload")
        );
        assert!(
            snapshot_lines(&transcript_layout(&state, Some(&expanded), 60).lines)
                .contains("model payload")
        );
    }
}

#[test]
fn cached_producer_layout_stays_hidden_until_consumed_then_stabilizes() {
    let session_id = SessionId::new_v7();
    let mut state = SessionState {
        transcript: vec![TranscriptItem::ProducerMessage {
            summary: None,
            id: 41,
            seq: 9,
            accepted_at: Timestamp::now(),
            message_id: ProducerMessageId::new_v7(),
            producer_owner: ProducerOwner::external(
                cookie_agent_protocol::ProducerSource::plugin("ci").unwrap(),
                cookie_agent_protocol::ProducerName::default_name(),
            ),
            mode: ProducerDeliveryMode::Queue,
            body: "cached payload".to_owned(),
            reminder: None,
            status: crate::state::ProducerMessageStatus::Pending,
        }],
        ..SessionState::default()
    };
    let mut cache = LayoutCache::default();
    let render = |cache: &mut LayoutCache, state: &SessionState| {
        ensure_cached_transcript_layout(
            cache,
            session_id,
            state,
            None,
            None,
            60,
            &Theme::default(),
            &PlainHighlighter,
            crate::state::EventLevel::Info,
            0,
            &crate::state::ModelDisplayNames::default(),
        )
    };

    assert!(!render(&mut cache, &state));
    assert!(cache.layout.lines.to_vec().is_empty());
    assert!(render(&mut cache, &state));
    assert_eq!(cache.item_layout_passes, 1);

    let TranscriptItem::ProducerMessage { id, status, .. } = &mut state.transcript[0] else {
        panic!("producer transcript item")
    };
    assert_eq!(*id, 41);
    *status = crate::state::ProducerMessageStatus::Consumed;
    assert!(!render(&mut cache, &state));
    assert!(
        snapshot_lines(&cache.layout.lines.to_vec()).contains("◇ ▸ plugin:ci · default · queue")
    );
    assert!(!snapshot_lines(&cache.layout.lines.to_vec()).contains("cached payload"));
    assert_eq!(cache.item_layout_passes, 2);
    assert!(render(&mut cache, &state));
    assert_eq!(cache.item_layout_passes, 2);
}

#[tokio::test]
async fn claimed_delegation_result_shows_above_the_reply_while_it_streams() {
    let (mut app, session, run) = app_with_active_run().await;
    let message_id = ProducerMessageId::new_v7();
    let attempt = AttemptId::new_v7();
    for stored in [
        producer_accepted(
            session,
            1,
            message_id,
            ProducerOwner::Delegation {
                invocation_id: InvocationId::new_v7(),
            },
            ProducerDeliveryMode::Steer,
            "delegation finished",
            None,
        ),
        event(
            session,
            2,
            run,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
        event(
            session,
            3,
            run,
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![message_id],
            },
        ),
        attempt_started(session, 4, run, attempt, None),
        text_delta(session, 5, run, attempt, "streaming reply"),
    ] {
        assert!(app.store.apply_event(stored));
    }
    // No `ModelTurnCommitted` yet: the result the reply answers is already
    // on screen, above the reply, not deferred until the turn commits.
    let layout = transcript_layout(&app.store.sessions[&session], None, 80);
    let row = layout
        .regions
        .iter()
        .find(|region| region.id == BlockId::ProducerMessage(message_id))
        .expect("claimed delegation result renders");
    let reply = layout
        .lines
        .iter()
        .position(|line| line.to_string().contains("streaming reply"))
        .expect("the reply streams");
    assert!(row.end_line <= reply, "result above the reply");
}
