use std::collections::{HashMap, HashSet};

use cookie_agent_protocol::{
    ContextCheckpoint, ContextCheckpointBoundaries, ContextCheckpointBudgets,
    ContextCheckpointCommit, ContextRehydratedFile, InternalAgentKind, InternalSummaryCheckpoint,
    PersistedAssistantPart, RunId, SessionId, SessionStatus, Sha256Digest, StoredEvent,
    SummaryByteLimit, ToolCallId,
};
use oven_sdk::{ModelError, ToolDefinition};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::titles::active_fallback_index;
use super::{
    Engine, EngineError, Event, FrozenInternalAgentPolicy, InternalAgentExecution,
    InternalAgentHistoryInput, SessionCommand,
    helpers::{safe_display, truncate_utf8},
};
use crate::{
    events::OutputHub,
    model_history::{self, assemble_model_context},
    policy::FrozenRunPolicy,
    tool_api::{ProgressSink, ToolCall, ToolExecutionContext},
};

pub(super) const COMPACTION_INSTRUCTION: &str = "Create a detailed technical summary of the conversation so work can continue without the earlier context. Include: the goal/objective; decisions and their rationale; files changed and current code state; commands run and their outcomes; errors encountered and fixes applied; and the pending next step. Preserve exact identifiers, paths, constraints, and unresolved questions. Return summary text only and do not call tools.";
pub(super) const TOOL_OUTPUT_ELISION_MIN_BYTES: usize = 8 * 1024;
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
    pub(super) focus: Option<&'a str>,
    pub(super) actor_direct: bool,
}

struct RehydrationInput<'a> {
    session: SessionId,
    run: RunId,
    owner_policy: &'a FrozenRunPolicy,
    cancellation: &'a CancellationToken,
    events: &'a [StoredEvent],
}

impl Engine {
    pub async fn compact_session(
        &self,
        session: SessionId,
        focus: Option<&str>,
    ) -> Result<bool, EngineError> {
        let focus = focus.map(str::to_owned);
        self.request(session, |reply| SessionCommand::Compact { focus, reply })
            .await
    }

    pub(super) async fn compact_session_direct(
        &self,
        session: SessionId,
        focus: Option<&str>,
    ) -> Result<bool, EngineError> {
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
        let compacted = self
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
                focus,
                actor_direct: false,
            })
            .await?;
        Ok(latest_checkpoint_seq(&compacted) > before)
    }

    pub(super) async fn maybe_compact_context(
        &self,
        input: CompactionInput<'_>,
    ) -> Result<Vec<StoredEvent>, EngineError> {
        let Some(context_limit) = input.binding.descriptor.capabilities.limits.context else {
            return Ok(input.events);
        };
        let config = &self.inner.config.runtime.context_compaction;
        let trigger_tokens = effective_compaction_limit(context_limit, config.buffer_tokens);
        let auto_disabled = self
            .inner
            .compaction_auto_disabled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&input.session);
        if !compaction_gate(
            input.force,
            config.auto_compaction,
            auto_disabled,
            trigger_tokens,
        ) {
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
            let pending_postcheck = if usage_seq > last_checkpoint_seq {
                self.inner
                    .compaction_postcheck_pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&input.session)
            } else {
                false
            };
            if observed_tokens < trigger_tokens {
                return Ok(input.events);
            }
            if should_latch_auto_compaction(
                pending_postcheck,
                usage_seq,
                last_checkpoint_seq,
                observed_tokens,
                trigger_tokens,
            ) {
                self.inner
                    .compaction_auto_disabled
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(input.session);
                self.append_compaction_event(
                    input.session,
                    Some(input.run),
                    Event::ContextCompactionAutoDisabled {
                        observed_tokens,
                        trigger_tokens,
                    },
                    input.actor_direct,
                )
                .await?;
                return Ok(self.inner.store.get(input.session)?.log.events());
            }
        }

        let mut events = self
            .stage_tool_output_elision(input.session, input.events.clone(), input.actor_direct)
            .await?;
        let composed_prompt = self.run_agent_prompt(input.session, input.run)?;
        let context = assemble_model_context(
            &events,
            &self.inner.artifacts,
            input.binding,
            &composed_prompt,
        )?;
        let input_tokens_before = estimated_request_tokens(&context.history, input.tools)?;
        if !input.force && input_tokens_before < trigger_tokens {
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
        let (history, instruction) = compaction_history(context.history, input.focus);
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
        let input_tokens_after = estimated_tokens_for_bytes(
            model_history::framed_compaction_summary(checkpoint.summary()).len(),
        );
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
        self.inner
            .context_token_estimators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(input.session)
            .or_default()
            .record_compaction(input_tokens_after);
        self.inner
            .compaction_postcheck_pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(input.session);
        let files = self
            .rehydrated_files(RehydrationInput {
                session: input.session,
                run: input.run,
                owner_policy: input.owner_policy,
                cancellation: input.cancellation,
                events: &events,
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
                )
                .await;
            let Ok(prepared) = prepared.prepared else {
                continue;
            };
            let permission = self.inner.permissions.decide_operation(
                &input.owner_policy.agent,
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
        self.rehydrated_files(RehydrationInput {
            session,
            run,
            owner_policy,
            cancellation: &CancellationToken::new(),
            events,
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

pub(super) fn effective_compaction_limit(context_limit: u64, buffer_tokens: u64) -> u64 {
    context_limit.saturating_sub(buffer_tokens)
}

fn compaction_gate(force: bool, auto: bool, auto_disabled: bool, trigger_tokens: u64) -> bool {
    force || (auto && !auto_disabled && trigger_tokens > 0)
}

fn latest_real_usage(events: &[StoredEvent]) -> Option<(u64, u64)> {
    events.iter().rev().find_map(|event| match &event.payload {
        Event::ModelTurnCommitted { turn, .. } => {
            let input = turn.usage.input_tokens;
            let output = turn.usage.output_tokens;
            (input.is_some() || output.is_some()).then(|| {
                (
                    event.seq,
                    input
                        .unwrap_or_default()
                        .saturating_add(output.unwrap_or_default()),
                )
            })
        }
        _ => None,
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

fn estimated_request_tokens(
    history: &[oven_sdk::HistoryTurn],
    tools: &[ToolDefinition],
) -> Result<u64, EngineError> {
    let bytes = serde_json::to_vec(&(history, tools))
        .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?
        .len();
    Ok(estimated_tokens_for_bytes(bytes))
}

fn estimated_tokens_for_bytes(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
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

fn should_latch_auto_compaction(
    pending_postcheck: bool,
    usage_seq: u64,
    checkpoint_seq: u64,
    observed_tokens: u64,
    trigger_tokens: u64,
) -> bool {
    pending_postcheck
        && usage_seq > checkpoint_seq
        && observed_tokens >= trigger_tokens
        && trigger_tokens > 0
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
        PersistedAssistantPart, PersistedModelTurn, PersistedToolResult as ToolResult, RunId,
        SessionId, StoredEvent, ToolCallId, ToolCallPresentation, ToolCallStart,
        ToolCallTermination, ToolTerminationOutcome,
    };
    use oven_sdk::{JsonSchema, Request as ModelRequest, ToolDefinition};

    use super::{
        COMPACTION_INSTRUCTION, TOOL_OUTPUT_ELISION_MIN_BYTES, compaction_gate, compaction_history,
        compaction_instruction, effective_compaction_limit, recent_read_candidates,
        should_elide_tool_output, should_latch_auto_compaction,
    };
    use crate::{
        model_history::{assemble_model_context, wire_model},
        runtime::{
            Event,
            artifacts::ArtifactStore,
            helpers::{safe_code, safe_display},
            tool_execution::fallback_operation_fingerprint,
        },
        tool_api::ToolCall,
    };

    #[test]
    fn compaction_buffer_math_is_saturating() {
        assert_eq!(effective_compaction_limit(200_000, 33_000), 167_000);
        assert_eq!(effective_compaction_limit(8_192, 33_000), 0);
    }

    #[test]
    fn auto_off_blocks_automatic_compaction_but_not_manual_force() {
        assert!(!compaction_gate(false, false, false, 100));
        assert!(compaction_gate(true, false, true, 0));
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
            event_schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
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
    fn anti_thrash_latches_only_for_the_first_over_limit_postcheck() {
        assert!(should_latch_auto_compaction(true, 11, 10, 90, 80));
        assert!(!should_latch_auto_compaction(false, 11, 10, 90, 80));
        assert!(!should_latch_auto_compaction(true, 11, 10, 79, 80));
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
                event_schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
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
                event_schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
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
                        operation_fingerprint: fallback_operation_fingerprint(&ToolCall {
                            id: call_id,
                            name: name.into(),
                            arguments: serde_json::json!({"filePath": path}),
                        }),
                    },
                },
            });
            events.push(StoredEvent {
                event_schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
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
