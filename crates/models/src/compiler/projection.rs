use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;

use crate::{
    adapters::OvenAdapterFamily,
    authoring::{
        CancellationCapability, Modality, ModelCapabilities, ReplayCapability, RequestDefaults,
    },
    catalog::CatalogModelRecord,
};

const BROAD_VIDEO_MIME_TYPES: &[&str] = &[
    "video/mp4",
    "video/mpeg",
    "video/mpg",
    "video/mov",
    "video/quicktime",
    "video/avi",
    "video/x-msvideo",
    "video/x-flv",
    "video/webm",
    "video/wmv",
    "video/3gpp",
    "video/x-matroska",
];

// Keep catalog projection limited to families whose pinned Oven profiles accept video. Expand
// when Anthropic-compatible/MiniMax and OpenAI-compatible `video_url` profiles are available.
fn video_profile(family: OvenAdapterFamily) -> Option<(&'static [&'static str], u32)> {
    match family {
        OvenAdapterFamily::GoogleGemini | OvenAdapterFamily::GoogleVertexGemini => {
            Some((BROAD_VIDEO_MIME_TYPES, 2))
        }
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
        | OvenAdapterFamily::AnthropicCompatible
        | OvenAdapterFamily::OpenaiChat
        | OvenAdapterFamily::OpenaiResponses
        | OvenAdapterFamily::OpenaiCompatible
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
                || matches!(value.as_str(), "image" | "audio" | "pdf")
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
    let native_replay = if family == OvenAdapterFamily::AnthropicCompatible && model.reasoning {
        "required"
    } else {
        "unsupported"
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
