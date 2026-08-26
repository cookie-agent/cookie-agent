use std::sync::{Arc, atomic::Ordering};

use cookie_agent_protocol::{
    AgentId, AgentMode, ApprovalInternalDecisionKind, FrozenInternalAgentDefinition,
    FrozenInternalAgentFallback, InternalAgentBackend, InternalAgentFailure,
    InternalAgentInvocationId, InternalAgentKind, InternalAgentRunId, RunId, SafeInternalAgentCall,
    SafeInternalAgentResult, SessionId, Sha256Digest,
};
use oven_sdk::{ModelError, Request as ModelRequest, ToolDefinition};
use serde::Deserialize;

use super::{
    ActiveRun, Engine, EngineError, Event, FrozenInternalAgentPolicy, InternalAgentExecution,
    InternalAgentHistoryInput, InternalAgentLimits, InternalAgentTextResult,
    UNAVAILABLE_BUILTIN_REVISION,
    helpers::{safe_code, safe_display, safe_error, truncate_utf8},
};
use crate::{
    model_bridge::AbortBridge,
    model_history::wire_model,
    model_policy::summary as model_error_summary,
    policy::{self, FrozenRunPolicy},
};

const UNKNOWN_INTERNAL_CONTEXT_LIMIT: u64 = 16_384;
const DEFAULT_INTERNAL_TIMEOUT_MS: u64 = 30_000;

impl Engine {
    pub(super) async fn run_internal_text_agent(
        &self,
        session: SessionId,
        parent_run: Option<RunId>,
        kind: InternalAgentKind,
        policy: &FrozenInternalAgentPolicy,
        input: String,
        execution: InternalAgentExecution<'_>,
    ) -> Result<InternalAgentTextResult, EngineError> {
        let max_input_bytes = usize::try_from(internal_agent_max_input_limit(policy))
            .unwrap_or(usize::MAX)
            .saturating_mul(4);
        let input = truncate_utf8(&input, max_input_bytes);
        let history = vec![
            oven_sdk::HistoryTurn::system(oven_sdk::SystemMessage::new(vec![
                oven_sdk::SystemPart::Text(oven_sdk::TextPart::new(
                    policy.agent.composed_prompt.clone(),
                )),
            ])),
            oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(vec![
                oven_sdk::InputPart::Text(oven_sdk::TextPart::new(input.clone())),
            ])),
        ];
        self.run_internal_history_agent(
            session,
            parent_run,
            kind,
            policy,
            InternalAgentHistoryInput {
                history,
                summary_source: input,
                tools: Vec::new(),
                reject_non_text: false,
            },
            execution,
        )
        .await
    }

    pub(super) async fn run_internal_history_agent(
        &self,
        session: SessionId,
        parent_run: Option<RunId>,
        kind: InternalAgentKind,
        policy: &FrozenInternalAgentPolicy,
        input: InternalAgentHistoryInput,
        execution: InternalAgentExecution<'_>,
    ) -> Result<InternalAgentTextResult, EngineError> {
        let name = match kind {
            InternalAgentKind::Approval => "approval",
            InternalAgentKind::ContextCompaction => "context_compaction",
            InternalAgentKind::SessionTitle => "session_title",
        };
        let policy = policy.clone();
        let input_tokens = internal_history_tokens(&input.history, &input.tools)?;
        let invocation_id = InternalAgentInvocationId::new_v7();
        let internal_run_id = InternalAgentRunId::new_v7();
        let call = SafeInternalAgentCall {
            name: safe_code(name),
            input_summary: safe_display(&format!(
                "bounded {name} input ({} bytes)",
                input.summary_source.len()
            )),
            input_digest: Sha256Digest::of_bytes(input.summary_source.as_bytes()),
        };
        let mut previous_backend = None;
        let mut last_failure = InternalAgentFailure {
            code: safe_code("agent_unavailable"),
            message: safe_error("no frozen internal model is available"),
            retryable: false,
            model_error: None,
        };
        for (index, binding) in policy.models.iter().enumerate() {
            let backend = InternalAgentBackend::Model {
                resolved_model: wire_model(binding),
            };
            if index == 0 {
                self.append_internal_agent_event(
                    session,
                    parent_run,
                    Event::InternalAgentStarted {
                        invocation_id,
                        internal_run_id,
                        kind,
                        backend: backend.clone(),
                        call: call.clone(),
                    },
                    execution.actor_direct,
                )
                .await?;
            } else if let Some(from) = previous_backend.take() {
                self.append_internal_agent_event(
                    session,
                    parent_run,
                    Event::InternalAgentFallback {
                        invocation_id,
                        internal_run_id,
                        kind,
                        from,
                        to: backend.clone(),
                        failure: last_failure.clone(),
                        attempts: index as u32,
                    },
                    execution.actor_direct,
                )
                .await?;
            }
            let max_input_tokens = internal_agent_input_limit(binding, &policy);
            if !internal_agent_input_fits(input_tokens, binding, &policy) {
                last_failure = InternalAgentFailure {
                    code: safe_code("input_too_large"),
                    message: safe_error(&format!(
                        "internal agent input is {input_tokens} estimated tokens, exceeding this model's frozen {max_input_tokens}-token limit"
                    )),
                    retryable: false,
                    model_error: None,
                };
                previous_backend = Some(backend);
                continue;
            }
            let runtime = policy
                .runtime
                .as_ref()
                .ok_or(EngineError::NoRunnableModel)?;
            let model = policy::resolve_model(binding, runtime)?;
            let max_output_tokens = internal_agent_output_limit(binding, &policy);
            let request = internal_model_request(
                input.history.clone(),
                input.tools.clone(),
                max_output_tokens,
            );
            let cache_strategy = policy.cache_strategy(binding, session);
            let request =
                model.prepare_request_with_cache_strategy(request, cache_strategy.as_ref());
            let abort = AbortBridge::new(execution.cancellation.child_token());
            let call_future = model.model().complete(request, abort.signal());
            let result = tokio::select! {
                result = tokio::time::timeout(
                    std::time::Duration::from_millis(match policy.limits.timeout_ms {
                        0 => DEFAULT_INTERNAL_TIMEOUT_MS,
                        configured => configured,
                    }),
                    call_future,
                ) => match result {
                    Ok(result) => result,
                    Err(_) => {
                        abort.abort();
                        Err(ModelError::timeout("internal agent timed out"))
                    },
                },
                _ = execution.cancellation.cancelled() => {
                    abort.abort();
                    Err(ModelError::abort("internal agent was cancelled"))
                }
            };
            if execution.cancellation.is_cancelled() {
                abort.abort();
                self.append_internal_agent_event(
                    session,
                    parent_run,
                    Event::InternalAgentCancelled {
                        invocation_id,
                        internal_run_id,
                        kind,
                        reason: Some(safe_error("parent run cancelled")),
                    },
                    execution.actor_direct,
                )
                .await?;
                return Err(ModelError::abort("internal agent was cancelled").into());
            }
            match result {
                Ok(completed) => {
                    let usage =
                        crate::model_history::persist_usage(completed.turn.finish.usage.clone());
                    let resolved_model = wire_model(binding);
                    let estimated_cost_pico_usd = crate::usage::estimated_cost_pico_usd(
                        &resolved_model,
                        &usage,
                        &self.inner.config.runtime.pricing,
                        &self.catalog_pricing(),
                    );
                    self.append_internal_agent_event(
                        session,
                        parent_run,
                        Event::InternalAgentUsageRecorded {
                            internal_run_id,
                            kind,
                            agent_id: policy.agent.agent.clone(),
                            resolved_model,
                            usage,
                            estimated_cost_pico_usd,
                        },
                        execution.actor_direct,
                    )
                    .await?;
                    if invalid_internal_output(
                        &completed.turn.message.content,
                        input.reject_non_text,
                    ) {
                        last_failure = InternalAgentFailure {
                            code: safe_code("invalid_non_text_output"),
                            message: safe_error(
                                "internal agent returned non-text output that cannot be executed",
                            ),
                            retryable: false,
                            model_error: None,
                        };
                        previous_backend = Some(backend);
                        continue;
                    }
                    let output = completed
                        .turn
                        .message
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            oven_sdk::AssistantPart::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    let output_exceeds_document_limit = (policy.limits.max_output_tokens != 0)
                        && output.len()
                            > usize::try_from(policy.limits.max_output_tokens)
                                .unwrap_or(usize::MAX)
                                .saturating_mul(4);
                    if output_exceeds_document_limit {
                        last_failure = InternalAgentFailure {
                            code: safe_code("output_too_large"),
                            message: safe_error("internal agent output exceeded its hard bound"),
                            retryable: false,
                            model_error: None,
                        };
                        previous_backend = Some(backend);
                        continue;
                    }
                    self.append_internal_agent_event(
                        session,
                        parent_run,
                        Event::InternalAgentCompleted {
                            invocation_id,
                            internal_run_id,
                            kind,
                            result: SafeInternalAgentResult {
                                output_summary: safe_display(&format!(
                                    "validated {name} output ({} bytes)",
                                    output.len()
                                )),
                                output_digest: Sha256Digest::of_bytes(output.as_bytes()),
                            },
                        },
                        execution.actor_direct,
                    )
                    .await?;
                    return Ok(InternalAgentTextResult {
                        invocation_id,
                        internal_run_id,
                        text: output,
                    });
                }
                Err(error) => {
                    last_failure = InternalAgentFailure {
                        code: safe_code("model_failure"),
                        message: safe_error(&error.message),
                        retryable: error.retryable,
                        model_error: Some(model_error_summary(&error)),
                    };
                    previous_backend = Some(backend);
                }
            }
        }
        if policy.models.is_empty() {
            self.append_internal_agent_event(
                session,
                parent_run,
                Event::InternalAgentStarted {
                    invocation_id,
                    internal_run_id,
                    kind,
                    backend: InternalAgentBackend::Builtin {
                        name: safe_code("unavailable"),
                        revision: safe_display(UNAVAILABLE_BUILTIN_REVISION),
                    },
                    call,
                },
                execution.actor_direct,
            )
            .await?;
        }
        self.append_internal_agent_event(
            session,
            parent_run,
            Event::InternalAgentFailed {
                invocation_id,
                internal_run_id,
                kind,
                failure: last_failure,
            },
            execution.actor_direct,
        )
        .await?;
        Err(ModelError::invalid_response("internal agent failed safely").into())
    }

    async fn append_internal_agent_event(
        &self,
        session: SessionId,
        run: Option<RunId>,
        event: Event,
        actor_direct: bool,
    ) -> Result<(), EngineError> {
        if actor_direct {
            self.append_direct(
                session,
                run,
                super::event_origin("engine:internal-agent"),
                event,
            )
        } else {
            self.append(
                session,
                run,
                super::event_origin("engine:internal-agent"),
                event,
            )
            .await
        }
    }

    pub(super) fn active_internal_policy(
        &self,
        active: &ActiveRun,
        kind: InternalAgentKind,
    ) -> Result<FrozenInternalAgentPolicy, EngineError> {
        let fallback_index = active.fallback_index.load(Ordering::Acquire) as usize;
        self.internal_agent_policy(
            kind,
            &active.policy,
            active.policy.active_suffix(fallback_index).first(),
        )
    }

    pub(crate) fn freeze_internal_agent_definitions(
        &self,
        owner: &FrozenRunPolicy,
    ) -> Result<Vec<FrozenInternalAgentDefinition>, EngineError> {
        [
            InternalAgentKind::Approval,
            InternalAgentKind::ContextCompaction,
            InternalAgentKind::SessionTitle,
        ]
        .into_iter()
        .map(|kind| {
            let id = internal_agent_id(kind)?;
            let resolved = owner
                .registry
                .get(&id)
                .ok_or_else(|| EngineError::InvalidRuntimeAgent(id.clone()))?;
            if resolved.document.frontmatter.mode != cookie_agent_config::AgentMode::Internal {
                return Err(EngineError::InvalidRuntimeAgent(id));
            }
            let fallbacks = resolved
                .resolved_fallback
                .iter()
                .filter_map(|fallback| match fallback {
                    crate::runtime_snapshot::ResolvedAgentFallback::ParentModel { .. } => {
                        Some(Ok(FrozenInternalAgentFallback::ParentModel))
                    }
                    crate::runtime_snapshot::ResolvedAgentFallback::Selection {
                        selection, ..
                    } => owner
                        .runtime
                        .models
                        .model(&selection.model)
                        .is_some_and(|model| {
                            model.model.status
                                == cookie_agent_models::compiler::CompiledModelStatus::Available
                        })
                        .then(|| {
                            crate::model_snapshots::binding_for_selection(
                                &owner.runtime.current_manifest,
                                &owner.runtime.models,
                                selection,
                            )
                            .map(|binding| {
                                FrozenInternalAgentFallback::Model {
                                    binding: Box::new(binding),
                                }
                            })
                        }),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let document = &resolved.document;
            let definition = FrozenInternalAgentDefinition {
                kind,
                agent: document.id.clone(),
                description: document.frontmatter.description.clone(),
                document_source: document.source,
                document_fingerprint: Sha256Digest::new(document.document_fingerprint.as_str())
                    .map_err(|_| EngineError::RuntimeCompileFailed)?,
                composed_prompt: document.body.clone(),
                prompt_fingerprint: Sha256Digest::new(document.prompt_fingerprint.as_str())
                    .map_err(|_| EngineError::RuntimeCompileFailed)?,
                enabled: document.frontmatter.enabled,
                max_output_tokens: document.frontmatter.limits.max_output_tokens,
                timeout_ms: document.frontmatter.limits.timeout_ms,
                fallbacks,
            };
            definition
                .validate()
                .map_err(|_| EngineError::RuntimeCompileFailed)?;
            Ok(definition)
        })
        .collect()
    }

    pub(crate) fn internal_agent_policy(
        &self,
        kind: InternalAgentKind,
        owner: &FrozenRunPolicy,
        parent_binding: Option<&cookie_agent_protocol::FrozenModelBinding>,
    ) -> Result<FrozenInternalAgentPolicy, EngineError> {
        if let Some(definition) = owner
            .internal_agents
            .iter()
            .find(|definition| definition.kind == kind)
        {
            return frozen_internal_policy_from_definition(definition, owner, parent_binding);
        }
        let id = internal_agent_id(kind)?;
        let resolved = owner
            .registry
            .get(&id)
            .ok_or_else(|| EngineError::InvalidRuntimeAgent(id.clone()))?;
        if resolved.document.frontmatter.mode != cookie_agent_config::AgentMode::Internal {
            return Err(EngineError::InvalidRuntimeAgent(id));
        }
        let mut models = Vec::new();
        let fallbacks = if resolved.document.frontmatter.enabled {
            resolved.resolved_fallback.as_slice()
        } else {
            &[]
        };
        for fallback in fallbacks {
            match fallback {
                crate::runtime_snapshot::ResolvedAgentFallback::ParentModel { .. } => {
                    if let Some(binding) = parent_binding {
                        models.push(binding.clone());
                    }
                }
                crate::runtime_snapshot::ResolvedAgentFallback::Selection { selection, .. } => {
                    if owner
                        .runtime
                        .models
                        .model(&selection.model)
                        .is_some_and(|model| {
                            model.model.status
                                == cookie_agent_models::compiler::CompiledModelStatus::Available
                        })
                    {
                        models.push(crate::model_snapshots::binding_for_selection(
                            &owner.runtime.current_manifest,
                            &owner.runtime.models,
                            selection,
                        )?);
                    }
                }
            }
        }
        let document = &resolved.document;
        let snapshot = cookie_agent_protocol::AgentSnapshot {
            agent: document.id.clone(),
            schema: cookie_agent_protocol::AgentSchemaVersion::current(),
            mode: AgentMode::Internal,
            description: document.frontmatter.description.clone(),
            document_source: match document.source {
                cookie_agent_config::AgentDocumentSource::BuiltIn => {
                    cookie_agent_protocol::AgentDocumentSource::BuiltIn
                }
                cookie_agent_config::AgentDocumentSource::User => {
                    cookie_agent_protocol::AgentDocumentSource::User
                }
                cookie_agent_config::AgentDocumentSource::Workspace => {
                    cookie_agent_protocol::AgentDocumentSource::Workspace
                }
            },
            document_fingerprint: Sha256Digest::new(document.document_fingerprint.as_str())
                .map_err(|_| EngineError::RuntimeCompileFailed)?,
            composed_prompt: document.body.clone(),
            prompt_fingerprint: Sha256Digest::new(document.prompt_fingerprint.as_str())
                .map_err(|_| EngineError::RuntimeCompileFailed)?,
            max_output_tokens: document.frontmatter.limits.max_output_tokens,
            permissions: Vec::new(),
            delegation: None,
            fallback_chain: models.clone(),
            selected_suffix_start: 0,
        };
        let limits = &document.frontmatter.limits;
        let inherit_parent_cache = resolved.resolved_fallback.iter().any(|fallback| {
            matches!(
                fallback,
                crate::runtime_snapshot::ResolvedAgentFallback::ParentModel { cache: None }
            )
        });
        let cache_strategies = internal_cache_strategies(
            kind,
            &models,
            Some(resolved),
            owner,
            parent_binding,
            inherit_parent_cache,
        )?;
        Ok(FrozenInternalAgentPolicy {
            agent: snapshot,
            models,
            runtime: Some(Arc::clone(&owner.runtime)),
            limits: InternalAgentLimits {
                max_output_tokens: limits.max_output_tokens,
                timeout_ms: limits.timeout_ms,
            },
            cache_strategies,
        })
    }
}

fn internal_agent_id(kind: InternalAgentKind) -> Result<AgentId, EngineError> {
    AgentId::new(match kind {
        InternalAgentKind::Approval => cookie_agent_config::BUILT_IN_APPROVAL_AGENT_ID,
        InternalAgentKind::ContextCompaction => cookie_agent_config::BUILT_IN_COMPACTION_AGENT_ID,
        InternalAgentKind::SessionTitle => cookie_agent_config::BUILT_IN_TITLE_AGENT_ID,
    })
    .map_err(|_| EngineError::RuntimeCompileFailed)
}

fn frozen_internal_policy_from_definition(
    definition: &FrozenInternalAgentDefinition,
    owner: &FrozenRunPolicy,
    parent_binding: Option<&cookie_agent_protocol::FrozenModelBinding>,
) -> Result<FrozenInternalAgentPolicy, EngineError> {
    let models = if definition.enabled {
        definition
            .fallbacks
            .iter()
            .filter_map(|fallback| match fallback {
                FrozenInternalAgentFallback::ParentModel => parent_binding.cloned(),
                FrozenInternalAgentFallback::Model { binding } => Some(binding.as_ref().clone()),
            })
            .collect()
    } else {
        Vec::new()
    };
    let agent = cookie_agent_protocol::AgentSnapshot {
        agent: definition.agent.clone(),
        schema: cookie_agent_protocol::AgentSchemaVersion::current(),
        mode: AgentMode::Internal,
        description: definition.description.clone(),
        document_source: definition.document_source,
        document_fingerprint: definition.document_fingerprint.clone(),
        composed_prompt: definition.composed_prompt.clone(),
        prompt_fingerprint: definition.prompt_fingerprint.clone(),
        max_output_tokens: definition.max_output_tokens,
        permissions: Vec::new(),
        delegation: None,
        fallback_chain: models.clone(),
        selected_suffix_start: 0,
    };
    let resolved = owner.registry.get(&definition.agent);
    let inherit_parent_cache = resolved.map_or_else(
        || {
            definition
                .fallbacks
                .iter()
                .any(|fallback| matches!(fallback, FrozenInternalAgentFallback::ParentModel))
        },
        |resolved| {
            resolved.resolved_fallback.iter().any(|fallback| {
                matches!(
                    fallback,
                    crate::runtime_snapshot::ResolvedAgentFallback::ParentModel { cache: None }
                )
            })
        },
    );
    let cache_strategies = internal_cache_strategies(
        definition.kind,
        &models,
        resolved,
        owner,
        parent_binding,
        inherit_parent_cache,
    )?;
    Ok(FrozenInternalAgentPolicy {
        agent,
        models,
        runtime: Some(Arc::clone(&owner.runtime)),
        limits: InternalAgentLimits {
            max_output_tokens: definition.max_output_tokens,
            timeout_ms: definition.timeout_ms,
        },
        cache_strategies,
    })
}

fn internal_cache_strategies(
    kind: InternalAgentKind,
    models: &[cookie_agent_protocol::FrozenModelBinding],
    resolved: Option<&crate::runtime_snapshot::ResolvedAgent>,
    owner: &FrozenRunPolicy,
    parent_binding: Option<&cookie_agent_protocol::FrozenModelBinding>,
    inherit_parent_cache: bool,
) -> Result<Vec<Option<cookie_agent_models::adapters::CacheStrategyConfig>>, EngineError> {
    models
        .iter()
        .map(|binding| {
            let mut strategy = if inherit_parent_cache && parent_binding == Some(binding) {
                owner.raw_cache_strategy(binding)
            } else {
                policy::resolve_cache_strategy(resolved, binding, &owner.runtime_cache)?
            };
            if matches!(
                kind,
                InternalAgentKind::Approval | InternalAgentKind::SessionTitle
            ) && let Some(cookie_agent_models::adapters::CacheStrategyConfig::Anthropic(
                strategy,
            )) = &mut strategy
            {
                strategy.rolling = None;
            }
            Ok(strategy)
        })
        .collect()
}

fn internal_agent_max_input_limit(policy: &FrozenInternalAgentPolicy) -> u64 {
    policy
        .models
        .iter()
        .map(|binding| internal_agent_input_limit(binding, policy))
        .max()
        .unwrap_or(UNKNOWN_INTERNAL_CONTEXT_LIMIT)
}

pub(super) fn internal_agent_input_limit(
    binding: &cookie_agent_protocol::FrozenModelBinding,
    policy: &FrozenInternalAgentPolicy,
) -> u64 {
    binding.descriptor.capabilities.limits.context.map_or(
        UNKNOWN_INTERNAL_CONTEXT_LIMIT,
        |context| {
            context
                .saturating_sub(internal_agent_output_limit(binding, policy).unwrap_or(0))
                .max(1)
        },
    )
}

pub(super) fn internal_agent_input_fits(
    input_tokens: u64,
    binding: &cookie_agent_protocol::FrozenModelBinding,
    policy: &FrozenInternalAgentPolicy,
) -> bool {
    input_tokens <= internal_agent_input_limit(binding, policy)
}

pub(super) fn internal_agent_output_limit(
    binding: &cookie_agent_protocol::FrozenModelBinding,
    policy: &FrozenInternalAgentPolicy,
) -> Option<u64> {
    match (
        binding.descriptor.capabilities.limits.output,
        policy.limits.max_output_tokens,
    ) {
        (Some(model), 0) => Some(model),
        (Some(model), document) => Some(model.min(document)),
        (None, 0) => None,
        (None, document) => Some(document),
    }
}

fn internal_model_request(
    history: Vec<oven_sdk::HistoryTurn>,
    tools: Vec<ToolDefinition>,
    max_output_tokens: Option<u64>,
) -> ModelRequest {
    let mut request = ModelRequest::new(history).with_tools(tools);
    request.inference.max_output_tokens = max_output_tokens;
    request
}

pub(super) fn internal_history_tokens(
    history: &[oven_sdk::HistoryTurn],
    tools: &[ToolDefinition],
) -> Result<u64, EngineError> {
    let bytes = serde_json::to_vec(&(history, tools))
        .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?
        .len();
    Ok((bytes as u64).div_ceil(4))
}

fn invalid_internal_output(parts: &[oven_sdk::AssistantPart], reject_non_text: bool) -> bool {
    // Reasoning parts are visible assistant text, not executable output; only
    // parts that would need execution or attachment handling are invalid.
    reject_non_text
        && parts.iter().any(|part| {
            !matches!(
                part,
                oven_sdk::AssistantPart::Text(_) | oven_sdk::AssistantPart::Reasoning(_)
            )
        })
}

pub(super) fn parse_internal_approval(value: &str) -> Option<ApprovalInternalDecisionKind> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Decision {
        decision: String,
    }
    match serde_json::from_str::<Decision>(value.trim())
        .ok()?
        .decision
        .as_str()
    {
        "allow" => Some(ApprovalInternalDecisionKind::Allow),
        "deny" => Some(ApprovalInternalDecisionKind::Deny),
        "ask" => Some(ApprovalInternalDecisionKind::Ask),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FrozenInternalAgentPolicy, InternalAgentLimits, UNKNOWN_INTERNAL_CONTEXT_LIMIT,
        internal_agent_input_fits, internal_agent_input_limit, internal_history_tokens,
        internal_model_request, invalid_internal_output,
    };

    #[test]
    fn internal_model_requests_are_structurally_toolless() {
        let request = internal_model_request(Vec::new(), Vec::new(), Some(128));
        assert!(request.tools.is_empty());
        assert_eq!(request.inference.max_output_tokens, Some(128));
    }

    #[test]
    fn compaction_rejects_tool_calls_instead_of_executing_them() {
        let parts = vec![oven_sdk::AssistantPart::ToolCall(
            oven_sdk::ToolCallPart::new("call", "read", serde_json::json!({"filePath":"x"})),
        )];
        assert!(invalid_internal_output(&parts, true));
        assert!(!invalid_internal_output(&parts, false));
    }

    #[test]
    fn approval_rejects_json_plus_tool_call_hybrids() {
        let parts = vec![
            oven_sdk::AssistantPart::Text(oven_sdk::TextPart::new(r#"{"decision":"allow"}"#)),
            oven_sdk::AssistantPart::ToolCall(oven_sdk::ToolCallPart::new(
                "call",
                "read",
                serde_json::json!({"filePath":"secret"}),
            )),
        ];
        assert!(invalid_internal_output(&parts, true));
    }

    #[test]
    fn reasoning_parts_are_not_rejected_as_non_text() {
        // Thinking-capable models emit visible reasoning alongside the text
        // decision; reasoning is not executable output and must not fail the
        // approval agent.
        let parts = vec![
            oven_sdk::AssistantPart::Reasoning(oven_sdk::ReasoningPart::new("weighing risk")),
            oven_sdk::AssistantPart::Text(oven_sdk::TextPart::new(r#"{"decision":"allow"}"#)),
        ];
        assert!(!invalid_internal_output(&parts, true));
    }

    #[test]
    fn history_input_estimate_counts_the_full_history_and_tools() {
        let history = vec![oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(
            vec![oven_sdk::InputPart::Text(oven_sdk::TextPart::new(
                "x".repeat(4_000),
            ))],
        ))];
        assert!(internal_history_tokens(&history, &[]).unwrap() >= 1_000);
    }

    #[test]
    fn internal_input_limit_uses_model_context_minus_output_reserve() {
        let mut binding = crate::test_support::model_binding();
        binding.descriptor.capabilities.limits.context = Some(200_000);
        let mut policy = FrozenInternalAgentPolicy {
            agent: crate::test_support::agent_snapshot(
                "compaction",
                cookie_agent_protocol::AgentMode::Internal,
            ),
            models: vec![binding],
            runtime: None,
            limits: InternalAgentLimits {
                max_output_tokens: 2_048,
                timeout_ms: 30_000,
            },
            cache_strategies: vec![None],
        };
        assert_eq!(
            internal_agent_input_limit(&policy.models[0], &policy),
            197_952
        );

        policy.models[0].descriptor.capabilities.limits.context = None;
        assert_eq!(
            internal_agent_input_limit(&policy.models[0], &policy),
            UNKNOWN_INTERNAL_CONTEXT_LIMIT
        );
    }

    #[test]
    fn mixed_context_fallbacks_fit_per_binding_in_either_order() {
        let mut small = crate::test_support::model_binding_named("fallback-zero");
        small.descriptor.capabilities.limits.context = Some(4_096);
        let mut large = crate::test_support::model_binding_named("fallback-one");
        large.descriptor.capabilities.limits.context = Some(200_000);
        let policy = |models: Vec<_>| {
            let cache_strategies = vec![None; models.len()];
            FrozenInternalAgentPolicy {
                agent: crate::test_support::agent_snapshot(
                    "compaction",
                    cookie_agent_protocol::AgentMode::Internal,
                ),
                models,
                runtime: None,
                limits: InternalAgentLimits {
                    max_output_tokens: 2_048,
                    timeout_ms: 30_000,
                },
                cache_strategies,
            }
        };

        let small_first = policy(vec![small.clone(), large.clone()]);
        assert!(!internal_agent_input_fits(
            10_000,
            &small_first.models[0],
            &small_first
        ));
        assert!(internal_agent_input_fits(
            10_000,
            &small_first.models[1],
            &small_first
        ));

        let large_first = policy(vec![large, small]);
        assert!(internal_agent_input_fits(
            10_000,
            &large_first.models[0],
            &large_first
        ));
        assert!(!internal_agent_input_fits(
            10_000,
            &large_first.models[1],
            &large_first
        ));
    }
}
