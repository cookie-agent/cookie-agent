use crate::ui::transcript::*;

use cookie_agent_protocol::{
    AgentId, AttemptId, EventPayload, ProviderId, RunId, SafeCode, SafeDisplayText, SessionId,
    Sha256Digest,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use ratatui::{Terminal, backend::TestBackend, text::Line};

use crate::client::ClientDelivery;

use crate::state::{AssistantChild, SessionState, StateStore};

use crate::theme::{ColorLevel, ThemeKind};

use crate::ui::app::*;

use crate::ui::slash::SlashCommand;

use cookie_agent_server::MessageFrame;

use super::support::*;

#[test]
fn internal_blocks_are_headerless_without_changing_other_role_headers() {
    let theme = Theme::default();
    let lines = role_block(
        Role::Internal,
        vec![Line::from("⚙ ▸ system prompt"), Line::from("expanded body")],
        80,
        &theme,
    );
    insta::assert_snapshot!(snapshot_lines(&lines), @"
        · ⚙ ▸ system prompt
        · expanded body
        ");
    assert_eq!(
        snapshot_lines(&role_block(
            Role::Internal,
            vec![Line::from("row")],
            7,
            &theme
        )),
        "[I] row"
    );
    for (role, header) in [
        (Role::User, "┌─ USER"),
        (Role::Action, "-- ACTION"),
        (Role::Goal, "◆─ GOAL"),
        (Role::ToolRunning, "┏… TOOL RUNNING"),
        (Role::ToolSuccess, "┏✓ TOOL SUCCESS"),
        (Role::ToolFailure, "┏! TOOL FAILURE"),
        (Role::Debug, "·· DEBUG [D]"),
        (Role::Warning, "⚠️─ WARNING [W]"),
        (Role::Error, "!! ERROR [E]"),
    ] {
        let rendered = snapshot_lines(&role_block(role, vec![Line::from("row")], 80, &theme));
        assert_eq!(rendered.lines().next(), Some(header));
    }
}

#[test]
fn warning_marker_carries_emoji_presentation_so_width_matches_terminals() {
    // U+26A0 alone is East_Asian_Width=Ambiguous: the width table counts 1
    // while emoji-capable terminals paint 2, so the header overran its
    // wrap budget by one cell and the marker's second cell landed on the
    // block boundary. VS16 (U+FE0F) selects emoji presentation, making the
    // width table agree with the terminal.
    let marker = "\u{26A0}\u{FE0F}";
    assert_eq!(marker.graphemes(true).count(), 1, "VS16 joins the marker");
    assert_eq!(
        UnicodeWidthStr::width(marker),
        2,
        "emoji marker is two cells"
    );

    let theme = Theme::default();
    for width in 8..=80u16 {
        let lines = role_block(Role::Warning, vec![Line::from("row")], width, &theme);
        let header = lines.first().expect("warning header");
        let text: String = header
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            text.contains('\u{FE0F}'),
            "width {width}: warning header keeps VS16: {text:?}"
        );
        assert!(
            header.width() <= usize::from(width),
            "width {width}: warning header {text:?} fits its budget"
        );
    }
}

#[tokio::test]
async fn system_prompt_hover_covers_wrapped_item_rows_but_not_expanded_body() {
    use ratatui::style::Modifier;

    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    assert!(app.store.apply_event(session_created(session, 1)));
    assert!(app.store.apply_event(run_started_with_suffix(
        session,
        2,
        run_id(),
        vec![resolved_model(None)]
    )));
    app.store
        .sessions
        .get_mut(&session)
        .unwrap()
        .run_snapshot
        .as_mut()
        .unwrap()
        .composed_prompt = "expanded body\n".repeat(40);
    for (kind, level) in [
        (ThemeKind::Default, ColorLevel::TrueColor),
        (ThemeKind::Dark, ColorLevel::TrueColor),
        (ThemeKind::Mono, ColorLevel::None),
        (ThemeKind::HighContrast, ColorLevel::Ansi16),
    ] {
        app.theme = Theme::new(kind, level);
        for width in [24, 100] {
            for expanded in [false, true] {
                app.expanded_blocks.insert(
                    session,
                    if expanded {
                        HashSet::from([BlockId::SystemPrompt])
                    } else {
                        HashSet::new()
                    },
                );
                app.conversation_scroll.following = false;
                app.conversation_scroll.offset = 0;
                app.hover = None;
                let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
                terminal.draw(|frame| app.draw_for_test(frame)).unwrap();
                let region = *app
                    .layout_cache
                    .layout
                    .regions
                    .iter()
                    .find(|region| region.id == BlockId::SystemPrompt)
                    .unwrap();
                let header_lines = region.header_lines.unwrap();
                assert!(
                    app.layout_cache.layout.lines[region.start_line]
                        .to_string()
                        .starts_with("· ⚙")
                );
                assert!(!snapshot_lines(&app.layout_cache.layout.lines).contains("EVENT [I]"));
                assert_eq!(header_lines > 1, width == 24);
                if !expanded {
                    assert_eq!(header_lines, region.end_line - region.start_line);
                }
                let mut offsets = vec![0];
                if expanded {
                    offsets.extend([region.start_line + 1, region.start_line + header_lines]);
                }
                for offset in offsets {
                    app.conversation_scroll.following = false;
                    app.conversation_scroll.offset = offset;
                    app.hover = None;
                    terminal.draw(|frame| app.draw_for_test(frame)).unwrap();
                    let before = terminal.backend().buffer().clone();
                    let hit = *app
                        .hit_map
                        .blocks
                        .iter()
                        .find(|hit| hit.id == BlockId::SystemPrompt)
                        .unwrap();
                    assert_eq!(
                        hit.hover_rect.map_or(0, |rect| usize::from(rect.height)),
                        (region.start_line + header_lines).saturating_sub(offset)
                    );
                    app.hover = app.hover_target_at(hit.rect.x, hit.rect.y);
                    assert_eq!(
                        app.hover,
                        hit.toggle_rect
                            .map(|_| HoverTarget::TranscriptBlock(BlockId::SystemPrompt))
                    );
                    terminal.draw(|frame| app.draw_for_test(frame)).unwrap();
                    let after = terminal.backend().buffer();
                    for y in hit.rect.y..hit.rect.bottom() {
                        for x in hit.rect.x..hit.rect.right() {
                            let cell = &after[(x, y)];
                            if hit.hover_rect.is_some_and(|rect| {
                                rect.contains(ratatui::layout::Position::new(x, y))
                            }) {
                                assert_eq!(
                                    cell.bg,
                                    app.theme.block_hover().bg.unwrap_or(before[(x, y)].bg)
                                );
                                assert_eq!(cell.fg, before[(x, y)].fg);
                                assert!(!cell.modifier.contains(Modifier::UNDERLINED));
                                if level == ColorLevel::None {
                                    assert!(cell.modifier.contains(Modifier::BOLD));
                                }
                            } else {
                                assert_eq!(
                                    cell,
                                    &before[(x, y)],
                                    "expanded body changed at {x},{y}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn new_transcript_blocks_are_visible_at_default_threshold_and_expand() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let media = cookie_agent_protocol::PersistedFilePart {
        media_type: cookie_agent_protocol::MimeType::new("image/png").expect("mime"),
        filename: Some("chart.png".into()),
        source: cookie_agent_protocol::PersistedFileSource::Artifact {
            byte_length: 4_096,
            sha256: Sha256Digest::of_bytes(b"image"),
            reference: cookie_agent_protocol::ArtifactReference {
                uri: "artifact://chart".into(),
            },
        },
        metadata: None,
    };
    let events = [
        session_created(session, 1),
        runless_event(
            session,
            2,
            EventPayload::MessageInjected {
                role: cookie_agent_protocol::ExtensionMessageRole::User,
                input: "plugin first\nplugin second".into(),
            },
        ),
        runless_event(
            session,
            3,
            EventPayload::ContextCheckpointCommitted {
                commit: checkpoint_commit("summary first\nsummary second"),
            },
        ),
        run_started_with_suffix(session, 4, run, vec![resolved_model(None)]),
        attempt_started(session, 5, run, attempt, None),
        turn_committed(
            session,
            6,
            run,
            attempt,
            77,
            vec![cookie_agent_protocol::PersistedAssistantPart::File { file: media }],
            Vec::new(),
            None,
        ),
    ];
    let mut store = StateStore::default();
    for event in events {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];

    let collapsed = transcript_layout_with_level(
        state,
        None,
        100,
        &Theme::default(),
        &crate::markdown::SyntectHighlighter::default(),
        crate::state::EventLevel::Warning,
    );
    let rendered = snapshot_lines(&collapsed.lines);
    assert!(!rendered.contains("EVENT [I]"));
    assert!(
        rendered.contains("⚙ ▸ system prompt · primary (last run) (2 lines)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("🧩 ▸ plugin message (user, 2 lines)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("🗜 ▸ context compacted (internal summary, 9000→1200 tokens)"),
        "{rendered}"
    );
    assert!(rendered.contains("🖼 ▸ image/png · chart.png"), "{rendered}");
    assert!(!rendered.contains("plugin first"), "{rendered}");
    assert!(!rendered.contains("summary first"), "{rendered}");
    assert!(!rendered.contains("byte size: 4096"), "{rendered}");
    assert_eq!(
        collapsed
            .regions
            .iter()
            .map(|region| region.id)
            .collect::<Vec<_>>(),
        vec![
            BlockId::SystemPrompt,
            BlockId::PluginMessage(2),
            BlockId::Compaction(3),
            BlockId::MediaFile {
                turn_seq: 77,
                content_index: 0,
            },
        ]
    );

    let expanded = HashSet::from([
        BlockId::SystemPrompt,
        BlockId::PluginMessage(2),
        BlockId::Compaction(3),
        BlockId::MediaFile {
            turn_seq: 77,
            content_index: 0,
        },
    ]);
    for width in [7, 24, 100] {
        let collapsed = transcript_layout(state, None, width);
        let open = transcript_layout(state, Some(&expanded), width);
        for id in [
            BlockId::SystemPrompt,
            BlockId::PluginMessage(2),
            BlockId::Compaction(3),
        ] {
            let closed_region = collapsed
                .regions
                .iter()
                .find(|region| region.id == id)
                .unwrap();
            let open_region = open.regions.iter().find(|region| region.id == id).unwrap();
            let header_lines = closed_region.end_line - closed_region.start_line;
            assert_eq!(closed_region.header_lines, Some(header_lines));
            assert_eq!(open_region.header_lines, Some(header_lines));
            assert!(open_region.end_line > open_region.start_line + header_lines);
        }
        assert!(!snapshot_lines(&open.lines).contains("EVENT [I]"));
    }
    let rendered = snapshot_lines(
        &transcript_layout_with_level(
            state,
            Some(&expanded),
            100,
            &Theme::default(),
            &crate::markdown::SyntectHighlighter::default(),
            crate::state::EventLevel::Warning,
        )
        .lines,
    );
    assert!(
        rendered.contains("You are the primary test agent."),
        "{rendered}"
    );
    assert!(
        rendered.contains("plugin first\n· plugin second"),
        "{rendered}"
    );
    assert!(
        rendered.contains("summary first\n· summary second"),
        "{rendered}"
    );
    assert!(rendered.contains("retained range: 2..4"), "{rendered}");
    assert!(rendered.contains("byte size: 4096"), "{rendered}");
    assert!(rendered.contains("sha256:"), "{rendered}");
    assert!(rendered.contains("uri: artifact://chart"), "{rendered}");
}

#[test]
fn expanded_new_blocks_cap_rendering_and_keep_full_state() {
    let system_prompt = (0..300)
        .map(|index| format!("system-line-{index:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    let oversized = (0..100)
        .map(|index| format!("line-{index:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    let control_input = "\u{1b}".repeat(MAX_SYSTEM_PROMPT_BODY_BYTES);
    let created = session_created(SessionId::new_v7(), 1);
    let EventPayload::SessionCreated {
        mut creation_agent, ..
    } = created.payload
    else {
        unreachable!()
    };
    creation_agent.composed_prompt = system_prompt.clone();
    creation_agent.prompt_fingerprint = Sha256Digest::of_bytes(system_prompt.as_bytes());
    let media_url = "x".repeat(MAX_EXPANDED_BODY_BYTES + 128);
    let state = SessionState {
        run_snapshot: Some(creation_agent),
        transcript: vec![
            TranscriptItem::Compaction {
                id: 1,
                version: 0,
                seq: 10,
                commit: checkpoint_commit(&oversized),
            },
            TranscriptItem::PluginMessage {
                id: 2,
                version: 0,
                seq: 11,
                role: cookie_agent_protocol::ExtensionMessageRole::System,
                input: control_input.clone(),
            },
            TranscriptItem::Assistant {
                id: 3,
                version: 0,
                attribution: attribution(None),
                committed_turn_seq: Some(12),
                children: vec![AssistantChild::MediaFile {
                    turn_seq: 12,
                    content_index: 0,
                    file: cookie_agent_protocol::PersistedFilePart {
                        media_type: cookie_agent_protocol::MimeType::new("image/png")
                            .expect("mime"),
                        filename: Some("large.png".into()),
                        source: cookie_agent_protocol::PersistedFileSource::Url {
                            url: media_url.clone(),
                        },
                        metadata: None,
                    },
                }],
            },
        ],
        ..SessionState::default()
    };
    let expanded = HashSet::from([
        BlockId::SystemPrompt,
        BlockId::Compaction(10),
        BlockId::PluginMessage(11),
        BlockId::MediaFile {
            turn_seq: 12,
            content_index: 0,
        },
    ]);
    let rendered = snapshot_lines(&transcript_layout(&state, Some(&expanded), 100).lines);

    assert_eq!(rendered.matches("… truncated (").count(), 4, "{rendered}");
    assert!(
        rendered.contains("… truncated (36 more lines)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("… truncated (44 more lines)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("… truncated (1 more lines)"),
        "{rendered}"
    );
    assert!(rendered.contains("system-line-255"), "{rendered}");
    assert!(!rendered.contains("system-line-256"), "{rendered}");
    assert!(rendered.contains("\n· line-063"), "{rendered}");
    assert!(!rendered.contains("\n· line-064"), "{rendered}");
    assert!(!rendered.contains('\u{1b}'), "{rendered}");
    let bounded_control = bounded_safe_display_text(
        &control_input,
        Style::default(),
        MAX_EXPANDED_BODY_LINES,
        MAX_EXPANDED_BODY_BYTES,
    );
    assert!(
        bounded_control
            .last()
            .is_some_and(|line| line.to_string() == "… truncated (1 more lines)")
    );
    assert!(
        bounded_control[..bounded_control.len() - 1]
            .iter()
            .map(|line| line.to_string().len())
            .sum::<usize>()
            <= MAX_EXPANDED_BODY_BYTES
    );
    let bounded_system_control = bounded_safe_display_text(
        &control_input,
        Style::default(),
        MAX_SYSTEM_PROMPT_BODY_LINES,
        MAX_SYSTEM_PROMPT_BODY_BYTES,
    );
    let system_control_bytes = bounded_system_control[..bounded_system_control.len() - 1]
        .iter()
        .map(|line| line.to_string().len())
        .sum::<usize>();
    assert!(system_control_bytes > MAX_EXPANDED_BODY_BYTES);
    assert!(system_control_bytes <= MAX_SYSTEM_PROMPT_BODY_BYTES);
    assert_eq!(
        state
            .run_snapshot
            .as_ref()
            .expect("run snapshot")
            .composed_prompt,
        system_prompt
    );
    assert!(matches!(
        &state.transcript[1],
        TranscriptItem::PluginMessage { input, .. } if input == &control_input
    ));
    assert!(matches!(
        &state.transcript[2],
        TranscriptItem::Assistant { children, .. }
            if matches!(&children[0], AssistantChild::MediaFile { file, .. }
                if matches!(&file.source, cookie_agent_protocol::PersistedFileSource::Url { url }
                    if url == &media_url))
    ));
}

#[test]
fn system_prompt_is_hidden_until_first_run_then_uses_latest_snapshot() {
    let session = SessionId::new_v7();
    let mut store = StateStore::default();
    assert!(store.apply_event(session_created(session, 1)));
    let expanded = HashSet::from([BlockId::SystemPrompt]);
    let before_run =
        snapshot_lines(&transcript_layout(&store.sessions[&session], Some(&expanded), 80).lines);
    assert!(before_run.is_empty(), "{before_run}");

    for (seq, prompt) in [(2, "first run prompt"), (3, "latest run prompt")] {
        let mut started =
            run_started_with_suffix(session, seq, RunId::new_v7(), vec![resolved_model(None)]);
        let EventPayload::RunStarted { agent, .. } = &mut started.payload else {
            unreachable!()
        };
        agent.composed_prompt = prompt.into();
        agent.prompt_fingerprint = Sha256Digest::of_bytes(prompt.as_bytes());
        assert!(store.apply_event(started));
    }
    let latest =
        snapshot_lines(&transcript_layout(&store.sessions[&session], Some(&expanded), 80).lines);
    assert!(
        latest.contains("⚙ ▾ system prompt · primary (last run) (1 lines)"),
        "{latest}"
    );
    assert!(latest.contains("latest run prompt"), "{latest}");
    assert!(!latest.contains("first run prompt"), "{latest}");

    assert!(
        transcript_layout(&SessionState::default(), None, 80)
            .lines
            .is_empty()
    );
}

#[tokio::test]
async fn system_prompt_provenance_tracks_draft_agent_across_cache_reuse() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true), descriptor("reviewer", true)];
    let session = SessionId::new_v7();
    assert!(app.store.apply_event(session_created(session, 1)));
    assert!(app.store.apply_event(run_started_with_suffix(
        session,
        2,
        RunId::new_v7(),
        vec![resolved_model(None)],
    )));
    app.selected = Some(session);

    let initial = rendered_frame(&mut app, 100, 24);
    assert!(
        initial.contains("⚙ ▸ system prompt · primary (last run) (2 lines)"),
        "{initial}"
    );
    assert!(!initial.contains("next:"), "{initial}");
    let cache_key = app.layout_cache.key;
    let cached = rendered_frame(&mut app, 100, 24);
    assert_eq!(app.layout_cache.key, cache_key);
    assert_eq!(cached, initial);

    app.cycle_agent(false);
    let next = rendered_frame(&mut app, 100, 24);
    assert_eq!(app.layout_cache.key, cache_key);
    assert!(
        next.contains("⚙ ▸ system prompt · primary (last run) · next: reviewer (2 lines)"),
        "{next}"
    );

    app.cycle_agent(false);
    let restored = rendered_frame(&mut app, 100, 24);
    assert_eq!(app.layout_cache.key, cache_key);
    assert!(
        restored.contains("⚙ ▸ system prompt · primary (last run) (2 lines)"),
        "{restored}"
    );
    assert!(!restored.contains("next:"), "{restored}");
}

#[tokio::test]
async fn new_session_draft_does_not_leak_into_selected_prompt_provenance() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true), descriptor("reviewer", true)];
    let session = SessionId::new_v7();
    assert!(app.store.apply_event(session_created(session, 1)));
    assert!(app.store.apply_event(run_started_with_suffix(
        session,
        2,
        RunId::new_v7(),
        vec![resolved_model(None)],
    )));
    app.selected = Some(session);

    let expected = "⚙ ▸ system prompt · primary (last run) (2 lines)";
    let before = rendered_frame(&mut app, 100, 30);
    assert!(before.contains(expected), "{before}");

    app.run_command(SlashCommand::New).await;
    app.cycle_agent(false);
    assert_eq!(
        app.new_session_draft
            .as_ref()
            .map(|draft| draft.agent.as_str()),
        Some("reviewer")
    );
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("primary")
    );
    let while_new = rendered_frame(&mut app, 100, 30);
    assert!(while_new.contains(expected), "{while_new}");
    assert!(
        !while_new.contains("system prompt · primary (last run) · next: reviewer"),
        "{while_new}"
    );

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert!(app.new_session_draft.is_none());
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("primary")
    );
    let cancelled = rendered_frame(&mut app, 100, 30);
    assert!(cancelled.contains(expected), "{cancelled}");
    assert!(!cancelled.contains("next:"), "{cancelled}");
}

#[tokio::test]
async fn session_switch_during_new_rebinds_provenance_without_draft_transfer() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true), descriptor("reviewer", true)];
    let current = SessionId::new_v7();
    let forked = SessionId::new_v7();
    assert!(app.store.apply_event(session_created(current, 1)));
    assert!(app.store.apply_event(run_started_with_suffix(
        current,
        2,
        RunId::new_v7(),
        vec![resolved_model(None)],
    )));
    assert!(app.store.apply_event(session_created_with(
        forked,
        1,
        "reviewer",
        vec![resolved_model(None)],
        0,
    )));
    let mut forked_run =
        run_started_with_suffix(forked, 2, RunId::new_v7(), vec![resolved_model(None)]);
    let EventPayload::RunStarted {
        selection, agent, ..
    } = &mut forked_run.payload
    else {
        unreachable!()
    };
    let reviewer = AgentId::new("reviewer").expect("reviewer");
    selection.agent = reviewer.clone();
    agent.agent = reviewer.clone();
    assert!(app.store.apply_event(forked_run));
    let mut forked_meta = session_meta(forked);
    forked_meta.creation_selection.agent = reviewer;
    app.sessions = vec![session_meta(current), forked_meta];
    app.selected = Some(current);
    app.read_only_sessions.extend([current, forked]);

    app.run_command(SlashCommand::New).await;
    app.cycle_agent(false);
    assert_eq!(
        app.new_session_draft
            .as_ref()
            .map(|draft| draft.agent.as_str()),
        Some("reviewer")
    );

    app.handle_rpc_update(RpcUpdate::Forked { forked });
    assert_eq!(app.selected, Some(forked));
    assert!(app.owned_sessions.contains(&forked));
    assert!(!app.read_only_sessions.contains(&forked));
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("reviewer")
    );
    let switched = rendered_frame(&mut app, 100, 30);
    assert!(switched.contains("Type a message · / for commands"));
    assert!(!switched.contains("Read-only snapshot"));
    assert!(
        switched.contains("system prompt · reviewer (last run) (2 lines)"),
        "{switched}"
    );
    assert!(!switched.contains("next:"), "{switched}");

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert!(app.new_session_draft.is_none());
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("reviewer")
    );
    let cancelled = rendered_frame(&mut app, 100, 30);
    assert!(!cancelled.contains("next:"), "{cancelled}");
}

#[tokio::test]
async fn failed_then_repeated_new_never_replaces_selected_session_draft() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true), descriptor("reviewer", true)];
    let session = SessionId::new_v7();
    assert!(app.store.apply_event(session_created(session, 1)));
    assert!(app.store.apply_event(run_started_with_suffix(
        session,
        2,
        RunId::new_v7(),
        vec![resolved_model(None)],
    )));
    app.selected = Some(session);
    app.set_draft_agent(AgentId::new("reviewer").expect("reviewer"));
    let expected = "⚙ ▸ system prompt · primary (last run) · next: reviewer (2 lines)";
    let before = rendered_frame(&mut app, 100, 30);
    assert!(before.contains(expected), "{before}");

    let (client, recorded, incoming) = live_recording_client();
    app.client = client;
    app.run_command(SlashCommand::New).await;
    app.choose_picker_entry(0).await;
    let recorded_for_response = recorded.clone();
    let response = tokio::spawn(async move {
        let id = wait_for_recorded_request(&recorded_for_response, "session.create", 1).await;
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": "create failed"}
            })))
            .expect("script create failure");
    });
    type_input(&mut app, "first message").await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    response.await.expect("create failure response");

    assert_eq!(app.modal, Modal::None);
    assert_eq!(recorded_method_count(&recorded, "session.create"), 1);
    assert_eq!(app.input.as_str(), "first message");
    assert_eq!(
        app.new_session_draft.as_ref().map(|d| d.agent.as_str()),
        Some("primary")
    );
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("reviewer")
    );
    let failed = rendered_frame(&mut app, 100, 30);
    assert!(failed.contains(expected), "{failed}");

    app.run_command(SlashCommand::New).await;
    assert_eq!(
        app.new_session_draft
            .as_ref()
            .map(|draft| draft.agent.as_str()),
        Some("primary")
    );
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("reviewer")
    );
    let repeated = rendered_frame(&mut app, 100, 30);
    assert!(repeated.contains(expected), "{repeated}");
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert!(app.new_session_draft.is_none());
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("reviewer")
    );
}

#[test]
fn native_compaction_expands_window_reference_details() {
    let fingerprint = Sha256Digest::of_bytes(b"native selection");
    let window = cookie_agent_protocol::NativeContextWindow::new(
        SafeCode::new("openai-responses").expect("adapter"),
        fingerprint.clone(),
        cookie_agent_protocol::NativeContextScope {
            provider_id: ProviderId::new("gateway").expect("provider"),
            model_id: cookie_agent_protocol::ProviderModelId::new("arbitrary-model")
                .expect("model"),
            resource_id: SafeDisplayText::new("response-123").expect("resource"),
        },
        serde_json::json!({"opaque": true}),
    )
    .expect("native window");
    let max = cookie_agent_protocol::SummaryByteLimit::new(1024).expect("summary limit");
    let state = SessionState {
        transcript: vec![TranscriptItem::Compaction {
            id: 1,
            version: 0,
            seq: 9,
            commit: cookie_agent_protocol::ContextCheckpointCommit {
                checkpoint: cookie_agent_protocol::ContextCheckpoint::NativeWindow { window },
                boundaries: cookie_agent_protocol::ContextCheckpointBoundaries {
                    source_from_seq: 2,
                    source_through_seq: 3,
                    recent_from_seq: None,
                    input_through_seq: 4,
                    prior_checkpoint_seq: None,
                },
                budgets: cookie_agent_protocol::ContextCheckpointBudgets {
                    context_limit_tokens: 10_000,
                    trigger_tokens: 8_000,
                    input_tokens_before: 9_000,
                    input_tokens_after: 1_200,
                    keep_recent_tokens: 0,
                    max_summary_bytes: max,
                },
            },
        }],
        ..SessionState::default()
    };
    let collapsed = snapshot_lines(&transcript_layout(&state, None, 100).lines);
    assert!(collapsed.contains("context compacted (native window, 9000→1200 tokens)"));
    assert!(!collapsed.contains("response-123"));

    let expanded = HashSet::from([BlockId::Compaction(9)]);
    let rendered = snapshot_lines(&transcript_layout(&state, Some(&expanded), 100).lines);
    assert!(rendered.contains("adapter: openai-responses"), "{rendered}");
    assert!(
        rendered.contains("model: gateway/arbitrary-model"),
        "{rendered}"
    );
    assert!(rendered.contains("resource: response-123"), "{rendered}");
    assert!(rendered.contains(&format!("selection fingerprint: {fingerprint}")));
    assert!(rendered.contains("retained range: 2..4"), "{rendered}");
}

#[tokio::test]
async fn new_transcript_block_click_is_mouse_only_and_state_is_per_session() {
    let mut app = test_app().await;
    let first = SessionId::new_v7();
    let second = SessionId::new_v7();
    assert!(app.store.apply_event(session_created(first, 1)));
    assert!(app.store.apply_event(session_created(second, 1)));
    assert!(app.store.apply_event(run_started_with_suffix(
        first,
        2,
        RunId::new_v7(),
        vec![resolved_model(None)],
    )));
    assert!(app.store.apply_event(run_started_with_suffix(
        second,
        2,
        RunId::new_v7(),
        vec![resolved_model(None)],
    )));
    app.selected = Some(first);
    app.tree_root = Some(first);
    rendered_frame(&mut app, 80, 24);
    let hit = app
        .hit_map
        .blocks
        .iter()
        .find(|hit| hit.id == BlockId::SystemPrompt)
        .copied()
        .expect("system prompt hit");
    app.handle_click(hit.rect.x, hit.rect.y).await;
    assert!(app.expanded_blocks[&first].contains(&BlockId::SystemPrompt));

    app.selected = Some(second);
    rendered_frame(&mut app, 80, 24);
    assert!(!app.expanded_blocks.contains_key(&second));
    assert!(rendered_frame(&mut app, 80, 24).contains('▸'));

    app.selected = Some(first);
    assert!(rendered_frame(&mut app, 80, 24).contains('▾'));
}

#[test]
fn replay_rebuild_restores_new_event_backed_blocks() {
    let session = SessionId::new_v7();
    let events = [
        session_created(session, 1),
        runless_event(
            session,
            2,
            EventPayload::MessageInjected {
                role: cookie_agent_protocol::ExtensionMessageRole::Assistant,
                input: "replayed plugin text".into(),
            },
        ),
        runless_event(
            session,
            3,
            EventPayload::ContextCheckpointCommitted {
                commit: checkpoint_commit("replayed summary text"),
            },
        ),
    ];
    let mut store = StateStore::default();
    let _ = store.apply_delivery(ClientDelivery::ReplayStart {
        session_id: session,
        generation: 0,
        final_seq: 3,
        rebuild: true,
    });
    for event in events {
        let _ = store.apply_delivery(ClientDelivery::ReplayEvent {
            session_id: session,
            generation: 0,
            final_seq: 3,
            event: Box::new(event),
        });
    }
    let _ = store.apply_delivery(ClientDelivery::ReplayEnd {
        session_id: session,
        generation: 0,
        final_seq: 3,
    });
    let state = &store.sessions[&session];
    assert!(matches!(
        state.transcript[0],
        TranscriptItem::PluginMessage { seq: 2, .. }
    ));
    assert!(matches!(
        state.transcript[1],
        TranscriptItem::Compaction { seq: 3, .. }
    ));
    let expanded = HashSet::from([BlockId::PluginMessage(2), BlockId::Compaction(3)]);
    let rendered = snapshot_lines(&transcript_layout(state, Some(&expanded), 80).lines);
    assert!(rendered.contains("replayed plugin text"), "{rendered}");
    assert!(rendered.contains("replayed summary text"), "{rendered}");
}
