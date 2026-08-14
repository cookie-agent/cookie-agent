use std::collections::{BTreeMap, BTreeSet};

use cookie_agent_identity::{ProviderId, ProviderModelId, VariantId};
use serde::Serialize;

use crate::{
    ModelCapabilities, ProviderOptions, Sha256Digest,
    adapters::{
        OvenAdapterFamily, custom_setup_recipe, validate_capability_ceiling,
        validate_custom_endpoint, validate_managed_base_url, validate_static_headers,
        wire_adapter_for_custom,
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
        COMPILER_VERSION, FamilyKind, FamilyRecipe, FamilyRecipeRegistry, ValidatedSetup,
        auth_method, family_registry, placeholders, resolve_model, substitute_placeholders,
        validate_auth_definition, validate_setup,
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
    pub family_id: String,
    pub effective_npm: String,
    pub adapter_id: String,
    pub resolved_shape: String,
    pub reasoning_field: String,
    pub adapter: OvenAdapterFamily,
    pub endpoint: Option<String>,
    pub setup: Option<ValidatedSetup>,
    pub auth: CompiledAuthShape,
    pub capabilities: ModelCapabilities,
    pub defaults: RequestDefaults,
    pub options: ProviderOptions,
    pub variants: BTreeMap<VariantId, CompiledVariant>,
    pub variant_order: Vec<VariantId>,
    pub default_variant: Option<VariantId>,
    pub status: CompiledModelStatus,
    pub behavior_fingerprint: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UnsupportedModel {
    pub id: ProviderModelId,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompiledDynamicProvider {
    pub id: ProviderId,
    pub models: BTreeMap<ProviderModelId, CompiledDynamicModel>,
    pub unsupported_models: Vec<UnsupportedModel>,
    pub fingerprint: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DynamicCompileError {
    #[error("no_known_protocol_family")]
    UnsupportedProvider,
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
    registry: FamilyRecipeRegistry,
}

impl Default for DynamicCompiler {
    fn default() -> Self {
        Self::family_registry()
    }
}

impl DynamicCompiler {
    #[must_use]
    pub const fn family_registry() -> Self {
        Self {
            registry: family_registry(),
        }
    }

    pub fn compile_managed(
        &self,
        catalog_revision: &str,
        record: &CatalogProviderRecord,
        authored: Option<&ModelsDevProvider>,
    ) -> Result<CompiledDynamicProvider, DynamicCompileError> {
        let family = self
            .registry
            .classify(record)
            .ok_or(DynamicCompileError::UnsupportedProvider)?;
        if let Some(authored) = authored
            && let Some(base_url) = authored.base_url.as_ref()
        {
            validate_managed_base_url(
                crate::recipes::EndpointPolicy::DefaultWithAuthoredHttpsOverride {
                    default: "https://invalid.example",
                },
                Some(base_url),
            )
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
        let mut unsupported_models = Vec::new();
        for (table_id, entry) in &record.models {
            let Some(model) = entry.record.as_ref() else {
                continue;
            };
            if model.status == crate::catalog::CatalogModelStatus::Deprecated
                || !model.modalities.output.iter().any(|value| value == "text")
            {
                continue;
            }
            let override_ = authored.and_then(|value| value.model_overrides.get(table_id));
            if override_.and_then(|value| value.enabled) == Some(false) {
                continue;
            }
            let resolved = match resolve_model(
                record,
                model,
                authored.and_then(|value| value.shape),
                override_.and_then(|value| value.shape),
            ) {
                Ok(resolved) => resolved,
                Err(error) => {
                    unsupported_models.push(UnsupportedModel {
                        id: table_id.clone(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            match self.compile_managed_model(
                catalog_revision,
                &record.id,
                model,
                family,
                &resolved,
                authored,
                override_,
            ) {
                Ok(compiled) => {
                    models.insert(table_id.clone(), compiled);
                }
                Err(ModelLocalError::Unsupported(reason)) => {
                    unsupported_models.push(UnsupportedModel {
                        id: table_id.clone(),
                        reason,
                    });
                }
                Err(ModelLocalError::Provider(error)) => return Err(error),
            }
        }
        unsupported_models.sort_by(|left, right| left.id.cmp(&right.id));
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
                &unsupported_models,
            ),
        );
        Ok(CompiledDynamicProvider {
            id: record.id.clone(),
            models,
            unsupported_models,
            fingerprint: provider_fingerprint,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_managed_model(
        &self,
        catalog_revision: &str,
        provider_id: &ProviderId,
        model: &CatalogModelRecord,
        provider_family: &'static FamilyRecipe,
        resolved: &crate::recipes::ResolvedFamilyModel,
        authored: Option<&ModelsDevProvider>,
        override_: Option<&ManagedModelOverride>,
    ) -> Result<CompiledDynamicModel, ModelLocalError> {
        let adapter = resolved.adapter;
        let adapter_id = match adapter {
            OvenAdapterFamily::OpenaiCompatible => {
                format!("oven.openai-compatible.chat.{}", provider_id.as_str())
            }
            OvenAdapterFamily::AnthropicCompatible => {
                format!(
                    "oven.anthropic-compatible.messages.{}",
                    provider_id.as_str()
                )
            }
            _ => adapter.protocol_recipe().to_owned(),
        };
        let mut capabilities = capabilities_from_catalog(model, adapter).map_err(|_| {
            ModelLocalError::Unsupported("unsupported_model_capabilities".to_owned())
        })?;
        apply_compaction_config(
            &mut capabilities,
            adapter,
            provider_id,
            override_.map_or(crate::NativeCompactionConfig::Unsupported, |value| {
                value.compaction
            }),
        )
        .map_err(ModelLocalError::Provider)?;
        if !validate_capability_shape(&capabilities)
            || validate_capability_ceiling(adapter, &capabilities).is_err()
        {
            return Err(ModelLocalError::Unsupported(
                "unsupported_model_capabilities".to_owned(),
            ));
        }
        let template = authored
            .and_then(|value| value.base_url.as_ref())
            .map(crate::authoring::EndpointUrl::as_str)
            .map(str::to_owned)
            .or_else(|| resolved.endpoint_template.clone());
        let (setup, endpoint) = resolved_managed_setup_and_endpoint(
            provider_family,
            resolved.recipe.family,
            template.as_deref(),
            authored,
        )?;
        let required_auth_method = match adapter {
            OvenAdapterFamily::AwsBedrockConverse => Some("aws-sigv4-credentials-v1"),
            OvenAdapterFamily::OpenaiResponses if resolved.recipe.family == FamilyKind::Bedrock => {
                Some("bearer-api-key-v1")
            }
            _ => None,
        };
        let auth = managed_auth(
            provider_family,
            resolved.recipe,
            required_auth_method,
            authored,
        )?;
        let mut defaults = managed_defaults(model);
        if let Some(override_) = override_ {
            apply_partial_defaults(&mut defaults, &override_.defaults);
        }
        if !validate_defaults(&defaults, &capabilities) {
            return Err(ModelLocalError::Unsupported(
                "unsupported_model_capabilities".to_owned(),
            ));
        }
        let (variants, variant_order, default_variant) =
            managed_variants(&model.reasoning_options, override_, adapter).map_err(|_| {
                ModelLocalError::Unsupported("unsupported_protocol_feature".to_owned())
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
                    resolved.recipe.family.id(),
                    &adapter_id,
                    adapter,
                ),
                &endpoint,
                &setup,
                &auth,
                &capabilities,
                &defaults,
                &options,
                &variants,
                &variant_order,
                &default_variant,
                "managed_catalog",
            ),
        );
        Ok(CompiledDynamicModel {
            id: model.id.clone(),
            display_name,
            family_id: resolved.recipe.family.id().to_owned(),
            effective_npm: resolved.npm.clone(),
            adapter_id,
            resolved_shape: match resolved.shape {
                crate::recipes::ResolvedShape::Chat => "chat",
                crate::recipes::ResolvedShape::Responses => "responses",
            }
            .to_owned(),
            reasoning_field: match model.interleaved {
                Some(crate::catalog::CatalogInterleaved::Reasoning) => "reasoning",
                Some(crate::catalog::CatalogInterleaved::ReasoningContent)
                | Some(crate::catalog::CatalogInterleaved::Default)
                | None => "reasoning_content",
            }
            .to_owned(),
            adapter,
            endpoint,
            setup,
            auth,
            capabilities,
            defaults,
            options,
            variants,
            variant_order,
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
            let capabilities = model.capabilities.clone();
            if !validate_capability_shape(&capabilities)
                || validate_capability_ceiling(adapter, &capabilities).is_err()
                || !validate_defaults(&model.defaults, &capabilities)
                || !validate_custom_options(&model.options, adapter)
                || !validate_no_auth_profile(&auth, adapter, &model.capabilities, &model.options)
            {
                return Err(DynamicCompileError::CustomModel);
            }
            let (variants, variant_order, default_variant) =
                custom_variants(&model.variants, model.default_variant.as_ref())
                    .map_err(|_| DynamicCompileError::Variant)?;
            if variants.values().any(|variant| {
                !validate_defaults(&variant.defaults, &capabilities)
                    || variant.reasoning.is_some() && !capabilities.reasoning
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
                    wire.adapter_id,
                    adapter,
                    &endpoint,
                    &setup,
                    &auth,
                    &safe_headers,
                    &capabilities,
                    &model.defaults,
                    &options,
                    (&variants, &variant_order, &default_variant),
                    "custom_authored",
                ),
            );
            models.insert(
                id.clone(),
                CompiledDynamicModel {
                    id: id.clone(),
                    display_name: model.display_name.clone(),
                    family_id: "custom".into(),
                    effective_npm: "custom".into(),
                    adapter_id: wire.adapter_id.into(),
                    resolved_shape: if adapter == OvenAdapterFamily::OpenaiResponses
                        || adapter == OvenAdapterFamily::AzureOpenaiResponses
                    {
                        "responses"
                    } else {
                        "chat"
                    }
                    .into(),
                    reasoning_field: "reasoning_content".into(),
                    adapter,
                    endpoint: Some(endpoint),
                    setup: Some(setup.clone()),
                    auth: auth.clone(),
                    capabilities,
                    defaults: model.defaults.clone(),
                    options,
                    variants,
                    variant_order,
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
            unsupported_models: Vec::new(),
            fingerprint: provider_fingerprint,
        })
    }
}

fn apply_compaction_config(
    capabilities: &mut ModelCapabilities,
    adapter: OvenAdapterFamily,
    provider_id: &ProviderId,
    config: crate::NativeCompactionConfig,
) -> Result<(), DynamicCompileError> {
    capabilities.compaction = match (adapter, config) {
        (_, crate::NativeCompactionConfig::Unsupported) => crate::CompactionCapability::Unsupported,
        (
            OvenAdapterFamily::OpenaiResponses,
            crate::NativeCompactionConfig::OpenAiResponsesCompact,
        ) if provider_id.as_str() == "openai" => crate::CompactionCapability::Native,
        (
            OvenAdapterFamily::AzureOpenaiResponses,
            crate::NativeCompactionConfig::AzureResponsesCompact,
        ) if provider_id.as_str() == "azure.openai" => crate::CompactionCapability::Native,
        _ => return Err(DynamicCompileError::CustomModel),
    };
    Ok(())
}

enum ModelLocalError {
    Unsupported(String),
    Provider(DynamicCompileError),
}

fn resolved_managed_setup_and_endpoint(
    _provider_family: &'static FamilyRecipe,
    family: FamilyKind,
    template: Option<&str>,
    authored: Option<&ModelsDevProvider>,
) -> Result<(Option<ValidatedSetup>, Option<String>), ModelLocalError> {
    if template.is_none() && !matches!(family, FamilyKind::Vertex | FamilyKind::VertexAnthropic) {
        return Ok((None, None));
    }
    let input = authored
        .map(|value| &value.setup)
        .cloned()
        .unwrap_or_default();
    let mut values = BTreeMap::new();
    for (id, value) in &input {
        let crate::authoring::SafeSetupValue::String(value) = value else {
            return Err(ModelLocalError::Provider(DynamicCompileError::Setup));
        };
        values.insert(id.as_str().to_owned(), value.as_str().to_owned());
    }
    let mut required = template
        .map(placeholders)
        .unwrap_or_default()
        .into_iter()
        .map(|name| crate::recipes::setup_field_name(&name))
        .collect::<Vec<_>>();
    match family {
        FamilyKind::Vertex | FamilyKind::VertexAnthropic => {
            required.extend(["project".into(), "location".into()])
        }
        FamilyKind::Bedrock => required.push("region".into()),
        FamilyKind::Azure => required.push("resource_name".into()),
        _ => {}
    }
    required.sort();
    required.dedup();
    if required.iter().any(|field| !values.contains_key(field)) {
        return Ok((None, None));
    }
    let endpoint = template
        .and_then(|template| substitute_placeholders(template, &values))
        .map(|value| value.trim_end_matches('/').to_owned())
        .or_else(|| match family {
            FamilyKind::Vertex | FamilyKind::VertexAnthropic => Some(format!(
                "https://{}-aiplatform.googleapis.com/v1/projects/{}/locations/{}",
                values.get("location")?,
                values.get("project")?,
                values.get("location")?
            )),
            _ => None,
        });
    let setup = ValidatedSetup {
        recipe_id: "family-derived-setup-v1",
        values,
    };
    Ok((Some(setup), endpoint))
}

fn managed_auth(
    provider_family: &FamilyRecipe,
    effective_recipe: &FamilyRecipe,
    required_method: Option<&'static str>,
    authored: Option<&ModelsDevProvider>,
) -> Result<CompiledAuthShape, ModelLocalError> {
    if let Some(authored) = authored {
        if authored.api_key.is_some() {
            let source_method = provider_family.default_auth_method;
            let target_method =
                compatible_model_auth(source_method, effective_recipe, required_method);
            let Some(method) = target_method.and_then(auth_method) else {
                return Ok(auth_shape(
                    auth_method(effective_recipe.default_auth_method)
                        .ok_or(ModelLocalError::Provider(DynamicCompileError::Auth))?,
                    BTreeMap::new(),
                    AuthSourceCategory::Unavailable,
                ));
            };
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
            let target_method =
                compatible_model_auth(auth.method.as_str(), effective_recipe, required_method);
            let Some(method) = target_method.and_then(auth_method) else {
                return Ok(auth_shape(
                    auth_method(effective_recipe.default_auth_method)
                        .ok_or(ModelLocalError::Provider(DynamicCompileError::Auth))?,
                    BTreeMap::new(),
                    AuthSourceCategory::Unavailable,
                ));
            };
            return Ok(auth_shape(
                method,
                BTreeMap::new(),
                AuthSourceCategory::AuthoredOverride,
            ));
        }
    }
    let method = auth_method(required_method.unwrap_or(effective_recipe.default_auth_method))
        .ok_or(ModelLocalError::Provider(DynamicCompileError::Auth))?;
    Ok(auth_shape(
        method,
        BTreeMap::new(),
        AuthSourceCategory::Unavailable,
    ))
}

fn compatible_model_auth(
    source_method: &str,
    effective_recipe: &FamilyRecipe,
    required_method: Option<&'static str>,
) -> Option<&'static str> {
    let mapped = crate::recipes::compatible_auth_method(source_method, effective_recipe)?;
    required_method.map_or(Some(mapped), |required| {
        (mapped == required).then_some(required)
    })
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
            OvenAdapterFamily::Anthropic | OvenAdapterFamily::AnthropicCompatible => {
                !has_openai && !has_compatible
            }
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
                | OvenAdapterFamily::AnthropicCompatible
                | OvenAdapterFamily::AwsBedrockConverse
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
        && options.store.is_none_or(|store| !store)
}
