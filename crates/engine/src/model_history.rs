//! Role-safe Oven history assembly and durable turn conversion.

use std::collections::{BTreeMap, HashMap, VecDeque};

use bytes::Bytes;
use cookie_agent_models::FrozenModelBinding;
use cookie_agent_protocol::{
    ApprovalDecisionSource, ContextCheckpoint, Event, EventEnvelope, ModelFinishReason, ModelRef,
    NativeContextScope, NativeReplayArtifact, PersistedAssistantPart, PersistedContentValue,
    PersistedFilePart, PersistedFileSource, PersistedModelTurn, PersistedToolContent,
    ReplayDecision, ReplayDisposition, Sha256Digest, ToolAttachment, ToolCallId, ToolResult, Usage,
};
use oven_sdk::{
    AdapterId, AssistantMessage, AssistantPart, CompletedTurn, ContentValue, CustomPart, FilePart,
    FileSource, Finish, FinishReason, HistoryTurn, InputPart, ModelError,
    NativeContextScope as OvenNativeContextScope, NativeContextWindow,
    NativeReplayArtifact as OvenReplayArtifact, ProviderId, ReasoningPart,
    ReplayDecision as OvenReplayDecision, ReplayDisposition as OvenReplayDisposition, ResourceId,
    SourcePart, TextPart, ToolApprovalPart, ToolCallPart, ToolContent, ToolMessage, ToolResultPart,
    UserMessage,
};
use thiserror::Error;

use crate::ArtifactStore;

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
pub(crate) fn wire_model(binding: &FrozenModelBinding) -> ModelRef {
    let descriptor = &binding.descriptor;
    ModelRef {
        name: binding.alias.clone(),
        provider_id: descriptor.identity.provider_id.as_str().to_owned(),
        model_id: descriptor.identity.model_id.as_str().to_owned(),
        adapter_id: descriptor.adapter_id.as_str().to_owned(),
    }
}

pub(crate) fn persist_turn(
    turn: CompletedTurn,
    store: &ArtifactStore,
) -> Result<PersistedModelTurn, HistoryError> {
    Ok(PersistedModelTurn {
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
        warnings: turn.warnings,
        native_replay: turn.finish.native_replay.map(persist_replay).transpose()?,
    })
}

pub(crate) fn replay_decisions(decisions: &[OvenReplayDecision]) -> Vec<ReplayDecision> {
    decisions
        .iter()
        .map(|decision| ReplayDecision {
            history_index: decision.history_index as u64,
            disposition: match &decision.disposition {
                OvenReplayDisposition::Replayed => ReplayDisposition::Replayed,
                OvenReplayDisposition::NoArtifact => ReplayDisposition::NoArtifact,
                OvenReplayDisposition::DiscardedForeignAdapter { found, expected } => {
                    ReplayDisposition::DiscardedForeignAdapter {
                        found: found.as_str().to_owned(),
                        expected: expected.as_str().to_owned(),
                    }
                }
                OvenReplayDisposition::DiscardedForeignScope { found, expected } => {
                    ReplayDisposition::DiscardedForeignScope {
                        found: persist_scope(found),
                        expected: persist_scope(expected),
                    }
                }
                OvenReplayDisposition::DiscardedInvalidPayload { reason } => {
                    ReplayDisposition::DiscardedInvalidPayload {
                        reason: reason.clone(),
                    }
                }
                OvenReplayDisposition::ReconstructedNormalized => {
                    ReplayDisposition::ReconstructedNormalized
                }
            },
        })
        .collect()
}

#[derive(Clone)]
enum LogicalTurn {
    User(UserMessage),
    Assistant(Box<AssistantRecord>),
}

#[derive(Clone)]
struct AssistantRecord {
    turn: PersistedModelTurn,
    run_id: Option<cookie_agent_protocol::RunId>,
    calls: Vec<CallRecord>,
}

#[derive(Clone)]
struct CallRecord {
    model_call_id: String,
    engine_call_id: Option<ToolCallId>,
    result: Option<ToolResultPart>,
    in_stream_result: bool,
}

pub(crate) struct ModelContext {
    pub(crate) history: Vec<HistoryTurn>,
    pub(crate) native_context: Option<NativeContextWindow>,
}

pub(crate) fn assemble_model_context(
    events: &[EventEnvelope],
    store: &ArtifactStore,
    binding: &FrozenModelBinding,
) -> Result<ModelContext, HistoryError> {
    let checkpoint = events.iter().rev().find_map(|event| match &event.event {
        Event::ContextCheckpointCommitted { commit } => Some(commit),
        _ => None,
    });
    let Some(commit) = checkpoint else {
        return Ok(ModelContext {
            history: assemble_history(events, store)?,
            native_context: None,
        });
    };
    let after = events
        .iter()
        .filter(|event| event.seq > commit.boundaries().source_through_seq)
        .cloned()
        .collect::<Vec<_>>();
    match commit.checkpoint() {
        ContextCheckpoint::InternalSummary { checkpoint } => {
            let mut history = assemble_history(&after, store)?;
            history.insert(0, HistoryTurn::user(user_text(checkpoint.summary())));
            Ok(ModelContext {
                history,
                native_context: None,
            })
        }
        ContextCheckpoint::ProviderNative {
            model,
            native_context,
        } if model == &wire_model(binding)
            && native_context.adapter_id == binding.descriptor.adapter_id.as_str()
            && native_context.scope.provider_id
                == binding.descriptor.identity.provider_id.as_str()
            && native_context.scope.model_id == binding.descriptor.identity.model_id.as_str()
            && !native_context.scope.resource_id.is_empty() =>
        {
            let payload = store.read_verified_native_context(native_context)?;
            let window = serde_json::from_str::<NativeContextWindow>(&payload)
                .map_err(|error| HistoryError::Corrupt(error.to_string()))?;
            let expected_scope = restore_scope(&native_context.scope)?;
            if window.adapter_id() != &AdapterId::new(native_context.adapter_id.clone())
                || window.scope() != &expected_scope
            {
                return Err(HistoryError::Corrupt(
                    "native context artifact metadata does not match its exact private scope"
                        .into(),
                ));
            }
            Ok(ModelContext {
                history: assemble_history(&after, store)?,
                native_context: Some(window),
            })
        }
        ContextCheckpoint::ProviderNative { .. } => Ok(ModelContext {
            history: assemble_history(events, store)?,
            native_context: None,
        }),
    }
}

pub(crate) fn assemble_history(
    events: &[EventEnvelope],
    store: &ArtifactStore,
) -> Result<Vec<HistoryTurn>, HistoryError> {
    let mut logical = Vec::<LogicalTurn>::new();
    let mut submitted = HashMap::<u64, String>::new();
    let mut pending_model_calls =
        HashMap::<(cookie_agent_protocol::RunId, String), VecDeque<(usize, usize)>>::new();
    let mut engine_calls =
        HashMap::<(cookie_agent_protocol::RunId, ToolCallId), (usize, usize)>::new();

    for envelope in events {
        match &envelope.event {
            Event::RunStarted { input, .. } => logical.push(LogicalTurn::User(user_text(input))),
            Event::UserInputSubmitted { input } => {
                submitted.insert(envelope.seq, input.clone());
            }
            Event::UserInputApplied { user_input_seq } => {
                if let Some(input) = submitted.remove(user_input_seq) {
                    logical.push(LogicalTurn::User(user_text(&input)));
                }
            }
            Event::ModelTurnCommitted { turn, .. } => {
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
                    run_id: envelope.run_id,
                    calls,
                })));
            }
            Event::ToolCallStarted {
                tool_call_id,
                model_call_id,
                ..
            } => {
                let Some(run_id) = envelope.run_id else {
                    continue;
                };
                let occurrence = pending_model_calls
                    .get_mut(&(run_id, model_call_id.clone()))
                    .and_then(VecDeque::pop_front);
                if let Some((logical_index, call_index)) = occurrence {
                    let LogicalTurn::Assistant(assistant) = &mut logical[logical_index] else {
                        return Err(HistoryError::Corrupt(
                            "tool call mapped to a non-assistant turn".into(),
                        ));
                    };
                    assistant.calls[call_index].engine_call_id = Some(*tool_call_id);
                    engine_calls.insert((run_id, *tool_call_id), (logical_index, call_index));
                }
            }
            Event::ToolCallCompleted {
                tool_call_id,
                result,
            } => {
                attach_result(
                    &mut logical,
                    &engine_calls,
                    envelope.run_id,
                    *tool_call_id,
                    tool_result_part(result, store)?,
                )?;
            }
            Event::ToolCallFailed {
                tool_call_id,
                message,
                ..
            } => {
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
                    (ToolContent::Text(message.clone()), None)
                };
                attach_result(
                    &mut logical,
                    &engine_calls,
                    envelope.run_id,
                    *tool_call_id,
                    ToolResultPart {
                        tool_call_id: String::new(),
                        content,
                        is_error: true,
                        metadata,
                    },
                )?;
            }
            _ => {}
        }
    }

    let mut history = Vec::new();
    for turn in logical {
        match turn {
            LogicalTurn::User(user) => history.push(HistoryTurn::user(user)),
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
                if retained.len() != original_call_count {
                    assistant.turn.native_replay = None;
                }
                let has_content = !assistant.turn.content.is_empty();
                if has_content {
                    history.push(HistoryTurn::assistant(restore_turn_with_store(
                        &assistant.turn,
                        store,
                    )?));
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
                                matches!(part, PersistedAssistantPart::ToolCall { id, .. } if id == &result.tool_call_id)
                            })
                            .unwrap_or(usize::MAX)
                    });
                    history.push(HistoryTurn::tool(ToolMessage::new(results)));
                }
                let _ = assistant.run_id;
            }
        }
    }
    Ok(history)
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
    result.tool_call_id = assistant.calls[call_index].model_call_id.clone();
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
    result: &ToolResult,
    store: &ArtifactStore,
) -> Result<ToolResultPart, HistoryError> {
    let mut values = vec![
        ContentValue::Text(result.output.clone()),
        ContentValue::Json(serde_json::json!({
            "title": result.title,
            "metadata": result.metadata,
            "truncation": result.truncation,
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
        media_type: attachment.mime_type.clone(),
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
            id: part.id,
            provider_item_id: part.provider_item_id,
            name: part.name,
            input: part.input,
            raw_input: part.raw_input,
            metadata: part.metadata,
        },
        AssistantPart::ToolResult(part) => PersistedAssistantPart::ToolResult {
            tool_call_id: part.tool_call_id,
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
            tool_call_id: part.tool_call_id,
            message: part.message,
            metadata: part.metadata,
        },
        AssistantPart::Custom(part) => PersistedAssistantPart::Custom {
            kind: part.kind,
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
            id: id.clone(),
            provider_item_id: provider_item_id.clone(),
            name: name.clone(),
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
            tool_call_id: tool_call_id.clone(),
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
            tool_call_id: tool_call_id.clone(),
            message: message.clone(),
            metadata: metadata.clone(),
        }),
        PersistedAssistantPart::Custom {
            kind,
            data,
            metadata,
        } => AssistantPart::Custom(CustomPart {
            kind: kind.clone(),
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
            provider_id: provider.as_str().to_owned(),
            id,
        },
    };
    Ok(PersistedFilePart {
        media_type: file.media_type,
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
                provider: ProviderId::new(provider_id.clone()),
                id: id.clone(),
            }
        }
    };
    Ok(FilePart {
        media_type: file.media_type.clone(),
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

fn persist_replay(artifact: OvenReplayArtifact) -> Result<NativeReplayArtifact, HistoryError> {
    NativeReplayArtifact::new(
        artifact.adapter_id().as_str().to_owned(),
        persist_scope(artifact.scope()),
        artifact.payload().clone(),
    )
    .map_err(|error| HistoryError::Corrupt(error.to_string()))
}

fn restore_replay(artifact: &NativeReplayArtifact) -> Result<OvenReplayArtifact, HistoryError> {
    OvenReplayArtifact::new(
        AdapterId::new(artifact.adapter_id().to_owned()),
        restore_scope(artifact.scope())?,
        artifact.payload().clone(),
    )
    .map_err(|error| HistoryError::Corrupt(error.to_string()))
}

fn persist_scope(scope: &OvenNativeContextScope) -> NativeContextScope {
    NativeContextScope {
        provider_id: scope.provider_id.as_str().to_owned(),
        model_id: scope.model_id.as_str().to_owned(),
        resource_id: scope.resource_id.as_str().to_owned(),
    }
}

fn restore_scope(scope: &NativeContextScope) -> Result<OvenNativeContextScope, HistoryError> {
    OvenNativeContextScope::new(
        ProviderId::new(scope.provider_id.clone()),
        oven_sdk::ModelId::new(scope.model_id.clone()),
        ResourceId::new(scope.resource_id.clone())?,
    )
    .map_err(HistoryError::from)
}

fn persist_usage(usage: oven_sdk::Usage) -> Usage {
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
                tool_call_id: tool_call_id.clone(),
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
    store: &ArtifactStore,
) -> Result<CompletedTurn, HistoryError> {
    Ok(CompletedTurn {
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
            native_replay: turn
                .native_replay
                .as_ref()
                .map(restore_replay)
                .transpose()?,
        },
        warnings: turn.warnings.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cookie_agent_protocol::{
        AgentType, DepthLimit, Event, EventEnvelope, EventSchemaVersion, ModelFinishReason,
        ModelRef, NativeContextScope, NativeReplayArtifact, PersistedAssistantPart,
        PersistedModelTurn, ProfileIdentity, ProfileSnapshot, ReplayDisposition, RunId, SessionId,
        ToolCallId, ToolResult, Usage,
    };
    use oven_sdk::{
        NativeContextScope as OvenNativeContextScope, ReplayDecision as OvenReplayDecision,
        ReplayDisposition as OvenReplayDisposition, ResourceId,
    };

    use super::{assemble_history, replay_decisions};
    use crate::ArtifactStore;

    #[test]
    fn normalized_history_keeps_native_artifacts_for_adapter_scoped_replay() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::open(directory.path().join("artifacts")).expect("store");
        let artifact = NativeReplayArtifact::new(
            "adapter.one".into(),
            NativeContextScope {
                provider_id: "provider".into(),
                model_id: "model".into(),
                resource_id: "resource".into(),
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
            warnings: Vec::new(),
            native_replay: Some(artifact),
        };
        let session = SessionId::new_v7();
        let run = RunId::new_v7();
        let events = vec![EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: Some(run),
            seq: 1,
            timestamp: jiff::Timestamp::now(),
            event: Event::ModelTurnCommitted {
                model: ModelRef {
                    name: "one".into(),
                    provider_id: "provider".into(),
                    model_id: "model".into(),
                    adapter_id: "adapter.one".into(),
                },
                input_through_seq: 1,
                turn,
            },
        }];
        let history = assemble_history(&events, &store).expect("history");
        let oven_sdk::HistoryTurn::Assistant(turn) = &history[0] else {
            panic!("assistant turn");
        };
        let replay = turn.finish.native_replay.as_ref().expect("native replay");
        assert_eq!(replay.adapter_id().as_str(), "adapter.one");
        assert_eq!(replay.scope().resource_id.as_str(), "resource");
    }

    #[test]
    fn foreign_scope_replay_decision_is_persisted_exactly() {
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
        let decisions = replay_decisions(&[OvenReplayDecision {
            history_index: 3,
            disposition: OvenReplayDisposition::DiscardedForeignScope {
                found: found.clone(),
                expected: expected.clone(),
            },
        }]);
        assert!(matches!(
            &decisions[0].disposition,
            ReplayDisposition::DiscardedForeignScope { found: persisted_found, expected: persisted_expected }
                if persisted_found.model_id == found.model_id.as_str()
                    && persisted_expected.resource_id == expected.resource_id.as_str()
        ));
    }

    #[test]
    fn assembled_tool_transcript_snapshot_is_stable() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::open(directory.path().join("artifacts")).expect("store");
        let session = SessionId(uuid::Uuid::from_u128(1));
        let run = RunId(uuid::Uuid::from_u128(2));
        let call = ToolCallId(uuid::Uuid::from_u128(8));
        let envelope = |seq, event| EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id: session,
            run_id: Some(run),
            seq,
            timestamp: jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            event,
        };
        let profile = ProfileSnapshot {
            name: "test".into(),
            agent_type: AgentType::Primary,
            models: Vec::new(),
            tools: vec!["read".into()],
            delegation: cookie_agent_protocol::DelegationSnapshot {
                enabled: false,
                allowed_profiles: Vec::new(),
                depth_limit: DepthLimit::Finite(0),
                result_limit_bytes: 0,
            },
            permission_rules: Vec::new(),
        };
        let events = vec![
            envelope(
                1,
                Event::RunStarted {
                    client_run_id: "run".into(),
                    input: "inspect the workspace".into(),
                    current_profile: ProfileIdentity {
                        name: "test".into(),
                        agent_type: AgentType::Primary,
                    },
                    profile,
                },
            ),
            envelope(
                2,
                Event::ModelTurnCommitted {
                    model: ModelRef {
                        name: "test".into(),
                        provider_id: "provider".into(),
                        model_id: "model".into(),
                        adapter_id: "adapter".into(),
                    },
                    input_through_seq: 1,
                    turn: PersistedModelTurn {
                        content: vec![PersistedAssistantPart::ToolCall {
                            id: "provider-call".into(),
                            provider_item_id: None,
                            name: "read".into(),
                            input: serde_json::json!({"filePath":"README.md"}),
                            raw_input: None,
                            metadata: None,
                        }],
                        provider_options: BTreeMap::new(),
                        finish_reason: ModelFinishReason::ToolCalls,
                        usage: Usage::default(),
                        response_metadata: BTreeMap::new(),
                        provider_metadata: BTreeMap::new(),
                        warnings: Vec::new(),
                        native_replay: None,
                    },
                },
            ),
            envelope(
                3,
                Event::ToolCallStarted {
                    tool_call_id: call,
                    model_call_id: "provider-call".into(),
                    provider_item_id: None,
                    tool: "read".into(),
                    arguments: serde_json::json!({"filePath":"README.md"}),
                },
            ),
            envelope(
                4,
                Event::ToolCallCompleted {
                    tool_call_id: call,
                    result: ToolResult {
                        title: "Read README.md".into(),
                        output: "contents".into(),
                        metadata: serde_json::json!({}),
                        truncation: None,
                        attachments: Vec::new(),
                    },
                },
            ),
        ];
        let history = assemble_history(&events, &store).expect("assembled history");
        insta::with_settings!({ prepend_module_to_snapshot => false }, {
            insta::assert_json_snapshot!(
                "cookie_agent_engine__tests__assembled_tool_transcript_snapshot_is_stable",
                history
            );
        });
    }
}
