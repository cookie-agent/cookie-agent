use std::sync::Arc;

use cookie_agent_models::adapters::{
    BedrockCachePoint, BedrockCacheStrategy, BedrockCacheTtl, BedrockMessageCachePoint,
    CacheStrategyConfig, GoogleCacheMode, GoogleCacheStrategyConfig, OpenAiCacheStrategyConfig,
    OpenAiPromptCacheRetention, OvenAdapterFamily,
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
    pub runtime_cache: cookie_agent_config::CacheConfig,
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
    pub cache_strategies: Vec<Option<cookie_agent_models::adapters::CacheStrategyConfig>>,
    pub runtime_cache: cookie_agent_config::CacheConfig,
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
}

pub(crate) fn freeze_root_agent_policy(
    agent: &ResolvedAgent,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    selection: &protocol::ModelSelection,
    max_depth: u32,
    result_limits: ResultLimits,
    runtime_cache: cookie_agent_config::CacheConfig,
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
            runtime_cache,
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
    let targets = delegation_targets(&document.frontmatter.permissions);
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
                .map(|binding| resolve_cache_strategy(None, binding, &options.runtime_cache))
                .collect::<Result<Vec<_>, _>>()?
        }
    } else {
        bindings
            .iter()
            .map(|binding| resolve_cache_strategy(Some(agent), binding, &options.runtime_cache))
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
        cache_strategies,
        runtime_cache: options.runtime_cache,
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
    runtime_cache: cookie_agent_config::CacheConfig,
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
        runtime_cache,
    )?;
    if let Some(strategies) = restored_cache_strategies {
        policy.cache_strategies = strategies;
    }
    Ok(policy)
}

pub(crate) fn policy_from_snapshot(
    agent: protocol::AgentSnapshot,
    selected_suffix: Vec<protocol::FrozenModelBinding>,
    registry: Arc<AgentRegistry>,
    runtime: Arc<PublishedRuntime>,
    tool_output_max_lines: usize,
    tool_output_max_bytes: usize,
    runtime_cache: cookie_agent_config::CacheConfig,
) -> Result<FrozenRunPolicy, EngineError> {
    if selected_suffix.is_empty() {
        return Err(EngineError::NoRunnableModel);
    }
    let preset = registry.preset().map(str::to_owned);
    let cache_strategies = selected_suffix
        .iter()
        .map(|binding| resolve_cache_strategy(registry.get(&agent.agent), binding, &runtime_cache))
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
        cache_strategies,
        runtime_cache,
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
    runtime_cache: &cookie_agent_config::CacheConfig,
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
            + usize::from(config.google.is_some())
            + usize::from(config.openai.is_some());
        if configured_sections > 1 {
            return Err(EngineError::CacheStrategy(
                "an agent model cache entry must configure only its provider family".into(),
            ));
        }
    }
    let config = authored.unwrap_or(runtime_cache);
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
            OvenAdapterFamily::GoogleGemini | OvenAdapterFamily::GoogleVertexGemini => {
                config.google.is_some()
            }
            OvenAdapterFamily::OpenaiChat
            | OvenAdapterFamily::OpenaiResponses
            | OvenAdapterFamily::AzureOpenaiChat
            | OvenAdapterFamily::AzureOpenaiResponses => config.openai.is_some(),
            OvenAdapterFamily::OpenaiCompatible | OvenAdapterFamily::CohereV2Chat => false,
        };
        let has_section = config.anthropic.is_some()
            || config.bedrock.is_some()
            || config.google.is_some()
            || config.openai.is_some();
        if has_section && !matches_family {
            return Err(EngineError::CacheStrategy(format!(
                "cache strategy does not match the {} model adapter family",
                family.id()
            )));
        }
    }
    let strategy = match family {
        OvenAdapterFamily::Anthropic | OvenAdapterFamily::AnthropicCompatible => {
            config.anthropic.as_ref().map(|config| {
                CacheStrategyConfig::Anthropic(
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
                )
            })
        }
        OvenAdapterFamily::AwsBedrockConverse => config.bedrock.as_ref().map(|config| {
            let strategy = if config.enabled {
                let system = config
                    .system
                    .unwrap_or(cookie_agent_config::CacheTtl::OneHour);
                let tools = config
                    .tools
                    .unwrap_or(cookie_agent_config::CacheTtl::OneHour);
                let messages = config.messages.as_ref().map_or_else(
                    || {
                        vec![BedrockMessageCachePoint {
                            history_index: usize::MAX,
                            cache_point: BedrockCachePoint {
                                ttl: Some(BedrockCacheTtl::FiveMinutes),
                            },
                        }]
                    },
                    |messages| {
                        messages
                            .iter()
                            .map(|message| BedrockMessageCachePoint {
                                history_index: message.history_index,
                                cache_point: BedrockCachePoint {
                                    ttl: Some(match message.ttl {
                                        cookie_agent_config::BedrockCacheTtl::OneHour => {
                                            BedrockCacheTtl::OneHour
                                        }
                                        cookie_agent_config::BedrockCacheTtl::FiveMinutes => {
                                            BedrockCacheTtl::FiveMinutes
                                        }
                                    }),
                                },
                            })
                            .collect()
                    },
                );
                BedrockCacheStrategy {
                    system: bedrock_cache_point(system),
                    tools: bedrock_cache_point(tools),
                    messages,
                }
            } else {
                BedrockCacheStrategy::default()
            };
            CacheStrategyConfig::Bedrock(strategy)
        }),
        OvenAdapterFamily::GoogleGemini | OvenAdapterFamily::GoogleVertexGemini => {
            config.google.as_ref().map(|config| {
                CacheStrategyConfig::Google(GoogleCacheStrategyConfig {
                    mode: match config.mode {
                        cookie_agent_config::GoogleCacheMode::Implicit => GoogleCacheMode::Implicit,
                        cookie_agent_config::GoogleCacheMode::Explicit => GoogleCacheMode::Explicit,
                        cookie_agent_config::GoogleCacheMode::Off => GoogleCacheMode::Off,
                    },
                    cached_content: config.cached_content.clone(),
                })
            })
        }
        OvenAdapterFamily::OpenaiChat
        | OvenAdapterFamily::OpenaiResponses
        | OvenAdapterFamily::AzureOpenaiChat
        | OvenAdapterFamily::AzureOpenaiResponses => config.openai.as_ref().map(|config| {
            CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
                prompt_cache_key: config.prompt_cache_key.clone(),
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
            })
        }),
        OvenAdapterFamily::OpenaiCompatible | OvenAdapterFamily::CohereV2Chat => {
            if authored.is_some() {
                return Err(EngineError::CacheStrategy(format!(
                    "{} does not support an authored cache strategy",
                    family.id()
                )));
            }
            None
        }
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

fn wire_adapter(value: &str) -> protocol::AdaptorId {
    match cookie_agent_models::adapters::wire_adapter_for_protocol(value)
        .expect("frozen binding contains a reviewed protocol adapter")
    {
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

#[cfg(test)]
mod cache_strategy_tests {
    use super::*;
    use cookie_agent_identity::ProtocolRecipeId;

    fn binding(recipe: &str) -> protocol::FrozenModelBinding {
        let mut binding = crate::test_support::model_binding();
        binding.protocol_recipe = ProtocolRecipeId::new(recipe).unwrap();
        binding
    }

    #[test]
    fn mixed_provider_bindings_resolve_their_own_runtime_cache_sections() {
        let runtime = cookie_agent_config::CacheConfig {
            anthropic: Some(cookie_agent_config::AnthropicCacheConfig::default()),
            bedrock: Some(cookie_agent_config::BedrockCacheConfig::default()),
            google: Some(cookie_agent_config::GoogleCacheConfig::default()),
            openai: Some(cookie_agent_config::OpenAiCacheConfig {
                prompt_cache_key: Some("mixed-${session_id}".into()),
                prompt_cache_retention: None,
            }),
        };

        assert!(matches!(
            resolve_cache_strategy(None, &binding("oven.anthropic.messages"), &runtime).unwrap(),
            Some(CacheStrategyConfig::Anthropic(_))
        ));
        assert!(matches!(
            resolve_cache_strategy(None, &binding("oven.bedrock.converse"), &runtime).unwrap(),
            Some(CacheStrategyConfig::Bedrock(_))
        ));
        assert!(matches!(
            resolve_cache_strategy(
                None,
                &binding("oven.google.gemini.generate-content"),
                &runtime
            )
            .unwrap(),
            Some(CacheStrategyConfig::Google(_))
        ));
        assert!(matches!(
            resolve_cache_strategy(None, &binding("oven.openai.responses"), &runtime).unwrap(),
            Some(CacheStrategyConfig::OpenAi(_))
        ));
        assert!(
            resolve_cache_strategy(None, &binding("oven.openai-compatible.chat"), &runtime)
                .unwrap()
                .is_none()
        );
    }
}
