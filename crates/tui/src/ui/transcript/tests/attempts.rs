use crate::ui::transcript::*;

use cookie_agent_protocol::{
    AttemptId, EventPayload, GoalId, ProducerMessageId, SafeDisplayText, SafeErrorMessage,
    SessionId, Sha256Digest, ToolCallId,
};

use jiff::Timestamp;

use crate::markdown::PlainHighlighter;

use crate::state::{AssistantChild, SessionState, StateStore};

use super::support::*;

#[test]
fn checkpoint_mid_run_splits_the_assistant_block_below_the_marker() {
    let session = SessionId::new_v7();
    let run = run_id();
    let before = AttemptId::new_v7();
    let after = AttemptId::new_v7();
    let before_call = ToolCallId::new_v7();
    let after_call = ToolCallId::new_v7();
    let mut store = StateStore::default();
    let events = checkpoint_mid_run_events(session, run, before, after, before_call, after_call);
    for event in events {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    // The marker sits at its true position: the run's pre-compaction block
    // above it, everything emitted after it in a fresh block below.
    assert_eq!(
        transcript_shape(state),
        [
            "assistant[thinking:pondering the pre-flight plan,text:answer before the checkpoint,tool]",
            "compaction:8",
            "assistant[text:answer after the checkpoint,thinking:pondering the post-flight plan,tool]",
        ]
    );
    let assistants = assistant_items(state);
    assert_eq!(
        assistants.len(),
        2,
        "the checkpoint ends the run's first block"
    );
    assert!(children_has_tool(assistants[0], before_call));
    assert!(!children_has_tool(assistants[1], before_call));
    assert!(children_has_tool(assistants[1], after_call));
    // Rendered order agrees with the projection order, segment by segment.
    let mut expanded = HashSet::from([BlockId::Compaction(8)]);
    expand_thinking(state, &mut expanded);
    let rendered = snapshot_lines(&transcript_layout(state, Some(&expanded), 100).lines);
    let position = |needle: &str| {
        rendered
            .lines()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle:?} in\n{rendered}"))
    };
    let marker = position("context compacted");
    assert!(
        position("pondering the pre-flight plan") < position("answer before the checkpoint"),
        "{rendered}"
    );
    assert!(
        position("answer before the checkpoint") < marker,
        "{rendered}"
    );
    assert!(
        position("pre-call") < marker,
        "the pre-compaction tool row renders inside its own segment"
    );
    assert!(
        marker < position("answer after the checkpoint"),
        "{rendered}"
    );
    assert!(
        position("answer after the checkpoint") < position("pondering the post-flight plan"),
        "{rendered}"
    );
    assert!(
        position("pondering the post-flight plan") < position("post-call"),
        "the post-compaction tool row renders below the marker in its own segment"
    );
}

#[test]
fn checkpoint_mid_run_split_survives_replay_rebuild() {
    let session = SessionId::new_v7();
    let run = run_id();
    let before = AttemptId::new_v7();
    let after = AttemptId::new_v7();
    let before_call = ToolCallId::new_v7();
    let after_call = ToolCallId::new_v7();
    let events = checkpoint_mid_run_events(session, run, before, after, before_call, after_call);
    let mut live = StateStore::default();
    for event in &events {
        assert!(live.apply_event(event.clone()));
    }
    let mut rebuilt = StateStore::default();
    assert!(rebuilt.rebuild_session(session, 0, events));
    assert_eq!(
        transcript_shape(&live.sessions[&session]),
        [
            "assistant[thinking:pondering the pre-flight plan,text:answer before the checkpoint,tool]",
            "compaction:8",
            "assistant[text:answer after the checkpoint,thinking:pondering the post-flight plan,tool]",
        ]
    );
    assert_eq!(
        transcript_shape(&rebuilt.sessions[&session]),
        transcript_shape(&live.sessions[&session]),
        "a rebuilt projection splits the run at the checkpoint too"
    );
}

#[test]
fn checkpoint_after_a_pruned_abandoned_attempt_leaves_no_empty_block() {
    for warned in [false, true] {
        let session = SessionId::new_v7();
        let run = run_id();
        let first = AttemptId::new_v7();
        let retry = AttemptId::new_v7();
        let events = checkpoint_recovery_events(session, run, first, retry, warned);
        let marker = if warned {
            "compaction:6"
        } else {
            "compaction:5"
        };
        let mut store = StateStore::default();
        for event in events {
            assert!(store.apply_event(event));
        }
        let state = &store.sessions[&session];
        assert_eq!(
            transcript_shape(state),
            [marker, "assistant[text:recovered answer]"],
            "the checkpoint consumes the split of the emptied block (warned: {warned})"
        );
        let rendered = snapshot_lines(&transcript_layout(state, None, 100).lines);
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.starts_with("╭─ primary •"))
                .count(),
            1,
            "no empty assistant header survives around the marker (warned: {warned}):\n{rendered}"
        );
    }
}

#[test]
fn same_model_retry_after_abandonment_prunes_partials_without_marker() {
    let session = SessionId::new_v7();
    let run = run_id();
    let first = AttemptId::new_v7();
    let second = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, first, None),
        text_delta(session, 2, run, first, "partial"),
        event(
            session,
            3,
            run,
            EventPayload::AttemptAbandoned {
                attempt_id: first,
                model_error: None,
            },
        ),
        attempt_started(session, 4, run, second, None),
        text_delta(session, 5, run, second, "final"),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let assistants = state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Assistant { children, .. } => Some(children),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(assistants.len(), 1);
    assert!(matches!(
        assistants[0].as_slice(),
        [AssistantChild::Text { markdown, .. }] if markdown.as_str() == "final"
    ));
}

#[test]
fn model_change_inserts_marker_and_keeps_first_header() {
    let session = SessionId::new_v7();
    let run = run_id();
    let base = AttemptId::new_v7();
    let high = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        session_created(session, 1),
        attempt_started(session, 2, run, base, None),
        turn_committed(
            session,
            3,
            run,
            base,
            1,
            vec![text_part("base answer")],
            Vec::new(),
            None,
        ),
        attempt_started(session, 4, run, high, Some("high")),
        turn_committed(
            session,
            5,
            run,
            high,
            2,
            vec![text_part("high answer")],
            Vec::new(),
            Some("high"),
        ),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let rendered = transcript_layout(state, None, 60)
        .lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        rendered
            .matches("primary • gateway/arbitrary-model[base]")
            .count(),
        1
    );
    assert_eq!(
        rendered
            .matches("├─ now using gateway/arbitrary-model[high]")
            .count(),
        1
    );
    let assistant = state
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Assistant {
                attribution,
                children,
                ..
            } => Some((attribution, children)),
            _ => None,
        })
        .expect("assistant");
    assert_eq!(assistant.0.variant_label(), "base");
    assert!(matches!(
        assistant.1.as_slice(),
        [
            AssistantChild::Text { markdown: first, .. },
            AssistantChild::Attribution { resolved_model },
            AssistantChild::Text { markdown: second, .. },
        ] if first.as_str() == "base answer"
            && resolved_model.selection.variant.as_ref().is_some_and(|variant| variant.as_str() == "high")
            && second.as_str() == "high answer"
    ));
}

#[test]
fn multi_attempt_run_merges_committed_turns_and_tool_in_order() {
    let session = SessionId::new_v7();
    let run = run_id();
    let first_attempt = AttemptId::new_v7();
    let second_attempt = AttemptId::new_v7();
    let call_id = ToolCallId::new_v7();
    let events = vec![
        attempt_started(session, 1, run, first_attempt, None),
        turn_committed(
            session,
            2,
            run,
            first_attempt,
            10,
            vec![text_part("turn one"), tool_part("call-one")],
            Vec::new(),
            None,
        ),
        tool_started_at(session, 3, run, call_id, 10, "call-one", 1, "bash", None),
        attempt_started(session, 4, run, second_attempt, None),
        turn_committed(
            session,
            5,
            run,
            second_attempt,
            11,
            vec![reasoning_part("turn two thought"), text_part("turn two")],
            Vec::new(),
            None,
        ),
    ];
    let mut store = StateStore::default();
    for event in events {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    assert_eq!(assistant_projection(state).len(), 1);
    let children = match state
        .transcript
        .iter()
        .find(|item| matches!(item, TranscriptItem::Assistant { .. }))
        .expect("assistant")
    {
        TranscriptItem::Assistant { children, .. } => children,
        _ => unreachable!(),
    };
    assert!(matches!(
        children.as_slice(),
        [
            AssistantChild::Text { markdown: first, .. },
            AssistantChild::Tool { call_id: linked },
            AssistantChild::Thinking { text: thought, .. },
            AssistantChild::Text { markdown: second, .. },
        ] if first.as_str() == "turn one"
            && *linked == call_id
            && thought == "turn two thought"
            && second.as_str() == "turn two"
    ));
}

#[test]
fn retry_started_before_input_promotion_rebinds_without_losing_committed_tools() {
    for variant in [None, Some("high")] {
        let mut retry_model = resolved_model(variant);
        if variant.is_some() {
            retry_model.selection.model = "gateway/fallback-model".parse().unwrap();
            retry_model.model_id =
                cookie_agent_protocol::ProviderModelId::new("fallback-model").unwrap();
            retry_model.selection_fingerprint = Sha256Digest::of_bytes(b"fallback selection");
        }
        let suffix = if variant.is_some() {
            vec![resolved_model(None), retry_model.clone()]
        } else {
            vec![resolved_model(None)]
        };
        let session = SessionId::new_v7();
        let run = run_id();
        let first = AttemptId::new_v7();
        let failed = AttemptId::new_v7();
        let retry = AttemptId::new_v7();
        let next = AttemptId::new_v7();
        let call = ToolCallId::new_v7();
        let control = ProducerMessageId::new_v7();
        let plugin = ProducerMessageId::new_v7();
        let mut events = vec![
            session_created(session, 1),
            run_started_with_suffix(session, 2, run, suffix),
            attempt_started(session, 3, run, first, None),
            text_delta(session, 4, run, first, "before steering"),
            turn_committed(
                session,
                5,
                run,
                first,
                5,
                vec![text_part("before steering"), tool_part("call-one")],
                Vec::new(),
                None,
            ),
            tool_started_at(session, 6, run, call, 5, "call-one", 1, "bash", None),
            tool_terminated(
                session,
                7,
                run,
                call,
                5,
                "call-one",
                cookie_agent_protocol::ToolTerminationOutcome::Completed,
            ),
            attempt_started(session, 8, run, failed, None),
            text_delta(session, 9, run, failed, "abandoned partial"),
            event(
                session,
                10,
                run,
                EventPayload::AttemptAbandoned {
                    attempt_id: failed,
                    model_error: None,
                },
            ),
            producer_accepted(
                session,
                11,
                control,
                ProducerOwner::GoalControl {
                    goal_id: GoalId::new_v7(),
                },
                ProducerDeliveryMode::Steer,
                "cancel steering",
                None,
            ),
            producer_accepted(
                session,
                12,
                plugin,
                ProducerOwner::Plugin {
                    plugin: "test".into(),
                },
                ProducerDeliveryMode::Steer,
                "plugin steering",
                None,
            ),
            event(
                session,
                13,
                run,
                EventPayload::UserInputAdmitted {
                    input: "user steering".into(),
                },
            ),
            // stream_attempt emits retry/fallback metadata BEFORE prompt_events
            // promotes inputs received during backoff (model_loop.rs:1346).
            attempt_started(session, 14, run, retry, variant),
            event(
                session,
                15,
                run,
                EventPayload::ProducerMessageAdmitted {
                    message_id: control,
                },
            ),
            event(
                session,
                16,
                run,
                EventPayload::UserInputSubmitted {
                    input: "user steering".into(),
                },
            ),
            event(
                session,
                17,
                run,
                EventPayload::UserInputApplied { user_input_seq: 16 },
            ),
            event(
                session,
                18,
                run,
                EventPayload::ProducerMessageAdmitted { message_id: plugin },
            ),
            event(
                session,
                19,
                run,
                EventPayload::ProducerMessagesClaimed {
                    message_ids: vec![control, plugin],
                },
            ),
            reasoning_delta(session, 20, run, retry, "retry reasoning"),
            text_delta(session, 21, run, retry, "after steering"),
            turn_committed(
                session,
                22,
                run,
                retry,
                22,
                vec![
                    reasoning_part("retry reasoning"),
                    text_part("after steering"),
                ],
                Vec::new(),
                variant,
            ),
            usage_recorded(session, 23, run, 5, Some(10)),
            usage_recorded(session, 24, run, 22, Some(20)),
            attempt_started(session, 25, run, next, variant),
            text_delta(session, 26, run, next, "latest response"),
            turn_committed(
                session,
                27,
                run,
                next,
                27,
                vec![text_part("latest response")],
                Vec::new(),
                variant,
            ),
        ];
        for stored in &mut events {
            stored.timestamp = Timestamp::new(stored.seq as i64, 0).unwrap();
            if let EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal,
                retry_ordinal,
                fallback_index,
                resolved_model,
                ..
            } = &mut stored.payload
            {
                *attempt_ordinal = match stored.seq {
                    3 => 1,
                    8 => 2,
                    14 => 3,
                    25 => 4,
                    _ => unreachable!(),
                };
                if *attempt_id == retry || *attempt_id == next {
                    *resolved_model = retry_model.clone();
                    *fallback_index = u32::from(variant.is_some());
                    *retry_ordinal = u32::from(*attempt_id == retry && variant.is_none());
                }
            }
            if let EventPayload::ModelTurnCommitted {
                input_through_seq,
                resolved_model,
                ..
            } = &mut stored.payload
            {
                *input_through_seq = match stored.seq {
                    5 => 2,
                    22 => 19,
                    27 => 22,
                    _ => unreachable!(),
                };
                if stored.seq != 5 {
                    *resolved_model = retry_model.clone();
                }
            }
            if stored.seq == 24
                && let EventPayload::ModelUsageRecorded { resolved_model, .. } = &mut stored.payload
            {
                *resolved_model = retry_model.clone();
            }
        }
        let mut live = StateStore::default();
        let mut cache = LayoutCache::default();
        let expanded = HashSet::from([BlockId::Tool(call)]);
        let mut split_id = None;
        for (index, stored) in events.iter().enumerate() {
            assert!(live.apply_event(stored.clone()));
            let state = &live.sessions[&session];
            let mut replay = StateStore::default();
            assert!(replay.rebuild_session(session, 0, events[..=index].to_vec()));
            ensure_cached_transcript_layout(
                &mut cache,
                session,
                state,
                None,
                Some(&expanded),
                80,
                &Theme::default(),
                &PlainHighlighter,
                crate::state::EventLevel::Warning,
                0,
            );
            let layout = |state: &SessionState| {
                transcript_layout_with_level(
                    state,
                    Some(&expanded),
                    80,
                    &Theme::default(),
                    &PlainHighlighter,
                    crate::state::EventLevel::Warning,
                )
            };
            let fresh = layout(state);
            let rebuilt = layout(&replay.sessions[&session]);
            assert_eq!(cache.layout.lines, fresh.lines);
            assert_eq!(cache.layout.regions, fresh.regions);
            assert_eq!(cache.layout.user_regions, fresh.user_regions);
            assert_eq!(fresh.lines, rebuilt.lines);
            assert_eq!(fresh.regions, rebuilt.regions);
            assert_eq!(fresh.user_regions, rebuilt.user_regions);
            if stored.seq >= 15 {
                let assistants = state
                    .transcript
                    .iter()
                    .filter(|item| matches!(item, TranscriptItem::Assistant { .. }))
                    .collect::<Vec<_>>();
                assert_eq!(
                    assistants.len(),
                    2,
                    "retry must own a post-input block before streaming"
                );
                let old = assistants[0];
                let new = assistants[1];
                assert_eq!(
                    *split_id.get_or_insert(new.id()),
                    new.id(),
                    "multiple boundaries reuse the empty retry block"
                );
                assert!(
                    matches!(old, TranscriptItem::Assistant { children, attribution, .. }
                        if attribution.variant_label() == "base" && matches!(children.as_slice(), [AssistantChild::Text { markdown, .. }, AssistantChild::Tool { call_id }]
                            if markdown.as_str() == "before steering" && *call_id == call))
                );
                assert!(
                    matches!(new, TranscriptItem::Assistant { attribution, .. } if attribution.resolved_model == retry_model)
                );
                assert_eq!(state.turn_items[&5], old.id());
            }
            if stored.seq == 19 {
                let ordered = state
                    .transcript
                    .iter()
                    .filter(|item| {
                        matches!(
                            item,
                            TranscriptItem::Assistant { .. }
                                | TranscriptItem::ProducerMessage { .. }
                                | TranscriptItem::User { .. }
                        )
                    })
                    .collect::<Vec<_>>();
                assert!(
                    matches!(ordered.as_slice(), [TranscriptItem::Assistant { .. }, TranscriptItem::ProducerMessage { message_id: a, .. }, TranscriptItem::User { .. }, TranscriptItem::ProducerMessage { message_id: b, .. }, TranscriptItem::Assistant { .. }] if *a == control && *b == plugin)
                );
            }
        }
        let state = &live.sessions[&session];
        let before = state.turn_items[&5];
        let after = state.turn_items[&22];
        assert_ne!(before, after);
        assert_eq!(after, state.turn_items[&27]);
        assert_eq!(state.assistant_metrics.len(), 2);
        assert_eq!(state.assistant_metrics[&before].timed_turns, 1);
        assert_eq!(state.assistant_metrics[&before].timed_output_tokens, 4);
        assert_eq!(
            state.assistant_metrics[&before].estimated_cost_pico_usd,
            Some(10)
        );
        assert_eq!(state.assistant_metrics[&after].timed_turns, 2);
        assert_eq!(state.assistant_metrics[&after].timed_output_tokens, 8);
        assert_eq!(
            state.assistant_metrics[&after].estimated_cost_pico_usd,
            Some(20)
        );
        assert_eq!(state.tools[&call].status, ToolStatus::Completed);
        assert!(state.pending_inputs.is_empty());
        let rendered = snapshot_lines(&cache.layout.lines);
        let mut cursor = 0;
        for text in [
            "before steering",
            "◇ ▸ goal control · steer",
            "user steering",
            "◇ ▸ plugin test · steer",
            "after steering",
            "latest response",
        ] {
            cursor += rendered[cursor..]
                .find(text)
                .unwrap_or_else(|| panic!("missing ordered {text}: {rendered}"))
                + text.len();
        }
        assert!(!rendered.contains("abandoned partial"));
    }
}

#[test]
fn input_boundary_does_not_relocate_an_already_streaming_attempt() {
    let session = SessionId::new_v7();
    let run = run_id();
    let first = AttemptId::new_v7();
    let streaming = AttemptId::new_v7();
    let next = AttemptId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let mut events = vec![
        attempt_started(session, 1, run, first, None),
        turn_committed(
            session,
            2,
            run,
            first,
            2,
            vec![text_part("committed prefix")],
            Vec::new(),
            None,
        ),
        attempt_started(session, 3, run, streaming, None),
        text_delta(session, 4, run, streaming, "already streaming"),
        producer_accepted(
            session,
            5,
            message_id,
            ProducerOwner::Plugin {
                plugin: "test".into(),
            },
            ProducerDeliveryMode::Steer,
            "late input",
            None,
        ),
        event(
            session,
            6,
            run,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
        text_delta(session, 7, run, streaming, " complete"),
        turn_committed(
            session,
            8,
            run,
            streaming,
            8,
            vec![text_part("already streaming complete")],
            Vec::new(),
            None,
        ),
        attempt_started(session, 9, run, next, None),
        turn_committed(
            session,
            10,
            run,
            next,
            10,
            vec![text_part("next response")],
            Vec::new(),
            None,
        ),
    ];
    if let EventPayload::ModelTurnCommitted {
        input_through_seq, ..
    } = &mut events[7].payload
    {
        *input_through_seq = 3;
    }
    let mut live = StateStore::default();
    for stored in &events {
        assert!(live.apply_event(stored.clone()));
    }
    let state = &live.sessions[&session];
    let projection = assistant_projection(state);
    assert_eq!(projection.len(), 2);
    assert!(
        projection[0]
            .2
            .iter()
            .any(|text| text.contains("committed prefix"))
    );
    assert!(
        projection[0]
            .2
            .iter()
            .any(|text| text.contains("already streaming complete"))
    );
    assert_eq!(state.turn_items[&2], state.turn_items[&8]);
    assert_ne!(state.turn_items[&8], state.turn_items[&10]);
    let mut replay = StateStore::default();
    assert!(replay.rebuild_session(session, 0, events));
    assert_eq!(projection, assistant_projection(&replay.sessions[&session]));
}

#[test]
fn goal_activation_precedes_triggered_streaming_and_preserves_existing_output() {
    for existing_run in [false, true] {
        let session = SessionId::new_v7();
        let run = run_id();
        let goal_id = GoalId::new_v7();
        let reminder_id = ProducerMessageId::new_v7();
        let attempts = std::array::from_fn::<_, 4, _>(|_| AttemptId::new_v7());
        let commit = |attempt, turn_seq, input, text| {
            let mut stored = turn_committed(
                session,
                0,
                run,
                attempt,
                turn_seq,
                vec![text_part(text)],
                Vec::new(),
                None,
            );
            let EventPayload::ModelTurnCommitted {
                input_through_seq, ..
            } = &mut stored.payload
            else {
                unreachable!()
            };
            *input_through_seq = input;
            stored
        };
        let mut events = vec![session_created(session, 0)];
        if existing_run {
            events.extend([
                run_started_with_suffix(session, 0, run, vec![resolved_model(None)]),
                attempt_started(session, 0, run, attempts[0], None),
                text_delta(session, 0, run, attempts[0], "old committed"),
                commit(attempts[0], 1, 2, "old committed"),
                attempt_started(session, 0, run, attempts[1], None),
                text_delta(session, 0, run, attempts[1], "old partial"),
            ]);
        }
        let activation_index = events.len();
        events.push(runless_event(
            session,
            0,
            EventPayload::GoalActivated {
                goal_id,
                objective: "finish  the parser".into(),
                revision: 1,
                selection: None,
            },
        ));
        if existing_run {
            // This response was already in flight before activation. Its
            // prior committed prefix and partial must not migrate or vanish.
            events.extend([
                text_delta(session, 0, run, attempts[1], " finished"),
                commit(attempts[1], 2, 5, "old partial finished"),
            ]);
        }
        events.push(runless_event(
            session,
            0,
            EventPayload::ProducerMessageAccepted {
                description: Default::default(),
                message_id: reminder_id,
                producer_owner: ProducerOwner::Goal { goal_id },
                mode: ProducerDeliveryMode::Steer,
                idempotency_key: cookie_agent_protocol::ProducerIdempotencyKey::new(
                    "initial goal input",
                )
                .unwrap(),
                body: "INTERNAL GOAL INPUT".into(),
                reminder: Some(cookie_agent_protocol::GoalReminderIdentity {
                    goal_id,
                    revision: 1,
                    kind: cookie_agent_protocol::GoalReminderKind::Started,
                }),
                agent_hop: None,
            },
        ));
        if !existing_run {
            events.push(run_started_with_suffix(
                session,
                0,
                run,
                vec![resolved_model(None)],
            ));
        }
        events.extend([
            event(
                session,
                0,
                run,
                EventPayload::ProducerMessageAdmitted {
                    message_id: reminder_id,
                },
            ),
            event(
                session,
                0,
                run,
                EventPayload::ProducerMessagesClaimed {
                    message_ids: vec![reminder_id],
                },
            ),
        ]);
        let input = events.len() as u64;
        events.extend([
            attempt_started(session, 0, run, attempts[2], None),
            text_delta(session, 0, run, attempts[2], "new response"),
            commit(attempts[2], 3, input, "new response"),
            event(
                session,
                0,
                run,
                EventPayload::ProducerMessageConsumed {
                    message_id: reminder_id,
                    run_id: run,
                },
            ),
            event(
                session,
                0,
                run,
                EventPayload::ProducerMessagesReleased { claim_seq: input },
            ),
            attempt_started(session, 0, run, attempts[3], None),
            text_delta(session, 0, run, attempts[3], "next response"),
            commit(attempts[3], 4, input + 5, "next response"),
        ]);
        for (index, stored) in events.iter_mut().enumerate() {
            stored.seq = index as u64 + 1;
            stored.timestamp = Timestamp::new(stored.seq as i64, 0).unwrap();
        }
        let mut live = StateStore::default();
        let mut caches = [LayoutCache::default(), LayoutCache::default()];
        for (index, stored) in events.iter().enumerate() {
            assert!(live.apply_event(stored.clone()));
            let mut replay = StateStore::default();
            assert!(replay.rebuild_session(session, 0, events[..=index].to_vec()));
            let state = &live.sessions[&session];
            for (width, cache) in [18, 80].into_iter().zip(&mut caches) {
                ensure_cached_transcript_layout(
                    cache,
                    session,
                    state,
                    None,
                    None,
                    width,
                    &Theme::default(),
                    &PlainHighlighter,
                    crate::state::EventLevel::Warning,
                    0,
                );
                let rebuilt = transcript_layout_with_level(
                    &replay.sessions[&session],
                    None,
                    width,
                    &Theme::default(),
                    &PlainHighlighter,
                    crate::state::EventLevel::Warning,
                );
                assert_eq!(
                    cache.layout.lines, rebuilt.lines,
                    "seq {} existing {existing_run}",
                    stored.seq
                );
                assert_eq!(cache.layout.regions, rebuilt.regions);
                assert!(
                    cache.layout.user_regions.is_empty(),
                    "goal action is not a model prompt"
                );
                assert!(
                    cache
                        .layout
                        .lines
                        .iter()
                        .all(|line| line.width() <= usize::from(width))
                );
            }
            if index >= activation_index {
                let rendered = snapshot_lines(&caches[1].layout.lines);
                let action = rendered.find("/goal finish  the parser").unwrap();
                assert_eq!(rendered.matches("/goal finish  the parser").count(), 1);
                assert!(rendered.contains("ACTION"));
                if stored.seq >= input {
                    let started = rendered.find("GoalStarted:").unwrap();
                    assert_eq!(rendered.matches("GoalStarted:").count(), 1);
                    assert!(action < started);
                    for text in ["new response", "next response"] {
                        if let Some(response) = rendered.find(text) {
                            assert!(started < response);
                        }
                    }
                } else {
                    assert!(
                        !rendered.contains("GoalStarted:"),
                        "start remains in the pending queue before claim"
                    );
                }
                if existing_run {
                    assert!(rendered.find("old committed").unwrap() < action);
                    assert!(rendered.find("old partial").unwrap() < action);
                }
            }
        }
        let state = &live.sessions[&session];
        let assistants = assistant_projection(state);
        let rendered = snapshot_lines(&caches[1].layout.lines);
        assert_eq!(rendered.matches("GoalStarted:").count(), 1);
        assert!(!rendered.contains("Continue"));
        assert_eq!(assistants.len(), if existing_run { 2 } else { 1 });
        assert!(
            assistants
                .last()
                .unwrap()
                .2
                .iter()
                .any(|part| part.contains("new response"))
        );
        assert!(
            assistants
                .last()
                .unwrap()
                .2
                .iter()
                .any(|part| part.contains("next response"))
        );
        assert!(state.pending_inputs.is_empty());
        assert!(state.voided_inputs.is_empty());
        assert_eq!(state.goal.as_ref().unwrap().status, GoalStatus::Active);
        let version = state.version;
        let item_count = state.transcript.len();
        assert!(live.apply_event(events[activation_index].clone()));
        assert_eq!(live.sessions[&session].version, version);
        assert_eq!(live.sessions[&session].transcript.len(), item_count);
    }
}

#[test]
fn steering_boundaries_split_assistants_in_model_input_order_live_and_replay() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempts = std::array::from_fn::<_, 5, _>(|_| AttemptId::new_v7());
    let call = ToolCallId::new_v7();
    let goal_id = GoalId::new_v7();
    let pause = ProducerMessageId::new_v7();
    let cancel = ProducerMessageId::new_v7();
    let control = |message_id, body: &str| EventPayload::ProducerMessageAccepted {
        description: Default::default(),
        message_id,
        producer_owner: ProducerOwner::GoalControl { goal_id },
        mode: ProducerDeliveryMode::Steer,
        idempotency_key: cookie_agent_protocol::ProducerIdempotencyKey::new(body).unwrap(),
        body: body.into(),
        reminder: None,
        agent_hop: None,
    };
    let commit = |seq, attempt, input, content, variant| {
        let mut stored = turn_committed(
            session,
            seq,
            run,
            attempt,
            seq,
            content,
            Vec::new(),
            variant,
        );
        let EventPayload::ModelTurnCommitted {
            input_through_seq, ..
        } = &mut stored.payload
        else {
            unreachable!()
        };
        *input_through_seq = input;
        stored
    };
    let mut events = vec![
        session_created(session, 1),
        run_started_with_suffix(session, 2, run, vec![resolved_model(None)]),
        event(
            session,
            3,
            run,
            EventPayload::UserInputSubmitted {
                input: "initial request".into(),
            },
        ),
        event(
            session,
            4,
            run,
            EventPayload::UserInputApplied { user_input_seq: 3 },
        ),
        attempt_started(session, 5, run, attempts[0], None),
        text_delta(session, 6, run, attempts[0], "before steering"),
        commit(
            7,
            attempts[0],
            4,
            vec![text_part("before steering"), tool_part("call-one")],
            None,
        ),
        tool_started_at(session, 8, run, call, 7, "call-one", 1, "bash", None),
        runless_event(
            session,
            9,
            EventPayload::GoalActivated {
                goal_id,
                objective: "test objective".into(),
                revision: 1,
                selection: None,
            },
        ),
        runless_event(
            session,
            10,
            EventPayload::GoalLifecycleChanged {
                goal_id,
                status: GoalStatus::Paused,
                revision: 2,
                selection: None,
            },
        ),
        // Receipt during tool work is not the model-visible boundary.
        runless_event(session, 11, control(pause, "pause steering")),
        event(
            session,
            12,
            run,
            EventPayload::GoalChecklistRevised {
                goal_id,
                items: Vec::new(),
                revision: 3,
            },
        ),
        tool_terminated(
            session,
            13,
            run,
            call,
            7,
            "call-one",
            cookie_agent_protocol::ToolTerminationOutcome::Completed,
        ),
        event(
            session,
            14,
            run,
            EventPayload::ProducerMessageAdmitted { message_id: pause },
        ),
        event(
            session,
            15,
            run,
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![pause],
            },
        ),
        attempt_started(session, 16, run, attempts[1], None),
        text_delta(session, 17, run, attempts[1], "after pause"),
        runless_event(
            session,
            18,
            EventPayload::GoalLifecycleChanged {
                goal_id,
                status: GoalStatus::Cancelled,
                revision: 4,
                selection: None,
            },
        ),
        runless_event(session, 19, control(cancel, "cancel steering")),
        text_delta(session, 20, run, attempts[1], " still before cancel"),
        commit(
            21,
            attempts[1],
            15,
            vec![text_part("after pause still before cancel")],
            None,
        ),
        // Consumption is recorded after the response, not where it belongs visually.
        event(
            session,
            22,
            run,
            EventPayload::ProducerMessageConsumed {
                message_id: pause,
                run_id: run,
            },
        ),
        event(
            session,
            23,
            run,
            EventPayload::ProducerMessagesReleased { claim_seq: 15 },
        ),
        event(
            session,
            24,
            run,
            EventPayload::ProducerMessageAdmitted { message_id: cancel },
        ),
        event(
            session,
            25,
            run,
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![cancel],
            },
        ),
        attempt_started(session, 26, run, attempts[2], None),
        event(
            session,
            27,
            run,
            EventPayload::UserInputAdmitted {
                input: "user steering".into(),
            },
        ),
        commit(28, attempts[2], 25, vec![text_part("after cancel")], None),
        event(
            session,
            29,
            run,
            EventPayload::ProducerMessageConsumed {
                message_id: cancel,
                run_id: run,
            },
        ),
        event(
            session,
            30,
            run,
            EventPayload::ProducerMessagesReleased { claim_seq: 25 },
        ),
        event(
            session,
            31,
            run,
            EventPayload::UserInputSubmitted {
                input: "user steering".into(),
            },
        ),
        event(
            session,
            32,
            run,
            EventPayload::UserInputApplied { user_input_seq: 31 },
        ),
        attempt_started(session, 33, run, attempts[3], Some("high")),
        text_delta(session, 34, run, attempts[3], "after user"),
        text_delta(session, 35, run, attempts[3], " appended"),
        commit(
            36,
            attempts[3],
            32,
            vec![text_part("after user appended")],
            Some("high"),
        ),
        attempt_started(session, 37, run, attempts[4], Some("high")),
        text_delta(session, 38, run, attempts[4], "latest turn"),
        commit(
            39,
            attempts[4],
            36,
            vec![text_part("latest turn")],
            Some("high"),
        ),
    ];
    for stored in &mut events {
        stored.timestamp = Timestamp::new(stored.seq as i64, 0).unwrap();
    }
    let mut live = StateStore::default();
    let mut caches = [LayoutCache::default(), LayoutCache::default()];
    let expanded = HashSet::from([BlockId::Tool(call)]);
    for (index, stored) in events.iter().enumerate() {
        assert!(live.apply_event(stored.clone()));
        let state = &live.sessions[&session];
        let mut replay = StateStore::default();
        assert!(replay.rebuild_session(session, 0, events[..=index].to_vec()));
        assert_eq!(
            assistant_projection(state),
            assistant_projection(&replay.sessions[&session])
        );
        for (width, cache) in [18, 80].into_iter().zip(&mut caches) {
            ensure_cached_transcript_layout(
                cache,
                session,
                state,
                None,
                Some(&expanded),
                width,
                &Theme::default(),
                &PlainHighlighter,
                crate::state::EventLevel::Warning,
                0,
            );
            let layout = |state: &SessionState| {
                transcript_layout_with_level(
                    state,
                    Some(&expanded),
                    width,
                    &Theme::default(),
                    &PlainHighlighter,
                    crate::state::EventLevel::Warning,
                )
            };
            let fresh = layout(state);
            let rebuilt = layout(&replay.sessions[&session]);
            assert_eq!(
                cache.layout.lines, fresh.lines,
                "cached seq {} width {width}",
                stored.seq
            );
            assert_eq!(cache.layout.regions, fresh.regions);
            assert_eq!(cache.layout.user_regions, fresh.user_regions);
            assert_eq!(
                fresh.lines, rebuilt.lines,
                "replay seq {} width {width}",
                stored.seq
            );
            assert_eq!(fresh.regions, rebuilt.regions);
            assert_eq!(fresh.user_regions, rebuilt.user_regions);
            for line in &fresh.lines {
                assert!(
                    line.width() <= usize::from(width),
                    "seq {} width {width}: {line}",
                    stored.seq
                );
            }
        }
        if stored.seq == 13 {
            assert_eq!(assistant_projection(state).len(), 1);
            assert!(state.open_run_assistant.is_some());
            assert!(state.pending_inputs.is_empty());
        }
        if stored.seq == 20 {
            let projection = assistant_projection(state);
            assert_eq!(projection.len(), 2, "acceptance must not split streaming");
            assert!(
                projection[1]
                    .2
                    .iter()
                    .any(|text| text.contains("after pause still before cancel"))
            );
            let rendered = snapshot_lines(&caches[1].layout.lines);
            assert!(
                !rendered.contains("pause steering"),
                "claimed is not consumed"
            );
        }
    }
    let state = &live.sessions[&session];
    let conversation = state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::User { text, .. } => Some(format!("user: {text}")),
            TranscriptItem::Assistant {
                children,
                attribution,
                id,
                ..
            } => {
                assert_eq!(attribution.agent, agent_id());
                let texts = children
                    .iter()
                    .filter_map(|child| match child {
                        AssistantChild::Text { markdown, .. } => Some(markdown.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let metrics = state.assistant_metrics[id];
                assert_eq!(metrics.timed_turns, if texts.len() == 2 { 2 } else { 1 });
                assert_eq!(metrics.context_tokens, Some(14));
                Some(format!("assistant: {}", texts.join(" | ")))
            }
            TranscriptItem::ProducerMessage { body, status, .. } => {
                assert_eq!(*status, crate::state::ProducerMessageStatus::Consumed);
                Some(format!("producer: {body}"))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        conversation,
        [
            "user: initial request",
            "assistant: before steering",
            "producer: pause steering",
            "assistant: after pause still before cancel",
            "producer: cancel steering",
            "assistant: after cancel",
            "user: user steering",
            "assistant: after user appended | latest turn",
        ]
    );
    let first_assistant = state
        .transcript
        .iter()
        .find(|item| matches!(item, TranscriptItem::Assistant { .. }))
        .unwrap();
    assert!(children_has_tool(first_assistant, call));
    assert_eq!(state.tools[&call].status, ToolStatus::Completed);
    assert_eq!(state.turn_items[&7], first_assistant.id());
    assert_ne!(state.turn_items[&7], state.turn_items[&21]);
    assert_eq!(state.turn_items[&36], state.turn_items[&39]);
    assert!(state.pending_inputs.is_empty());
    assert!(state.voided_inputs.is_empty());
    let layout = &caches[1].layout;
    assert_eq!(
        layout
            .user_regions
            .iter()
            .map(|region| region.seq)
            .collect::<Vec<_>>(),
        [3, 31]
    );
    let rendered = snapshot_lines(&layout.lines);
    assert_eq!(rendered.matches("◇ ▸ goal control · steer").count(), 2);
    assert!(!rendered.contains("Continue"));
    assert_eq!(
        rendered
            .matches("primary • gateway/arbitrary-model[base]")
            .count(),
        3
    );
    assert_eq!(
        rendered
            .matches("primary • gateway/arbitrary-model[high]")
            .count(),
        1
    );
}

#[test]
fn replayed_failure_diagnostic_precedes_output_beyond_the_render_budget() {
    let session = SessionId::new_v7();
    let run = run_id();
    let call_id = ToolCallId::new_v7();
    let output = "output line\n".repeat(1500);
    for streamed in [false, true] {
        let attempt = AttemptId::new_v7();
        let mut terminal = tool_terminated(
            session,
            5,
            run,
            call_id,
            3,
            "bash",
            cookie_agent_protocol::ToolTerminationOutcome::Failed,
        );
        let EventPayload::ToolCallTerminated { termination } = &mut terminal.payload else {
            unreachable!()
        };
        termination.error.as_mut().unwrap().message =
            SafeErrorMessage::new("bash timed out").unwrap();
        if !streamed {
            termination.result = Some(cookie_agent_protocol::PersistedToolResult {
                title: SafeDisplayText::new("Bash").unwrap(),
                output: output.clone(),
                display: None,
                metadata: serde_json::Value::Null,
                retained_output: None,
                truncation: None,
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            });
        }
        let mut events = vec![
            session_created(session, 1),
            attempt_started(session, 2, run, attempt, None),
            turn_committed(
                session,
                3,
                run,
                attempt,
                3,
                vec![tool_part("bash")],
                Vec::new(),
                None,
            ),
            tool_started(session, 4, run, call_id, 3, "bash"),
        ];
        if streamed {
            for (index, chunk) in output.as_bytes().chunks(440).enumerate() {
                events.push(event(
                    session,
                    5 + index as u64,
                    run,
                    EventPayload::ToolCallProgress {
                        tool_call_id: call_id,
                        message: SafeDisplayText::new("bash stdout").unwrap(),
                        display: Some(String::from_utf8(chunk.to_vec()).unwrap()),
                    },
                ));
            }
        }
        terminal.seq = events.last().unwrap().seq + 1;
        events.push(terminal);
        let mut store = StateStore::default();
        assert!(store.rebuild_session(session, 0, events));
        let state = &store.sessions[&session];
        assert!(!state.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::Event {
                level: crate::state::EventLevel::Error,
                ..
            }
        )));
        for expand_output in [false, true] {
            let mut expanded = HashSet::from([BlockId::Tool(call_id)]);
            if expand_output {
                expanded.insert(tool_output_id(call_id, ToolOutputSection::Detail));
            }
            let layout = transcript_layout(state, Some(&expanded), 80);
            let rows = layout
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            assert!(
                rows.iter().any(|row| row.contains("bash timed out")),
                "streamed={streamed}, expanded={expand_output}"
            );
            assert!(
                rows.iter()
                    .filter(|row| row.contains("output line"))
                    .count()
                    < 1500
            );
        }
    }
}

#[test]
fn rebuild_session_matches_live_run_assistant_projection() {
    let session = SessionId::new_v7();
    let run = run_id();
    let first = AttemptId::new_v7();
    let second = AttemptId::new_v7();
    let events = vec![
        attempt_started(session, 1, run, first, None),
        turn_committed(
            session,
            2,
            run,
            first,
            1,
            vec![text_part("first")],
            Vec::new(),
            None,
        ),
        attempt_started(session, 3, run, second, Some("high")),
        text_delta(session, 4, run, second, "partial"),
    ];
    let mut live = StateStore::default();
    for event in events.clone() {
        assert!(live.apply_event(event));
    }
    let mut rebuilt = StateStore::default();
    assert!(rebuilt.rebuild_session(session, 0, events));
    assert_eq!(
        assistant_projection(&live.sessions[&session]),
        assistant_projection(&rebuilt.sessions[&session])
    );
    let live_projection = live.sessions[&session]
        .open_run_assistant
        .as_ref()
        .expect("live run projection");
    let rebuilt_projection = rebuilt.sessions[&session]
        .open_run_assistant
        .as_ref()
        .expect("rebuilt run projection");
    assert_eq!(live_projection.run_id, rebuilt_projection.run_id);
    assert_eq!(
        live_projection.committed_prefix,
        rebuilt_projection.committed_prefix
    );
    assert_eq!(
        live_projection.current_model,
        rebuilt_projection.current_model
    );
}

#[test]
fn a_second_run_starts_a_second_assistant_item() {
    let session = SessionId::new_v7();
    let first_run = run_id();
    let second_run = run_id();
    let first_attempt = AttemptId::new_v7();
    let second_attempt = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        run_started_with_suffix(session, 1, first_run, vec![resolved_model(None)]),
        attempt_started(session, 2, first_run, first_attempt, None),
        text_delta(session, 3, first_run, first_attempt, "first run"),
        event(
            session,
            4,
            first_run,
            EventPayload::RunCompleted { final_text: None },
        ),
        run_started_with_suffix(session, 5, second_run, vec![resolved_model(None)]),
        attempt_started(session, 6, second_run, second_attempt, None),
        text_delta(session, 7, second_run, second_attempt, "second run"),
    ] {
        assert!(store.apply_event(event));
    }
    assert_eq!(assistant_projection(&store.sessions[&session]).len(), 2);
}

#[test]
fn committed_tools_with_same_content_index_link_to_their_own_turns() {
    let session = SessionId::new_v7();
    let run = run_id();
    let first_attempt = AttemptId::new_v7();
    let second_attempt = AttemptId::new_v7();
    let first_call = ToolCallId::new_v7();
    let second_call = ToolCallId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, first_attempt, None),
        turn_committed(
            session,
            2,
            run,
            first_attempt,
            10,
            vec![tool_part("first-call")],
            Vec::new(),
            None,
        ),
        attempt_started(session, 3, run, second_attempt, None),
        turn_committed(
            session,
            4,
            run,
            second_attempt,
            11,
            vec![tool_part("second-call")],
            Vec::new(),
            None,
        ),
        tool_started(session, 5, run, second_call, 11, "second-call"),
        tool_started(session, 6, run, first_call, 10, "first-call"),
    ] {
        assert!(store.apply_event(event));
    }
    let projection = assistant_projection(&store.sessions[&session]);
    assert_eq!(projection.len(), 1);
    assert_eq!(
        projection[0].2,
        vec![format!("tool:{first_call}"), format!("tool:{second_call}")]
    );
}

#[test]
fn runless_attempts_remain_one_item_per_attempt() {
    let session = SessionId::new_v7();
    let first = AttemptId::new_v7();
    let second = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        runless_event(
            session,
            1,
            EventPayload::ModelAttemptStarted {
                attempt_id: first,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved_model(None),
                prompt_fingerprint: Sha256Digest::of_bytes(b"first"),
            },
        ),
        runless_event(
            session,
            2,
            EventPayload::TextDelta {
                attempt_id: first,
                text: "first".into(),
            },
        ),
        runless_event(
            session,
            3,
            EventPayload::ModelAttemptStarted {
                attempt_id: second,
                attempt_ordinal: 2,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved_model(None),
                prompt_fingerprint: Sha256Digest::of_bytes(b"second"),
            },
        ),
        runless_event(
            session,
            4,
            EventPayload::TextDelta {
                attempt_id: second,
                text: "second".into(),
            },
        ),
    ] {
        assert!(store.apply_event(event));
    }
    assert_eq!(assistant_projection(&store.sessions[&session]).len(), 2);
}

#[test]
fn tool_call_only_attempt_adds_no_empty_segments() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, attempt, None),
        turn_committed(
            session,
            2,
            run,
            attempt,
            9,
            vec![tool_part("only-call")],
            Vec::new(),
            None,
        ),
    ] {
        assert!(store.apply_event(event));
    }
    let projection = assistant_projection(&store.sessions[&session]);
    assert_eq!(projection[0].2, vec!["placeholder:9:0"]);
}
