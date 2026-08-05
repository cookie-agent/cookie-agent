use serde::Serialize;

use crate::{
    adapters::OvenAdapterFamily,
    recipes::{RecipeQuarantineReason, route_openai_model},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct WireAdapterMapping {
    pub provider_recipe_id: &'static str,
    pub adapter_recipe_id: &'static str,
    pub family: OvenAdapterFamily,
}

pub fn wire_adapter_for_recipe(
    provider_recipe_id: &str,
    model_id: &str,
) -> Result<WireAdapterMapping, RecipeQuarantineReason> {
    let mapping = match provider_recipe_id {
        "anthropic.messages.v1" => mapping(
            "anthropic.messages.v1",
            "oven.anthropic.messages",
            OvenAdapterFamily::Anthropic,
        ),
        "openai.responses.v1" => mapping(
            "openai.responses.v1",
            "oven.openai.responses",
            OvenAdapterFamily::OpenaiResponses,
        ),
        "openai.chat.v1" => mapping(
            "openai.chat.v1",
            "oven.openai.chat",
            OvenAdapterFamily::OpenaiChat,
        ),
        "openrouter.chat.v1"
        | "compatible.groq.v1"
        | "compatible.togetherai.v1"
        | "compatible.deepinfra.v1"
        | "compatible.fireworks.v1" => {
            let provider_recipe_id = match provider_recipe_id {
                "openrouter.chat.v1" => "openrouter.chat.v1",
                "compatible.groq.v1" => "compatible.groq.v1",
                "compatible.togetherai.v1" => "compatible.togetherai.v1",
                "compatible.deepinfra.v1" => "compatible.deepinfra.v1",
                "compatible.fireworks.v1" => "compatible.fireworks.v1",
                _ => unreachable!(),
            };
            mapping(
                provider_recipe_id,
                "oven.openai-compatible.chat",
                OvenAdapterFamily::OpenaiCompatible,
            )
        }
        "google.gemini.v1" => mapping(
            "google.gemini.v1",
            "oven.google.gemini.generate-content",
            OvenAdapterFamily::GoogleGemini,
        ),
        "cohere.chat.v2" if model_id == "north-mini-code-1-0" => mapping(
            "cohere.chat.v2",
            "oven.openai-compatible.chat",
            OvenAdapterFamily::OpenaiCompatible,
        ),
        "cohere.chat.v2" => mapping(
            "cohere.chat.v2",
            "oven.cohere.chat-v2",
            OvenAdapterFamily::CohereV2Chat,
        ),
        "google.vertex.gemini.v1" => mapping(
            "google.vertex.gemini.v1",
            "oven.google.vertex.generate-content",
            OvenAdapterFamily::GoogleVertexGemini,
        ),
        "amazon.bedrock.converse.v1" => mapping(
            "amazon.bedrock.converse.v1",
            "oven.bedrock.converse",
            OvenAdapterFamily::AwsBedrockConverse,
        ),
        "azure.openai.v1" => match route_openai_model(model_id)? {
            "responses" => mapping(
                "azure.openai.v1",
                "oven.azure.openai.responses",
                OvenAdapterFamily::AzureOpenaiResponses,
            ),
            "chat" => mapping(
                "azure.openai.v1",
                "oven.azure.openai.chat",
                OvenAdapterFamily::AzureOpenaiChat,
            ),
            _ => unreachable!(),
        },
        _ => return Err(RecipeQuarantineReason::NoReviewedProviderRecipe),
    };
    Ok(mapping)
}

#[must_use]
pub fn wire_adapter_for_protocol(adapter_recipe_id: &str) -> Option<OvenAdapterFamily> {
    super::families().find(|family| family.protocol_recipe() == adapter_recipe_id)
}

#[must_use]
pub const fn wire_adapter_for_custom(family: OvenAdapterFamily) -> WireAdapterMapping {
    mapping("custom.provider.v1", family.protocol_recipe(), family)
}

const fn mapping(
    provider_recipe_id: &'static str,
    adapter_recipe_id: &'static str,
    family: OvenAdapterFamily,
) -> WireAdapterMapping {
    WireAdapterMapping {
        provider_recipe_id,
        adapter_recipe_id,
        family,
    }
}
