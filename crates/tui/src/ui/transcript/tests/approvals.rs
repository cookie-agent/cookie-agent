use cookie_agent_protocol::{ApprovalUserDecision, SessionId};

use crate::ui::app::*;

use super::support::*;

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

#[test]
fn approval_modal_no_color_snapshot_remains_textually_complete_and_scrollable() {
    let approval = bash_approval_state();
    let content = approval_content(&approval);
    assert!(content.contains("PERMISSION REQUIRED · ESCALATED"));
    assert!(content.contains("git status"));
    let lines = content.lines().count();
    assert!(lines > 20);
}

#[tokio::test]
async fn roomy_approval_renders_glyph_buttons_with_tiled_hit_regions() {
    let mut app = app_with_approval().await;
    let rendered = rendered_frame(&mut app, 120, 40);
    // Glyph-bearing labels: the decision never relies on color alone.
    assert!(rendered.contains("✓ Allow once"), "{rendered}");
    assert!(rendered.contains("✗ Reject"), "{rendered}");
    assert!(rendered.contains("⎋ Cancel"), "{rendered}");
    // Rounded button frames read as distinct buttons.
    assert!(rendered.contains('╭'), "{rendered}");

    let actions = &app.hit_map.approval_actions;
    assert_eq!(
        actions.iter().map(|hit| hit.decision).collect::<Vec<_>>(),
        vec![
            ApprovalUserDecision::ApproveOnce,
            ApprovalUserDecision::Reject,
            ApprovalUserDecision::Cancel,
        ]
    );
    // The roomy panel gets three-row buttons whose hit regions tile the
    // inner width contiguously: no gaps, no overlaps.
    assert!(actions.iter().all(|hit| hit.rect.height == 3));
    let mut column = actions[0].rect.x;
    let row = actions[0].rect.y;
    for hit in actions {
        assert_eq!(hit.rect.x, column);
        assert_eq!(hit.rect.y, row);
        column = column.saturating_add(hit.rect.width);
    }
}

#[tokio::test]
async fn cramped_approval_falls_back_to_a_single_action_row() {
    let mut app = app_with_approval().await;
    let rendered = rendered_frame(&mut app, 80, 24);
    let actions = &app.hit_map.approval_actions;
    assert_eq!(actions.len(), 3);
    assert!(actions.iter().all(|hit| hit.rect.height == 1));
    let mut column = actions[0].rect.x;
    for hit in actions {
        assert_eq!(hit.rect.x, column);
        column = column.saturating_add(hit.rect.width);
    }
    assert!(rendered.contains("✓ Allow once"), "{rendered}");
}
