use std::collections::BTreeMap;

use cookie_agent_protocol::{
    McpConfigTarget, McpPendingApproval, McpServerDefinition, McpServerInfo, PermissionAction,
    PermissionEffect, PermissionRuleSource, SessionPermissionGetResult,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::theme::Theme;

use super::{app::paint_panel, input::InputState};

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
    pub(super) approvals: BTreeMap<String, McpPendingApproval>,
    pub(super) selection: ListState,
    pub(super) form: Option<McpForm>,
    pub(super) refresh_in_flight: bool,
}

impl McpPanel {
    pub(super) fn install(
        &mut self,
        mut servers: Vec<McpServerInfo>,
        approvals: Vec<McpPendingApproval>,
    ) {
        servers.sort_by(|left, right| left.name.cmp(&right.name));
        self.servers = servers;
        self.approvals = approvals
            .into_iter()
            .map(|approval| (approval.server.clone(), approval))
            .collect();
        self.refresh_in_flight = false;
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
            let connection = panel
                .approvals
                .get(&server.name)
                .map(|approval| approval.connection.clone())
                .unwrap_or_else(|| definition_display(&server.definition));
            let mut text = format!("{}\n{}", server.name, connection);
            if let Some(message) = &server.message {
                text.push_str("\n\n");
                text.push_str(message);
            }
            text
        },
    );
    let hint = panel
        .selected()
        .is_some_and(|server| panel.approvals.contains_key(&server.name))
        .then_some("a approve | x reject");
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
        cookie_agent_protocol::McpServerState::PendingApproval => "pending_approval",
        cookie_agent_protocol::McpServerState::Disabled => "disabled",
        cookie_agent_protocol::McpServerState::LazyNotConnected => "lazy-not-connected",
        cookie_agent_protocol::McpServerState::Disconnected => "disconnected",
        cookie_agent_protocol::McpServerState::Rejected => "rejected",
    }
}

fn action_label(action: PermissionAction) -> &'static str {
    match action {
        PermissionAction::Read => "read",
        PermissionAction::Write => "write",
        PermissionAction::Bash => "bash",
        PermissionAction::Delegate => "delegate",
        PermissionAction::Mcp => "mcp",
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
        EffectivePermissionAction, McpConfigSource, McpPendingApproval, McpServerDefinition,
        McpServerInfo, McpServerState, PermissionAction, PermissionEffect, PermissionRuleSource,
        SessionPermissionGetResult,
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
    fn mcp_panel_renders_live_state_and_full_pending_connection() {
        let mut panel = super::McpPanel::default();
        panel.install(
            vec![McpServerInfo {
                name: "remote".into(),
                source: McpConfigSource::WorkspaceFile,
                definition: McpServerDefinition {
                    command: None,
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    cwd: None,
                    url: Some("https://example.test/mcp".into()),
                    headers: BTreeMap::from([("Authorization".into(), "Bearer test".into())]),
                    enabled: true,
                    lazy: false,
                    timeout_ms: None,
                },
                state: McpServerState::PendingApproval,
                tool_count: 0,
                message: None,
            }],
            vec![McpPendingApproval {
                server: "remote".into(),
                connection: "https://example.test/mcp\nAuthorization: Bearer test".into(),
            }],
        );
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
        assert!(text.contains("pending_approval"), "{text}");
        assert!(text.contains("Authorization: Bearer test"), "{text}");
        assert!(text.contains("a approve | x reject"), "{text}");
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
    fn permission_panel_is_noninteractive_while_another_session_loads() {
        let mut panel = super::PermissionPanel::default();
        panel.install(SessionPermissionGetResult {
            permissions: vec![EffectivePermissionAction {
                action: PermissionAction::Write,
                effect: PermissionEffect::Deny,
                source: PermissionRuleSource::SessionOverlay,
                patterns: Vec::new(),
            }],
        });
        assert!(!panel.rows().is_empty());

        panel.begin_load();

        assert!(panel.rows().is_empty());
        assert!(panel.selected().is_none());
    }
}
