use crate::ui::transcript::*;

use cookie_agent_protocol::{AttemptId, ModelCallId, SafeCode, SessionId, ToolCallId};

use crate::state::{AssistantChild, StateStore, ToolCallState};

use super::support::*;

#[tokio::test]
async fn running_tool_rows_pulse_with_the_clock() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let call = ToolCallId::new_v7();
    for event in [
        session_created(session, 1),
        attempt_started(session, 2, run, attempt, None),
        turn_committed(
            session,
            3,
            run,
            attempt,
            1,
            vec![cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                id: ModelCallId::new("call-1").expect("call"),
                provider_item_id: None,
                name: SafeCode::new("bash").expect("tool"),
                input: serde_json::json!({"command": "sleep 2"}),
                raw_input: None,
                metadata: None,
            }],
            Vec::new(),
            None,
        ),
        tool_started_at(
            session,
            4,
            run,
            call,
            1,
            "call-1",
            0,
            "bash",
            Some("sleep 2"),
        ),
    ] {
        assert!(app.store.apply_event(event));
    }
    app.selected = Some(session);

    // A running tool keeps the animation clock alive on its own.
    assert!(app.animation_active());
    let state = &app.store.sessions[&session];
    let mut cache = LayoutCache::default();
    let mut seen = Vec::new();
    for bucket in 0..4u8 {
        ensure_cached_transcript_layout(
            &mut cache,
            session,
            state,
            None,
            None,
            60,
            &Theme::default(),
            &crate::markdown::SyntectHighlighter::default(),
            crate::state::EventLevel::Debug,
            bucket,
        );
        seen.push(snapshot_lines(&cache.layout.lines));
    }
    assert!(seen[0].contains("💻 ▸ bash sleep 2 …"), "{}", seen[0]);
    assert!(seen[1].contains("💻 ▸ bash sleep 2 ."), "{}", seen[1]);
    assert!(seen[2].contains("💻 ▸ bash sleep 2 .."), "{}", seen[2]);
    assert!(seen[3].contains("💻 ▸ bash sleep 2 ..."), "{}", seen[3]);
    // Each bucket re-rendered the cached live item in place.
    assert!(seen.windows(2).all(|pair| pair[0] != pair[1]));

    // Completion settles the row and stops the clock.
    assert!(app.store.apply_event(tool_terminated(
        session,
        5,
        run,
        call,
        1,
        "call-1",
        cookie_agent_protocol::ToolTerminationOutcome::Completed,
    )));
    assert!(!app.animation_active());
    let state = &app.store.sessions[&session];
    let settled = snapshot_lines(&transcript_layout(state, None, 60).lines);
    assert!(settled.contains("💻 ▸ bash sleep 2"), "{settled}");
    assert!(!settled.contains('…'), "{settled}");
}

#[test]
fn expandable_rows_render_emoji_before_collapsed_and_expanded_chevrons() {
    let call_id = ToolCallId::new_v7();
    let mut state = assistant_state(vec![
        AssistantChild::Thinking {
            id: 10,
            version: 0,
            text: "thought".into(),
        },
        AssistantChild::Tool { call_id },
    ]);
    state.tools.insert(
        call_id,
        ToolCallState {
            id: call_id,
            owner: owner(1, "call-1"),
            presentation: presentation("bash", Some("true")),
            arguments: r#"{"command":"true"}"#.into(),
            status: ToolStatus::Completed,
            detail: String::new(),
            has_output_chunks: false,
        },
    );

    let collapsed = snapshot_lines(&transcript_layout(&state, None, 60).lines);
    assert!(collapsed.contains("💭 ▸ thought"));
    assert!(collapsed.contains("💻 ▸ bash true"));

    let expanded = HashSet::from([BlockId::Thinking(10), BlockId::Tool(call_id)]);
    let expanded_layout = transcript_layout(&state, Some(&expanded), 60);
    let expanded_rendered = snapshot_lines(&expanded_layout.lines);
    assert!(expanded_rendered.contains("💭 ▾ thought"));
    assert!(expanded_rendered.contains("💻 ▾ bash true"));
    assert_eq!(expanded_layout.regions.len(), 2);

    let tiny = transcript_layout(&state, Some(&expanded), 4);
    assert_eq!(tiny.regions.len(), 2);
    for width in [6, 7] {
        let layout = transcript_layout(&state, Some(&expanded), width);
        assert_eq!(layout.regions.len(), 2);
        let rendered = snapshot_lines(&layout.lines);
        assert!(rendered.contains('💭'));
        // Below 8 columns `tool_block_lines` swaps the assistant gutter for a
        // compact `[T✓]` role label, which leaves no room for the tool's own
        // icon: the chevron survives as the only expand/collapse cue.
        assert!(rendered.contains("[T✓] ▾"), "{rendered:?}");
        assert!(!rendered.contains('💻'), "{rendered:?}");
        assert_eq!(rendered.matches('▾').count(), 2);
    }
    // Every row of the block from four columns up: the narrow role label and
    // its continuation indent may simplify, but must never overflow. Below
    // four columns even the two-column row icons cannot be drawn.
    for width in [4, 5, 6, 7, 8, 12, 18] {
        let layout = transcript_layout(&state, Some(&expanded), width);
        assert!(
            layout.lines.iter().all(|line| {
                UnicodeWidthStr::width(line.to_string().as_str()) <= usize::from(width)
            }),
            "width {width}: {}",
            snapshot_lines(&layout.lines)
        );
    }
}

#[test]
fn unstarted_committed_tool_placeholder_renders_pending_row_not_error() {
    let state = assistant_state(vec![AssistantChild::CommittedTool {
        turn_seq: 10,
        content_index: 0,
        name: SafeCode::new("bash").expect("tool"),
    }]);
    let layout = transcript_layout(&state, None, 60);
    let text = snapshot_lines(&layout.lines);
    assert!(text.contains("│  💻 ▸ bash · pending"), "{text}");
    assert!(!text.contains("TOOL RUNNING"), "{text}");
    assert!(!text.contains("unavailable payload"), "{text}");
}

#[test]
fn committed_tool_blocks_from_two_turns_have_distinct_ids() {
    let state = assistant_state(vec![
        AssistantChild::CommittedTool {
            turn_seq: 10,
            content_index: 0,
            name: SafeCode::new("bash").expect("tool"),
        },
        AssistantChild::CommittedTool {
            turn_seq: 11,
            content_index: 0,
            name: SafeCode::new("bash").expect("tool"),
        },
    ]);
    let layout = transcript_layout(&state, None, 60);
    assert_eq!(
        layout
            .regions
            .iter()
            .map(|region| region.id)
            .collect::<Vec<_>>(),
        vec![
            BlockId::CommittedTool {
                turn_seq: 10,
                content_index: 0,
            },
            BlockId::CommittedTool {
                turn_seq: 11,
                content_index: 0,
            },
        ]
    );
}

#[test]
fn narrow_attribution_marker_preserves_its_gutter() {
    let state = assistant_state(vec![AssistantChild::Attribution {
        resolved_model: resolved_model(Some("high")),
    }]);
    let width = 18;
    let marker_lines = transcript_layout(&state, None, width)
        .lines
        .into_iter()
        .filter(|line| line.to_string().contains("now") || line.to_string().starts_with("├─ "))
        .collect::<Vec<_>>();
    assert!(marker_lines.len() > 1, "marker should wrap at narrow width");
    assert!(marker_lines.iter().all(|line| {
        line.spans
            .first()
            .is_some_and(|span| span.content.as_ref() == "├─ ")
    }));
    assert!(marker_lines.iter().all(|line| {
        unicode_width::UnicodeWidthStr::width(line.to_string().as_str()) <= usize::from(width)
    }));
}

#[test]
fn tool_children_render_compact_titles_with_status_semantics() {
    let call_id = ToolCallId::new_v7();
    let mut state = assistant_state(vec![AssistantChild::Tool { call_id }]);
    state.tools.insert(
        call_id,
        ToolCallState {
            id: call_id,
            owner: owner(1, "call-1"),
            presentation: presentation("bash", Some("touch README.md")),
            arguments: r#"{"command": "touch README.md"}"#.into(),
            status: ToolStatus::Running,
            detail: String::new(),
            has_output_chunks: false,
        },
    );
    let rendered = transcript_layout(&state, None, 60)
        .lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered
            .lines()
            .any(|line| line.trim_end().ends_with("💻 ▸ bash touch README.md …"))
    );
    assert!(!rendered.contains("COMPLETED"));

    state.tools.get_mut(&call_id).expect("tool").status = ToolStatus::Completed;
    let rendered = transcript_layout(&state, None, 60)
        .lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered
            .lines()
            .any(|line| line.trim_end() == "💻 ▸ bash touch README.md"
                || line.trim_end() == "│  💻 ▸ bash touch README.md")
    );
    assert!(!rendered.contains('…'));
    assert!(!rendered.contains("failed"));

    // A command's exit code is data on a completed tool.
    state.tools.get_mut(&call_id).expect("tool").detail = "Exit status: 1".into();
    let expanded = HashSet::from([BlockId::Tool(call_id)]);
    let rendered = transcript_layout(&state, Some(&expanded), 60)
        .lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Exit status: 1"));
    assert!(!rendered.contains("failed"));

    // Genuine execution failures still carry the inline suffix.
    state.tools.get_mut(&call_id).expect("tool").status = ToolStatus::Failed;
    let rendered = transcript_layout(&state, None, 60)
        .lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("💻 ▸ bash touch README.md failed"));
}

#[test]
fn wrapped_tool_arguments_keep_the_assistant_gutter() {
    let call_id = ToolCallId::new_v7();
    let mut state = assistant_state(vec![AssistantChild::Tool { call_id }]);
    state.tools.insert(
        call_id,
        ToolCallState {
            id: call_id,
            owner: owner(1, "call-1"),
            presentation: presentation("bash", None),
            arguments: r#"{"command":"printf a-very-long-single-line-tool-argument"}"#.into(),
            status: ToolStatus::Running,
            detail: String::new(),
            has_output_chunks: false,
        },
    );
    let width = 18;
    let expanded = std::collections::HashSet::from([BlockId::Tool(call_id)]);
    let layout = transcript_layout(&state, Some(&expanded), width);
    let region = layout
        .regions
        .iter()
        .find(|region| region.id == BlockId::Tool(call_id))
        .expect("tool region");
    let body = &layout.lines[region.start_line..region.end_line];

    assert!(body.len() > 2, "long argument should wrap");
    assert!(body.iter().all(|line| {
        line.spans
            .first()
            .is_some_and(|span| span.content.as_ref() == "│ ")
    }));
    assert!(body.iter().all(|line| {
        unicode_width::UnicodeWidthStr::width(line.to_string().as_str()) <= usize::from(width)
    }));
}

#[test]
fn parallel_tool_children_stay_in_committed_order_not_completion_order() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let first = ToolCallId::new_v7();
    let second = ToolCallId::new_v7();
    let mut store = StateStore::default();
    let events = [
        attempt_started(session, 1, run, attempt, None),
        turn_committed(
            session,
            2,
            run,
            attempt,
            7,
            vec![
                cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                    id: ModelCallId::new("call-a").expect("call"),
                    provider_item_id: None,
                    name: SafeCode::new("bash").expect("tool"),
                    input: serde_json::json!({"command": "sleep 2"}),
                    raw_input: None,
                    metadata: None,
                },
                cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                    id: ModelCallId::new("call-b").expect("call"),
                    provider_item_id: None,
                    name: SafeCode::new("bash").expect("tool"),
                    input: serde_json::json!({"command": "true"}),
                    raw_input: None,
                    metadata: None,
                },
            ],
            Vec::new(),
            None,
        ),
        tool_started_at(session, 3, run, first, 7, "call-a", 0, "bash", None),
        tool_started_at(session, 4, run, second, 7, "call-b", 1, "bash", None),
        // The second tool terminates first; order must not change.
        tool_terminated(
            session,
            5,
            run,
            second,
            7,
            "call-b",
            cookie_agent_protocol::ToolTerminationOutcome::Completed,
        ),
    ];
    for event in events {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let TranscriptItem::Assistant { children, .. } = &state.transcript[0] else {
        panic!("assistant item");
    };
    let tool_order = children
        .iter()
        .filter_map(|child| match child {
            AssistantChild::Tool { call_id } => Some(*call_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_order, vec![first, second]);
}

#[test]
fn tool_rows_project_from_committed_turn_ownership() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let call_id = ToolCallId::new_v7();
    let mut store = StateStore::default();
    let events = [
        session_created(session, 1),
        attempt_started(session, 2, run, attempt, Some("high")),
        turn_committed(
            session,
            3,
            run,
            attempt,
            3,
            vec![cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                id: ModelCallId::new("call-1").expect("call"),
                provider_item_id: None,
                name: SafeCode::new("bash").expect("tool"),
                input: serde_json::json!({"command": "git status"}),
                raw_input: None,
                metadata: None,
            }],
            Vec::new(),
            Some("high"),
        ),
        tool_started(session, 4, run, call_id, 3, "call-1"),
    ];
    for event in events {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    let tool = &state.tools[&call_id];
    assert_eq!(tool.presentation.title.as_str(), "call-1");
    assert_eq!(tool.arguments, r#"{"command":"git status"}"#);
    let TranscriptItem::Assistant {
        attribution,
        committed_turn_seq,
        ..
    } = &state.transcript[0]
    else {
        panic!("assistant item");
    };
    assert_eq!(*committed_turn_seq, Some(3));
    assert_eq!(
        attribution.header(),
        "primary • gateway/arbitrary-model[high]"
    );
    assert_eq!(attribution.variant_label(), "high");
    assert!(children_has_tool(&state.transcript[0], call_id));
}
