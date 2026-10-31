//! The forms in which session history records a model binding.

use cookie_agent_protocol as protocol;

/// The adapter of an ACP agent's synthetic model binding.
pub const ACP_ADAPTER: &str = "acp";

/// The model reference attempts made with `binding` record.
pub fn wire_resolved(binding: &protocol::FrozenModelBinding) -> protocol::ResolvedModelRef {
    let adapter_id = wire_adapter(binding.descriptor.adapter_id.as_str());
    protocol::ResolvedModelRef {
        selection: binding.selection.clone(),
        provider_id: binding.selection.model.provider_id(),
        model_id: binding.selection.model.model_id(),
        adapter_id,
        selection_fingerprint: binding.selection_fingerprint.clone(),
    }
}

/// The wire adapter ID of a frozen binding's protocol adapter `value`.
pub fn wire_adapter(value: &str) -> protocol::AdaptorId {
    use cookie_agent_models::adapters::OvenAdapterFamily as Oven;
    match adapter_family(value) {
        Oven::Anthropic | Oven::AnthropicCompatible => protocol::AdaptorId::ANTHROPIC,
        Oven::OpenaiChat => protocol::AdaptorId::OPENAI_CHAT,
        Oven::OpenaiResponses => protocol::AdaptorId::OPENAI_RESPONSES,
        Oven::OpenaiCompatible => protocol::AdaptorId::OPENAI_COMPATIBLE,
        Oven::GoogleGemini => protocol::AdaptorId::GOOGLE_GEMINI,
        Oven::GoogleVertexGemini => protocol::AdaptorId::GOOGLE_VERTEX_GEMINI,
        Oven::AwsBedrockConverse => protocol::AdaptorId::AWS_BEDROCK_CONVERSE,
        Oven::AzureOpenaiChat => protocol::AdaptorId::AZURE_OPENAI_CHAT,
        Oven::AzureOpenaiResponses => protocol::AdaptorId::AZURE_OPENAI_RESPONSES,
        Oven::CohereV2Chat => protocol::AdaptorId::COHERE_V2_CHAT,
    }
}

/// The adapter family of a frozen binding's protocol adapter `value`.
pub fn adapter_family(value: &str) -> cookie_agent_models::adapters::OvenAdapterFamily {
    cookie_agent_models::adapters::wire_adapter_for_protocol(value)
        .expect("frozen binding contains a reviewed protocol adapter")
}
