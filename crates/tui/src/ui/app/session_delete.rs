//! The `/sessions` picker's session deletion, its action row (rename,
//! delete), and its choice between this directory's sessions and every
//! directory's.

use cookie_agent_client_state::session_delete::{
    self, DAEMON_TOO_OLD_TO_DELETE, SessionDeleteOutcome,
};

use super::*;

/// A button on the session picker's action row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum SessionPickerAction {
    /// Opens a title field for the highlighted session (Ctrl+R).
    Rename,
    /// Asks to delete the highlighted session (Ctrl+D).
    Delete,
    /// Switches between this directory's sessions and every directory's
    /// (Ctrl+A).
    ToggleScope,
    /// Cycles the project filter: all, each project, no project (Ctrl+P).
    CycleProject,
}

impl SessionPickerAction {
    fn label(self, all_directories: bool, project: &str) -> String {
        match self {
            Self::Rename => " Rename (ctrl+r) ".to_owned(),
            Self::Delete => " Delete (ctrl+d) ".to_owned(),
            Self::ToggleScope if all_directories => " This directory (ctrl+a) ".to_owned(),
            Self::ToggleScope => " All directories (ctrl+a) ".to_owned(),
            Self::CycleProject => format!(" {project} (ctrl+p) "),
        }
    }
}

/// A session delete waiting for the user to confirm it.
#[derive(Clone, Debug)]
pub(in crate::ui) struct SessionDeletePrompt {
    /// The root session as the picker showed it; its last event seq guards
    /// the delete.
    pub(in crate::ui) session: SessionMeta,
    /// How many subagent sessions go with it, once `session.tree` answered.
    pub(in crate::ui) subagents: Option<usize>,
}

impl App {
    /// The session picker's title for the directories and the project it
    /// lists.
    pub(super) fn session_scope_title(&self) -> String {
        let scope = if self.session_scope_all {
            "Sessions — all directories"
        } else {
            "Sessions — this directory"
        };
        if self.session_project_filter.is_some() {
            format!("{scope} · {}", self.session_project_filter_label())
        } else {
            scope.to_owned()
        }
    }

    /// Ctrl+A in the picker: lists every directory's sessions, or this
    /// directory's again.
    pub(in crate::ui) async fn toggle_session_scope(&mut self) {
        self.session_scope_all = !self.session_scope_all;
        self.note_sessions_changed();
        self.refresh_session_list().await;
        self.session_search_changed();
    }

    /// Ctrl+D, Delete, or the Delete button: asks to delete the highlighted
    /// session. Nothing is sent before the user confirms, and nothing at all
    /// to a daemon older than client protocol 1.5.
    pub(in crate::ui) fn begin_session_delete(&mut self) {
        let index = self.picker_state.selected().unwrap_or(0);
        let Some(session_id) = self.session_search_ids().get(index).copied() else {
            self.status = "There is no session to delete.".into();
            return;
        };
        if !session_delete::daemon_can_delete_sessions(self.client.daemon_version()) {
            self.status = DAEMON_TOO_OLD_TO_DELETE.into();
            return;
        }
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .cloned()
        else {
            return;
        };
        self.session_delete = Some(SessionDeletePrompt {
            session,
            subagents: None,
        });
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            if let Ok(result) = client
                .session_tree(cookie_agent_protocol::SessionTreeParams { session_id })
                .await
            {
                let _ = updates.send(RpcUpdate::SessionDeleteSubagents {
                    session_id,
                    subagents: subagent_count(&result.tree),
                });
            }
        });
    }

    /// Records how many subagents the waiting delete takes with it.
    pub(super) fn note_session_delete_subagents(&mut self, session_id: SessionId, count: usize) {
        if let Some(prompt) = self.session_delete.as_mut()
            && prompt.session.session_id == session_id
        {
            prompt.subagents = Some(count);
        }
    }

    /// Answers the waiting delete from its keys or its buttons.
    pub(in crate::ui) fn answer_session_delete(&mut self, button: ConfirmButton) {
        let Some(prompt) = self.session_delete.take() else {
            return;
        };
        let title = session_title(&prompt.session);
        if button == ConfirmButton::Cancel {
            self.status = format!("Kept {title}.");
            return;
        }
        self.status = format!("Deleting {title}…");
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let outcome = session_delete::delete_session(&client, &prompt.session).await;
            let _ = updates.send(RpcUpdate::SessionDeleted { title, outcome });
        });
    }

    /// Shows how a delete ended. The sessions themselves leave every view
    /// through the `sessions.changed` removals the daemon sends.
    pub(super) fn finish_session_delete(&mut self, title: String, outcome: SessionDeleteOutcome) {
        match outcome {
            SessionDeleteOutcome::Deleted(deleted) => {
                for session_id in deleted {
                    self.forget_removed_session(session_id);
                }
                self.status = format!("Deleted {title}.");
            }
            SessionDeleteOutcome::DaemonTooOld => self.status = DAEMON_TOO_OLD_TO_DELETE.into(),
            SessionDeleteOutcome::Failed(error) => {
                self.handle_rpc_update(RpcUpdate::failure(&error));
            }
        }
    }

    /// Forgets a session the daemon removed. When it is the viewed one, or
    /// the root of the viewed tree, the TUI leaves it for a new session's
    /// draft, as it starts without one.
    pub(super) fn forget_removed_session(&mut self, session_id: SessionId) {
        let before = self.sessions.len();
        self.sessions
            .retain(|session| session.session_id != session_id);
        if self.sessions.len() != before {
            self.note_sessions_changed();
        }
        self.subscriptions.release(session_id);
        self.store.forget_session(session_id);
        if self
            .session_delete
            .as_ref()
            .is_some_and(|prompt| prompt.session.session_id == session_id)
        {
            self.session_delete = None;
        }
        if self.tree_root == Some(session_id) {
            self.leave_deleted_session();
        } else if self.selected == Some(session_id) {
            match self.tree_root {
                Some(root) => self.watch_session(root),
                None => self.leave_deleted_session(),
            }
        }
        self.follow_composer_slot();
        self.forget_composer_draft(session_id);
    }

    fn leave_deleted_session(&mut self) {
        self.selected = None;
        self.tree_root = None;
        self.tree = None;
        self.tree_cursor = None;
        self.tree_refresh_in_flight = None;
        self.tree_refresh_pending = false;
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.conversation_scroll = ConversationScroll::default();
        self.scrollbar_geometry = None;
        self.scrollbar_drag = None;
        if matches!(self.selection, Some(TextSelection::Conversation { .. })) {
            self.selection = None;
        }
        self.new_session_draft = self.default_draft_selection();
        self.status = "The session was deleted.".into();
    }

    /// The picker's action row along its bottom edge, or the waiting
    /// delete's confirmation over the picker.
    pub(super) fn render_session_picker_actions(
        &mut self,
        frame: &mut ratatui::Frame,
        row: Rect,
        screen: Rect,
    ) {
        self.hit_map.session_picker_actions.clear();
        if let Some(prompt) = self.session_delete.clone() {
            self.render_session_delete_confirm(frame, &prompt, screen);
            return;
        }
        let mut spans = Vec::new();
        let mut x = row.x;
        let project = self.session_project_filter_label();
        let actions = [
            SessionPickerAction::Rename,
            SessionPickerAction::Delete,
            SessionPickerAction::ToggleScope,
            SessionPickerAction::CycleProject,
        ];
        let shown = if self.daemon_has_projects() {
            &actions[..]
        } else {
            &actions[..3]
        };
        for &action in shown {
            let label = action.label(self.session_scope_all, &project);
            let width = u16::try_from(UnicodeWidthStr::width(label.as_str())).unwrap_or(u16::MAX);
            if x.saturating_add(width) <= row.right() {
                self.hit_map
                    .session_picker_actions
                    .push((Rect::new(x, row.y, width, 1), action));
            }
            spans.push(Span::styled(label, self.theme.link()));
            spans.push(Span::raw(" "));
            x = x.saturating_add(width + 1);
        }
        spans.push(Span::styled(
            "enter open · esc close",
            self.theme.internal(),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), row);
    }

    fn render_session_delete_confirm(
        &mut self,
        frame: &mut ratatui::Frame,
        prompt: &SessionDeletePrompt,
        screen: Rect,
    ) {
        let theme = &self.theme;
        let title = session_title(&prompt.session);
        let subagents = match prompt.subagents {
            Some(0) => "It has no subagents.".to_owned(),
            Some(1) => "Its 1 subagent session goes with it.".to_owned(),
            Some(count) => format!("Its {count} subagent sessions go with it."),
            None => "Every subagent session of it goes with it.".to_owned(),
        };
        let body = |width: u16| {
            let mut lines = vec![
                Line::styled(format!("Delete {title}?"), theme.heading()),
                Line::default(),
            ];
            lines.extend(wrapped_line(
                Line::styled(
                    format!(
                        "{subagents} Its messages, tool output, and attachments are removed \
                         from disk, and this cannot be undone. To keep a copy, fork it first."
                    ),
                    theme.muted_text(),
                ),
                width,
            ));
            lines
        };
        self.hit_map.confirm_buttons =
            render_confirm_panel(frame, screen, "Delete session", body, theme);
    }

    /// Keys while a delete waits for its answer: y or Enter deletes, n or
    /// Esc keeps the session. Returns whether a prompt took the key.
    pub(super) fn handle_session_delete_key(&mut self, key: KeyEvent) -> bool {
        if self.session_delete.is_none() {
            return false;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                self.answer_session_delete(ConfirmButton::Cancel);
            }
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                self.answer_session_delete(ConfirmButton::Confirm);
            }
            _ => {}
        }
        true
    }

    /// Acts on a picker action from its button.
    pub(in crate::ui) async fn activate_session_picker_action(
        &mut self,
        action: SessionPickerAction,
    ) {
        match action {
            SessionPickerAction::Rename => self.begin_session_rename(),
            SessionPickerAction::Delete => self.begin_session_delete(),
            SessionPickerAction::ToggleScope => self.toggle_session_scope().await,
            SessionPickerAction::CycleProject => self.cycle_session_project_filter(),
        }
    }
}

/// A session's title as the picker names it.
fn session_title(session: &SessionMeta) -> String {
    session.title.as_ref().map_or_else(
        || {
            format!(
                "the untitled session {}",
                crate::ui::pickers::short_id(session)
            )
        },
        |title| format!("“{title}”"),
    )
}

/// How many sessions `tree` holds below its root.
fn subagent_count(tree: &cookie_agent_protocol::SessionTree) -> usize {
    tree.children
        .iter()
        .map(|child| 1 + subagent_count(child))
        .sum()
}
