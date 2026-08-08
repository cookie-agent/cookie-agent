use crate::{
    adapters::OvenAdapterFamily,
    authoring::{CancellationCapability, MediaKind, Modality, ModelCapabilities, ReplayCapability},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AdapterCapabilityError {
    #[error("unsupported_model_capabilities")]
    Unsupported,
}

pub fn validate_capability_ceiling(
    family: OvenAdapterFamily,
    capabilities: &ModelCapabilities,
) -> Result<(), AdapterCapabilityError> {
    if capabilities
        .output
        .iter()
        .any(|value| *value != Modality::Text)
        || capabilities.cancellation != CancellationCapability::LocalOnly
        || capabilities.parallel_tool_calls && !capabilities.tool_calling
        || capabilities.context_tokens == 0
        || capabilities.output_tokens == 0
        || capabilities.output_tokens > capabilities.context_tokens
    {
        return Err(AdapterCapabilityError::Unsupported);
    }
    let replay_supported = matches!(
        family,
        OvenAdapterFamily::AnthropicCompatible
            | OvenAdapterFamily::OpenaiResponses
            | OvenAdapterFamily::AzureOpenaiResponses
    );
    if capabilities.native_replay != ReplayCapability::Unsupported && !replay_supported {
        return Err(AdapterCapabilityError::Unsupported);
    }
    for (kind, media) in &capabilities.media {
        let modality = match kind {
            MediaKind::Image => Modality::Image,
            MediaKind::Audio => Modality::Audio,
            MediaKind::Pdf => Modality::Pdf,
        };
        if !capabilities.input.contains(&modality)
            || media.mime_types.is_empty()
            || media.max_bytes == 0
            || media.max_count == 0
        {
            return Err(AdapterCapabilityError::Unsupported);
        }
    }
    if capabilities
        .input
        .iter()
        .filter(|modality| **modality != Modality::Text)
        .any(|modality| {
            let kind = match modality {
                Modality::Image => MediaKind::Image,
                Modality::Audio => MediaKind::Audio,
                Modality::Pdf => MediaKind::Pdf,
                Modality::Text => unreachable!(),
            };
            !capabilities.media.contains_key(&kind)
        })
    {
        return Err(AdapterCapabilityError::Unsupported);
    }
    Ok(())
}
