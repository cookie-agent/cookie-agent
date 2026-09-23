use crate::ui::transcript::*;

use cookie_agent_protocol::SessionId;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::AssistantChild;

use crate::ui::app::*;

use super::support::*;

#[tokio::test]
async fn user_message_click_opens_the_menu_only_on_user_rows() {
    let (mut app, session, _) = app_with_user_messages().await;
    // A thinking block gives an assistant-owned row with a real click
    // action of its own; it must never open the message menu.
    app.store
        .sessions
        .get_mut(&session)
        .expect("session")
        .transcript
        .push(crate::state::TranscriptItem::Assistant {
            id: 9,
            version: 0,
            attribution: attribution(None),
            committed_turn_seq: Some(1),
            children: vec![AssistantChild::Thinking {
                id: 1,
                version: 0,
                text: "thought".into(),
            }],
        });
    rendered_frame(&mut app, 80, 24);
    let hit = user_hit(&app, 1);
    app.handle_click(hit.rect.x + 2, hit.rect.y).await;
    assert_eq!(app.modal, Modal::UserMessage);
    let menu = app.user_menu.as_ref().expect("menu state");
    assert_eq!(menu.seq, 1);
    assert_eq!(menu.text, "first question");
    // The menu does not open from assistant/tool rows; they keep their
    // expand/collapse toggle. (The menu closes first so its overlay
    // does not swallow the click.)
    app.modal = Modal::None;
    app.user_menu = None;
    let block = app.hit_map.blocks.first().copied().expect("block hit");
    app.handle_click(block.rect.x, block.rect.y).await;
    assert_eq!(app.modal, Modal::None);
    assert!(app.user_menu.is_none());
    assert!(
        app.expanded_blocks
            .get(&session)
            .is_some_and(|set| set.contains(&block.id)),
        "the block kept its toggle"
    );
}

#[tokio::test]
async fn menu_copy_captures_the_message_text() {
    let (mut app, _, copied) = app_with_user_messages().await;
    rendered_frame(&mut app, 80, 24);
    let hit = user_hit(&app, 2);
    app.handle_click(hit.rect.x + 2, hit.rect.y).await;
    assert_eq!(app.modal, Modal::UserMessage);
    // Copy is the first row: Enter activates the keyboard selection.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::None);
    assert_eq!(
        copied.lock().expect("capture").as_slice(),
        ["second question"]
    );
}

#[tokio::test]
async fn menu_revert_is_confirm_guarded_and_targets_seq_minus_one() {
    let (mut app, session, _) = app_with_user_messages().await;
    let (client, recorded, _incoming) = live_recording_client();
    app.client = client;
    rendered_frame(&mut app, 80, 24);
    let hit = user_hit(&app, 2);
    app.handle_click(hit.rect.x + 2, hit.rect.y).await;
    // Choosing revert opens the confirm guard; Esc backs out to the
    // menu without any RPC.
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::RevertConfirm);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::UserMessage);
    assert_eq!(recorded_method_count(&recorded, "session.revert"), 0);
    // Confirming dispatches with through_seq = seq - 1: the message
    // itself leaves the visible branch.
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::None);
    let id = wait_for_recorded_request(&recorded, "session.revert", 1).await;
    let request = recorded
        .lock()
        .expect("recorded")
        .iter()
        .find(|value| value["id"].as_i64() == Some(id))
        .cloned()
        .expect("revert request");
    assert_eq!(
        request["params"]["session_id"].as_str(),
        Some(session.to_string().as_str())
    );
    assert_eq!(request["params"]["through_seq"].as_u64(), Some(1));
}

#[tokio::test]
async fn menu_fork_targets_the_message_seq_and_switches_sessions() {
    let (mut app, session, _) = app_with_user_messages().await;
    let (client, recorded, _incoming) = live_recording_client();
    app.client = client;
    rendered_frame(&mut app, 80, 24);
    let hit = user_hit(&app, 2);
    app.handle_click(hit.rect.x + 2, hit.rect.y).await;
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::None);
    let id = wait_for_recorded_request(&recorded, "session.fork", 1).await;
    let request = recorded
        .lock()
        .expect("recorded")
        .iter()
        .find(|value| value["id"].as_i64() == Some(id))
        .cloned()
        .expect("fork request");
    assert_eq!(
        request["params"]["session_id"].as_str(),
        Some(session.to_string().as_str())
    );
    // Fork keeps the message in the copied prefix: through_seq = seq.
    assert_eq!(request["params"]["through_seq"].as_u64(), Some(2));
    // The committed fork switches the viewed session.
    let forked = SessionId::new_v7();
    app.handle_rpc_update(RpcUpdate::Forked { forked });
    assert_eq!(app.selected, Some(forked));
}

#[tokio::test]
async fn reverted_update_restores_the_message_text_into_the_composer() {
    let (mut app, session, _) = app_with_user_messages().await;
    app.handle_rpc_update(RpcUpdate::Reverted {
        session_id: session,
        text: "second question".into(),
    });
    assert_eq!(app.input.as_str(), "second question");
    assert!(app.composer_focused());
}

#[test]
fn user_message_hit_rects_clip_and_shift_like_block_hits() {
    let region = UserRegion {
        seq: 7,
        start_line: 10,
        end_line: 20,
    };
    let viewport = Rect::new(0, 0, 40, 5);
    let hit = user_message_hit(region, viewport, 8).expect("hit");
    assert_eq!(hit.rect.y, 2);
    assert_eq!(hit.rect.height, 3);
    assert_eq!(hit.seq, 7);
    assert!(user_message_hit(region, viewport, 25).is_none());
}
