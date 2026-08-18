use std::collections::BTreeMap;

use cookie_agent_protocol::{
    McpConfigTarget, McpServerDefinition, McpServerInfo, McpServerState, ModelUsageRollup,
    PermissionAction, PermissionEffect, PermissionRuleSource, SessionPermissionGetResult,
    SessionTreeUsageResult, SessionUsageResult, SkillsListResult, UsageRollup,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
};

use crate::theme::Theme;

use super::{
    app::{format_cost_usd, paint_panel, truncate_with_ellipsis},
    input::InputState,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum McpTransport {
    Stdio,
    Http,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PersistChoice {
    Runtime,
    User,
    Workspace,
}

impl PersistChoice {
    pub(super) const fn target(self) -> Option<McpConfigTarget> {
        match self {
            Self::Runtime => None,
            Self::User => Some(McpConfigTarget::UserFile),
            Self::Workspace => Some(McpConfigTarget::WorkspaceFile),
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Runtime => "runtime only",
            Self::User => "user config.toml",
            Self::Workspace => "project config.toml",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum McpFormFocus {
    Name,
    Transport,
    Endpoint,
    Extras,
    Environment,
    Cwd,
    Enabled,
    Lazy,
    Persist,
    Submit,
}

pub(super) struct McpForm {
    pub(super) editing: bool,
    pub(super) original_name: Option<String>,
    pub(super) name: InputState,
    pub(super) transport: McpTransport,
    pub(super) endpoint: InputState,
    pub(super) extras: InputState,
    pub(super) environment: InputState,
    pub(super) cwd: InputState,
    pub(super) enabled: bool,
    pub(super) lazy: bool,
    pub(super) persist: PersistChoice,
    pub(super) focus: McpFormFocus,
    base: McpServerDefinition,
}

impl McpForm {
    pub(super) fn add() -> Self {
        Self {
            editing: false,
            original_name: None,
            name: InputState::default(),
            transport: McpTransport::Stdio,
            endpoint: InputState::default(),
            extras: json_input("[]"),
            environment: json_input("{}"),
            cwd: InputState::default(),
            enabled: true,
            lazy: false,
            persist: PersistChoice::Runtime,
            focus: McpFormFocus::Name,
            base: McpServerDefinition {
                command: Some(String::new()),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
                url: None,
                headers: BTreeMap::new(),
                oauth: None,
                enabled: true,
                lazy: false,
                timeout_ms: None,
            },
        }
    }

    pub(super) fn edit(server: &McpServerInfo) -> Self {
        let transport = if server.definition.command.is_some() {
            McpTransport::Stdio
        } else {
            McpTransport::Http
        };
        let endpoint = server
            .definition
            .command
            .as_ref()
            .or(server.definition.url.as_ref())
            .cloned()
            .unwrap_or_default();
        let extras = match transport {
            McpTransport::Stdio => serde_json::to_string(&server.definition.args),
            McpTransport::Http => serde_json::to_string(&server.definition.headers),
        }
        .unwrap_or_default();
        Self {
            editing: true,
            original_name: Some(server.name.clone()),
            name: json_input(&server.name),
            transport,
            endpoint: json_input(&endpoint),
            extras: json_input(&extras),
            environment: json_input(
                &serde_json::to_string(&server.definition.env).unwrap_or_default(),
            ),
            cwd: json_input(server.definition.cwd.as_deref().unwrap_or_default()),
            enabled: server.definition.enabled,
            lazy: server.definition.lazy,
            persist: PersistChoice::Runtime,
            focus: McpFormFocus::Name,
            base: server.definition.clone(),
        }
    }

    pub(super) fn move_focus(&mut self, backward: bool) {
        const STDIO_FIELDS: [McpFormFocus; 10] = [
            McpFormFocus::Name,
            McpFormFocus::Transport,
            McpFormFocus::Endpoint,
            McpFormFocus::Extras,
            McpFormFocus::Environment,
            McpFormFocus::Cwd,
            McpFormFocus::Enabled,
            McpFormFocus::Lazy,
            McpFormFocus::Persist,
            McpFormFocus::Submit,
        ];
        const HTTP_FIELDS: [McpFormFocus; 8] = [
            McpFormFocus::Name,
            McpFormFocus::Transport,
            McpFormFocus::Endpoint,
            McpFormFocus::Extras,
            McpFormFocus::Enabled,
            McpFormFocus::Lazy,
            McpFormFocus::Persist,
            McpFormFocus::Submit,
        ];
        let fields: &[McpFormFocus] = match self.transport {
            McpTransport::Stdio => &STDIO_FIELDS,
            McpTransport::Http => &HTTP_FIELDS,
        };
        let index = fields
            .iter()
            .position(|field| *field == self.focus)
            .unwrap_or(0);
        self.focus = fields[if backward {
            (index + fields.len() - 1) % fields.len()
        } else {
            (index + 1) % fields.len()
        }];
    }

    pub(super) fn cycle_choice(&mut self, backward: bool) {
        match self.focus {
            McpFormFocus::Transport => {
                self.transport = match self.transport {
                    McpTransport::Stdio => McpTransport::Http,
                    McpTransport::Http => McpTransport::Stdio,
                };
                self.extras.set_buffer(match self.transport {
                    McpTransport::Stdio => "[]".into(),
                    McpTransport::Http => "{}".into(),
                });
            }
            McpFormFocus::Enabled => self.enabled = !self.enabled,
            McpFormFocus::Lazy => self.lazy = !self.lazy,
            McpFormFocus::Persist => {
                self.persist = match (self.persist, backward) {
                    (PersistChoice::Runtime, false) | (PersistChoice::Workspace, true) => {
                        PersistChoice::User
                    }
                    (PersistChoice::User, false) => PersistChoice::Workspace,
                    (PersistChoice::Workspace, false) | (PersistChoice::User, true) => {
                        PersistChoice::Runtime
                    }
                    (PersistChoice::Runtime, true) => PersistChoice::Workspace,
                }
            }
            _ => {}
        }
    }

    pub(super) fn focused_input(&mut self) -> Option<&mut InputState> {
        match self.focus {
            McpFormFocus::Name => Some(&mut self.name),
            McpFormFocus::Endpoint => Some(&mut self.endpoint),
            McpFormFocus::Extras => Some(&mut self.extras),
            McpFormFocus::Environment => Some(&mut self.environment),
            McpFormFocus::Cwd => Some(&mut self.cwd),
            _ => None,
        }
    }

    pub(super) fn definition(&self) -> Result<(String, McpServerDefinition), String> {
        let name = self.name.as_str().trim().to_owned();
        if name.is_empty() {
            return Err("server name is required".into());
        }
        let endpoint = self.endpoint.as_str().trim().to_owned();
        if endpoint.is_empty() {
            return Err("command or URL is required".into());
        }
        let mut definition = self.base.clone();
        definition.enabled = self.enabled;
        definition.lazy = self.lazy;
        match self.transport {
            McpTransport::Stdio => {
                definition.command = Some(endpoint);
                definition.url = None;
                definition.headers.clear();
                definition.args = serde_json::from_str(self.extras.as_str())
                    .map_err(|error| format!("args must be a JSON string array: {error}"))?;
                definition.env = serde_json::from_str(self.environment.as_str())
                    .map_err(|error| format!("env must be a JSON string object: {error}"))?;
                let cwd = self.cwd.as_str().trim();
                definition.cwd = (!cwd.is_empty()).then(|| cwd.to_owned());
            }
            McpTransport::Http => {
                definition.command = None;
                definition.args.clear();
                definition.env.clear();
                definition.cwd = None;
                definition.url = Some(endpoint);
                definition.headers = serde_json::from_str(self.extras.as_str())
                    .map_err(|error| format!("headers must be a JSON string object: {error}"))?;
            }
        }
        Ok((name, definition))
    }
}

fn json_input(value: &str) -> InputState {
    let mut input = InputState::default();
    input.set_buffer(value.to_owned());
    input
}

#[derive(Default)]
pub(super) struct McpPanel {
    pub(super) servers: Vec<McpServerInfo>,
    pub(super) selection: ListState,
    pub(super) form: Option<McpForm>,
    pub(super) refresh_in_flight: bool,
    pub(super) auth: Option<McpAuthView>,
}

pub(super) struct McpAuthView {
    pub(super) server: String,
    pub(super) authorization_url: String,
}

impl McpPanel {
    pub(super) fn install(&mut self, mut servers: Vec<McpServerInfo>) {
        servers.sort_by(|left, right| left.name.cmp(&right.name));
        self.servers = servers;
        self.refresh_in_flight = false;
        if self.auth.as_ref().is_some_and(|auth| {
            !self.servers.iter().any(|server| {
                server.name == auth.server
                    && server.state == McpServerState::NeedsAuth
                    && server.auth_in_progress == Some(true)
            })
        }) {
            self.auth = None;
        }
        self.clamp();
    }

    pub(super) fn selected(&self) -> Option<&McpServerInfo> {
        self.selection
            .selected()
            .and_then(|index| self.servers.get(index))
    }

    pub(super) fn clamp(&mut self) {
        self.selection.select((!self.servers.is_empty()).then(|| {
            self.selection
                .selected()
                .unwrap_or(0)
                .min(self.servers.len() - 1)
        }));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PermissionRow {
    pub(super) action: PermissionAction,
    pub(super) resource: String,
    pub(super) effect: PermissionEffect,
    pub(super) source: PermissionRuleSource,
}

pub(super) fn permission_rows(result: &SessionPermissionGetResult) -> Vec<PermissionRow> {
    result
        .permissions
        .iter()
        .flat_map(|action| {
            std::iter::once(PermissionRow {
                action: action.action,
                resource: "*".into(),
                effect: action.effect,
                source: action.source,
            })
            .chain(action.patterns.iter().map(|rule| PermissionRow {
                action: action.action,
                resource: rule.resource.as_str().to_owned(),
                effect: rule.effect,
                source: rule.source,
            }))
        })
        .collect()
}

pub(super) struct PermissionForm {
    pub(super) action: PermissionAction,
    pub(super) pattern: InputState,
    pub(super) effect: PermissionEffect,
    pub(super) focus_pattern: bool,
}

impl PermissionForm {
    pub(super) fn new(action: PermissionAction) -> Self {
        Self {
            action,
            pattern: InputState::default(),
            effect: PermissionEffect::Ask,
            focus_pattern: true,
        }
    }

    pub(super) fn cycle_action(&mut self, backward: bool) {
        let actions = [
            PermissionAction::Read,
            PermissionAction::Write,
            PermissionAction::Bash,
            PermissionAction::Delegate,
            PermissionAction::Mcp,
        ];
        let index = actions
            .iter()
            .position(|action| *action == self.action)
            .unwrap_or(0);
        self.action = actions[if backward {
            (index + actions.len() - 1) % actions.len()
        } else {
            (index + 1) % actions.len()
        }];
    }
}

#[derive(Default)]
pub(super) struct PermissionPanel {
    pub(super) result: Option<SessionPermissionGetResult>,
    pub(super) selection: ListState,
    pub(super) form: Option<PermissionForm>,
}

#[derive(Default)]
pub(super) struct SkillPanel {
    pub(super) result: Option<SkillsListResult>,
    pub(super) selection: ListState,
}

impl SkillPanel {
    pub(super) fn install(&mut self, result: SkillsListResult) {
        self.result = Some(result);
        let len = self.result.as_ref().map_or(0, |result| result.skills.len());
        self.selection.select((len > 0).then_some(0));
    }
}

#[derive(Default)]
pub(super) struct UsagePanel {
    pub(super) session: Option<SessionUsageResult>,
    pub(super) tree: Option<SessionTreeUsageResult>,
    pub(super) tree_corrupt: bool,
    pub(super) loading: bool,
    pub(super) scroll: u16,
    max_scroll: u16,
    page_size: u16,
}

impl UsagePanel {
    pub(super) fn begin_load(&mut self) {
        self.session = None;
        self.tree = None;
        self.tree_corrupt = false;
        self.loading = true;
        self.scroll = 0;
        self.max_scroll = 0;
    }

    pub(super) fn scroll_up(&mut self, lines: u16) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    pub(super) fn scroll_down(&mut self, lines: u16) {
        self.scroll = self.scroll.saturating_add(lines).min(self.max_scroll);
    }

    pub(super) fn page_up(&mut self) {
        self.scroll_up(self.page_size.max(1));
    }

    pub(super) fn page_down(&mut self) {
        self.scroll_down(self.page_size.max(1));
    }
}

impl PermissionPanel {
    pub(super) fn begin_load(&mut self) {
        self.result = None;
        self.form = None;
        self.selection.select(None);
    }

    pub(super) fn rows(&self) -> Vec<PermissionRow> {
        self.result
            .as_ref()
            .map(permission_rows)
            .unwrap_or_default()
    }

    pub(super) fn selected(&self) -> Option<PermissionRow> {
        self.selection
            .selected()
            .and_then(|index| self.rows().get(index).cloned())
    }

    pub(super) fn install(&mut self, result: SessionPermissionGetResult) {
        self.result = Some(result);
        let len = self.rows().len();
        self.selection
            .select((len > 0).then(|| self.selection.selected().unwrap_or(0).min(len - 1)));
    }
}

pub(super) fn render_mcp(frame: &mut Frame, area: Rect, panel: &mut McpPanel, theme: &Theme) {
    paint_panel(frame, area, theme);
    if let Some(form) = panel.form.as_ref() {
        render_mcp_form(frame, area, form, theme);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);
    let rows = panel
        .servers
        .iter()
        .map(|server| {
            let state = mcp_state_label(server.state);
            let tools = (server.tool_count > 0).then(|| format!(" ({})", server.tool_count));
            ListItem::new(format!(
                "{}  {}{}  [{:?}]",
                server.name,
                state,
                tools.unwrap_or_default(),
                server.source
            ))
        })
        .collect::<Vec<_>>();
    frame.render_stateful_widget(
        List::new(rows)
            .highlight_symbol("> ")
            .highlight_style(theme.selected())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.panel_border())
                    .title("MCP servers")
                    .title_bottom(
                        Line::from(Span::styled(
                            "n add | e edit | d remove | space toggle | r reconnect | esc close",
                            theme.internal(),
                        ))
                        .right_aligned(),
                    ),
            ),
        chunks[0],
        &mut panel.selection,
    );
    let detail = panel.selected().map_or_else(
        || "No MCP servers configured.".to_owned(),
        |server| {
            let connection = definition_display(&server.definition);
            let mut text = format!("{}\n{}", server.name, connection);
            if let Some(message) = &server.message {
                text.push_str("\n\n");
                text.push_str(message);
            }
            if let Some(auth) = panel
                .auth
                .as_ref()
                .filter(|auth| auth.server == server.name)
            {
                text.push_str("\n\nAuthorization URL:\n");
                text.push_str(&auth.authorization_url);
            }
            text
        },
    );
    let hint = panel.selected().and_then(|server| {
        if panel
            .auth
            .as_ref()
            .is_some_and(|auth| auth.server == server.name)
        {
            Some("c copy URL | esc cancel")
        } else if server.state == McpServerState::NeedsAuth {
            Some("a authenticate")
        } else {
            None
        }
    });
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border())
        .title("Connection details");
    if let Some(hint) = hint {
        block =
            block.title_bottom(Line::from(Span::styled(hint, theme.internal())).right_aligned());
    }
    frame.render_widget(
        Paragraph::new(detail)
            .wrap(Wrap { trim: false })
            .block(block),
        chunks[1],
    );
}

fn render_mcp_form(frame: &mut Frame, area: Rect, form: &McpForm, theme: &Theme) {
    let transport = match form.transport {
        McpTransport::Stdio => "stdio",
        McpTransport::Http => "http",
    };
    let extras_label = match form.transport {
        McpTransport::Stdio => "Args JSON array",
        McpTransport::Http => "Headers JSON object",
    };
    let mut rows = vec![
        (McpFormFocus::Name, "Name", form.name.as_str().to_owned()),
        (McpFormFocus::Transport, "Transport", transport.into()),
        (
            McpFormFocus::Endpoint,
            "Command / URL",
            form.endpoint.as_str().to_owned(),
        ),
        (
            McpFormFocus::Extras,
            extras_label,
            form.extras.as_str().to_owned(),
        ),
    ];
    if form.transport == McpTransport::Stdio {
        rows.extend([
            (
                McpFormFocus::Environment,
                "Env JSON object",
                form.environment.as_str().to_owned(),
            ),
            (
                McpFormFocus::Cwd,
                "Working directory",
                form.cwd.as_str().to_owned(),
            ),
        ]);
    }
    rows.extend([
        (McpFormFocus::Enabled, "Enabled", form.enabled.to_string()),
        (McpFormFocus::Lazy, "Lazy", form.lazy.to_string()),
        (McpFormFocus::Persist, "Save", form.persist.label().into()),
        (McpFormFocus::Submit, "Submit", "press enter".into()),
    ]);
    let lines = rows
        .into_iter()
        .map(|(focus, label, value)| {
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
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border())
                .title(if form.editing {
                    "Edit MCP server"
                } else {
                    "Add MCP server"
                })
                .title_bottom(
                    Line::from(Span::styled(
                        "tab fields | arrows choices | enter save | esc cancel",
                        theme.internal(),
                    ))
                    .right_aligned(),
                ),
        ),
        area,
    );
}

pub(super) fn render_permissions(
    frame: &mut Frame,
    area: Rect,
    panel: &mut PermissionPanel,
    theme: &Theme,
) {
    paint_panel(frame, area, theme);
    if let Some(form) = &panel.form {
        let action = format!("{:?}", form.action).to_ascii_lowercase();
        let effect = format!("{:?}", form.effect).to_ascii_lowercase();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!("Action: {action}")),
                Line::from(format!("Pattern: {}", form.pattern.as_str())),
                Line::from(format!("Effect: {effect}")),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.panel_border())
                    .title("Add permission pattern")
                    .title_bottom(
                        Line::from(Span::styled(
                            "tab field | arrows action/effect | enter add | esc cancel",
                            theme.internal(),
                        ))
                        .right_aligned(),
                    ),
            ),
            area,
        );
        return;
    }
    let rows = panel.rows();
    let items = rows
        .iter()
        .map(|row| {
            ListItem::new(format!(
                "{}  {}  {}  [{}]",
                action_label(row.action),
                row.resource,
                effect_label(row.effect),
                source_label(row.source),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_style(theme.selected())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.panel_border())
                    .title("Session permissions")
                    .title_bottom(
                        Line::from(Span::styled(
                            "arrows effect | n add pattern | d clear overlay | esc close",
                            theme.internal(),
                        ))
                        .right_aligned(),
                    ),
            ),
        area,
        &mut panel.selection,
    );
}

pub(super) fn render_skills(frame: &mut Frame, area: Rect, panel: &mut SkillPanel, theme: &Theme) {
    paint_panel(frame, area, theme);
    let items = panel
        .result
        .as_ref()
        .map(|result| {
            result
                .skills
                .iter()
                .map(|skill| {
                    ListItem::new(format!(
                        "{}  {:?}  {:?}  {}  {}",
                        skill.name,
                        skill.source,
                        skill.permission_effect,
                        if skill.precedence_winner {
                            "winner"
                        } else {
                            "shadowed"
                        },
                        skill.location
                    ))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_style(theme.selected())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.panel_border())
                    .title("Skills")
                    .title_bottom(
                        Line::from(Span::styled("arrows move | esc close", theme.internal()))
                            .right_aligned(),
                    ),
            ),
        area,
        &mut panel.selection,
    );
}

pub(super) fn render_usage(frame: &mut Frame, area: Rect, panel: &mut UsagePanel, theme: &Theme) {
    paint_panel(frame, area, theme);
    let base_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border())
        .title(Line::from(Span::styled("Usage", theme.heading())));
    let inner = base_block.inner(area);
    let mut content_width = usize::from(inner.width);
    let mut lines = usage_panel_lines(panel, content_width, theme);
    let mut scrollable = lines.len() > usize::from(inner.height);
    if scrollable {
        content_width = content_width.saturating_sub(2);
        lines = usage_panel_lines(panel, content_width, theme);
        scrollable = lines.len() > usize::from(inner.height);
    }
    let content_length = lines.len();
    panel.page_size = inner.height;
    panel.max_scroll =
        u16::try_from(content_length.saturating_sub(usize::from(inner.height))).unwrap_or(u16::MAX);
    panel.scroll = panel.scroll.min(panel.max_scroll);

    let hint = if scrollable {
        "arrows scroll | esc close"
    } else {
        "esc close"
    };
    let block =
        base_block.title_bottom(Line::from(Span::styled(hint, theme.internal())).right_aligned());
    frame.render_widget(block, area);

    let paragraph_area = Rect {
        width: inner.width.saturating_sub(if scrollable { 2 } else { 0 }),
        ..inner
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme.body())
            .scroll((panel.scroll, 0)),
        paragraph_area,
    );
    if scrollable {
        let mut state = ScrollbarState::new(content_length)
            .position(usize::from(panel.scroll))
            .viewport_content_length(usize::from(inner.height));
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .track_style(theme.panel_border())
                .thumb_symbol("█")
                .thumb_style(theme.muted()),
            inner,
            &mut state,
        );
    }
}

fn usage_panel_lines(panel: &UsagePanel, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    if panel.loading {
        return vec![Line::from(Span::styled("Loading usage...", theme.body()))];
    }

    let usages = [
        panel.session.as_ref().map(|result| &result.usage),
        panel.tree.as_ref().map(|result| &result.usage),
    ];
    let stats = StatsWidths::from_usages(usages);
    let table = TableWidths::from_usages(usages);
    let mut lines = usage_section_lines(
        "This session",
        None,
        false,
        None,
        usages[0],
        width,
        &stats,
        &table,
        theme,
    );
    lines.push(Line::default());
    let tree_session_count = panel.tree.as_ref().map(|result| result.session_count);
    let qualifier = tree_session_count.map(|count| format!("· {} sessions", format_count(count)));
    lines.extend(usage_section_lines(
        "Session tree",
        qualifier.as_deref(),
        tree_session_count == Some(1),
        panel
            .tree_corrupt
            .then_some("Tree usage unavailable: corrupted delegation record"),
        usages[1],
        width,
        &stats,
        &table,
        theme,
    ));
    lines
}

#[allow(clippy::too_many_arguments)]
fn usage_section_lines(
    title: &str,
    qualifier: Option<&str>,
    degenerate_tree: bool,
    unavailable_message: Option<&'static str>,
    usage: Option<&UsageRollup>,
    width: usize,
    stats: &StatsWidths,
    table: &TableWidths,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![section_header(title, qualifier, width, theme)];
    if let Some(message) = unavailable_message {
        lines.push(Line::from(Span::styled(message, theme.muted())));
        return lines;
    }
    let Some(usage) = usage else {
        lines.push(Line::from(Span::styled(
            "No usage available.",
            theme.muted(),
        )));
        return lines;
    };
    if degenerate_tree {
        lines.push(Line::from(Span::styled(
            "No delegated sessions — totals match this session.",
            theme.muted(),
        )));
        return lines;
    }
    if usage.request_count == 0 {
        lines.push(Line::from(Span::styled(
            "No usage recorded yet.",
            theme.muted(),
        )));
        return lines;
    }
    lines.extend(stats.lines(usage, width, theme));
    if !usage.by_model.is_empty() {
        lines.push(Line::default());
        lines.extend(table.lines(usage, width, theme));
    }
    lines
}

fn section_header(
    title: &str,
    qualifier: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = vec![Span::styled(title.to_owned(), theme.heading())];
    let mut used = title.len();
    if let Some(qualifier) = qualifier {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(qualifier.to_owned(), theme.muted()));
        used += qualifier.len() + 1;
    }
    let dashes = width.saturating_sub(used + 1);
    if dashes >= 4 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("─".repeat(dashes), theme.panel_border()));
    }
    Line::from(spans)
}

#[derive(Default)]
struct StatsWidths {
    values: [usize; 3],
    stacked_value: usize,
}

impl StatsWidths {
    fn from_usages(usages: [Option<&UsageRollup>; 2]) -> Self {
        let mut widths = Self::default();
        for usage in usages.into_iter().flatten() {
            let values = stats_values(usage);
            for (index, value) in values.iter().enumerate() {
                let column = match index {
                    2 | 5 => 1,
                    3 | 6 => 2,
                    _ => 0,
                };
                widths.values[column] = widths.values[column].max(value.len());
                widths.stacked_value = widths.stacked_value.max(value.len());
            }
        }
        widths
    }

    fn lines(&self, usage: &UsageRollup, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let values = stats_values(usage);
        let grid_width =
            (10 + 1 + self.values[0]) + 3 + (6 + 1 + self.values[1]) + 3 + (9 + 1 + self.values[2]);
        if grid_width > width {
            return [
                "Requests",
                "Input",
                "Output",
                "Reasoning",
                "Cache read",
                "Cache write",
                "Cache hit",
                "Cost",
            ]
            .into_iter()
            .zip(values)
            .map(|(label, value)| stat_line(&[(label, 11, value, self.stacked_value)], theme))
            .collect();
        }
        vec![
            stat_line(
                &[("Requests", 10, values[0].clone(), self.values[0])],
                theme,
            ),
            stat_line(
                &[
                    ("Input", 10, values[1].clone(), self.values[0]),
                    ("Output", 6, values[2].clone(), self.values[1]),
                    ("Reasoning", 9, values[3].clone(), self.values[2]),
                ],
                theme,
            ),
            stat_line(
                &[
                    ("Cache read", 10, values[4].clone(), self.values[0]),
                    ("Write", 6, values[5].clone(), self.values[1]),
                    ("Hit", 9, values[6].clone(), self.values[2]),
                ],
                theme,
            ),
            stat_line(&[("Cost", 10, values[7].clone(), self.values[0])], theme),
        ]
    }
}

fn stats_values(usage: &UsageRollup) -> [String; 8] {
    [
        format_count(usage.request_count),
        format_count(usage.input_tokens),
        format_count(usage.output_tokens),
        format_count(usage.reasoning_tokens),
        format_count(usage.cache_read_tokens),
        format_count(usage.cache_write_tokens),
        format_hit_rate(usage.cache_hit_rate),
        format_cost(usage.estimated_cost_usd),
    ]
}

fn stat_line(cells: &[(&str, usize, String, usize)], theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (label, label_width, value, value_width)) in cells.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(
            format!("{label:<label_width$} "),
            theme.muted(),
        ));
        let style = if value == "unpriced" || value == "n/a" {
            theme.muted()
        } else {
            theme.body()
        };
        spans.push(Span::styled(format!("{value:>value_width$}"), style));
    }
    Line::from(spans)
}

#[derive(Default)]
struct TableWidths {
    req: usize,
    input: usize,
    output: usize,
    reasoning: usize,
    hit: usize,
    cost: usize,
}

impl TableWidths {
    fn from_usages(usages: [Option<&UsageRollup>; 2]) -> Self {
        let mut widths = Self {
            req: 3,
            input: 5,
            output: 6,
            reasoning: 9,
            hit: 3,
            cost: 4,
        };
        for usage in usages.into_iter().flatten() {
            for model in usage.by_model.values() {
                widths.req = widths.req.max(format_count(model.request_count).len());
                widths.input = widths.input.max(format_count(model.input_tokens).len());
                widths.output = widths.output.max(format_count(model.output_tokens).len());
                widths.reasoning = widths
                    .reasoning
                    .max(format_count(model.reasoning_tokens).len());
                widths.hit = widths.hit.max(format_hit_rate(model.cache_hit_rate).len());
                widths.cost = widths.cost.max(format_cost(model.estimated_cost_usd).len());
            }
        }
        widths
    }

    fn lines(&self, usage: &UsageRollup, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let columns = self.columns(width);
        let mut lines = vec![self.header_line(&columns, theme)];
        let mut models = usage.by_model.iter().collect::<Vec<_>>();
        models.sort_by(|(name_a, usage_a), (name_b, usage_b)| {
            match (usage_a.estimated_cost_usd, usage_b.estimated_cost_usd) {
                (Some(a), Some(b)) => b.total_cmp(&a),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
            .then_with(|| usage_b.input_tokens.cmp(&usage_a.input_tokens))
            .then_with(|| name_a.cmp(name_b))
        });
        lines.extend(
            models
                .into_iter()
                .map(|(name, model)| self.model_line(name.to_string(), model, &columns, theme)),
        );
        lines
    }

    fn columns(&self, width: usize) -> TableColumns {
        let mut visible = [true; 5];
        let column_widths = [self.req, self.input, self.output, self.reasoning, self.hit];
        let model_width = |visible: &[bool; 5]| {
            let fixed = 2
                + 2
                + self.cost
                + visible
                    .iter()
                    .zip(column_widths)
                    .filter_map(|(visible, width)| visible.then_some(2 + width))
                    .sum::<usize>();
            width.saturating_sub(fixed)
        };
        for drop in [3, 4, 2, 0, 1] {
            if model_width(&visible) >= 16 {
                break;
            }
            visible[drop] = false;
        }
        TableColumns {
            visible,
            model: model_width(&visible).max(8),
        }
    }

    fn header_line(&self, columns: &TableColumns, theme: &Theme) -> Line<'static> {
        let mut text = format!("  {:<width$}", "MODEL", width = columns.model);
        self.push_columns(
            &mut text,
            columns,
            ["REQ", "INPUT", "OUTPUT", "REASONING", "HIT"],
            "COST",
        );
        Line::from(Span::styled(text, theme.muted()))
    }

    fn model_line(
        &self,
        name: String,
        model: &ModelUsageRollup,
        columns: &TableColumns,
        theme: &Theme,
    ) -> Line<'static> {
        let name = truncate_with_ellipsis(&name, columns.model);
        let values = [
            format_count(model.request_count),
            format_count(model.input_tokens),
            format_count(model.output_tokens),
            format_count(model.reasoning_tokens),
            format_hit_rate(model.cache_hit_rate),
        ];
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(
                format!("{name:<width$}", width = columns.model),
                theme.body(),
            ),
        ];
        let widths = [self.req, self.input, self.output, self.reasoning, self.hit];
        for (index, value) in values.into_iter().enumerate() {
            if columns.visible[index] {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("{value:>width$}", width = widths[index]),
                    if value == "n/a" {
                        theme.muted()
                    } else {
                        theme.body()
                    },
                ));
            }
        }
        let cost = format_cost(model.estimated_cost_usd);
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{cost:>width$}", width = self.cost),
            if cost == "unpriced" {
                theme.muted()
            } else {
                theme.body()
            },
        ));
        Line::from(spans)
    }

    fn push_columns(
        &self,
        text: &mut String,
        columns: &TableColumns,
        values: [&str; 5],
        cost: &str,
    ) {
        let widths = [self.req, self.input, self.output, self.reasoning, self.hit];
        for (index, value) in values.into_iter().enumerate() {
            if columns.visible[index] {
                text.push_str(&format!("  {value:>width$}", width = widths[index]));
            }
        }
        text.push_str(&format!("  {cost:>width$}", width = self.cost));
    }
}

struct TableColumns {
    visible: [bool; 5],
    model: usize,
}

fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let first = digits.len() % 3;
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    if first > 0 {
        output.push_str(&digits[..first]);
    }
    for chunk in digits.as_bytes()[first..].chunks(3) {
        if !output.is_empty() {
            output.push(',');
        }
        output.push_str(std::str::from_utf8(chunk).expect("decimal digits are UTF-8"));
    }
    output
}

fn format_hit_rate(rate: Option<f64>) -> String {
    rate.map_or_else(|| "n/a".to_owned(), |rate| format!("{:.1}%", rate * 100.0))
}

fn format_cost(cost: Option<f64>) -> String {
    cost.map_or_else(|| "unpriced".to_owned(), format_cost_usd)
}

fn definition_display(definition: &McpServerDefinition) -> String {
    if let Some(command) = &definition.command {
        let mut parts = vec![command.clone()];
        parts.extend(definition.args.clone());
        let mut value = parts.join(" ");
        if let Some(cwd) = &definition.cwd {
            value.push_str(&format!("\ncwd: {cwd}"));
        }
        for (name, item) in &definition.env {
            value.push_str(&format!("\nenv {name}={item}"));
        }
        value
    } else {
        let mut value = definition.url.clone().unwrap_or_default();
        for (name, item) in &definition.headers {
            value.push_str(&format!("\n{name}: {item}"));
        }
        value
    }
}

fn mcp_state_label(state: cookie_agent_protocol::McpServerState) -> &'static str {
    match state {
        cookie_agent_protocol::McpServerState::Connected => "connected",
        cookie_agent_protocol::McpServerState::Connecting => "connecting",
        cookie_agent_protocol::McpServerState::Failed => "failed",
        cookie_agent_protocol::McpServerState::NeedsAuth => "needs_auth",
        cookie_agent_protocol::McpServerState::Disabled => "disabled",
        cookie_agent_protocol::McpServerState::LazyNotConnected => "lazy-not-connected",
        cookie_agent_protocol::McpServerState::Disconnected => "disconnected",
    }
}

fn action_label(action: PermissionAction) -> &'static str {
    match action {
        PermissionAction::Read => "read",
        PermissionAction::Write => "write",
        PermissionAction::Bash => "bash",
        PermissionAction::Delegate => "delegate",
        PermissionAction::Mcp => "mcp",
        PermissionAction::Plugin => "plugin",
        PermissionAction::Skill => "skill",
    }
}

fn effect_label(effect: PermissionEffect) -> &'static str {
    match effect {
        PermissionEffect::Allow => "allow",
        PermissionEffect::Ask => "ask",
        PermissionEffect::Deny => "deny",
    }
}

fn source_label(source: PermissionRuleSource) -> &'static str {
    match source {
        PermissionRuleSource::SessionOverlay => "session_overlay",
        PermissionRuleSource::AgentDocument => "agent_document",
        PermissionRuleSource::Default => "default",
    }
}

pub(super) fn cycle_effect(effect: PermissionEffect, backward: bool) -> PermissionEffect {
    match (effect, backward) {
        (PermissionEffect::Allow, false) | (PermissionEffect::Deny, true) => PermissionEffect::Ask,
        (PermissionEffect::Ask, false) => PermissionEffect::Deny,
        (PermissionEffect::Deny, false) | (PermissionEffect::Ask, true) => PermissionEffect::Allow,
        (PermissionEffect::Allow, true) => PermissionEffect::Deny,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cookie_agent_protocol::{
        EffectivePermissionAction, McpConfigSource, McpOAuthDefinition, McpServerDefinition,
        McpServerInfo, McpServerState, ModelUsageRollup, PermissionAction, PermissionEffect,
        PermissionRuleSource, SessionId, SessionPermissionGetResult, SessionTreeUsageResult,
        SessionUsageResult, SkillDescriptor, SkillSource, SkillsListResult, UsageRollup,
    };
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn permission_rows_keep_action_defaults_and_sources() {
        let result = SessionPermissionGetResult {
            permissions: vec![EffectivePermissionAction {
                action: PermissionAction::Bash,
                effect: PermissionEffect::Deny,
                source: PermissionRuleSource::SessionOverlay,
                patterns: Vec::new(),
            }],
            current_mode: None,
        };
        assert_eq!(
            super::permission_rows(&result),
            vec![super::PermissionRow {
                action: PermissionAction::Bash,
                resource: "*".into(),
                effect: PermissionEffect::Deny,
                source: PermissionRuleSource::SessionOverlay,
            }]
        );
    }

    #[test]
    fn stdio_mcp_form_accepts_args_environment_and_working_directory() {
        let mut form = super::McpForm::add();
        form.name.set_buffer("local".into());
        form.endpoint.set_buffer("server-command".into());
        form.extras.set_buffer("[\"--stdio\"]".into());
        form.environment.set_buffer("{\"TOKEN\":\"value\"}".into());
        form.cwd.set_buffer("/workspace/tools".into());

        let (name, definition) = form.definition().expect("valid stdio form");
        assert_eq!(name, "local");
        assert_eq!(definition.command.as_deref(), Some("server-command"));
        assert_eq!(definition.args, ["--stdio"]);
        assert_eq!(definition.env["TOKEN"], "value");
        assert_eq!(definition.cwd.as_deref(), Some("/workspace/tools"));
    }

    #[test]
    fn mcp_panel_renders_live_state_and_connection() {
        let mut panel = super::McpPanel::default();
        panel.install(vec![McpServerInfo {
            name: "remote".into(),
            source: McpConfigSource::WorkspaceFile,
            definition: McpServerDefinition {
                command: None,
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
                url: Some("https://example.test/mcp".into()),
                headers: BTreeMap::from([("Authorization".into(), "Bearer test".into())]),
                oauth: Some(McpOAuthDefinition::Bool(true)),
                enabled: true,
                lazy: false,
                timeout_ms: None,
            },
            state: McpServerState::Disconnected,
            tool_count: 0,
            message: None,
            auth_in_progress: Some(false),
        }]);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| {
                super::render_mcp(
                    frame,
                    frame.area(),
                    &mut panel,
                    &crate::theme::Theme::default(),
                );
            })
            .expect("render MCP panel");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("disconnected"), "{text}");
        assert!(text.contains("Authorization: Bearer test"), "{text}");
    }

    #[test]
    fn mcp_panel_renders_copyable_oauth_wait() {
        let mut panel = super::McpPanel::default();
        panel.install(vec![McpServerInfo {
            name: "remote".into(),
            source: McpConfigSource::UserFile,
            definition: McpServerDefinition {
                command: None,
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
                url: Some("https://example.test/mcp".into()),
                headers: BTreeMap::new(),
                oauth: Some(McpOAuthDefinition::Bool(true)),
                enabled: true,
                lazy: false,
                timeout_ms: None,
            },
            state: McpServerState::NeedsAuth,
            tool_count: 0,
            message: Some("waiting for OAuth browser callback".into()),
            auth_in_progress: Some(true),
        }]);
        panel.auth = Some(super::McpAuthView {
            server: "remote".into(),
            authorization_url: "https://auth.example.test/authorize?state=test".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| {
                super::render_mcp(
                    frame,
                    frame.area(),
                    &mut panel,
                    &crate::theme::Theme::default(),
                );
            })
            .expect("render MCP OAuth panel");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("needs_auth"), "{text}");
        assert!(
            text.contains("https://auth.example.test/authorize?state=test"),
            "{text}"
        );
        assert!(text.contains("c copy URL | esc cancel"), "{text}");

        let mut terminal_state = panel.servers.clone();
        terminal_state[0].auth_in_progress = Some(false);
        terminal_state[0].message = Some("OAuth authorization timed out".into());
        panel.install(terminal_state);
        assert!(panel.auth.is_none());
    }

    #[test]
    fn permission_editor_renders_effect_and_source() {
        let mut panel = super::PermissionPanel::default();
        panel.install(SessionPermissionGetResult {
            permissions: vec![EffectivePermissionAction {
                action: PermissionAction::Write,
                effect: PermissionEffect::Deny,
                source: PermissionRuleSource::SessionOverlay,
                patterns: Vec::new(),
            }],
            current_mode: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
        terminal
            .draw(|frame| {
                super::render_permissions(
                    frame,
                    frame.area(),
                    &mut panel,
                    &crate::theme::Theme::default(),
                );
            })
            .expect("render permissions");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("write  *  deny  [session_overlay]"), "{text}");
    }

    #[test]
    fn skills_panel_renders_source_precedence_permission_and_location() {
        let mut panel = super::SkillPanel::default();
        panel.install(SkillsListResult {
            skills: vec![SkillDescriptor {
                name: "release-check".into(),
                description: "Check a release".into(),
                when_to_use: None,
                location: "/workspace/.cookie-agent/skills/release-check/SKILL.md".into(),
                source: SkillSource::Project,
                precedence_winner: true,
                permission_effect: PermissionEffect::Allow,
                visible: true,
                user_invocable: true,
                argument_hint: Some("<tag>".into()),
            }],
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 8)).expect("terminal");
        terminal
            .draw(|frame| {
                super::render_skills(
                    frame,
                    frame.area(),
                    &mut panel,
                    &crate::theme::Theme::default(),
                );
            })
            .expect("render skills");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            text.contains("release-check  Project  Allow  winner"),
            "{text}"
        );
        assert!(text.contains("/workspace/.cookie-agent/skills"), "{text}");
    }

    #[test]
    fn permission_panel_is_noninteractive_while_another_session_loads() {
        let mut panel = super::PermissionPanel::default();
        panel.install(SessionPermissionGetResult {
            permissions: vec![EffectivePermissionAction {
                action: PermissionAction::Write,
                effect: PermissionEffect::Deny,
                source: PermissionRuleSource::SessionOverlay,
                patterns: Vec::new(),
            }],
            current_mode: None,
        });
        assert!(!panel.rows().is_empty());

        panel.begin_load();

        assert!(panel.rows().is_empty());
        assert!(panel.selected().is_none());
    }

    #[test]
    fn usage_panel_renders_both_sections_sorted_models_and_unpriced_values() {
        let session_id = SessionId::new_v7();
        let session_usage = UsageRollup {
            input_tokens: 1_000,
            output_tokens: 200,
            reasoning_tokens: 50,
            cache_read_tokens: 500,
            cache_write_tokens: 100,
            request_count: 3,
            cache_hit_rate: Some(0.5),
            estimated_cost_usd: None,
            by_model: BTreeMap::from([
                (
                    "test/unpriced".parse().unwrap(),
                    ModelUsageRollup {
                        input_tokens: 2_000,
                        request_count: 1,
                        cache_hit_rate: None,
                        estimated_cost_usd: None,
                        ..ModelUsageRollup::default()
                    },
                ),
                (
                    "test/expensive".parse().unwrap(),
                    ModelUsageRollup {
                        input_tokens: 100,
                        request_count: 1,
                        cache_hit_rate: Some(0.0),
                        estimated_cost_usd: Some(0.2),
                        ..ModelUsageRollup::default()
                    },
                ),
                (
                    "test/cheap".parse().unwrap(),
                    ModelUsageRollup {
                        input_tokens: 3_000,
                        request_count: 1,
                        cache_hit_rate: Some(0.5),
                        estimated_cost_usd: Some(0.01),
                        ..ModelUsageRollup::default()
                    },
                ),
            ]),
            ..UsageRollup::default()
        };
        let tree_usage = UsageRollup {
            input_tokens: 4_000,
            output_tokens: 800,
            reasoning_tokens: 100,
            cache_read_tokens: 1_500,
            cache_write_tokens: 200,
            request_count: 6,
            cache_hit_rate: Some(0.375),
            estimated_cost_usd: Some(0.21),
            by_model: BTreeMap::from([(
                "test/tree".parse().unwrap(),
                ModelUsageRollup {
                    input_tokens: 1_000,
                    output_tokens: 200,
                    reasoning_tokens: 50,
                    cache_read_tokens: 500,
                    cache_write_tokens: 100,
                    request_count: 3,
                    cache_hit_rate: Some(0.5),
                    estimated_cost_usd: Some(0.21),
                    ..ModelUsageRollup::default()
                },
            )]),
            ..UsageRollup::default()
        };
        let panel = super::UsagePanel {
            session: Some(SessionUsageResult {
                session_id,
                usage: session_usage,
            }),
            tree: Some(SessionTreeUsageResult {
                session_id,
                usage: tree_usage,
                session_count: 3,
            }),
            loading: false,
            ..super::UsagePanel::default()
        };
        let lines = super::usage_panel_lines(&panel, 100, &crate::theme::Theme::default());
        let text = lines
            .iter()
            .map(ratatui::text::Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("This session"), "{text}");
        assert!(text.contains("Session tree · 3 sessions"), "{text}");
        assert!(text.contains("1,000"), "{text}");
        assert!(text.contains("n/a"), "{text}");
        assert!(text.contains("unpriced"), "{text}");
        let expensive = text.find("test/expensive").expect("expensive model");
        let cheap = text.find("test/cheap").expect("cheap model");
        let unpriced = text.find("test/unpriced").expect("unpriced model");
        assert!(expensive < cheap && cheap < unpriced, "{text}");
    }

    #[test]
    fn usage_counts_are_grouped_before_widths_are_computed() {
        let session_id = SessionId::new_v7();
        let usage = UsageRollup {
            request_count: 12_345,
            by_model: BTreeMap::from([(
                "test/counts".parse().unwrap(),
                ModelUsageRollup {
                    request_count: 1_234_567,
                    ..ModelUsageRollup::default()
                },
            )]),
            ..UsageRollup::default()
        };
        let panel = super::UsagePanel {
            session: Some(SessionUsageResult {
                session_id,
                usage: usage.clone(),
            }),
            tree: Some(SessionTreeUsageResult {
                session_id,
                usage: usage.clone(),
                session_count: 12_345,
            }),
            ..super::UsagePanel::default()
        };
        let text = super::usage_panel_lines(&panel, 100, &crate::theme::Theme::default())
            .iter()
            .map(ratatui::text::Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("· 12,345 sessions"), "{text}");
        assert!(text.contains("12,345"), "{text}");
        assert!(text.contains("1,234,567"), "{text}");
        let widths = super::TableWidths::from_usages([Some(&usage), None]);
        assert_eq!(widths.req, "1,234,567".len());
    }

    #[test]
    fn usage_model_sort_ties_use_input_then_name() {
        let model = |input_tokens| ModelUsageRollup {
            input_tokens,
            request_count: 1,
            estimated_cost_usd: Some(1.0),
            ..ModelUsageRollup::default()
        };
        let usage = UsageRollup {
            request_count: 3,
            by_model: BTreeMap::from([
                ("test/zeta".parse().unwrap(), model(200)),
                ("test/alpha".parse().unwrap(), model(200)),
                ("test/middle".parse().unwrap(), model(300)),
            ]),
            ..UsageRollup::default()
        };
        let text = super::TableWidths::from_usages([Some(&usage), None])
            .lines(&usage, 100, &crate::theme::Theme::default())
            .iter()
            .map(ratatui::text::Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let middle = text.find("test/middle").unwrap();
        let alpha = text.find("test/alpha").unwrap();
        let zeta = text.find("test/zeta").unwrap();
        assert!(middle < alpha && alpha < zeta, "{text}");
    }

    #[test]
    fn usage_panel_degenerate_tree_replaces_totals_with_note() {
        let session_id = SessionId::new_v7();
        let usage = UsageRollup {
            request_count: 1,
            ..UsageRollup::default()
        };
        let panel = super::UsagePanel {
            session: Some(SessionUsageResult {
                session_id,
                usage: usage.clone(),
            }),
            tree: Some(SessionTreeUsageResult {
                session_id,
                usage,
                session_count: 1,
            }),
            ..super::UsagePanel::default()
        };
        let text = super::usage_panel_lines(&panel, 80, &crate::theme::Theme::default())
            .iter()
            .map(ratatui::text::Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Session tree · 1 sessions"), "{text}");
        assert!(
            text.contains("No delegated sessions — totals match this session."),
            "{text}"
        );
    }

    #[test]
    fn usage_table_drops_every_column_in_the_locked_narrow_order() {
        let usage = UsageRollup {
            request_count: 1,
            by_model: BTreeMap::from([(
                "test/a-model-name".parse().unwrap(),
                ModelUsageRollup {
                    request_count: 12_345,
                    input_tokens: 1_000,
                    output_tokens: 2_000,
                    reasoning_tokens: 3_000,
                    cache_hit_rate: Some(0.5),
                    estimated_cost_usd: None,
                    ..ModelUsageRollup::default()
                },
            )]),
            ..UsageRollup::default()
        };
        let widths = super::TableWidths::from_usages([Some(&usage), None]);
        let column_widths = [
            widths.req,
            widths.input,
            widths.output,
            widths.reasoning,
            widths.hit,
        ];
        let fixed_width = |visible: [bool; 5]| {
            2 + 2
                + widths.cost
                + visible
                    .into_iter()
                    .zip(column_widths)
                    .filter_map(|(visible, width)| visible.then_some(2 + width))
                    .sum::<usize>()
        };
        let transitions = [
            [true, true, true, true, true],
            [true, true, true, false, true],
            [true, true, true, false, false],
            [true, true, false, false, false],
            [false, true, false, false, false],
            [false, false, false, false, false],
        ];
        for expected in transitions {
            let columns = widths.columns(fixed_width(expected) + 16);
            assert_eq!(columns.visible, expected);
            assert_eq!(columns.model, 16);
        }
        let floor = widths.columns(fixed_width([false; 5]) + 8);
        assert_eq!(floor.visible, [false; 5]);
        assert_eq!(floor.model, 8);
    }

    #[test]
    fn usage_panel_footer_switches_when_content_scrolls() {
        let session_id = SessionId::new_v7();
        let usage = UsageRollup {
            request_count: 1,
            ..UsageRollup::default()
        };
        let mut fitting = super::UsagePanel {
            tree: Some(SessionTreeUsageResult {
                session_id,
                usage: usage.clone(),
                session_count: 1,
            }),
            session: Some(SessionUsageResult { session_id, usage }),
            ..super::UsagePanel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| {
                super::render_usage(
                    frame,
                    frame.area(),
                    &mut fitting,
                    &crate::theme::Theme::default(),
                );
            })
            .expect("render fitting usage");
        let fitting_text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(fitting_text.contains("esc close"), "{fitting_text}");
        assert!(!fitting_text.contains("arrows scroll"), "{fitting_text}");

        let mut terminal = Terminal::new(TestBackend::new(50, 8)).expect("terminal");
        terminal
            .draw(|frame| {
                super::render_usage(
                    frame,
                    frame.area(),
                    &mut fitting,
                    &crate::theme::Theme::default(),
                );
            })
            .expect("render scrolling usage");
        let scrolling_text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            scrolling_text.contains("arrows scroll | esc close"),
            "{scrolling_text}"
        );
        let buffer = terminal.backend().buffer();
        for y in 1..7 {
            assert_eq!(buffer[(47, y)].symbol(), " ", "reserved gutter at row {y}");
            assert!(
                matches!(buffer[(48, y)].symbol(), "│" | "█"),
                "scrollbar at row {y}: {}",
                buffer[(48, y)].symbol()
            );
        }
    }
}
