use std::collections::BTreeMap;

use cookie_agent_config::{
    LoadedConfiguration, LoadedMcpServer, McpOAuthConfig, McpOAuthSettings, McpServerConfig,
    McpServerSource,
};
use cookie_agent_protocol::{
    McpConfigSource, McpConfigTarget, McpOAuthDefinition, McpOAuthSettingsDefinition,
    McpServerDefinition, McpServerInfo, McpServerState as WireMcpServerState,
};

use crate::{Engine, EngineError, McpServerState};

#[derive(Clone, Debug)]
pub(crate) struct ConfigStore {
    user: BTreeMap<String, McpServerConfig>,
    runtime: BTreeMap<String, Option<McpServerConfig>>,
}

impl ConfigStore {
    pub(crate) fn new(config: &LoadedConfiguration) -> Self {
        Self {
            user: config.user_mcp_servers.clone(),
            runtime: BTreeMap::new(),
        }
    }

    /// Replaces the user layer, as a configuration reload does, and returns
    /// each server whose effective configuration changed: its new one, or
    /// `None` when it is gone. The runtime overlay, tombstones included,
    /// stays on top.
    pub(crate) fn rebase(
        &mut self,
        user: BTreeMap<String, McpServerConfig>,
    ) -> Vec<(String, Option<LoadedMcpServer>)> {
        let before = self.effective_all();
        self.user = user;
        let after = self.effective_all();
        before
            .keys()
            .chain(after.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .filter_map(|name| {
                let (old, new) = (before.get(name), after.get(name));
                let changed = match (old, new) {
                    (Some(old), Some(new)) => old.config != new.config || old.source != new.source,
                    (None, None) => false,
                    _ => true,
                };
                changed.then(|| (name.clone(), new.cloned()))
            })
            .collect()
    }

    fn effective(&self, name: &str) -> Option<LoadedMcpServer> {
        if let Some(runtime) = self.runtime.get(name) {
            return runtime.clone().map(|config| LoadedMcpServer {
                source: McpServerSource::Runtime,
                config,
            });
        }
        self.user.get(name).cloned().map(|config| LoadedMcpServer {
            source: McpServerSource::UserFile,
            config,
        })
    }

    fn effective_all(&self) -> BTreeMap<String, LoadedMcpServer> {
        let names = self
            .user
            .keys()
            .chain(self.runtime.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        names
            .into_iter()
            .filter_map(|name| self.effective(&name).map(|server| (name, server)))
            .collect()
    }

    fn add(&mut self, name: &str, config: McpServerConfig) -> Result<(), EngineError> {
        if self.effective(name).is_some() {
            return Err(EngineError::Mcp(format!("server `{name}` already exists")));
        }
        config
            .validate(name)
            .map_err(|error| EngineError::Mcp(error.to_string()))?;
        self.runtime.insert(name.to_owned(), Some(config));
        Ok(())
    }

    fn edit(&mut self, name: &str, config: McpServerConfig) -> Result<(), EngineError> {
        if self.effective(name).is_none() {
            return Err(EngineError::Mcp(format!("unknown server `{name}`")));
        }
        config
            .validate(name)
            .map_err(|error| EngineError::Mcp(error.to_string()))?;
        self.runtime.insert(name.to_owned(), Some(config));
        Ok(())
    }

    fn remove(&mut self, name: &str) -> Result<(), EngineError> {
        if self.effective(name).is_none() {
            return Err(EngineError::Mcp(format!("unknown server `{name}`")));
        }
        self.runtime.insert(name.to_owned(), None);
        Ok(())
    }

    fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<(), EngineError> {
        let mut server = self
            .effective(name)
            .ok_or_else(|| EngineError::Mcp(format!("unknown server `{name}`")))?
            .config;
        server.enabled = enabled;
        self.runtime.insert(name.to_owned(), Some(server));
        Ok(())
    }

    /// The runtime entry of `name`: its configuration, or `None` for a
    /// removal.
    fn runtime_entry(&self, name: &str) -> Option<Option<McpServerConfig>> {
        self.runtime.get(name).cloned()
    }

    /// Drops the runtime entry of `name` once the user file holds it, if it
    /// is still `entry`.
    fn settle(&mut self, name: &str, entry: &Option<McpServerConfig>) {
        if self.runtime.get(name) == Some(entry) {
            self.runtime.remove(name);
        }
    }
}

impl Engine {
    /// The MCP servers of every runtime: the global runtime's (top-level
    /// `[mcp]` and runtime-layer entries), then those each running instance
    /// of another runtime hosts, with the instance's workspace.
    #[must_use]
    pub fn list_mcp_servers(&self) -> Vec<McpServerInfo> {
        let store = self
            .inner
            .config_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let statuses = self
            .inner
            .mcp
            .statuses()
            .into_iter()
            .map(|status| (status.server.clone(), status))
            .collect::<BTreeMap<_, _>>();
        store
            .effective_all()
            .into_iter()
            .map(|(name, loaded)| {
                let status = statuses.get(&name);
                let state = status.map_or(WireMcpServerState::Disconnected, |status| match status
                    .state
                {
                    McpServerState::Disabled => WireMcpServerState::Disabled,
                    McpServerState::Disconnected => WireMcpServerState::Disconnected,
                    McpServerState::Connecting => WireMcpServerState::Connecting,
                    McpServerState::Connected => WireMcpServerState::Connected,
                    McpServerState::Reconnecting => WireMcpServerState::Reconnecting,
                    McpServerState::NeedsAuth => WireMcpServerState::NeedsAuth,
                    McpServerState::Failed => WireMcpServerState::Failed,
                });
                McpServerInfo {
                    runtime_id: cookie_agent_protocol::RuntimeId::new(
                        cookie_agent_protocol::RuntimeId::GLOBAL,
                    )
                    .expect("`global` is a valid runtime ID"),
                    workspace: None,
                    name,
                    source: source_to_wire(loaded.source),
                    definition: definition_to_wire(&loaded.config),
                    state,
                    tool_count: status.map_or(0, |status| status.tools.len() as u32),
                    message: status.and_then(|status| status.message.clone()),
                    auth_in_progress: status.map(|status| status.auth_in_progress),
                }
            })
            .chain(
                self.inner
                    .runtimes
                    .hosted_mcp_servers()
                    .into_iter()
                    .filter_map(hosted_to_wire),
            )
            .collect()
    }

    /// The MCP servers of every runtime that are failed now, as a client
    /// that just subscribed to a session should hear about them.
    #[must_use]
    pub fn mcp_failures(&self) -> Vec<crate::McpServerFailure> {
        self.list_mcp_servers()
            .into_iter()
            .filter(|server| server.state == WireMcpServerState::Failed)
            .map(|server| crate::McpServerFailure {
                runtime_id: server.runtime_id.as_str().to_owned(),
                workspace: server.workspace,
                server: server.name,
                reason: server.message.unwrap_or_default(),
            })
            .collect()
    }

    /// Reconnects the MCP server `name` that the instance of `runtime_id` at
    /// `workspace` hosts, waiting for the attempt. `runtime_id` `global`
    /// means a top-level server.
    pub async fn reconnect_runtime_mcp_server(
        &self,
        runtime_id: &str,
        workspace: Option<&str>,
        name: String,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        if runtime_id == cookie_agent_protocol::RuntimeId::GLOBAL {
            return self.reconnect_mcp_server(name).await;
        }
        let connection = self
            .inner
            .runtimes
            .instance_at(runtime_id, workspace)
            .and_then(|instance| instance.connection())
            .ok_or_else(|| {
                EngineError::Mcp(match workspace {
                    Some(workspace) => {
                        format!("runtime `{runtime_id}` at {workspace} is not running")
                    }
                    None => format!("runtime `{runtime_id}` is not running"),
                })
            })?;
        connection.reconnect_mcp(&name).await?;
        Ok(self.list_mcp_servers().into_iter().find(|server| {
            server.name == name
                && server.runtime_id.as_str() == runtime_id
                && server.workspace.as_deref() == workspace
        }))
    }

    pub async fn add_mcp_server(
        &self,
        name: String,
        definition: McpServerDefinition,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        self.mutate_mcp(&name, |store| {
            store.add(&name, definition_from_wire(definition))
        })
        .await
    }

    pub async fn edit_mcp_server(
        &self,
        name: String,
        definition: McpServerDefinition,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        self.mutate_mcp(&name, |store| {
            store.edit(&name, definition_from_wire(definition))
        })
        .await
    }

    pub async fn remove_mcp_server(
        &self,
        name: String,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        self.mutate_mcp(&name, |store| store.remove(&name)).await
    }

    pub async fn set_mcp_server_enabled(
        &self,
        name: String,
        enabled: bool,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        self.mutate_mcp(&name, |store| store.set_enabled(&name, enabled))
            .await
    }

    pub async fn reconnect_mcp_server(
        &self,
        name: String,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        let _mutation = self.inner.mcp_mutation.lock().await;
        self.inner
            .mcp
            .reconnect_server(&name)
            .await
            .map_err(|error| EngineError::Mcp(error.to_string()))?;
        Ok(self
            .list_mcp_servers()
            .into_iter()
            .find(|server| server.name == name))
    }

    pub async fn begin_mcp_auth(
        &self,
        name: String,
        redirect_uri: String,
    ) -> Result<String, EngineError> {
        let _mutation = self.inner.mcp_mutation.lock().await;
        self.inner
            .mcp
            .begin_auth(&name, &redirect_uri)
            .await
            .map_err(|error| EngineError::Mcp(error.to_string()))
    }

    /// Not serialized with other MCP mutations: the exchange is a network
    /// round trip, and a cancel must be able to interrupt it.
    pub async fn complete_mcp_auth(
        &self,
        name: String,
        callback_url: String,
    ) -> Result<(), EngineError> {
        self.inner
            .mcp
            .complete_auth(&name, &callback_url)
            .await
            .map_err(|error| EngineError::Mcp(error.to_string()))
    }

    pub async fn cancel_mcp_auth(&self, name: String) -> Result<(), EngineError> {
        let _mutation = self.inner.mcp_mutation.lock().await;
        self.inner
            .mcp
            .cancel_auth(&name)
            .map_err(|error| EngineError::Mcp(error.to_string()))
    }

    pub async fn persist_mcp_server(
        &self,
        name: String,
        target: McpConfigTarget,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        let McpConfigTarget::UserFile = target;
        let entry = self
            .inner
            .config_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .runtime_entry(&name)
            .ok_or_else(|| {
                EngineError::Mcp(format!("server `{name}` has no runtime entry to persist"))
            })?;
        // Written like any configuration change, so the applied
        // configuration follows the file.
        let (server, written) = (name.clone(), entry.clone());
        self.edit_configuration(move |_, candidate| {
            candidate
                .set_mcp_server(&server, written.as_ref())
                .map_err(|error| EngineError::Mcp(error.to_string()))
        })
        .await?;
        {
            let _mutation = self.inner.mcp_mutation.lock().await;
            self.inner
                .config_store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .settle(&name, &entry);
        }
        Ok(self
            .list_mcp_servers()
            .into_iter()
            .find(|server| server.name == name))
    }

    async fn mutate_mcp(
        &self,
        name: &str,
        mutation: impl FnOnce(&mut ConfigStore) -> Result<(), EngineError>,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        let _mutation = self.inner.mcp_mutation.lock().await;
        let previous = {
            let mut store = self
                .inner
                .config_store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = store.clone();
            mutation(&mut store)?;
            previous
        };
        let effective = self
            .inner
            .config_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .effective(name);
        let result = match effective {
            Some(server) => self.inner.mcp.upsert_server(name.to_owned(), server).await,
            None => self.inner.mcp.remove_server(name).await,
        };
        if let Err(error) = result {
            *self
                .inner
                .config_store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous;
            return Err(EngineError::Mcp(error.to_string()));
        }
        Ok(self
            .list_mcp_servers()
            .into_iter()
            .find(|server| server.name == name))
    }
}

/// A server a runtime instance hosts, as `mcp.server.list` shows it. Its
/// definition comes from the runtime's template in the user file; a server
/// the runtime's own machine defines has the source `machine` and an empty
/// definition.
fn hosted_to_wire(server: crate::runtimes::HostedMcpServer) -> Option<McpServerInfo> {
    use cookie_agent_runtime_api::protocol::McpServerState as Hosted;
    let (source, definition) = match &server.definition {
        Some(definition) => (McpConfigSource::UserFile, definition_to_wire(definition)),
        None => (
            McpConfigSource::Machine,
            McpServerDefinition {
                command: None,
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
                url: None,
                headers: BTreeMap::new(),
                oauth: None,
                enabled: server.report.state != Hosted::Disabled,
                timeout_ms: None,
            },
        ),
    };
    Some(McpServerInfo {
        runtime_id: cookie_agent_protocol::RuntimeId::new(server.runtime_id.as_str()).ok()?,
        workspace: server.workspace,
        name: server.report.name,
        source,
        definition,
        state: match server.report.state {
            Hosted::Disabled => WireMcpServerState::Disabled,
            Hosted::Disconnected => WireMcpServerState::Disconnected,
            Hosted::Connecting => WireMcpServerState::Connecting,
            Hosted::Connected => WireMcpServerState::Connected,
            Hosted::Reconnecting => WireMcpServerState::Reconnecting,
            Hosted::Failed => WireMcpServerState::Failed,
        },
        tool_count: u32::try_from(server.report.tools.len()).unwrap_or(u32::MAX),
        message: server.report.message,
        auth_in_progress: None,
    })
}

fn source_to_wire(source: McpServerSource) -> McpConfigSource {
    match source {
        McpServerSource::UserFile => McpConfigSource::UserFile,
        McpServerSource::Runtime => McpConfigSource::Runtime,
    }
}

fn definition_to_wire(config: &McpServerConfig) -> McpServerDefinition {
    McpServerDefinition {
        command: config.command.clone(),
        args: config.args.clone(),
        env: config.env.clone(),
        cwd: config.cwd.clone(),
        url: config.url.clone(),
        headers: config.headers.clone(),
        oauth: Some(match &config.oauth {
            McpOAuthConfig::Disabled => McpOAuthDefinition::Bool(false),
            McpOAuthConfig::Auto | McpOAuthConfig::Enabled => McpOAuthDefinition::Bool(true),
            McpOAuthConfig::Settings(settings) => {
                McpOAuthDefinition::Settings(McpOAuthSettingsDefinition {
                    client_id: settings.client_id.clone(),
                    client_secret: settings.client_secret.clone(),
                    client_metadata_url: settings.client_metadata_url.clone(),
                    scopes: (!settings.scopes.is_empty()).then(|| settings.scopes.clone()),
                })
            }
        }),
        enabled: config.enabled,
        timeout_ms: config.timeout_ms,
    }
}

fn definition_from_wire(definition: McpServerDefinition) -> McpServerConfig {
    let stdio = definition.command.is_some();
    McpServerConfig {
        command: definition.command,
        args: definition.args,
        env: definition.env,
        cwd: definition.cwd,
        url: definition.url,
        headers: definition.headers,
        oauth: match definition.oauth {
            None => McpOAuthConfig::Auto,
            Some(McpOAuthDefinition::Bool(true)) if stdio => McpOAuthConfig::Auto,
            Some(McpOAuthDefinition::Bool(true)) => McpOAuthConfig::Enabled,
            Some(McpOAuthDefinition::Bool(false)) => McpOAuthConfig::Disabled,
            Some(McpOAuthDefinition::Settings(settings)) => {
                McpOAuthConfig::Settings(McpOAuthSettings {
                    client_id: settings.client_id,
                    client_secret: settings.client_secret,
                    client_metadata_url: settings.client_metadata_url,
                    scopes: settings.scopes.unwrap_or_default(),
                })
            }
        },
        enabled: definition.enabled,
        timeout_ms: definition.timeout_ms,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cookie_agent_config::{McpServerSource, load_from_user_root};

    use super::ConfigStore;

    #[test]
    fn runtime_layer_overrides_the_user_file_can_remove_and_survives_a_rebase() {
        let user = tempfile::tempdir().expect("user root");
        fs::write(
            user.path().join("config.toml"),
            "[mcp.servers.demo]\ncommand = \"user\"\n",
        )
        .expect("user config");
        let loaded = load_from_user_root(Some(user.path())).expect("user layer");
        let mut store = ConfigStore::new(&loaded);
        assert_eq!(
            store.effective("demo").expect("user server").source,
            McpServerSource::UserFile
        );
        let mut runtime = store.effective("demo").expect("server").config;
        runtime.command = Some("runtime".into());
        store.edit("demo", runtime.clone()).expect("runtime edit");
        assert_eq!(
            store.effective("demo").expect("runtime server").source,
            McpServerSource::Runtime
        );

        // A reload that changes the user file under a runtime entry changes
        // nothing that runs; a new user server starts.
        let mut user_layer = loaded.user_mcp_servers.clone();
        user_layer.get_mut("demo").expect("demo").command = Some("edited".into());
        let mut other = runtime.clone();
        other.command = Some("other".into());
        user_layer.insert("other".into(), other.clone());
        let changes = store.rebase(user_layer.clone());
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "other");
        assert_eq!(
            store.effective("demo").expect("runtime server").config,
            runtime
        );

        // A runtime removal stays a removal across a rebase.
        store.remove("demo").expect("runtime tombstone");
        assert!(store.effective("demo").is_none());
        assert!(store.rebase(user_layer).is_empty());
        assert!(store.effective("demo").is_none());
    }
}
