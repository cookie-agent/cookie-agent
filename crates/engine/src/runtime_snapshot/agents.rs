use std::collections::BTreeMap;

use cookie_agent_config::{
    AgentDocument, AgentDocumentSource, AgentFrontmatter, AgentLimits, AgentMode,
    AgentModelFallback, AgentModelRef, AgentRegistry as ConfigAgentRegistry,
    BUILT_IN_APPROVAL_AGENT_ID, BUILT_IN_COMPACTION_AGENT_ID, BUILT_IN_DEFAULT_AGENT_ID,
    BUILT_IN_TITLE_AGENT_ID, PermissionAction, PermissionEffect, PermissionValue,
};
use cookie_agent_identity::{AgentId as IdentityAgentId, WildcardPattern};
use cookie_agent_models::{CompiledModelRuntime, compiler::CompiledModelStatus};
use cookie_agent_protocol::{AgentDescriptor, AgentId, ModelSelection};
use indexmap::IndexMap;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::EngineError;

#[derive(Clone, Debug)]
pub(crate) struct ResolvedAgent {
    pub document: AgentDocument,
    pub resolved_fallback: Vec<ResolvedAgentFallback>,
    pub runnable_as_root: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum ResolvedAgentFallback {
    Selection {
        selection: ModelSelection,
        cache: Option<cookie_agent_config::CacheConfig>,
    },
    ParentModel {
        cache: Option<cookie_agent_config::CacheConfig>,
    },
}

#[derive(Clone, Debug)]
pub struct AgentRegistry {
    preset: Option<String>,
    agents: BTreeMap<AgentId, ResolvedAgent>,
    descriptors: Vec<AgentDescriptor>,
}

impl AgentRegistry {
    pub(crate) fn resolve(
        authored: &ConfigAgentRegistry,
        models: &CompiledModelRuntime,
        preset: Option<String>,
    ) -> Result<Self, EngineError> {
        let mut documents = built_in_internal_documents()?;
        documents.extend(
            authored
                .documents()
                .iter()
                .map(|(id, document)| (id.clone(), document.clone())),
        );
        let mut agents = BTreeMap::new();
        for document in documents.into_values() {
            let id = document.id.clone();
            if id.as_str() == BUILT_IN_DEFAULT_AGENT_ID {
                return Err(EngineError::RuntimeCompileFailed);
            }
            let mut resolved_fallback = Vec::new();
            for fallback in &document.frontmatter.models {
                match &fallback.model {
                    AgentModelRef::ParentModel => {
                        resolved_fallback.push(ResolvedAgentFallback::ParentModel {
                            cache: fallback.cache.clone(),
                        });
                    }
                    AgentModelRef::Model(model_key) => {
                        let variant = match (&fallback.variant, models.model(model_key)) {
                            (None, Some(model)) => model.model.default_variant.clone(),
                            (Some(cookie_agent_identity::ConfiguredVariantRef::Base), _) => None,
                            (Some(cookie_agent_identity::ConfiguredVariantRef::Named(id)), _) => {
                                Some(id.clone())
                            }
                            (None, None) => None,
                        };
                        resolved_fallback.push(ResolvedAgentFallback::Selection {
                            selection: ModelSelection {
                                model: model_key.clone(),
                                variant,
                            },
                            cache: fallback.cache.clone(),
                        });
                    }
                }
            }
            let available = resolved_fallback
                .iter()
                .filter_map(|fallback| match fallback {
                    ResolvedAgentFallback::Selection { selection, .. } => Some(selection),
                    ResolvedAgentFallback::ParentModel { .. } => None,
                })
                .any(|selection| selection_available(models, selection));
            let runnable_as_root = document.frontmatter.enabled
                && matches!(
                    document.frontmatter.mode,
                    AgentMode::Primary | AgentMode::All
                )
                && available;
            agents.insert(
                id,
                ResolvedAgent {
                    document,
                    resolved_fallback,
                    runnable_as_root,
                },
            );
        }
        if !agents.values().any(|agent| agent.runnable_as_root)
            && let Some(selection) = first_available_selection(models)
        {
            let document = built_in_default_document(&selection)?;
            agents.insert(
                document.id.clone(),
                ResolvedAgent {
                    document,
                    resolved_fallback: vec![ResolvedAgentFallback::Selection {
                        selection,
                        cache: None,
                    }],
                    runnable_as_root: true,
                },
            );
        }
        let descriptors = agents
            .iter()
            .map(|(id, agent)| AgentDescriptor {
                id: id.clone(),
                preset: preset.clone(),
                description: agent.document.frontmatter.description.clone(),
                mode: agent.document.frontmatter.mode,
                enabled: agent.document.frontmatter.enabled,
                runnable_as_root: agent.runnable_as_root,
                resolved_fallback: agent
                    .resolved_fallback
                    .iter()
                    .filter_map(|fallback| match fallback {
                        ResolvedAgentFallback::Selection { selection, .. } => {
                            Some(selection.clone())
                        }
                        ResolvedAgentFallback::ParentModel { .. } => None,
                    })
                    .collect(),
                delegation_targets: delegation_targets(&agent.document.frontmatter.permissions),
            })
            .collect();
        Ok(Self {
            preset,
            agents,
            descriptors,
        })
    }

    pub(crate) fn get(&self, id: &AgentId) -> Option<&ResolvedAgent> {
        self.agents.get(id)
    }

    pub(crate) fn descriptors(&self) -> &[AgentDescriptor] {
        &self.descriptors
    }

    pub(crate) fn preset(&self) -> Option<&str> {
        self.preset.as_deref()
    }
}

fn first_available_selection(models: &CompiledModelRuntime) -> Option<ModelSelection> {
    models.models().values().find_map(|model| {
        (model.model.status == CompiledModelStatus::Available).then(|| ModelSelection {
            model: model.key.clone(),
            variant: model.model.default_variant.clone(),
        })
    })
}

fn built_in_internal_documents() -> Result<BTreeMap<AgentId, AgentDocument>, EngineError> {
    [
        (
            BUILT_IN_APPROVAL_AGENT_ID,
            "Built-in approval evaluator",
            "Evaluate the supplied approval request conservatively. Return only the requested structured decision.\n",
            AgentLimits {
                timeout_ms: 30_000,
                max_output_tokens: 2_048,
            },
        ),
        (
            BUILT_IN_COMPACTION_AGENT_ID,
            "Built-in context compaction agent",
            "Summarize conversation context faithfully within the supplied bounds. Return summary text only.\n",
            AgentLimits {
                timeout_ms: 30_000,
                max_output_tokens: 2_048,
            },
        ),
        (
            BUILT_IN_TITLE_AGENT_ID,
            "Built-in session title agent",
            "Generate a concise plain-text title from the supplied first user message. Return title text only.\n",
            AgentLimits {
                timeout_ms: 10_000,
                max_output_tokens: 128,
            },
        ),
    ]
    .into_iter()
    .map(|(id, description, body, limits)| {
        let document = built_in_internal_document(id, description, body, limits)?;
        Ok((document.id.clone(), document))
    })
    .collect()
}

fn built_in_internal_document(
    id: &str,
    description: &str,
    body: &str,
    limits: AgentLimits,
) -> Result<AgentDocument, EngineError> {
    let id = IdentityAgentId::new(id).map_err(|_| EngineError::RuntimeCompileFailed)?;
    let body = body.to_owned();
    let frontmatter = AgentFrontmatter {
        description: description.to_owned(),
        mode: AgentMode::Internal,
        enabled: true,
        models: vec![AgentModelFallback {
            model: AgentModelRef::ParentModel,
            variant: None,
            cache: None,
        }],
        limits,
        permissions: IndexMap::new(),
    };
    let document_fingerprint = fingerprint(
        "cookie-agent/built-in-internal-agent-document/v2",
        &(id.as_str(), &frontmatter, &body),
    )?;
    let prompt_fingerprint = fingerprint("cookie-agent/system-prompt/v1", &body)?;
    Ok(AgentDocument {
        id,
        frontmatter,
        body,
        source: AgentDocumentSource::BuiltIn,
        document_fingerprint,
        prompt_fingerprint,
    })
}

fn built_in_default_document(selection: &ModelSelection) -> Result<AgentDocument, EngineError> {
    let id = IdentityAgentId::new(BUILT_IN_DEFAULT_AGENT_ID)
        .map_err(|_| EngineError::RuntimeCompileFailed)?;
    let body = "You are Cookie Agent's built-in default coding agent. Help the user inspect, understand, and modify software safely and precisely.\n".to_owned();
    let frontmatter = AgentFrontmatter {
        description: "Built-in default coding agent".to_owned(),
        mode: AgentMode::Primary,
        enabled: true,
        models: vec![AgentModelFallback {
            model: AgentModelRef::Model(selection.model.clone()),
            variant: selection
                .variant
                .clone()
                .map(cookie_agent_identity::ConfiguredVariantRef::Named),
            cache: None,
        }],
        limits: AgentLimits::default(),
        permissions: built_in_default_permissions()?,
    };
    let document_fingerprint = fingerprint(
        "cookie-agent/built-in-default-document/v2",
        &(id.as_str(), &frontmatter, &body),
    )?;
    let prompt_fingerprint = fingerprint("cookie-agent/system-prompt/v1", &body)?;
    Ok(AgentDocument {
        id,
        frontmatter,
        body,
        source: AgentDocumentSource::BuiltIn,
        document_fingerprint,
        prompt_fingerprint,
    })
}

fn fingerprint(
    domain: &str,
    value: &impl Serialize,
) -> Result<cookie_agent_models::Sha256Digest, EngineError> {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(value).map_err(|_| EngineError::RuntimeCompileFailed)?);
    cookie_agent_models::Sha256Digest::new(format!("{:x}", hasher.finalize()))
        .map_err(|_| EngineError::RuntimeCompileFailed)
}

fn built_in_default_permissions() -> Result<IndexMap<PermissionAction, PermissionValue>, EngineError>
{
    let definitions = [
        (
            PermissionAction::Read,
            "tool_result:*",
            PermissionEffect::Allow,
        ),
        (PermissionAction::Read, "*?*", PermissionEffect::Allow),
        (PermissionAction::Write, "*", PermissionEffect::Ask),
        (PermissionAction::Bash, "*", PermissionEffect::Ask),
        (PermissionAction::Delegate, "*", PermissionEffect::Ask),
        (PermissionAction::Read, "/*", PermissionEffect::Ask),
        (PermissionAction::Read, ".env", PermissionEffect::Deny),
        (PermissionAction::Read, "*/.env", PermissionEffect::Deny),
        (PermissionAction::Read, ".env.*", PermissionEffect::Deny),
        (PermissionAction::Read, "*/.env.*", PermissionEffect::Deny),
        (
            PermissionAction::Read,
            ".env.example",
            PermissionEffect::Allow,
        ),
        (
            PermissionAction::Read,
            "*/.env.example",
            PermissionEffect::Allow,
        ),
        (
            PermissionAction::Read,
            "store-v3.json",
            PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            "*/store-v3.json",
            PermissionEffect::Deny,
        ),
        (PermissionAction::Read, "token-v1", PermissionEffect::Deny),
        (PermissionAction::Read, "*/token-v1", PermissionEffect::Deny),
        (PermissionAction::Read, "id_*", PermissionEffect::Deny),
        (PermissionAction::Read, "*/id_*", PermissionEffect::Deny),
        (PermissionAction::Read, ".netrc", PermissionEffect::Deny),
        (PermissionAction::Read, "*/.netrc", PermissionEffect::Deny),
        (
            PermissionAction::Read,
            "application_default_credentials.json",
            PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            "*/application_default_credentials.json",
            PermissionEffect::Deny,
        ),
    ];
    let mut permissions = IndexMap::new();
    for (action, resource, effect) in definitions {
        let entry = permissions
            .entry(action)
            .or_insert_with(|| PermissionValue::Resources(IndexMap::new()));
        let PermissionValue::Resources(resources) = entry else {
            unreachable!("synthetic permissions use map form")
        };
        resources.insert(
            WildcardPattern::new(resource).map_err(|_| EngineError::RuntimeCompileFailed)?,
            effect,
        );
    }
    Ok(permissions)
}

pub(crate) fn delegation_targets(
    permissions: &IndexMap<PermissionAction, PermissionValue>,
) -> Vec<AgentId> {
    let Some(PermissionValue::Resources(resources)) = permissions.get(&PermissionAction::Delegate)
    else {
        return Vec::new();
    };
    let mut targets = resources
        .iter()
        .filter(|(_, effect)| **effect != PermissionEffect::Deny)
        .filter_map(|(resource, _)| AgentId::new(resource.as_str()).ok())
        .collect::<Vec<_>>();
    targets.sort();
    targets
}

fn selection_available(models: &CompiledModelRuntime, selection: &ModelSelection) -> bool {
    models.model(&selection.model).is_some_and(|model| {
        model.model.status == CompiledModelStatus::Available
            && selection
                .variant
                .as_ref()
                .is_none_or(|variant| model.model.variants.contains_key(variant))
    })
}

#[cfg(test)]
mod tests {
    use cookie_agent_config::{PermissionAction, PermissionEffect, PermissionValue};
    use cookie_agent_identity::WildcardPattern;
    use indexmap::IndexMap;

    use super::delegation_targets;

    #[test]
    fn delegation_targets_require_named_non_deny_permissions() {
        assert!(delegation_targets(&IndexMap::new()).is_empty());

        let mut resources = IndexMap::new();
        resources.insert(WildcardPattern::new("*").unwrap(), PermissionEffect::Deny);
        resources.insert(
            WildcardPattern::new("reviewer").unwrap(),
            PermissionEffect::Ask,
        );
        let permissions = IndexMap::from([(
            PermissionAction::Delegate,
            PermissionValue::Resources(resources),
        )]);
        assert_eq!(
            delegation_targets(&permissions),
            [cookie_agent_protocol::AgentId::new("reviewer").unwrap()]
        );
    }
}
