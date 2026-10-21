//! The root session's goal: what the goal sheet asks for, and which
//! lifecycle actions the goal bar offers, as the TUI's bar does.

use cookie_agent_protocol::{GoalLifecycleAction, GoalStatus};

/// What the goal sheet or bar asks for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalCommand {
    /// Show the goal and its controls.
    Show,
    /// Set a new objective.
    Objective(String),
    /// Pause, resume, or cancel the goal.
    Lifecycle(GoalLifecycleAction),
}

/// The lifecycle actions a goal in `status` offers; none in a read-only
/// session.
pub fn goal_actions(status: GoalStatus, read_only: bool) -> &'static [GoalLifecycleAction] {
    if read_only {
        return &[];
    }
    match status {
        GoalStatus::Active => &[GoalLifecycleAction::Pause, GoalLifecycleAction::Cancel],
        GoalStatus::Paused => &[GoalLifecycleAction::Resume, GoalLifecycleAction::Cancel],
        GoalStatus::Completed | GoalStatus::Cancelled => &[],
    }
}

pub fn action_label(action: GoalLifecycleAction) -> &'static str {
    match action {
        GoalLifecycleAction::Pause => "Pause",
        GoalLifecycleAction::Resume => "Resume",
        GoalLifecycleAction::Cancel => "Cancel",
    }
}

pub fn status_name(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Completed => "completed",
        GoalStatus::Cancelled => "cancelled",
    }
}

/// Whether `action` needs the draft's agent and model: resuming runs the
/// goal again.
pub fn needs_selection(action: GoalLifecycleAction) -> bool {
    action == GoalLifecycleAction::Resume
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{GoalLifecycleAction, GoalStatus};

    use super::goal_actions;

    #[test]
    fn the_bar_offers_what_the_goal_status_allows() {
        assert_eq!(
            goal_actions(GoalStatus::Active, false),
            [GoalLifecycleAction::Pause, GoalLifecycleAction::Cancel]
        );
        assert_eq!(
            goal_actions(GoalStatus::Paused, false),
            [GoalLifecycleAction::Resume, GoalLifecycleAction::Cancel]
        );
        assert!(goal_actions(GoalStatus::Completed, false).is_empty());
        assert!(goal_actions(GoalStatus::Active, true).is_empty());
    }
}
