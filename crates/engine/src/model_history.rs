//! Role-safe Oven history assembly and durable turn conversion.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use cookie_agent_protocol::{
    ApprovalDecisionSource, ArtifactReference, ContextCheckpoint, ContextRehydratedFile,
    DelegatedContextRole, EventPayload, FrozenModelBinding, ModelFinishReason, ModelSelection,
    NativeContextScope, NativeReplayArtifact, PersistedAssistantPart, PersistedContentValue,
    PersistedFilePart, PersistedFileSource, PersistedModelTurn, PersistedToolContent,
    PersistedToolResult, ReplayDecision, ReplayDisposition, ResolvedModelRef, SafeCode,
    SafeErrorMessage, SessionId, Sha256Digest, StoredEvent, ToolAttachment, ToolCallId,
    ToolEmittedContent, ToolEmittedMessage, ToolEmittedMessageRole, ToolTerminationOutcome, Usage,
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

use crate::{ArtifactRouter, goal_projection::GoalProducerProjection};

pub(crate) const COMPACTION_SUMMARY_PREFIX: &str = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n<summary>\n";
pub(crate) const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>\n\nPlease continue the conversation from where we left off without asking the user any further questions.";
pub(crate) const TOOL_EMITTED_SYSTEM_USER_MARKER: &str =
    "[tool-emitted system message; materialized as user history]";

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
    let pinned_agent_md = usize::from(latest_agent_md_event(events).is_some());
    let pinned_count = pinned_agent_md.saturating_add(pinned_skills);
    let mut history = history.iter();
    let mut retained = history.next().cloned().into_iter().collect::<Vec<_>>();
    retained.extend(
        history
            .filter(|turn| !is_framed_summary_turn(turn))
            .take(pinned_count)
            .cloned(),
    );
    if let Some(summary) = summary {
        retained.push(HistoryTurn::user(user_text(&framed_compaction_summary(
            summary,
        ))));
    }
    retained
}

fn is_framed_summary_turn(turn: &HistoryTurn) -> bool {
    matches!(
        turn,
        HistoryTurn::User(message)
            if matches!(message.content.as_slice(), [InputPart::Text(text)]
                if text.text.starts_with(COMPACTION_SUMMARY_PREFIX)
                    && text.text.ends_with(COMPACTION_SUMMARY_SUFFIX))
    )
}

pub(crate) fn tool_output_elision_marker(
    artifact_id: &str,
    original_bytes: u64,
    additional_message_count: usize,
) -> String {
    let mut marker = format!(
        "[tool output elided; retained at artifact://{artifact_id}; {original_bytes} bytes]"
    );
    if additional_message_count > 0 {
        marker.push_str(&format!(
            " {additional_message_count} tool-emitted message(s) were elided with this result and are not recoverable."
        ));
    }
    marker
}

fn retained_artifact_id(reference: &ArtifactReference) -> Result<&str, HistoryError> {
    let digest = reference
        .uri
        .strip_prefix("artifact://sha256/")
        .ok_or_else(|| HistoryError::Corrupt("invalid retained artifact reference".into()))?;
    Sha256Digest::new(digest)
        .map_err(|_| HistoryError::Corrupt("invalid retained artifact digest".into()))?;
    Ok(digest)
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
    mut turn: CompletedTurn,
    store: &ArtifactRouter,
    session: SessionId,
    binding: &FrozenModelBinding,
) -> Result<(PersistedModelTurn, Vec<SafeErrorMessage>), HistoryError> {
    turn.finish
        .provider_metadata
        .remove("cookie_agent.replay_source_wire_model_id");
    if let Some(source) = turn
        .finish
        .native_replay
        .as_ref()
        .and_then(OvenReplayArtifact::source_wire_model_id)
    {
        turn.finish.provider_metadata.insert(
            "cookie_agent.replay_source_wire_model_id".into(),
            serde_json::Value::String(source.as_str().into()),
        );
    }
    let mut warnings = turn
        .warnings
        .iter()
        .map(|warning| {
            SafeErrorMessage::new(sanitize_control_free(warning, SafeErrorMessage::MAX_BYTES))
                .map_err(|error| HistoryError::Corrupt(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let content = turn
        .message
        .content
        .into_iter()
        .map(|part| persist_assistant_part(part, store, session, &mut warnings))
        .collect::<Result<_, _>>()?;
    Ok((
        PersistedModelTurn {
            content,
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
        coalesce_warnings(warnings),
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
    additional_messages: Vec<ToolEmittedMessage>,
    in_stream_result: bool,
}

pub(crate) struct ModelContext {
    pub(crate) history: Vec<HistoryTurn>,
    pub(crate) native_context: Option<OvenNativeContextWindow>,
    pub(crate) replay_decisions: Vec<ReplayDecision>,
}

fn latest_checkpoint(
    events: &[StoredEvent],
) -> Option<&cookie_agent_protocol::ContextCheckpointCommit> {
    events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::ContextCheckpointCommitted { commit } => Some(commit),
        _ => None,
    })
}

fn is_pinned_event(
    event: &StoredEvent,
    latest_agent_md_seq: Option<u64>,
    through_seq: u64,
) -> bool {
    event.seq <= through_seq
        && (matches!(event.payload, EventPayload::SkillLoaded { .. })
            || latest_agent_md_seq == Some(event.seq))
}

#[derive(Clone, Copy)]
enum CheckpointSelectionMode {
    InternalSummary,
    NativeWindow,
}

fn selected_checkpoint_events(
    events: &[StoredEvent],
    source_through_seq: u64,
    recent_from_seq: Option<u64>,
    close_dependencies: bool,
    mode: CheckpointSelectionMode,
) -> Vec<StoredEvent> {
    let latest_agent_md_seq = latest_agent_md_event(events).map(|event| event.seq);
    let pending_producer_admissions = match mode {
        CheckpointSelectionMode::InternalSummary => GoalProducerProjection::from_events(events)
            .messages
            .into_iter()
            .filter(|message| !message.consumed && !message.discarded)
            .filter_map(|message| message.admission.map(|(_, seq)| seq))
            .collect::<HashSet<_>>(),
        CheckpointSelectionMode::NativeWindow => HashSet::new(),
    };
    let mut selected = events
        .iter()
        .filter(|event| {
            is_pinned_event(event, latest_agent_md_seq, source_through_seq)
                || pending_producer_admissions.contains(&event.seq)
                || recent_from_seq
                    .is_some_and(|from| (from..=source_through_seq).contains(&event.seq))
                || event.seq > source_through_seq
        })
        .cloned()
        .collect::<Vec<_>>();
    if close_dependencies {
        close_event_dependencies(events, &mut selected);
    }
    selected.sort_by_key(|event| event.seq);
    selected.dedup_by_key(|event| event.seq);
    selected
}

fn close_event_dependencies(events: &[StoredEvent], selected: &mut Vec<StoredEvent>) {
    // Applied inputs and late tool completions can refer to events before the retained range.
    loop {
        let selected_seqs = selected
            .iter()
            .map(|event| event.seq)
            .collect::<HashSet<_>>();
        let mut required_seqs = HashSet::new();
        for event in selected.iter() {
            match &event.payload {
                EventPayload::UserInputApplied { user_input_seq } => {
                    required_seqs.insert(*user_input_seq);
                }
                EventPayload::ToolCallStarted { start } => {
                    if let Some(commit) = events.iter().rev().find(|candidate| {
                        candidate.seq <= event.seq
                            && candidate.run_id == event.run_id
                            && matches!(&candidate.payload,
                                EventPayload::ModelTurnCommitted { model_turn_seq, turn, .. }
                                    if *model_turn_seq == start.owner.model_turn_seq
                                        && turn.content.iter().any(|part| matches!(part,
                                            PersistedAssistantPart::ToolCall { id, .. }
                                                if id == &start.owner.model_call_id)))
                    }) {
                        required_seqs.insert(commit.seq);
                    }
                }
                EventPayload::ToolCallTerminated { termination } => {
                    if let Some(start) = events.iter().rev().find(|candidate| {
                        candidate.seq <= event.seq
                            && candidate.run_id == event.run_id
                            && matches!(&candidate.payload,
                                EventPayload::ToolCallStarted { start }
                                    if start.tool_call_id == termination.tool_call_id)
                    }) {
                        required_seqs.insert(start.seq);
                    }
                }
                _ => {}
            }
        }
        let additions = events
            .iter()
            .filter(|event| {
                required_seqs.contains(&event.seq) && !selected_seqs.contains(&event.seq)
            })
            .cloned()
            .collect::<Vec<_>>();
        if additions.is_empty() {
            break;
        }
        selected.extend(additions);
    }
}

fn pinned_history_len(events: &[StoredEvent]) -> usize {
    usize::from(latest_agent_md_event(events).is_some())
        + events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::SkillLoaded { .. }))
            .count()
}

fn insert_summary(assembled: &mut AssembledHistory, events: &[StoredEvent], summary: &str) {
    let history_index = 1 + pinned_history_len(events);
    assembled.history.insert(
        history_index,
        HistoryTurn::user(user_text(&framed_compaction_summary(summary))),
    );
    for decision in &mut assembled.replay_decisions {
        if decision.history_index >= history_index as u64 {
            decision.history_index += 1;
        }
    }
}

pub(crate) fn project_summary_context(
    events: &[StoredEvent],
    store: &ArtifactRouter,
    binding: &FrozenModelBinding,
    composed_prompt: &str,
    source_through_seq: u64,
    recent_from_seq: Option<u64>,
    summary: &str,
) -> Result<ModelContext, HistoryError> {
    let selected = selected_checkpoint_events(
        events,
        source_through_seq,
        recent_from_seq,
        true,
        CheckpointSelectionMode::InternalSummary,
    );
    let mut assembled =
        assemble_history_with_replay(&selected, events, store, binding, composed_prompt)?;
    insert_summary(&mut assembled, events, summary);
    Ok(ModelContext {
        history: assembled.history,
        native_context: None,
        replay_decisions: assembled.replay_decisions,
    })
}

pub(crate) fn compaction_prefix_history(
    events: &[StoredEvent],
    store: &ArtifactRouter,
    binding: &FrozenModelBinding,
    composed_prompt: &str,
    recent_from_seq: Option<u64>,
) -> Result<Vec<HistoryTurn>, HistoryError> {
    let Some(recent_from_seq) = recent_from_seq else {
        return Ok(assemble_model_context(events, store, binding, composed_prompt)?.history);
    };
    let (mut visible, prior_summary) = if let Some(commit) = latest_checkpoint(events) {
        let (retained_from, summary, mode) = match &commit.checkpoint {
            ContextCheckpoint::InternalSummary { checkpoint } => (
                commit.boundaries.recent_from_seq,
                Some(checkpoint.summary()),
                CheckpointSelectionMode::InternalSummary,
            ),
            ContextCheckpoint::NativeWindow { .. } => {
                (None, None, CheckpointSelectionMode::NativeWindow)
            }
        };
        (
            selected_checkpoint_events(
                events,
                commit.boundaries.source_through_seq,
                retained_from,
                true,
                mode,
            ),
            summary,
        )
    } else {
        (events.to_vec(), None)
    };
    let latest_agent_md_seq = latest_agent_md_event(events).map(|event| event.seq);
    visible.retain(|event| {
        event.seq < recent_from_seq || is_pinned_event(event, latest_agent_md_seq, u64::MAX)
    });
    close_event_dependencies(events, &mut visible);
    visible.sort_by_key(|event| event.seq);
    visible.dedup_by_key(|event| event.seq);
    let mut assembled =
        assemble_history_with_replay(&visible, events, store, binding, composed_prompt)?;
    if let Some(summary) = prior_summary {
        insert_summary(&mut assembled, events, summary);
    }
    Ok(assembled.history)
}

pub(crate) fn compaction_tail_candidates(events: &[StoredEvent]) -> Vec<u64> {
    #[derive(Default)]
    struct Group {
        start: u64,
        end: u64,
        unresolved_calls: usize,
    }

    let visible = if let Some(commit) = latest_checkpoint(events) {
        let (recent_from, mode) = match &commit.checkpoint {
            ContextCheckpoint::InternalSummary { .. } => (
                commit.boundaries.recent_from_seq,
                CheckpointSelectionMode::InternalSummary,
            ),
            ContextCheckpoint::NativeWindow { .. } => (None, CheckpointSelectionMode::NativeWindow),
        };
        selected_checkpoint_events(
            events,
            commit.boundaries.source_through_seq,
            recent_from,
            true,
            mode,
        )
    } else {
        events.to_vec()
    };
    let visible_seqs = visible
        .iter()
        .map(|event| event.seq)
        .collect::<HashSet<_>>();
    let producer_admissions = GoalProducerProjection::from_events(events)
        .messages
        .into_iter()
        .filter(|message| !message.discarded)
        .filter_map(|message| message.admission.map(|(_, seq)| seq))
        .collect::<HashSet<_>>();
    let submitted = events
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::UserInputSubmitted { .. } => Some(event.seq),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut groups = Vec::<Group>::new();
    let mut pending = HashMap::<
        (
            cookie_agent_protocol::RunId,
            cookie_agent_protocol::ModelCallId,
        ),
        VecDeque<usize>,
    >::new();
    let mut started = HashMap::<(cookie_agent_protocol::RunId, ToolCallId), usize>::new();
    let mut terminated = HashSet::new();

    for event in &visible {
        match &event.payload {
            EventPayload::UserInputApplied { user_input_seq }
                if submitted.contains(user_input_seq) =>
            {
                groups.push(Group {
                    start: *user_input_seq,
                    end: event.seq,
                    ..Group::default()
                });
            }
            EventPayload::MessageInjected { role, .. }
                if *role != cookie_agent_protocol::ExtensionMessageRole::Tool =>
            {
                groups.push(Group {
                    start: event.seq,
                    end: event.seq,
                    ..Group::default()
                });
            }
            EventPayload::ProducerMessageAdmitted { .. }
                if producer_admissions.contains(&event.seq) =>
            {
                groups.push(Group {
                    start: event.seq,
                    end: event.seq,
                    ..Group::default()
                });
            }
            EventPayload::DelegatedContextSeeded { turns, .. } if !turns.is_empty() => {
                groups.push(Group {
                    start: event.seq,
                    end: event.seq,
                    ..Group::default()
                });
            }
            EventPayload::ModelTurnCommitted { turn, .. } => {
                let in_stream_results = turn
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        PersistedAssistantPart::ToolResult { tool_call_id, .. } => {
                            Some(tool_call_id.as_str())
                        }
                        _ => None,
                    })
                    .collect::<HashSet<_>>();
                let index = groups.len();
                let mut unresolved_calls = 0;
                if let Some(run_id) = event.run_id {
                    for id in turn.content.iter().filter_map(|part| match part {
                        PersistedAssistantPart::ToolCall { id, .. }
                            if !in_stream_results.contains(id.as_str()) =>
                        {
                            Some(id)
                        }
                        _ => None,
                    }) {
                        unresolved_calls += 1;
                        pending
                            .entry((run_id, id.clone()))
                            .or_default()
                            .push_back(index);
                    }
                }
                groups.push(Group {
                    start: event.seq,
                    end: event.seq,
                    unresolved_calls,
                });
            }
            EventPayload::ToolCallStarted { start } => {
                let Some(run_id) = event.run_id else { continue };
                if let Some(index) = pending
                    .get_mut(&(run_id, start.owner.model_call_id.clone()))
                    .and_then(VecDeque::pop_front)
                {
                    started.insert((run_id, start.tool_call_id), index);
                }
            }
            EventPayload::ToolCallTerminated { termination } => {
                let Some(run_id) = event.run_id else { continue };
                if terminated.insert((run_id, termination.tool_call_id))
                    && let Some(index) = started.get(&(run_id, termination.tool_call_id))
                {
                    groups[*index].unresolved_calls =
                        groups[*index].unresolved_calls.saturating_sub(1);
                    groups[*index].end = groups[*index].end.max(event.seq);
                }
            }
            EventPayload::ContextRehydrated { .. }
            | EventPayload::PluginEventAdded { .. }
            | EventPayload::DelegateFinished { .. }
            | EventPayload::DelegateFinishedV2 { .. } => groups.push(Group {
                start: event.seq,
                end: event.seq,
                ..Group::default()
            }),
            _ => {}
        }
    }

    groups.sort_by_key(|group| group.start);
    groups.dedup_by_key(|group| group.start);
    let unresolved_from = groups
        .iter()
        .filter(|group| group.unresolved_calls != 0)
        .map(|group| group.start)
        .min();
    let spans = groups
        .iter()
        .filter(|group| group.end > group.start)
        .map(|group| (group.start, group.end))
        .collect::<Vec<_>>();
    groups
        .into_iter()
        .skip(1)
        .filter(|group| unresolved_from.is_none_or(|start| group.start <= start))
        .filter(|group| {
            !spans
                .iter()
                .any(|(start, end)| *start < group.start && group.start <= *end)
        })
        .filter(|group| visible_seqs.contains(&group.start))
        .map(|group| group.start)
        .collect()
}

struct AssembledHistory {
    history: Vec<HistoryTurn>,
    replay_decisions: Vec<ReplayDecision>,
}

pub(crate) fn assemble_model_context(
    events: &[StoredEvent],
    store: &ArtifactRouter,
    binding: &FrozenModelBinding,
    composed_prompt: &str,
) -> Result<ModelContext, HistoryError> {
    let checkpoint = latest_checkpoint(events);
    let Some(commit) = checkpoint else {
        let assembled =
            assemble_history_with_replay(events, events, store, binding, composed_prompt)?;
        return Ok(ModelContext {
            history: assembled.history,
            native_context: None,
            replay_decisions: assembled.replay_decisions,
        });
    };
    match &commit.checkpoint {
        ContextCheckpoint::InternalSummary { checkpoint } => project_summary_context(
            events,
            store,
            binding,
            composed_prompt,
            commit.boundaries.source_through_seq,
            commit.boundaries.recent_from_seq,
            checkpoint.summary(),
        ),
        ContextCheckpoint::NativeWindow { window } => {
            commit
                .validate_for_binding(binding)
                .map_err(|error| HistoryError::Corrupt(error.to_string()))?;
            let selected = selected_checkpoint_events(
                events,
                commit.boundaries.source_through_seq,
                None,
                false,
                CheckpointSelectionMode::NativeWindow,
            );
            let assembled =
                assemble_history_with_replay(&selected, events, store, binding, composed_prompt)?;
            Ok(ModelContext {
                history: assembled.history,
                native_context: Some(restore_native_context(window, binding)?),
                replay_decisions: assembled.replay_decisions,
            })
        }
    }
}

pub(crate) fn assemble_full_history(
    events: &[StoredEvent],
    store: &ArtifactRouter,
    binding: &FrozenModelBinding,
    composed_prompt: &str,
) -> Result<Vec<HistoryTurn>, HistoryError> {
    Ok(assemble_history_with_replay(events, events, store, binding, composed_prompt)?.history)
}

// Message selection must not roll back session-wide replay decisions or pinned context.
// context_events is the full visible snapshot, before checkpoint or summary-prefix filtering.
fn assemble_history_with_replay(
    events: &[StoredEvent],
    context_events: &[StoredEvent],
    store: &ArtifactRouter,
    binding: &FrozenModelBinding,
    composed_prompt: &str,
) -> Result<AssembledHistory, HistoryError> {
    let producer_projection = GoalProducerProjection::from_events(context_events);
    let current_run = context_events.iter().rev().find_map(|event| {
        matches!(event.payload, EventPayload::RunStarted { .. })
            .then_some(event.run_id)
            .flatten()
    });
    let producer_admissions = producer_projection
        .messages
        .iter()
        .filter(|message| !message.discarded)
        .filter_map(|message| {
            message
                .admission
                .and_then(|(admission_run, admission_seq)| {
                    (message.consumed || current_run.is_none_or(|run| admission_run == run))
                        .then_some((admission_seq, message.body.as_str()))
                })
        })
        .collect::<HashMap<_, _>>();
    let producer_delegations = producer_projection
        .messages
        .iter()
        .filter_map(|message| match &message.producer_owner {
            cookie_agent_protocol::ProducerOwner::Delegation { invocation_id } => {
                Some(*invocation_id)
            }
            _ => None,
        })
        .collect::<HashSet<_>>();
    let loaded_skills = context_events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::SkillLoaded { rendered_body, .. } => Some(rendered_body.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let loaded_agent_md = latest_agent_md_event(context_events).and_then(|event| {
        let EventPayload::AgentMdLoaded { entries } = &event.payload else {
            return None;
        };
        Some(agent_md_turn(entries))
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
    let delegation_children = context_events
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::ToolCallLinked {
                tool_call_id,
                child_session_id,
            } => Some(((event.run_id, tool_call_id), child_session_id)),
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    let elisions = context_events
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
            EventPayload::ProducerMessageAdmitted { .. } => {
                if let Some(body) = producer_admissions.get(&envelope.seq) {
                    logical.push(LogicalTurn::User(user_text(body)));
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
                            additional_messages: Vec::new(),
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
                if termination.outcome == ToolTerminationOutcome::Completed
                    || termination
                        .result
                        .as_ref()
                        .and_then(|result| result.retained_output.as_ref())
                        .is_some_and(|output| output.incomplete)
                    // Opt-out pages have no retained_output. Only the commit boundary's
                    // finalized-result reason admits them after cancellation.
                    || (termination.outcome == ToolTerminationOutcome::Cancelled
                        && termination.result.is_some()
                        && termination.error.as_ref().is_some_and(|error| {
                            error.code.as_str() == crate::runtime::CANCELLED_AFTER_COMPLETION
                        }))
                    || (termination.outcome == ToolTerminationOutcome::Cancelled
                        && termination
                            .result
                            .as_ref()
                            .zip(
                                delegation_children
                                    .get(&(envelope.run_id, termination.tool_call_id)),
                            )
                            .is_some_and(|(result, child)| {
                                crate::delegation_api::delegate_result_matches_child(result, *child)
                            })) =>
            {
                if let Some(result) = &termination.result {
                    let (mut result_part, additional_messages) =
                        if let Some((original_bytes, retained)) =
                            elisions.get(&termination.tool_call_id)
                        {
                            (
                                ToolResultPart::new(
                                    String::new(),
                                    ToolContent::Text(tool_output_elision_marker(
                                        retained_artifact_id(retained)?,
                                        *original_bytes,
                                        result.additional_messages.len(),
                                    )),
                                ),
                                Vec::new(),
                            )
                        } else {
                            (
                                tool_result_part(result, store)?,
                                result.additional_messages.clone(),
                            )
                        };
                    result_part.is_error = termination.outcome != ToolTerminationOutcome::Completed;
                    if let Some(error) = &termination.error {
                        match &mut result_part.content {
                            ToolContent::Mixed(values) => {
                                values.push(ContentValue::Text(error.message.to_string()))
                            }
                            ToolContent::Text(text) => {
                                text.push('\n');
                                text.push_str(error.message.as_str());
                            }
                            _ => {}
                        }
                    }
                    attach_result(
                        &mut logical,
                        &engine_calls,
                        envelope.run_id,
                        termination.tool_call_id,
                        result_part,
                        additional_messages,
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
                    Vec::new(),
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
                short_id,
                status,
                preview,
                total_lines,
            } => {
                logical.push(LogicalTurn::User(user_text(
                    &crate::runtime::render_subagent_notification(
                        preview,
                        *status,
                        *total_lines,
                        short_id.as_deref().unwrap_or(&session_id.to_string()),
                    ),
                )));
            }
            EventPayload::DelegateFinishedV2 {
                invocation_id,
                session_id,
                short_id,
                status,
                preview,
                total_lines,
                ..
            } if !producer_delegations.contains(invocation_id) => {
                logical.push(LogicalTurn::User(user_text(
                    &crate::runtime::render_subagent_notification(
                        preview,
                        *status,
                        *total_lines,
                        short_id.as_deref().unwrap_or(&session_id.to_string()),
                    ),
                )));
            }
            _ => {}
        }
    }

    let mut history = vec![HistoryTurn::system(SystemMessage::new(vec![
        SystemPart::Text(TextPart::new(composed_prompt)),
    ]))];
    if let Some(agent_md) = loaded_agent_md {
        history.push(HistoryTurn::user(user_text(&agent_md)));
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
                let Some(HistoryTurn::System(message)) = history.first_mut() else {
                    unreachable!("assembled history starts with a system message")
                };
                message.content.push(SystemPart::Text(TextPart::new(text)));
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
                    .filter_map(|call| call.result.map(|result| (result, call.additional_messages)))
                    .collect::<Vec<_>>();
                if !results.is_empty() {
                    if !has_content {
                        return Err(HistoryError::Corrupt(
                            "tool result has no retained assistant call".into(),
                        ));
                    }
                    let call_order = assistant
                        .turn
                        .content
                        .iter()
                        .enumerate()
                        .filter_map(|(index, part)| match part {
                            PersistedAssistantPart::ToolCall { id, .. } => {
                                Some((id.as_str(), index))
                            }
                            _ => None,
                        })
                        .collect::<HashMap<_, _>>();
                    results.sort_by_key(|(result, _)| {
                        call_order
                            .get(result.tool_call_id.as_str())
                            .copied()
                            .unwrap_or(usize::MAX)
                    });
                    let mut additional_messages = Vec::new();
                    let results = results
                        .into_iter()
                        .map(|(result, messages)| {
                            additional_messages.extend(messages);
                            result
                        })
                        .collect();
                    history.push(HistoryTurn::tool(ToolMessage::new(results)));
                    for message in additional_messages {
                        append_tool_emitted_message(&mut history, &message, store)?;
                    }
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

fn latest_agent_md_event(events: &[StoredEvent]) -> Option<&StoredEvent> {
    let latest_run = events.iter().rev().find_map(|event| {
        matches!(event.payload, EventPayload::RunStarted { .. }).then_some(event.run_id)
    });
    events.iter().rev().find(|event| {
        matches!(event.payload, EventPayload::AgentMdLoaded { .. })
            && latest_run.is_none_or(|run| event.run_id == run)
    })
}

fn agent_md_turn(entries: &[cookie_agent_protocol::AgentMdEntry]) -> String {
    entries
        .iter()
        .map(|entry| {
            let source = escape_xml_attribute(entry.source.as_str());
            let marker = entry.truncated.then(|| {
                format!(
                    "\n[AGENTS.md context truncated; original size: {} bytes]",
                    entry.original_bytes
                )
            });
            format!(
                "<agent_md source=\"{source}\">\n{}{}\n</agent_md>",
                entry.content,
                marker.as_deref().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
pub(crate) fn agent_md_turn_for_test(entries: &[cookie_agent_protocol::AgentMdEntry]) -> String {
    agent_md_turn(entries)
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
    additional_messages: Vec<ToolEmittedMessage>,
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
    assistant.calls[call_index].additional_messages = additional_messages;
    Ok(())
}

fn append_tool_emitted_message(
    history: &mut Vec<HistoryTurn>,
    message: &ToolEmittedMessage,
    store: &ArtifactRouter,
) -> Result<(), HistoryError> {
    let mut content = Vec::with_capacity(
        message.content.len() + usize::from(message.role == ToolEmittedMessageRole::System),
    );
    if message.role == ToolEmittedMessageRole::System {
        content.push(InputPart::Text(TextPart::new(
            TOOL_EMITTED_SYSTEM_USER_MARKER,
        )));
    }
    content.extend(
        message
            .content
            .iter()
            .map(|part| match part {
                ToolEmittedContent::Text(text) => Ok(InputPart::Text(TextPart::new(text))),
                ToolEmittedContent::File(attachment) => {
                    attachment_file(attachment, store).map(InputPart::File)
                }
            })
            .collect::<Result<Vec<_>, _>>()?,
    );
    history.push(HistoryTurn::user(UserMessage::new(content)));
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
    store: &ArtifactRouter,
) -> Result<ToolResultPart, HistoryError> {
    let truncation = if let Some(truncation) = &result.truncation {
        let artifact_id = retained_artifact_id(&truncation.retained)?;
        Some(serde_json::json!({
            "original_bytes":truncation.original_bytes,
            "original_lines":truncation.original_lines,
            "artifact_id":artifact_id,
            "read_more":{
                "tool":"read",
                "arguments":{"filePath":format!("artifact://{artifact_id}")}
            }
        }))
    } else {
        None
    };
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
    store: &ArtifactRouter,
) -> Result<FilePart, HistoryError> {
    let bytes = store.read_verified_attachment(attachment)?;
    Ok(FilePart {
        media_type: attachment.mime_type.to_string(),
        filename: attachment.filename.clone(),
        source: FileSource::Bytes(bytes),
        metadata: None,
    })
}

fn persist_assistant_part(
    part: AssistantPart,
    store: &ArtifactRouter,
    session: SessionId,
    warnings: &mut Vec<SafeErrorMessage>,
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
            name: normalize_safe_code(part.name, "tool call name", warnings),
            input: part.input,
            raw_input: part.raw_input,
            metadata: part.metadata,
        },
        AssistantPart::ToolResult(part) => PersistedAssistantPart::ToolResult {
            tool_call_id: cookie_agent_protocol::ModelCallId::new(part.tool_call_id)
                .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
            content: persist_tool_content(part.content, store, session)?,
            is_error: part.is_error,
            metadata: part.metadata,
        },
        AssistantPart::File(file) => PersistedAssistantPart::File {
            file: persist_file(file, store, session)?,
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
            kind: normalize_safe_code(part.kind, "custom part kind", warnings),
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
    store: &ArtifactRouter,
    session: SessionId,
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
                        file: persist_file(file, store, session)?,
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

fn persist_file(
    file: FilePart,
    store: &ArtifactRouter,
    session: SessionId,
) -> Result<PersistedFilePart, HistoryError> {
    let source = match file.source {
        FileSource::Bytes(bytes) => persisted_artifact(store, session, &bytes)?,
        FileSource::Text(text) => persisted_artifact(store, session, text.as_bytes())?,
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
    store: &ArtifactRouter,
    session: SessionId,
    bytes: &[u8],
) -> Result<PersistedFileSource, HistoryError> {
    let (reference, sha256) = store.retain(session, bytes)?;
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
    store: &ArtifactRouter,
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
    let wire_model = binding
        .descriptor
        .provider_metadata
        .get("cookie_agent.wire_model_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(binding.descriptor.identity.model_id.as_str());
    let wire_provider = binding
        .descriptor
        .provider_metadata
        .get("cookie_agent.wire_provider_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(binding.descriptor.identity.provider_id.as_str());
    if artifact.adapter_id() != &binding.descriptor.adapter_id
        || artifact.scope().provider_id.as_str() != wire_provider
        || artifact.scope().model_id.as_str() != wire_model
    {
        return Err(HistoryError::Corrupt(
            "native replay artifact does not match its exact frozen model binding".into(),
        ));
    }
    let scope = NativeContextScope {
        provider_id: binding.selection.model.provider_id(),
        model_id: binding.selection.model.model_id(),
        resource_id: cookie_agent_protocol::SafeDisplayText::new(
            artifact.scope().resource_id.as_str(),
        )
        .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
    };
    NativeReplayArtifact::new(
        cookie_agent_protocol::SafeCode::new(artifact.adapter_id().as_str())
            .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
        cookie_agent_protocol::Sha256Digest::new(binding.selection_fingerprint.as_str())
            .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
        scope,
        artifact.payload().clone(),
    )
    .map_err(|error| HistoryError::Corrupt(error.to_string()))
}

fn restore_replay(
    artifact: &NativeReplayArtifact,
    resolved_model: &ResolvedModelRef,
    _binding: &FrozenModelBinding,
    source_wire_model_id: Option<&str>,
) -> (Option<OvenReplayArtifact>, Option<ReplayDisposition>) {
    // Persisted-turn integrity: the artifact must have been recorded by the
    // adapter the turn itself resolved to, before any current-eligibility
    // checks expose its opaque payload. Artifacts record the Oven adapter ID
    // while the persisted turn carries the protocol adapter ID, so compare
    // through the shared family mapping.
    if cookie_agent_models::adapters::wire_adapter_for_protocol(artifact.adapter_id().as_str())
        .is_none()
        || crate::policy::wire_adapter(artifact.adapter_id().as_str()) != resolved_model.adapter_id
    {
        return (
            None,
            Some(ReplayDisposition::DiscardedInvalidPayload {
                reason: safe_replay_reason(
                    "native replay adapter does not match its persisted model turn",
                ),
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
    // Source attribution is integrity data. Target codecs decide block eligibility.
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
    )
    .and_then(|artifact| match source_wire_model_id {
        Some(source) => artifact.with_source_wire_model_id(oven_sdk::ModelId::new(source)),
        None => Ok(artifact),
    }) {
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
        NativeContextScope {
            provider_id: binding.selection.model.provider_id(),
            model_id: binding.selection.model.model_id(),
            resource_id: cookie_agent_protocol::SafeDisplayText::new(
                window.scope().resource_id.as_str(),
            )
            .map_err(|error| HistoryError::Corrupt(error.to_string()))?,
        },
        window.payload().clone(),
    )
    .map_err(|error| HistoryError::Corrupt(error.to_string()))
}

fn restore_native_context(
    window: &cookie_agent_protocol::NativeContextWindow,
    binding: &FrozenModelBinding,
) -> Result<OvenNativeContextWindow, HistoryError> {
    let identity = |key: &str, fallback: &str| -> Result<String, HistoryError> {
        match binding.descriptor.provider_metadata.get(key) {
            None => Ok(fallback.into()),
            Some(serde_json::Value::String(value)) => Ok(value.clone()),
            Some(_) => Err(HistoryError::Corrupt(
                "invalid frozen wire identity metadata".into(),
            )),
        }
    };
    let scope = OvenNativeContextScope::new(
        ProviderId::new(identity(
            "cookie_agent.wire_provider_id",
            binding.descriptor.identity.provider_id.as_str(),
        )?),
        oven_sdk::ModelId::new(identity(
            "cookie_agent.wire_model_id",
            binding.descriptor.identity.model_id.as_str(),
        )?),
        ResourceId::new(window.scope().resource_id.as_str())?,
    )?;
    OvenNativeContextWindow::new(
        AdapterId::new(window.adapter_id().as_str()),
        scope,
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
    store: &ArtifactRouter,
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
    store: &ArtifactRouter,
    binding: &FrozenModelBinding,
) -> Result<(CompletedTurn, Option<ReplayDisposition>), HistoryError> {
    let (native_replay, replay_disposition) =
        turn.native_replay
            .as_ref()
            .map_or((None, None), |artifact| {
                restore_replay(
                    artifact,
                    resolved_model,
                    binding,
                    turn.provider_metadata
                        .get("cookie_agent.replay_source_wire_model_id")
                        .and_then(serde_json::Value::as_str),
                )
            });
    // A model switch may project normalized reasoning only when the target
    // declares support for replaying provider-authoritative reasoning.
    let preserve_reasoning =
        native_replay.is_some() || binding.descriptor.capabilities.replay.reasoning;
    Ok((
        CompletedTurn {
            message: AssistantMessage {
                content: turn
                    .content
                    .iter()
                    .filter(|part| {
                        preserve_reasoning
                            || !matches!(part, PersistedAssistantPart::Reasoning { .. })
                    })
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

/// Normalize a model-supplied identifier into a valid [`SafeCode`].
///
/// Providers can emit tool-call names and custom part kinds that are not valid
/// `SafeCode` values. Stored history cannot represent those, so coerce them to a
/// valid placeholder and record a user-visible warning instead of failing the run.
fn normalize_safe_code(
    value: String,
    label: &str,
    warnings: &mut Vec<SafeErrorMessage>,
) -> SafeCode {
    if is_valid_safe_code(&value) {
        return SafeCode::new(value).expect("validated model identifier");
    }
    let normalized = normalized_code(&value);
    let original = sanitize_control_free(&value, SafeErrorMessage::MAX_BYTES);
    warnings.push(
        SafeErrorMessage::new(sanitize_control_free(
            &format!(
                "model {label} \"{original}\" was normalized to \"{normalized}\" because it is not a valid identifier"
            ),
            SafeErrorMessage::MAX_BYTES,
        ))
        .expect("sanitized normalization warning is valid"),
    );
    SafeCode::new(normalized).expect("normalized model identifier is valid")
}

/// Mirrors [`SafeCode::new`] without consuming the caller's string, so valid
/// identifiers move into the [`SafeCode`] without an extra allocation.
fn is_valid_safe_code(value: &str) -> bool {
    (1..=SafeCode::MAX_BYTES).contains(&value.len())
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        })
}

/// Deduplicate and bound warnings so the persisted [`PersistedModelTurn`] event
/// satisfies `ModelTurnCommitted`'s 256-warning limit. Identical warnings
/// collapse first; if the remainder still exceeds the cap, keep the earliest
/// warnings and append one deterministic truncation summary.
fn coalesce_warnings(warnings: Vec<SafeErrorMessage>) -> Vec<SafeErrorMessage> {
    const MAX_WARNINGS: usize = 256;
    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(warnings.len());
    for warning in warnings {
        if seen.insert(warning.clone()) {
            deduped.push(warning);
        }
    }
    if deduped.len() <= MAX_WARNINGS {
        return deduped;
    }
    let omitted = deduped.len() - (MAX_WARNINGS - 1);
    deduped.truncate(MAX_WARNINGS - 1);
    deduped.push(
        SafeErrorMessage::new(format!("additional warnings truncated ({omitted} omitted)"))
            .expect("bounded truncation summary is valid"),
    );
    deduped
}

fn normalized_code(value: &str) -> String {
    let mut code = value
        .bytes()
        .take(SafeCode::MAX_BYTES)
        .map(|byte| {
            if byte.is_ascii_alphanumeric() {
                char::from(byte.to_ascii_lowercase())
            } else {
                '_'
            }
        })
        .collect::<String>();
    if code.is_empty() {
        code.push_str("unnamed-tool");
    }
    if !code.as_bytes()[0].is_ascii_lowercase() && !code.as_bytes()[0].is_ascii_digit() {
        code.insert(0, 'x');
    }
    code.truncate(SafeCode::MAX_BYTES);
    code
}

#[cfg(test)]
mod tests;
