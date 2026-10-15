//! The `/runtime` panel: the selected session's runtime attachments with
//! their instance status, attaching a template, and detaching.

use cookie_agent_protocol::{
    RuntimeAttachment, RuntimeId, RuntimeInstanceInfo, RuntimeInstanceStatus, RuntimeKind,
    RuntimeTemplateInfo, RuntimesListResult,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{theme::Theme, ui::input::InputState};

use super::super::app::paint_panel;

/// One attachment row: what the session records and the instance it refers
/// to, when the daemon reports one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::ui) struct RuntimeRow {
    pub(in crate::ui) attachment: RuntimeAttachment,
    pub(in crate::ui) instance: Option<RuntimeInstanceInfo>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum RuntimeFormFocus {
    Template,
    Workspace,
}

/// The attach step: a template, and a workspace path when it takes one.
pub(in crate::ui) struct RuntimeAttachForm {
    pub(in crate::ui) template: usize,
    pub(in crate::ui) workspace: InputState,
    pub(in crate::ui) focus: RuntimeFormFocus,
}

impl RuntimeAttachForm {
    fn new() -> Self {
        Self {
            template: 0,
            workspace: InputState::default(),
            focus: RuntimeFormFocus::Template,
        }
    }
}

#[derive(Default)]
pub(in crate::ui) struct RuntimePanel {
    pub(in crate::ui) listing: Option<RuntimesListResult>,
    pub(in crate::ui) selection: ListState,
    pub(in crate::ui) form: Option<RuntimeAttachForm>,
    pub(in crate::ui) refresh_in_flight: bool,
}

impl RuntimePanel {
    pub(in crate::ui) fn install(&mut self, listing: RuntimesListResult) {
        self.listing = Some(listing);
        self.refresh_in_flight = false;
    }

    pub(in crate::ui) fn templates(&self) -> &[RuntimeTemplateInfo] {
        self.listing
            .as_ref()
            .map_or(&[], |listing| listing.templates.as_slice())
    }

    /// Opens the attach step; there is nothing to attach before
    /// `runtimes.list` answered.
    pub(in crate::ui) fn begin_attach(&mut self) -> Result<(), String> {
        if self.templates().is_empty() {
            return Err("no runtime templates are available yet".into());
        }
        self.form = Some(RuntimeAttachForm::new());
        Ok(())
    }

    pub(in crate::ui) fn selected_template(&self) -> Option<&RuntimeTemplateInfo> {
        let form = self.form.as_ref()?;
        self.templates().get(form.template)
    }

    pub(in crate::ui) fn cycle_template(&mut self, backward: bool) {
        let count = self.templates().len();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if count == 0 {
            return;
        }
        form.template = if backward {
            (form.template + count - 1) % count
        } else {
            (form.template + 1) % count
        };
        if form.focus == RuntimeFormFocus::Workspace
            && !self
                .selected_template()
                .is_some_and(|template| template.takes_workspace)
            && let Some(form) = self.form.as_mut()
        {
            form.focus = RuntimeFormFocus::Template;
        }
    }

    /// Moves between the template and the workspace field; the workspace is
    /// only reachable for a template that takes one.
    pub(in crate::ui) fn move_focus(&mut self) {
        let takes_workspace = self
            .selected_template()
            .is_some_and(|template| template.takes_workspace);
        if let Some(form) = self.form.as_mut() {
            form.focus = match form.focus {
                RuntimeFormFocus::Template if takes_workspace => RuntimeFormFocus::Workspace,
                _ => RuntimeFormFocus::Template,
            };
        }
    }

    /// The attach request the form describes.
    pub(in crate::ui) fn attach_request(&self) -> Result<(RuntimeId, Option<String>), String> {
        let form = self.form.as_ref().ok_or("no attach in progress")?;
        let template = self
            .selected_template()
            .ok_or("choose a runtime template")?;
        let workspace = form.workspace.as_str().trim();
        let workspace = if template.takes_workspace {
            if workspace.is_empty() {
                return Err(format!(
                    "runtime `{}` needs a workspace path",
                    template.runtime_id
                ));
            }
            Some(workspace.to_owned())
        } else {
            None
        };
        Ok((template.runtime_id.clone(), workspace))
    }

    pub(in crate::ui) fn clamp(&mut self, rows: usize) {
        self.selection
            .select((rows > 0).then(|| self.selection.selected().unwrap_or(0).min(rows - 1)));
    }
}

/// The rows for `attachments`, each matched to its instance in `listing`.
/// `None` attachments is a session created before runtimes: `global` and
/// `local`.
pub(in crate::ui) fn runtime_rows(
    attachments: Option<&[RuntimeAttachment]>,
    listing: Option<&RuntimesListResult>,
) -> Vec<RuntimeRow> {
    let legacy;
    let attachments = match attachments {
        Some(attachments) => attachments,
        None => {
            legacy = legacy_attachments(listing);
            &legacy
        }
    };
    attachments
        .iter()
        .map(|attachment| RuntimeRow {
            attachment: attachment.clone(),
            instance: listing.and_then(|listing| {
                listing
                    .instances
                    .iter()
                    .find(|instance| {
                        instance.runtime_id == attachment.runtime_id
                            && instance.workspace == attachment.workspace
                    })
                    .cloned()
            }),
        })
        .collect()
}

/// What a session created before runtimes is attached to.
fn legacy_attachments(listing: Option<&RuntimesListResult>) -> Vec<RuntimeAttachment> {
    [RuntimeId::GLOBAL, RuntimeId::LOCAL]
        .into_iter()
        .filter_map(|id| {
            let runtime_id = RuntimeId::new(id).ok()?;
            let instance = listing.and_then(|listing| {
                listing
                    .instances
                    .iter()
                    .find(|instance| instance.runtime_id == runtime_id)
            });
            Some(RuntimeAttachment {
                name: instance
                    .and_then(|instance| instance.attachment_name.clone())
                    .or_else(|| cookie_agent_protocol::AttachmentName::new(id).ok())?,
                kind: instance.and_then(|instance| instance.kind).unwrap_or(
                    if id == RuntimeId::GLOBAL {
                        RuntimeKind::Global
                    } else {
                        RuntimeKind::Coding
                    },
                ),
                workspace: instance.and_then(|instance| instance.workspace.clone()),
                runtime_id,
            })
        })
        .collect()
}

const fn status_label(status: Option<RuntimeInstanceStatus>) -> &'static str {
    match status {
        Some(RuntimeInstanceStatus::Starting) => "starting",
        Some(RuntimeInstanceStatus::Ready) => "ready",
        Some(RuntimeInstanceStatus::Reconnecting) => "reconnecting",
        Some(RuntimeInstanceStatus::Failed) => "failed",
        Some(RuntimeInstanceStatus::Stopped) => "stopped",
        None => "not running",
    }
}

pub(in crate::ui) fn render_runtimes(
    frame: &mut Frame,
    area: Rect,
    panel: &mut RuntimePanel,
    rows: &[RuntimeRow],
    theme: &Theme,
) {
    paint_panel(frame, area, theme);
    if panel.form.is_some() {
        render_attach_form(frame, area, panel, theme);
        return;
    }
    panel.clamp(rows.len());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);
    let items = rows
        .iter()
        .map(|row| {
            ListItem::new(format!(
                "{}  {}  {}  {}",
                row.attachment.name,
                row.attachment.runtime_id,
                row.attachment
                    .workspace
                    .as_deref()
                    .unwrap_or("(no workspace)"),
                status_label(row.instance.as_ref().map(|instance| instance.status)),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_style(theme.selected())
            .block(
                crate::ui::panel_block()
                    .border_style(theme.panel_border())
                    .title(crate::ui::panel_title("Session runtimes"))
                    .title_bottom(crate::ui::panel_title(
                        Line::from(Span::styled(
                            "a attach | d detach | esc close",
                            theme.internal(),
                        ))
                        .right_aligned(),
                    )),
            ),
        chunks[0],
        &mut panel.selection,
    );
    let detail = panel
        .selection
        .selected()
        .and_then(|index| rows.get(index))
        .map_or_else(
            || {
                if panel.listing.is_none() {
                    "Loading runtimes...".to_owned()
                } else {
                    "No runtimes attached. Press a to attach one.".to_owned()
                }
            },
            row_detail,
        );
    frame.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: false }).block(
            crate::ui::panel_block()
                .border_style(theme.panel_border())
                .title(crate::ui::panel_title("Runtime details")),
        ),
        chunks[1],
    );
}

fn row_detail(row: &RuntimeRow) -> String {
    let attachment = &row.attachment;
    let mut text = format!(
        "{}\nruntime: {} ({})",
        attachment.name,
        attachment.runtime_id,
        attachment.kind.as_str()
    );
    if let Some(workspace) = &attachment.workspace {
        text.push_str(&format!("\nworkspace: {workspace}"));
    }
    text.push_str(&format!(
        "\nstatus: {}",
        status_label(row.instance.as_ref().map(|instance| instance.status))
    ));
    if let Some(platform) = row
        .instance
        .as_ref()
        .and_then(|instance| instance.platform.as_deref())
    {
        text.push_str(&format!("\nplatform: {platform}"));
    }
    if let Some(message) = row
        .instance
        .as_ref()
        .and_then(|instance| instance.message.as_deref())
    {
        text.push_str("\n\n");
        text.push_str(message);
    }
    text
}

fn render_attach_form(frame: &mut Frame, area: Rect, panel: &RuntimePanel, theme: &Theme) {
    let Some(form) = panel.form.as_ref() else {
        return;
    };
    let field = |focus: RuntimeFormFocus, label: &str, value: String| {
        let marker = if form.focus == focus { ">" } else { " " };
        Line::from(vec![
            Span::styled(format!("{marker} {label}: "), theme.muted()),
            Span::styled(
                value,
                if form.focus == focus {
                    theme.selected()
                } else {
                    theme.body()
                },
            ),
        ])
    };
    let template = panel.selected_template();
    let mut lines = vec![field(
        RuntimeFormFocus::Template,
        "Runtime",
        template.map_or_else(String::new, |template| template.runtime_id.to_string()),
    )];
    if template.is_some_and(|template| template.takes_workspace) {
        lines.push(field(
            RuntimeFormFocus::Workspace,
            "Workspace path",
            form.workspace.as_str().to_owned(),
        ));
    } else {
        lines.push(Line::from(Span::styled(
            "  This runtime takes no workspace.",
            theme.muted(),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            crate::ui::panel_block()
                .border_style(theme.panel_border())
                .title(crate::ui::panel_title("Attach runtime"))
                .title_bottom(crate::ui::panel_title(
                    Line::from(Span::styled(
                        "arrows runtime | tab fields | enter attach | esc cancel",
                        theme.internal(),
                    ))
                    .right_aligned(),
                )),
        ),
        area,
    );
}
