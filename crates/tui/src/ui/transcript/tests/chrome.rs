use std::time::Duration;

use crate::ui::transcript::*;

use cookie_agent_protocol::{
    AttemptId, EventPayload, ModelSelection, ProducerMessageId, RunSelection, SessionId,
    SessionOrigin, SessionStatus, SessionTree, StoredEvent,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use ratatui::{Terminal, backend::TestBackend};

use crate::client::ClientDelivery;

use crate::state::{SessionState, StateStore};

use crate::ui::app::*;

use crate::ui::slash::SlashCommand;

use crate::ui::terminal_layout_with_tree_rows;

use cookie_agent_server::MessageFrame;

use super::support::*;

#[test]
fn model_turn_committed_updates_latest_context_tokens() {
    let session = SessionId::new_v7();
    let run = run_id();
    let first_attempt = AttemptId::new_v7();
    let second_attempt = AttemptId::new_v7();
    let with_input_tokens = |mut event: StoredEvent, input_tokens| {
        let EventPayload::ModelTurnCommitted { turn, .. } = &mut event.payload else {
            panic!("expected committed turn");
        };
        turn.usage.input_tokens = Some(input_tokens);
        event
    };
    let events = vec![
        session_created(session, 1),
        attempt_started(session, 2, run, first_attempt, None),
        with_input_tokens(
            turn_committed(
                session,
                3,
                run,
                first_attempt,
                1,
                Vec::new(),
                Vec::new(),
                None,
            ),
            1_200,
        ),
        attempt_started(session, 4, run, second_attempt, None),
        with_input_tokens(
            turn_committed(
                session,
                5,
                run,
                second_attempt,
                2,
                Vec::new(),
                Vec::new(),
                None,
            ),
            48_200,
        ),
    ];
    let mut store = StateStore::default();
    assert!(store.rebuild_session(session, 1, events));
    // End-of-turn total: 48,200 consumed plus the fixture's 4 generated.
    assert_eq!(store.sessions[&session].context_tokens, Some(48_204));
}

#[test]
fn terminal_layout_has_exact_rects_for_wide_square_tall_and_tiny_terminals() {
    for (width, height) in [(160, 50), (80, 24), (40, 12), (20, 8), (8, 2), (4, 1)] {
        let layout = terminal_layout_with_tree_rows(Rect::new(0, 0, width, height), 3, 0, false, 1);
        assert_eq!(layout.agent.y, 0);
        assert_eq!(layout.conversation.y, layout.agent.height);
        assert_eq!(layout.bar.height, 1.min(height));
        assert_eq!(layout.bar.y + layout.bar.height, height);
        assert_eq!(layout.input.y + layout.input.height, layout.bar.y);
        assert!(layout.status.y + layout.status.height <= layout.input.y);
    }
}

#[tokio::test]
async fn bottom_bar_renders_cwd_context_and_narrow_degradation_order() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.sessions = vec![session_meta(session)];
    app.store.sessions.insert(
        session,
        SessionState {
            context_tokens: Some(48_200),
            estimated_cost_usd: Some(0.18),
            ..SessionState::default()
        },
    );
    let mut descriptor = model_descriptor();
    descriptor.capabilities.context_tokens = 200_000;
    app.models = vec![descriptor];
    app.draft = Some(RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: model_key(),
            variant: None,
        },
        preset: None,
    });

    let wide = rendered_row(&mut app, 100, 24, 23);
    assert!(wide.contains("/workspace"));
    assert!(wide.contains("auto-approve    $0.18    ctx 48.2K (24%)    `ctrl+p` commands"));

    let without_hint = rendered_row(&mut app, 55, 24, 23);
    assert!(without_hint.contains("auto-approve    $0.18    ctx 48.2K (24%)"));
    assert!(!without_hint.contains("ctrl+p"));

    let without_cost = rendered_row(&mut app, 39, 24, 23);
    assert!(
        without_cost.contains("auto-approve    ctx 48.2K (24%)"),
        "{without_cost}"
    );
    assert!(!without_cost.contains("$0.18"));

    let without_percentage = rendered_row(&mut app, 30, 24, 23);
    assert!(without_percentage.contains("auto-approve    ctx 48.2K"));
    assert!(!without_percentage.contains("24%"));

    let mode_only = rendered_row(&mut app, 18, 24, 23);
    assert!(mode_only.contains("auto-approve"));
    assert!(!mode_only.contains("ctx"));

    app.store
        .sessions
        .get_mut(&session)
        .expect("session")
        .context_tokens = Some(u64::MAX);
    let no_reintroduced_hint = rendered_row(&mut app, 33, 24, 23);
    assert!(no_reintroduced_hint.contains("auto-approve"));
    assert!(!no_reintroduced_hint.contains("ctrl+p"));
    assert!(!no_reintroduced_hint.contains("ctx"));

    app.store
        .sessions
        .get_mut(&session)
        .expect("session")
        .estimated_cost_usd = Some(0.0031);
    let compact = rendered_row(&mut app, 100, 24, 23);
    assert!(compact.contains("$0.0031"), "{compact}");
    app.store
        .sessions
        .get_mut(&session)
        .expect("session")
        .estimated_cost_usd = None;
    let unpriced = rendered_row(&mut app, 100, 24, 23);
    assert!(!unpriced.contains('$'), "{unpriced}");
}

#[tokio::test]
async fn bottom_bar_working_indicator_tracks_running_queued_and_idle_states() {
    let (mut app, session, run) = app_with_active_run().await;
    app.sessions = vec![session_meta(session)];
    app.animation_ticks = 0;
    let row = rendered_row(&mut app, 100, 24, 23);
    assert!(row.starts_with("◐ working"), "{row}");
    assert!(!row.contains("/workspace"));
    let initial_mode_hit = app.hit_map.permission_mode;
    for glyph in ['◓', '◑', '◒', '◐'] {
        for _ in 0..12 {
            app.animation_tick();
        }
        assert!(rendered_row(&mut app, 100, 24, 23).starts_with(&format!("{glyph} working")));
        assert_eq!(app.hit_map.permission_mode, initial_mode_hit);
    }
    app.store.sessions.get_mut(&session).unwrap().active_run = None;
    let idle = rendered_row(&mut app, 100, 24, 23);
    assert!(idle.starts_with("/workspace"), "{idle}");
    assert!(!idle.contains("working"));
    assert!(
        app.store
            .apply_event(admitted(session, 1, run, "user input"))
    );
    for (seq, status) in [
        (2, ProducerMessageStatus::Pending),
        (3, ProducerMessageStatus::Admitted),
    ] {
        let id = ProducerMessageId::new_v7();
        let mut event = producer_accepted(
            session,
            seq,
            id,
            ProducerOwner::Plugin {
                plugin: "ci".into(),
            },
            ProducerDeliveryMode::Queue,
            "raw body stays hidden",
            None,
        );
        let EventPayload::ProducerMessageAccepted { description, .. } = &mut event.payload else {
            unreachable!()
        };
        *description = cookie_agent_protocol::SafeDisplayText::new("Build finished").unwrap();
        assert!(app.store.apply_event(event));
        set_producer_status(&mut app, session, id, status);
    }
    assert_eq!(app.selected_queue_entries()[1].preview, "Build finished");
    let queued = rendered_row(&mut app, 100, 24, 23);
    assert!(queued.starts_with("◐ 3 queued"), "{queued}");
    assert!(!queued.contains("/workspace"));
    assert_eq!(app.hit_map.permission_mode, initial_mode_hit);
    app.store
        .sessions
        .get_mut(&session)
        .unwrap()
        .pending_inputs
        .clear();
    for item in &mut app.store.sessions.get_mut(&session).unwrap().transcript {
        if let TranscriptItem::ProducerMessage { status, .. } = item {
            *status = ProducerMessageStatus::Consumed;
        }
    }
    assert_eq!(rendered_row(&mut app, 100, 24, 23), idle);
}

#[tokio::test]
async fn animation_active_covers_active_runs_and_pending_producers() {
    let (mut app, session, _) = app_with_active_run().await;
    assert!(app.animation_active());
    app.store.sessions.get_mut(&session).unwrap().active_run = None;
    assert!(!app.animation_active());
    let id = ProducerMessageId::new_v7();
    assert!(app.store.apply_event(producer_accepted(
        session,
        1,
        id,
        ProducerOwner::Plugin {
            plugin: "ci".into()
        },
        ProducerDeliveryMode::Queue,
        "body",
        None
    )));
    for status in [
        ProducerMessageStatus::Pending,
        ProducerMessageStatus::Admitted,
        ProducerMessageStatus::Claimed,
        ProducerMessageStatus::Consumed,
        ProducerMessageStatus::Discarded,
    ] {
        set_producer_status(&mut app, session, id, status);
        let pending = matches!(
            status,
            ProducerMessageStatus::Pending | ProducerMessageStatus::Admitted
        );
        assert_eq!(
            app.store.sessions[&session].has_pending_producers(),
            pending
        );
        assert_eq!(app.animation_active(), pending);
    }
    app.selected = Some(SessionId::new_v7());
    assert!(!app.animation_active());
}

#[tokio::test]
async fn clicking_bottom_bar_cost_opens_usage_panel() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.sessions = vec![session_meta(session)];
    app.store.sessions.insert(
        session,
        SessionState {
            estimated_cost_usd: Some(0.18),
            ..SessionState::default()
        },
    );
    rendered_frame(&mut app, 80, 24);
    let hit = app.hit_map.session_cost.expect("session cost hit");
    app.handle_click(hit.x, hit.y).await;
    assert_eq!(app.modal, Modal::Usage);
    assert!(app.usage_panel.loading);
}

#[tokio::test]
async fn tree_usage_corruption_has_a_distinct_panel_state_from_missing_session() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.modal = Modal::Usage;
    app.usage_load_generation = 1;
    app.usage_panel.begin_load();
    app.handle_rpc_update(failed_tree_usage_update(
        1,
        session,
        cookie_agent_protocol::SESSION_TREE_USAGE_CORRUPT_DELEGATION_CODE,
        "session tree usage corrupted delegation record",
    ));
    let corrupt = rendered_frame(&mut app, 100, 24);
    assert!(
        corrupt.contains("Tree usage unavailable: corrupted delegation record"),
        "{corrupt}"
    );

    app.usage_load_generation = 2;
    app.usage_panel.begin_load();
    app.handle_rpc_update(failed_tree_usage_update(
        2,
        session,
        cookie_agent_protocol::SESSION_TREE_USAGE_MISSING_SESSION_CODE,
        "session tree usage session not found",
    ));
    let missing = rendered_frame(&mut app, 100, 24);
    assert!(
        !missing.contains("corrupted delegation record"),
        "{missing}"
    );
    assert!(missing.contains("No usage available."), "{missing}");
}

#[tokio::test]
async fn stale_usage_load_cannot_clobber_reopen_for_a_different_session() {
    let (client, _requests) = recording_client();
    let mut app = App::new(client).await.expect("test app");
    let first = SessionId::new_v7();
    let second = SessionId::new_v7();
    app.sessions = vec![session_meta(first), session_meta(second)];
    for session_id in [first, second] {
        app.store.sessions.insert(
            session_id,
            SessionState {
                estimated_cost_usd: Some(0.18),
                ..SessionState::default()
            },
        );
    }

    app.selected = Some(first);
    open_usage_from_bottom_bar(&mut app).await;
    let stale_generation = app.usage_load_generation;
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    app.selected = Some(second);
    open_usage_from_bottom_bar(&mut app).await;
    let current_generation = app.usage_load_generation;
    assert!(current_generation > stale_generation);

    app.handle_rpc_update(usage_loaded_update(stale_generation, first, 99));
    assert!(app.usage_panel.loading);
    assert!(app.usage_panel.session.is_none());
    assert!(app.usage_panel.tree.is_none());
    app.handle_rpc_update(usage_loaded_update(current_generation, second, 2));
    assert!(!app.usage_panel.loading);
    assert_eq!(
        app.usage_panel
            .session
            .as_ref()
            .map(|result| (result.session_id, result.usage.request_count)),
        Some((second, 2))
    );
}

#[tokio::test]
async fn stale_usage_load_cannot_clobber_reopen_for_the_same_session() {
    let (client, _requests) = recording_client();
    let mut app = App::new(client).await.expect("test app");
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.sessions = vec![session_meta(session)];
    app.store.sessions.insert(
        session,
        SessionState {
            estimated_cost_usd: Some(0.18),
            ..SessionState::default()
        },
    );

    open_usage_from_bottom_bar(&mut app).await;
    let stale_generation = app.usage_load_generation;
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    open_usage_from_bottom_bar(&mut app).await;
    let current_generation = app.usage_load_generation;

    app.handle_rpc_update(usage_loaded_update(stale_generation, session, 99));
    assert!(app.usage_panel.loading);
    assert!(app.usage_panel.session.is_none());
    app.handle_rpc_update(usage_loaded_update(current_generation, session, 1));
    assert_eq!(
        app.usage_panel
            .session
            .as_ref()
            .map(|result| result.usage.request_count),
        Some(1)
    );
}

#[tokio::test]
async fn usage_panel_loads_session_and_tree_without_refreshing_bottom_bar_cost() {
    let (startup_client, _startup) = recording_client();
    let mut app = App::new(startup_client).await.expect("test app");
    let (client, recorded, incoming) = live_recording_client();
    app.client = client;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.sessions = vec![session_meta(session)];
    app.store.sessions.insert(
        session,
        SessionState {
            estimated_cost_usd: Some(0.18),
            ..SessionState::default()
        },
    );
    rendered_frame(&mut app, 80, 24);
    let hit = app.hit_map.session_cost.expect("session cost hit");
    app.handle_click(hit.x, hit.y).await;

    let session_request = wait_for_recorded_request(&recorded, "session.usage", 1).await;
    let tree_request = wait_for_recorded_request(&recorded, "session.tree_usage", 1).await;
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": session_request,
            "result": cookie_agent_protocol::SessionUsageResult {
                session_id: session,
                usage: cookie_agent_protocol::UsageRollup {
                    request_count: 1,
                    estimated_cost_usd: Some(99.0),
                    ..cookie_agent_protocol::UsageRollup::default()
                }
            }
        })))
        .expect("session usage response");
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": tree_request,
            "result": cookie_agent_protocol::SessionTreeUsageResult {
                session_id: session,
                usage: cookie_agent_protocol::UsageRollup {
                    request_count: 3,
                    estimated_cost_usd: Some(0.42),
                    ..cookie_agent_protocol::UsageRollup::default()
                },
                session_count: 3,
            }
        })))
        .expect("tree usage response");
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("usage panel update timeout")
        .expect("usage panel update");
    app.handle_rpc_update(update);

    assert_eq!(
        app.usage_panel
            .session
            .as_ref()
            .map(|result| result.usage.request_count),
        Some(1)
    );
    assert_eq!(
        app.usage_panel
            .tree
            .as_ref()
            .map(|result| (result.usage.request_count, result.session_count)),
        Some((3, 3))
    );
    assert_eq!(app.store.sessions[&session].estimated_cost_usd, Some(0.18));
}

#[tokio::test]
async fn usage_modal_owns_keyboard_and_wheel_scrolling() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    let by_model = (0..8)
        .map(|index| {
            (
                format!("test/model-{index}").parse().unwrap(),
                cookie_agent_protocol::ModelUsageRollup {
                    request_count: 1,
                    input_tokens: 1_000,
                    estimated_cost_usd: Some(f64::from(index) / 100.0),
                    ..cookie_agent_protocol::ModelUsageRollup::default()
                },
            )
        })
        .collect();
    let usage = cookie_agent_protocol::UsageRollup {
        request_count: 8,
        by_model,
        ..cookie_agent_protocol::UsageRollup::default()
    };
    app.usage_panel.session = Some(cookie_agent_protocol::SessionUsageResult {
        session_id: session,
        usage: usage.clone(),
    });
    app.usage_panel.tree = Some(cookie_agent_protocol::SessionTreeUsageResult {
        session_id: session,
        usage,
        session_count: 2,
    });
    app.modal = Modal::Usage;
    rendered_frame(&mut app, 80, 16);
    app.conversation_scroll.offset = 17;
    app.conversation_scroll.following = false;

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    assert_eq!(app.usage_panel.scroll, 1);
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })
    .await;
    assert_eq!(app.usage_panel.scroll, 4);
    app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
        .await;
    assert!(app.usage_panel.scroll > 4);
    for _ in 0..100 {
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .await;
    }
    let max_scroll = app.usage_panel.scroll;
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    assert_eq!(app.usage_panel.scroll, max_scroll);
    assert_eq!(app.conversation_scroll.offset, 17);
    assert!(!app.conversation_scroll.following);
    app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
        .await;
    assert!(app.usage_panel.scroll < max_scroll);
    assert_eq!(app.conversation_scroll.offset, 17);
}

#[tokio::test]
async fn usage_events_refresh_session_cost_single_flight() {
    let (startup_client, _startup) = recording_client();
    let mut app = App::new(startup_client).await.expect("test app");
    let (client, recorded, incoming) = live_recording_client();
    app.client = client;
    let session = SessionId::new_v7();
    let run = run_id();
    app.selected = Some(session);
    app.store.sessions.insert(session, SessionState::default());

    app.handle_delivery(live_event(usage_recorded(session, 1, run, 1, Some(1))))
        .await;
    // Leading-edge refresh starts without waiting for a debounce update
    // to be driven through the app loop.
    let id = wait_for_recorded_request(&recorded, "session.usage", 1).await;
    assert_eq!(recorded_method_count(&recorded, "session.usage"), 1);
    // This commit arrives after the first request was captured, so its
    // authoritative total requires one trailing refresh.
    app.handle_delivery(live_event(usage_recorded(session, 2, run, 2, Some(1))))
        .await;
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": cookie_agent_protocol::SessionUsageResult {
                session_id: session,
                usage: cookie_agent_protocol::UsageRollup {
                    estimated_cost_usd: Some(0.10),
                    ..cookie_agent_protocol::UsageRollup::default()
                }
            }
        })))
        .expect("script usage response");
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("usage update timeout")
        .expect("usage update");
    app.handle_rpc_update(update);

    let id = drive_until_recorded_request(&mut app, &recorded, "session.usage", 2).await;
    assert_eq!(recorded_method_count(&recorded, "session.usage"), 2);
    let current_request_id = app
        .session_cost_request_id_for_test(session)
        .expect("trailing request id");
    app.handle_rpc_update(RpcUpdate::SessionCostLoaded {
        session_id: session,
        request_id: current_request_id.wrapping_sub(1),
        result: Ok(cookie_agent_protocol::SessionUsageResult {
            session_id: session,
            usage: cookie_agent_protocol::UsageRollup {
                estimated_cost_usd: Some(99.0),
                ..cookie_agent_protocol::UsageRollup::default()
            },
        }),
    });
    assert_eq!(app.store.sessions[&session].estimated_cost_usd, Some(0.10));
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": cookie_agent_protocol::SessionUsageResult {
                session_id: session,
                usage: cookie_agent_protocol::UsageRollup {
                    estimated_cost_usd: Some(0.18),
                    ..cookie_agent_protocol::UsageRollup::default()
                }
            }
        })))
        .expect("script trailing usage response");
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("trailing usage update timeout")
        .expect("trailing usage update");
    app.handle_rpc_update(update);

    assert!(app.session_cost_refresh_idle_for_test(session));
    assert_eq!(app.store.sessions[&session].estimated_cost_usd, Some(0.18));
}

#[tokio::test]
async fn bottom_bar_permission_mode_is_shared_by_tree_and_child_click_updates_root() {
    let (client, _startup_requests) = recording_client();
    let mut app = App::new(client).await.expect("test app");
    let (client, requests, _incoming) = live_recording_client();
    app.client = client;
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.sessions = vec![session_meta(root)];
    app.tree_root = Some(root);
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: delegated_meta(child, root, "worker"),
            children: Vec::new(),
        }],
    });
    app.selected = Some(child);
    app.permission_modes
        .insert(root, cookie_agent_protocol::PermissionMode::Ask);
    requests.lock().expect("requests lock").clear();

    let child_row = rendered_row(&mut app, 80, 24, 23);
    // Without token data the bar shows no placeholder segment — just
    // the mode and the commands hint.
    assert!(child_row.contains("ask    `ctrl+p` commands"));
    assert!(!child_row.contains("ctx"), "{child_row}");
    let hit = app.hit_map.permission_mode.expect("permission mode hit");
    app.handle_click(hit.x, hit.y).await;
    assert_eq!(
        app.permission_modes[&root],
        cookie_agent_protocol::PermissionMode::Yolo
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if requests
                .lock()
                .expect("requests lock")
                .iter()
                .any(|request| {
                    request["method"] == "session.set_permission_mode"
                        && request["params"]["session_id"] == serde_json::json!(root)
                        && request["params"]["mode"] == "yolo"
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("permission mode RPC");
    assert!(
        requests
            .lock()
            .expect("requests lock")
            .iter()
            .any(|request| {
                request["method"] == "session.set_permission_mode"
                    && request["params"]["session_id"] == serde_json::json!(root)
                    && request["params"]["mode"] == "yolo"
            })
    );

    app.selected = Some(root);
    assert!(rendered_row(&mut app, 80, 24, 23).contains("yolo"));
    for (expected_mode, expected_label) in [
        (
            cookie_agent_protocol::PermissionMode::AutoApprove,
            "auto-approve",
        ),
        (
            cookie_agent_protocol::PermissionMode::AutoApproveN,
            "auto-n",
        ),
        (
            cookie_agent_protocol::PermissionMode::AutoApproveY,
            "auto-y",
        ),
        (cookie_agent_protocol::PermissionMode::Ask, "ask"),
    ] {
        rendered_row(&mut app, 80, 24, 23);
        let hit = app.hit_map.permission_mode.expect("permission mode hit");
        app.handle_click(hit.x, hit.y).await;
        assert_eq!(app.permission_modes[&root], expected_mode);
        assert!(
            rendered_row(&mut app, 80, 24, 23).contains(expected_label),
            "missing permission mode label {expected_label}"
        );
    }
}

#[tokio::test]
async fn selecting_a_child_loads_the_tree_permission_mode_from_the_root() {
    let (startup_client, _startup) = recording_client();
    let mut app = App::new(startup_client).await.expect("test app");
    let (client, recorded, incoming) = live_recording_client();
    app.client = client;
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.sessions = vec![session_meta(root), delegated_meta(child, root, "worker")];
    app.store.sessions.insert(child, SessionState::default());

    app.set_selected_session(child);
    let id = wait_for_recorded_request(&recorded, "session.permission.get", 1).await;
    assert!(
        recorded
            .lock()
            .expect("requests lock")
            .iter()
            .any(|request| {
                request["method"] == "session.permission.get"
                    && request["params"]["session_id"] == serde_json::json!(root)
            })
    );
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "permissions": [],
                "current_mode": "auto_approve_y"
            }
        })))
        .expect("script permission mode response");
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("permission mode update timeout")
        .expect("permission mode update");
    app.handle_rpc_update(update);

    assert_eq!(
        app.permission_modes[&root],
        cookie_agent_protocol::PermissionMode::AutoApproveY
    );
    assert!(!app.permission_modes.contains_key(&child));
    assert!(rendered_row(&mut app, 80, 24, 23).contains("auto-y"));
}

#[tokio::test]
async fn failed_mode_click_reloads_authoritative_state_after_stale_hydration() {
    let (startup_client, _startup) = recording_client();
    let mut app = App::new(startup_client).await.expect("test app");
    let (client, recorded, incoming) = live_recording_client();
    app.client = client;
    let session = SessionId::new_v7();
    app.sessions = vec![session_meta(session)];
    app.store.sessions.insert(session, SessionState::default());

    app.set_selected_session(session);
    let hydration_id = wait_for_recorded_request(&recorded, "session.permission.get", 1).await;
    rendered_row(&mut app, 80, 24, 23);
    let hit = app.hit_map.permission_mode.expect("permission mode hit");
    app.handle_click(hit.x, hit.y).await;
    assert_eq!(
        app.permission_modes[&session],
        cookie_agent_protocol::PermissionMode::AutoApproveN
    );
    let mutation_id = wait_for_recorded_request(&recorded, "session.set_permission_mode", 1).await;

    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": hydration_id,
            "result": {
                "permissions": [],
                "current_mode": "auto_approve_y"
            }
        })))
        .expect("script stale hydration response");
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("stale hydration update timeout")
        .expect("stale hydration update");
    app.handle_rpc_update(update);
    assert_eq!(
        app.permission_modes[&session],
        cookie_agent_protocol::PermissionMode::AutoApproveN
    );

    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": mutation_id,
            "error": {
                "code": -32000,
                "message": "set mode failed",
                "data": null
            }
        })))
        .expect("script failed mutation response");
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("mutation failure update timeout")
        .expect("mutation failure update");
    app.handle_rpc_update(update);
    assert!(!app.permission_modes.contains_key(&session));

    let retry_id = wait_for_recorded_request(&recorded, "session.permission.get", 2).await;
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": retry_id,
            "result": {
                "permissions": [],
                "current_mode": "auto_approve_y"
            }
        })))
        .expect("script authoritative hydration response");
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("authoritative hydration update timeout")
        .expect("authoritative hydration update");
    app.handle_rpc_update(update);

    assert_eq!(
        app.permission_modes[&session],
        cookie_agent_protocol::PermissionMode::AutoApproveY
    );
    assert!(rendered_row(&mut app, 80, 24, 23).contains("auto-y"));
}

#[tokio::test]
async fn agent_panel_text_rows_are_clamped_1_to_4_with_borders_outside() {
    let mut app = test_app().await;
    for sessions in [0usize, 1] {
        let layout = terminal_layout_with_tree_rows(Rect::new(0, 0, 80, 24), sessions, 0, false, 1);
        assert_eq!(layout.agent.height, 0);
        assert_eq!(layout.conversation.y, 0);
    }
    for (sessions, expected_rows) in [(2usize, 4u16), (3, 5), (4, 6), (5, 6), (9, 6)] {
        let layout = terminal_layout_with_tree_rows(Rect::new(0, 0, 80, 24), sessions, 0, false, 1);
        app.tree = Some(SessionTree {
            session: session_meta(SessionId::new_v7()),
            children: (1..sessions)
                .map(|_| SessionTree {
                    session: session_meta(SessionId::new_v7()),
                    children: Vec::new(),
                })
                .collect(),
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let entries = app.tree_entries();
        terminal
            .draw(|frame| app.render_tree(frame, layout.agent, &entries))
            .expect("render");
        let buffer = terminal.backend().buffer().clone();
        let top = buffer[(0, 0)].symbol() == "╭";
        let bottom = buffer[(0, expected_rows - 1)].symbol() == "╰";
        let below = buffer[(0, expected_rows)].symbol() == "╰";
        assert!(top, "sessions {sessions}");
        assert!(bottom, "sessions {sessions}");
        assert!(!below, "sessions {sessions}");
    }
    let tiny = terminal_layout_with_tree_rows(Rect::new(0, 0, 20, 8), 20, 0, false, 1);
    // The single-row composer is three rows tall, so the eight-row
    // terminal leaves four rows above the bar: one for the status line,
    // one guaranteed conversation row, and the rest for the agent panel
    // (borders only at this extreme). The four-row viewport cap never
    // lifts the panel above the remaining content space.
    assert_eq!(tiny.agent.height, 2);
    assert_eq!(tiny.conversation.height, 1);
}

#[tokio::test]
async fn agent_panel_caps_at_four_rows_and_scrolls_a_longer_tree() {
    // One frame render, read back as the scroll offset, the hit-tested
    // rows, and the text actually painted inside the viewport.
    fn panel_state(app: &mut App) -> (usize, Vec<SessionId>, Vec<String>) {
        let rows = frame_rows(app, 80, 30);
        let inner = app.hit_map.tree.expect("agents viewport");
        let visible = app
            .hit_map
            .tree_rows
            .iter()
            .map(|hit| hit.session_id)
            .collect::<Vec<_>>();
        let text = (usize::from(inner.y)..usize::from(inner.bottom()))
            .map(|row| rows[row].clone())
            .collect::<Vec<_>>();
        (app.tree_offset, visible, text)
    }

    let max_rows = crate::ui::MAX_AGENT_PANEL_ROWS;
    // Pinned literally: the viewport is four rows, not the eight rows the
    // panel used to allow. Everything below derives from that cap.
    assert_eq!(max_rows, 4);
    let mut app = test_app().await;
    let root = SessionId::new_v7();
    app.selected = Some(root);
    app.tree_root = Some(root);
    app.tree = Some(SessionTree {
        session: titled_meta(root, "root", 1),
        children: (0..(max_rows * 2 - 1))
            .map(|index| SessionTree {
                session: titled_meta(
                    SessionId::new_v7(),
                    &format!("row {index}"),
                    u64::try_from(index).expect("index") + 1,
                ),
                children: Vec::new(),
            })
            .collect(),
    });
    // Panel order is the flattened tree, whose children sort by activity
    // and session id, so every expectation reads back from `entries`.
    let entries = app.tree_entries();
    assert_eq!(entries.len(), max_rows * 2);
    let labels = entries
        .iter()
        .map(|(_, meta, _)| format!("primary:{}", meta.title.as_ref().expect("title")))
        .collect::<Vec<_>>();

    // Seven live entries still earn only the capped viewport: MAX rows plus
    // two borders, with the conversation starting right below it.
    let layout =
        terminal_layout_with_tree_rows(Rect::new(0, 0, 80, 30), entries.len(), 0, false, 1);
    assert_eq!(
        layout.agent.height,
        u16::try_from(max_rows + 2).expect("rows")
    );
    assert_eq!(layout.conversation.y, layout.agent.height);

    let (offset, visible, text) = panel_state(&mut app);
    assert_eq!(app.tree_viewport_height, max_rows);
    assert_eq!(offset, 0);
    assert_eq!(
        visible,
        entries[..max_rows]
            .iter()
            .map(|(session_id, _, _)| *session_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(text.len(), max_rows);
    assert!(text.iter().any(|row| row.contains(&labels[max_rows - 1])));
    assert!(!text.iter().any(|row| row.contains(&labels[max_rows])));

    // Walking the cursor down scrolls the window one row at a time only
    // once the cursor leaves the viewport, and never past the last page.
    let max_offset = entries.len() - max_rows;
    for (step, label) in labels.iter().enumerate().skip(1) {
        app.move_tree_selection(false);
        let expected_offset = step
            .saturating_add(1)
            .saturating_sub(max_rows)
            .min(max_offset);
        let (offset, visible, text) = panel_state(&mut app);
        assert_eq!(offset, expected_offset, "cursor on {label}");
        assert_eq!(
            visible,
            entries[expected_offset..expected_offset + max_rows]
                .iter()
                .map(|(session_id, _, _)| *session_id)
                .collect::<Vec<_>>()
        );
        assert!(
            text.iter().any(|row| row.contains(label)),
            "cursor row {label} must stay visible: {text:?}",
        );
    }
    let (_, _, tail) = panel_state(&mut app);
    assert!(
        tail.iter()
            .any(|row| row.contains(labels.last().expect("row")))
    );
    assert!(!tail.iter().any(|row| row.contains(&labels[0])));

    // Walking back up scrolls symmetrically and lands on the first page.
    for _ in 1..entries.len() {
        app.move_tree_selection(true);
    }
    let (offset, visible, _) = panel_state(&mut app);
    assert_eq!(offset, 0);
    assert_eq!(visible.len(), max_rows);
}

#[tokio::test]
async fn agent_panel_tracks_delegated_agent_visibility_transitions() {
    let mut app = test_app().await;
    let (client, recorded, _incoming) = live_recording_client();
    app.client = client;
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let root_tree = || SessionTree {
        session: session_meta(root),
        children: Vec::new(),
    };
    app.selected = Some(root);
    app.tree_root = Some(root);
    app.tree = Some(root_tree());

    let root_only = frame_rows(&mut app, 80, 24);
    assert!(!root_only.iter().any(|row| row.contains("Agents")));
    assert_eq!(app.hit_map.conversation.expect("conversation").y, 1);
    assert!(app.hit_map.tree.is_none());

    let mut child_meta = delegated_meta(child, root, "worker");
    child_meta.status = SessionStatus::Running;
    app.tree = Some(SessionTree {
        session: titled_meta(root, "root session", 1),
        children: vec![SessionTree {
            session: child_meta,
            children: Vec::new(),
        }],
    });
    let delegated = frame_rows(&mut app, 80, 24);
    assert!(delegated.iter().any(|row| row.contains("Agents")));
    assert!(app.hit_map.conversation.expect("conversation").y > 1);
    assert!(app.hit_map.tree.is_some());

    app.handle_delivery(live_event(event(
        child,
        3,
        run_id(),
        EventPayload::RunCompleted { final_text: None },
    )))
    .await;
    assert_eq!(app.tree.as_ref().expect("tree").children.len(), 1);
    assert_eq!(
        app.tree.as_ref().expect("tree").children[0].session.status,
        SessionStatus::Completed
    );
    let completed = frame_rows(&mut app, 80, 24);
    assert!(!completed.iter().any(|row| row.contains("Agents")));
    assert_eq!(app.hit_map.conversation.expect("conversation").y, 1);
    assert!(app.hit_map.tree.is_none());
    assert!(app.hit_map.tree_rows.is_empty());
    wait_for_recorded_request(&recorded, "session.tree", 1).await;
}

#[tokio::test]
async fn agent_panel_keeps_watched_subagent_history_after_completion_at_each_depth() {
    for nested in [false, true] {
        let mut app = test_app().await;
        let (client, _recorded, _incoming) = live_recording_client();
        app.client = client;
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        let watched = if nested { SessionId::new_v7() } else { child };
        let mut watched_meta = delegated_meta(watched, root, "worker");
        watched_meta.status = SessionStatus::Running;
        if nested
            && let SessionOrigin::Delegated {
                parent_session_id,
                depth,
                ..
            } = &mut watched_meta.origin
        {
            *parent_session_id = child;
            *depth = 2;
        }
        let watched_tree = SessionTree {
            session: watched_meta,
            children: Vec::new(),
        };
        let child_tree = if nested {
            let mut child_meta = delegated_meta(child, root, "parent");
            child_meta.status = SessionStatus::Completed;
            SessionTree {
                session: child_meta,
                children: vec![watched_tree],
            }
        } else {
            watched_tree
        };
        app.tree_root = Some(root);
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![child_tree],
        });
        app.set_selected_session(root);
        assert_agent_panel_visible(&mut app, true);
        app.watch_session(watched);
        assert_agent_panel_visible(&mut app, true);
        let conversation_y = app.hit_map.conversation.unwrap().y;

        app.handle_delivery(live_event(event(
            watched,
            3,
            run_id(),
            EventPayload::RunCompleted { final_text: None },
        )))
        .await;
        assert_agent_panel_visible(&mut app, true);
        assert_eq!(app.hit_map.conversation.unwrap().y, conversation_y);
        assert_eq!(app.selected, Some(watched));
        assert_eq!(app.tree_root, Some(root));

        // An empty cursor replay must not change the watched-history rule.
        app.handle_delivery(ClientDelivery::ReplayStart {
            session_id: watched,
            generation: 0,
            final_seq: 3,
            rebuild: false,
        })
        .await;
        assert_agent_panel_visible(&mut app, true);
        app.handle_delivery(ClientDelivery::ReplayEnd {
            session_id: watched,
            generation: 0,
            final_seq: 3,
        })
        .await;
        assert_agent_panel_visible(&mut app, true);

        app.watch_session(root);
        assert_agent_panel_visible(&mut app, false);
        // Selecting already-completed history also enables automatic display.
        app.watch_session(watched);
        assert_agent_panel_visible(&mut app, true);
        app.collapsed_sessions.insert(root);
        assert_eq!(app.tree_entries().len(), 1);
        assert_agent_panel_visible(&mut app, true);
        app.watch_session(root);
        assert_agent_panel_visible(&mut app, false);
    }
}

#[tokio::test]
async fn agent_panel_manual_override_wins_for_completed_history_and_rerooting() {
    let mut app = test_app().await;
    let (client, _recorded, _incoming) = live_recording_client();
    app.client = client;
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let mut child_meta = delegated_meta(child, root, "worker");
    child_meta.status = SessionStatus::Completed;
    app.sessions = vec![session_meta(root), child_meta.clone()];
    let tree = SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: child_meta.clone(),
            children: Vec::new(),
        }],
    };
    app.tree_root = Some(root);
    app.tree = Some(tree.clone());
    app.set_selected_session(root);
    assert_agent_panel_visible(&mut app, false);
    app.run_command(SlashCommand::HideAgentPanel).await;
    app.watch_session(child);
    assert_agent_panel_visible(&mut app, false);
    app.watch_session(root);
    app.reroot_tree(root);
    app.tree = Some(tree);
    app.watch_session(child);
    assert_agent_panel_visible(&mut app, false);

    app.run_command(SlashCommand::ShowAgentPanel).await;
    assert_agent_panel_visible(&mut app, true);
    app.watch_session(root);
    assert_agent_panel_visible(&mut app, true);

    // A different root resets the override as before. Delegated identity
    // comes from metadata, even when the watched child is the tree's root.
    app.run_command(SlashCommand::HideAgentPanel).await;
    app.reroot_tree(child);
    assert_agent_panel_visible(&mut app, true);
    app.tree = Some(SessionTree {
        session: child_meta,
        children: Vec::new(),
    });
    assert_agent_panel_visible(&mut app, true);
    app.reroot_tree(root);
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: Vec::new(),
    });
    assert_agent_panel_visible(&mut app, false);
}

#[test]
fn composer_grows_with_text_rows_and_reclaims_conversation() {
    let area = Rect::new(0, 0, 80, 24);
    let single = terminal_layout_with_tree_rows(area, 3, 0, false, 1);
    assert_eq!(single.input.height, 3);
    let grown = terminal_layout_with_tree_rows(area, 3, 0, false, 4);
    assert_eq!(grown.input.height, 6);
    // Every added composer row comes out of the conversation pane; the
    // agent panel, status line, and bar keep their geometry.
    assert_eq!(
        single.conversation.height - grown.conversation.height,
        grown.input.height - single.input.height
    );
    assert_eq!(grown.agent, single.agent);
    assert_eq!(grown.bar, single.bar);
    // The ceiling is five text rows plus borders, and the box stays
    // glued to the bar above it.
    let ceiling = terminal_layout_with_tree_rows(area, 3, 0, false, 99);
    assert_eq!(ceiling.input.height, 7);
    assert_eq!(ceiling.input.y + ceiling.input.height, ceiling.bar.y);
}
