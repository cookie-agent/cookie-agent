use std::sync::Arc;

use cookie_agent_models::adapters::{
    BedrockCachePoint, BedrockCacheStrategy, BedrockCacheTtl, BedrockMessageCachePoint,
    CacheStrategyConfig, GoogleCacheMode, GoogleCacheStrategyConfig, OpenAiCacheMode,
    OpenAiCacheStrategyConfig, OpenAiPromptCacheRetention, OpenAiPromptCacheTtl, OvenAdapterFamily,
};
use cookie_agent_protocol as protocol;

use crate::{
    EngineError,
    model_snapshots::binding_for_selection,
    runtime_snapshot::{
        AgentRegistry, PublishedRuntime, ResolvedAgent, ResolvedAgentFallback, delegation_targets,
    },
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResultLimits {
    pub tool_output_max_lines: usize,
    pub tool_output_max_bytes: usize,
}

#[derive(Clone)]
pub(crate) struct FreezeOptions {
    pub result_limits: ResultLimits,
    pub model_retry: cookie_agent_config::ModelRetryConfig,
    pub inherited_cache_strategies:
        Option<Vec<Option<cookie_agent_models::adapters::CacheStrategyConfig>>>,
}

#[derive(Clone)]
pub(crate) struct FrozenRunPolicy {
    pub agent: protocol::AgentSnapshot,
    pub preset: Option<String>,
    pub internal_agents: Vec<protocol::FrozenInternalAgentDefinition>,
    pub historical_delegation: bool,
    pub selected_suffix: Vec<protocol::FrozenModelBinding>,
    pub runtime: Arc<PublishedRuntime>,
    pub registry: Arc<AgentRegistry>,
    pub result_limits: ResultLimits,
    pub model_retry: cookie_agent_config::ModelRetryConfig,
    pub cache_strategies: Vec<Option<cookie_agent_models::adapters::CacheStrategyConfig>>,
}

impl std::fmt::Debug for FrozenRunPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrozenRunPolicy")
            .field("agent", &self.agent.agent)
            .field("preset", &self.preset)
            .field("internal_agents", &self.internal_agents.len())
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn freeze_root_agent_policy(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    selection: &protocol::ModelSelection,
    max_depth: u32,
    result_limits: ResultLimits,
    model_retry: cookie_agent_config::ModelRetryConfig,
) -> Result<FrozenRunPolicy, EngineError> {
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
            inherited_cache_strategies: None,
        },
    )
}

pub(crate) fn freeze_delegated_agent_policy(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    selection: &protocol::ModelSelection,
    inherited_suffix: &[protocol::FrozenModelBinding],
    inherited_depth_ceiling: u32,
    options: FreezeOptions,
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
    )
}

fn freeze_planned_policy(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    selections: Vec<protocol::ModelSelection>,
    selection: &protocol::ModelSelection,
    inherited_depth_ceiling: Option<u32>,
    options: FreezeOptions,
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
    )
}

fn freeze_with_bindings(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    bindings: Vec<protocol::FrozenModelBinding>,
    selection: &protocol::ModelSelection,
    inherited_depth_ceiling: Option<u32>,
    options: FreezeOptions,
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
        schema: protocol::AgentSchemaVersion::current(),
        mode: document.frontmatter.mode,
        description: document.frontmatter.description.clone(),
        document_source: document.source,
        document_fingerprint: wire_digest(&document.document_fingerprint)?,
        composed_prompt: document.body.clone(),
        prompt_fingerprint: wire_digest(&document.prompt_fingerprint)?,
        max_output_tokens: document.frontmatter.limits.max_output_tokens,
        permissions: document
            .frontmatter
            .permissions
            .iter()
            .flat_map(|(action, value)| value.rules(*action))
            .collect(),
        delegation,
        fallback_chain: bindings.clone(),
        selected_suffix_start: 0,
    };
    let run_selection = protocol::RunSelection {
        agent: document.id.clone(),
        model: selection.clone(),
        preset: registry.preset().map(str::to_owned),
    };
    snapshot
        .validate_selected_suffix(&run_selection, &bindings)
        .map_err(|_| EngineError::NoRunnableModel)?;
    let cache_strategies = if agent.resolved_fallback.is_empty() {
        if let Some(strategies) = options
            .inherited_cache_strategies
            .clone()
            .filter(|strategies| strategies.len() == bindings.len())
        {
            strategies
        } else {
            bindings
                .iter()
                .map(|binding| resolve_cache_strategy(None, binding, &runtime))
                .collect::<Result<Vec<_>, _>>()?
        }
    } else {
        bindings
            .iter()
            .map(|binding| resolve_cache_strategy(Some(agent), binding, &runtime))
            .collect::<Result<Vec<_>, _>>()?
    };
    let preset = registry.preset().map(str::to_owned);
    Ok(FrozenRunPolicy {
        agent: snapshot,
        preset,
        internal_agents: Vec::new(),
        historical_delegation: false,
        selected_suffix: bindings,
        runtime,
        registry,
        result_limits: options.result_limits,
        model_retry: options.model_retry,
        cache_strategies,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn policy_for_session_selection(
    mut agent: protocol::AgentSnapshot,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    selection: &protocol::RunSelection,
    tool_output_max_lines: usize,
    tool_output_max_bytes: usize,
    model_retry: cookie_agent_config::ModelRetryConfig,
    frozen_cache_strategies: Option<&[Option<protocol::FrozenCacheStrategy>]>,
) -> Result<FrozenRunPolicy, EngineError> {
    if selection.agent != agent.agent || selection.preset.as_deref() != registry.preset() {
        return Err(EngineError::NoRunnableModel);
    }
    let index = agent
        .fallback_chain
        .iter()
        .position(|binding| binding.selection == selection.model)
        .ok_or(EngineError::NoRunnableModel)?;
    let restored_cache_strategies = match frozen_cache_strategies {
        Some(strategies) if strategies.len() == agent.fallback_chain.len() => {
            Some(runtime_cache_strategies(&strategies[index..])?)
        }
        Some([]) | None => None,
        Some(_) => {
            return Err(EngineError::CacheStrategy(
                "frozen cache strategies do not align with the delegated model suffix".into(),
            ));
        }
    };
    agent.selected_suffix_start = index as u32;
    let suffix = agent.fallback_chain[index..].to_vec();
    let mut policy = policy_from_snapshot(
        agent,
        suffix,
        registry,
        runtime,
        tool_output_max_lines,
        tool_output_max_bytes,
        model_retry,
    )?;
    if let Some(strategies) = restored_cache_strategies {
        policy.cache_strategies = strategies;
    }
    Ok(policy)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn policy_from_snapshot(
    agent: protocol::AgentSnapshot,
    selected_suffix: Vec<protocol::FrozenModelBinding>,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    tool_output_max_lines: usize,
    tool_output_max_bytes: usize,
    model_retry: cookie_agent_config::ModelRetryConfig,
) -> Result<FrozenRunPolicy, EngineError> {
    if selected_suffix.is_empty() {
        return Err(EngineError::NoRunnableModel);
    }
    let preset = registry.preset().map(str::to_owned);
    let cache_strategies = selected_suffix
        .iter()
        .map(|binding| resolve_cache_strategy(registry.get(&agent.agent), binding, &runtime))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FrozenRunPolicy {
        agent,
        preset,
        internal_agents: Vec::new(),
        historical_delegation: false,
        selected_suffix,
        runtime,
        registry,
        result_limits: ResultLimits {
            tool_output_max_lines,
            tool_output_max_bytes,
        },
        model_retry,
        cache_strategies,
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

pub(crate) fn resolve_cache_strategy(
    agent: Option<&ResolvedAgent>,
    binding: &protocol::FrozenModelBinding,
    runtime: &PublishedRuntime,
) -> Result<Option<CacheStrategyConfig>, EngineError> {
    let authored = agent.and_then(|agent| {
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
                    ResolvedAgentFallback::ParentModel { cache } => Some(cache),
                    ResolvedAgentFallback::Selection { .. } => None,
                })
        };
        exact
            .or_else(same_model)
            .or_else(parent)
            .and_then(Option::as_ref)
    });
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
    let family =
        cookie_agent_models::adapters::wire_adapter_for_protocol(binding.protocol_recipe.as_str())
            .ok_or_else(|| {
                EngineError::CacheStrategy("model adapter family is unavailable".into())
            })?;
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
            cookie_agent_models::ProviderDefinition::ModelsDev(provider) => provider.cache.as_ref(),
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
            if config.explicitly_requests_one_hour()
                && !matches!(
                    &binding.options,
                    protocol::ProviderOptions::Anthropic { beta, .. }
                        if beta.iter().any(|value| value == "extended-cache-ttl-2025-04-11")
                )
            {
                return Err(EngineError::CacheStrategy(
                    "explicit Anthropic 1h caching requires beta extended-cache-ttl-2025-04-11"
                        .into(),
                ));
            }
            Some(CacheStrategyConfig::Anthropic(
                cookie_agent_models::adapters::AnthropicCacheStrategyConfig {
                    system: cache_ttl(config.system),
                    tools: cache_ttl(config.tools),
                    rolling: match config.rolling {
                        cookie_agent_config::RollingCacheTtl::FiveMinutes => Some(
                            cookie_agent_models::adapters::AnthropicCacheTtlConfig::FiveMinutes,
                        ),
                        cookie_agent_config::RollingCacheTtl::Off => None,
                    },
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
            Some(CacheStrategyConfig::Bedrock(bedrock_strategy(&config)))
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

fn bedrock_strategy(config: &cookie_agent_config::BedrockCacheConfig) -> BedrockCacheStrategy {
    let messages = match config.rolling {
        cookie_agent_config::RollingCacheTtl::FiveMinutes => vec![BedrockMessageCachePoint {
            history_index: usize::MAX,
            cache_point: BedrockCachePoint {
                ttl: Some(BedrockCacheTtl::FiveMinutes),
            },
        }],
        cookie_agent_config::RollingCacheTtl::Off => Vec::new(),
    };
    BedrockCacheStrategy {
        system: bedrock_cache_point(config.system),
        tools: bedrock_cache_point(config.tools),
        messages,
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
            }),
            system: strategy.system.then_some(true),
            rolling: strategy.rolling.then_some(true),
        },
    })
}

pub(crate) fn runtime_cache_strategies(
    strategies: &[Option<protocol::FrozenCacheStrategy>],
) -> Result<Vec<Option<CacheStrategyConfig>>, EngineError> {
    strategies
        .iter()
        .map(|strategy| strategy.as_ref().map(runtime_cache_strategy).transpose())
        .collect()
}

fn runtime_cache_strategy(
    strategy: &protocol::FrozenCacheStrategy,
) -> Result<CacheStrategyConfig, EngineError> {
    strategy
        .validate()
        .map_err(|error| EngineError::CacheStrategy(error.into()))?;
    Ok(match strategy {
        protocol::FrozenCacheStrategy::Anthropic {
            system,
            tools,
            rolling,
        } => CacheStrategyConfig::Anthropic(
            cookie_agent_models::adapters::AnthropicCacheStrategyConfig {
                system: runtime_anthropic_ttl(*system),
                tools: runtime_anthropic_ttl(*tools),
                rolling: runtime_anthropic_ttl(*rolling),
            },
        ),
        protocol::FrozenCacheStrategy::Bedrock {
            system,
            tools,
            messages,
        } => CacheStrategyConfig::Bedrock(BedrockCacheStrategy {
            system: runtime_bedrock_point(*system),
            tools: runtime_bedrock_point(*tools),
            messages: messages
                .iter()
                .map(|point| {
                    Ok(BedrockMessageCachePoint {
                        history_index: usize::try_from(point.history_index).map_err(|_| {
                            EngineError::CacheStrategy(
                                "frozen Bedrock cache history index exceeds this platform".into(),
                            )
                        })?,
                        cache_point: runtime_bedrock_point(point.ttl).ok_or_else(|| {
                            EngineError::CacheStrategy(
                                "frozen Bedrock message cache point cannot be off".into(),
                            )
                        })?,
                    })
                })
                .collect::<Result<Vec<_>, EngineError>>()?,
        }),
        protocol::FrozenCacheStrategy::Google {
            mode,
            cached_content,
        } => CacheStrategyConfig::Google(GoogleCacheStrategyConfig {
            mode: match mode {
                protocol::FrozenGoogleCacheMode::Implicit => GoogleCacheMode::Implicit,
                protocol::FrozenGoogleCacheMode::Explicit => GoogleCacheMode::Explicit,
                protocol::FrozenGoogleCacheMode::Off => GoogleCacheMode::Off,
            },
            cached_content: cached_content.clone(),
        }),
        protocol::FrozenCacheStrategy::OpenAi {
            prompt_cache_key,
            prompt_cache_retention,
            mode,
            ttl,
            system,
            rolling,
        } => CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
            prompt_cache_key: prompt_cache_key.clone(),
            prompt_cache_retention: prompt_cache_retention.map(|retention| match retention {
                protocol::FrozenOpenAiCacheRetention::InMemory => {
                    OpenAiPromptCacheRetention::InMemory
                }
                protocol::FrozenOpenAiCacheRetention::TwentyFourHours => {
                    OpenAiPromptCacheRetention::TwentyFourHours
                }
            }),
            mode: mode.map(|mode| match mode {
                protocol::FrozenOpenAiCacheMode::Implicit => OpenAiCacheMode::Implicit,
                protocol::FrozenOpenAiCacheMode::Explicit => OpenAiCacheMode::Explicit,
            }),
            ttl: ttl.map(|ttl| match ttl {
                protocol::FrozenOpenAiPromptCacheTtl::ThirtyMinutes => {
                    OpenAiPromptCacheTtl::ThirtyMinutes
                }
            }),
            system: system.unwrap_or_default(),
            rolling: rolling.unwrap_or_default(),
        }),
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

const fn runtime_anthropic_ttl(
    ttl: protocol::FrozenCacheTtl,
) -> Option<cookie_agent_models::adapters::AnthropicCacheTtlConfig> {
    match ttl {
        protocol::FrozenCacheTtl::OneHour => {
            Some(cookie_agent_models::adapters::AnthropicCacheTtlConfig::OneHour)
        }
        protocol::FrozenCacheTtl::FiveMinutes => {
            Some(cookie_agent_models::adapters::AnthropicCacheTtlConfig::FiveMinutes)
        }
        protocol::FrozenCacheTtl::Off => None,
    }
}

const fn runtime_bedrock_point(ttl: protocol::FrozenCacheTtl) -> Option<BedrockCachePoint> {
    Some(BedrockCachePoint {
        ttl: Some(match ttl {
            protocol::FrozenCacheTtl::OneHour => BedrockCacheTtl::OneHour,
            protocol::FrozenCacheTtl::FiveMinutes => BedrockCacheTtl::FiveMinutes,
            protocol::FrozenCacheTtl::Off => return None,
        }),
    })
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

pub(crate) fn wire_adapter(value: &str) -> protocol::AdaptorId {
    match adapter_family(value) {
        cookie_agent_models::adapters::OvenAdapterFamily::Anthropic
        | cookie_agent_models::adapters::OvenAdapterFamily::AnthropicCompatible => {
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

pub(crate) fn adapter_family(value: &str) -> cookie_agent_models::adapters::OvenAdapterFamily {
    cookie_agent_models::adapters::wire_adapter_for_protocol(value)
        .expect("frozen binding contains a reviewed protocol adapter")
}

#[cfg(test)]
mod cache_strategy_tests {
    use super::*;

    #[test]
    fn all_off_bedrock_config_emits_no_cache_points() {
        let config: cookie_agent_config::BedrockCacheConfig = serde_json::from_value(
            serde_json::json!({"system":"off","tools":"off","rolling":"off"}),
        )
        .unwrap();
        let strategy = bedrock_strategy(&config);
        assert!(strategy.system.is_none());
        assert!(strategy.tools.is_none());
        assert!(strategy.messages.is_empty());
    }
}
