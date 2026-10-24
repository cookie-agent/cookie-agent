//! The `/runtime` panel: the selected session's attachments, or the
//! runtimes a new-session draft will attach, with their instance status
//! and the MCP servers each hosts;
//! attaching a runtime's capability, detaching, and retrying a failed
//! instance. Its machines view groups the daemon's runtimes by machine and
//! renames, merges, and forgets machines and moves runtimes between them.

use cookie_agent_protocol::{
    CapabilityName, MachineAssignment, MachineInfo, MachineName, MachinesListResult, McpServerInfo,
    RuntimeId, RuntimeListing, RuntimesListResult,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

pub(in crate::ui) use cookie_agent_client_state::runtimes::{
    DraftRuntime, MachineAction, MachineEntry, MachineRequest, RuntimeChoice, RuntimeRow,
    draft_runtime_rows, runtime_rows,
};
use cookie_agent_client_state::{
    mcp::{runtime_servers, server_message, server_summary, servers_badge, servers_overview},
    runtimes::{
        attach_parameters, machine_action_request, machine_entries, machine_form_title,
        machine_name_request, reconnect_request, runtime_choices, status_label,
    },
};

use crate::{theme::Theme, ui::input::InputState};

use super::super::app::paint_panel;

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

/// The panel's two views.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::ui) enum RuntimeView {
    /// The session's attachments.
    #[default]
    Attachments,
    /// The daemon's machines and the runtimes on each.
    Machines,
}

impl RuntimeView {
    pub(in crate::ui) const ALL: [Self; 2] = [Self::Attachments, Self::Machines];

    const fn label(self) -> &'static str {
        match self {
            Self::Attachments => " Attached runtimes ",
            Self::Machines => " Machines ",
        }
    }
}

/// An action waiting for the machine name it needs.
pub(in crate::ui) struct MachineForm {
    pub(in crate::ui) action: MachineAction,
    pub(in crate::ui) entry: MachineEntry,
    pub(in crate::ui) input: InputState,
}

/// Where the panel's clickable parts landed this frame.
#[derive(Clone, Debug, Default)]
pub(in crate::ui) struct RuntimeHits {
    pub(in crate::ui) tabs: Vec<(Rect, RuntimeView)>,
    /// (row rect, machines-view row index).
    pub(in crate::ui) machine_rows: Vec<(Rect, usize)>,
    pub(in crate::ui) machine_actions: Vec<(Rect, MachineAction)>,
}

#[derive(Default)]
pub(in crate::ui) struct RuntimePanel {
    pub(in crate::ui) listing: Option<RuntimesListResult>,
    pub(in crate::ui) selection: ListState,
    pub(in crate::ui) form: Option<RuntimeAttachForm>,
    pub(in crate::ui) refresh: crate::ui::app::ListingRefresh,
    pub(in crate::ui) view: RuntimeView,
    /// `machines.list`, or why it failed.
    pub(in crate::ui) machines: Option<Result<MachinesListResult, String>>,
    pub(in crate::ui) machine_selection: ListState,
    pub(in crate::ui) machine_form: Option<MachineForm>,
    pub(in crate::ui) machines_in_flight: bool,
    /// `mcp.server.list`, for the MCP servers each runtime hosts; `None`
    /// until it answers.
    pub(in crate::ui) mcp_servers: Option<Vec<McpServerInfo>>,
}

impl RuntimePanel {
    pub(in crate::ui) fn install(&mut self, listing: RuntimesListResult) {
        self.listing = Some(listing);
    }

    pub(in crate::ui) fn install_mcp_servers(&mut self, servers: Vec<McpServerInfo>) {
        self.mcp_servers = Some(servers);
    }

    /// The MCP servers `row`'s instance hosts.
    fn row_mcp_servers(&self, row: &RuntimeRow) -> Vec<McpServerInfo> {
        row.mcp_servers(self.mcp_servers.as_deref().unwrap_or_default())
    }

    /// The MCP servers any instance of `runtime_id` hosts.
    fn runtime_mcp_servers(&self, runtime_id: &RuntimeId) -> Vec<McpServerInfo> {
        runtime_servers(self.mcp_servers.as_deref().unwrap_or_default(), runtime_id)
    }

    pub(in crate::ui) fn install_machines(&mut self, machines: Result<MachinesListResult, String>) {
        self.machines = Some(machines);
        self.machines_in_flight = false;
        let count = self.machine_entries().len();
        self.machine_selection.select((count > 0).then(|| {
            self.machine_selection
                .selected()
                .unwrap_or(0)
                .min(count - 1)
        }));
    }

    /// `workspace`, a path of the runtime `runtime_id`, shown under the home
    /// that runtime reported (`~/…`), else absolute.
    fn display_workspace(&self, runtime_id: &RuntimeId, workspace: &str) -> String {
        let home = self
            .runtimes()
            .iter()
            .find(|runtime| runtime.runtime_id == *runtime_id)
            .and_then(|runtime| runtime.home.as_deref());
        cookie_agent_client_state::paths::display_path(workspace, home)
    }

    /// The machine of `runtime_id` as `runtimes.list` reports it.
    fn machine_of(&self, runtime_id: &RuntimeId) -> Option<&MachineName> {
        self.runtimes()
            .iter()
            .find(|runtime| runtime.runtime_id == *runtime_id)
            .and_then(|runtime| runtime.machine.as_ref())
    }

    /// The machines view's rows.
    pub(in crate::ui) fn machine_entries(&self) -> Vec<MachineEntry> {
        match &self.machines {
            Some(Ok(listing)) => machine_entries(listing),
            _ => Vec::new(),
        }
    }

    pub(in crate::ui) fn selected_machine_entry(&self) -> Option<MachineEntry> {
        let index = self.machine_selection.selected()?;
        self.machine_entries().into_iter().nth(index)
    }

    pub(in crate::ui) fn move_machine_selection(&mut self, up: bool) {
        let count = self.machine_entries().len();
        if count > 0 {
            let index = self
                .machine_selection
                .selected()
                .unwrap_or(0)
                .min(count - 1);
            self.machine_selection.select(Some(cycle(index, count, up)));
        }
    }

    /// Starts `action` on the selected row: a request ready to send, or
    /// `None` when the form now asks for a machine name.
    pub(in crate::ui) fn begin_machine_action(
        &mut self,
        action: MachineAction,
    ) -> Result<Option<MachineRequest>, String> {
        let entry = self
            .selected_machine_entry()
            .ok_or("select a machine or a runtime first")?;
        let request = machine_action_request(&entry, action)?;
        if request.is_none() {
            self.machine_form = Some(MachineForm {
                action,
                entry,
                input: InputState::default(),
            });
        }
        Ok(request)
    }

    /// The request the open form describes.
    pub(in crate::ui) fn submit_machine_form(&mut self) -> Result<MachineRequest, String> {
        let form = self
            .machine_form
            .as_ref()
            .ok_or("no machine change in progress")?;
        let request = machine_name_request(form.action, &form.entry, form.input.as_str())?;
        self.machine_form = None;
        Ok(request)
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

    /// Every runtime's capabilities that match the search.
    pub(in crate::ui) fn choices(&self) -> Vec<RuntimeChoice> {
        let query = self.form.as_ref().map_or("", |form| form.search.as_str());
        runtime_choices(self.runtimes(), query)
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
        attach_parameters(choice, form.workspace.as_str())
    }

    /// The `runtimes.reconnect` request that retries `row`'s instance.
    pub(in crate::ui) fn reconnect_request(
        &self,
        row: &RuntimeRow,
    ) -> Result<(RuntimeId, CapabilityName, Option<String>), String> {
        reconnect_request(self.runtimes(), row)
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

pub(in crate::ui) fn render_runtimes(
    frame: &mut Frame,
    area: Rect,
    panel: &mut RuntimePanel,
    rows: &[RuntimeRow],
    theme: &Theme,
) -> RuntimeHits {
    let mut hits = RuntimeHits::default();
    paint_panel(frame, area, theme);
    if panel.form.is_some() {
        render_attach_form(frame, area, panel, theme);
        return hits;
    }
    let [tabs, body] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .areas(area);
    render_tabs(frame, tabs, panel, theme, &mut hits);
    match panel.view {
        RuntimeView::Attachments => render_attachments(frame, body, panel, rows, theme),
        RuntimeView::Machines => render_machines(frame, body, panel, theme, &mut hits),
    }
    hits
}

/// The view tabs, each a click target while no form is open.
fn render_tabs(
    frame: &mut Frame,
    area: Rect,
    panel: &RuntimePanel,
    theme: &Theme,
    hits: &mut RuntimeHits,
) {
    let mut spans = Vec::new();
    let mut x = area.x;
    for view in RuntimeView::ALL {
        let label = view.label();
        let width = u16::try_from(label.len()).unwrap_or(u16::MAX);
        let style = if view == panel.view {
            theme.selected()
        } else {
            theme.link()
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
        if panel.machine_form.is_none() && x.saturating_add(width) <= area.right() {
            hits.tabs.push((Rect::new(x, area.y, width, 1), view));
        }
        x = x.saturating_add(width + 1);
    }
    spans.push(Span::styled("m switch", theme.internal()));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_attachments(
    frame: &mut Frame,
    area: Rect,
    panel: &mut RuntimePanel,
    rows: &[RuntimeRow],
    theme: &Theme,
) {
    panel.clamp(rows.len());
    // The list takes what its rows need, up to a little over half; the
    // details, which list each server the instance hosts, take the rest.
    let list_height = u16::try_from(rows.len().max(1) + 2)
        .unwrap_or(u16::MAX)
        .clamp(3, (area.height * 55 / 100).max(3));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(list_height), Constraint::Min(0)])
        .split(area);
    let items = rows
        .iter()
        .map(|row| {
            let mut line = format!(
                "{}  {}/{}  {}  {}",
                row.attachment.name,
                row.attachment.runtime_id,
                row.attachment.capability,
                row.attachment.workspace.as_deref().map_or_else(
                    || "(no workspace)".to_owned(),
                    |workspace| panel.display_workspace(&row.attachment.runtime_id, workspace)
                ),
                status_label(row.instance.as_ref().map(|instance| instance.status)),
            );
            if let Some(machine) = panel.machine_of(&row.attachment.runtime_id) {
                line.push_str(&format!("  on {machine}"));
            }
            if let Some(badge) = servers_badge(&panel.row_mcp_servers(row)) {
                line.push_str(&format!("  {badge}"));
            }
            ListItem::new(line)
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
                            "a attach | d detach | r retry | m machines | esc close",
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
            |row| row_detail(row, panel),
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

/// The machines view: each machine with its runtimes, the selected row's
/// details, the actions it offers, and the form an action that needs a
/// machine name opens.
fn render_machines(
    frame: &mut Frame,
    area: Rect,
    panel: &mut RuntimePanel,
    theme: &Theme,
    hits: &mut RuntimeHits,
) {
    let entries = panel.machine_entries();
    let form_height = if panel.machine_form.is_some() { 3 } else { 0 };
    let [list_area, detail_area, actions_area, form_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(6),
            Constraint::Length(1),
            Constraint::Length(form_height),
        ])
        .areas(area);
    let machines = match &panel.machines {
        Some(Ok(listing)) => listing.machines.as_slice(),
        _ => &[],
    };
    let items = entries
        .iter()
        .map(|entry| ListItem::new(machine_entry_line(entry, machines, panel, theme)))
        .collect::<Vec<_>>();
    let block = crate::ui::panel_block()
        .border_style(theme.panel_border())
        .title(crate::ui::panel_title("Machines"))
        .title_bottom(crate::ui::panel_title(
            Line::from(Span::styled(
                "n rename | v move | u unpin | g merge | f forget | m attachments | esc close",
                theme.internal(),
            ))
            .right_aligned(),
        ));
    let inner = block.inner(list_area);
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_style(theme.selected())
            .block(block),
        list_area,
        &mut panel.machine_selection,
    );
    if panel.machine_form.is_none() {
        let offset = panel.machine_selection.offset();
        for (line, index) in (offset..entries.len())
            .take(usize::from(inner.height))
            .enumerate()
        {
            let y = inner.y + u16::try_from(line).unwrap_or(u16::MAX);
            hits.machine_rows
                .push((Rect::new(inner.x, y, inner.width, 1), index));
        }
    }
    let selected = panel.selected_machine_entry();
    let detail = match (&panel.machines, &selected) {
        (None, _) => "Loading machines...".to_owned(),
        (Some(Err(error)), _) => format!("Machines are unavailable: {error}"),
        (Some(Ok(_)), None) => "No machines yet.".to_owned(),
        (Some(Ok(_)), Some(entry)) => machine_entry_detail(entry, machines, panel),
    };
    frame.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: false }).block(
            crate::ui::panel_block()
                .border_style(theme.panel_border())
                .title(crate::ui::panel_title("Details")),
        ),
        detail_area,
    );
    let mut spans = Vec::new();
    let mut x = actions_area.x;
    for action in selected.iter().flat_map(MachineEntry::actions) {
        let label = format!(" {} ({}) ", action.label(), action.key());
        let width = u16::try_from(label.len()).unwrap_or(u16::MAX);
        if panel.machine_form.is_none() && x.saturating_add(width) <= actions_area.right() {
            hits.machine_actions
                .push((Rect::new(x, actions_area.y, width, 1), action));
        }
        spans.push(Span::styled(label, theme.link()));
        spans.push(Span::raw(" "));
        x = x.saturating_add(width + 1);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), actions_area);
    if let Some(form) = panel.machine_form.as_mut() {
        let title = machine_form_title(form.action, &form.entry);
        crate::ui::input::render(
            frame,
            form_area,
            &mut form.input,
            true,
            title,
            Some("enter apply | esc cancel"),
            theme,
        );
    }
}

fn machine_entry_line(
    entry: &MachineEntry,
    machines: &[MachineInfo],
    panel: &RuntimePanel,
    theme: &Theme,
) -> Line<'static> {
    match entry {
        MachineEntry::Machine(name) => {
            let machine = machines.iter().find(|machine| machine.name == *name);
            let mut spans = vec![Span::styled(name.to_string(), theme.body())];
            if let Some(platform) = machine.and_then(|machine| machine.platform.as_deref()) {
                spans.push(Span::styled(format!(" ({platform})"), theme.muted()));
            }
            let memories = machine.map_or(0, |machine| machine.memories);
            spans.push(Span::styled(
                match memories {
                    1 => "  1 memory".to_owned(),
                    count => format!("  {count} memories"),
                },
                theme.muted(),
            ));
            Line::from(spans)
        }
        MachineEntry::Runtime {
            runtime,
            assigned_by,
            pinned,
            ..
        } => {
            let mut spans = vec![
                Span::styled(format!("  {runtime}"), theme.body()),
                Span::styled(
                    format!("  {}", runtime_placement(*assigned_by, *pinned)),
                    theme.muted(),
                ),
            ];
            if let Some(badge) = servers_badge(&panel.runtime_mcp_servers(runtime)) {
                spans.push(Span::styled(format!("  {badge}"), theme.muted()));
            }
            Line::from(spans)
        }
    }
}

/// Why a runtime row sits under its machine.
fn runtime_placement(assigned_by: Option<MachineAssignment>, pinned: bool) -> &'static str {
    match (assigned_by, pinned) {
        (None, _) => "pinned, not connected",
        (Some(MachineAssignment::Pinned), _) | (Some(_), true) => "pinned",
        (Some(MachineAssignment::MachineId), false) => "by machine ID",
        (Some(MachineAssignment::LastSeen), false) => "last seen here",
        (Some(MachineAssignment::Unknown), false) => "connected",
    }
}

fn machine_entry_detail(
    entry: &MachineEntry,
    machines: &[MachineInfo],
    panel: &RuntimePanel,
) -> String {
    match entry {
        MachineEntry::Machine(name) => {
            let Some(machine) = machines.iter().find(|machine| machine.name == *name) else {
                return name.to_string();
            };
            let mut text = format!(
                "{}\nmemories: {}\nconnected runtimes: {}",
                machine.name,
                machine.memories,
                machine.connected.len()
            );
            if !machine.pinned.is_empty() {
                let pinned = machine
                    .pinned
                    .iter()
                    .map(RuntimeId::to_string)
                    .collect::<Vec<_>>();
                text.push_str(&format!("\npinned: {}", pinned.join(", ")));
            }
            if !machine.ids.is_empty() {
                text.push_str(&format!("\nmachine IDs: {}", machine.ids.join(", ")));
            }
            text
        }
        MachineEntry::Runtime {
            machine,
            runtime,
            assigned_by,
            pinned,
        } => {
            let mut text = format!(
                "runtime {runtime} on machine {machine}: {}",
                runtime_placement(*assigned_by, *pinned)
            );
            let servers = panel.runtime_mcp_servers(runtime);
            if let Some(overview) = servers_overview(&servers) {
                text.push_str(&format!("\n{overview}"));
                for server in &servers {
                    text.push_str(&format!("\n  {}", server_summary(server)));
                    if let Some(workspace) = &server.workspace {
                        let shown = panel.display_workspace(runtime, workspace);
                        text.push_str(&format!("  at {shown}"));
                    }
                }
            }
            text
        }
    }
}

fn row_detail(row: &RuntimeRow, panel: &RuntimePanel) -> String {
    let attachment = &row.attachment;
    let machine = panel.machine_of(&attachment.runtime_id);
    let mut text = format!(
        "{}\nruntime: {}\ncapability: {} ({})",
        attachment.name,
        attachment.runtime_id,
        attachment.capability,
        attachment.kind.as_str()
    );
    if let Some(machine) = machine {
        text.push_str(&format!("\nmachine: {machine}"));
    }
    if let Some(description) = &row.description {
        text.push_str(&format!("\ndescription: {description}"));
    }
    if let Some(workspace) = &attachment.workspace {
        let shown = panel.display_workspace(&attachment.runtime_id, workspace);
        text.push_str(&format!("\nworkspace: {shown}"));
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
    text.push_str(&mcp_detail(&panel.row_mcp_servers(row), panel));
    text
}

/// The MCP servers of a runtime row's details: each one's state and tools,
/// and below it why it failed or what it waits for.
fn mcp_detail(servers: &[McpServerInfo], panel: &RuntimePanel) -> String {
    if panel.mcp_servers.is_none() {
        return "\n\nMCP servers: loading...".to_owned();
    }
    if servers.is_empty() {
        return "\n\nMCP servers: none".to_owned();
    }
    let mut text = "\n\nMCP servers:".to_owned();
    for server in servers {
        text.push_str(&format!("\n  {}", server_summary(server)));
        for line in server_message(server)
            .iter()
            .flat_map(|message| message.lines())
        {
            text.push_str(&format!("\n    {line}"));
        }
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
            Constraint::Length(5),
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
                        "{} ({}){}{}",
                        choice.label(),
                        capability.kind.as_str(),
                        if capability.takes_workspace {
                            ", takes a workspace"
                        } else {
                            ", takes no workspace"
                        },
                        if capability.description.is_empty() {
                            String::new()
                        } else {
                            format!("\n{}", capability.description)
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

/// Two machines: `laptop` with `global` and `local` by machine ID and a
/// pinned runtime `box` that is not connected, and `labpc` with one pinned
/// runtime and a memory.
#[cfg(test)]
pub(in crate::ui) fn sample_machines() -> MachinesListResult {
    use cookie_agent_protocol::MachineRuntime;
    let runtime = |id: &str, assigned_by| MachineRuntime {
        runtime_id: RuntimeId::new(id).unwrap(),
        assigned_by,
        platform: Some("linux".into()),
    };
    MachinesListResult {
        machines: vec![
            MachineInfo {
                name: MachineName::new("labpc").unwrap(),
                ids: vec!["id-labpc".into()],
                pinned: vec![RuntimeId::new("labpc").unwrap()],
                connected: vec![runtime("labpc", MachineAssignment::Pinned)],
                remembered: Vec::new(),
                platform: Some("linux".into()),
                memories: 1,
            },
            MachineInfo {
                name: MachineName::new("laptop").unwrap(),
                ids: vec!["id-laptop".into()],
                pinned: vec![RuntimeId::new("box").unwrap()],
                connected: vec![
                    runtime("global", MachineAssignment::MachineId),
                    runtime("local", MachineAssignment::MachineId),
                ],
                remembered: Vec::new(),
                platform: Some("linux".into()),
                memories: 0,
            },
        ],
    }
}
