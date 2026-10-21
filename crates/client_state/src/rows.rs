//! How the transcript's non-message rows read in every client: producer
//! messages (goal control, background-call reports, delegation reports),
//! goals, and the system prompt and tools rows pinned at its top, with the
//! bounds every client shows an opened row's body within.

use cookie_agent_protocol::{
    AgentId, AgentSnapshot, GoalStatus, ProducerDeliveryMode, ProducerOwner,
};

use crate::SessionState;

/// The most lines an opened row shows of its body (the tools row's list
/// included).
pub const MAX_EXPANDED_BODY_LINES: usize = 64;

/// The most bytes an opened row shows of its body.
pub const MAX_EXPANDED_BODY_BYTES: usize = 8 * 1024;

/// The most lines the opened system prompt row shows.
pub const MAX_SYSTEM_PROMPT_BODY_LINES: usize = 256;

/// The most bytes the opened system prompt row shows.
pub const MAX_SYSTEM_PROMPT_BODY_BYTES: usize = 32 * 1024;

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

/// The agent snapshot the system prompt row shows: the latest run's, else
/// the one the session was created with; and whether it is the run's.
pub fn system_prompt_source(state: &SessionState) -> Option<(&AgentSnapshot, bool)> {
    state
        .run_snapshot
        .as_deref()
        .map(|snapshot| (snapshot, true))
        .or_else(|| {
            state
                .creation_agent
                .as_deref()
                .map(|snapshot| (snapshot, false))
        })
}

/// The system prompt row's title, after its icon and chevron:
/// `system prompt · primary (last run) · next: fast (42 lines)`. The next
/// agent shows only when the draft's differs from the snapshot's.
pub fn system_prompt_summary(
    snapshot: &AgentSnapshot,
    from_run: bool,
    draft_agent: Option<&AgentId>,
) -> String {
    let when = if from_run { "last run" } else { "at creation" };
    let next = draft_agent
        .filter(|draft_agent| **draft_agent != snapshot.agent)
        .map(|draft_agent| format!(" · next: {draft_agent}"))
        .unwrap_or_default();
    format!(
        "system prompt · {} ({when}){next} ({} lines)",
        snapshot.agent,
        display_line_count(&snapshot.composed_prompt)
    )
}

/// The tools row's title, after its icon and chevron.
pub fn model_tools_summary(tools: &[String]) -> String {
    format!("tools · {} available (last request)", tools.len())
}

/// A body cut to the bounds an opened row shows: its lines, control
/// characters but tabs shown as U+FFFD, and how many lines did not fit.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BoundedText {
    pub lines: Vec<String>,
    pub omitted_lines: usize,
}

/// `text` within `max_lines` lines and `max_bytes` bytes: whole lines,
/// then the part of the line the bytes run out in. The lines past the
/// bounds, and a line cut short, count as omitted.
pub fn bounded_text(text: &str, max_lines: usize, max_bytes: usize) -> BoundedText {
    let mut bounded = BoundedText::default();
    let mut bytes = 0usize;
    let mut whole = 0usize;
    let mut total = 0usize;
    for line in text.split('\n') {
        total += 1;
        if bounded.lines.len() >= max_lines || bytes >= max_bytes {
            continue;
        }
        let mut shown = String::with_capacity(line.len().min(max_bytes - bytes));
        let mut complete = true;
        for character in line.chars() {
            let character = if character.is_control() && character != '\t' {
                '\u{FFFD}'
            } else {
                character
            };
            if bytes + shown.len() + character.len_utf8() > max_bytes {
                complete = false;
                break;
            }
            shown.push(character);
        }
        if complete {
            bytes += shown.len();
            whole += 1;
            bounded.lines.push(shown);
        } else {
            if !shown.is_empty() {
                bounded.lines.push(shown);
            }
            bytes = max_bytes;
        }
    }
    bounded.omitted_lines = total - whole;
    bounded
}

/// The tools the opened tools row lists: as many as fit
/// [`MAX_EXPANDED_BODY_BYTES`] joined with `, `, as the TUI's line does,
/// and how many more there are.
pub fn bounded_tools(tools: &[String]) -> (&[String], usize) {
    let mut bytes = 0usize;
    let mut shown = 0usize;
    for tool in tools {
        let next = bytes + tool.len() + if shown == 0 { 0 } else { 2 };
        if next > MAX_EXPANDED_BODY_BYTES {
            break;
        }
        bytes = next;
        shown += 1;
    }
    (&tools[..shown], tools.len() - shown)
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{ProducerDeliveryMode, ProducerOwner};

    use super::*;

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

    #[test]
    fn bodies_keep_within_their_line_and_byte_bounds() {
        assert_eq!(
            bounded_text("a\nb\nc", 2, 100),
            BoundedText {
                lines: vec!["a".into(), "b".into()],
                omitted_lines: 1
            }
        );
        // The bytes run out inside the second line: its start shows, and
        // it counts as omitted.
        assert_eq!(
            bounded_text("abc\ndefg\nh", 10, 5),
            BoundedText {
                lines: vec!["abc".into(), "de".into()],
                omitted_lines: 2
            }
        );
        assert_eq!(
            bounded_text("tab\there\u{7}", 10, 100).lines,
            ["tab\there\u{FFFD}"]
        );
        assert_eq!(bounded_text("", 1, 1).lines, [""]);
    }

    #[test]
    fn the_tools_row_lists_what_fits_its_bytes() {
        let tools = vec!["read".to_owned(), "bash".to_owned()];
        assert_eq!(bounded_tools(&tools), (&tools[..], 0));
        let many = (0..2000)
            .map(|index| format!("tool_{index:04}"))
            .collect::<Vec<_>>();
        let (shown, omitted) = bounded_tools(&many);
        // Each name is 9 bytes, then 2 for the separator.
        assert_eq!(shown.len(), (MAX_EXPANDED_BODY_BYTES + 2) / 11);
        assert_eq!(shown.len() + omitted, many.len());
        assert_eq!(
            model_tools_summary(&tools),
            "tools · 2 available (last request)"
        );
    }

    #[test]
    fn the_system_prompt_row_names_its_agent_and_the_next() {
        let snapshot = AgentSnapshot {
            agent: AgentId::new("primary").unwrap(),
            mode: cookie_agent_protocol::AgentMode::Primary,
            description: "Test primary agent".into(),
            document_source: cookie_agent_protocol::AgentDocumentSource::User,
            document_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"document"),
            composed_prompt: "one\ntwo".into(),
            prompt_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"prompt"),
            max_output_tokens: 0,
            permissions: Vec::new(),
            delegation: None,
            fallback_chain: Vec::new(),
            selected_suffix_start: 0,
        };
        let fast = AgentId::new("fast").unwrap();
        assert_eq!(
            system_prompt_summary(&snapshot, true, Some(&snapshot.agent)),
            "system prompt · primary (last run) (2 lines)"
        );
        assert_eq!(
            system_prompt_summary(&snapshot, false, Some(&fast)),
            "system prompt · primary (at creation) · next: fast (2 lines)"
        );
    }
}
