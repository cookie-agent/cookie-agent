use crate::ui::transcript::*;

use cookie_agent_protocol::{AttemptId, EventPayload, ModelCallId, SafeCode, SessionId};

use crate::state::{AssistantChild, StateStore};

use super::support::*;

#[test]
fn streaming_deltas_group_under_the_attempt_header() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, attempt, None),
        reasoning_delta(session, 2, run, attempt, "r1"),
        reasoning_delta(session, 3, run, attempt, "+r2"),
        text_delta(session, 4, run, attempt, "t1"),
        reasoning_delta(session, 5, run, attempt, "r3"),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let TranscriptItem::Assistant { children, .. } = &state.transcript[0] else {
        panic!("assistant item");
    };
    assert!(matches!(
        children.as_slice(),
        [
            AssistantChild::Thinking { text, .. },
            AssistantChild::Text { markdown, .. },
            AssistantChild::Thinking { text: second, .. },
        ] if text == "r1+r2" && markdown.as_str() == "t1" && second == "r3"
    ));
}

#[test]
fn committed_turn_appends_unstreamed_content_in_model_order() {
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
            1,
            vec![
                cookie_agent_protocol::PersistedAssistantPart::Text {
                    text: "durable text".into(),
                    metadata: None,
                },
                cookie_agent_protocol::PersistedAssistantPart::Reasoning {
                    text: "durable thinking".into(),
                    metadata: None,
                },
            ],
            vec!["context near limit"],
            None,
        ),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let TranscriptItem::Assistant { children, .. } = &state.transcript[0] else {
        panic!("assistant item");
    };
    assert!(matches!(
        children.as_slice(),
        [
            AssistantChild::Text { markdown, .. },
            AssistantChild::Thinking { text, .. },
        ] if markdown.as_str() == "durable text" && text == "durable thinking"
    ));
    assert!(state.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::Event {
            level: crate::state::EventLevel::Warning,
            text,
            ..
        } if text.contains("context near limit")
    )));
}

#[test]
fn empty_deltas_do_not_open_parts() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, attempt, None),
        text_delta(session, 2, run, attempt, ""),
        reasoning_delta(session, 3, run, attempt, ""),
        text_delta(session, 4, run, attempt, "hi"),
        turn_committed(
            session,
            5,
            run,
            attempt,
            1,
            vec![text_part("hi")],
            Vec::new(),
            None,
        ),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let assistants = assistant_items(state);
    assert_eq!(assistants.len(), 1);
    let TranscriptItem::Assistant { children, .. } = assistants[0] else {
        unreachable!()
    };
    assert_eq!(children.len(), 1, "empty deltas must not leave blank parts");
    assert_eq!(assistant_texts(assistants[0]), ["hi"]);
}

#[test]
fn whitespace_only_committed_parts_are_dropped() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, attempt, None),
        text_delta(session, 2, run, attempt, ""),
        reasoning_delta(session, 3, run, attempt, "thinking hard"),
        text_delta(session, 4, run, attempt, "\n\n"),
        turn_committed(
            session,
            5,
            run,
            attempt,
            1,
            vec![
                text_part("\n\n"),
                cookie_agent_protocol::PersistedAssistantPart::Reasoning {
                    text: "thinking hard".into(),
                    metadata: None,
                },
                cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                    id: ModelCallId::new("call-1").expect("call"),
                    provider_item_id: None,
                    name: SafeCode::new("bash").expect("tool"),
                    input: serde_json::json!({"command": "ls"}),
                    raw_input: None,
                    metadata: None,
                },
            ],
            Vec::new(),
            None,
        ),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let assistants = assistant_items(state);
    assert_eq!(assistants.len(), 1);
    let TranscriptItem::Assistant { children, .. } = assistants[0] else {
        unreachable!()
    };
    assert!(
        matches!(
            children.as_slice(),
            [
                AssistantChild::Thinking { .. },
                AssistantChild::CommittedTool { .. }
            ]
        ),
        "whitespace-only text part must not render a blank line: {children:?}"
    );

    // Once the block holds committed content, the same whitespace-only
    // part is meaningful spacing between turns and stays.
    let second = AttemptId::new_v7();
    for event in [
        attempt_started(session, 6, run, second, None),
        turn_committed(
            session,
            7,
            run,
            second,
            2,
            vec![text_part("\n\n"), text_part("the answer")],
            Vec::new(),
            None,
        ),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let assistants = assistant_items(state);
    assert_eq!(assistants.len(), 1);
    let TranscriptItem::Assistant { children, .. } = assistants[0] else {
        unreachable!()
    };
    assert!(
        matches!(
            children.as_slice(),
            [
                AssistantChild::Thinking { .. },
                AssistantChild::CommittedTool { .. },
                AssistantChild::Text { .. },
                AssistantChild::Text { .. }
            ]
        ),
        "mid-block whitespace spacing is preserved: {children:?}"
    );
}

#[test]
fn low_level_events_do_not_split_in_flight_block() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, attempt, None),
        text_delta(session, 2, run, attempt, "a"),
        mid_stream_info(session, 3, run),
        text_delta(session, 4, run, attempt, "b"),
        reasoning_delta(session, 5, run, attempt, "still one block"),
        turn_committed(
            session,
            6,
            run,
            attempt,
            1,
            vec![
                text_part("ab"),
                cookie_agent_protocol::PersistedAssistantPart::Reasoning {
                    text: "still one block".into(),
                    metadata: None,
                },
            ],
            Vec::new(),
            None,
        ),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let assistants = assistant_items(state);
    assert_eq!(
        assistants.len(),
        1,
        "Info rows never split an in-flight block"
    );
    assert_eq!(assistant_texts(assistants[0]), ["ab"]);
}

#[test]
fn event_mid_stream_keeps_current_part_in_existing_block() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, attempt, None),
        text_delta(session, 2, run, attempt, "hello"),
        mid_stream_failure(session, 3, run),
        text_delta(session, 4, run, attempt, " world"),
        turn_committed(
            session,
            5,
            run,
            attempt,
            1,
            vec![cookie_agent_protocol::PersistedAssistantPart::Text {
                text: "hello world".into(),
                metadata: None,
            }],
            Vec::new(),
            None,
        ),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let assistants = assistant_items(state);
    assert_eq!(assistants.len(), 1, "text-only turn stays in one block");
    assert_eq!(assistant_texts(assistants[0]), ["hello world"]);
    // The event row lands after the block; the block keeps its place.
    let kinds = state
        .transcript
        .iter()
        .map(|item| match item {
            TranscriptItem::Assistant { .. } => "assistant",
            TranscriptItem::Event { .. } => "event",
            _ => "other",
        })
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["assistant", "event", "event"]);
}

#[test]
fn event_mid_stream_splits_new_segment_below_event() {
    let session = SessionId::new_v7();
    let run = run_id();
    let first = AttemptId::new_v7();
    let second = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, first, None),
        text_delta(session, 2, run, first, "one"),
        turn_committed(
            session,
            3,
            run,
            first,
            1,
            vec![cookie_agent_protocol::PersistedAssistantPart::Text {
                text: "one".into(),
                metadata: None,
            }],
            Vec::new(),
            None,
        ),
        attempt_started(session, 4, run, second, None),
        text_delta(session, 5, run, second, "two"),
        mid_stream_failure(session, 6, run),
        reasoning_delta(session, 7, run, second, "think"),
        turn_committed(
            session,
            8,
            run,
            second,
            2,
            vec![
                cookie_agent_protocol::PersistedAssistantPart::Text {
                    text: "two".into(),
                    metadata: None,
                },
                cookie_agent_protocol::PersistedAssistantPart::Reasoning {
                    text: "think".into(),
                    metadata: None,
                },
            ],
            Vec::new(),
            None,
        ),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let assistants = assistant_items(state);
    assert_eq!(assistants.len(), 2, "new segment splits into a fresh block");
    // The first block keeps exactly the previously committed turn; the
    // pre-event streamed text is superseded by the canonical rebuild in
    // the new block below the event row.
    assert_eq!(assistant_texts(assistants[0]), ["one"]);
    assert_eq!(assistant_texts(assistants[1]), ["two"]);
    let TranscriptItem::Assistant { children, .. } = assistants[1] else {
        unreachable!()
    };
    assert!(
        children
            .iter()
            .any(|child| matches!(child, AssistantChild::Thinking { text, .. } if text == "think"))
    );
    // Ordering: first block, then the event row, then the new block.
    let kinds = state
        .transcript
        .iter()
        .map(|item| match item {
            TranscriptItem::Assistant { .. } => "assistant",
            TranscriptItem::Event { .. } => "event",
            _ => "other",
        })
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["assistant", "event", "event", "assistant", "event"]);
}

#[test]
fn event_mid_stream_tool_turn_moves_below_event() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, attempt, None),
        text_delta(session, 2, run, attempt, "A"),
        mid_stream_failure(session, 3, run),
        turn_committed(
            session,
            4,
            run,
            attempt,
            1,
            vec![
                cookie_agent_protocol::PersistedAssistantPart::Text {
                    text: "A".into(),
                    metadata: None,
                },
                cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                    id: ModelCallId::new("call-1").expect("call"),
                    provider_item_id: None,
                    name: SafeCode::new("bash").expect("tool"),
                    input: serde_json::json!({"command": "ls"}),
                    raw_input: None,
                    metadata: None,
                },
            ],
            Vec::new(),
            None,
        ),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let assistants = assistant_items(state);
    assert_eq!(assistants.len(), 1, "turn consolidates below the event");
    let TranscriptItem::Assistant { children, .. } = assistants[0] else {
        unreachable!()
    };
    assert!(matches!(
        children.as_slice(),
        [
            AssistantChild::Text { .. },
            AssistantChild::CommittedTool { .. }
        ]
    ));
    // The event row precedes the block holding the tool call.
    let first = state.transcript.first().expect("event row");
    assert!(matches!(first, TranscriptItem::Event { .. }));
}

#[test]
fn abandonment_after_event_split_prunes_both_blocks() {
    let session = SessionId::new_v7();
    let run = run_id();
    let first = AttemptId::new_v7();
    let second = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, first, None),
        text_delta(session, 2, run, first, "A"),
        mid_stream_failure(session, 3, run),
        reasoning_delta(session, 4, run, first, "R"),
        event(
            session,
            5,
            run,
            EventPayload::AttemptAbandoned {
                attempt_id: first,
                model_error: None,
            },
        ),
        attempt_started(session, 6, run, second, None),
        text_delta(session, 7, run, second, "final"),
        turn_committed(
            session,
            8,
            run,
            second,
            1,
            vec![cookie_agent_protocol::PersistedAssistantPart::Text {
                text: "final".into(),
                metadata: None,
            }],
            Vec::new(),
            None,
        ),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let assistants = assistant_items(state);
    assert_eq!(assistants.len(), 1, "abandoned split blocks are pruned");
    assert_eq!(assistant_texts(assistants[0]), ["final"]);
}

#[test]
fn event_between_turns_does_not_split_block() {
    let session = SessionId::new_v7();
    let run = run_id();
    let first = AttemptId::new_v7();
    let second = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, first, None),
        text_delta(session, 2, run, first, "one"),
        turn_committed(
            session,
            3,
            run,
            first,
            1,
            vec![cookie_agent_protocol::PersistedAssistantPart::Text {
                text: "one".into(),
                metadata: None,
            }],
            vec!["context near limit"],
            None,
        ),
        attempt_started(session, 4, run, second, None),
        text_delta(session, 5, run, second, "two"),
        turn_committed(
            session,
            6,
            run,
            second,
            2,
            vec![cookie_agent_protocol::PersistedAssistantPart::Text {
                text: "two".into(),
                metadata: None,
            }],
            Vec::new(),
            None,
        ),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let assistants = assistant_items(state);
    assert_eq!(
        assistants.len(),
        1,
        "events between turns keep the run's turns in one block"
    );
    assert_eq!(assistant_texts(assistants[0]), ["one", "two"]);
}
