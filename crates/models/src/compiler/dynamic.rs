use std::collections::{BTreeMap, BTreeSet};

use cookie_agent_identity::{ProviderId, ProviderModelId};
use serde::Serialize;

use crate::{
    ModelCapabilities, ProviderOptions, Sha256Digest,
    adapters::{
        OvenAdapterFamily, build_endpoint, custom_setup_recipe, validate_capability_ceiling,
        validate_custom_endpoint, validate_managed_base_url, validate_static_headers,
        wire_adapter_for_custom, wire_adapter_for_recipe,
    },
    authoring::{
        AuthDefinition, CustomProvider, ManagedModelOverride, ModelsDevProvider,
        PartialRequestDefaults, RequestDefaults,
    },
    catalog::{CatalogModelRecord, CatalogProviderRecord},
    compiler::{
        fingerprint::fingerprint,
        projection::{
            capabilities_from_catalog, managed_defaults, validate_capability_shape,
            validate_defaults,
        },
        variants::{CompiledVariant, custom_variants, managed_variants},
    },
    recipes::{
        COMPILER_VERSION, CatalogModelClaimInput, CatalogProviderClaimInput, EndpointPolicy,
        ModelRecipeMatch, ProviderRecipe, ProviderRecipeMatch, RecipeQuarantineReason,
        RecipeRegistry, SetupRecipe, ValidatedSetup, auth_method, registry1,
        validate_auth_definition, validate_auth_override, validate_setup,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthSourceCategory {
    AuthoredApiKey,
    AuthoredOverride,
    AuthoredCustom,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompiledAuthShape {
    pub method: String,
    pub safe_parameters: BTreeMap<String, String>,
    pub credential_fields: Vec<String>,
    pub owned_headers: Vec<String>,
    pub source: AuthSourceCategory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompiledModelStatus {
    Available,
    CredentialsUnavailable,
    SetupUnavailable,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompiledDynamicModel {
    pub id: ProviderModelId,
    pub display_name: String,
    pub provider_recipe: String,
    pub protocol_recipe: String,
    pub adapter: OvenAdapterFamily,
    pub endpoint: Option<String>,
    pub setup: Option<ValidatedSetup>,
    pub auth: CompiledAuthShape,
    pub capabilities: ModelCapabilities,
    pub defaults: RequestDefaults,
    pub options: ProviderOptions,
    pub variants: BTreeMap<cookie_agent_identity::VariantId, CompiledVariant>,
    pub default_variant: Option<cookie_agent_identity::VariantId>,
    pub status: CompiledModelStatus,
    pub behavior_fingerprint: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelQuarantine {
    pub id: ProviderModelId,
    pub reason: RecipeQuarantineReason,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompiledDynamicProvider {
    pub id: ProviderId,
    pub models: BTreeMap<ProviderModelId, CompiledDynamicModel>,
    pub quarantined_models: Vec<ModelQuarantine>,
    pub provider_quarantine: Option<RecipeQuarantineReason>,
    pub fingerprint: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DynamicCompileError {
    #[error("no_reviewed_provider_recipe")]
    UnsupportedProvider,
    #[error("catalog_provider_claim_drift: {0:?}")]
    ProviderQuarantined(RecipeQuarantineReason),
    #[error("unknown_model_override")]
    UnknownModelOverride,
    #[error("invalid_setup")]
    Setup,
    #[error("invalid_endpoint")]
    Endpoint,
    #[error("invalid_auth")]
    Auth,
    #[error("authored_base_url_requires_auth")]
    BaseUrlWithoutAuth,
    #[error("unsupported_adaptor")]
    UnsupportedAdapter,
    #[error("invalid_static_headers")]
    StaticHeaders,
    #[error("invalid_custom_model")]
    CustomModel,
    #[error("invalid_variant")]
    Variant,
}

#[derive(Clone, Copy, Debug)]
pub struct DynamicCompiler {
    registry: RecipeRegistry,
}

impl Default for DynamicCompiler {
    fn default() -> Self {
        Self::registry1()
    }
}

impl DynamicCompiler {
    #[must_use]
    pub const fn registry1() -> Self {
        Self {
            registry: registry1(),
        }
    }

    pub fn compile_managed(
        &self,
        catalog_revision: &str,
        record: &CatalogProviderRecord,
        authored: Option<&ModelsDevProvider>,
    ) -> Result<CompiledDynamicProvider, DynamicCompileError> {
        let input = CatalogProviderClaimInput::from_record(record);
        match self.registry.match_provider(&input) {
            ProviderRecipeMatch::Unsupported(_) => {
                return Err(DynamicCompileError::UnsupportedProvider);
            }
            ProviderRecipeMatch::Quarantined(reason) => {
                return Ok(quarantined_provider(record.id.clone(), reason));
            }
            ProviderRecipeMatch::Supported(_) => {}
        }
        if let Some(authored) = authored {
            let recipe = self
                .registry
                .provider_recipes(record.id.as_str())
                .into_iter()
                .next()
                .ok_or(DynamicCompileError::UnsupportedProvider)?;
            validate_managed_base_url(recipe.endpoint, authored.base_url.as_ref())
                .map_err(|_| DynamicCompileError::Endpoint)?;
        }
        if authored.is_some_and(|value| {
            value.base_url.is_some() && value.api_key.is_none() && value.auth_override.is_none()
        }) {
            return Err(DynamicCompileError::BaseUrlWithoutAuth);
        }
        if let Some(authored) = authored {
            for id in authored.model_overrides.keys() {
                if !record.models.contains_key(id) {
                    return Err(DynamicCompileError::UnknownModelOverride);
                }
            }
        }
        let mut models = BTreeMap::new();
        let mut quarantined_models = Vec::new();
        for (table_id, entry) in &record.models {
            let Some(model) = entry.record.as_ref() else {
                continue;
            };
            let claim = CatalogModelClaimInput::from_record(table_id.as_str(), model);
            let recipe = match self.registry.match_model(record.id.as_str(), &claim) {
                ModelRecipeMatch::Supported(recipe) => recipe,
                ModelRecipeMatch::Quarantined(reason) => {
                    quarantined_models.push(ModelQuarantine {
                        id: table_id.clone(),
                        reason,
                    });
                    continue;
                }
                ModelRecipeMatch::Omitted => continue,
            };
            let override_ = authored.and_then(|value| value.model_overrides.get(table_id));
            if override_.and_then(|value| value.enabled) == Some(false) {
                continue;
            }
            match self.compile_managed_model(
                catalog_revision,
                &record.id,
                model,
                recipe,
                authored,
                override_,
            ) {
                Ok(compiled) => {
                    models.insert(table_id.clone(), compiled);
                }
                Err(ModelLocalError::Quarantine(reason)) => {
                    quarantined_models.push(ModelQuarantine {
                        id: table_id.clone(),
                        reason,
                    });
                }
                Err(ModelLocalError::Provider(error)) => return Err(error),
            }
        }
        quarantined_models.sort_by(|left, right| left.id.cmp(&right.id));
        let provider_fingerprint = fingerprint(
            "cookie-agent/dynamic-provider/v1",
            &(
                self.registry.revision(),
                COMPILER_VERSION,
                catalog_revision,
                &record.id,
                models
                    .iter()
                    .map(|(id, model)| (id, &model.behavior_fingerprint))
                    .collect::<Vec<_>>(),
                &quarantined_models,
            ),
        );
        Ok(CompiledDynamicProvider {
            id: record.id.clone(),
            models,
            quarantined_models,
            provider_quarantine: None,
            fingerprint: provider_fingerprint,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_managed_model(
        &self,
        catalog_revision: &str,
        provider_id: &ProviderId,
        model: &CatalogModelRecord,
        recipe: &'static ProviderRecipe,
        authored: Option<&ModelsDevProvider>,
        override_: Option<&ManagedModelOverride>,
    ) -> Result<CompiledDynamicModel, ModelLocalError> {
        let wire = wire_adapter_for_recipe(recipe.id, model.id.as_str())
            .map_err(ModelLocalError::Quarantine)?;
        let adapter = wire.family;
        let protocol_recipe = wire.adapter_recipe_id;
        let capabilities = capabilities_from_catalog(model, adapter).map_err(|_| {
            ModelLocalError::Quarantine(RecipeQuarantineReason::UnsupportedModelCapabilities)
        })?;
        if !validate_capability_shape(&capabilities)
            || validate_capability_ceiling(adapter, &capabilities).is_err()
        {
            return Err(ModelLocalError::Quarantine(
                RecipeQuarantineReason::UnsupportedModelCapabilities,
            ));
        }
        let setup = resolved_managed_setup(recipe.setup, authored)?;
        let endpoint_policy =
            if provider_id.as_str() == "cohere" && model.id.as_str() == "north-mini-code-1-0" {
                EndpointPolicy::DefaultWithAuthoredHttpsOverride {
                    default: "https://api.cohere.ai/compatibility/v1",
                }
            } else {
                recipe.endpoint
            };
        let endpoint = match &setup {
            Some(setup) => Some(
                build_endpoint(
                    endpoint_policy,
                    authored.and_then(|value| value.base_url.as_ref()),
                    setup,
                )
                .map_err(|_| ModelLocalError::Provider(DynamicCompileError::Endpoint))?,
            ),
            None => match endpoint_policy {
                EndpointPolicy::DefaultWithAuthoredHttpsOverride { default } => Some(
                    authored
                        .and_then(|value| value.base_url.as_ref())
                        .map(crate::authoring::EndpointUrl::as_str)
                        .unwrap_or(default)
                        .trim_end_matches('/')
                        .to_owned(),
                ),
                EndpointPolicy::VertexPublisher
                | EndpointPolicy::BedrockRegional
                | EndpointPolicy::AzureOpenai => None,
            },
        };
        let auth = managed_auth(recipe, authored)?;
        let mut defaults = managed_defaults(model);
        if let Some(override_) = override_ {
            apply_partial_defaults(&mut defaults, &override_.defaults);
        }
        if !validate_defaults(&defaults, &capabilities) {
            return Err(ModelLocalError::Quarantine(
                RecipeQuarantineReason::UnsupportedModelCapabilities,
            ));
        }
        let (variants, default_variant) =
            managed_variants(&model.reasoning_options, override_, adapter).map_err(|_| {
                ModelLocalError::Quarantine(RecipeQuarantineReason::UnsupportedProtocolFeature)
            })?;
        if variants
            .values()
            .any(|variant| !validate_defaults(&variant.defaults, &capabilities))
        {
            return Err(ModelLocalError::Provider(DynamicCompileError::Variant));
        }
        let display_name = override_
            .and_then(|value| value.display_name.clone())
            .unwrap_or_else(|| model.name.clone());
        let status = if setup.is_none() {
            CompiledModelStatus::SetupUnavailable
        } else if auth.source == AuthSourceCategory::Unavailable {
            CompiledModelStatus::CredentialsUnavailable
        } else {
            CompiledModelStatus::Available
        };
        let options = ProviderOptions::default();
        let behavior_fingerprint = fingerprint(
            "cookie-agent/dynamic-model-behavior/v1",
            &(
                (
                    self.registry.revision(),
                    COMPILER_VERSION,
                    catalog_revision,
                    provider_id,
                    &model.id,
                    recipe.id,
                    protocol_recipe,
                    adapter,
                ),
                &endpoint,
                &setup,
                &auth,
                &capabilities,
                &defaults,
                &options,
                &variants,
                &default_variant,
                "managed_catalog",
            ),
        );
        Ok(CompiledDynamicModel {
            id: model.id.clone(),
            display_name,
            provider_recipe: recipe.id.to_owned(),
            protocol_recipe: protocol_recipe.to_owned(),
            adapter,
            endpoint,
            setup,
            auth,
            capabilities,
            defaults,
            options,
            variants,
            default_variant,
            status,
            behavior_fingerprint,
        })
    }

    pub fn compile_custom(
        &self,
        provider_id: &ProviderId,
        provider: &CustomProvider,
    ) -> Result<CompiledDynamicProvider, DynamicCompileError> {
        if !provider_id.as_str().starts_with("custom.") {
            return Err(DynamicCompileError::UnsupportedProvider);
        }
        let adapter = OvenAdapterFamily::parse(provider.adaptor.as_str())
            .ok_or(DynamicCompileError::UnsupportedAdapter)?;
        let wire = wire_adapter_for_custom(adapter);
        validate_custom_endpoint(adapter, &provider.endpoint)
            .map_err(|_| DynamicCompileError::Endpoint)?;
        let setup_recipe = custom_setup_recipe(adapter);
        let setup = validate_setup(setup_recipe, &provider.setup)
            .map_err(|_| DynamicCompileError::Setup)?;
        let auth_method = validate_auth_definition(&provider.auth, adapter.allowed_auth_methods())
            .map_err(|_| DynamicCompileError::Auth)?;
        validate_static_headers(adapter, &provider.auth, &provider.headers)
            .map_err(|_| DynamicCompileError::StaticHeaders)?;
        let auth = custom_auth_shape(&provider.auth, auth_method);
        let mut models = BTreeMap::new();
        for (id, model) in &provider.models {
            if !validate_capability_shape(&model.capabilities)
                || validate_capability_ceiling(adapter, &model.capabilities).is_err()
                || !validate_defaults(&model.defaults, &model.capabilities)
                || !validate_custom_options(&model.options, adapter)
                || !validate_no_auth_profile(&auth, adapter, &model.capabilities, &model.options)
            {
                return Err(DynamicCompileError::CustomModel);
            }
            let (variants, default_variant) =
                custom_variants(&model.variants, model.default_variant.as_ref())
                    .map_err(|_| DynamicCompileError::Variant)?;
            if variants.values().any(|variant| {
                !validate_defaults(&variant.defaults, &model.capabilities)
                    || variant.reasoning.is_some() && !model.capabilities.reasoning
                    || !validate_custom_options(&variant.options, adapter)
                    || !reasoning_supported(variant.reasoning.as_ref(), adapter)
            }) {
                return Err(DynamicCompileError::Variant);
            }
            if !model.enabled {
                continue;
            }
            let endpoint = provider.endpoint.as_str().trim_end_matches('/').to_owned();
            let options = model.options.clone();
            let safe_headers = provider
                .headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect::<Vec<_>>();
            let behavior_fingerprint = fingerprint(
                "cookie-agent/custom-model-behavior/v1",
                &(
                    self.registry.revision(),
                    COMPILER_VERSION,
                    provider_id,
                    id,
                    wire.adapter_recipe_id,
                    adapter,
                    &endpoint,
                    &setup,
                    &auth,
                    &safe_headers,
                    &model.capabilities,
                    &model.defaults,
                    &options,
                    &variants,
                    &default_variant,
                    "custom_authored",
                ),
            );
            models.insert(
                id.clone(),
                CompiledDynamicModel {
                    id: id.clone(),
                    display_name: model.display_name.clone(),
                    provider_recipe: wire.provider_recipe_id.into(),
                    protocol_recipe: wire.adapter_recipe_id.into(),
                    adapter,
                    endpoint: Some(endpoint),
                    setup: Some(setup.clone()),
                    auth: auth.clone(),
                    capabilities: model.capabilities.clone(),
                    defaults: model.defaults.clone(),
                    options,
                    variants,
                    default_variant,
                    status: CompiledModelStatus::Available,
                    behavior_fingerprint,
                },
            );
        }
        let safe_headers = provider
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        let provider_fingerprint = fingerprint(
            "cookie-agent/custom-provider/v1",
            &(
                self.registry.revision(),
                COMPILER_VERSION,
                provider_id,
                provider.endpoint.as_str(),
                adapter,
                &setup,
                &auth,
                &safe_headers,
                models
                    .iter()
                    .map(|(id, model)| (id, &model.behavior_fingerprint))
                    .collect::<Vec<_>>(),
            ),
        );
        Ok(CompiledDynamicProvider {
            id: provider_id.clone(),
            models,
            quarantined_models: Vec::new(),
            provider_quarantine: None,
            fingerprint: provider_fingerprint,
        })
    }
}

enum ModelLocalError {
    Quarantine(RecipeQuarantineReason),
    Provider(DynamicCompileError),
}

fn quarantined_provider(id: ProviderId, reason: RecipeQuarantineReason) -> CompiledDynamicProvider {
    let provider_fingerprint = fingerprint("cookie-agent/quarantined-provider/v1", &(&id, reason));
    CompiledDynamicProvider {
        id,
        models: BTreeMap::new(),
        quarantined_models: Vec::new(),
        provider_quarantine: Some(reason),
        fingerprint: provider_fingerprint,
    }
}

fn resolved_managed_setup(
    recipe: &'static SetupRecipe,
    authored: Option<&ModelsDevProvider>,
) -> Result<Option<ValidatedSetup>, ModelLocalError> {
    let input = authored.map(|value| &value.setup);
    let required = recipe.fields.iter().any(|field| field.required);
    if input.is_none_or(BTreeMap::is_empty) && required {
        return Ok(None);
    }
    validate_setup(recipe, input.unwrap_or(&BTreeMap::new()))
        .map(Some)
        .map_err(|_| ModelLocalError::Provider(DynamicCompileError::Setup))
}

fn managed_auth(
    recipe: &ProviderRecipe,
    authored: Option<&ModelsDevProvider>,
) -> Result<CompiledAuthShape, ModelLocalError> {
    if let Some(authored) = authored {
        if authored.api_key.is_some() {
            let method = auth_method(recipe.default_auth_method)
                .ok_or(ModelLocalError::Provider(DynamicCompileError::Auth))?;
            let required_api_key = method.credentials.len() == 1
                && method.credentials[0].required
                && method.credentials[0].name == "api_key";
            if !required_api_key {
                return Err(ModelLocalError::Provider(DynamicCompileError::Auth));
            }
            return Ok(auth_shape(
                method,
                BTreeMap::new(),
                AuthSourceCategory::AuthoredApiKey,
            ));
        }
        if let Some(auth) = &authored.auth_override {
            let method = validate_auth_override(auth, recipe.allowed_auth_methods)
                .map_err(|_| ModelLocalError::Provider(DynamicCompileError::Auth))?;
            return Ok(auth_shape(
                method,
                BTreeMap::new(),
                AuthSourceCategory::AuthoredOverride,
            ));
        }
    }
    let method = auth_method(recipe.default_auth_method)
        .ok_or(ModelLocalError::Provider(DynamicCompileError::Auth))?;
    Ok(auth_shape(
        method,
        BTreeMap::new(),
        AuthSourceCategory::Unavailable,
    ))
}

fn custom_auth_shape(
    auth: &AuthDefinition,
    method: &'static crate::recipes::AuthMethodRecipe,
) -> CompiledAuthShape {
    let parameters = auth
        .parameters
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.as_str().to_owned()))
        .collect();
    auth_shape(method, parameters, AuthSourceCategory::AuthoredCustom)
}

fn auth_shape(
    method: &'static crate::recipes::AuthMethodRecipe,
    safe_parameters: BTreeMap<String, String>,
    source: AuthSourceCategory,
) -> CompiledAuthShape {
    let credential_fields = method
        .credentials
        .iter()
        .map(|field| field.name.to_owned())
        .collect();
    let mut owned_headers = method
        .owned_headers
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<BTreeSet<_>>();
    if method.id == "api-key-header-v1"
        && let Some(header) = safe_parameters.get("header_name")
    {
        owned_headers.insert(header.clone());
    }
    CompiledAuthShape {
        method: method.id.to_owned(),
        safe_parameters,
        credential_fields,
        owned_headers: owned_headers.into_iter().collect(),
        source,
    }
}

fn apply_partial_defaults(base: &mut RequestDefaults, overlay: &PartialRequestDefaults) {
    base.temperature = overlay.temperature.or(base.temperature);
    base.top_p = overlay.top_p.or(base.top_p);
    base.max_output_tokens = overlay.max_output_tokens.or(base.max_output_tokens);
    if let Some(stop) = &overlay.stop {
        base.stop.clone_from(stop);
    }
    base.seed = overlay.seed.or(base.seed);
    base.tool_choice = overlay
        .tool_choice
        .clone()
        .or_else(|| base.tool_choice.clone());
}

fn validate_custom_options(options: &ProviderOptions, adapter: OvenAdapterFamily) -> bool {
    let has_openai =
        options.organization.is_some() || options.project.is_some() || options.store.is_some();
    let has_anthropic = !options.beta.is_empty();
    let has_compatible = options.api_path.is_some();
    let has_setup_leak = options.api_version.is_some()
        || options.location.is_some()
        || options.region.is_some()
        || options.deployment.is_some();
    !has_setup_leak
        && match adapter {
            OvenAdapterFamily::Anthropic => !has_openai && !has_compatible,
            OvenAdapterFamily::OpenaiChat | OvenAdapterFamily::OpenaiResponses => {
                !has_anthropic && !has_compatible
            }
            OvenAdapterFamily::OpenaiCompatible => !has_anthropic && !has_openai,
            OvenAdapterFamily::GoogleGemini
            | OvenAdapterFamily::GoogleVertexGemini
            | OvenAdapterFamily::AwsBedrockConverse
            | OvenAdapterFamily::AzureOpenaiChat
            | OvenAdapterFamily::AzureOpenaiResponses
            | OvenAdapterFamily::CohereV2Chat => !has_anthropic && !has_openai && !has_compatible,
        }
}

fn reasoning_supported(
    reasoning: Option<&crate::ReasoningBehavior>,
    adapter: OvenAdapterFamily,
) -> bool {
    match reasoning {
        None => true,
        Some(crate::ReasoningBehavior::Effort { .. }) => adapter != OvenAdapterFamily::CohereV2Chat,
        Some(
            crate::ReasoningBehavior::Toggle { .. } | crate::ReasoningBehavior::BudgetTokens { .. },
        ) => matches!(
            adapter,
            OvenAdapterFamily::Anthropic
                | OvenAdapterFamily::GoogleGemini
                | OvenAdapterFamily::GoogleVertexGemini
                | OvenAdapterFamily::CohereV2Chat
        ),
    }
}

fn validate_no_auth_profile(
    auth: &CompiledAuthShape,
    adapter: OvenAdapterFamily,
    capabilities: &ModelCapabilities,
    options: &ProviderOptions,
) -> bool {
    if auth.method != "no-auth-v1" || adapter != OvenAdapterFamily::OpenaiResponses {
        return true;
    }
    capabilities.input == BTreeSet::from([crate::Modality::Text])
        && capabilities.output == BTreeSet::from([crate::Modality::Text])
        && !capabilities.tool_calling
        && !capabilities.parallel_tool_calls
        && !capabilities.structured_output
        && !capabilities.reasoning
        && capabilities.media.is_empty()
        && capabilities.native_replay == crate::ReplayCapability::Unsupported
        && capabilities.native_compaction == crate::CompactionCapability::Unsupported
        && options.store.is_none_or(|store| !store)
}
