use crate::ui::transcript::*;

use cookie_agent_protocol::{
    EventPayload, ModelSelection, RunId, RunSelection, SessionId, SessionStatus, SessionTree,
    StoredEvent,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use jiff::Timestamp;

use crate::theme::{ColorLevel, ThemeKind};

use crate::ui::app::*;

use crate::ui::slash::{SlashCommand, Submission, command_help, command_spec, parse_submission};

use super::support::*;

#[test]
fn slash_commands_parse_and_escape_prompts() {
    assert_eq!(
        parse_submission("/quit").expect("quit"),
        Submission::Command(SlashCommand::Quit)
    );
    assert_eq!(
        parse_submission("//literal /quit").expect("escaped"),
        Submission::Prompt("/literal /quit".into())
    );
    assert_eq!(
        parse_submission("line one\n/quit").expect("multiline"),
        Submission::Prompt("line one\n/quit".into())
    );
    assert!(parse_submission("/nope").is_err());
    assert_eq!(
        parse_submission("/show agent panel").expect("show agent panel"),
        Submission::Command(SlashCommand::ShowAgentPanel)
    );
    assert_eq!(
        parse_submission("/hide agent panel").expect("hide agent panel"),
        Submission::Command(SlashCommand::HideAgentPanel)
    );
    assert_eq!(
        command_spec("show").map(|spec| spec.usage),
        Some("/show agent panel")
    );
    assert_eq!(
        command_spec("hide").map(|spec| spec.usage),
        Some("/hide agent panel")
    );
}

#[tokio::test]
async fn manual_agent_panel_override_wins_and_commands_are_context_sensitive() {
    let mut app = test_app().await;
    let root = SessionId::new_v7();
    let first = SessionId::new_v7();
    let second = SessionId::new_v7();
    let mut first_meta = delegated_meta(first, root, "worker");
    first_meta.status = SessionStatus::Running;
    app.selected = Some(root);
    app.tree_root = Some(root);
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: first_meta,
            children: Vec::new(),
        }],
    });

    let auto = frame_rows(&mut app, 80, 24);
    assert!(auto.iter().any(|row| row.contains("Agents")));
    let visible_conversation_y = app.hit_map.conversation.expect("conversation").y;
    app.input.set_buffer("/".into());
    let labels = app.skill_palette_labels_for_test();
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("/hide agent panel"))
    );
    assert!(
        !labels
            .iter()
            .any(|label| label.starts_with("/show agent panel"))
    );

    app.run_command(SlashCommand::HideAgentPanel).await;
    let hidden = frame_rows(&mut app, 80, 24);
    assert!(!hidden.iter().any(|row| row.contains("Agents")));
    assert_eq!(app.hit_map.conversation.expect("conversation").y, 1);
    assert!(visible_conversation_y > 1);
    app.input.set_buffer("/".into());
    let labels = app.skill_palette_labels_for_test();
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("/show agent panel"))
    );
    assert!(
        !labels
            .iter()
            .any(|label| label.starts_with("/hide agent panel"))
    );

    let mut second_meta = delegated_meta(second, root, "reviewer");
    second_meta.status = SessionStatus::Running;
    app.tree.as_mut().expect("tree").children.push(SessionTree {
        session: second_meta,
        children: Vec::new(),
    });
    let hidden_with_new_delegation = frame_rows(&mut app, 80, 24);
    assert!(
        !hidden_with_new_delegation
            .iter()
            .any(|row| row.contains("Agents"))
    );

    app.run_command(SlashCommand::ShowAgentPanel).await;
    app.apply_status_patch(first, SessionStatus::Completed, 3);
    app.apply_status_patch(second, SessionStatus::Failed, 3);
    let shown_after_completion = frame_rows(&mut app, 80, 24);
    assert!(
        shown_after_completion
            .iter()
            .any(|row| row.contains("Agents"))
    );
    assert!(app.hit_map.conversation.expect("conversation").y > 1);

    app.input.set_buffer("/".into());
    let labels = app.skill_palette_labels_for_test();
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("/hide agent panel"))
    );
    assert!(
        !labels
            .iter()
            .any(|label| label.starts_with("/show agent panel"))
    );
    app.show_help();
    let help = app.transient_notices.last().expect("command help");
    assert!(help.contains("/hide agent panel"));
    assert!(!help.contains("/show agent panel"));
    assert!(help.contains("until toggled or a different root session is selected"));
}

#[tokio::test]
async fn agent_panel_commands_latch_without_an_immediate_visibility_change() {
    let live_tree = |root: SessionId, child: SessionId| {
        let mut child_meta = delegated_meta(child, root, "worker");
        child_meta.status = SessionStatus::Running;
        SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: child_meta,
                children: Vec::new(),
            }],
        }
    };

    let mut shown = test_app().await;
    let shown_root = SessionId::new_v7();
    let shown_child = SessionId::new_v7();
    shown.selected = Some(shown_root);
    shown.tree_root = Some(shown_root);
    shown.tree = Some(live_tree(shown_root, shown_child));
    assert!(
        frame_rows(&mut shown, 80, 24)
            .iter()
            .any(|row| row.contains("Agents"))
    );
    shown.run_command(SlashCommand::ShowAgentPanel).await;
    shown.apply_status_patch(shown_child, SessionStatus::Completed, 3);
    assert!(
        frame_rows(&mut shown, 80, 24)
            .iter()
            .any(|row| row.contains("Agents"))
    );

    let mut hidden = test_app().await;
    let hidden_root = SessionId::new_v7();
    let hidden_child = SessionId::new_v7();
    hidden.selected = Some(hidden_root);
    hidden.tree_root = Some(hidden_root);
    hidden.tree = Some(SessionTree {
        session: session_meta(hidden_root),
        children: Vec::new(),
    });
    assert!(
        !frame_rows(&mut hidden, 80, 24)
            .iter()
            .any(|row| row.contains("Agents"))
    );
    hidden.run_command(SlashCommand::HideAgentPanel).await;
    hidden.tree = Some(live_tree(hidden_root, hidden_child));
    assert!(
        !frame_rows(&mut hidden, 80, 24)
            .iter()
            .any(|row| row.contains("Agents"))
    );

    hidden.reroot_tree(hidden_root);
    hidden.tree = Some(live_tree(hidden_root, hidden_child));
    assert!(
        !frame_rows(&mut hidden, 80, 24)
            .iter()
            .any(|row| row.contains("Agents"))
    );

    let next_root = SessionId::new_v7();
    let next_child = SessionId::new_v7();
    hidden.reroot_tree(next_root);
    hidden.tree = Some(live_tree(next_root, next_child));
    assert!(
        frame_rows(&mut hidden, 80, 24)
            .iter()
            .any(|row| row.contains("Agents"))
    );
}

#[tokio::test]
async fn selected_session_visibility_changes_refresh_skills() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    assert!(app.store.apply_event(session_created(session, 1)));
    app.selected = Some(session);

    let permission_event = StoredEvent {
        engine_version: None,
        origin: None,
        session_id: session,
        run_id: None,
        seq: 2,
        timestamp: Timestamp::now(),
        payload: EventPayload::SessionPermissionOverlaySet {
            overlay: cookie_agent_protocol::SessionPermissionOverlay::empty(),
        },
    };
    app.refresh_skills_for_event_for_test(&permission_event);
    assert_eq!(app.skill_refresh_count_for_test(), 1);

    let run_event =
        run_started_with_suffix(session, 3, RunId::new_v7(), vec![resolved_model(None)]);
    app.refresh_skills_for_event_for_test(&run_event);
    assert_eq!(app.skill_refresh_count_for_test(), 2);
}

#[test]
fn newline_keys_insert_and_bare_enter_submits() {
    let newline = |key: KeyEvent| {
        matches!(
            (key.code, key.modifiers),
            (
                KeyCode::Enter,
                KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT
            ) | (KeyCode::Char('j'), KeyModifiers::CONTROL)
        )
    };
    assert!(newline(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)));
    assert!(newline(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
    assert!(newline(KeyEvent::new(
        KeyCode::Char('j'),
        KeyModifiers::CONTROL
    )));
    assert!(!newline(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
}

#[tokio::test]
async fn ctrl_p_opens_palette_plain_p_types_and_removed_commands_are_rejected() {
    let mut app = test_app().await;
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await;
    assert_eq!(app.input.as_str(), "/");
    assert!(app.command_palette_visible());

    app.input.set_buffer(String::new());
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
        .await;
    assert_eq!(app.input.as_str(), "p");
    assert!(command_spec("block").is_none());
    assert!(command_spec("scroll").is_none());
    assert!(command_spec("stdin").is_none());
    assert!(command_spec("eof").is_none());
    assert!(command_spec("tree").is_none());
    assert!(command_spec("watch").is_none());
    assert!(parse_submission("/block next").is_err());
    assert!(parse_submission("/scroll top").is_err());
    assert!(parse_submission("/stdin").is_err());
    assert!(parse_submission("/stdin next").is_err());
    assert!(parse_submission("/eof").is_err());
    assert!(parse_submission("/tree up").is_err());
    assert!(parse_submission("/tree down").is_err());
    assert!(parse_submission("/tree toggle").is_err());
    assert!(parse_submission("/watch").is_err());
}

#[tokio::test]
async fn page_keys_scroll_conversation_by_viewport_height() {
    let mut app = test_app().await;
    app.hit_map.conversation = Some(Rect::new(0, 0, 80, 10));
    app.conversation_scroll.offset = 30;
    app.conversation_scroll.following = false;

    app.handle_input_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
        .await;
    assert_eq!(app.conversation_scroll.offset, 20);
    app.handle_input_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
        .await;
    assert_eq!(app.conversation_scroll.offset, 30);
}

#[tokio::test]
async fn chrome_stays_coherent_across_themes_and_tiny_terminals() {
    for theme in [
        Theme::default(),
        Theme::new(ThemeKind::Mono, ColorLevel::None),
        Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16),
    ] {
        let mut app = test_app().await;
        let kind = theme.key();
        app.theme = theme;
        let session = SessionId::new_v7();
        assert!(app.store.apply_event(session_created(session, 1)));
        app.selected = Some(session);
        for (width, height) in [(100, 30), (40, 12), (20, 8)] {
            let rendered = rendered_frame(&mut app, width, height);
            // Every theme kind renders the same textual chrome: the
            // fresh-session guidance, the composer placeholder, and the
            // command hint. State never depends on color alone.
            if width >= 100 {
                assert!(
                    rendered.contains("Fresh session") && !rendered.contains("⚙ ▸ system prompt"),
                    "{kind:?}: {rendered}"
                );
                assert!(rendered.contains("ctrl+p"), "{kind:?}: {rendered}");
            }
            if width >= 40 {
                assert!(rendered.contains("Type a message"), "{kind:?}: {rendered}");
            }
            assert!(rendered.contains("Conversation"), "{kind:?}: {rendered}");
        }
        // A detached viewport announces itself in every theme.
        app.store
            .sessions
            .get_mut(&session)
            .expect("session")
            .transcript = (0..30)
            .map(|index| TranscriptItem::user(format!("message {index}")))
            .collect();
        let _ = rendered_frame(&mut app, 100, 30);
        app.conversation_scroll.top();
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("↑ scrolled · PgDn: bottom"), "{kind:?}");
    }
}

#[tokio::test]
async fn help_lists_each_command_on_its_own_transcript_line() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    assert!(app.store.apply_event(session_created(session, 1)));
    app.selected = Some(session);
    submit_direct_command(&mut app, "/help").await;
    let rendered = rendered_frame(&mut app, 110, 40);
    assert!(
        rendered.contains("NOTICE: Available commands:"),
        "{rendered}"
    );
    for expected in [
        "/quit — exit the TUI",
        "/new — start a new root session",
        "/preset — select the preset for the next root run and future new sessions",
        "/approve once|all|reject|cancel — answer an approval",
        "/events debug|info|warning|error — set the diagnostic level filter for this view",
        "/help — show command help",
        "Use // to send a prompt beginning with /.",
    ] {
        assert!(rendered.contains(expected), "{expected}: {rendered}");
    }
    // The wall of semicolon-joined text is gone.
    assert!(!rendered.contains("; /new"), "{rendered}");
}

#[tokio::test]
async fn scroll_follow_state_is_loud_in_the_conversation_title() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    assert!(app.store.apply_event(session_created(session, 1)));
    // Enough top-level content to overflow the conversation viewport.
    app.store
        .sessions
        .get_mut(&session)
        .expect("session")
        .transcript = (0..30)
        .map(|index| TranscriptItem::user(format!("message {index}")))
        .collect();
    app.selected = Some(session);

    // Following: the title row carries no scroll warning.
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(!rendered.contains("PgDn: bottom"), "{rendered}");

    // Detached: the title row says so, with the truthful way back.
    app.conversation_scroll.top();
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("↑ scrolled · PgDn: bottom"), "{rendered}");

    // Paging down re-engages following at the exact bottom and clears
    // the warning.
    for _ in 0..10 {
        if app.conversation_scroll.following {
            break;
        }
        app.handle_input_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .await;
        let _ = rendered_frame(&mut app, 100, 30);
    }
    assert!(app.conversation_scroll.following);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(!rendered.contains("PgDn: bottom"), "{rendered}");
}

#[test]
fn command_registry_drives_help_and_parser() {
    let help = command_help();
    assert!(help.contains("/new"));
    assert!(help.contains("/connect"));
    assert!(help.contains("/events"));
    assert!(!help.contains("/block"));
    assert!(!help.contains("/scroll"));
    // The stdin era is over: /message left with it.
    assert!(!help.contains("/message"));
}

#[tokio::test]
async fn command_palette_no_matches_renders_empty_state_then_reports_unknown_command() {
    let mut app = test_app().await;
    type_input(&mut app, "/definitely-not-a-command").await;
    assert!(app.command_palette_visible());

    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("Commands"));
    assert!(rendered.contains("No matching commands"));
    assert!(app.hit_map.palette.is_some());
    assert!(app.hit_map.palette_rows.is_empty());

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert!(app.input.as_str().is_empty());
    assert!(app.status.contains("unknown command"));
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("unknown command"));
}

#[tokio::test]
async fn empty_agent_and_model_selectors_are_truthful_and_safe() {
    let mut agents = test_app().await;
    agents.agents.clear();
    submit_direct_command(&mut agents, "/new").await;
    assert_eq!(agents.modal, Modal::Agents);
    let rendered = rendered_frame(&mut agents, 100, 30);
    assert!(rendered.contains("No root-runnable agents are available."));
    assert!(!rendered.contains("Backspace or Ctrl-U clears the filter"));
    assert!(agents.hit_map.picker_rows.is_empty());
    agents
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(agents.modal, Modal::Agents);
    agents
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert_eq!(agents.modal, Modal::None);

    let mut models = test_app().await;
    models.agents = vec![descriptor("primary", true)];
    models.models.clear();
    models.draft = Some(RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: model_key(),
            variant: None,
        },
        preset: None,
    });
    rendered_frame(&mut models, 100, 30);
    let model_hit = models
        .hit_map
        .title_segments
        .iter()
        .find(|hit| hit.segment == TitleSegment::Model)
        .copied()
        .expect("model title hit");
    models
        .handle_click(model_hit.rect.x, model_hit.rect.y)
        .await;
    assert_eq!(models.modal, Modal::Models);
    let rendered = rendered_frame(&mut models, 100, 30);
    assert!(rendered.contains("No models are available for this draft."));
    assert!(!rendered.contains("Backspace or Ctrl-U clears the filter"));
    assert!(models.hit_map.picker_rows.is_empty());
    models
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(models.modal, Modal::Models);
    models
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert_eq!(models.modal, Modal::None);
}

#[tokio::test]
async fn preset_picker_updates_root_drafts_and_future_new_sessions() {
    let mut app = test_app().await;
    let session_id = SessionId::new_v7();
    app.sessions.push(session_meta(session_id));
    app.selected = Some(session_id);
    app.agents.extend([
        descriptor("reviewer", true),
        preset_descriptor("python", "primary"),
        preset_descriptor("python", "reviewer"),
        preset_descriptor("rust", "primary"),
    ]);

    app.run_command(SlashCommand::Preset).await;
    assert_eq!(app.modal, Modal::Presets);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("None (shared)"));
    assert!(rendered.contains("python"));
    assert!(rendered.contains("rust"));
    app.choose_picker_entry(1).await;
    assert_eq!(app.selected_preset.as_deref(), Some("python"));
    assert_eq!(
        app.draft.as_ref().and_then(|draft| draft.preset.as_deref()),
        Some("python")
    );
    assert!(app.status.contains("Draft run preset"));
    app.cycle_agent(false);
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("reviewer")
    );

    app.run_command(SlashCommand::Preset).await;
    app.choose_picker_entry(2).await;
    assert_eq!(app.selected_preset.as_deref(), Some("rust"));
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("primary")
    );
    assert_eq!(
        app.draft.as_ref().and_then(|draft| draft.preset.as_deref()),
        Some("rust")
    );

    app.run_command(SlashCommand::Preset).await;
    app.choose_picker_entry(1).await;

    app.run_command(SlashCommand::New).await;
    assert!(app.new_session_draft.is_some());
    assert_eq!(app.modal, Modal::Agents);
    assert_eq!(
        app.new_session_draft
            .as_ref()
            .and_then(|draft| draft.preset.as_deref()),
        Some("python")
    );
    app.run_command(SlashCommand::Preset).await;
    app.choose_picker_entry(2).await;
    assert_eq!(app.selected_preset.as_deref(), Some("rust"));
    assert_eq!(
        app.new_session_draft
            .as_ref()
            .and_then(|draft| draft.preset.as_deref()),
        Some("rust")
    );
    assert!(app.status.contains("preset rust"));
    assert!(!app.status.contains("preset python"));
    assert_eq!(
        app.draft.as_ref().and_then(|draft| draft.preset.as_deref()),
        Some("python")
    );
    assert_eq!(
        app.selectable_agents()
            .iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>(),
        ["primary", "reviewer"]
    );
    app.run_command(SlashCommand::Preset).await;
    app.choose_picker_entry(1).await;
    assert_eq!(app.selected_preset.as_deref(), Some("python"));
    app.cycle_agent(false);
    assert_eq!(
        app.new_session_draft
            .as_ref()
            .map(|draft| draft.agent.as_str()),
        Some("reviewer")
    );
    assert_eq!(
        app.new_session_draft
            .as_ref()
            .and_then(|draft| draft.preset.as_deref()),
        Some("python")
    );

    app.run_command(SlashCommand::New).await;
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert!(app.new_session_draft.is_none());
    let fresh = test_app().await;
    assert_eq!(fresh.selected_preset, None);
}

#[tokio::test]
async fn read_only_session_allows_starting_new_session_with_new_command() {
    let mut app = test_app().await;
    let session_id = SessionId::new_v7();
    app.sessions.push(session_meta(session_id));
    app.selected = Some(session_id);
    app.read_only_sessions.insert(session_id);

    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .await;
    assert!(app.input.as_str().is_empty());
    assert!(app.status.contains("input is disabled"));

    type_input(&mut app, "/new").await;
    if app.command_palette_visible() {
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;

    assert!(
        app.input.as_str().is_empty(),
        "input={:?}, modal={:?}, status={:?}",
        app.input.as_str(),
        app.modal,
        app.status
    );
    assert!(app.new_session_draft.is_some());
    assert_eq!(app.modal, Modal::Agents);
}

#[tokio::test]
async fn every_slash_command_variant_dispatches_from_key_events_without_starting_a_run() {
    let cases = [
        ("/quit", None),
        ("/new", Some("Agent")),
        ("/preset", Some("Agent preset")),
        ("/connect", Some("Connect provider")),
        ("/sessions", Some("Sessions")),
        ("/cancel", Some("no active run")),
        ("/approve once", None),
        ("/approve all", None),
        ("/approve reject", None),
        ("/approve cancel", None),
        ("/events debug", Some("diagnostic event filter")),
        ("/events info", Some("diagnostic event filter")),
        ("/events warning", Some("diagnostic event filter")),
        ("/events error", Some("diagnostic event filter")),
        ("/help", Some("Available commands:")),
    ];

    for (command, expected) in cases {
        let mut app = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        app.client = client;
        submit_direct_command(&mut app, command).await;
        settle_recording().await;
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(!rendered.is_empty(), "{command}");
        if let Some(expected) = expected {
            assert!(rendered.contains(expected), "{command}: {rendered}");
        }
        assert_eq!(
            recorded_method_count(&recorded, "run.start"),
            0,
            "{command}"
        );
        assert_eq!(
            recorded_method_count(&recorded, "run.steer"),
            0,
            "{command}"
        );
        drop(incoming_guard);
    }
}

#[tokio::test]
async fn command_palette_mouse_activation_uses_the_same_local_dispatch() {
    let mut app = test_app().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    app.agents.clear();
    type_input(&mut app, "/ne").await;
    rendered_frame(&mut app, 100, 30);
    let row = app
        .hit_map
        .palette_rows
        .first()
        .copied()
        .expect("new palette row");
    app.handle_click(row.rect.x, row.rect.y).await;
    assert_eq!(app.modal, Modal::Agents);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("No root-runnable agents are available."));
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "run.start"), 0);
    assert_eq!(recorded_method_count(&recorded, "run.steer"), 0);
    drop(incoming_guard);
}

#[tokio::test]
async fn rpc_slash_commands_issue_only_their_intended_methods() {
    let mut cancel = test_app().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    cancel.client = client;
    let session = SessionId::new_v7();
    cancel.selected = Some(session);
    cancel.store.sessions.entry(session).or_default().active_run = Some(run_id());
    submit_direct_command(&mut cancel, "/cancel").await;
    wait_for_method(&recorded, "run.cancel", 1).await;
    assert_eq!(recorded_method_count(&recorded, "run.cancel"), 1);
    assert_eq!(recorded_method_count(&recorded, "run.start"), 0);
    assert_eq!(recorded_method_count(&recorded, "run.steer"), 0);
    drop(incoming_guard);

    for command in [
        "/approve once",
        "/approve all",
        "/approve reject",
        "/approve cancel",
    ] {
        let mut app = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        app.client = client;
        let approval = bash_approval_state();
        app.selected = Some(approval.session_id);
        app.store
            .sessions
            .entry(approval.session_id)
            .or_default()
            .approvals
            .push(approval);
        submit_direct_command(&mut app, command).await;
        wait_for_method(&recorded, "approval.respond", 1).await;
        assert_eq!(
            recorded_method_count(&recorded, "approval.respond"),
            1,
            "{command}"
        );
        assert_eq!(
            recorded_method_count(&recorded, "run.start"),
            0,
            "{command}"
        );
        assert_eq!(
            recorded_method_count(&recorded, "run.steer"),
            0,
            "{command}"
        );
        drop(incoming_guard);
    }
}
