use super::*;

impl Engine {
    pub(super) async fn start_run_direct(
        &self,
        params: RunStartParams,
        admission: Option<(InvocationId, u64)>,
    ) -> Result<RunStartResult, EngineError> {
        if let Some((invocation_id, generation)) = admission
            && !self.admission_generation_live(invocation_id, generation)
        {
            return Err(EngineError::MissingTool(
                "delegate admission was abandoned or superseded".into(),
            ));
        }
        let session = self.inner.store.get(params.session_id)?;
        if let Some(run) = session
            .runs
            .values()
            .find(|run| run.client_run_id == params.client_run_id)
        {
            if run.input != params.input || run.selection != params.selection {
                return Err(EngineError::RunIdempotencyConflict);
            }
            return Ok(RunStartResult { run_id: run.id });
        }
        if session.status == SessionStatus::Running {
            return Err(EngineError::SessionRunning(params.session_id));
        }
        self.resolve_interrupted_direct(params.session_id).await?;
        let result_limits = policy::ResultLimits {
            tool_output_max_lines: self.inner.config.runtime.tool_output.max_lines,
            tool_output_max_bytes: self.inner.config.runtime.tool_output.max_bytes,
        };
        let run_policy = match &session.meta.origin {
            SessionOrigin::Root => {
                self.reconcile_provider_store()?;
                let runtime = self.current_runtime();
                let agents = Arc::clone(&runtime.agents);
                let agent = resolve_agent(&agents, &params.selection.agent)?;
                if !agent.runnable_as_root {
                    return Err(EngineError::NoRunnableModel);
                }
                freeze_root_agent_policy(
                    agent,
                    Arc::clone(&agents),
                    runtime,
                    &params.selection.model,
                    self.inner.config.runtime.delegation.max_depth,
                    result_limits,
                )?
            }
            SessionOrigin::Delegated { .. } => {
                let runtime = self.current_runtime();
                let agents = Arc::clone(&runtime.agents);
                policy_for_session_selection(
                    session.creation_agent.clone(),
                    agents,
                    runtime,
                    &params.selection,
                    result_limits.tool_output_max_lines,
                    result_limits.tool_output_max_bytes,
                )?
            }
        };
        let run_id = RunId::new_v7();
        let input_through_seq = session.meta.last_event_seq;
        self.append(
            params.session_id,
            Some(run_id),
            Event::RunStarted {
                client_run_id: params.client_run_id.clone(),
                selection: params.selection.clone(),
                agent: Box::new(run_policy.agent.clone()),
                runtime_revision: run_policy.runtime.result.snapshot.runtime_revision.clone(),
                catalog_revision: run_policy.runtime.result.snapshot.catalog_revision.clone(),
                provider_state_revision: run_policy
                    .runtime
                    .result
                    .snapshot
                    .provider_state_revision
                    .clone(),
                model_revision: run_policy.runtime.result.snapshot.model_revision.clone(),
                agent_revision: run_policy.runtime.result.snapshot.agent_revision.clone(),
                recipe_registry_revision: run_policy
                    .runtime
                    .result
                    .snapshot
                    .recipe_registry_revision
                    .clone(),
                manifest_revision: run_policy.selected_suffix_wire[0].manifest_revision.clone(),
                selected_suffix: run_policy.selected_suffix_wire.clone(),
                input_through_seq,
            },
        )
        .await?;
        let active = Arc::new(ActiveRun {
            session: params.session_id,
            policy: Arc::new(run_policy),
            cancellation: CancellationToken::new(),
            cancelled_committed: Mutex::new(false),
            stdin: Mutex::new(HashMap::new()),
            prompt_seq: AtomicU64::new(0),
            fallback_index: AtomicU64::new(0),
        });
        self.inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id, active.clone());
        let compacted = self
            .maybe_predictive_compact_before_input(PredictiveCompactionInput {
                session: params.session_id,
                run: run_id,
                input: &params.input,
                policy: &active.policy,
                fallback_index: 0,
                cancellation: &active.cancellation,
                actor_direct: false,
            })
            .await;
        if active.cancellation.is_cancelled() {
            self.append_run_cancelled_once(&active, run_id, None)?;
            self.inner
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&run_id);
            return Ok(RunStartResult { run_id });
        }
        if let Err(error) = compacted {
            self.inner
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&run_id);
            return Err(error);
        }
        if let Err(error) = self
            .append(
                params.session_id,
                Some(run_id),
                Event::UserInputSubmitted {
                    input: params.input,
                },
            )
            .await
        {
            self.inner
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&run_id);
            return Err(error);
        }
        // A sweeper may have terminalized this durable run before active-run
        // registration. Never resurrect a cancelled run with a live loop.
        if self.run_cancelled_recorded(params.session_id, run_id)? {
            return Ok(RunStartResult { run_id });
        }
        if let Some((invocation_id, generation)) = admission
            && let Err(error) =
                self.publish_admission_run(invocation_id, generation, params.session_id, run_id)
        {
            let cancelled = self
                .cancel_run_durably(run_id, Some("delegate admission publication failed".into()));
            self.inner
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&run_id);
            cancelled?;
            return Err(error);
        }
        if self.run_cancelled_recorded(params.session_id, run_id)? {
            self.inner
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&run_id);
            return Ok(RunStartResult { run_id });
        }
        let engine = self.clone();
        tokio::spawn(async move {
            if let Err(error) = engine.run_loop(run_id, active).await {
                // A provider-attempt persistence error may also prevent the
                // terminal append. Retain this active tombstone for reopen
                // reconciliation rather than clearing a durably Running run.
                eprintln!("run {run_id} terminalization failed: {error}");
                return;
            }
            if let Ok(mut active_runs) = engine.inner.active.lock() {
                active_runs.remove(&run_id);
            }
        });
        Ok(RunStartResult { run_id })
    }

    pub(super) async fn maybe_predictive_compact_before_input(
        &self,
        input: PredictiveCompactionInput<'_>,
    ) -> Result<(), EngineError> {
        let Some(binding) = input.policy.selected_suffix.get(input.fallback_index) else {
            return Ok(());
        };
        let Some(context_limit) = binding.descriptor.capabilities.limits.context else {
            return Ok(());
        };
        let config = &self.inner.config.runtime.context_compaction;
        let trigger_tokens = effective_compaction_limit(context_limit, config.buffer_tokens);
        let message_bytes = serde_json::to_vec(input.input)
            .map_err(|error| ModelError::invalid_request(error.to_string()))?
            .len();
        let estimator = self
            .inner
            .context_token_estimators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&input.session)
            .copied()
            .unwrap_or_default();
        if !config.auto_compaction
            || trigger_tokens == 0
            || self
                .inner
                .compaction_auto_disabled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&input.session)
            || !should_run_predictive_compaction(
                estimator,
                message_bytes,
                trigger_tokens,
                self.inner.store.is_persisted(input.session)?,
            )
        {
            return Ok(());
        }
        let events = self.inner.store.get(input.session)?.log.events();
        let tools = self.tool_definitions(input.session, input.policy)?;
        let internal_policy = self.internal_agent_policy(
            InternalAgentKind::ContextCompaction,
            input.policy,
            Some(binding),
        )?;
        self.maybe_compact_context(CompactionInput {
            session: input.session,
            run: input.run,
            cancellation: input.cancellation,
            binding,
            owner_policy: input.policy,
            internal_policy: &internal_policy,
            tools: &tools,
            events,
            force: true,
            focus: None,
            actor_direct: input.actor_direct,
        })
        .await?;
        Ok(())
    }

    pub(super) async fn run_loop(
        &self,
        run_id: RunId,
        active: Arc<ActiveRun>,
    ) -> Result<(), EngineError> {
        // Sticky chain position belongs to this run, not one agent-loop pass.
        let mut fallback_entry = 0_usize;
        loop {
            if active.cancellation.is_cancelled() {
                self.append_run_cancelled_once(&active, run_id, None)?;
                return Ok(());
            }
            let tools = self.tool_definitions(active.session, &active.policy)?;
            let prompt_events = self.prompt_events(active.session, run_id).await?;
            let attempt = match self
                .stream_attempt(
                    active.session,
                    run_id,
                    &active.cancellation,
                    &active.policy,
                    &mut fallback_entry,
                    prompt_events,
                    tools,
                )
                .await
            {
                Ok(events) => events,
                Err(error) => {
                    if active.cancellation.is_cancelled() {
                        self.append_run_cancelled_once(&active, run_id, None)?;
                        return Ok(());
                    }
                    self.append(
                        active.session,
                        Some(run_id),
                        Event::RunFailed {
                            error: safe_error(&error.to_string()),
                        },
                    )
                    .await?;
                    return Ok(());
                }
            };
            let final_text = attempt
                .turn
                .content
                .iter()
                .filter_map(|part| match part {
                    PersistedAssistantPart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            if matches!(
                attempt.turn.finish_reason,
                cookie_agent_protocol::ModelFinishReason::Cancelled
                    | cookie_agent_protocol::ModelFinishReason::Aborted
            ) {
                active.cancellation.cancel();
            }
            let in_stream_results = attempt
                .turn
                .content
                .iter()
                .filter_map(|part| match part {
                    PersistedAssistantPart::ToolResult { tool_call_id, .. } => {
                        Some(tool_call_id.as_str())
                    }
                    _ => None,
                })
                .collect::<HashSet<_>>();
            let approvals = attempt
                .turn
                .content
                .iter()
                .filter_map(|part| match part {
                    PersistedAssistantPart::ToolApproval {
                        tool_call_id,
                        message,
                        ..
                    } => Some((tool_call_id.as_str(), message.clone())),
                    _ => None,
                })
                .collect::<HashMap<_, _>>();
            let calls = attempt
                .turn
                .content
                .iter()
                .enumerate()
                .filter_map(|(content_index, part)| match part {
                    PersistedAssistantPart::ToolCall {
                        id,
                        provider_item_id,
                        name,
                        input,
                        ..
                    } if !in_stream_results.contains(id.as_str()) => Some((
                        ToolCallId::new_v7(),
                        content_index as u32,
                        id.clone(),
                        provider_item_id.clone(),
                        name.clone(),
                        input.clone(),
                        approvals.get(id.as_str()).cloned().flatten(),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if active.cancellation.is_cancelled() {
                self.append_run_cancelled_once(&active, run_id, None)?;
                return Ok(());
            }
            if calls.is_empty() {
                let steering = self
                    .request(active.session, |reply| {
                        SessionCommand::CompleteIfNoSteering {
                            run: run_id,
                            final_text: (!final_text.is_empty()).then_some(final_text.clone()),
                            reply,
                        }
                    })
                    .await?;
                if steering {
                    continue;
                }
                return Ok(());
            }
            if calls.len() > MAX_PENDING_PREPARED_TOOLS {
                return Err(ModelError::invalid_response(format!(
                    "model requested {} prepared tools; the limit is {MAX_PENDING_PREPARED_TOOLS}",
                    calls.len()
                ))
                .into());
            }
            let mut prepared = Vec::new();
            for (id, content_index, model_call_id, provider_item_id, tool, arguments, approval) in
                &calls
            {
                self.inner
                    .output_hubs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .entry(*id)
                    .or_insert_with(|| OutputHub::new(*id, 64 * 1024));
                let call = ToolCall {
                    id: *id,
                    name: tool.to_string(),
                    arguments: arguments.clone(),
                };
                let prepared_call = self
                    .prepare_tool_call(active.session, run_id, call, &active.policy)
                    .await;
                let operation_fingerprint = prepared_call.prepared.as_ref().map_or_else(
                    |_| fallback_operation_fingerprint(&prepared_call.call),
                    |prepared| OperationFingerprint::from_prepared_operation(prepared.operation()),
                );
                self.append(
                    active.session,
                    Some(run_id),
                    Event::ToolCallStarted {
                        start: ToolCallStart {
                            tool_call_id: *id,
                            owner: cookie_agent_protocol::AssistantToolCallRef {
                                model_turn_seq: attempt.model_turn_seq,
                                content_index: *content_index,
                                model_call_id: model_call_id.clone(),
                                provider_item_id: provider_item_id.clone(),
                            },
                            presentation: prepared_call.presentation.clone(),
                            operation_fingerprint,
                        },
                    },
                )
                .await?;
                prepared.push((prepared_call, approval.clone()));
            }
            let mut tasks = Vec::new();
            for (prepared, approval) in prepared {
                if prepared.prepared.is_err() {
                    let Err(error) = prepared.prepared else {
                        unreachable!()
                    };
                    tasks.push(PendingTool::ImmediateFailure(error));
                    continue;
                }
                if let Some(message) = approval {
                    let outcome = self
                        .request_model_approval(
                            &active,
                            run_id,
                            ModelApprovalInput {
                                operation: prepared
                                    .prepared
                                    .as_ref()
                                    .expect("prepared operation")
                                    .operation(),
                                policy_labels: &prepared
                                    .prepared
                                    .as_ref()
                                    .expect("prepared operation")
                                    .policy_labels,
                                executor: prepared
                                    .prepared
                                    .as_ref()
                                    .expect("prepared operation")
                                    .executor
                                    .clone(),
                                message: Some(message),
                                tool: ApprovalToolInput {
                                    name: &prepared.call.name,
                                    normalized_parameters: prepared
                                        .prepared
                                        .as_ref()
                                        .expect("prepared operation")
                                        .normalized_arguments(),
                                },
                            },
                        )
                        .await?;
                    if outcome.approved {
                        tasks.push(PendingTool::Prepared(Box::new(prepared)));
                    } else {
                        tasks.push(PendingTool::ImmediateFailure(ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message: denied_tool_failure(
                                ApprovalDecisionSource::Model,
                                "model approval refused by user",
                                outcome.feedback,
                            ),
                        }));
                    }
                } else {
                    tasks.push(PendingTool::Prepared(Box::new(prepared)));
                }
            }
            // Awaiting task handles is outside any session actor. Results are
            // committed in provider tool-call order, regardless of completion order.
            for (id, task) in calls.iter().map(|call| call.0).zip(tasks) {
                let result = if active.cancellation.is_cancelled() {
                    Err(ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: "tool call cancelled after it started".into(),
                    })
                } else {
                    match task {
                        PendingTool::Prepared(prepared) => {
                            self.execute_tool(active.clone(), run_id, *prepared).await
                        }
                        PendingTool::ImmediateFailure(failure) => Err(failure),
                    }
                };
                self.submit_tool_result_status(active.session, run_id, id, result)
                    .await?;
            }
            if active.cancellation.is_cancelled() {
                self.append_run_cancelled_once(&active, run_id, None)?;
                return Ok(());
            }
        }
    }

    /// Streams one Oven attempt directly into the session actor and commits a
    /// complete turn only after strict lifecycle validation succeeds.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn stream_attempt(
        &self,
        session: SessionId,
        run: RunId,
        cancellation: &CancellationToken,
        policy: &FrozenRunPolicy,
        sticky_entry: &mut usize,
        prompt_events: Vec<StoredEvent>,
        tools: Vec<ToolDefinition>,
    ) -> Result<AttemptTurn, EngineError> {
        let chain = &policy.selected_suffix;
        let composed_prompt = &policy.agent.composed_prompt;
        let prompt_fingerprint = &policy.agent.prompt_fingerprint;
        let mut entry = *sticky_entry;
        let mut last_error = ModelError::invalid_request("model fallback chain is empty");
        let mut first_request = true;
        while entry < chain.len() {
            let binding = &chain[entry];
            let model = policy::resolve_model(binding, &policy.runtime)?;
            let mut attempts = 0_u32;
            let mut context_recovery_attempted = false;
            loop {
                attempts += 1;
                let attempt_id = cookie_agent_protocol::AttemptId::new_v7();
                let attempt_ordinal = self
                    .inner
                    .store
                    .get(session)?
                    .log
                    .events()
                    .iter()
                    .filter(|event| {
                        event.run_id == Some(run)
                            && matches!(event.payload, Event::ModelAttemptStarted { .. })
                    })
                    .count() as u32
                    + 1;
                self.append(
                    session,
                    Some(run),
                    Event::ModelAttemptStarted {
                        attempt_id,
                        attempt_ordinal,
                        fallback_index: entry as u32,
                        retry_ordinal: attempts - 1,
                        resolved_model: wire_model(binding),
                        prompt_fingerprint: prompt_fingerprint.clone(),
                    },
                )
                .await?;
                let request_events = if first_request {
                    first_request = false;
                    prompt_events.clone()
                } else {
                    self.prompt_events(session, run).await?
                };
                let compaction_policy = self.internal_agent_policy(
                    InternalAgentKind::ContextCompaction,
                    policy,
                    Some(binding),
                )?;
                let request_events = self
                    .maybe_compact_context(CompactionInput {
                        session,
                        run,
                        cancellation,
                        binding,
                        owner_policy: policy,
                        internal_policy: &compaction_policy,
                        tools: &tools,
                        events: request_events,
                        force: false,
                        focus: None,
                        actor_direct: false,
                    })
                    .await?;
                let input_through_seq = request_events.last().map_or(0, |event| event.seq);
                let context = assemble_model_context(
                    &request_events,
                    &self.inner.artifacts,
                    binding,
                    composed_prompt,
                )?;
                let serialized_history_bytes = serde_json::to_vec(&context.history)
                    .map_err(|error| ModelError::invalid_request(error.to_string()))?
                    .len();
                let replay_preflight = context.replay_decisions;
                let request = ModelRequest::new(context.history).with_tools(tools.clone());
                let request = model.prepare_request(request);
                let abort = AbortBridge::new(cancellation.clone());
                let response = tokio::select! {
                    result = model.model().stream(request, abort.signal()) => result,
                    _ = cancellation.cancelled() => {
                        abort.abort();
                        Err(ModelError::abort("model request was cancelled"))
                    }
                };
                let (result, meaningful_output) = match response {
                    Ok(response) => {
                        let oven_sdk::StreamResponse {
                            mut stream,
                            request,
                            response,
                        } = response;
                        self.append(
                            session,
                            Some(run),
                            Event::ModelReplayEvaluated {
                                attempt_id,
                                resolved_model: wire_model(binding),
                                ordered_decisions: replay_decisions_with_preflight(
                                    &request.replay.decisions,
                                    binding,
                                    &replay_preflight,
                                ),
                            },
                        )
                        .await?;
                        let mut accumulator = TurnAccumulator::default();
                        let mut failure = None;
                        let mut meaningful_output = false;
                        loop {
                            let item = tokio::select! {
                                item = stream.next() => item,
                                _ = cancellation.cancelled() => {
                                    abort.abort();
                                    failure = Some(Box::new(ModelError::abort("model stream was cancelled")));
                                    break;
                                }
                            };
                            let Some(item) = item else { break };
                            match item {
                                Ok(part) => match accumulator.push(part) {
                                    Ok(effect) => {
                                        meaningful_output |= effect.meaningful;
                                        if let Some(text) = effect.text_delta {
                                            self.append(
                                                session,
                                                Some(run),
                                                Event::TextDelta { attempt_id, text },
                                            )
                                            .await?;
                                        }
                                        if let Some(text) = effect.reasoning_delta {
                                            self.append(
                                                session,
                                                Some(run),
                                                Event::ReasoningDelta { attempt_id, text },
                                            )
                                            .await?;
                                        }
                                    }
                                    Err(error) => {
                                        failure = Some(error);
                                        break;
                                    }
                                },
                                Err(error) => {
                                    failure = Some(Box::new(error));
                                    break;
                                }
                            }
                        }
                        let completed = match failure {
                            Some(error) => Err(error),
                            None => accumulator.finish(),
                        };
                        let completed = completed.map(|mut turn| {
                            for (key, value) in response.response_metadata {
                                turn.finish.response_metadata.entry(key).or_insert(value);
                            }
                            if let Some(status) = response.http_status {
                                turn.finish
                                    .response_metadata
                                    .entry("oven.http_status".into())
                                    .or_insert_with(|| serde_json::Value::from(status));
                            }
                            if let Some(request_id) = response.request_id {
                                turn.finish
                                    .response_metadata
                                    .entry("oven.request_id".into())
                                    .or_insert_with(|| serde_json::Value::from(request_id));
                            }
                            if !request.provider_metadata.is_empty() {
                                turn.finish.provider_metadata.insert(
                                    "oven.request".into(),
                                    serde_json::to_value(request.provider_metadata)
                                        .expect("safe request metadata serializes"),
                                );
                            }
                            turn
                        });
                        (completed, meaningful_output)
                    }
                    Err(error) => (Err(Box::new(error)), false),
                };
                match result {
                    Ok(turn) => {
                        let (turn, warnings) = persist_turn(turn, &self.inner.artifacts, binding)?;
                        let resolved_model = wire_model(binding);
                        let model_turn_seq = self.next_model_turn_seq(session)?;
                        self.append(
                            session,
                            Some(run),
                            Event::ModelTurnCommitted {
                                attempt_id,
                                model_turn_seq,
                                resolved_model,
                                input_through_seq,
                                turn: turn.clone(),
                                warnings,
                            },
                        )
                        .await?;
                        self.inner
                            .context_token_estimators
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .entry(session)
                            .or_default()
                            .record_committed_turn(
                                serialized_history_bytes,
                                turn.usage.input_tokens,
                            );
                        let title_policy = self.internal_agent_policy(
                            InternalAgentKind::SessionTitle,
                            policy,
                            Some(binding),
                        )?;
                        self.maybe_generate_session_title(
                            session,
                            run,
                            input_through_seq,
                            cancellation,
                            &title_policy,
                        )
                        .await?;
                        return Ok(AttemptTurn {
                            turn,
                            model_turn_seq,
                        });
                    }
                    Err(error)
                        if error.kind == oven_sdk::ModelErrorKind::ContextLength
                            && !meaningful_output
                            && !context_recovery_attempted =>
                    {
                        self.append(session, Some(run), Event::AttemptAbandoned { attempt_id })
                            .await?;
                        context_recovery_attempted = true;
                        let before = self
                            .inner
                            .store
                            .get(session)?
                            .log
                            .events()
                            .iter()
                            .rev()
                            .find_map(|event| {
                                matches!(event.payload, Event::ContextCheckpointCommitted { .. })
                                    .then_some(event.seq)
                            })
                            .unwrap_or(0);
                        let recovery_events = self.prompt_events(session, run).await?;
                        let recovery_policy = self.internal_agent_policy(
                            InternalAgentKind::ContextCompaction,
                            policy,
                            Some(binding),
                        )?;
                        let recovered = self
                            .maybe_compact_context(CompactionInput {
                                session,
                                run,
                                cancellation,
                                binding,
                                owner_policy: policy,
                                internal_policy: &recovery_policy,
                                tools: &tools,
                                events: recovery_events,
                                force: true,
                                focus: None,
                                actor_direct: false,
                            })
                            .await?;
                        let after = recovered
                            .iter()
                            .rev()
                            .find_map(|event| {
                                matches!(event.payload, Event::ContextCheckpointCommitted { .. })
                                    .then_some(event.seq)
                            })
                            .unwrap_or(0);
                        if after > before {
                            continue;
                        }
                        return Err(EngineError::Model(error));
                    }
                    Err(error) if classify_model_error(&error) == ErrorPolicy::FailRun => {
                        self.append(session, Some(run), Event::AttemptAbandoned { attempt_id })
                            .await?;
                        return Err(EngineError::Model(error));
                    }
                    Err(error)
                        if classify_model_error(&error) == ErrorPolicy::RetryEntry
                            && attempts <= 2
                            && !meaningful_output =>
                    {
                        self.append(session, Some(run), Event::AttemptAbandoned { attempt_id })
                            .await?;
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_millis(100_u64 << (attempts - 1))) => {}
                            _ = cancellation.cancelled() => return Err(ModelError::abort("model retry was cancelled").into()),
                        }
                    }
                    Err(error) => {
                        self.append(session, Some(run), Event::AttemptAbandoned { attempt_id })
                            .await?;
                        last_error = *error;
                        break;
                    }
                }
            }
            let Some(next) = chain.get(entry + 1) else {
                return Err(last_error.into());
            };
            self.append(
                session,
                Some(run),
                Event::ModelFallback {
                    from: wire_model(binding),
                    to: wire_model(next),
                    from_fallback_index: entry as u32,
                    to_fallback_index: entry as u32 + 1,
                    error: model_error_summary(&last_error),
                    attempts_on_from: attempts,
                },
            )
            .await?;
            entry += 1;
            *sticky_entry = entry;
            if let Ok(active_runs) = self.inner.active.lock()
                && let Some(active) = active_runs.get(&run)
            {
                active.fallback_index.store(entry as u64, Ordering::Release);
            }
        }
        Err(last_error.into())
    }
}
