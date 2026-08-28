use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;

use crate::{
    adapters::OvenAdapterFamily,
    authoring::{
        CancellationCapability, Modality, ModelCapabilities, ReplayCapability, RequestDefaults,
    },
    catalog::CatalogModelRecord,
};

const OPENAI_COMPATIBLE_VIDEO_MIME_TYPES: &[&str] = &["video/mp4"];
const ANTHROPIC_COMPATIBLE_VIDEO_MIME_TYPES: &[&str] = &[
    "video/mp4",
    "video/avi",
    "video/x-msvideo",
    "video/quicktime",
    "video/mov",
    "video/x-matroska",
];
const GEMINI_VIDEO_MIME_TYPES: &[&str] = &[
    "video/mp4",
    "video/mpeg",
    "video/mov",
    "video/avi",
    "video/x-flv",
    "video/mpg",
    "video/webm",
    "video/wmv",
    "video/3gpp",
];
const VERTEX_VIDEO_MIME_TYPES: &[&str] = &[
    "video/x-flv",
    "video/quicktime",
    "video/mpeg",
    "video/mpegs",
    "video/mpg",
    "video/mp4",
    "video/webm",
    "video/wmv",
    "video/3gpp",
];

// Keep catalog projection limited to families whose pinned Oven profiles
// implement each modality: only the Gemini/Vertex profiles accept audio input.
const fn audio_supported(family: OvenAdapterFamily) -> bool {
    matches!(
        family,
        OvenAdapterFamily::GoogleGemini | OvenAdapterFamily::GoogleVertexGemini
    )
}

// Keep catalog projection limited to families whose pinned Oven profiles accept video.
fn video_profile(family: OvenAdapterFamily) -> Option<(&'static [&'static str], u32)> {
    match family {
        OvenAdapterFamily::AnthropicCompatible => Some((ANTHROPIC_COMPATIBLE_VIDEO_MIME_TYPES, 2)),
        OvenAdapterFamily::OpenaiCompatible => Some((OPENAI_COMPATIBLE_VIDEO_MIME_TYPES, 2)),
        OvenAdapterFamily::GoogleGemini => Some((GEMINI_VIDEO_MIME_TYPES, 2)),
        OvenAdapterFamily::GoogleVertexGemini => Some((VERTEX_VIDEO_MIME_TYPES, 2)),
        OvenAdapterFamily::AwsBedrockConverse => Some((
            &[
                "video/x-matroska",
                "video/quicktime",
                "video/mp4",
                "video/webm",
                "video/x-flv",
                "video/mpeg",
                "video/mpg",
                "video/wmv",
                "video/3gpp",
            ],
            1,
        )),
        OvenAdapterFamily::Anthropic
        | OvenAdapterFamily::OpenaiChat
        | OvenAdapterFamily::OpenaiResponses
        | OvenAdapterFamily::AzureOpenaiChat
        | OvenAdapterFamily::AzureOpenaiResponses
        | OvenAdapterFamily::CohereV2Chat => None,
    }
}

pub(crate) fn capabilities_from_catalog(
    model: &CatalogModelRecord,
    family: OvenAdapterFamily,
) -> Result<ModelCapabilities, serde_json::Error> {
    let video_profile = video_profile(family);
    let input = model
        .modalities
        .input
        .iter()
        .filter(|value| {
            value.as_str() == "text"
                || matches!(value.as_str(), "image" | "pdf")
                || value.as_str() == "audio" && audio_supported(family)
                || value.as_str() == "video" && video_profile.is_some()
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let output = model
        .modalities
        .output
        .iter()
        .filter(|value| value.as_str() == "text")
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut media = BTreeMap::new();
    for modality in &input {
        let value = match modality.as_str() {
            "image" => Some((
                "image",
                ["image/jpeg", "image/png", "image/gif", "image/webp"].as_slice(),
                20 * 1024 * 1024_u64,
                20_u32,
            )),
            "audio" => Some((
                "audio",
                ["audio/mpeg", "audio/wav", "audio/ogg"].as_slice(),
                25 * 1024 * 1024_u64,
                5_u32,
            )),
            "pdf" => Some((
                "pdf",
                ["application/pdf"].as_slice(),
                32 * 1024 * 1024_u64,
                5_u32,
            )),
            "video" => video_profile.map(|(mime_types, max_count)| {
                ("video", mime_types, 25 * 1024 * 1024_u64, max_count)
            }),
            _ => None,
        };
        if let Some((kind, mime_types, max_bytes, max_count)) = value {
            media.insert(
                kind,
                json!({
                    "mime_types": mime_types,
                    "max_bytes": max_bytes,
                    "max_count": max_count
                }),
            );
        }
    }
    // Anthropic reasoning blocks must be replayed verbatim, so replay is
    // required. OpenAI/Azure Responses reasoning items can be replayed as
    // encrypted items but the adaptor accepts falling back, so replay is
    // optional; `oven_capabilities` derives `replay.reasoning` from this and
    // the pinned Responses profiles reject reasoning models without
    // reasoning-aware replay.
    let native_replay = if !model.reasoning {
        "unsupported"
    } else {
        match family {
            OvenAdapterFamily::AnthropicCompatible => "required",
            OvenAdapterFamily::OpenaiResponses | OvenAdapterFamily::AzureOpenaiResponses => {
                "optional"
            }
            _ => "unsupported",
        }
    };
    serde_json::from_value(json!({
        "input": input,
        "output": output,
        "context_tokens": model.limits.context,
        "output_tokens": model.limits.output,
        "tool_calling": model.tool_call,
        "parallel_tool_calls": false,
        "structured_output": model.structured_output.unwrap_or(false),
        "reasoning": model.reasoning,
        "temperature": model.temperature.unwrap_or(false),
        "top_p": false,
        "seed": false,
        "compaction": "unsupported",
        "native_replay": native_replay,
        "cancellation": "local_only",
        "media": media
    }))
}

pub(crate) fn managed_defaults(model: &CatalogModelRecord) -> RequestDefaults {
    RequestDefaults {
        max_output_tokens: Some(model.limits.output.min(16_384)),
        ..RequestDefaults::default()
    }
}

pub(crate) fn validate_capability_shape(capabilities: &ModelCapabilities) -> bool {
    !capabilities.input.is_empty()
        && capabilities.input.contains(&Modality::Text)
        && capabilities.output == BTreeSet::from([Modality::Text])
        && capabilities.context_tokens > 0
        && capabilities.output_tokens > 0
        && capabilities.output_tokens <= capabilities.context_tokens
        && (!capabilities.parallel_tool_calls || capabilities.tool_calling)
        && matches!(
            capabilities.compaction,
            crate::CompactionCapability::Unsupported | crate::CompactionCapability::Native
        )
        && capabilities.cancellation == CancellationCapability::LocalOnly
        && matches!(
            capabilities.native_replay,
            ReplayCapability::Unsupported | ReplayCapability::Optional | ReplayCapability::Required
        )
}

pub(crate) fn validate_defaults(
    defaults: &RequestDefaults,
    capabilities: &ModelCapabilities,
) -> bool {
    defaults
        .temperature
        .is_none_or(|value| capabilities.temperature && (0.0..=2.0).contains(&value.get()))
        && defaults
            .top_p
            .is_none_or(|value| capabilities.top_p && (0.0..=1.0).contains(&value.get()))
        && defaults.seed.is_none_or(|_| capabilities.seed)
        && defaults
            .max_output_tokens
            .is_none_or(|value| value > 0 && value <= capabilities.output_tokens)
        && defaults
            .tool_choice
            .as_ref()
            .is_none_or(|_| capabilities.tool_calling)
        && defaults.stop.len() <= 8
        && defaults.stop.iter().all(|value| {
            !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authoring::MediaKind;
    use crate::catalog::{
        CatalogLimits, CatalogModalities, CatalogModelRecord, CatalogModelStatus,
    };

    fn catalog_model(reasoning: bool) -> CatalogModelRecord {
        CatalogModelRecord {
            id: cookie_agent_identity::ProviderModelId::new("gpt-test".to_owned()).unwrap(),
            name: "GPT Test".to_owned(),
            description: String::new(),
            family: None,
            attachment: false,
            reasoning,
            tool_call: true,
            structured_output: Some(true),
            temperature: Some(false),
            open_weights: false,
            status: CatalogModelStatus::Stable,
            release_date: String::new(),
            last_updated: String::new(),
            modalities: CatalogModalities {
                input: vec!["text".to_owned()],
                output: vec!["text".to_owned()],
            },
            limits: CatalogLimits {
                context: 272_000,
                input: None,
                output: 128_000,
            },
            shape: None,
            provider: None,
            reasoning_options: Vec::new(),
            cost: None,
            interleaved: None,
            canonical_provenance: None,
        }
    }

    #[test]
    fn reasoning_catalog_models_project_reasoning_aware_replay() {
        let model = catalog_model(true);
        // The pinned Responses profiles reject reasoning models without
        // reasoning-aware native replay (`replay.reasoning` derives from this).
        for family in [
            OvenAdapterFamily::OpenaiResponses,
            OvenAdapterFamily::AzureOpenaiResponses,
        ] {
            let capabilities = capabilities_from_catalog(&model, family).unwrap();
            assert_eq!(
                capabilities.native_replay,
                ReplayCapability::Optional,
                "{family:?}"
            );
        }
        let capabilities =
            capabilities_from_catalog(&model, OvenAdapterFamily::AnthropicCompatible).unwrap();
        assert_eq!(capabilities.native_replay, ReplayCapability::Required);
        for family in [
            OvenAdapterFamily::OpenaiChat,
            OvenAdapterFamily::OpenaiCompatible,
            OvenAdapterFamily::GoogleGemini,
        ] {
            let capabilities = capabilities_from_catalog(&model, family).unwrap();
            assert_eq!(
                capabilities.native_replay,
                ReplayCapability::Unsupported,
                "{family:?}"
            );
        }
        // Non-reasoning models never project native replay.
        let capabilities =
            capabilities_from_catalog(&catalog_model(false), OvenAdapterFamily::OpenaiResponses)
                .unwrap();
        assert_eq!(capabilities.native_replay, ReplayCapability::Unsupported);
    }

    #[test]
    fn audio_catalog_input_projects_only_for_gemini_families() {
        let mut model = catalog_model(false);
        model.modalities.input = vec!["text".to_owned(), "audio".to_owned()];
        for family in [
            OvenAdapterFamily::GoogleGemini,
            OvenAdapterFamily::GoogleVertexGemini,
        ] {
            let capabilities = capabilities_from_catalog(&model, family).unwrap();
            assert!(capabilities.input.contains(&Modality::Audio), "{family:?}");
            assert!(
                capabilities.media.contains_key(&MediaKind::Audio),
                "{family:?}"
            );
        }
        // The pinned OpenAI/Azure/Anthropic/Bedrock/Cohere profiles reject
        // audio declarations; projection must drop the modality instead.
        for family in [
            OvenAdapterFamily::OpenaiChat,
            OvenAdapterFamily::OpenaiResponses,
            OvenAdapterFamily::AzureOpenaiChat,
            OvenAdapterFamily::AzureOpenaiResponses,
            OvenAdapterFamily::OpenaiCompatible,
            OvenAdapterFamily::AnthropicCompatible,
            OvenAdapterFamily::AwsBedrockConverse,
        ] {
            let capabilities = capabilities_from_catalog(&model, family).unwrap();
            assert!(!capabilities.input.contains(&Modality::Audio), "{family:?}");
        }
    }

    #[test]
    fn video_profiles_match_pinned_oven_declarations() {
        assert_eq!(video_profile(OvenAdapterFamily::Anthropic), None);
        for (family, expected) in [
            (
                OvenAdapterFamily::OpenaiCompatible,
                OPENAI_COMPATIBLE_VIDEO_MIME_TYPES,
            ),
            (
                OvenAdapterFamily::AnthropicCompatible,
                ANTHROPIC_COMPATIBLE_VIDEO_MIME_TYPES,
            ),
            (OvenAdapterFamily::GoogleGemini, GEMINI_VIDEO_MIME_TYPES),
            (
                OvenAdapterFamily::GoogleVertexGemini,
                VERTEX_VIDEO_MIME_TYPES,
            ),
        ] {
            assert_eq!(video_profile(family), Some((expected, 2)), "{family:?}");
        }
    }
}
