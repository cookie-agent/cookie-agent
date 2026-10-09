use cookie_agent_protocol::{
    ApprovalBoundary, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation, ApprovalId,
    ApprovalRecord, ApprovalRequest, ApprovalResourceSource, ApprovalStatus, ApprovalTrigger,
    ApprovalUserDecision, DecisionTrace, MatchedPermissionRule, PermissionAction, PermissionEffect,
    PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
    PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity, SafeCode,
    SessionId, Sha256Digest, ToolCallId, WildcardPattern,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

use crate::state::{ApprovalState, ToolCallState, ToolStatus};
use crate::ui::app::*;

use super::support::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Link a running tool call to `approval` the way the engine does: the
/// call's prepared operation carries the approval's fingerprint.
fn link_tool(app: &mut App, approval: &ApprovalState, title: &str, arguments: &str) {
    let primary = (title != "bash").then_some("src/lib.rs");
    let call_id = ToolCallId::new_v7();
    app.store
        .sessions
        .entry(approval.session_id)
        .or_default()
        .tools
        .insert(
            call_id,
            ToolCallState {
                id: call_id,
                owner: owner(1, "call-1"),
                presentation: presentation(title, primary),
                arguments: arguments.into(),
                operation_fingerprint: approval.operation_fingerprint.clone(),
                status: ToolStatus::Running,
                detail: String::new(),
                has_output_chunks: false,
            },
        );
}

async fn app_with_linked_bash() -> App {
    let mut app = app_with_approval().await;
    let approval = app.current_approval().cloned().expect("approval");
    link_tool(&mut app, &approval, "bash", r#"{"command": "git status"}"#);
    app
}

/// An edit to `src/lib.rs` that the session's `write *` rule asks about.
fn edit_approval(session_id: SessionId) -> ApprovalState {
    let resource = PreparedApprovalResource {
        capability: PermissionAction::Write,
        canonical: PreparedResourceIdentity::new("path:src-lib.rs").expect("identity"),
        binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"src/lib.rs"),
        binding_lifetime: PreparedBindingLifetime::ProcessLocal,
        boundary: ApprovalBoundary::Exact,
        source: ApprovalResourceSource::PrimaryOperation,
    };
    let resource_digest = resource.binding_digest.clone();
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"edit arguments"),
        vec![ApprovalCapability {
            action: PermissionAction::Write,
            operation: PreparedCapabilityOperation::new("edit").expect("operation"),
        }],
        vec![resource],
        Sha256Digest::of_bytes(b"execution context"),
    )
    .expect("prepared operation");
    let request = ApprovalRequest::new(
        ApprovalId::new_v7(),
        1,
        ApprovalTrigger::PermissionPolicy,
        operation,
        vec![ApprovalEvaluation {
            resource_digest,
            effect: PermissionEffect::Ask,
            trace: DecisionTrace {
                action: PermissionAction::Write,
                normalized_resource: "src/lib.rs".into(),
                candidates: vec![MatchedPermissionRule {
                    source_layer: SafeCode::new("agent_document").expect("layer"),
                    action: PermissionAction::Write,
                    resource: WildcardPattern::new("*").expect("pattern"),
                    effect: PermissionEffect::Ask,
                }],
                effect: PermissionEffect::Ask,
                precedence_reason: "most-specific matching pattern".into(),
            },
        }],
        ApprovalConstraints {
            allow_once: true,
            allow_tree_grant: false,
            cancellable: true,
            expires_at: None,
        },
    )
    .expect("approval request");
    crate::state::approval_state_from_record(ApprovalRecord {
        session_id,
        request,
        status: ApprovalStatus::Escalated,
        internal_decision: None,
        user_decision: None,
        final_decision: None,
    })
    .expect("escalated approval state")
}

async fn recording_app_with_linked_bash() -> (
    App,
    std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    impl Sized,
) {
    let mut app = app_with_linked_bash().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    rendered_frame(&mut app, 120, 40);
    app.arm_approval_hotkeys_for_test();
    (app, recorded, incoming_guard)
}

#[test]
fn approval_content_shows_identity_resources_and_constraints() {
    let session = SessionId::new_v7();
    let state = approval(session);
    let content = approval_content(&state);
    for needle in [
        "PERMISSION REQUIRED · ESCALATED",
        "git status",
        "operation fingerprint",
        "CAPABILITIES (1)",
        "RESOURCES (1)",
        "EVALUATIONS (1)",
        "RESPONSE CONSTRAINTS",
    ] {
        assert!(content.contains(needle), "missing {needle}");
    }
}

#[test]
fn escalated_only_visibility_and_optimistic_response_identity() {
    let session = SessionId::new_v7();
    let mut state = approval(session);
    assert!(state.is_visible_user_escalation());
    state.escalated = false;
    assert!(!state.is_visible_user_escalation());
}

#[test]
fn bash_prepared_approval_identity_snapshot_is_complete() {
    let approval = bash_approval_state();
    insta::assert_snapshot!(stable_approval_snapshot(
        &approval,
        approval_content(&approval)
    ));
}

#[tokio::test]
async fn panel_leads_with_the_command_why_it_asks_and_what_the_focus_does() {
    let mut app = app_with_linked_bash().await;
    let rendered = rendered_frame(&mut app, 120, 40);
    for needle in [
        "⚠ Permission needed",
        "$ git status",
        "the model asked first: model requested approval",
        "runs this exact call now",
        "▸ details",
        "←→ choose · ⏎ confirm · e note · d details",
    ] {
        assert!(rendered.contains(needle), "missing {needle}: {rendered}");
    }
    // The prepared identity stays behind the details toggle.
    assert!(!rendered.contains("operation fingerprint"), "{rendered}");
}

#[tokio::test]
async fn unlinked_request_falls_back_to_its_resources() {
    let mut app = app_with_approval().await;
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains(" bash     git status"), "{rendered}");
}

#[tokio::test]
async fn panel_docks_over_the_composer_and_sizes_to_its_content() {
    let mut app = app_with_linked_bash().await;
    rendered_frame(&mut app, 120, 40);
    let panel = app.hit_map.approval.expect("panel");
    let input = app.hit_map.input.expect("input").rect;
    assert_eq!(panel.bottom(), input.bottom());
    assert_eq!(panel.width, 120);
    assert!(
        panel.height < 20,
        "content-sized, not a fixed share: {panel:?}"
    );
}

#[tokio::test]
async fn details_toggle_by_key_and_click_reveals_the_prepared_identity() {
    let mut app = app_with_linked_bash().await;
    rendered_frame(&mut app, 120, 40);
    // No grace: the toggle answers nothing.
    app.handle_key(key(KeyCode::Char('d'))).await;
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("▾ details"), "{rendered}");
    assert!(rendered.contains("operation fingerprint"), "{rendered}");

    // The longer body scrolls, and its row range shows in the border.
    assert!(app.approval_panel.max_scroll > 0);
    assert!(rendered.contains("↑↓ scroll"), "{rendered}");

    let toggle = app.hit_map.approval_details.expect("details toggle");
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        toggle.x,
        toggle.y,
    ))
    .await;
    assert!(!app.approval_panel.details);
}

#[tokio::test]
async fn arrows_move_focus_and_enter_answers_with_the_focused_decision() {
    let (mut app, recorded, incoming_guard) = recording_app_with_linked_bash().await;
    assert_eq!(
        app.approval_panel.focus,
        Some(ApprovalUserDecision::ApproveOnce)
    );
    app.handle_key(key(KeyCode::Right)).await;
    assert_eq!(app.approval_panel.focus, Some(ApprovalUserDecision::Reject));
    // The effect row follows focus.
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("refuses this call"), "{rendered}");
    // Focus wraps both ways.
    app.handle_key(key(KeyCode::Tab)).await;
    app.handle_key(key(KeyCode::Tab)).await;
    assert_eq!(
        app.approval_panel.focus,
        Some(ApprovalUserDecision::ApproveOnce)
    );
    app.handle_key(key(KeyCode::Left)).await;
    assert_eq!(app.approval_panel.focus, Some(ApprovalUserDecision::Cancel));
    app.handle_key(key(KeyCode::Left)).await;

    app.handle_key(key(KeyCode::Enter)).await;
    wait_for_method(&recorded, "approval.respond", 1).await;
    let params = last_request_params(&recorded, "approval.respond");
    assert_eq!(params["decision"], "reject");
    assert!(params["feedback"].is_null());
    drop(incoming_guard);
}

#[tokio::test]
async fn enter_waits_out_the_grace_and_says_so_in_the_panel() {
    let mut app = app_with_linked_bash().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    rendered_frame(&mut app, 120, 40);
    app.handle_key(key(KeyCode::Enter)).await;
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "approval.respond"), 0);
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(
        rendered.contains("just appeared · press again"),
        "{rendered}"
    );
    drop(incoming_guard);
}

#[tokio::test]
async fn rejection_note_takes_typing_and_is_sent_as_feedback() {
    let (mut app, recorded, incoming_guard) = recording_app_with_linked_bash().await;
    app.handle_key(key(KeyCode::Char('e'))).await;
    assert_eq!(app.approval_panel.focus, Some(ApprovalUserDecision::Reject));
    // Hotkey letters are text while the note is open.
    type_input(&mut app, "try git log, then y").await;
    app.handle_paste(" instead\nplease");
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "approval.respond"), 0);
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("Note for the agent"), "{rendered}");
    assert!(rendered.contains("⏎ reject with this note"), "{rendered}");

    app.handle_key(key(KeyCode::Enter)).await;
    wait_for_method(&recorded, "approval.respond", 1).await;
    let params = last_request_params(&recorded, "approval.respond");
    assert_eq!(params["decision"], "reject");
    assert_eq!(
        params["feedback"]["message"],
        "try git log, then y instead please"
    );
    drop(incoming_guard);
}

#[tokio::test]
async fn esc_closes_the_note_without_answering() {
    let (mut app, recorded, incoming_guard) = recording_app_with_linked_bash().await;
    app.handle_key(key(KeyCode::Char('e'))).await;
    type_input(&mut app, "nope").await;
    app.handle_key(key(KeyCode::Esc)).await;
    assert!(app.approval_panel.note.is_none());
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "approval.respond"), 0);
    // A blank note sends a plain rejection.
    app.handle_key(key(KeyCode::Char('e'))).await;
    type_input(&mut app, "   ").await;
    app.handle_key(key(KeyCode::Enter)).await;
    wait_for_method(&recorded, "approval.respond", 1).await;
    assert!(last_request_params(&recorded, "approval.respond")["feedback"].is_null());
    drop(incoming_guard);
}

#[tokio::test]
async fn edit_approval_previews_its_diff() {
    let mut app = test_app().await;
    let approval = edit_approval(SessionId::new_v7());
    app.selected = Some(approval.session_id);
    app.store
        .sessions
        .entry(approval.session_id)
        .or_default()
        .approvals
        .push(approval.clone());
    link_tool(
        &mut app,
        &approval,
        "edit",
        r#"{"filePath": "src/lib.rs", "oldString": "fn old() {}\n", "newString": "fn new() {\n    todo!()\n}\n"}"#,
    );
    let rendered = rendered_frame(&mut app, 120, 40);
    for needle in [
        "edit  src/lib.rs",
        "1 - fn old() {}",
        "1 + fn new() {",
        "2 +     todo!()",
        "your write rule `*` asks first (agent document)",
    ] {
        assert!(rendered.contains(needle), "missing {needle}: {rendered}");
    }
}

#[tokio::test]
async fn countdown_shows_the_time_left() {
    let mut app = app_with_linked_bash().await;
    let session = app.selected.expect("session");
    app.store.sessions.get_mut(&session).unwrap().approvals[0]
        .constraints
        .expires_at = Some(jiff::Timestamp::now() + jiff::SignedDuration::from_secs(95));
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("expires in 1:3"), "{rendered}");
}

#[tokio::test]
async fn buttons_are_pills_on_one_row_with_their_keys() {
    let mut app = app_with_linked_bash().await;
    let session = app.selected.expect("session");
    app.store.sessions.get_mut(&session).unwrap().approvals[0]
        .constraints
        .allow_tree_grant = true;
    let rendered = rendered_frame(&mut app, 140, 40);
    for label in [
        "✓ Allow once y",
        "✓ Allow all a",
        "✗ Reject n",
        "⎋ Cancel esc",
    ] {
        assert!(rendered.contains(label), "{label}: {rendered}");
    }
    let actions = &app.hit_map.approval_actions;
    assert_eq!(
        actions.iter().map(|hit| hit.decision).collect::<Vec<_>>(),
        vec![
            ApprovalUserDecision::ApproveOnce,
            ApprovalUserDecision::ApproveTree,
            ApprovalUserDecision::Reject,
            ApprovalUserDecision::Cancel,
        ]
    );
    let row = actions[0].rect.y;
    for pair in actions.windows(2) {
        assert_eq!(pair[0].rect.height, 1);
        assert_eq!(pair[1].rect.y, row);
        assert!(pair[0].rect.right() < pair[1].rect.x, "{pair:?}");
    }
}

#[tokio::test]
async fn cramped_panel_keeps_every_button_and_the_command() {
    let mut app = app_with_linked_bash().await;
    let rendered = rendered_frame(&mut app, 40, 12);
    assert_eq!(app.hit_map.approval_actions.len(), 3);
    assert!(rendered.contains("✓ Once y"), "{rendered}");
    assert!(rendered.contains("$ git status"), "{rendered}");
}
