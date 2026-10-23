//! Onboarding as clients see it: the engine's state and kit plans on the
//! wire, and the wire's run request for the engine.

use base64::Engine as _;
use cookie_agent_engine::{
    EngineError, KitFileAction, KitPreview, KitSource, OnboardingOutcome, OnboardingRequest,
    OnboardingRequirement, OnboardingState,
};
use cookie_agent_protocol::{
    MAX_ONBOARDING_ARCHIVE_BYTES, OnboardingFileAction, OnboardingKitFile, OnboardingKitSource,
    OnboardingModel, OnboardingModelInput, OnboardingPreviewResult, OnboardingRunParams,
    OnboardingRunResult, OnboardingStatus,
};

pub(crate) fn requirement_to_wire(
    requirement: OnboardingRequirement,
) -> cookie_agent_protocol::OnboardingRequirement {
    match requirement {
        OnboardingRequirement::Provider => cookie_agent_protocol::OnboardingRequirement::Provider,
        OnboardingRequirement::DefaultModel => {
            cookie_agent_protocol::OnboardingRequirement::DefaultModel
        }
        OnboardingRequirement::SmallModel => {
            cookie_agent_protocol::OnboardingRequirement::SmallModel
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
    }
}

pub(crate) fn source_from_wire(
    source: Option<OnboardingKitSource>,
) -> Result<KitSource, EngineError> {
    match source.unwrap_or_default() {
        OnboardingKitSource::Release => Ok(KitSource::Release),
        OnboardingKitSource::Archive { data } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .map_err(|_| {
                    EngineError::OnboardingKit(cookie_agent_engine::KitError::Archive(
                        "the archive is not standard base64".into(),
                    ))
                })?;
            if bytes.len() > MAX_ONBOARDING_ARCHIVE_BYTES {
                return Err(EngineError::OnboardingKit(
                    cookie_agent_engine::KitError::TooLarge,
                ));
            }
            Ok(KitSource::Archive(bytes))
        }
    }
}

pub(crate) fn preview_to_wire(preview: KitPreview) -> OnboardingPreviewResult {
    OnboardingPreviewResult {
        kit_version: preview.manifest.kit_version,
        cookie_version: preview.manifest.cookie_version,
        origin: preview.origin,
        files: preview
            .files
            .into_iter()
            .map(|file| OnboardingKitFile {
                path: file.path.as_str().to_owned(),
                action: match file.action {
                    KitFileAction::Write => OnboardingFileAction::Write,
                    KitFileAction::Skip => OnboardingFileAction::Skip,
                    KitFileAction::WriteNew => OnboardingFileAction::WriteNew,
                },
                diff: file.diff,
            })
            .collect(),
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

pub(crate) fn request_from_wire(
    params: OnboardingRunParams,
) -> Result<OnboardingRequest, EngineError> {
    Ok(OnboardingRequest {
        install_kit: params.install_kit,
        source: if params.install_kit {
            source_from_wire(params.source)?
        } else {
            KitSource::Release
        },
        default_model: params.default_model.map(model_value),
        small_model: params.small_model.map(model_value),
    })
}

pub(crate) fn outcome_to_wire(outcome: OnboardingOutcome) -> OnboardingRunResult {
    OnboardingRunResult {
        written: outcome.written,
        skipped: outcome.skipped,
        written_new: outcome.written_new,
        settings: outcome.settings,
        status: status_to_wire(outcome.state),
    }
}
