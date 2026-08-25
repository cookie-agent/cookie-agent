//! Role-safe Oven history assembly and durable turn conversion.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use bytes::Bytes;
use cookie_agent_protocol::{
    ApprovalDecisionSource, ArtifactReference, ContextCheckpoint, ContextRehydratedFile,
    DelegatedContextRole, EventPayload, FrozenModelBinding, ModelFinishReason, ModelSelection,
    NativeContextScope, NativeReplayArtifact, PersistedAssistantPart, PersistedContentValue,
    PersistedFilePart, PersistedFileSource, PersistedModelTurn, PersistedToolContent,
    PersistedToolResult, ReplayDecision, ReplayDisposition, ResolvedModelRef, SafeCode,
    SafeErrorMessage, Sha256Digest, StoredEvent, ToolAttachment, ToolCallId,
    ToolTerminationOutcome, Usage,
};
use oven_sdk::{
    AdapterId, AssistantMessage, AssistantPart, CompletedTurn, ContentValue, CustomPart, FilePart,
    FileSource, Finish, FinishReason, HistoryTurn, InputPart, ModelError,
    NativeContextScope as OvenNativeContextScope, NativeContextWindow as OvenNativeContextWindow,
    NativeReplayArtifact as OvenReplayArtifact, ProviderId, ReasoningPart,
    ReplayDecision as OvenReplayDecision, ReplayDisposition as OvenReplayDisposition, ResourceId,
    SourcePart, SystemMessage, SystemPart, TextPart, ToolApprovalPart, ToolCallPart, ToolContent,
    ToolMessage, ToolResultPart, UserMessage,
};
use thiserror::Error;

use crate::ArtifactStore;

pub(crate) const COMPACTION_SUMMARY_PREFIX: &str = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n<summary>\n";
pub(crate) const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>\n\nPlease continue the conversation from where we left off without asking the user any further questions.";

pub(crate) fn framed_compaction_summary(summary: &str) -> String {
    format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}")
}

pub(crate) fn checkpoint_retained_history(
    history: &[HistoryTurn],
    events: &[StoredEvent],
    summary: Option<&str>,
) -> Vec<HistoryTurn> {
    let pinned_skills = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::SkillLoaded { .. }))
        .count();
    let pinned_project_context = usize::from(latest_project_context_event(events).is_some());
    let mut retained = history
        .iter()
        .take(
            1_usize
                .saturating_add(pinned_project_context)
                .saturating_add(pinned_skills),
        )
        .cloned()
        .collect::<Vec<_>>();
    if let Some(summary) = summary {
        retained.push(HistoryTurn::user(user_text(&framed_compaction_summary(
            summary,
        ))));
    }
    retained
}

pub(crate) fn tool_output_elision_marker(
    retained: &ArtifactReference,
    original_bytes: u64,
) -> String {
    format!(
        "[tool output elided; retained at {}; {original_bytes} bytes]",
        retained.uri
    )
}

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("stored model history is corrupt: {0}")]
    Corrupt(String),
    #[error("model history artifact failure: {0}")]
    Artifact(#[from] std::io::Error),
    #[error("model history could not be represented: {0}")]
    Model(Box<ModelError>),
}

impl From<ModelError> for HistoryError {
    fn from(error: ModelError) -> Self {
        Self::Model(Box::new(error))
    }
}

#[must_use]
pub(crate) fn wire_model(binding: &FrozenModelBinding) -> ResolvedModelRef {
    crate::policy::wire_resolved(binding)
}

pub(crate) fn persist_turn(
    turn: CompletedTurn,
    store: &ArtifactStore,
    binding: &FrozenModelBinding,
) -> Result<(PersistedModelTurn, Vec<SafeErrorMessage>), HistoryError> {
    let warnings = turn
        .warnings
        .iter()
        .map(|warning| {
            SafeErrorMessage::new(sanitize_control_free(warning, SafeErrorMessage::MAX_BYTES))
                .map_err(|error| HistoryError::Corrupt(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        PersistedModelTurn {
            content: turn
                .message
                .content
                .into_iter()
                .map(|part| persist_assistant_part(part, store))
                .collect::<Result<_, _>>()?,
            provider_options: turn.message.provider_options,
            finish_reason: persist_finish_reason(turn.finish.finish_reason),
            usage: persist_usage(turn.finish.usage),
            response_metadata: turn.finish.response_metadata,
            provider_metadata: turn.finish.provider_metadata,
            native_replay: turn
                .finish
                .native_replay
                .map(|artifact| persist_replay(artifact, binding))
                .transpose()?,
        },
        warnings,
    ))
}

pub(crate) fn replay_decisions(
    decisions: &[OvenReplayDecision],
    binding: &FrozenModelBinding,
) -> Vec<ReplayDecision> {
    decisions
        .iter()
        .map(|decision| ReplayDecision {
            history_index: decision.history_index as u64,
            disposition: match &decision.disposition {
                OvenReplayDisposition::Replayed => ReplayDisposition::Replayed,
                OvenReplayDisposition::NoArtifact => ReplayDisposition::NoArtifact,
                OvenReplayDisposition::DiscardedForeignAdapter { found, expected } => {
                    ReplayDisposition::DiscardedForeignAdapter {
                        found: exact_adapter_code(found.as_str()),
                        expected: exact_adapter_code(expected.as_str()),
                    }
                }
                OvenReplayDisposition::DiscardedForeignScope { found, expected } => {
                    let found_selection = scope_selection(found);
                    let expected_selection = scope_selection(expected);
                    if found_selection.model == expected_selection.model {
                        ReplayDisposition::DiscardedForeignVariant {
                            found: found_selection.variant,
                            expected: binding.selection.variant.clone(),
                        }
                    } else {
                        ReplayDisposition::DiscardedForeignModelSelection {
                            found: found_selection,
                            expected: binding.selection.clone(),
                        }
                    }
                }
                OvenReplayDisposition::DiscardedInvalidPayload { reason } => {
                    ReplayDisposition::DiscardedInvalidPayload {
                        reason: SafeErrorMessage::new(sanitize_control_free(
                            reason,
                            SafeErrorMessage::MAX_BYTES,
                        ))
                        .expect("sanitized safe replay error"),
                    }
                }
                OvenReplayDisposition::ReconstructedNormalized => {
                    ReplayDisposition::ReconstructedNormalizedHistory
                }
            },
        })
        .collect()
}

pub(crate) fn replay_decisions_with_preflight(
    decisions: &[OvenReplayDecision],
    binding: &FrozenModelBinding,
    preflight: &[ReplayDecision],
) -> Vec<ReplayDecision> {
    let preflight = preflight
        .iter()
        .map(|decision| (decision.history_index, decision.disposition.clone()))
        .collect::<HashMap<_, _>>();
    let mut emitted = HashSet::new();
    let mut merged = Vec::new();
    for decision in replay_decisions(decisions, binding) {
        let Some(disposition) = preflight.get(&decision.history_index) else {
            merged.push(decision);
            continue;
        };
        if emitted.insert(decision.history_index) {
            merged.push(ReplayDecision {
                history_index: decision.history_index,
                disposition: disposition.clone(),
            });
        }
        if !matches!(decision.disposition, ReplayDisposition::NoArtifact) {
            merged.push(decision);
        }
    }
    for (history_index, disposition) in preflight {
        if emitted.insert(history_index) {
            merged.push(ReplayDecision {
                history_index,
                disposition,
            });
            merged.push(ReplayDecision {
                history_index,
                disposition: ReplayDisposition::ReconstructedNormalizedHistory,
            });
        } else if !merged.iter().any(|decision| {
            decision.history_index == history_index
                && matches!(
                    decision.disposition,
                    ReplayDisposition::ReconstructedNormalizedHistory
                )
        }) {
            merged.push(ReplayDecision {
                history_index,
                disposition: ReplayDisposition::ReconstructedNormalizedHistory,
            });
        }
    }
    merged.sort_by_key(|decision| decision.history_index);
    merged
}

#[derive(Clone)]
enum LogicalTurn {
    System(String),
    User(UserMessage),
    SeedAssistant(String),
    Assistant(Box<AssistantRecord>),
    Rehydration(Vec<ContextRehydratedFile>),
}

#[derive(Clone)]
struct AssistantRecord {
    turn: PersistedModelTurn,
    resolved_model: ResolvedModelRef,
    run_id: Option<cookie_agent_protocol::RunId>,
    calls: Vec<CallRecord>,
}

#[derive(Clone)]
struct CallRecord {
    model_call_id: cookie_agent_protocol::ModelCallId,
    engine_call_id: Option<ToolCallId>,
    result: Option<ToolResultPart>,
    in_stream_result: bool,
}

pub(crate) struct ModelContext {
    pub(crate) history: Vec<HistoryTurn>,
    pub(crate) native_context: Option<OvenNativeContextWindow>,
    pub(crate) replay_decisions: Vec<ReplayDecision>,
}

struct AssembledHistory {
    history: Vec<HistoryTurn>,
    replay_decisions: Vec<ReplayDecision>,
}

pub(crate) fn assemble_model_context(
    events: &[StoredEvent],
    store: &ArtifactStore,
    binding: &FrozenModelBinding,
    composed_prompt: &str,
) -> Result<ModelContext, HistoryError> {
    let checkpoint = events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::ContextCheckpointCommitted { commit } => Some(commit),
        _ => None,
    });
    let Some(commit) = checkpoint else {
        let assembled = assemble_history_with_replay(events, store, binding, composed_prompt)?;
        return Ok(ModelContext {
            history: assembled.history,
            native_context: None,
            replay_decisions: assembled.replay_decisions,
        });
    };
    let mut after = events
        .iter()
        .filter(|event| event.seq > commit.boundaries.source_through_seq)
        .cloned()
        .collect::<Vec<_>>();
    let project_context_seq = latest_project_context_event(events)
        .filter(|event| event.seq <= commit.boundaries.source_through_seq)
        .map(|event| event.seq);
    let mut pinned_context = events
        .iter()
        .filter(|event| {
            event.seq <= commit.boundaries.source_through_seq
                && (matches!(event.payload, EventPayload::SkillLoaded { .. })
                    || project_context_seq == Some(event.seq))
        })
        .cloned()
        .collect::<Vec<_>>();
    pinned_context.append(&mut after);
    let after = pinned_context;
    match &commit.checkpoint {
        ContextCheckpoint::InternalSummary { checkpoint } => {
            let mut assembled =
                assemble_history_with_replay(&after, store, binding, composed_prompt)?;
            assembled.history.insert(
                1,
                HistoryTurn::user(user_text(&framed_compaction_summary(checkpoint.summary()))),
            );
            for decision in &mut assembled.replay_decisions {
                if decision.history_index >= 1 {
                    decision.history_index += 1;
                }
            }
            Ok(ModelContext {
                history: assembled.history,
                native_context: None,
                replay_decisions: assembled.replay_decisions,
            })
        }
        ContextCheckpoint::NativeWindow { window } => {
            commit
                .validate_for_binding(binding)
                .map_err(|error| HistoryError::Corrupt(error.to_string()))?;
            let assembled = assemble_history_with_replay(&after, store, binding, composed_prompt)?;
            Ok(ModelContext {
                history: assembled.history,
                native_context: Some(restore_native_context(window)?),
                replay_decisions: assembled.replay_decisions,
            })
        }
    }
}

pub(crate) fn assemble_full_history(
    events: &[StoredEvent],
    store: &ArtifactStore,
    binding: &FrozenModelBinding,
    composed_prompt: &str,
) -> Result<Vec<HistoryTurn>, HistoryError> {
    Ok(assemble_history_with_replay(events, store, binding, composed_prompt)?.history)
}

fn assemble_history_with_replay(
    events: &[StoredEvent],
    store: &ArtifactStore,
    binding: &FrozenModelBinding,
    composed_prompt: &str,
) -> Result<AssembledHistory, HistoryError> {
    let loaded_skills = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::SkillLoaded { rendered_body, .. } => Some(rendered_body.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let loaded_project_context = latest_project_context_event(events).and_then(|event| {
        let EventPayload::ProjectContextLoaded { entries } = &event.payload else {
            return None;
        };
        Some(project_context_turn(entries))
    });
    let mut logical = Vec::<LogicalTurn>::new();
    let mut submitted = HashMap::<u64, String>::new();
    let mut pending_model_calls = HashMap::<
        (
            cookie_agent_protocol::RunId,
            cookie_agent_protocol::ModelCallId,
        ),
        VecDeque<(usize, usize)>,
    >::new();
    let mut engine_calls =
        HashMap::<(cookie_agent_protocol::RunId, ToolCallId), (usize, usize)>::new();
    let elisions = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolOutputElided {
                tool_call_id,
                original_bytes,
                retained,
            } => Some((*tool_call_id, (*original_bytes, retained.clone()))),
            _ => None,
        })
        .collect::<HashMap<_, _>>();

    for envelope in events {
        match &envelope.payload {
            EventPayload::DelegatedContextSeeded { turns, .. } => {
                logical.extend(turns.iter().map(|turn| match turn.role {
                    DelegatedContextRole::User => LogicalTurn::User(user_text(&turn.text)),
                    DelegatedContextRole::Assistant => {
                        LogicalTurn::SeedAssistant(turn.text.clone())
                    }
                }));
            }
            EventPayload::MessageInjected { role, input } => match role {
                cookie_agent_protocol::ExtensionMessageRole::System => {
                    logical.push(LogicalTurn::System(input.clone()));
                }
                cookie_agent_protocol::ExtensionMessageRole::User => {
                    logical.push(LogicalTurn::User(user_text(input)));
                }
                cookie_agent_protocol::ExtensionMessageRole::Assistant => {
                    logical.push(LogicalTurn::SeedAssistant(input.clone()));
                }
                cookie_agent_protocol::ExtensionMessageRole::Tool => {}
            },
            EventPayload::UserInputSubmitted { input, .. } => {
                submitted.insert(envelope.seq, input.clone());
            }
            EventPayload::UserInputApplied { user_input_seq } => {
                if let Some(input) = submitted.remove(user_input_seq) {
                    logical.push(LogicalTurn::User(user_text(&input)));
                }
            }
            EventPayload::ModelTurnCommitted {
                resolved_model,
                turn,
                ..
            } => {
                let logical_index = logical.len();
                let mut calls = Vec::new();
                let in_stream_results = turn
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        PersistedAssistantPart::ToolResult { tool_call_id, .. } => {
                            Some(tool_call_id.as_str())
                        }
                        _ => None,
                    })
                    .collect::<std::collections::HashSet<_>>();
                for part in &turn.content {
                    if let PersistedAssistantPart::ToolCall { id, .. } = part {
                        let call_index = calls.len();
                        let in_stream_result = in_stream_results.contains(id.as_str());
                        calls.push(CallRecord {
                            model_call_id: id.clone(),
                            engine_call_id: None,
                            result: None,
                            in_stream_result,
                        });
                        if !in_stream_result && let Some(run_id) = envelope.run_id {
                            pending_model_calls
                                .entry((run_id, id.clone()))
                                .or_default()
                                .push_back((logical_index, call_index));
                        }
                    }
                }
                logical.push(LogicalTurn::Assistant(Box::new(AssistantRecord {
                    turn: turn.clone(),
                    resolved_model: resolved_model.clone(),
                    run_id: envelope.run_id,
                    calls,
                })));
            }
            EventPayload::ToolCallStarted { start } => {
                let Some(run_id) = envelope.run_id else {
                    continue;
                };
                let occurrence = pending_model_calls
                    .get_mut(&(run_id, start.owner.model_call_id.clone()))
                    .and_then(VecDeque::pop_front);
                if let Some((logical_index, call_index)) = occurrence {
                    let LogicalTurn::Assistant(assistant) = &mut logical[logical_index] else {
                        return Err(HistoryError::Corrupt(
                            "tool call mapped to a non-assistant turn".into(),
                        ));
                    };
                    assistant.calls[call_index].engine_call_id = Some(start.tool_call_id);
                    engine_calls.insert((run_id, start.tool_call_id), (logical_index, call_index));
                }
            }
            EventPayload::ToolCallTerminated { termination }
                if termination.outcome == ToolTerminationOutcome::Completed =>
            {
                if let Some(result) = &termination.result {
                    let result = elisions.get(&termination.tool_call_id).map_or_else(
                        || tool_result_part(result, termination.tool_call_id, store),
                        |(original_bytes, retained)| {
                            Ok(ToolResultPart::new(
                                String::new(),
                                ToolContent::Text(tool_output_elision_marker(
                                    retained,
                                    *original_bytes,
                                )),
                            ))
                        },
                    )?;
                    attach_result(
                        &mut logical,
                        &engine_calls,
                        envelope.run_id,
                        termination.tool_call_id,
                        result,
                    )?;
                }
            }
            EventPayload::ToolCallTerminated { termination } => {
                let message = termination
                    .error
                    .as_ref()
                    .map_or("tool failed", |error| error.message.as_str());
                let (content, metadata) = if let Some(denied) = denied_failure(message) {
                    let visible_reason = denied.feedback.as_ref().map_or_else(
                        || denied.reason.clone(),
                        |feedback| format!("{}: {feedback}", denied.reason),
                    );
                    let mut metadata = BTreeMap::from([
                        (
                            "denial_source".into(),
                            serde_json::Value::String(match denied.source {
                                ApprovalDecisionSource::Policy => "policy".into(),
                                ApprovalDecisionSource::Model => "model".into(),
                                ApprovalDecisionSource::InternalAgent => "internal_agent".into(),
                                ApprovalDecisionSource::PermissionMode => "permission_mode".into(),
                                ApprovalDecisionSource::TreeGrant => "tree_grant".into(),
                                ApprovalDecisionSource::User => "user".into(),
                                ApprovalDecisionSource::DoomLoopGuard => "doom_loop_guard".into(),
                                ApprovalDecisionSource::System => "system".into(),
                            }),
                        ),
                        (
                            "denial_reason".into(),
                            serde_json::Value::String(denied.reason),
                        ),
                    ]);
                    if let Some(feedback) = denied.feedback {
                        metadata.insert("feedback".into(), serde_json::Value::String(feedback));
                    }
                    (
                        ToolContent::Denied {
                            reason: Some(visible_reason),
                        },
                        Some(metadata),
                    )
                } else {
                    (ToolContent::Text(message.to_owned()), None)
                };
                attach_result(
                    &mut logical,
                    &engine_calls,
                    envelope.run_id,
                    termination.tool_call_id,
                    ToolResultPart {
                        tool_call_id: String::new(),
                        content,
                        is_error: true,
                        metadata,
                    },
                )?;
            }
            EventPayload::ContextRehydrated { files } => {
                logical.push(LogicalTurn::Rehydration(files.clone()));
            }
            EventPayload::PluginEventAdded {
                plugin,
                name,
                payload,
            } => {
                logical.push(LogicalTurn::User(user_text(&format!(
                    "<plugin_event>{}</plugin_event>",
                    serde_json::json!({
                        "plugin": plugin,
                        "name": name,
                        "payload": payload,
                    })
                ))));
            }
            EventPayload::DelegateFinished {
                session_id,
                status,
                preview,
                total_lines,
            }
            | EventPayload::DelegateFinishedV2 {
                session_id,
                status,
                preview,
                total_lines,
                ..
            } => {
                logical.push(LogicalTurn::User(user_text(&format!(
                    "<subagent_notification>{}</subagent_notification>",
                    serde_json::json!({
                        "session_id": session_id,
                        "status": format!("{status:?}").to_ascii_lowercase(),
                        "preview": preview,
                        "total_lines": total_lines,
                    })
                ))));
            }
            _ => {}
        }
    }

    let mut history = vec![HistoryTurn::system(SystemMessage::new(vec![
        SystemPart::Text(TextPart::new(composed_prompt)),
    ]))];
    if let Some(project_context) = loaded_project_context {
        history.push(HistoryTurn::user(user_text(&project_context)));
    }
    history.extend(
        loaded_skills
            .iter()
            .map(|body| HistoryTurn::user(user_text(body))),
    );
    let mut replay_decisions = Vec::new();
    for turn in logical {
        match turn {
            LogicalTurn::System(text) => {
                history[0] = match history.remove(0) {
                    HistoryTurn::System(mut message) => {
                        message.content.push(SystemPart::Text(TextPart::new(text)));
                        HistoryTurn::system(message)
                    }
                    _ => unreachable!("assembled history starts with a system message"),
                };
            }
            LogicalTurn::User(user) => history.push(HistoryTurn::user(user)),
            LogicalTurn::SeedAssistant(text) => {
                history.push(HistoryTurn::assistant(CompletedTurn::new(
                    AssistantMessage::new(vec![AssistantPart::Text(TextPart::new(text))]),
                    Finish::new(oven_sdk::Usage::default(), FinishReason::Stop),
                )));
            }
            LogicalTurn::Assistant(mut assistant) => {
                let retained = assistant
                    .calls
                    .iter()
                    .filter(|call| call.in_stream_result || call.result.is_some())
                    .map(|call| call.model_call_id.as_str())
                    .collect::<std::collections::HashSet<_>>();
                let original_call_count = assistant.calls.len();
                assistant.turn.content.retain(|part| match part {
                    PersistedAssistantPart::ToolCall { id, .. } => retained.contains(id.as_str()),
                    PersistedAssistantPart::ToolApproval { tool_call_id, .. } => {
                        retained.contains(tool_call_id.as_str())
                    }
                    _ => true,
                });
                let forced_disposition = if retained.len() != original_call_count
                    && assistant.turn.native_replay.is_some()
                {
                    assistant.turn.native_replay = None;
                    Some(ReplayDisposition::DiscardedInvalidPayload {
                        reason: safe_replay_reason(
                            "native replay was discarded because normalized tool-call history changed",
                        ),
                    })
                } else {
                    None
                };
                let has_content = !assistant.turn.content.is_empty();
                if has_content {
                    let history_index = history.len() as u64;
                    let (restored, disposition) = restore_turn_with_store(
                        &assistant.turn,
                        &assistant.resolved_model,
                        store,
                        binding,
                    )?;
                    if let Some(disposition) = forced_disposition.or(disposition) {
                        replay_decisions.push(ReplayDecision {
                            history_index,
                            disposition,
                        });
                    }
                    history.push(HistoryTurn::assistant(restored));
                }
                let mut results = assistant
                    .calls
                    .into_iter()
                    .filter_map(|call| call.result)
                    .collect::<Vec<_>>();
                if !results.is_empty() {
                    if !has_content {
                        return Err(HistoryError::Corrupt(
                            "tool result has no retained assistant call".into(),
                        ));
                    }
                    results.sort_by_key(|result| {
                        assistant
                            .turn
                            .content
                            .iter()
                            .position(|part| {
                                matches!(part, PersistedAssistantPart::ToolCall { id, .. } if id.as_str() == result.tool_call_id)
                            })
                            .unwrap_or(usize::MAX)
                    });
                    history.push(HistoryTurn::tool(ToolMessage::new(results)));
                }
                let _ = assistant.run_id;
            }
            LogicalTurn::Rehydration(files) => {
                let mut calls = Vec::with_capacity(files.len());
                let mut results = Vec::with_capacity(files.len());
                for (index, file) in files.into_iter().enumerate() {
                    let id = format!("context-rehydration-{index}");
                    calls.push(AssistantPart::ToolCall(ToolCallPart::new(
                        id.clone(),
                        "read",
                        serde_json::json!({"filePath": file.path.as_str()}),
                    )));
                    results.push(ToolResultPart::new(id, ToolContent::Text(file.content)));
                }
                history.push(HistoryTurn::assistant(CompletedTurn::new(
                    AssistantMessage::new(calls),
                    Finish::new(oven_sdk::Usage::default(), FinishReason::ToolCalls),
                )));
                history.push(HistoryTurn::tool(ToolMessage::new(results)));
            }
        }
    }
    Ok(AssembledHistory {
        history,
        replay_decisions,
    })
}

fn latest_project_context_event(events: &[StoredEvent]) -> Option<&StoredEvent> {
    let latest_run = events.iter().rev().find_map(|event| {
        matches!(event.payload, EventPayload::RunStarted { .. }).then_some(event.run_id)
    });
    events.iter().rev().find(|event| {
        matches!(event.payload, EventPayload::ProjectContextLoaded { .. })
            && latest_run.is_none_or(|run| event.run_id == run)
    })
}

fn project_context_turn(entries: &[cookie_agent_protocol::ProjectContextEntry]) -> String {
    entries
        .iter()
        .map(|entry| {
            let source = escape_xml_attribute(entry.source.as_str());
            let marker = entry.truncated.then(|| {
                format!(
                    "\n[project context truncated; original size: {} bytes]",
                    entry.original_bytes
                )
            });
            format!(
                "<project_context source=\"{source}\">\n{}{}\n</project_context>",
                entry.content,
                marker.as_deref().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
pub(crate) fn project_context_turn_for_test(
    entries: &[cookie_agent_protocol::ProjectContextEntry],
) -> String {
    project_context_turn(entries)
}

fn escape_xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn attach_result(
    logical: &mut [LogicalTurn],
    engine_calls: &HashMap<(cookie_agent_protocol::RunId, ToolCallId), (usize, usize)>,
    run_id: Option<cookie_agent_protocol::RunId>,
    tool_call_id: ToolCallId,
    mut result: ToolResultPart,
) -> Result<(), HistoryError> {
    let Some(run_id) = run_id else {
        return Ok(());
    };
    let Some(&(logical_index, call_index)) = engine_calls.get(&(run_id, tool_call_id)) else {
        return Ok(());
    };
    let LogicalTurn::Assistant(assistant) = &mut logical[logical_index] else {
        return Err(HistoryError::Corrupt(
            "tool result mapped to a non-assistant turn".into(),
        ));
    };
    result.tool_call_id = assistant.calls[call_index].model_call_id.to_string();
    assistant.calls[call_index].result = Some(result);
    Ok(())
}

fn user_text(input: &str) -> UserMessage {
    UserMessage::new(vec![InputPart::Text(TextPart::new(input))])
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DeniedToolFailure {
    kind: String,
    source: ApprovalDecisionSource,
    reason: String,
    feedback: Option<String>,
}

fn denied_failure(message: &str) -> Option<DeniedToolFailure> {
    serde_json::from_str::<DeniedToolFailure>(message)
        .ok()
        .filter(|denied| denied.kind == "tool_denied")
}

fn tool_result_part(
    result: &PersistedToolResult,
    tool_call_id: ToolCallId,
    store: &ArtifactStore,
) -> Result<ToolResultPart, HistoryError> {
    let truncation = result.truncation.as_ref().map(|truncation| {
        serde_json::json!({
            "original_bytes":truncation.original_bytes,
            "original_lines":truncation.original_lines,
            "retained":truncation.retained,
            "read_more":{
                "tool":"read_tool_result",
                "arguments":{"tool_call_id":tool_call_id}
            }
        })
    });
    let mut values = vec![
        ContentValue::Text(result.output.clone()),
        ContentValue::Json(serde_json::json!({
            "title": result.title,
            "metadata": result.metadata,
            "truncation": truncation,
        })),
    ];
    for attachment in &result.attachments {
        values.push(ContentValue::File(attachment_file(attachment, store)?));
    }
    Ok(ToolResultPart::new(
        String::new(),
        ToolContent::Mixed(values),
    ))
}

fn attachment_file(
    attachment: &ToolAttachment,
    store: &ArtifactStore,
) -> Result<FilePart, HistoryError> {
    let bytes = store.read_verified_attachment(attachment)?;
    Ok(FilePart {
        media_type: attachment.mime_type.to_string(),
        filename: attachment.filename.clone(),
        source: FileSource::Bytes(Bytes::from(bytes)),
        metadata: None,
    })
}

fn persist_assistant_part(
    part: AssistantPart,
    store: &ArtifactStore,
) -> Result<PersistedAssistantPart, HistoryError> {
    Ok(match part {
        AssistantPart::Text(part) => PersistedAssistantPart::Text {
            text: part.text,
            metadata: part.metadata,
        },
        AssistantPart::Reasoning(part) => PersistedAssistantPart::Reasoning {
            text: part.text,
            metadata: part.metadata,
        },
        AssistantPart::ToolCall(part) => PersistedAssistantPart::ToolCall {
            id: cookie_agent_protocol::ModelCallId::new(part.id)
                .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
            provider_item_id: part
                .provider_item_id
                .map(cookie_agent_protocol::ProviderItemId::new)
                .transpose()
                .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
            name: cookie_agent_protocol::SafeCode::new(part.name)
                .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
            input: part.input,
            raw_input: part.raw_input,
            metadata: part.metadata,
        },
        AssistantPart::ToolResult(part) => PersistedAssistantPart::ToolResult {
            tool_call_id: cookie_agent_protocol::ModelCallId::new(part.tool_call_id)
                .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
            content: persist_tool_content(part.content, store)?,
            is_error: part.is_error,
            metadata: part.metadata,
        },
        AssistantPart::File(file) => PersistedAssistantPart::File {
            file: persist_file(file, store)?,
        },
        AssistantPart::Source(part) => PersistedAssistantPart::Source {
            id: part.id,
            url: part.url.map(|url| url.to_string()),
            title: part.title,
            media_type: part.media_type,
            excerpt: part.excerpt,
            metadata: part.metadata,
        },
        AssistantPart::ToolApproval(part) => PersistedAssistantPart::ToolApproval {
            tool_call_id: cookie_agent_protocol::ModelCallId::new(part.tool_call_id)
                .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
            message: part.message,
            metadata: part.metadata,
        },
        AssistantPart::Custom(part) => PersistedAssistantPart::Custom {
            kind: cookie_agent_protocol::SafeCode::new(part.kind)
                .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
            data: part.data,
            metadata: part.metadata,
        },
    })
}

fn restore_assistant_part(part: &PersistedAssistantPart) -> Result<AssistantPart, HistoryError> {
    Ok(match part {
        PersistedAssistantPart::Text { text, metadata } => AssistantPart::Text(TextPart {
            text: text.clone(),
            metadata: metadata.clone(),
        }),
        PersistedAssistantPart::Reasoning { text, metadata } => {
            AssistantPart::Reasoning(ReasoningPart {
                text: text.clone(),
                metadata: metadata.clone(),
            })
        }
        PersistedAssistantPart::ToolCall {
            id,
            provider_item_id,
            name,
            input,
            raw_input,
            metadata,
        } => AssistantPart::ToolCall(ToolCallPart {
            id: id.to_string(),
            provider_item_id: provider_item_id.as_ref().map(ToString::to_string),
            name: name.to_string(),
            input: input.clone(),
            raw_input: raw_input.clone(),
            metadata: metadata.clone(),
        }),
        PersistedAssistantPart::ToolResult {
            tool_call_id,
            content,
            is_error,
            metadata,
        } => AssistantPart::ToolResult(ToolResultPart {
            tool_call_id: tool_call_id.to_string(),
            content: restore_tool_content(content)?,
            is_error: *is_error,
            metadata: metadata.clone(),
        }),
        PersistedAssistantPart::File { file } => AssistantPart::File(restore_file(file)?),
        PersistedAssistantPart::Source {
            id,
            url,
            title,
            media_type,
            excerpt,
            metadata,
        } => AssistantPart::Source(SourcePart {
            id: id.clone(),
            url: url
                .as_ref()
                .map(|url| url.parse())
                .transpose()
                .map_err(|error| HistoryError::Corrupt(format!("invalid source URL: {error}")))?,
            title: title.clone(),
            media_type: media_type.clone(),
            excerpt: excerpt.clone(),
            metadata: metadata.clone(),
        }),
        PersistedAssistantPart::ToolApproval {
            tool_call_id,
            message,
            metadata,
        } => AssistantPart::ToolApproval(ToolApprovalPart {
            tool_call_id: tool_call_id.to_string(),
            message: message.clone(),
            metadata: metadata.clone(),
        }),
        PersistedAssistantPart::Custom {
            kind,
            data,
            metadata,
        } => AssistantPart::Custom(CustomPart {
            kind: kind.to_string(),
            data: data.clone(),
            metadata: metadata.clone(),
        }),
    })
}

fn persist_tool_content(
    content: ToolContent,
    store: &ArtifactStore,
) -> Result<PersistedToolContent, HistoryError> {
    Ok(match content {
        ToolContent::Text(text) => PersistedToolContent::Text { text },
        ToolContent::Json(value) => PersistedToolContent::Json { value },
        ToolContent::Mixed(values) => PersistedToolContent::Mixed {
            values: values
                .into_iter()
                .map(|value| match value {
                    ContentValue::Text(text) => Ok(PersistedContentValue::Text { text }),
                    ContentValue::Json(value) => Ok(PersistedContentValue::Json { value }),
                    ContentValue::File(file) => Ok(PersistedContentValue::File {
                        file: persist_file(file, store)?,
                    }),
                })
                .collect::<Result<_, HistoryError>>()?,
        },
        ToolContent::Denied { reason } => PersistedToolContent::Denied { reason },
    })
}

fn restore_tool_content(content: &PersistedToolContent) -> Result<ToolContent, HistoryError> {
    Ok(match content {
        PersistedToolContent::Text { text } => ToolContent::Text(text.clone()),
        PersistedToolContent::Json { value } => ToolContent::Json(value.clone()),
        PersistedToolContent::Mixed { values } => ToolContent::Mixed(
            values
                .iter()
                .map(|value| match value {
                    PersistedContentValue::Text { text } => Ok(ContentValue::Text(text.clone())),
                    PersistedContentValue::Json { value } => Ok(ContentValue::Json(value.clone())),
                    PersistedContentValue::File { file } => {
                        Ok(ContentValue::File(restore_file(file)?))
                    }
                })
                .collect::<Result<_, HistoryError>>()?,
        ),
        PersistedToolContent::Denied { reason } => ToolContent::Denied {
            reason: reason.clone(),
        },
    })
}

fn persist_file(file: FilePart, store: &ArtifactStore) -> Result<PersistedFilePart, HistoryError> {
    let source = match file.source {
        FileSource::Bytes(bytes) => persisted_artifact(store, &bytes)?,
        FileSource::Text(text) => persisted_artifact(store, text.as_bytes())?,
        FileSource::Url(url) => PersistedFileSource::Url {
            url: url.to_string(),
        },
        FileSource::ProviderReference { provider, id } => PersistedFileSource::ProviderReference {
            provider_id: cookie_agent_protocol::ProviderId::new(provider.as_str())
                .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
            id: cookie_agent_protocol::SafeDisplayText::new(id)
                .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
        },
    };
    Ok(PersistedFilePart {
        media_type: cookie_agent_protocol::MimeType::new(file.media_type)
            .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
        filename: file.filename,
        source,
        metadata: file.metadata,
    })
}

fn persisted_artifact(
    store: &ArtifactStore,
    bytes: &[u8],
) -> Result<PersistedFileSource, HistoryError> {
    let (reference, sha256) = store.retain(bytes)?;
    Ok(PersistedFileSource::Artifact {
        byte_length: bytes.len() as u64,
        sha256: Sha256Digest::new(sha256)
            .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
        reference,
    })
}

fn restore_file(file: &PersistedFilePart) -> Result<FilePart, HistoryError> {
    let source = match &file.source {
        PersistedFileSource::Artifact { .. } => {
            return Err(HistoryError::Corrupt(
                "persisted artifact file was restored without the artifact store".into(),
            ));
        }
        PersistedFileSource::Url { url } => FileSource::Url(
            url.parse()
                .map_err(|error| HistoryError::Corrupt(format!("invalid file URL: {error}")))?,
        ),
        PersistedFileSource::ProviderReference { provider_id, id } => {
            FileSource::ProviderReference {
                provider: ProviderId::new(provider_id.as_str()),
                id: id.to_string(),
            }
        }
    };
    Ok(FilePart {
        media_type: file.media_type.to_string(),
        filename: file.filename.clone(),
        source,
        metadata: file.metadata.clone(),
    })
}

fn restore_file_with_store(
    file: &PersistedFilePart,
    store: &ArtifactStore,
) -> Result<FilePart, HistoryError> {
    if let PersistedFileSource::Artifact {
        byte_length,
        sha256,
        reference,
    } = &file.source
    {
        let attachment = ToolAttachment {
            mime_type: file.media_type.clone(),
            filename: file.filename.clone(),
            byte_length: *byte_length,
            sha256: sha256.clone(),
            reference: reference.clone(),
        };
        return attachment_file(&attachment, store);
    }
    restore_file(file)
}

fn persist_replay(
    artifact: OvenReplayArtifact,
    binding: &FrozenModelBinding,
) -> Result<NativeReplayArtifact, HistoryError> {
    if artifact.adapter_id() != &binding.descriptor.adapter_id
        || artifact.scope().provider_id != binding.descriptor.identity.provider_id
        || artifact.scope().model_id != binding.descriptor.identity.model_id
    {
        return Err(HistoryError::Corrupt(
            "native replay artifact does not match its exact frozen model binding".into(),
        ));
    }
    NativeReplayArtifact::new(
        cookie_agent_protocol::SafeCode::new(artifact.adapter_id().as_str())
            .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
        cookie_agent_protocol::Sha256Digest::new(binding.selection_fingerprint.as_str())
            .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
        persist_scope(artifact.scope()),
        artifact.payload().clone(),
    )
    .map_err(|error| HistoryError::Corrupt(error.to_string()))
}

fn restore_replay(
    artifact: &NativeReplayArtifact,
    resolved_model: &ResolvedModelRef,
    binding: &FrozenModelBinding,
) -> (Option<OvenReplayArtifact>, Option<ReplayDisposition>) {
    let expected_adapter = exact_adapter_code(binding.descriptor.adapter_id.as_str());
    if artifact.adapter_id() != &expected_adapter {
        return (
            None,
            Some(ReplayDisposition::DiscardedForeignAdapter {
                found: artifact.adapter_id().clone(),
                expected: expected_adapter,
            }),
        );
    }
    if artifact.selection_fingerprint() != &resolved_model.selection_fingerprint
        || artifact.scope().provider_id != resolved_model.provider_id
        || artifact.scope().model_id != resolved_model.model_id
    {
        return (
            None,
            Some(ReplayDisposition::DiscardedInvalidPayload {
                reason: safe_replay_reason(
                    "native replay identity does not match its persisted model turn",
                ),
            }),
        );
    }
    if resolved_model.selection != binding.selection {
        let disposition = if resolved_model.selection.model == binding.selection.model {
            ReplayDisposition::DiscardedForeignVariant {
                found: resolved_model.selection.variant.clone(),
                expected: binding.selection.variant.clone(),
            }
        } else {
            ReplayDisposition::DiscardedForeignModelSelection {
                found: resolved_model.selection.clone(),
                expected: binding.selection.clone(),
            }
        };
        return (None, Some(disposition));
    }
    if artifact.selection_fingerprint().as_str() != binding.selection_fingerprint.as_str() {
        return (
            None,
            Some(ReplayDisposition::DiscardedInvalidPayload {
                reason: safe_replay_reason(
                    "native replay selection fingerprint no longer matches the frozen binding",
                ),
            }),
        );
    }
    let scope = match restore_scope(artifact.scope()) {
        Ok(scope) => scope,
        Err(error) => {
            return (
                None,
                Some(ReplayDisposition::DiscardedInvalidPayload {
                    reason: safe_replay_reason(&error.to_string()),
                }),
            );
        }
    };
    match OvenReplayArtifact::new(
        AdapterId::new(artifact.adapter_id().as_str()),
        scope,
        artifact.payload().clone(),
    ) {
        Ok(artifact) => (Some(artifact), None),
        Err(error) => (
            None,
            Some(ReplayDisposition::DiscardedInvalidPayload {
                reason: safe_replay_reason(&error.to_string()),
            }),
        ),
    }
}

fn exact_adapter_code(value: &str) -> SafeCode {
    SafeCode::new(value).unwrap_or_else(|_| {
        SafeCode::new("invalid-adapter-id").expect("static adapter fallback is valid")
    })
}

fn safe_replay_reason(value: &str) -> SafeErrorMessage {
    SafeErrorMessage::new(sanitize_control_free(value, SafeErrorMessage::MAX_BYTES))
        .expect("sanitized replay reason")
}

fn persist_scope(scope: &OvenNativeContextScope) -> NativeContextScope {
    NativeContextScope {
        provider_id: cookie_agent_protocol::ProviderId::new(scope.provider_id.as_str())
            .expect("validated provider id"),
        model_id: cookie_agent_protocol::ProviderModelId::new(scope.model_id.as_str())
            .expect("validated model id"),
        resource_id: cookie_agent_protocol::SafeDisplayText::new(scope.resource_id.as_str())
            .expect("validated resource id"),
    }
}

fn restore_scope(scope: &NativeContextScope) -> Result<OvenNativeContextScope, HistoryError> {
    OvenNativeContextScope::new(
        ProviderId::new(scope.provider_id.as_str()),
        oven_sdk::ModelId::new(scope.model_id.as_str()),
        ResourceId::new(scope.resource_id.as_str())?,
    )
    .map_err(HistoryError::from)
}

pub(crate) fn persist_native_context(
    window: OvenNativeContextWindow,
    binding: &FrozenModelBinding,
) -> Result<cookie_agent_protocol::NativeContextWindow, HistoryError> {
    cookie_agent_protocol::NativeContextWindow::new(
        exact_adapter_code(window.adapter_id().as_str()),
        binding.blueprint_fingerprint.clone(),
        persist_scope(window.scope()),
        window.payload().clone(),
    )
    .map_err(|error| HistoryError::Corrupt(error.to_string()))
}

fn restore_native_context(
    window: &cookie_agent_protocol::NativeContextWindow,
) -> Result<OvenNativeContextWindow, HistoryError> {
    OvenNativeContextWindow::new(
        AdapterId::new(window.adapter_id().as_str()),
        restore_scope(window.scope())?,
        window.payload().clone(),
    )
    .map_err(|error| HistoryError::Corrupt(error.to_string()))
}

pub(crate) fn persist_usage(usage: oven_sdk::Usage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens,
        input_tokens_no_cache: usage.input_tokens_no_cache,
        input_tokens_cache_read: usage.input_tokens_cache_read,
        input_tokens_cache_write: usage.input_tokens_cache_write,
        output_tokens: usage.output_tokens,
        output_tokens_text: usage.output_tokens_text,
        output_tokens_reasoning: usage.output_tokens_reasoning,
    }
}

fn restore_usage(usage: &Usage) -> oven_sdk::Usage {
    oven_sdk::Usage {
        input_tokens: usage.input_tokens,
        input_tokens_no_cache: usage.input_tokens_no_cache,
        input_tokens_cache_read: usage.input_tokens_cache_read,
        input_tokens_cache_write: usage.input_tokens_cache_write,
        output_tokens: usage.output_tokens,
        output_tokens_text: usage.output_tokens_text,
        output_tokens_reasoning: usage.output_tokens_reasoning,
        raw: None,
    }
}

fn persist_finish_reason(reason: FinishReason) -> ModelFinishReason {
    match reason {
        FinishReason::Stop => ModelFinishReason::Stop,
        FinishReason::ToolCalls => ModelFinishReason::ToolCalls,
        FinishReason::Length => ModelFinishReason::Length,
        FinishReason::ContentFilter => ModelFinishReason::ContentFilter,
        FinishReason::Cancelled => ModelFinishReason::Cancelled,
        FinishReason::Error => ModelFinishReason::Error,
        FinishReason::Aborted => ModelFinishReason::Aborted,
        FinishReason::Timeout => ModelFinishReason::Timeout,
        FinishReason::Refused => ModelFinishReason::Refused,
        FinishReason::Unknown => ModelFinishReason::Unknown,
        FinishReason::Other(value) => ModelFinishReason::Other(value),
    }
}

fn restore_finish_reason(reason: &ModelFinishReason) -> FinishReason {
    match reason {
        ModelFinishReason::Stop => FinishReason::Stop,
        ModelFinishReason::ToolCalls => FinishReason::ToolCalls,
        ModelFinishReason::Length => FinishReason::Length,
        ModelFinishReason::ContentFilter => FinishReason::ContentFilter,
        ModelFinishReason::Cancelled => FinishReason::Cancelled,
        ModelFinishReason::Error => FinishReason::Error,
        ModelFinishReason::Aborted => FinishReason::Aborted,
        ModelFinishReason::Timeout => FinishReason::Timeout,
        ModelFinishReason::Refused => FinishReason::Refused,
        ModelFinishReason::Unknown => FinishReason::Unknown,
        ModelFinishReason::Other(value) => FinishReason::Other(value.clone()),
    }
}

// Artifact-backed files need the store only while assembling a live request.
fn restore_assistant_part_with_store(
    part: &PersistedAssistantPart,
    store: &ArtifactStore,
) -> Result<AssistantPart, HistoryError> {
    match part {
        PersistedAssistantPart::File { file } => {
            Ok(AssistantPart::File(restore_file_with_store(file, store)?))
        }
        PersistedAssistantPart::ToolResult {
            tool_call_id,
            content: PersistedToolContent::Mixed { values },
            is_error,
            metadata,
        } => {
            let values = values
                .iter()
                .map(|value| match value {
                    PersistedContentValue::Text { text } => Ok(ContentValue::Text(text.clone())),
                    PersistedContentValue::Json { value } => Ok(ContentValue::Json(value.clone())),
                    PersistedContentValue::File { file } => {
                        Ok(ContentValue::File(restore_file_with_store(file, store)?))
                    }
                })
                .collect::<Result<_, HistoryError>>()?;
            Ok(AssistantPart::ToolResult(ToolResultPart {
                tool_call_id: tool_call_id.to_string(),
                content: ToolContent::Mixed(values),
                is_error: *is_error,
                metadata: metadata.clone(),
            }))
        }
        _ => restore_assistant_part(part),
    }
}

fn restore_turn_with_store(
    turn: &PersistedModelTurn,
    resolved_model: &ResolvedModelRef,
    store: &ArtifactStore,
    binding: &FrozenModelBinding,
) -> Result<(CompletedTurn, Option<ReplayDisposition>), HistoryError> {
    let (native_replay, replay_disposition) = turn
        .native_replay
        .as_ref()
        .map_or((None, None), |artifact| {
            restore_replay(artifact, resolved_model, binding)
        });
    Ok((
        CompletedTurn {
            message: AssistantMessage {
                content: turn
                    .content
                    .iter()
                    .map(|part| restore_assistant_part_with_store(part, store))
                    .collect::<Result<_, _>>()?,
                provider_options: turn.provider_options.clone(),
            },
            finish: Finish {
                usage: restore_usage(&turn.usage),
                finish_reason: restore_finish_reason(&turn.finish_reason),
                response_metadata: turn.response_metadata.clone(),
                provider_metadata: turn.provider_metadata.clone(),
                native_replay,
            },
            warnings: Vec::new(),
        },
        replay_disposition,
    ))
}

fn scope_selection(scope: &OvenNativeContextScope) -> ModelSelection {
    let provider = cookie_agent_protocol::ProviderId::new(scope.provider_id.as_str())
        .expect("validated provider id");
    let model = cookie_agent_protocol::ProviderModelId::new(scope.model_id.as_str())
        .expect("validated model id");
    ModelSelection {
        model: cookie_agent_protocol::ModelKey::new(provider, model).expect("validated model key"),
        variant: None,
    }
}

fn sanitize_control_free(value: &str, maximum: usize) -> String {
    let mut output = String::with_capacity(value.len().min(maximum));
    for character in value.chars() {
        if output.len() >= maximum {
            break;
        }
        let replacement = if character.is_control() {
            ' '
        } else {
            character
        };
        if output.len() + replacement.len_utf8() > maximum {
            break;
        }
        output.push(replacement);
    }
    if output.is_empty() {
        "unavailable".to_owned()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cookie_agent_protocol::{
        ArtifactReference, AssistantToolCallRef, ContextCheckpoint, ContextCheckpointBoundaries,
        ContextCheckpointBudgets, ContextCheckpointCommit, DelegatedContextRole,
        DelegatedContextTurn, EventPayload, InternalAgentInvocationId, InternalAgentRunId,
        InternalSummaryCheckpoint, ModelCallId, ModelFinishReason, ModelKey, ModelSelection,
        NativeContextScope, NativeReplayArtifact, OperationFingerprint, PermissionAction,
        PersistedAssistantPart, PersistedModelTurn, PersistedToolResult, PreparedApprovalResource,
        PreparedBindingLifetime, PreparedCapabilityOperation, PreparedOperationIdentity,
        PreparedResourceDigest, PreparedResourceIdentity, ProjectContextEntry, ProviderId,
        ProviderModelId, ReplayDisposition, ResolvedModelRef, RunId, SafeCode, SafeDisplayText,
        SessionId, Sha256Digest, StoredEvent, SummaryByteLimit, ToolCallId, ToolCallPresentation,
        ToolCallStart, ToolCallTermination, ToolOutputTruncation, ToolTerminationOutcome, Usage,
    };
    use oven_sdk::{
        AdapterId, HistoryTurn, NativeContextScope as OvenNativeContextScope,
        NativeContextWindow as OvenNativeContextWindow, ReplayDecision as OvenReplayDecision,
        ReplayDisposition as OvenReplayDisposition, ResourceId, SystemMessage, SystemPart,
        TextPart,
    };

    use super::{
        COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, assemble_full_history,
        assemble_model_context, checkpoint_retained_history, framed_compaction_summary,
        replay_decisions, replay_decisions_with_preflight, restore_replay,
        tool_output_elision_marker, tool_result_part, wire_model,
    };

    #[test]
    fn truncated_tool_result_names_the_readback_tool() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::ArtifactStore::open(directory.path().join("artifacts")).unwrap();
        let call_id = ToolCallId::new_v7();
        let result = PersistedToolResult {
            title: SafeDisplayText::new("Truncated").unwrap(),
            output: "preview".into(),
            metadata: serde_json::Value::Null,
            truncation: Some(ToolOutputTruncation {
                original_bytes: 100,
                original_lines: 10,
                retained: ArtifactReference {
                    uri: format!("artifact://sha256/{}", "a".repeat(64)),
                },
            }),
            attachments: Vec::new(),
        };
        let encoded = serde_json::to_value(tool_result_part(&result, call_id, &store).unwrap())
            .unwrap()
            .to_string();
        assert!(encoded.contains("read_tool_result"));
        assert!(encoded.contains(&call_id.to_string()));
    }

    #[test]
    fn compaction_summary_framing_is_byte_stable() {
        assert_eq!(
            COMPACTION_SUMMARY_PREFIX,
            "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n<summary>\n"
        );
        assert_eq!(
            COMPACTION_SUMMARY_SUFFIX,
            "\n</summary>\n\nPlease continue the conversation from where we left off without asking the user any further questions."
        );
        assert_eq!(
            framed_compaction_summary("state"),
            format!("{COMPACTION_SUMMARY_PREFIX}state{COMPACTION_SUMMARY_SUFFIX}")
        );
    }

    #[test]
    fn checkpoint_accounting_retains_project_context_and_every_skill_body() {
        let run = RunId::new_v7();
        let skill_event = |seq, name: &str, body: &str| {
            event(
                seq,
                run,
                EventPayload::SkillLoaded {
                    name: name.into(),
                    rendered_body: body.into(),
                    source_path: format!("/{name}/SKILL.md"),
                    args: String::new(),
                    base_dir: format!("/{name}"),
                    supporting_files: Vec::new(),
                },
            )
        };
        let events = vec![
            event(
                1,
                run,
                EventPayload::ProjectContextLoaded {
                    entries: vec![ProjectContextEntry {
                        source: SafeDisplayText::new("AGENTS.md").unwrap(),
                        content: "pinned project context".into(),
                        truncated: false,
                        original_bytes: 22,
                    }],
                },
            ),
            skill_event(2, "one", "first pinned body"),
            skill_event(3, "two", "second pinned body"),
        ];
        let history = vec![
            HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(TextPart::new(
                "system",
            ))])),
            HistoryTurn::user(super::user_text("pinned project context")),
            HistoryTurn::user(super::user_text("first pinned body")),
            HistoryTurn::user(super::user_text("second pinned body")),
            HistoryTurn::user(super::user_text("discarded conversation")),
        ];
        let native = checkpoint_retained_history(&history, &events, None);
        assert_eq!(native.len(), 4);
        let summarized = checkpoint_retained_history(&history, &events, Some("summary"));
        assert_eq!(summarized.len(), 5);
        let encoded = serde_json::to_string(&summarized).expect("history JSON");
        assert!(encoded.contains("first pinned body"));
        assert!(encoded.contains("second pinned body"));
        assert!(encoded.contains("pinned project context"));
        assert!(encoded.contains("<summary>\\nsummary"));
        assert!(!encoded.contains("discarded conversation"));
    }

    #[test]
    fn truncated_project_context_turn_has_provenance_and_size_marker() {
        let rendered = super::project_context_turn(&[ProjectContextEntry {
            source: SafeDisplayText::new("AGENTS.md").unwrap(),
            content: "bounded".into(),
            truncated: true,
            original_bytes: 42,
        }]);
        assert_eq!(
            rendered,
            "<project_context source=\"AGENTS.md\">\nbounded\n[project context truncated; original size: 42 bytes]\n</project_context>"
        );
    }

    #[test]
    fn replay_orders_project_context_then_skills_then_delegated_seed() {
        let run = RunId::new_v7();
        let mut delegated = event(
            1,
            run,
            EventPayload::DelegatedContextSeeded {
                invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
                turns: vec![DelegatedContextTurn {
                    role: DelegatedContextRole::User,
                    text: "delegated seed".into(),
                }],
            },
        );
        delegated.run_id = None;
        let events = vec![
            delegated,
            event(
                2,
                run,
                EventPayload::SkillLoaded {
                    name: "ordered-skill".into(),
                    rendered_body: "loaded skill body".into(),
                    source_path: "/skills/ordered-skill/SKILL.md".into(),
                    args: String::new(),
                    base_dir: "/skills/ordered-skill".into(),
                    supporting_files: Vec::new(),
                },
            ),
            event(
                3,
                run,
                EventPayload::ProjectContextLoaded {
                    entries: vec![ProjectContextEntry {
                        source: SafeDisplayText::new("AGENTS.md").unwrap(),
                        content: "project context body".into(),
                        truncated: false,
                        original_bytes: 20,
                    }],
                },
            ),
        ];
        let directory = tempfile::tempdir().unwrap();
        let store = crate::ArtifactStore::open(directory.path().join("artifacts")).unwrap();
        let history = assemble_full_history(&events, &store, &binding(), "system prompt").unwrap();
        assert_eq!(history.len(), 4);
        let rendered = history
            .iter()
            .map(|turn| serde_json::to_string(turn).unwrap())
            .collect::<Vec<_>>();
        assert!(rendered[0].contains("system prompt"));
        assert!(rendered[1].contains("project context body"));
        assert!(rendered[2].contains("loaded skill body"));
        assert!(rendered[3].contains("delegated seed"));
    }

    #[test]
    fn tool_elision_marker_is_stable_and_contains_the_artifact_reference() {
        let retained = cookie_agent_protocol::ArtifactReference {
            uri:
                "artifact://sha256/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .into(),
        };
        assert_eq!(
            tool_output_elision_marker(&retained, 12_345),
            "[tool output elided; retained at artifact://sha256/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; 12345 bytes]"
        );
    }
    use crate::{
        ArtifactStore,
        test_support::{model_binding as binding, variant_model_binding},
    };

    fn event(seq: u64, run: RunId, payload: EventPayload) -> StoredEvent {
        StoredEvent {
            engine_version: None,
            origin: None,
            session_id: SessionId(uuid::Uuid::from_u128(1)),
            run_id: Some(run),
            seq,
            timestamp: jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        }
    }

    fn operation() -> PreparedOperationIdentity {
        let digest = PreparedResourceDigest::from_canonical_binding_bytes(b"binding");
        PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"args"),
            vec![cookie_agent_protocol::ApprovalCapability {
                action: PermissionAction::Read,
                operation: PreparedCapabilityOperation::new("read:read").expect("operation"),
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Read,
                canonical: PreparedResourceIdentity::new("file:readme").expect("identity"),
                binding_digest: digest,
                binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                boundary: cookie_agent_protocol::ApprovalBoundary::Exact,
                source: cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"context"),
        )
        .expect("prepared operation")
    }

    #[test]
    fn normalized_history_keeps_native_artifacts_for_adapter_scoped_replay() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::open(directory.path().join("artifacts")).expect("store");
        let binding = binding();
        let resolved = wire_model(&binding);
        let artifact = NativeReplayArtifact::new(
            SafeCode::new(binding.descriptor.adapter_id.as_str()).expect("adapter id"),
            resolved.selection_fingerprint.clone(),
            NativeContextScope {
                provider_id: resolved.provider_id.clone(),
                model_id: resolved.model_id.clone(),
                resource_id: SafeDisplayText::new("resource").expect("resource"),
            },
            serde_json::json!({"opaque": true}),
        )
        .expect("artifact");
        let turn = PersistedModelTurn {
            content: vec![PersistedAssistantPart::Text {
                text: "answer".into(),
                metadata: None,
            }],
            provider_options: BTreeMap::new(),
            finish_reason: ModelFinishReason::Stop,
            usage: Usage::default(),
            response_metadata: BTreeMap::new(),
            provider_metadata: BTreeMap::new(),
            native_replay: Some(artifact),
        };
        let run = RunId::new_v7();
        let events = vec![event(
            1,
            run,
            EventPayload::ModelTurnCommitted {
                attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                model_turn_seq: 1,
                resolved_model: resolved,
                input_through_seq: 1,
                turn,
                warnings: Vec::new(),
            },
        )];
        let history =
            assemble_full_history(&events, &store, &binding, "System prompt.").expect("history");
        let oven_sdk::HistoryTurn::Assistant(turn) = &history[1] else {
            panic!("assistant turn");
        };
        let replay = turn.finish.native_replay.as_ref().expect("native replay");
        assert_eq!(
            replay.adapter_id().as_str(),
            binding.descriptor.adapter_id.as_str()
        );
        assert_eq!(replay.scope().resource_id.as_str(), "resource");
    }

    #[test]
    fn native_replay_is_not_reused_across_variants() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::open(directory.path().join("artifacts")).expect("store");
        let base = binding();
        let resolved = wire_model(&base);
        let artifact = NativeReplayArtifact::new(
            SafeCode::new(base.descriptor.adapter_id.as_str()).expect("adapter id"),
            resolved.selection_fingerprint,
            NativeContextScope {
                provider_id: resolved.provider_id,
                model_id: resolved.model_id,
                resource_id: SafeDisplayText::new("resource").expect("resource"),
            },
            serde_json::json!({"opaque": true}),
        )
        .expect("artifact");
        let run = RunId::new_v7();
        let events = vec![event(
            1,
            run,
            EventPayload::ModelTurnCommitted {
                attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                model_turn_seq: 1,
                resolved_model: wire_model(&base),
                input_through_seq: 1,
                turn: PersistedModelTurn {
                    content: vec![PersistedAssistantPart::Text {
                        text: "answer".into(),
                        metadata: None,
                    }],
                    provider_options: BTreeMap::new(),
                    finish_reason: ModelFinishReason::Stop,
                    usage: Usage::default(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: Some(artifact),
                },
                warnings: Vec::new(),
            },
        )];
        let variant = variant_model_binding();
        let context = assemble_model_context(&events, &store, &variant, "System prompt.")
            .expect("foreign variant falls back to normalized history");
        let oven_sdk::HistoryTurn::Assistant(turn) = &context.history[1] else {
            panic!("assistant turn");
        };
        assert!(turn.finish.native_replay.is_none());
        assert!(matches!(
            &context.replay_decisions[0].disposition,
            ReplayDisposition::DiscardedForeignVariant { found, expected }
                if found.is_none() && expected.as_ref().is_some_and(|variant| variant.as_str() == "fast")
        ));
        let merged = replay_decisions_with_preflight(
            &[
                OvenReplayDecision {
                    history_index: 1,
                    disposition: OvenReplayDisposition::NoArtifact,
                },
                OvenReplayDecision {
                    history_index: 1,
                    disposition: OvenReplayDisposition::ReconstructedNormalized,
                },
            ],
            &variant,
            &context.replay_decisions,
        );
        assert!(matches!(
            merged.as_slice(),
            [
                cookie_agent_protocol::ReplayDecision {
                    disposition: ReplayDisposition::DiscardedForeignVariant { .. },
                    ..
                },
                cookie_agent_protocol::ReplayDecision {
                    disposition: ReplayDisposition::ReconstructedNormalizedHistory,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn exact_foreign_adapter_is_recorded_without_generic_conversion() {
        let binding = binding();
        let resolved = wire_model(&binding);
        let artifact = NativeReplayArtifact::new(
            SafeCode::new("vendor.custom-adapter.v2").expect("adapter"),
            resolved.selection_fingerprint.clone(),
            NativeContextScope {
                provider_id: resolved.provider_id.clone(),
                model_id: resolved.model_id.clone(),
                resource_id: SafeDisplayText::new("resource").expect("resource"),
            },
            serde_json::json!({"opaque": true}),
        )
        .expect("artifact");
        let (restored, disposition) = restore_replay(&artifact, &resolved, &binding);
        assert!(restored.is_none());
        assert!(matches!(
            disposition,
            Some(ReplayDisposition::DiscardedForeignAdapter { found, expected })
                if found.as_str() == "vendor.custom-adapter.v2"
                    && expected.as_str() == binding.descriptor.adapter_id.as_str()
        ));
    }

    #[test]
    fn identical_foreign_dispositions_on_distinct_history_entries_are_preserved() {
        let binding = binding();
        let foreign = ReplayDisposition::DiscardedForeignAdapter {
            found: SafeCode::new("anthropic").expect("adapter"),
            expected: SafeCode::new(binding.descriptor.adapter_id.as_str()).expect("adapter"),
        };
        let preflight = vec![
            cookie_agent_protocol::ReplayDecision {
                history_index: 1,
                disposition: foreign.clone(),
            },
            cookie_agent_protocol::ReplayDecision {
                history_index: 3,
                disposition: foreign,
            },
        ];
        let merged = replay_decisions_with_preflight(
            &[
                OvenReplayDecision {
                    history_index: 1,
                    disposition: OvenReplayDisposition::NoArtifact,
                },
                OvenReplayDecision {
                    history_index: 1,
                    disposition: OvenReplayDisposition::ReconstructedNormalized,
                },
                OvenReplayDecision {
                    history_index: 3,
                    disposition: OvenReplayDisposition::NoArtifact,
                },
                OvenReplayDecision {
                    history_index: 3,
                    disposition: OvenReplayDisposition::ReconstructedNormalized,
                },
            ],
            &binding,
            &preflight,
        );
        assert!(matches!(
            merged.as_slice(),
            [
                cookie_agent_protocol::ReplayDecision {
                    history_index: 1,
                    disposition: ReplayDisposition::DiscardedForeignAdapter { .. },
                },
                cookie_agent_protocol::ReplayDecision {
                    history_index: 1,
                    disposition: ReplayDisposition::ReconstructedNormalizedHistory,
                },
                cookie_agent_protocol::ReplayDecision {
                    history_index: 3,
                    disposition: ReplayDisposition::DiscardedForeignAdapter { .. },
                },
                cookie_agent_protocol::ReplayDecision {
                    history_index: 3,
                    disposition: ReplayDisposition::ReconstructedNormalizedHistory,
                },
            ]
        ));
    }

    #[test]
    fn foreign_model_selection_discards_without_aborting_history_restore() {
        let binding = binding();
        let current = wire_model(&binding);
        let provider_id = ProviderId::new("other").expect("provider");
        let model_id = ProviderModelId::new("model").expect("model");
        let selection = ModelSelection {
            model: ModelKey::new(provider_id.clone(), model_id.clone()).expect("model key"),
            variant: None,
        };
        let found = ResolvedModelRef {
            selection: selection.clone(),
            provider_id: provider_id.clone(),
            model_id: model_id.clone(),
            adapter_id: current.adapter_id,
            selection_fingerprint: Sha256Digest::of_bytes(b"foreign selection"),
        };
        let artifact = NativeReplayArtifact::new(
            SafeCode::new(binding.descriptor.adapter_id.as_str()).expect("adapter"),
            found.selection_fingerprint.clone(),
            NativeContextScope {
                provider_id,
                model_id,
                resource_id: SafeDisplayText::new("resource").expect("resource"),
            },
            serde_json::json!({"opaque": true}),
        )
        .expect("artifact");
        let (restored, disposition) = restore_replay(&artifact, &found, &binding);
        assert!(restored.is_none());
        assert!(matches!(
            disposition,
            Some(ReplayDisposition::DiscardedForeignModelSelection {
                found: persisted,
                expected,
            }) if persisted == selection && expected == binding.selection
        ));
    }

    #[test]
    fn invalid_same_selection_payload_discards_and_reconstructs() {
        let binding = binding();
        let resolved = wire_model(&binding);
        let artifact = NativeReplayArtifact::new(
            SafeCode::new(binding.descriptor.adapter_id.as_str()).expect("adapter"),
            resolved.selection_fingerprint.clone(),
            NativeContextScope {
                provider_id: resolved.provider_id.clone(),
                model_id: resolved.model_id.clone(),
                resource_id: SafeDisplayText::new("resource").expect("wire resource"),
            },
            serde_json::json!({"semantically":"invalid"}),
        )
        .expect("artifact");
        let (restored, preflight) = restore_replay(&artifact, &resolved, &binding);
        assert!(restored.is_some());
        assert!(preflight.is_none());
        let merged = replay_decisions_with_preflight(
            &[
                OvenReplayDecision {
                    history_index: 1,
                    disposition: OvenReplayDisposition::DiscardedInvalidPayload {
                        reason: "payload did not match normalized content".into(),
                    },
                },
                OvenReplayDecision {
                    history_index: 1,
                    disposition: OvenReplayDisposition::ReconstructedNormalized,
                },
            ],
            &binding,
            &[],
        );
        assert!(matches!(
            merged.as_slice(),
            [
                cookie_agent_protocol::ReplayDecision {
                    disposition: ReplayDisposition::DiscardedInvalidPayload { .. },
                    ..
                },
                cookie_agent_protocol::ReplayDecision {
                    disposition: ReplayDisposition::ReconstructedNormalizedHistory,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn foreign_scope_replay_decision_is_persisted_as_model_selection() {
        let binding = binding();
        let found = OvenNativeContextScope::new(
            oven_sdk::ProviderId::new("provider"),
            oven_sdk::ModelId::new("one"),
            ResourceId::new("resource-one").expect("resource"),
        )
        .expect("scope");
        let expected = OvenNativeContextScope::new(
            oven_sdk::ProviderId::new("provider"),
            oven_sdk::ModelId::new("two"),
            ResourceId::new("resource-two").expect("resource"),
        )
        .expect("scope");
        let decisions = replay_decisions(
            &[OvenReplayDecision {
                history_index: 3,
                disposition: OvenReplayDisposition::DiscardedForeignScope {
                    found: found.clone(),
                    expected,
                },
            }],
            &binding,
        );
        assert!(matches!(
            &decisions[0].disposition,
            ReplayDisposition::DiscardedForeignModelSelection { found: persisted_found, expected: persisted_expected }
                if persisted_found.model.to_string() == "provider/one"
                    && persisted_expected == &binding.selection
        ));
    }

    #[test]
    fn checkpoint_before_new_user_keeps_summary_and_user_live() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::open(directory.path().join("artifacts")).expect("store");
        let binding = binding();
        let run = RunId::new_v7();
        let summary_limit = SummaryByteLimit::new(1024).expect("limit");
        let checkpoint = InternalSummaryCheckpoint::new(
            "predictive summary".into(),
            InternalAgentInvocationId::new_v7(),
            InternalAgentRunId::new_v7(),
            summary_limit,
        )
        .expect("checkpoint");
        let events = vec![
            event(
                1,
                run,
                EventPayload::UserInputSubmitted {
                    input: "old compacted input".into(),
                },
            ),
            event(
                2,
                run,
                EventPayload::ContextCheckpointCommitted {
                    commit: ContextCheckpointCommit {
                        checkpoint: ContextCheckpoint::InternalSummary { checkpoint },
                        boundaries: ContextCheckpointBoundaries {
                            source_from_seq: 1,
                            source_through_seq: 1,
                            input_through_seq: 1,
                            prior_checkpoint_seq: None,
                        },
                        budgets: ContextCheckpointBudgets {
                            context_limit_tokens: 100,
                            trigger_tokens: 70,
                            input_tokens_before: 60,
                            input_tokens_after: 5,
                            max_summary_bytes: summary_limit,
                        },
                    },
                },
            ),
            event(
                3,
                run,
                EventPayload::UserInputSubmitted {
                    input: "extremely long live user input".into(),
                },
            ),
            event(4, run, EventPayload::UserInputApplied { user_input_seq: 3 }),
        ];

        assert!(events[1].seq < events[2].seq);
        let context = assemble_model_context(&events, &store, &binding, "System prompt.")
            .expect("assembled context");
        let serialized = serde_json::to_string(&context.history).expect("serialized history");
        assert!(serialized.contains("predictive summary"));
        assert!(serialized.contains("extremely long live user input"));
        assert!(!serialized.contains("old compacted input"));
    }

    #[test]
    fn injected_and_transformed_messages_replay_from_durable_events() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::open(directory.path().join("artifacts")).expect("store");
        let binding = binding();
        let run = RunId::new_v7();
        let events = vec![
            event(
                1,
                run,
                EventPayload::MessageInjected {
                    role: cookie_agent_protocol::ExtensionMessageRole::User,
                    input: "durable injected context".into(),
                },
            ),
            event(
                2,
                run,
                EventPayload::UserInputTransformed {
                    original_input: "original command".into(),
                    input: "committed transformed input".into(),
                },
            ),
            event(
                3,
                run,
                EventPayload::UserInputSubmitted {
                    input: "committed transformed input".into(),
                },
            ),
            event(4, run, EventPayload::UserInputApplied { user_input_seq: 3 }),
        ];

        let context = assemble_model_context(&events, &store, &binding, "System prompt.")
            .expect("assembled context");
        let serialized = serde_json::to_string(&context.history).expect("serialized history");
        assert!(serialized.contains("durable injected context"));
        assert!(serialized.contains("committed transformed input"));
        assert!(!serialized.contains("original command"));
    }

    #[test]
    fn native_checkpoint_carries_window_and_drops_pre_checkpoint_history() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::open(directory.path().join("artifacts")).expect("store");
        let mut binding = binding();
        binding.descriptor.capabilities.compaction = oven_sdk::CompactionCapability::Native;
        let run = RunId::new_v7();
        let sdk_window = OvenNativeContextWindow::new(
            AdapterId::new(binding.descriptor.adapter_id.as_str()),
            OvenNativeContextScope::new(
                oven_sdk::ProviderId::new(binding.selection.model.provider_id().as_str()),
                oven_sdk::ModelId::new(binding.selection.model.model_id().as_str()),
                ResourceId::new("native-window-v1").expect("resource"),
            )
            .expect("scope"),
            serde_json::json!({"type": "compaction", "id": "cmp_1"}),
        )
        .expect("SDK window");
        let window =
            super::persist_native_context(sdk_window.clone(), &binding).expect("persisted window");
        let summary_limit = SummaryByteLimit::new(1024).expect("limit");
        let events = vec![
            event(
                1,
                run,
                EventPayload::UserInputSubmitted {
                    input: "old compacted input".into(),
                },
            ),
            event(
                2,
                run,
                EventPayload::ContextCheckpointCommitted {
                    commit: ContextCheckpointCommit {
                        checkpoint: ContextCheckpoint::NativeWindow { window },
                        boundaries: ContextCheckpointBoundaries {
                            source_from_seq: 1,
                            source_through_seq: 1,
                            input_through_seq: 1,
                            prior_checkpoint_seq: None,
                        },
                        budgets: ContextCheckpointBudgets {
                            context_limit_tokens: 100,
                            trigger_tokens: 70,
                            input_tokens_before: 60,
                            input_tokens_after: 5,
                            max_summary_bytes: summary_limit,
                        },
                    },
                },
            ),
            event(
                3,
                run,
                EventPayload::UserInputSubmitted {
                    input: "live input".into(),
                },
            ),
            event(4, run, EventPayload::UserInputApplied { user_input_seq: 3 }),
        ];

        let context = assemble_model_context(&events, &store, &binding, "System prompt.")
            .expect("assembled context");
        let serialized = serde_json::to_string(&context.history).expect("serialized history");
        assert!(!serialized.contains("old compacted input"));
        assert!(serialized.contains("live input"));
        assert_eq!(context.native_context, Some(sdk_window));
        let request = oven_sdk::Request::new(context.history)
            .with_native_context(context.native_context.expect("native context"));
        assert!(request.native_context.is_some());
    }

    #[test]
    fn revert_voids_checkpoint_beyond_boundary_and_keeps_older_checkpoint() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::open(directory.path().join("artifacts")).expect("store");
        let binding = binding();
        let run = RunId::new_v7();
        let summary_limit = SummaryByteLimit::new(1024).expect("limit");
        let checkpoint = |summary: &str, through_seq| ContextCheckpointCommit {
            checkpoint: ContextCheckpoint::InternalSummary {
                checkpoint: InternalSummaryCheckpoint::new(
                    summary.into(),
                    InternalAgentInvocationId::new_v7(),
                    InternalAgentRunId::new_v7(),
                    summary_limit,
                )
                .expect("checkpoint"),
            },
            boundaries: ContextCheckpointBoundaries {
                source_from_seq: 1,
                source_through_seq: through_seq,
                input_through_seq: through_seq,
                prior_checkpoint_seq: None,
            },
            budgets: ContextCheckpointBudgets {
                context_limit_tokens: 100,
                trigger_tokens: 70,
                input_tokens_before: 60,
                input_tokens_after: 5,
                max_summary_bytes: summary_limit,
            },
        };
        let session = SessionId(uuid::Uuid::from_u128(1));
        let mut events = vec![
            event(
                1,
                run,
                EventPayload::UserInputSubmitted {
                    input: "old input".into(),
                },
            ),
            event(
                2,
                run,
                EventPayload::ContextCheckpointCommitted {
                    commit: checkpoint("older summary", 1),
                },
            ),
            event(
                3,
                run,
                EventPayload::UserInputSubmitted {
                    input: "void input".into(),
                },
            ),
            event(
                4,
                run,
                EventPayload::ContextCheckpointCommitted {
                    commit: checkpoint("void summary", 3),
                },
            ),
        ];
        events.push(StoredEvent {
            engine_version: None,
            origin: None,
            session_id: session,
            run_id: None,
            seq: 5,
            timestamp: jiff::Timestamp::new(5, 0).expect("timestamp"),
            payload: EventPayload::SessionReverted { through_seq: 2 },
        });
        let visible = cookie_agent_protocol::visible_events(&events);
        let context = assemble_model_context(&visible, &store, &binding, "System prompt.")
            .expect("assembled context");
        let serialized = serde_json::to_string(&context.history).expect("serialized history");
        assert!(serialized.contains("older summary"));
        assert!(!serialized.contains("void summary"));
        assert!(!serialized.contains("void input"));
    }

    #[test]
    fn assembled_tool_transcript_snapshot_is_stable() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::open(directory.path().join("artifacts")).expect("store");
        let binding = binding();
        let resolved = wire_model(&binding);
        let run = RunId(uuid::Uuid::from_u128(2));
        let call = ToolCallId(uuid::Uuid::from_u128(8));
        let owner = AssistantToolCallRef {
            model_turn_seq: 1,
            content_index: 0,
            model_call_id: ModelCallId::new("provider-call").expect("model call id"),
            provider_item_id: None,
        };
        let events = vec![
            event(
                1,
                run,
                EventPayload::UserInputSubmitted {
                    input: "inspect the workspace".into(),
                },
            ),
            event(2, run, EventPayload::UserInputApplied { user_input_seq: 1 }),
            event(
                3,
                run,
                EventPayload::ModelTurnCommitted {
                    attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                    model_turn_seq: 1,
                    resolved_model: resolved,
                    input_through_seq: 1,
                    turn: PersistedModelTurn {
                        content: vec![PersistedAssistantPart::ToolCall {
                            id: owner.model_call_id.clone(),
                            provider_item_id: None,
                            name: SafeCode::new("read").expect("tool name"),
                            input: serde_json::json!({"filePath":"README.md"}),
                            raw_input: None,
                            metadata: None,
                        }],
                        provider_options: BTreeMap::new(),
                        finish_reason: ModelFinishReason::ToolCalls,
                        usage: Usage::default(),
                        response_metadata: BTreeMap::new(),
                        provider_metadata: BTreeMap::new(),
                        native_replay: None,
                    },
                    warnings: Vec::new(),
                },
            ),
            event(
                4,
                run,
                EventPayload::ToolCallStarted {
                    start: ToolCallStart {
                        tool_call_id: call,
                        owner: owner.clone(),
                        presentation: ToolCallPresentation {
                            title: SafeDisplayText::new("Read README.md").expect("title"),
                            primary_argument: Some(
                                SafeDisplayText::new("README.md").expect("argument"),
                            ),
                        },
                        operation_fingerprint: OperationFingerprint::from_prepared_operation(
                            &operation(),
                        ),
                    },
                },
            ),
            event(
                5,
                run,
                EventPayload::ToolCallTerminated {
                    termination: ToolCallTermination {
                        tool_call_id: call,
                        owner,
                        outcome: ToolTerminationOutcome::Completed,
                        result: Some(PersistedToolResult {
                            title: SafeDisplayText::new("Read README.md").expect("title"),
                            output: "contents".into(),
                            metadata: serde_json::json!({}),
                            truncation: None,
                            attachments: Vec::new(),
                        }),
                        error: None,
                    },
                },
            ),
        ];
        let history = assemble_full_history(&events, &store, &binding, "System prompt.")
            .expect("assembled history");
        insta::with_settings!({ prepend_module_to_snapshot => false }, {
            insta::assert_json_snapshot!(
                "cookie_agent_engine__tests__assembled_tool_transcript_snapshot_is_stable",
                history
            );
        });
    }
}
