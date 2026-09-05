use std::{
    collections::{HashMap, HashSet},
    io,
    sync::Arc,
};

use cookie_agent_config::ContextCompactionTrigger;
use cookie_agent_protocol::{
    ContextCheckpoint, ContextCheckpointBoundaries, ContextCheckpointBudgets,
    ContextCheckpointCommit, ExtensionSessionBeforeCompactParams, InternalAgentKind,
    InternalSummaryCheckpoint, PersistedToolResult, PluginDiagnosticKind, RunId,
    SessionCompactResult, SessionId, SessionStatus, StoredEvent, SummaryByteLimit,
    ToolEmittedContent,
};
use oven_sdk::{
    CompactionCapability, CompactionRequest, ModelError, Request as ModelRequest, ToolDefinition,
};
use oven_sdk_azure::{AzureOpenAiCompactionOptions, AzureOpenAiCompactionRequestExt as _};
use oven_sdk_openai::{OpenAiResponsesCompactionOptions, OpenAiResponsesCompactionRequestExt as _};
use tokio_util::sync::CancellationToken;

use super::titles::active_fallback_index;
use super::{
    Engine, EngineError, Event, FrozenInternalAgentPolicy, InternalAgentExecution,
    InternalAgentHistoryInput, SessionCommand,
    internal_agents::{internal_agent_input_fits, internal_agent_output_limit},
};
use crate::{
    model_bridge::AbortBridge,
    model_history::{self, assemble_model_context},
    policy::{self, FrozenRunPolicy},
};

pub(crate) const COMPACTION_INSTRUCTION: &str = "Create a detailed technical summary of the conversation so work can continue without the earlier context. Include: the goal/objective; decisions and their rationale; files changed and current code state; commands run and their outcomes; errors encountered and fixes applied; and the pending next step. Preserve exact identifiers, paths, constraints, and unresolved questions. Return summary text only and do not call tools.";
pub(super) const TOOL_OUTPUT_ELISION_MIN_BYTES: usize = 8 * 1024;
// Frozen policies normally provide this value. Keep substantial headroom when replaying an
// unavailable or legacy policy whose output limit is zero.
pub(super) const DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS: u64 = 20_000;
// Video is frame-sampled and can cost tens of thousands of tokens independent of file bytes.
const VIDEO_FILE_FIT_SURROGATE_BYTES: usize = 40_000 * 4;

pub(super) struct CompactionInput<'a> {
    pub(super) session: SessionId,
    pub(super) run: RunId,
    pub(super) cancellation: &'a CancellationToken,
    pub(super) binding: &'a cookie_agent_protocol::FrozenModelBinding,
    pub(super) owner_policy: &'a FrozenRunPolicy,
    pub(super) internal_policy: &'a FrozenInternalAgentPolicy,
    pub(super) tools: &'a [ToolDefinition],
    pub(super) events: Arc<[StoredEvent]>,
    pub(super) force: bool,
    pub(super) overflow_recovery: bool,
    pub(super) focus: Option<&'a str>,
    pub(super) actor_direct: bool,
    pub(super) origin: cookie_agent_protocol::EventOrigin,
}

impl Engine {
    pub async fn compact_session(
        &self,
        session: SessionId,
        focus: Option<&str>,
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<bool, EngineError> {
        self.compact_session_result(session, focus, origin)
            .await
            .map(|result| result.compacted)
    }

    pub async fn compact_session_result(
        &self,
        session: SessionId,
        focus: Option<&str>,
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<SessionCompactResult, EngineError> {
        let focus = focus.map(str::to_owned);
        self.request(session, |reply| SessionCommand::Compact {
            focus,
            origin,
            reply,
        })
        .await
    }

    pub(super) async fn compact_session_direct(
        &self,
        session: SessionId,
        focus: Option<&str>,
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<SessionCompactResult, EngineError> {
        let projection = self.inner.store.get(session)?;
        if projection.status == SessionStatus::Running {
            return Err(EngineError::SessionRunning(session));
        }
        let events = projection.log.event_snapshot();
        let run = projection
            .log
            .last_run_started()
            .map(|(_, run, _)| run)
            .ok_or(EngineError::NoRunnableModel)?;
        let policy = self.historical_title_policy(&events, run)?;
        let binding = active_compaction_binding(&policy, &events, run)?;
        let internal_policy = self.internal_agent_policy(
            InternalAgentKind::ContextCompaction,
            &policy,
            Some(binding),
        )?;
        let tools = self.tool_definitions(session, &policy)?;
        let before = projection.log.latest_checkpoint_seq();
        match self
            .maybe_compact_context(CompactionInput {
                session,
                run,
                cancellation: &CancellationToken::new(),
                binding,
                owner_policy: &policy,
                internal_policy: &internal_policy,
                tools: &tools,
                events,
                force: true,
                overflow_recovery: false,
                focus,
                actor_direct: false,
                origin,
            })
            .await
        {
            Ok(_) => {}
            Err(EngineError::CompactionCancelled(reason)) => {
                return Ok(SessionCompactResult {
                    compacted: false,
                    cancellation_reason: Some(reason),
                });
            }
            Err(error) => return Err(error),
        }
        Ok(SessionCompactResult {
            compacted: projection.log.latest_checkpoint_seq() > before,
            cancellation_reason: None,
        })
    }

    pub(super) async fn maybe_compact_context(
        &self,
        mut input: CompactionInput<'_>,
    ) -> Result<Arc<[StoredEvent]>, EngineError> {
        let Some(context_limit) = input.binding.descriptor.capabilities.limits.context else {
            return Ok(input.events);
        };
        let config = &self.inner.config.runtime.context_compaction;
        let trigger_tokens = resolve_compaction_trigger(context_limit, &config.trigger);
        if !compaction_gate(input.force, config.auto_compaction, trigger_tokens) {
            return Ok(input.events);
        }
        let projection = self.inner.store.get(input.session)?;
        if !input.force {
            let log = &projection.log;
            let Some((usage_seq, observed_tokens)) = log.latest_real_usage() else {
                return Ok(input.events);
            };
            let last_checkpoint_seq = log.latest_checkpoint_seq();
            if usage_seq < last_checkpoint_seq {
                return Ok(input.events);
            }
            if !usage_reaches_compaction_trigger(observed_tokens, trigger_tokens) {
                return Ok(input.events);
            }
        }

        let requested_input_through_seq = input.events.last().map_or(0, |event| event.seq);
        let current_events = projection.log.event_snapshot();
        if projection
            .log
            .checkpoint_covers_input(requested_input_through_seq)
        {
            return Ok(current_events);
        }

        let has_producer_input =
            crate::goal_projection::GoalProducerProjection::from_events(&input.events)
                .messages
                .iter()
                .any(|message| {
                    !message.consumed
                        && !message.discarded
                        && message.admission.is_some_and(|(run, _)| run == input.run)
                });
        let producer_claim = if !has_producer_input {
            None
        } else if input.actor_direct {
            Some(self.claim_producer_snapshot_direct(input.session, input.run)?)
        } else {
            Some(
                self.claim_existing_producer_inputs(input.session, input.run)
                    .await?,
            )
        };
        if let Some(claim) = &producer_claim {
            input.events = Arc::clone(&claim.events);
        }

        let mut compaction_focus = input.focus.map(str::to_owned);
        let mut additions = Vec::new();
        let context_id = crate::plugin::plugin_context_id();
        for plugin in self.inner.plugins.interception_plugins(
            cookie_agent_protocol::ExtensionInterceptionHook::SessionBeforeCompact,
        ) {
            let result = self
                .inner
                .plugins
                .intercept_named::<_, cookie_agent_protocol::ExtensionSessionBeforeCompactResult>(
                    &plugin,
                    cookie_agent_protocol::PLUGIN_INTERCEPT_SESSION_BEFORE_COMPACT_METHOD,
                    &ExtensionSessionBeforeCompactParams {
                        session_id: input.session,
                        context_id: context_id.clone(),
                        checkpoint_id: format!("{}:{requested_input_through_seq}", input.session),
                        additions: additions.clone(),
                        instructions: compaction_focus.clone(),
                    },
                    Some(input.session),
                    Some(&context_id),
                )
                .await;
            match result {
                Ok(result) => {
                    if result.cancel {
                        let reason = result
                            .reason
                            .unwrap_or_else(|| "compaction cancelled by plugin".into());
                        self.record_plugin_diagnostic(
                            input.session,
                            plugin,
                            PluginDiagnosticKind::HookBlocked,
                            reason.clone(),
                        );
                        return Err(EngineError::CompactionCancelled(reason));
                    }
                    if let Some(instructions) = result.instructions_override {
                        if instructions.len() > 64 * 1024 {
                            self.record_plugin_diagnostic(
                                input.session,
                                plugin.clone(),
                                PluginDiagnosticKind::InvalidModification,
                                "plugin compaction instruction override exceeds the 64 KiB limit"
                                    .into(),
                            );
                        } else {
                            compaction_focus = Some(instructions);
                        }
                    }
                    if let Some(addendum) = result.addendum.filter(|value| !value.is_empty()) {
                        let addition_bytes = additions.iter().map(String::len).sum::<usize>();
                        if addition_bytes.saturating_add(addendum.len()) > 64 * 1024 {
                            self.record_plugin_diagnostic(
                                input.session,
                                plugin,
                                PluginDiagnosticKind::InvalidModification,
                                "plugin compaction additions exceed the 64 KiB limit".into(),
                            );
                            continue;
                        }
                        let focus = compaction_focus.get_or_insert_with(String::new);
                        if !focus.is_empty() {
                            focus.push('\n');
                        }
                        focus.push_str(&addendum);
                        additions.push(addendum);
                    }
                }
                Err(error) => {
                    let kind = if error.contains("crashed") || error.contains("not connected") {
                        PluginDiagnosticKind::InterceptionCrash
                    } else {
                        PluginDiagnosticKind::InterceptionTimeout
                    };
                    self.record_plugin_diagnostic(input.session, plugin, kind, error);
                }
            }
        }

        let composed_prompt = self.run_agent_prompt(input.session, input.run)?;
        let mut events = input.events.to_vec();
        let mut context = assemble_model_context(
            &events,
            &self.inner.artifacts,
            input.binding,
            &composed_prompt,
        )?;
        let raw_fits = if let Some(raw_fits) = raw_fit_from_real_usage(
            input.overflow_recovery,
            projection
                .log
                .latest_real_usage()
                .map(|(_, observed_tokens)| observed_tokens),
            |tokens| compaction_input_fits(input.binding, input.internal_policy, tokens),
        ) {
            raw_fits
        } else {
            let raw_fit_tokens = if input.binding.descriptor.capabilities.compaction
                == CompactionCapability::Native
            {
                self.estimated_request_tokens(input.session, &context.history, input.tools)?
            } else {
                let (history, _) = compaction_history(
                    context.history.clone(),
                    compaction_focus.as_deref(),
                    &input.internal_policy.agent.composed_prompt,
                );
                self.estimated_request_tokens(input.session, &history, input.tools)?
            };
            compaction_input_fits(input.binding, input.internal_policy, raw_fit_tokens)
        };
        let context_tokens_before = if raw_fits {
            self.estimated_request_tokens(input.session, &context.history, input.tools)?
        } else {
            events = self
                .stage_tool_output_elision(
                    input.session,
                    events,
                    input.actor_direct,
                    input.origin.clone(),
                )
                .await?;
            context = assemble_model_context(
                &events,
                &self.inner.artifacts,
                input.binding,
                &composed_prompt,
            )?;
            self.estimated_request_tokens(input.session, &context.history, input.tools)?
        };
        if !input.force && context_tokens_before < trigger_tokens {
            return Ok(Arc::from(events));
        }

        let input_through_seq = events.last().map_or(0, |event| event.seq);
        let previous = projection.log.latest_checkpoint_seq();
        let mut source_from_seq = if previous == 0 {
            1
        } else {
            previous.saturating_add(1)
        };
        if let Some(recent_from) = events
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                Event::ContextCheckpointCommitted { commit } => {
                    Some(commit.boundaries.recent_from_seq)
                }
                _ => None,
            })
            .flatten()
        {
            source_from_seq = source_from_seq.min(recent_from);
        }
        let mut boundaries = ContextCheckpointBoundaries {
            source_from_seq,
            source_through_seq: input_through_seq,
            input_through_seq,
            prior_checkpoint_seq: (previous > 0).then_some(previous),
            recent_from_seq: None,
        };
        let summary_limit = SummaryByteLimit::new(config.max_summary_bytes as u64)
            .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?;
        let native_checkpoint = if input.binding.descriptor.capabilities.compaction
            == CompactionCapability::Native
        {
            if let Ok(model) = policy::resolve_model(input.binding, &input.owner_policy.runtime) {
                let mut request = ModelRequest::new(context.history.clone())
                    .with_tools(input.tools.to_vec())
                    .with_header_context(self.model_header_context(input.session)?);
                crate::media::validate_media_part_counts(
                    &request.history,
                    &input
                        .owner_policy
                        .model_capabilities(input.binding)
                        .ok_or(EngineError::NoRunnableModel)?,
                )
                .map_err(ModelError::invalid_request)?;
                if let Some(native_context) = context.native_context.clone() {
                    request = request.with_native_context(native_context);
                }
                let cache_strategy = input
                    .owner_policy
                    .cache_strategy(input.binding, input.session);
                let request =
                    model.prepare_request_with_cache_strategy(request, cache_strategy.as_ref());
                let mut compact_request = CompactionRequest::new(request);
                let instructions = compaction_focus.clone();
                compact_request = match input.binding.protocol_recipe.as_str() {
                    "oven.openai.responses" => compact_request
                        .with_openai_responses_compaction_options(
                            OpenAiResponsesCompactionOptions {
                                instructions,
                                ..OpenAiResponsesCompactionOptions::default()
                            },
                        ),
                    "oven.azure.openai.responses" => compact_request
                        .with_azure_openai_compaction_options(AzureOpenAiCompactionOptions {
                            instructions,
                            ..AzureOpenAiCompactionOptions::default()
                        }),
                    _ => compact_request,
                };
                if model.model().supports_compaction(&compact_request) {
                    let abort = AbortBridge::new(input.cancellation.child_token());
                    match model.model().compact(compact_request, abort.signal()).await {
                        Ok(result) => model_history::persist_native_context(
                            result.native_context,
                            input.binding,
                        )
                        .ok(),
                        Err(_) => None,
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        let checkpoint_prefix =
            model_history::checkpoint_retained_history(&context.history, &events, None);
        if let Some(window) = native_checkpoint {
            let input_tokens_after = estimated_request_tokens(&checkpoint_prefix, input.tools)?;
            let budgets = ContextCheckpointBudgets {
                context_limit_tokens: context_limit,
                trigger_tokens: trigger_tokens.max(1).min(context_limit),
                input_tokens_before: context_tokens_before,
                input_tokens_after,
                max_summary_bytes: summary_limit,
                keep_recent_tokens: 0,
            };
            let commit = ContextCheckpointCommit {
                checkpoint: ContextCheckpoint::NativeWindow { window },
                boundaries: boundaries.clone(),
                budgets,
            };
            if commit.validate_for_binding(input.binding).is_ok() {
                self.append_compaction_event(
                    input.session,
                    Some(input.run),
                    Event::ContextCheckpointCommitted { commit },
                    input.actor_direct,
                    input.origin.clone(),
                )
                .await?;
                return self.finalize_context_checkpoint(input.session, input_tokens_after);
            }
        }

        // Reserve an output-sized summary before selecting a suffix. The final fit check uses
        // the actual replay projection and the same calibrated estimator as the input.
        let output_reserve = match (
            input.binding.descriptor.capabilities.limits.output,
            input.owner_policy.agent.max_output_tokens,
        ) {
            (Some(model), agent) if agent > 0 => model.min(agent),
            (Some(model), _) => model,
            (None, agent) if agent > 0 => agent,
            _ => DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS,
        };
        let retained_limit = context_limit
            .saturating_sub(output_reserve)
            .min(if trigger_tokens > 0 {
                trigger_tokens - 1
            } else {
                context_limit
            })
            .min(context_tokens_before.saturating_sub(1));
        let summary_output_limit = input
            .internal_policy
            .models
            .iter()
            .filter_map(|binding| internal_agent_output_limit(binding, input.internal_policy))
            .max()
            .unwrap_or(DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS);
        let summary_reserve_bytes = summary_output_limit
            .saturating_mul(4)
            .min(config.max_summary_bytes as u64) as usize;
        let summary_reserve = "x".repeat(summary_reserve_bytes);
        let base = model_history::project_summary_context(
            &events,
            &self.inner.artifacts,
            input.binding,
            &composed_prompt,
            input_through_seq,
            None,
            &summary_reserve,
        )?;
        let base_tokens =
            self.estimated_request_tokens(input.session, &base.history, input.tools)?;
        let keep_recent_tokens = effective_recent_budget(
            config.keep_recent_tokens,
            context_limit,
            retained_limit.saturating_sub(base_tokens),
        );
        let recent_from_seq = select_recent_tail(
            model_history::compaction_tail_candidates(&events),
            keep_recent_tokens,
            base_tokens,
            retained_limit,
            |candidate| {
                let projected = model_history::project_summary_context(
                    &events,
                    &self.inner.artifacts,
                    input.binding,
                    &composed_prompt,
                    input_through_seq,
                    Some(candidate),
                    &summary_reserve,
                )?;
                self.estimated_request_tokens(input.session, &projected.history, input.tools)
            },
        )?;
        let prefix = model_history::compaction_prefix_history(
            &events,
            &self.inner.artifacts,
            input.binding,
            &composed_prompt,
            recent_from_seq,
        )?;
        let prior_summary_count = usize::from(
            events
                .iter()
                .rev()
                .find_map(|event| match &event.payload {
                    Event::ContextCheckpointCommitted { commit } => Some(matches!(
                        commit.checkpoint,
                        ContextCheckpoint::InternalSummary { .. }
                    )),
                    _ => None,
                })
                .unwrap_or(false),
        );
        if prefix.len() <= checkpoint_prefix.len().saturating_add(prior_summary_count) {
            return Ok(Arc::from(events));
        }
        let (history, instruction) = compaction_history(
            prefix,
            compaction_focus.as_deref(),
            &input.internal_policy.agent.composed_prompt,
        );
        let summary = self
            .run_internal_history_agent(
                input.session,
                Some(input.run),
                InternalAgentKind::ContextCompaction,
                input.internal_policy,
                InternalAgentHistoryInput {
                    history,
                    summary_source: instruction,
                    tools: input.tools.to_vec(),
                    reject_non_text: true,
                },
                InternalAgentExecution {
                    cancellation: input.cancellation,
                    actor_direct: input.actor_direct,
                },
            )
            .await;
        let Ok(summary) = summary else {
            // Deliberately leave history fully intact when the raw context fit. Failed
            // compaction no longer performs consolation elision on that path.
            return Ok(Arc::from(events));
        };
        if summary.text.trim().is_empty() {
            return Ok(Arc::from(events));
        }
        let checkpoint = InternalSummaryCheckpoint::new(
            summary.text,
            summary.invocation_id,
            summary.internal_run_id,
            summary_limit,
        )
        .map_err(|error| EngineError::from(ModelError::invalid_response(error.to_string())))?;
        let retained_context = model_history::project_summary_context(
            &events,
            &self.inner.artifacts,
            input.binding,
            &composed_prompt,
            input_through_seq,
            recent_from_seq,
            checkpoint.summary(),
        )?;
        let input_tokens_after =
            self.estimated_request_tokens(input.session, &retained_context.history, input.tools)?;
        let actual_base = model_history::project_summary_context(
            &events,
            &self.inner.artifacts,
            input.binding,
            &composed_prompt,
            input_through_seq,
            None,
            checkpoint.summary(),
        )?;
        let actual_base_tokens =
            self.estimated_request_tokens(input.session, &actual_base.history, input.tools)?;
        let keep_recent_tokens = effective_recent_budget(
            config.keep_recent_tokens,
            context_limit,
            retained_limit.saturating_sub(actual_base_tokens),
        );
        if input_tokens_after > retained_limit
            || input_tokens_after.saturating_sub(actual_base_tokens) > keep_recent_tokens
        {
            return Ok(Arc::from(events));
        }
        boundaries.recent_from_seq = recent_from_seq;
        if let Some(recent_from_seq) = recent_from_seq {
            boundaries.source_from_seq = boundaries.source_from_seq.min(recent_from_seq);
        }
        let budgets = ContextCheckpointBudgets {
            context_limit_tokens: context_limit,
            trigger_tokens: trigger_tokens.max(1).min(context_limit),
            input_tokens_before: context_tokens_before,
            input_tokens_after,
            max_summary_bytes: summary_limit,
            keep_recent_tokens,
        };
        let commit = ContextCheckpointCommit {
            checkpoint: ContextCheckpoint::InternalSummary { checkpoint },
            boundaries,
            budgets,
        };
        if commit.validate().is_err() {
            return Ok(Arc::from(events));
        }
        self.append_compaction_event(
            input.session,
            Some(input.run),
            Event::ContextCheckpointCommitted { commit },
            input.actor_direct,
            input.origin.clone(),
        )
        .await?;
        self.finalize_context_checkpoint(input.session, input_tokens_after)
    }

    fn finalize_context_checkpoint(
        &self,
        session: SessionId,
        input_tokens_after: u64,
    ) -> Result<Arc<[StoredEvent]>, EngineError> {
        self.inner
            .context_token_estimators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(session)
            .or_default()
            .record_compaction(input_tokens_after);
        Ok(self.inner.store.get(session)?.log.event_snapshot())
    }

    async fn stage_tool_output_elision(
        &self,
        session: SessionId,
        events: Vec<StoredEvent>,
        actor_direct: bool,
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<Vec<StoredEvent>, EngineError> {
        let protected_turns = events
            .iter()
            .rev()
            .filter_map(|event| match &event.payload {
                Event::ModelTurnCommitted { model_turn_seq, .. } => Some(*model_turn_seq),
                _ => None,
            })
            .take(2)
            .collect::<HashSet<_>>();
        let starts = events
            .iter()
            .filter_map(|event| match &event.payload {
                Event::ToolCallStarted { start } => {
                    Some((start.tool_call_id, start.owner.model_turn_seq))
                }
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let already_elided = events
            .iter()
            .filter_map(|event| match event.payload {
                Event::ToolOutputElided { tool_call_id, .. } => Some(tool_call_id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        for event in &events {
            let Event::ToolCallTerminated { termination } = &event.payload else {
                continue;
            };
            let Some(result) = &termination.result else {
                continue;
            };
            let Some(model_turn_seq) = starts.get(&termination.tool_call_id) else {
                continue;
            };
            if !should_elide_tool_output(
                *model_turn_seq,
                &protected_turns,
                already_elided.contains(&termination.tool_call_id),
                elidable_bytes(result),
            ) {
                continue;
            }
            let (retained, _) = self.inner.artifacts.retain(result.output.as_bytes())?;
            self.append_compaction_event(
                session,
                event.run_id,
                Event::ToolOutputElided {
                    tool_call_id: termination.tool_call_id,
                    original_bytes: result.output.len() as u64,
                    retained,
                },
                actor_direct,
                origin.clone(),
            )
            .await?;
        }
        Ok(self.inner.store.get(session)?.log.events())
    }

    fn estimated_request_tokens(
        &self,
        session: SessionId,
        history: &[oven_sdk::HistoryTurn],
        tools: &[ToolDefinition],
    ) -> Result<u64, EngineError> {
        let bytes = serialized_fit_request_bytes(history, tools)?;
        let calibrated = self
            .inner
            .context_token_estimators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session)
            .copied()
            .and_then(|estimator| estimator.estimated_context_tokens(bytes));
        if let Some(calibrated) = calibrated {
            return Ok(calibrated);
        }
        Ok(estimated_tokens_for_bytes(bytes))
    }

    async fn append_compaction_event(
        &self,
        session: SessionId,
        run: Option<RunId>,
        event: Event,
        actor_direct: bool,
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<(), EngineError> {
        if actor_direct {
            self.append_direct(session, run, origin, event)
        } else {
            self.append(session, run, origin, event).await
        }
    }
}

pub(crate) fn active_compaction_binding<'a>(
    policy: &'a FrozenRunPolicy,
    events: &[StoredEvent],
    run: RunId,
) -> Result<&'a cookie_agent_protocol::FrozenModelBinding, EngineError> {
    policy
        .selected_suffix
        .get(active_fallback_index(events, run))
        .ok_or(EngineError::NoRunnableModel)
}

pub(super) fn resolve_compaction_trigger(
    context_limit: u64,
    trigger: &ContextCompactionTrigger,
) -> u64 {
    match trigger {
        ContextCompactionTrigger::Percent { percent } => context_limit
            .saturating_mul(u64::from(*percent))
            .saturating_div(100),
        ContextCompactionTrigger::BufferTokens { buffer_tokens } => {
            context_limit.saturating_sub(*buffer_tokens)
        }
    }
}

fn compaction_gate(force: bool, auto: bool, trigger_tokens: u64) -> bool {
    force || (auto && trigger_tokens > 0)
}

fn usage_reaches_compaction_trigger(observed_tokens: u64, trigger_tokens: u64) -> bool {
    observed_tokens >= trigger_tokens
}

fn effective_recent_budget(target: u64, context_limit: u64, available: u64) -> u64 {
    target.min(context_limit / 4).min(available)
}

fn select_recent_tail(
    candidates: Vec<u64>,
    budget: u64,
    base_tokens: u64,
    retained_limit: u64,
    mut estimate: impl FnMut(u64) -> Result<u64, EngineError>,
) -> Result<Option<u64>, EngineError> {
    let mut selected = None;
    if budget == 0 {
        return Ok(selected);
    }
    for candidate in candidates.into_iter().rev() {
        let tokens = estimate(candidate)?;
        if tokens > retained_limit || tokens.saturating_sub(base_tokens) > budget {
            break;
        }
        selected = Some(candidate);
    }
    Ok(selected)
}

#[cfg(test)]
fn checkpoint_covers_input(events: &[StoredEvent], input_through_seq: u64) -> bool {
    events.iter().rev().any(|event| {
        matches!(
            &event.payload,
            Event::ContextCheckpointCommitted { commit }
                if commit.boundaries.input_through_seq >= input_through_seq
        )
    })
}

pub(crate) fn serialized_fit_request_bytes(
    history: &[oven_sdk::HistoryTurn],
    tools: &[ToolDefinition],
) -> Result<usize, EngineError> {
    let (history, attachment_surrogate_bytes) = fit_history(history);
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, &(history, tools))
        .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?;
    Ok(writer.bytes.saturating_add(attachment_surrogate_bytes))
}

fn fit_history(history: &[oven_sdk::HistoryTurn]) -> (Vec<oven_sdk::HistoryTurn>, usize) {
    // Media costs are flat-ish, usually hundreds to low-thousands of tokens per part, and do not
    // track serialized byte size. Exclude image/PDF/audio files; video alone gets a flat surrogate
    // because frame sampling can be materially expensive. Overestimation can trigger unnecessary
    // compaction and discard attachments, while real usage calibrates residual error.
    let mut history = history.to_vec();
    let mut part_bytes = 0_usize;
    for turn in &mut history {
        match turn {
            oven_sdk::HistoryTurn::System(message) => {
                message.content.retain(|part| match part {
                    oven_sdk::SystemPart::Text(text) => {
                        part_bytes =
                            part_bytes.saturating_add(fit_part_bytes(FitPart::Text(&text.text)));
                        false
                    }
                    oven_sdk::SystemPart::Custom(_) => true,
                });
            }
            oven_sdk::HistoryTurn::User(message) => {
                message.content.retain(|part| match part {
                    oven_sdk::InputPart::Text(text) => {
                        part_bytes =
                            part_bytes.saturating_add(fit_part_bytes(FitPart::Text(&text.text)));
                        false
                    }
                    oven_sdk::InputPart::File(file) => {
                        part_bytes = part_bytes
                            .saturating_add(fit_part_bytes(FitPart::File(&file.media_type)));
                        false
                    }
                    oven_sdk::InputPart::Custom(_) => true,
                });
            }
            oven_sdk::HistoryTurn::Assistant(turn) => {
                for part in &mut turn.message.content {
                    if let oven_sdk::AssistantPart::ToolResult(result) = part {
                        part_bytes = part_bytes
                            .saturating_add(remove_tool_content_parts(&mut result.content));
                    }
                }
                turn.message.content.retain(|part| match part {
                    oven_sdk::AssistantPart::Text(text) => {
                        part_bytes =
                            part_bytes.saturating_add(fit_part_bytes(FitPart::Text(&text.text)));
                        false
                    }
                    oven_sdk::AssistantPart::File(file) => {
                        part_bytes = part_bytes
                            .saturating_add(fit_part_bytes(FitPart::File(&file.media_type)));
                        false
                    }
                    _ => true,
                });
            }
            oven_sdk::HistoryTurn::Tool(message) => {
                for result in &mut message.results {
                    part_bytes =
                        part_bytes.saturating_add(remove_tool_content_parts(&mut result.content));
                }
            }
        }
    }
    (history, part_bytes)
}

fn remove_tool_content_parts(content: &mut oven_sdk::ToolContent) -> usize {
    match content {
        oven_sdk::ToolContent::Text(text) => {
            let bytes = fit_part_bytes(FitPart::Text(text));
            text.clear();
            bytes
        }
        oven_sdk::ToolContent::Mixed(values) => {
            let mut part_bytes = 0_usize;
            values.retain(|value| match value {
                oven_sdk::ContentValue::Text(text) => {
                    part_bytes = part_bytes.saturating_add(fit_part_bytes(FitPart::Text(text)));
                    false
                }
                oven_sdk::ContentValue::File(file) => {
                    part_bytes =
                        part_bytes.saturating_add(fit_part_bytes(FitPart::File(&file.media_type)));
                    false
                }
                oven_sdk::ContentValue::Json(_) => true,
            });
            part_bytes
        }
        oven_sdk::ToolContent::Json(_) | oven_sdk::ToolContent::Denied { .. } => 0,
    }
}

#[derive(Clone, Copy)]
enum FitPart<'a> {
    Text(&'a str),
    File(&'a str),
}

fn fit_part_bytes(part: FitPart<'_>) -> usize {
    match part {
        FitPart::Text(text) => text.len(),
        FitPart::File(media_type) if media_type.starts_with("video/") => {
            VIDEO_FILE_FIT_SURROGATE_BYTES
        }
        FitPart::File(_) => 0,
    }
}

fn elidable_bytes(result: &PersistedToolResult) -> usize {
    let emitted_bytes = result
        .additional_messages
        .iter()
        .map(|message| {
            let marker_bytes =
                usize::from(message.role == cookie_agent_protocol::ToolEmittedMessageRole::System)
                    .saturating_mul(fit_part_bytes(FitPart::Text(
                        model_history::TOOL_EMITTED_SYSTEM_USER_MARKER,
                    )));
            message.content.iter().fold(marker_bytes, |total, part| {
                total.saturating_add(match part {
                    ToolEmittedContent::Text(text) => fit_part_bytes(FitPart::Text(text)),
                    ToolEmittedContent::File(attachment) => {
                        fit_part_bytes(FitPart::File(attachment.mime_type.as_str()))
                    }
                })
            })
        })
        .fold(0_usize, usize::saturating_add);
    result.attachments.iter().fold(
        fit_part_bytes(FitPart::Text(&result.output)).saturating_add(emitted_bytes),
        |total, attachment| {
            total.saturating_add(fit_part_bytes(FitPart::File(attachment.mime_type.as_str())))
        },
    )
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl io::Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn estimated_request_tokens(
    history: &[oven_sdk::HistoryTurn],
    tools: &[ToolDefinition],
) -> Result<u64, EngineError> {
    Ok(estimated_tokens_for_bytes(serialized_fit_request_bytes(
        history, tools,
    )?))
}

fn estimated_tokens_for_bytes(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

fn raw_fit_from_real_usage(
    overflow_recovery: bool,
    observed_tokens: Option<u64>,
    fits: impl FnOnce(u64) -> bool,
) -> Option<bool> {
    if overflow_recovery {
        Some(false)
    } else {
        observed_tokens.filter(|tokens| fits(*tokens)).map(|_| true)
    }
}

fn compaction_input_fits(
    binding: &cookie_agent_protocol::FrozenModelBinding,
    internal_policy: &FrozenInternalAgentPolicy,
    input_tokens: u64,
) -> bool {
    if binding.descriptor.capabilities.compaction != CompactionCapability::Native {
        return internal_policy
            .models
            .iter()
            .any(|candidate| internal_agent_input_fits(input_tokens, candidate, internal_policy));
    }
    input_tokens <= native_compaction_input_budget(binding, internal_policy)
}

fn native_compaction_input_budget(
    binding: &cookie_agent_protocol::FrozenModelBinding,
    internal_policy: &FrozenInternalAgentPolicy,
) -> u64 {
    let output_reserve = internal_agent_output_limit(binding, internal_policy)
        .unwrap_or(DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS);
    binding
        .descriptor
        .capabilities
        .limits
        .context
        .unwrap_or(0)
        .saturating_sub(output_reserve)
        .max(1)
}

fn compaction_instruction(focus: Option<&str>) -> String {
    focus.map_or_else(
        || COMPACTION_INSTRUCTION.to_owned(),
        |focus| format!("{COMPACTION_INSTRUCTION}\n\nUser-requested focus: {focus}"),
    )
}

fn compaction_history(
    mut history: Vec<oven_sdk::HistoryTurn>,
    focus: Option<&str>,
    system_prompt: &str,
) -> (Vec<oven_sdk::HistoryTurn>, String) {
    let system = oven_sdk::HistoryTurn::system(oven_sdk::SystemMessage::new(vec![
        oven_sdk::SystemPart::Text(oven_sdk::TextPart::new(system_prompt)),
    ]));
    if matches!(history.first(), Some(oven_sdk::HistoryTurn::System(_))) {
        history[0] = system;
    } else {
        history.insert(0, system);
    }
    let instruction = compaction_instruction(focus);
    history.push(oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(
        vec![oven_sdk::InputPart::Text(oven_sdk::TextPart::new(
            instruction.clone(),
        ))],
    )));
    (history, instruction)
}

fn should_elide_tool_output(
    model_turn_seq: u64,
    protected_turns: &HashSet<u64>,
    already_elided: bool,
    output_bytes: usize,
) -> bool {
    !protected_turns.contains(&model_turn_seq)
        && !already_elided
        && output_bytes >= TOOL_OUTPUT_ELISION_MIN_BYTES
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use cookie_agent_protocol::{
        ArtifactReference, ContextCheckpoint, ContextCheckpointBoundaries,
        ContextCheckpointBudgets, ContextCheckpointCommit, InternalSummaryCheckpoint, MimeType,
        PersistedToolResult as ToolResult, RunId, SafeDisplayText, SessionId, Sha256Digest,
        StoredEvent, SummaryByteLimit, ToolAttachment, ToolEmittedContent, ToolEmittedMessage,
        ToolEmittedMessageRole,
    };
    use oven_sdk::{
        CompactionCapability, FilePart, FileSource, HistoryTurn, InputPart, JsonSchema,
        Request as ModelRequest, SystemMessage, SystemPart, TextPart, ToolContent, ToolDefinition,
        ToolMessage, ToolResultPart, UserMessage,
    };

    use super::{
        COMPACTION_INSTRUCTION, DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS, FitPart,
        TOOL_OUTPUT_ELISION_MIN_BYTES, VIDEO_FILE_FIT_SURROGATE_BYTES, checkpoint_covers_input,
        compaction_gate, compaction_history, compaction_input_fits, compaction_instruction,
        effective_recent_budget, elidable_bytes, estimated_request_tokens, fit_part_bytes,
        native_compaction_input_budget, raw_fit_from_real_usage, resolve_compaction_trigger,
        select_recent_tail, serialized_fit_request_bytes, should_elide_tool_output,
        usage_reaches_compaction_trigger,
    };
    use crate::{
        model_history::assemble_model_context,
        runtime::{
            ContextTokenEstimator, Event, FrozenInternalAgentPolicy, InternalAgentLimits,
            artifacts::ArtifactStore,
        },
    };
    use cookie_agent_config::ContextCompactionTrigger;

    #[test]
    fn compaction_trigger_math_supports_percent_and_saturating_buffer() {
        assert_eq!(
            resolve_compaction_trigger(200_000, &ContextCompactionTrigger::Percent { percent: 70 }),
            140_000
        );
        assert_eq!(
            resolve_compaction_trigger(
                200_000,
                &ContextCompactionTrigger::BufferTokens {
                    buffer_tokens: 33_000
                }
            ),
            167_000
        );
        assert_eq!(
            resolve_compaction_trigger(
                8_192,
                &ContextCompactionTrigger::BufferTokens {
                    buffer_tokens: 33_000
                }
            ),
            0
        );
    }

    #[test]
    fn request_estimate_counts_tool_prompt_section_bytes() {
        let base = vec![HistoryTurn::system(SystemMessage::new(vec![
            SystemPart::Text(TextPart::new("Base prompt.")),
        ]))];
        let with_section = vec![HistoryTurn::system(SystemMessage::new(vec![
            SystemPart::Text(TextPart::new(
                "Base prompt.\n<tool_instructions provider=\"test\">\nProvider policy text.\n</tool_instructions>",
            )),
        ]))];
        assert!(
            estimated_request_tokens(&with_section, &[]).unwrap()
                > estimated_request_tokens(&base, &[]).unwrap()
        );
    }

    #[test]
    fn automatic_compaction_gate_changes_at_proportional_threshold() {
        let trigger =
            resolve_compaction_trigger(200_000, &ContextCompactionTrigger::Percent { percent: 70 });
        assert!(compaction_gate(false, true, trigger));
        assert!(!usage_reaches_compaction_trigger(139_999, trigger));
        assert!(usage_reaches_compaction_trigger(140_000, trigger));
    }

    #[test]
    fn auto_off_blocks_automatic_compaction_but_not_manual_force() {
        assert!(!compaction_gate(false, false, 100));
        assert!(compaction_gate(true, false, 0));
    }

    fn file_history(files: Vec<FilePart>) -> Vec<HistoryTurn> {
        vec![HistoryTurn::user(UserMessage::new(
            files.into_iter().map(InputPart::File).collect(),
        ))]
    }

    #[test]
    fn one_megabyte_image_contributes_zero_to_fit_estimate() {
        let media = file_history(vec![FilePart::image(
            "image/png",
            FileSource::Bytes(bytes::Bytes::from(vec![0_u8; 1024 * 1024])),
        )]);
        let empty = file_history(Vec::new());

        assert_eq!(
            estimated_request_tokens(&media, &[]).unwrap(),
            estimated_request_tokens(&empty, &[]).unwrap()
        );
    }

    #[test]
    fn several_images_do_not_trigger_but_text_heavy_history_still_does() {
        let text = TextPart::new("small text-only context");
        let mut media_parts = vec![InputPart::Text(text.clone())];
        media_parts.extend(
            (0..3)
                .map(|_| {
                    FilePart::image(
                        "image/png",
                        FileSource::Bytes(bytes::Bytes::from(vec![0_u8; 1024 * 1024])),
                    )
                })
                .map(InputPart::File),
        );
        let images = vec![HistoryTurn::user(UserMessage::new(media_parts))];
        let text_only = vec![HistoryTurn::user(UserMessage::new(vec![InputPart::Text(
            text,
        )]))];
        let text_heavy = vec![HistoryTurn::user(UserMessage::new(vec![InputPart::Text(
            TextPart::new("x".repeat(128 * 1024)),
        )]))];
        let trigger = 10_000;

        assert_eq!(
            estimated_request_tokens(&images, &[]).unwrap(),
            estimated_request_tokens(&text_only, &[]).unwrap()
        );
        assert!(!usage_reaches_compaction_trigger(
            estimated_request_tokens(&images, &[]).unwrap(),
            trigger
        ));
        assert!(usage_reaches_compaction_trigger(
            estimated_request_tokens(&text_heavy, &[]).unwrap(),
            trigger
        ));
    }

    #[test]
    fn media_calibration_uses_text_bytes_and_still_triggers_for_heavy_text() {
        let calibration_text = TextPart::new("x".repeat(16 * 1024));
        let text_only = vec![HistoryTurn::user(UserMessage::new(vec![InputPart::Text(
            calibration_text.clone(),
        )]))];
        let with_image = vec![HistoryTurn::user(UserMessage::new(vec![
            InputPart::Text(calibration_text),
            InputPart::File(FilePart::image(
                "image/png",
                FileSource::Bytes(bytes::Bytes::from(vec![0_u8; 1024 * 1024])),
            )),
        ]))];
        let text_bytes = serialized_fit_request_bytes(&text_only, &[]).unwrap();
        let media_bytes = serialized_fit_request_bytes(&with_image, &[]).unwrap();
        assert_eq!(media_bytes, text_bytes);

        let observed_tokens = (text_bytes as u64).div_ceil(4);
        let mut text_estimator = ContextTokenEstimator::default();
        text_estimator.record_committed_turn(text_bytes, Some(observed_tokens));
        let mut media_estimator = ContextTokenEstimator::default();
        media_estimator.record_committed_turn(media_bytes, Some(observed_tokens));
        assert!(
            (media_estimator.tokens_per_byte - text_estimator.tokens_per_byte).abs() < f64::EPSILON
        );

        let heavy = vec![HistoryTurn::user(UserMessage::new(vec![
            InputPart::Text(TextPart::new("x".repeat(128 * 1024))),
            InputPart::File(FilePart::image(
                "image/png",
                FileSource::Bytes(bytes::Bytes::from(vec![0_u8; 1024 * 1024])),
            )),
        ]))];
        let heavy_bytes = serialized_fit_request_bytes(&heavy, &[]).unwrap();
        assert!(
            media_estimator
                .estimated_context_tokens(heavy_bytes)
                .is_some_and(|tokens| tokens >= 10_000)
        );
    }

    #[test]
    fn large_pdf_contributes_zero_to_fit_estimate() {
        let pdf = file_history(vec![FilePart::document(
            "application/pdf",
            FileSource::Bytes(bytes::Bytes::from(vec![0_u8; 2 * 1024 * 1024])),
        )]);
        let empty = file_history(Vec::new());

        assert_eq!(
            estimated_request_tokens(&pdf, &[]).unwrap(),
            estimated_request_tokens(&empty, &[]).unwrap()
        );
    }

    #[test]
    fn video_uses_flat_fit_cost_and_multiple_videos_trigger() {
        let video = || {
            FilePart::video(
                "video/mp4",
                FileSource::Bytes(bytes::Bytes::from(vec![0_u8; 1024 * 1024])),
            )
        };
        let empty_tokens = estimated_request_tokens(&file_history(Vec::new()), &[]).unwrap();
        let one_video_tokens = estimated_request_tokens(&file_history(vec![video()]), &[]).unwrap();
        let two_video_tokens =
            estimated_request_tokens(&file_history(vec![video(), video()]), &[]).unwrap();

        assert_eq!(VIDEO_FILE_FIT_SURROGATE_BYTES, 160_000);
        assert_eq!(one_video_tokens - empty_tokens, 40_000);
        assert_eq!(two_video_tokens - empty_tokens, 80_000);
        assert!(!usage_reaches_compaction_trigger(one_video_tokens, 50_000));
        assert!(usage_reaches_compaction_trigger(two_video_tokens, 50_000));
    }

    #[test]
    fn real_usage_fit_is_inclusive_and_overflow_recovery_uses_elision_path() {
        let fits = |tokens| tokens <= 100;
        assert_eq!(raw_fit_from_real_usage(false, Some(99), fits), Some(true));
        assert_eq!(raw_fit_from_real_usage(false, Some(100), fits), Some(true));
        assert_eq!(raw_fit_from_real_usage(false, Some(101), fits), None);
        assert_eq!(raw_fit_from_real_usage(false, None, fits), None);
        assert_eq!(raw_fit_from_real_usage(true, Some(1), fits), Some(false));
    }

    #[test]
    fn compaction_budget_uses_harness_or_native_limit_and_reserves_output() {
        let mut harness_binding = crate::test_support::model_binding();
        harness_binding.descriptor.capabilities.limits.context = Some(100_000);
        let mut policy = FrozenInternalAgentPolicy {
            agent: crate::test_support::agent_snapshot(
                "compaction",
                cookie_agent_protocol::AgentMode::Internal,
            ),
            models: vec![harness_binding.clone()],
            runtime: None,
            limits: InternalAgentLimits {
                max_output_tokens: 2_048,
                timeout_ms: 30_000,
            },
            cache_strategies: vec![None],
        };
        assert!(compaction_input_fits(&harness_binding, &policy, 97_952));
        assert!(!compaction_input_fits(&harness_binding, &policy, 97_953));

        let mut native_binding = harness_binding;
        native_binding.descriptor.capabilities.compaction = CompactionCapability::Native;
        native_binding.descriptor.capabilities.limits.context = Some(50_000);
        assert_eq!(
            native_compaction_input_budget(&native_binding, &policy),
            47_952
        );

        policy.limits.max_output_tokens = 0;
        assert_eq!(
            native_compaction_input_budget(&native_binding, &policy),
            47_952
        );
        native_binding.descriptor.capabilities.limits.output = None;
        assert_eq!(
            native_compaction_input_budget(&native_binding, &policy),
            50_000 - DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS
        );
        native_binding.descriptor.capabilities.limits.context = Some(10_000);
        assert_eq!(native_compaction_input_budget(&native_binding, &policy), 1);
    }

    #[test]
    fn compaction_raw_fit_accepts_any_fitting_fallback_order() {
        let mut small = crate::test_support::model_binding_named("fallback-zero");
        small.descriptor.capabilities.limits.context = Some(4_096);
        let mut large = crate::test_support::model_binding_named("fallback-one");
        large.descriptor.capabilities.limits.context = Some(200_000);
        let owner = crate::test_support::model_binding();
        let mut policy = FrozenInternalAgentPolicy {
            agent: crate::test_support::agent_snapshot(
                "compaction",
                cookie_agent_protocol::AgentMode::Internal,
            ),
            models: vec![small.clone(), large.clone()],
            runtime: None,
            limits: InternalAgentLimits {
                max_output_tokens: 2_048,
                timeout_ms: 30_000,
            },
            cache_strategies: vec![None, None],
        };

        assert!(compaction_input_fits(&owner, &policy, 10_000));
        policy.models = vec![large, small];
        assert!(compaction_input_fits(&owner, &policy, 10_000));
        policy.models.truncate(1);
        policy.models[0].descriptor.capabilities.limits.context = Some(4_096);
        assert!(!compaction_input_fits(&owner, &policy, 10_000));
    }

    #[test]
    fn checkpoint_dedup_includes_the_exact_snapshot_boundary() {
        let session = SessionId::new_v7();
        let run = RunId::new_v7();
        let checkpoint = InternalSummaryCheckpoint::new(
            "summary".into(),
            cookie_agent_protocol::InternalAgentInvocationId::new_v7(),
            cookie_agent_protocol::InternalAgentRunId::new_v7(),
            SummaryByteLimit::new(1_024).unwrap(),
        )
        .unwrap();
        let events = vec![StoredEvent {
            engine_version: None,
            origin: None,
            session_id: session,
            run_id: Some(run),
            seq: 11,
            timestamp: jiff::Timestamp::now(),
            payload: Event::ContextCheckpointCommitted {
                commit: ContextCheckpointCommit {
                    checkpoint: ContextCheckpoint::InternalSummary { checkpoint },
                    boundaries: ContextCheckpointBoundaries {
                        source_from_seq: 1,
                        source_through_seq: 10,
                        input_through_seq: 10,
                        prior_checkpoint_seq: None,
                        recent_from_seq: None,
                    },
                    budgets: ContextCheckpointBudgets {
                        context_limit_tokens: 100,
                        trigger_tokens: 70,
                        input_tokens_before: 50,
                        input_tokens_after: 10,
                        max_summary_bytes: SummaryByteLimit::new(1_024).unwrap(),
                        keep_recent_tokens: 0,
                    },
                },
            },
        }];
        assert!(checkpoint_covers_input(&events, 10));
        assert!(!checkpoint_covers_input(&events, 11));
    }

    #[test]
    fn focus_is_appended_without_changing_the_fixed_instruction() {
        assert_eq!(compaction_instruction(None), COMPACTION_INSTRUCTION);
        assert_eq!(
            compaction_instruction(Some("preserve parser work")),
            format!("{COMPACTION_INSTRUCTION}\n\nUser-requested focus: preserve parser work")
        );
    }

    #[test]
    fn compact_provider_request_is_the_assembled_normal_prefix_plus_one_instruction() {
        let temporary = tempfile::TempDir::new().expect("temp directory");
        let artifacts =
            ArtifactStore::open(temporary.path().join("artifacts")).expect("artifact store");
        let (runtime, binding) = crate::test_support::model_runtime_and_binding();
        let session = SessionId::new_v7();
        let run = RunId::new_v7();
        let events = vec![StoredEvent {
            engine_version: None,
            origin: None,
            session_id: session,
            run_id: Some(run),
            seq: 1,
            timestamp: jiff::Timestamp::now(),
            payload: Event::UserInputSubmitted {
                input: "work".into(),
            },
        }];
        let context = assemble_model_context(&events, &artifacts, &binding, "system")
            .expect("assembled context");
        let tools = vec![ToolDefinition::new(
            "read",
            "Read a file",
            JsonSchema::new(serde_json::json!({
                "type": "object",
                "properties": {"filePath": {"type": "string"}},
                "required": ["filePath"]
            }))
            .expect("schema"),
        )];
        let model = runtime.resolve(&binding.selection).expect("resolved model");
        let normal_request = model
            .prepare_request(ModelRequest::new(context.history.clone()).with_tools(tools.clone()));
        let (compact_history, _) = compaction_history(context.history, None, "system");
        let compact_request =
            model.prepare_request(ModelRequest::new(compact_history).with_tools(tools));
        let normal = serde_json::to_value(normal_request).expect("normal provider request");
        let compact = serde_json::to_value(compact_request).expect("compact provider request");
        let normal_history = normal["history"].as_array().expect("normal history");
        let compact_history = compact["history"].as_array().expect("compact history");
        assert_eq!(
            serde_json::to_vec(normal_history).unwrap(),
            serde_json::to_vec(&compact_history[..normal_history.len()]).unwrap()
        );
        assert_eq!(compact_history.len(), normal_history.len() + 1);
        let mut normal_without_history = normal;
        let mut compact_without_history = compact;
        normal_without_history
            .as_object_mut()
            .unwrap()
            .remove("history");
        compact_without_history
            .as_object_mut()
            .unwrap()
            .remove("history");
        assert_eq!(normal_without_history, compact_without_history);
    }

    #[test]
    fn elision_protects_recent_turns_and_requires_bulky_output() {
        let protected = HashSet::from([8, 9]);
        assert!(!should_elide_tool_output(
            9,
            &protected,
            false,
            TOOL_OUTPUT_ELISION_MIN_BYTES
        ));
        assert!(!should_elide_tool_output(
            7,
            &protected,
            false,
            TOOL_OUTPUT_ELISION_MIN_BYTES - 1
        ));
        assert!(should_elide_tool_output(
            7,
            &protected,
            false,
            TOOL_OUTPUT_ELISION_MIN_BYTES
        ));
        assert!(!should_elide_tool_output(
            7,
            &protected,
            true,
            TOOL_OUTPUT_ELISION_MIN_BYTES
        ));
    }

    #[test]
    fn elision_and_estimator_share_emitted_part_byte_accounting() {
        let attachment = ToolAttachment {
            mime_type: MimeType::new("video/mp4").unwrap(),
            filename: Some("clip.mp4".into()),
            byte_length: 4,
            sha256: Sha256Digest::of_bytes(b"clip"),
            reference: ArtifactReference {
                uri: format!("artifact://sha256/{}", "a".repeat(64)),
            },
        };
        let result = ToolResult {
            title: SafeDisplayText::new("Video").unwrap(),
            output: "x".repeat(60),
            metadata: serde_json::Value::Null,
            truncation: None,
            attachments: Vec::new(),
            additional_messages: vec![
                ToolEmittedMessage::new(
                    ToolEmittedMessageRole::User,
                    vec![
                        ToolEmittedContent::Text("e".repeat(4 * 1024)),
                        ToolEmittedContent::File(attachment.clone()),
                    ],
                )
                .unwrap(),
            ],
        };
        let expected = fit_part_bytes(FitPart::Text(&result.output))
            + fit_part_bytes(FitPart::Text("e".repeat(4 * 1024).as_str()))
            + fit_part_bytes(FitPart::File(attachment.mime_type.as_str()));
        assert_eq!(
            fit_part_bytes(FitPart::File("video/mp4")),
            VIDEO_FILE_FIT_SURROGATE_BYTES
        );
        assert_eq!(elidable_bytes(&result), expected);
        assert!(expected >= TOOL_OUTPUT_ELISION_MIN_BYTES);
        assert!(should_elide_tool_output(
            1,
            &HashSet::new(),
            false,
            elidable_bytes(&result)
        ));

        let baseline = vec![
            HistoryTurn::tool(ToolMessage::new(vec![ToolResultPart::new(
                "call",
                ToolContent::Text(String::new()),
            )])),
            HistoryTurn::user(UserMessage::new(Vec::new())),
        ];
        let with_emitted_parts = vec![
            HistoryTurn::tool(ToolMessage::new(vec![ToolResultPart::new(
                "call",
                ToolContent::Text(result.output.clone()),
            )])),
            HistoryTurn::user(UserMessage::new(vec![
                InputPart::Text(TextPart::new("e".repeat(4 * 1024))),
                InputPart::File(FilePart::video(
                    "video/mp4",
                    FileSource::Bytes(b"clip".to_vec().into()),
                )),
            ])),
        ];
        let baseline_bytes = serialized_fit_request_bytes(&baseline, &[]).unwrap();
        let with_emitted_bytes = serialized_fit_request_bytes(&with_emitted_parts, &[]).unwrap();
        assert_eq!(
            with_emitted_bytes - baseline_bytes,
            expected,
            "estimator and selector must charge the same shared per-part bytes"
        );
    }

    #[test]
    fn recent_budget_handles_zero_exact_fit_overbudget_and_huge_targets() {
        assert_eq!(effective_recent_budget(0, 100_000, 20_000), 0);
        assert_eq!(effective_recent_budget(16_384, 100_000, 16_384), 16_384);
        assert_eq!(effective_recent_budget(16_384, 100_000, 123), 123);
        assert_eq!(effective_recent_budget(16_384, 100_000, 0), 0);
        assert_eq!(effective_recent_budget(u64::MAX, 100_000, u64::MAX), 25_000);
        assert_eq!(
            effective_recent_budget(u64::MAX, u64::MAX, u64::MAX),
            u64::MAX / 4
        );
    }

    #[test]
    fn recent_selection_keeps_only_fitting_complete_suffixes() {
        let select = |budget, limit| {
            select_recent_tail(vec![10, 20, 30], budget, 100, limit, |seq| {
                Ok(match seq {
                    10 => 400,
                    20 => 300,
                    _ => 200,
                })
            })
            .unwrap()
        };
        assert_eq!(select(0, 1_000), None);
        assert_eq!(select(200, 1_000), Some(20));
        assert_eq!(select(199, 1_000), Some(30));
        assert_eq!(select(99, 1_000), None);
        assert_eq!(select(u64::MAX, 199), None);
        assert_eq!(select(u64::MAX, 300), Some(20));
        assert_eq!(select(u64::MAX, u64::MAX), Some(10));
    }
}
