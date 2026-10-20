//! The root session's goal: what `/goal` asks for, and which lifecycle
//! actions the goal bar offers, as the TUI's bar does.

use cookie_agent_protocol::{GoalLifecycleAction, GoalStatus};

/// What a `/goal` argument asks for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalCommand {
    /// No argument: show the goal and its controls.
    Show,
    /// Set a new objective.
    Objective(String),
    /// Pause, resume, or cancel the goal.
    Lifecycle(GoalLifecycleAction),
}

impl GoalCommand {
    /// `pause`, `resume`, and `cancel` control the goal; any other text is
    /// the new objective.
    pub fn parse(argument: &str) -> Self {
        match argument.trim() {
            "" => Self::Show,
            "pause" => Self::Lifecycle(GoalLifecycleAction::Pause),
            "resume" => Self::Lifecycle(GoalLifecycleAction::Resume),
            "cancel" => Self::Lifecycle(GoalLifecycleAction::Cancel),
            objective => Self::Objective(objective.to_owned()),
        }
    }
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

    use super::{GoalCommand, goal_actions};

    #[test]
    fn goal_arguments_control_the_goal_or_set_its_objective() {
        assert_eq!(GoalCommand::parse("  "), GoalCommand::Show);
        assert_eq!(
            GoalCommand::parse("pause"),
            GoalCommand::Lifecycle(GoalLifecycleAction::Pause)
        );
        assert_eq!(
            GoalCommand::parse(" resume "),
            GoalCommand::Lifecycle(GoalLifecycleAction::Resume)
        );
        assert_eq!(
            GoalCommand::parse("ship the release"),
            GoalCommand::Objective("ship the release".into())
        );
    }

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
