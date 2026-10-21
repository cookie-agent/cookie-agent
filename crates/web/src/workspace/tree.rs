//! The subagent tree: which tree the viewed session belongs to, reading it
//! (`session.tree`) when delegations change it, and following its live
//! sessions so their status stays current, as the TUI does.

use cookie_agent_client_state::subscriptions::SubscribeIntent;
use cookie_agent_protocol::{
    SessionGetParams, SessionId, SessionOrigin, SessionStatus, SessionTree, SessionTreeParams,
};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;
use crate::{
    browser,
    tree::{TreeRow, contains, expanded_from_storage, expanded_key, running_rows, tree_rows},
};

impl Workspace {
    /// Whether `session_id` is in the tree of the viewed session.
    pub(super) fn in_viewed_tree(self, session_id: SessionId) -> bool {
        self.tree_root.get_untracked() == Some(session_id)
            || self
                .tree
                .with_untracked(|tree| tree.as_ref().is_some_and(|tree| contains(tree, session_id)))
    }

    /// Makes `session_id` the root of the tree the page shows.
    pub(super) fn reroot_tree(self, session_id: SessionId) {
        if self.tree_root.get_untracked() != Some(session_id) {
            self.tree_expanded.set(expanded_from_storage(
                browser::load(&expanded_key(session_id)).as_deref(),
            ));
            self.collapsed.set(Default::default());
        }
        self.tree_root.set(Some(session_id));
        self.tree.set(None);
        self.refresh_tree();
    }

    /// Finds the tree `session_id` belongs to: the one shown, a root of the
    /// session list, or the root its origin names.
    pub(super) fn locate_tree(self, session_id: SessionId) {
        if self.in_viewed_tree(session_id) {
            self.refresh_tree();
            return;
        }
        let known = self.sessions.with_untracked(|sessions| {
            sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map(|session| session.origin.clone())
        });
        match known {
            Some(SessionOrigin::Delegated {
                root_session_id, ..
            }) => self.reroot_tree(root_session_id),
            Some(_) => self.reroot_tree(session_id),
            None => {
                let Some(client) = self.client() else {
                    return;
                };
                spawn_local(async move {
                    let root = match client.get_session(SessionGetParams { session_id }).await {
                        Ok(result) => match result.session.origin {
                            SessionOrigin::Delegated {
                                root_session_id, ..
                            } => root_session_id,
                            _ => session_id,
                        },
                        Err(_) => session_id,
                    };
                    if self.selected.get_untracked() == Some(session_id) {
                        self.reroot_tree(root);
                    }
                });
            }
        }
    }

    /// Reads the shown tree again; an older answer is dropped.
    pub(super) fn refresh_tree(self) {
        let (Some(client), Some(root)) = (self.client(), self.tree_root.get_untracked()) else {
            return;
        };
        self.tree_request.update_value(|request| *request += 1);
        let request = self.tree_request.get_value();
        spawn_local(async move {
            let result = client
                .session_tree(SessionTreeParams { session_id: root })
                .await;
            if self.tree_request.get_value() != request
                || self.tree_root.get_untracked() != Some(root)
            {
                return;
            }
            match result {
                Ok(result) => {
                    self.follow_live_sessions(&result.tree);
                    self.tree.set(Some(result.tree));
                }
                Err(error) => self.status.set(format!(
                    "session tree unavailable: {}",
                    crate::errors::describe(&error)
                )),
            }
        });
    }

    /// Subscribes the tree's live sessions (running or idle), so their
    /// status and titles stay current; a finished one is replayed only when
    /// it is opened. They share the root's ownership.
    fn follow_live_sessions(self, tree: &SessionTree) {
        let owned = self.owned(tree.session.session_id);
        let mut live = Vec::new();
        collect_live(tree, &mut live);
        for session_id in live {
            if owned {
                self.owned.update_value(|owned| {
                    owned.insert(session_id);
                });
            }
            let classifying = self
                .ownership_classifications
                .with_value(|classifications| classifications.contains_key(&session_id));
            if classifying {
                continue;
            }
            if let Some(subscribe) = self
                .subscriptions
                .try_update_value(|subscriptions| {
                    subscriptions.subscribe(session_id, owned, SubscribeIntent::Follow)
                })
                .flatten()
            {
                self.send_subscribe(subscribe);
            }
        }
    }

    /// Switches the open session's tree between collapsed (running
    /// subagents only) and expanded (all of them), remembered in the
    /// browser.
    pub fn toggle_tree(self) {
        let Some(root) = self.tree_root.get_untracked() else {
            return;
        };
        let expanded = !self.tree_expanded.get_untracked();
        if expanded {
            browser::store(&expanded_key(root), "expanded");
        } else {
            browser::forget(&expanded_key(root));
        }
        self.tree_expanded.set(expanded);
    }

    /// Folds or unfolds `session_id`'s subagents.
    pub fn toggle_folded(self, session_id: SessionId) {
        self.collapsed.update(|collapsed| {
            if !collapsed.remove(&session_id) {
                collapsed.insert(session_id);
            }
        });
    }

    /// The subagent rows under the open session's row, its own row
    /// excluded: all of them when the tree is expanded, else the running
    /// ones and the viewed one, under their dimmed ancestors.
    pub fn subagent_rows(self) -> Vec<TreeRow> {
        let expanded = self.tree_expanded.get();
        let viewed = self.selected.get();
        let collapsed = self.collapsed.get();
        self.tree
            .with(|tree| {
                tree.as_ref().map(|tree| {
                    if expanded {
                        tree_rows(tree, &collapsed)
                    } else {
                        running_rows(tree, viewed)
                    }
                })
            })
            .unwrap_or_default()
            .into_iter()
            .filter(|row| row.depth > 0)
            .collect()
    }
}

fn collect_live(tree: &SessionTree, sessions: &mut Vec<SessionId>) {
    if matches!(
        tree.session.status,
        SessionStatus::Running | SessionStatus::Idle
    ) {
        sessions.push(tree.session.session_id);
    }
    for child in &tree.children {
        collect_live(child, sessions);
    }
}
