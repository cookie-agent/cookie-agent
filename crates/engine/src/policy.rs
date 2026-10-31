use std::sync::Arc;

use cookie_agent_config::{CacheLayer, CacheTtls};
use cookie_agent_models::adapters::{
    BedrockCachePoint, BedrockCacheStrategy, BedrockCacheTtl, BedrockMessageCachePoint,
    CacheStrategyConfig, GoogleCacheMode, OpenAiCacheMode, OpenAiCacheStrategyConfig,
    OpenAiPromptCacheRetention, OpenAiPromptCacheTtl, OvenAdapterFamily,
};
use cookie_agent_protocol as protocol;

pub(crate) use cookie_agent_engine_store::bindings::{
    ACP_ADAPTER, adapter_family, wire_adapter, wire_resolved,
};

use crate::{
    EngineError,
    model_snapshots::binding_for_selection,
    permissions::MemoryVariables,
    runtime_snapshot::{
        AgentRegistry, PublishedRuntime, ResolvedAgent, ResolvedAgentFallback, delegation_targets,
    },
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResultLimits {
    pub tool_output: crate::preview::PreviewLimits,
}

impl ResultLimits {
    pub(crate) fn from_config(config: &cookie_agent_config::EngineConfig) -> Self {
        Self {
            tool_output: (&config.tool_output).into(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct FreezeOptions {
    pub result_limits: ResultLimits,
    pub model_retry: cookie_agent_config::ModelRetryConfig,
    /// The authored cache entries a delegated child that inherits its
    /// parent's models inherits with them, one per inherited binding.
    pub inherited_authored_caches: Option<Vec<Option<cookie_agent_config::CacheConfig>>>,
}

#[derive(Clone)]
pub(crate) struct FrozenRunPolicy {
    pub agent: protocol::AgentSnapshot,
    pub preset: Option<String>,
    pub internal_agents: Vec<protocol::FrozenInternalAgentDefinition>,
    pub historical_delegation: bool,
    pub goal_tools_enabled: bool,
    /// Where the composed prompt carries its prompt blocks (stable prefix
    /// spec, section 6), recorded as admission composes them.
    pub prompt_blocks: Vec<protocol::PromptBlockSpan>,
    /// Whether admission composed the prompt blocks rather than reusing the
    /// previous run's; `None` when the run records none (an ACP agent's, or
    /// one whose prompt a plugin replaced).
    pub prompt_blocks_live: Option<bool>,
    pub selected_suffix: Vec<protocol::FrozenModelBinding>,
    pub runtime: Arc<PublishedRuntime>,
    pub registry: Arc<AgentRegistry>,
    pub result_limits: ResultLimits,
    pub model_retry: cookie_agent_config::ModelRetryConfig,
    pub cache_strategies: Vec<Option<cookie_agent_models::adapters::CacheStrategyConfig>>,
    /// The agent-level cache entry each binding of the suffix resolved
    /// with, kept for the delegated children that inherit the models.
    pub authored_caches: Vec<Option<cookie_agent_config::CacheConfig>>,
    /// The session layer whose cache defaults the run uses.
    pub cache_layer: CacheLayer,
}

impl std::fmt::Debug for FrozenRunPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrozenRunPolicy")
            .field("agent", &self.agent.agent)
            .field("preset", &self.preset)
            .field("internal_agents", &self.internal_agents.len())
            .field("selected_suffix", &self.selected_suffix)
            .field("runtime_revision", &self.runtime.revisions.runtime_revision)
            .finish_non_exhaustive()
    }
}

impl FrozenRunPolicy {
    pub fn active_suffix(&self, fallback_index: usize) -> &[protocol::FrozenModelBinding] {
        self.selected_suffix
            .get(fallback_index..)
            .unwrap_or_default()
    }

    pub(crate) fn model_capabilities(
        &self,
        binding: &protocol::FrozenModelBinding,
    ) -> Option<protocol::ModelCapabilities> {
        self.runtime
            .result
            .snapshot
            .models
            .iter()
            .find(|model| model.key == binding.selection.model)
            .map(|model| model.capabilities.clone())
    }

    pub(crate) fn cache_strategy(
        &self,
        binding: &protocol::FrozenModelBinding,
        session: protocol::SessionId,
    ) -> Option<cookie_agent_models::adapters::CacheStrategyConfig> {
        let mut strategy = self.raw_cache_strategy(binding)?;
        if let cookie_agent_models::adapters::CacheStrategyConfig::OpenAi(config) = &mut strategy
            && let Some(key) = &mut config.prompt_cache_key
        {
            *key = key.replace("${session_id}", &session.to_string());
        }
        Some(strategy)
    }

    pub(crate) fn raw_cache_strategy(
        &self,
        binding: &protocol::FrozenModelBinding,
    ) -> Option<cookie_agent_models::adapters::CacheStrategyConfig> {
        let index = self
            .selected_suffix
            .iter()
            .position(|candidate| candidate == binding)?;
        self.cache_strategies.get(index)?.clone()
    }

    /// The agent-level cache entry `binding` resolved with, if any.
    pub(crate) fn authored_cache(
        &self,
        binding: &protocol::FrozenModelBinding,
    ) -> Option<cookie_agent_config::CacheConfig> {
        let index = self
            .selected_suffix
            .iter()
            .position(|candidate| candidate == binding)?;
        self.authored_caches.get(index)?.clone()
    }

    pub(crate) fn delegation_target_available(&self, target: &protocol::AgentId) -> bool {
        if self.historical_delegation {
            return self
                .agent
                .delegation
                .as_ref()
                .is_some_and(|delegation| delegation.targets.contains(target));
        }
        self.registry.get(target).is_some_and(|agent| {
            agent.document.frontmatter.enabled
                && matches!(
                    agent.document.frontmatter.mode,
                    cookie_agent_config::AgentMode::Subagent | cookie_agent_config::AgentMode::All
                )
        })
    }

    /// Whether the agent's rules name `target` itself as a delegation
    /// target, with an effect other than `deny`, and no agent has it: an
    /// ACP agent whose runtime has not reported it yet (or a typo).
    pub(crate) fn names_unknown_target(&self, target: &protocol::AgentId) -> bool {
        self.registry.get(target).is_none()
            && self.agent.permissions.iter().any(|rule| {
                rule.action == protocol::PermissionAction::DELEGATE
                    && rule.effect != protocol::PermissionEffect::Deny
                    && rule.resource.as_str() == target.as_str()
            })
    }

    /// Why a delegation to `target`, which is not one of the run's
    /// targets, is refused.
    pub(crate) fn delegate_refusal(&self, target: &protocol::AgentId) -> String {
        if self.names_unknown_target(target) {
            format!("no agent `{target}` (no connected runtime offers it)")
        } else {
            "delegate target is not allowed".to_owned()
        }
    }

    pub(crate) fn delegate_targets(&self, depth: u32) -> Vec<protocol::AgentId> {
        let Some(delegation) = &self.agent.delegation else {
            return Vec::new();
        };
        if depth >= delegation.effective_depth_ceiling {
            return Vec::new();
        }
        delegation
            .targets
            .iter()
            .filter(|target| self.delegation_target_available(target))
            .cloned()
            .collect()
    }
}

fn missing_model_error(
    agent: &ResolvedAgent,
    runtime: &crate::runtime_snapshot::PublishedRuntime,
    selection: &protocol::ModelSelection,
) -> Option<EngineError> {
    if runtime.models.model(&selection.model).is_some() {
        return None;
    }
    let error = if runtime
        .models
        .catalog()
        .model(&selection.model.provider_id(), &selection.model.model_id())
        .is_some()
    {
        EngineError::UnavailableAgentModel {
            agent: agent.document.id.clone(),
            model: selection.model.clone(),
        }
    } else {
        EngineError::UnknownAgentModel {
            agent: agent.document.id.clone(),
            model: selection.model.clone(),
        }
    };
    Some(error)
}

/// A selection the live runtime can run: its model is available and its
/// variant is one of that model's own. A variant the model no longer offers
/// becomes the model's default; no variant (base behaviour) stays as it is.
pub(crate) fn live_model_selection(
    runtime: &PublishedRuntime,
    selection: &protocol::ModelSelection,
) -> Option<protocol::ModelSelection> {
    let descriptor = runtime
        .result
        .snapshot
        .models
        .iter()
        .find(|descriptor| descriptor.key == selection.model)?;
    let variant = match &selection.variant {
        Some(variant) if !descriptor.variants.iter().any(|known| &known.id == variant) => {
            descriptor.default_variant.clone()
        }
        variant => variant.clone(),
    };
    Some(protocol::ModelSelection {
        model: selection.model.clone(),
        variant,
    })
}

/// The first live model in an agent's own fallback chain.
pub(crate) fn agent_default_model(
    runtime: &PublishedRuntime,
    agent: &ResolvedAgent,
) -> Option<protocol::ModelSelection> {
    agent
        .resolved_fallback
        .iter()
        .find_map(|candidate| match candidate {
            ResolvedAgentFallback::Selection { selection, .. } => {
                live_model_selection(runtime, selection)
            }
            ResolvedAgentFallback::ParentModel { .. } => None,
        })
}

/// The agent a run falls back to when its own is gone: `[agents] default`,
/// else the first root-runnable agent of the registry.
pub(crate) fn default_root_agent(registry: &AgentRegistry) -> Option<&ResolvedAgent> {
    let runnable = || {
        registry
            .agents()
            .values()
            .filter(|agent| agent.runnable_as_root)
    };
    runnable()
        .find(|agent| agent.document.id == *registry.default_agent())
        .or_else(|| runnable().next())
}

/// Repairs a root selection against the live runtime instead of failing. A
/// selection carried over from history may name a preset, agent, model, or
/// variant that no longer exists: an unknown preset falls back to the shared
/// agents, an agent that cannot run as root to the default agent, and a model
/// that is gone (or any model, once the agent changed) to the agent's default
/// model. The transcript keeps showing what earlier turns actually used.
pub(crate) fn best_effort_root_selection(
    runtime: &PublishedRuntime,
    selection: &protocol::RunSelection,
) -> Result<protocol::RunSelection, EngineError> {
    let (agents, preset) = match runtime.agents_for_preset(selection.preset.as_deref()) {
        Ok(agents) => (agents, selection.preset.clone()),
        Err(_) => (runtime.agents_for_preset(None)?, None),
    };
    // An ACP agent is never swapped for another: when its runtime is away,
    // the run is refused instead (`docs/site/specs/acp-agents.md`
    // section 3).
    if let Some(agent) = agents.get(&selection.agent)
        && agent.acp.is_some()
    {
        return Ok(protocol::RunSelection {
            agent: selection.agent.clone(),
            model: protocol::acp_model_selection(&selection.agent),
            preset,
        });
    }
    let requested = agents
        .get(&selection.agent)
        .filter(|agent| agent.runnable_as_root);
    let agent = requested
        .or_else(|| default_root_agent(&agents))
        .ok_or(EngineError::NoRunnableModel)?;
    let model = requested
        .and_then(|_| live_model_selection(runtime, &selection.model))
        .or_else(|| agent_default_model(runtime, agent))
        .or_else(|| {
            runtime
                .result
                .snapshot
                .models
                .first()
                .map(|descriptor| protocol::ModelSelection {
                    model: descriptor.key.clone(),
                    variant: descriptor.default_variant.clone(),
                })
        })
        .ok_or(EngineError::NoRunnableModel)?;
    Ok(protocol::RunSelection {
        agent: agent.document.id.clone(),
        model,
        preset,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn freeze_root_agent_policy(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    selection: &protocol::ModelSelection,
    max_depth: u32,
    result_limits: ResultLimits,
    model_retry: cookie_agent_config::ModelRetryConfig,
    memory: &MemoryVariables,
) -> Result<FrozenRunPolicy, EngineError> {
    if let Some(error) = missing_model_error(agent, runtime.as_ref(), selection) {
        return Err(error);
    }
    if !agent.runnable_as_root {
        return Err(EngineError::NoRunnableModel);
    }
    let start = agent
        .resolved_fallback
        .iter()
        .position(|candidate| {
            matches!(candidate, ResolvedAgentFallback::Selection { selection: candidate, .. } if candidate.model == selection.model)
        });
    let authored = start.map_or(agent.resolved_fallback.as_slice(), |index| {
        &agent.resolved_fallback[index..]
    });
    let mut selections = authored
        .iter()
        .filter_map(|candidate| match candidate {
            ResolvedAgentFallback::Selection {
                selection: candidate,
                ..
            } if runtime.models.model(&candidate.model).is_some_and(|model| {
                model.model.status == cookie_agent_models::compiler::CompiledModelStatus::Available
            }) =>
            {
                Some(candidate.clone())
            }
            ResolvedAgentFallback::Selection { .. } | ResolvedAgentFallback::ParentModel { .. } => {
                None
            }
        })
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
        Some(max_depth),
        FreezeOptions {
            result_limits,
            model_retry,
            inherited_authored_caches: None,
        },
        memory,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn freeze_delegated_agent_policy(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    selection: &protocol::ModelSelection,
    inherited_suffix: &[protocol::FrozenModelBinding],
    inherited_depth_ceiling: u32,
    options: FreezeOptions,
    memory: &MemoryVariables,
) -> Result<FrozenRunPolicy, EngineError> {
    if let Some(error) = missing_model_error(agent, runtime.as_ref(), selection) {
        return Err(error);
    }
    let bindings = if agent.resolved_fallback.is_empty() {
        if inherited_suffix.first().map(|binding| &binding.selection) != Some(selection) {
            return Err(EngineError::NoRunnableModel);
        }
        inherited_suffix.to_vec()
    } else {
        let index = agent
            .resolved_fallback
            .iter()
            .position(|candidate| {
                matches!(candidate, ResolvedAgentFallback::Selection { selection: candidate, .. } if candidate.model == selection.model)
            })
            .ok_or(EngineError::NoRunnableModel)?;
        let mut selections = agent.resolved_fallback[index..]
            .iter()
            .filter_map(|candidate| match candidate {
                ResolvedAgentFallback::Selection {
                    selection: candidate,
                    ..
                } if runtime.models.model(&candidate.model).is_some_and(|model| {
                    model.model.status
                        == cookie_agent_models::compiler::CompiledModelStatus::Available
                }) =>
                {
                    Some(candidate.clone())
                }
                ResolvedAgentFallback::Selection { .. }
                | ResolvedAgentFallback::ParentModel { .. } => None,
            })
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
        options,
        Delegated(true),
        memory,
    )
}

#[allow(clippy::too_many_arguments)]
fn freeze_planned_policy(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    selections: Vec<protocol::ModelSelection>,
    selection: &protocol::ModelSelection,
    inherited_depth_ceiling: Option<u32>,
    options: FreezeOptions,
    memory: &MemoryVariables,
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
        options,
        Delegated(false),
        memory,
    )
}

/// Whether a policy is frozen for a delegated run, whose session's origin
/// is `SessionOrigin::Delegated`: such a run gets the delegated memory
/// default (`docs/site/specs/memory.md` section 16.9.11) and the cache
/// defaults of [`CacheLayer::Delegated`].
#[derive(Clone, Copy)]
struct Delegated(bool);

#[allow(clippy::too_many_arguments)]
fn freeze_with_bindings(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    bindings: Vec<protocol::FrozenModelBinding>,
    selection: &protocol::ModelSelection,
    inherited_depth_ceiling: Option<u32>,
    options: FreezeOptions,
    Delegated(delegated): Delegated,
    memory: &MemoryVariables,
) -> Result<FrozenRunPolicy, EngineError> {
    if bindings.is_empty() {
        return Err(EngineError::NoRunnableModel);
    }
    let document = &agent.document;
    let targets = delegation_targets(&document.frontmatter.permissions, registry.agents());
    let delegation = (!targets.is_empty()).then(|| protocol::FrozenDelegationPolicy {
        targets,
        effective_depth_ceiling: inherited_depth_ceiling.unwrap_or_default(),
    });
    let snapshot = protocol::AgentSnapshot {
        agent: document.id.clone(),
        mode: document.frontmatter.mode,
        description: document.frontmatter.description.clone(),
        document_source: document.source,
        document_fingerprint: wire_digest(&document.document_fingerprint)?,
        composed_prompt: document.body.clone(),
        prompt_fingerprint: wire_digest(&document.prompt_fingerprint)?,
        max_output_tokens: document.frontmatter.limits.max_output_tokens,
        permissions: crate::permissions::agent_permission_rules(document, delegated, memory),
        delegation,
        fallback_chain: bindings
            .iter()
            .map(persisted_binding)
            .collect::<Result<_, _>>()?,
        selected_suffix_start: 0,
    };
    let run_selection = protocol::RunSelection {
        agent: document.id.clone(),
        model: selection.clone(),
        preset: registry.preset().map(str::to_owned),
    };
    snapshot
        .validate_selection(&run_selection)
        .map_err(|_| EngineError::NoRunnableModel)?;
    let cache_layer = if delegated {
        CacheLayer::Delegated
    } else {
        CacheLayer::Root
    };
    let authored_caches = if agent.resolved_fallback.is_empty() {
        options
            .inherited_authored_caches
            .clone()
            .filter(|caches| caches.len() == bindings.len())
            .unwrap_or_else(|| vec![None; bindings.len()])
    } else {
        bindings
            .iter()
            .map(|binding| authored_cache(Some(agent), binding).cloned())
            .collect()
    };
    let cache_strategies = bindings
        .iter()
        .zip(&authored_caches)
        .map(|(binding, authored)| {
            resolve_authored_cache_strategy(authored.as_ref(), binding, &runtime, cache_layer)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let preset = registry.preset().map(str::to_owned);
    Ok(FrozenRunPolicy {
        agent: snapshot,
        preset,
        internal_agents: Vec::new(),
        historical_delegation: false,
        goal_tools_enabled: false,
        prompt_blocks: Vec::new(),
        prompt_blocks_live: None,
        selected_suffix: bindings,
        runtime,
        registry,
        result_limits: options.result_limits,
        model_retry: options.model_retry,
        cache_strategies,
        authored_caches,
        cache_layer,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn policy_from_snapshot(
    agent: protocol::AgentSnapshot,
    selected_suffix: Vec<protocol::FrozenModelBinding>,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    result_limits: ResultLimits,
    model_retry: cookie_agent_config::ModelRetryConfig,
    cache_layer: CacheLayer,
) -> Result<FrozenRunPolicy, EngineError> {
    if selected_suffix.is_empty() {
        return Err(EngineError::NoRunnableModel);
    }
    let preset = registry.preset().map(str::to_owned);
    let authored_caches = selected_suffix
        .iter()
        .map(|binding| authored_cache(registry.get(&agent.agent), binding).cloned())
        .collect::<Vec<_>>();
    let cache_strategies = selected_suffix
        .iter()
        .zip(&authored_caches)
        .map(|(binding, authored)| {
            resolve_authored_cache_strategy(authored.as_ref(), binding, &runtime, cache_layer)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FrozenRunPolicy {
        agent,
        preset,
        internal_agents: Vec::new(),
        historical_delegation: false,
        goal_tools_enabled: false,
        prompt_blocks: Vec::new(),
        prompt_blocks_live: None,
        selected_suffix,
        runtime,
        registry,
        result_limits,
        model_retry,
        cache_strategies,
        authored_caches,
        cache_layer,
    })
}

/// The revisions of `runtime`, frozen with the model snapshot
/// `manifest_revision`.
pub(crate) fn runtime_revisions(
    runtime: &PublishedRuntime,
    manifest_revision: protocol::ModelSnapshotRevision,
) -> protocol::RuntimeRevisions {
    let revisions = &runtime.revisions;
    protocol::RuntimeRevisions {
        runtime_revision: revisions.runtime_revision.clone(),
        catalog_revision: revisions.catalog_revision.clone(),
        provider_state_revision: revisions.provider_state_revision.clone(),
        model_revision: revisions.model_revision.clone(),
        agent_revision: revisions.agent_revision.clone(),
        recipe_registry_revision: revisions.recipe_registry_revision.clone(),
        manifest_revision,
    }
}

/// The form session history stores `binding` in.
pub(crate) fn persisted_binding(
    binding: &protocol::FrozenModelBinding,
) -> Result<protocol::PersistedModelBinding, EngineError> {
    protocol::PersistedModelBinding::new(binding, wire_resolved(binding))
        .map_err(|_| EngineError::RuntimeCompileFailed)
}

/// Decodes the complete bindings of stored fallback entries, failing when
/// any one is not a binding this build can run.
pub(crate) fn decode_bindings(
    bindings: &[protocol::PersistedModelBinding],
) -> Result<Vec<protocol::FrozenModelBinding>, EngineError> {
    bindings.iter().map(decode_binding).collect()
}

/// The complete binding of a stored entry, when this build can run it: it
/// decodes and speaks an adapter this build has.
pub(crate) fn decode_binding(
    binding: &protocol::PersistedModelBinding,
) -> Result<protocol::FrozenModelBinding, EngineError> {
    binding
        .decode()
        .ok()
        .filter(|decoded| {
            cookie_agent_models::adapters::wire_adapter_for_protocol(
                decoded.descriptor.adapter_id.as_str(),
            )
            .is_some()
        })
        .ok_or_else(|| EngineError::UnreadableModelBinding(binding.selection.model.clone()))
}

/// The cache strategy of `binding` for a run of `layer`, from the agent's
/// cache entry for it, the provider's `cache` table, or the family defaults
/// of `layer`.
pub(crate) fn resolve_cache_strategy(
    agent: Option<&ResolvedAgent>,
    binding: &protocol::FrozenModelBinding,
    runtime: &PublishedRuntime,
    layer: CacheLayer,
) -> Result<Option<CacheStrategyConfig>, EngineError> {
    resolve_authored_cache_strategy(authored_cache(agent, binding), binding, runtime, layer)
}

/// The agent-level cache entry that applies to `binding`: the entry of the
/// exact selection, else of the same model, else of the parent-model entry.
fn authored_cache<'a>(
    agent: Option<&'a ResolvedAgent>,
    binding: &protocol::FrozenModelBinding,
) -> Option<&'a cookie_agent_config::CacheConfig> {
    agent.and_then(|agent| {
        let exact = agent
            .resolved_fallback
            .iter()
            .find_map(|fallback| match fallback {
                ResolvedAgentFallback::Selection { selection, cache }
                    if selection == &binding.selection =>
                {
                    Some(cache)
                }
                ResolvedAgentFallback::Selection { .. }
                | ResolvedAgentFallback::ParentModel { .. } => None,
            });
        let same_model = || {
            agent
                .resolved_fallback
                .iter()
                .find_map(|fallback| match fallback {
                    ResolvedAgentFallback::Selection { selection, cache }
                        if selection.model == binding.selection.model =>
                    {
                        Some(cache)
                    }
                    ResolvedAgentFallback::Selection { .. }
                    | ResolvedAgentFallback::ParentModel { .. } => None,
                })
        };
        let parent = || {
            agent
                .resolved_fallback
                .iter()
                .find_map(|fallback| match fallback {
                    ResolvedAgentFallback::ParentModel { cache, .. } => Some(cache),
                    ResolvedAgentFallback::Selection { .. } => None,
                })
        };
        exact
            .or_else(same_model)
            .or_else(parent)
            .and_then(Option::as_ref)
    })
}

/// What `[approval] cache_ttl` did on a reviewer model's provider, for
/// configuration status (approval reviewer spec section 5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReviewerCacheNote {
    /// The provider does not offer the TTL asked for: the reviewer caches
    /// for `used`.
    Substituted {
        used: &'static str,
        why: &'static str,
    },
    /// The provider caches on its own terms: the knob changes nothing.
    NoEffect(&'static str),
}

/// The provider's TTL nearest `asked` by ratio, on a log scale; a tie goes
/// to the longer one.
fn nearest_ttl(asked: u32, offered: &[(u32, &'static str)]) -> (u32, &'static str) {
    let distance = |minutes: u32| (f64::from(minutes) / f64::from(asked)).ln().abs();
    offered
        .iter()
        .copied()
        .fold(None::<(u32, &'static str)>, |best, candidate| match best {
            Some(best)
                if distance(best.0) < distance(candidate.0) - 1e-9
                    || ((distance(best.0) - distance(candidate.0)).abs() <= 1e-9
                        && best.0 > candidate.0) =>
            {
                Some(best)
            }
            _ => Some(candidate),
        })
        .expect("a provider offers a TTL")
}

/// The approval reviewer's cache strategy for `binding` under `ttl`, from
/// `provider`, the strategy its provider's cache configuration gives the
/// reviewed session's layer (approval reviewer spec section 5).
pub(crate) fn reviewer_cache_strategy(
    provider: Option<CacheStrategyConfig>,
    binding: &protocol::FrozenModelBinding,
    ttl: cookie_agent_config::ApprovalCacheTtl,
) -> (Option<CacheStrategyConfig>, Option<ReviewerCacheNote>) {
    use cookie_agent_models::adapters::{AnthropicCacheStrategyConfig, AnthropicCacheTtlConfig};
    let Some(asked) = ttl.minutes() else {
        if ttl == cookie_agent_config::ApprovalCacheTtl::Provider {
            return (provider, None);
        }
        // `off`.
        return match provider {
            Some(CacheStrategyConfig::Anthropic(_)) => (
                Some(CacheStrategyConfig::Anthropic(
                    AnthropicCacheStrategyConfig::default(),
                )),
                None,
            ),
            Some(CacheStrategyConfig::Bedrock(_)) => (
                Some(CacheStrategyConfig::Bedrock(BedrockCacheStrategy {
                    system: None,
                    tools: None,
                    messages: Vec::new(),
                })),
                None,
            ),
            Some(CacheStrategyConfig::OpenAi(mut strategy)) if strategy.mode.is_some() => {
                // Explicit mode with no breakpoint writes no cache.
                strategy.mode = Some(OpenAiCacheMode::Explicit);
                strategy.system = false;
                strategy.rolling = false;
                (Some(CacheStrategyConfig::OpenAi(strategy)), None)
            }
            other => (
                other,
                reviewer_cache_no_effect(binding).or(Some(ReviewerCacheNote::NoEffect(
                    "its provider's automatic caching cannot be turned off",
                ))),
            ),
        };
    };
    let structural = [(5, "5m"), (60, "1h")];
    match provider {
        Some(CacheStrategyConfig::Anthropic(provider)) => {
            let (minutes, used) = nearest_ttl(asked, &structural);
            let note = (minutes != asked).then_some(ReviewerCacheNote::Substituted {
                used,
                why: "the nearest TTL its provider offers",
            });
            let one_hour = minutes == 60;
            // A one-hour marker cannot follow a five-minute one.
            let raise = |ttl: Option<AnthropicCacheTtlConfig>| match ttl {
                Some(AnthropicCacheTtlConfig::FiveMinutes) if one_hour => {
                    Some(AnthropicCacheTtlConfig::OneHour)
                }
                other => other,
            };
            (
                Some(CacheStrategyConfig::Anthropic(
                    AnthropicCacheStrategyConfig {
                        system: raise(provider.system),
                        tools: raise(provider.tools),
                        rolling: Some(if one_hour {
                            AnthropicCacheTtlConfig::OneHour
                        } else {
                            AnthropicCacheTtlConfig::FiveMinutes
                        }),
                    },
                )),
                note,
            )
        }
        Some(CacheStrategyConfig::Bedrock(provider)) => {
            let supports_one_hour = cookie_agent_models::adapters::bedrock_supports_one_hour_cache(
                wire_model_id(binding),
            );
            let (minutes, note) = if supports_one_hour {
                let (minutes, used) = nearest_ttl(asked, &structural);
                (
                    minutes,
                    (minutes != asked).then_some(ReviewerCacheNote::Substituted {
                        used,
                        why: "the nearest TTL its provider offers",
                    }),
                )
            } else {
                (
                    5,
                    (asked != 5).then_some(ReviewerCacheNote::Substituted {
                        used: "5m",
                        why: "the model takes no one-hour cache TTL on Bedrock",
                    }),
                )
            };
            let ttl = if minutes == 60 {
                BedrockCacheTtl::OneHour
            } else {
                BedrockCacheTtl::FiveMinutes
            };
            let raise = |point: Option<BedrockCachePoint>| {
                point.map(|point| match point.ttl {
                    Some(BedrockCacheTtl::FiveMinutes) | None if minutes == 60 => {
                        BedrockCachePoint {
                            ttl: Some(BedrockCacheTtl::OneHour),
                        }
                    }
                    _ => point,
                })
            };
            (
                Some(CacheStrategyConfig::Bedrock(BedrockCacheStrategy {
                    system: raise(provider.system),
                    tools: raise(provider.tools),
                    messages: vec![BedrockMessageCachePoint {
                        history_index: usize::MAX,
                        cache_point: BedrockCachePoint { ttl: Some(ttl) },
                    }],
                })),
                note,
            )
        }
        Some(CacheStrategyConfig::OpenAi(mut strategy)) if strategy.mode.is_some() => {
            let responses = matches!(
                reviewer_family(binding),
                Some(OvenAdapterFamily::OpenaiResponses)
            );
            let (minutes, used) = if responses {
                nearest_ttl(asked, &[(30, "30m"), (720, "12h")])
            } else {
                (30, "30m")
            };
            let note = (minutes != asked).then_some(ReviewerCacheNote::Substituted {
                used,
                why: if responses {
                    "the nearest TTL its provider offers"
                } else {
                    "the only TTL its provider offers"
                },
            });
            strategy.ttl = Some(if minutes == 720 {
                OpenAiPromptCacheTtl::TwelveHours
            } else {
                OpenAiPromptCacheTtl::ThirtyMinutes
            });
            strategy.rolling = true;
            (Some(CacheStrategyConfig::OpenAi(strategy)), note)
        }
        other => (other, reviewer_cache_no_effect(binding)),
    }
}

/// The adapter family of `binding`.
fn reviewer_family(binding: &protocol::FrozenModelBinding) -> Option<OvenAdapterFamily> {
    if binding
        .descriptor
        .adapter_id
        .as_str()
        .starts_with("oven.openai-compatible.")
    {
        return Some(OvenAdapterFamily::OpenaiCompatible);
    }
    cookie_agent_models::adapters::wire_adapter_for_protocol(binding.descriptor.adapter_id.as_str())
}

/// Why `[approval] cache_ttl` changes nothing on `binding`'s provider.
fn reviewer_cache_no_effect(binding: &protocol::FrozenModelBinding) -> Option<ReviewerCacheNote> {
    Some(ReviewerCacheNote::NoEffect(
        match reviewer_family(binding)? {
            OvenAdapterFamily::OpenaiChat
            | OvenAdapterFamily::OpenaiResponses
            | OvenAdapterFamily::AzureOpenaiChat
            | OvenAdapterFamily::AzureOpenaiResponses => {
                "its provider caches automatically unless its cache table enables the GPT-5.6 controls"
            }
            OvenAdapterFamily::OpenaiCompatible => "its provider manages caching itself",
            OvenAdapterFamily::GoogleGemini | OvenAdapterFamily::GoogleVertexGemini => {
                "Gemini caches implicitly"
            }
            OvenAdapterFamily::CohereV2Chat => "Cohere caches automatically on its servers",
            OvenAdapterFamily::Anthropic
            | OvenAdapterFamily::AnthropicCompatible
            | OvenAdapterFamily::AwsBedrockConverse => return None,
        },
    ))
}

fn resolve_authored_cache_strategy(
    authored: Option<&cookie_agent_config::CacheConfig>,
    binding: &protocol::FrozenModelBinding,
    runtime: &PublishedRuntime,
    layer: CacheLayer,
) -> Result<Option<CacheStrategyConfig>, EngineError> {
    if let Some(config) = authored {
        let configured_sections = usize::from(config.anthropic.is_some())
            + usize::from(config.bedrock.is_some())
            + usize::from(config.openai.is_some());
        if configured_sections > 1 {
            return Err(EngineError::CacheStrategy(
                "an agent model cache entry must configure only its provider family".into(),
            ));
        }
    }
    let family = if binding
        .descriptor
        .adapter_id
        .as_str()
        .starts_with("oven.openai-compatible.")
    {
        OvenAdapterFamily::OpenaiCompatible
    } else {
        cookie_agent_models::adapters::wire_adapter_for_protocol(
            binding.descriptor.adapter_id.as_str(),
        )
        .ok_or_else(|| EngineError::CacheStrategy("model adapter family is unavailable".into()))?
    };
    if let Some(config) = authored {
        let matches_family = match family {
            OvenAdapterFamily::Anthropic | OvenAdapterFamily::AnthropicCompatible => {
                config.anthropic.is_some()
            }
            OvenAdapterFamily::AwsBedrockConverse => config.bedrock.is_some(),
            OvenAdapterFamily::GoogleGemini | OvenAdapterFamily::GoogleVertexGemini => false,
            OvenAdapterFamily::OpenaiChat
            | OvenAdapterFamily::OpenaiResponses
            | OvenAdapterFamily::AzureOpenaiChat
            | OvenAdapterFamily::AzureOpenaiResponses => config.openai.is_some(),
            OvenAdapterFamily::OpenaiCompatible | OvenAdapterFamily::CohereV2Chat => false,
        };
        let has_section =
            config.anthropic.is_some() || config.bedrock.is_some() || config.openai.is_some();
        if has_section && !matches_family {
            return Err(EngineError::CacheStrategy(format!(
                "cache strategy does not match the {} model adapter family",
                family.id()
            )));
        }
    }
    let provider_cache = runtime
        .models
        .authored()
        .get(&binding.selection.model.provider_id())
        .and_then(|provider| match provider {
            cookie_agent_models::ProviderDefinition::ModelsDev(provider)
            | cookie_agent_models::ProviderDefinition::ChatGpt(provider) => provider.cache.as_ref(),
            cookie_agent_models::ProviderDefinition::Custom(provider) => provider.cache.as_ref(),
        });
    let strategy = match family {
        OvenAdapterFamily::Anthropic | OvenAdapterFamily::AnthropicCompatible => {
            let config = if let Some(authored) = authored {
                let Some(config) = authored.anthropic.clone() else {
                    return Ok(None);
                };
                config
            } else if let Some(cache) = provider_cache {
                cache.anthropic().map_err(cache_strategy_error)?
            } else {
                cookie_agent_config::AnthropicCacheConfig::default()
            };
            // The one-hour TTL is generally available on the Anthropic API
            // and takes no beta header (release notes, 2025-08-13).
            let ttls = resolved_ttls(&config, CacheTtls::defaults(layer, true))?;
            Some(CacheStrategyConfig::Anthropic(
                cookie_agent_models::adapters::AnthropicCacheStrategyConfig {
                    system: cache_ttl(ttls.system),
                    tools: cache_ttl(ttls.tools),
                    rolling: cache_ttl(ttls.rolling),
                },
            ))
        }
        OvenAdapterFamily::AwsBedrockConverse => {
            let config = if let Some(authored) = authored {
                let Some(config) = authored.bedrock.clone() else {
                    return Ok(None);
                };
                config
            } else if let Some(cache) = provider_cache {
                cache.bedrock().map_err(cache_strategy_error)?
            } else {
                cookie_agent_config::BedrockCacheConfig::default()
            };
            let one_hour = cookie_agent_models::adapters::bedrock_supports_one_hour_cache(
                wire_model_id(binding),
            );
            let ttls = resolved_ttls(&config, CacheTtls::defaults(layer, one_hour))?;
            Some(CacheStrategyConfig::Bedrock(bedrock_strategy(ttls)))
        }
        OvenAdapterFamily::GoogleGemini | OvenAdapterFamily::GoogleVertexGemini => None,
        OvenAdapterFamily::OpenaiChat
        | OvenAdapterFamily::OpenaiResponses
        | OvenAdapterFamily::AzureOpenaiChat
        | OvenAdapterFamily::AzureOpenaiResponses => {
            let config = if let Some(authored) = authored {
                authored.openai.clone().unwrap_or_default()
            } else if let Some(cache) = provider_cache {
                cache.openai().map_err(cache_strategy_error)?
            } else {
                cookie_agent_config::OpenAiCacheConfig::default()
            };
            let controls = config.gpt_5_6_controls_enabled();
            if controls
                && config.effective_ttl() == cookie_agent_config::OpenAiPromptCacheTtl::TwelveHours
                && family != OvenAdapterFamily::OpenaiResponses
            {
                return Err(EngineError::CacheStrategy(
                    cookie_agent_models::authoring::TWELVE_HOUR_TTL_REQUIRES_RESPONSES.into(),
                ));
            }
            Some(CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
                prompt_cache_key: Some("${session_id}".into()),
                prompt_cache_retention: config.prompt_cache_retention.map(|retention| {
                    match retention {
                        cookie_agent_config::OpenAiPromptCacheRetention::InMemory => {
                            OpenAiPromptCacheRetention::InMemory
                        }
                        cookie_agent_config::OpenAiPromptCacheRetention::TwentyFourHours => {
                            OpenAiPromptCacheRetention::TwentyFourHours
                        }
                    }
                }),
                mode: controls.then(|| match config.effective_mode() {
                    cookie_agent_config::OpenAiCacheMode::Auto => OpenAiCacheMode::Implicit,
                    cookie_agent_config::OpenAiCacheMode::Explicit => OpenAiCacheMode::Explicit,
                }),
                ttl: controls.then(|| match config.effective_ttl() {
                    cookie_agent_config::OpenAiPromptCacheTtl::ThirtyMinutes => {
                        OpenAiPromptCacheTtl::ThirtyMinutes
                    }
                    cookie_agent_config::OpenAiPromptCacheTtl::TwelveHours => {
                        OpenAiPromptCacheTtl::TwelveHours
                    }
                }),
                system: config.system,
                rolling: config.rolling,
            }))
        }
        OvenAdapterFamily::OpenaiCompatible => {
            if authored.is_some() {
                return Err(EngineError::CacheStrategy(format!(
                    "{} does not support an authored cache strategy",
                    family.id()
                )));
            }
            let configured = provider_cache
                .map(cookie_agent_models::ProviderCacheConfig::openai_compatible)
                .transpose()
                .map_err(cache_strategy_error)?
                .and_then(|config| config.prompt_cache_key);
            let prompt_cache_key = match configured.as_deref() {
                None => Some("${session_id}".into()),
                Some("") => None,
                Some(_) => configured,
            };
            Some(CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
                prompt_cache_key,
                prompt_cache_retention: None,
                mode: None,
                ttl: None,
                system: false,
                rolling: false,
            }))
        }
        OvenAdapterFamily::CohereV2Chat => None,
    };
    if let Some(CacheStrategyConfig::OpenAi(config)) = &strategy
        && let Some(key) = &config.prompt_cache_key
    {
        let expanded = key.replace("${session_id}", "00000000-0000-0000-0000-000000000000");
        if expanded.contains("${") || expanded.chars().count() > 64 {
            return Err(EngineError::CacheStrategy(
                "OpenAI prompt_cache_key supports only ${session_id} and must expand to at most 64 characters".into(),
            ));
        }
    }
    Ok(strategy)
}

/// The TTLs `config` selects over `defaults`, refused when an authored
/// one-hour marker follows a five-minute default (a Bedrock model without
/// one-hour support).
fn resolved_ttls(
    config: &cookie_agent_config::StructuralCacheConfig,
    defaults: CacheTtls,
) -> Result<CacheTtls, EngineError> {
    let ttls = config.resolve(defaults);
    ttls.validate().map_err(|error| {
        EngineError::CacheStrategy(format!(
            "{error}; this model's default cache markers are five minutes, so author the \
             earlier markers too"
        ))
    })?;
    Ok(ttls)
}

/// The model ID `binding` sends on the wire.
fn wire_model_id(binding: &protocol::FrozenModelBinding) -> &str {
    binding
        .descriptor
        .provider_metadata
        .get("cookie_agent.wire_model_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(binding.descriptor.identity.model_id.as_str())
}

fn cache_strategy_error(error: String) -> EngineError {
    EngineError::CacheStrategy(error)
}

const fn cache_ttl(
    ttl: cookie_agent_config::CacheTtl,
) -> Option<cookie_agent_models::adapters::AnthropicCacheTtlConfig> {
    match ttl {
        cookie_agent_config::CacheTtl::OneHour => {
            Some(cookie_agent_models::adapters::AnthropicCacheTtlConfig::OneHour)
        }
        cookie_agent_config::CacheTtl::FiveMinutes => {
            Some(cookie_agent_models::adapters::AnthropicCacheTtlConfig::FiveMinutes)
        }
        cookie_agent_config::CacheTtl::Off => None,
    }
}

fn bedrock_cache_point(ttl: cookie_agent_config::CacheTtl) -> Option<BedrockCachePoint> {
    Some(BedrockCachePoint {
        ttl: Some(match ttl {
            cookie_agent_config::CacheTtl::OneHour => BedrockCacheTtl::OneHour,
            cookie_agent_config::CacheTtl::FiveMinutes => BedrockCacheTtl::FiveMinutes,
            cookie_agent_config::CacheTtl::Off => return None,
        }),
    })
}

fn bedrock_strategy(ttls: CacheTtls) -> BedrockCacheStrategy {
    BedrockCacheStrategy {
        system: bedrock_cache_point(ttls.system),
        tools: bedrock_cache_point(ttls.tools),
        messages: bedrock_cache_point(ttls.rolling)
            .map(|cache_point| BedrockMessageCachePoint {
                history_index: usize::MAX,
                cache_point,
            })
            .into_iter()
            .collect(),
    }
}

pub(crate) fn wire_cache_strategies(
    strategies: &[Option<CacheStrategyConfig>],
) -> Result<Vec<Option<protocol::FrozenCacheStrategy>>, EngineError> {
    strategies
        .iter()
        .map(|strategy| strategy.as_ref().map(wire_cache_strategy).transpose())
        .collect()
}

fn wire_cache_strategy(
    strategy: &CacheStrategyConfig,
) -> Result<protocol::FrozenCacheStrategy, EngineError> {
    Ok(match strategy {
        CacheStrategyConfig::Anthropic(strategy) => protocol::FrozenCacheStrategy::Anthropic {
            system: wire_optional_anthropic_ttl(strategy.system),
            tools: wire_optional_anthropic_ttl(strategy.tools),
            rolling: wire_optional_anthropic_ttl(strategy.rolling),
        },
        CacheStrategyConfig::Bedrock(strategy) => protocol::FrozenCacheStrategy::Bedrock {
            system: wire_optional_bedrock_point(strategy.system.as_ref()),
            tools: wire_optional_bedrock_point(strategy.tools.as_ref()),
            messages: strategy
                .messages
                .iter()
                .map(|point| {
                    Ok(protocol::FrozenBedrockMessageCachePoint {
                        history_index: u64::try_from(point.history_index).map_err(|_| {
                            EngineError::CacheStrategy(
                                "Bedrock cache history index exceeds the frozen wire bound".into(),
                            )
                        })?,
                        ttl: wire_optional_bedrock_point(Some(&point.cache_point)),
                    })
                })
                .collect::<Result<Vec<_>, EngineError>>()?,
        },
        CacheStrategyConfig::Google(strategy) => protocol::FrozenCacheStrategy::Google {
            mode: match strategy.mode {
                GoogleCacheMode::Implicit => protocol::FrozenGoogleCacheMode::Implicit,
                GoogleCacheMode::Explicit => protocol::FrozenGoogleCacheMode::Explicit,
                GoogleCacheMode::Off => protocol::FrozenGoogleCacheMode::Off,
            },
            cached_content: strategy.cached_content.clone(),
        },
        CacheStrategyConfig::OpenAi(strategy) => protocol::FrozenCacheStrategy::OpenAi {
            prompt_cache_key: strategy.prompt_cache_key.clone(),
            prompt_cache_retention: strategy.prompt_cache_retention.map(
                |retention| match retention {
                    OpenAiPromptCacheRetention::InMemory => {
                        protocol::FrozenOpenAiCacheRetention::InMemory
                    }
                    OpenAiPromptCacheRetention::TwentyFourHours => {
                        protocol::FrozenOpenAiCacheRetention::TwentyFourHours
                    }
                },
            ),
            mode: strategy.mode.map(|mode| match mode {
                OpenAiCacheMode::Implicit => protocol::FrozenOpenAiCacheMode::Implicit,
                OpenAiCacheMode::Explicit => protocol::FrozenOpenAiCacheMode::Explicit,
            }),
            ttl: strategy.ttl.map(|ttl| match ttl {
                OpenAiPromptCacheTtl::ThirtyMinutes => {
                    protocol::FrozenOpenAiPromptCacheTtl::ThirtyMinutes
                }
                OpenAiPromptCacheTtl::TwelveHours => {
                    protocol::FrozenOpenAiPromptCacheTtl::TwelveHours
                }
            }),
            system: strategy.system.then_some(true),
            rolling: strategy.rolling.then_some(true),
        },
    })
}

const fn wire_optional_anthropic_ttl(
    ttl: Option<cookie_agent_models::adapters::AnthropicCacheTtlConfig>,
) -> protocol::FrozenCacheTtl {
    match ttl {
        Some(cookie_agent_models::adapters::AnthropicCacheTtlConfig::OneHour) => {
            protocol::FrozenCacheTtl::OneHour
        }
        Some(cookie_agent_models::adapters::AnthropicCacheTtlConfig::FiveMinutes) => {
            protocol::FrozenCacheTtl::FiveMinutes
        }
        None => protocol::FrozenCacheTtl::Off,
    }
}

fn wire_optional_bedrock_point(point: Option<&BedrockCachePoint>) -> protocol::FrozenCacheTtl {
    match point.and_then(|point| point.ttl) {
        Some(BedrockCacheTtl::OneHour) => protocol::FrozenCacheTtl::OneHour,
        Some(BedrockCacheTtl::FiveMinutes) => protocol::FrozenCacheTtl::FiveMinutes,
        None if point.is_some() => protocol::FrozenCacheTtl::FiveMinutes,
        None => protocol::FrozenCacheTtl::Off,
    }
}

pub(crate) type ResolvedRuntimeModel = cookie_agent_models::ResolvedExecutableModel;

/// Resolves a binding's model and variant against `runtime`: the runtime a run
/// was admitted with, so every attempt of that run uses the same compiled
/// models. Nothing is rehydrated from storage.
pub(crate) fn resolve_model(
    binding: &protocol::FrozenModelBinding,
    runtime: &PublishedRuntime,
) -> Result<ResolvedRuntimeModel, EngineError> {
    Ok(runtime.models.resolve(&binding.selection)?)
}

/// The output-token cap sent with a request. A nonzero agent `cap` is bounded by the model's
/// output limit; otherwise an authored `generation_options.max_output_tokens` applies, falling
/// back to the model's output limit (catalog models author none, so they send their limit).
pub(crate) fn effective_max_output_tokens(
    binding: &protocol::FrozenModelBinding,
    cap: u64,
) -> Option<u64> {
    let limit = binding.descriptor.capabilities.limits.output;
    if cap == 0 {
        return binding
            .defaults
            .request
            .max_output_tokens
            .map(|default| limit.map_or(default, |limit| default.min(limit)))
            .or(limit);
    }
    Some(limit.map_or(cap, |limit| limit.min(cap)))
}

/// Largest output reservation subtracted from the context window when a model declares no input
/// limit (opencode's output cap). Requests may still ask for more output than this.
const MAX_OUTPUT_RESERVE_TOKENS: u64 = 32_000;

/// The input tokens a request may occupy: the model's separately documented input limit when it
/// has one, otherwise the context window less an output reserve of
/// `min(max_output_tokens, 32000, context / 2)`.
///
/// The reserve is capped at 32,000 so models with very large output limits (128k on a 400k
/// window) do not give up a third of their window to output that rarely materializes, and at half
/// the context so catalog entries whose output limit equals their context (a shared window) keep
/// a usable budget instead of zero; half still leaves the model room to answer.
pub(crate) fn input_token_budget(
    binding: &protocol::FrozenModelBinding,
    max_output_tokens: Option<u64>,
) -> Option<u64> {
    let limits = &binding.descriptor.capabilities.limits;
    let context = limits.context?;
    Some(match limits.input.filter(|input| *input > 0) {
        Some(input) => input.min(context),
        None => {
            let reserve = max_output_tokens
                .unwrap_or(0)
                .min(MAX_OUTPUT_RESERVE_TOKENS)
                .min(context / 2);
            context - reserve
        }
    })
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

/// The media tools may return to a model of `capabilities` behind a frozen
/// binding's protocol adapter `adapter`.
pub(crate) fn tool_input_media(
    adapter: &str,
    capabilities: &protocol::ModelCapabilities,
) -> Vec<protocol::runtime_protocol::ToolInputMedia> {
    cookie_agent_runtime_api::media::tool_input_media(tool_adapter_family(adapter), capabilities)
}

/// The media a model of `capabilities` behind a frozen binding's protocol
/// adapter `adapter` accepts in a user turn.
pub(crate) fn user_input_media(
    adapter: &str,
    capabilities: &protocol::ModelCapabilities,
) -> Vec<protocol::InputMedia> {
    cookie_agent_runtime_api::media::user_input_media(tool_adapter_family(adapter), capabilities)
}

/// The adapter family of a frozen binding's protocol adapter, for media
/// delivery.
fn tool_adapter_family(value: &str) -> cookie_agent_runtime_api::AdapterFamily {
    media_family(adapter_family(value))
}

/// The media delivery family of an adapter family.
pub(crate) fn media_family(
    family: cookie_agent_models::adapters::OvenAdapterFamily,
) -> cookie_agent_runtime_api::AdapterFamily {
    use cookie_agent_models::adapters::OvenAdapterFamily as Oven;
    use cookie_agent_runtime_api::AdapterFamily as Tool;
    match family {
        Oven::Anthropic => Tool::Anthropic,
        Oven::AnthropicCompatible => Tool::AnthropicCompatible,
        Oven::OpenaiChat => Tool::OpenaiChat,
        Oven::OpenaiResponses => Tool::OpenaiResponses,
        Oven::OpenaiCompatible => Tool::OpenaiCompatible,
        Oven::GoogleGemini => Tool::GoogleGemini,
        Oven::GoogleVertexGemini => Tool::GoogleVertexGemini,
        Oven::AwsBedrockConverse => Tool::AwsBedrockConverse,
        Oven::AzureOpenaiChat => Tool::AzureOpenaiChat,
        Oven::AzureOpenaiResponses => Tool::AzureOpenaiResponses,
        Oven::CohereV2Chat => Tool::CohereV2Chat,
    }
}

/// The synthetic model binding an ACP agent's runs record
/// (`docs/site/specs/acp-agents.md` section 6): the model `acp/<agent>`,
/// adapter `acp`, whose complete binding no build can run as a model.
pub(crate) fn acp_binding(
    agent: &protocol::AgentId,
) -> Result<protocol::PersistedModelBinding, EngineError> {
    let selection = protocol::acp_model_selection(agent);
    let fingerprint = protocol::Sha256Digest::of_bytes(
        format!("cookie-agent/acp-binding/v2\0{agent}").as_bytes(),
    );
    let binding = protocol::PersistedModelBinding {
        selection: selection.clone(),
        provider_id: selection.model.provider_id(),
        provider_model_id: selection.model.model_id(),
        adapter: ACP_ADAPTER.to_owned(),
        resolved: protocol::ResolvedModelRef {
            selection: selection.clone(),
            provider_id: selection.model.provider_id(),
            model_id: selection.model.model_id(),
            adapter_id: protocol::AdaptorId::new(ACP_ADAPTER)
                .map_err(|_| EngineError::RuntimeCompileFailed)?,
            selection_fingerprint: fingerprint.clone(),
        },
        selection_fingerprint: fingerprint.clone(),
        blueprint_fingerprint: fingerprint,
        frozen: serde_json::json!({"acp": {"agent": agent}}),
    };
    binding
        .validate()
        .map_err(|_| EngineError::RuntimeCompileFailed)?;
    Ok(binding)
}

/// Freezes a run of ACP agent `agent`, which names no models: its rules
/// (`acp` defaulting to `ask`), its delegation targets, and the synthetic
/// binding of [`acp_binding`]. The policy's model suffix is empty; the ACP
/// runner takes the run instead of the model loop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn freeze_acp_agent_policy(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    depth_ceiling: u32,
    delegated: bool,
    result_limits: ResultLimits,
    model_retry: cookie_agent_config::ModelRetryConfig,
    memory: &MemoryVariables,
) -> Result<(FrozenRunPolicy, protocol::RunSelection), EngineError> {
    let acp = agent
        .acp
        .as_ref()
        .ok_or_else(|| EngineError::InvalidRuntimeAgent(agent.document.id.clone()))?;
    // Which runtime runs it is the session's to say (section 15, naming
    // of 2026-10-23); here it only needs one that could.
    if !acp.any_available() {
        return Err(EngineError::Acp(format!(
            "ACP agent `{}` is unavailable: no runtime that offers it ({}) is connected with it \
             enabled",
            acp.id,
            crate::runtime_snapshot::join_runtimes(&acp.runtimes())
        )));
    }
    let document = &agent.document;
    let permissions = crate::permissions::agent_permission_rules(document, delegated, memory);
    let targets = delegation_targets(&document.frontmatter.permissions, registry.agents());
    let delegation = (!targets.is_empty()).then_some(protocol::FrozenDelegationPolicy {
        targets,
        effective_depth_ceiling: depth_ceiling,
    });
    let composed_prompt = if document.body.trim().is_empty() {
        format!("ACP agent `{}`.\n", acp.id)
    } else {
        document.body.clone()
    };
    let binding = acp_binding(&document.id)?;
    let snapshot = protocol::AgentSnapshot {
        agent: document.id.clone(),
        mode: document.frontmatter.mode,
        description: document.frontmatter.description.clone(),
        document_source: document.source,
        document_fingerprint: wire_digest(&document.document_fingerprint)?,
        prompt_fingerprint: protocol::Sha256Digest::of_bytes(composed_prompt.as_bytes()),
        composed_prompt,
        max_output_tokens: 0,
        permissions,
        delegation,
        fallback_chain: vec![binding.clone()],
        selected_suffix_start: 0,
    };
    let selection = protocol::RunSelection {
        agent: document.id.clone(),
        model: binding.selection,
        preset: registry.preset().map(str::to_owned),
    };
    snapshot
        .validate_selection(&selection)
        .map_err(|_| EngineError::RuntimeCompileFailed)?;
    Ok((
        FrozenRunPolicy {
            agent: snapshot,
            preset: registry.preset().map(str::to_owned),
            internal_agents: Vec::new(),
            historical_delegation: false,
            goal_tools_enabled: false,
            prompt_blocks: Vec::new(),
            prompt_blocks_live: None,
            selected_suffix: Vec::new(),
            runtime,
            registry,
            result_limits,
            model_retry,
            // One per entry of the snapshot's suffix, as history records
            // them: the synthetic binding has none.
            cache_strategies: vec![None],
            authored_caches: vec![None],
            cache_layer: if delegated {
                CacheLayer::Delegated
            } else {
                CacheLayer::Root
            },
        },
        selection,
    ))
}

impl FrozenRunPolicy {
    /// The model selection the run starts with: its first model, or an ACP
    /// agent's synthetic one.
    pub(crate) fn run_model_selection(&self) -> protocol::ModelSelection {
        self.selected_suffix.first().map_or_else(
            || self.agent.selected_suffix()[0].selection.clone(),
            |binding| binding.selection.clone(),
        )
    }

    /// The model snapshot the run is frozen against: its first model's, or
    /// the runtime's current one for an ACP agent.
    pub(crate) fn manifest_revision(&self) -> protocol::ModelSnapshotRevision {
        self.selected_suffix.first().map_or_else(
            || self.runtime.current_manifest.revision.clone(),
            |binding| binding.manifest_revision.clone(),
        )
    }

    /// The ACP agent the run is frozen for, when it is one.
    pub(crate) fn acp_agent(&self) -> Option<&crate::runtime_snapshot::AcpOffers> {
        self.registry
            .get(&self.agent.agent)
            .and_then(|agent| agent.acp.as_ref())
            .filter(|_| self.selected_suffix.is_empty())
    }
}

#[cfg(test)]
mod cache_strategy_tests {
    use super::*;
    use cookie_agent_config::ApprovalCacheTtl;
    use cookie_agent_models::adapters::{AnthropicCacheStrategyConfig, AnthropicCacheTtlConfig};

    /// The test binding on the adapter `recipe`, sending `wire_model`.
    fn reviewer_binding(recipe: &str, wire_model: &str) -> protocol::FrozenModelBinding {
        let mut binding = crate::test_support::model_binding();
        binding.descriptor.adapter_id = oven_sdk::AdapterId::new(recipe);
        binding.descriptor.provider_metadata.insert(
            "cookie_agent.wire_model_id".into(),
            serde_json::Value::String(wire_model.into()),
        );
        binding
    }

    fn anthropic(
        system: Option<AnthropicCacheTtlConfig>,
        tools: Option<AnthropicCacheTtlConfig>,
        rolling: Option<AnthropicCacheTtlConfig>,
    ) -> Option<CacheStrategyConfig> {
        Some(CacheStrategyConfig::Anthropic(
            AnthropicCacheStrategyConfig {
                system,
                tools,
                rolling,
            },
        ))
    }

    fn openai(ttl: Option<OpenAiPromptCacheTtl>) -> Option<CacheStrategyConfig> {
        Some(CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
            prompt_cache_key: Some("${session_id}".into()),
            prompt_cache_retention: None,
            mode: ttl.map(|_| OpenAiCacheMode::Implicit),
            ttl,
            system: ttl.is_some(),
            rolling: false,
        }))
    }

    #[test]
    fn the_nearest_ttl_goes_by_ratio_and_ties_go_longer() {
        let structural = [(5, "5m"), (60, "1h")];
        assert_eq!(nearest_ttl(30, &structural), (60, "1h"));
        assert_eq!(nearest_ttl(720, &structural), (60, "1h"));
        assert_eq!(nearest_ttl(5, &structural), (5, "5m"));
        let responses = [(30, "30m"), (720, "12h")];
        assert_eq!(nearest_ttl(60, &responses), (30, "30m"));
        assert_eq!(nearest_ttl(5, &responses), (30, "30m"));
        assert_eq!(nearest_ttl(720, &responses), (720, "12h"));
        // 10 is as far from 5 as from 20: the longer wins.
        assert_eq!(nearest_ttl(10, &[(5, "5m"), (20, "20m")]), (20, "20m"));
    }

    /// Anthropic (approval reviewer spec section 5): a one-hour rolling
    /// marker raises five-minute system and tools markers; five minutes
    /// leaves them; `off` writes none; `provider` keeps the provider's.
    #[test]
    fn the_reviewer_ttl_on_anthropic() {
        use AnthropicCacheTtlConfig::{FiveMinutes, OneHour};
        let binding = reviewer_binding("oven.anthropic.messages", "claude-haiku-5");
        let provider = anthropic(Some(FiveMinutes), None, Some(FiveMinutes));
        for (ttl, expected, note) in [
            (
                ApprovalCacheTtl::OneHour,
                anthropic(Some(OneHour), None, Some(OneHour)),
                None,
            ),
            (
                ApprovalCacheTtl::ThirtyMinutes,
                anthropic(Some(OneHour), None, Some(OneHour)),
                Some("1h"),
            ),
            (
                ApprovalCacheTtl::TwelveHours,
                anthropic(Some(OneHour), None, Some(OneHour)),
                Some("1h"),
            ),
            (
                ApprovalCacheTtl::FiveMinutes,
                anthropic(Some(FiveMinutes), None, Some(FiveMinutes)),
                None,
            ),
            (ApprovalCacheTtl::Off, anthropic(None, None, None), None),
            (ApprovalCacheTtl::Provider, provider.clone(), None),
        ] {
            let (strategy, got) = reviewer_cache_strategy(provider.clone(), &binding, ttl);
            assert_eq!(strategy, expected, "{ttl:?}");
            assert_eq!(
                got,
                note.map(|used| ReviewerCacheNote::Substituted {
                    used,
                    why: "the nearest TTL its provider offers",
                }),
                "{ttl:?}"
            );
        }
    }

    #[test]
    fn the_reviewer_ttl_on_bedrock_follows_the_models_one_hour_support() {
        let point = |ttl| BedrockCachePoint { ttl: Some(ttl) };
        let provider = Some(CacheStrategyConfig::Bedrock(BedrockCacheStrategy {
            system: Some(point(BedrockCacheTtl::FiveMinutes)),
            tools: Some(point(BedrockCacheTtl::FiveMinutes)),
            messages: Vec::new(),
        }));
        let rolling = |strategy: &Option<CacheStrategyConfig>| match strategy {
            Some(CacheStrategyConfig::Bedrock(strategy)) => (
                strategy.system.as_ref().and_then(|point| point.ttl),
                strategy
                    .messages
                    .iter()
                    .map(|point| point.cache_point.ttl)
                    .collect::<Vec<_>>(),
            ),
            _ => panic!("a Bedrock strategy"),
        };
        let modern = reviewer_binding(
            "oven.bedrock.converse",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        );
        let (strategy, note) =
            reviewer_cache_strategy(provider.clone(), &modern, ApprovalCacheTtl::OneHour);
        assert_eq!(
            rolling(&strategy),
            (
                Some(BedrockCacheTtl::OneHour),
                vec![Some(BedrockCacheTtl::OneHour)]
            )
        );
        assert_eq!(note, None);
        let older = reviewer_binding(
            "oven.bedrock.converse",
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
        );
        let (strategy, note) =
            reviewer_cache_strategy(provider.clone(), &older, ApprovalCacheTtl::OneHour);
        assert_eq!(
            rolling(&strategy),
            (
                Some(BedrockCacheTtl::FiveMinutes),
                vec![Some(BedrockCacheTtl::FiveMinutes)]
            )
        );
        assert!(matches!(
            note,
            Some(ReviewerCacheNote::Substituted { used: "5m", .. })
        ));
        let (strategy, _) = reviewer_cache_strategy(provider, &older, ApprovalCacheTtl::Off);
        assert_eq!(rolling(&strategy), (None, vec![]));
    }

    /// OpenAI GPT-5.6 controls: Responses offers 30m and 12h, Chat
    /// Completions and Azure only 30m; a provider without the controls
    /// caches automatically and the knob changes nothing.
    #[test]
    fn the_reviewer_ttl_on_openai_follows_the_controls_and_the_endpoint() {
        let ttl_of = |strategy: Option<CacheStrategyConfig>| match strategy {
            Some(CacheStrategyConfig::OpenAi(strategy)) => (strategy.ttl, strategy.rolling),
            _ => panic!("an OpenAI strategy"),
        };
        let controls = openai(Some(OpenAiPromptCacheTtl::ThirtyMinutes));
        let responses = reviewer_binding("oven.openai.responses", "gpt-5.6");
        let chat = reviewer_binding("oven.openai.chat", "gpt-5.6");
        let azure = reviewer_binding("oven.azure.openai.responses", "gpt-5.6");
        for (binding, ttl, expected, substituted) in [
            (
                &responses,
                ApprovalCacheTtl::OneHour,
                OpenAiPromptCacheTtl::ThirtyMinutes,
                true,
            ),
            (
                &responses,
                ApprovalCacheTtl::TwelveHours,
                OpenAiPromptCacheTtl::TwelveHours,
                false,
            ),
            (
                &responses,
                ApprovalCacheTtl::ThirtyMinutes,
                OpenAiPromptCacheTtl::ThirtyMinutes,
                false,
            ),
            (
                &chat,
                ApprovalCacheTtl::TwelveHours,
                OpenAiPromptCacheTtl::ThirtyMinutes,
                true,
            ),
            (
                &azure,
                ApprovalCacheTtl::FiveMinutes,
                OpenAiPromptCacheTtl::ThirtyMinutes,
                true,
            ),
        ] {
            let (strategy, note) = reviewer_cache_strategy(controls.clone(), binding, ttl);
            assert_eq!(ttl_of(strategy), (Some(expected), true), "{ttl:?}");
            assert_eq!(note.is_some(), substituted, "{ttl:?}");
        }
        let (strategy, note) = reviewer_cache_strategy(controls, &responses, ApprovalCacheTtl::Off);
        let Some(CacheStrategyConfig::OpenAi(off)) = strategy else {
            panic!("an OpenAI strategy");
        };
        assert_eq!(off.mode, Some(OpenAiCacheMode::Explicit));
        assert!(!off.system && !off.rolling);
        assert_eq!(note, None);

        let automatic = openai(None);
        for ttl in [ApprovalCacheTtl::OneHour, ApprovalCacheTtl::Off] {
            let (strategy, note) = reviewer_cache_strategy(automatic.clone(), &chat, ttl);
            assert_eq!(strategy, automatic);
            assert!(
                matches!(note, Some(ReviewerCacheNote::NoEffect(_))),
                "{ttl:?}"
            );
        }
    }

    #[test]
    fn the_reviewer_ttl_has_no_effect_on_implicit_caches() {
        for recipe in [
            "oven.google.gemini.generate-content",
            "oven.cohere.chat-v2",
            "oven.openai-compatible.chat.local",
        ] {
            let binding = reviewer_binding(recipe, "model");
            let (strategy, note) =
                reviewer_cache_strategy(None, &binding, ApprovalCacheTtl::OneHour);
            assert_eq!(strategy, None, "{recipe}");
            assert!(
                matches!(note, Some(ReviewerCacheNote::NoEffect(_))),
                "{recipe}"
            );
            let (_, note) = reviewer_cache_strategy(None, &binding, ApprovalCacheTtl::Provider);
            assert_eq!(note, None, "{recipe}");
        }
    }

    #[test]
    fn all_off_bedrock_config_emits_no_cache_points() {
        let config: cookie_agent_config::BedrockCacheConfig = serde_json::from_value(
            serde_json::json!({"system":"off","tools":"off","rolling":"off"}),
        )
        .unwrap();
        let ttls = resolved_ttls(&config, CacheTtls::defaults(CacheLayer::Root, true)).unwrap();
        let strategy = bedrock_strategy(ttls);
        assert!(strategy.system.is_none());
        assert!(strategy.tools.is_none());
        assert!(strategy.messages.is_empty());
    }

    #[test]
    fn bedrock_rolling_point_carries_its_resolved_ttl() {
        let config = cookie_agent_config::BedrockCacheConfig::default();
        for (layer, expected) in [
            (CacheLayer::Root, BedrockCacheTtl::OneHour),
            (CacheLayer::Delegated, BedrockCacheTtl::FiveMinutes),
        ] {
            let strategy =
                bedrock_strategy(resolved_ttls(&config, CacheTtls::defaults(layer, true)).unwrap());
            assert_eq!(strategy.messages.len(), 1);
            assert_eq!(strategy.messages[0].history_index, usize::MAX);
            assert_eq!(strategy.messages[0].cache_point.ttl, Some(expected));
        }
    }

    #[test]
    fn bedrock_without_one_hour_support_sends_no_one_hour_default() {
        let config = cookie_agent_config::BedrockCacheConfig::default();
        for layer in [CacheLayer::Root, CacheLayer::Delegated] {
            let strategy = bedrock_strategy(
                resolved_ttls(&config, CacheTtls::defaults(layer, false)).unwrap(),
            );
            let ttls = [
                strategy.system.and_then(|point| point.ttl),
                strategy.tools.and_then(|point| point.ttl),
                strategy.messages[0].cache_point.ttl,
            ];
            assert!(!ttls.contains(&Some(BedrockCacheTtl::OneHour)), "{ttls:?}");
        }
    }

    #[test]
    fn authored_one_hour_after_five_minute_defaults_fails_to_resolve() {
        let config: cookie_agent_config::BedrockCacheConfig =
            serde_json::from_value(serde_json::json!({"rolling":"1h"})).unwrap();
        assert!(matches!(
            resolved_ttls(&config, CacheTtls::defaults(CacheLayer::Root, false)),
            Err(EngineError::CacheStrategy(_))
        ));
        assert!(resolved_ttls(&config, CacheTtls::defaults(CacheLayer::Delegated, true)).is_ok());
    }
}
