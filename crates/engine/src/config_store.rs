use std::{collections::BTreeMap, path::PathBuf};

use cookie_agent_config::{
    LoadedConfiguration, LoadedMcpServer, McpServerConfig, McpServerSource, write_mcp_server,
};
use cookie_agent_protocol::{
    McpConfigSource, McpConfigTarget, McpServerDefinition, McpServerInfo,
    McpServerState as WireMcpServerState,
};

use crate::{Engine, EngineError, McpServerState};

#[derive(Clone, Debug)]
pub(crate) struct ConfigStore {
    user: BTreeMap<String, McpServerConfig>,
    workspace: BTreeMap<String, McpServerConfig>,
    runtime: BTreeMap<String, Option<McpServerConfig>>,
    user_path: Option<PathBuf>,
    workspace_path: Option<PathBuf>,
}

impl ConfigStore {
    pub(crate) fn new(config: &LoadedConfiguration) -> Self {
        Self {
            user: config.user_mcp_servers.clone(),
            workspace: config.workspace_mcp_servers.clone(),
            runtime: BTreeMap::new(),
            user_path: config.config_paths.user.clone(),
            workspace_path: config.config_paths.workspace.clone(),
        }
    }

    fn effective(&self, name: &str) -> Option<LoadedMcpServer> {
        if let Some(runtime) = self.runtime.get(name) {
            return runtime.clone().map(|config| LoadedMcpServer {
                source: McpServerSource::Runtime,
                config,
            });
        }
        self.workspace
            .get(name)
            .cloned()
            .map(|config| LoadedMcpServer {
                source: McpServerSource::WorkspaceFile,
                config,
            })
            .or_else(|| {
                self.user.get(name).cloned().map(|config| LoadedMcpServer {
                    source: McpServerSource::UserFile,
                    config,
                })
            })
    }

    fn effective_all(&self) -> BTreeMap<String, LoadedMcpServer> {
        let names = self
            .user
            .keys()
            .chain(self.workspace.keys())
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

    fn persist(&mut self, name: &str, target: McpConfigTarget) -> Result<(), EngineError> {
        let config = self
            .runtime
            .get(name)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or_else(|| {
                EngineError::Mcp(format!("server `{name}` has no runtime entry to persist"))
            })?;
        let (path, layer) = match target {
            McpConfigTarget::UserFile => (&self.user_path, &mut self.user),
            McpConfigTarget::WorkspaceFile => (&self.workspace_path, &mut self.workspace),
        };
        let path = path.as_ref().ok_or_else(|| {
            EngineError::Mcp("the selected configuration file layer is unavailable".into())
        })?;
        write_mcp_server(path, name, &config)
            .map_err(|error| EngineError::Mcp(error.to_string()))?;
        layer.insert(name.to_owned(), config);
        Ok(())
    }
}

impl Engine {
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
                    McpServerState::PendingApproval => WireMcpServerState::PendingApproval,
                    McpServerState::Disconnected if loaded.config.lazy => {
                        WireMcpServerState::LazyNotConnected
                    }
                    McpServerState::Disconnected => WireMcpServerState::Disconnected,
                    McpServerState::Connecting => WireMcpServerState::Connecting,
                    McpServerState::Connected => WireMcpServerState::Connected,
                    McpServerState::Failed => WireMcpServerState::Failed,
                    McpServerState::Rejected => WireMcpServerState::Rejected,
                });
                McpServerInfo {
                    name,
                    source: source_to_wire(loaded.source),
                    definition: definition_to_wire(&loaded.config),
                    state,
                    tool_count: status.map_or(0, |status| status.tools.len() as u32),
                    message: status.and_then(|status| status.message.clone()),
                }
            })
            .collect()
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

    pub async fn persist_mcp_server(
        &self,
        name: String,
        target: McpConfigTarget,
    ) -> Result<Option<McpServerInfo>, EngineError> {
        let _mutation = self.inner.mcp_mutation.lock().await;
        self.inner
            .config_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .persist(&name, target)?;
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

fn source_to_wire(source: McpServerSource) -> McpConfigSource {
    match source {
        McpServerSource::UserFile => McpConfigSource::UserFile,
        McpServerSource::WorkspaceFile => McpConfigSource::WorkspaceFile,
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
        enabled: config.enabled,
        lazy: config.lazy,
        timeout_ms: config.timeout_ms,
    }
}

fn definition_from_wire(definition: McpServerDefinition) -> McpServerConfig {
    McpServerConfig {
        command: definition.command,
        args: definition.args,
        env: definition.env,
        cwd: definition.cwd,
        url: definition.url,
        headers: definition.headers,
        enabled: definition.enabled,
        lazy: definition.lazy,
        timeout_ms: definition.timeout_ms,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cookie_agent_config::{McpServerSource, load_from_roots};
    use cookie_agent_protocol::McpConfigTarget;

    use super::ConfigStore;

    #[test]
    fn runtime_layer_overrides_files_can_remove_and_writes_back_strictly() {
        let user = tempfile::tempdir().expect("user root");
        let workspace = tempfile::tempdir().expect("workspace root");
        fs::write(
            user.path().join("config.toml"),
            "# user comment\n[mcp.servers.demo]\ncommand = \"user\"\n",
        )
        .expect("user config");
        fs::write(
            workspace.path().join("config.toml"),
            "[mcp.servers.demo]\ncommand = \"workspace\"\n",
        )
        .expect("workspace config");
        let loaded = load_from_roots(Some(user.path()), Some(workspace.path())).expect("layers");
        let mut store = ConfigStore::new(&loaded);
        assert_eq!(
            store.effective("demo").expect("workspace server").source,
            McpServerSource::WorkspaceFile
        );
        let mut runtime = store.effective("demo").expect("server").config;
        runtime.command = Some("runtime".into());
        store.edit("demo", runtime.clone()).expect("runtime edit");
        assert_eq!(
            store.effective("demo").expect("runtime server").source,
            McpServerSource::Runtime
        );
        store
            .persist("demo", McpConfigTarget::UserFile)
            .expect("write user layer");
        assert!(
            fs::read_to_string(user.path().join("config.toml"))
                .expect("written user config")
                .contains("runtime")
        );
        store.remove("demo").expect("runtime tombstone");
        assert!(store.effective("demo").is_none());
        let reloaded = load_from_roots(Some(user.path()), Some(workspace.path()))
            .expect("strictly reloaded file layers");
        assert_eq!(
            reloaded.mcp_servers["demo"].source,
            McpServerSource::WorkspaceFile
        );
    }
}
