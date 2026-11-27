//! `[runtimes.<id>]`: configured ways to start runtimes
//! (`docs/site/specs/runtimes.md` section 4.1), and the settings of the
//! runtimes the daemon does not start: the built-in `local` one and those
//! that dial in (`docs/site/specs/runtime-management.md` section 3.1).

use std::{collections::BTreeMap, time::Duration};

use cookie_agent_identity::{AgentId, RuntimeId};
use serde::Deserialize;

use crate::{ConfigError, McpConfig, McpOAuthConfig, McpServerConfig, ShellEnvironmentOverride};

/// The argv element templates used to substitute with a workspace before
/// one runtime served every workspace; a command that still has it is
/// refused with an explanation.
const REMOVED_WORKSPACE_PLACEHOLDER: &str = "{workspace}";

const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 3_000;

/// The argument text a `scope = "session"` template's command, `env`
/// values, and hooks have replaced with the ID of the root session that owns
/// the instance (`docs/site/specs/temporary-runtimes.md` section 2).
pub const SESSION_ID_PLACEHOLDER: &str = "{session_id}";

/// The variable a template's process and hooks find the owning root
/// session's ID in, when the template is per session.
pub const SESSION_ID_ENV: &str = "COOKIE_SESSION_ID";

/// How long an idle per-session instance runs when the template does not
/// say.
const DEFAULT_SESSION_IDLE_STOP: Duration = Duration::from_secs(30 * 60);

/// Who shares a template's process (`docs/site/specs/temporary-runtimes.md`
/// section 2).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeScope {
    /// One process serves every session.
    #[default]
    Shared,
    /// One process per session tree, owned by its root session.
    Session,
}

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
    shell_environment: Option<ShellEnvironmentOverride>,
    scope: Option<RuntimeScope>,
    idle_stop_after: Option<crate::runtime::McpIdleStop>,
    stop_command: Option<Vec<String>>,
    delete_command: Option<Vec<String>>,
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
    cache_ttl: Option<String>,
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
    /// How long the agent's prompt cache lives, as authored (`"1h"`;
    /// `"0s"` never expires): set, cookie compacts its idle sessions before
    /// expiry (`docs/site/specs/cache-aware-idle.md` section 4.4).
    pub cache_ttl: Option<String>,
}

impl AcpAgentConfig {
    /// [`Self::cache_ttl`] as a duration; zero is a cache that never
    /// expires.
    #[must_use]
    pub fn cache_ttl_duration(&self) -> Option<std::time::Duration> {
        self.cache_ttl
            .as_deref()
            .and_then(crate::runtime::parse_cache_ttl)
    }
}

/// A validated runtime template. `[runtimes.local]`, which configures the
/// built-in local coding runtime, and a table without `command`, which holds
/// the settings of the runtime that dials in with its ID, carry only MCP
/// servers, ACP agents, and the shell environment: their `command` and `env`
/// are empty.
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
    /// `[runtimes.<id>.shell_environment]`: the keys that replace
    /// `[shell_environment]`'s for this runtime.
    pub shell_environment: ShellEnvironmentOverride,
    /// Whether one process serves every session or each session tree gets
    /// its own.
    pub scope: RuntimeScope,
    /// How long an idle process runs before the daemon stops it; `None`
    /// keeps it until the daemon exits or a client disconnects it.
    pub idle_stop_after: Option<Duration>,
    /// Run on the daemon's machine after every stop of the process; empty
    /// when not authored.
    pub stop_command: Vec<String>,
    /// Run on the daemon's machine once the owning root session is
    /// deleted; empty when not authored, and always for a shared template.
    pub delete_command: Vec<String>,
}

/// What a template starts and runs for one owner: its commands with
/// [`SESSION_ID_PLACEHOLDER`] replaced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeLaunch {
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub stop_command: Vec<String>,
    pub delete_command: Vec<String>,
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

    /// Whether the template's process is meant to come and go: per session,
    /// stopped when idle, or cleaned up after by a `stop_command`. The
    /// daemon places such a runtime on a machine of its own rather than by
    /// the machine ID each new process reports.
    #[must_use]
    pub const fn is_temporary(&self) -> bool {
        matches!(self.scope, RuntimeScope::Session)
            || self.idle_stop_after.is_some()
            || !self.stop_command.is_empty()
    }

    /// The commands and environment for the instance `session` owns, the
    /// root session's ID, or for the one shared instance when `None`.
    #[must_use]
    pub fn launch(&self, session: Option<&str>) -> RuntimeLaunch {
        let substitute = |values: &[String]| -> Vec<String> {
            values
                .iter()
                .map(|value| match session {
                    Some(session) => value.replace(SESSION_ID_PLACEHOLDER, session),
                    None => value.clone(),
                })
                .collect()
        };
        let mut env = self
            .env
            .iter()
            .map(|(name, value)| {
                let value = match session {
                    Some(session) => value.replace(SESSION_ID_PLACEHOLDER, session),
                    None => value.clone(),
                };
                (name.clone(), value)
            })
            .collect::<BTreeMap<_, _>>();
        if let Some(session) = session {
            env.insert(SESSION_ID_ENV.to_owned(), session.to_owned());
        }
        RuntimeLaunch {
            command: substitute(&self.command),
            env,
            stop_command: substitute(&self.stop_command),
            delete_command: substitute(&self.delete_command),
        }
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
    let shell_environment = template.shell_environment.unwrap_or_default();
    let lifecycle_fields = template.scope.is_some()
        || template.idle_stop_after.is_some()
        || template.stop_command.is_some()
        || template.delete_command.is_some();
    let launch_fields = template.env.is_some()
        || template.startup_timeout_ms.is_some()
        || template.shutdown_grace_ms.is_some()
        || lifecycle_fields;
    if id == RuntimeId::LOCAL && (template.command.is_some() || launch_fields) {
        return Err(invalid(
            id,
            "the built-in `local` runtime accepts only `mcp`, `acp`, and `shell_environment` tables",
        ));
    }
    if id != RuntimeId::LOCAL && template.command.is_none() && launch_fields {
        return Err(invalid(
            id,
            "a table without `command` holds the settings of the runtime that dials in as \
             this ID and accepts only `mcp`, `acp`, and `shell_environment` tables; give it \
             a `command` for the daemon to start it (only runtimes the daemon starts can be \
             per session or stop when idle)",
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
            shell_environment,
            scope: RuntimeScope::Shared,
            idle_stop_after: None,
            stop_command: Vec::new(),
            delete_command: Vec::new(),
        });
    }
    let command = validate_command(id, "", template.command)?;
    let stop_command = validate_hook(id, "stop_command", template.stop_command)?;
    let delete_command = validate_hook(id, "delete_command", template.delete_command)?;
    let scope = template.scope.unwrap_or_default();
    if scope == RuntimeScope::Shared && !delete_command.is_empty() {
        return Err(invalid(
            id,
            "`delete_command` runs when the session that owns an instance is deleted, so it \
             needs `scope = \"session\"`",
        ));
    }
    if command
        .iter()
        .chain(&stop_command)
        .chain(&delete_command)
        .any(|argument| argument.contains(REMOVED_WORKSPACE_PLACEHOLDER))
    {
        return Err(invalid(
            id,
            "`{workspace}` is no longer substituted: one runtime process serves every \
             workspace, which sessions name when they attach",
        ));
    }
    let env = validate_env(id, "", template.env)?;
    if scope == RuntimeScope::Shared
        && command
            .iter()
            .chain(&stop_command)
            .chain(env.values())
            .any(|argument| argument.contains(SESSION_ID_PLACEHOLDER))
    {
        return Err(invalid(
            id,
            "`{session_id}` names the session that owns an instance, so it needs \
             `scope = \"session\"`; a shared runtime serves every session",
        ));
    }
    let idle_stop_after = match template.idle_stop_after {
        Some(crate::runtime::McpIdleStop(after)) => after,
        None => match scope {
            RuntimeScope::Shared => None,
            RuntimeScope::Session => Some(DEFAULT_SESSION_IDLE_STOP),
        },
    };
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
        shell_environment,
        scope,
        idle_stop_after,
        stop_command,
        delete_command,
    })
}

/// A hook command as authored: absent is none, and one that is given must
/// name a program.
fn validate_hook(
    id: &str,
    key: &str,
    command: Option<Vec<String>>,
) -> Result<Vec<String>, ConfigError> {
    match command {
        None => Ok(Vec::new()),
        Some(command) if command.first().is_none_or(String::is_empty) => Err(invalid(
            id,
            format!("`{key}` must be a nonempty array whose first element names a program"),
        )),
        Some(command) if command.iter().any(|argument| argument.contains('\0')) => Err(invalid(
            id,
            format!("`{key}` elements must not contain NUL"),
        )),
        Some(command) => Ok(command),
    }
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
            if let Some(ttl) = &agent.cache_ttl
                && crate::runtime::parse_cache_ttl(ttl).is_none()
            {
                return Err(invalid(
                    id,
                    format!(
                        "{what}`cache_ttl` must be an integer followed by ms, s, m, h, or d"
                    ),
                ));
            }
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
                    cache_ttl: agent.cache_ttl,
                },
            ))
        })
        .collect()
}
