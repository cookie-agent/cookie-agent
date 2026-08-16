use std::sync::{Arc, atomic::Ordering};

use cookie_agent_protocol::{
    AgentId, AgentMode, ApprovalInternalDecisionKind, InternalAgentBackend, InternalAgentFailure,
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
        let max_input_bytes = usize::try_from(policy.limits.max_input_tokens)
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
        let max_input_tokens = internal_agent_input_limit(kind, &policy);
        if input_tokens > max_input_tokens {
            return Err(ModelError::invalid_request(format!(
                "internal agent input is {input_tokens} estimated tokens, exceeding the frozen {max_input_tokens}-token limit"
            ))
            .into());
        }
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
            let runtime = policy
                .runtime
                .as_ref()
                .ok_or(EngineError::NoRunnableModel)?;
            let model = policy::resolve_model(binding, runtime)?;
            let max_output_tokens = binding
                .descriptor
                .capabilities
                .limits
                .output
                .unwrap_or(policy.limits.max_output_tokens)
                .min(policy.limits.max_output_tokens);
            let request = internal_model_request(
                input.history.clone(),
                input.tools.clone(),
                max_output_tokens,
            );
            let request = model.prepare_request(request);
            let abort = AbortBridge::new(execution.cancellation.child_token());
            let call_future = model.model().complete(request, abort.signal());
            let result = tokio::select! {
                result = tokio::time::timeout(
                    std::time::Duration::from_millis(policy.limits.timeout_ms),
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
                    self.append_internal_agent_event(
                        session,
                        parent_run,
                        Event::InternalAgentUsageRecorded {
                            internal_run_id,
                            kind,
                            agent_id: policy.agent.agent.clone(),
                            resolved_model: wire_model(binding),
                            usage: crate::model_history::persist_usage(
                                completed.turn.finish.usage.clone(),
                            ),
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
                    let max_output_bytes = usize::try_from(policy.limits.max_output_tokens)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(4);
                    if output.len() > max_output_bytes {
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
            self.append_direct(session, run, event)
        } else {
            self.append(session, run, event).await
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

    pub(crate) fn internal_agent_policy(
        &self,
        kind: InternalAgentKind,
        owner: &FrozenRunPolicy,
        parent_binding: Option<&cookie_agent_protocol::FrozenModelBinding>,
    ) -> Result<FrozenInternalAgentPolicy, EngineError> {
        let id = AgentId::new(match kind {
            InternalAgentKind::Approval => cookie_agent_config::BUILT_IN_APPROVAL_AGENT_ID,
            InternalAgentKind::ContextCompaction => {
                cookie_agent_config::BUILT_IN_COMPACTION_AGENT_ID
            }
            InternalAgentKind::SessionTitle => cookie_agent_config::BUILT_IN_TITLE_AGENT_ID,
        })
        .map_err(|_| EngineError::RuntimeCompileFailed)?;
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
                crate::runtime_snapshot::ResolvedAgentFallback::ParentModel => {
                    if let Some(binding) = parent_binding {
                        models.push(binding.clone());
                    }
                }
                crate::runtime_snapshot::ResolvedAgentFallback::Selection(selection) => {
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
            permissions: Vec::new(),
            delegation: None,
            fallback_chain: models.clone(),
            selected_suffix_start: 0,
        };
        let limits = &document.frontmatter.limits;
        Ok(FrozenInternalAgentPolicy {
            agent: snapshot,
            models,
            runtime: Some(Arc::clone(&owner.runtime)),
            limits: InternalAgentLimits {
                max_input_tokens: limits.max_input_tokens,
                max_output_tokens: limits.max_output_tokens,
                timeout_ms: limits.timeout_ms,
            },
        })
    }
}

pub(super) fn internal_agent_input_limit(
    kind: InternalAgentKind,
    policy: &FrozenInternalAgentPolicy,
) -> u64 {
    if kind != InternalAgentKind::ContextCompaction {
        return policy.limits.max_input_tokens;
    }
    policy
        .models
        .iter()
        .filter_map(|binding| binding.descriptor.capabilities.limits.context)
        .max()
        .unwrap_or(0)
        .max(policy.limits.max_input_tokens)
}

fn internal_model_request(
    history: Vec<oven_sdk::HistoryTurn>,
    tools: Vec<ToolDefinition>,
    max_output_tokens: u64,
) -> ModelRequest {
    let mut request = ModelRequest::new(history).with_tools(tools);
    request.inference.max_output_tokens = Some(max_output_tokens);
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
    reject_non_text
        && parts
            .iter()
            .any(|part| !matches!(part, oven_sdk::AssistantPart::Text(_)))
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
        FrozenInternalAgentPolicy, InternalAgentKind, InternalAgentLimits,
        internal_agent_input_limit, internal_history_tokens, internal_model_request,
        invalid_internal_output,
    };

    #[test]
    fn internal_model_requests_are_structurally_toolless() {
        let request = internal_model_request(Vec::new(), Vec::new(), 128);
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
    fn history_input_estimate_counts_the_full_history_and_tools() {
        let history = vec![oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(
            vec![oven_sdk::InputPart::Text(oven_sdk::TextPart::new(
                "x".repeat(4_000),
            ))],
        ))];
        assert!(internal_history_tokens(&history, &[]).unwrap() >= 1_000);
    }

    #[test]
    fn compaction_input_limit_scales_to_parent_context_window() {
        let mut binding = crate::test_support::model_binding();
        binding.descriptor.capabilities.limits.context = Some(200_000);
        let policy = FrozenInternalAgentPolicy {
            agent: crate::test_support::agent_snapshot(
                "compaction",
                cookie_agent_protocol::AgentMode::Internal,
            ),
            models: vec![binding],
            runtime: None,
            limits: InternalAgentLimits {
                max_input_tokens: 16_384,
                max_output_tokens: 2_048,
                timeout_ms: 30_000,
            },
        };
        assert_eq!(
            internal_agent_input_limit(InternalAgentKind::ContextCompaction, &policy),
            200_000
        );
        assert_eq!(
            internal_agent_input_limit(InternalAgentKind::Approval, &policy),
            16_384
        );
    }
}
