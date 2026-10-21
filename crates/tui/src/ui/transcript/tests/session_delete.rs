//! `/sessions`: deleting the highlighted session behind a confirmation, and
//! Ctrl+A between this directory's sessions and every directory's.

use std::sync::{Arc, Mutex};

use cookie_agent_protocol::{SessionId, SessionsChangedNotification};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use crate::ui::app::*;

use super::support::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
}

fn requests(recorded: &Arc<Mutex<Vec<Value>>>, method: &str) -> Vec<Value> {
    recorded
        .lock()
        .expect("recorded")
        .iter()
        .filter(|value| value["method"].as_str() == Some(method))
        .map(|value| value["params"].clone())
        .collect()
}

/// A picker over two sessions, the first viewed, talking to a daemon of
/// client protocol `version` that deletes whatever it is asked to.
async fn picker_app(version: &str) -> (App, SessionId, SessionId, Arc<Mutex<Vec<Value>>>) {
    let mut app = test_app().await;
    let viewed = SessionId::new_v7();
    let other = SessionId::new_v7();
    app.sessions = vec![
        titled_meta(viewed, "viewed session", 1),
        titled_meta(other, "other session", 1),
    ];
    app.selected = Some(viewed);
    app.tree_root = Some(viewed);
    let version = version.to_owned();
    let (client, recorded) = answering_client(move |method, params| match method {
        "handshake" => Ok(json!({
            "protocol_version": version,
            "engine_version": "0.3.0",
            "origin": "client:tui-0192f3a45b6c7d8e9f00112233445566",
            "features": []
        })),
        "session.tree" => Ok(json!({ "tree": {
            "session": serde_json::to_value(titled_meta(
                params["session_id"].as_str().unwrap().parse().unwrap(),
                "tree",
                1,
            ))
            .unwrap(),
            "children": [{
                "session": serde_json::to_value(titled_meta(SessionId::new_v7(), "child", 1))
                    .unwrap(),
                "children": []
            }]
        }})),
        "session.delete" => Ok(json!({ "deleted": [params["session_id"]] })),
        "session.list" => Ok(json!({ "sessions": [] })),
        _ => Err(json!({"code": -32601, "message": "not scripted"})),
    });
    client.handshake().await.expect("handshake");
    app.client = client;
    app.modal = Modal::Sessions;
    frame_rows(&mut app, 100, 30);
    (app, viewed, other, recorded)
}

/// Highlights the picker row of `session`.
fn highlight(app: &mut App, session: SessionId) {
    let index = app
        .session_search_ids()
        .iter()
        .position(|id| *id == session)
        .expect("listed session");
    app.picker_state.select(Some(index));
}

async fn next_update(app: &mut App) {
    let update = tokio::time::timeout(std::time::Duration::from_secs(5), app.rpc_updates_rx.recv())
        .await
        .expect("rpc update timeout")
        .expect("rpc update");
    app.handle_rpc_update(update);
}

#[tokio::test]
async fn deleting_asks_first_and_sends_the_guard_the_picker_showed() {
    let (mut app, viewed, other, recorded) = picker_app("1.5.0").await;
    highlight(&mut app, other);
    let shown = app
        .sessions
        .iter()
        .find(|session| session.session_id == other)
        .expect("other")
        .last_event_seq;

    app.handle_key(ctrl('d')).await;
    let prompt = app
        .session_delete
        .clone()
        .expect("a delete waits for confirmation");
    assert_eq!(prompt.session.session_id, other);
    // The tree's size arrives on its own.
    next_update(&mut app).await;
    assert_eq!(app.session_delete.as_ref().unwrap().subagents, Some(1));
    let frame = frame_rows(&mut app, 100, 30).join("\n");
    assert!(frame.contains("Delete “other session”?"), "{frame}");
    assert!(
        frame.contains("Its 1 subagent session goes with it."),
        "{frame}"
    );

    // Esc keeps it, and nothing was sent.
    app.handle_key(key(KeyCode::Esc)).await;
    assert!(app.session_delete.is_none());
    assert_eq!(app.modal, Modal::Sessions);
    assert!(requests(&recorded, "session.delete").is_empty());

    // Delete in the list asks again; y deletes.
    app.session_search.focus_list();
    app.handle_key(key(KeyCode::Delete)).await;
    assert!(app.session_delete.is_some());
    app.handle_key(key(KeyCode::Char('y'))).await;
    while requests(&recorded, "session.delete").is_empty() {
        next_update(&mut app).await;
    }
    loop {
        next_update(&mut app).await;
        if app.status.starts_with("Deleted") {
            break;
        }
    }
    assert_eq!(
        requests(&recorded, "session.delete"),
        [json!({ "session_id": other, "expected_last_seq": shown })]
    );
    assert!(
        app.sessions
            .iter()
            .all(|session| session.session_id != other)
    );
    assert_eq!(
        app.selected,
        Some(viewed),
        "another session's delete leaves the view"
    );
}

#[tokio::test]
async fn a_daemon_older_than_1_5_is_never_asked_to_delete() {
    let (mut app, _, other, recorded) = picker_app("1.4.0").await;
    highlight(&mut app, other);
    app.handle_key(ctrl('d')).await;
    assert!(app.session_delete.is_none());
    assert_eq!(app.status, "The daemon is too old to delete sessions.");
    assert!(requests(&recorded, "session.delete").is_empty());
    assert!(requests(&recorded, "session.tree").is_empty());
}

#[tokio::test]
async fn the_buttons_confirm_and_cancel_like_their_keys() {
    let (mut app, _, other, _) = picker_app("1.5.0").await;
    highlight(&mut app, other);
    let (delete, _) = app
        .hit_map
        .session_picker_actions
        .iter()
        .copied()
        .find(|(_, action)| *action == SessionPickerAction::Delete)
        .expect("the delete button");
    app.handle_click(delete.x, delete.y).await;
    assert!(app.session_delete.is_some());
    frame_rows(&mut app, 100, 30);
    // While it waits, the rows behind it do not act.
    let row = app.hit_map.picker_rows[0].rect;
    assert_eq!(app.pointer_target_at(row.x, row.y), None);
    let (cancel, _) = app
        .hit_map
        .confirm_buttons
        .iter()
        .copied()
        .find(|(_, button)| *button == ConfirmButton::Cancel)
        .expect("cancel");
    app.handle_click(cancel.x, cancel.y).await;
    assert!(app.session_delete.is_none());
    assert_eq!(app.status, "Kept “other session”.");
}

#[tokio::test]
async fn ctrl_a_lists_every_directory_and_back() {
    let (mut app, _, _, recorded) = picker_app("1.5.0").await;
    app.cwd = Some("/workspace".into());
    assert!(
        frame_rows(&mut app, 100, 30)
            .join("\n")
            .contains("Sessions — this directory")
    );

    app.handle_key(ctrl('a')).await;
    assert!(app.session_scope_all);
    let listed = requests(&recorded, "session.list");
    assert_eq!(listed.last(), Some(&json!({})), "no directory filter");
    let frame = frame_rows(&mut app, 100, 30).join("\n");
    assert!(frame.contains("Sessions — all directories"), "{frame}");
    assert!(frame.contains("This directory (ctrl+a)"), "{frame}");

    let (toggle, _) = app
        .hit_map
        .session_picker_actions
        .iter()
        .copied()
        .find(|(_, action)| *action == SessionPickerAction::ToggleScope)
        .expect("the scope button");
    app.handle_click(toggle.x, toggle.y).await;
    assert!(!app.session_scope_all);
    assert_eq!(
        requests(&recorded, "session.list").last(),
        Some(&json!({ "cwd_identity": "/workspace" }))
    );
}

#[tokio::test]
async fn every_directory_rows_name_the_directory() {
    let (mut app, _, other, _) = picker_app("1.5.0").await;
    let mut elsewhere = titled_meta(SessionId::new_v7(), "elsewhere", 1);
    elsewhere.cwd_identity = cookie_agent_protocol::CwdIdentity::new("/srv/project").unwrap();
    app.sessions.push(elsewhere);
    app.session_scope_all = true;
    app.note_sessions_changed();
    let frame = frame_rows(&mut app, 100, 30).join("\n");
    assert!(frame.contains("· project)"), "{frame}");
    assert!(frame.contains("· workspace)"), "{frame}");
    let _ = other;
}

#[tokio::test]
async fn a_removed_viewed_session_is_left_for_a_new_session() {
    let (mut app, viewed, other, _) = picker_app("1.5.0").await;
    app.modal = Modal::None;
    app.apply_sessions_change(SessionsChangedNotification::Removed { session_id: viewed })
        .await;
    assert_eq!(app.selected, None);
    assert_eq!(app.tree_root, None);
    assert!(
        app.sessions
            .iter()
            .all(|session| session.session_id != viewed)
    );
    assert!(
        app.sessions
            .iter()
            .any(|session| session.session_id == other)
    );
    assert_eq!(app.status, "The session was deleted.");
}
