//! Renaming sessions (`docs/site/specs/projects.md` section 8): `/rename`
//! with a title, with `--auto` for the generated title, or alone for a
//! field holding the current title; and Ctrl+R in the session picker, for
//! the highlighted session. Enter on an emptied field goes back to the
//! automatic title.

use cookie_agent_client_state::session_rename::{self, RenameCommand, TitleError};
use cookie_agent_protocol::SessionRenameChange;

use super::*;
use crate::ui::{
    input::InputState,
    slash::{PaletteStep, TextTarget},
};

/// The session picker's title field for the highlighted session, over the
/// picker's search.
pub(in crate::ui) struct SessionRenamePrompt {
    pub(in crate::ui) session_id: SessionId,
    pub(in crate::ui) input: InputState,
}

/// What a title field's key hint says.
pub(in crate::ui) const TITLE_HINT: &str =
    "enter: rename · empty enter: automatic title · esc: back";

impl App {
    /// The title `session_id` has now, as its title field starts.
    fn current_session_title(&self, session_id: SessionId) -> String {
        self.sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .map(session_rename::current_title)
            .or_else(|| {
                self.tree
                    .as_ref()
                    .and_then(|tree| find_session(tree, session_id))
                    .map(session_rename::current_title)
            })
            .unwrap_or_default()
    }

    /// A field holding `session_id`'s title, its caret at the end.
    fn title_input(&self, session_id: SessionId) -> InputState {
        let mut input = InputState::default();
        input.set_buffer(self.current_session_title(session_id));
        input.move_buffer_end();
        input
    }

    /// `/rename` chosen in the palette, with the text typed after it: a
    /// title, `--auto`, or nothing, which asks for the title.
    pub(in crate::ui) fn run_rename_command(&mut self, arguments: &str) {
        if self.new_session_draft.is_some() {
            self.status = "Send the first message before renaming the session.".into();
            return;
        }
        let Some(session_id) = self.selected else {
            self.status = "Select a session to rename.".into();
            return;
        };
        match session_rename::rename_command(arguments) {
            Ok(RenameCommand::Prompt) => {
                let input = self.title_input(session_id);
                if let Some(palette) = self.palette.as_mut() {
                    palette.steps.push(PaletteStep::Text {
                        target: TextTarget::SessionTitle { session_id },
                        input,
                    });
                }
            }
            Ok(RenameCommand::Change(change)) => {
                self.close_command_palette();
                self.send_session_rename(session_id, change);
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    /// Enter in a title field: renames to what it holds, or goes back to
    /// the automatic title when it is empty. Returns whether it sent the
    /// rename; a title the daemon would refuse stays to be fixed.
    pub(in crate::ui) fn submit_session_title(
        &mut self,
        session_id: SessionId,
        text: &str,
    ) -> bool {
        let change = match session_rename::title_change(text) {
            Ok(change) => change,
            Err(TitleError::Empty) => SessionRenameChange::Reset,
            Err(error) => {
                self.status = error.to_string();
                return false;
            }
        };
        self.send_session_rename(session_id, change);
        true
    }

    fn send_session_rename(&mut self, session_id: SessionId, change: SessionRenameChange) {
        self.status = "Renaming the session…".into();
        let params = session_rename::rename_params(
            session_id,
            change.clone(),
            &uuid::Uuid::now_v7().to_string(),
        );
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = session_rename::rename_session(&client, params).await;
            let _ = updates.send(RpcUpdate::SessionRenamed {
                change,
                result: Box::new(result),
            });
        });
    }

    /// Shows how a rename ended, and the title it left in the session list
    /// at once; `sessions.changed` tells every other client.
    pub(super) fn finish_session_rename(
        &mut self,
        change: SessionRenameChange,
        result: Result<SessionMeta, ClientError>,
    ) {
        match result {
            Ok(session) => {
                let session = self.merge_session_meta(session);
                if let Some(existing) = self
                    .sessions
                    .iter_mut()
                    .find(|existing| existing.session_id == session.session_id)
                {
                    *existing = session;
                    self.note_sessions_changed();
                }
                self.status = session_rename::renamed_status(&change);
            }
            Err(error) => self.handle_rpc_update(RpcUpdate::failure(&error)),
        }
    }

    /// Ctrl+R or the Rename button in the picker: a title field for the
    /// highlighted session, over the picker's search.
    pub(in crate::ui) fn begin_session_rename(&mut self) {
        let index = self.picker_state.selected().unwrap_or(0);
        let Some(session_id) = self.session_search_ids().get(index).copied() else {
            self.status = "There is no session to rename.".into();
            return;
        };
        let input = self.title_input(session_id);
        self.session_rename = Some(SessionRenamePrompt { session_id, input });
    }

    /// Keys while the picker's title field is open: Enter renames, Esc
    /// goes back to the picker, and the rest edit the title. Returns
    /// whether the field took the key.
    pub(super) fn handle_session_rename_key(&mut self, key: KeyEvent) -> bool {
        let Some(prompt) = self.session_rename.as_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Esc => self.session_rename = None,
            KeyCode::Enter => {
                let (session_id, text) = (prompt.session_id, prompt.input.as_str().to_owned());
                if self.submit_session_title(session_id, &text) {
                    self.session_rename = None;
                }
            }
            _ => keys::edit_plain_input(&mut prompt.input, key),
        }
        true
    }

    /// Paste into the picker's title field as one line.
    pub(super) fn paste_into_session_rename(&mut self, text: &str) -> bool {
        let Some(prompt) = self.session_rename.as_mut() else {
            return false;
        };
        prompt
            .input
            .insert_text(&text.replace("\r\n", " ").replace(['\r', '\n'], " "));
        true
    }

    /// The picker's title field over its search, at the top of `area`.
    pub(super) fn render_session_rename(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let theme = self.theme.clone();
        let Some(prompt) = self.session_rename.as_mut() else {
            return;
        };
        crate::ui::slash::render_text(
            frame,
            area,
            "Rename session",
            "The session's title; empty for the automatic one",
            TITLE_HINT,
            &mut prompt.input,
            &theme,
        );
    }
}
