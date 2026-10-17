//! The `/runtime` panel: the selected session's attachments, or the
//! runtimes a new-session draft will attach, with their instance status;
//! attaching a runtime's capability, detaching, and retrying a failed
//! instance.

use cookie_agent_protocol::{
    AttachmentName, CapabilityName, RuntimeAttachRequest, RuntimeAttachment, RuntimeCapabilityInfo,
    RuntimeId, RuntimeInstanceInfo, RuntimeInstanceStatus, RuntimeKind, RuntimeListing,
    RuntimesListResult,
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

/// A runtime a new-session draft attaches. Its instance is opened as soon as
/// it joins the draft, so its status, and why it failed, show before the
/// session exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::ui) struct DraftRuntime {
    /// Matches the answer of the open request to this entry.
    pub(in crate::ui) id: u64,
    pub(in crate::ui) request: RuntimeAttachRequest,
    /// The instance the open request answered, or why it failed.
    pub(in crate::ui) opened: Option<Result<RuntimeInstanceInfo, String>>,
}

impl DraftRuntime {
    pub(in crate::ui) fn is_global(&self) -> bool {
        self.request.runtime_id.as_str() == RuntimeId::GLOBAL
    }

    /// The opened instance's canonical workspace, else the one requested.
    pub(in crate::ui) fn workspace(&self) -> Option<&str> {
        match &self.opened {
            Some(Ok(instance)) => instance.workspace.as_deref(),
            _ => self.request.workspace.as_deref(),
        }
    }

    /// Whether `other` names the same instance as this entry.
    pub(in crate::ui) fn same_instance(&self, other: &RuntimeAttachRequest) -> bool {
        self.request.runtime_id == other.runtime_id
            && self.request.capability == other.capability
            && (self.request.workspace == other.workspace
                || self.workspace() == other.workspace.as_deref())
    }
}

/// One thing the attach step offers: a capability of a runtime, or a runtime
/// that has not said what it offers yet, which attaching connects and takes
/// its only capability of.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::ui) struct RuntimeChoice {
    pub(in crate::ui) runtime_id: RuntimeId,
    pub(in crate::ui) capability: Option<RuntimeCapabilityInfo>,
    pub(in crate::ui) connected: bool,
    /// Why the runtime last failed to connect.
    pub(in crate::ui) message: Option<String>,
}

impl RuntimeChoice {
    /// Whether attaching asks for a workspace: the capability takes one, or
    /// the runtime has not said what it offers yet (then it is optional).
    pub(in crate::ui) fn asks_workspace(&self) -> bool {
        self.capability
            .as_ref()
            .is_none_or(|capability| capability.takes_workspace)
    }

    fn label(&self) -> String {
        match &self.capability {
            Some(capability) => format!("{}/{}", self.runtime_id, capability.name),
            None => self.runtime_id.to_string(),
        }
    }

    /// Case-insensitive substring match over the runtime ID and the
    /// capability's name, kind, and description.
    fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let mut haystack = self.runtime_id.to_string();
        if let Some(capability) = &self.capability {
            haystack.push(' ');
            haystack.push_str(capability.name.as_str());
            haystack.push(' ');
            haystack.push_str(capability.kind.as_str());
            haystack.push(' ');
            haystack.push_str(&capability.description);
        }
        haystack.to_lowercase().contains(&query)
    }
}

/// Attaching takes two steps: choosing a runtime's capability from every
/// runtime's, filtered by a search, then the parameters it needs (a
/// workspace path) when it needs any.
#[derive(Default)]
pub(in crate::ui) struct RuntimeAttachForm {
    pub(in crate::ui) search: InputState,
    pub(in crate::ui) selection: ListState,
    /// The choice whose parameters are being filled in; `None` while
    /// choosing.
    pub(in crate::ui) chosen: Option<RuntimeChoice>,
    pub(in crate::ui) workspace: InputState,
}

impl RuntimeAttachForm {
    /// The field typing edits: the search, or the workspace path.
    pub(in crate::ui) fn input_mut(&mut self) -> &mut InputState {
        if self.chosen.is_some() {
            &mut self.workspace
        } else {
            &mut self.search
        }
    }
}

/// What the attach step decided.
pub(in crate::ui) enum AttachStep {
    /// The chosen capability needs parameters: the form now asks for them.
    Parameters,
    /// Everything is known: attach this.
    Attach(RuntimeId, Option<CapabilityName>, Option<String>),
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
        let mut form = RuntimeAttachForm::default();
        form.selection.select(Some(0));
        self.form = Some(form);
        Ok(())
    }

    /// Every runtime's capabilities, in listing order, that match the
    /// search; a runtime that has not said what it offers is one choice.
    pub(in crate::ui) fn choices(&self) -> Vec<RuntimeChoice> {
        let query = self.form.as_ref().map_or("", |form| form.search.as_str());
        self.runtimes()
            .iter()
            .flat_map(|runtime| {
                let choice = |capability| RuntimeChoice {
                    runtime_id: runtime.runtime_id.clone(),
                    capability,
                    connected: runtime.connected,
                    message: runtime.message.clone(),
                };
                if runtime.capabilities.is_empty() {
                    vec![choice(None)]
                } else {
                    runtime
                        .capabilities
                        .iter()
                        .cloned()
                        .map(|capability| choice(Some(capability)))
                        .collect()
                }
            })
            .filter(|choice| choice.matches(query))
            .collect()
    }

    /// Keeps the highlighted choice within the filtered list.
    pub(in crate::ui) fn clamp_choice(&mut self) {
        let count = self.choices().len();
        if let Some(form) = self.form.as_mut() {
            form.selection
                .select((count > 0).then(|| form.selection.selected().unwrap_or(0).min(count - 1)));
        }
    }

    pub(in crate::ui) fn move_choice(&mut self, up: bool) {
        let count = self.choices().len();
        if let Some(form) = self.form.as_mut()
            && count > 0
        {
            let index = form.selection.selected().unwrap_or(0);
            form.selection.select(Some(cycle(index, count, up)));
        }
    }

    /// Takes the highlighted choice: on to its parameters when it needs
    /// any, else straight to attaching it.
    pub(in crate::ui) fn choose(&mut self) -> Result<AttachStep, String> {
        let choices = self.choices();
        let form = self.form.as_mut().ok_or("no attach in progress")?;
        let choice = form
            .selection
            .selected()
            .and_then(|index| choices.get(index))
            .ok_or("no runtime matches the search")?
            .clone();
        if choice.asks_workspace() {
            form.workspace = InputState::default();
            form.chosen = Some(choice);
            return Ok(AttachStep::Parameters);
        }
        Ok(AttachStep::Attach(
            choice.runtime_id,
            choice.capability.map(|capability| capability.name),
            None,
        ))
    }

    /// Esc: from the parameters back to the list, from the list out of the
    /// attach step.
    pub(in crate::ui) fn back(&mut self) {
        match self.form.as_mut() {
            Some(form) if form.chosen.is_some() => form.chosen = None,
            _ => self.form = None,
        }
    }

    /// The attach request the parameters describe.
    pub(in crate::ui) fn attach_request(
        &self,
    ) -> Result<(RuntimeId, Option<CapabilityName>, Option<String>), String> {
        let form = self.form.as_ref().ok_or("no attach in progress")?;
        let choice = form.chosen.as_ref().ok_or("choose a runtime")?;
        let workspace = form.workspace.as_str().trim();
        let workspace = match (&choice.capability, workspace.is_empty()) {
            (Some(capability), true) => {
                return Err(format!(
                    "capability `{}` of runtime `{}` needs a workspace path",
                    capability.name, choice.runtime_id
                ));
            }
            (None, true) => None,
            (_, false) => Some(workspace.to_owned()),
        };
        Ok((
            choice.runtime_id.clone(),
            choice
                .capability
                .as_ref()
                .map(|capability| capability.name.clone()),
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

/// The rows for a new-session draft: each entry with its live instance from
/// `listing`, else the one its open request answered. Until the instance
/// reports its attachment name and kind, the row shows the runtime ID and the
/// capability.
pub(in crate::ui) fn draft_runtime_rows(
    entries: &[DraftRuntime],
    listing: Option<&RuntimesListResult>,
) -> Vec<RuntimeRow> {
    entries
        .iter()
        .filter_map(|entry| {
            let capability = match (&entry.request.capability, &entry.opened) {
                (Some(capability), _) => capability.clone(),
                (None, Some(Ok(instance))) => instance.capability.clone(),
                (None, _) => cookie_agent_protocol::default_capability(&entry.request.runtime_id),
            };
            let workspace = entry.workspace().map(str::to_owned);
            let live = listing.and_then(|listing| {
                listing
                    .instances
                    .iter()
                    .find(|instance| {
                        instance.runtime_id == entry.request.runtime_id
                            && instance.capability == capability
                            && instance.workspace == workspace
                    })
                    .cloned()
            });
            let instance = live.or_else(|| match &entry.opened {
                Some(Ok(instance)) => Some(instance.clone()),
                Some(Err(message)) => Some(RuntimeInstanceInfo {
                    runtime_id: entry.request.runtime_id.clone(),
                    capability: capability.clone(),
                    attachment_name: None,
                    kind: None,
                    workspace: workspace.clone(),
                    platform: None,
                    status: RuntimeInstanceStatus::Failed,
                    message: Some(message.clone()),
                }),
                None => None,
            });
            Some(RuntimeRow {
                attachment: RuntimeAttachment {
                    name: match instance
                        .as_ref()
                        .and_then(|instance| instance.attachment_name.clone())
                    {
                        Some(name) => name,
                        None => AttachmentName::new(entry.request.runtime_id.as_str()).ok()?,
                    },
                    kind: match instance.as_ref().and_then(|instance| instance.kind.clone()) {
                        Some(kind) => kind,
                        None => RuntimeKind::new(capability.as_str()).ok()?,
                    },
                    runtime_id: entry.request.runtime_id.clone(),
                    capability,
                    workspace,
                },
                instance,
            })
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
                    .title(crate::ui::panel_title("Attached runtimes"))
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

fn render_attach_form(frame: &mut Frame, area: Rect, panel: &mut RuntimePanel, theme: &Theme) {
    let choices = panel.choices();
    let total = panel
        .runtimes()
        .iter()
        .map(|runtime| runtime.capabilities.len().max(1))
        .sum::<usize>();
    let Some(form) = panel.form.as_mut() else {
        return;
    };
    if let Some(choice) = form.chosen.clone() {
        render_attach_parameters(frame, area, &choice, form, theme);
        return;
    }
    let block = |title: String, hint: &'static str| {
        crate::ui::panel_block()
            .border_style(theme.panel_border())
            .title(crate::ui::panel_title(title))
            .title_bottom(crate::ui::panel_title(
                Line::from(Span::styled(hint, theme.internal())).right_aligned(),
            ))
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(4),
        ])
        .split(area);
    crate::ui::input::render(
        frame,
        chunks[0],
        &mut form.search,
        true,
        "Search runtimes",
        Some("Filter…"),
        theme,
    );
    let items = choices
        .iter()
        .map(|choice| {
            let mut spans = vec![Span::styled(choice.label(), theme.body())];
            match &choice.capability {
                Some(capability) if !capability.description.is_empty() => spans.push(Span::styled(
                    format!("  {}", capability.description),
                    theme.muted(),
                )),
                Some(_) => {}
                None => spans.push(Span::styled(
                    "  capabilities known once it connects",
                    theme.muted(),
                )),
            }
            if !choice.connected {
                spans.push(Span::styled("  (not connected)", theme.muted()));
            }
            ListItem::new(Line::from(spans))
        })
        .collect::<Vec<_>>();
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_style(theme.selected())
            .block(block(
                format!("Attach runtime ({}/{total})", choices.len()),
                "type to filter | arrows choose | enter next | esc cancel",
            )),
        chunks[1],
        &mut form.selection,
    );
    let detail = form
        .selection
        .selected()
        .and_then(|index| choices.get(index))
        .map_or_else(
            || "No runtime matches the search.".to_owned(),
            |choice| {
                let mut text = match &choice.capability {
                    Some(capability) => format!(
                        "{} ({}){}",
                        choice.label(),
                        capability.kind.as_str(),
                        if capability.takes_workspace {
                            ", takes a workspace"
                        } else {
                            ", takes no workspace"
                        }
                    ),
                    None => format!(
                        "{}: attaching connects it and takes its only capability",
                        choice.runtime_id
                    ),
                };
                if let Some(message) = choice.message.as_deref().filter(|_| !choice.connected) {
                    text.push_str(&format!("\nLast connection failed: {message}"));
                }
                text
            },
        );
    frame.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: false }).block(
            crate::ui::panel_block()
                .border_style(theme.panel_border())
                .title(crate::ui::panel_title("Details")),
        ),
        chunks[2],
    );
}

fn render_attach_parameters(
    frame: &mut Frame,
    area: Rect,
    choice: &RuntimeChoice,
    form: &mut RuntimeAttachForm,
    theme: &Theme,
) {
    let mut lines = vec![Line::from(vec![
        Span::styled("Runtime: ", theme.muted()),
        Span::styled(choice.runtime_id.to_string(), theme.body()),
    ])];
    lines.push(match &choice.capability {
        Some(capability) => Line::from(vec![
            Span::styled("Capability: ", theme.muted()),
            Span::styled(
                if capability.description.is_empty() {
                    capability.name.to_string()
                } else {
                    format!("{} - {}", capability.name, capability.description)
                },
                theme.body(),
            ),
        ]),
        None => Line::from(Span::styled(
            "Its only capability, known once it connects; the workspace is optional.",
            theme.muted(),
        )),
    });
    if let Some(message) = choice.message.as_deref().filter(|_| !choice.connected) {
        lines.push(Line::from(Span::styled(
            format!("Last connection failed: {message}"),
            theme.muted(),
        )));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(3)])
        .split(area);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            crate::ui::panel_block()
                .border_style(theme.panel_border())
                .title(crate::ui::panel_title(format!("Attach {}", choice.label())))
                .title_bottom(crate::ui::panel_title(
                    Line::from(Span::styled("enter attach | esc back", theme.internal()))
                        .right_aligned(),
                )),
        ),
        chunks[0],
    );
    crate::ui::input::render(
        frame,
        chunks[1],
        &mut form.workspace,
        true,
        "Workspace path (on the runtime's machine)",
        Some("~/project"),
        theme,
    );
}
