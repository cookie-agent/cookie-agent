//! Reviewed Oven adapter recipes used by Registry 1.

mod attribution;
mod cache;
mod capabilities;
mod endpoints;
mod headers;
pub(crate) mod no_auth_responses;
pub(crate) mod oven;

pub(crate) use attribution::reattribute;
pub use cache::{
    BedrockCachePoint, BedrockCacheStrategy, BedrockCacheTtl, BedrockMessageCachePoint,
    CacheStrategyConfig, GoogleCacheMode, GoogleCacheStrategyConfig, OpenAiCacheMode,
    OpenAiCacheStrategyConfig, OpenAiPromptCacheRetention, OpenAiPromptCacheTtl,
};
pub use capabilities::{AdapterCapabilityError, validate_capability_ceiling};
pub use endpoints::{
    BaseUrlOverridePolicy, EndpointBuildError, build_endpoint, custom_endpoint_policy,
    managed_base_url_policy, validate_custom_endpoint, validate_managed_base_url,
};
pub use headers::{StaticHeaderError, validate_static_headers};
pub use oven::{AnthropicCacheStrategyConfig, AnthropicCacheTtlConfig};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct WireAdapterSelection {
    pub family_id: &'static str,
    pub adapter_id: &'static str,
    pub family: OvenAdapterFamily,
}

#[must_use]
pub fn wire_adapter_for_protocol(adapter_id: &str) -> Option<OvenAdapterFamily> {
    families()
        .find(|family| family.protocol_recipe() == adapter_id)
        .or_else(|| {
            adapter_id
                .starts_with("oven.openai-compatible.chat.")
                .then_some(OvenAdapterFamily::OpenaiCompatible)
        })
        .or_else(|| {
            adapter_id
                .starts_with("oven.anthropic-compatible.messages.")
                .then_some(OvenAdapterFamily::AnthropicCompatible)
        })
}

#[must_use]
pub const fn wire_adapter_for_custom(family: OvenAdapterFamily) -> WireAdapterSelection {
    WireAdapterSelection {
        family_id: "custom",
        adapter_id: family.protocol_recipe(),
        family,
    }
}

/// Exact current Oven constructor family. Package presence does not add entries.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OvenAdapterFamily {
    Anthropic,
    AnthropicCompatible,
    OpenaiChat,
    OpenaiResponses,
    OpenaiCompatible,
    GoogleGemini,
    GoogleVertexGemini,
    AwsBedrockConverse,
    AzureOpenaiChat,
    AzureOpenaiResponses,
    CohereV2Chat,
}

impl OvenAdapterFamily {
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::AnthropicCompatible => "anthropic-compatible",
            Self::OpenaiChat => "openai-chat",
            Self::OpenaiResponses => "openai-responses",
            Self::OpenaiCompatible => "openai-compatible",
            Self::GoogleGemini => "google-gemini",
            Self::GoogleVertexGemini => "google-vertex-gemini",
            Self::AwsBedrockConverse => "aws-bedrock-converse",
            Self::AzureOpenaiChat => "azure-openai-chat",
            Self::AzureOpenaiResponses => "azure-openai-responses",
            Self::CohereV2Chat => "cohere-v2-chat",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        ALL.iter().copied().find(|family| family.id() == value)
    }

    #[must_use]
    pub const fn protocol_recipe(self) -> &'static str {
        match self {
            Self::Anthropic => "oven.anthropic.messages",
            Self::AnthropicCompatible => "oven.anthropic-compatible.messages",
            Self::OpenaiChat => "oven.openai.chat",
            Self::OpenaiResponses => "oven.openai.responses",
            Self::OpenaiCompatible => "oven.openai-compatible.chat",
            Self::GoogleGemini => "oven.google.gemini.generate-content",
            Self::GoogleVertexGemini => "oven.google.vertex.generate-content",
            Self::AwsBedrockConverse => "oven.bedrock.converse",
            Self::AzureOpenaiChat => "oven.azure.openai.chat",
            Self::AzureOpenaiResponses => "oven.azure.openai.responses",
            Self::CohereV2Chat => "oven.cohere.chat-v2",
        }
    }

    #[must_use]
    pub const fn allowed_auth_methods(self) -> &'static [&'static str] {
        match self {
            Self::OpenaiCompatible => &["bearer-api-key-v1", "api-key-header-v1", "no-auth-v1"],
            Self::OpenaiChat | Self::OpenaiResponses => &["bearer-api-key-v1", "no-auth-v1"],
            Self::Anthropic => &["anthropic-api-key-v1"],
            Self::AnthropicCompatible => {
                &["anthropic-api-key-v1", "bearer-api-key-v1", "no-auth-v1"]
            }
            Self::GoogleGemini => &["google-api-key-header-v1"],
            Self::GoogleVertexGemini => &["oauth-access-token-v1"],
            Self::AwsBedrockConverse => &["aws-sigv4-credentials-v1"],
            Self::AzureOpenaiChat | Self::AzureOpenaiResponses => &["azure-api-key-v1"],
            Self::CohereV2Chat => &["bearer-api-key-v1"],
        }
    }
}

const ALL: &[OvenAdapterFamily] = &[
    OvenAdapterFamily::Anthropic,
    OvenAdapterFamily::AnthropicCompatible,
    OvenAdapterFamily::OpenaiChat,
    OvenAdapterFamily::OpenaiResponses,
    OvenAdapterFamily::OpenaiCompatible,
    OvenAdapterFamily::GoogleGemini,
    OvenAdapterFamily::GoogleVertexGemini,
    OvenAdapterFamily::AwsBedrockConverse,
    OvenAdapterFamily::AzureOpenaiChat,
    OvenAdapterFamily::AzureOpenaiResponses,
    OvenAdapterFamily::CohereV2Chat,
];

const EMPTY_CUSTOM_SETUP: crate::recipes::SetupRecipe = crate::recipes::SetupRecipe {
    id: "custom-no-setup-v1",
    fields: &[],
};
const VERTEX_CUSTOM_FIELDS: &[crate::recipes::SetupFieldRecipe] = &[
    crate::recipes::SetupFieldRecipe {
        id: "location",
        value_type: crate::recipes::SetupFieldType::String,
        required: true,
        default: None,
        environment_alias: None,
    },
    crate::recipes::SetupFieldRecipe {
        id: "project",
        value_type: crate::recipes::SetupFieldType::String,
        required: true,
        default: None,
        environment_alias: None,
    },
    crate::recipes::SetupFieldRecipe {
        id: "resource",
        value_type: crate::recipes::SetupFieldType::String,
        required: false,
        default: Some("publishers/google"),
        environment_alias: None,
    },
];
const VERTEX_CUSTOM_SETUP: crate::recipes::SetupRecipe = crate::recipes::SetupRecipe {
    id: "custom-google-vertex-setup-v1",
    fields: VERTEX_CUSTOM_FIELDS,
};
const BEDROCK_CUSTOM_FIELDS: &[crate::recipes::SetupFieldRecipe] =
    &[crate::recipes::SetupFieldRecipe {
        id: "region",
        value_type: crate::recipes::SetupFieldType::String,
        required: true,
        default: None,
        environment_alias: None,
    }];
const BEDROCK_CUSTOM_SETUP: crate::recipes::SetupRecipe = crate::recipes::SetupRecipe {
    id: "custom-amazon-bedrock-setup-v1",
    fields: BEDROCK_CUSTOM_FIELDS,
};
const AZURE_CUSTOM_FIELDS: &[crate::recipes::SetupFieldRecipe] = &[
    crate::recipes::SetupFieldRecipe {
        id: "api_version",
        value_type: crate::recipes::SetupFieldType::String,
        required: true,
        default: None,
        environment_alias: None,
    },
    crate::recipes::SetupFieldRecipe {
        id: "deployment",
        value_type: crate::recipes::SetupFieldType::String,
        required: true,
        default: None,
        environment_alias: None,
    },
    crate::recipes::SetupFieldRecipe {
        id: "model",
        value_type: crate::recipes::SetupFieldType::String,
        required: false,
        default: None,
        environment_alias: None,
    },
    crate::recipes::SetupFieldRecipe {
        id: "version",
        value_type: crate::recipes::SetupFieldType::String,
        required: false,
        default: None,
        environment_alias: None,
    },
    crate::recipes::SetupFieldRecipe {
        id: "deployment_type",
        value_type: crate::recipes::SetupFieldType::String,
        required: false,
        default: None,
        environment_alias: None,
    },
];
const AZURE_CUSTOM_SETUP: crate::recipes::SetupRecipe = crate::recipes::SetupRecipe {
    id: "custom-azure-openai-setup-v1",
    fields: AZURE_CUSTOM_FIELDS,
};

#[must_use]
pub const fn custom_setup_recipe(
    family: OvenAdapterFamily,
) -> &'static crate::recipes::SetupRecipe {
    match family {
        OvenAdapterFamily::GoogleVertexGemini => &VERTEX_CUSTOM_SETUP,
        OvenAdapterFamily::AwsBedrockConverse => &BEDROCK_CUSTOM_SETUP,
        OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses => {
            &AZURE_CUSTOM_SETUP
        }
        OvenAdapterFamily::Anthropic
        | OvenAdapterFamily::AnthropicCompatible
        | OvenAdapterFamily::OpenaiChat
        | OvenAdapterFamily::OpenaiResponses
        | OvenAdapterFamily::OpenaiCompatible
        | OvenAdapterFamily::GoogleGemini
        | OvenAdapterFamily::CohereV2Chat => &EMPTY_CUSTOM_SETUP,
    }
}

pub fn families() -> impl ExactSizeIterator<Item = OvenAdapterFamily> {
    ALL.iter().copied()
}
