use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    sync::{Arc, Mutex},
};

use cookie_agent_config::ContextCompactionTrigger;
use cookie_agent_models::adapters::with_native_compaction_instructions;
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
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

use super::titles::active_fallback_index;
use super::{
    Engine, EngineError, Event, FrozenInternalAgentPolicy, InternalAgentExecution,
    InternalAgentHistoryInput, SessionCommand, internal_agents::internal_agent_output_limit,
};
use crate::{
    model_bridge::AbortBridge,
    model_history::{self, assemble_model_context},
    policy::{self, FrozenRunPolicy},
};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ContextTokenEstimator {
    pub(crate) tokens_per_byte: f64,
    pub(crate) last_committed_input_tokens: u64,
}

pub(crate) struct PredictiveCompactionInput<'a> {
    pub(crate) session: SessionId,
    pub(crate) run: RunId,
    pub(crate) serialized_message_bytes: usize,
    pub(crate) policy: &'a FrozenRunPolicy,
    pub(crate) fallback_index: usize,
    pub(crate) cancellation: &'a CancellationToken,
    pub(crate) actor_direct: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum CompactionDeferredKind {
    Start,
    PromotePendingInputs,
    PromotePendingOrComplete,
    Resume,
}

impl ContextTokenEstimator {
    pub(crate) fn record_committed_turn(
        &mut self,
        serialized_context_bytes: usize,
        input_tokens: Option<u64>,
    ) {
        self.last_committed_input_tokens = input_tokens.unwrap_or(0);
        if serialized_context_bytes > 0
            && let Some(input_tokens) = input_tokens.filter(|tokens| *tokens > 0)
        {
            self.tokens_per_byte = input_tokens as f64 / serialized_context_bytes as f64;
        }
    }

    pub(crate) fn projected_tokens(self, serialized_message_bytes: usize) -> Option<u64> {
        (self.tokens_per_byte > 0.0).then(|| {
            self.last_committed_input_tokens
                .saturating_add((serialized_message_bytes as f64 * self.tokens_per_byte) as u64)
        })
    }

    pub(crate) fn estimated_context_tokens(self, serialized_context_bytes: usize) -> Option<u64> {
        (self.tokens_per_byte > 0.0)
            .then(|| (serialized_context_bytes as f64 * self.tokens_per_byte).ceil() as u64)
    }

    pub(crate) fn should_compact(self, serialized_message_bytes: usize, soft_tokens: u64) -> bool {
        self.projected_tokens(serialized_message_bytes)
            .is_some_and(|projected| projected >= soft_tokens)
    }

    pub(crate) fn record_compaction(&mut self, estimated_input_tokens: u64) {
        self.last_committed_input_tokens = estimated_input_tokens;
    }
}

pub(crate) fn should_run_predictive_compaction(
    estimator: ContextTokenEstimator,
    serialized_message_bytes: usize,
    soft_tokens: u64,
    session_persisted: bool,
) -> bool {
    session_persisted && estimator.should_compact(serialized_message_bytes, soft_tokens)
}

/// Compaction runtime state owned by [`super::Inner`].
#[derive(Default)]
pub(crate) struct CompactionState {
    pub(crate) in_progress: Mutex<HashSet<SessionId>>,
    pub(super) deferred: Mutex<HashMap<SessionId, VecDeque<SessionCommand>>>,
    pub(crate) context_token_estimators: Mutex<HashMap<SessionId, ContextTokenEstimator>>,
}

pub(crate) const COMPACTION_INSTRUCTION: &str = "Create a detailed technical summary of the conversation so work can continue without the earlier context. Include: the goal/objective; decisions and their rationale; files changed and current code state; commands run and their outcomes; errors encountered and fixes applied; and the pending next step. Preserve exact identifiers, paths, constraints, and unresolved questions. Return summary text only and do not call tools.";
pub(super) const TOOL_OUTPUT_ELISION_MIN_BYTES: usize = 8 * 1024;
// Frozen policies normally provide this value. Keep substantial headroom when replaying an
// unavailable or legacy policy whose output limit is zero.
pub(super) const DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS: u64 = 20_000;
const MAX_AUTO_COMPACTION_FAILURES: u8 = 3;
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
    /// Predictive compaction has already made its own trigger decision.
    pub(super) skip_usage_trigger: bool,
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
                skip_usage_trigger: false,
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
        let active_run = self
            .inner
            .sessions
            .active
            .lock()
            .ok()
            .and_then(|runs| runs.get(&input.run).cloned());
        if !input.force
            && active_run.as_ref().is_some_and(|run| {
                run.auto_compaction_failures.load(Ordering::Relaxed) >= MAX_AUTO_COMPACTION_FAILURES
            })
        {
            return Ok(input.events);
        }
        let projection = self.inner.store.get(input.session)?;
        if !input.force && !input.skip_usage_trigger {
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
        // Native compaction keeps its existing durable elision policy. Summary compaction
        // checks the actual internal-agent request and only prunes a private retry snapshot.
        let raw_fits =
            if input.binding.descriptor.capabilities.compaction != CompactionCapability::Native {
                true
            } else if let Some(raw_fits) = raw_fit_from_real_usage(
                input.overflow_recovery,
                projection
                    .log
                    .latest_real_usage()
                    .map(|(_, observed_tokens)| observed_tokens),
                |tokens| compaction_input_fits(input.binding, input.internal_policy, tokens),
            ) {
                raw_fits
            } else {
                let raw_fit_tokens =
                    self.estimated_request_tokens(input.session, &context.history, input.tools)?;
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
                let compact_request = with_native_compaction_instructions(
                    CompactionRequest::new(request),
                    input.binding.descriptor.adapter_id.as_str(),
                    compaction_focus.clone(),
                );
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

        // Reserve an output-sized summary before selecting a suffix. The reserve is the
        // compaction agent's own effective output cap, which it inherits from the owner run
        // when its document declares none. The final fit check uses the actual replay
        // projection and the same calibrated estimator as the input.
        let output_reserve = internal_agent_output_limit(input.binding, input.internal_policy)
            .unwrap_or(DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS);
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
            context.history,
            compaction_focus.as_deref(),
            &input.internal_policy.agent.composed_prompt,
        );
        let mut summary = self
            .run_internal_history_agent(
                input.session,
                Some(input.run),
                InternalAgentKind::ContextCompaction,
                input.internal_policy,
                InternalAgentHistoryInput {
                    history,
                    summary_source: instruction,
                    // First trial keeps the session tool definitions so the
                    // request stays a cache-friendly extension of the latest
                    // conversation turn; a tool-call answer is rejected by
                    // reject_non_text and counted as a compaction failure.
                    tools: input.tools.to_vec(),
                    reject_non_text: true,
                },
                InternalAgentExecution {
                    cancellation: input.cancellation,
                    actor_direct: input.actor_direct,
                },
            )
            .await;
        if matches!(&summary, Err(EngineError::Model(error))
            if error.kind == oven_sdk::ModelErrorKind::ContextLength)
            && !input.cancellation.is_cancelled()
        {
            let history = pruned_compaction_history(
                &events,
                &self.inner.artifacts,
                input.session,
                input.binding,
                &composed_prompt,
            )?;
            let (history, instruction) = compaction_history(
                history,
                compaction_focus.as_deref(),
                &input.internal_policy.agent.composed_prompt,
            );
            summary = self
                .run_internal_history_agent(
                    input.session,
                    Some(input.run),
                    InternalAgentKind::ContextCompaction,
                    input.internal_policy,
                    InternalAgentHistoryInput {
                        history,
                        summary_source: instruction,
                        // The pruned retry already rewrites tool outputs, so it
                        // drops the tool definitions too: cache affinity is
                        // lost either way, and fewer tools means less to send.
                        tools: Vec::new(),
                        reject_non_text: true,
                    },
                    InternalAgentExecution {
                        cancellation: input.cancellation,
                        actor_direct: input.actor_direct,
                    },
                )
                .await;
        }
        let Ok(summary) = summary else {
            if !input.force
                && let Some(run) = &active_run
                && run.auto_compaction_failures.fetch_add(1, Ordering::Relaxed) + 1
                    == MAX_AUTO_COMPACTION_FAILURES
                && !run
                    .auto_compaction_diagnostic_emitted
                    .swap(true, Ordering::Relaxed)
            {
                self.record_plugin_diagnostic(
                    input.session,
                    "engine:auto-compact".into(),
                    PluginDiagnosticKind::UnsupportedCapability,
                    "automatic context compaction disabled after 3 consecutive failures for this run"
                        .to_string(),
                );
            }
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
        if let Some(run) = active_run {
            run.auto_compaction_failures.store(0, Ordering::Relaxed);
        }
        self.finalize_context_checkpoint(input.session, input_tokens_after)
    }

    fn finalize_context_checkpoint(
        &self,
        session: SessionId,
        input_tokens_after: u64,
    ) -> Result<Arc<[StoredEvent]>, EngineError> {
        self.inner
            .compaction
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
            let (retained, _) = self
                .inner
                .artifacts
                .retain(session, result.output.as_bytes())?;
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
            .compaction
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
        // Provider admission is authoritative for internal agents. The estimate remains
        // useful for native compaction budgeting, but must not reject a prompt locally.
        let _ = (internal_policy, input_tokens);
        return true;
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

fn pruned_compaction_history(
    events: &[StoredEvent],
    store: &super::artifacts::ArtifactRouter,
    session: SessionId,
    binding: &cookie_agent_protocol::FrozenModelBinding,
    composed_prompt: &str,
) -> Result<Vec<oven_sdk::HistoryTurn>, EngineError> {
    // Emitted messages lose their tool ownership after assembly. Remove them in a private
    // snapshot first, then prune only results in the active checkpoint-aware history.
    let mut events = events.to_vec();
    for event in &mut events {
        if let Event::ToolCallTerminated { termination } = &mut event.payload
            && let Some(result) = &mut termination.result
        {
            result.additional_messages.clear();
            result.attachments.clear();
        }
    }
    let mut history = assemble_model_context(&events, store, binding, composed_prompt)?.history;
    let mut retrieval_calls = HashSet::new();
    for turn in &mut history {
        match turn {
            oven_sdk::HistoryTurn::Assistant(turn) => {
                for part in &turn.message.content {
                    if let oven_sdk::AssistantPart::ToolCall(call) = part {
                        if call.name == "read"
                            && call
                                .input
                                .get("filePath")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|path| {
                                    cookie_agent_protocol::ArtifactReadPath::parse(path).is_ok()
                                })
                        {
                            retrieval_calls.insert(call.id.clone());
                        } else {
                            retrieval_calls.remove(&call.id);
                        }
                    }
                }
                for part in &mut turn.message.content {
                    if let oven_sdk::AssistantPart::ToolResult(result) = part {
                        prune_compaction_result(result, &retrieval_calls, store, session)?;
                    }
                }
            }
            oven_sdk::HistoryTurn::Tool(message) => {
                for result in &mut message.results {
                    prune_compaction_result(result, &retrieval_calls, store, session)?;
                }
            }
            _ => {}
        }
    }
    Ok(history)
}

fn prune_compaction_result(
    result: &mut oven_sdk::ToolResultPart,
    retrieval_calls: &HashSet<String>,
    store: &super::artifacts::ArtifactRouter,
    session: SessionId,
) -> Result<(), EngineError> {
    let marker = if retrieval_calls.contains(&result.tool_call_id) {
        // This is a private summary-input copy; the original result stays in history.
        "[artifact read output omitted for compaction]".to_owned()
    } else {
        let content = match &result.content {
            oven_sdk::ToolContent::Text(text) => text.as_bytes().to_vec(),
            content => serde_json::to_vec(content)
                .map_err(|error| ModelError::invalid_request(error.to_string()))?,
        };
        let (_, artifact_id) = store.retain(session, &content)?;
        let mut marker =
            model_history::tool_output_elision_marker(&artifact_id, content.len() as u64, 0);
        if !matches!(result.content, oven_sdk::ToolContent::Text(_)) {
            marker.push_str(" Stored as serialized tool content (JSON).");
        }
        let read_more = serde_json::json!({
            "read_more": {
                "tool": "read",
                "arguments": {"filePath": format!("artifact://{artifact_id}")}
            }
        });
        marker.push('\n');
        marker.push_str(&read_more.to_string());
        marker
    };
    result.content = oven_sdk::ToolContent::Text(marker);
    result.metadata = None;
    Ok(())
}

fn compaction_history(
    mut history: Vec<oven_sdk::HistoryTurn>,
    focus: Option<&str>,
    system_prompt: &str,
) -> (Vec<oven_sdk::HistoryTurn>, String) {
    // Keep the history (including its system prompt) intact so the summarizer
    // request is a cache-friendly extension of the latest conversation turn;
    // the compaction agent's own prompt rides along in the trailing
    // instruction instead of replacing the session system prompt.
    let base = compaction_instruction(focus);
    let instruction = if system_prompt.trim().is_empty() {
        base
    } else {
        format!("{}\n\n{base}", system_prompt.trim())
    };
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
mod tests;
