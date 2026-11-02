//! Onboarding (`docs/site/specs/default-models-and-onboarding.md` section
//! 8, and `docs/site/specs/machines-on-demand.md` section 2.1): whether the
//! daemon is set up, derived from the live configuration and runtime with
//! nothing stored for it, and the one call that sets it up: the model
//! fields. There is no starter kit: the agents are official ones the daemon
//! keeps up to date, and onboarding fetches them when the default agent is
//! missing.
//!
//! A daemon is set up by either of two paths
//! (`docs/site/specs/acp-onboarding.md` section 2): the models path (a
//! connected provider and `default_model`) or the ACP path (an enabled ACP
//! agent a runtime that can run it offers). `onboarding.run` also declares
//! ACP agents, adds MCP servers, and sets `[agents] default`.

#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, sync::Arc};

use cookie_agent_config::{
    AgentMode, ConfigTree, ConfiguredModels, LoadedConfiguration, ModelField, load_from_tree,
    write_tree,
};
use cookie_agent_identity::{AgentId, ConfiguredVariantRef, ModelKey, ProviderId, RuntimeId};
use cookie_agent_models::{CompiledModelRuntime, compiler::CompiledModelStatus};

use crate::runtime::configuration::config_error;
use crate::{Engine, EngineError};

/// What onboarding still needs.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OnboardingRequirement {
    /// No model provider is connected: none offers an available model.
    Provider,
    /// `default_model` is unset or names no available model.
    DefaultModel,
    /// `[agents] default` names no agent that can start a session, such as
    /// before the official agents were first fetched.
    DefaultAgent,
}

/// Whether the daemon is set up, from its configuration and runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnboardingState {
    /// Sorted; empty when the daemon is set up.
    pub missing: Vec<OnboardingRequirement>,
    /// The providers that offer at least one available model, sorted.
    pub providers_connected: Vec<ProviderId>,
    pub default_model: Option<OnboardingField>,
    pub small_model: Option<OnboardingField>,
    pub utility_model: Option<OnboardingField>,
    /// What the models path still needs (`provider`, `default_model`).
    pub models_missing: Vec<OnboardingRequirement>,
    /// Every ACP agent declaration, in ID and place order.
    pub acp_agents: Vec<OnboardingAcpAgent>,
    /// The configured MCP servers, by name and place (`None`: top level).
    pub mcp_servers: Vec<(String, Option<RuntimeId>)>,
    /// `[agents] default` as configured.
    pub default_agent: Option<AgentId>,
    /// The presets onboarding offers.
    pub presets: Arc<super::agents::OfficialPresets>,
}

/// An ACP agent declaration as onboarding reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnboardingAcpAgent {
    pub id: AgentId,
    /// The runtime whose table declares it; `None` for `[acp.agents]`.
    pub runtime: Option<RuntimeId>,
    pub command: Vec<String>,
    pub env_keys: Vec<String>,
    pub description: String,
    pub enabled: bool,
    /// The preset whose command and environment it has.
    pub preset: Option<AgentId>,
    /// The runtimes that offer it, each with whether it can run it now.
    pub runtimes: Vec<(RuntimeId, bool)>,
}

impl OnboardingAcpAgent {
    /// Whether a runtime that offers it can run it now.
    #[must_use]
    pub fn runnable(&self) -> bool {
        self.enabled && self.runtimes.iter().any(|(_, available)| *available)
    }
}

/// A set model field as onboarding reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnboardingField {
    /// The field as configured, its references to other fields kept.
    pub configured: ConfiguredModels,
    /// The first model the field reaches and that model's variant as
    /// configured; `None` when it reaches none (it names only unset fields).
    pub first: Option<(ModelKey, Option<ConfiguredVariantRef>)>,
}

impl OnboardingState {
    #[must_use]
    pub fn ready(&self) -> bool {
        self.missing.is_empty()
    }

    /// Whether the models path is done.
    #[must_use]
    pub fn models_complete(&self) -> bool {
        self.models_missing.is_empty()
    }

    /// Whether the ACP path is done.
    #[must_use]
    pub fn acp_complete(&self) -> bool {
        self.acp_agents.iter().any(OnboardingAcpAgent::runnable)
    }
}

/// An `onboarding.run`: the model fields to set, each as the configuration
/// takes it, a key string or `{ model, variant }`, and the ACP agents, MCP
/// servers, and default agent to configure.
#[derive(Clone, Debug, Default)]
pub struct OnboardingRequest {
    pub default_model: Option<serde_json::Value>,
    pub small_model: Option<serde_json::Value>,
    pub utility_model: Option<serde_json::Value>,
    pub acp_agents: Vec<OnboardingAcpRequest>,
    pub mcp_servers: Vec<OnboardingMcpRequest>,
    pub default_agent: Option<AgentId>,
}

/// An ACP agent `onboarding.run` declares.
#[derive(Clone, Debug)]
pub struct OnboardingAcpRequest {
    pub id: AgentId,
    /// `None`: `[acp.agents]`, every runtime.
    pub runtime: Option<RuntimeId>,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub description: Option<String>,
    /// The agent's model and effort values for `agents/<id>.md`.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The declaration's `cache_ttl`, from its preset.
    pub cache_ttl: Option<String>,
}

/// An MCP server `onboarding.run` adds.
#[derive(Clone, Debug)]
pub struct OnboardingMcpRequest {
    pub name: String,
    /// `None`: `[mcp.servers]`.
    pub runtime: Option<RuntimeId>,
    pub server: serde_json::Map<String, serde_json::Value>,
}

/// What an `onboarding.run` did.
#[derive(Clone, Debug)]
pub struct OnboardingOutcome {
    /// The configuration fields set.
    pub settings: Vec<String>,
    pub state: OnboardingState,
}

/// The onboarding state the engine publishes.
pub(crate) struct OnboardingRuntimeState {
    /// The onboarding state as of the runtime published last, computed once
    /// per publication for every connection to compare.
    state: tokio::sync::watch::Sender<OnboardingState>,
}

impl OnboardingRuntimeState {
    /// The state of an engine that opens in onboarding state `state`.
    pub(crate) fn new(state: OnboardingState) -> Self {
        Self {
            state: tokio::sync::watch::Sender::new(state),
        }
    }
}

/// `missing` as a sentence: "no provider is connected; `default_model` …".
pub(crate) fn describe_missing(missing: &[OnboardingRequirement]) -> String {
    missing
        .iter()
        .map(|requirement| match requirement {
            OnboardingRequirement::Provider => "no model provider is connected",
            OnboardingRequirement::DefaultModel => {
                "`default_model` does not name an available model"
            }
            OnboardingRequirement::DefaultAgent => {
                "`[agents] default` names no agent that can start a session"
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Whether the default agent of `config` is one a session can start with:
/// it exists, is enabled, and is a `primary` or `all` agent, or it is an ACP
/// declaration that `acp_agents` lists enabled. With `[agents] default`
/// unset, a complete ACP path is enough: new sessions start on the first
/// agent that can run as a root.
fn default_agent_present(config: &LoadedConfiguration, acp_agents: &[OnboardingAcpAgent]) -> bool {
    let id = config.runtime.agents.default_agent();
    let document = config.agents.get(&id).is_some_and(|document| {
        document.frontmatter.enabled
            && matches!(
                document.frontmatter.mode,
                AgentMode::Primary | AgentMode::All
            )
    });
    let declared = acp_agents
        .iter()
        .any(|agent| agent.id == id && agent.enabled);
    document
        || declared
        || (config.runtime.agents.default.is_none()
            && acp_agents.iter().any(OnboardingAcpAgent::runnable))
}

/// The ACP agent declarations of `config`, each with the runtimes that offer
/// it by `availability` (every registered runtime, and whether it can run
/// ACP agents now), and the preset of `presets` it matches.
fn acp_agents(
    config: &LoadedConfiguration,
    availability: &BTreeMap<String, bool>,
    presets: &super::agents::OfficialPresets,
) -> Vec<OnboardingAcpAgent> {
    let preset_of = |declaration: &cookie_agent_config::AcpAgentConfig| {
        presets
            .acp
            .iter()
            .find(|preset| preset.command == declaration.command && preset.env == declaration.env)
            .map(|preset| preset.id.clone())
    };
    let runtime_id = |id: &str| RuntimeId::new(id).ok();
    let mut agents = Vec::new();
    for (id, declaration) in &config.runtime.acp.agents {
        // Every runtime but `global` offers it, unless its own table
        // declares the same key.
        let runtimes = availability
            .iter()
            .filter(|(runtime, _)| runtime.as_str() != RuntimeId::GLOBAL)
            .filter(|(runtime, _)| {
                !config.runtimes.iter().any(|(template_id, template)| {
                    template_id.as_str() == runtime.as_str() && template.acp_agents.contains_key(id)
                })
            })
            .filter_map(|(runtime, available)| Some((runtime_id(runtime)?, *available)))
            .collect();
        agents.push(OnboardingAcpAgent {
            id: id.clone(),
            runtime: None,
            command: declaration.command.clone(),
            env_keys: declaration.env.keys().cloned().collect(),
            description: declaration.description.clone(),
            enabled: declaration.enabled,
            preset: preset_of(declaration),
            runtimes,
        });
    }
    for (template_id, template) in &config.runtimes {
        for (id, declaration) in &template.acp_agents {
            let available = availability
                .get(template_id.as_str())
                .copied()
                // A template the registry does not know yet, such as one
                // a run is adding: the daemon starts one with a command,
                // and `local` always runs.
                .unwrap_or(template.daemon_starts() || template_id.as_str() == "local");
            agents.push(OnboardingAcpAgent {
                id: id.clone(),
                runtime: Some(template_id.clone()),
                command: declaration.command.clone(),
                env_keys: declaration.env.keys().cloned().collect(),
                description: declaration.description.clone(),
                enabled: declaration.enabled,
                preset: preset_of(declaration),
                runtimes: vec![(template_id.clone(), available)],
            });
        }
    }
    agents.sort_by(|left, right| (&left.id, &left.runtime).cmp(&(&right.id, &right.runtime)));
    agents
}

/// The MCP servers of `config`, by name and place.
fn mcp_servers(config: &LoadedConfiguration) -> Vec<(String, Option<RuntimeId>)> {
    let mut servers = config
        .user_mcp_servers
        .keys()
        .map(|name| (name.clone(), None))
        .collect::<Vec<_>>();
    for (runtime, template) in &config.runtimes {
        servers.extend(
            template
                .mcp_servers
                .keys()
                .map(|name| (name.clone(), Some(runtime.clone()))),
        );
    }
    servers
}

/// The onboarding state of `config` against `models`, with `availability`
/// the registered runtimes and whether each can run ACP agents now.
pub(crate) fn onboarding_state(
    config: &LoadedConfiguration,
    models: &CompiledModelRuntime,
    availability: &BTreeMap<String, bool>,
    presets: Arc<super::agents::OfficialPresets>,
) -> OnboardingState {
    let mut providers_connected = models
        .models()
        .values()
        .filter(|model| model.model.status == CompiledModelStatus::Available)
        .map(|model| model.key.provider_id())
        .collect::<Vec<_>>();
    providers_connected.sort();
    providers_connected.dedup();
    let variables = crate::runtime_snapshot::ModelVariables::of(&config.runtime);
    let runtime = &config.runtime;
    let mut models_missing = Vec::new();
    if providers_connected.is_empty() {
        models_missing.push(OnboardingRequirement::Provider);
    }
    if !variables
        .selections(models, ModelField::Default, None, &mut Vec::new())
        .iter()
        .any(|selection| crate::runtime_snapshot::selection_available(models, selection))
    {
        models_missing.push(OnboardingRequirement::DefaultModel);
    }
    // The daemon's own machine always runs its agents, whether or not its
    // runtime has reported yet.
    let mut availability = availability.clone();
    availability.insert("local".to_owned(), true);
    let acp_agents = acp_agents(config, &availability, &presets);
    // While the ACP path is complete, what only the models path needs is
    // not missing.
    let mut missing = if acp_agents.iter().any(OnboardingAcpAgent::runnable) {
        Vec::new()
    } else {
        models_missing.clone()
    };
    if !default_agent_present(config, &acp_agents) {
        missing.push(OnboardingRequirement::DefaultAgent);
    }
    let field = |field: ModelField| {
        runtime
            .model_field(field)
            .map(|configured| OnboardingField {
                configured: configured.clone(),
                first: variables
                    .expand(models, field, None, &mut Vec::new())
                    .into_iter()
                    .next()
                    .map(|first| (first.model, first.variant)),
            })
    };
    OnboardingState {
        missing,
        providers_connected,
        default_model: field(ModelField::Default),
        small_model: field(ModelField::Small),
        utility_model: field(ModelField::Utility),
        models_missing,
        acp_agents,
        mcp_servers: mcp_servers(config),
        default_agent: runtime.agents.default.clone(),
        presets,
    }
}

/// `agents/<id>.md` for an ACP agent `onboarding.run` gives a model: the
/// `_acp` template it inherits, the user's when there is one, and `models`.
fn acp_model_document(tree: &ConfigTree, model: &str, effort: Option<&str>) -> String {
    let user_template = cookie_agent_config::ConfigPath::new("agents/_acp.md")
        .ok()
        .is_some_and(|path| tree.get(&path).is_some());
    let inherit = if user_template {
        "_acp.md"
    } else {
        "../official_agents/_acp.md"
    };
    let entry = match effort {
        Some(effort) => serde_json::json!({ "model": model, "variant": effort }),
        None => serde_json::json!(model),
    };
    format!(
        "---\ninherit: {inherit}\nmodels: [{}]\n---\n",
        serde_json::to_string(&entry).expect("a JSON value serializes")
    )
}

/// Applies `request`'s ACP agents, MCP servers, and default agent to
/// `candidate`, naming each setting in `settings`.
fn apply_agents_and_tools(
    candidate: &mut ConfigTree,
    request: &OnboardingRequest,
    settings: &mut Vec<String>,
) -> Result<(), EngineError> {
    let refused = |error: cookie_agent_config::ConfigError| {
        EngineError::ConfigChangeRefused(error.to_string())
    };
    let config = load_from_tree(candidate).map_err(config_error)?;
    for agent in &request.acp_agents {
        if agent
            .command
            .first()
            .is_none_or(|program| program.trim().is_empty())
        {
            return Err(EngineError::ConfigChangeRefused(format!(
                "ACP agent `{}` has no command",
                agent.id
            )));
        }
        let mut key = match &agent.runtime {
            None => vec!["acp".to_owned()],
            Some(runtime) => vec!["runtimes".to_owned(), runtime.to_string(), "acp".to_owned()],
        };
        key.extend(["agents".to_owned(), agent.id.to_string()]);
        let mut table = serde_json::Map::new();
        table.insert("command".into(), serde_json::json!(agent.command));
        if !agent.env.is_empty() {
            table.insert("env".into(), serde_json::json!(agent.env));
        }
        if let Some(description) = agent.description.as_deref().filter(|text| !text.is_empty()) {
            table.insert("description".into(), serde_json::json!(description));
        }
        if let Some(cache_ttl) = &agent.cache_ttl {
            table.insert("cache_ttl".into(), serde_json::json!(cache_ttl));
        }
        candidate
            .set_key_json(&key, &serde_json::Value::Object(table))
            .map_err(refused)?;
        settings.push(key.join("."));
        if let Some(model) = &agent.model {
            let path = cookie_agent_config::ConfigPath::new(&format!("agents/{}.md", agent.id))
                .map_err(refused)?;
            if candidate.get(&path).is_none() && !config.agents.contains_key(&agent.id) {
                let document = acp_model_document(candidate, model, agent.effort.as_deref());
                candidate
                    .put(path.clone(), document.into_bytes())
                    .map_err(refused)?;
                settings.push(path.to_string());
            }
        }
    }
    for server in &request.mcp_servers {
        let taken = match &server.runtime {
            None => config.user_mcp_servers.contains_key(&server.name),
            Some(runtime) => config
                .runtimes
                .get(runtime)
                .is_some_and(|template| template.mcp_servers.contains_key(&server.name)),
        };
        if taken {
            return Err(EngineError::ConfigChangeRefused(format!(
                "MCP server `{}` is already configured there",
                server.name
            )));
        }
        if let Some(problem) = super::agents::mcp_server_problem(
            &server.name,
            &server.server,
            server.runtime.is_none(),
        ) {
            return Err(EngineError::ConfigChangeRefused(format!(
                "MCP server `{}`: {problem}",
                server.name
            )));
        }
        let mut key = match &server.runtime {
            None => vec!["mcp".to_owned()],
            Some(runtime) => vec!["runtimes".to_owned(), runtime.to_string(), "mcp".to_owned()],
        };
        key.extend(["servers".to_owned(), server.name.clone()]);
        candidate
            .set_key_json(&key, &serde_json::Value::Object(server.server.clone()))
            .map_err(refused)?;
        settings.push(key.join("."));
    }
    if let Some(agent) = &request.default_agent {
        candidate
            .set_key_json(
                &["agents".to_owned(), "default".to_owned()],
                &serde_json::json!(agent.as_str()),
            )
            .map_err(refused)?;
        settings.push("agents.default".to_owned());
    }
    Ok(())
}

impl Engine {
    /// Whether the daemon is set up, from the applied configuration and the
    /// current runtime.
    #[must_use]
    pub fn onboarding_state(&self) -> OnboardingState {
        let config = self.inner.config.load();
        onboarding_state(
            &config,
            &self.current_runtime().models,
            &self.inner.runtimes.acp_availability(),
            self.official_presets(),
        )
    }

    /// The onboarding state, updated with each runtime publication when it
    /// changes: a configuration or runtime change publishes one.
    #[must_use]
    pub fn subscribe_onboarding(&self) -> tokio::sync::watch::Receiver<OnboardingState> {
        self.inner.onboarding.state.subscribe()
    }

    /// Recomputes the onboarding state against `models`, about to be
    /// published, telling subscribers when it changed.
    pub(crate) fn publish_onboarding_state(&self, models: &CompiledModelRuntime) {
        let state = onboarding_state(
            &self.inner.config.load(),
            models,
            &self.inner.runtimes.acp_availability(),
            self.official_presets(),
        );
        self.inner.onboarding.state.send_if_modified(|current| {
            let changed = *current != state;
            if changed {
                *current = state;
            }
            changed
        });
    }

    /// Sets the model fields `request` names, as one change, after fetching
    /// the official agents when the default agent is missing. Refuses with
    /// [`EngineError::OnboardingIncomplete`], writing nothing, when the
    /// daemon would not be set up afterwards.
    pub async fn run_onboarding(
        &self,
        request: OnboardingRequest,
    ) -> Result<OnboardingOutcome, EngineError> {
        let current = self.inner.config.load();
        let needs_officials = current.runtime.agents.official
            && (!current
                .agents
                .contains_key(&current.runtime.agents.default_agent())
                || (!request.acp_agents.is_empty() && current.templates.is_empty()));
        drop(current);
        if needs_officials {
            // The official agents (and the `_acp` template) arrive with
            // this; a failure leaves the default agent missing, which the
            // readiness check reports.
            self.refresh_official_agents().await;
        }
        let availability = self.inner.runtimes.acp_availability();
        let presets = self.official_presets();
        let _mutation = self.inner.config_mutation.lock().await;
        let root = self.configuration_root()?;
        let models = Arc::clone(&self.current_runtime().models);
        let request_rest = OnboardingRequest {
            default_model: None,
            small_model: None,
            utility_model: None,
            ..request.clone()
        };
        let fields = [
            ("default_model", request.default_model),
            ("small_model", request.small_model),
            ("utility_model", request.utility_model),
        ];
        let (settings, loaded) = crate::runtime::blocking_io::run(move || {
            let tree = ConfigTree::read(Some(&root)).map_err(config_error)?;
            let mut candidate = tree.clone();
            let mut settings = Vec::new();
            for (field, value) in fields {
                if let Some(value) = value {
                    candidate
                        .set_key_json(&[field.to_owned()], &value)
                        .map_err(|error| EngineError::ConfigChangeRefused(error.to_string()))?;
                    settings.push(field.to_owned());
                }
            }
            apply_agents_and_tools(&mut candidate, &request_rest, &mut settings)?;
            let loaded = load_from_tree(&candidate).map_err(config_error)?;
            let state = onboarding_state(&loaded, &models, &availability, presets);
            if !state.ready() {
                return Err(EngineError::OnboardingIncomplete(state.missing));
            }
            let changed = tree.revision() != candidate.revision();
            if changed {
                write_tree(&tree, &candidate).map_err(|error| match error {
                    cookie_agent_config::ConfigError::ChangedOnDisk(path) => {
                        EngineError::ConfigConflict(format!(
                            "{} changed while it was written",
                            path.display()
                        ))
                    }
                    error => config_error(error),
                })?;
            }
            Ok::<_, EngineError>((settings, changed.then_some(loaded)))
        })
        .await
        .map_err(|error| EngineError::Runtime(error.to_string()))??;
        if let Some(loaded) = loaded {
            self.apply_configuration_locked(loaded).await?;
        }
        Ok(OnboardingOutcome {
            settings,
            state: self.onboarding_state(),
        })
    }
}
