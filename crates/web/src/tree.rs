//! The subagent tree panel: which rows a session tree shows, folded where
//! the user folded it, and when the panel shows at all (`/agent-panel`).

use std::collections::HashSet;

use cookie_agent_protocol::{SessionId, SessionMeta, SessionStatus, SessionTree};

/// Whether the agent panel shows, as `/agent-panel show|hide|auto` sets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PanelMode {
    /// While a delegated agent is live, or a child session is viewed.
    #[default]
    Auto,
    Shown,
    Hidden,
}

/// One row of the panel.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeRow {
    pub session_id: SessionId,
    pub depth: usize,
    pub label: String,
    pub running: bool,
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
        label: row_label(&tree.session),
        running: tree.session.status == SessionStatus::Running,
        has_children: !tree.children.is_empty(),
        collapsed: folded,
    });
    if !folded {
        for child in &tree.children {
            push_rows(child, depth + 1, collapsed, rows);
        }
    }
}

fn row_label(session: &SessionMeta) -> String {
    let title = session
        .title
        .as_ref()
        .map_or_else(|| "untitled".to_owned(), ToString::to_string);
    format!("{}: {title}", session.creation_selection.agent)
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

    use super::{PanelMode, contains, has_live_delegate, panel_visible, tree_rows};

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
                .map(|row| (row.depth, row.label.as_str()))
                .collect::<Vec<_>>(),
            [
                (0, "primary: untitled"),
                (1, "worker: untitled"),
                (2, "helper: untitled"),
            ]
        );
        assert!(rows[1].running && rows[1].has_children);
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
}
