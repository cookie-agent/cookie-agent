//! The `/mcp` panel's rules every client shares: the servers in display
//! order, which ones `mcp.server.*` edits, a server's add or edit form as
//! text and the definition it makes, and the labels the list shows; and the
//! servers each runtime instance hosts, as the runtime panels list them.

use std::collections::BTreeMap;

use cookie_agent_protocol::{
    McpConfigSource, McpConfigTarget, McpServerDefinition, McpServerInfo, McpServerState, RuntimeId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpTransport {
    Stdio,
    Http,
}

impl McpTransport {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
        }
    }

    /// What the extras field holds for this transport, and its empty value.
    pub const fn extras(self) -> (&'static str, &'static str) {
        match self {
            Self::Stdio => ("Args JSON array", "[]"),
            Self::Http => ("Headers JSON object", "{}"),
        }
    }
}

/// Where an added or edited server is kept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistChoice {
    /// Only in the running daemon.
    Runtime,
    /// Also in the user's `config.toml`.
    User,
}

impl PersistChoice {
    pub const fn target(self) -> Option<McpConfigTarget> {
        match self {
            Self::Runtime => None,
            Self::User => Some(McpConfigTarget::UserFile),
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Runtime => "runtime only",
            Self::User => "user config.toml",
        }
    }
}

/// A server form's fields as typed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpFields {
    pub name: String,
    pub transport: McpTransport,
    /// The command (stdio) or URL (http).
    pub endpoint: String,
    /// The args (stdio) or headers (http), as JSON.
    pub extras: String,
    /// The environment (stdio), as a JSON object.
    pub environment: String,
    pub cwd: String,
    pub enabled: bool,
}

impl McpFields {
    /// A new server's form.
    pub fn add() -> Self {
        Self {
            name: String::new(),
            transport: McpTransport::Stdio,
            endpoint: String::new(),
            extras: "[]".into(),
            environment: "{}".into(),
            cwd: String::new(),
            enabled: true,
        }
    }

    /// `server`'s form.
    pub fn edit(server: &McpServerInfo) -> Self {
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
            name: server.name.clone(),
            transport,
            endpoint,
            extras,
            environment: serde_json::to_string(&server.definition.env).unwrap_or_default(),
            cwd: server.definition.cwd.clone().unwrap_or_default(),
            enabled: server.definition.enabled,
        }
    }

    /// Switches the transport; the extras start over as its empty value.
    pub fn set_transport(&mut self, transport: McpTransport) {
        if self.transport != transport {
            self.transport = transport;
            self.extras = transport.extras().1.to_owned();
        }
    }

    /// The server's name and the definition the fields make over `base`
    /// (what a new server starts from, or the edited server's), or what is
    /// wrong with them.
    pub fn definition(
        &self,
        base: &McpServerDefinition,
    ) -> Result<(String, McpServerDefinition), String> {
        let name = self.name.trim().to_owned();
        if name.is_empty() {
            return Err("server name is required".into());
        }
        let endpoint = self.endpoint.trim().to_owned();
        if endpoint.is_empty() {
            return Err("command or URL is required".into());
        }
        let mut definition = base.clone();
        definition.enabled = self.enabled;
        match self.transport {
            McpTransport::Stdio => {
                definition.command = Some(endpoint);
                definition.url = None;
                definition.headers.clear();
                definition.args = serde_json::from_str(&self.extras)
                    .map_err(|error| format!("args must be a JSON string array: {error}"))?;
                definition.env = serde_json::from_str(&self.environment)
                    .map_err(|error| format!("env must be a JSON string object: {error}"))?;
                let cwd = self.cwd.trim();
                definition.cwd = (!cwd.is_empty()).then(|| cwd.to_owned());
            }
            McpTransport::Http => {
                definition.command = None;
                definition.args.clear();
                definition.env.clear();
                definition.cwd = None;
                definition.url = Some(endpoint);
                definition.headers = serde_json::from_str(&self.extras)
                    .map_err(|error| format!("headers must be a JSON string object: {error}"))?;
            }
        }
        Ok((name, definition))
    }
}

/// What a new server's definition starts from.
pub fn new_definition() -> McpServerDefinition {
    McpServerDefinition {
        command: Some(String::new()),
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: None,
        url: None,
        headers: BTreeMap::new(),
        oauth: None,
        enabled: true,
        timeout_ms: None,
    }
}

/// Servers grouped by runtime instance, the editable global ones first.
pub fn sort_servers(servers: &mut [McpServerInfo]) {
    servers.sort_by(|left, right| {
        (
            !is_global(left),
            left.runtime_id.as_str(),
            &left.workspace,
            &left.name,
        )
            .cmp(&(
                !is_global(right),
                right.runtime_id.as_str(),
                &right.workspace,
                &right.name,
            ))
    });
}

/// The client protocol minor from which `mcp.auth.begin` takes an `https`
/// redirect, such as a web page's own address.
pub const PAGE_REDIRECT_MINOR: u32 = 11;

/// Whether a daemon whose handshake reported `version` accepts a web
/// page's own address as the OAuth redirect, so the page can offer sign-in.
pub fn daemon_accepts_page_redirects(version: Option<cookie_agent_protocol::SemVer>) -> bool {
    version.is_some_and(|version| {
        version
            .shared_minor(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
            .is_some_and(|minor| minor >= PAGE_REDIRECT_MINOR)
    })
}

pub fn is_global(server: &McpServerInfo) -> bool {
    server.runtime_id.as_str() == RuntimeId::GLOBAL
}

/// Why `server` cannot be edited, removed, or toggled here: only the
/// global runtime's servers are configured with `mcp.server.*`; a coding
/// runtime's come from its template, or from its own machine's
/// configuration.
pub fn edit_refusal(server: &McpServerInfo) -> Option<String> {
    if server.source == McpConfigSource::Machine {
        return Some(format!(
            "MCP server `{}` is defined on the machine of runtime `{}`; edit [runtimes.local.mcp] \
             in that machine's config.toml",
            server.name, server.runtime_id
        ));
    }
    (!is_global(server)).then(|| {
        format!(
            "MCP server `{}` belongs to runtime `{}`; edit its [runtimes.{}.mcp] template",
            server.name, server.runtime_id, server.runtime_id
        )
    })
}

/// The runtime hosting `server`, with the instance's workspace when it has
/// one, so two instances of one runtime read apart.
pub fn runtime_label(server: &McpServerInfo) -> String {
    match &server.workspace {
        Some(workspace) => format!("{} at {workspace}", server.runtime_id),
        None => server.runtime_id.to_string(),
    }
}

pub const fn source_label(source: McpConfigSource) -> &'static str {
    match source {
        McpConfigSource::UserFile => "user config",
        McpConfigSource::Machine => "runtime machine config",
        McpConfigSource::Runtime => "runtime only",
    }
}

/// How `server` is reached: its [definition](definition_display), or, for a
/// server its runtime's machine defines, where that definition lives.
pub fn server_definition_display(server: &McpServerInfo) -> String {
    if server.source == McpConfigSource::Machine {
        "defined in [runtimes.local.mcp] on the runtime's machine".to_owned()
    } else {
        definition_display(&server.definition)
    }
}

/// How a server is reached, one line per detail.
pub fn definition_display(definition: &McpServerDefinition) -> String {
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

pub const fn state_label(state: McpServerState) -> &'static str {
    match state {
        McpServerState::Connected => "connected",
        McpServerState::Connecting => "connecting",
        McpServerState::Reconnecting => "reconnecting",
        McpServerState::Failed => "failed",
        McpServerState::NeedsAuth => "needs_auth",
        McpServerState::Disabled => "disabled",
        McpServerState::Disconnected => "disconnected",
        McpServerState::Unknown => "unknown",
    }
}

/// The servers among `servers` that the instance of `runtime_id` at
/// `workspace` hosts, in name order. Instances of one runtime are told
/// apart by their workspace; a runtime's instance without one (as `global`
/// has) hosts the servers listed without one.
pub fn instance_servers(
    servers: &[McpServerInfo],
    runtime_id: &RuntimeId,
    workspace: Option<&str>,
) -> Vec<McpServerInfo> {
    let mut hosted = servers
        .iter()
        .filter(|server| {
            server.runtime_id == *runtime_id && server.workspace.as_deref() == workspace
        })
        .cloned()
        .collect::<Vec<_>>();
    hosted.sort_by(|left, right| left.name.cmp(&right.name));
    hosted
}

/// The servers among `servers` that any instance of `runtime_id` hosts,
/// grouped by workspace, then in name order.
pub fn runtime_servers(servers: &[McpServerInfo], runtime_id: &RuntimeId) -> Vec<McpServerInfo> {
    let mut hosted = servers
        .iter()
        .filter(|server| server.runtime_id == *runtime_id)
        .cloned()
        .collect::<Vec<_>>();
    hosted
        .sort_by(|left, right| (&left.workspace, &left.name).cmp(&(&right.workspace, &right.name)));
    hosted
}

/// `count` tools, in words.
pub fn tools_label(count: u32) -> String {
    match count {
        1 => "1 tool".to_owned(),
        count => format!("{count} tools"),
    }
}

/// How a server's state reads at a glance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateTone {
    /// Connected: its tools are offered.
    Ready,
    /// On its way, or waiting for a sign-in: no failure yet.
    Pending,
    /// Failed: its tools are missing.
    Failed,
    /// Disabled, disconnected, or a state this build does not know.
    Off,
}

pub const fn state_tone(state: McpServerState) -> StateTone {
    match state {
        McpServerState::Connected => StateTone::Ready,
        McpServerState::Connecting | McpServerState::Reconnecting | McpServerState::NeedsAuth => {
            StateTone::Pending
        }
        McpServerState::Failed => StateTone::Failed,
        McpServerState::Disabled | McpServerState::Disconnected | McpServerState::Unknown => {
            StateTone::Off
        }
    }
}

/// A server's line in a runtime panel: its name, state, and tool count, as
/// `docs  connected  3 tools`.
pub fn server_summary(server: &McpServerInfo) -> String {
    format!(
        "{}  {}  {}",
        server.name,
        state_label(server.state),
        tools_label(server.tool_count)
    )
}

/// What a runtime panel says under a server: why it failed, or what it
/// waits for, with whitespace runs folded so a long reason wraps as prose;
/// nothing for a server whose state needs no words.
pub fn server_message(server: &McpServerInfo) -> Option<String> {
    let message = server.message.as_deref()?;
    let readable = message
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!readable.is_empty()).then_some(readable)
}

/// `servers` counted by state in first-seen order, as `2 connected, 1 failed`.
fn state_counts(servers: &[McpServerInfo]) -> String {
    let mut counts = Vec::<(&'static str, usize)>::new();
    for server in servers {
        let label = state_label(server.state);
        match counts.iter_mut().find(|(known, _)| *known == label) {
            Some((_, count)) => *count += 1,
            None => counts.push((label, 1)),
        }
    }
    counts
        .iter()
        .map(|(label, count)| format!("{count} {label}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A count of `servers` by state, as `2 MCP servers: 1 connected, 1 failed`;
/// `None` when there are none.
pub fn servers_overview(servers: &[McpServerInfo]) -> Option<String> {
    if servers.is_empty() {
        return None;
    }
    let noun = if servers.len() == 1 {
        "MCP server"
    } else {
        "MCP servers"
    };
    Some(format!(
        "{} {noun}: {}",
        servers.len(),
        state_counts(servers)
    ))
}

/// The short form a list row shows, as `MCP 1 connected, 1 failed`; `None`
/// when there are no servers.
pub fn servers_badge(servers: &[McpServerInfo]) -> Option<String> {
    (!servers.is_empty()).then(|| format!("MCP {}", state_counts(servers)))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        McpFields, McpTransport, StateTone, daemon_accepts_page_redirects, edit_refusal,
        instance_servers, new_definition, runtime_servers, server_definition_display,
        server_message, server_summary, servers_badge, servers_overview, state_tone,
    };
    use cookie_agent_protocol::{
        McpConfigSource, McpServerDefinition, McpServerInfo, McpServerState, RuntimeId, SemVer,
    };

    fn server(
        runtime: &str,
        workspace: Option<&str>,
        name: &str,
        state: McpServerState,
        message: Option<&str>,
    ) -> McpServerInfo {
        McpServerInfo {
            runtime_id: RuntimeId::new(runtime).unwrap(),
            workspace: workspace.map(str::to_owned),
            name: name.into(),
            scope: None,
            source: McpConfigSource::UserFile,
            definition: McpServerDefinition {
                command: Some("server".into()),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
                url: None,
                headers: BTreeMap::new(),
                oauth: None,
                enabled: true,
                timeout_ms: None,
            },
            state,
            tool_count: 0,
            message: message.map(str::to_owned),
            auth_in_progress: None,
        }
    }

    fn names(servers: &[McpServerInfo]) -> Vec<(Option<&str>, &str)> {
        servers
            .iter()
            .map(|server| (server.workspace.as_deref(), server.name.as_str()))
            .collect()
    }

    #[test]
    fn an_instance_lists_the_servers_of_its_runtime_and_workspace_by_name() {
        let servers = [
            server(
                "labpc",
                Some("/srv/web"),
                "lsp",
                McpServerState::Connected,
                None,
            ),
            server(
                "labpc",
                Some("/srv/api"),
                "lsp",
                McpServerState::Failed,
                None,
            ),
            server(
                "labpc",
                Some("/srv/api"),
                "docs",
                McpServerState::Connected,
                None,
            ),
            server("global", None, "github", McpServerState::NeedsAuth, None),
        ];
        let labpc = RuntimeId::new("labpc").unwrap();
        assert_eq!(
            names(&instance_servers(&servers, &labpc, Some("/srv/api"))),
            [(Some("/srv/api"), "docs"), (Some("/srv/api"), "lsp")]
        );
        assert!(instance_servers(&servers, &labpc, None).is_empty());
        assert_eq!(
            names(&instance_servers(
                &servers,
                &RuntimeId::new("global").unwrap(),
                None
            )),
            [(None, "github")]
        );
        assert_eq!(
            names(&runtime_servers(&servers, &labpc)),
            [
                (Some("/srv/api"), "docs"),
                (Some("/srv/api"), "lsp"),
                (Some("/srv/web"), "lsp")
            ]
        );
    }

    #[test]
    fn a_server_reads_as_name_state_tools_and_a_readable_reason() {
        let mut failed = server(
            "local",
            Some("/srv/api"),
            "lsp",
            McpServerState::Failed,
            Some("  MCP server `lsp` disconnected\n\n   and did not   reconnect  "),
        );
        assert_eq!(server_summary(&failed), "lsp  failed  0 tools");
        assert_eq!(
            server_message(&failed).as_deref(),
            Some("MCP server `lsp` disconnected\nand did not reconnect")
        );
        failed.message = Some("   ".into());
        assert_eq!(server_message(&failed), None);
        let mut ready = server("global", None, "docs", McpServerState::Connected, None);
        ready.tool_count = 1;
        assert_eq!(server_summary(&ready), "docs  connected  1 tool");
        assert_eq!(state_tone(ready.state), StateTone::Ready);
        assert_eq!(state_tone(McpServerState::NeedsAuth), StateTone::Pending);
        assert_eq!(state_tone(McpServerState::Failed), StateTone::Failed);
        assert_eq!(state_tone(McpServerState::Disabled), StateTone::Off);
    }

    #[test]
    fn servers_are_counted_by_state() {
        assert_eq!(servers_overview(&[]), None);
        let servers = [
            server("local", None, "a", McpServerState::Connected, None),
            server("local", None, "b", McpServerState::Failed, None),
            server("local", None, "c", McpServerState::Connected, None),
        ];
        assert_eq!(
            servers_overview(&servers).as_deref(),
            Some("3 MCP servers: 2 connected, 1 failed")
        );
        assert_eq!(
            servers_overview(&servers[1..2]).as_deref(),
            Some("1 MCP server: 1 failed")
        );
        assert_eq!(
            servers_badge(&servers).as_deref(),
            Some("MCP 2 connected, 1 failed")
        );
        assert_eq!(servers_badge(&[]), None);
    }

    #[test]
    fn a_machine_server_says_where_it_is_defined() {
        let mut machine = server(
            "client_box",
            Some("/home/me/app"),
            "chrome",
            McpServerState::Connected,
            None,
        );
        machine.source = McpConfigSource::Machine;
        assert_eq!(
            server_definition_display(&machine),
            "defined in [runtimes.local.mcp] on the runtime's machine"
        );
        assert!(
            edit_refusal(&machine)
                .is_some_and(|refusal| refusal.contains("on the machine of runtime `client_box`"))
        );
    }

    #[test]
    fn only_a_1_11_daemon_takes_a_page_redirect() {
        assert!(!daemon_accepts_page_redirects(None));
        assert!(!daemon_accepts_page_redirects(Some(SemVer::new(1, 10, 0))));
        assert!(daemon_accepts_page_redirects(Some(SemVer::new(1, 11, 0))));
        assert!(daemon_accepts_page_redirects(Some(SemVer::new(1, 14, 2))));
        assert!(!daemon_accepts_page_redirects(Some(SemVer::new(2, 11, 0))));
    }

    #[test]
    fn a_form_makes_a_stdio_or_http_definition_or_says_what_is_wrong() {
        let mut fields = McpFields::add();
        assert_eq!(
            fields.definition(&new_definition()),
            Err("server name is required".into())
        );
        fields.name = " echo ".into();
        fields.endpoint = "python3".into();
        fields.extras = r#"["server.py"]"#.into();
        fields.environment = r#"{"MODE":"test"}"#.into();
        let (name, definition) = fields.definition(&new_definition()).expect("stdio");
        assert_eq!(name, "echo");
        assert_eq!(definition.command.as_deref(), Some("python3"));
        assert_eq!(definition.args, ["server.py"]);
        assert_eq!(definition.env.get("MODE").map(String::as_str), Some("test"));
        fields.set_transport(McpTransport::Http);
        assert_eq!(fields.extras, "{}");
        fields.endpoint = "https://example.com/mcp".into();
        let (_, definition) = fields.definition(&new_definition()).expect("http");
        assert_eq!(definition.command, None);
        assert!(definition.env.is_empty());
        fields.extras = "[".into();
        assert!(
            fields
                .definition(&new_definition())
                .unwrap_err()
                .starts_with("headers must be a JSON string object")
        );
    }
}
