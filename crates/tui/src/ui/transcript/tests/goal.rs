use crate::ui::transcript::*;

use crate::markdown::PlainHighlighter;

use crate::state::SessionState;

use super::support::*;

#[test]
fn goal_rows_are_always_visible_with_progress_and_checklist() {
    let goal = test_goal(
        GoalStatus::Active,
        vec![
            goal_item("Render the objective", true),
            goal_item("Verify narrow panes", false),
        ],
    );
    let state = SessionState {
        transcript: vec![TranscriptItem::Goal {
            id: 1,
            seq: 7,
            activation: false,
            goal: goal.clone(),
        }],
        ..SessionState::default()
    };
    let layout = transcript_layout_with_level(
        &state,
        None,
        48,
        &Theme::default(),
        &PlainHighlighter,
        crate::state::EventLevel::Error,
    );
    let rendered = snapshot_lines(&layout.lines);
    assert!(rendered.contains("GOAL"));
    assert!(rendered.contains(&goal.objective));
    assert!(rendered.contains("status: active · 1/2 finished"));
    assert!(rendered.contains("[x] Render the objective"));
    assert!(rendered.contains("[ ] Verify narrow panes"));
}

#[test]
fn goal_activation_action_fits_narrow_viewports() {
    let mut goal = test_goal(GoalStatus::Active, Vec::new());
    goal.objective = "finish  the parser\nthen verify a-very-long-objective-token".into();
    for width in [0, 1, 3, 7, 8, 18, 80] {
        let lines = goal_activation_layout(&goal, width, &Theme::default());
        assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
        if width == 0 {
            assert!(lines.is_empty());
        }
    }
}

#[test]
fn goal_rows_preserve_duplicate_descriptions_and_finished_marker_order() {
    let goal = test_goal(
        GoalStatus::Active,
        vec![
            goal_item("Repeat verification", false),
            goal_item("Repeat verification", true),
        ],
    );
    let rendered = snapshot_lines(&goal_layout(&goal, 60, &Theme::default()));
    assert!(rendered.contains("1/2 finished"));
    assert_eq!(rendered.matches("Repeat verification").count(), 2);
    let unfinished = rendered
        .find("[ ] Repeat verification")
        .expect("unfinished item");
    let finished = rendered
        .find("[x] Repeat verification")
        .expect("finished item");
    assert!(unfinished < finished);
}

#[test]
fn goal_rows_render_empty_and_terminal_states_cleanly() {
    for status in [
        GoalStatus::Paused,
        GoalStatus::Completed,
        GoalStatus::Cancelled,
    ] {
        let rendered = snapshot_lines(&goal_layout(
            &test_goal(status, Vec::new()),
            40,
            &Theme::default(),
        ));
        assert!(rendered.contains(&format!(
            "status: {} · 0/0 finished",
            goal_status_label(status)
        )));
        assert!(rendered.contains("checklist: empty"));
        assert!(!rendered.contains("[ ] "));
    }
}
