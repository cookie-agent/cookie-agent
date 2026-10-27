//! Keeping a shown subagent tree current as sessions join it.
//!
//! A child joins its parent's tree in two ways. A delegation of cookie's
//! records the link in the parent's log (`tool_call_linked`), which the
//! clients watch. An ACP agent's own subagent records nothing in its parent:
//! its session only appears, announced by `sessions.changed` `upserted`. So
//! a client re-reads the tree it shows when an upserted session belongs to
//! that tree and the tree does not have it yet.

use cookie_agent_protocol::{SessionId, SessionMeta, SessionOrigin, SessionTree};

/// Whether `session_id` is `tree`'s root or one of its descendants.
#[must_use]
pub fn contains(tree: &SessionTree, session_id: SessionId) -> bool {
    tree.session.session_id == session_id
        || tree
            .children
            .iter()
            .any(|child| contains(child, session_id))
}

/// Whether `session`, just announced, is a session of the tree rooted at
/// `root` that `shown` (that tree as last read) lacks: the tree must be read
/// again to show it.
#[must_use]
pub fn joins(root: Option<SessionId>, shown: Option<&SessionTree>, session: &SessionMeta) -> bool {
    let SessionOrigin::Delegated {
        root_session_id, ..
    } = &session.origin
    else {
        return false;
    };
    root == Some(*root_session_id) && !shown.is_some_and(|tree| contains(tree, session.session_id))
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{RunId, SessionId, SessionOrigin, SessionTree, ToolCallId};

    use super::joins;

    fn meta(origin: SessionOrigin) -> cookie_agent_protocol::SessionMeta {
        let revision = format!("sha256:{}", "a".repeat(64));
        let mut meta: cookie_agent_protocol::SessionMeta =
            serde_json::from_value(serde_json::json!({
                "session_id": SessionId::new_v7(),
                "origin": {"type": "root"},
                "short_id": null,
                "cwd_identity": "/workspace",
                "creation_selection": {
                    "agent": "claude",
                    "model": {"model": "acp/claude", "variant": null},
                    "preset": null
                },
                "runtime_revision": revision,
                "catalog_revision": revision,
                "provider_state_revision": revision,
                "model_revision": revision,
                "agent_revision": revision,
                "recipe_registry_revision": revision,
                "manifest_revision": revision,
                "title": null,
                "title_updated_seq": 0,
                "last_event_seq": 1,
                "last_activity": "2026-10-26T12:00:00Z",
                "status": "running",
                "skipped_events": []
            }))
            .expect("session meta");
        meta.origin = origin;
        meta
    }

    fn child_of(root: SessionId) -> cookie_agent_protocol::SessionMeta {
        meta(SessionOrigin::Delegated {
            root_session_id: root,
            parent_session_id: root,
            parent_run_id: RunId::new_v7(),
            parent_tool_call_id: ToolCallId::new_v7(),
            invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
            depth: 1,
        })
    }

    #[test]
    fn a_new_child_of_the_shown_tree_joins_it_once() {
        let root = meta(SessionOrigin::Root);
        let root_id = root.session_id;
        let mut tree = SessionTree {
            session: root.clone(),
            children: Vec::new(),
        };
        let child = child_of(root_id);
        assert!(joins(Some(root_id), Some(&tree), &child));
        assert!(joins(Some(root_id), None, &child), "a tree not read yet");
        tree.children.push(SessionTree {
            session: child.clone(),
            children: Vec::new(),
        });
        assert!(!joins(Some(root_id), Some(&tree), &child), "already shown");
        // Another tree's child, and a root, join nothing shown.
        assert!(!joins(
            Some(root_id),
            Some(&tree),
            &child_of(SessionId::new_v7())
        ));
        assert!(!joins(Some(root_id), Some(&tree), &root));
        assert!(!joins(None, None, &child));
    }
}
