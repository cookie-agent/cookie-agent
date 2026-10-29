//! How the transcript's non-message rows read in every client: producer
//! messages (goal control, background-call reports, delegation reports),
//! goals, and the system prompt and tools rows pinned at its top, with the
//! bounds every client shows an opened row's body within.

use cookie_agent_protocol::{
    AgentId, AgentSnapshot, GoalStatus, ProducerDeliveryMode, ProducerOwner, SessionMeta,
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

/// The one-line summary of a message between an ACP agent's sessions:
/// which way it went, and the other session.
pub fn agent_session_message_summary(message: &cookie_agent_protocol::AcpSessionMessage) -> String {
    let peer = message
        .peer
        .as_deref()
        .map(|peer| peer.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|peer| !peer.is_empty());
    let received = message.direction != cookie_agent_protocol::AcpMessageDirection::Sent;
    match (received, peer.as_deref()) {
        (true, Some("parent")) => "message from the parent session".to_owned(),
        (false, Some("parent")) => "message to the parent session".to_owned(),
        (true, Some(peer)) => format!("message from subagent {peer}"),
        (false, Some(peer)) => format!("message to subagent {peer}"),
        (true, None) => "message from another of the agent's sessions".to_owned(),
        (false, None) => "message to another of the agent's sessions".to_owned(),
    }
}

/// The one-line summary of an ACP agent's command that ended after running
/// in the background, as a background call's report reads: what it ran,
/// else its tool.
pub fn agent_background_ended_summary(ended: &cookie_agent_protocol::AcpBackgroundEnded) -> String {
    let what = ended
        .display
        .as_deref()
        .map(|display| display.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|display| !display.is_empty())
        .unwrap_or_else(|| ended.tool.clone());
    format!("Background: {what} ended")
}

/// What an opened row of an ACP agent's ended background command says
/// before the command itself: its tool, how long it ran until `ended_at`,
/// and how it exited when the agent said so.
pub fn agent_background_ended_details(
    ended: &cookie_agent_protocol::AcpBackgroundEnded,
    ended_at: jiff::Timestamp,
) -> String {
    let ran = ended_at
        .duration_since(ended.started_at)
        .unsigned_abs()
        .as_secs();
    let ran = match ran {
        0..60 => format!("{ran}s"),
        60..3600 => format!("{}m {}s", ran / 60, ran % 60),
        _ => format!("{}h {}m", ran / 3600, ran % 3600 / 60),
    };
    let exit = match (&ended.signal, ended.exit_code) {
        (Some(signal), _) => format!("ended by {signal}"),
        (None, Some(code)) => format!("exit code {code}"),
        (None, None) => "no exit code reported".to_owned(),
    };
    format!(
        "the agent's {} call · ran in the background for {ran} · {exit}",
        ended.tool
    )
}

/// The header of a compaction of an ACP agent's own.
pub const AGENT_COMPACTION_HEADER: &str = "context compacted (by the agent)";

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
        ProducerOwner::AcpAgent { agent } => format!("{agent}, on its own"),
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

/// Whether a session counts as active in a session list: it runs, it is
/// held and waits for the user to resume it, or it waits on a producer that
/// will start its next run (an active goal, a background call, a subagent,
/// an external producer).
pub fn session_is_active(session: &SessionMeta) -> bool {
    matches!(
        crate::activity::session_activity(session),
        crate::activity::SessionActivity::Running
            | crate::activity::SessionActivity::Held(_)
            | crate::activity::SessionActivity::Waiting(_)
    )
}

/// Orders a session list: the active sessions first, then the rest, each
/// group newest [`ordering_activity`] first; sessions that tie keep their
/// order. It knows nothing of how a list is grouped, so a client orders each
/// group (each project's sessions, say) on its own.
pub fn order_sessions(sessions: &mut [SessionMeta]) {
    sessions.sort_by_key(|session| {
        (
            !session_is_active(session),
            std::cmp::Reverse(ordering_activity(session)),
        )
    });
}

/// When a session last had activity that orders it: the user's last input
/// or delegation (`last_agent_activity`), which its own output and tool
/// results never move, so a busy session keeps its place while it works;
/// else, from an older daemon or before any such activity, its last event.
pub fn ordering_activity(session: &SessionMeta) -> jiff::Timestamp {
    session.last_agent_activity.unwrap_or(session.last_activity)
}

/// Orders a session tree's subagents as [`order_sessions`] orders a list,
/// under each session at every depth: the active ones first (running, held,
/// or waiting on a producer), then the rest, each group newest
/// [`ordering_activity`] first. The root keeps its place.
pub fn order_tree(tree: &mut cookie_agent_protocol::SessionTree) {
    tree.children.sort_by_key(|child| {
        (
            !session_is_active(&child.session),
            std::cmp::Reverse(ordering_activity(&child.session)),
        )
    });
    for child in &mut tree.children {
        order_tree(child);
    }
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

/// The tools row's title, after its icon and chevron: the tools of the
/// latest model request, or for an ACP agent the cookie tools its MCP
/// bridge offers beside the agent's own.
pub fn model_tools_summary(state: &SessionState, tools: &[String]) -> String {
    if bridged_tools(state) {
        format!("cookie tools · {} offered to the agent", tools.len())
    } else if state.model_deferred_tools.is_empty() {
        format!("tools · {} available (last request)", tools.len())
    } else {
        let (provided, searchable) = model_tool_groups(state, tools);
        format!(
            "tools · {} provided, {} searchable (last request)",
            provided.len(),
            searchable.len()
        )
    }
}

/// The deferred tools prompt: every announcement since the latest
/// checkpoint, in order, as the model reads them; `None` before any.
pub fn deferred_tools_prompt(state: &SessionState) -> Option<String> {
    (!state.deferred_announcements.is_empty()).then(|| state.deferred_announcements.join("\n\n"))
}

/// Each of `tools`' group slot, by the order its group first appears among
/// them, so the clients colour a group's tools alike; a tool of no known
/// group shares the slot of the others like it.
pub fn tool_group_slots(state: &SessionState, tools: &[&str]) -> Vec<usize> {
    let mut groups = Vec::<Option<&str>>::new();
    tools
        .iter()
        .map(|tool| {
            let group = state.deferred_tool_groups.get(*tool).map(String::as_str);
            groups
                .iter()
                .position(|known| *known == group)
                .unwrap_or_else(|| {
                    groups.push(group);
                    groups.len() - 1
                })
        })
        .collect()
}

/// The latest request's tools split in two: the ones it provided (sent up
/// front, or deferred and loaded) and the ones it left searchable
/// (deferred, not loaded), each in the request's order.
pub fn model_tool_groups<'a>(
    state: &SessionState,
    tools: &'a [String],
) -> (Vec<&'a str>, Vec<&'a str>) {
    let deferred = state
        .model_deferred_tools
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    tools
        .iter()
        .map(String::as_str)
        .partition(|tool| !deferred.contains(tool))
}

/// Whether the session's tools row lists what cookie offers an ACP agent
/// over its MCP bridge: its latest run is of the agent of its ACP session.
fn bridged_tools(state: &SessionState) -> bool {
    state.acp.agent.is_some()
        && state.run_snapshot.as_ref().map(|snapshot| &snapshot.agent) == state.acp.agent.as_ref()
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
pub fn bounded_tools<T: AsRef<str>>(tools: &[T]) -> (&[T], usize) {
    let mut bytes = 0usize;
    let mut shown = 0usize;
    for tool in tools {
        let next = bytes + tool.as_ref().len() + if shown == 0 { 0 } else { 2 };
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
        let mut state = SessionState::default();
        assert_eq!(
            model_tools_summary(&state, &tools),
            "tools · 2 available (last request)"
        );
        // A request that deferred tools splits them.
        let with_deferred = vec![
            "read".to_owned(),
            "chrome_click".to_owned(),
            "bash".to_owned(),
        ];
        state.model_deferred_tools = vec!["chrome_click".to_owned()].into();
        assert_eq!(
            model_tools_summary(&state, &with_deferred),
            "tools · 2 provided, 1 searchable (last request)"
        );
        assert_eq!(
            model_tool_groups(&state, &with_deferred),
            (vec!["read", "bash"], vec!["chrome_click"])
        );
        // An ACP agent's run lists the tools cookie's bridge offers it.
        let claude = AgentId::new("claude").unwrap();
        state.acp.agent = Some(claude.clone());
        state.run_snapshot = Some(Box::new(snapshot_of(claude)));
        assert_eq!(
            model_tools_summary(&state, &tools),
            "cookie tools · 2 offered to the agent"
        );
    }

    fn snapshot_of(agent: AgentId) -> AgentSnapshot {
        AgentSnapshot {
            agent,
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
        }
    }

    #[test]
    fn the_system_prompt_row_names_its_agent_and_the_next() {
        let snapshot = snapshot_of(AgentId::new("primary").unwrap());
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

    fn session(name: &str, status: &str, at: &str) -> SessionMeta {
        serde_json::from_value(serde_json::json!({
            "session_id": cookie_agent_protocol::SessionId::new_v7(),
            "origin": {"type": "root"},
            "short_id": name,
            "cwd_identity": "/w",
            "title": null,
            "last_agent_activity": null,
            "creation_selection": {
                "agent": "primary",
                "model": {"model": "custom.local/test", "variant": null},
            },
            "runtime_revision": format!("sha256:{}", "1".repeat(64)),
            "catalog_revision": format!("sha256:{}", "2".repeat(64)),
            "provider_state_revision": format!("sha256:{}", "3".repeat(64)),
            "model_revision": format!("sha256:{}", "4".repeat(64)),
            "agent_revision": format!("sha256:{}", "5".repeat(64)),
            "recipe_registry_revision": format!("sha256:{}", "6".repeat(64)),
            "manifest_revision": format!("sha256:{}", "7".repeat(64)),
            "title_updated_seq": 0,
            "last_event_seq": 1,
            "last_activity": at,
            "status": status,
            "skipped_events": [],
        }))
        .expect("session meta")
    }

    #[test]
    fn a_busy_session_keeps_its_place_by_the_users_last_activity() {
        // Both run; `busy` produced output since, but the user wrote to
        // `asked` last, so `asked` stays first.
        let mut busy = session("busy", "running", "2026-10-20T12:00:00Z");
        busy.last_agent_activity = Some("2026-10-20T09:00:00Z".parse().unwrap());
        let mut asked = session("asked", "running", "2026-10-20T11:00:00Z");
        asked.last_agent_activity = Some("2026-10-20T10:00:00Z".parse().unwrap());
        // Without the user's activity, the last event orders it.
        let unknown = session("unknown", "running", "2026-10-20T09:30:00Z");
        let mut sessions = vec![busy, unknown, asked];
        order_sessions(&mut sessions);
        let names = sessions
            .iter()
            .map(|session| session.short_id.as_deref().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(names, ["asked", "unknown", "busy"]);
    }

    #[test]
    fn subagents_come_in_the_session_list_order_at_every_depth() {
        let node = |meta: SessionMeta, children: Vec<cookie_agent_protocol::SessionTree>| {
            cookie_agent_protocol::SessionTree {
                session: meta,
                children,
            }
        };
        let mut tree = node(
            session("root", "running", "2026-10-20T12:00:00Z"),
            vec![
                node(
                    session("done", "completed", "2026-10-20T11:00:00Z"),
                    Vec::new(),
                ),
                node(
                    session("old-running", "running", "2026-10-20T08:00:00Z"),
                    vec![
                        node(
                            session("deep-done", "completed", "2026-10-20T09:00:00Z"),
                            Vec::new(),
                        ),
                        node(
                            session("deep-running", "running", "2026-10-20T07:00:00Z"),
                            Vec::new(),
                        ),
                    ],
                ),
                node(
                    session("newer-done", "failed", "2026-10-20T11:30:00Z"),
                    Vec::new(),
                ),
            ],
        );
        order_tree(&mut tree);
        let names = |tree: &cookie_agent_protocol::SessionTree| {
            tree.children
                .iter()
                .map(|child| child.session.short_id.clone().unwrap_or_default())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&tree), ["old-running", "newer-done", "done"]);
        assert_eq!(names(&tree.children[0]), ["deep-running", "deep-done"]);
    }

    #[test]
    fn active_sessions_come_first_then_the_rest_newest_first() {
        let mut sessions = vec![
            session("old-idle", "idle", "2026-10-20T08:00:00Z"),
            session("old-running", "running", "2026-10-20T07:00:00Z"),
            session("new-failed", "failed", "2026-10-20T12:00:00Z"),
            session("tie-a", "completed", "2026-10-20T10:00:00Z"),
            session("new-running", "running", "2026-10-20T11:00:00Z"),
            session("tie-b", "cancelled", "2026-10-20T10:00:00Z"),
            {
                // Waiting on a producer counts as active, under the newer
                // running sessions but above every idle one.
                let mut waiting = session("old-waiting", "idle", "2026-10-20T06:00:00Z");
                waiting.waiting_on = vec![cookie_agent_protocol::WaitingOn::Goal];
                waiting
            },
        ];
        order_sessions(&mut sessions);
        let names = sessions
            .iter()
            .map(|session| session.short_id.as_deref().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "new-running",
                "old-running",
                "old-waiting",
                "new-failed",
                "tie-a",
                "tie-b",
                "old-idle"
            ]
        );
    }
}
