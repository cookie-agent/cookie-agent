use std::collections::BTreeMap;

#[cfg(unix)]
use std::fs;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use crate::ui::transcript::*;

use cookie_agent_protocol::{AttemptId, EventPayload, RunSelection, SessionId, StoredEvent, Usage};

use jiff::Timestamp;

use ratatui::{Terminal, backend::TestBackend};

use crate::markdown::MarkdownDocument;

use crate::state::{AssistantChild, SessionState, StateStore};

use crate::ui::app::*;

use super::support::*;

#[test]
fn assistant_header_projects_exact_agent_model_and_variant() {
    assert_eq!(
        attribution(None).header(),
        "primary • gateway/arbitrary-model[base]"
    );
    assert_eq!(
        attribution(Some("high")).header(),
        "primary • gateway/arbitrary-model[high]"
    );
    assert_eq!(
        attribution(Some("default")).header(),
        "primary • gateway/arbitrary-model[default]"
    );
    assert_eq!(attribution(Some("high")).variant_label(), "high");
    assert_eq!(attribution(None).variant_label(), "base");
}

#[test]
fn tiny_header_wraps_frozen_attribution_never_reduces_to_a_tag() {
    let state = assistant_state(vec![AssistantChild::Text {
        id: 1,
        version: 0,
        markdown: MarkdownDocument::new("answer".into()),
    }]);
    for width in [3u16, 6, 12, 24] {
        let layout = transcript_layout(&state, None, width);
        // Render into a real terminal buffer at the exact panel width:
        // every row, including continuation prefixes, must fit.
        let backend =
            TestBackend::new(width, u16::try_from(layout.lines.len().max(1)).unwrap_or(1));
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(ratatui::text::Text::from(
                        layout.lines.clone(),
                    )),
                    area,
                );
            })
            .expect("render");
        let buffer = terminal.backend().buffer();
        let mut visible = String::new();
        for y in 0..buffer.area.height {
            let row = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>();
            visible.push_str(row.trim_end());
        }
        let squashed = visible.replace(['│', ' '], "");
        assert!(
            squashed.contains("primary•gateway/arbitrary-model"),
            "width {width}: {visible}"
        );
        assert!(!visible.contains("[A]"), "width {width}");
    }
    let rendered = snapshot_lines(&transcript_layout(&state, None, 80).lines);
    assert!(rendered.contains("╭─ primary • gateway/arbitrary-model[base]"));
}

#[test]
fn assistant_item_renders_one_header_with_inline_children() {
    let state = assistant_state(vec![
        AssistantChild::Thinking {
            id: 1,
            version: 0,
            text: "thinking".into(),
        },
        AssistantChild::Text {
            id: 2,
            version: 0,
            markdown: MarkdownDocument::new("answer".into()),
        },
    ]);
    let layout = transcript_layout(&state, None, 60);
    let rendered = layout
        .lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        rendered
            .matches("╭─ primary • gateway/arbitrary-model[base]")
            .count(),
        1
    );
    assert!(!rendered.contains("ASSISTANT"));
    assert!(!rendered.contains("REASONING"));
    assert!(!rendered.contains("TOOL"));
    let (collapsed, expanded) = chevron_counts(&rendered);
    assert_eq!(expanded, 0);
    assert_eq!(collapsed, 1);
}

#[test]
fn two_turn_assistant_item_renders_one_header() {
    let state = assistant_state(vec![
        AssistantChild::Text {
            id: 10,
            version: 0,
            markdown: MarkdownDocument::new("first turn".into()),
        },
        AssistantChild::Text {
            id: 20,
            version: 0,
            markdown: MarkdownDocument::new("second turn".into()),
        },
    ]);
    let rendered = snapshot_lines(&transcript_layout(&state, None, 60).lines);
    assert_eq!(
        rendered
            .matches("╭─ primary • gateway/arbitrary-model[base]")
            .count(),
        1
    );
    assert!(rendered.contains("first turn"));
    assert!(rendered.contains("second turn"));
}

#[test]
fn attribution_marker_renders_without_region_and_is_skipped_by_navigation() {
    let item = TranscriptItem::Assistant {
        id: 1,
        version: 0,
        attribution: attribution(None),
        committed_turn_seq: Some(2),
        children: vec![
            AssistantChild::Thinking {
                id: 10,
                version: 0,
                text: "thought".into(),
            },
            AssistantChild::Attribution {
                resolved_model: resolved_model(Some("high")),
            },
        ],
    };
    let state = SessionState {
        transcript: vec![item.clone()],
        ..SessionState::default()
    };
    let layout = transcript_layout(&state, None, 60);
    assert!(snapshot_lines(&layout.lines).contains("├─ now using gateway/arbitrary-model[high]"));
    assert_eq!(layout.regions.len(), 1);
    assert_eq!(item_block_ids(&item), vec![BlockId::Thinking(10)]);
}

#[tokio::test]
async fn assistant_footer_shows_speed_and_context_from_durable_timestamps() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    // 84 output tokens over the 2s span between the input-closing event
    // and the commit: 42.0 tps; the ctx is the end-of-turn total,
    // 12,400 input + 84 generated = 12,484 → 12.5K.
    let mut app = app_with_footer_log(
        footer_event_log(session, run, attempt, Some((12_400, 84)), 2),
        session,
    )
    .await;
    let rendered = frame_rows(&mut app, 100, 30).join("\n");
    // ⚡ is a two-cell glyph; assert the gutter and the values around it.
    assert!(rendered.contains("╰─ ⚡"), "gutter: {rendered}");
    assert!(
        rendered.contains("42.0 tps · 12.5K ctx"),
        "footer: {rendered}"
    );
    // The footer is passive: it registers no hover target.
    let footer_row = rendered
        .lines()
        .position(|line| line.contains("tps"))
        .map(|row| row as u16)
        .expect("footer row");
    assert_eq!(app.hover_target_at(2, footer_row), None);
}

#[tokio::test]
async fn assistant_footer_shows_priced_cost_and_omits_unpriced_cost() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let mut events = footer_event_log(session, run, attempt, Some((12_400, 84)), 2);
    events.push(usage_recorded(session, 5, run, 1, Some(3_100_000_000)));
    let mut app = app_with_footer_log(events, session).await;
    let rendered = frame_rows(&mut app, 100, 30).join("\n");
    assert!(rendered.contains("12.5K ctx · $0.0031"), "{rendered}");

    let attempt = AttemptId::new_v7();
    let mut events = footer_event_log(session, run, attempt, Some((12_400, 84)), 2);
    events.push(usage_recorded(session, 5, run, 1, None));
    let mut app = app_with_footer_log(events, session).await;
    let rendered = frame_rows(&mut app, 100, 30).join("\n");
    assert!(rendered.contains("12.5K ctx"), "{rendered}");
    assert!(!rendered.contains('$'), "{rendered}");
}

#[tokio::test]
async fn replayed_footer_cost_matches_engine_session_usage() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let base: Timestamp = "2026-08-06T12:00:00Z".parse().unwrap();
    let at = |seconds: i64| {
        base.checked_add(jiff::SignedDuration::from_secs(seconds))
            .unwrap()
    };
    let stamp = |stored: StoredEvent, seconds: i64| StoredEvent {
        timestamp: at(seconds),
        ..stored
    };
    let reported = Usage {
        input_tokens: Some(12_400),
        input_tokens_cache_read: Some(0),
        output_tokens: Some(84),
        output_tokens_reasoning: Some(0),
        ..Usage::default()
    };
    let mut resolved = resolved_model(None);
    resolved.adapter_id = cookie_agent_protocol::AdaptorId::OpenaiResponses;
    let mut binding = frozen_binding(resolved.clone());
    binding.protocol_recipe =
        cookie_agent_protocol::ProtocolRecipeId::new("oven.openai.responses").unwrap();
    binding.descriptor = serde_json::from_value(serde_json::json!({
        "identity": {"provider_id": "gateway", "model_id": "arbitrary-model"},
        "adapter_id": "openai-responses",
        "capabilities": {
            "features": [],
            "limits": {"context": 8192, "input": null, "output": 2048},
            "modalities": {"input": ["text"], "output": ["text"]},
            "media": {"input": {}},
            "cancellation": "local_only",
            "compaction": "unsupported",
            "replay": {"policy": "never", "capability": "unsupported", "reasoning": false}
        },
        "provider_metadata": {}
    }))
    .unwrap();
    binding.options = cookie_agent_protocol::ProviderOptions::OpenAiResponses {
        organization: None,
        project: None,
        store: None,
    };
    let selection = RunSelection {
        agent: agent_id(),
        model: resolved.selection.clone(),
        preset: None,
    };
    let created =
        session_created_from_bindings(session, 1, selection.clone(), vec![binding.clone()], 0);
    let EventPayload::SessionCreated {
        creation_agent,
        runtime_revision,
        catalog_revision,
        provider_state_revision,
        model_revision,
        agent_revision,
        recipe_registry_revision,
        manifest_revision,
        ..
    } = &created.payload
    else {
        unreachable!()
    };
    let run_started = event(
        session,
        2,
        run,
        EventPayload::RunStarted {
            client_run_id: cookie_agent_protocol::ClientRunId::new("cost-invariant").unwrap(),
            selection,
            agent: creation_agent.clone(),
            runtime_revision: runtime_revision.clone(),
            catalog_revision: catalog_revision.clone(),
            provider_state_revision: provider_state_revision.clone(),
            model_revision: model_revision.clone(),
            agent_revision: agent_revision.clone(),
            recipe_registry_revision: recipe_registry_revision.clone(),
            manifest_revision: manifest_revision.clone(),
            selected_suffix: vec![binding],
            internal_agents: Vec::new(),
            input_through_seq: 1,
        },
    );
    let mut commit = turn_committed(
        session,
        5,
        run,
        attempt,
        1,
        vec![text_part("the answer")],
        Vec::new(),
        None,
    );
    let EventPayload::ModelTurnCommitted {
        input_through_seq,
        resolved_model: committed_model,
        turn,
        ..
    } = &mut commit.payload
    else {
        unreachable!()
    };
    *input_through_seq = 3;
    *committed_model = resolved.clone();
    turn.usage = reported.clone();
    let mut usage = usage_recorded(session, 6, run, 1, Some(3_100_000_000));
    let EventPayload::ModelUsageRecorded {
        resolved_model,
        usage: event_usage,
        ..
    } = &mut usage.payload
    else {
        unreachable!()
    };
    *resolved_model = resolved.clone();
    *event_usage = reported;
    let attempt_started = event(
        session,
        4,
        run,
        EventPayload::ModelAttemptStarted {
            attempt_id: attempt,
            attempt_ordinal: 1,
            fallback_index: 0,
            retry_ordinal: 0,
            resolved_model: resolved,
            prompt_fingerprint: creation_agent.prompt_fingerprint.clone(),
        },
    );
    let events = [
        stamp(created, 0),
        stamp(run_started, 0),
        stamp(
            event(
                session,
                3,
                run,
                EventPayload::UserInputSubmitted {
                    input: "question".into(),
                },
            ),
            0,
        ),
        stamp(attempt_started, 0),
        stamp(commit, 2),
        stamp(usage, 2),
    ];

    let mut tui_store = StateStore::default();
    for event in events.iter().cloned() {
        assert!(tui_store.apply_event(event));
    }
    let state = &tui_store.sessions[&session];
    let item_id = state
        .transcript
        .iter()
        .find_map(|item| matches!(item, TranscriptItem::Assistant { .. }).then(|| item.id()))
        .expect("assistant item");
    let footer = assistant_footer_line(state, item_id, 100, &Theme::default())
        .expect("assistant footer")
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let footer_cost = footer.rsplit(" · ").next().expect("footer cost");

    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let data_dir = directory.path().join("data");
    let session_dir = cookie_agent_engine::session::SessionStore::open(&data_dir, directory.path())
        .expect("session store")
        .workdir_dir_path()
        .to_path_buf()
        .join(session.to_string());
    #[cfg(unix)]
    fs::create_dir_all(&session_dir).unwrap();
    #[cfg(windows)]
    cookie_agent_models::secure_store::SecureDirectory::open(&session_dir)
        .expect("private session directory");
    let jsonl = events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    #[cfg(unix)]
    fs::write(session_dir.join("events.jsonl"), jsonl).unwrap();
    #[cfg(windows)]
    {
        use std::io::Write as _;

        let path = session_dir.join("events.jsonl");
        let mut file = cookie_agent_models::secure_store::create_windows_private_file(&path)
            .expect("private event log");
        file.write_all(jsonl.as_bytes()).expect("write event log");
        file.sync_all().expect("sync event log");
    }
    let engine_sessions =
        cookie_agent_engine::session::SessionStore::open(&data_dir, directory.path()).unwrap();
    let session_cost = format_cost_usd(
        engine_sessions
            .session_usage(
                session,
                &cookie_agent_config::PricingConfig::default(),
                &BTreeMap::new(),
            )
            .unwrap()
            .usage
            .estimated_cost_usd
            .expect("session cost"),
    );
    assert_eq!(footer_cost, session_cost);
}

#[tokio::test]
async fn assistant_footer_hides_without_usage_or_a_positive_duration() {
    let session = SessionId::new_v7();
    let run = run_id();
    // Old sessions carry no usage at all: no placeholder, no row.
    let attempt = AttemptId::new_v7();
    let mut app =
        app_with_footer_log(footer_event_log(session, run, attempt, None, 2), session).await;
    let rendered = frame_rows(&mut app, 100, 30).join("\n");
    assert!(!rendered.contains("tps"), "no usage: {rendered}");

    // A zero-duration span (commit timestamp equals the input's) hides
    // the footer rather than dividing by zero.
    let attempt = AttemptId::new_v7();
    let mut app = app_with_footer_log(
        footer_event_log(session, run, attempt, Some((12_400, 84)), 0),
        session,
    )
    .await;
    let rendered = frame_rows(&mut app, 100, 30).join("\n");
    assert!(!rendered.contains("tps"), "zero duration: {rendered}");
}

#[tokio::test]
async fn interrupted_run_footer_marks_interrupted_block() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let mut events = footer_event_log(session, run, attempt, Some((12_400, 84)), 2);
    events.push(event(
        session,
        5,
        run,
        EventPayload::RunInterrupted { reason: None },
    ));
    let mut app = app_with_footer_log(events, session).await;
    let rendered = frame_rows(&mut app, 100, 30).join("\n");
    assert!(
        rendered.contains("42.0 tps · 12.5K ctx · interrupted"),
        "interrupted footer: {rendered}"
    );

    // A normally completed run's footer carries no marker.
    let attempt = AttemptId::new_v7();
    let mut app = app_with_footer_log(
        footer_event_log(session, run, attempt, Some((12_400, 84)), 2),
        session,
    )
    .await;
    let rendered = frame_rows(&mut app, 100, 30).join("\n");
    assert!(!rendered.contains("interrupted"), "clean: {rendered}");
}

#[tokio::test]
async fn assistant_footer_is_replay_stable_for_the_same_event_log() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let footer_of = |app: &mut App| {
        frame_rows(app, 100, 30)
            .into_iter()
            .find(|row| row.contains("tps"))
            .expect("footer row")
    };
    let mut first = app_with_footer_log(
        footer_event_log(session, run, attempt, Some((12_400, 84)), 2),
        session,
    )
    .await;
    let first = footer_of(&mut first);
    let mut second = app_with_footer_log(
        footer_event_log(session, run, attempt, Some((12_400, 84)), 2),
        session,
    )
    .await;
    let second = footer_of(&mut second);
    assert_eq!(first, second);
}

#[tokio::test]
async fn assistant_footer_wraps_within_narrow_widths() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let mut app = app_with_footer_log(
        footer_event_log(session, run, attempt, Some((12_400, 84)), 2),
        session,
    )
    .await;
    // Every width renders without overflow; once the wrapped footer
    // fits whole words (≈16 cells) it stays visible.
    for width in [3, 8, 12, 16, 24] {
        let rows = frame_rows(&mut app, width, 40);
        assert!(
            rows.iter()
                .all(|row| row.chars().count() <= usize::from(width)),
            "width {width}: {rows:?}"
        );
    }
    for width in [16, 24] {
        let rendered = frame_rows(&mut app, width, 40).join("\n");
        assert!(
            rendered.contains("tps"),
            "width {width} keeps the footer: {rendered}"
        );
    }
}
