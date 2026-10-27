//! Changing a runtime's `[runtimes.<id>]` table from a client: its MCP
//! servers and ACP agents (`docs/site/specs/runtime-management.md`
//! section 4). Every change is one configuration edit, written to the
//! daemon's `config.toml` and applied like `config.apply`, so a connected
//! runtime gets it at once (`runtime/settings_changed`).

use cookie_agent_config::{AcpAgentConfig, ConfigTree, LoadedConfiguration, McpServerConfig};
use cookie_agent_protocol::{
    AcpAgentDefinition, AgentId, McpServerDefinition, McpServerInfo, RuntimeAgentDeclaration,
    RuntimeId,
};

use super::{Engine, EngineError, configuration::config_error};

/// The key of `[runtimes.<runtime>]` and the path below it.
fn runtime_key(runtime: &RuntimeId, path: &[&str]) -> Vec<String> {
    ["runtimes", runtime.as_str()]
        .into_iter()
        .chain(path.iter().copied())
        .map(str::to_owned)
        .collect()
}

fn refused(reason: impl Into<String>) -> EngineError {
    EngineError::ConfigChangeRefused(reason.into())
}

/// Refuses `global`, whose servers are the top-level `[mcp]` ones.
fn table_runtime(runtime: &RuntimeId) -> Result<(), EngineError> {
    if runtime.as_str() == RuntimeId::GLOBAL {
        return Err(refused(
            "`global` has no [runtimes.global] table; its servers are the top-level [mcp] ones",
        ));
    }
    Ok(())
}

fn set(
    candidate: &mut ConfigTree,
    key: Vec<String>,
    value: Option<toml::Value>,
) -> Result<(), EngineError> {
    candidate.set_key(&key, value).map_err(config_error)
}

fn toml_value<T: serde::Serialize>(what: &str, value: &T) -> Result<toml::Value, EngineError> {
    toml::Value::try_from(value).map_err(|error| refused(format!("{what}: {error}")))
}

/// `runtime`'s MCP servers as `config` has them.
fn runtime_servers(
    config: &LoadedConfiguration,
    runtime: &RuntimeId,
) -> std::collections::BTreeMap<String, McpServerConfig> {
    config
        .runtimes
        .get(runtime)
        .map(|template| template.mcp_servers.clone())
        .unwrap_or_default()
}

/// `runtime`'s ACP agent declarations as `config` has them: `global`'s are
/// `[acp.agents]`, those every runtime offers.
fn runtime_agents(
    config: &LoadedConfiguration,
    runtime: &RuntimeId,
) -> std::collections::BTreeMap<cookie_agent_identity::AgentId, AcpAgentConfig> {
    if runtime.as_str() == RuntimeId::GLOBAL {
        return config.runtime.acp.agents.clone();
    }
    config
        .runtimes
        .get(runtime)
        .map(|template| template.acp_agents.clone())
        .unwrap_or_default()
}

/// The configuration key of `runtime`'s declaration of `agent`:
/// `[acp.agents.<agent>]` for `global`, which every runtime offers.
fn agent_key(runtime: &RuntimeId, agent: &str) -> Vec<String> {
    if runtime.as_str() == RuntimeId::GLOBAL {
        return ["acp", "agents", agent].map(str::to_owned).to_vec();
    }
    runtime_key(runtime, &["acp", "agents", agent])
}

fn identity_agent(agent: &AgentId) -> Result<cookie_agent_identity::AgentId, EngineError> {
    cookie_agent_identity::AgentId::new(agent.as_str())
        .map_err(|_| refused(format!("invalid agent ID `{agent}`")))
}

fn agent_config(definition: AcpAgentDefinition) -> AcpAgentConfig {
    AcpAgentConfig {
        command: definition.command,
        env: definition.env,
        description: definition.description,
        enabled: definition.enabled,
    }
}

/// The agent documents that run declaration `agent`: the one of its own ID
/// and the templates whose `acp.agent` names it, as `config.get` paths.
fn documents_running(config: &LoadedConfiguration, agent: &str) -> Vec<String> {
    let runs = |id: &cookie_agent_identity::AgentId,
                document: &cookie_agent_config::AgentDocument| {
        document.frontmatter.acp.as_ref().is_some_and(|acp| {
            acp.agent
                .as_ref()
                .map_or(id.as_str() == agent, |declared| declared.as_str() == agent)
        })
    };
    let mut documents = config
        .agents
        .iter()
        .filter(|(id, document)| runs(id, document))
        .map(|(id, _)| format!("agents/{id}.md"))
        .chain(config.agent_presets.iter().flat_map(|(preset, agents)| {
            agents
                .iter()
                .filter(|(id, document)| runs(id, document))
                .map(move |(id, _)| format!("agents/{preset}/{id}.md"))
        }))
        .collect::<Vec<_>>();
    documents.sort();
    documents
}

impl Engine {
    /// The servers `mcp.server.list` lists for `runtime` and `name`, the
    /// first of them when several instances host it.
    fn listed_server(&self, runtime: &RuntimeId, name: &str) -> Option<McpServerInfo> {
        self.list_mcp_servers()
            .into_iter()
            .find(|server| server.runtime_id == *runtime && server.name == name)
    }

    /// Adds `[runtimes.<runtime>.mcp.servers.<name>]`.
    pub async fn add_runtime_mcp_server(
        &self,
        runtime: RuntimeId,
        name: String,
        definition: McpServerDefinition,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        table_runtime(&runtime)?;
        if runtime_servers(&self.configuration(), &runtime).contains_key(&name) {
            return Err(refused(format!(
                "runtime `{runtime}` already has an MCP server `{name}`"
            )));
        }
        self.write_runtime_server(&runtime, &name, definition)
            .await?;
        Ok(self.listed_server(&runtime, &name))
    }

    /// Replaces `[runtimes.<runtime>.mcp.servers.<name>]`.
    pub async fn edit_runtime_mcp_server(
        &self,
        runtime: RuntimeId,
        name: String,
        definition: McpServerDefinition,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        table_runtime(&runtime)?;
        self.known_runtime_server(&runtime, &name)?;
        self.write_runtime_server(&runtime, &name, definition)
            .await?;
        Ok(self.listed_server(&runtime, &name))
    }

    /// Removes `[runtimes.<runtime>.mcp.servers.<name>]`.
    pub async fn remove_runtime_mcp_server(
        &self,
        runtime: RuntimeId,
        name: String,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        table_runtime(&runtime)?;
        self.known_runtime_server(&runtime, &name)?;
        let key = runtime_key(&runtime, &["mcp", "servers", &name]);
        self.edit_configuration(move |_, candidate| set(candidate, key, None))
            .await?;
        Ok(None)
    }

    /// Sets `enabled` of `[runtimes.<runtime>.mcp.servers.<name>]`.
    pub async fn set_runtime_mcp_server_enabled(
        &self,
        runtime: RuntimeId,
        name: String,
        enabled: bool,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        table_runtime(&runtime)?;
        self.known_runtime_server(&runtime, &name)?;
        let key = runtime_key(&runtime, &["mcp", "servers", &name, "enabled"]);
        self.edit_configuration(move |_, candidate| {
            set(candidate, key, Some(toml::Value::Boolean(enabled)))
        })
        .await?;
        Ok(self.listed_server(&runtime, &name))
    }

    /// Moves the server `name` of `runtime`, `global` when `None`, to
    /// `new_name` in one configuration edit. A `global` server changed in
    /// the running daemon only (`/mcp` without persisting) is refused: its
    /// change lives outside the file.
    pub async fn rename_mcp_server(
        &self,
        runtime: Option<RuntimeId>,
        name: String,
        new_name: String,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        if name == new_name {
            return Err(refused("the new name is the server's name"));
        }
        let config = self.configuration();
        let (old_key, new_key, server, listed_runtime) = match runtime
            .filter(|runtime| runtime.as_str() != RuntimeId::GLOBAL)
        {
            Some(runtime) => {
                let servers = runtime_servers(&config, &runtime);
                let server = servers.get(&name).cloned().ok_or_else(|| {
                    refused(format!("runtime `{runtime}` has no MCP server `{name}`"))
                })?;
                if servers.contains_key(&new_name) {
                    return Err(refused(format!(
                        "runtime `{runtime}` already has an MCP server `{new_name}`"
                    )));
                }
                (
                    runtime_key(&runtime, &["mcp", "servers", &name]),
                    runtime_key(&runtime, &["mcp", "servers", &new_name]),
                    server,
                    runtime,
                )
            }
            None => {
                if self.mcp_runtime_overlay(&name) {
                    return Err(refused(format!(
                        "MCP server `{name}` was changed in the running daemon only; persist or \
                         remove that change before renaming it"
                    )));
                }
                let server = config.user_mcp_servers.get(&name).cloned().ok_or_else(|| {
                    refused(format!("config.toml has no MCP server `{name}` to rename"))
                })?;
                if config.mcp_servers.contains_key(&new_name) {
                    return Err(refused(format!("an MCP server `{new_name}` exists")));
                }
                let key = |name: &str| vec!["mcp".to_owned(), "servers".to_owned(), name.into()];
                (
                    key(&name),
                    key(&new_name),
                    server,
                    RuntimeId::new(RuntimeId::GLOBAL).expect("`global` is a valid runtime ID"),
                )
            }
        };
        let value = toml_value(&format!("MCP server `{name}`"), &server)?;
        self.edit_configuration(move |_, candidate| {
            set(candidate, old_key, None)?;
            set(candidate, new_key, Some(value))
        })
        .await?;
        Ok(self.listed_server(&listed_runtime, &new_name))
    }

    fn known_runtime_server(&self, runtime: &RuntimeId, name: &str) -> Result<(), EngineError> {
        if runtime_servers(&self.configuration(), runtime).contains_key(name) {
            Ok(())
        } else {
            Err(refused(format!(
                "runtime `{runtime}` has no MCP server `{name}` in [runtimes.{runtime}.mcp]"
            )))
        }
    }

    async fn write_runtime_server(
        &self,
        runtime: &RuntimeId,
        name: &str,
        definition: McpServerDefinition,
    ) -> Result<(), EngineError> {
        let server = crate::config_store::definition_from_wire(definition);
        let value = toml_value(&format!("MCP server `{name}`"), &server)?;
        let key = runtime_key(runtime, &["mcp", "servers", name]);
        self.edit_configuration(move |_, candidate| set(candidate, key, Some(value)))
            .await?;
        Ok(())
    }

    /// `runtime`'s ACP agent declarations, in key order.
    #[must_use]
    pub fn runtime_agent_declarations(&self, runtime: &RuntimeId) -> Vec<RuntimeAgentDeclaration> {
        let config = self.configuration();
        let mut reports = self.inner.runtimes.acp_agents();
        // One every runtime offers is available while some runtime runs it.
        let reported = if runtime.as_str() == RuntimeId::GLOBAL {
            reports.into_values().collect::<Vec<_>>()
        } else {
            reports.remove(runtime.as_str()).into_iter().collect()
        };
        runtime_agents(&config, runtime)
            .into_iter()
            .filter_map(|(agent, declared)| {
                let available = reported.iter().any(|reported| {
                    reported.available
                        && reported
                            .agents
                            .iter()
                            .any(|report| report.id == agent.as_str() && report.enabled)
                });
                Some(RuntimeAgentDeclaration {
                    documents: documents_running(&config, agent.as_str()),
                    agent: AgentId::new(agent.as_str()).ok()?,
                    definition: AcpAgentDefinition {
                        command: declared.command,
                        env: declared.env,
                        description: declared.description,
                        enabled: declared.enabled,
                    },
                    available,
                })
            })
            .collect()
    }

    /// Adds or replaces `[runtimes.<runtime>.acp.agents.<agent>]`, or for
    /// `global` `[acp.agents.<agent>]`, which every runtime offers.
    pub async fn set_runtime_agent(
        &self,
        runtime: RuntimeId,
        agent: AgentId,
        definition: AcpAgentDefinition,
    ) -> Result<Vec<RuntimeAgentDeclaration>, EngineError> {
        identity_agent(&agent)?;
        let value = toml_value(&format!("ACP agent `{agent}`"), &agent_config(definition))?;
        let key = agent_key(&runtime, agent.as_str());
        self.edit_configuration(move |_, candidate| set(candidate, key, Some(value)))
            .await?;
        Ok(self.runtime_agent_declarations(&runtime))
    }

    /// Removes `[runtimes.<runtime>.acp.agents.<agent>]`; its documents
    /// stay.
    pub async fn remove_runtime_agent(
        &self,
        runtime: RuntimeId,
        agent: AgentId,
    ) -> Result<Vec<RuntimeAgentDeclaration>, EngineError> {
        self.known_runtime_agent(&runtime, &agent)?;
        let key = agent_key(&runtime, agent.as_str());
        self.edit_configuration(move |_, candidate| set(candidate, key, None))
            .await?;
        Ok(self.runtime_agent_declarations(&runtime))
    }

    /// Moves `[runtimes.<runtime>.acp.agents.<agent>]` to `new_agent`. When
    /// no other runtime declares `agent` afterwards, the same edit moves
    /// `agents/<agent>.md` to `agents/<new_agent>.md`; templates whose
    /// `acp.agent` names `agent` are not rewritten, and the result names
    /// them. Returns the declarations, the moved documents, and those
    /// templates.
    pub async fn rename_runtime_agent(
        &self,
        runtime: RuntimeId,
        agent: AgentId,
        new_agent: AgentId,
    ) -> Result<(Vec<RuntimeAgentDeclaration>, Vec<String>, Vec<String>), EngineError> {
        if agent == new_agent {
            return Err(refused("the new key is the agent's key"));
        }
        identity_agent(&new_agent)?;
        let config = self.configuration();
        let declared = runtime_agents(&config, &runtime);
        let declaration = declared
            .get(&identity_agent(&agent)?)
            .cloned()
            .ok_or_else(|| {
                refused(format!(
                    "runtime `{runtime}` declares no ACP agent `{agent}`"
                ))
            })?;
        if declared.contains_key(&identity_agent(&new_agent)?) {
            return Err(refused(format!(
                "runtime `{runtime}` already declares an ACP agent `{new_agent}`"
            )));
        }
        let still_declared = config.runtimes.iter().any(|(id, template)| {
            *id.as_str() != *runtime.as_str()
                && template
                    .acp_agents
                    .keys()
                    .any(|key| key.as_str() == agent.as_str())
        }) || (runtime.as_str() != RuntimeId::GLOBAL
            && config
                .runtime
                .acp
                .agents
                .keys()
                .any(|key| key.as_str() == agent.as_str()));
        let own = format!("agents/{agent}.md");
        let moved = (!still_declared
            && config.agents.keys().any(|id| id.as_str() == agent.as_str()))
        .then(|| own.clone());
        let stale_templates = if still_declared {
            Vec::new()
        } else {
            documents_running(&config, agent.as_str())
                .into_iter()
                .filter(|path| *path != own)
                .collect()
        };
        let value = toml_value(&format!("ACP agent `{agent}`"), &declaration)?;
        let (old_key, new_key) = (
            agent_key(&runtime, agent.as_str()),
            agent_key(&runtime, new_agent.as_str()),
        );
        let move_document = moved
            .clone()
            .map(|old| (old, format!("agents/{new_agent}.md")));
        self.edit_configuration(move |_, candidate| {
            set(candidate, old_key, None)?;
            set(candidate, new_key, Some(value))?;
            if let Some((old, new)) = move_document {
                let old = cookie_agent_config::ConfigPath::new(&old).map_err(config_error)?;
                let new = cookie_agent_config::ConfigPath::new(&new).map_err(config_error)?;
                if candidate.get(&new).is_some() {
                    return Err(refused(format!(
                        "{} exists; move or remove it first",
                        new.as_str()
                    )));
                }
                let text = candidate
                    .get(&old)
                    .map(<[u8]>::to_vec)
                    .ok_or_else(|| refused(format!("{} is gone", old.as_str())))?;
                candidate.put(new, text).map_err(config_error)?;
                candidate.delete(&old);
            }
            Ok(())
        })
        .await?;
        Ok((
            self.runtime_agent_declarations(&runtime),
            moved.into_iter().collect(),
            stale_templates,
        ))
    }

    fn known_runtime_agent(&self, runtime: &RuntimeId, agent: &AgentId) -> Result<(), EngineError> {
        if runtime_agents(&self.configuration(), runtime)
            .keys()
            .any(|key| key.as_str() == agent.as_str())
        {
            Ok(())
        } else {
            Err(refused(format!(
                "runtime `{runtime}` declares no ACP agent `{agent}`"
            )))
        }
    }
}
