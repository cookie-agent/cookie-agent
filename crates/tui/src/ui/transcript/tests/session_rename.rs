//! Renaming sessions: `/rename` with a title, `--auto`, or nothing, and
//! Ctrl+R in the session picker.

use std::sync::{Arc, Mutex};

use cookie_agent_protocol::SessionId;
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

async fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        app.handle_key(key(KeyCode::Char(character))).await;
    }
}

fn renames(recorded: &Arc<Mutex<Vec<Value>>>) -> Vec<Value> {
    recorded
        .lock()
        .expect("recorded")
        .iter()
        .filter(|value| value["method"].as_str() == Some("session.rename"))
        .map(|value| value["params"].clone())
        .collect()
}

/// Two sessions, the first viewed, and a daemon that renames whatever it
/// is asked to: a title it sets, "generated title" for a reset.
async fn rename_app() -> (App, SessionId, SessionId, Arc<Mutex<Vec<Value>>>) {
    let mut app = test_app().await;
    let viewed = SessionId::new_v7();
    let other = SessionId::new_v7();
    app.sessions = vec![
        titled_meta(viewed, "viewed session", 1),
        titled_meta(other, "other session", 1),
    ];
    app.selected = Some(viewed);
    app.tree_root = Some(viewed);
    let (client, recorded) = answering_client(move |method, params| match method {
        "session.rename" => {
            let session_id = params["session_id"].as_str().unwrap().parse().unwrap();
            let title = params["change"]["title"]
                .as_str()
                .unwrap_or("generated title");
            Ok(json!({
                "client_rename_id": params["client_rename_id"],
                "session": serde_json::to_value(titled_meta(session_id, title, 2)).unwrap(),
            }))
        }
        "session.list" => Ok(json!({ "sessions": [] })),
        _ => Err(json!({"code": -32601, "message": "not scripted"})),
    });
    app.client = client;
    (app, viewed, other, recorded)
}

async fn next_update(app: &mut App) {
    let update = tokio::time::timeout(std::time::Duration::from_secs(5), app.rpc_updates_rx.recv())
        .await
        .expect("rpc update timeout")
        .expect("rpc update");
    app.handle_rpc_update(update);
}

/// Waits for the rename the app sent to come back.
async fn renamed(app: &mut App) {
    loop {
        next_update(app).await;
        if !app.status.starts_with("Renaming") {
            break;
        }
    }
}

fn title_of(app: &App, session_id: SessionId) -> Option<String> {
    app.sessions
        .iter()
        .find(|session| session.session_id == session_id)
        .and_then(|session| session.title.as_ref().map(ToString::to_string))
}

#[tokio::test]
async fn rename_with_a_title_sets_it_and_auto_resets_it() {
    let (mut app, viewed, _, recorded) = rename_app().await;
    app.open_command_palette();
    type_text(&mut app, "rename Fix The Parser ").await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert!(!app.command_palette_visible());
    renamed(&mut app).await;
    let sent = renames(&recorded);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["session_id"], viewed.to_string());
    assert_eq!(
        sent[0]["change"],
        json!({"type": "set", "title": "Fix The Parser"})
    );
    assert_eq!(title_of(&app, viewed).as_deref(), Some("Fix The Parser"));
    assert_eq!(app.status, "Renamed the session to “Fix The Parser”.");

    app.open_command_palette();
    type_text(&mut app, "rename --auto").await;
    app.handle_key(key(KeyCode::Enter)).await;
    renamed(&mut app).await;
    let sent = renames(&recorded);
    assert_eq!(sent[1]["change"], json!({"type": "reset"}));
    // Each rename is its own request.
    assert_ne!(sent[0]["client_rename_id"], sent[1]["client_rename_id"]);
    assert_eq!(title_of(&app, viewed).as_deref(), Some("generated title"));
}

#[tokio::test]
async fn rename_alone_asks_starting_from_the_current_title() {
    let (mut app, viewed, _, recorded) = rename_app().await;
    app.open_command_palette();
    type_text(&mut app, "rename").await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert!(app.command_palette_visible());
    let frame = frame_rows(&mut app, 100, 30).join("\n");
    assert!(frame.contains("Rename session"), "{frame}");
    assert!(frame.contains("viewed session"), "{frame}");
    assert!(frame.contains("empty enter: automatic title"), "{frame}");
    assert!(renames(&recorded).is_empty());

    // The caret is at the end: typing extends the title.
    type_text(&mut app, " v2").await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert!(!app.command_palette_visible());
    renamed(&mut app).await;
    assert_eq!(
        renames(&recorded)[0]["change"],
        json!({"type": "set", "title": "viewed session v2"})
    );
    assert_eq!(title_of(&app, viewed).as_deref(), Some("viewed session v2"));
}

#[tokio::test]
async fn a_title_the_daemon_would_refuse_stays_to_be_fixed() {
    let (mut app, _, _, recorded) = rename_app().await;
    app.open_command_palette();
    type_text(&mut app, "rename").await;
    app.handle_key(key(KeyCode::Enter)).await;
    type_text(&mut app, &"x".repeat(600)).await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert!(app.command_palette_visible(), "the field stays open");
    assert_eq!(app.status, "A title is at most 512 bytes.");
    assert!(renames(&recorded).is_empty());
}

#[tokio::test]
async fn a_new_session_draft_has_nothing_to_rename() {
    let (mut app, _, _, recorded) = rename_app().await;
    app.selected = None;
    app.open_command_palette();
    type_text(&mut app, "rename Later").await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert_eq!(app.status, "Select a session to rename.");
    assert!(renames(&recorded).is_empty());
}

#[tokio::test]
async fn ctrl_r_in_the_picker_renames_the_highlighted_session() {
    let (mut app, _, other, recorded) = rename_app().await;
    app.modal = Modal::Sessions;
    frame_rows(&mut app, 100, 30);
    let index = app
        .session_search_ids()
        .iter()
        .position(|id| *id == other)
        .expect("listed");
    app.picker_state.select(Some(index));

    app.handle_key(ctrl('r')).await;
    let prompt = app.session_rename.as_ref().expect("a title field");
    assert_eq!(prompt.session_id, other);
    assert_eq!(prompt.input.as_str(), "other session");
    let frame = frame_rows(&mut app, 100, 30).join("\n");
    assert!(frame.contains("Rename session"), "{frame}");
    // The field owns the picker: its rows and buttons do nothing.
    assert!(app.pointer_target_at(10, 10).is_none());

    // Esc goes back to the picker without renaming.
    app.handle_key(key(KeyCode::Esc)).await;
    assert!(app.session_rename.is_none());
    assert_eq!(app.modal, Modal::Sessions);
    assert!(renames(&recorded).is_empty());

    // An emptied field goes back to the automatic title.
    app.handle_key(ctrl('r')).await;
    app.handle_key(ctrl('u')).await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert!(app.session_rename.is_none());
    assert_eq!(app.modal, Modal::Sessions);
    renamed(&mut app).await;
    let sent = renames(&recorded);
    assert_eq!(sent[0]["session_id"], other.to_string());
    assert_eq!(sent[0]["change"], json!({"type": "reset"}));
    assert_eq!(app.status, "The session uses its automatic title again.");
}

#[tokio::test]
async fn the_pickers_rename_button_opens_the_field() {
    let (mut app, viewed, _, _) = rename_app().await;
    app.modal = Modal::Sessions;
    frame_rows(&mut app, 100, 30);
    app.picker_state.select(Some(0));
    let (rect, _) = app
        .hit_map
        .session_picker_actions
        .iter()
        .copied()
        .find(|(_, action)| *action == SessionPickerAction::Rename)
        .expect("a Rename button");
    assert_eq!(
        app.hover_target_at(rect.x, rect.y),
        Some(PointerTarget::SessionPickerAction(
            SessionPickerAction::Rename
        ))
    );
    app.handle_click(rect.x, rect.y).await;
    let first = app.session_search_ids()[0];
    assert!(app.session_search_ids().contains(&viewed));
    let prompt = app.session_rename.as_ref().expect("a title field");
    assert_eq!(prompt.session_id, first);
}
