//! Onboarding as clients see it: the engine's state on the wire, and the
//! wire's run request for the engine. There is no starter kit since client
//! protocol 1.56.0: `onboarding.preview` refuses, and `onboarding.run`
//! ignores `install_kit` and `source` and writes no files.

use cookie_agent_config::{ConfiguredModel, ConfiguredModelRef, ModelField};
use cookie_agent_engine::{
    OnboardingAcpRequest, OnboardingMcpRequest, OnboardingOutcome, OnboardingRequest,
    OnboardingRequirement, OnboardingState,
};
use cookie_agent_protocol::{
    OnboardingModel, OnboardingModelEntry, OnboardingModelField, OnboardingModelInput,
    OnboardingRunParams, OnboardingRunResult, OnboardingStatus,
};

pub(crate) fn requirement_to_wire(
    requirement: OnboardingRequirement,
) -> cookie_agent_protocol::OnboardingRequirement {
    match requirement {
        OnboardingRequirement::Provider => cookie_agent_protocol::OnboardingRequirement::Provider,
        OnboardingRequirement::DefaultModel => {
            cookie_agent_protocol::OnboardingRequirement::DefaultModel
        }
        OnboardingRequirement::DefaultAgent => {
            cookie_agent_protocol::OnboardingRequirement::DefaultAgent
        }
    }
}

pub(crate) fn status_to_wire(state: OnboardingState) -> OnboardingStatus {
    let model = |field: &cookie_agent_engine::OnboardingField| {
        field
            .first
            .clone()
            .map(|(model, variant)| OnboardingModel { model, variant })
    };
    let list = |field: &cookie_agent_engine::OnboardingField| {
        field
            .configured
            .entries()
            .iter()
            .map(entry_to_wire)
            .collect::<Vec<_>>()
    };
    let paths = cookie_agent_protocol::OnboardingPaths {
        models: cookie_agent_protocol::OnboardingModelsPath {
            complete: state.models_complete(),
            missing: state
                .models_missing
                .iter()
                .copied()
                .map(requirement_to_wire)
                .collect(),
        },
        acp: cookie_agent_protocol::OnboardingAcpPath {
            complete: state.acp_complete(),
            agents: state
                .acp_agents
                .iter()
                .map(|agent| cookie_agent_protocol::OnboardingAcpAgent {
                    id: agent.id.clone(),
                    runtime: agent.runtime.clone(),
                    command: agent.command.clone(),
                    env_keys: agent.env_keys.clone(),
                    description: agent.description.clone(),
                    enabled: agent.enabled,
                    preset: agent.preset.clone(),
                    runtimes: agent
                        .runtimes
                        .iter()
                        .map(|(runtime_id, available)| {
                            cookie_agent_protocol::OnboardingAcpRuntime {
                                runtime_id: runtime_id.clone(),
                                available: *available,
                            }
                        })
                        .collect(),
                })
                .collect(),
        },
    };
    OnboardingStatus {
        ready: state.ready(),
        paths: Some(paths),
        acp_presets: state.presets.acp.clone(),
        mcp_presets: state.presets.mcp.clone(),
        mcp_servers: state
            .mcp_servers
            .iter()
            .map(
                |(name, runtime)| cookie_agent_protocol::OnboardingMcpServer {
                    name: name.clone(),
                    runtime: runtime.clone(),
                },
            )
            .collect(),
        default_agent: state.default_agent.clone(),
        missing: state.missing.into_iter().map(requirement_to_wire).collect(),
        providers_connected: state.providers_connected,
        default_model: state.default_model.as_ref().and_then(model),
        small_model: state.small_model.as_ref().and_then(model),
        utility_model: state.utility_model.as_ref().and_then(model),
        default_model_list: state.default_model.as_ref().map(list),
        small_model_list: state.small_model.as_ref().map(list),
        utility_model_list: state.utility_model.as_ref().map(list),
    }
}

const fn field_to_wire(field: ModelField) -> OnboardingModelField {
    match field {
        ModelField::Default => OnboardingModelField::DefaultModel,
        ModelField::Small => OnboardingModelField::SmallModel,
        ModelField::Utility => OnboardingModelField::UtilityModel,
    }
}

fn entry_to_wire(entry: &ConfiguredModel) -> OnboardingModelEntry {
    let (model, field) = match &entry.model {
        ConfiguredModelRef::Model(model) => (Some(model.clone()), None),
        ConfiguredModelRef::Field(field) => (None, Some(field_to_wire(*field))),
    };
    OnboardingModelEntry {
        model,
        field,
        variant: entry.variant.clone(),
    }
}

fn model_value(model: OnboardingModelInput) -> serde_json::Value {
    match model {
        OnboardingModelInput::Key(key) => serde_json::Value::String(key.to_string()),
        OnboardingModelInput::Table(table) => {
            serde_json::to_value(table).expect("a model table serializes")
        }
    }
}

/// `entries` as the configuration takes them: an entry is a model key or a
/// field's variable, a table when it has a variant, and a list of one is
/// the entry alone.
fn list_value(name: &str, entries: Vec<OnboardingModelEntry>) -> Result<serde_json::Value, String> {
    if entries.is_empty() || entries.len() > cookie_agent_protocol::MAX_ONBOARDING_MODEL_ENTRIES {
        return Err(format!(
            "`{name}_list` holds 1 to {} entries",
            cookie_agent_protocol::MAX_ONBOARDING_MODEL_ENTRIES
        ));
    }
    let mut values = entries
        .into_iter()
        .map(|entry| {
            let model = match (entry.model, entry.field) {
                (Some(model), None) => model.to_string(),
                (None, Some(field)) => {
                    let field = match field {
                        OnboardingModelField::DefaultModel => ModelField::Default,
                        OnboardingModelField::SmallModel => ModelField::Small,
                        OnboardingModelField::UtilityModel => ModelField::Utility,
                        OnboardingModelField::Unknown => {
                            return Err(format!("`{name}_list` names an unknown field"));
                        }
                    };
                    field.expression().to_owned()
                }
                _ => {
                    return Err(format!(
                        "each entry of `{name}_list` has exactly one of `model` and `field`"
                    ));
                }
            };
            Ok(match entry.variant {
                None => serde_json::Value::String(model),
                Some(variant) => serde_json::json!({ "model": model, "variant": variant }),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(if values.len() == 1 {
        values.remove(0)
    } else {
        serde_json::Value::Array(values)
    })
}

/// The engine's request for `params`; a message for `invalid_params` when a
/// field is given both as one model and as a list, or a list is malformed.
pub(crate) fn request_from_wire(params: OnboardingRunParams) -> Result<OnboardingRequest, String> {
    let field = |name: &str,
                 single: Option<OnboardingModelInput>,
                 list: Option<Vec<OnboardingModelEntry>>| {
        match (single, list) {
            (Some(_), Some(_)) => Err(format!("give `{name}` or `{name}_list`, not both")),
            (Some(single), None) => Ok(Some(model_value(single))),
            (None, Some(list)) => list_value(name, list).map(Some),
            (None, None) => Ok(None),
        }
    };
    Ok(OnboardingRequest {
        default_model: field(
            "default_model",
            params.default_model,
            params.default_model_list,
        )?,
        small_model: field("small_model", params.small_model, params.small_model_list)?,
        utility_model: field(
            "utility_model",
            params.utility_model,
            params.utility_model_list,
        )?,
        acp_agents: params
            .acp_agents
            .unwrap_or_default()
            .into_iter()
            .map(|agent| OnboardingAcpRequest {
                id: agent.id,
                runtime: agent.runtime,
                command: agent.command,
                env: agent.env,
                description: agent.description,
                model: agent.model,
                effort: agent.effort,
                cache_ttl: agent.cache_ttl,
            })
            .collect(),
        mcp_servers: params
            .mcp_servers
            .unwrap_or_default()
            .into_iter()
            .map(|server| OnboardingMcpRequest {
                name: server.name,
                runtime: server.runtime,
                server: server.server,
            })
            .collect(),
        default_agent: params.default_agent,
    })
}

pub(crate) fn outcome_to_wire(outcome: OnboardingOutcome) -> OnboardingRunResult {
    OnboardingRunResult {
        written: Vec::new(),
        skipped: Vec::new(),
        written_new: Vec::new(),
        settings: outcome.settings,
        status: status_to_wire(outcome.state),
    }
}
