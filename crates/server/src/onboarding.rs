//! Onboarding as clients see it: the engine's state on the wire, and the
//! wire's run request for the engine. There is no starter kit since client
//! protocol 1.56.0: `onboarding.preview` refuses, and `onboarding.run`
//! ignores `install_kit` and `source` and writes no files.

use cookie_agent_engine::{
    OnboardingOutcome, OnboardingRequest, OnboardingRequirement, OnboardingState,
};
use cookie_agent_protocol::{
    OnboardingModel, OnboardingModelInput, OnboardingRunParams, OnboardingRunResult,
    OnboardingStatus,
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
    let model = |model: cookie_agent_config::ConfiguredModel| OnboardingModel {
        model: model.model,
        variant: model.variant,
    };
    OnboardingStatus {
        ready: state.ready(),
        missing: state.missing.into_iter().map(requirement_to_wire).collect(),
        providers_connected: state.providers_connected,
        default_model: state.default_model.map(model),
        small_model: state.small_model.map(model),
        utility_model: state.utility_model.map(model),
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

pub(crate) fn request_from_wire(params: OnboardingRunParams) -> OnboardingRequest {
    OnboardingRequest {
        default_model: params.default_model.map(model_value),
        small_model: params.small_model.map(model_value),
        utility_model: params.utility_model.map(model_value),
    }
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
