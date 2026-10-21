//! The subagents nested under their session in the sessions list: which
//! rows a session tree shows, folded where the user folded it, whether the
//! session's row expands at all (`/agent-panel`), the title bar's path to
//! a viewed subagent, and the subagent a delegate call started.

use std::collections::HashSet;

use cookie_agent_protocol::{
    SessionId, SessionMeta, SessionOrigin, SessionStatus, SessionTree, ToolCallId,
};

/// Whether the open session's row shows its subagents, as
/// `/agent-panel show|hide|auto` sets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PanelMode {
    /// While a delegated agent is live, or a child session is viewed.
    #[default]
    Auto,
    Shown,
    Hidden,
}

/// One row of the tree.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeRow {
    pub session_id: SessionId,
    pub depth: usize,
    /// The agent's name.
    pub agent: String,
    /// What it was asked: the session's title.
    pub task: String,
    /// The short handle the model refers to it by.
    pub handle: Option<String>,
    pub status: SessionStatus,
    pub has_children: bool,
    pub collapsed: bool,
}

/// The rows of `tree`, depth first, without the children of collapsed
/// sessions.
pub fn tree_rows(tree: &SessionTree, collapsed: &HashSet<SessionId>) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    push_rows(tree, 0, collapsed, &mut rows);
    rows
}

fn push_rows(
    tree: &SessionTree,
    depth: usize,
    collapsed: &HashSet<SessionId>,
    rows: &mut Vec<TreeRow>,
) {
    let session_id = tree.session.session_id;
    let folded = collapsed.contains(&session_id);
    rows.push(TreeRow {
        session_id,
        depth,
        agent: tree.session.creation_selection.agent.to_string(),
        task: task(&tree.session),
        handle: tree.session.short_id.clone(),
        status: tree.session.status,
        has_children: !tree.children.is_empty(),
        collapsed: folded,
    });
    if !folded {
        for child in &tree.children {
            push_rows(child, depth + 1, collapsed, rows);
        }
    }
}

fn task(session: &SessionMeta) -> String {
    session
        .title
        .as_ref()
        .map_or_else(|| "untitled".to_owned(), ToString::to_string)
}

/// A subagent's name in the title bar: its agent and its task.
pub fn subagent_label(session: &SessionMeta) -> String {
    format!("{}: {}", session.creation_selection.agent, task(session))
}

/// The sessions from `tree`'s root down to `session_id`, both included;
/// empty when the tree does not hold it.
pub fn path(tree: &SessionTree, session_id: SessionId) -> Vec<&SessionMeta> {
    if tree.session.session_id == session_id {
        return vec![&tree.session];
    }
    for child in &tree.children {
        let mut below = path(child, session_id);
        if !below.is_empty() {
            below.insert(0, &tree.session);
            return below;
        }
    }
    Vec::new()
}

/// The subagents `parent` started, by the delegate call that started each.
pub fn delegates_of(tree: &SessionTree, parent: SessionId) -> Vec<(ToolCallId, &SessionMeta)> {
    let mut found = Vec::new();
    collect_delegates(tree, parent, &mut found);
    found
}

fn collect_delegates<'a>(
    tree: &'a SessionTree,
    parent: SessionId,
    found: &mut Vec<(ToolCallId, &'a SessionMeta)>,
) {
    for child in &tree.children {
        if let SessionOrigin::Delegated {
            parent_session_id,
            parent_tool_call_id,
            ..
        } = &child.session.origin
            && *parent_session_id == parent
        {
            found.push((*parent_tool_call_id, &child.session));
        }
        collect_delegates(child, parent, found);
    }
}

/// What a running subagent is doing, as a delegate call's row says it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Activity {
    WaitingForApproval,
    /// Running a tool call.
    RunningTool,
    /// Its model is answering.
    Thinking,
}

impl Activity {
    /// What the subagent's session state says it does: an approval
    /// outranks a running tool, which outranks the run itself. `None` for
    /// a subagent that is not running.
    pub fn of(running: bool, tool_running: bool, approval_waiting: bool) -> Option<Self> {
        if approval_waiting {
            Some(Self::WaitingForApproval)
        } else if tool_running {
            Some(Self::RunningTool)
        } else if running {
            Some(Self::Thinking)
        } else {
            None
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::WaitingForApproval => "waiting for approval",
            Self::RunningTool => "running a tool",
            Self::Thinking => "thinking",
        }
    }
}

/// Whether a delegated session of `tree` is still live (idle or running).
pub fn has_live_delegate(tree: &SessionTree) -> bool {
    tree.children.iter().any(|child| {
        matches!(
            child.session.status,
            SessionStatus::Idle | SessionStatus::Running
        ) || has_live_delegate(child)
    })
}

/// Whether the panel shows in `mode` for `tree`, with `viewing_child` when
/// the viewed session is not the tree's root.
pub fn panel_visible(mode: PanelMode, tree: Option<&SessionTree>, viewing_child: bool) -> bool {
    match mode {
        PanelMode::Shown => true,
        PanelMode::Hidden => false,
        PanelMode::Auto => viewing_child || tree.is_some_and(has_live_delegate),
    }
}

/// The metadata of `session_id` in `tree`.
pub fn find(tree: &SessionTree, session_id: SessionId) -> Option<&SessionMeta> {
    if tree.session.session_id == session_id {
        return Some(&tree.session);
    }
    tree.children
        .iter()
        .find_map(|child| find(child, session_id))
}

/// Whether `session_id` is `tree`'s root or one of its descendants.
pub fn contains(tree: &SessionTree, session_id: SessionId) -> bool {
    tree.session.session_id == session_id
        || tree
            .children
            .iter()
            .any(|child| contains(child, session_id))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use cookie_agent_protocol::{SessionId, SessionMeta, SessionStatus, SessionTree};

    use super::{
        Activity, PanelMode, contains, delegates_of, has_live_delegate, panel_visible, path,
        subagent_label, tree_rows,
    };

    fn meta(agent: &str, status: SessionStatus) -> SessionMeta {
        let mut meta: SessionMeta = serde_json::from_value(serde_json::json!({
            "session_id": SessionId::new_v7(),
            "origin": {"type": "root"},
            "short_id": null,
            "cwd_identity": "/repo",
            "title": null,
            "last_agent_activity": null,
            "creation_selection": {
                "agent": agent,
                "model": {"model": "custom.local/test", "variant": null},
            },
            "runtime_revision": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "catalog_revision": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            "provider_state_revision": "sha256:3333333333333333333333333333333333333333333333333333333333333333",
            "model_revision": "sha256:4444444444444444444444444444444444444444444444444444444444444444",
            "agent_revision": "sha256:5555555555555555555555555555555555555555555555555555555555555555",
            "recipe_registry_revision": "sha256:6666666666666666666666666666666666666666666666666666666666666666",
            "manifest_revision": "sha256:7777777777777777777777777777777777777777777777777777777777777777",
            "title_updated_seq": 0,
            "last_event_seq": 1,
            "last_activity": "2026-10-20T00:00:00Z",
            "status": "idle",
            "skipped_events": [],
        }))
        .expect("session meta");
        meta.status = status;
        meta
    }

    fn node(agent: &str, status: SessionStatus, children: Vec<SessionTree>) -> SessionTree {
        SessionTree {
            session: meta(agent, status),
            children,
        }
    }

    #[test]
    fn rows_follow_the_tree_and_folding_hides_children() {
        let tree = node(
            "primary",
            SessionStatus::Idle,
            vec![node(
                "worker",
                SessionStatus::Running,
                vec![node("helper", SessionStatus::Completed, Vec::new())],
            )],
        );
        let rows = tree_rows(&tree, &HashSet::new());
        assert_eq!(
            rows.iter()
                .map(|row| (row.depth, row.agent.as_str(), row.task.as_str()))
                .collect::<Vec<_>>(),
            [
                (0, "primary", "untitled"),
                (1, "worker", "untitled"),
                (2, "helper", "untitled"),
            ]
        );
        assert!(rows[1].status == SessionStatus::Running && rows[1].has_children);
        let worker = tree.children[0].session.session_id;
        let folded = tree_rows(&tree, &HashSet::from([worker]));
        assert_eq!(folded.len(), 2);
        assert!(folded[1].collapsed);
        assert!(contains(
            &tree,
            tree.children[0].children[0].session.session_id
        ));
        assert!(!contains(&tree, SessionId::new_v7()));
    }

    #[test]
    fn the_auto_panel_shows_for_live_delegates_or_a_viewed_child() {
        let finished = node(
            "primary",
            SessionStatus::Idle,
            vec![node("worker", SessionStatus::Completed, Vec::new())],
        );
        let live = node(
            "primary",
            SessionStatus::Running,
            vec![node("worker", SessionStatus::Running, Vec::new())],
        );
        assert!(!has_live_delegate(&finished));
        assert!(has_live_delegate(&live));
        assert!(!panel_visible(PanelMode::Auto, Some(&finished), false));
        assert!(panel_visible(PanelMode::Auto, Some(&finished), true));
        assert!(panel_visible(PanelMode::Auto, Some(&live), false));
        assert!(panel_visible(PanelMode::Shown, None, false));
        assert!(!panel_visible(PanelMode::Hidden, Some(&live), true));
    }

    /// `child` as delegated by `parent`'s call `call`.
    fn delegated(mut child: SessionTree, parent: &SessionTree, call: &str) -> SessionTree {
        child.session.origin = serde_json::from_value(serde_json::json!({
            "type": "delegated",
            "root_session_id": parent.session.session_id,
            "parent_session_id": parent.session.session_id,
            "parent_run_id": uuid::Uuid::now_v7(),
            "parent_tool_call_id": call,
            "invocation_id": uuid::Uuid::now_v7(),
            "depth": 1,
        }))
        .expect("a delegated origin");
        child
    }

    #[test]
    fn the_path_leads_from_the_root_to_a_subagent() {
        let mut tree = node("primary", SessionStatus::Idle, Vec::new());
        let mut worker = node("worker", SessionStatus::Running, Vec::new());
        worker.session.title =
            Some(cookie_agent_protocol::SessionTitle::new("check tests").expect("a title"));
        let helper = node("helper", SessionStatus::Idle, Vec::new());
        let helper_id = helper.session.session_id;
        worker.children.push(helper);
        tree.children.push(worker);
        let labels = path(&tree, helper_id)
            .into_iter()
            .map(|session| session.creation_selection.agent.to_string())
            .collect::<Vec<_>>();
        assert_eq!(labels, ["primary", "worker", "helper"]);
        assert_eq!(
            subagent_label(&tree.children[0].session),
            "worker: check tests"
        );
        assert_eq!(path(&tree, tree.session.session_id).len(), 1);
        assert!(path(&tree, SessionId::new_v7()).is_empty());
    }

    #[test]
    fn a_delegate_call_finds_the_subagent_it_started() {
        let (call_7, call_9) = (
            uuid::Uuid::now_v7().to_string(),
            uuid::Uuid::now_v7().to_string(),
        );
        let mut tree = node("primary", SessionStatus::Idle, Vec::new());
        let child = delegated(
            node("worker", SessionStatus::Running, Vec::new()),
            &tree,
            &call_7,
        );
        let grandchild = delegated(
            node("helper", SessionStatus::Idle, Vec::new()),
            &child,
            &call_9,
        );
        let mut child = child;
        child.children.push(grandchild);
        let child_id = child.session.session_id;
        tree.children.push(child);
        let found = delegates_of(&tree, tree.session.session_id);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.to_string(), call_7);
        assert_eq!(found[0].1.session_id, child_id);
        assert_eq!(delegates_of(&tree, child_id)[0].0.to_string(), call_9);
    }

    #[test]
    fn an_approval_outranks_a_tool_which_outranks_thinking() {
        assert_eq!(
            Activity::of(true, true, true),
            Some(Activity::WaitingForApproval)
        );
        assert_eq!(Activity::of(true, true, false), Some(Activity::RunningTool));
        assert_eq!(Activity::of(true, false, false), Some(Activity::Thinking));
        assert_eq!(Activity::of(false, false, false), None);
    }
}
