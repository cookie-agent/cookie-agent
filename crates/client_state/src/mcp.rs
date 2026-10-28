//! The `/mcp` panel's rules every client shares: the servers in display
//! order, which ones `mcp.server.*` edits, a server's add or edit form as
//! text and the definition it makes, and the labels the list shows; and the
//! servers each runtime instance hosts, as the runtime panels list them.

use std::collections::BTreeMap;

use cookie_agent_protocol::{
    McpConfigSource, McpConfigTarget, McpServerDefinition, McpServerInfo, McpServerScope,
    McpServerState, RuntimeId,
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
    /// The `include_tools` patterns, comma-separated; empty publishes every
    /// tool.
    pub include_tools: String,
    /// The `exclude_tools` patterns, comma-separated.
    pub exclude_tools: String,
    /// `always_load`: every tool up front on a model that defers tools.
    pub always_load: bool,
    /// The `always_load_tools` patterns, comma-separated.
    pub always_load_tools: String,
    /// `instructions`: the server's instructions are its summary.
    pub instructions: bool,
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
            include_tools: String::new(),
            exclude_tools: String::new(),
            always_load: false,
            always_load_tools: String::new(),
            instructions: true,
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
            include_tools: server
                .definition
                .include_tools
                .as_deref()
                .unwrap_or_default()
                .join(", "),
            exclude_tools: server.definition.exclude_tools.join(", "),
            always_load: server.definition.always_load,
            always_load_tools: server.definition.always_load_tools.join(", "),
            instructions: server.definition.instructions,
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
        let include = tool_patterns(&self.include_tools);
        definition.include_tools = (!include.is_empty()).then_some(include);
        definition.exclude_tools = tool_patterns(&self.exclude_tools);
        definition.always_load = self.always_load;
        definition.always_load_tools = tool_patterns(&self.always_load_tools);
        definition.instructions = self.instructions;
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

/// The tool patterns a comma-separated filter field lists, blanks dropped.
fn tool_patterns(field: &str) -> Vec<String> {
    field
        .split(',')
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_owned)
        .collect()
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
        include_tools: None,
        exclude_tools: Vec::new(),
        scope: None,
        always_load: false,
        always_load_tools: Vec::new(),
        instructions: true,
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

/// Why `server` cannot be edited, removed, or toggled here: only one a
/// dial-in runtime older than runtime protocol 1.10.0 defines on its own
/// machine, which the daemon does not read. Every other server is in the
/// daemon's `config.toml`, `global`'s under `[mcp]` and a runtime's under
/// `[runtimes.<id>.mcp]` (client protocol 1.37.0).
pub fn edit_refusal(server: &McpServerInfo) -> Option<String> {
    (server.source == McpConfigSource::Machine).then(|| {
        format!(
            "MCP server `{}` is defined on the machine of runtime `{}`, which runs an older \
             cookie; move it to [runtimes.{}.mcp] in the daemon's config.toml",
            server.name, server.runtime_id, server.runtime_id
        )
    })
}

/// The runtime an `mcp.server.*` mutation of `server` names: `None` for
/// `global`'s top-level servers.
pub fn mutation_runtime(server: &McpServerInfo) -> Option<RuntimeId> {
    (!is_global(server)).then(|| server.runtime_id.clone())
}

/// Whether `definition`'s `include_tools` and `exclude_tools` publish
/// `tool`, as the runtime decides.
pub fn publishes_tool(definition: &McpServerDefinition, tool: &str) -> bool {
    let matches = |pattern: &String| wildcard_match(pattern, tool);
    definition
        .include_tools
        .as_ref()
        .is_none_or(|include| include.iter().any(matches))
        && !definition.exclude_tools.iter().any(matches)
}

/// `definition` with `tool`, by the server's own name, left out (`leave_out`)
/// or taken back (`docs/site/specs/runtime-management.md` section 4.1). A
/// tool is left out by adding its exact name to `exclude_tools`, and taken
/// back by removing that name, then, when `include_tools` still leaves it
/// out, adding the exact name there. Patterns are never rewritten: a tool an
/// `exclude_tools` pattern leaves out is refused with the pattern.
pub fn toggle_tool(
    definition: &McpServerDefinition,
    tool: &str,
    leave_out: bool,
) -> Result<McpServerDefinition, String> {
    let mut toggled = definition.clone();
    if leave_out {
        if !publishes_tool(definition, tool) {
            return Err(format!("`{tool}` is left out already"));
        }
        toggled.exclude_tools.push(tool.to_owned());
        return Ok(toggled);
    }
    toggled.exclude_tools.retain(|pattern| pattern != tool);
    if let Some(pattern) = toggled
        .exclude_tools
        .iter()
        .find(|pattern| wildcard_match(pattern, tool))
    {
        return Err(format!(
            "`{tool}` is left out by the pattern `{pattern}` in exclude_tools; edit the server to \
             change it"
        ));
    }
    if let Some(include) = toggled.include_tools.as_mut()
        && !include.iter().any(|pattern| wildcard_match(pattern, tool))
    {
        include.push(tool.to_owned());
    }
    Ok(toggled)
}

/// `*` and `?` over a server's tool names, as `include_tools` and
/// `exclude_tools` match them; a trailing ` *` also matches the bare
/// prefix.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    fn matches(pattern: &[char], name: &[char]) -> bool {
        let (mut p, mut n, mut star, mut retry) = (0, 0, None, 0);
        while n < name.len() {
            if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
                p += 1;
                n += 1;
            } else if p < pattern.len() && pattern[p] == '*' {
                star = Some(p);
                p += 1;
                retry = n;
            } else if let Some(index) = star {
                p = index + 1;
                retry += 1;
                n = retry;
            } else {
                return false;
            }
        }
        pattern[p..].iter().all(|c| *c == '*')
    }
    let name = name.chars().collect::<Vec<_>>();
    matches(&pattern.chars().collect::<Vec<_>>(), &name)
        || pattern
            .strip_suffix(" *")
            .is_some_and(|prefix| matches(&prefix.chars().collect::<Vec<_>>(), &name))
}

/// The runtime hosting `server`, with the instance's workspace when it has
/// one, so two instances of one runtime read apart.
pub fn runtime_label(server: &McpServerInfo) -> String {
    match (&server.workspace, server.scope) {
        (Some(workspace), _) => format!("{} at {workspace}", server.runtime_id),
        (None, Some(McpServerScope::Runtime)) => format!("{}, runtime-wide", server.runtime_id),
        (None, Some(McpServerScope::Workspace)) => {
            format!("{}, in each workspace", server.runtime_id)
        }
        (None, None) => server.runtime_id.to_string(),
    }
}

/// Whether `server` is a runtime's workspace-scoped server that no
/// instance hosts yet: listed without a workspace, it starts when a
/// workspace of its runtime opens.
pub fn waits_for_workspace(server: &McpServerInfo) -> bool {
    server.scope == Some(McpServerScope::Workspace)
        && server.workspace.is_none()
        && server.state != McpServerState::Disabled
}

/// The state a runtime panel shows for `server`: its state, or for one
/// that [waits for a workspace](waits_for_workspace), that it starts with
/// one.
pub fn server_state_label(server: &McpServerInfo) -> &'static str {
    if waits_for_workspace(server) {
        "starts with a workspace"
    } else {
        state_label(server.state)
    }
}

/// How `server`'s state reads at a glance; one that waits for a workspace
/// is off, not failed.
pub fn server_tone(server: &McpServerInfo) -> StateTone {
    if waits_for_workspace(server) {
        StateTone::Off
    } else {
        state_tone(server.state)
    }
}

/// What a runtime panel says about where `server` runs, when it is one
/// process its runtime shares across its workspaces.
pub fn scope_note(server: &McpServerInfo) -> Option<&'static str> {
    (server.scope == Some(McpServerScope::Runtime)).then_some("runtime-wide")
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

/// How a server is reached, one line per detail, then its tool filters.
pub fn definition_display(definition: &McpServerDefinition) -> String {
    let mut value = transport_display(definition);
    if let Some(include) = &definition.include_tools {
        value.push_str(&format!("\ninclude tools: {}", include.join(", ")));
    }
    if !definition.exclude_tools.is_empty() {
        value.push_str(&format!(
            "\nexclude tools: {}",
            definition.exclude_tools.join(", ")
        ));
    }
    value
}

fn transport_display(definition: &McpServerDefinition) -> String {
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
/// `workspace` uses, in name order. Instances of one runtime are told
/// apart by their workspace; a runtime's instance without one (as `global`
/// has) hosts the servers listed without one. An instance with a workspace
/// also uses its runtime's runtime-scoped servers, which every workspace of
/// the runtime shares.
pub fn instance_servers(
    servers: &[McpServerInfo],
    runtime_id: &RuntimeId,
    workspace: Option<&str>,
) -> Vec<McpServerInfo> {
    let mut hosted = servers
        .iter()
        .filter(|server| {
            server.runtime_id == *runtime_id
                && (server.workspace.as_deref() == workspace
                    || workspace.is_some() && server.scope == Some(McpServerScope::Runtime))
        })
        .cloned()
        .collect::<Vec<_>>();
    hosted.sort_by(|left, right| left.name.cmp(&right.name));
    hosted
}

/// The servers among `servers` that `runtime_id` hosts or is configured
/// with, grouped by workspace (those without one, runtime-scoped or waiting
/// for a workspace, first), then in name order.
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

/// `server`'s published tools in words, with how many its filters left
/// out when they left out any, as `12 tools, 30 filtered`.
pub fn server_tools_label(server: &McpServerInfo) -> String {
    let tools = tools_label(server.tool_count);
    match server.filtered_tools {
        Some(filtered) if filtered > 0 => format!("{tools}, {filtered} filtered"),
        _ => tools,
    }
}

/// `server`'s tools by the server's own names, in its order: those it
/// offers, then those its filters left out. Both empty while it has listed
/// nothing, and for a runtime too old to say.
pub fn server_tool_names(server: &McpServerInfo) -> (Vec<&str>, Vec<&str>) {
    let (mut offered, mut left_out) = (Vec::new(), Vec::new());
    for tool in &server.server_tools {
        if tool.filtered {
            left_out.push(tool.name.as_str());
        } else {
            offered.push(tool.name.as_str());
        }
    }
    (offered, left_out)
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
/// `docs  connected  3 tools`, followed by `runtime-wide` for one its
/// runtime shares across workspaces; for one that waits for a workspace,
/// `lsp  starts with a workspace`.
pub fn server_summary(server: &McpServerInfo) -> String {
    if waits_for_workspace(server) {
        return format!("{}  {}", server.name, server_state_label(server));
    }
    let mut summary = format!(
        "{}  {}  {}",
        server.name,
        state_label(server.state),
        server_tools_label(server)
    );
    if let Some(note) = scope_note(server) {
        summary.push_str("  ");
        summary.push_str(note);
    }
    summary
}

/// What a runtime panel says under a server: why it failed, or what it
/// waits for, with whitespace runs folded so a long reason wraps as prose;
/// nothing for a server whose state needs no words.
pub fn server_message(server: &McpServerInfo) -> Option<String> {
    if waits_for_workspace(server) {
        // Its state says it.
        return None;
    }
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
        let label = if waits_for_workspace(server) {
            "waiting for a workspace"
        } else {
            state_label(server.state)
        };
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
        instance_servers, new_definition, runtime_label, runtime_servers,
        server_definition_display, server_message, server_summary, server_tone, servers_badge,
        servers_overview, state_tone, waits_for_workspace,
    };
    use cookie_agent_protocol::{
        McpConfigSource, McpServerDefinition, McpServerInfo, McpServerScope, McpServerState,
        RuntimeId, SemVer,
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
                include_tools: None,
                exclude_tools: Vec::new(),
                scope: None,
                always_load: false,
                always_load_tools: Vec::new(),
                instructions: true,
            },
            state,
            tool_count: 0,
            filtered_tools: None,
            server_tools: Vec::new(),
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
    fn a_runtime_wide_server_is_shared_and_a_waiting_one_starts_with_a_workspace() {
        let mut browser = server("labpc", None, "browser", McpServerState::Connected, None);
        browser.scope = Some(McpServerScope::Runtime);
        browser.tool_count = 12;
        let mut lsp = server(
            "labpc",
            None,
            "lsp",
            McpServerState::Disconnected,
            Some("starts when a workspace opens"),
        );
        lsp.scope = Some(McpServerScope::Workspace);
        let mut hosted = server(
            "labpc",
            Some("/srv/api"),
            "lsp",
            McpServerState::Connected,
            None,
        );
        hosted.scope = Some(McpServerScope::Workspace);
        let mut disabled = lsp.clone();
        disabled.state = McpServerState::Disabled;

        assert_eq!(
            server_summary(&browser),
            "browser  connected  12 tools  runtime-wide"
        );
        assert_eq!(server_summary(&lsp), "lsp  starts with a workspace");
        assert_eq!(server_message(&lsp), None);
        assert_eq!(server_tone(&lsp), StateTone::Off);
        assert!(waits_for_workspace(&lsp));
        assert!(!waits_for_workspace(&hosted));
        assert!(
            !waits_for_workspace(&disabled),
            "a disabled one never starts"
        );
        assert_eq!(server_summary(&disabled), "lsp  disabled  0 tools");
        assert_eq!(runtime_label(&browser), "labpc, runtime-wide");
        assert_eq!(runtime_label(&lsp), "labpc, in each workspace");
        assert_eq!(runtime_label(&hosted), "labpc at /srv/api");

        let servers = [browser.clone(), lsp.clone(), hosted.clone()];
        assert_eq!(
            servers_overview(&servers).as_deref(),
            Some("3 MCP servers: 2 connected, 1 waiting for a workspace")
        );
        // A workspace's instance uses the runtime-wide server too; one
        // waiting for a workspace is the runtime's, not an instance's.
        let labpc = RuntimeId::new("labpc").unwrap();
        assert_eq!(
            names(&instance_servers(&servers, &labpc, Some("/srv/api"))),
            [(None, "browser"), (Some("/srv/api"), "lsp")]
        );
        assert_eq!(
            names(&runtime_servers(&servers, &labpc)),
            [(None, "browser"), (None, "lsp"), (Some("/srv/api"), "lsp")]
        );
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
        assert!(edit_refusal(&machine).is_some_and(|refusal| {
            refusal.contains("on the machine of runtime `client_box`")
                && refusal.contains("[runtimes.client_box.mcp]")
        }));
    }

    #[test]
    fn a_runtimes_server_is_edited_in_its_table_and_names_its_runtime() {
        let hosted = server("labpc", None, "browser", McpServerState::Connected, None);
        assert_eq!(edit_refusal(&hosted), None);
        assert_eq!(
            super::mutation_runtime(&hosted),
            Some(RuntimeId::new("labpc").unwrap())
        );
        let global = server("global", None, "docs", McpServerState::Connected, None);
        assert_eq!(super::mutation_runtime(&global), None);
    }

    #[test]
    fn a_tool_is_left_out_and_taken_back_by_its_exact_name() {
        use super::{publishes_tool, toggle_tool};
        let base = server("labpc", None, "browser", McpServerState::Connected, None).definition;
        let left_out = toggle_tool(&base, "browser_install", true).expect("left out");
        assert_eq!(left_out.exclude_tools, ["browser_install"]);
        assert!(!publishes_tool(&left_out, "browser_install"));
        assert!(toggle_tool(&left_out, "browser_install", true).is_err());
        let back = toggle_tool(&left_out, "browser_install", false).expect("taken back");
        assert!(back.exclude_tools.is_empty());
        assert!(publishes_tool(&back, "browser_install"));

        // A tool `include_tools` does not name is taken back by naming it.
        let mut included = base.clone();
        included.include_tools = Some(vec!["browser_c*".into()]);
        assert!(publishes_tool(&included, "browser_click"));
        assert!(!publishes_tool(&included, "browser_type"));
        let back = toggle_tool(&included, "browser_type", false).expect("named");
        assert_eq!(
            back.include_tools.as_deref(),
            Some(&["browser_c*".to_owned(), "browser_type".to_owned()][..])
        );
        // A pattern that leaves it out is the user's to change.
        let mut patterned = base;
        patterned.exclude_tools = vec!["browser_tab*".into()];
        let refused = toggle_tool(&patterned, "browser_tab_close", false).expect_err("pattern");
        assert!(refused.contains("`browser_tab*`"), "{refused}");
        assert_eq!(patterned.exclude_tools, ["browser_tab*"]);
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

    #[test]
    fn a_form_edits_what_is_sent_up_front_and_the_instructions_toggle() {
        let mut fields = McpFields::add();
        fields.name = "my_chrome".into();
        fields.endpoint = "chrome-mcp".into();
        fields.always_load_tools = " tab_*, ".into();
        fields.instructions = false;
        let (_, definition) = fields.definition(&new_definition()).expect("always load");
        assert_eq!(definition.always_load_tools, ["tab_*"]);
        assert!(!definition.always_load);
        assert!(!definition.instructions);
        fields.always_load = true;
        let (_, definition) = fields.definition(&new_definition()).expect("always load");
        assert!(definition.always_load);
    }

    #[test]
    fn a_form_edits_the_tool_filters_as_comma_separated_patterns() {
        let mut fields = McpFields::add();
        fields.name = "my_chrome".into();
        fields.endpoint = "chrome-mcp".into();
        fields.include_tools = " browser_*, ,tab_? ".into();
        fields.exclude_tools = "browser_install".into();
        let (_, definition) = fields.definition(&new_definition()).expect("filters");
        assert_eq!(
            definition.include_tools.as_deref(),
            Some(&["browser_*".to_owned(), "tab_?".to_owned()][..])
        );
        assert_eq!(definition.exclude_tools, ["browser_install"]);
        assert!(
            super::definition_display(&definition)
                .ends_with("\ninclude tools: browser_*, tab_?\nexclude tools: browser_install")
        );

        let mut listed = server("global", None, "my_chrome", McpServerState::Connected, None);
        listed.definition = definition;
        let edited = McpFields::edit(&listed);
        assert_eq!(edited.include_tools, "browser_*, tab_?");
        assert_eq!(edited.exclude_tools, "browser_install");

        // A blank include field publishes every tool rather than none.
        fields.include_tools = " , ".into();
        fields.exclude_tools.clear();
        let (_, definition) = fields.definition(&listed.definition).expect("no filters");
        assert_eq!(definition.include_tools, None);
        assert!(definition.exclude_tools.is_empty());
    }

    #[test]
    fn a_server_with_filtered_tools_says_how_many() {
        let mut browser = server("global", None, "browser", McpServerState::Connected, None);
        browser.tool_count = 12;
        browser.filtered_tools = Some(30);
        assert_eq!(super::server_tools_label(&browser), "12 tools, 30 filtered");
        assert_eq!(
            server_summary(&browser),
            "browser  connected  12 tools, 30 filtered"
        );
        browser.filtered_tools = Some(0);
        assert_eq!(super::server_tools_label(&browser), "12 tools");
    }

    #[test]
    fn a_servers_tools_split_into_offered_and_left_out_in_its_order() {
        let mut browser = server("labpc", None, "browser", McpServerState::Connected, None);
        assert_eq!(super::server_tool_names(&browser), (Vec::new(), Vec::new()));
        let tool = |name: &str, filtered| cookie_agent_protocol::McpServerTool {
            name: name.into(),
            filtered,
            always_load: false,
        };
        browser.server_tools = vec![
            tool("click", false),
            tool("install", true),
            tool("type", false),
            tool("tab_close", true),
        ];
        assert_eq!(
            super::server_tool_names(&browser),
            (vec!["click", "type"], vec!["install", "tab_close"])
        );
    }
}
