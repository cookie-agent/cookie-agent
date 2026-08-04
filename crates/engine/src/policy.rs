use std::{collections::BTreeSet, sync::Arc};

use cookie_agent_config::{AgentRegistry, ResolvedAgent};
use cookie_agent_models as models;
use cookie_agent_protocol as protocol;
use protocol::{AgentId, ModelSelection, RunSelection};

use crate::{EngineError, ModelError};

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResultLimits {
    pub tool_output_max_lines: usize,
    pub tool_output_max_bytes: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct FrozenRunPolicy {
    pub agent: protocol::AgentSnapshot,
    pub selected_suffix: Vec<models::FrozenModelBinding>,
    pub selected_suffix_wire: Vec<protocol::FrozenModelBinding>,
    pub model_snapshot: Arc<models::ModelSnapshot>,
    pub registry: Arc<AgentRegistry>,
    pub result_limits: ResultLimits,
}

impl FrozenRunPolicy {
    pub fn active_suffix(&self, fallback_index: usize) -> &[models::FrozenModelBinding] {
        self.selected_suffix
            .get(fallback_index..)
            .unwrap_or_default()
    }

    pub fn tools(&self) -> BTreeSet<String> {
        self.agent
            .tools
            .iter()
            .map(|tool| tool_name(*tool).to_owned())
            .collect()
    }
}

pub(crate) fn freeze_agent_policy(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    model_snapshot: Arc<models::ModelSnapshot>,
    selection: &ModelSelection,
    inherited_suffix: Option<&[models::FrozenModelBinding]>,
    inherited_depth_ceiling: Option<u32>,
    result_limits: ResultLimits,
) -> Result<FrozenRunPolicy, EngineError> {
    let model_set = model_snapshot.model_set();
    let (fallback_chain, selected_suffix, selected_suffix_start) =
        if agent.resolved_fallback.is_empty() {
            let inherited = inherited_suffix
                .filter(|suffix| !suffix.is_empty())
                .ok_or_else(|| model_error("empty-chain agent requires a delegated active suffix"))?
                .to_vec();
            if inherited[0].resolved.selection != *selection {
                return Err(model_error(
                    "delegated inherited selection does not match active suffix",
                ));
            }
            (inherited.clone(), inherited, 0)
        } else {
            let index = agent
                .resolved_fallback
                .iter()
                .position(|candidate| candidate.model == selection.model)
                .ok_or_else(|| model_error("selected model is not in the agent fallback chain"))?;
            let fallback_chain = agent
                .resolved_fallback
                .iter()
                .map(|candidate| model_set.freeze(candidate).map_err(model_set_error))
                .collect::<Result<Vec<_>, _>>()?;
            let mut suffix = fallback_chain[index..].to_vec();
            suffix[0] = model_set.freeze(selection).map_err(model_set_error)?;
            (fallback_chain, suffix, index as u32)
        };

    let document = &agent.document;
    let delegation = document.frontmatter.delegation.as_ref().map(|delegation| {
        let mut targets = delegation.agents.clone();
        targets.sort();
        let effective_depth_ceiling = inherited_depth_ceiling
            .map_or(delegation.max_depth, |parent| {
                parent.min(delegation.max_depth)
            });
        protocol::FrozenDelegationPolicy {
            targets,
            max_depth: delegation.max_depth,
            effective_depth_ceiling,
        }
    });
    let snapshot = protocol::AgentSnapshot {
        agent: document.id.clone(),
        schema: protocol::AgentSchemaVersion::current(),
        mode: wire_agent_mode(document.frontmatter.mode),
        description: document.frontmatter.description.clone(),
        document_source: wire_document_source(document.source),
        document_fingerprint: wire_digest(&document.document_fingerprint)?,
        composed_prompt: document.body.clone(),
        prompt_fingerprint: wire_digest(&document.prompt_fingerprint)?,
        tools: document
            .frontmatter
            .tools
            .iter()
            .copied()
            .map(wire_tool)
            .collect(),
        permissions: document
            .frontmatter
            .permissions
            .iter()
            .map(wire_permission)
            .collect::<Result<Vec<_>, _>>()?,
        delegation,
        fallback_chain: fallback_chain
            .iter()
            .map(wire_binding)
            .collect::<Result<Vec<_>, _>>()?,
        selected_suffix_start,
    };
    let selected_suffix_wire = selected_suffix
        .iter()
        .map(wire_binding)
        .collect::<Result<Vec<_>, _>>()?;
    snapshot
        .validate_selected_suffix(
            &RunSelection {
                agent: document.id.clone(),
                model: selection.clone(),
            },
            &selected_suffix_wire,
        )
        .map_err(|error| model_error(error.to_string()))?;
    Ok(FrozenRunPolicy {
        agent: snapshot,
        selected_suffix,
        selected_suffix_wire,
        model_snapshot,
        registry,
        result_limits,
    })
}

pub(crate) fn policy_for_session_selection(
    mut agent: protocol::AgentSnapshot,
    registry: Arc<AgentRegistry>,
    model_snapshot: Arc<models::ModelSnapshot>,
    selection: &RunSelection,
    tool_output_max_lines: usize,
    tool_output_max_bytes: usize,
) -> Result<FrozenRunPolicy, EngineError> {
    let model_set = model_snapshot.model_set();
    if selection.agent != agent.agent {
        return Err(model_error(
            "run selection does not match the session agent",
        ));
    }
    let index = agent
        .fallback_chain
        .iter()
        .position(|binding| binding.resolved.selection.model == selection.model.model)
        .ok_or_else(|| model_error("selected model is not in the frozen agent fallback chain"))?;
    agent.selected_suffix_start = index as u32;
    let mut suffix = agent.fallback_chain[index..].to_vec();
    let selected = model_set
        .freeze(&selection.model)
        .map_err(model_set_error)?;
    suffix[0] = wire_binding(&selected)?;
    agent
        .validate_selected_suffix(selection, &suffix)
        .map_err(|error| model_error(error.to_string()))?;
    policy_from_snapshot(
        agent,
        suffix,
        registry,
        model_snapshot,
        tool_output_max_lines,
        tool_output_max_bytes,
    )
}

pub(crate) fn policy_from_snapshot(
    agent: protocol::AgentSnapshot,
    selected_suffix_wire: Vec<protocol::FrozenModelBinding>,
    registry: Arc<AgentRegistry>,
    model_snapshot: Arc<models::ModelSnapshot>,
    tool_output_max_lines: usize,
    tool_output_max_bytes: usize,
) -> Result<FrozenRunPolicy, EngineError> {
    let model_set = model_snapshot.model_set();
    let selected_suffix = selected_suffix_wire
        .iter()
        .map(|binding| {
            let current = model_set
                .freeze(&binding.resolved.selection)
                .map_err(model_set_error)?;
            if wire_binding(&current)? != *binding {
                return Err(model_error("obsolete_model_fingerprint"));
            }
            Ok(current)
        })
        .collect::<Result<Vec<_>, EngineError>>()?;
    if selected_suffix.is_empty() {
        return Err(model_error("frozen selected suffix is empty"));
    }
    Ok(FrozenRunPolicy {
        agent,
        selected_suffix,
        selected_suffix_wire,
        model_snapshot,
        registry,
        result_limits: ResultLimits {
            tool_output_max_lines,
            tool_output_max_bytes,
        },
    })
}

pub(crate) fn wire_binding(
    binding: &models::FrozenModelBinding,
) -> Result<protocol::FrozenModelBinding, EngineError> {
    let adapter_id = wire_adapter(binding.resolved.adapter_id);
    let selection_fingerprint = wire_digest(&binding.resolved.selection_fingerprint)?;
    let behavior_fingerprint = wire_digest(&binding.behavior_fingerprint)?;
    let value = protocol::FrozenModelBinding {
        resolved: protocol::ResolvedModelRef {
            selection: binding.resolved.selection.clone(),
            provider_id: binding.resolved.provider_id.clone(),
            model_id: binding.resolved.model_id.clone(),
            adapter_id,
            selection_fingerprint,
        },
        descriptor: binding.descriptor.clone(),
        defaults: wire_defaults(&binding.defaults)?,
        provider_options: wire_provider_options(
            binding.resolved.adapter_id,
            &binding.provider_options,
        )?,
        behavior_fingerprint,
    };
    value
        .validate()
        .map_err(|error| model_error(error.to_string()))?;
    Ok(value)
}

pub(crate) fn wire_resolved(
    binding: &models::FrozenModelBinding,
) -> Result<protocol::ResolvedModelRef, EngineError> {
    Ok(wire_binding(binding)?.resolved)
}

fn wire_digest(value: &models::Sha256Digest) -> Result<protocol::Sha256Digest, EngineError> {
    protocol::Sha256Digest::new(value.as_str()).map_err(|error| model_error(error.to_string()))
}

fn wire_defaults(
    defaults: &models::ResolvedRequestDefaults,
) -> Result<protocol::ResolvedRequestDefaults, EngineError> {
    let json = serde_json::to_value(defaults).map_err(|error| model_error(error.to_string()))?;
    serde_json::from_value(json).map_err(|error| model_error(error.to_string()))
}

fn wire_provider_options(
    adapter: models::AdaptorId,
    options: &models::ProviderOptions,
) -> Result<protocol::ProviderOptions, EngineError> {
    let missing = |field: &str| model_error(format!("frozen provider option `{field}` is missing"));
    Ok(match adapter {
        models::AdaptorId::Anthropic => protocol::ProviderOptions::Anthropic {
            api_version: options.api_version.clone(),
            beta: options.beta.clone(),
        },
        models::AdaptorId::OpenaiChat => protocol::ProviderOptions::OpenAiChat {
            organization: options.organization.clone(),
            project: options.project.clone(),
        },
        models::AdaptorId::OpenaiResponses => protocol::ProviderOptions::OpenAiResponses {
            organization: options.organization.clone(),
            project: options.project.clone(),
            store: options.store,
        },
        models::AdaptorId::OpenaiCompatible => protocol::ProviderOptions::OpenAiCompatible {
            api_path: options.api_path.clone(),
        },
        models::AdaptorId::GoogleGemini => protocol::ProviderOptions::GoogleGemini {
            api_version: options.api_version.clone(),
        },
        models::AdaptorId::GoogleVertexGemini => protocol::ProviderOptions::GoogleVertexGemini {
            project: options.project.clone().ok_or_else(|| missing("project"))?,
            location: options
                .location
                .clone()
                .ok_or_else(|| missing("location"))?,
        },
        models::AdaptorId::AwsBedrockConverse => protocol::ProviderOptions::AwsBedrockConverse {
            region: options.region.clone().ok_or_else(|| missing("region"))?,
        },
        models::AdaptorId::AzureOpenaiChat => protocol::ProviderOptions::AzureOpenAiChat {
            deployment: options
                .deployment
                .clone()
                .ok_or_else(|| missing("deployment"))?,
            api_version: options
                .api_version
                .clone()
                .ok_or_else(|| missing("api_version"))?,
        },
        models::AdaptorId::AzureOpenaiResponses => {
            protocol::ProviderOptions::AzureOpenAiResponses {
                deployment: options
                    .deployment
                    .clone()
                    .ok_or_else(|| missing("deployment"))?,
                api_version: options
                    .api_version
                    .clone()
                    .ok_or_else(|| missing("api_version"))?,
            }
        }
        models::AdaptorId::CohereV2Chat => protocol::ProviderOptions::CohereV2Chat {
            api_version: options.api_version.clone(),
        },
        models::AdaptorId::OpenResponses => protocol::ProviderOptions::OpenResponses {
            protocol_mode: match options
                .protocol_mode
                .unwrap_or(models::OpenResponsesMode::Standard)
            {
                models::OpenResponsesMode::Standard => protocol::OpenResponsesMode::Standard,
                models::OpenResponsesMode::Compact => protocol::OpenResponsesMode::Compact,
            },
        },
    })
}

fn wire_permission(
    rule: &cookie_agent_config::PermissionRule,
) -> Result<protocol::PermissionRule, EngineError> {
    Ok(protocol::PermissionRule {
        id: protocol::SafeCode::new(rule.id.as_str())
            .map_err(|error| model_error(error.to_string()))?,
        action: match rule.action {
            cookie_agent_config::PermissionAction::Read => protocol::PermissionAction::Read,
            cookie_agent_config::PermissionAction::Write => protocol::PermissionAction::Write,
            cookie_agent_config::PermissionAction::Bash => protocol::PermissionAction::Bash,
            cookie_agent_config::PermissionAction::Grep => protocol::PermissionAction::Grep,
            cookie_agent_config::PermissionAction::Glob => protocol::PermissionAction::Glob,
            cookie_agent_config::PermissionAction::Delegate => protocol::PermissionAction::Delegate,
            cookie_agent_config::PermissionAction::ExternalDirectory => {
                protocol::PermissionAction::ExternalDirectory
            }
        },
        resource: protocol::WildcardPattern::new(rule.resource.as_str())
            .map_err(|error| model_error(error.to_string()))?,
        effect: match rule.effect {
            cookie_agent_config::PermissionEffect::Allow => protocol::PermissionEffect::Allow,
            cookie_agent_config::PermissionEffect::Ask => protocol::PermissionEffect::Ask,
            cookie_agent_config::PermissionEffect::Deny => protocol::PermissionEffect::Deny,
        },
    })
}

fn wire_agent_mode(mode: cookie_agent_config::AgentMode) -> protocol::AgentMode {
    match mode {
        cookie_agent_config::AgentMode::Primary => protocol::AgentMode::Primary,
        cookie_agent_config::AgentMode::Subagent => protocol::AgentMode::Subagent,
        cookie_agent_config::AgentMode::All => protocol::AgentMode::All,
    }
}

fn wire_document_source(
    source: cookie_agent_config::AgentDocumentSource,
) -> protocol::AgentDocumentSource {
    match source {
        cookie_agent_config::AgentDocumentSource::BuiltIn => protocol::AgentDocumentSource::BuiltIn,
        cookie_agent_config::AgentDocumentSource::User => protocol::AgentDocumentSource::User,
        cookie_agent_config::AgentDocumentSource::Workspace => {
            protocol::AgentDocumentSource::Workspace
        }
    }
}

fn wire_tool(tool: cookie_agent_config::ToolName) -> protocol::ToolName {
    match tool {
        cookie_agent_config::ToolName::Read => protocol::ToolName::Read,
        cookie_agent_config::ToolName::Write => protocol::ToolName::Write,
        cookie_agent_config::ToolName::Edit => protocol::ToolName::Edit,
        cookie_agent_config::ToolName::Bash => protocol::ToolName::Bash,
        cookie_agent_config::ToolName::Grep => protocol::ToolName::Grep,
        cookie_agent_config::ToolName::Glob => protocol::ToolName::Glob,
    }
}

pub(crate) const fn tool_name(tool: protocol::ToolName) -> &'static str {
    match tool {
        protocol::ToolName::Read => "read",
        protocol::ToolName::Write => "write",
        protocol::ToolName::Edit => "edit",
        protocol::ToolName::Bash => "bash",
        protocol::ToolName::Grep => "grep",
        protocol::ToolName::Glob => "glob",
    }
}

fn wire_adapter(adapter: models::AdaptorId) -> protocol::AdaptorId {
    match adapter {
        models::AdaptorId::Anthropic => protocol::AdaptorId::Anthropic,
        models::AdaptorId::OpenaiChat => protocol::AdaptorId::OpenaiChat,
        models::AdaptorId::OpenaiResponses => protocol::AdaptorId::OpenaiResponses,
        models::AdaptorId::OpenaiCompatible => protocol::AdaptorId::OpenaiCompatible,
        models::AdaptorId::GoogleGemini => protocol::AdaptorId::GoogleGemini,
        models::AdaptorId::GoogleVertexGemini => protocol::AdaptorId::GoogleVertexGemini,
        models::AdaptorId::AwsBedrockConverse => protocol::AdaptorId::AwsBedrockConverse,
        models::AdaptorId::AzureOpenaiChat => protocol::AdaptorId::AzureOpenaiChat,
        models::AdaptorId::AzureOpenaiResponses => protocol::AdaptorId::AzureOpenaiResponses,
        models::AdaptorId::CohereV2Chat => protocol::AdaptorId::CohereV2Chat,
        models::AdaptorId::OpenResponses => protocol::AdaptorId::OpenResponses,
    }
}

fn model_set_error(error: models::ModelSetError) -> EngineError {
    model_error(error.to_string())
}

fn model_error(message: impl Into<String>) -> EngineError {
    EngineError::from(ModelError::invalid_request(message.into()))
}

pub(crate) fn resolve_agent<'a>(
    registry: &'a AgentRegistry,
    id: &AgentId,
) -> Result<&'a ResolvedAgent, EngineError> {
    registry
        .get(id)
        .ok_or_else(|| model_error(format!("unknown agent `{id}`")))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cookie_agent_config::AgentRegistry;
    use cookie_agent_protocol::{AgentId, RunSelection, VariantId};

    use super::{policy_for_session_selection, wire_binding};
    use crate::test_support::{agent_snapshot, model_set, model_snapshot, other_model_selection};

    #[test]
    fn public_selection_uses_the_exact_frozen_chain_suffix_and_variant() {
        let models = model_set();
        let mut agent = agent_snapshot("primary", cookie_agent_protocol::AgentMode::Primary);
        let mut selected = other_model_selection();
        agent.fallback_chain.push(
            wire_binding(&models.freeze(&selected).expect("other binding")).expect("wire binding"),
        );
        selected.variant = Some(VariantId::new("fast").expect("variant"));
        let policy = policy_for_session_selection(
            agent,
            Arc::new(AgentRegistry::resolve(Default::default(), &models).expect("empty registry")),
            model_snapshot(),
            &RunSelection {
                agent: AgentId::new("primary").expect("agent"),
                model: selected.clone(),
            },
            100,
            1_000,
        )
        .expect("selected suffix");
        assert_eq!(policy.agent.selected_suffix_start, 1);
        assert_eq!(policy.selected_suffix.len(), 1);
        assert_eq!(policy.selected_suffix[0].resolved.selection, selected);
    }

    #[test]
    fn public_selection_rejects_configured_models_outside_the_frozen_chain() {
        let models = model_set();
        let agent = agent_snapshot("primary", cookie_agent_protocol::AgentMode::Primary);
        let result = policy_for_session_selection(
            agent,
            Arc::new(AgentRegistry::resolve(Default::default(), &models).expect("empty registry")),
            model_snapshot(),
            &RunSelection {
                agent: AgentId::new("primary").expect("agent"),
                model: other_model_selection(),
            },
            100,
            1_000,
        );
        assert!(result.is_err());
    }
}
