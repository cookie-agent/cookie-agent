//! The `/config` panel: the daemon's `config.toml` and agent documents,
//! edited in `$VISUAL`/`$EDITOR` and applied with `config.apply`; creating
//! and deleting agents; and reloading hand edits with `config.reload`.

use std::collections::HashMap;

pub(in crate::ui) use cookie_agent_client_state::config::{ConfigApplyOutcome, ConfigRow};
use cookie_agent_client_state::config::{
    config_rows, document_text, new_agent_refusal, reload_status, restart_note,
};
use cookie_agent_protocol::{
    ConfigApplyParams, ConfigChange, ConfigGetResult, ConfigRevision, ConfigStatus,
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use super::{App, Modal, RpcUpdate, centered, paint_panel};
use crate::ui::input::InputState;

/// A document to open in the editor; the event loop runs the editor and
/// hands the result to [`App::finish_config_edit`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::ui) struct EditorRequest {
    pub(in crate::ui) path: String,
    pub(in crate::ui) text: String,
    /// The document's text on disk, `None` for a new one.
    pub(in crate::ui) original: Option<String>,
    pub(in crate::ui) expected_fingerprint: Option<String>,
    pub(in crate::ui) revision: ConfigRevision,
}

#[derive(Default)]
pub(in crate::ui) struct ConfigPanelState {
    /// `None` until `config.get` answers.
    pub(in crate::ui) config: Option<ConfigGetResult>,
    pub(in crate::ui) selection: ListState,
    pub(in crate::ui) refresh_in_flight: bool,
    /// Edits the daemon refused, by path: the next edit reopens them.
    pub(in crate::ui) drafts: HashMap<String, EditorRequest>,
    /// Why the last edit was refused.
    pub(in crate::ui) error: Option<String>,
    /// The path of a new agent document being typed.
    pub(in crate::ui) new_path: Option<InputState>,
    /// The document a delete waits to be confirmed for.
    pub(in crate::ui) confirm_delete: Option<String>,
}

impl ConfigPanelState {
    pub(in crate::ui) fn rows(&self) -> Vec<ConfigRow> {
        self.config.as_ref().map(config_rows).unwrap_or_default()
    }

    fn selected_row(&self) -> Option<ConfigRow> {
        self.selection
            .selected()
            .and_then(|index| self.rows().into_iter().nth(index))
    }
}

impl App {
    pub(super) fn open_config_panel(&mut self) {
        let drafts = std::mem::take(&mut self.config_panel.drafts);
        self.config_panel = ConfigPanelState {
            drafts,
            ..ConfigPanelState::default()
        };
        self.modal = Modal::Config;
        self.poll_config();
    }

    pub(super) fn poll_config(&mut self) {
        if self.modal != Modal::Config || self.config_panel.refresh_in_flight {
            return;
        }
        self.config_panel.refresh_in_flight = true;
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client.get_config().await.map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::ConfigLoaded { result });
        });
    }

    pub(super) fn install_config(&mut self, result: Result<ConfigGetResult, String>) {
        self.config_panel.refresh_in_flight = false;
        match result {
            Ok(config) => self.config_panel.config = Some(config),
            Err(error) => self.status = format!("configuration unavailable: {error}"),
        }
    }

    /// Opens `path` in the editor: a refused edit's text when there is one,
    /// else the document, else a new document's template.
    pub(in crate::ui) fn begin_config_edit(&mut self, path: &str) {
        if let Some(draft) = self.config_panel.drafts.get(path) {
            self.pending_editor = Some(draft.clone());
            return;
        }
        let Some(config) = &self.config_panel.config else {
            return;
        };
        let document = config
            .documents
            .iter()
            .find(|document| document.path == path);
        let original = document.map(|document| document.text.clone());
        self.pending_editor = Some(EditorRequest {
            path: path.to_owned(),
            text: document_text(config, path),
            original,
            expected_fingerprint: document.map(|document| document.fingerprint.clone()),
            revision: config.revision.clone(),
        });
    }

    /// What the editor left for `request`: applied unless unchanged.
    pub(in crate::ui) fn finish_config_edit(
        &mut self,
        request: EditorRequest,
        edited: Result<String, String>,
    ) {
        let text = match edited {
            Ok(text) => text,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        if request.original.as_deref() == Some(text.as_str()) {
            self.config_panel.drafts.remove(&request.path);
            self.status = format!("No changes to {}.", request.path);
            return;
        }
        let request = EditorRequest { text, ..request };
        self.status = format!("applying {}…", request.path);
        let change = ConfigChange::Put {
            path: request.path.clone(),
            text: request.text.clone(),
            expected_fingerprint: request.expected_fingerprint.clone(),
        };
        self.dispatch_config_apply(request.revision.clone(), change, Some(request));
    }

    fn dispatch_config_apply(
        &mut self,
        revision: ConfigRevision,
        change: ConfigChange,
        edit: Option<EditorRequest>,
    ) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let path = match &change {
            ConfigChange::Put { path, .. } | ConfigChange::Delete { path, .. } => path.clone(),
            ConfigChange::Set { key, .. } => key.join("."),
        };
        self.spawn_rpc(async move {
            let outcome = ConfigApplyOutcome::from_result(
                client
                    .apply_config(ConfigApplyParams {
                        expected_revision: revision,
                        changes: vec![change],
                    })
                    .await,
            );
            let _ = updates.send(RpcUpdate::ConfigApplied {
                path,
                edit,
                outcome,
            });
        });
    }

    pub(super) fn install_config_apply(
        &mut self,
        path: String,
        edit: Option<EditorRequest>,
        outcome: ConfigApplyOutcome,
    ) {
        let report = outcome.report(&path);
        match edit {
            Some(edit)
                if report.keep_draft && !matches!(outcome, ConfigApplyOutcome::Failed(_)) =>
            {
                self.config_panel.drafts.insert(path.clone(), edit);
            }
            _ if !report.keep_draft => {
                self.config_panel.drafts.remove(&path);
            }
            _ => {}
        }
        if !matches!(outcome, ConfigApplyOutcome::Failed(_)) {
            self.config_panel.error = report.error;
        }
        self.status = report.status;
        self.poll_config();
    }

    pub(super) fn handle_config_key(&mut self, key: KeyEvent) {
        if let Some(input) = self.config_panel.new_path.as_mut() {
            match key.code {
                KeyCode::Esc => self.config_panel.new_path = None,
                KeyCode::Enter => {
                    let path = input.as_str().trim().to_owned();
                    self.config_panel.new_path = None;
                    let refusal = self
                        .config_panel
                        .config
                        .as_ref()
                        .and_then(|config| new_agent_refusal(config, &path));
                    if let Some(refusal) = refusal {
                        self.status = refusal;
                    } else {
                        self.begin_config_edit(&path);
                    }
                }
                _ => super::keys::edit_plain_input(input, key),
            }
            return;
        }
        if let Some(path) = self.config_panel.confirm_delete.take() {
            if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                self.delete_config_document(&path);
            } else {
                self.status = "Delete cancelled.".into();
            }
            return;
        }
        let count = self.config_panel.rows().len();
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Up => {
                super::move_picker_selection(&mut self.config_panel.selection, count, true);
            }
            KeyCode::Down => {
                super::move_picker_selection(&mut self.config_panel.selection, count, false);
            }
            KeyCode::Enter => {
                if let Some(path) = self.config_panel.selected_row().and_then(|row| row.path) {
                    self.begin_config_edit(&path);
                }
            }
            KeyCode::Char('n') => {
                let prefix = match self.config_panel.selected_row().and_then(|row| row.preset) {
                    Some(preset) => format!("agents/{preset}/"),
                    None => "agents/".to_owned(),
                };
                let mut input = InputState::default();
                input.set_buffer(prefix);
                self.config_panel.new_path = Some(input);
            }
            KeyCode::Char('d') => match self.config_panel.selected_row().and_then(|row| row.path) {
                Some(path) if path != "config.toml" => {
                    self.config_panel.confirm_delete = Some(path);
                }
                _ => self.status = "only an agent document can be deleted".into(),
            },
            KeyCode::Char('r') => self.reload_config(),
            _ => {}
        }
    }

    fn delete_config_document(&mut self, path: &str) {
        let Some(config) = &self.config_panel.config else {
            return;
        };
        let Some(document) = config
            .documents
            .iter()
            .find(|document| document.path == path)
        else {
            return;
        };
        self.status = format!("deleting {path}…");
        let change = ConfigChange::Delete {
            path: path.to_owned(),
            expected_fingerprint: document.fingerprint.clone(),
        };
        self.dispatch_config_apply(config.revision.clone(), change, None);
    }

    fn reload_config(&mut self) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.status = "reloading the configuration…".into();
        self.spawn_rpc(async move {
            let update = match client.reload_config().await {
                Ok(result) => RpcUpdate::ConfigReloaded {
                    status: result.status,
                },
                Err(error) => RpcUpdate::failure(&error),
            };
            let _ = updates.send(update);
        });
    }

    pub(super) fn install_config_reload(&mut self, status: ConfigStatus) {
        self.status = reload_status(&status);
        self.poll_config();
    }

    pub(super) fn render_config_panel(&mut self, frame: &mut ratatui::Frame) {
        let area = centered(frame.area(), 80, 70);
        paint_panel(frame, area, &self.theme);
        let rows = self.config_panel.rows();
        let count = rows.len();
        self.config_panel.selection.select((count > 0).then(|| {
            self.config_panel
                .selection
                .selected()
                .unwrap_or(0)
                .min(count - 1)
        }));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);
        let items = rows
            .iter()
            .map(|row| {
                let style = if row.path.is_some() {
                    self.theme.body()
                } else {
                    self.theme.muted()
                };
                let draft = row
                    .path
                    .as_ref()
                    .is_some_and(|path| self.config_panel.drafts.contains_key(path));
                ListItem::new(Line::from(vec![
                    Span::styled(row.label.clone(), style),
                    Span::styled(
                        if draft { "  (not applied)" } else { "" },
                        self.theme.warning(),
                    ),
                ]))
            })
            .collect::<Vec<_>>();
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("> ")
                .highlight_style(self.theme.selected())
                .block(
                    crate::ui::panel_block()
                        .border_style(self.theme.panel_border())
                        .title(crate::ui::panel_title("Configuration"))
                        .title_bottom(crate::ui::panel_title(
                            Line::from(Span::styled(
                                "enter edit | n new agent | d delete | r reload | esc close",
                                self.theme.internal(),
                            ))
                            .right_aligned(),
                        )),
                ),
            chunks[0],
            &mut self.config_panel.selection,
        );
        let detail = self.config_detail();
        frame.render_widget(
            Paragraph::new(detail).wrap(Wrap { trim: false }).block(
                crate::ui::panel_block()
                    .border_style(self.theme.panel_border())
                    .title(crate::ui::panel_title("Status")),
            ),
            chunks[1],
        );
    }

    fn config_detail(&self) -> String {
        let panel = &self.config_panel;
        if let Some(input) = &panel.new_path {
            return format!(
                "New agent document (agents/<id>.md or agents/<preset>/<id>.md):\n{}\n\nEnter opens it in the editor; Esc cancels.",
                input.as_str()
            );
        }
        if let Some(path) = &panel.confirm_delete {
            return format!("Delete {path}? y deletes; any other key cancels.");
        }
        let Some(config) = &panel.config else {
            return "Loading the configuration...".into();
        };
        let mut lines = vec![format!(
            "Applied revision {}",
            short_revision(&config.status.applied_revision)
        )];
        if config.revision != config.status.applied_revision {
            lines.push("The files differ from the applied configuration; r reloads them.".into());
        }
        if let Some(error) = &config.status.error {
            lines.push(format!("The last reload failed: {error}"));
        }
        let restart = restart_note(&config.status);
        if !restart.is_empty() {
            lines.push(restart.trim().to_owned());
        }
        if let Some(error) = &panel.error {
            lines.push(String::new());
            lines.push(error.clone());
        }
        lines.push(String::new());
        lines.push(
            "Enter edits the selected document in $VISUAL or $EDITOR; saving applies it.".into(),
        );
        lines.join("\n")
    }
}

fn short_revision(revision: &ConfigRevision) -> &str {
    let hex = revision.as_str().trim_start_matches("sha256:");
    &hex[..hex.len().min(12)]
}

/// Runs the editor on `text`, the document `path`, with the terminal handed
/// over to it; the edited text, or why there is none.
pub(super) fn run_editor(path: &str, text: &str) -> Result<String, String> {
    let suffix = if path.ends_with(".md") {
        ".md"
    } else {
        ".toml"
    };
    let mut file = tempfile::Builder::new()
        .prefix("cookie-config-")
        .suffix(suffix)
        .tempfile()
        .map_err(|error| format!("create a file to edit: {error}"))?;
    std::io::Write::write_all(&mut file, text.as_bytes())
        .map_err(|error| format!("write the file to edit: {error}"))?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .filter(|editor| !editor.trim().is_empty())
        .unwrap_or_else(|| "vi".to_owned());
    let status = editor_command(&editor, file.path())
        .status()
        .map_err(|error| format!("run the editor `{editor}`: {error}"))?;
    if !status.success() {
        return Err(format!("the editor `{editor}` exited with {status}"));
    }
    std::fs::read_to_string(file.path()).map_err(|error| format!("read the edited file: {error}"))
}

/// The editor, which may carry arguments (`code --wait`), on `path`.
fn editor_command(editor: &str, path: &std::path::Path) -> std::process::Command {
    #[cfg(unix)]
    {
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(path);
        command
    }
    #[cfg(not(unix))]
    {
        let mut words = editor.split_whitespace();
        let mut command = std::process::Command::new(words.next().unwrap_or("notepad"));
        command.args(words).arg(path);
        command
    }
}
