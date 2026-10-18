use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    sync::{Arc, Mutex},
};

use cookie_agent_config::{ContextCompactionTrigger, InternalPromptValues, render_internal_prompt};
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
    model_history::{self, assemble_model_context, wire_model},
    policy::{self, FrozenRunPolicy},
};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ContextTokenEstimator {
    pub(crate) tokens_per_byte: f64,
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
        if serialized_context_bytes > 0
            && let Some(input_tokens) = input_tokens.filter(|tokens| *tokens > 0)
        {
            self.tokens_per_byte = input_tokens as f64 / serialized_context_bytes as f64;
        }
    }

    pub(crate) fn estimated_context_tokens(self, serialized_context_bytes: usize) -> Option<u64> {
        (self.tokens_per_byte > 0.0)
            .then(|| (serialized_context_bytes as f64 * self.tokens_per_byte).ceil() as u64)
    }
}

/// Compaction runtime state owned by [`super::Inner`].
#[derive(Default)]
pub(crate) struct CompactionState {
    pub(crate) in_progress: Mutex<HashSet<SessionId>>,
    /// Woken whenever a session's reservation is released.
    pub(crate) finished: tokio::sync::Notify,
    /// The cancellation of each running manual compaction.
    pub(crate) manual: Mutex<HashMap<SessionId, CancellationToken>>,
    pub(super) deferred: Mutex<HashMap<SessionId, VecDeque<SessionCommand>>>,
    pub(crate) context_token_estimators: Mutex<HashMap<SessionId, ContextTokenEstimator>>,
}

pub(crate) const COMPACTION_PROMPT_INTRO: &str = "Summarize the conversation above into a checkpoint that another agent will use to continue the work. The checkpoint replaces everything above except the system prompt and pinned context; any later messages stay verbatim after it.";
pub(crate) const COMPACTION_UPDATE_PROMPT_INTRO: &str = "The conversation above begins with an earlier checkpoint inside <summary> tags. Write one updated checkpoint that merges it with everything after it; the new checkpoint replaces both, so carry forward whatever still matters from the earlier one. Newer information wins. Drop items that are resolved or no longer relevant, but keep user requirements that still apply.";
const COMPACTION_PROMPT_TEMPLATE: &str = r#"Use exactly these Markdown sections, in this order. Write "None." under a section with nothing to record.

## Objective
The user's overall goal and the current task, in their terms.

## Requirements and Constraints
Explicit user instructions, preferences, and corrections, including what not to do. Quote short instructions verbatim.

## Decisions
Choices made and why, including approaches tried and rejected.

## Work State
- Completed: what is done and how it was verified.
- In progress: what was underway when the conversation was cut.
- Blocked: open problems and unanswered questions.

## Files and Code
Files read, created, or changed: what changed and why. Key functions, types, and signatures.

## Commands and Errors
Commands that matter and their outcomes; errors hit and how they were fixed.

## Next Step
The immediate next action, specific enough to act on without rereading anything.

## Important Context
Anything else needed to continue: environment facts, identifiers, URLs, numbers.

Rules:
- Be terse; prefer bullets to prose.
- Preserve exact paths, identifiers, commands, error messages, and numbers.
- Omit pinned context such as AGENTS.md and loaded skills; it stays available separately.
- Do not mention this checkpoint or the summarization.
- Do not continue the task, ask questions, or call tools. Return the checkpoint text only."#;
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
    pub(super) events: crate::events::EventSnapshot,
    pub(super) force: bool,
    pub(super) overflow_recovery: bool,
    pub(super) focus: Option<&'a str>,
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
        let (reply, receiver) = tokio::sync::oneshot::channel();
        self.on_actor(session, move |engine| {
            engine.spawn_compaction(session, focus, origin, reply);
            Ok(())
        })
        .await?;
        receiver.await.map_err(|_| EngineError::ActorStopped)?
    }

    /// Reserves the session for compaction and runs it off the actor, sending
    /// the outcome to `reply`; a session already held for compaction or a
    /// run start is rejected as running. Called on the session actor.
    pub(super) fn spawn_compaction(
        &self,
        session: SessionId,
        focus: Option<String>,
        origin: cookie_agent_protocol::EventOrigin,
        reply: tokio::sync::oneshot::Sender<Result<SessionCompactResult, EngineError>>,
    ) {
        if !self.reserve_compaction(session) {
            let _ = reply.send(Err(EngineError::SessionRunning(session)));
            return;
        }
        let engine = self.clone();
        tokio::spawn(async move {
            #[cfg(test)]
            let hook = engine
                .inner
                .test_hooks
                .compaction_execution_hook
                .lock()
                .expect("compaction execution hook lock poisoned")
                .take();
            #[cfg(test)]
            if let Some(hook) = hook {
                if let Some(reached) = hook
                    .reached
                    .lock()
                    .expect("compaction execution reached lock poisoned")
                    .take()
                {
                    let _ = reached.send(());
                }
                hook.release.notified().await;
            }
            let mut result = engine
                .compact_session_direct(session, focus.as_deref(), origin)
                .await;
            if let Err(error) = engine.finish_compaction(session).await
                && result.is_ok()
            {
                result = Err(error);
            }
            let _ = reply.send(result);
        });
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
        let cancellation = CancellationToken::new();
        self.inner
            .compaction
            .manual
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session, cancellation.clone());
        let _registered = ManualCompaction {
            engine: self,
            session,
        };
        match self
            .maybe_compact_context(CompactionInput {
                session,
                run,
                cancellation: &cancellation,
                binding,
                owner_policy: &policy,
                internal_policy: &internal_policy,
                tools: &tools,
                events,
                force: true,
                overflow_recovery: false,
                focus,
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
            Err(_) if cancellation.is_cancelled() => {
                return Ok(SessionCompactResult {
                    compacted: false,
                    cancellation_reason: Some("compaction cancelled".into()),
                });
            }
            Err(error) => return Err(error),
        }
        Ok(SessionCompactResult {
            compacted: projection.log.latest_checkpoint_seq() > before,
            cancellation_reason: cancellation
                .is_cancelled()
                .then(|| "compaction cancelled".to_owned()),
        })
    }

    /// Cancels `session`'s running manual compaction. Returns whether one
    /// was running.
    pub fn cancel_compaction(&self, session: SessionId) -> bool {
        self.inner
            .compaction
            .manual
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session)
            .map(CancellationToken::cancel)
            .is_some()
    }

    pub(super) async fn maybe_compact_context(
        &self,
        mut input: CompactionInput<'_>,
    ) -> Result<crate::events::EventSnapshot, EngineError> {
        let Some(context_limit) = input.binding.descriptor.capabilities.limits.context else {
            return Ok(input.events);
        };
        // Triggers and retained-history budgets use the input budget, not the whole context
        // window: a model's input limit or its reserved output can be well below the window.
        let input_budget = policy::input_token_budget(
            input.binding,
            policy::effective_max_output_tokens(
                input.binding,
                input.owner_policy.agent.max_output_tokens,
            ),
        )
        .unwrap_or(context_limit);
        let config = &self.inner.config.load().runtime.context_compaction;
        let trigger_tokens = resolve_compaction_trigger(input_budget, &config.trigger);
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
                    self.record_plugin_diagnostic(
                        input.session,
                        plugin,
                        error.kind(),
                        error.to_string(),
                    );
                }
            }
        }

        let composed_prompt = self.run_agent_prompt(input.session, input.run)?;
        let mut events = input.events.clone();
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
                .stage_tool_output_elision(input.session, &events, input.origin.clone())
                .await?;
            context = assemble_model_context(
                &events,
                &self.inner.artifacts,
                input.binding,
                &composed_prompt,
            )?;
            self.estimated_request_tokens(input.session, &context.history, input.tools)?
        };
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
                crate::media::elide_excess_media(
                    &mut request.history,
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
                    let resolved_model = wire_model(input.binding);
                    self.publish_transient(
                        input.session,
                        input.run,
                        Event::NativeCompactionStarted {
                            resolved_model: resolved_model.clone(),
                        },
                    );
                    let abort = AbortBridge::new(input.cancellation.child_token());
                    let result = model.model().compact(compact_request, abort.signal()).await;
                    self.publish_transient(
                        input.session,
                        input.run,
                        Event::NativeCompactionFinished { resolved_model },
                    );
                    match result {
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
                self.append(
                    input.session,
                    Some(input.run),
                    input.origin.clone(),
                    Event::ContextCheckpointCommitted { commit },
                )
                .await?;
                return Ok(self.inner.store.log(input.session)?.event_snapshot());
            }
        }

        // Reserve an output-sized summary before selecting a suffix. The reserve is the
        // compaction agent's own effective output cap, which it inherits from the owner run
        // when its document declares none. The final fit check uses the actual replay
        // projection and the same calibrated estimator as the input.
        let output_reserve = internal_agent_output_limit(input.binding, input.internal_policy)
            .unwrap_or(DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS);
        let retained_limit = input_budget
            .saturating_sub(output_reserve)
            .min(if trigger_tokens > 0 {
                trigger_tokens - 1
            } else {
                input_budget
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
            input_budget,
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
            return Ok(events);
        }
        // The summarizer sees only the span the checkpoint replaces; the
        // retained recent suffix follows the summary verbatim.
        let instruction = compaction_instruction(
            &input.internal_policy.agent.composed_prompt,
            compaction_focus.as_deref(),
            prior_summary_count > 0,
        );
        let mut summary = self
            .run_internal_history_agent(
                input.session,
                Some(input.run),
                InternalAgentKind::ContextCompaction,
                input.internal_policy,
                InternalAgentHistoryInput {
                    history: compaction_history(prefix, &instruction),
                    summary_source: instruction.clone(),
                    // First trial keeps the session tool definitions so the
                    // request stays a cache-friendly prefix of the latest
                    // conversation request; a tool-call answer is rejected by
                    // reject_non_text and counted as a compaction failure.
                    tools: input.tools.to_vec(),
                    reject_non_text: true,
                },
                InternalAgentExecution {
                    cancellation: input.cancellation,
                    actor_direct: false,
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
                recent_from_seq,
            )?;
            summary = self
                .run_internal_history_agent(
                    input.session,
                    Some(input.run),
                    InternalAgentKind::ContextCompaction,
                    input.internal_policy,
                    InternalAgentHistoryInput {
                        history: compaction_history(history, &instruction),
                        summary_source: instruction,
                        // The pruned retry already rewrites tool outputs, so it
                        // drops the tool definitions too: cache affinity is
                        // lost either way, and fewer tools means less to send.
                        tools: Vec::new(),
                        reject_non_text: true,
                    },
                    InternalAgentExecution {
                        cancellation: input.cancellation,
                        actor_direct: false,
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
                let engine = self.clone();
                let session = input.session;
                tokio::spawn(async move {
                    let _ = engine
                        .on_actor(session, move |engine| {
                            engine.append_direct(
                                session,
                                None,
                                super::event_origin("engine:auto-compact"),
                                Event::Diagnostic {
                                    source: cookie_agent_protocol::DiagnosticSource::engine("auto-compact")
                                        .expect("static diagnostic source is valid"),
                                    kind: cookie_agent_protocol::EngineDiagnosticKind::AutoCompactionDisabled.label(),
                                    message: "automatic context compaction disabled after 3 consecutive failures for this run".into(),
                                    count: None,
                                    detail: None,
                                },
                            )
                        })
                        .await;
                });
            }
            return Ok(events);
        };
        if summary.text.trim().is_empty() {
            return Ok(events);
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
            input_budget,
            retained_limit.saturating_sub(actual_base_tokens),
        );
        if input_tokens_after > retained_limit
            || input_tokens_after.saturating_sub(actual_base_tokens) > keep_recent_tokens
        {
            return Ok(events);
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
            return Ok(events);
        }
        self.append(
            input.session,
            Some(input.run),
            input.origin.clone(),
            Event::ContextCheckpointCommitted { commit },
        )
        .await?;
        if let Some(run) = active_run {
            run.auto_compaction_failures.store(0, Ordering::Relaxed);
        }
        Ok(self.inner.store.log(input.session)?.event_snapshot())
    }

    async fn stage_tool_output_elision(
        &self,
        session: SessionId,
        events: &[Arc<StoredEvent>],
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<crate::events::EventSnapshot, EngineError> {
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
        for event in events {
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
            self.append(
                session,
                event.run_id,
                origin.clone(),
                Event::ToolOutputElided {
                    tool_call_id: termination.tool_call_id,
                    original_bytes: result.output.len() as u64,
                    retained,
                },
            )
            .await?;
        }
        Ok(self.inner.store.log(session)?.event_snapshot())
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
}

/// Unregisters a manual compaction's cancellation however it ends.
struct ManualCompaction<'a> {
    engine: &'a Engine,
    session: SessionId,
}

impl Drop for ManualCompaction<'_> {
    fn drop(&mut self) {
        self.engine
            .inner
            .compaction
            .manual
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.session);
    }
}

pub(crate) fn active_compaction_binding<'a>(
    policy: &'a FrozenRunPolicy,
    events: &[Arc<StoredEvent>],
    run: RunId,
) -> Result<&'a cookie_agent_protocol::FrozenModelBinding, EngineError> {
    policy
        .selected_suffix
        .get(active_fallback_index(events, run))
        .ok_or(EngineError::NoRunnableModel)
}

/// Resolves the trigger against the model's input budget (see `policy::input_token_budget`).
pub(super) fn resolve_compaction_trigger(
    input_budget: u64,
    trigger: &ContextCompactionTrigger,
) -> u64 {
    match trigger {
        ContextCompactionTrigger::Percent { percent } => input_budget
            .saturating_mul(u64::from(*percent))
            .saturating_div(100),
        ContextCompactionTrigger::BufferTokens { buffer_tokens } => {
            input_budget.saturating_sub(*buffer_tokens)
        }
    }
}

fn compaction_gate(force: bool, auto: bool, trigger_tokens: u64) -> bool {
    force || (auto && trigger_tokens > 0)
}

fn usage_reaches_compaction_trigger(observed_tokens: u64, trigger_tokens: u64) -> bool {
    observed_tokens >= trigger_tokens
}

fn effective_recent_budget(target: u64, input_budget: u64, available: u64) -> u64 {
    target.min(input_budget / 4).min(available)
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
fn checkpoint_covers_input(events: &[Arc<StoredEvent>], input_through_seq: u64) -> bool {
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
    // Media costs are flat-ish, usually hundreds to low-thousands of tokens per part, and do not
    // track serialized byte size. Text parts count their raw length, image/PDF/audio files count
    // nothing, and video alone gets a flat surrogate because frame sampling can be materially
    // expensive. Everything else counts its serialized size. Overestimation can trigger
    // unnecessary compaction and discard attachments, while real usage calibrates residual error.
    let mut part_bytes = 0_usize;
    let history = history
        .iter()
        .map(|turn| fit::FitTurn::new(turn, &mut part_bytes))
        .collect::<Vec<_>>();
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, &(history, tools))
        .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?;
    Ok(writer.bytes.saturating_add(part_bytes))
}

/// Borrowing mirrors of the Oven history types that serialize exactly like the
/// originals with text and file parts stripped, so fit sizing never clones history.
/// Every constructor destructures its source exhaustively: a new Oven field fails
/// to compile here instead of silently drifting out of the count.
mod fit {
    use oven_sdk::{
        AssistantPart, CompletedTurn, ContentValue, HistoryTurn, InputPart, SystemPart,
        ToolContent, ToolResultPart,
    };
    use serde::Serialize;

    use super::{FitPart, fit_part_bytes};

    fn add(total: &mut usize, part: FitPart<'_>) {
        *total = total.saturating_add(fit_part_bytes(part));
    }

    #[derive(Serialize)]
    #[serde(tag = "type", content = "value", rename_all = "snake_case")]
    pub(super) enum FitTurn<'a> {
        System(FitMessage<'a, &'a SystemPart>),
        User(FitMessage<'a, &'a InputPart>),
        Assistant(FitCompletedTurn<'a>),
        Tool(FitToolMessage<'a>),
    }

    #[derive(Serialize)]
    pub(super) struct FitMessage<'a, P> {
        content: Vec<P>,
        provider_options: &'a oven_sdk::ProviderOptions,
    }

    #[derive(Serialize)]
    pub(super) struct FitCompletedTurn<'a> {
        message: FitMessage<'a, FitAssistantPart<'a>>,
        finish: &'a oven_sdk::Finish,
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        warnings: &'a [String],
    }

    #[derive(Serialize)]
    #[serde(untagged)]
    pub(super) enum FitAssistantPart<'a> {
        Kept(&'a AssistantPart),
        Result(FitAssistantResult<'a>),
    }

    #[derive(Serialize)]
    #[serde(tag = "type", content = "value", rename_all = "snake_case")]
    pub(super) enum FitAssistantResult<'a> {
        ToolResult(FitToolResult<'a>),
    }

    #[derive(Serialize)]
    pub(super) struct FitToolMessage<'a> {
        results: Vec<FitToolResult<'a>>,
        provider_options: &'a oven_sdk::ProviderOptions,
    }

    #[derive(Serialize)]
    pub(super) struct FitToolResult<'a> {
        tool_call_id: &'a str,
        content: FitToolContent<'a>,
        is_error: bool,
        metadata: &'a oven_sdk::PartMetadata,
    }

    #[derive(Serialize)]
    #[serde(tag = "type", content = "value", rename_all = "snake_case")]
    pub(super) enum FitToolContent<'a> {
        Text(&'static str),
        Json(&'a serde_json::Value),
        Mixed(Vec<&'a ContentValue>),
        Denied { reason: &'a Option<String> },
    }

    impl<'a> FitTurn<'a> {
        pub(super) fn new(turn: &'a HistoryTurn, part_bytes: &mut usize) -> Self {
            match turn {
                HistoryTurn::System(oven_sdk::SystemMessage {
                    content,
                    provider_options,
                }) => Self::System(FitMessage {
                    content: content
                        .iter()
                        .filter(|part| match part {
                            SystemPart::Text(text) => {
                                add(part_bytes, FitPart::Text(&text.text));
                                false
                            }
                            SystemPart::Custom(_) => true,
                        })
                        .collect(),
                    provider_options,
                }),
                HistoryTurn::User(oven_sdk::UserMessage {
                    content,
                    provider_options,
                }) => Self::User(FitMessage {
                    content: content
                        .iter()
                        .filter(|part| match part {
                            InputPart::Text(text) => {
                                add(part_bytes, FitPart::Text(&text.text));
                                false
                            }
                            InputPart::File(file) => {
                                add(part_bytes, FitPart::File(&file.media_type));
                                false
                            }
                            InputPart::Custom(_) => true,
                        })
                        .collect(),
                    provider_options,
                }),
                HistoryTurn::Assistant(turn) => {
                    let CompletedTurn {
                        message:
                            oven_sdk::AssistantMessage {
                                content,
                                provider_options,
                            },
                        finish,
                        warnings,
                    } = turn.as_ref();
                    Self::Assistant(FitCompletedTurn {
                        message: FitMessage {
                            content: content
                                .iter()
                                .filter_map(|part| match part {
                                    AssistantPart::Text(text) => {
                                        add(part_bytes, FitPart::Text(&text.text));
                                        None
                                    }
                                    AssistantPart::File(file) => {
                                        add(part_bytes, FitPart::File(&file.media_type));
                                        None
                                    }
                                    AssistantPart::ToolResult(result) => Some(
                                        FitAssistantPart::Result(FitAssistantResult::ToolResult(
                                            FitToolResult::new(result, part_bytes),
                                        )),
                                    ),
                                    AssistantPart::Reasoning(_)
                                    | AssistantPart::ToolCall(_)
                                    | AssistantPart::Source(_)
                                    | AssistantPart::ToolApproval(_)
                                    | AssistantPart::Custom(_) => {
                                        Some(FitAssistantPart::Kept(part))
                                    }
                                })
                                .collect(),
                            provider_options,
                        },
                        finish,
                        warnings,
                    })
                }
                HistoryTurn::Tool(oven_sdk::ToolMessage {
                    results,
                    provider_options,
                }) => Self::Tool(FitToolMessage {
                    results: results
                        .iter()
                        .map(|result| FitToolResult::new(result, part_bytes))
                        .collect(),
                    provider_options,
                }),
            }
        }
    }

    impl<'a> FitToolResult<'a> {
        fn new(result: &'a ToolResultPart, part_bytes: &mut usize) -> Self {
            let ToolResultPart {
                tool_call_id,
                content,
                is_error,
                metadata,
            } = result;
            let content = match content {
                ToolContent::Text(text) => {
                    add(part_bytes, FitPart::Text(text));
                    FitToolContent::Text("")
                }
                ToolContent::Json(value) => FitToolContent::Json(value),
                ToolContent::Mixed(values) => FitToolContent::Mixed(
                    values
                        .iter()
                        .filter(|value| match value {
                            ContentValue::Text(text) => {
                                add(part_bytes, FitPart::Text(text));
                                false
                            }
                            ContentValue::File(file) => {
                                add(part_bytes, FitPart::File(&file.media_type));
                                false
                            }
                            ContentValue::Json(_) => true,
                        })
                        .collect(),
                ),
                ToolContent::Denied { reason } => FitToolContent::Denied { reason },
            };
            Self {
                tool_call_id,
                content,
                is_error: *is_error,
                metadata,
            }
        }
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
    policy::input_token_budget(binding, Some(output_reserve))
        .unwrap_or(0)
        .max(1)
}

/// The built-in summarizer prompt; `updates_summary` selects the variant that
/// merges an earlier internal summary instead of starting fresh.
pub(crate) fn default_compaction_prompt(updates_summary: bool) -> String {
    let intro = if updates_summary {
        COMPACTION_UPDATE_PROMPT_INTRO
    } else {
        COMPACTION_PROMPT_INTRO
    };
    format!("{intro}\n\n{COMPACTION_PROMPT_TEMPLATE}")
}

/// Expands the compaction agent's document body, whose `{{default_prompt}}`
/// and `{{focus}}` placeholders the config loader has already validated.
fn compaction_instruction(template: &str, focus: Option<&str>, updates_summary: bool) -> String {
    render_internal_prompt(
        template,
        InternalPromptValues {
            default_prompt: &default_compaction_prompt(updates_summary),
            focus,
        },
    )
}

fn pruned_compaction_history(
    events: &[Arc<StoredEvent>],
    store: &super::artifacts::ArtifactRouter,
    session: SessionId,
    binding: &cookie_agent_protocol::FrozenModelBinding,
    composed_prompt: &str,
    recent_from_seq: Option<u64>,
) -> Result<Vec<oven_sdk::HistoryTurn>, EngineError> {
    // Emitted messages lose their tool ownership after assembly. Remove them in a private
    // snapshot first, then prune only results in the active checkpoint-aware history.
    let mut events = events.to_vec();
    for event in &mut events {
        let carries_result = matches!(
            &event.payload,
            Event::ToolCallTerminated { termination } if termination.result.is_some()
        );
        // Copy only the records this private snapshot changes.
        if carries_result
            && let Event::ToolCallTerminated { termination } = &mut Arc::make_mut(event).payload
            && let Some(result) = &mut termination.result
        {
            result.additional_messages.clear();
            result.attachments.clear();
        }
    }
    let mut history = model_history::compaction_prefix_history(
        &events,
        store,
        binding,
        composed_prompt,
        recent_from_seq,
    )?;
    let mut retrieval_calls = HashSet::new();
    for turn in &mut history {
        match turn {
            oven_sdk::HistoryTurn::Assistant(turn) => {
                for part in &turn.message.content {
                    if let oven_sdk::AssistantPart::ToolCall(call) = part {
                        if call.name == "read_artifact" {
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
                "tool": "read_artifact",
                "arguments": {"artifact": artifact_id}
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
    instruction: &str,
) -> Vec<oven_sdk::HistoryTurn> {
    // `history` is the span the checkpoint replaces. It starts with the
    // session's own system prompt and pinned context, so the summarizer
    // request is a cache-friendly prefix of the latest conversation request;
    // the compaction agent's prompt rides along in the trailing instruction
    // instead of replacing the session system prompt.
    history.push(oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(
        vec![oven_sdk::InputPart::Text(oven_sdk::TextPart::new(
            instruction.to_owned(),
        ))],
    )));
    history
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
