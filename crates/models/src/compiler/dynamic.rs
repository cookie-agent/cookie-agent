use std::collections::{BTreeMap, BTreeSet};

use cookie_agent_identity::{ProviderId, ProviderModelId, VariantId};
use serde::Serialize;

use crate::{
    HeaderName, ModelCapabilities, ProviderOptions, SafeStaticHeaderValue, Sha256Digest,
    adapters::{
        OvenAdapterFamily, custom_setup_recipe, validate_capability_ceiling,
        validate_custom_endpoint, validate_managed_base_url, wire_adapter_for_custom,
    },
    authoring::{
        AuthDefinition, CustomProvider, ManagedModelOverride, ModelsDevProvider, RequestDefaults,
        validate_header_limits, validate_header_ownership,
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
    /// Whether this model comes from an authored custom provider
    /// (`source = "custom"`). Replay identity is attributed per provider for
    /// custom Responses providers so separate gateways never share history.
    pub custom: bool,
    pub id: ProviderModelId,
    pub wire_model_id: crate::authoring::WireModelId,
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
    pub replay_declaration: Option<crate::ReplayCapability>,
    pub defaults: RequestDefaults,
    pub options: ProviderOptions,
    pub headers: BTreeMap<HeaderName, SafeStaticHeaderValue>,
    #[serde(skip)]
    pub cost: Option<crate::catalog::CatalogModelCost>,
    pub variants: BTreeMap<VariantId, CompiledVariant>,
    pub variant_order: Vec<VariantId>,
    pub default_variant: Option<VariantId>,
    pub status: CompiledModelStatus,
    pub behavior_fingerprint: Sha256Digest,
}

impl CompiledDynamicModel {
    pub(crate) fn selected(
        &self,
        options: &ProviderOptions,
        model_id: Option<&crate::authoring::WireModelId>,
    ) -> Result<Self, DynamicCompileError> {
        let mut selected = self.clone();
        if let Some(model_id) = model_id {
            selected.wire_model_id = model_id.clone();
        }
        let compatible = self.adapter_id.starts_with("oven.openai-compatible.");
        let family = if compatible {
            OvenAdapterFamily::OpenaiCompatible
        } else {
            self.adapter
        };
        selected.adapter = if options.request_endpoint.is_none() {
            self.adapter
        } else {
            family.with_endpoint(options.request_endpoint)?
        };
        selected.resolved_shape = if matches!(
            selected.adapter,
            OvenAdapterFamily::OpenaiResponses | OvenAdapterFamily::AzureOpenaiResponses
        ) {
            "responses"
        } else {
            "chat"
        }
        .into();
        selected.adapter_id = if compatible {
            self.adapter_id.replacen(
                if self.adapter == OvenAdapterFamily::OpenaiResponses {
                    ".responses"
                } else {
                    ".chat"
                },
                if selected.adapter == OvenAdapterFamily::OpenaiResponses {
                    ".responses"
                } else {
                    ".chat"
                },
                1,
            )
        } else if selected.adapter == self.adapter {
            self.adapter_id.clone()
        } else {
            selected.adapter.protocol_recipe().into()
        };
        selected.options = options.clone();
        selected.capabilities.native_replay = self.replay_declaration.unwrap_or_else(|| {
            automatic_replay(
                selected.adapter,
                selected.capabilities.reasoning,
                selected.setup.as_ref(),
            )
        });
        Ok(selected)
    }

    fn validate_settings(&self) -> Result<(), DynamicCompileError> {
        let compatible_accounts = self.adapter_id.starts_with("oven.openai-compatible.")
            || self.auth.method == "no-auth-v1"
                && self.adapter == OvenAdapterFamily::OpenaiResponses;
        if compatible_accounts
            && (self.options.organization.is_some() || self.options.project.is_some())
        {
            return Err(DynamicCompileError::EndpointSelection("organization/project adaptor_options require official OpenAI authentication, not compatible or unauthenticated Responses".into()));
        }
        if self.options.store == Some(true) {
            return Err(DynamicCompileError::EndpointSelection("adaptor_options.store = true is unsupported: the integrated Responses encoder uses stateless store = false requests".into()));
        }
        let validate = |selected: &Self,
                        defaults: &RequestDefaults,
                        reasoning: Option<&crate::ReasoningBehavior>| {
            validate_capability_shape(&selected.capabilities)
                && validate_capability_ceiling(selected.adapter, &selected.capabilities).is_ok()
                && validate_defaults(defaults, &selected.capabilities)
                && validate_custom_options(&selected.options, selected.adapter)
                && (reasoning.is_none() || selected.capabilities.reasoning)
                && reasoning_supported(reasoning, selected.adapter)
        };
        if !validate(self, &self.defaults, None) {
            return Err(DynamicCompileError::CustomModel);
        }
        for variant in self.variants.values() {
            let selected = self.selected(&variant.options, variant.model_id.as_ref())?;
            if compatible_accounts
                && (selected.options.organization.is_some() || selected.options.project.is_some())
            {
                return Err(DynamicCompileError::EndpointSelection(format!(
                    "variant `{}`: organization/project adaptor_options require official OpenAI authentication",
                    variant.id
                )));
            }
            if selected.options.store == Some(true) {
                return Err(DynamicCompileError::EndpointSelection(format!(
                    "variant `{}`: adaptor_options.store = true is unsupported by the stateless Responses encoder",
                    variant.id
                )));
            }
            if !validate(&selected, &variant.defaults, variant.reasoning.as_ref()) {
                return Err(DynamicCompileError::Variant);
            }
        }
        Ok(())
    }
}

fn automatic_replay(
    adapter: OvenAdapterFamily,
    reasoning: bool,
    _setup: Option<&ValidatedSetup>,
) -> crate::ReplayCapability {
    adapter.automatic_replay(reasoning)
}

pub(crate) fn provider_wire_model_id(
    id: &ProviderModelId,
    adapter: OvenAdapterFamily,
    setup: Option<&ValidatedSetup>,
) -> Result<crate::authoring::WireModelId, DynamicCompileError> {
    let wire = if matches!(
        adapter,
        OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses
    ) {
        setup
            .and_then(|setup| setup.values.get("deployment"))
            .map_or(id.as_str(), String::as_str)
    } else {
        id.as_str()
    };
    crate::authoring::WireModelId::new(wire)
        .map_err(|reason| DynamicCompileError::EndpointSelection(reason.into()))
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
    #[error("{0}")]
    EndpointSelection(String),
    #[error("invalid_auth")]
    Auth,
    #[error("authored_base_url_requires_auth")]
    BaseUrlWithoutAuth,
    #[error("unsupported_adaptor")]
    UnsupportedAdapter,
    #[error("{0}")]
    StaticHeaders(#[source] crate::authoring::AuthoringError),
    #[error(
        "invalid model settings: check generation_options/adaptor_options against the selected endpoint and capabilities; tool_calling = false requires parallel_tool_calls = false"
    )]
    CustomModel,
    #[error(
        "invalid variant: check inherited settings against the selected endpoint, enabled = false conflicts, and default_variant"
    )]
    Variant,
    #[error("invalid cache config: {0}")]
    Cache(String),
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
        self.compile_managed_with_headers(catalog_revision, record, authored, &BTreeMap::new())
    }

    pub fn compile_managed_with_headers(
        &self,
        catalog_revision: &str,
        record: &CatalogProviderRecord,
        authored: Option<&ModelsDevProvider>,
        global_headers: &BTreeMap<HeaderName, SafeStaticHeaderValue>,
    ) -> Result<CompiledDynamicProvider, DynamicCompileError> {
        let family = self
            .registry
            .classify(record)
            .ok_or(DynamicCompileError::UnsupportedProvider)?;
        validate_managed_cache(record, authored, family)?;
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
            let resolved = match resolve_model(record, model, None, None) {
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
                global_headers,
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
        global_headers: &BTreeMap<HeaderName, SafeStaticHeaderValue>,
    ) -> Result<CompiledDynamicModel, ModelLocalError> {
        let options =
            override_.map_or_else(ProviderOptions::default, |value| value.options.resolve());
        let endpoint_family = if resolved.recipe.family == FamilyKind::OpenAiCompatibleChat {
            OvenAdapterFamily::OpenaiCompatible
        } else {
            resolved.adapter
        };
        let adapter = if options.request_endpoint.is_none() {
            resolved.adapter
        } else {
            endpoint_family
                .with_endpoint(options.request_endpoint)
                .map_err(ModelLocalError::Provider)?
        };
        let adapter_id = match adapter {
            _ if resolved.recipe.family == FamilyKind::OpenAiCompatibleChat => {
                format!(
                    "oven.openai-compatible.{}.{}",
                    if adapter == OvenAdapterFamily::OpenaiResponses {
                        "responses"
                    } else {
                        "chat"
                    },
                    provider_id.as_str()
                )
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
            return Err(if override_.is_some() {
                ModelLocalError::Provider(DynamicCompileError::CustomModel)
            } else {
                ModelLocalError::Unsupported("unsupported_model_capabilities; tool_calling = false conflicts with the parallel_tool_calls = true fallback".to_owned())
            });
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
        capabilities.native_replay =
            automatic_replay(adapter, capabilities.reasoning, setup.as_ref());
        let wire_model_id = override_
            .and_then(|value| value.model_id.clone())
            .map_or_else(
                || provider_wire_model_id(&model.id, adapter, setup.as_ref()),
                Ok,
            )
            .map_err(ModelLocalError::Provider)?;
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
            override_.defaults.apply(&mut defaults);
        }
        if !validate_defaults(&defaults, &capabilities) {
            return Err(if override_.is_some() {
                ModelLocalError::Provider(DynamicCompileError::CustomModel)
            } else {
                ModelLocalError::Unsupported("unsupported_model_capabilities".to_owned())
            });
        }
        let (mut variants, variant_order, default_variant) =
            managed_variants(&model.reasoning_options, override_, &defaults, &options).map_err(
                |_| {
                    if override_.is_some() {
                        ModelLocalError::Provider(DynamicCompileError::Variant)
                    } else {
                        ModelLocalError::Unsupported("unsupported_protocol_feature".to_owned())
                    }
                },
            )?;
        if variants
            .values()
            .any(|variant| !validate_defaults(&variant.defaults, &capabilities))
        {
            return Err(ModelLocalError::Provider(DynamicCompileError::Variant));
        }
        let headers = merge_headers([
            (global_headers, "global".to_owned()),
            (
                authored.map_or(&EMPTY_HEADERS, |value| &value.headers),
                format!("provider `{provider_id}`"),
            ),
            (
                override_.map_or(&EMPTY_HEADERS, |value| &value.headers),
                format!("provider `{provider_id}` model `{}`", model.id),
            ),
        ])
        .map_err(ModelLocalError::Provider)?;
        for variant in variants.values_mut() {
            variant.headers = merge_headers([
                (
                    &headers,
                    format!("provider `{provider_id}` model `{}`", model.id),
                ),
                (
                    &variant.headers,
                    format!(
                        "provider `{provider_id}` model `{}` variant `{}`",
                        model.id, variant.id
                    ),
                ),
            ])
            .map_err(ModelLocalError::Provider)?;
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
        let behavior_fingerprint = fingerprint(
            "cookie-agent/dynamic-model-behavior/v1",
            &(
                (
                    self.registry.revision(),
                    COMPILER_VERSION,
                    catalog_revision,
                    provider_id,
                    &model.id,
                    &wire_model_id,
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
                &headers,
                &variants,
                &variant_order,
                &default_variant,
                "managed_catalog",
            ),
        );
        let compiled = CompiledDynamicModel {
            custom: false,
            id: model.id.clone(),
            wire_model_id,
            display_name,
            family_id: resolved.recipe.family.id().to_owned(),
            effective_npm: resolved.npm.clone(),
            adapter_id,
            resolved_shape: if matches!(
                adapter,
                OvenAdapterFamily::OpenaiResponses | OvenAdapterFamily::AzureOpenaiResponses
            ) {
                "responses"
            } else {
                "chat"
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
            replay_declaration: None,
            defaults,
            options,
            headers,
            cost: model.cost.clone(),
            variants,
            variant_order,
            default_variant,
            status,
            behavior_fingerprint,
        };
        compiled.validate_settings().map_err(|error| {
            if override_.is_some() {
                ModelLocalError::Provider(error)
            } else {
                ModelLocalError::Unsupported(error.to_string())
            }
        })?;
        Ok(compiled)
    }

    pub fn compile_custom(
        &self,
        provider_id: &ProviderId,
        provider: &CustomProvider,
    ) -> Result<CompiledDynamicProvider, DynamicCompileError> {
        self.compile_custom_with_headers(provider_id, provider, &BTreeMap::new())
    }

    pub fn compile_custom_with_headers(
        &self,
        provider_id: &ProviderId,
        provider: &CustomProvider,
        global_headers: &BTreeMap<HeaderName, SafeStaticHeaderValue>,
    ) -> Result<CompiledDynamicProvider, DynamicCompileError> {
        let adapter = OvenAdapterFamily::parse(provider.adaptor.as_str())
            .ok_or(DynamicCompileError::UnsupportedAdapter)?;
        crate::authoring::validate_provider_cache(provider.cache.as_ref(), adapter.id())
            .map_err(DynamicCompileError::Cache)?;
        validate_custom_endpoint(adapter, &provider.endpoint)
            .map_err(|_| DynamicCompileError::Endpoint)?;
        let setup_recipe = custom_setup_recipe(adapter);
        let setup = validate_setup(setup_recipe, &provider.setup)
            .map_err(|_| DynamicCompileError::Setup)?;
        let auth_method = validate_auth_definition(&provider.auth, adapter.allowed_auth_methods())
            .map_err(|_| DynamicCompileError::Auth)?;
        let auth = custom_auth_shape(&provider.auth, auth_method);
        let mut models = BTreeMap::new();
        for (id, model) in &provider.models {
            let options = model.options.resolve();
            let resolved_adapter = adapter.with_endpoint(options.request_endpoint)?;
            let wire = wire_adapter_for_custom(resolved_adapter);
            let mut capabilities = model.capabilities.resolve(resolved_adapter);
            if model.capabilities.native_replay.is_none() {
                capabilities.native_replay =
                    automatic_replay(resolved_adapter, capabilities.reasoning, Some(&setup));
            }
            let defaults = model.defaults.resolve();
            let wire_model_id = model.model_id.clone().map_or_else(
                || provider_wire_model_id(id, resolved_adapter, Some(&setup)),
                Ok,
            )?;
            if !validate_capability_shape(&capabilities)
                || validate_capability_ceiling(resolved_adapter, &capabilities).is_err()
                || !validate_defaults(&defaults, &capabilities)
                || !validate_custom_options(&options, resolved_adapter)
            {
                return Err(DynamicCompileError::CustomModel);
            }
            let (mut variants, variant_order, default_variant) = custom_variants(
                &model.variants,
                model.default_variant.as_ref(),
                &defaults,
                &options,
            )
            .map_err(|_| DynamicCompileError::Variant)?;
            let headers = merge_headers([
                (global_headers, "global".to_owned()),
                (&provider.headers, format!("provider `{provider_id}`")),
                (
                    &model.headers,
                    format!("provider `{provider_id}` model `{id}`"),
                ),
            ])?;
            for variant in variants.values_mut() {
                variant.headers = merge_headers([
                    (&headers, format!("provider `{provider_id}` model `{id}`")),
                    (
                        &variant.headers,
                        format!(
                            "provider `{provider_id}` model `{id}` variant `{}`",
                            variant.id
                        ),
                    ),
                ])?;
            }
            if !model.enabled {
                continue;
            }
            let endpoint = provider.endpoint.as_str().trim_end_matches('/').to_owned();
            let safe_headers = headers
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
                    (&variants, &variant_order, &default_variant, &wire_model_id),
                    "custom_authored",
                ),
            );
            models.insert(
                id.clone(),
                CompiledDynamicModel {
                    custom: true,
                    id: id.clone(),
                    wire_model_id,
                    display_name: model.display_name.clone(),
                    family_id: "custom".into(),
                    effective_npm: "custom".into(),
                    adapter_id: if adapter == OvenAdapterFamily::OpenaiCompatible {
                        format!(
                            "oven.openai-compatible.{}.{}",
                            if resolved_adapter == OvenAdapterFamily::OpenaiResponses {
                                "responses"
                            } else {
                                "chat"
                            },
                            provider_id
                        )
                    } else {
                        wire.adapter_id.into()
                    },
                    resolved_shape: if resolved_adapter == OvenAdapterFamily::OpenaiResponses
                        || resolved_adapter == OvenAdapterFamily::AzureOpenaiResponses
                    {
                        "responses"
                    } else {
                        "chat"
                    }
                    .into(),
                    reasoning_field: "reasoning_content".into(),
                    adapter: resolved_adapter,
                    endpoint: Some(endpoint),
                    setup: Some(setup.clone()),
                    auth: auth.clone(),
                    capabilities,
                    replay_declaration: model.capabilities.native_replay,
                    defaults,
                    options,
                    headers,
                    cost: None,
                    variants,
                    variant_order,
                    default_variant,
                    status: CompiledModelStatus::Available,
                    behavior_fingerprint,
                },
            );
            models[id].validate_settings()?;
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

static EMPTY_HEADERS: BTreeMap<HeaderName, SafeStaticHeaderValue> = BTreeMap::new();

fn merge_headers<'a>(
    layers: impl IntoIterator<Item = (&'a BTreeMap<HeaderName, SafeStaticHeaderValue>, String)>,
) -> Result<BTreeMap<HeaderName, SafeStaticHeaderValue>, DynamicCompileError> {
    let mut merged = BTreeMap::new();
    for (layer, scope) in layers {
        validate_header_ownership(layer, scope).map_err(DynamicCompileError::StaticHeaders)?;
        for (name, value) in layer {
            if value.as_str().is_empty() {
                merged.remove(name);
            } else {
                merged.insert(name.clone(), value.clone());
            }
        }
    }
    validate_header_limits(&merged).map_err(DynamicCompileError::StaticHeaders)?;
    Ok(merged)
}

pub(crate) fn validate_managed_cache(
    record: &CatalogProviderRecord,
    authored: Option<&ModelsDevProvider>,
    family: &'static FamilyRecipe,
) -> Result<(), DynamicCompileError> {
    let Some(authored) = authored else {
        return Ok(());
    };
    let Some(cache) = authored.cache.as_ref() else {
        return Ok(());
    };
    cache.validate_supported_shape().map_err(|error| {
        DynamicCompileError::Cache(format!("provider `{}`: {error}", record.id))
    })?;

    let mut adapters = BTreeSet::from([managed_provider_adapter(
        family.family,
        None,
        record.shape.as_deref(),
    )]);
    for (model_id, entry) in &record.models {
        let Some(model) = entry.record.as_ref() else {
            continue;
        };
        let override_ = authored.model_overrides.get(model_id);
        if let Ok(resolved) = resolve_model(record, model, None, None) {
            let adapter = resolved
                .adapter
                .with_endpoint(override_.and_then(|value| value.options.request_endpoint))?;
            adapters.insert(
                if resolved.recipe.family == FamilyKind::OpenAiCompatibleChat {
                    OvenAdapterFamily::OpenaiCompatible
                } else {
                    adapter
                },
            );
        }
    }
    for adapter in adapters {
        crate::authoring::validate_provider_cache(Some(cache), adapter.id()).map_err(|error| {
            DynamicCompileError::Cache(format!("provider `{}`: {error}", record.id))
        })?;
    }
    Ok(())
}

pub(crate) fn managed_provider_adapter(
    family: FamilyKind,
    authored_shape: Option<crate::ManagedModelShape>,
    catalog_shape: Option<&str>,
) -> OvenAdapterFamily {
    let responses = matches!(authored_shape, Some(crate::ManagedModelShape::Responses))
        || authored_shape.is_none() && catalog_shape == Some("responses")
        || authored_shape.is_none() && catalog_shape.is_none() && family == FamilyKind::OpenAi;
    match family {
        FamilyKind::OpenAiCompatibleChat => OvenAdapterFamily::OpenaiCompatible,
        FamilyKind::Anthropic => OvenAdapterFamily::AnthropicCompatible,
        FamilyKind::OpenAi if responses => OvenAdapterFamily::OpenaiResponses,
        FamilyKind::OpenAi => OvenAdapterFamily::OpenaiChat,
        FamilyKind::Google => OvenAdapterFamily::GoogleGemini,
        FamilyKind::Vertex | FamilyKind::VertexAnthropic => OvenAdapterFamily::GoogleVertexGemini,
        FamilyKind::Bedrock if responses => OvenAdapterFamily::OpenaiResponses,
        FamilyKind::Bedrock => OvenAdapterFamily::AwsBedrockConverse,
        FamilyKind::Azure if responses => OvenAdapterFamily::AzureOpenaiResponses,
        FamilyKind::Azure => OvenAdapterFamily::AzureOpenaiChat,
        FamilyKind::Cohere => OvenAdapterFamily::CohereV2Chat,
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

fn validate_custom_options(options: &ProviderOptions, adapter: OvenAdapterFamily) -> bool {
    let has_openai =
        options.organization.is_some() || options.project.is_some() || options.store.is_some();
    let has_anthropic = !options.beta.is_empty();
    let has_compatible = options.request_endpoint.is_some()
        && !matches!(
            adapter,
            OvenAdapterFamily::OpenaiChat
                | OvenAdapterFamily::OpenaiResponses
                | OvenAdapterFamily::OpenaiCompatible
                | OvenAdapterFamily::AzureOpenaiChat
                | OvenAdapterFamily::AzureOpenaiResponses
        );
    let has_setup_leak = options.api_version.is_some()
        || options.location.is_some()
        || options.region.is_some()
        || options.deployment.is_some();
    !has_setup_leak
        && match adapter {
            OvenAdapterFamily::Anthropic | OvenAdapterFamily::AnthropicCompatible => {
                !has_openai && !has_compatible
            }
            OvenAdapterFamily::OpenaiChat => {
                !has_anthropic && !has_compatible && options.store.is_none()
            }
            OvenAdapterFamily::OpenaiResponses => !has_anthropic && !has_compatible,
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
