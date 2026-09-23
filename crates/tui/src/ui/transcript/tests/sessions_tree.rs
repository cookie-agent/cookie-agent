use crate::ui::transcript::*;

use cookie_agent_protocol::{
    EventPayload, ModelSelection, RunSelection, SafeErrorMessage, SessionId, SessionStatus,
    SessionTitle, SessionTree,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use ratatui::{Terminal, backend::TestBackend, style::Modifier};

use crate::Client;

use crate::client::ClientDelivery;

use crate::ui::app::*;

use crate::ui::pickers::SearchPickerFocus;

use crate::ui::slash::SlashCommand;

use super::support::*;

#[tokio::test]
async fn session_picker_rows_invalidate_on_title_patch() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.sessions.push(titled_meta(session, "before title", 1));
    let labels = |app: &mut App| -> Vec<String> {
        app.current_session_search_rows()
            .iter()
            .filter_map(|row| match row {
                crate::ui::pickers::SessionSearchRow::Session { label, .. } => Some(label.clone()),
                _ => None,
            })
            .collect()
    };
    let before = labels(&mut app);
    assert!(
        before.iter().any(|label| label.contains("before title")),
        "cached rows contain the initial title: {before:?}"
    );

    app.apply_title_patch(
        session,
        Some(SessionTitle::new("after title").expect("title")),
        2,
    );

    let after = labels(&mut app);
    assert!(
        after.iter().any(|label| label.contains("after title")),
        "title patch rebuilds the cached rows: {after:?}"
    );
    assert!(
        !after.iter().any(|label| label.contains("before title")),
        "stale title is gone from the rebuilt rows: {after:?}"
    );
}

#[tokio::test]
async fn title_events_patch_tree_rows_immediately_and_stale_tree_cannot_overwrite() {
    let mut app = test_app().await;
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.tree_root = Some(root);
    app.selected = Some(root);
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: session_meta(child),
            children: Vec::new(),
        }],
    });
    // Immediate patch from the title event.
    app.apply_title_patch(
        child,
        Some(SessionTitle::new("worker done").expect("title")),
        7,
    );
    let entries = app.tree_entries();
    assert!(entries.iter().any(|(id, meta, _)| {
        *id == child
            && meta
                .title
                .as_ref()
                .is_some_and(|t| t.as_str() == "worker done")
    }));
    assert_eq!(app.title_sequences[&child], 7);

    // A stale tree response (title seq 3 < 7) must not overwrite.
    let mut stale = SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: titled_meta(child, "old title", 3),
            children: Vec::new(),
        }],
    };
    app.patch_tree_titles(&mut stale);
    assert_eq!(stale.children[0].session.title_updated_seq, 7);
    assert_eq!(
        stale.children[0]
            .session
            .title
            .as_ref()
            .map(|title| title.as_str()),
        Some("worker done")
    );

    // An older event never patches over a newer one.
    app.apply_title_patch(
        child,
        Some(SessionTitle::new("regression").expect("title")),
        5,
    );
    let entries = app.tree_entries();
    assert!(entries.iter().any(|(id, meta, _)| {
        *id == child
            && meta
                .title
                .as_ref()
                .is_some_and(|t| t.as_str() == "worker done")
    }));

    // A user reset clears the title at a newer sequence.
    app.apply_title_patch(child, None, 9);
    let entries = app.tree_entries();
    assert!(
        entries.iter().any(|(id, meta, _)| *id == child
            && meta.title.is_none()
            && meta.title_updated_seq == 9)
    );
    let _ = root;
}

#[tokio::test]
async fn agent_tree_rows_are_agent_colon_title() {
    let mut app = test_app().await;
    let root = SessionId::new_v7();
    app.tree = Some(SessionTree {
        session: titled_meta(root, "fix the flaky test", 2),
        children: Vec::new(),
    });
    app.selected = Some(root);
    let entries = app.tree_entries();
    // Primary text is exactly `agent-id:session-title` with no session
    // ID; cursor/watch markers live in prefix cells only.
    let label = app.tree_row_label(&entries[0], false);
    assert_eq!(label, "  ●    primary:fix the flaky test");
    let label = app.tree_row_label(&entries[0], true);
    assert_eq!(label, "> ●    primary:fix the flaky test");
    let root_id = root.to_string();
    assert!(!label.contains(&root_id));
    assert!(!label.contains(&root_id[..8]));
    // Untitled sessions render the exact untitled placeholder.
    let untitled = SessionId::new_v7();
    app.tree = Some(SessionTree {
        session: session_meta(untitled),
        children: Vec::new(),
    });
    let entries = app.tree_entries();
    assert_eq!(
        app.tree_row_label(&entries[0], false),
        "       primary:untitled"
    );
}

#[tokio::test]
async fn watched_tree_row_keeps_its_glyph_but_never_a_color_marker() {
    let mut app = test_app().await;
    // Pin the default true-color theme so the assertions do not depend
    // on the developer's ambient tui.toml or terminal detection.
    app.theme = Theme::default();
    let meta = |id: SessionId, title: &str| {
        let mut meta = titled_meta(id, title, 1);
        // Space-padded statuses keep every glyph single-width: ratatui
        // resets the continuation cell after a wide emoji, which would
        // otherwise break the cell-for-cell comparison below.
        meta.status = SessionStatus::Failed;
        meta
    };
    let watched = SessionId::new_v7();
    let other = SessionId::new_v7();
    let sibling = SessionId::new_v7();
    let mut other_meta = meta(other, "cursor child");
    other_meta.status = SessionStatus::Running;
    app.selected = Some(watched);
    app.tree_root = Some(watched);
    app.tree = Some(SessionTree {
        session: meta(watched, "watched root"),
        children: vec![
            SessionTree {
                session: other_meta,
                children: Vec::new(),
            },
            SessionTree {
                session: meta(sibling, "plain sibling"),
                children: Vec::new(),
            },
        ],
    });
    // Park the keyboard cursor on the first child so the watched row
    // shows exactly its own styling, not the cursor's.
    app.tree_cursor = Some(other);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("app render");
    let buffer = terminal.backend().buffer();
    assert_eq!(app.hit_map.tree_rows.len(), 3);
    let watched_row = app.hit_map.tree_rows[0].rect;
    let cursor_row = app.hit_map.tree_rows[1].rect;
    let reference_row = app.hit_map.tree_rows[2].rect;

    // The `●` glyph remains the only "current session" marker…
    let entries = app.tree_entries();
    assert!(app.tree_row_label(&entries[0], false).contains("● "));
    // …while the row renders exactly like every other plain row:
    // cell-for-cell identical styles (no toasted selection band, no
    // user-role color), and none of the bold/reverse modifiers the
    // removed accents carried.
    for x in watched_row.x..watched_row.x.saturating_add(watched_row.width) {
        let cell = buffer[(x, watched_row.y)].style();
        let reference = buffer[(x, reference_row.y)].style();
        assert_eq!(cell, reference, "same as every other row: {cell:?}");
        assert!(
            !cell.add_modifier.contains(Modifier::BOLD),
            "no bold accent: {cell:?}"
        );
        assert!(
            !cell.add_modifier.contains(Modifier::REVERSED),
            "no reverse accent: {cell:?}"
        );
    }
    // The keyboard cursor row keeps its assistant accent — keyboard
    // selection is a separate, intentional highlight.
    let cursor_cell = buffer[(cursor_row.x, cursor_row.y)].style();
    assert_eq!(
        cursor_cell.fg,
        app.theme.assistant().fg,
        "cursor accent: {cursor_cell:?}"
    );
}

#[tokio::test]
async fn session_search_filters_titles_and_untitled_placeholder() {
    let mut app = test_app().await;
    let first = titled_meta(SessionId::new_v7(), "quarterly report", 1);
    let second = session_meta(SessionId::new_v7());
    app.sessions = vec![first, second];
    assert_eq!(
        app.current_session_search_rows()
            .iter()
            .filter(|row| row.session_id().is_some())
            .count(),
        2
    );
    app.session_search
        .input_mut()
        .set_buffer("quarterly".into());
    assert_eq!(
        app.current_session_search_rows()
            .iter()
            .filter(|row| row.session_id().is_some())
            .count(),
        1
    );
    app.session_search.input_mut().set_buffer("untitled".into());
    assert_eq!(
        app.current_session_search_rows()
            .iter()
            .filter(|row| row.session_id().is_some())
            .count(),
        1
    );
    app.session_search.input_mut().set_buffer("primary".into());
    assert!(
        app.current_session_search_rows()
            .iter()
            .all(|row| row.session_id().is_none())
    );
}

#[tokio::test]
async fn session_picker_lists_only_root_sessions() {
    let mut app = test_app().await;
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    // Watching a delegated child keeps its metadata in the general session
    // cache (titles, statuses, permission-mode rooting all read it), but
    // the picker selects a session tree so the child must not resurface.
    let mut delegated = delegated_meta(child, root, "worker");
    delegated.title =
        Some(cookie_agent_protocol::SessionTitle::new("delegated leaf").expect("title"));
    app.sessions = vec![session_meta(root), delegated];

    let ids = app
        .current_session_search_rows()
        .iter()
        .filter_map(|row| row.session_id())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![root]);
    assert_eq!(app.picker_sessions().count(), 1);

    app.modal = Modal::Sessions;
    let frame = rendered_frame(&mut app, 80, 24);
    assert!(frame.contains("Sessions (1/1)"), "picker header: {frame}");
    assert!(
        frame.contains("untitled"),
        "the root row is missing: {frame}"
    );
    assert!(
        !frame.contains("delegated leaf"),
        "delegated child leaked into the picker: {frame}"
    );
}

#[tokio::test]
async fn session_search_headers_are_not_clickable_and_click_reroots() {
    let mut app = test_app().await;
    let first = SessionId::new_v7();
    let second = SessionId::new_v7();
    app.sessions = vec![
        titled_meta(first, "first session", 1),
        titled_meta(second, "second session", 1),
    ];
    app.modal = Modal::Sessions;
    frame_rows(&mut app, 100, 30);
    assert_eq!(app.hit_map.picker_rows.len(), 2);
    let picker = app.hit_map.picker.expect("picker");
    app.handle_wheel(picker.x + 1, picker.y + 1, false);
    assert_eq!(app.session_search.focus(), SearchPickerFocus::List);
    assert_eq!(app.picker_state.selected(), Some(1));
    let header_y = app.hit_map.picker_rows[0].rect.y.saturating_sub(1);
    let picker_x = picker.x + 1;
    app.handle_click(picker_x, header_y).await;
    assert_eq!(app.modal, Modal::Sessions);
    let hit = app.hit_map.picker_rows[1].rect;
    app.handle_click(hit.x, hit.y).await;
    assert_eq!(app.modal, Modal::None);
    assert_eq!(app.tree_root, Some(second));
}

#[tokio::test]
async fn session_search_enter_selects_and_live_title_patch_updates_open_overlay() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.sessions = vec![titled_meta(session, "before rename", 1)];
    app.modal = Modal::Sessions;
    let before = frame_rows(&mut app, 100, 30).join("\n");
    assert!(before.contains("before rename"));
    app.apply_title_patch(
        session,
        Some(SessionTitle::new("after rename").expect("title")),
        2,
    );
    let after = frame_rows(&mut app, 100, 30).join("\n");
    assert!(after.contains("after rename"));
    assert!(!after.contains("before rename"));

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.session_search.focus(), SearchPickerFocus::List);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::None);
    assert_eq!(app.tree_root, Some(session));
}

#[tokio::test]
async fn agent_tree_status_icons_keep_row_hit_geometry_intact() {
    let mut app = test_app().await;
    let statuses = [
        (SessionStatus::Running, "⏳ "),
        (SessionStatus::Idle, "   "),
        (SessionStatus::Completed, "   "),
        (SessionStatus::Failed, "   "),
        (SessionStatus::Cancelled, "   "),
        (SessionStatus::Interrupted, "   "),
    ];
    let root = SessionId::new_v7();
    let mut root_meta = titled_meta(root, "root", 1);
    root_meta.status = statuses[0].0;
    app.tree = Some(SessionTree {
        session: root_meta,
        children: statuses[1..]
            .iter()
            .enumerate()
            .map(|(index, (status, _))| {
                let mut meta = titled_meta(SessionId::new_v7(), &format!("child {index}"), 1);
                meta.status = *status;
                SessionTree {
                    session: meta,
                    children: Vec::new(),
                }
            })
            .collect(),
    });
    let entries = app.tree_entries();
    for (entry, (_, icon)) in entries.iter().zip(statuses) {
        let label = app.tree_row_label(entry, false);
        assert!(label.contains(&format!("{icon}primary:")));
        if icon == "   " {
            assert!(!label.contains(['✅', '⏳', '✓', '✗']));
        }
        let mut running = entry.clone();
        running.1.status = SessionStatus::Running;
        let running_label = app.tree_row_label(&running, false);
        assert_eq!(
            UnicodeWidthStr::width(label.split_once("primary:").unwrap().0),
            UnicodeWidthStr::width(running_label.split_once("primary:").unwrap().0),
            "status changes must not move the agent name"
        );
    }
    frame_rows(&mut app, 80, 30);
    // The six rows exceed the viewport cap, so the hit map covers exactly
    // the visible window and the tail stays reachable by scrolling.
    assert_eq!(
        app.hit_map.tree_rows.len(),
        statuses.len().min(crate::ui::MAX_AGENT_PANEL_ROWS)
    );
    assert!(
        app.hit_map
            .tree_rows
            .iter()
            .all(|hit| hit.rect.width == app.hit_map.tree.expect("tree rect").width)
    );
    app.run_command(SlashCommand::ShowAgentPanel).await;
    for width in [20, 80] {
        frame_rows(&mut app, width, 30);
        let hits = app
            .hit_map
            .tree_rows
            .iter()
            .map(|hit| (hit.session_id, hit.rect))
            .collect::<Vec<_>>();
        for status in [
            SessionStatus::Running,
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Cancelled,
            SessionStatus::Interrupted,
        ] {
            let tree = app.tree.as_mut().unwrap();
            tree.session.status = status;
            for child in &mut tree.children {
                child.session.status = status;
            }
            frame_rows(&mut app, width, 30);
            assert_eq!(
                app.hit_map
                    .tree_rows
                    .iter()
                    .map(|hit| (hit.session_id, hit.rect))
                    .collect::<Vec<_>>(),
                hits
            );
        }
    }
}

#[tokio::test]
async fn run_lifecycle_events_patch_watched_and_background_panel_statuses_without_tree_rpc() {
    let (client, requests) = recording_client();
    let mut app = App::new(client).await.expect("test app");
    let watched = SessionId::new_v7();
    let background = SessionId::new_v7();
    app.selected = Some(watched);
    app.tree_root = Some(watched);
    app.sessions = vec![session_meta(watched), session_meta(background)];
    app.tree = Some(SessionTree {
        session: session_meta(watched),
        children: vec![SessionTree {
            session: session_meta(background),
            children: Vec::new(),
        }],
    });
    assert!(app.store.apply_event(session_created(watched, 1)));
    assert!(app.store.apply_event(session_created(background, 1)));
    requests.lock().expect("requests lock").clear();

    for session_id in [watched, background] {
        let run = run_id();
        app.handle_delivery(ClientDelivery::Live {
            message: Box::new(cookie_agent_protocol::EventSubscriptionMessage::Event {
                event: Box::new(run_started_with_suffix(
                    session_id,
                    2,
                    run,
                    vec![resolved_model(None)],
                )),
            }),
            generation: 0,
        })
        .await;
        assert_eq!(
            app.sessions
                .iter()
                .find(|meta| meta.session_id == session_id)
                .expect("session list meta")
                .status,
            SessionStatus::Running
        );
        let running_entry = app
            .tree_entries()
            .into_iter()
            .find(|entry| entry.0 == session_id)
            .expect("tree meta");
        assert_eq!(running_entry.1.status, SessionStatus::Running);
        assert!(app.tree_row_label(&running_entry, false).contains("⏳ "));

        app.handle_delivery(ClientDelivery::Live {
            message: Box::new(cookie_agent_protocol::EventSubscriptionMessage::Event {
                event: Box::new(event(
                    session_id,
                    3,
                    run,
                    EventPayload::RunCompleted { final_text: None },
                )),
            }),
            generation: 0,
        })
        .await;
        assert_eq!(
            app.sessions
                .iter()
                .find(|meta| meta.session_id == session_id)
                .expect("session list meta")
                .status,
            SessionStatus::Completed
        );
        let completed_entry = app
            .tree_entries()
            .into_iter()
            .find(|entry| entry.0 == session_id)
            .expect("tree meta");
        assert_eq!(completed_entry.1.status, SessionStatus::Completed);
        let completed_label = app.tree_row_label(&completed_entry, false);
        assert!(!completed_label.contains(['✅', '⏳']));
        assert!(completed_label.contains("   primary:"));
    }

    let merged = app.merge_session_meta(session_meta(watched));
    assert_eq!(merged.last_event_seq, 3);
    assert_eq!(merged.status, SessionStatus::Completed);
    let mut stale_tree = SessionTree {
        session: session_meta(watched),
        children: vec![SessionTree {
            session: session_meta(background),
            children: Vec::new(),
        }],
    };
    app.patch_tree_titles(&mut stale_tree);
    assert_eq!(stale_tree.session.status, SessionStatus::Completed);
    assert_eq!(
        stale_tree.children[0].session.status,
        SessionStatus::Completed
    );

    tokio::task::yield_now().await;
    assert_eq!(recorded_method_count(&requests, "session.tree"), 0);
}

#[tokio::test]
async fn tree_panel_hides_empty_sessions_until_their_first_message_lands() {
    let mut app = test_app().await;
    let root = SessionId::new_v7();
    let worker = SessionId::new_v7();
    let ghost = SessionId::new_v7();
    let mut ghost_meta = session_meta(ghost);
    // Only `SessionCreated` in the log: no user message yet.
    ghost_meta.last_event_seq = 1;
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![
            SessionTree {
                session: session_meta(worker),
                children: Vec::new(),
            },
            SessionTree {
                session: ghost_meta,
                children: Vec::new(),
            },
        ],
    });

    // The ghost renders nowhere: flattened entries and click hit rows
    // skip it while content sessions keep their order.
    assert_eq!(
        app.tree_entries()
            .iter()
            .map(|(id, _, _)| *id)
            .collect::<Vec<_>>(),
        vec![root, worker]
    );
    frame_rows(&mut app, 100, 30);
    assert_eq!(
        app.hit_map
            .tree_rows
            .iter()
            .map(|hit| hit.session_id)
            .collect::<Vec<_>>(),
        vec![root, worker]
    );

    // Its first run event bumps the sequence and the row appears.
    app.apply_status_patch(ghost, SessionStatus::Running, 2);
    assert_eq!(
        app.tree_entries()
            .iter()
            .map(|(id, _, _)| *id)
            .collect::<Vec<_>>(),
        vec![root, worker, ghost]
    );

    // A root-only tree stays hidden, including when the root is a ghost.
    let mut solo = session_meta(root);
    solo.last_event_seq = 1;
    app.tree = Some(SessionTree {
        session: solo,
        children: Vec::new(),
    });
    assert!(app.tree_entries().is_empty());
    let rendered = frame_rows(&mut app, 100, 30).join("\n");
    assert!(!rendered.contains("Agents"));
    assert!(!rendered.contains("No sessions yet"));
}

#[tokio::test]
async fn tree_navigation_skips_hidden_sessions_and_heals_a_hidden_cursor() {
    let mut app = test_app().await;
    let root = SessionId::new_v7();
    let first = SessionId::new_v7();
    let ghost = SessionId::new_v7();
    let last = SessionId::new_v7();
    let mut ghost_meta = session_meta(ghost);
    ghost_meta.last_event_seq = 1;
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![
            SessionTree {
                session: session_meta(first),
                children: Vec::new(),
            },
            SessionTree {
                session: ghost_meta,
                children: Vec::new(),
            },
            SessionTree {
                session: session_meta(last),
                children: Vec::new(),
            },
        ],
    });

    // Navigation walks root → first → last and never lands on the ghost.
    app.tree_cursor = Some(root);
    app.move_tree_selection(false);
    assert_eq!(app.tree_cursor, Some(first));
    app.move_tree_selection(false);
    assert_eq!(app.tree_cursor, Some(last));
    app.move_tree_selection(false);
    assert_eq!(app.tree_cursor, Some(last));
    app.move_tree_selection(true);
    assert_eq!(app.tree_cursor, Some(first));

    // A cursor left pointing at a hidden session — the just-created
    // watched session before its first message — never navigates onto
    // the ghost and heals onto the first visible row at render.
    app.tree_cursor = Some(ghost);
    app.move_tree_selection(true);
    assert_eq!(app.tree_cursor, Some(root));
    app.tree_cursor = Some(ghost);
    frame_rows(&mut app, 100, 30);
    assert_eq!(app.tree_cursor, Some(root));
}

#[tokio::test]
async fn hidden_current_session_stays_fully_usable() {
    let mut app = test_app().await;
    let current = SessionId::new_v7();
    let mut meta = session_meta(current);
    meta.last_event_seq = 1;
    app.selected = Some(current);
    app.tree_root = Some(current);
    app.tree = Some(SessionTree {
        session: meta,
        children: Vec::new(),
    });
    app.tree_cursor = Some(current);
    app.draft = Some(RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: model_key(),
            variant: None,
        },
        preset: None,
    });

    let rendered = frame_rows(&mut app, 100, 30).join("\n");
    // The root-only panel stays hidden with no ghost row or hit region…
    assert!(app.tree_entries().is_empty());
    assert!(app.hit_map.tree_rows.is_empty());
    assert!(!rendered.contains("Agents"));
    assert!(!rendered.contains("No sessions yet"));
    // …while the composer and the Message title bar keep working.
    assert!(app.hit_map.input.is_some());
    assert!(!app.hit_map.title_segments.is_empty());
    assert_eq!(app.selected, Some(current));
}

#[test]
fn run_terminal_status_patches_match_engine_session_projection() {
    let session = SessionId::new_v7();
    let run = run_id();
    let cases = [
        (
            EventPayload::RunCompleted { final_text: None },
            SessionStatus::Completed,
        ),
        (
            EventPayload::RunFailed {
                error: SafeErrorMessage::new("failed").expect("error"),
                model_error: None,
                resolved_model: None,
            },
            SessionStatus::Failed,
        ),
        (
            EventPayload::RunCancelled { reason: None },
            SessionStatus::Cancelled,
        ),
        (
            EventPayload::RunInterrupted { reason: None },
            SessionStatus::Interrupted,
        ),
    ];
    for (payload, expected) in cases {
        assert_eq!(
            status_change_from_event(&event(session, 2, run, payload)),
            Some((session, expected, 2))
        );
    }
    assert_eq!(
        status_change_from_event(&runless_event(
            session,
            3,
            EventPayload::DelegateChildTerminated {
                status: SessionStatus::Cancelled,
                reason: None,
            },
        )),
        Some((session, SessionStatus::Cancelled, 3))
    );
}

#[tokio::test]
async fn runless_delegate_terminal_event_patches_live_session_and_tree_status() {
    let (client, _requests) = recording_client();
    let mut app = App::new(client).await.expect("test app");
    let child = SessionId::new_v7();
    app.sessions = vec![session_meta(child)];
    app.tree_root = Some(child);
    app.tree = Some(SessionTree {
        session: session_meta(child),
        children: Vec::new(),
    });
    assert!(app.store.apply_event(session_created(child, 1)));

    app.handle_delivery(live_event(runless_event(
        child,
        2,
        EventPayload::DelegateChildTerminated {
            status: SessionStatus::Cancelled,
            reason: None,
        },
    )))
    .await;

    assert_eq!(app.sessions[0].status, SessionStatus::Cancelled);
    assert_eq!(
        app.tree.as_ref().expect("tree").session.status,
        SessionStatus::Cancelled
    );
}

#[tokio::test]
async fn watching_a_descendant_keeps_the_stable_root() {
    let mut app = test_app().await;
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.tree_root = Some(root);
    app.selected = Some(root);
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: session_meta(child),
            children: Vec::new(),
        }],
    });
    app.owned_sessions.insert(root);
    app.watch_session(child);
    assert_eq!(app.tree_root, Some(root));
    assert_eq!(app.selected, Some(child));
    assert!(app.tree.is_some());
    assert!(app.read_only_sessions.contains(&child));
    assert!(!app.composer_focused());
    let child_generation = app.ownership_classifications[&child];
    app.handle_rpc_update(RpcUpdate::SessionOwnershipClassified {
        session_id: child,
        generation: child_generation,
        outcome: SessionOwnershipOutcome::Foreign,
    });
    assert!(app.read_only_sessions.contains(&child));
    assert!(rendered_frame(&mut app, 100, 30).contains("Read-only snapshot"));
    // Watching a session outside the tree is the intentional reroot.
    let outside = SessionId::new_v7();
    app.watch_session(outside);
    assert_eq!(app.tree_root, Some(outside));
    assert!(app.tree.is_none());
    assert!(app.read_only_sessions.contains(&outside));
    assert!(!app.composer_focused());
    let outside_generation = app.ownership_classifications[&outside];
    app.handle_rpc_update(RpcUpdate::SessionOwnershipClassified {
        session_id: outside,
        generation: outside_generation,
        outcome: SessionOwnershipOutcome::Owned(Box::new(session_meta(outside))),
    });
    assert!(app.owned_sessions.contains(&outside));
    assert!(!app.read_only_sessions.contains(&outside));
    assert!(app.composer_focused());
}

#[test]
fn ownership_classification_uses_rpc_code_instead_of_message_text() {
    let owned = crate::client::ClientError::Rpc(cookie_agent_protocol::JsonRpcError {
        code: SESSION_OWNED_BY_ANOTHER_PROCESS_CODE,
        message: "unrelated wording".into(),
        data: None,
    });
    let same_message = crate::client::ClientError::Rpc(cookie_agent_protocol::JsonRpcError {
        code: -32000,
        message: "session is owned by another cookie process".into(),
        data: None,
    });

    assert!(session_owned_by_another_process(&owned));
    assert!(!session_owned_by_another_process(&same_message));
}

#[test]
fn store_contention_retry_policy_is_exactly_once() {
    let contention = crate::client::ClientError::Rpc(cookie_agent_protocol::JsonRpcError {
        code: -32011,
        message: "provider connect error".into(),
        data: Some(serde_json::json!({"code": "lock_contention"})),
    });
    let other = crate::client::ClientError::Rpc(cookie_agent_protocol::JsonRpcError {
        code: -32011,
        message: "provider connect error".into(),
        data: Some(serde_json::json!({"code": "provider_store_write_failed"})),
    });

    assert!(retry_store_contention_once(0, &contention));
    assert!(!retry_store_contention_once(1, &contention));
    assert!(!retry_store_contention_once(0, &other));
    assert_eq!(
        store_contention_message(&contention, "providers"),
        "Another cookie-agent process is writing the providers — try again."
    );
    assert_eq!(
        store_contention_message(&other, "providers"),
        other.to_string()
    );
}

#[tokio::test]
async fn tree_children_use_their_own_ownership_classification() {
    let mut app = test_app().await;
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let grandchild = SessionId::new_v7();
    app.tree_root = Some(root);
    app.selected = Some(root);
    app.selection_generation = 7;
    app.tree_refresh_in_flight = Some((7, 11));
    app.read_only_sessions.insert(root);
    app.handle_rpc_update(RpcUpdate::Tree {
        session_id: root,
        generation: 7,
        request_id: 11,
        tree: Box::new(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: session_meta(child),
                children: vec![SessionTree {
                    session: session_meta(grandchild),
                    children: Vec::new(),
                }],
            }],
        }),
    });

    assert!(!app.read_only_sessions.contains(&child));
    assert!(!app.read_only_sessions.contains(&grandchild));
    app.watch_session(child);
    let child_generation = app.ownership_classifications[&child];
    app.handle_rpc_update(RpcUpdate::SessionOwnershipClassified {
        session_id: child,
        generation: child_generation,
        outcome: SessionOwnershipOutcome::Foreign,
    });
    assert!(app.read_only_sessions.contains(&child));

    app.watch_session(grandchild);
    let grandchild_generation = app.ownership_classifications[&grandchild];
    app.handle_rpc_update(RpcUpdate::SessionOwnershipClassified {
        session_id: grandchild,
        generation: grandchild_generation,
        outcome: SessionOwnershipOutcome::Owned(Box::new(session_meta(grandchild))),
    });
    assert_eq!(app.selected, Some(grandchild));
    assert!(app.owned_sessions.contains(&grandchild));
    assert!(!app.read_only_sessions.contains(&grandchild));
    assert!(app.composer_focused());
    assert!(rendered_frame(&mut app, 100, 30).contains("Type a message · / for commands"));
}

#[tokio::test]
async fn session_picker_selection_always_runs_per_session_classification() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.sessions = vec![session_meta(session)];
    app.modal = Modal::Sessions;
    app.choose_picker_entry(0).await;

    assert_eq!(app.selected, Some(session));
    assert!(app.read_only_sessions.contains(&session));
    assert!(!app.owned_sessions.contains(&session));
    assert!(!app.ownership_classifications.contains_key(&session));
}

#[tokio::test]
async fn successful_new_after_delivery_handoff_uses_event_loop_receiver() {
    let (_directory, server) = crate::tests::in_process_server();
    let client = Client::connect_in_process(server);
    client.handshake().await.expect("handshake");
    let mut app = App::new(client).await.expect("app");
    let mut deliveries = app.take_deliveries();

    app.run_command(SlashCommand::New).await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    // Agent selection only closes the picker. The first prompt performs
    // session.create, adopts the new root, and then submits run.start.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    type_input(&mut app, "first message").await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;

    let session_id = app.selected.expect("new session selected");
    assert!(app.owned_sessions.contains(&session_id));
    assert!(app.new_session_draft.is_none());
    assert!(!app.store.sessions.contains_key(&session_id));

    handle_detached_replay(&mut app, &mut deliveries, session_id).await;
    assert_eq!(app.store.sessions[&session_id].last_seq, 1);
    assert!(app.store.sessions[&session_id].creation_agent.is_some());
}

#[tokio::test]
async fn successful_sessions_picker_selection_after_delivery_handoff_uses_event_loop_receiver() {
    let (_directory, server) = crate::tests::in_process_server();
    let client = Client::connect_in_process(server);
    client.handshake().await.expect("handshake");
    let mut app = App::new(client.clone()).await.expect("app");
    let mut deliveries = app.take_deliveries();
    let session = client
        .create_session(cookie_agent_protocol::SessionCreateParams {
            selection: crate::tests::test_run_selection(),
        })
        .await
        .expect("create session")
        .session;
    app.refresh_lists().await;

    app.run_command(SlashCommand::Sessions).await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;

    assert_eq!(app.selected, Some(session.session_id));
    assert!(app.owned_sessions.contains(&session.session_id));
    assert!(!app.store.sessions.contains_key(&session.session_id));

    handle_detached_replay(&mut app, &mut deliveries, session.session_id).await;
    assert_eq!(app.store.sessions[&session.session_id].last_seq, 1);
    assert!(
        app.store.sessions[&session.session_id]
            .creation_agent
            .is_some()
    );
}

#[tokio::test]
async fn adopted_session_retries_live_subscription_after_snapshot_replay_ends() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.sessions = vec![session_meta(session)];
    app.read_only_sessions.insert(session);
    app.ownership_classifications.insert(session, 7);
    app.handle_delivery(ClientDelivery::ReplayStart {
        session_id: session,
        generation: 0,
        final_seq: 1,
        rebuild: true,
    })
    .await;
    app.handle_delivery(ClientDelivery::ReplayEvent {
        session_id: session,
        generation: 0,
        final_seq: 1,
        event: Box::new(session_created(session, 1)),
    })
    .await;
    app.handle_rpc_update(RpcUpdate::SessionOwnershipClassified {
        session_id: session,
        generation: 7,
        outcome: SessionOwnershipOutcome::Owned(Box::new(session_meta(session))),
    });
    assert!(app.owned_sessions.contains(&session));
    assert!(app.pending_live_subscriptions.contains(&session));
    let first_attempt = app.live_subscription_attempts[&session];
    app.handle_rpc_update(RpcUpdate::SessionLiveSubscriptionFinished {
        session_id: session,
        live_attempt: None,
        outcome: SessionLiveSubscriptionOutcome::Established,
    });
    assert!(app.pending_live_subscriptions.contains(&session));
    assert_eq!(app.live_subscription_attempts[&session], first_attempt);
    app.handle_rpc_update(RpcUpdate::SessionLiveSubscriptionFinished {
        session_id: session,
        live_attempt: Some(first_attempt),
        outcome: SessionLiveSubscriptionOutcome::ReplayInProgress,
    });
    assert!(app.pending_live_subscriptions.contains(&session));
    assert!(!app.live_subscription_attempts.contains_key(&session));

    app.handle_delivery(ClientDelivery::ReplayStart {
        session_id: session,
        generation: 1,
        final_seq: 1,
        rebuild: false,
    })
    .await;
    app.handle_delivery(ClientDelivery::ReplayEnd {
        session_id: session,
        generation: 0,
        final_seq: 1,
    })
    .await;
    assert_eq!(app.store.sessions[&session].last_seq, 1);
    let second_attempt = app.live_subscription_attempts[&session];
    assert!(!app.replay_ended_for_live_subscription.contains(&session));

    app.handle_rpc_update(RpcUpdate::SessionLiveSubscriptionFinished {
        session_id: session,
        live_attempt: Some(second_attempt),
        outcome: SessionLiveSubscriptionOutcome::ReplayInProgress,
    });
    assert!(app.pending_live_subscriptions.contains(&session));
    assert!(!app.live_subscription_attempts.contains_key(&session));

    app.handle_delivery(ClientDelivery::ReplayEnd {
        session_id: session,
        generation: 1,
        final_seq: 1,
    })
    .await;
    let third_attempt = app.live_subscription_attempts[&session];
    assert_ne!(third_attempt, second_attempt);
    assert!(!app.replay_ended_for_live_subscription.contains(&session));
    app.handle_rpc_update(RpcUpdate::SessionLiveSubscriptionFinished {
        session_id: session,
        live_attempt: None,
        outcome: SessionLiveSubscriptionOutcome::Established,
    });
    assert_eq!(app.live_subscription_attempts[&session], third_attempt);
    assert!(app.pending_live_subscriptions.contains(&session));
    app.handle_rpc_update(RpcUpdate::SessionLiveSubscriptionFinished {
        session_id: session,
        live_attempt: Some(third_attempt),
        outcome: SessionLiveSubscriptionOutcome::Established,
    });
    assert!(!app.pending_live_subscriptions.contains(&session));
    assert!(!app.live_subscription_attempts.contains_key(&session));

    let live = runless_event(
        session,
        2,
        EventPayload::PluginDiagnostic {
            plugin: "ownership-test".into(),
            kind: cookie_agent_protocol::PluginDiagnosticKind::HookBlocked,
            message: "live after adoption".into(),
            count: 1,
        },
    );
    app.handle_delivery(live_event(live.clone())).await;
    assert_eq!(app.store.sessions[&session].last_seq, 2);
    let transcript_len = app.store.sessions[&session].transcript.len();
    app.handle_delivery(live_event(live)).await;
    assert_eq!(app.store.sessions[&session].last_seq, 2);
    assert_eq!(
        app.store.sessions[&session].transcript.len(),
        transcript_len
    );
}

#[tokio::test]
async fn clicking_child_then_root_preserves_multilevel_tree_depth_and_hit_regions() {
    for width in [16, 40] {
        let mut app = test_app().await;
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        let grandchild = SessionId::new_v7();
        app.tree_root = Some(root);
        app.selected = Some(root);
        app.tree_cursor = Some(root);
        app.tree = Some(SessionTree {
            session: titled_meta(root, "root", 1),
            children: vec![SessionTree {
                session: titled_meta(child, "child", 1),
                children: vec![SessionTree {
                    session: titled_meta(grandchild, "grandchild", 1),
                    children: Vec::new(),
                }],
            }],
        });

        let expected = vec![(root, 0), (child, 1), (grandchild, 2)];
        let depths = |app: &App| {
            app.tree_entries()
                .into_iter()
                .map(|(session_id, _, depth)| (session_id, depth))
                .collect::<Vec<_>>()
        };
        let root_selected = rendered_agent_rows(&mut app, width);
        assert_eq!(depths(&app), expected, "width {width}");
        let child_hit = app
            .hit_map
            .tree_rows
            .iter()
            .find(|hit| hit.session_id == child)
            .copied()
            .expect("child row hit");
        let child_expand = child_hit.expand_rect.expect("child expand hit");

        app.handle_click(
            child_hit.rect.x + child_hit.rect.width - 1,
            child_hit.rect.y,
        )
        .await;
        let child_selected = rendered_agent_rows(&mut app, width);
        assert_eq!(app.selected, Some(child));
        assert_eq!(app.tree_root, Some(root));
        assert_eq!(depths(&app), expected, "width {width}");
        let selected_child_expand = app
            .hit_map
            .tree_rows
            .iter()
            .find(|hit| hit.session_id == child)
            .and_then(|hit| hit.expand_rect)
            .expect("selected child expand hit");
        assert_eq!(selected_child_expand, child_expand, "width {width}");

        app.apply_title_patch(
            grandchild,
            Some(SessionTitle::new("updated").expect("title")),
            2,
        );
        let root_hit = app
            .hit_map
            .tree_rows
            .iter()
            .find(|hit| hit.session_id == root)
            .copied()
            .expect("root row hit");
        app.handle_click(root_hit.rect.x + root_hit.rect.width - 1, root_hit.rect.y)
            .await;
        let root_selected_again = rendered_agent_rows(&mut app, width);

        assert_eq!(app.selected, Some(root));
        assert_eq!(app.tree_root, Some(root));
        assert_eq!(depths(&app), expected, "width {width}");
        assert!(
            root_selected[0].starts_with("> ●    "),
            "width {width}: {root_selected:?}"
        );
        assert!(root_selected[1].starts_with("  -      "));
        assert!(root_selected[2].starts_with("           "));
        assert!(child_selected[0].starts_with("       "));
        assert!(child_selected[1].starts_with("> - ●    "));
        assert!(child_selected[2].starts_with("           "));
        assert!(root_selected_again[1].starts_with("  -      "));
        assert!(root_selected_again[2].starts_with("           "));

        // These columns come from the actual rendered buffer. Selection
        // changes cursor/watch cells only; agent text retains depth 0/1/2.
        assert_eq!(text_column(&root_selected[0], "p"), 7);
        assert_eq!(text_column(&root_selected[1], "p"), 9);
        assert_eq!(text_column(&root_selected[2], "p"), 11);
        assert_eq!(text_column(&child_selected[0], "p"), 7);
        assert_eq!(text_column(&child_selected[1], "p"), 9);
        assert_eq!(text_column(&child_selected[2], "p"), 11);
        assert_eq!(text_column(&root_selected_again[0], "p"), 7);
        assert_eq!(text_column(&root_selected_again[1], "p"), 9);
        assert_eq!(text_column(&root_selected_again[2], "p"), 11);
    }
}
