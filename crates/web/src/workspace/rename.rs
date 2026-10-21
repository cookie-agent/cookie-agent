//! Renaming sessions (`session.rename`): the title bar's field (a click
//! on the title, or ⋮ › Rename) and the session rows' pencil. Enter on a
//! field holding a title sets it; an emptied one goes back to the
//! automatic title. The shared rules are `client_state::session_rename`.

use cookie_agent_client_state::session_rename::{self, TitleError};
use cookie_agent_protocol::{SessionId, SessionMeta, SessionRenameChange};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;

impl Workspace {
    /// The session `session_id` as the page knows it: in the list, or in
    /// the viewed tree.
    fn known_session(self, session_id: SessionId) -> Option<SessionMeta> {
        self.sessions
            .with_untracked(|sessions| {
                sessions
                    .iter()
                    .find(|session| session.session_id == session_id)
                    .cloned()
            })
            .or_else(|| {
                self.tree.with_untracked(|tree| {
                    tree.as_ref()
                        .and_then(|tree| crate::tree::find(tree, session_id))
                        .cloned()
                })
            })
    }

    /// What `session_id`'s title field starts with: its title, else
    /// nothing.
    pub fn title_draft(self, session_id: SessionId) -> String {
        self.known_session(session_id)
            .as_ref()
            .map(session_rename::current_title)
            .unwrap_or_default()
    }

    /// Whether the title bar's title can be edited: a session the page
    /// views and may write to.
    pub fn title_editable(self) -> bool {
        !self.new_session.get()
            && self.selected.get().is_some_and(|session_id| {
                !self
                    .read_only
                    .with(|read_only| read_only.contains(&session_id))
            })
    }

    /// A click on the title bar's title, or ⋮ › Rename: its field, holding
    /// the title.
    pub fn begin_title_edit(self) {
        if !untrack(|| self.title_editable()) {
            self.status.set("open a session to rename it".into());
            return;
        }
        let Some(session_id) = self.selected.get_untracked() else {
            return;
        };
        self.title_edit.set(Some(self.title_draft(session_id)));
    }

    /// Enter in a title field of `session_id`: renames to the title it
    /// holds, or goes back to the automatic one when it is empty. Returns
    /// whether it sent the rename; a title the daemon would refuse stays
    /// in the field, its reason in the status line.
    pub fn submit_title(self, session_id: SessionId, text: &str) -> bool {
        let change = match session_rename::title_change(text) {
            Ok(change) => change,
            Err(TitleError::Empty) => SessionRenameChange::Reset,
            Err(error) => {
                self.status.set(error.to_string());
                return false;
            }
        };
        self.rename_session(session_id, change);
        true
    }

    /// Sends `change` for `session_id`. The answer is the session as it
    /// left it, which the list shows at once; `sessions.changed` tells the
    /// other clients. A refusal shows under the session's row.
    pub fn rename_session(self, session_id: SessionId, change: SessionRenameChange) {
        let Some(client) = self.client() else {
            return;
        };
        self.rename_error.set(None);
        self.status.set("renaming the session…".into());
        let params = session_rename::rename_params(
            session_id,
            change.clone(),
            &uuid::Uuid::now_v7().to_string(),
        );
        spawn_local(async move {
            match session_rename::rename_session(&client, params).await {
                Ok(session) => {
                    let listed = self.sessions.try_update(|sessions| {
                        sessions
                            .iter_mut()
                            .find(|existing| existing.session_id == session.session_id)
                            .map(|existing| *existing = session)
                            .is_some()
                    });
                    if listed != Some(true) && self.tree.with_untracked(Option::is_some) {
                        // A subagent's title is in the tree.
                        self.refresh_tree();
                    }
                    self.status.set(match change {
                        SessionRenameChange::Set { title } => {
                            format!("renamed the session to “{title}”")
                        }
                        SessionRenameChange::Clear | SessionRenameChange::Reset => {
                            "the session uses its automatic title again".to_owned()
                        }
                    });
                }
                Err(error) => {
                    let message = crate::errors::describe(&error);
                    self.status.set(format!("rename failed: {message}"));
                    self.rename_error.set(Some((session_id, message)));
                }
            }
        });
    }
}
