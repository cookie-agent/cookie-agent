//! Applying a new configuration generation to a running engine: the
//! settings tables, agent documents, providers, headers, and MCP servers take
//! effect at once; `server`, `plugins`, and `runtimes` are reported as needing
//! a restart. A run keeps what it captured at admission (its agent, result
//! limits, retry policy, and delegation depth).

use std::{collections::BTreeMap, sync::Arc};

use cookie_agent_config::{ConfigTree, LoadedConfiguration, load_from_tree};
use cookie_agent_protocol::{ConfigRevision, RuntimeChangeReason};

use crate::{Engine, EngineError};

/// The configuration parts that apply only when the daemon restarts.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RestartRequiredPart {
    Server,
    Plugins,
    Runtimes,
}

impl RestartRequiredPart {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Plugins => "plugins",
            Self::Runtimes => "runtimes",
        }
    }
}

/// Where the engine's configuration stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigurationStatus {
    /// The revision of the applied generation.
    pub applied_revision: ConfigRevision,
    /// Parts whose applied value differs from the one the engine opened
    /// with.
    pub restart_required: Vec<RestartRequiredPart>,
    /// Why the last reload failed, until one succeeds.
    pub error: Option<String>,
}

impl Engine {
    /// The applied configuration generation.
    #[must_use]
    pub fn configuration(&self) -> Arc<LoadedConfiguration> {
        self.inner.config.load_full()
    }

    #[must_use]
    pub fn configuration_status(&self) -> ConfigurationStatus {
        let applied = self.inner.config.load();
        let started = &self.inner.started_config;
        let mut restart_required = Vec::new();
        let (now, then) = (&applied.runtime.server, &started.runtime.server);
        if now.host != then.host || now.port != then.port || now.token != then.token {
            restart_required.push(RestartRequiredPart::Server);
        }
        if applied.plugins != started.plugins {
            restart_required.push(RestartRequiredPart::Plugins);
        }
        if applied.runtimes != started.runtimes {
            restart_required.push(RestartRequiredPart::Runtimes);
        }
        ConfigurationStatus {
            applied_revision: applied.revision.clone(),
            restart_required,
            error: self
                .inner
                .config_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        }
    }

    /// The user root the configuration is read from and written to.
    pub(crate) fn configuration_root(&self) -> Result<std::path::PathBuf, EngineError> {
        self.inner
            .config
            .load()
            .config_paths
            .user
            .as_deref()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
            .ok_or_else(|| {
                EngineError::Runtime("the daemon has no user configuration directory".into())
            })
    }

    /// Reads `config.toml` and the agent documents from disk and applies
    /// them, for edits made by hand. A tree that fails validation leaves the
    /// applied generation in place and is reported in the status.
    pub async fn reload_configuration(&self) -> Result<ConfigurationStatus, EngineError> {
        let _mutation = self.inner.config_mutation.lock().await;
        let root = self.configuration_root()?;
        let candidate = tokio::task::spawn_blocking(move || {
            ConfigTree::read(Some(&root)).and_then(|tree| load_from_tree(&tree))
        })
        .await
        .map_err(|error| EngineError::Runtime(error.to_string()))?;
        match candidate {
            Ok(candidate) => {
                self.apply_configuration_locked(candidate).await?;
                Ok(self.configuration_status())
            }
            Err(error) => {
                *self
                    .inner
                    .config_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
                Err(EngineError::Config(Box::new(error)))
            }
        }
    }

    /// Applies a validated generation; the caller holds `config_mutation`.
    pub(crate) async fn apply_configuration_locked(
        &self,
        mut candidate: LoadedConfiguration,
    ) -> Result<(), EngineError> {
        let previous = self.inner.config.load_full();
        // Skills are read once at start; a reload keeps them.
        candidate.skills = previous.skills.clone();
        let mut reasons = vec![RuntimeChangeReason::ConfigReloaded];
        if agent_fingerprints(&previous) != agent_fingerprints(&candidate) {
            reasons.push(RuntimeChangeReason::AgentReloaded);
        }
        let candidate = Arc::new(candidate);
        {
            let _runtime = self
                .inner
                .runtime_mutation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let published = self.current_runtime();
            let (_, publication) = self.inner.model_manager.reload_configuration(
                candidate.runtime.providers.clone(),
                candidate.runtime.headers.clone(),
                Arc::clone(published.models.catalog()),
                |models| {
                    self.prepare_publication(&candidate, models, &published, reasons)
                        .map_err(|_| cookie_agent_models::ModelManagerError::RuntimeCompileFailed)
                },
            )?;
            self.inner.config.store(Arc::clone(&candidate));
            self.publish(publication);
        }
        *self
            .inner
            .config_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.rebase_mcp_servers(&candidate).await;
        Ok(())
    }

    /// Puts the new user layer under the MCP store's runtime overlay and
    /// brings the registry in line with what changed.
    async fn rebase_mcp_servers(&self, config: &LoadedConfiguration) {
        let _mutation = self.inner.mcp_mutation.lock().await;
        let changes = self
            .inner
            .config_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .rebase(config.user_mcp_servers.clone());
        // A server that fails to start reports through the MCP failure
        // notifications like any other.
        for (name, server) in changes {
            let _ = match server {
                Some(server) => self.inner.mcp.upsert_server(name, server).await,
                None => self.inner.mcp.remove_server(&name).await,
            };
        }
    }
}

/// Each agent document's fingerprint, shared and per preset.
fn agent_fingerprints(
    config: &LoadedConfiguration,
) -> BTreeMap<(Option<&str>, &str), &cookie_agent_models::Sha256Digest> {
    config
        .agents
        .iter()
        .map(|(id, document)| ((None, id.as_str()), &document.document_fingerprint))
        .chain(config.agent_presets.iter().flat_map(|(preset, agents)| {
            agents.iter().map(move |(id, document)| {
                (
                    (Some(preset.as_str()), id.as_str()),
                    &document.document_fingerprint,
                )
            })
        }))
        .collect()
}
