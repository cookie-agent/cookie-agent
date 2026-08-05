use std::{collections::BTreeSet, sync::Arc};

use cookie_agent_protocol as protocol;

use crate::{
    EngineError,
    model_snapshots::binding_for_selection,
    runtime_snapshot::{AgentRegistry, PublishedRuntime, ResolvedAgent},
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResultLimits {
    pub tool_output_max_lines: usize,
    pub tool_output_max_bytes: usize,
}

#[derive(Clone)]
pub(crate) struct FrozenRunPolicy {
    pub agent: protocol::AgentSnapshot,
    pub selected_suffix: Vec<protocol::FrozenModelBinding>,
    pub selected_suffix_wire: Vec<protocol::FrozenModelBinding>,
    pub runtime: Arc<PublishedRuntime>,
    pub registry: Arc<AgentRegistry>,
    pub result_limits: ResultLimits,
}

impl std::fmt::Debug for FrozenRunPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrozenRunPolicy")
            .field("agent", &self.agent.agent)
            .field("selected_suffix", &self.selected_suffix)
            .field(
                "runtime_revision",
                &self.runtime.result.snapshot.runtime_revision,
            )
            .finish_non_exhaustive()
    }
}

impl FrozenRunPolicy {
    pub fn active_suffix(&self, fallback_index: usize) -> &[protocol::FrozenModelBinding] {
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

pub(crate) fn freeze_root_agent_policy(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    selection: &protocol::ModelSelection,
    result_limits: ResultLimits,
) -> Result<FrozenRunPolicy, EngineError> {
    if !agent.runnable_as_root {
        return Err(EngineError::NoRunnableModel);
    }
    let start = agent
        .resolved_fallback
        .iter()
        .position(|candidate| candidate.model == selection.model);
    let authored = start.map_or(agent.resolved_fallback.as_slice(), |index| {
        &agent.resolved_fallback[index..]
    });
    let mut selections = authored
        .iter()
        .filter(|candidate| {
            runtime.models.model(&candidate.model).is_some_and(|model| {
                model.model.status == cookie_agent_models::compiler::CompiledModelStatus::Available
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    if start.is_some() {
        if selections.is_empty() {
            return Err(EngineError::NoRunnableModel);
        }
        selections[0] = selection.clone();
    } else {
        selections.insert(0, selection.clone());
    }
    freeze_planned_policy(
        agent,
        registry,
        runtime,
        selections,
        selection,
        None,
        result_limits,
    )
}

pub(crate) fn freeze_delegated_agent_policy(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    selection: &protocol::ModelSelection,
    inherited_suffix: &[protocol::FrozenModelBinding],
    inherited_depth_ceiling: u32,
    result_limits: ResultLimits,
) -> Result<FrozenRunPolicy, EngineError> {
    let bindings = if agent.resolved_fallback.is_empty() {
        if inherited_suffix.first().map(|binding| &binding.selection) != Some(selection) {
            return Err(EngineError::NoRunnableModel);
        }
        inherited_suffix.to_vec()
    } else {
        let index = agent
            .resolved_fallback
            .iter()
            .position(|candidate| candidate.model == selection.model)
            .ok_or(EngineError::NoRunnableModel)?;
        let mut selections = agent.resolved_fallback[index..]
            .iter()
            .filter(|candidate| {
                runtime.models.model(&candidate.model).is_some_and(|model| {
                    model.model.status
                        == cookie_agent_models::compiler::CompiledModelStatus::Available
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if selections.is_empty() {
            return Err(EngineError::NoRunnableModel);
        }
        selections[0] = selection.clone();
        selections
            .iter()
            .map(|candidate| {
                binding_for_selection(&runtime.current_manifest, &runtime.models, candidate)
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    freeze_with_bindings(
        agent,
        registry,
        runtime,
        bindings,
        selection,
        Some(inherited_depth_ceiling),
        result_limits,
    )
}

fn freeze_planned_policy(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    selections: Vec<protocol::ModelSelection>,
    selection: &protocol::ModelSelection,
    inherited_depth_ceiling: Option<u32>,
    result_limits: ResultLimits,
) -> Result<FrozenRunPolicy, EngineError> {
    let bindings = selections
        .iter()
        .map(|candidate| {
            binding_for_selection(&runtime.current_manifest, &runtime.models, candidate)
        })
        .collect::<Result<Vec<_>, _>>()?;
    freeze_with_bindings(
        agent,
        registry,
        runtime,
        bindings,
        selection,
        inherited_depth_ceiling,
        result_limits,
    )
}

fn freeze_with_bindings(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    bindings: Vec<protocol::FrozenModelBinding>,
    selection: &protocol::ModelSelection,
    inherited_depth_ceiling: Option<u32>,
    result_limits: ResultLimits,
) -> Result<FrozenRunPolicy, EngineError> {
    if bindings.is_empty() {
        return Err(EngineError::NoRunnableModel);
    }
    let document = &agent.document;
    let delegation = document.frontmatter.delegation.as_ref().map(|delegation| {
        let mut targets = delegation.agents.clone();
        targets.sort();
        protocol::FrozenDelegationPolicy {
            targets,
            max_depth: delegation.max_depth,
            effective_depth_ceiling: inherited_depth_ceiling
                .map_or(delegation.max_depth, |parent| {
                    parent.min(delegation.max_depth)
                }),
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
        fallback_chain: bindings.clone(),
        selected_suffix_start: 0,
    };
    let run_selection = protocol::RunSelection {
        agent: document.id.clone(),
        model: selection.clone(),
    };
    snapshot
        .validate_selected_suffix(&run_selection, &bindings)
        .map_err(|_| EngineError::NoRunnableModel)?;
    Ok(FrozenRunPolicy {
        agent: snapshot,
        selected_suffix: bindings.clone(),
        selected_suffix_wire: bindings,
        runtime,
        registry,
        result_limits,
    })
}

pub(crate) fn policy_for_session_selection(
    mut agent: protocol::AgentSnapshot,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    selection: &protocol::RunSelection,
    tool_output_max_lines: usize,
    tool_output_max_bytes: usize,
) -> Result<FrozenRunPolicy, EngineError> {
    if selection.agent != agent.agent {
        return Err(EngineError::NoRunnableModel);
    }
    let index = agent
        .fallback_chain
        .iter()
        .position(|binding| binding.selection == selection.model)
        .ok_or(EngineError::NoRunnableModel)?;
    agent.selected_suffix_start = index as u32;
    let suffix = agent.fallback_chain[index..].to_vec();
    policy_from_snapshot(
        agent,
        suffix,
        registry,
        runtime,
        tool_output_max_lines,
        tool_output_max_bytes,
    )
}

pub(crate) fn policy_from_snapshot(
    agent: protocol::AgentSnapshot,
    selected_suffix: Vec<protocol::FrozenModelBinding>,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    tool_output_max_lines: usize,
    tool_output_max_bytes: usize,
) -> Result<FrozenRunPolicy, EngineError> {
    if selected_suffix.is_empty() {
        return Err(EngineError::NoRunnableModel);
    }
    Ok(FrozenRunPolicy {
        agent,
        selected_suffix: selected_suffix.clone(),
        selected_suffix_wire: selected_suffix,
        runtime,
        registry,
        result_limits: ResultLimits {
            tool_output_max_lines,
            tool_output_max_bytes,
        },
    })
}

pub(crate) fn wire_resolved(binding: &protocol::FrozenModelBinding) -> protocol::ResolvedModelRef {
    let adapter_id = wire_adapter(binding.protocol_recipe.as_str());
    protocol::ResolvedModelRef {
        selection: binding.selection.clone(),
        provider_id: binding.selection.model.provider_id(),
        model_id: binding.selection.model.model_id(),
        adapter_id,
        selection_fingerprint: binding.selection_fingerprint.clone(),
    }
}

pub(crate) type ResolvedRuntimeModel = cookie_agent_models::ResolvedExecutableModel;

pub(crate) fn resolve_model(
    binding: &protocol::FrozenModelBinding,
    runtime: &PublishedRuntime,
) -> Result<ResolvedRuntimeModel, EngineError> {
    let rehydrated = runtime
        .manifests
        .rehydrate(
            binding,
            runtime.models.authored(),
            runtime.models.store(),
            cookie_agent_models::safe_definition_fingerprint,
        )
        .map_err(EngineError::SnapshotRehydration)?;
    let resolved = runtime
        .models
        .resolve_frozen(binding, &rehydrated.blueprint)?;
    if resolved.behavior_fingerprint().as_str() != binding.behavior_fingerprint.as_str() {
        return Err(EngineError::SnapshotRehydration(
            cookie_agent_models::manifests::RehydrationError::SnapshotRehydrationMismatch,
        ));
    }
    Ok(resolved)
}

fn wire_permission(
    rule: &cookie_agent_config::PermissionRule,
) -> Result<protocol::PermissionRule, EngineError> {
    Ok(protocol::PermissionRule {
        id: protocol::SafeCode::new(rule.id.as_str())
            .map_err(|_| EngineError::RuntimeCompileFailed)?,
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
            .map_err(|_| EngineError::RuntimeCompileFailed)?,
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

fn wire_digest(
    value: &cookie_agent_models::Sha256Digest,
) -> Result<protocol::Sha256Digest, EngineError> {
    protocol::Sha256Digest::new(value.as_str()).map_err(|_| EngineError::RuntimeCompileFailed)
}

pub(crate) fn resolve_agent<'a>(
    registry: &'a AgentRegistry,
    id: &protocol::AgentId,
) -> Result<&'a ResolvedAgent, EngineError> {
    registry
        .get(id)
        .ok_or_else(|| EngineError::InvalidRuntimeAgent(id.clone()))
}

fn wire_adapter(value: &str) -> protocol::AdaptorId {
    match cookie_agent_models::adapters::wire_adapter_for_protocol(value)
        .expect("frozen binding contains a reviewed protocol adapter")
    {
        cookie_agent_models::adapters::OvenAdapterFamily::Anthropic => {
            protocol::AdaptorId::Anthropic
        }
        cookie_agent_models::adapters::OvenAdapterFamily::OpenaiChat => {
            protocol::AdaptorId::OpenaiChat
        }
        cookie_agent_models::adapters::OvenAdapterFamily::OpenaiResponses => {
            protocol::AdaptorId::OpenaiResponses
        }
        cookie_agent_models::adapters::OvenAdapterFamily::OpenaiCompatible => {
            protocol::AdaptorId::OpenaiCompatible
        }
        cookie_agent_models::adapters::OvenAdapterFamily::GoogleGemini => {
            protocol::AdaptorId::GoogleGemini
        }
        cookie_agent_models::adapters::OvenAdapterFamily::GoogleVertexGemini => {
            protocol::AdaptorId::GoogleVertexGemini
        }
        cookie_agent_models::adapters::OvenAdapterFamily::AwsBedrockConverse => {
            protocol::AdaptorId::AwsBedrockConverse
        }
        cookie_agent_models::adapters::OvenAdapterFamily::AzureOpenaiChat => {
            protocol::AdaptorId::AzureOpenaiChat
        }
        cookie_agent_models::adapters::OvenAdapterFamily::AzureOpenaiResponses => {
            protocol::AdaptorId::AzureOpenaiResponses
        }
        cookie_agent_models::adapters::OvenAdapterFamily::CohereV2Chat => {
            protocol::AdaptorId::CohereV2Chat
        }
    }
}
