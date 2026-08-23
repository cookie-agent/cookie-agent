use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use cookie_agent_protocol::{
    ApprovalDecisionSource, ExtensionAgentBeforeStartParams, ExtensionToolAfterResultAction,
    ExtensionToolAfterResultParams, ExtensionUserBeforeInputAction, ExtensionUserBeforeInputParams,
    InternalAgentKind, InvocationId, OperationFingerprint, PersistedAssistantPart,
    PluginDiagnosticKind, RunId, RunStartParams, RunStartResult, SessionId, SessionOrigin,
    SessionStatus, Sha256Digest, StoredEvent, ToolCallId, ToolCallStart,
};
use futures_util::StreamExt;
use oven_sdk::{ModelError, Request as ModelRequest, ToolDefinition};
use tokio_util::sync::CancellationToken;

use super::{
    ActiveRun, ApprovalToolInput, AttemptTurn, Engine, EngineError, Event,
    MAX_PENDING_PREPARED_TOOLS, ModelApprovalInput, PendingTool, PredictiveCompactionInput,
    SessionCommand, ToolCallFailureCode, ToolFailure, UserInputInterception,
    approval_projection::denied_tool_failure,
    compaction::{CompactionInput, latest_checkpoint_seq, resolve_compaction_trigger},
    helpers::safe_error,
    should_run_predictive_compaction,
    tool_execution::fallback_operation_fingerprint,
};
use crate::{
    events::OutputHub,
    model_bridge::{AbortBridge, TurnAccumulator},
    model_history::{
        assemble_model_context, persist_turn, replay_decisions_with_preflight, wire_model,
    },
    model_policy::{ErrorPolicy, classify as classify_model_error, summary as model_error_summary},
    policy::{
        self, FrozenRunPolicy, freeze_root_agent_policy, policy_for_session_selection,
        resolve_agent,
    },
    tool_api::{ToolCall, TurnAgentContext},
};

impl Engine {
    pub(super) async fn start_run_direct(
        &self,
        mut params: RunStartParams,
        admission: Option<(InvocationId, u64)>,
    ) -> Result<RunStartResult, EngineError> {
        self.inner.mcp.await_eager_ready().await;
        self.inner.plugins.await_eager_ready().await;
        if let Some((invocation_id, generation)) = admission
            && !self.admission_generation_live(invocation_id, generation)
        {
            return Err(EngineError::MissingTool(
                "delegate admission was abandoned or superseded".into(),
            ));
        }
        let session = self.inner.store.get(params.session_id)?;
        let mut original_input = match self
            .intercept_user_input(params.session_id, params.input)
            .await?
        {
            UserInputInterception::Accepted {
                input,
                original_input,
            } => {
                params.input = input;
                original_input
            }
            UserInputInterception::Handled { reason } => {
                return Err(EngineError::InputHandled(reason));
            }
        };
        let from_model = session
            .log
            .events()
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                Event::RunStarted { selection, .. } => Some(selection.model.clone()),
                _ => None,
            });
        if from_model.as_ref() != Some(&params.selection.model) {
            let context_id = crate::plugin::plugin_context_id();
            for plugin in self.inner.plugins.interception_plugins(
                cookie_agent_protocol::ExtensionInterceptionHook::ModelBeforeSelect,
            ) {
                let result = self
                    .inner
                    .plugins
                    .intercept_named::<_, cookie_agent_protocol::ExtensionAllowBlockResult>(
                        &plugin,
                        cookie_agent_protocol::PLUGIN_INTERCEPT_MODEL_BEFORE_SELECT_METHOD,
                        &cookie_agent_protocol::ExtensionModelBeforeSelectParams {
                            session_id: params.session_id,
                            context_id: context_id.clone(),
                            from: from_model.clone(),
                            to: params.selection.model.clone(),
                            source: if from_model.is_some() {
                                cookie_agent_protocol::ExtensionModelSelectSource::User
                            } else {
                                cookie_agent_protocol::ExtensionModelSelectSource::Config
                            },
                        },
                        Some(params.session_id),
                        Some(&context_id),
                    )
                    .await;
                match result {
                    Ok(result)
                        if result.action
                            == cookie_agent_protocol::ExtensionAllowBlockAction::Block =>
                    {
                        let reason = result
                            .reason
                            .unwrap_or_else(|| format!("model selection blocked by {plugin}"));
                        self.record_plugin_diagnostic(
                            params.session_id,
                            plugin,
                            PluginDiagnosticKind::HookBlocked,
                            reason.clone(),
                        );
                        return Err(EngineError::ModelSelectionBlocked(reason));
                    }
                    Ok(_) => {}
                    Err(error) => self.record_interception_error(params.session_id, plugin, error),
                }
            }
        }
        let staged_skill = self.pending_child_skill(params.session_id);
        let direct_skill = (staged_skill.is_none())
            .then(|| cookie_agent_protocol::decode_skill_submission(&params.input))
            .flatten()
            .map(|(name, args, prompt)| {
                params.input = prompt
                    .unwrap_or_else(|| "Apply the loaded skill to the current session.".into());
                (name, args)
            });
        original_input = original_input.filter(|original| original != &params.input);
        let direct_plan = direct_skill
            .as_ref()
            .map(|(name, args)| self.prepare_user_skill_invocation(name, args))
            .transpose()?;
        let prospective_plan = staged_skill.as_ref().or(direct_plan.as_ref());
        let prospective_grants = prospective_plan.and_then(|plan| plan.grants());
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
        let mut run_policy = match &session.meta.origin {
            SessionOrigin::Root => {
                self.reconcile_provider_store()?;
                let runtime = self.current_runtime();
                let agents = runtime.agents_for_preset(params.selection.preset.as_deref())?;
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
                    self.inner.config.runtime.prompt_caching.strategy(),
                )?
            }
            SessionOrigin::Delegated { .. } => {
                let runtime = self.current_runtime();
                if params.selection.preset != session.meta.creation_selection.preset {
                    return Err(EngineError::NoRunnableModel);
                }
                let agents = runtime.agents_for_preset(params.selection.preset.as_deref())?;
                policy_for_session_selection(
                    session.creation_agent.clone(),
                    agents,
                    runtime,
                    &params.selection,
                    result_limits.tool_output_max_lines,
                    result_limits.tool_output_max_bytes,
                    self.inner.config.runtime.prompt_caching.strategy(),
                )?
            }
        };
        self.compose_skill_listing(
            params.session_id,
            &mut run_policy,
            prospective_grants.as_ref(),
        )?;
        let context_id = crate::plugin::plugin_context_id();
        let mut injected_messages = Vec::new();
        for plugin in self.inner.plugins.interception_plugins(
            cookie_agent_protocol::ExtensionInterceptionHook::AgentBeforeStart,
        ) {
            let result = self
                .inner
                .plugins
                .intercept_named::<_, cookie_agent_protocol::ExtensionAgentBeforeStartResult>(
                    &plugin,
                    cookie_agent_protocol::PLUGIN_INTERCEPT_AGENT_BEFORE_START_METHOD,
                    &ExtensionAgentBeforeStartParams {
                        session_id: params.session_id,
                        context_id: context_id.clone(),
                        agent_path: run_policy.agent.agent.to_string(),
                        prompt_context: serde_json::json!({
                            "session_id": params.session_id,
                            "input": params.input,
                            "system_prompt": run_policy.agent.composed_prompt,
                            "injected_messages": injected_messages,
                        }),
                    },
                    Some(params.session_id),
                    Some(&context_id),
                )
                .await;
            match result {
                Ok(result) => {
                    if let Some(replacement) = result
                        .replace_system_prompt
                        .filter(|value| !value.is_empty())
                    {
                        if replacement.len() > 128 * 1024 {
                            self.record_plugin_diagnostic(
                                params.session_id,
                                plugin.clone(),
                                PluginDiagnosticKind::InvalidModification,
                                "plugin replacement system prompt exceeds the byte limit".into(),
                            );
                        } else {
                            run_policy.agent.composed_prompt = replacement;
                            run_policy.agent.prompt_fingerprint =
                                Sha256Digest::of_bytes(run_policy.agent.composed_prompt.as_bytes());
                        }
                    }
                    if let Some(addendum) = result
                        .append_to_system_prompt
                        .or(result.addendum)
                        .filter(|value| !value.is_empty())
                    {
                        if run_policy.agent.composed_prompt.len() + 1 + addendum.len() > 128 * 1024
                        {
                            self.record_plugin_diagnostic(
                                params.session_id,
                                plugin.clone(),
                                PluginDiagnosticKind::InvalidModification,
                                "plugin agent addendum exceeds the system prompt byte limit".into(),
                            );
                        } else {
                            run_policy.agent.composed_prompt.push('\n');
                            run_policy.agent.composed_prompt.push_str(&addendum);
                            run_policy.agent.prompt_fingerprint =
                                Sha256Digest::of_bytes(run_policy.agent.composed_prompt.as_bytes());
                        }
                    }
                    if let Some(message) = result.inject_message {
                        if message.content.trim().is_empty()
                            || message.role == cookie_agent_protocol::ExtensionMessageRole::Tool
                        {
                            self.record_plugin_diagnostic(
                                params.session_id,
                                plugin,
                                PluginDiagnosticKind::InvalidModification,
                                "plugin injected an empty or unsupported tool-role message".into(),
                            );
                        } else {
                            injected_messages.push(message);
                        }
                    }
                }
                Err(error) => {
                    let kind = if error.contains("crashed") || error.contains("not connected") {
                        PluginDiagnosticKind::InterceptionCrash
                    } else {
                        PluginDiagnosticKind::InterceptionTimeout
                    };
                    self.record_plugin_diagnostic(params.session_id, plugin, kind, error);
                }
            }
        }
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
                manifest_revision: run_policy.selected_suffix[0].manifest_revision.clone(),
                selected_suffix: run_policy.selected_suffix.clone(),
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
            fallback_index: AtomicU64::new(0),
        });
        self.inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id, active.clone());
        for message in injected_messages {
            if let Err(error) = self
                .append(
                    params.session_id,
                    Some(run_id),
                    Event::MessageInjected {
                        role: message.role,
                        input: message.content,
                    },
                )
                .await
            {
                return Err(self
                    .terminalize_run_setup_failure(&active, run_id, error)
                    .await);
            }
        }
        let serialized_message_bytes = match serialized_input_bytes(&params.input) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err(self
                    .terminalize_run_setup_failure(&active, run_id, error)
                    .await);
            }
        };
        let compacted = self
            .maybe_predictive_compact_before_input(PredictiveCompactionInput {
                session: params.session_id,
                run: run_id,
                serialized_message_bytes,
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
            return Err(self
                .terminalize_run_setup_failure(&active, run_id, error)
                .await);
        }
        let setup_result = async {
            if let Some(original_input) = original_input {
                self.append(
                    params.session_id,
                    Some(run_id),
                    Event::UserInputTransformed {
                        original_input,
                        input: params.input.clone(),
                    },
                )
                .await?;
            }
            self.append(
                params.session_id,
                Some(run_id),
                Event::UserInputSubmitted {
                    input: params.input,
                },
            )
            .await
        }
        .await;
        if let Err(error) = setup_result {
            return Err(self
                .terminalize_run_setup_failure(&active, run_id, error)
                .await);
        }
        self.clear_skill_turn_state(params.session_id);
        if staged_skill.is_some()
            && let Some(plan) = self.take_pending_child_skill(params.session_id)
            && let Err(error) = self
                .install_prepared_skill(params.session_id, Some(run_id), &plan)
                .await
        {
            return Err(self
                .terminalize_run_setup_failure(&active, run_id, error)
                .await);
        }
        // A sweeper may have terminalized this durable run before active-run
        // registration. Never resurrect a cancelled run with a live loop.
        if self.run_cancelled_recorded(params.session_id, run_id)? {
            self.inner
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&run_id);
            return Ok(RunStartResult { run_id });
        }
        if let Some((invocation_id, generation)) = admission
            && let Err(error) =
                self.publish_admission_run(invocation_id, generation, params.session_id, run_id)
        {
            self.cancel_run_durably(run_id, Some("delegate admission publication failed".into()))?;
            self.inner
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&run_id);
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
            if let Some((name, args)) = direct_skill
                && let Err(error) = engine
                    .execute_direct_skill(active.clone(), run_id, name, args)
                    .await
            {
                if matches!(error, EngineError::Permission(_)) {
                    tokio::select! {
                        () = active.cancellation.cancelled() => {
                            let _ = engine.append_run_cancelled_once(&active, run_id, None);
                        }
                        () = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                            let _ = engine
                                .append(
                                    active.session,
                                    Some(run_id),
                                    Event::RunFailed {
                                        error: safe_error(&error.to_string()),
                                    },
                                )
                                .await;
                        }
                    }
                } else if active.cancellation.is_cancelled() {
                    let _ = engine.append_run_cancelled_once(&active, run_id, None);
                } else {
                    let _ = engine
                        .append(
                            active.session,
                            Some(run_id),
                            Event::RunFailed {
                                error: safe_error(&error.to_string()),
                            },
                        )
                        .await;
                }
                if let Ok(mut active_runs) = engine.inner.active.lock() {
                    active_runs.remove(&run_id);
                }
                return;
            }
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
    ) -> Result<bool, EngineError> {
        let Some(binding) = input.policy.selected_suffix.get(input.fallback_index) else {
            return Ok(false);
        };
        let Some(context_limit) = binding.descriptor.capabilities.limits.context else {
            return Ok(false);
        };
        let config = &self.inner.config.runtime.context_compaction;
        let trigger_tokens = resolve_compaction_trigger(context_limit, &config.trigger);
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
            || !should_run_predictive_compaction(
                estimator,
                input.serialized_message_bytes,
                trigger_tokens,
                self.inner.store.is_persisted(input.session)?,
            )
        {
            return Ok(false);
        }
        let events = self.inner.store.get(input.session)?.log.events();
        let before = latest_checkpoint_seq(&events);
        let tools = self.tool_definitions(input.session, input.policy)?;
        let internal_policy = self.internal_agent_policy(
            InternalAgentKind::ContextCompaction,
            input.policy,
            Some(binding),
        )?;
        let compacted = match self
            .maybe_compact_context(CompactionInput {
                session: input.session,
                run: input.run,
                cancellation: input.cancellation,
                binding,
                owner_policy: input.policy,
                internal_policy: &internal_policy,
                tools: &tools,
                events,
                force: true,
                overflow_recovery: false,
                focus: None,
                actor_direct: input.actor_direct,
            })
            .await
        {
            Ok(compacted) => compacted,
            Err(EngineError::CompactionCancelled(_)) => return Ok(false),
            Err(error) => return Err(error),
        };
        Ok(latest_checkpoint_seq(&compacted) > before)
    }

    pub(super) async fn intercept_user_input(
        &self,
        session_id: SessionId,
        input: String,
    ) -> Result<UserInputInterception, EngineError> {
        let session = self.inner.store.get(session_id)?;
        if !matches!(session.meta.origin, SessionOrigin::Root) {
            return Ok(UserInputInterception::Accepted {
                input,
                original_input: None,
            });
        }
        self.inner.plugins.await_eager_ready().await;
        let original_input = input.clone();
        let mut current = input;
        let context_id = crate::plugin::plugin_context_id();
        for plugin in self
            .inner
            .plugins
            .interception_plugins(cookie_agent_protocol::ExtensionInterceptionHook::UserBeforeInput)
        {
            let result = self
                .inner
                .plugins
                .intercept_named::<_, cookie_agent_protocol::ExtensionUserBeforeInputResult>(
                    &plugin,
                    cookie_agent_protocol::PLUGIN_INTERCEPT_USER_BEFORE_INPUT_METHOD,
                    &ExtensionUserBeforeInputParams {
                        session_id,
                        context_id: context_id.clone(),
                        text: current.clone(),
                    },
                    Some(session_id),
                    Some(&context_id),
                )
                .await;
            match result {
                Ok(result) if result.action == ExtensionUserBeforeInputAction::Transform => {
                    match result.new_text.filter(|text| !text.trim().is_empty()) {
                        Some(text) => current = text,
                        None => self.record_plugin_diagnostic(
                            session_id,
                            plugin,
                            PluginDiagnosticKind::InvalidModification,
                            "plugin transformed user input to empty text".into(),
                        ),
                    }
                }
                Ok(result) if result.action == ExtensionUserBeforeInputAction::Handled => {
                    return Ok(UserInputInterception::Handled {
                        reason: result
                            .reason
                            .unwrap_or_else(|| format!("handled by {plugin}")),
                    });
                }
                Ok(_) => {}
                Err(error) => self.record_interception_error(session_id, plugin, error),
            }
        }
        let audit_original = (current != original_input).then_some(original_input);
        Ok(UserInputInterception::Accepted {
            input: current,
            original_input: audit_original,
        })
    }

    async fn terminalize_run_setup_failure(
        &self,
        active: &Arc<ActiveRun>,
        run_id: RunId,
        setup_error: EngineError,
    ) -> EngineError {
        match self
            .append(
                active.session,
                Some(run_id),
                Event::RunFailed {
                    error: safe_error(&setup_error.to_string()),
                },
            )
            .await
        {
            Ok(()) => {
                self.inner
                    .active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&run_id);
                setup_error
            }
            Err(terminal_error) => terminal_error,
        }
    }

    #[cfg(test)]
    pub(crate) fn has_active_run_for_test(&self, run_id: RunId) -> bool {
        self.inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&run_id)
    }

    #[cfg(test)]
    pub(crate) async fn retry_run_setup_terminalization_for_test(
        &self,
        run_id: RunId,
    ) -> Result<(), EngineError> {
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        self.append(
            active.session,
            Some(run_id),
            Event::RunFailed {
                error: safe_error("run setup terminalization retried"),
            },
        )
        .await?;
        self.inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&run_id);
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
            let override_model = self.take_skill_model_override(active.session);
            let mut turn_policy = None;
            if let Some(model) = override_model {
                let descriptor = active
                    .policy
                    .runtime
                    .result
                    .snapshot
                    .models
                    .iter()
                    .find(|descriptor| descriptor.key == model)
                    .ok_or(EngineError::NoRunnableModel)?;
                let selection = cookie_agent_protocol::ModelSelection {
                    model,
                    variant: descriptor.default_variant.clone(),
                };
                let binding = crate::model_snapshots::binding_for_selection(
                    &active.policy.runtime.current_manifest,
                    &active.policy.runtime.models,
                    &selection,
                )?;
                let mut policy = active.policy.as_ref().clone();
                policy.selected_suffix = vec![binding];
                turn_policy = Some(policy);
                fallback_entry = 0;
            }
            let policy = turn_policy.as_ref().unwrap_or(&active.policy);
            let published_tools = self.published_tool_definitions(active.session, policy)?;
            let prompt_events = self.prompt_events(active.session, run_id).await?;
            let attempt = match self
                .stream_attempt(
                    active.session,
                    run_id,
                    &active.cancellation,
                    policy,
                    &mut fallback_entry,
                    prompt_events,
                    published_tools.definitions.clone(),
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
                let promoted = self
                    .request(active.session, |reply| {
                        SessionCommand::PromotePendingOrComplete {
                            run: run_id,
                            final_text: (!final_text.is_empty()).then_some(final_text.clone()),
                            complete_if_empty: true,
                            reply,
                        }
                    })
                    .await?;
                if promoted {
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
                    .prepare_published_tool_call(
                        active.session,
                        run_id,
                        call,
                        &active.policy,
                        Arc::clone(&attempt.turn_context),
                        published_tools.tools.get(tool.as_str()),
                    )
                    .await;
                let operation_fingerprint = prepared_call.prepared.as_ref().map_or_else(
                    |_| {
                        fallback_operation_fingerprint(
                            &prepared_call.call,
                            prepared_call.permission_name.as_deref(),
                        )
                    },
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
            for (call, task) in calls.iter().zip(tasks) {
                let id = call.0;
                let (mut result, arguments) = if active.cancellation.is_cancelled() {
                    (
                        Err(ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message: "tool call cancelled after it started".into(),
                        }),
                        call.5.clone(),
                    )
                } else {
                    match task {
                        PendingTool::Prepared(prepared) => {
                            let intercepted_arguments = prepared.intercepted_arguments.clone();
                            let result = self
                                .execute_tool(
                                    active.clone(),
                                    run_id,
                                    *prepared,
                                    Arc::clone(&attempt.turn_context),
                                )
                                .await;
                            let arguments = intercepted_arguments
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .clone();
                            (result, arguments)
                        }
                        PendingTool::ImmediateFailure(failure) => (Err(failure), call.5.clone()),
                    }
                };
                let (mut result_content, is_error) = match &result {
                    Ok(result) => (result.output.clone(), false),
                    Err(failure) => (failure.message.clone(), true),
                };
                let context_id = crate::plugin::plugin_context_id();
                for plugin in self.inner.plugins.interception_plugins(
                    cookie_agent_protocol::ExtensionInterceptionHook::ToolAfterResult,
                ) {
                    let intercepted = self
                        .inner
                        .plugins
                        .intercept_named::<_, cookie_agent_protocol::ExtensionToolAfterResultResult>(
                            &plugin,
                            cookie_agent_protocol::PLUGIN_INTERCEPT_TOOL_AFTER_RESULT_METHOD,
                            &ExtensionToolAfterResultParams {
                                session_id: active.session,
                                context_id: context_id.clone(),
                                tool: call.4.to_string(),
                                arguments: arguments.clone(),
                                result_content: result_content.clone(),
                                is_error,
                            },
                            Some(active.session),
                            Some(&context_id),
                        )
                        .await;
                    match intercepted {
                        Ok(intercepted)
                            if intercepted.action == ExtensionToolAfterResultAction::Replace =>
                        {
                            if let Some(replacement) = intercepted.replacement_content {
                                if replacement.len()
                                    > active.policy.result_limits.tool_output_max_bytes
                                {
                                    self.record_plugin_diagnostic(
                                        active.session,
                                        plugin,
                                        PluginDiagnosticKind::InvalidModification,
                                        "plugin replacement exceeds the tool output byte limit"
                                            .into(),
                                    );
                                    continue;
                                }
                                result_content.clone_from(&replacement);
                                match &mut result {
                                    Ok(result) => result.output = replacement,
                                    Err(failure) => failure.message = replacement,
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let kind =
                                if error.contains("crashed") || error.contains("not connected") {
                                    PluginDiagnosticKind::InterceptionCrash
                                } else {
                                    PluginDiagnosticKind::InterceptionTimeout
                                };
                            self.record_plugin_diagnostic(active.session, plugin, kind, error);
                        }
                    }
                }
                self.submit_tool_result_status(active.session, run_id, id, result)
                    .await?;
            }
            if active.cancellation.is_cancelled() {
                self.append_run_cancelled_once(&active, run_id, None)?;
                return Ok(());
            }
            self.request(active.session, |reply| {
                SessionCommand::PromotePendingOrComplete {
                    run: run_id,
                    final_text: None,
                    complete_if_empty: false,
                    reply,
                }
            })
            .await?;
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
                let uncompacted_events = request_events.clone();
                let request_events = match self
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
                        overflow_recovery: false,
                        focus: None,
                        actor_direct: false,
                    })
                    .await
                {
                    Ok(events) => events,
                    Err(EngineError::CompactionCancelled(_)) => uncompacted_events,
                    Err(error) => return Err(error),
                };
                let input_through_seq = request_events.last().map_or(0, |event| event.seq);
                let context = assemble_model_context(
                    &request_events,
                    &self.inner.artifacts,
                    binding,
                    composed_prompt,
                )?;
                let replay_preflight = context.replay_decisions;
                let turn_context = Arc::new(TurnAgentContext {
                    agent: policy.agent.agent.clone(),
                    capabilities: policy
                        .model_capabilities(binding)
                        .ok_or(EngineError::NoRunnableModel)?,
                });
                let mut request = ModelRequest::new(context.history).with_tools(tools.clone());
                request.inference.max_output_tokens = match (
                    binding.descriptor.capabilities.limits.output,
                    policy.agent.max_output_tokens,
                ) {
                    (Some(model), 0) => Some(model),
                    (Some(model), document) => Some(model.min(document)),
                    (None, 0) => None,
                    (None, document) => Some(document),
                };
                if let Some(native_context) = context.native_context {
                    request = request.with_native_context(native_context);
                }
                let mut request = model.prepare_request_before_cache_strategy(
                    request,
                    policy.prompt_cache_strategy.as_ref(),
                );
                let context_id = crate::plugin::plugin_context_id();
                for plugin in self.inner.plugins.interception_plugins(
                    cookie_agent_protocol::ExtensionInterceptionHook::ModelBeforeRequest,
                ) {
                    let messages =
                        extension_messages(&request.history).map_err(EngineError::Model)?;
                    let result = self.inner.plugins.intercept_named::<_, cookie_agent_protocol::ExtensionModelBeforeRequestResult>(
                        &plugin,
                        cookie_agent_protocol::PLUGIN_INTERCEPT_MODEL_BEFORE_REQUEST_METHOD,
                        &cookie_agent_protocol::ExtensionModelBeforeRequestParams {
                            session_id: session,
                            context_id: context_id.clone(),
                            attempt_id,
                            messages,
                            model: wire_model(binding),
                            params: extension_model_params(&request),
                        },
                        Some(session),
                        Some(&context_id),
                    ).await;
                    match result {
                        Ok(result) => {
                            let candidate = (|| {
                                let mut candidate = request.clone();
                                match result.action {
                                    cookie_agent_protocol::ExtensionModelBeforeRequestAction::Keep => {}
                                    cookie_agent_protocol::ExtensionModelBeforeRequestAction::Replace => {
                                        let messages = result.messages.ok_or_else(|| {
                                            Box::new(ModelError::invalid_request(
                                                "replace requires messages",
                                            ))
                                        })?;
                                        candidate.history =
                                            history_from_extension_messages(messages)?;
                                    }
                                }
                                if let Some(adjustments) = result.params_adjustments {
                                    apply_model_params(&mut candidate, adjustments);
                                }
                                model
                                    .model()
                                    .validate_request(&candidate)
                                    .map_err(Box::new)?;
                                Ok::<_, Box<ModelError>>(candidate)
                            })();
                            match candidate {
                                Ok(candidate) => request = candidate,
                                Err(error) => self.record_plugin_diagnostic(
                                    session,
                                    plugin,
                                    PluginDiagnosticKind::InvalidModification,
                                    format!("invalid model request interception result: {error}"),
                                ),
                            }
                        }
                        Err(error) => self.record_interception_error(session, plugin, error),
                    }
                }
                request = model
                    .apply_prompt_cache_strategy(request, policy.prompt_cache_strategy.as_ref());
                for plugin in self.inner.plugins.interception_plugins(
                    cookie_agent_protocol::ExtensionInterceptionHook::ProviderBeforeHeaders,
                ) {
                    let result = self.inner.plugins.intercept_named::<_, cookie_agent_protocol::ExtensionProviderBeforeHeadersResult>(
                        &plugin,
                        cookie_agent_protocol::PLUGIN_INTERCEPT_PROVIDER_BEFORE_HEADERS_METHOD,
                        &cookie_agent_protocol::ExtensionProviderBeforeHeadersParams {
                            session_id: session,
                            context_id: context_id.clone(),
                            attempt_id,
                            headers: std::collections::BTreeMap::new(),
                        },
                        Some(session), Some(&context_id),
                    ).await;
                    match result {
                        Ok(result) if !result.set.is_empty() || !result.delete.is_empty() => self
                            .record_plugin_diagnostic(
                                session,
                                plugin,
                                PluginDiagnosticKind::UnsupportedCapability,
                                "provider header mutation is unavailable with pinned Oven adapters"
                                    .into(),
                            ),
                        Ok(_) => {}
                        Err(error) => self.record_interception_error(session, plugin, error),
                    }
                }
                for plugin in self.inner.plugins.interception_plugins(
                    cookie_agent_protocol::ExtensionInterceptionHook::ProviderBeforeRequest,
                ) {
                    let payload = model
                        .provider_request_payload(&request)
                        .map_err(EngineError::Model)?;
                    let result = self.inner.plugins.intercept_named::<_, cookie_agent_protocol::ExtensionProviderBeforeRequestResult>(
                        &plugin,
                        cookie_agent_protocol::PLUGIN_INTERCEPT_PROVIDER_BEFORE_REQUEST_METHOD,
                        &cookie_agent_protocol::ExtensionProviderBeforeRequestParams {
                            session_id: session,
                            context_id: context_id.clone(),
                            attempt_id,
                            payload,
                        },
                        Some(session), Some(&context_id),
                    ).await;
                    match result {
                        Ok(result) if result.action == cookie_agent_protocol::ExtensionProviderBeforeRequestAction::Replace => {
                            let candidate = result
                                .payload
                                .ok_or_else(|| {
                                    Box::new(ModelError::invalid_request("replace requires payload"))
                                })
                                .and_then(|payload| model.request_from_provider_payload(payload));
                            match candidate {
                                Ok(candidate) => request = candidate,
                                Err(error) => self.record_plugin_diagnostic(
                                    session,
                                    plugin,
                                    PluginDiagnosticKind::InvalidModification,
                                    error.to_string(),
                                ),
                            }
                        }
                        Ok(_) => {}
                        Err(error) => self.record_interception_error(session, plugin, error),
                    }
                }
                let authoritative_prompt = serde_json::to_vec(&request)
                    .map_err(|error| ModelError::invalid_request(error.to_string()))?;
                self.append(
                    session,
                    Some(run),
                    Event::ModelRequestPrepared {
                        attempt_id,
                        prompt_fingerprint: Sha256Digest::of_bytes(&authoritative_prompt),
                    },
                )
                .await?;
                let serialized_context_bytes =
                    serde_json::to_vec(&(&request.history, &request.tools))
                        .map_err(|error| ModelError::invalid_request(error.to_string()))?
                        .len();
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
                        if let Some(status) = response.http_status {
                            for plugin in self.inner.plugins.interception_plugins(
                                cookie_agent_protocol::ExtensionInterceptionHook::ProviderAfterResponse,
                            ) {
                                let result = self.inner.plugins.intercept_named::<_, cookie_agent_protocol::ExtensionProviderAfterResponseResult>(
                                    &plugin,
                                    cookie_agent_protocol::PLUGIN_INTERCEPT_PROVIDER_AFTER_RESPONSE_METHOD,
                                    &cookie_agent_protocol::ExtensionProviderAfterResponseParams {
                                        session_id: session,
                                        context_id: context_id.clone(),
                                        attempt_id,
                                        status,
                                        headers: std::collections::BTreeMap::new(),
                                    },
                                    Some(session), Some(&context_id),
                                ).await;
                                if let Err(error) = result {
                                    self.record_interception_error(session, plugin, error);
                                }
                            }
                        }
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
                        let (mut turn, warnings) =
                            persist_turn(turn, &self.inner.artifacts, binding)?;
                        for plugin in self.inner.plugins.interception_plugins(
                            cookie_agent_protocol::ExtensionInterceptionHook::MessageEnd,
                        ) {
                            let result = self.inner.plugins.intercept_named::<_, cookie_agent_protocol::ExtensionMessageEndResult>(
                                &plugin,
                                cookie_agent_protocol::PLUGIN_INTERCEPT_MESSAGE_END_METHOD,
                                &cookie_agent_protocol::ExtensionMessageEndParams {
                                    session_id: session,
                                    context_id: context_id.clone(),
                                    attempt_id,
                                    role: cookie_agent_protocol::ExtensionMessageRole::Assistant,
                                    content: turn.content.clone(),
                                },
                                Some(session), Some(&context_id),
                            ).await;
                            match result {
                                Ok(result) if result.action == cookie_agent_protocol::ExtensionMessageEndAction::Replace => {
                                    if let Some(content) = result.content {
                                        let mut candidate = turn.clone();
                                        candidate.content = content;
                                        match candidate.validate_for(&wire_model(binding)) {
                                            Ok(()) => turn = candidate,
                                            Err(error) => self.record_plugin_diagnostic(session, plugin, PluginDiagnosticKind::InvalidModification, format!("invalid message replacement: {error}")),
                                        }
                                    }
                                }
                                Ok(_) => {}
                                Err(error) => self.record_interception_error(session, plugin, error),
                            }
                        }
                        let resolved_model = wire_model(binding);
                        let estimated_cost_pico_usd = crate::usage::estimated_cost_pico_usd(
                            &resolved_model,
                            &turn.usage,
                            &self.inner.config.runtime.pricing,
                            &self.catalog_pricing(),
                        );
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
                        self.append(
                            session,
                            Some(run),
                            Event::ModelUsageRecorded {
                                model_turn_seq,
                                agent_id: policy.agent.agent.clone(),
                                resolved_model: wire_model(binding),
                                usage: turn.usage.clone(),
                                estimated_cost_pico_usd,
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
                                serialized_context_bytes,
                                turn.usage.input_tokens,
                            );
                        if !matches!(
                            self.inner.store.get(session)?.meta.origin,
                            cookie_agent_protocol::SessionOrigin::Delegated { .. }
                        ) {
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
                        }
                        return Ok(AttemptTurn {
                            turn,
                            model_turn_seq,
                            turn_context,
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
                        let unrecovered_events = recovery_events.clone();
                        let recovered = match self
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
                                overflow_recovery: true,
                                focus: None,
                                actor_direct: false,
                            })
                            .await
                        {
                            Ok(events) => events,
                            Err(EngineError::CompactionCancelled(_)) => unrecovered_events,
                            Err(error) => return Err(error),
                        };
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
            let context_id = crate::plugin::plugin_context_id();
            let mut selection_blocked = false;
            for plugin in self.inner.plugins.interception_plugins(
                cookie_agent_protocol::ExtensionInterceptionHook::ModelBeforeSelect,
            ) {
                let result = self
                    .inner
                    .plugins
                    .intercept_named::<_, cookie_agent_protocol::ExtensionAllowBlockResult>(
                        &plugin,
                        cookie_agent_protocol::PLUGIN_INTERCEPT_MODEL_BEFORE_SELECT_METHOD,
                        &cookie_agent_protocol::ExtensionModelBeforeSelectParams {
                            session_id: session,
                            context_id: context_id.clone(),
                            from: Some(binding.selection.clone()),
                            to: next.selection.clone(),
                            source:
                                cookie_agent_protocol::ExtensionModelSelectSource::FallbackRestore,
                        },
                        Some(session),
                        Some(&context_id),
                    )
                    .await;
                match result {
                    Ok(result)
                        if result.action
                            == cookie_agent_protocol::ExtensionAllowBlockAction::Block =>
                    {
                        let reason = result
                            .reason
                            .unwrap_or_else(|| format!("model fallback blocked by {plugin}"));
                        self.record_plugin_diagnostic(
                            session,
                            plugin,
                            PluginDiagnosticKind::HookBlocked,
                            reason,
                        );
                        selection_blocked = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => self.record_interception_error(session, plugin, error),
                }
            }
            if selection_blocked {
                return Err(last_error.into());
            }
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

    pub(super) fn record_interception_error(
        &self,
        session: SessionId,
        plugin: String,
        error: String,
    ) {
        let kind = if error.contains("crashed") || error.contains("not connected") {
            PluginDiagnosticKind::InterceptionCrash
        } else {
            PluginDiagnosticKind::InterceptionTimeout
        };
        self.record_plugin_diagnostic(session, plugin, kind, error);
    }
}

fn extension_messages(
    history: &[oven_sdk::HistoryTurn],
) -> Result<Vec<cookie_agent_protocol::ExtensionModelMessage>, Box<ModelError>> {
    history
        .iter()
        .map(|turn| {
            let (role, content) = match turn {
                oven_sdk::HistoryTurn::System(message) => (
                    cookie_agent_protocol::ExtensionMessageRole::System,
                    serde_json::to_value(message),
                ),
                oven_sdk::HistoryTurn::User(message) => (
                    cookie_agent_protocol::ExtensionMessageRole::User,
                    serde_json::to_value(message),
                ),
                oven_sdk::HistoryTurn::Assistant(message) => (
                    cookie_agent_protocol::ExtensionMessageRole::Assistant,
                    serde_json::to_value(message),
                ),
                oven_sdk::HistoryTurn::Tool(message) => (
                    cookie_agent_protocol::ExtensionMessageRole::Tool,
                    serde_json::to_value(message),
                ),
            };
            Ok(cookie_agent_protocol::ExtensionModelMessage {
                role,
                content: content
                    .map_err(|error| Box::new(ModelError::invalid_request(error.to_string())))?,
            })
        })
        .collect()
}

fn history_from_extension_messages(
    messages: Vec<cookie_agent_protocol::ExtensionModelMessage>,
) -> Result<Vec<oven_sdk::HistoryTurn>, Box<ModelError>> {
    if messages.is_empty() {
        return Err(Box::new(ModelError::invalid_request(
            "replacement message list must not be empty",
        )));
    }
    messages
        .into_iter()
        .map(|message| {
            let kind = match message.role {
                cookie_agent_protocol::ExtensionMessageRole::System => "system",
                cookie_agent_protocol::ExtensionMessageRole::User => "user",
                cookie_agent_protocol::ExtensionMessageRole::Assistant => "assistant",
                cookie_agent_protocol::ExtensionMessageRole::Tool => "tool",
            };
            serde_json::from_value(serde_json::json!({
                "type": kind,
                "value": message.content,
            }))
            .map_err(|error| Box::new(ModelError::invalid_request(error.to_string())))
        })
        .collect()
}

fn extension_model_params(
    request: &ModelRequest,
) -> cookie_agent_protocol::ExtensionModelParamsAdjustments {
    cookie_agent_protocol::ExtensionModelParamsAdjustments {
        max_tokens: request.inference.max_output_tokens,
        temperature: request.inference.temperature,
        top_p: request.inference.top_p,
    }
}

fn apply_model_params(
    request: &mut ModelRequest,
    adjustments: cookie_agent_protocol::ExtensionModelParamsAdjustments,
) {
    if let Some(max_tokens) = adjustments.max_tokens {
        request.inference.max_output_tokens = Some(max_tokens);
    }
    if let Some(temperature) = adjustments.temperature {
        request.inference.temperature = Some(temperature);
    }
    if let Some(top_p) = adjustments.top_p {
        request.inference.top_p = Some(top_p);
    }
}

pub(super) fn serialized_input_bytes(input: &str) -> Result<usize, EngineError> {
    serde_json::to_vec(input)
        .map(|bytes| bytes.len())
        .map_err(|error| ModelError::invalid_request(error.to_string()).into())
}
