//! Current-only dynamic provider/model facade.

pub struct ConstructedAdapter {
    pub model: std::sync::Arc<dyn oven_sdk::LanguageModel>,
    pub provider_options: oven_sdk::ProviderOptions,
}

mod model_types;

pub mod adapters;
pub mod authoring;
#[path = "catalog/mod.rs"]
pub mod catalog;
pub mod compiler;
#[path = "manager/mod.rs"]
pub mod manager;
pub mod manifests;
pub mod provider_store;
pub mod recipes;
pub mod secure_store;

#[cfg(any(test, feature = "test-support"))]
mod test_support;

pub use authoring::{
    AnthropicCacheConfig, AuthDefinition, BedrockCacheConfig, CacheTtl, HeaderName,
    ModelsDevProvider, OpenAiCacheConfig, OpenAiCacheMode, OpenAiCompatibleCacheConfig,
    OpenAiPromptCacheRetention, OpenAiPromptCacheTtl, ProviderCacheConfig, ProviderDefinition,
    RollingCacheTtl, SecretString,
};
pub use authoring::{
    AuthOverride, BoundedSetupString, ConfigSetupValue, CustomModelDefinition, CustomProvider,
    EndpointUrl, ManagedModelOverride, ManagedModelShape, PartialRequestDefaults,
    SafeAuthParameterValue, SafeSetupValue, SafeStaticHeaderValue,
};
pub use manager::{
    CompiledModelRuntime, CompiledProviderState, CompiledRuntimeModel, EffectiveCredentialSource,
    ModelManager, ModelManagerError, ModelMutationResult, ProviderConnectRequest,
    ProviderDisconnectRequest, ProviderPresence, ResolvedExecutableModel, RuntimeProviderSource,
    safe_definition_fingerprint,
};
pub use model_types::*;
#[cfg(any(test, feature = "test-support"))]
pub use test_support::{ScriptedModel, ScriptedStep};
