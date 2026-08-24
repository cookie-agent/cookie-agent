use std::sync::Arc;

use oven_sdk::{
    AbortSignal, BoxFuture, LanguageModel, LanguageModelDescriptor, ModelCapabilities, ModelError,
    ModelId, ModelIdentity, ProviderId, Request, StreamResponse,
};

use crate::{ConstructedAdapter, adapters::oven::ModelBuildError};

pub(crate) fn reattribute(
    compiled: ConstructedAdapter,
    provider_id: &str,
    model_id: &str,
    adapter_recipe_id: &str,
    capabilities: ModelCapabilities,
) -> Result<ConstructedAdapter, ModelBuildError> {
    let descriptor = LanguageModelDescriptor::new(
        ModelIdentity::new(
            ProviderId::new(provider_id.to_owned()),
            ModelId::new(model_id.to_owned()),
        )?,
        oven_sdk::AdapterId::new(adapter_recipe_id.to_owned()),
        capabilities,
    )?;
    Ok(ConstructedAdapter {
        model: Arc::new(ReattributedModel {
            inner: compiled.model,
            descriptor,
        }),
        provider_options: compiled.provider_options,
    })
}

struct ReattributedModel {
    inner: Arc<dyn LanguageModel>,
    descriptor: LanguageModelDescriptor,
}

impl LanguageModel for ReattributedModel {
    fn descriptor(&self) -> &LanguageModelDescriptor {
        &self.descriptor
    }

    fn validate_request(&self, request: &Request) -> Result<(), ModelError> {
        self.inner.validate_request(request)
    }

    fn supports_request(&self, request: &Request) -> bool {
        self.inner.supports_request(request)
    }

    fn stream<'a>(
        &'a self,
        request: Request,
        abort: AbortSignal,
    ) -> BoxFuture<'a, Result<StreamResponse, ModelError>> {
        self.inner.stream(request, abort)
    }
}
