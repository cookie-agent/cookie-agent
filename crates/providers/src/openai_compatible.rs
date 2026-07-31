//! Configurable-base-URL Chat Completions provider for OpenAI-compatible APIs.

use crate::{
    ModelId, NormalizedEvent, Provider, ProviderCapabilities, ProviderError, ProviderRequest,
    openai::{OpenAiEndpoint, OpenAiProvider},
};
use async_trait::async_trait;
use reqwest::Client;

#[derive(Clone, Debug)]
pub struct OpenAiCompatibleProvider {
    inner: OpenAiProvider,
}

impl OpenAiCompatibleProvider {
    #[must_use]
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            inner: OpenAiProvider::with_base_url(api_key, base_url)
                .with_default_endpoint(OpenAiEndpoint::ChatCompletions),
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
                .with_default_endpoint(OpenAiEndpoint::ChatCompletions),
        }
    }
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn capabilities(&self, model: &ModelId) -> ProviderCapabilities {
        self.inner.capabilities(model)
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
