//! The `/mcp` panel's rules every client shares: the servers in display
//! order, which ones `mcp.server.*` edits, a server's add or edit form as
//! text and the definition it makes, and the labels the list shows.

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

pub fn is_global(server: &McpServerInfo) -> bool {
    server.runtime_id.as_str() == RuntimeId::GLOBAL
}

/// Why `server` cannot be edited, removed, or toggled here: only the
/// global runtime's servers are configured with `mcp.server.*`; a coding
/// runtime's come from its template.
pub fn edit_refusal(server: &McpServerInfo) -> Option<String> {
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
        McpConfigSource::Runtime => "runtime only",
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

#[cfg(test)]
mod tests {
    use super::{McpFields, McpTransport, new_definition};

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
