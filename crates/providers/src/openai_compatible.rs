//! Configurable-base-URL Chat Completions provider for OpenAI-compatible APIs.

use crate::{
    EncodedHistory, ModelId, NormalizedEvent, PersistedTurn, Provider, ProviderCapabilities,
    ProviderError, ProviderProtocol, ProviderRequest,
    openai::{OpenAiEndpoint, OpenAiProvider},
};
use async_trait::async_trait;
use reqwest::Client;

#[derive(Clone, Debug)]
pub struct OpenAiCompatibleProvider {
    inner: OpenAiProvider,
    probe: OpenAiCompatibleProbe,
}

/// Features demonstrated by an OpenAI-compatible endpoint. Advanced OpenAI
/// features stay false until a caller has independently probed them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenAiCompatibleProbe {
    pub chat_text: bool,
    pub tool_echo: bool,
    pub tool_result_pairing: bool,
    pub basic_sse: bool,
    pub rate_limit_429: bool,
}

impl OpenAiCompatibleProvider {
    #[must_use]
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            inner: OpenAiProvider::with_base_url(api_key, base_url)
                .with_default_endpoint(OpenAiEndpoint::ChatCompletions)
                .with_opaque_protocol(ProviderProtocol::OpenAiCompatible),
            probe: OpenAiCompatibleProbe {
                chat_text: true,
                tool_echo: true,
                tool_result_pairing: true,
                basic_sse: true,
                rate_limit_429: true,
            },
        }
    }

    #[must_use]
    pub fn with_client(
        client: Client,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            inner: OpenAiProvider::with_client(client, api_key, base_url)
                .with_default_endpoint(OpenAiEndpoint::ChatCompletions)
                .with_opaque_protocol(ProviderProtocol::OpenAiCompatible),
            probe: OpenAiCompatibleProbe {
                chat_text: true,
                tool_echo: true,
                tool_result_pairing: true,
                basic_sse: true,
                rate_limit_429: true,
            },
        }
    }

    #[must_use]
    pub fn with_capability_probe(mut self, probe: OpenAiCompatibleProbe) -> Self {
        self.probe = probe;
        self
    }

    #[must_use]
    pub const fn capability_probe(&self) -> OpenAiCompatibleProbe {
        self.probe
    }
}

/// Rebuilds compatible chat history; artifacts from a different endpoint are
/// explicitly discarded by the shared encoder.
#[must_use]
pub fn encode_history(turns: &[PersistedTurn]) -> EncodedHistory {
    crate::openai::encode_chat_history(turns, ProviderProtocol::OpenAiCompatible)
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn capabilities(&self, model: &ModelId) -> ProviderCapabilities {
        let _ = model;
        ProviderCapabilities {
            tool_calling: self.probe.tool_echo && self.probe.tool_result_pairing,
            parallel_tool_calls: false,
            streaming_tool_argument_deltas: false,
            reasoning_deltas: false,
            reasoning_replayable: false,
            image_input: false,
            pdf_input: false,
            structured_output: false,
            prompt_caching: false,
            context_limit: None,
            output_limit: None,
            usage_reporting: false,
            cancellation: crate::CancellationSemantics::DropStream,
        }
    }

    async fn stream(
        &self,
        request: ProviderRequest,
    ) -> Result<
        futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
        ProviderError,
    > {
        self.inner.stream(request).await
    }
}
