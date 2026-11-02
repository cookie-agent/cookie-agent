//! The `/agents` panel (`docs/site/specs/acp-onboarding.md` section 7):
//! every agent document with where it comes from and why its agent cannot
//! run, and Customize (or Open), turning an agent off or on, Reset, and
//! Replace. Customize, Open, and Replace end on the document in the editor.

use cookie_agent_client_state::agent_documents::{
    AgentDocumentAction, actions, group, origin_label, unavailable_line,
};
use cookie_agent_protocol::{
    AgentDocumentInfo, AgentDocumentParams, AgentsDocumentsParams, AgentsSetEnabledParams,
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use super::{App, Modal, RpcUpdate, centered, paint_panel};

#[derive(Debug, Default)]
pub(in crate::ui) struct AgentDocumentsPanelState {
    /// `agents.documents`; `None` until it answers.
    pub(in crate::ui) documents: Option<Result<Vec<AgentDocumentInfo>, String>>,
    pub(in crate::ui) selection: ListState,
    /// An action that asks first, waiting for its key again.
    pub(in crate::ui) confirm: Option<(String, AgentDocumentAction)>,
}

/// The key that does `action` in the panel.
const fn action_key(action: AgentDocumentAction) -> &'static str {
    match action {
        AgentDocumentAction::Customize | AgentDocumentAction::Open => "enter",
        AgentDocumentAction::Disable | AgentDocumentAction::Enable => "d",
        AgentDocumentAction::Reset => "x",
        AgentDocumentAction::Replace => "R",
    }
}

impl App {
    pub(super) fn open_agent_documents_panel(&mut self) {
        self.agent_documents_panel = AgentDocumentsPanelState::default();
        self.modal = Modal::AgentDocuments;
        self.poll_agent_documents();
    }

    pub(super) fn poll_agent_documents(&mut self) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .agent_documents(AgentsDocumentsParams {})
                .await
                .map(|result| result.documents)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::AgentDocumentsListed { result });
        });
    }

    pub(super) fn install_agent_documents(
        &mut self,
        result: Result<Vec<AgentDocumentInfo>, String>,
    ) {
        self.agent_documents_panel.documents = Some(result);
    }

    /// The rows in the panel's order: by group, then ID.
    fn agent_document_rows(&self) -> Vec<&AgentDocumentInfo> {
        let mut rows = match &self.agent_documents_panel.documents {
            Some(Ok(documents)) => documents.iter().collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        rows.sort_by(|left, right| {
            (group(left), &left.preset, &left.id).cmp(&(group(right), &right.preset, &right.id))
        });
        rows
    }

    fn selected_agent_document(&self) -> Option<AgentDocumentInfo> {
        let index = self.agent_documents_panel.selection.selected()?;
        self.agent_document_rows()
            .get(index)
            .map(|info| (*info).clone())
    }

    pub(super) fn render_agent_documents_panel(&mut self, frame: &mut ratatui::Frame) {
        let area = centered(frame.area(), 84, 76);
        paint_panel(frame, area, &self.theme);
        let rows = self
            .agent_document_rows()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let count = rows.len();
        self.agent_documents_panel
            .selection
            .select((count > 0).then(|| {
                self.agent_documents_panel
                    .selection
                    .selected()
                    .unwrap_or(0)
                    .min(count - 1)
            }));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(area);
        let items = rows
            .iter()
            .map(|info| {
                let name = match &info.preset {
                    Some(preset) => format!("{preset}/{}", info.id),
                    None => info.id.clone(),
                };
                let mut spans = vec![
                    Span::styled(name, self.theme.body()),
                    Span::styled(
                        format!("  {} · {}", group(info).label(), origin_label(info)),
                        self.theme.muted(),
                    ),
                ];
                if unavailable_line(info, &self.agents).is_some() {
                    spans.push(Span::styled("  cannot run", self.theme.warning()));
                }
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("> ")
                .highlight_style(self.theme.selected())
                .block(
                    crate::ui::panel_block()
                        .border_style(self.theme.panel_border())
                        .title(crate::ui::panel_title("Agents"))
                        .title_bottom(crate::ui::panel_title(
                            Line::from(Span::styled(
                                "enter customize/open | d off/on | x reset | R replace | esc close",
                                self.theme.internal(),
                            ))
                            .right_aligned(),
                        )),
                ),
            chunks[0],
            &mut self.agent_documents_panel.selection,
        );
        let mut lines = Vec::new();
        match (
            &self.agent_documents_panel.documents,
            self.selected_agent_document(),
        ) {
            (None, _) => lines.push(Line::styled("Loading the agents…", self.theme.muted())),
            (Some(Err(error)), _) => {
                lines.push(Line::styled(error.clone(), self.theme.error()));
            }
            (Some(Ok(_)), None) => lines.push(Line::styled("No agents.", self.theme.muted())),
            (Some(Ok(_)), Some(info)) => {
                lines.push(Line::styled(
                    format!("{} · {}", info.id, origin_label(&info)),
                    self.theme.body(),
                ));
                if let Some(path) = &info.path {
                    lines.push(Line::styled(
                        format!("Your document: {path}"),
                        self.theme.muted(),
                    ));
                }
                if let Some(inherits) = &info.inherits {
                    lines.push(Line::styled(
                        format!("Inherits: {inherits}"),
                        self.theme.muted(),
                    ));
                }
                if let Some(reason) = unavailable_line(&info, &self.agents) {
                    lines.push(Line::styled(
                        format!("Cannot run: {reason}"),
                        self.theme.warning(),
                    ));
                }
                let offered = actions(&info)
                    .into_iter()
                    .map(|action| {
                        let asking = self.agent_documents_panel.confirm.as_ref()
                            == Some(&(info.id.clone(), action));
                        if asking {
                            format!(
                                "{} again to {}",
                                action_key(action),
                                action.label().to_lowercase()
                            )
                        } else {
                            format!("{} ({})", action.label(), action_key(action))
                        }
                    })
                    .collect::<Vec<_>>();
                if !offered.is_empty() {
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(offered.join("   "), self.theme.link()));
                }
            }
        }
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                crate::ui::panel_block()
                    .border_style(self.theme.panel_border())
                    .title(crate::ui::panel_title("Agent")),
            ),
            chunks[1],
        );
    }

    pub(in crate::ui) fn handle_agent_documents_key(&mut self, key: KeyEvent) {
        let count = self.agent_document_rows().len();
        let action = match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                return;
            }
            KeyCode::Up => {
                self.agent_documents_panel.confirm = None;
                super::move_picker_selection(
                    &mut self.agent_documents_panel.selection,
                    count,
                    true,
                );
                return;
            }
            KeyCode::Down => {
                self.agent_documents_panel.confirm = None;
                super::move_picker_selection(
                    &mut self.agent_documents_panel.selection,
                    count,
                    false,
                );
                return;
            }
            KeyCode::Enter => {
                [AgentDocumentAction::Customize, AgentDocumentAction::Open].as_slice()
            }
            KeyCode::Char('d') => {
                [AgentDocumentAction::Disable, AgentDocumentAction::Enable].as_slice()
            }
            KeyCode::Char('x') => [AgentDocumentAction::Reset].as_slice(),
            KeyCode::Char('R') => [AgentDocumentAction::Replace].as_slice(),
            _ => return,
        };
        let Some(info) = self.selected_agent_document() else {
            return;
        };
        let Some(action) = actions(&info)
            .into_iter()
            .find(|offered| action.contains(offered))
        else {
            self.status = format!("`{}` offers no such action", info.id);
            return;
        };
        let key = (info.id.clone(), action);
        if action.confirms() && self.agent_documents_panel.confirm.as_ref() != Some(&key) {
            self.agent_documents_panel.confirm = Some(key);
            return;
        }
        self.agent_documents_panel.confirm = None;
        self.run_agent_document_action(action, info);
    }

    /// Opens the user's document `path` in the editor.
    fn fetch_agent_document_for_editor(&mut self, path: String) {
        self.fetch_agent_document(path, String::new());
    }

    fn run_agent_document_action(&mut self, action: AgentDocumentAction, info: AgentDocumentInfo) {
        if action == AgentDocumentAction::Open {
            if let Some(path) = info.path {
                self.fetch_agent_document_for_editor(path);
            }
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let params = AgentDocumentParams {
                id: info.id.clone(),
                preset: info.preset.clone(),
            };
            let result = match action {
                AgentDocumentAction::Customize => client.customize_agent(params).await,
                AgentDocumentAction::Disable | AgentDocumentAction::Enable => {
                    client
                        .set_agent_enabled(AgentsSetEnabledParams {
                            id: info.id.clone(),
                            preset: info.preset.clone(),
                            enabled: action == AgentDocumentAction::Enable,
                        })
                        .await
                }
                AgentDocumentAction::Reset => client.reset_agent(params).await,
                AgentDocumentAction::Replace => client.replace_agent(params).await,
                AgentDocumentAction::Open => return,
            };
            let _ = updates.send(RpcUpdate::AgentDocumentChanged {
                action,
                id: info.id,
                result: result
                    .map(|result| result.path)
                    .map_err(|error| error.to_string()),
            });
        });
    }

    pub(super) fn install_agent_document_change(
        &mut self,
        action: AgentDocumentAction,
        id: String,
        result: Result<String, String>,
    ) {
        match result {
            Ok(path) => {
                self.status = match action {
                    AgentDocumentAction::Disable => format!("`{id}` is off."),
                    AgentDocumentAction::Enable => format!("`{id}` is on."),
                    AgentDocumentAction::Reset => format!("`{id}` is the official one again."),
                    AgentDocumentAction::Replace => {
                        format!("{path} holds a copy of the official `{id}`.")
                    }
                    _ => format!("{path} inherits `{id}`: change the keys you want."),
                };
                if matches!(
                    action,
                    AgentDocumentAction::Customize | AgentDocumentAction::Replace
                ) {
                    self.fetch_agent_document_for_editor(path);
                }
            }
            Err(error) => self.status = error,
        }
        if self.modal == Modal::AgentDocuments {
            self.poll_agent_documents();
        }
    }
}
