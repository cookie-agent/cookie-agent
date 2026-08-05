use super::*;

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn maybe_compact_context(
        &self,
        session: SessionId,
        run: RunId,
        cancellation: &CancellationToken,
        binding: &cookie_agent_protocol::FrozenModelBinding,
        model: &policy::ResolvedRuntimeModel,
        internal_policy: &FrozenInternalAgentPolicy,
        events: Vec<StoredEvent>,
        force: bool,
    ) -> Result<Vec<StoredEvent>, EngineError> {
        let Some(context_limit) = binding.descriptor.capabilities.limits.context else {
            return Ok(events);
        };
        let config = &self.inner.config.runtime.context_compaction;
        let composed_prompt = self.run_agent_prompt(session, run)?;
        let context =
            assemble_model_context(&events, &self.inner.artifacts, binding, &composed_prompt)?;
        let serialized = serde_json::to_vec(&context.history)
            .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?;
        let input_tokens_before = (serialized.len() as u64).div_ceil(4);
        let soft_tokens = context_limit.saturating_mul(config.soft_threshold_percent as u64) / 100;
        let hard_tokens = context_limit.saturating_mul(config.hard_threshold_percent as u64) / 100;
        if !force && input_tokens_before < soft_tokens {
            return Ok(events);
        }
        let input_through_seq = events.last().map_or(0, |event| event.seq);
        if events.iter().rev().any(|event| {
            matches!(
                &event.payload,
                Event::ContextCheckpointCommitted { commit }
                    if commit.boundaries.input_through_seq >= input_through_seq
            )
        }) {
            return Ok(events);
        }
        let hard = input_tokens_before >= hard_tokens;
        let target_tokens = context_limit.saturating_mul(config.target_percent as u64) / 100;
        let previous = events.iter().rev().find_map(|event| match &event.payload {
            Event::ContextCheckpointCommitted { .. } => Some(event.seq),
            _ => None,
        });
        let source_from_seq = previous.map_or(1, |seq| seq.saturating_add(1));
        let boundaries = ContextCheckpointBoundaries {
            source_from_seq,
            source_through_seq: input_through_seq,
            input_through_seq,
            prior_checkpoint_seq: previous,
        };
        let summary_limit = SummaryByteLimit::new(config.max_summary_bytes as u64)
            .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?;

        let native_input_within_budget =
            input_tokens_before <= internal_policy.limits.max_input_tokens;
        if binding.descriptor.capabilities.compaction == CompactionCapability::Native
            && native_input_within_budget
        {
            let invocation_id = InternalAgentInvocationId::new_v7();
            let internal_run_id = InternalAgentRunId::new_v7();
            let backend = InternalAgentBackend::ProviderNative {
                resolved_model: wire_model(binding),
            };
            let digest = Sha256Digest::of_bytes(&serialized);
            self.append(
                session,
                Some(run),
                Event::InternalAgentStarted {
                    invocation_id,
                    internal_run_id,
                    kind: InternalAgentKind::ContextCompaction,
                    backend: backend.clone(),
                    call: SafeInternalAgentCall {
                        name: safe_code("provider_native_compaction"),
                        input_summary: safe_display(&format!("bounded native compaction input ({input_tokens_before} estimated tokens)")),
                        input_digest: digest,
                    },
                },
            )
            .await?;
            let mut request = ModelRequest::new(context.history);
            if let Some(native_context) = context.native_context {
                request = request.with_native_context(native_context);
            }
            let request = model.prepare_request(request);
            let abort = AbortBridge::new(cancellation.child_token());
            let compact_future = model
                .model()
                .compact(CompactionRequest::new(request), abort.signal());
            let compact = tokio::select! {
                result = tokio::time::timeout(
                    std::time::Duration::from_millis(internal_policy.limits.timeout_ms),
                    compact_future,
                ) => match result {
                    Ok(result) => result,
                    Err(_) => {
                        abort.abort();
                        Err(ModelError::timeout("provider-native compaction timed out"))
                    }
                },
                _ = cancellation.cancelled() => {
                    abort.abort();
                    self.append(
                        session,
                        Some(run),
                        Event::InternalAgentCancelled {
                            invocation_id,
                            internal_run_id,
                            kind: InternalAgentKind::ContextCompaction,
                            reason: Some(safe_error("parent run cancelled")),
                        },
                    ).await?;
                    return Err(ModelError::abort("context compaction was cancelled").into());
                }
            };
            match compact {
                Ok(result)
                    if result.native_context.adapter_id() == &binding.descriptor.adapter_id
                        && result.native_context.scope().provider_id
                            == binding.descriptor.identity.provider_id
                        && result.native_context.scope().model_id
                            == binding.descriptor.identity.model_id
                        && result.usage.input_tokens.is_none_or(|tokens| {
                            tokens <= internal_policy.limits.max_input_tokens
                        })
                        && result.usage.output_tokens.is_none_or(|tokens| {
                            tokens <= internal_policy.limits.max_output_tokens
                        }) =>
                {
                    let payload = serde_json::to_vec(&result.native_context).map_err(|error| {
                        EngineError::from(ModelError::native_context(error.to_string()))
                    })?;
                    if payload.len() <= config.max_native_context_bytes {
                        let (reference, digest) = self.inner.artifacts.retain(&payload)?;
                        let checkpoint = ContextCheckpoint::ProviderNative {
                            model: wire_model(binding),
                            native_context: Box::new(NativeContextArtifact {
                                adapter_id: safe_code(result.native_context.adapter_id().as_str()),
                                selection_fingerprint: wire_model(binding).selection_fingerprint,
                                scope: cookie_agent_protocol::NativeContextScope {
                                    provider_id: binding.selection.model.provider_id(),
                                    model_id: binding.selection.model.model_id(),
                                    resource_id: safe_display(
                                        result.native_context.scope().resource_id.as_str(),
                                    ),
                                },
                                byte_length: payload.len() as u64,
                                sha256: Sha256Digest::new(digest).map_err(|error| {
                                    EngineError::from(ModelError::native_context(error.to_string()))
                                })?,
                                reference,
                            }),
                        };
                        self.append(
                            session,
                            Some(run),
                            Event::InternalAgentCompleted {
                                invocation_id,
                                internal_run_id,
                                kind: InternalAgentKind::ContextCompaction,
                                result: SafeInternalAgentResult {
                                    output_summary: safe_display(&format!(
                                        "validated native context ({} bytes)",
                                        payload.len()
                                    )),
                                    output_digest: Sha256Digest::of_bytes(&payload),
                                },
                            },
                        )
                        .await?;
                        let budgets = ContextCheckpointBudgets {
                            context_limit_tokens: context_limit,
                            trigger_tokens: soft_tokens,
                            target_tokens,
                            input_tokens_before,
                            input_tokens_after: target_tokens,
                            max_summary_bytes: summary_limit,
                        };
                        let commit = ContextCheckpointCommit {
                            checkpoint,
                            boundaries,
                            budgets,
                        };
                        commit.validate().map_err(|error| {
                            EngineError::from(ModelError::native_context(error.to_string()))
                        })?;
                        self.append(
                            session,
                            Some(run),
                            Event::ContextCheckpointCommitted { commit },
                        )
                        .await?;
                        return Ok(self.inner.store.get(session)?.log.events());
                    }
                    self.append(
                        session,
                        Some(run),
                        Event::InternalAgentFailed {
                            invocation_id,
                            internal_run_id,
                            kind: InternalAgentKind::ContextCompaction,
                            failure: InternalAgentFailure {
                                code: safe_code("native_context_too_large"),
                                message: safe_error("provider-native context exceeded the configured persistence bound"),
                                retryable: false,
                                model_error: None,
                            },
                        },
                    ).await?;
                }
                Ok(_) => {
                    self.append(
                        session,
                        Some(run),
                        Event::InternalAgentFailed {
                            invocation_id,
                            internal_run_id,
                            kind: InternalAgentKind::ContextCompaction,
                            failure: InternalAgentFailure {
                                code: safe_code("native_context_scope_mismatch"),
                                message: safe_error("provider-native context did not match the exact configured model scope"),
                                retryable: false,
                                model_error: None,
                            },
                        },
                    ).await?;
                }
                Err(error) => {
                    self.append(
                        session,
                        Some(run),
                        Event::InternalAgentFailed {
                            invocation_id,
                            internal_run_id,
                            kind: InternalAgentKind::ContextCompaction,
                            failure: InternalAgentFailure {
                                code: safe_code("native_compaction_failed"),
                                message: safe_error(&error.message),
                                retryable: error.retryable,
                                model_error: Some(model_error_summary(&error)),
                            },
                        },
                    )
                    .await?;
                }
            }
            {
                let fallback_backend = internal_policy.models.first().map_or_else(
                    || InternalAgentBackend::Builtin {
                        name: safe_code("bounded_summary"),
                        revision: safe_display(BOUNDED_SUMMARY_BUILTIN_REVISION),
                    },
                    |binding| InternalAgentBackend::Model {
                        resolved_model: wire_model(binding),
                    },
                );
                self.append(
                    session,
                    Some(run),
                    Event::InternalAgentFallback {
                        invocation_id,
                        internal_run_id,
                        kind: InternalAgentKind::ContextCompaction,
                        from: backend,
                        to: fallback_backend,
                        failure: InternalAgentFailure {
                            code: safe_code("native_compaction_unusable"),
                            message: safe_error(
                                "provider-native compaction did not produce a valid checkpoint",
                            ),
                            retryable: false,
                            model_error: None,
                        },
                        attempts: 1,
                    },
                )
                .await?;
            }
        }

        let durable = serde_json::to_string(&events)
            .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?;
        let summary = self
            .run_internal_text_agent(
                session,
                Some(run),
                InternalAgentKind::ContextCompaction,
                internal_policy,
                format!(
                    "Summarize the durable conversation below without omitting system policy, approval boundaries, attachments, or complete tool call/result pairs. Return summary text only.\n{durable}"
                ),
                cancellation,
            )
            .await;
        match summary {
            Ok(summary) if !summary.text.trim().is_empty() => {
                let checkpoint = InternalSummaryCheckpoint::new(
                    summary.text,
                    summary.invocation_id,
                    summary.internal_run_id,
                    summary_limit,
                )
                .map_err(|error| {
                    EngineError::from(ModelError::invalid_response(error.to_string()))
                })?;
                let input_tokens_after = checkpoint.byte_length().div_ceil(4);
                let budgets = ContextCheckpointBudgets {
                    context_limit_tokens: context_limit,
                    trigger_tokens: soft_tokens,
                    target_tokens,
                    input_tokens_before,
                    input_tokens_after,
                    max_summary_bytes: summary_limit,
                };
                let commit = ContextCheckpointCommit {
                    checkpoint: ContextCheckpoint::InternalSummary { checkpoint },
                    boundaries,
                    budgets,
                };
                commit.validate().map_err(|error| {
                    EngineError::from(ModelError::invalid_response(error.to_string()))
                })?;
                self.append(
                    session,
                    Some(run),
                    Event::ContextCheckpointCommitted { commit },
                )
                .await?;
                Ok(self.inner.store.get(session)?.log.events())
            }
            _ if hard => Err(ModelError::new(
                oven_sdk::ModelErrorKind::ContextLength,
                "hard context limit reached and no valid context checkpoint could be produced",
            )
            .into()),
            _ => Ok(events),
        }
    }
}
