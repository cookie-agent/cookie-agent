use std::time::Duration;

use crate::ui::transcript::*;

use cookie_agent_protocol::{
    AttemptId, EventPayload, GoalId, InvocationId, ProducerMessageId, RunId, SessionId, ToolCallId,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use ratatui::{Terminal, backend::TestBackend};

use crate::client::ClientDelivery;

use crate::markdown::PlainHighlighter;

use crate::state::SessionState;

use crate::ui::app::*;

use crate::ui::terminal_layout_with_tree_rows;

use cookie_agent_server::MessageFrame;

use super::support::*;

#[tokio::test]
async fn pending_lane_tracks_admit_promote_and_recall_events() {
    let (mut app, session, run) = app_with_active_run().await;
    for (seq, input) in ["alpha", "beta", "gamma"].iter().enumerate() {
        assert!(
            app.store
                .apply_event(admitted(session, seq as u64 + 1, run, input))
        );
    }
    assert_eq!(pending_texts(&app, session), ["alpha", "beta", "gamma"]);
    assert_eq!(app.queue_strip_height(), 5);

    // Promotion removes the oldest lane entry positionally and renders
    // the user row exactly once, as it always has.
    app.handle_delivery(live_event(user_input(session, 4, run, "alpha")))
        .await;
    assert_eq!(pending_texts(&app, session), ["beta", "gamma"]);
    let rendered: Vec<&str> = app.store.sessions[&session]
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(rendered, ["alpha"]);

    // Recall removes the newest entry positionally.
    app.handle_delivery(live_event(recalled(session, 5, run, "gamma")))
        .await;
    assert_eq!(pending_texts(&app, session), ["beta"]);

    // The reduction never consults payload text — promotion pops the
    // front, recall pops the back, mirroring the engine's own replay
    // exactly. A payload that names no lane entry still withdraws the
    // newest one, so nothing the engine says is gone can be stranded.
    app.handle_delivery(live_event(recalled(session, 6, run, "ghost")))
        .await;
    assert!(pending_texts(&app, session).is_empty());
    assert_eq!(app.queue_strip_height(), 0);
}

#[tokio::test]
async fn producer_started_tool_loop_promotes_steers_without_composer_restore_and_replays() {
    let session = SessionId::new_v7();
    let run = run_id();
    let message_id = ProducerMessageId::new_v7();
    let first_attempt = AttemptId::new_v7();
    let second_attempt = AttemptId::new_v7();
    let tool_call_id = ToolCallId::new_v7();
    let producer_owner = ProducerOwner::Plugin {
        plugin: "goal-scheduler".into(),
    };
    let mut first_commit = turn_committed(
        session,
        8,
        run,
        first_attempt,
        1,
        vec![tool_part("continue-goal")],
        Vec::new(),
        None,
    );
    let EventPayload::ModelTurnCommitted { turn, .. } = &mut first_commit.payload else {
        unreachable!("turn fixture")
    };
    turn.finish_reason = cookie_agent_protocol::ModelFinishReason::ToolCalls;
    let events = vec![
        session_created(session, 1),
        producer_accepted(
            session,
            2,
            message_id,
            producer_owner.clone(),
            ProducerDeliveryMode::Queue,
            "begin scheduled goal",
            None,
        ),
        run_started_with_suffix(session, 3, run, vec![resolved_model(None)]),
        // This steer is pending before producer admission. The producer
        // admission, not this lane entry, is the run's initial input.
        admitted(session, 4, run, "first steer"),
        event(
            session,
            5,
            run,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
        user_input(session, 6, run, "first steer"),
        attempt_started(session, 7, run, first_attempt, None),
        first_commit,
        event(
            session,
            9,
            run,
            EventPayload::ProducerMessageConsumed {
                message_id,
                run_id: run,
            },
        ),
        tool_started_at(
            session,
            10,
            run,
            tool_call_id,
            1,
            "continue-goal",
            0,
            "bash",
            Some("continue-goal"),
        ),
        tool_terminated(
            session,
            11,
            run,
            tool_call_id,
            1,
            "continue-goal",
            cookie_agent_protocol::ToolTerminationOutcome::Completed,
        ),
        admitted(session, 12, run, "second steer"),
        user_input(session, 13, run, "second steer"),
        attempt_started(session, 14, run, second_attempt, None),
        turn_committed(
            session,
            15,
            run,
            second_attempt,
            2,
            vec![text_part("goal complete")],
            Vec::new(),
            None,
        ),
        event(
            session,
            16,
            run,
            EventPayload::RunCompleted {
                final_text: Some("goal complete".into()),
            },
        ),
    ];
    assert!(
        !events[..5]
            .iter()
            .any(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
    );

    let mut live = test_app().await;
    live.selected = Some(session);
    live.tree_root = Some(session);
    for event in events.iter().cloned() {
        let seq = event.seq;
        live.handle_delivery(live_event(event)).await;
        if matches!(seq, 6 | 13) {
            assert!(pending_texts(&live, session).is_empty());
            assert!(live.input.as_str().is_empty());
        }
        if seq == 8 {
            assert!(live.selected_queue_entries().is_empty());
            assert_eq!(live.queue_strip_height(), 0);
        }
    }
    assert!(live.input.as_str().is_empty());
    assert!(!live.status.contains("restored to the composer"));

    let live_state = &live.store.sessions[&session];
    let producer_rows = live_state
        .transcript
        .iter()
        .filter(|item| {
            matches!(
                item,
                TranscriptItem::ProducerMessage {
                    message_id: row_id,
                    producer_owner: row_owner,
                    mode: ProducerDeliveryMode::Queue,
                    status: crate::state::ProducerMessageStatus::Consumed,
                    ..
                } if *row_id == message_id && row_owner == &producer_owner
            )
        })
        .count();
    assert_eq!(producer_rows, 1);
    assert_eq!(assistant_projection(live_state).len(), 1);
    assert_eq!(
        assistant_projection(live_state)[0].0,
        attribution(None).header()
    );
    let live_transcript = snapshot_lines(&transcript_layout(live_state, None, 100).lines);

    let mut replay = test_app().await;
    replay.selected = Some(session);
    replay.tree_root = Some(session);
    replay
        .handle_delivery(ClientDelivery::ReplayStart {
            session_id: session,
            generation: 0,
            final_seq: 16,
            rebuild: true,
        })
        .await;
    for event in events {
        replay
            .handle_delivery(ClientDelivery::ReplayEvent {
                session_id: session,
                generation: 0,
                final_seq: 16,
                event: Box::new(event),
            })
            .await;
    }
    replay
        .handle_delivery(ClientDelivery::ReplayEnd {
            session_id: session,
            generation: 0,
            final_seq: 16,
        })
        .await;

    let replayed_state = &replay.store.sessions[&session];
    assert!(replayed_state.pending_inputs.is_empty());
    assert!(replay.input.as_str().is_empty());
    assert!(!replay.status.contains("restored to the composer"));
    assert_eq!(
        assistant_projection(replayed_state),
        assistant_projection(live_state)
    );
    assert_eq!(
        snapshot_lines(&transcript_layout(replayed_state, None, 100).lines),
        live_transcript
    );
    assert_eq!(
        replayed_state
            .transcript
            .iter()
            .filter(|item| matches!(
                item,
                TranscriptItem::ProducerMessage {
                    message_id: row_id,
                    status: crate::state::ProducerMessageStatus::Consumed,
                    ..
                } if *row_id == message_id
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn goal_reminder_kind_drives_queue_and_transcript_without_body_heuristics() {
    use cookie_agent_protocol::{GoalReminderIdentity, GoalReminderKind};

    for (kind, label, body) in [
        (
            GoalReminderKind::Started,
            "GoalStarted: finish the parser",
            "Continue the root goal.",
        ),
        (
            GoalReminderKind::Continuation,
            "GoalContinue: finish the parser",
            "Goal started. Pursue the new root objective below.",
        ),
    ] {
        let (mut app, session, run) = app_with_active_run().await;
        let message_id = ProducerMessageId::new_v7();
        let goal_id = GoalId::new_v7();
        app.store.sessions.get_mut(&session).unwrap().goal = Some(GoalState {
            goal_id,
            objective: "finish the parser".into(),
            status: GoalStatus::Active,
            items: Vec::new(),
            revision: 1,
        });
        let events = [
            producer_accepted(
                session,
                1,
                message_id,
                ProducerOwner::Goal { goal_id },
                ProducerDeliveryMode::Steer,
                body,
                Some(GoalReminderIdentity {
                    goal_id,
                    revision: 1,
                    kind,
                }),
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
            event(
                session,
                4,
                run,
                EventPayload::ProducerMessagesReleased { claim_seq: 3 },
            ),
            event(
                session,
                5,
                run,
                EventPayload::ProducerMessagesClaimed {
                    message_ids: vec![message_id],
                },
            ),
            turn_committed(
                session,
                6,
                run,
                AttemptId::new_v7(),
                1,
                Vec::new(),
                Vec::new(),
                None,
            ),
            event(
                session,
                7,
                run,
                EventPayload::ProducerMessagesReleased { claim_seq: 5 },
            ),
            event(
                session,
                8,
                run,
                EventPayload::ProducerMessageConsumed {
                    message_id,
                    run_id: run,
                },
            ),
        ];
        let mut cache = LayoutCache::default();
        for stored in events {
            assert!(app.store.apply_event(stored));
            let state = &app.store.sessions[&session];
            let status = state
                .transcript
                .iter()
                .find_map(|item| match item {
                    TranscriptItem::ProducerMessage { status, .. } => Some(*status),
                    _ => None,
                })
                .unwrap();
            let entries = app.selected_queue_entries();
            let queued = matches!(
                status,
                ProducerMessageStatus::Pending | ProducerMessageStatus::Admitted
            );
            assert_eq!(entries.len(), usize::from(queued));
            if queued {
                assert_eq!(entries[0].kind, QueueEntryKind::Producer(message_id));
                assert!(entries[0].preview.ends_with(label));
            }
            ensure_cached_transcript_layout(
                &mut cache,
                session,
                state,
                None,
                None,
                80,
                &Theme::default(),
                &PlainHighlighter,
                crate::state::EventLevel::Warning,
                0,
            );
            let rendered = snapshot_lines(&cache.layout.lines);
            let visible = matches!(
                status,
                ProducerMessageStatus::Claimed | ProducerMessageStatus::Consumed
            );
            assert_eq!(rendered.matches(label).count(), usize::from(visible));
            assert!(
                !rendered.contains(body),
                "metadata, not the opposite body prefix, selects the label"
            );
            assert!(cache.layout.user_regions.is_empty());
            assert!(state.pending_inputs.is_empty());
            assert!(state.voided_inputs.is_empty());
            if visible {
                assert!(
                    entries.is_empty(),
                    "a reminder cannot appear twice across queue and transcript"
                );
                assert!(!rendered.contains(" · claimed"));
                assert!(!rendered.contains(" · consumed"));
                let expanded = HashSet::from([BlockId::ProducerMessage(message_id)]);
                let expanded = snapshot_lines(&transcript_layout(state, Some(&expanded), 80).lines);
                assert!(expanded.contains(body));
                assert!(
                    expanded.contains(if status == ProducerMessageStatus::Claimed {
                        " · claimed"
                    } else {
                        " · consumed"
                    })
                );
            }
        }
    }
}

#[tokio::test]
async fn selected_queue_entries_merge_users_and_all_producer_sources_by_sequence() {
    let (mut app, session, run) = app_with_active_run().await;
    let plugin_id = ProducerMessageId::new_v7();
    let delegation_id = ProducerMessageId::new_v7();
    let reminder_id = ProducerMessageId::new_v7();
    let control_id = ProducerMessageId::new_v7();
    let invocation_id = InvocationId::new_v7();
    let goal_id = GoalId::new_v7();

    assert!(
        app.store
            .apply_event(admitted(session, 1, run, "user first"))
    );
    assert!(app.store.apply_event(producer_accepted(
        session,
        2,
        plugin_id,
        ProducerOwner::Plugin {
            plugin: "watcher".to_owned(),
        },
        ProducerDeliveryMode::Steer,
        "plugin body",
        None,
    )));
    assert!(app.store.apply_event(producer_accepted(
        session,
        3,
        delegation_id,
        ProducerOwner::Delegation { invocation_id },
        ProducerDeliveryMode::Queue,
        "delegation body",
        None,
    )));
    assert!(app.store.apply_event(producer_accepted(
        session,
        4,
        reminder_id,
        ProducerOwner::Goal { goal_id },
        ProducerDeliveryMode::Queue,
        "NOISY REMINDER BODY",
        Some(cookie_agent_protocol::GoalReminderIdentity {
            goal_id,
            revision: 3,
            kind: cookie_agent_protocol::GoalReminderKind::Continuation,
        }),
    )));
    assert!(app.store.apply_event(producer_accepted(
        session,
        5,
        control_id,
        ProducerOwner::GoalControl { goal_id },
        ProducerDeliveryMode::Steer,
        "Goal paused. Stop pursuing the objective.",
        None,
    )));
    assert!(
        app.store
            .apply_event(admitted(session, 6, run, "user last"))
    );
    assert!(app.store.apply_event(event(
        session,
        7,
        run,
        EventPayload::ProducerMessageAdmitted {
            message_id: delegation_id,
        },
    )));

    let entries = app.selected_queue_entries();
    assert_eq!(
        entries.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
    assert_eq!(entries[0].kind, QueueEntryKind::User);
    assert_eq!(entries[1].kind, QueueEntryKind::Producer(plugin_id));
    assert_eq!(entries[2].kind, QueueEntryKind::Producer(delegation_id));
    assert_eq!(entries[3].kind, QueueEntryKind::Producer(reminder_id));
    assert_eq!(entries[4].kind, QueueEntryKind::Producer(control_id));
    assert_eq!(entries[5].kind, QueueEntryKind::User);
    assert_eq!(entries[0].preview, "user first");
    assert_eq!(entries[1].preview, "plugin watcher · steer");
    assert_eq!(
        entries[2].preview,
        format!("delegation {invocation_id} · queue")
    );
    assert_eq!(entries[3].preview, "goal controller · queue");
    assert_eq!(entries[4].preview, "goal control · steer");
    assert_eq!(entries[5].preview, "user last");

    app.store
        .sessions
        .get_mut(&session)
        .expect("session")
        .pending_inputs
        .clear();
    set_producer_status(
        &mut app,
        session,
        control_id,
        crate::state::ProducerMessageStatus::Consumed,
    );
    let frame = rendered_frame(&mut app, 120, 24);
    assert!(frame.contains("plugin watcher · steer"));
    assert!(frame.contains(&format!("delegation {invocation_id} · queue")));
    assert!(frame.contains("goal controller · queue"));
    assert!(!frame.contains("NOISY REMINDER BODY"));
}

#[tokio::test]
async fn producer_claims_hide_waiting_rows_and_release_restores_acceptance_order() {
    let (mut app, session, run) = app_with_active_run().await;
    let plugin_id = ProducerMessageId::new_v7();
    let control_id = ProducerMessageId::new_v7();
    let plugin = ProducerOwner::Plugin {
        plugin: "claim-test".into(),
    };
    let cancelled = test_goal(GoalStatus::Cancelled, Vec::new());
    let control = ProducerOwner::GoalControl {
        goal_id: cancelled.goal_id,
    };
    app.store.sessions.get_mut(&session).unwrap().goal = Some(cancelled);
    for accepted in [
        producer_accepted(
            session,
            1,
            plugin_id,
            plugin.clone(),
            ProducerDeliveryMode::Queue,
            "plugin pending body",
            None,
        ),
        producer_accepted(
            session,
            3,
            control_id,
            control.clone(),
            ProducerDeliveryMode::Steer,
            "Goal cancelled. Stop pursuing the objective.",
            None,
        ),
    ] {
        if accepted.seq == 3 {
            assert!(
                app.store
                    .apply_event(admitted(session, 2, run, "user waiting"))
            );
        }
        assert!(app.store.apply_event(accepted));
    }
    for (seq, message_id) in [(4, plugin_id), (5, control_id)] {
        assert!(app.store.apply_event(event(
            session,
            seq,
            run,
            EventPayload::ProducerMessageAdmitted { message_id }
        )));
    }
    assert!(app.store.apply_event(event(
        session,
        6,
        run,
        EventPayload::ProducerMessagesClaimed {
            message_ids: vec![plugin_id, control_id],
        }
    )));
    assert_eq!(
        app.selected_queue_entries()
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [2]
    );
    let transcript =
        snapshot_lines(&transcript_layout(&app.store.sessions[&session], None, 100).lines);
    assert!(!transcript.contains("plugin pending body"));
    assert!(!transcript.contains("Goal cancelled. Stop pursuing the objective."));
    assert!(app.store.apply_event(event(
        session,
        7,
        RunId::new_v7(),
        EventPayload::ProducerMessagesReleased { claim_seq: 6 }
    )));
    assert_eq!(app.selected_queue_entries().len(), 1);
    assert!(app.store.apply_event(event(
        session,
        8,
        run,
        EventPayload::ProducerMessagesReleased { claim_seq: 6 }
    )));
    let entries = app.selected_queue_entries();
    assert_eq!(
        entries.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(entries[0].preview.ends_with(" · queue"));
    assert!(entries[2].preview.contains("goal control · steer"));

    assert!(app.store.apply_event(runless_event(
        session,
        9,
        EventPayload::ProducerMessageDiscarded {
            message_id: plugin_id,
            producer_owner: Some(plugin),
            reminder: None,
        }
    )));
    assert_eq!(
        app.selected_queue_entries()
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [2, 3]
    );
    assert!(app.store.apply_event(event(
        session,
        10,
        run,
        EventPayload::ProducerMessagesClaimed {
            message_ids: vec![control_id]
        }
    )));
    assert!(app.store.apply_event(turn_committed(
        session,
        11,
        run,
        AttemptId::new_v7(),
        1,
        Vec::new(),
        Vec::new(),
        None
    )));
    assert!(app.store.apply_event(event(
        session,
        12,
        run,
        EventPayload::ProducerMessagesReleased { claim_seq: 10 }
    )));
    assert!(app.store.apply_event(runless_event(
        session,
        13,
        EventPayload::ProducerMessageDiscarded {
            message_id: control_id,
            producer_owner: Some(control),
            reminder: None,
        }
    )));
    assert_eq!(
        app.selected_queue_entries()
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [2]
    );
    let history =
        snapshot_lines(&transcript_layout(&app.store.sessions[&session], None, 100).lines);
    assert_eq!(
        history
            .matches("Goal cancelled. Stop pursuing the objective.")
            .count(),
        0
    );
    let expanded = HashSet::from([BlockId::ProducerMessage(control_id)]);
    let expanded = snapshot_lines(
        &transcript_layout(&app.store.sessions[&session], Some(&expanded), 100).lines,
    );
    assert!(expanded.contains("Goal cancelled. Stop pursuing the objective."));
    assert!(history.contains("goal control"));
    assert!(!history.contains("Continue"));
    assert!(!history.contains("plugin pending body"));
    assert!(app.input.as_str().is_empty());
}

#[tokio::test]
async fn consumed_and_discarded_producers_leave_the_queue_and_producer_only_strip_hides() {
    let (mut app, session, run) = app_with_active_run().await;
    let consumed = ProducerMessageId::new_v7();
    let discarded = ProducerMessageId::new_v7();
    for (seq, message_id, body) in [(1, consumed, "consume me"), (2, discarded, "discard me")] {
        assert!(app.store.apply_event(producer_accepted(
            session,
            seq,
            message_id,
            ProducerOwner::Plugin {
                plugin: "queue-test".to_owned(),
            },
            ProducerDeliveryMode::Queue,
            body,
            None,
        )));
    }
    assert_eq!(app.selected_queue_entries().len(), 2);
    assert_eq!(app.queue_strip_height(), 4);

    assert!(app.store.apply_event(event(
        session,
        3,
        run,
        EventPayload::ProducerMessageAdmitted {
            message_id: consumed
        }
    )));
    assert!(app.store.apply_event(turn_committed(
        session,
        4,
        run,
        AttemptId::new_v7(),
        1,
        Vec::new(),
        Vec::new(),
        None
    )));
    assert!(app.store.apply_event(runless_event(
        session,
        5,
        EventPayload::ProducerMessageDiscarded {
            message_id: discarded,
            producer_owner: Some(ProducerOwner::Plugin {
                plugin: "queue-test".into()
            }),
            reminder: None,
        }
    )));
    assert!(app.selected_queue_entries().is_empty());
    assert_eq!(app.queue_strip_height(), 0);
    assert!(!rendered_frame(&mut app, 80, 24).contains("Pending"));
}

#[tokio::test]
async fn producer_queue_rows_are_passive_with_a_real_user_entry_present() {
    let (startup_client, _startup) = recording_client();
    let mut app = App::new(startup_client).await.expect("test app");
    let (client, recorded, _incoming) = live_recording_client();
    app.client = client;
    app.install_initial_runtime(runtime_snapshot(
        "1",
        Vec::new(),
        vec![model_descriptor()],
        vec![descriptor("primary", true)],
    ));
    let session = SessionId::new_v7();
    let run = run_id();
    let producer_id = ProducerMessageId::new_v7();
    app.selected = Some(session);
    app.store.sessions.insert(
        session,
        SessionState {
            active_run: Some(run),
            run_agent: Some(agent_id()),
            ..SessionState::default()
        },
    );
    assert!(app.store.apply_event(producer_accepted(
        session,
        1,
        producer_id,
        ProducerOwner::Plugin {
            plugin: "watcher".to_owned(),
        },
        ProducerDeliveryMode::Steer,
        "do not recall",
        None,
    )));
    assert!(
        app.store
            .apply_event(admitted(session, 2, run, "recallable user"))
    );
    app.input.set_buffer("draft stays".to_owned());
    rendered_frame(&mut app, 80, 24);
    let producer_hit = app
        .hit_map
        .queue_entries
        .iter()
        .find(|hit| hit.kind == QueueEntryKind::Producer(producer_id))
        .copied()
        .expect("producer hit row");
    assert!(
        app.hover_target_at(producer_hit.rect.x, producer_hit.rect.y)
            .is_none()
    );
    app.handle_click(producer_hit.rect.x, producer_hit.rect.y)
        .await;
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "run.recall_steer"), 0);
    assert_eq!(app.input.as_str(), "draft stays");
    assert_eq!(pending_texts(&app, session), ["recallable user"]);
}

#[tokio::test]
async fn pending_producer_survives_run_terminal_and_cramped_frames_do_not_overlap_composer() {
    let (mut app, session, run) = app_with_active_run().await;
    let producer_id = ProducerMessageId::new_v7();
    assert!(app.store.apply_event(producer_accepted(
        session,
        1,
        producer_id,
        ProducerOwner::Plugin {
            plugin: "terminal".to_owned(),
        },
        ProducerDeliveryMode::Queue,
        "survives terminal",
        None,
    )));
    assert!(app.store.apply_event(event(
        session,
        2,
        run,
        EventPayload::RunCompleted { final_text: None },
    )));
    assert_eq!(
        app.selected_queue_entries()[0].kind,
        QueueEntryKind::Producer(producer_id)
    );

    for (width, height) in [(12, 8), (20, 9), (28, 10)] {
        let frame = rendered_frame(&mut app, width, height);
        assert!(!frame.is_empty());
        if let Some(input) = app.hit_map.input {
            assert!(app.hit_map.queue_entries.iter().all(|hit| {
                hit.rect.y.saturating_add(hit.rect.height) <= input.rect.y
                    || input.rect.y.saturating_add(input.rect.height) <= hit.rect.y
            }));
        }
    }
}

#[tokio::test]
async fn recall_ignores_payload_text_and_pops_the_newest_entry() {
    let (mut app, session, run) = app_with_active_run().await;
    assert!(app.store.apply_event(admitted(session, 1, run, "alpha")));
    assert!(app.store.apply_event(admitted(session, 2, run, "beta")));
    // The recalled payload names the OLDEST entry; positional replay
    // still withdraws the newest, exactly like the engine.
    app.handle_delivery(live_event(recalled(session, 3, run, "alpha")))
        .await;
    assert_eq!(pending_texts(&app, session), ["alpha"]);
    // A promotion payload naming the newest entry still graduates the
    // oldest.
    app.handle_delivery(live_event(user_input(session, 4, run, "anything")))
        .await;
    assert!(pending_texts(&app, session).is_empty());
}

#[tokio::test]
async fn duplicate_texts_resolve_by_position_not_identity() {
    let (mut app, session, run) = app_with_active_run().await;
    assert!(app.store.apply_event(admitted(session, 1, run, "same")));
    assert!(app.store.apply_event(admitted(session, 2, run, "same")));
    // Promotion takes the oldest duplicate; recall takes the newest.
    app.handle_delivery(live_event(user_input(session, 3, run, "same")))
        .await;
    assert_eq!(pending_texts(&app, session), ["same"]);
    app.handle_delivery(live_event(recalled(session, 4, run, "same")))
        .await;
    assert!(pending_texts(&app, session).is_empty());
}

#[tokio::test]
async fn submit_sends_steer_and_the_strip_waits_for_admission() {
    let (mut app, session, _run) = app_with_active_run().await;
    app.submit_prompt("hold on".into()).await;
    // No optimistic entry: the strip derives from engine events only.
    assert!(pending_texts(&app, session).is_empty());
    assert_eq!(app.queue_strip_height(), 0);
    assert!(app.input.as_str().is_empty());
}

#[tokio::test]
async fn pending_lane_rebuilds_from_replay_alone() {
    let (mut app, session, run) = app_with_active_run().await;
    // A rebuild replay derives the lane purely from events: no client
    // state survives or is consulted.
    app.handle_delivery(ClientDelivery::ReplayStart {
        session_id: session,
        generation: 0,
        final_seq: 2,
        rebuild: true,
    })
    .await;
    for seq in 1..=2 {
        app.handle_delivery(ClientDelivery::ReplayEvent {
            session_id: session,
            generation: 0,
            final_seq: 2,
            event: Box::new(admitted(session, seq, run, &format!("m{seq}"))),
        })
        .await;
    }
    app.handle_delivery(ClientDelivery::ReplayEnd {
        session_id: session,
        generation: 0,
        final_seq: 2,
    })
    .await;
    assert_eq!(pending_texts(&app, session), ["m1", "m2"]);
    assert_eq!(app.queue_strip_height(), 4);
}

#[tokio::test]
async fn run_end_voids_pending_and_restores_the_composer() {
    let (mut app, session, run) = app_with_active_run().await;
    assert!(app.store.apply_event(admitted(session, 1, run, "first")));
    assert!(app.store.apply_event(admitted(session, 2, run, "second")));
    app.handle_delivery(live_event(event(
        session,
        3,
        run,
        EventPayload::RunCompleted { final_text: None },
    )))
    .await;
    // The engine voided the lane without per-entry events: the strip
    // clears and the text returns to the composer, FIFO order intact.
    assert!(pending_texts(&app, session).is_empty());
    assert_eq!(app.input.as_str(), "first\nsecond");
    assert!(app.status.contains("restored to the composer"));
}

#[tokio::test]
async fn run_end_in_a_background_session_restores_on_select() {
    let (mut app, session_a, _run_a) = app_with_active_run().await;
    let session_b = SessionId::new_v7();
    let run_b = run_id();
    app.store.sessions.insert(
        session_b,
        SessionState {
            active_run: Some(run_b),
            run_agent: Some(agent_id()),
            ..SessionState::default()
        },
    );
    assert!(
        app.store
            .apply_event(admitted(session_b, 1, run_b, "for b"))
    );
    app.handle_delivery(live_event(event(
        session_b,
        2,
        run_b,
        EventPayload::RunCancelled { reason: None },
    )))
    .await;
    // The composer belongs to session A right now: B's text is parked,
    // not leaked, and its strip is cleared.
    assert!(app.input.as_str().is_empty());
    assert!(pending_texts(&app, session_b).is_empty());
    app.set_selected_session(session_b);
    assert_eq!(app.input.as_str(), "for b");
    let _ = session_a;
}

#[tokio::test]
async fn steer_transport_failure_restores_the_submitted_text() {
    let (mut app, session, _run) = app_with_active_run().await;
    app.input.set_buffer("next draft".into());
    app.handle_rpc_update(RpcUpdate::SteerFailed {
        session_id: session,
        input: "keep me".into(),
        error: "transport closed".into(),
    });
    assert_eq!(app.input.as_str(), "keep me\nnext draft");
    assert!(app.status.contains("restored to the composer"));
}

#[tokio::test]
async fn clicking_a_strip_entry_recalls_and_restores_the_returned_text() {
    // Startup uses the short-lived recording client; the live client
    // swaps in afterwards so the recall RPC can be answered.
    let (startup_client, _startup) = recording_client();
    let mut app = App::new(startup_client).await.expect("test app");
    let (client, recorded, incoming) = live_recording_client();
    app.client = client;
    app.install_initial_runtime(runtime_snapshot(
        "1",
        Vec::new(),
        vec![model_descriptor()],
        vec![descriptor("primary", true)],
    ));
    let session = SessionId::new_v7();
    let run = run_id();
    app.selected = Some(session);
    app.store.sessions.insert(
        session,
        SessionState {
            active_run: Some(run),
            run_agent: Some(agent_id()),
            ..SessionState::default()
        },
    );
    assert!(app.store.apply_event(admitted(session, 1, run, "first")));
    assert!(app.store.apply_event(admitted(session, 2, run, "second")));
    rendered_frame(&mut app, 80, 24);
    let hit = app.hit_map.queue_entries[0];
    app.handle_click(hit.rect.x, hit.rect.y).await;
    // Any row click recalls the newest pending input.
    let id = wait_for_recorded_request(&recorded, "run.recall_steer", 1).await;
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "recalled": "second" }
        })))
        .expect("script recall response");
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("recall update timeout")
        .expect("recall update");
    app.handle_rpc_update(update);
    assert_eq!(app.input.as_str(), "second");
    assert!(app.status.contains("recalled message restored"));
    // The recalled event removes the entry from the strip itself.
    app.handle_delivery(live_event(recalled(session, 3, run, "second")))
        .await;
    assert_eq!(pending_texts(&app, session), ["first"]);
}

#[tokio::test]
async fn recall_reports_when_the_engine_lane_is_already_empty() {
    let (startup_client, _startup) = recording_client();
    let mut app = App::new(startup_client).await.expect("test app");
    let (client, recorded, incoming) = live_recording_client();
    app.client = client;
    app.install_initial_runtime(runtime_snapshot(
        "1",
        Vec::new(),
        vec![model_descriptor()],
        vec![descriptor("primary", true)],
    ));
    let session = SessionId::new_v7();
    let run = run_id();
    app.selected = Some(session);
    app.store.sessions.insert(
        session,
        SessionState {
            active_run: Some(run),
            run_agent: Some(agent_id()),
            ..SessionState::default()
        },
    );
    assert!(app.store.apply_event(admitted(session, 1, run, "raced")));
    app.recall_newest_pending();
    let id = wait_for_recorded_request(&recorded, "run.recall_steer", 1).await;
    // A promotion raced the recall: the engine has nothing to withdraw.
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "recalled": null }
        })))
        .expect("script empty recall response");
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("recall update timeout")
        .expect("recall update");
    app.handle_rpc_update(update);
    assert!(app.input.as_str().is_empty());
    assert!(app.status.contains("nothing pending to recall"));
}

#[tokio::test]
async fn press_same_frame_as_overlay_arrival_hits_nothing_underneath() {
    let (mut app, session, run) = app_with_active_run().await;
    assert!(app.store.apply_event(admitted(session, 1, run, "first")));
    assert!(app.store.apply_event(admitted(session, 2, run, "second")));
    rendered_frame(&mut app, 80, 24);
    let hit = app.hit_map.queue_entries[0];
    // An approval arrives AFTER the frame was rendered: state knows the
    // panel, the hit map does not. A press landing where a queue entry
    // was must be swallowed by the panel's ownership, not leak through
    // to the recall action underneath.
    app.store
        .sessions
        .entry(session)
        .or_default()
        .approvals
        .push(approval(session));
    assert!(app.current_approval().is_some());
    let status_before = app.status.clone();
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        hit.rect.x,
        hit.rect.y,
    ))
    .await;
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        hit.rect.x,
        hit.rect.y,
    ))
    .await;
    assert_eq!(app.modal, Modal::None, "nothing underneath opened");
    assert_eq!(
        pending_texts(&app, session),
        ["first", "second"],
        "no recall fired underneath the panel"
    );
    assert_eq!(app.status, status_before, "no content action ran");
    assert!(
        app.current_approval().is_some(),
        "the approval was not answered either"
    );
    // Hover is state-owned the same way: no content target shows
    // through the not-yet-rendered panel.
    assert!(app.hover_target_at(hit.rect.x, hit.rect.y).is_none());
}

#[tokio::test]
async fn up_in_an_empty_composer_recalls_instead_of_moving_the_cursor() {
    let (startup_client, _startup) = recording_client();
    let mut app = App::new(startup_client).await.expect("test app");
    let (client, recorded, _incoming) = live_recording_client();
    app.client = client;
    app.install_initial_runtime(runtime_snapshot(
        "1",
        Vec::new(),
        vec![model_descriptor()],
        vec![descriptor("primary", true)],
    ));
    let session = SessionId::new_v7();
    let run = run_id();
    app.selected = Some(session);
    app.store.sessions.insert(
        session,
        SessionState {
            active_run: Some(run),
            run_agent: Some(agent_id()),
            ..SessionState::default()
        },
    );
    assert!(app.store.apply_event(admitted(session, 1, run, "pending")));
    app.input_focused = true;
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await;
    wait_for_recorded_request(&recorded, "run.recall_steer", 1).await;
    // With text in the composer, Up keeps its plain cursor semantics.
    app.input.set_buffer("draft".into());
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await;
    assert_eq!(recorded_method_count(&recorded, "run.recall_steer"), 1);
}

#[tokio::test]
async fn strip_entries_highlight_on_hover() {
    let (mut app, session, run) = app_with_active_run().await;
    assert!(app.store.apply_event(admitted(session, 1, run, "first")));
    assert!(app.store.apply_event(admitted(session, 2, run, "second")));
    rendered_frame(&mut app, 80, 24);
    let second = app.hit_map.queue_entries[1].rect;
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: second.x,
        row: second.y,
        modifiers: KeyModifiers::NONE,
    })
    .await;
    assert_eq!(app.hover, Some(HoverTarget::QueueEntry(1)));
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("render");
    let buffer = terminal.backend().buffer();
    let first_row = app.hit_map.queue_entries[0].rect;
    let hovered = buffer[(second.x + 2, second.y)].style();
    let plain = buffer[(first_row.x + 2, first_row.y)].style();
    // The hover patch merges over the row: the glaze background lands
    // while the muted foreground stays.
    assert_eq!(hovered.bg, app.theme.hover().bg);
    assert_ne!(plain.bg, app.theme.hover().bg);
}

#[test]
fn queue_strip_reclaims_conversation_rows_and_keeps_status_pinned() {
    let area = Rect::new(0, 0, 80, 24);
    let plain = terminal_layout_with_tree_rows(area, 3, 0, false, 1);
    let queued = terminal_layout_with_tree_rows(area, 3, 5, false, 1);
    assert_eq!(queued.queue.height, 5);
    assert_eq!(plain.conversation.height - queued.conversation.height, 5);
    // Status line, composer, bar, and the agent panel never move.
    assert_eq!(queued.status, plain.status);
    assert_eq!(queued.input, plain.input);
    assert_eq!(queued.bar, plain.bar);
    assert_eq!(queued.agent, plain.agent);
    // The strip sits flush between conversation and status.
    assert_eq!(
        queued.queue.y,
        queued.conversation.y + queued.conversation.height
    );
    assert_eq!(queued.queue.y + queued.queue.height, queued.status.y);
    // On a cramped terminal the strip shrinks away rather than taking
    // the conversation's last row.
    let tiny = terminal_layout_with_tree_rows(Rect::new(0, 0, 20, 8), 20, 5, false, 1);
    assert!(tiny.conversation.height >= 1);
    assert!(tiny.queue.height < 5);
    // Zero demand reserves nothing.
    assert_eq!(plain.queue.height, 0);
}

#[tokio::test]
async fn queue_strip_is_hidden_when_empty_and_folds_overflow_into_more_row() {
    let (mut app, session, run) = app_with_active_run().await;
    // Empty lane: no rows reserved, no title anywhere in the frame.
    assert_eq!(app.queue_strip_height(), 0);
    let frame = rendered_frame(&mut app, 80, 24);
    assert!(!frame.contains("Pending"));
    // Five entries render the capped three text rows: two entries and
    // the folded overflow count.
    for index in 1..=5 {
        assert!(
            app.store
                .apply_event(admitted(session, index, run, &format!("message {index}")))
        );
    }
    assert_eq!(app.queue_strip_height(), 5);
    let frame = rendered_frame(&mut app, 80, 24);
    assert!(frame.contains("Pending"));
    assert!(frame.contains("message 1"));
    assert!(frame.contains("message 2"));
    assert!(!frame.contains("message 3"));
    assert!(frame.contains("+3 more"));
    assert!(!frame.contains("message 5"));
}

#[tokio::test]
async fn queue_strip_ellipsizes_long_messages_and_flattens_newlines() {
    let (mut app, session, run) = app_with_active_run().await;
    assert!(
        app.store
            .apply_event(admitted(session, 1, run, &"x".repeat(200)))
    );
    assert!(
        app.store
            .apply_event(admitted(session, 2, run, "first\nsecond line"))
    );
    let frame = rendered_frame(&mut app, 60, 24);
    // The overlong entry truncates with an ellipsis…
    assert!(frame.contains('…'));
    // …and the multiline entry renders flattened onto one row.
    assert!(frame.contains("first second line"));
    // Two entries keep the strip at two text rows plus borders.
    assert_eq!(app.queue_strip_height(), 4);
}

#[test]
fn ellipsize_single_line_flattens_and_truncates_grapheme_safely() {
    assert_eq!(ellipsize_single_line("a\nb  c", 80), "a b c");
    assert_eq!(ellipsize_single_line("short", 80), "short");
    let truncated = ellipsize_single_line(&"東京タワー".repeat(10), 10);
    assert!(truncated.ends_with('…'));
    assert!(UnicodeWidthStr::width(truncated.as_str()) <= 10);
    // Width zero still emits the ellipsis marker only.
    assert_eq!(ellipsize_single_line("abc", 0), "…");
}

#[test]
fn queue_age_label_is_coarse_and_monotonic() {
    assert_eq!(queue_age_label(0), "<1m");
    assert_eq!(queue_age_label(59), "<1m");
    assert_eq!(queue_age_label(60), "1m");
    assert_eq!(queue_age_label(3599), "59m");
    assert_eq!(queue_age_label(3600), "1h");
    assert_eq!(queue_age_label(9000), "2h");
}

#[tokio::test]
async fn goal_bar_opens_details_by_mouse_and_keyboard_without_touching_draft() {
    let (mut app, session, _) = app_with_active_run().await;
    app.store.sessions.get_mut(&session).unwrap().goal = Some(test_goal(
        GoalStatus::Active,
        vec![goal_item("Verify the persistent goal bar", false)],
    ));
    app.input.set_buffer("draft remains intact".into());
    rendered_frame(&mut app, 80, 24);
    let (description, _) = app
        .hit_map
        .goal_actions
        .iter()
        .find(|(_, action)| *action == crate::ui::app::GoalBarAction::Details)
        .copied()
        .expect("description hit");
    assert_eq!(
        app.hover_target_at(description.x, description.y),
        Some(HoverTarget::GoalAction(
            crate::ui::app::GoalBarAction::Details
        ))
    );
    app.handle_click(description.x, description.y).await;
    assert_eq!(app.modal, Modal::GoalDetail);
    let frame = rendered_frame(&mut app, 80, 24);
    assert!(frame.contains("Verify the persistent goal bar"), "{frame}");
    let close = app.hit_map.goal_close.expect("close goal details");
    assert_eq!(
        app.hover_target_at(close.x, close.y),
        Some(HoverTarget::GoalClose)
    );
    app.handle_click(close.x, close.y).await;
    assert_eq!(app.modal, Modal::None);
    app.handle_key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE))
        .await;
    assert_eq!(app.goal_focus, Some(crate::ui::app::GoalBarAction::Details));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::GoalDetail);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::None);
    assert_eq!(app.input.as_str(), "draft remains intact");
}

#[tokio::test]
async fn goal_bar_and_waiting_queue_coexist_without_blank_rows_when_hidden() {
    let (mut app, session, run) = app_with_active_run().await;
    let goal = test_goal(
        GoalStatus::Paused,
        vec![goal_item("Queued verification", false)],
    );
    app.store.sessions.get_mut(&session).unwrap().goal = Some(goal);
    assert!(
        app.store
            .apply_event(admitted(session, 1, run, "waiting user message"))
    );
    app.input.set_buffer("USER DRAFT".into());
    for (width, height) in [
        (1, 8),
        (3, 8),
        (7, 9),
        (8, 10),
        (20, 18),
        (40, 24),
        (80, 24),
    ] {
        let frame = rendered_frame(&mut app, width, height);
        let input_rect = app.hit_map.input.expect("composer").rect;
        for (rect, _) in &app.hit_map.goal_actions {
            assert_eq!(rect.height, 1);
            assert_eq!(rect.bottom(), input_rect.y);
            assert!(rect.right() <= width);
        }
        for hit in &app.hit_map.queue_entries {
            assert!(hit.rect.bottom() <= input_rect.y.saturating_sub(1));
        }
        if width >= 40 {
            assert!(frame.contains("waiting user message"), "{frame}");
            assert!(frame.contains("USER DRAFT"), "{frame}");
            assert!(frame.contains("Resume"), "{frame}");
            assert!(frame.contains("Cancel"), "{frame}");
        }
        if width == 80 {
            let goal_row = app.hit_map.goal_actions[0].0.y;
            let bar = rendered_row(&mut app, width, height, goal_row)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            insta::assert_snapshot!(bar, @"🎯 : Ship transcript rendering without regressions [Resume] [Cancel]");
        }
    }
    app.store.sessions.get_mut(&session).unwrap().goal = None;
    app.store
        .sessions
        .get_mut(&session)
        .unwrap()
        .pending_inputs
        .clear();
    rendered_frame(&mut app, 80, 24);
    assert!(app.hit_map.goal_actions.is_empty());
    assert!(app.hit_map.queue_entries.is_empty());
    assert_eq!(app.queue_strip_height(), 0);
    let layout = terminal_layout_with_tree_rows(
        Rect::new(0, 0, 80, 24),
        app.tree_entries().len(),
        0,
        false,
        1,
    );
    assert_eq!(layout.goal.height, 0);
    assert_eq!(layout.status.bottom(), layout.input.y);
}

#[tokio::test]
async fn queue_strip_renders_mixed_sources() {
    let (mut app, session, run) = app_with_active_run().await;
    assert!(
        app.store
            .apply_event(admitted(session, 1, run, "User follow-up"))
    );
    assert!(app.store.apply_event(producer_accepted(
        session,
        2,
        ProducerMessageId::new_v7(),
        ProducerOwner::Plugin {
            plugin: "build".into()
        },
        ProducerDeliveryMode::Queue,
        "Build finished successfully",
        None,
    )));
    assert!(app.store.apply_event(producer_accepted(
        session,
        3,
        ProducerMessageId::new_v7(),
        ProducerOwner::GoalControl {
            goal_id: GoalId::new_v7()
        },
        ProducerDeliveryMode::Steer,
        "Goal paused. Stop pursuing the objective.",
        None,
    )));
    let frame = rendered_frame(&mut app, 100, 24);
    let entries = app.selected_queue_entries();
    for entry in &entries {
        assert!(frame.contains(&entry.preview), "{frame}");
    }
    insta::assert_snapshot!(
        entries.iter().map(|entry| entry.preview.as_str()).collect::<Vec<_>>().join("\n"),
        @"
        User follow-up
        plugin build · queue
        goal control · steer
        "
    );
}

#[tokio::test]
async fn queue_strip_renders_the_pending_lane() {
    let (mut app, session, run) = app_with_active_run().await;
    assert!(
        app.store
            .apply_event(admitted(session, 1, run, "first pending message"))
    );
    assert!(
        app.store
            .apply_event(admitted(session, 2, run, "second pending message"))
    );
    assert!(
        app.store
            .apply_event(admitted(session, 3, run, "third pending message"))
    );
    // Admission timestamps are the durable event timestamps: always
    // fresh here, so the coarse "<1m" age keeps the snapshot stable.
    let rendered = rendered_frame(&mut app, 60, 24);
    insta::assert_snapshot!(rendered);
}
