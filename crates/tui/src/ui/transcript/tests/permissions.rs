use cookie_agent_protocol::SessionId;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

use crate::ui::app::*;
use crate::ui::management::{PermissionFormFocus, sample_permissions};

use super::support::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// An app with `/permissions` open on the sample rules, selection on the
/// bash `git status*` agent rule, recording every request.
async fn app_with_permissions() -> (
    App,
    std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    impl Sized,
) {
    let mut app = test_app().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    app.selected = Some(SessionId::new_v7());
    app.modal = Modal::Permissions;
    app.permission_panel.install(sample_permissions());
    app.permission_panel.selection.select(Some(7));
    rendered_frame(&mut app, 120, 40);
    (app, recorded, incoming_guard)
}

#[tokio::test]
async fn arrows_step_the_effect_and_override_an_agent_rule() {
    let (mut app, recorded, incoming_guard) = app_with_permissions().await;
    app.handle_key(key(KeyCode::Right)).await;
    wait_for_method(&recorded, "session.permission.set", 1).await;
    let params = last_request_params(&recorded, "session.permission.set");
    assert_eq!(params["action"], "bash");
    assert_eq!(params["resource"], "git status*");
    assert_eq!(params["effect"], "ask");

    // read `*` is already allow, the leftmost effect: nothing to send.
    app.handle_key(key(KeyCode::Home)).await;
    app.handle_key(key(KeyCode::Left)).await;
    settle_recording().await;
    assert_eq!(
        recorded_method_count(&recorded, "session.permission.set"),
        1
    );
    drop(incoming_guard);
}

#[tokio::test]
async fn clicking_an_effect_segment_sets_it() {
    let (mut app, recorded, incoming_guard) = app_with_permissions().await;
    let (rect, _) = app
        .hit_map
        .permissions
        .effects
        .iter()
        .copied()
        .find(|(_, effect)| *effect == cookie_agent_protocol::PermissionEffect::Deny)
        .expect("deny segment");
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        rect.x,
        rect.y,
    ))
    .await;
    wait_for_method(&recorded, "session.permission.set", 1).await;
    assert_eq!(
        last_request_params(&recorded, "session.permission.set")["effect"],
        "deny"
    );
    drop(incoming_guard);
}

#[tokio::test]
async fn removing_an_agent_rule_explains_instead_of_failing_silently() {
    let (mut app, recorded, incoming_guard) = app_with_permissions().await;
    app.handle_key(key(KeyCode::Char('d'))).await;
    settle_recording().await;
    assert_eq!(
        recorded_method_count(&recorded, "session.permission.clear"),
        0
    );
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(
        rendered.contains("Only session rules can be removed"),
        "{rendered}"
    );

    // The write `docs/*` session rule clears.
    app.permission_panel.selection.select(Some(4));
    app.handle_key(key(KeyCode::Delete)).await;
    wait_for_method(&recorded, "session.permission.clear", 1).await;
    let params = last_request_params(&recorded, "session.permission.clear");
    assert_eq!(params["action"], "write");
    assert_eq!(params["resource"], "docs/*");
    drop(incoming_guard);
}

#[tokio::test]
async fn new_rule_form_validates_inline_and_adds_the_rule() {
    let (mut app, recorded, incoming_guard) = app_with_permissions().await;
    app.handle_key(key(KeyCode::Char('n'))).await;
    let form = app.permission_panel.form.as_ref().expect("form");
    // The form starts on the selected row's action, at the pattern.
    assert_eq!(form.action, cookie_agent_protocol::PermissionAction::Bash);
    assert_eq!(form.focus, PermissionFormFocus::Pattern);

    // An empty pattern is refused in the form, not the status line.
    app.handle_key(key(KeyCode::Enter)).await;
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(
        rendered.contains("enter a pattern, or * for everything"),
        "{rendered}"
    );

    type_input(&mut app, "git log*").await;
    // Effect: one step left from ask is allow.
    app.handle_key(key(KeyCode::Tab)).await;
    app.handle_key(key(KeyCode::Left)).await;
    // Action: back to write and forward to bash again.
    app.handle_key(key(KeyCode::Tab)).await;
    app.handle_key(key(KeyCode::Left)).await;
    app.handle_key(key(KeyCode::Right)).await;
    app.handle_key(key(KeyCode::Enter)).await;
    wait_for_method(&recorded, "session.permission.set", 1).await;
    let params = last_request_params(&recorded, "session.permission.set");
    assert_eq!(params["action"], "bash");
    assert_eq!(params["resource"], "git log*");
    assert_eq!(params["effect"], "allow");
    assert!(app.permission_panel.form.is_none());
    drop(incoming_guard);
}

#[tokio::test]
async fn enter_edits_a_session_rule_and_replaces_its_pattern() {
    let (mut app, recorded, incoming_guard) = app_with_permissions().await;
    app.permission_panel.selection.select(Some(4));
    app.handle_key(key(KeyCode::Enter)).await;
    let form = app.permission_panel.form.as_ref().expect("edit form");
    assert_eq!(form.pattern.as_str(), "docs/*");
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("Edit session rule"), "{rendered}");

    app.handle_key(key(KeyCode::End)).await;
    type_input(&mut app, "*").await;
    app.handle_key(key(KeyCode::Enter)).await;
    // The new rule is set first; the old one is cleared once that lands.
    wait_for_method(&recorded, "session.permission.set", 1).await;
    assert_eq!(
        last_request_params(&recorded, "session.permission.set")["resource"],
        "docs/**"
    );
    drop(incoming_guard);
}

#[tokio::test]
async fn new_rule_row_click_opens_the_form_and_m_cycles_the_mode() {
    let (mut app, recorded, incoming_guard) = app_with_permissions().await;
    let (rect, _) = *app.hit_map.permissions.rows.last().expect("new rule row");
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        rect.x + 2,
        rect.y,
    ))
    .await;
    assert!(app.permission_panel.form.is_some());
    app.handle_key(key(KeyCode::Esc)).await;
    assert!(app.permission_panel.form.is_none());
    assert_eq!(app.modal, Modal::Permissions);

    app.handle_key(key(KeyCode::Char('m'))).await;
    wait_for_method(&recorded, "session.set_permission_mode", 1).await;
    drop(incoming_guard);
}
