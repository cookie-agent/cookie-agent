use super::*;

impl Engine {
    pub(super) async fn run_internal_text_agent(
        &self,
        session: SessionId,
        parent_run: Option<RunId>,
        kind: InternalAgentKind,
        policy: &FrozenInternalAgentPolicy,
        input: String,
        cancellation: &CancellationToken,
    ) -> Result<InternalAgentTextResult, EngineError> {
        let name = match kind {
            InternalAgentKind::Approval => "approval",
            InternalAgentKind::ContextCompaction => "context_compaction",
            InternalAgentKind::SessionTitle => "session_title",
        };
        let policy = policy.clone();
        let max_input_bytes = usize::try_from(policy.limits.max_input_tokens)
            .unwrap_or(usize::MAX)
            .saturating_mul(4);
        let input = truncate_utf8(&input, max_input_bytes);
        let invocation_id = InternalAgentInvocationId::new_v7();
        let internal_run_id = InternalAgentRunId::new_v7();
        let call = SafeInternalAgentCall {
            name: safe_code(name),
            input_summary: safe_display(&format!("bounded {name} input ({} bytes)", input.len())),
            input_digest: Sha256Digest::of_bytes(input.as_bytes()),
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
                self.append(
                    session,
                    parent_run,
                    Event::InternalAgentStarted {
                        invocation_id,
                        internal_run_id,
                        kind,
                        backend: backend.clone(),
                        call: call.clone(),
                    },
                )
                .await?;
            } else if let Some(from) = previous_backend.take() {
                self.append(
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
                )
                .await?;
            }
            let runtime = policy
                .runtime
                .as_ref()
                .ok_or(EngineError::NoRunnableModel)?;
            let model = policy::resolve_model(binding, runtime)?;
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
            let mut request = ModelRequest::new(history);
            request.inference.max_output_tokens = Some(policy.limits.max_output_tokens);
            let request = model.prepare_request(request);
            let abort = AbortBridge::new(cancellation.child_token());
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
                _ = cancellation.cancelled() => {
                    abort.abort();
                    self.append(
                        session,
                        parent_run,
                        Event::InternalAgentCancelled {
                            invocation_id,
                            internal_run_id,
                            kind,
                            reason: Some(safe_error("parent run cancelled")),
                        },
                    ).await?;
                    return Err(ModelError::abort("internal agent was cancelled").into());
                }
            };
            match result {
                Ok(completed) => {
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
                    self.append(
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
            self.append(
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
            )
            .await?;
        }
        self.append(
            session,
            parent_run,
            Event::InternalAgentFailed {
                invocation_id,
                internal_run_id,
                kind,
                failure: last_failure,
            },
        )
        .await?;
        Err(ModelError::invalid_response("internal agent failed safely").into())
    }

    pub(super) fn active_internal_policy(
        &self,
        active: &ActiveRun,
        kind: InternalAgentKind,
    ) -> FrozenInternalAgentPolicy {
        let fallback_index = active.fallback_index.load(Ordering::Acquire) as usize;
        self.inner.internal_agents.policy(
            kind,
            &active.policy,
            active.policy.active_suffix(fallback_index),
        )
    }
}

pub(super) fn unavailable_internal_policy(
    timeout_ms: u64,
    max_input_tokens: u64,
    max_output_tokens: u64,
) -> FrozenInternalAgentPolicy {
    FrozenInternalAgentPolicy {
        agent: cookie_agent_protocol::AgentSnapshot {
            agent: AgentId::new("internal").expect("static agent id"),
            schema: cookie_agent_protocol::AgentSchemaVersion::current(),
            mode: AgentMode::All,
            description: "Internal engine work".into(),
            document_source: cookie_agent_protocol::AgentDocumentSource::BuiltIn,
            document_fingerprint: Sha256Digest::of_bytes(b"internal"),
            composed_prompt: "Perform the requested internal engine task safely.\n".into(),
            prompt_fingerprint: Sha256Digest::of_bytes(
                b"Perform the requested internal engine task safely.\n",
            ),
            tools: Vec::new(),
            permissions: Vec::new(),
            delegation: None,
            fallback_chain: Vec::new(),
            selected_suffix_start: 0,
        },
        models: Vec::new(),
        runtime: None,
        limits: InternalAgentLimits {
            max_input_tokens,
            max_output_tokens,
            timeout_ms,
        },
    }
}

pub(super) fn inherit_internal_policy(
    configured: &FrozenInternalAgentPolicy,
    owner: &FrozenRunPolicy,
    active_suffix: &[cookie_agent_protocol::FrozenModelBinding],
) -> FrozenInternalAgentPolicy {
    FrozenInternalAgentPolicy {
        agent: owner.agent.clone(),
        models: if configured.models.is_empty() {
            active_suffix.to_vec()
        } else {
            configured.models.clone()
        },
        runtime: Some(Arc::clone(&owner.runtime)),
        limits: configured.limits.clone(),
    }
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
