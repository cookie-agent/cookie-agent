//! Applying a new configuration generation to a running engine: the
//! settings tables, agent documents, providers, headers, and MCP servers take
//! effect at once; `server`, `plugins`, and `runtimes` are reported as needing
//! a restart. A run keeps what it captured at admission (its agent, result
//! limits, retry policy, and delegation depth).

use std::{collections::BTreeMap, sync::Arc};

use cookie_agent_config::{
    ConfigError, ConfigPath, ConfigTree, LoadedConfiguration, load_from_tree, write_tree,
};
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

/// One change of a [`Engine::apply_config_changes`] set.
#[derive(Clone, Debug)]
pub enum ConfigChange {
    /// Creates (`expected_fingerprint` absent) or replaces a document.
    Put {
        path: ConfigPath,
        text: String,
        expected_fingerprint: Option<String>,
    },
    /// Removes a document.
    Delete {
        path: ConfigPath,
        expected_fingerprint: String,
    },
    /// Sets one key of `config.toml`; `null` removes it.
    Set {
        key: Vec<String>,
        value: serde_json::Value,
    },
}

/// A document of the configuration on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigDocumentText {
    pub path: ConfigPath,
    pub text: String,
    pub fingerprint: String,
}

/// The configuration on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigDocuments {
    pub revision: ConfigRevision,
    pub documents: Vec<ConfigDocumentText>,
    /// Preset directories without documents.
    pub empty_presets: Vec<String>,
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
    /// What the applied configuration names that does not exist yet, such
    /// as a delegation target no agent has: an ACP agent whose runtime has
    /// not reported it, or a typo. Recomputed whenever the agents change.
    pub diagnostics: Vec<String>,
    /// The providers the native compaction record marks.
    pub native_compaction: Vec<cookie_agent_protocol::NativeCompactionMark>,
    pub deferred_tools: Vec<cookie_agent_protocol::DeferredToolsMark>,
}

impl Engine {
    /// Records the token the daemon serves with when it created one at
    /// start and wrote it to `config.toml`, so the file holding it does not
    /// read as a `server` change waiting for a restart.
    pub fn note_served_server_token(&self, token: &str) {
        let _ = self.inner.served_server_token.set(token.to_owned());
    }

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
        // A token the daemon created at start and wrote to the file is the
        // one it serves with.
        let served_token = self
            .inner
            .served_server_token
            .get()
            .map(String::as_str)
            .or(then.token.as_deref());
        if now.host != then.host || now.port != then.port || now.token.as_deref() != served_token {
            restart_required.push(RestartRequiredPart::Server);
        }
        if applied.plugins != started.plugins {
            restart_required.push(RestartRequiredPart::Plugins);
        }
        // A runtime's MCP servers and ACP agents reach it as they change
        // (`runtime/settings_changed`); how the daemon starts one takes a
        // restart.
        if runtime_launches(&applied) != runtime_launches(started) {
            restart_required.push(RestartRequiredPart::Runtimes);
        }
        ConfigurationStatus {
            applied_revision: applied.revision.clone(),
            restart_required,
            diagnostics: self.configuration_diagnostics(),
            native_compaction: self.inner.native_compaction.marks(),
            deferred_tools: self.inner.deferred_tools_record.marks(),
            error: self
                .inner
                .config_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        }
    }

    /// The ACP declarations a daemon agent document of the same ID shadows,
    /// one without an `acp` block or one whose `acp.agent` names another
    /// declaration: the document's agent keeps the ID and the declarations
    /// are ignored. Then the documents whose `acp.agent` no runtime
    /// declares yet.
    fn shadowed_acp_declarations(&self) -> Vec<String> {
        let config = self.inner.config.load();
        let declarations = self.inner.acp.runtime_agents();
        let mut by_agent =
            std::collections::BTreeMap::<cookie_agent_protocol::AgentId, Vec<String>>::new();
        for agent in &declarations {
            if config.agents.get(&agent.agent).is_some_and(|document| {
                document
                    .frontmatter
                    .acp
                    .as_ref()
                    .is_none_or(|settings| settings.declaration(&document.id) != &agent.agent)
            }) {
                by_agent
                    .entry(agent.agent.clone())
                    .or_default()
                    .push(agent.runtime.to_string());
            }
        }
        let mut diagnostics = by_agent
            .into_iter()
            .map(|(agent, runtimes)| {
                let declared_on = match &runtimes[..] {
                    [runtime] => format!("runtime `{runtime}`"),
                    _ => format!("runtimes {}", runtimes.join(", ")),
                };
                match config
                    .agents
                    .get(&agent)
                    .and_then(|document| document.frontmatter.acp.as_ref())
                    .and_then(|settings| settings.agent.as_ref())
                {
                    Some(other) => format!(
                        "ACP agent `{agent}` of {declared_on} is ignored: the agent document \
                         `{agent}` runs `{other}` (`acp.agent`), so `{agent}` is that \
                         document's agent"
                    ),
                    None => format!(
                        "ACP agent `{agent}` of {declared_on} is ignored: the agent document \
                         `{agent}` has no `acp` block, so `{agent}` is a daemon agent; give the \
                         document an `acp: {{}}` block to make it the ACP agent's"
                    ),
                }
            })
            .collect::<Vec<_>>();
        for document in config.agents.values() {
            let Some(declaration) = document
                .frontmatter
                .acp
                .as_ref()
                .and_then(|settings| settings.agent.as_ref())
            else {
                continue;
            };
            if !declarations.iter().any(|agent| &agent.agent == declaration) {
                diagnostics.push(format!(
                    "agent `{}` runs ACP agent `{declaration}` (`acp.agent`), which no runtime \
                     declares yet; it is an agent once one does",
                    document.id
                ));
            }
        }
        diagnostics
    }

    /// The diagnostics of the published agents: those of the shared agents,
    /// then each preset's own.
    fn configuration_diagnostics(&self) -> Vec<String> {
        let published = self.current_runtime();
        let mut diagnostics = self.shadowed_acp_declarations();
        diagnostics.extend(published.agents.diagnostics());
        for (preset, registry) in &published.agent_presets {
            for diagnostic in registry.diagnostics() {
                if !diagnostics.contains(&diagnostic) {
                    diagnostics.push(format!("preset `{preset}`: {diagnostic}"));
                }
            }
        }
        diagnostics.extend(self.official_skills_diagnostic());
        diagnostics.extend(self.approval_cache_diagnostics(&published));
        diagnostics.truncate(256);
        diagnostics
    }

    /// What `[approval] cache_ttl` does on each model the approval reviewer
    /// names: a TTL its provider does not offer, or a provider it cannot
    /// change (approval reviewer spec section 5). `${parent_model}` is the
    /// reviewed run's model, unknown until a review.
    fn approval_cache_diagnostics(
        &self,
        published: &crate::runtime_snapshot::PublishedRuntime,
    ) -> Vec<String> {
        let ttl = self.inner.config.load().runtime.approval.cache_ttl;
        let Ok(id) =
            cookie_agent_protocol::AgentId::new(cookie_agent_config::BUILT_IN_APPROVAL_AGENT_ID)
        else {
            return Vec::new();
        };
        let Some(approval) = published.agents.get(&id) else {
            return Vec::new();
        };
        approval
            .resolved_fallback
            .iter()
            .filter_map(|fallback| match fallback {
                crate::runtime_snapshot::ResolvedAgentFallback::Selection { selection, .. } => {
                    Some(selection)
                }
                crate::runtime_snapshot::ResolvedAgentFallback::ParentModel { .. } => None,
            })
            .filter_map(|selection| {
                let binding = crate::model_snapshots::binding_for_selection(
                    &published.current_manifest,
                    &published.models,
                    selection,
                )
                .ok()?;
                let provider = crate::policy::resolve_cache_strategy(
                    Some(approval),
                    &binding,
                    published,
                    cookie_agent_config::CacheLayer::Root,
                )
                .ok()
                .flatten();
                let (_, note) = crate::policy::reviewer_cache_strategy(provider, &binding, ttl);
                let model = selection.model.to_string();
                Some(match note? {
                    crate::policy::ReviewerCacheNote::Substituted { used, why } => format!(
                        "`[approval] cache_ttl = \"{}\"`: the reviewer's model `{model}` caches for {used}, {why}",
                        ttl.as_str()
                    ),
                    crate::policy::ReviewerCacheNote::NoEffect(why) => format!(
                        "`[approval] cache_ttl = \"{}\"` has no effect on the reviewer's model `{model}`: {why}",
                        ttl.as_str()
                    ),
                })
            })
            .collect()
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

    /// The configuration documents on disk, which may differ from the
    /// applied ones until a reload.
    pub async fn configuration_documents(&self) -> Result<ConfigDocuments, EngineError> {
        let root = self.configuration_root()?;
        let tree = tokio::task::spawn_blocking(move || ConfigTree::read(Some(&root)))
            .await
            .map_err(|error| EngineError::Runtime(error.to_string()))?
            .map_err(config_error)?;
        Ok(ConfigDocuments {
            revision: tree.revision(),
            documents: tree
                .documents()
                .map(|(path, bytes)| ConfigDocumentText {
                    path: path.clone(),
                    text: String::from_utf8_lossy(bytes).into_owned(),
                    fingerprint: ConfigTree::fingerprint(bytes),
                })
                .collect(),
            empty_presets: tree.empty_presets().map(str::to_owned).collect(),
        })
    }

    /// Applies `changes` to the configuration on disk as one set: each is
    /// checked against `expected_revision` and its own fingerprint, the
    /// result is validated whole, written, and applied. Returns the new
    /// revision.
    pub async fn apply_config_changes(
        &self,
        expected_revision: ConfigRevision,
        changes: Vec<ConfigChange>,
    ) -> Result<ConfigRevision, EngineError> {
        let edits_config = changes.iter().any(|change| match change {
            ConfigChange::Put { path, .. } | ConfigChange::Delete { path, .. } => {
                *path == ConfigPath::config()
            }
            ConfigChange::Set { .. } => false,
        });
        if edits_config
            && changes
                .iter()
                .any(|change| matches!(change, ConfigChange::Set { .. }))
        {
            return Err(EngineError::ConfigChangeRefused(
                "a set and a put or delete of config.toml cannot go together".into(),
            ));
        }
        self.edit_configuration(move |tree, candidate| {
            if tree.revision() != expected_revision {
                return Err(EngineError::ConfigConflict(
                    "the configuration changed since it was read".into(),
                ));
            }
            for change in changes {
                apply_change(candidate, change)?;
            }
            Ok(())
        })
        .await
    }

    /// Edits the configuration on disk with `edit`, which gets the tree as
    /// read and the candidate to change, then validates, writes, and applies
    /// the candidate. Returns the new revision.
    pub(crate) async fn edit_configuration(
        &self,
        edit: impl FnOnce(&ConfigTree, &mut ConfigTree) -> Result<(), EngineError> + Send + 'static,
    ) -> Result<ConfigRevision, EngineError> {
        let _mutation = self.inner.config_mutation.lock().await;
        let root = self.configuration_root()?;
        let (revision, candidate) = tokio::task::spawn_blocking(move || {
            let tree = ConfigTree::read(Some(&root)).map_err(config_error)?;
            let mut candidate = tree.clone();
            edit(&tree, &mut candidate)?;
            let loaded = load_from_tree(&candidate).map_err(config_error)?;
            write_tree(&tree, &candidate).map_err(|error| match error {
                ConfigError::ChangedOnDisk(path) => EngineError::ConfigConflict(format!(
                    "{} changed while it was written",
                    path.display()
                )),
                error => config_error(error),
            })?;
            Ok::<_, EngineError>((candidate.revision(), loaded))
        })
        .await
        .map_err(|error| EngineError::Runtime(error.to_string()))??;
        self.apply_configuration_locked(candidate).await?;
        Ok(revision)
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
        // The runtime mutation lock may be held through another change's
        // recompile, and this one recompiles too: both stay off the async
        // workers.
        let engine = self.clone();
        let applied = Arc::clone(&candidate);
        super::on_blocking_pool(move || {
            let _runtime = engine
                .inner
                .runtime_mutation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let published = engine.current_runtime();
            let (_, publication) = engine.inner.model_manager.reload_configuration(
                applied.runtime.providers.clone(),
                applied.runtime.headers.clone(),
                Arc::clone(published.models.catalog()),
                |models| {
                    engine
                        .prepare_publication(&applied, models, &published, reasons)
                        .map_err(|_| cookie_agent_models::ModelManagerError::RuntimeCompileFailed)
                },
            )?;
            engine.inner.config.store(Arc::clone(&applied));
            engine.publish(publication);
            engine.inner.runtimes.update_settings(&applied);
            Ok(())
        })
        .await?;
        // The publication also wakes the background sync; awaiting one here
        // makes the files current when the apply or reload answers.
        self.sync_agent_memory().await;
        self.drop_unconfigured_runtime_records(&candidate).await;
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

pub(crate) fn config_error(error: ConfigError) -> EngineError {
    EngineError::Config(Box::new(error))
}

fn apply_change(candidate: &mut ConfigTree, change: ConfigChange) -> Result<(), EngineError> {
    let current = |candidate: &ConfigTree, path: &ConfigPath| {
        candidate.get(path).map(ConfigTree::fingerprint)
    };
    match change {
        ConfigChange::Put {
            path,
            text,
            expected_fingerprint,
        } => {
            if current(candidate, &path) != expected_fingerprint {
                return Err(EngineError::ConfigConflict(match expected_fingerprint {
                    None => format!("{path} already exists"),
                    Some(_) => format!("{path} changed since it was read"),
                }));
            }
            candidate
                .put(path, text.into_bytes())
                .map_err(|error| EngineError::ConfigChangeRefused(error.to_string()))
        }
        ConfigChange::Delete {
            path,
            expected_fingerprint,
        } => {
            if current(candidate, &path).as_deref() != Some(expected_fingerprint.as_str()) {
                return Err(EngineError::ConfigConflict(format!(
                    "{path} changed since it was read"
                )));
            }
            candidate.delete(&path);
            Ok(())
        }
        ConfigChange::Set { key, value } => candidate
            .set_key_json(&key, &value)
            .map_err(|error| EngineError::ConfigChangeRefused(error.to_string())),
    }
}

/// How the daemon starts one runtime: its ID, command, environment, startup
/// timeout, and shutdown grace.
type RuntimeLaunch<'a> = (
    &'a str,
    &'a [String],
    &'a std::collections::BTreeMap<String, String>,
    u64,
    u64,
);

/// How the daemon starts each runtime `config` has a template for.
fn runtime_launches(config: &LoadedConfiguration) -> Vec<RuntimeLaunch<'_>> {
    config
        .runtimes
        .iter()
        .filter(|(_, template)| template.daemon_starts())
        .map(|(id, template)| {
            (
                id.as_str(),
                template.command.as_slice(),
                &template.env,
                template.startup_timeout_ms,
                template.shutdown_grace_ms,
            )
        })
        .collect()
}
