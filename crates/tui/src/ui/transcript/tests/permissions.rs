use cookie_agent_protocol::SessionId;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

use crate::ui::app::*;
use crate::ui::management::{PermissionFormFocus, sample_permissions};
use crate::ui::slash::SlashCommand;

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

/// Handle RPC updates until `done` holds.
async fn pump_until(app: &mut App, done: impl Fn(&App) -> bool) {
    for _ in 0..50 {
        if done(app) {
            return;
        }
        let update =
            tokio::time::timeout(std::time::Duration::from_secs(2), app.rpc_updates_rx.recv())
                .await
                .expect("rpc update timeout")
                .expect("rpc update");
        app.handle_rpc_update(update);
    }
    panic!("condition never held");
}

#[tokio::test]
async fn a_new_session_draft_holds_its_mode_and_rules_until_creation() {
    let (_directory, server) = crate::tests::in_process_server();
    let client = server.connect_in_process();
    client.handshake().await.expect("handshake");
    // An existing session stays selected behind the draft: nothing below
    // may touch it.
    let existing = client
        .create_session(cookie_agent_protocol::SessionCreateParams::new(
            crate::tests::test_run_selection(),
        ))
        .await
        .expect("existing session")
        .session
        .session_id;
    let mut app = App::new(client.clone()).await.expect("app");
    let _deliveries = app.take_deliveries();
    app.open_session(existing).await;
    app.run_command(SlashCommand::New).await;
    // The first Enter moves to the agent list, the second picks the agent.
    app.handle_key(key(KeyCode::Enter)).await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert_eq!(app.modal, Modal::None);
    assert!(app.new_session_draft.is_some());

    // Clicking the bottom-bar mode cycles the draft's mode.
    rendered_frame(&mut app, 120, 40);
    let mode = app.hit_map.permission_mode.expect("bottom-bar mode");
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        mode.x,
        mode.y,
    ))
    .await;
    assert_eq!(
        app.draft_permissions.mode,
        Some(cookie_agent_protocol::PermissionMode::AutoApproveN)
    );
    assert!(rendered_frame(&mut app, 120, 40).contains("auto-n"));

    // /permissions previews the draft agent's real rules.
    app.run_command(SlashCommand::Permissions).await;
    assert!(app.permission_panel.draft);
    pump_until(&mut app, |app| app.permission_panel.result.is_some()).await;
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("Permissions · new session"), "{rendered}");

    // Overriding the first rule edits only the draft, then re-previews.
    let first = app.permission_panel.selected().expect("first rule");
    // Step toward whichever end the effect is not already at.
    app.handle_key(key(
        if first.effect == cookie_agent_protocol::PermissionEffect::Deny {
            KeyCode::Left
        } else {
            KeyCode::Right
        },
    ))
    .await;
    assert_eq!(app.draft_permissions.rules.len(), 1);
    assert_eq!(app.draft_permissions.rules[0].action, first.action);
    pump_until(&mut app, |app| {
        app.permission_panel.selected().is_some_and(|row| {
            row.source == cookie_agent_protocol::PermissionRuleSource::SessionOverlay
        })
    })
    .await;
    app.handle_key(key(KeyCode::Esc)).await;

    let untouched = client
        .get_session_permissions(cookie_agent_protocol::SessionPermissionGetParams {
            session_id: existing,
        })
        .await
        .expect("existing permissions");
    assert_eq!(
        untouched.current_mode,
        Some(cookie_agent_protocol::PermissionMode::AutoApprove)
    );
    assert!(untouched.permissions.iter().all(|permission| {
        permission.source != cookie_agent_protocol::PermissionRuleSource::SessionOverlay
    }));

    // The first prompt creates the session with both settings.
    type_input(&mut app, "hello").await;
    app.handle_key(key(KeyCode::Enter)).await;
    let created = app.selected.expect("created session");
    assert_ne!(created, existing);
    assert!(app.new_session_draft.is_none());
    let live = client
        .get_session_permissions(cookie_agent_protocol::SessionPermissionGetParams {
            session_id: created,
        })
        .await
        .expect("created permissions");
    assert_eq!(
        live.current_mode,
        Some(cookie_agent_protocol::PermissionMode::AutoApproveN)
    );
    let overridden = live
        .permissions
        .iter()
        .find(|permission| permission.action == first.action)
        .expect("overridden action");
    assert_eq!(
        overridden.source,
        cookie_agent_protocol::PermissionRuleSource::SessionOverlay
    );
    // The draft's settings were consumed.
    assert!(app.draft_permissions.rules.is_empty());
    assert!(app.draft_permissions.mode.is_none());
}
