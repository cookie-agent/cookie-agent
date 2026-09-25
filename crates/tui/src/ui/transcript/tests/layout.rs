use crate::ui::transcript::*;

use cookie_agent_protocol::{
    AttemptId, EventPayload, ModelSelection, OutputDelta, OutputStream, SafeCode, SessionId,
    SessionTree, ToolCallId,
};

use jiff::Timestamp;

use ratatui::text::Line;

use crate::client::ClientDelivery;

use crate::markdown::{MarkdownDocument, PlainHighlighter};

use crate::state::{AssistantChild, StateStore, ToolCallState};

use crate::ui::events::RenderScheduler;

use base64::{Engine as _, engine::general_purpose::STANDARD};

use super::support::*;

#[tokio::test]
async fn full_app_degrades_safely_on_tiny_terminals() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.store.sessions.insert(
        session,
        assistant_state(vec![
            AssistantChild::Thinking {
                id: 1,
                version: 0,
                text: "thought".into(),
            },
            AssistantChild::Text {
                id: 2,
                version: 0,
                markdown: MarkdownDocument::new("answer".into()),
            },
        ]),
    );
    for (width, height) in [(8, 4), (12, 6), (20, 8), (40, 12)] {
        rendered_frame(&mut app, width, height);
    }
}

#[test]
fn mono_and_tiny_transcripts_keep_one_header_and_no_standalone_blocks() {
    let state = assistant_state(vec![
        AssistantChild::Thinking {
            id: 1,
            version: 0,
            text: "thought".into(),
        },
        AssistantChild::Text {
            id: 2,
            version: 0,
            markdown: MarkdownDocument::new("answer".into()),
        },
    ]);
    for width in [4, 6, 12, 80] {
        let rendered = transcript_layout_with(
            &state,
            None,
            width,
            &Theme::new(
                crate::theme::ThemeKind::Mono,
                crate::theme::ColorLevel::None,
            ),
            &PlainHighlighter,
        )
        .lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert!(!rendered.contains("REASONING"));
        assert!(!rendered.contains("TOOL"));
        assert!(!rendered.contains("ASSISTANT"));
    }
}

#[test]
fn render_scheduler_coalesces_streams_and_prioritizes_input() {
    let mut scheduler = RenderScheduler::default();
    let now = std::time::Instant::now();
    assert!(scheduler.should_draw(now));
    scheduler.drew(now);
    scheduler.mark_stream();
    assert!(!scheduler.should_draw(now));
    scheduler.mark_immediate();
    assert!(scheduler.should_draw(now));
}

#[test]
fn replay_evaluations_render_variant_scoped_discards() {
    let session = SessionId::new_v7();
    let run = run_id();
    let mut store = StateStore::default();
    let event = event(
        session,
        1,
        run,
        EventPayload::ModelReplayEvaluated {
            attempt_id: AttemptId::new_v7(),
            resolved_model: resolved_model(Some("high")),
            ordered_decisions: vec![
                cookie_agent_protocol::ReplayDecision {
                    history_index: 0,
                    disposition: cookie_agent_protocol::ReplayDisposition::Replayed,
                },
                cookie_agent_protocol::ReplayDecision {
                    history_index: 1,
                    disposition:
                        cookie_agent_protocol::ReplayDisposition::DiscardedForeignVariant {
                            found: None,
                            expected: Some(
                                cookie_agent_protocol::VariantId::new("high").expect("variant"),
                            ),
                        },
                },
            ],
        },
    );
    assert!(store.apply_event(event));
    let rendered = store.sessions[&session]
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Event { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("discarded foreign variant base (expected high)"));
    assert!(rendered.contains("gateway/arbitrary-model (high, openai-compatible)"));
}

#[test]
fn replay_projection_deduplicates_logical_transitions_without_losing_evidence() {
    let session = SessionId::new_v7();
    let first_run = run_id();
    let second_run = run_id();
    let resolved = resolved_model(None);
    let adapter_discard = cookie_agent_protocol::ReplayDisposition::DiscardedForeignAdapter {
        found: SafeCode::new("anthropic").expect("adapter"),
        expected: SafeCode::new(resolved.adapter_id.as_str()).expect("adapter"),
    };
    let adapter_evidence = vec![
        cookie_agent_protocol::ReplayDecision {
            history_index: 1,
            disposition: adapter_discard.clone(),
        },
        cookie_agent_protocol::ReplayDecision {
            history_index: 1,
            disposition: cookie_agent_protocol::ReplayDisposition::ReconstructedNormalizedHistory,
        },
        cookie_agent_protocol::ReplayDecision {
            history_index: 3,
            disposition: adapter_discard.clone(),
        },
        cookie_agent_protocol::ReplayDecision {
            history_index: 3,
            disposition: cookie_agent_protocol::ReplayDisposition::ReconstructedNormalizedHistory,
        },
    ];
    let other_model = ModelSelection {
        model: "other/model".parse().expect("model key"),
        variant: None,
    };
    let events = vec![
        event(
            session,
            1,
            first_run,
            EventPayload::ModelReplayEvaluated {
                attempt_id: AttemptId::new_v7(),
                resolved_model: resolved.clone(),
                ordered_decisions: adapter_evidence.clone(),
            },
        ),
        event(
            session,
            2,
            first_run,
            EventPayload::ModelReplayEvaluated {
                attempt_id: AttemptId::new_v7(),
                resolved_model: resolved.clone(),
                ordered_decisions: adapter_evidence,
            },
        ),
        event(
            session,
            3,
            first_run,
            EventPayload::ModelReplayEvaluated {
                attempt_id: AttemptId::new_v7(),
                resolved_model: resolved.clone(),
                ordered_decisions: vec![cookie_agent_protocol::ReplayDecision {
                    history_index: 5,
                    disposition:
                        cookie_agent_protocol::ReplayDisposition::DiscardedForeignModelSelection {
                            found: other_model,
                            expected: resolved.selection.clone(),
                        },
                }],
            },
        ),
        event(
            session,
            4,
            first_run,
            EventPayload::ModelReplayEvaluated {
                attempt_id: AttemptId::new_v7(),
                resolved_model: resolved.clone(),
                ordered_decisions: vec![cookie_agent_protocol::ReplayDecision {
                    history_index: 7,
                    disposition:
                        cookie_agent_protocol::ReplayDisposition::DiscardedForeignVariant {
                            found: Some(
                                cookie_agent_protocol::VariantId::new("foreign").expect("variant"),
                            ),
                            expected: None,
                        },
                }],
            },
        ),
        event(
            session,
            5,
            second_run,
            EventPayload::ModelReplayEvaluated {
                attempt_id: AttemptId::new_v7(),
                resolved_model: resolved,
                ordered_decisions: vec![cookie_agent_protocol::ReplayDecision {
                    history_index: 1,
                    disposition: adapter_discard,
                }],
            },
        ),
    ];

    let assert_projection = |store: &StateStore| {
        let projected = &store.sessions[&session].transcript;
        let warnings = projected
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Event {
                    level: crate::state::EventLevel::Warning,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(warnings.len(), 4);
        assert_eq!(
            warnings
                .iter()
                .filter(|warning| warning.contains("discarded foreign adapter"))
                .count(),
            2
        );
        assert_eq!(
            warnings
                .iter()
                .filter(|warning| warning.contains("discarded foreign model selection"))
                .count(),
            1
        );
        assert_eq!(
            warnings
                .iter()
                .filter(|warning| warning.contains("discarded foreign variant"))
                .count(),
            1
        );
        let reconstructions = projected
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    TranscriptItem::Event {
                        level: crate::state::EventLevel::Debug,
                        text,
                        ..
                    } if text.contains("reconstructed normalized history")
                )
            })
            .count();
        assert_eq!(reconstructions, 4);
    };

    let mut live = StateStore::default();
    for event in events.clone() {
        assert!(live.apply_event(event));
    }
    assert_projection(&live);

    let mut reopened = StateStore::default();
    assert!(reopened.rebuild_session(session, 0, events));
    assert_projection(&reopened);
}

#[test]
fn sustained_raw_output_and_gaps_do_not_change_tool_display() {
    let session = SessionId::new_v7();
    let call = ToolCallId::new_v7();
    let mut store = StateStore::default();
    store.sessions.entry(session).or_default().tools.insert(
        call,
        ToolCallState {
            id: call,
            owner: owner(1, "call-1"),
            presentation: presentation("bash", None),
            arguments: String::new(),
            status: ToolStatus::Running,
            detail: String::new(),
            has_output_chunks: false,
        },
    );
    let before = format!("{store:?}");
    store.apply_delivery(ClientDelivery::OutputGap(
        cookie_agent_protocol::OutputGap {
            call_id: call,
            stream: OutputStream::Stdout,
            next_offset: 3,
        },
    ));
    let data = STANDARD.encode(vec![b'x'; 64 * 1024]);
    for byte_offset in (0..1600).rev() {
        store.apply_delivery(ClientDelivery::OutputDelta(OutputDelta {
            call_id: call,
            stream: OutputStream::Stdout,
            byte_offset: byte_offset * 64 * 1024,
            data: data.clone(),
        }));
    }
    assert_eq!(format!("{store:?}"), before);
}

#[tokio::test]
async fn descendant_warnings_aggregate_with_attribution_without_duplication() {
    let mut app = test_app().await;
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let child_meta = titled_meta(child, "child session", 1);
    app.tree = Some(SessionTree {
        session: titled_meta(root, "root session", 1),
        children: vec![SessionTree {
            session: child_meta.clone(),
            children: Vec::new(),
        }],
    });
    app.tree_root = Some(root);
    push_model_warning(&mut app.store, root, "root warning");
    push_model_warning(&mut app.store, child, "child warning");
    let warnings = app.descendant_warnings(root);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].1.contains("child warning"));
    assert!(warnings[0].1.contains("child session"));
    assert!(
        warnings[0]
            .1
            .contains(&crate::ui::pickers::short_id(&child_meta))
    );
    // The warning carries the durable time of the child's event row.
    let child_state = app.store.sessions.get(&child).expect("child session");
    let event_time = child_state
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Event { .. } => child_state.item_time(item.id()),
            _ => None,
        })
        .expect("warning row time");
    assert_eq!(warnings[0].0, event_time);
}

#[test]
fn item_times_are_durable_and_replay_deterministic() {
    let session = SessionId::new_v7();
    let run = run_id();
    let first = AttemptId::new_v7();
    let second = AttemptId::new_v7();
    let events = || {
        vec![
            attempt_started(session, 1, run, first, None),
            text_delta(session, 2, run, first, "one"),
            turn_committed(
                session,
                3,
                run,
                first,
                1,
                vec![text_part("one")],
                Vec::new(),
                None,
            ),
            attempt_started(session, 4, run, second, None),
            turn_committed(
                session,
                5,
                run,
                second,
                2,
                vec![text_part("two")],
                Vec::new(),
                None,
            ),
        ]
    };
    let mut timed = events();
    for stored in &mut timed {
        stored.timestamp = Timestamp::new(stored.seq as i64, 0).unwrap();
    }
    let mut store = StateStore::default();
    for event in timed {
        assert!(store.apply_event(event));
    }
    let state = store.sessions.get(&session).expect("session");
    let ids = || {
        state
            .transcript
            .iter()
            .map(|item| item.id())
            .collect::<Vec<_>>()
    };
    let times = || {
        ids()
            .into_iter()
            .map(|id| state.item_time(id))
            .collect::<Vec<_>>()
    };
    // Three rows: the assistant block opened eagerly at attempt start,
    // then one info event row per committed turn.
    assert_eq!(times().len(), 3);
    // The recorded times are exactly the event timestamps of their
    // creating events, in push order.
    let expected = [
        Timestamp::new(1, 0).unwrap(),
        Timestamp::new(3, 0).unwrap(),
        Timestamp::new(5, 0).unwrap(),
    ]
    .map(Some);
    assert_eq!(times(), expected);
    // Re-reducing the identical event log reproduces identical times.
    let mut replay = events();
    for stored in &mut replay {
        stored.timestamp = Timestamp::new(stored.seq as i64, 0).unwrap();
    }
    let mut replay_store = StateStore::default();
    for event in replay {
        assert!(replay_store.apply_event(event));
    }
    let replay_state = replay_store.sessions.get(&session).expect("session");
    let replay_times = replay_state
        .transcript
        .iter()
        .map(|item| replay_state.item_time(item.id()))
        .collect::<Vec<_>>();
    assert_eq!(replay_times, times());
}

#[test]
fn descendant_warnings_splice_at_chronological_position() {
    let session = SessionId::new_v7();
    let run = run_id();
    let first = AttemptId::new_v7();
    let second = AttemptId::new_v7();
    let mut events = vec![
        attempt_started(session, 1, run, first, None),
        turn_committed(
            session,
            2,
            run,
            first,
            1,
            vec![text_part("first answer")],
            Vec::new(),
            None,
        ),
        attempt_started(session, 3, run, second, None),
        turn_committed(
            session,
            4,
            run,
            second,
            2,
            vec![text_part("second answer")],
            Vec::new(),
            None,
        ),
    ];
    for stored in &mut events {
        stored.timestamp = Timestamp::new(stored.seq as i64, 0).unwrap();
    }
    let mut store = StateStore::default();
    for event in events {
        assert!(store.apply_event(event));
    }
    let state = store.sessions.get(&session).expect("session");
    // Synthetic layout matching the three-item transcript (times
    // [1, 3, 5]): a system row, then one separator + content row per
    // item.
    let item_count = state.transcript.len();
    assert_eq!(item_count, 3);
    let mut lines = vec![Line::from("system")];
    let mut offsets = Vec::new();
    for index in 0..item_count {
        offsets.push(ItemAssemblyOffset {
            lines: lines.len(),
            regions: 0,
            user_regions: 0,
        });
        lines.push(Line::default());
        lines.push(Line::from(format!("item {index}")));
    }
    let warnings = vec![
        (Timestamp::new(0, 0).unwrap(), "early warning".to_owned()),
        (Timestamp::new(3, 0).unwrap(), "mid warning".to_owned()),
        (Timestamp::new(9, 0).unwrap(), "late warning".to_owned()),
    ];
    let (spliced, shifts) = App::splice_descendant_warnings(
        lines.clone(),
        &offsets,
        state,
        &warnings,
        80,
        &Theme::default(),
    );
    let text_of = |line: &Line<'_>| {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };
    let position = |needle: &str| {
        spliced
            .iter()
            .position(|line| text_of(line).contains(needle))
            .unwrap_or_else(|| panic!("{needle} rendered"))
    };
    // Early warning: after the system prompt, before the first item.
    assert!(position("system") < position("early warning"));
    assert!(position("early warning") < position("item 0"));
    // Mid warning: anchored at item 1 (time 3), before item 2 (time 5).
    assert!(position("item 1") < position("mid warning"));
    assert!(position("mid warning") < position("item 2"));
    // Late warning: after the last item.
    assert!(position("item 2") < position("late warning"));
    // The early warning block is preceded by the one-blank-row rhythm
    // (the block's badge row leads its text row).
    let system = position("system");
    let early = position("early warning");
    assert!(
        spliced[system..early]
            .iter()
            .any(|line| text_of(line).trim().is_empty())
    );
    // Splice shift map: original position → inserted line count.
    assert_eq!(
        shifts
            .iter()
            .map(|(position, _)| *position)
            .collect::<Vec<_>>(),
        [1, 5, 7]
    );
    assert!(shifts.iter().all(|(_, inserted)| *inserted > 0));
    // Empty warnings leave the layout untouched.
    let (untouched, empty_shifts) =
        App::splice_descendant_warnings(lines, &offsets, state, &[], 80, &Theme::default());
    assert_eq!(untouched.len(), 7);
    assert!(empty_shifts.is_empty());
}

#[tokio::test]
async fn descendant_warning_mid_stream_splits_viewed_open_block() {
    let mut app = test_app().await;
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.tree = Some(SessionTree {
        session: titled_meta(root, "root session", 1),
        children: vec![SessionTree {
            session: titled_meta(child, "child session", 1),
            children: Vec::new(),
        }],
    });
    app.tree_root = Some(root);
    app.selected = Some(root);
    // The viewed session streams an open, uncommitted block.
    let root_run = run_id();
    let first = AttemptId::new_v7();
    for event in [
        attempt_started(root, 1, root_run, first, None),
        text_delta(root, 2, root_run, first, "one"),
    ] {
        app.handle_delivery(live_event(event)).await;
    }
    // A descendant warning lands while the viewed session is mid-block.
    let child_run = run_id();
    let child_attempt = AttemptId::new_v7();
    for event in [
        attempt_started(child, 1, child_run, child_attempt, None),
        turn_committed(
            child,
            2,
            child_run,
            child_attempt,
            1,
            Vec::new(),
            vec!["child warning"],
            None,
        ),
    ] {
        app.handle_delivery(live_event(event)).await;
    }
    // The viewed session continues; the pre-warning content finishes in
    // place and the continuation opens a fresh block below the break.
    let second = AttemptId::new_v7();
    for event in [
        attempt_started(root, 3, root_run, second, None),
        text_delta(root, 4, root_run, second, "two"),
        turn_committed(
            root,
            5,
            root_run,
            second,
            2,
            vec![text_part("two")],
            Vec::new(),
            None,
        ),
    ] {
        app.handle_delivery(live_event(event)).await;
    }
    let state = app.store.sessions.get(&root).expect("root session");
    let assistants = assistant_items(state);
    assert_eq!(
        assistants.len(),
        2,
        "descendant warning splits the viewed session's open block"
    );
    assert_eq!(assistant_texts(assistants[0]), ["one"]);
    assert_eq!(assistant_texts(assistants[1]), ["two"]);
    // The descendant warning row remains visible to the viewer, spliced
    // between the two blocks rather than pinned at the bottom.
    let warnings = app.descendant_warnings(root);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].1.contains("child warning"));
}

#[test]
fn tool_output_expansion_invalidates_the_layout_cache() {
    let rows = (1..=70).map(|number| (number, "value")).collect::<Vec<_>>();
    let state = read_tool_state("src/main.rs", ToolStatus::Completed, &read_detail(&rows));
    let call_id = read_tool_id(&state);
    let session = SessionId::new_v7();
    let theme = Theme::default();
    let highlighter = crate::markdown::SyntectHighlighter::default();
    let mut cache = LayoutCache::default();
    let mut expanded = HashSet::from([BlockId::Tool(call_id)]);
    assert!(!ensure_cached_transcript_layout(
        &mut cache,
        session,
        &state,
        None,
        Some(&expanded),
        80,
        &theme,
        &highlighter,
        crate::state::EventLevel::Debug,
        0,
    ));
    let passes = cache.item_layout_passes;
    let collapsed = snapshot_lines(&cache.layout.lines);

    expanded.insert(tool_output_id(call_id, ToolOutputSection::Detail));
    assert!(!ensure_cached_transcript_layout(
        &mut cache,
        session,
        &state,
        None,
        Some(&expanded),
        80,
        &theme,
        &highlighter,
        crate::state::EventLevel::Debug,
        0,
    ));
    assert_eq!(cache.item_layout_passes, passes + 1);
    assert_ne!(snapshot_lines(&cache.layout.lines), collapsed);
}

#[test]
fn child_layout_cache_recomputes_only_the_changed_assistant_segment() {
    let state = assistant_state(vec![
        AssistantChild::Thinking {
            id: 1,
            version: 0,
            text: "stable thought".into(),
        },
        AssistantChild::Text {
            id: 2,
            version: 0,
            markdown: MarkdownDocument::new("stable text".into()),
        },
    ]);
    let mut cache = LayoutCache::default();
    let session = SessionId::new_v7();
    let theme = Theme::default();
    let highlighter = crate::markdown::SyntectHighlighter::default();
    ensure_cached_transcript_layout(
        &mut cache,
        session,
        &state,
        None,
        None,
        60,
        &theme,
        &highlighter,
        crate::state::EventLevel::Debug,
        0,
    );
    let passes = cache.assistant_part_layout_passes;
    assert_eq!(passes, 2);
    let item_passes = cache.item_layout_passes;
    let assembly_passes = cache.item_assembly_passes;
    let line_count = cache.layout.lines.len();
    let lines_ptr = cache.layout.lines.as_ptr();
    // A cache hit for the identical projection recomputes nothing.
    ensure_cached_transcript_layout(
        &mut cache,
        session,
        &state,
        None,
        None,
        60,
        &theme,
        &highlighter,
        crate::state::EventLevel::Debug,
        0,
    );
    assert_eq!(cache.assistant_part_layout_passes, passes);
    assert_eq!(cache.item_layout_passes, item_passes);
    assert_eq!(cache.item_assembly_passes, assembly_passes);
    assert_eq!(cache.layout.lines.len(), line_count);
    assert_eq!(cache.layout.lines.as_ptr(), lines_ptr);
    // A version bump on one child (and its owning item, exactly as the
    // reducer maintains) recomputes only that child.
    let mut changed = state;
    if let TranscriptItem::Assistant {
        version: item_version,
        children,
        ..
    } = &mut changed.transcript[0]
    {
        *item_version = 1;
        if let AssistantChild::Text { version, .. } = &mut children[1] {
            *version = 1;
        }
    }
    ensure_cached_transcript_layout(
        &mut cache,
        session,
        &changed,
        None,
        None,
        60,
        &theme,
        &highlighter,
        crate::state::EventLevel::Debug,
        0,
    );
    assert_eq!(cache.assistant_part_layout_passes, passes + 1);
}

#[test]
fn toggling_a_tool_while_text_streams_in_the_same_item_relayouts_the_tool() {
    let session = SessionId::new_v7();
    let run = run_id();
    let first = AttemptId::new_v7();
    let second = AttemptId::new_v7();
    let call_id = ToolCallId::new_v7();
    let mut store = StateStore::default();
    for event in [
        user_input(session, 1, run, "run it"),
        attempt_started(session, 2, run, first, None),
        turn_committed(
            session,
            3,
            run,
            first,
            3,
            vec![tool_part("call-1")],
            Vec::new(),
            None,
        ),
        tool_started(session, 4, run, call_id, 3, "call-1"),
        attempt_started(session, 5, run, second, None),
        text_delta(session, 6, run, second, "streaming"),
    ] {
        assert!(store.apply_event(event));
    }
    let theme = Theme::default();
    let highlighter = crate::markdown::SyntectHighlighter::default();
    let layout = |cache: &mut LayoutCache, store: &StateStore, expanded: &HashSet<BlockId>| {
        ensure_cached_transcript_layout(
            cache,
            session,
            &store.sessions[&session],
            None,
            Some(expanded),
            60,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
            0,
        );
    };
    let mut cache = LayoutCache::default();
    layout(&mut cache, &store, &HashSet::new());

    // The click and the next streamed delta land in the same frame.
    let expanded = HashSet::from([BlockId::Tool(call_id)]);
    assert!(store.apply_event(text_delta(session, 7, run, second, " tail")));
    layout(&mut cache, &store, &expanded);

    let mut fresh = LayoutCache::default();
    layout(&mut fresh, &store, &expanded);
    assert_eq!(
        snapshot_lines(&cache.layout.lines),
        snapshot_lines(&fresh.layout.lines)
    );
}

#[test]
fn streaming_delta_reassembles_only_the_tail_item() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        user_input(session, 1, run, "stable user message"),
        attempt_started(session, 2, run, attempt, None),
        reasoning_delta(session, 3, run, attempt, "stable thought"),
        text_delta(session, 4, run, attempt, "streaming"),
    ] {
        assert!(store.apply_event(event));
    }
    let mut cache = LayoutCache::default();
    let theme = Theme::default();
    let highlighter = crate::markdown::SyntectHighlighter::default();
    ensure_cached_transcript_layout(
        &mut cache,
        session,
        &store.sessions[&session],
        None,
        None,
        60,
        &theme,
        &highlighter,
        crate::state::EventLevel::Debug,
        0,
    );
    assert_eq!(cache.item_offsets.len(), 2);
    let tail_offset = cache.item_offsets[1];
    let clean_prefix = cache.layout.lines[..tail_offset.lines].to_vec();
    let item_passes = cache.item_layout_passes;
    let assembly_passes = cache.item_assembly_passes;
    let stable_part = &cache.items[1].assistant_parts[0];
    let stable_span_ptr = cache.items[1].layout.lines[stable_part.lines.start].spans[0]
        .content
        .as_ptr();

    assert!(store.apply_event(text_delta(session, 5, run, attempt, " tail")));
    ensure_cached_transcript_layout(
        &mut cache,
        session,
        &store.sessions[&session],
        None,
        None,
        60,
        &theme,
        &highlighter,
        crate::state::EventLevel::Debug,
        0,
    );

    assert_eq!(cache.item_layout_passes, item_passes);
    assert_eq!(cache.item_assembly_passes, assembly_passes + 1);
    assert_eq!(cache.item_offsets[1], tail_offset);
    assert_eq!(cache.layout.lines[..tail_offset.lines], clean_prefix);
    let stable_part = &cache.items[1].assistant_parts[0];
    assert_eq!(
        cache.items[1].layout.lines[stable_part.lines.start].spans[0]
            .content
            .as_ptr(),
        stable_span_ptr
    );
    assert!(snapshot_lines(&cache.layout.lines).contains("streaming tail"));
}

#[test]
fn canonical_parallel_ownership_preserves_content_index_order() {
    let (store, session, first, second) = parallel_tools_state();
    let state = &store.sessions[&session];
    let TranscriptItem::Assistant { children, .. } = state
        .transcript
        .iter()
        .find(|item| matches!(item, TranscriptItem::Assistant { .. }))
        .expect("assistant item")
    else {
        panic!("assistant item")
    };
    let rendered_kinds = children
        .iter()
        .map(|child| match child {
            AssistantChild::Text { .. } => "text",
            AssistantChild::Thinking { .. } => "thinking",
            AssistantChild::Tool { call_id } if *call_id == first => "tool-a",
            AssistantChild::Tool { call_id } if *call_id == second => "tool-b",
            AssistantChild::Tool { .. } => "tool",
            AssistantChild::Attribution { .. } => "attribution",
            AssistantChild::CommittedTool { .. } => "placeholder",
            AssistantChild::MediaFile { .. } => "media",
            AssistantChild::Notice { .. } => "notice",
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rendered_kinds,
        vec!["text", "tool-a", "thinking", "tool-b"],
        "children stay in committed content order"
    );
    let tool_a = &state.tools[&first];
    let tool_b = &state.tools[&second];
    assert_eq!(tool_a.compact_title(), "bash sleep 2");
    assert_eq!(tool_a.status, ToolStatus::Running);
    assert_eq!(tool_b.compact_title(), "read src/lib.rs");
    assert_eq!(tool_b.status, ToolStatus::Completed);
}

#[test]
fn parallel_tools_render_snapshot_across_widths_and_themes() {
    let (store, session, _, _) = parallel_tools_state();
    let state = &store.sessions[&session];
    let mut snapshots = Vec::new();
    for width in [24u16, 48, 96] {
        let layout = transcript_layout_with_level(
            state,
            None,
            width,
            &Theme::default(),
            &crate::markdown::SyntectHighlighter::default(),
            crate::state::EventLevel::Error,
        );
        snapshots.push(format!(
            "== width {width} ==\n{}",
            snapshot_lines(&layout.lines)
        ));
    }
    let mono = transcript_layout_with_level(
        state,
        None,
        48,
        &Theme::new(
            crate::theme::ThemeKind::Mono,
            crate::theme::ColorLevel::None,
        ),
        &crate::markdown::SyntectHighlighter::default(),
        crate::state::EventLevel::Error,
    );
    snapshots.push(format!("== mono 48 ==\n{}", snapshot_lines(&mono.lines)));
    insta::assert_snapshot!(snapshots.join("\n"));
}

#[test]
fn parallel_tools_expanded_hit_regions_track_each_tool() {
    let (store, session, first, second) = parallel_tools_state();
    let state = &store.sessions[&session];
    let expanded = std::collections::HashSet::from([
        BlockId::Tool(first),
        BlockId::Tool(second),
        BlockId::Thinking(6),
    ]);
    let layout = transcript_layout_with_level(
        state,
        Some(&expanded),
        60,
        &Theme::default(),
        &crate::markdown::SyntectHighlighter::default(),
        crate::state::EventLevel::Error,
    );
    // Tool and thinking rows are expanded. This synthetic log has no
    // RunStarted event, so it has no pinned system prompt.
    let rendered = snapshot_lines(&layout.lines);
    assert_eq!(rendered.matches('▸').count(), 0);
    assert_eq!(rendered.matches('▾').count(), 3);
    assert!(
        rendered.contains("arguments: {\"command\":\"sleep 2\"}") || rendered.contains("sleep 2")
    );
    let tool_regions = layout
        .regions
        .iter()
        .filter(|region| matches!(region.id, BlockId::Tool(_)))
        .count();
    assert_eq!(tool_regions, 2);
    insta::assert_snapshot!(rendered);
}

#[test]
fn cancelled_and_interrupted_tools_render_distinct_concise_markers() {
    let call_cancelled = ToolCallId::new_v7();
    let call_interrupted = ToolCallId::new_v7();
    let mut state = assistant_state(vec![
        AssistantChild::Tool {
            call_id: call_cancelled,
        },
        AssistantChild::Tool {
            call_id: call_interrupted,
        },
    ]);
    for (call_id, status) in [
        (call_cancelled, ToolStatus::Cancelled),
        (call_interrupted, ToolStatus::Interrupted),
    ] {
        state.tools.insert(
            call_id,
            ToolCallState {
                id: call_id,
                owner: owner(1, "call-1"),
                presentation: presentation("bash", Some("make")),
                arguments: "{}".into(),
                status,
                detail: String::new(),
                has_output_chunks: false,
            },
        );
    }
    let rendered = snapshot_lines(&transcript_layout(&state, None, 60).lines);
    assert!(rendered.contains("💻 ▸ bash make cancelled"));
    assert!(rendered.contains("💻 ▸ bash make interrupted"));
    assert!(!rendered.contains("failed"));
    assert!(!rendered.contains("COMPLETED"));
}
