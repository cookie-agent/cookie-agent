//! `[runtimes.<id>]`: configured ways to start runtimes
//! (`docs/site/specs/runtimes.md` section 4.1).

use std::{collections::BTreeMap, time::Duration};

use cookie_agent_identity::RuntimeId;
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
}

/// A validated runtime template. `[runtimes.local]` configures the built-in
/// local coding runtime and carries only MCP servers: its `command` and
/// `env` are empty.
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
}

impl RuntimeTemplateConfig {
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
    if id == RuntimeId::LOCAL {
        if template.command.is_some()
            || template.env.is_some()
            || template.startup_timeout_ms.is_some()
            || template.shutdown_grace_ms.is_some()
        {
            return Err(invalid(
                id,
                "the built-in `local` runtime accepts only an `mcp` table",
            ));
        }
        return Ok(RuntimeTemplateConfig {
            command: Vec::new(),
            env: BTreeMap::new(),
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            mcp_servers,
        });
    }
    let command = template
        .command
        .filter(|command| !command.is_empty())
        .ok_or_else(|| invalid(id, "`command` must be a nonempty array"))?;
    if command[0].is_empty() {
        return Err(invalid(
            id,
            "the first element of `command` must name a program",
        ));
    }
    if command.iter().any(|argument| argument.contains('\0')) {
        return Err(invalid(id, "`command` elements must not contain NUL"));
    }
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
    let env = template.env.unwrap_or_default();
    if let Some(name) = env.iter().find_map(|(name, value)| {
        (name.is_empty() || name.contains(['=', '\0']) || value.contains('\0')).then_some(name)
    }) {
        return Err(invalid(
            id,
            format!(
                "environment variable `{name}` is invalid: names must be nonempty without `=` or NUL, and values without NUL"
            ),
        ));
    }
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
    })
}
