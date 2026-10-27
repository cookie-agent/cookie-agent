//! `[runtimes.<id>]`: configured ways to start runtimes
//! (`docs/site/specs/runtimes.md` section 4.1), and the settings of the
//! runtimes the daemon does not start: the built-in `local` one and those
//! that dial in (`docs/site/specs/runtime-management.md` section 3.1).

use std::{collections::BTreeMap, time::Duration};

use cookie_agent_identity::{AgentId, RuntimeId};
use serde::Deserialize;

use crate::{ConfigError, McpConfig, McpOAuthConfig, McpServerConfig};

/// The argv element templates used to substitute with a workspace before
/// one runtime served every workspace; a command that still has it is
/// refused with an explanation.
const REMOVED_WORKSPACE_PLACEHOLDER: &str = "{workspace}";

const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 3_000;

/// A `[runtimes.<id>]` table as authored.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRuntimeTemplate {
    command: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    startup_timeout_ms: Option<u64>,
    shutdown_grace_ms: Option<u64>,
    mcp: Option<McpConfig>,
    acp: Option<RawAcpRuntime>,
}

/// `[runtimes.<id>.acp]` as authored.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAcpRuntime {
    #[serde(default)]
    agents: BTreeMap<String, RawAcpAgent>,
}

/// `[runtimes.<id>.acp.agents.<agent>]` or `[acp.agents.<agent>]` as
/// authored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAcpAgent {
    command: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    description: Option<String>,
    enabled: Option<bool>,
}

/// One ACP agent a runtime hosts (`docs/site/specs/acp-agents.md`
/// section 3).
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct AcpAgentConfig {
    /// The program and its arguments, run without a shell on the runtime's
    /// machine.
    pub command: Vec<String>,
    /// Variables merged over the runtime's environment.
    pub env: BTreeMap<String, String>,
    /// Shown where the agent is listed; empty when not authored.
    pub description: String,
    pub enabled: bool,
}

/// A validated runtime template. `[runtimes.local]`, which configures the
/// built-in local coding runtime, and a table without `command`, which holds
/// the settings of the runtime that dials in with its ID, carry only MCP
/// servers and ACP agents: their `command` and `env` are empty.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeTemplateConfig {
    /// The program and its arguments. One process serves every workspace
    /// and capability of the runtime.
    pub command: Vec<String>,
    /// Variables added to the daemon's environment for the child.
    pub env: BTreeMap<String, String>,
    /// Bound on starting the process and answering `runtime/initialize`.
    pub startup_timeout_ms: u64,
    /// How long a stopping runtime gets before it is killed.
    pub shutdown_grace_ms: u64,
    /// MCP servers the runtime hosts on its own machine.
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    /// ACP agents the runtime starts on its own machine, by their table
    /// key.
    pub acp_agents: BTreeMap<AgentId, AcpAgentConfig>,
}

impl RuntimeTemplateConfig {
    /// Whether the daemon starts the runtime from this template, rather
    /// than the table holding the settings of a runtime it does not start.
    #[must_use]
    pub const fn daemon_starts(&self) -> bool {
        !self.command.is_empty()
    }

    #[must_use]
    pub const fn startup_timeout(&self) -> Duration {
        Duration::from_millis(self.startup_timeout_ms)
    }

    #[must_use]
    pub const fn shutdown_grace(&self) -> Duration {
        Duration::from_millis(self.shutdown_grace_ms)
    }
}

fn invalid(runtime: &str, reason: impl Into<String>) -> ConfigError {
    ConfigError::RuntimeTemplate {
        runtime: runtime.to_owned(),
        reason: reason.into(),
    }
}

/// Validates the authored `[runtimes]` tables.
pub(crate) fn validate_runtime_templates(
    raw: BTreeMap<String, RawRuntimeTemplate>,
) -> Result<BTreeMap<RuntimeId, RuntimeTemplateConfig>, ConfigError> {
    raw.into_iter()
        .map(|(id, template)| {
            let runtime_id = RuntimeId::new(id.as_str()).map_err(|_| {
                invalid(
                    &id,
                    "runtime IDs match [a-z][a-z0-9_]{0,31}, because they become tool name prefixes",
                )
            })?;
            let template = validate_template(&id, template)?;
            Ok((runtime_id, template))
        })
        .collect()
}

fn validate_template(
    id: &str,
    template: RawRuntimeTemplate,
) -> Result<RuntimeTemplateConfig, ConfigError> {
    if id == RuntimeId::GLOBAL {
        return Err(invalid(
            id,
            "`global` is the daemon's global runtime; configure its MCP servers under [mcp]",
        ));
    }
    let mcp_servers = template.mcp.map(|mcp| mcp.servers).unwrap_or_default();
    for (name, server) in &mcp_servers {
        server
            .validate(name)
            .map_err(|error| invalid(id, error.to_string()))?;
        if matches!(
            server.oauth,
            McpOAuthConfig::Enabled | McpOAuthConfig::Settings(_)
        ) {
            return Err(invalid(
                id,
                format!(
                    "MCP server `{name}` enables OAuth, which only top-level [mcp] servers support"
                ),
            ));
        }
    }
    let acp_agents = validate_acp_agents(id, template.acp)?;
    let launch_fields = template.env.is_some()
        || template.startup_timeout_ms.is_some()
        || template.shutdown_grace_ms.is_some();
    if id == RuntimeId::LOCAL && (template.command.is_some() || launch_fields) {
        return Err(invalid(
            id,
            "the built-in `local` runtime accepts only `mcp` and `acp` tables",
        ));
    }
    if id != RuntimeId::LOCAL && template.command.is_none() && launch_fields {
        return Err(invalid(
            id,
            "a table without `command` holds the settings of the runtime that dials in as \
             this ID and accepts only `mcp` and `acp` tables; give it a `command` for the \
             daemon to start it",
        ));
    }
    if template.command.is_none() {
        return Ok(RuntimeTemplateConfig {
            command: Vec::new(),
            env: BTreeMap::new(),
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            mcp_servers,
            acp_agents,
        });
    }
    let command = validate_command(id, "", template.command)?;
    if command
        .iter()
        .any(|argument| argument.contains(REMOVED_WORKSPACE_PLACEHOLDER))
    {
        return Err(invalid(
            id,
            "`{workspace}` is no longer substituted: one runtime process serves every \
             workspace, which sessions name when they attach",
        ));
    }
    let env = validate_env(id, "", template.env)?;
    let startup_timeout_ms = template
        .startup_timeout_ms
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT_MS);
    if startup_timeout_ms == 0 {
        return Err(invalid(id, "`startup_timeout_ms` must be positive"));
    }
    let shutdown_grace_ms = template
        .shutdown_grace_ms
        .unwrap_or(DEFAULT_SHUTDOWN_GRACE_MS);
    if shutdown_grace_ms == 0 {
        return Err(invalid(id, "`shutdown_grace_ms` must be positive"));
    }
    Ok(RuntimeTemplateConfig {
        command,
        env,
        startup_timeout_ms,
        shutdown_grace_ms,
        mcp_servers,
        acp_agents,
    })
}

/// The longest description an ACP agent may carry, as for agent documents.
const MAX_ACP_DESCRIPTION: usize = 512;

fn validate_command(
    id: &str,
    what: &str,
    command: Option<Vec<String>>,
) -> Result<Vec<String>, ConfigError> {
    let command = command
        .filter(|command| !command.is_empty())
        .ok_or_else(|| invalid(id, format!("{what}`command` must be a nonempty array")))?;
    if command[0].is_empty() {
        return Err(invalid(
            id,
            format!("{what}the first element of `command` must name a program"),
        ));
    }
    if command.iter().any(|argument| argument.contains('\0')) {
        return Err(invalid(
            id,
            format!("{what}`command` elements must not contain NUL"),
        ));
    }
    Ok(command)
}

fn validate_env(
    id: &str,
    what: &str,
    env: Option<BTreeMap<String, String>>,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let env = env.unwrap_or_default();
    if let Some(name) = env.iter().find_map(|(name, value)| {
        (name.is_empty() || name.contains(['=', '\0']) || value.contains('\0')).then_some(name)
    }) {
        return Err(invalid(
            id,
            format!(
                "{what}environment variable `{name}` is invalid: names must be nonempty without `=` or NUL, and values without NUL"
            ),
        ));
    }
    Ok(env)
}

/// Checks `[acp.agents]`, the ACP agents every runtime offers, as a
/// runtime's own declarations are checked.
pub(crate) fn validate_shared_acp_agents(
    agents: BTreeMap<String, RawAcpAgent>,
) -> Result<BTreeMap<AgentId, AcpAgentConfig>, ConfigError> {
    validate_acp_agents("acp", Some(RawAcpRuntime { agents })).map_err(|error| match error {
        ConfigError::RuntimeTemplate { reason, .. } => ConfigError::SharedAcpAgent(reason),
        other => other,
    })
}

fn validate_acp_agents(
    id: &str,
    acp: Option<RawAcpRuntime>,
) -> Result<BTreeMap<AgentId, AcpAgentConfig>, ConfigError> {
    let Some(acp) = acp else {
        return Ok(BTreeMap::new());
    };
    acp.agents
        .into_iter()
        .map(|(key, agent)| {
            let agent_id = AgentId::new(key.as_str()).map_err(|_| {
                invalid(
                    id,
                    format!(
                        "ACP agent `{key}`: agent IDs are lowercase letters, digits, and single \
                         inner hyphens, at most 64 bytes"
                    ),
                )
            })?;
            let what = format!("ACP agent `{key}`: ");
            let command = validate_command(id, &what, agent.command)?;
            let env = validate_env(id, &what, agent.env)?;
            let description = agent.description.unwrap_or_default();
            if description.len() > MAX_ACP_DESCRIPTION
                || description.chars().any(char::is_control)
            {
                return Err(invalid(
                    id,
                    format!(
                        "{what}`description` must be at most {MAX_ACP_DESCRIPTION} bytes without control characters"
                    ),
                ));
            }
            Ok((
                agent_id,
                AcpAgentConfig {
                    command,
                    env,
                    description,
                    enabled: agent.enabled.unwrap_or(true),
                },
            ))
        })
        .collect()
}
