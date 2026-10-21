//! How the transcript's non-message rows read in every client: producer
//! messages (goal control, background-call reports, delegation reports),
//! and goals.

use cookie_agent_protocol::{GoalStatus, ProducerDeliveryMode, ProducerOwner};

/// The one-line summary of a producer message: its description when it has
/// one, else who produced it and how it is delivered.
pub fn producer_summary(
    owner: &ProducerOwner,
    mode: ProducerDeliveryMode,
    description: Option<&str>,
) -> String {
    if let Some(description) = description.filter(|text| !text.trim().is_empty()) {
        return description.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    format!(
        "{} · {}",
        producer_owner_label(owner),
        producer_mode_label(mode)
    )
}

/// Who produced a message, as a person reads it.
pub fn producer_owner_label(owner: &ProducerOwner) -> String {
    match owner {
        ProducerOwner::External { source, name } => format!("{source} · {name}"),
        ProducerOwner::Delegation { invocation_id } => format!("delegation {invocation_id}"),
        ProducerOwner::Goal { .. } => "goal controller".to_owned(),
        ProducerOwner::GoalControl { .. } => "goal control".to_owned(),
        ProducerOwner::Agent { session_id } => format!("agent {session_id}"),
        ProducerOwner::Tool { tool_call_id } => format!("background call {tool_call_id}"),
        ProducerOwner::Subagent { session_id } => format!("subagent {session_id}"),
    }
}

pub fn producer_mode_label(mode: ProducerDeliveryMode) -> &'static str {
    match mode {
        ProducerDeliveryMode::Steer => "steer",
        ProducerDeliveryMode::Queue => "queue",
    }
}

pub fn goal_status_label(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Completed => "completed",
        GoalStatus::Cancelled => "cancelled",
    }
}

/// Whether the bar above the composer shows a goal in `status`: only while
/// it is still being pursued. A completed or cancelled goal stays reachable
/// through the goal details, not the bar.
pub fn goal_bar_shows(status: GoalStatus) -> bool {
    matches!(status, GoalStatus::Active | GoalStatus::Paused)
}

/// How many lines `text` has, as the collapsed rows count them.
pub fn display_line_count(text: &str) -> usize {
    text.split('\n').count()
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{ProducerDeliveryMode, ProducerOwner};

    use super::producer_summary;

    #[test]
    fn a_producer_summary_prefers_the_description() {
        let owner = ProducerOwner::GoalControl {
            goal_id: cookie_agent_protocol::GoalId::new_v7(),
        };
        assert_eq!(
            producer_summary(
                &owner,
                ProducerDeliveryMode::Queue,
                Some("  build\n failed ")
            ),
            "build failed"
        );
        assert_eq!(
            producer_summary(&owner, ProducerDeliveryMode::Steer, Some(" ")),
            "goal control · steer"
        );
    }
}
