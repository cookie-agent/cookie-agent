//! Onboarding (`docs/site/specs/default-models-and-onboarding.md` section
//! 8, and `docs/site/specs/machines-on-demand.md` section 2.1): whether the
//! daemon is set up, derived from the live configuration and runtime with
//! nothing stored for it, and the one call that sets it up: the model
//! fields. There is no starter kit: the agents are official ones the daemon
//! keeps up to date, and onboarding fetches them when the default agent is
//! missing.

#[cfg(test)]
mod tests;

use std::sync::Arc;

use cookie_agent_config::{
    AgentMode, ConfigTree, ConfiguredModel, LoadedConfiguration, load_from_tree, write_tree,
};
use cookie_agent_identity::ProviderId;
use cookie_agent_models::{CompiledModelRuntime, compiler::CompiledModelStatus};

use super::configuration::config_error;
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
    pub default_model: Option<ConfiguredModel>,
    pub small_model: Option<ConfiguredModel>,
    pub utility_model: Option<ConfiguredModel>,
}

impl OnboardingState {
    #[must_use]
    pub fn ready(&self) -> bool {
        self.missing.is_empty()
    }
}

/// An `onboarding.run`: the model fields to set, each as the configuration
/// takes it, a key string or `{ model, variant }`.
#[derive(Clone, Debug, Default)]
pub struct OnboardingRequest {
    pub default_model: Option<serde_json::Value>,
    pub small_model: Option<serde_json::Value>,
    pub utility_model: Option<serde_json::Value>,
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
/// it exists, is enabled, and is a `primary` or `all` agent.
fn default_agent_present(config: &LoadedConfiguration) -> bool {
    config
        .agents
        .get(&config.runtime.agents.default_agent())
        .is_some_and(|document| {
            document.frontmatter.enabled
                && matches!(
                    document.frontmatter.mode,
                    AgentMode::Primary | AgentMode::All
                )
        })
}

/// The onboarding state of `config` against `models`.
pub(crate) fn onboarding_state(
    config: &LoadedConfiguration,
    models: &CompiledModelRuntime,
) -> OnboardingState {
    let mut providers_connected = models
        .models()
        .values()
        .filter(|model| model.model.status == CompiledModelStatus::Available)
        .map(|model| model.key.provider_id())
        .collect::<Vec<_>>();
    providers_connected.sort();
    providers_connected.dedup();
    let available = |configured: Option<&ConfiguredModel>| {
        configured.is_some_and(|configured| {
            crate::runtime_snapshot::selection_available(
                models,
                &crate::runtime_snapshot::configured_selection(models, configured, None),
            )
        })
    };
    let runtime = &config.runtime;
    let mut missing = Vec::new();
    if providers_connected.is_empty() {
        missing.push(OnboardingRequirement::Provider);
    }
    if !available(runtime.default_model.as_ref()) {
        missing.push(OnboardingRequirement::DefaultModel);
    }
    if !default_agent_present(config) {
        missing.push(OnboardingRequirement::DefaultAgent);
    }
    OnboardingState {
        missing,
        providers_connected,
        default_model: runtime.default_model.clone(),
        small_model: runtime.small_model.clone(),
        utility_model: runtime.utility_model.clone(),
    }
}

impl Engine {
    /// Whether the daemon is set up, from the applied configuration and the
    /// current runtime.
    #[must_use]
    pub fn onboarding_state(&self) -> OnboardingState {
        let config = self.inner.config.load();
        onboarding_state(&config, &self.current_runtime().models)
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
        let state = onboarding_state(&self.inner.config.load(), models);
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
        if !default_agent_present(&self.inner.config.load()) {
            // The official agents arrive with this; a failure leaves the
            // default agent missing, which the readiness check reports.
            self.refresh_official_agents().await;
        }
        let _mutation = self.inner.config_mutation.lock().await;
        let root = self.configuration_root()?;
        let models = Arc::clone(&self.current_runtime().models);
        let fields = [
            ("default_model", request.default_model),
            ("small_model", request.small_model),
            ("utility_model", request.utility_model),
        ];
        let (settings, loaded) = super::blocking_io::run(move || {
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
            let loaded = load_from_tree(&candidate).map_err(config_error)?;
            let state = onboarding_state(&loaded, &models);
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
