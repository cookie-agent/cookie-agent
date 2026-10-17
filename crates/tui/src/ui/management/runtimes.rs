//! The `/runtime` panel: the selected session's attachments with their
//! instance status, attaching a runtime's capability, detaching, and
//! retrying a failed instance.

use cookie_agent_protocol::{
    CapabilityName, RuntimeAttachment, RuntimeCapabilityInfo, RuntimeId, RuntimeInstanceInfo,
    RuntimeInstanceStatus, RuntimeKind, RuntimeListing, RuntimesListResult,
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
    Runtime,
    Capability,
    Workspace,
}

/// The attach step: a runtime, one of its capabilities, and a workspace path
/// when the capability takes one.
pub(in crate::ui) struct RuntimeAttachForm {
    pub(in crate::ui) runtime: usize,
    pub(in crate::ui) capability: usize,
    pub(in crate::ui) workspace: InputState,
    pub(in crate::ui) focus: RuntimeFormFocus,
}

impl RuntimeAttachForm {
    fn new() -> Self {
        Self {
            runtime: 0,
            capability: 0,
            workspace: InputState::default(),
            focus: RuntimeFormFocus::Runtime,
        }
    }
}

/// Whether the attach form asks for a workspace: the chosen capability takes
/// one, or the runtime has not said what it offers yet.
fn asks_workspace(capability: Option<&RuntimeCapabilityInfo>, known: bool) -> bool {
    capability.map_or(!known, |capability| capability.takes_workspace)
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

    pub(in crate::ui) fn runtimes(&self) -> &[RuntimeListing] {
        self.listing
            .as_ref()
            .map_or(&[], |listing| listing.runtimes.as_slice())
    }

    /// Opens the attach step; there is nothing to attach before
    /// `runtimes.list` answered.
    pub(in crate::ui) fn begin_attach(&mut self) -> Result<(), String> {
        if self.runtimes().is_empty() {
            return Err("no runtimes are available yet".into());
        }
        self.form = Some(RuntimeAttachForm::new());
        Ok(())
    }

    pub(in crate::ui) fn selected_runtime(&self) -> Option<&RuntimeListing> {
        let form = self.form.as_ref()?;
        self.runtimes().get(form.runtime)
    }

    /// The chosen capability; `None` while the runtime has not said what it
    /// offers, when attaching takes its only capability.
    pub(in crate::ui) fn selected_capability(&self) -> Option<&RuntimeCapabilityInfo> {
        let form = self.form.as_ref()?;
        self.selected_runtime()?.capabilities.get(form.capability)
    }

    fn asks_workspace(&self) -> bool {
        let known = self
            .selected_runtime()
            .is_some_and(|runtime| !runtime.capabilities.is_empty());
        asks_workspace(self.selected_capability(), known)
    }

    pub(in crate::ui) fn cycle_runtime(&mut self, backward: bool) {
        let count = self.runtimes().len();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if count == 0 {
            return;
        }
        form.runtime = cycle(form.runtime, count, backward);
        form.capability = 0;
    }

    pub(in crate::ui) fn cycle_capability(&mut self, backward: bool) {
        let count = self
            .selected_runtime()
            .map_or(0, |runtime| runtime.capabilities.len());
        if let Some(form) = self.form.as_mut()
            && count > 0
        {
            form.capability = cycle(form.capability, count, backward);
        }
    }

    /// Moves between the runtime, its capability, and the workspace field;
    /// the capability is only reachable when the runtime offers several,
    /// and the workspace only when the capability takes one.
    pub(in crate::ui) fn move_focus(&mut self) {
        let several = self
            .selected_runtime()
            .is_some_and(|runtime| runtime.capabilities.len() > 1);
        let workspace = self.asks_workspace();
        if let Some(form) = self.form.as_mut() {
            form.focus = match form.focus {
                RuntimeFormFocus::Runtime if several => RuntimeFormFocus::Capability,
                RuntimeFormFocus::Runtime | RuntimeFormFocus::Capability if workspace => {
                    RuntimeFormFocus::Workspace
                }
                _ => RuntimeFormFocus::Runtime,
            };
        }
    }

    /// The attach request the form describes.
    pub(in crate::ui) fn attach_request(
        &self,
    ) -> Result<(RuntimeId, Option<CapabilityName>, Option<String>), String> {
        let form = self.form.as_ref().ok_or("no attach in progress")?;
        let runtime = self.selected_runtime().ok_or("choose a runtime")?;
        let capability = self.selected_capability();
        let workspace = form.workspace.as_str().trim();
        let workspace = if self.asks_workspace() {
            match (capability, workspace.is_empty()) {
                (Some(capability), true) => {
                    return Err(format!(
                        "capability `{}` of runtime `{}` needs a workspace path",
                        capability.name, runtime.runtime_id
                    ));
                }
                (_, true) => None,
                (_, false) => Some(workspace.to_owned()),
            }
        } else {
            None
        };
        Ok((
            runtime.runtime_id.clone(),
            capability.map(|capability| capability.name.clone()),
            workspace,
        ))
    }

    /// The `runtimes.reconnect` request that retries `row`'s instance: its
    /// workspace goes along exactly when it has one.
    pub(in crate::ui) fn reconnect_request(
        &self,
        row: &RuntimeRow,
    ) -> Result<(RuntimeId, CapabilityName, Option<String>), String> {
        if !self
            .runtimes()
            .iter()
            .any(|runtime| runtime.runtime_id == row.attachment.runtime_id)
        {
            return Err(format!(
                "runtime `{}` is not a known runtime",
                row.attachment.runtime_id
            ));
        }
        Ok((
            row.attachment.runtime_id.clone(),
            row.attachment.capability.clone(),
            row.attachment.workspace.clone(),
        ))
    }

    pub(in crate::ui) fn clamp(&mut self, rows: usize) {
        self.selection
            .select((rows > 0).then(|| self.selection.selected().unwrap_or(0).min(rows - 1)));
    }
}

const fn cycle(index: usize, count: usize, backward: bool) -> usize {
    if backward {
        (index + count - 1) % count
    } else {
        (index + 1) % count
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
                            && instance.capability == attachment.capability
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
                capability: cookie_agent_protocol::default_capability(&runtime_id),
                name: instance
                    .and_then(|instance| instance.attachment_name.clone())
                    .or_else(|| cookie_agent_protocol::AttachmentName::new(id).ok())?,
                // Until the instance reports its kind, label the two built-ins
                // with the kinds they declare.
                kind: match instance.and_then(|instance| instance.kind.clone()) {
                    Some(kind) => kind,
                    None => RuntimeKind::new(if id == RuntimeId::GLOBAL {
                        "global"
                    } else {
                        "coding"
                    })
                    .ok()?,
                },
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
                "{}  {}/{}  {}  {}",
                row.attachment.name,
                row.attachment.runtime_id,
                row.attachment.capability,
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
                            "a attach | d detach | r retry | esc close",
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
        "{}\nruntime: {}\ncapability: {} ({})",
        attachment.name,
        attachment.runtime_id,
        attachment.capability,
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
    let runtime = panel.selected_runtime();
    let mut lines = vec![field(
        RuntimeFormFocus::Runtime,
        "Runtime",
        runtime.map_or_else(String::new, |runtime| runtime.runtime_id.to_string()),
    )];
    if let Some(message) = runtime
        .filter(|runtime| !runtime.connected)
        .and_then(|runtime| runtime.message.as_deref())
    {
        lines.push(Line::from(Span::styled(
            format!("  Last connection failed: {message}"),
            theme.muted(),
        )));
    }
    match runtime.map(|runtime| runtime.capabilities.as_slice()) {
        Some([]) | None => lines.push(Line::from(Span::styled(
            "  Its capabilities are known once it has connected; attaching uses its only one.",
            theme.muted(),
        ))),
        Some(_) => lines.push(field(
            RuntimeFormFocus::Capability,
            "Capability",
            panel
                .selected_capability()
                .map_or_else(String::new, |capability| {
                    if capability.description.is_empty() {
                        capability.name.to_string()
                    } else {
                        format!("{} - {}", capability.name, capability.description)
                    }
                }),
        )),
    }
    if panel.asks_workspace() {
        lines.push(field(
            RuntimeFormFocus::Workspace,
            "Workspace path",
            form.workspace.as_str().to_owned(),
        ));
    } else {
        lines.push(Line::from(Span::styled(
            "  This capability takes no workspace.",
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
                        "arrows choose | tab fields | enter attach | esc cancel",
                        theme.internal(),
                    ))
                    .right_aligned(),
                )),
        ),
        area,
    );
}
