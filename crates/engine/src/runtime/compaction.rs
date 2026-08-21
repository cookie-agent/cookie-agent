use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use cookie_agent_config::ContextCompactionTrigger;
use cookie_agent_protocol::{
    ContextCheckpoint, ContextCheckpointBoundaries, ContextCheckpointBudgets,
    ContextCheckpointCommit, ContextRehydratedFile, ExtensionSessionBeforeCompactParams,
    InternalAgentKind, InternalSummaryCheckpoint, PersistedAssistantPart, PluginDiagnosticKind,
    RunId, SessionCompactResult, SessionId, SessionStatus, Sha256Digest, StoredEvent,
    SummaryByteLimit, ToolCallId,
};
use oven_sdk::{
    CompactionCapability, CompactionRequest, ModelError, Request as ModelRequest, ToolDefinition,
};
use oven_sdk_azure::{AzureOpenAiCompactionOptions, AzureOpenAiCompactionRequestExt as _};
use oven_sdk_openai::{OpenAiResponsesCompactionOptions, OpenAiResponsesCompactionRequestExt as _};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::titles::active_fallback_index;
use super::{
    Engine, EngineError, Event, FrozenInternalAgentPolicy, InternalAgentExecution,
    InternalAgentHistoryInput, SessionCommand,
    helpers::{safe_display, truncate_utf8},
    internal_agents::{internal_agent_input_fits, internal_agent_output_limit},
};
use crate::{
    events::OutputHub,
    model_bridge::AbortBridge,
    model_history::{self, assemble_model_context},
    policy::{self, FrozenRunPolicy},
    tool_api::{ProgressSink, ToolCall, ToolExecutionContext, TurnAgentContext},
};

pub(crate) const COMPACTION_INSTRUCTION: &str = "Create a detailed technical summary of the conversation so work can continue without the earlier context. Include: the goal/objective; decisions and their rationale; files changed and current code state; commands run and their outcomes; errors encountered and fixes applied; and the pending next step. Preserve exact identifiers, paths, constraints, and unresolved questions. Return summary text only and do not call tools.";
pub(super) const TOOL_OUTPUT_ELISION_MIN_BYTES: usize = 8 * 1024;
// Frozen policies normally provide this value. Keep substantial headroom when replaying an
// unavailable or legacy policy whose output limit is zero.
pub(super) const DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS: u64 = 20_000;
pub(super) const REHYDRATION_MAX_FILES: usize = 5;
pub(super) const REHYDRATION_MAX_FILE_BYTES: usize = 32 * 1024;
pub(super) const REHYDRATION_MAX_TOTAL_BYTES: usize = 128 * 1024;

pub(super) struct CompactionInput<'a> {
    pub(super) session: SessionId,
    pub(super) run: RunId,
    pub(super) cancellation: &'a CancellationToken,
    pub(super) binding: &'a cookie_agent_protocol::FrozenModelBinding,
    pub(super) owner_policy: &'a FrozenRunPolicy,
    pub(super) internal_policy: &'a FrozenInternalAgentPolicy,
    pub(super) tools: &'a [ToolDefinition],
    pub(super) events: Vec<StoredEvent>,
    pub(super) force: bool,
    pub(super) overflow_recovery: bool,
    pub(super) focus: Option<&'a str>,
    pub(super) actor_direct: bool,
}

struct RehydrationInput<'a> {
    session: SessionId,
    run: RunId,
    owner_policy: &'a FrozenRunPolicy,
    cancellation: &'a CancellationToken,
    events: &'a [StoredEvent],
    turn_context: Arc<TurnAgentContext>,
}

impl Engine {
    pub async fn compact_session(
        &self,
        session: SessionId,
        focus: Option<&str>,
    ) -> Result<bool, EngineError> {
        self.compact_session_result(session, focus)
            .await
            .map(|result| result.compacted)
    }

    pub async fn compact_session_result(
        &self,
        session: SessionId,
        focus: Option<&str>,
    ) -> Result<SessionCompactResult, EngineError> {
        let focus = focus.map(str::to_owned);
        self.request(session, |reply| SessionCommand::Compact { focus, reply })
            .await
    }

    pub(super) async fn compact_session_direct(
        &self,
        session: SessionId,
        focus: Option<&str>,
    ) -> Result<SessionCompactResult, EngineError> {
        let projection = self.inner.store.get(session)?;
        if projection.status == SessionStatus::Running {
            return Err(EngineError::SessionRunning(session));
        }
        let events = projection.log.events();
        let run = events
            .iter()
            .rev()
            .find_map(|event| {
                matches!(event.payload, Event::RunStarted { .. }).then_some(event.run_id)
            })
            .flatten()
            .ok_or(EngineError::NoRunnableModel)?;
        let policy = self.historical_title_policy(&events, run)?;
        let binding = active_compaction_binding(&policy, &events, run)?;
        let internal_policy = self.internal_agent_policy(
            InternalAgentKind::ContextCompaction,
            &policy,
            Some(binding),
        )?;
        let tools = self.tool_definitions(session, &policy)?;
        let before = latest_checkpoint_seq(&events);
        let compacted = match self
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
            })
            .await
        {
            Ok(compacted) => compacted,
            Err(EngineError::CompactionCancelled(reason)) => {
                return Ok(SessionCompactResult {
                    compacted: false,
                    cancellation_reason: Some(reason),
                });
            }
            Err(error) => return Err(error),
        };
        Ok(SessionCompactResult {
            compacted: latest_checkpoint_seq(&compacted) > before,
            cancellation_reason: None,
        })
    }

    pub(super) async fn maybe_compact_context(
        &self,
        input: CompactionInput<'_>,
    ) -> Result<Vec<StoredEvent>, EngineError> {
        let Some(context_limit) = input.binding.descriptor.capabilities.limits.context else {
            return Ok(input.events);
        };
        let config = &self.inner.config.runtime.context_compaction;
        let trigger_tokens = resolve_compaction_trigger(context_limit, &config.trigger);
        if !compaction_gate(input.force, config.auto_compaction, trigger_tokens) {
            return Ok(input.events);
        }
        if !input.force {
            let Some((usage_seq, observed_tokens)) = latest_real_usage(&input.events) else {
                return Ok(input.events);
            };
            let last_checkpoint_seq = latest_checkpoint_seq(&input.events);
            if usage_seq < last_checkpoint_seq {
                return Ok(input.events);
            }
            if !usage_reaches_compaction_trigger(observed_tokens, trigger_tokens) {
                return Ok(input.events);
            }
        }

        let requested_input_through_seq = input.events.last().map_or(0, |event| event.seq);
        let current_events = self.inner.store.get(input.session)?.log.events();
        if checkpoint_covers_input(&current_events, requested_input_through_seq) {
            return Ok(current_events);
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
        let mut events = input.events.clone();
        let mut context = assemble_model_context(
            &events,
            &self.inner.artifacts,
            input.binding,
            &composed_prompt,
        )?;
        let raw_fits = if let Some(raw_fits) = raw_fit_from_real_usage(
            input.overflow_recovery,
            latest_real_usage(&events).map(|(_, observed_tokens)| observed_tokens),
            |tokens| compaction_input_fits(input.binding, input.internal_policy, tokens),
        ) {
            raw_fits
        } else {
            let raw_fit_tokens = if input.binding.descriptor.capabilities.compaction
                == CompactionCapability::Native
            {
                self.estimated_request_tokens(input.session, &context.history, input.tools)?
            } else {
                let (history, _) =
                    compaction_history(context.history.clone(), compaction_focus.as_deref());
                self.estimated_request_tokens(input.session, &history, input.tools)?
            };
            compaction_input_fits(input.binding, input.internal_policy, raw_fit_tokens)
        };
        let context_tokens_before = if raw_fits {
            self.estimated_request_tokens(input.session, &context.history, input.tools)?
        } else {
            events = self
                .stage_tool_output_elision(input.session, events, input.actor_direct)
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
            return Ok(events);
        }

        // Rehydration runs outside a normal turn, so it uses the owner and binding that triggered
        // this checkpoint.
        let rehydration_turn_context = Arc::new(TurnAgentContext {
            agent: input.owner_policy.agent.agent.clone(),
            capabilities: input
                .owner_policy
                .model_capabilities(input.binding)
                .ok_or(EngineError::NoRunnableModel)?,
        });

        let input_through_seq = events.last().map_or(0, |event| event.seq);
        let previous = latest_checkpoint_seq(&events);
        let source_from_seq = if previous == 0 {
            1
        } else {
            previous.saturating_add(1)
        };
        let boundaries = ContextCheckpointBoundaries {
            source_from_seq,
            source_through_seq: input_through_seq,
            input_through_seq,
            prior_checkpoint_seq: (previous > 0).then_some(previous),
        };
        let summary_limit = SummaryByteLimit::new(config.max_summary_bytes as u64)
            .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?;
        let native_checkpoint = if input.binding.descriptor.capabilities.compaction
            == CompactionCapability::Native
        {
            if let Ok(model) = policy::resolve_model(input.binding, &input.owner_policy.runtime) {
                let mut request =
                    ModelRequest::new(context.history.clone()).with_tools(input.tools.to_vec());
                if let Some(native_context) = context.native_context.clone() {
                    request = request.with_native_context(native_context);
                }
                let request = model.prepare_request_with_cache_strategy(
                    request,
                    input.owner_policy.prompt_cache_strategy.as_ref(),
                );
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
                )
                .await?;
                return self
                    .finalize_context_checkpoint(
                        input,
                        events,
                        input_tokens_after,
                        rehydration_turn_context,
                    )
                    .await;
            }
        }

        let (history, instruction) =
            compaction_history(context.history, compaction_focus.as_deref());
        let input_tokens_before =
            self.estimated_request_tokens(input.session, &history, input.tools)?;
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
        let retained_history = model_history::checkpoint_retained_history(
            &checkpoint_prefix,
            &events,
            Some(checkpoint.summary()),
        );
        let input_tokens_after = estimated_request_tokens(&retained_history, input.tools)?;
        let budgets = ContextCheckpointBudgets {
            context_limit_tokens: context_limit,
            trigger_tokens: trigger_tokens.max(1).min(context_limit),
            input_tokens_before,
            input_tokens_after,
            max_summary_bytes: summary_limit,
        };
        let commit = ContextCheckpointCommit {
            checkpoint: ContextCheckpoint::InternalSummary { checkpoint },
            boundaries,
            budgets,
        };
        if commit.validate().is_err() {
            return Ok(events);
        }
        self.append_compaction_event(
            input.session,
            Some(input.run),
            Event::ContextCheckpointCommitted { commit },
            input.actor_direct,
        )
        .await?;
        self.finalize_context_checkpoint(
            input,
            events,
            input_tokens_after,
            rehydration_turn_context,
        )
        .await
    }

    async fn finalize_context_checkpoint(
        &self,
        input: CompactionInput<'_>,
        mut events: Vec<StoredEvent>,
        input_tokens_after: u64,
        turn_context: Arc<TurnAgentContext>,
    ) -> Result<Vec<StoredEvent>, EngineError> {
        self.inner
            .context_token_estimators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(input.session)
            .or_default()
            .record_compaction(input_tokens_after);
        let files = self
            .rehydrated_files(RehydrationInput {
                session: input.session,
                run: input.run,
                owner_policy: input.owner_policy,
                cancellation: input.cancellation,
                events: &events,
                turn_context,
            })
            .await;
        if !files.is_empty() {
            self.append_compaction_event(
                input.session,
                Some(input.run),
                Event::ContextRehydrated { files },
                input.actor_direct,
            )
            .await?;
        }
        events = self.inner.store.get(input.session)?.log.events();
        Ok(events)
    }

    async fn stage_tool_output_elision(
        &self,
        session: SessionId,
        events: Vec<StoredEvent>,
        actor_direct: bool,
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
                result.output.len(),
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
        let bytes = serialized_request_bytes(history, tools)?;
        let calibrated = self
            .inner
            .context_token_estimators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session)
            .copied()
            .and_then(|estimator| estimator.estimated_context_tokens(bytes));
        Ok(calibrated.unwrap_or_else(|| estimated_tokens_for_bytes(bytes)))
    }

    async fn rehydrated_files(&self, input: RehydrationInput<'_>) -> Vec<ContextRehydratedFile> {
        let mut files = Vec::new();
        let mut total = 0_usize;
        for display_path in recent_read_candidates(input.events) {
            if total >= REHYDRATION_MAX_TOTAL_BYTES {
                break;
            }
            let call_id = ToolCallId::new_v7();
            let prepared = self
                .prepare_tool_call(
                    input.session,
                    input.run,
                    ToolCall {
                        id: call_id,
                        name: "read".into(),
                        arguments: serde_json::json!({
                            "filePath": display_path,
                            "limit": null,
                            "offset": null
                        }),
                    },
                    input.owner_policy,
                    Arc::clone(&input.turn_context),
                )
                .await;
            let Ok(prepared) = prepared.prepared else {
                continue;
            };
            let Ok(session) = self.inner.store.get(input.session) else {
                continue;
            };
            let permission_overlay = session.permission_overlay;
            let grants = self.skill_grants_for_session(input.session);
            let permission = self.inner.permissions.decide_operation_with_grants(
                &input.owner_policy.agent,
                Some(&permission_overlay),
                grants.as_ref(),
                &prepared.operation,
                &prepared.policy_labels,
                self.inner.store.cwd(),
            );
            if permission.effect != cookie_agent_protocol::PermissionEffect::Allow {
                continue;
            }
            let Some(executor) = prepared.executor.lock().await.take() else {
                continue;
            };
            let (progress_tx, _progress_rx) = mpsc::channel(1);
            let result = executor
                .execute(ToolExecutionContext {
                    session: input.session,
                    run: input.run,
                    progress: ProgressSink::new(progress_tx, OutputHub::new(call_id, 1024)),
                    cancellation: input.cancellation.child_token(),
                    stdin: None,
                    turn_context: Arc::clone(&input.turn_context),
                    artifacts: self.inner.artifacts.clone(),
                })
                .await;
            let Ok(result) = result else {
                continue;
            };
            let remaining = REHYDRATION_MAX_TOTAL_BYTES.saturating_sub(total);
            let content = truncate_utf8(&result.output, REHYDRATION_MAX_FILE_BYTES.min(remaining));
            if content.is_empty() {
                continue;
            }
            total = total.saturating_add(content.len());
            files.push(ContextRehydratedFile {
                path: safe_display(&display_path),
                byte_length: content.len() as u64,
                sha256: Sha256Digest::of_bytes(content.as_bytes()),
                content,
            });
        }
        files
    }

    #[cfg(test)]
    pub(crate) async fn rehydrated_files_for_test(
        &self,
        session: SessionId,
        run: RunId,
        owner_policy: &FrozenRunPolicy,
        events: &[StoredEvent],
    ) -> Vec<ContextRehydratedFile> {
        let binding = owner_policy
            .selected_suffix
            .first()
            .expect("test owner policy has a model binding");
        self.rehydrated_files(RehydrationInput {
            session,
            run,
            owner_policy,
            cancellation: &CancellationToken::new(),
            events,
            turn_context: Arc::new(TurnAgentContext {
                agent: owner_policy.agent.agent.clone(),
                capabilities: owner_policy
                    .model_capabilities(binding)
                    .expect("test binding has published capabilities"),
            }),
        })
        .await
    }

    async fn append_compaction_event(
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

fn latest_real_usage(events: &[StoredEvent]) -> Option<(u64, u64)> {
    let recorded = events.iter().rev().find_map(|event| match &event.payload {
        Event::ModelUsageRecorded { usage, .. } => usage_total(event.seq, usage),
        _ => None,
    });
    recorded.or_else(|| {
        events.iter().rev().find_map(|event| match &event.payload {
            Event::ModelTurnCommitted { turn, .. } => usage_total(event.seq, &turn.usage),
            _ => None,
        })
    })
}

fn usage_total(seq: u64, usage: &cookie_agent_protocol::Usage) -> Option<(u64, u64)> {
    let input = usage.input_tokens;
    let output = usage.output_tokens;
    (input.is_some() || output.is_some()).then(|| {
        (
            seq,
            input
                .unwrap_or_default()
                .saturating_add(output.unwrap_or_default()),
        )
    })
}

pub(super) fn latest_checkpoint_seq(events: &[StoredEvent]) -> u64 {
    events
        .iter()
        .rev()
        .find_map(|event| {
            matches!(event.payload, Event::ContextCheckpointCommitted { .. }).then_some(event.seq)
        })
        .unwrap_or(0)
}

fn checkpoint_covers_input(events: &[StoredEvent], input_through_seq: u64) -> bool {
    events.iter().rev().any(|event| {
        matches!(
            &event.payload,
            Event::ContextCheckpointCommitted { commit }
                if commit.boundaries.input_through_seq >= input_through_seq
        )
    })
}

fn serialized_request_bytes(
    history: &[oven_sdk::HistoryTurn],
    tools: &[ToolDefinition],
) -> Result<usize, EngineError> {
    Ok(serde_json::to_vec(&(history, tools))
        .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?
        .len())
}

fn estimated_request_tokens(
    history: &[oven_sdk::HistoryTurn],
    tools: &[ToolDefinition],
) -> Result<u64, EngineError> {
    Ok(estimated_tokens_for_bytes(serialized_request_bytes(
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
) -> (Vec<oven_sdk::HistoryTurn>, String) {
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

fn recent_read_candidates(events: &[StoredEvent]) -> Vec<String> {
    let turns = events
        .iter()
        .filter_map(|event| match &event.payload {
            Event::ModelTurnCommitted {
                model_turn_seq,
                turn,
                ..
            } => Some((*model_turn_seq, (event.run_id, turn))),
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    let starts = events
        .iter()
        .filter_map(|event| match &event.payload {
            Event::ToolCallStarted { start } => Some((
                start.tool_call_id,
                (
                    event.run_id,
                    start.owner.model_turn_seq,
                    start.owner.content_index,
                ),
            )),
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for event in events.iter().rev() {
        let Event::ToolCallTerminated { termination } = &event.payload else {
            continue;
        };
        if termination.result.is_none() {
            continue;
        }
        let Some((start_run, model_turn_seq, content_index)) =
            starts.get(&termination.tool_call_id)
        else {
            continue;
        };
        if event.run_id != *start_run {
            continue;
        }
        let Some((turn_run, turn)) = turns.get(model_turn_seq) else {
            continue;
        };
        if turn_run != start_run {
            continue;
        }
        let Some(PersistedAssistantPart::ToolCall { name, input, .. }) =
            turn.content.get(*content_index as usize)
        else {
            continue;
        };
        if name.as_str() != "read" {
            continue;
        }
        let Some(path) = input.get("filePath").and_then(Value::as_str) else {
            continue;
        };
        if seen.insert(path.to_owned()) {
            paths.push(path.to_owned());
        }
        if paths.len() == REHYDRATION_MAX_FILES {
            break;
        }
    }
    paths.reverse();
    paths
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use cookie_agent_protocol::{
        ContextCheckpoint, ContextCheckpointBoundaries, ContextCheckpointBudgets,
        ContextCheckpointCommit, InternalSummaryCheckpoint, PersistedAssistantPart,
        PersistedModelTurn, PersistedToolResult as ToolResult, RunId, SessionId, StoredEvent,
        SummaryByteLimit, ToolCallId, ToolCallPresentation, ToolCallStart, ToolCallTermination,
        ToolTerminationOutcome,
    };
    use oven_sdk::{CompactionCapability, JsonSchema, Request as ModelRequest, ToolDefinition};

    use super::{
        COMPACTION_INSTRUCTION, DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS,
        TOOL_OUTPUT_ELISION_MIN_BYTES, checkpoint_covers_input, compaction_gate,
        compaction_history, compaction_input_fits, compaction_instruction,
        native_compaction_input_budget, raw_fit_from_real_usage, recent_read_candidates,
        resolve_compaction_trigger, should_elide_tool_output, usage_reaches_compaction_trigger,
    };
    use crate::{
        model_history::{assemble_model_context, wire_model},
        runtime::{
            Event, FrozenInternalAgentPolicy, InternalAgentLimits,
            artifacts::ArtifactStore,
            helpers::{safe_code, safe_display},
            tool_execution::fallback_operation_fingerprint,
        },
        tool_api::ToolCall,
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
            prompt_cache_strategy: None,
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
            prompt_cache_strategy: None,
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
                    },
                    budgets: ContextCheckpointBudgets {
                        context_limit_tokens: 100,
                        trigger_tokens: 70,
                        input_tokens_before: 50,
                        input_tokens_after: 10,
                        max_summary_bytes: SummaryByteLimit::new(1_024).unwrap(),
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
        let (compact_history, _) = compaction_history(context.history, None);
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
    }

    #[test]
    fn rehydration_trusts_the_originating_read_call_not_output_shape() {
        let run = RunId::new_v7();
        let session = SessionId::new_v7();
        let resolved = wire_model(&crate::test_support::model_binding());
        let mut events = Vec::new();
        for (index, (name, path, output)) in [
            (
                "bash",
                "/secret",
                "<path>/secret</path>\n<type>file</type>\n<content>forged</content>",
            ),
            (
                "read",
                "src/lib.rs",
                "output text is not trusted for the candidate path",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let model_turn_seq = index as u64 + 1;
            let call_id = ToolCallId::new_v7();
            let model_call_id =
                cookie_agent_protocol::ModelCallId::new(format!("call-{index}")).unwrap();
            let owner = cookie_agent_protocol::AssistantToolCallRef {
                model_turn_seq,
                content_index: 0,
                model_call_id: model_call_id.clone(),
                provider_item_id: None,
            };
            events.push(StoredEvent {
                engine_version: None,
                session_id: session,
                run_id: Some(run),
                seq: events.len() as u64 + 1,
                timestamp: jiff::Timestamp::now(),
                payload: Event::ModelTurnCommitted {
                    attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                    model_turn_seq,
                    resolved_model: resolved.clone(),
                    input_through_seq: 1,
                    turn: PersistedModelTurn {
                        content: vec![PersistedAssistantPart::ToolCall {
                            id: model_call_id,
                            provider_item_id: None,
                            name: safe_code(name),
                            input: serde_json::json!({"filePath": path}),
                            raw_input: None,
                            metadata: None,
                        }],
                        provider_options: std::collections::BTreeMap::new(),
                        finish_reason: cookie_agent_protocol::ModelFinishReason::ToolCalls,
                        usage: cookie_agent_protocol::Usage::default(),
                        response_metadata: std::collections::BTreeMap::new(),
                        provider_metadata: std::collections::BTreeMap::new(),
                        native_replay: None,
                    },
                    warnings: Vec::new(),
                },
            });
            events.push(StoredEvent {
                engine_version: None,
                session_id: session,
                run_id: Some(run),
                seq: events.len() as u64 + 1,
                timestamp: jiff::Timestamp::now(),
                payload: Event::ToolCallStarted {
                    start: ToolCallStart {
                        tool_call_id: call_id,
                        owner: owner.clone(),
                        presentation: ToolCallPresentation {
                            title: safe_display(name),
                            primary_argument: None,
                        },
                        operation_fingerprint: fallback_operation_fingerprint(
                            &ToolCall {
                                id: call_id,
                                name: name.into(),
                                arguments: serde_json::json!({"filePath": path}),
                            },
                            Some("read"),
                        ),
                    },
                },
            });
            events.push(StoredEvent {
                engine_version: None,
                session_id: session,
                run_id: Some(run),
                seq: events.len() as u64 + 1,
                timestamp: jiff::Timestamp::now(),
                payload: Event::ToolCallTerminated {
                    termination: ToolCallTermination {
                        tool_call_id: call_id,
                        owner,
                        outcome: ToolTerminationOutcome::Completed,
                        result: Some(ToolResult {
                            title: safe_display(name),
                            output: output.into(),
                            metadata: serde_json::json!({}),
                            truncation: None,
                            attachments: Vec::new(),
                        }),
                        error: None,
                    },
                },
            });
        }
        assert_eq!(recent_read_candidates(&events), vec!["src/lib.rs"]);
    }
}
