use std::collections::{BTreeMap, BTreeSet};

use cookie_agent_models::{
    CompiledModelRuntime, EffectiveCredentialSource, ProviderPresence as ModelProviderPresence,
    catalog::CatalogQuarantineReason,
    compiler::{CompiledModelStatus, CompiledVariantOrigin},
    manager::RetainedFamilyMatch,
    recipes::{CredentialKind, auth_method, family_registry, placeholders, setup_field_name},
};
use cookie_agent_protocol as protocol;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use super::AgentRegistry;
use crate::EngineError;

pub(crate) fn build_runtime_snapshot(
    models: &CompiledModelRuntime,
    agents: &AgentRegistry,
) -> Result<protocol::RuntimeSnapshotV1, EngineError> {
    let providers = models
        .providers()
        .iter()
        .map(|provider| provider_descriptor(models, provider))
        .collect::<Result<Vec<_>, _>>()?;
    let available_models = models
        .models()
        .values()
        .filter(|model| model.model.status == CompiledModelStatus::Available)
        .map(model_descriptor)
        .collect::<Result<Vec<_>, _>>()?;
    let agent_descriptors = agents.descriptors().to_vec();
    let agent_revision = revision::<protocol::AgentRevision, _>(
        "cookie-agent/agent-runtime/v1",
        &agent_descriptors,
        protocol::AgentRevision::new,
    )?;
    let runtime_revision = runtime_revision(
        &family_registry().revision(),
        &models.catalog().revision,
        &models.provider_state_revision(),
        models.model_revision(),
        &agent_revision,
    )?;
    let catalog = models.catalog();
    let quarantine = quarantine_summary(models)?;
    let last_error = catalog
        .state
        .last_error
        .as_ref()
        .map(|error| {
            Ok::<_, EngineError>(protocol::CatalogSafeErrorMeta {
                code: protocol::SafeCode::new(error.code.clone())
                    .map_err(|_| EngineError::RuntimeCompileFailed)?,
                message: protocol::SafeErrorMessage::new(error.safe_message.clone())
                    .map_err(|_| EngineError::RuntimeCompileFailed)?,
                time: error.occurred_at,
            })
        })
        .transpose()?;
    let snapshot = protocol::RuntimeSnapshotV1 {
        snapshot_schema_version: protocol::RuntimeSnapshotSchemaVersion::current(),
        recipe_registry_revision: family_registry().revision(),
        catalog_revision: catalog.revision.clone(),
        catalog_source: match catalog.source {
            cookie_agent_models::catalog::CatalogSource::Network => {
                protocol::CatalogSource::Network
            }
            cookie_agent_models::catalog::CatalogSource::Cache => protocol::CatalogSource::Cache,
            cookie_agent_models::catalog::CatalogSource::Bootstrap => {
                protocol::CatalogSource::Bootstrap
            }
        },
        catalog_state: protocol::CatalogRuntimeState {
            stale: !matches!(
                catalog.state.availability,
                cookie_agent_models::catalog::CatalogAvailability::Ready
            ),
            provider_quarantine_count: quarantine.provider_count,
            model_quarantine_count: quarantine.model_count,
            quarantine_digest: quarantine.digest,
            last_error,
        },
        provider_state_revision: models.provider_state_revision(),
        provider_store_generation: protocol::ProviderStoreGeneration::new(
            models.store().generation().get(),
        )
        .map_err(|_| EngineError::RuntimeCompileFailed)?,
        model_revision: models.model_revision().clone(),
        agent_revision,
        runtime_revision,
        providers,
        models: available_models,
        agents: agent_descriptors,
    };
    snapshot
        .validate()
        .map_err(|_| EngineError::RuntimeCompileFailed)?;
    Ok(snapshot)
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "source", content = "reason", rename_all = "snake_case")]
enum RuntimeQuarantineReason {
    Parser(CatalogQuarantineReason),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct RuntimeQuarantineEntry {
    provider_id: Option<String>,
    model_id: Option<String>,
    canonical_model_id: Option<String>,
    reason: RuntimeQuarantineReason,
}

struct RuntimeQuarantineSummary {
    provider_count: u32,
    model_count: u32,
    digest: protocol::Sha256Digest,
}

fn quarantine_summary(
    runtime: &CompiledModelRuntime,
) -> Result<RuntimeQuarantineSummary, EngineError> {
    let catalog = runtime.catalog();
    let entries = catalog
        .quarantine
        .iter()
        .map(|entry| RuntimeQuarantineEntry {
            provider_id: entry.provider_id.clone(),
            model_id: entry.model_id.clone(),
            canonical_model_id: entry.canonical_model_id.clone(),
            reason: RuntimeQuarantineReason::Parser(entry.reason.clone()),
        })
        .collect::<BTreeSet<_>>();
    let provider_count = entries
        .iter()
        .filter(|entry| entry.model_id.is_none() && entry.canonical_model_id.is_none())
        .count() as u32;
    let model_count = entries.len() as u32 - provider_count;
    let digest = protocol::Sha256Digest::new(hash_bytes(
        "cookie-agent/runtime-quarantine/v1",
        &serde_json::to_vec(&entries).map_err(|_| EngineError::RuntimeCompileFailed)?,
    ))
    .map_err(|_| EngineError::RuntimeCompileFailed)?;
    Ok(RuntimeQuarantineSummary {
        provider_count,
        model_count,
        digest,
    })
}

pub(crate) fn runtime_revision(
    recipe_registry_revision: &protocol::RecipeRegistryRevision,
    catalog_revision: &protocol::CatalogRevision,
    provider_state_revision: &protocol::ProviderStateRevision,
    model_revision: &protocol::ModelRevision,
    agent_revision: &protocol::AgentRevision,
) -> Result<protocol::RuntimeRevision, EngineError> {
    revision::<protocol::RuntimeRevision, _>(
        "cookie-agent/engine-runtime/v1",
        &(
            recipe_registry_revision,
            catalog_revision,
            provider_state_revision,
            model_revision,
            agent_revision,
        ),
        protocol::RuntimeRevision::new,
    )
}

fn provider_descriptor(
    runtime: &CompiledModelRuntime,
    provider: &cookie_agent_models::CompiledProviderState,
) -> Result<protocol::ProviderDescriptor, EngineError> {
    let catalog_entry = runtime.catalog().provider(&provider.id);
    let quarantined = catalog_entry.is_some_and(|entry| entry.quarantine.is_some());
    let quarantine_code = catalog_entry
        .and_then(|entry| entry.quarantine.as_ref())
        .map_or_else(
            || "invalid_catalog_provider_record".to_owned(),
            |reason| reason.code().to_owned(),
        );
    let support_reason = provider
        .support_reason
        .as_deref()
        .map(safe_code)
        .transpose()?;
    let support = if quarantined {
        protocol::ProviderSupport {
            state: protocol::ProviderSupportState::Quarantined,
            reason: Some(safe_code(&quarantine_code)?),
        }
    } else if provider.retained_family_match == Some(RetainedFamilyMatch::SupportedRemoved) {
        protocol::ProviderSupport {
            state: protocol::ProviderSupportState::Supported,
            reason: None,
        }
    } else if provider.retained_family_match
        == Some(RetainedFamilyMatch::RemovedWithoutRetainedFamilyMatch)
    {
        protocol::ProviderSupport {
            state: protocol::ProviderSupportState::Unsupported,
            reason: Some(safe_code("removed_without_retained_recipe_match")?),
        }
    } else if let Some(reason) = support_reason {
        protocol::ProviderSupport {
            state: protocol::ProviderSupportState::Unsupported,
            reason: Some(reason),
        }
    } else {
        protocol::ProviderSupport {
            state: protocol::ProviderSupportState::Supported,
            reason: None,
        }
    };
    let record = catalog_entry.and_then(|entry| entry.record.as_ref());
    let recipe = record
        .and_then(|record| family_registry().classify(record))
        .or_else(|| {
            runtime
                .store()
                .provider(&provider.id)
                .and_then(|connection| {
                    family_registry().by_npm(connection.policy.package_claim.as_str())
                })
        });
    let mut setup_ids = BTreeMap::<String, bool>::new();
    let mut add_template = |template: &str| {
        for name in placeholders(template) {
            let secret = placeholder_is_secret(&name);
            setup_ids
                .entry(setup_field_name(&name))
                .and_modify(|value| *value |= secret)
                .or_insert(secret);
        }
    };
    if let Some(record) = record {
        if let Some(api) = record.api.as_deref() {
            add_template(api);
        }
        for model in record
            .models
            .values()
            .filter_map(|entry| entry.record.as_ref())
        {
            if let Some(api) = model
                .provider
                .as_ref()
                .and_then(|provider| provider.api.as_deref())
            {
                add_template(api);
            }
        }
    }
    if let Some(recipe) = recipe {
        match recipe.family {
            cookie_agent_models::recipes::FamilyKind::Vertex
            | cookie_agent_models::recipes::FamilyKind::VertexAnthropic => {
                setup_ids.insert("project".to_owned(), false);
                setup_ids.insert("location".to_owned(), false);
            }
            cookie_agent_models::recipes::FamilyKind::Bedrock => {
                setup_ids.insert("region".to_owned(), false);
            }
            cookie_agent_models::recipes::FamilyKind::Azure => {
                setup_ids.insert("resource_name".to_owned(), false);
            }
            _ => {}
        }
    }
    let mut setup_fields = setup_ids
        .iter()
        .map(|(id, secret)| setup_descriptor(id, *secret))
        .collect::<Result<Vec<_>, EngineError>>()?;
    setup_fields.sort_by(|left, right| left.id.cmp(&right.id));
    let mut auth_methods = if let Some(recipe) = recipe {
        recipe
            .allowed_auth_methods
            .iter()
            .filter_map(|id| auth_method(id))
            .map(auth_descriptor)
            .collect::<Result<Vec<_>, EngineError>>()?
    } else {
        Vec::new()
    };
    auth_methods.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(protocol::ProviderDescriptor {
        id: provider.id.clone(),
        display_name: protocol::SafeDisplayText::new(provider.display_name.clone())
            .map_err(|_| EngineError::RuntimeCompileFailed)?,
        presence: match provider.presence {
            ModelProviderPresence::Current => protocol::ProviderPresence::Current,
            ModelProviderPresence::Removed => protocol::ProviderPresence::Removed,
        },
        support,
        setup_fields,
        auth_methods,
        configuration: match (provider.authored, provider.stored) {
            (false, false) => protocol::ProviderConfigurationState::Unconfigured,
            (true, false) => protocol::ProviderConfigurationState::Authored,
            (false, true) => protocol::ProviderConfigurationState::Stored,
            (true, true) => protocol::ProviderConfigurationState::AuthoredAndStored,
        },
        effective_auth_state: effective_auth_state(provider.effective_auth),
        durable_connection: provider
            .durable_connection
            .as_ref()
            .map(project_durable_connection)
            .transpose()?,
        quarantine: quarantined.then(|| protocol::QuarantineDiagnostic {
            code: safe_code(&quarantine_code).expect("validated quarantine code is valid"),
            message: protocol::SafeErrorMessage::new("catalog provider record is quarantined")
                .expect("static quarantine message is valid"),
        }),
    })
}

fn placeholder_is_secret(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name.contains("KEY") || name.contains("TOKEN") || name.contains("SECRET")
}

fn setup_descriptor(
    field: &str,
    secret: bool,
) -> Result<protocol::SetupFieldDescriptor, EngineError> {
    let id = protocol::SetupFieldId::new(field).map_err(|_| EngineError::RuntimeCompileFailed)?;
    Ok(protocol::SetupFieldDescriptor {
        id,
        display_name: protocol::SafeDisplayText::new(field.replace('_', " "))
            .map_err(|_| EngineError::RuntimeCompileFailed)?,
        help: protocol::SafeDisplayText::new(format!("Provider setup field `{field}`"))
            .map_err(|_| EngineError::RuntimeCompileFailed)?,
        required: true,
        default: None,
        validation: protocol::SetupFieldValidation {
            value_type: protocol::SetupFieldType::String,
            min_length: Some(1),
            max_length: Some(256),
            minimum: None,
            maximum: None,
        },
        safe_to_project: !secret,
    })
}

fn auth_descriptor(
    method: &cookie_agent_models::recipes::AuthMethodRecipe,
) -> Result<protocol::AuthMethodDescriptor, EngineError> {
    let mut credentials = method
        .credentials
        .iter()
        .map(|field| {
            Ok(protocol::AuthCredentialDescriptor {
                id: protocol::AuthFieldName::new(field.name)
                    .map_err(|_| EngineError::RuntimeCompileFailed)?,
                display_name: protocol::SafeDisplayText::new(field.name.replace('_', " "))
                    .map_err(|_| EngineError::RuntimeCompileFailed)?,
                help: protocol::SafeDisplayText::new(format!("Secret credential `{}`", field.name))
                    .map_err(|_| EngineError::RuntimeCompileFailed)?,
                required: field.required,
                credential_type: match field.kind {
                    CredentialKind::ApiKey => protocol::CredentialFieldType::ApiKey,
                    CredentialKind::AccessToken => protocol::CredentialFieldType::AccessToken,
                    CredentialKind::AccessKeyId => protocol::CredentialFieldType::AccessKeyId,
                    CredentialKind::SecretAccessKey => {
                        protocol::CredentialFieldType::SecretAccessKey
                    }
                    CredentialKind::SessionToken => protocol::CredentialFieldType::SessionToken,
                },
            })
        })
        .collect::<Result<Vec<_>, EngineError>>()?;
    credentials.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(protocol::AuthMethodDescriptor {
        id: protocol::AuthMethodId::new(method.id)
            .map_err(|_| EngineError::RuntimeCompileFailed)?,
        display_name: protocol::SafeDisplayText::new(method.id.replace('-', " "))
            .map_err(|_| EngineError::RuntimeCompileFailed)?,
        credentials,
    })
}

fn model_descriptor(
    model: &cookie_agent_models::CompiledRuntimeModel,
) -> Result<protocol::AvailableModelDescriptor, EngineError> {
    let capabilities = serde_json::from_value(
        serde_json::to_value(&model.model.capabilities)
            .map_err(|_| EngineError::RuntimeCompileFailed)?,
    )
    .map_err(|_| EngineError::RuntimeCompileFailed)?;
    let variants = model
        .model
        .variants
        .values()
        .map(|variant| {
            let fingerprint = protocol::Sha256Digest::new(hash(
                "cookie-agent/model-variant/v1",
                &(
                    variant.id.clone(),
                    &variant.defaults,
                    &variant.options,
                    &variant.reasoning,
                ),
            )?)
            .map_err(|_| EngineError::RuntimeCompileFailed)?;
            Ok(protocol::AvailableVariantDescriptor {
                id: variant.id.clone(),
                display_name: variant.display_name.clone(),
                origin: match variant.origin {
                    CompiledVariantOrigin::ModelsDevEffort => {
                        protocol::VariantOrigin::ModelsDevEffort
                    }
                    CompiledVariantOrigin::ModelsDevToggle => {
                        protocol::VariantOrigin::ModelsDevToggle
                    }
                    CompiledVariantOrigin::ModelsDevBudgetTokens => {
                        protocol::VariantOrigin::ModelsDevBudgetTokens
                    }
                    CompiledVariantOrigin::Authored => protocol::VariantOrigin::Explicit,
                },
                behavior_fingerprint: fingerprint,
            })
        })
        .collect::<Result<Vec<_>, EngineError>>()?;
    Ok(protocol::AvailableModelDescriptor {
        key: model.key.clone(),
        display_name: model.model.display_name.clone(),
        capabilities,
        variants,
        default_variant: model.model.default_variant.clone(),
        behavior_fingerprint: protocol::Sha256Digest::new(
            model.model.behavior_fingerprint.as_str(),
        )
        .map_err(|_| EngineError::RuntimeCompileFailed)?,
    })
}

pub(crate) fn project_durable_connection(
    value: &cookie_agent_models::provider_store::DurableConnectionDescriptor,
) -> Result<protocol::DurableConnectionDescriptor, EngineError> {
    Ok(protocol::DurableConnectionDescriptor {
        provider_id: value.provider_id.clone(),
        setup_values: value
            .setup_values
            .iter()
            .map(|(id, value)| {
                let value = serde_json::from_value(
                    serde_json::to_value(value).map_err(|_| EngineError::RuntimeCompileFailed)?,
                )
                .map_err(|_| EngineError::RuntimeCompileFailed)?;
                Ok((id.clone(), value))
            })
            .collect::<Result<BTreeMap<_, _>, EngineError>>()?,
        setup_fingerprint: protocol::Sha256Digest::new(value.setup_fingerprint.as_str())
            .map_err(|_| EngineError::RuntimeCompileFailed)?,
        recipe_fingerprint: protocol::Sha256Digest::new(value.recipe_fingerprint.as_str())
            .map_err(|_| EngineError::RuntimeCompileFailed)?,
        auth_method: value.auth_method.clone(),
        credential_fields: value.credential_fields.clone(),
        connection_generation: protocol::ProviderConnectionGeneration::new(
            value.connection_generation.get(),
        )
        .map_err(|_| EngineError::RuntimeCompileFailed)?,
        connected_at: value.connected_at,
    })
}

pub(crate) fn effective_auth_state(
    value: EffectiveCredentialSource,
) -> protocol::EffectiveAuthState {
    match value {
        EffectiveCredentialSource::AuthoredApiKey => protocol::EffectiveAuthState::AuthoredApiKey,
        EffectiveCredentialSource::AuthoredOverride => {
            protocol::EffectiveAuthState::AuthoredOverride
        }
        EffectiveCredentialSource::ProviderStore => protocol::EffectiveAuthState::ProviderStore,
        EffectiveCredentialSource::NoAuth => protocol::EffectiveAuthState::NoAuth,
        EffectiveCredentialSource::Unavailable => protocol::EffectiveAuthState::Unavailable,
    }
}

pub(crate) fn effective_auth_source(
    value: EffectiveCredentialSource,
) -> Result<protocol::EffectiveAuthSource, EngineError> {
    match value {
        EffectiveCredentialSource::AuthoredApiKey => {
            Ok(protocol::EffectiveAuthSource::AuthoredApiKey)
        }
        EffectiveCredentialSource::AuthoredOverride => {
            Ok(protocol::EffectiveAuthSource::AuthoredOverride)
        }
        EffectiveCredentialSource::ProviderStore => {
            Ok(protocol::EffectiveAuthSource::ProviderStore)
        }
        EffectiveCredentialSource::NoAuth => Ok(protocol::EffectiveAuthSource::NoAuth),
        EffectiveCredentialSource::Unavailable => Err(EngineError::RuntimeCompileFailed),
    }
}

fn safe_code(value: &str) -> Result<protocol::SafeCode, EngineError> {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    protocol::SafeCode::new(normalized).map_err(|_| EngineError::RuntimeCompileFailed)
}

fn revision<T, E>(
    domain: &str,
    value: &impl Serialize,
    constructor: impl FnOnce(String) -> Result<T, E>,
) -> Result<T, EngineError> {
    constructor(format!("sha256:{}", hash(domain, value)?))
        .map_err(|_| EngineError::RuntimeCompileFailed)
}

fn hash(domain: &str, value: &impl Serialize) -> Result<String, EngineError> {
    let bytes = serde_json::to_vec(value).map_err(|_| EngineError::RuntimeCompileFailed)?;
    Ok(hash_bytes(domain, &bytes))
}

fn hash_bytes(domain: &str, bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}
