//! What hooks see of the engine's own types, and how a hook's answer is
//! turned back into them (`docs/site/specs/compatibility-v1.md` section 9.3).
//!
//! Hooks never see oven-sdk or persisted types. A view keeps what a plugin
//! can read and change (text, reasoning where the hook shows it, tool calls,
//! tool results as text, and files as metadata), and leaves out what only
//! the daemon or the provider understands (provider metadata, native replay,
//! sources, extensions). Turning an answer back keeps what the view left out
//! wherever the plugin returned a part or message unchanged, and refuses a
//! file the original did not hold: a plugin never sends bytes.

use std::collections::HashMap;

use cookie_agent_protocol::{
    ArtifactRef, ExtensionMessageRole, HookAssistantPart, HookContentPart, HookHistoryMessage,
    ModelCallId, Open, PersistedAssistantPart, PersistedFileSource, SafeCode, Sha256Digest,
    UserContent, UserContentPart,
};
use oven_sdk::{
    AssistantMessage, AssistantPart, CompletedTurn, ContentValue, FilePart, FileSource, Finish,
    FinishReason, HistoryTurn, InputPart, SystemMessage, SystemPart, TextPart, ToolCallPart,
    ToolContent, ToolMessage, ToolResultPart, Usage, UserMessage,
};

/// The history of a model request as `model_before_request` sees it.
pub(crate) fn history_view(history: &[HistoryTurn]) -> Vec<HookHistoryMessage> {
    history.iter().map(message_view).collect()
}

fn message_view(turn: &HistoryTurn) -> HookHistoryMessage {
    let mut content = Vec::new();
    let role = match turn {
        HistoryTurn::System(message) => {
            content.extend(message.content.iter().filter_map(|part| match part {
                SystemPart::Text(text) => Some(text_part(&text.text)),
                SystemPart::Custom(_) => None,
            }));
            ExtensionMessageRole::System
        }
        HistoryTurn::User(message) => {
            content.extend(message.content.iter().filter_map(|part| match part {
                InputPart::Text(text) => Some(text_part(&text.text)),
                InputPart::File(file) => file_part(file),
                InputPart::Custom(_) => None,
            }));
            ExtensionMessageRole::User
        }
        HistoryTurn::Assistant(turn) => {
            for part in &turn.message.content {
                match part {
                    AssistantPart::Text(text) => content.push(text_part(&text.text)),
                    AssistantPart::ToolCall(call) => {
                        content.push(Open::Known(HookContentPart::ToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.input.clone(),
                        }));
                    }
                    AssistantPart::ToolResult(result) => push_result(&mut content, result),
                    AssistantPart::File(file) => content.extend(file_part(file)),
                    AssistantPart::Reasoning(_)
                    | AssistantPart::Source(_)
                    | AssistantPart::ToolApproval(_)
                    | AssistantPart::Custom(_) => {}
                }
            }
            ExtensionMessageRole::Assistant
        }
        HistoryTurn::Tool(message) => {
            for result in &message.results {
                push_result(&mut content, result);
            }
            ExtensionMessageRole::Tool
        }
    };
    HookHistoryMessage { role, content }
}

fn text_part(text: &str) -> Open<HookContentPart> {
    Open::Known(HookContentPart::Text { text: text.into() })
}

/// A file as metadata, or nothing for one the daemon holds no bytes of (a
/// URL or a provider's own reference).
fn file_part(file: &FilePart) -> Option<Open<HookContentPart>> {
    file_artifact(file).map(|artifact| Open::Known(HookContentPart::File { artifact }))
}

fn file_artifact(file: &FilePart) -> Option<ArtifactRef> {
    let bytes = match &file.source {
        FileSource::Bytes(bytes) => &bytes[..],
        FileSource::Text(text) => text.as_bytes(),
        FileSource::Url(_) | FileSource::ProviderReference { .. } => return None,
    };
    Some(ArtifactRef {
        sha256: Sha256Digest::of_bytes(bytes).as_str().to_owned(),
        byte_length: bytes.len() as u64,
        media_type: file.media_type.clone(),
        filename: file.filename.clone(),
    })
}

/// A tool result as text, followed by the files its content holds.
fn push_result(content: &mut Vec<Open<HookContentPart>>, result: &ToolResultPart) {
    let mut files = Vec::new();
    let text = match &result.content {
        ToolContent::Text(text) => text.clone(),
        ToolContent::Json(value) => value.to_string(),
        ToolContent::Mixed(values) => values
            .iter()
            .filter_map(|value| match value {
                ContentValue::Text(text) => Some(text.clone()),
                ContentValue::Json(value) => Some(value.to_string()),
                ContentValue::File(file) => {
                    files.extend(file_part(file));
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        ToolContent::Denied { reason } => reason.clone().unwrap_or_default(),
    };
    content.push(Open::Known(HookContentPart::ToolResult {
        tool_call_id: result.tool_call_id.clone(),
        content: text,
        is_error: result.is_error,
    }));
    content.extend(files);
}

/// The history a `model_before_request` replacement names. A message equal
/// to the view of one of `original`'s turns is that turn, with everything
/// the view leaves out; any other message is built from its parts alone,
/// and its files must be files of `original`.
pub(crate) fn history_from_view(
    original: &[HistoryTurn],
    messages: Vec<HookHistoryMessage>,
) -> Result<Vec<HistoryTurn>, String> {
    if messages.is_empty() {
        return Err("replacement message list must not be empty".into());
    }
    let views = history_view(original);
    let mut used = vec![false; original.len()];
    let files = original_files(original);
    messages
        .into_iter()
        .enumerate()
        .map(|(index, message)| {
            if let Some(reused) = views
                .iter()
                .enumerate()
                .position(|(turn, view)| !used[turn] && *view == message)
            {
                used[reused] = true;
                return Ok(original[reused].clone());
            }
            build_turn(message, &files).map_err(|error| format!("messages[{index}]{error}"))
        })
        .collect()
}

/// Every file of `history` by digest, so that a rebuilt message can carry
/// one without the plugin sending its bytes.
fn original_files(history: &[HistoryTurn]) -> HashMap<String, FilePart> {
    fn result_files(result: &ToolResultPart) -> impl Iterator<Item = &FilePart> {
        let values = match &result.content {
            ToolContent::Mixed(values) => values.as_slice(),
            _ => &[],
        };
        values.iter().filter_map(|value| match value {
            ContentValue::File(file) => Some(file),
            _ => None,
        })
    }
    let mut found = Vec::new();
    for turn in history {
        match turn {
            HistoryTurn::System(_) => {}
            HistoryTurn::User(message) => {
                found.extend(message.content.iter().filter_map(|part| match part {
                    InputPart::File(file) => Some(file),
                    _ => None,
                }));
            }
            HistoryTurn::Assistant(turn) => {
                for part in &turn.message.content {
                    match part {
                        AssistantPart::File(file) => found.push(file),
                        AssistantPart::ToolResult(result) => found.extend(result_files(result)),
                        _ => {}
                    }
                }
            }
            HistoryTurn::Tool(message) => {
                found.extend(message.results.iter().flat_map(result_files));
            }
        }
    }
    let mut files = HashMap::new();
    for file in found {
        if let Some(artifact) = file_artifact(file) {
            files.entry(artifact.sha256).or_insert_with(|| file.clone());
        }
    }
    files
}

/// The parts of a replacement message, or `.content[i]: unknown type`
/// for the first part this build does not know.
fn known_parts(content: Vec<Open<HookContentPart>>) -> Result<Vec<HookContentPart>, String> {
    content
        .into_iter()
        .enumerate()
        .map(|(index, part)| match part {
            Open::Known(part) => Ok(part),
            Open::Unknown { tag, .. } => Err(format!(".content[{index}]: unknown type `{tag}`")),
        })
        .collect()
}

/// `error`, about part `index` of a message, as `.content[index]: error`.
fn at_part(index: usize) -> impl Fn(String) -> String {
    move |error| format!(".content[{index}]: {error}")
}

fn known_file(
    files: &HashMap<String, FilePart>,
    artifact: &ArtifactRef,
) -> Result<FilePart, String> {
    files.get(&artifact.sha256).cloned().ok_or_else(|| {
        format!(
            "file {} is not in the request; a replacement may not add files",
            artifact.sha256
        )
    })
}

/// Rebuilds one replacement message. Every error names the part it is
/// about, as `.content[i]: …`, which the caller prefixes with the message's
/// own path (`messages[n]`), so a plugin author can find it in the array
/// they sent.
fn build_turn(
    message: HookHistoryMessage,
    files: &HashMap<String, FilePart>,
) -> Result<HistoryTurn, String> {
    let parts = known_parts(message.content)?;
    let refused = |role: &str, index: usize, part: &HookContentPart| {
        Err(at_part(index)(format!(
            "a {role} message cannot hold a `{}` part",
            content_part_type(part)
        )))
    };
    match message.role {
        ExtensionMessageRole::System => {
            let mut content = Vec::new();
            for (index, part) in parts.into_iter().enumerate() {
                match part {
                    HookContentPart::Text { text } => {
                        content.push(SystemPart::Text(TextPart::new(text)));
                    }
                    other => return refused("system", index, &other),
                }
            }
            Ok(HistoryTurn::system(SystemMessage::new(content)))
        }
        ExtensionMessageRole::User => {
            let mut content = Vec::new();
            for (index, part) in parts.into_iter().enumerate() {
                match part {
                    HookContentPart::Text { text } => {
                        content.push(InputPart::Text(TextPart::new(text)))
                    }
                    HookContentPart::File { artifact } => {
                        content.push(InputPart::File(
                            known_file(files, &artifact).map_err(at_part(index))?,
                        ));
                    }
                    other => return refused("user", index, &other),
                }
            }
            Ok(HistoryTurn::user(UserMessage::new(content)))
        }
        ExtensionMessageRole::Assistant => {
            let mut content = Vec::new();
            for (index, part) in parts.into_iter().enumerate() {
                content.push(match part {
                    HookContentPart::Text { text } => AssistantPart::Text(TextPart::new(text)),
                    HookContentPart::ToolCall {
                        id,
                        name,
                        arguments,
                    } => AssistantPart::ToolCall(ToolCallPart::new(id, name, arguments)),
                    HookContentPart::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    } => AssistantPart::ToolResult(result_part(tool_call_id, content, is_error)),
                    HookContentPart::File { artifact } => {
                        AssistantPart::File(known_file(files, &artifact).map_err(at_part(index))?)
                    }
                });
            }
            let finish_reason = if content
                .iter()
                .any(|part| matches!(part, AssistantPart::ToolCall(_)))
            {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            };
            Ok(HistoryTurn::assistant(CompletedTurn::new(
                AssistantMessage::new(content),
                Finish::new(Usage::default(), finish_reason),
            )))
        }
        ExtensionMessageRole::Tool => {
            let mut results: Vec<ToolResultPart> = Vec::new();
            for (index, part) in parts.into_iter().enumerate() {
                match part {
                    HookContentPart::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    } => results.push(result_part(tool_call_id, content, is_error)),
                    // A file belongs to the result before it.
                    HookContentPart::File { artifact } => {
                        let file = known_file(files, &artifact).map_err(at_part(index))?;
                        let result = results.last_mut().ok_or_else(|| {
                            at_part(index)(
                                "a file in a tool message must follow a tool result".into(),
                            )
                        })?;
                        let mut values = match std::mem::replace(
                            &mut result.content,
                            ToolContent::Mixed(Vec::new()),
                        ) {
                            ToolContent::Text(text) => vec![ContentValue::Text(text)],
                            ToolContent::Mixed(values) => values,
                            other => unreachable!("rebuilt results are text or mixed: {other:?}"),
                        };
                        values.push(ContentValue::File(file));
                        result.content = ToolContent::Mixed(values);
                    }
                    other => return refused("tool", index, &other),
                }
            }
            Ok(HistoryTurn::tool(ToolMessage::new(results)))
        }
    }
}

fn result_part(tool_call_id: String, content: String, is_error: bool) -> ToolResultPart {
    let mut result = ToolResultPart::new(tool_call_id, ToolContent::Text(content));
    result.is_error = is_error;
    result
}

fn content_part_type(part: &HookContentPart) -> &'static str {
    match part {
        HookContentPart::Text { .. } => "text",
        HookContentPart::File { .. } => "file",
        HookContentPart::ToolCall { .. } => "tool_call",
        HookContentPart::ToolResult { .. } => "tool_result",
    }
}

/// A committed assistant message's content as `message_end` sees it, one
/// entry per part of `content`: `None` for a part the view leaves out.
fn assistant_views(content: &[PersistedAssistantPart]) -> Vec<Option<HookAssistantPart>> {
    content
        .iter()
        .map(|part| match part {
            PersistedAssistantPart::Text { text, .. } => {
                Some(HookAssistantPart::Text { text: text.clone() })
            }
            PersistedAssistantPart::Reasoning { text, .. } => {
                Some(HookAssistantPart::Reasoning { text: text.clone() })
            }
            PersistedAssistantPart::ToolCall {
                id, name, input, ..
            } => Some(HookAssistantPart::ToolCall {
                id: id.as_str().to_owned(),
                name: name.as_str().to_owned(),
                arguments: input.clone(),
            }),
            PersistedAssistantPart::File { file } => match &file.source {
                PersistedFileSource::Artifact {
                    byte_length,
                    sha256,
                    ..
                } => Some(HookAssistantPart::File {
                    artifact: ArtifactRef {
                        sha256: sha256.as_str().to_owned(),
                        byte_length: *byte_length,
                        media_type: file.media_type.as_str().to_owned(),
                        filename: file.filename.clone(),
                    },
                }),
                PersistedFileSource::Url { .. } | PersistedFileSource::ProviderReference { .. } => {
                    None
                }
            },
            PersistedAssistantPart::ToolResult { .. }
            | PersistedAssistantPart::Source { .. }
            | PersistedAssistantPart::ToolApproval { .. }
            | PersistedAssistantPart::Custom { .. } => None,
        })
        .collect()
}

/// The content `message_end` sees.
pub(crate) fn assistant_view(content: &[PersistedAssistantPart]) -> Vec<Open<HookAssistantPart>> {
    assistant_views(content)
        .into_iter()
        .flatten()
        .map(Open::Known)
        .collect()
}

/// The content a `message_end` replacement names. A part equal to the view
/// of one of `original`'s parts is that part, with its metadata, and the
/// parts the view leaves out follow the part they followed (or lead, or end
/// the message when that part is gone). Any other part is built from its
/// fields; a file must be a file of `original`.
pub(crate) fn assistant_from_view(
    original: &[PersistedAssistantPart],
    parts: Vec<Open<HookAssistantPart>>,
) -> Result<Vec<PersistedAssistantPart>, String> {
    let views = assistant_views(original);
    // Each hidden part, with the visible part it follows.
    let mut hidden: Vec<(Option<usize>, &PersistedAssistantPart)> = Vec::new();
    let mut previous = None;
    for (index, view) in views.iter().enumerate() {
        if view.is_some() {
            previous = Some(index);
        } else {
            hidden.push((previous, &original[index]));
        }
    }
    let mut used = vec![false; original.len()];
    let mut content: Vec<PersistedAssistantPart> = hidden
        .iter()
        .filter(|(anchor, _)| anchor.is_none())
        .map(|(_, part)| (*part).clone())
        .collect();
    for (position, part) in parts.into_iter().enumerate() {
        let part = match part {
            Open::Known(part) => part,
            Open::Unknown { tag, .. } => {
                return Err(format!("content[{position}]: unknown type `{tag}`"));
            }
        };
        if let Some(reused) = views
            .iter()
            .enumerate()
            .position(|(index, view)| !used[index] && view.as_ref() == Some(&part))
        {
            used[reused] = true;
            content.push(original[reused].clone());
            content.extend(
                hidden
                    .iter()
                    .filter(|(anchor, _)| *anchor == Some(reused))
                    .map(|(_, part)| (*part).clone()),
            );
            continue;
        }
        content.push(
            build_assistant_part(original, part)
                .map_err(|error| format!("content[{position}]: {error}"))?,
        );
    }
    content.extend(
        hidden
            .iter()
            .filter(|(anchor, _)| anchor.is_some_and(|anchor| !used[anchor]))
            .map(|(_, part)| (*part).clone()),
    );
    Ok(content)
}

fn build_assistant_part(
    original: &[PersistedAssistantPart],
    part: HookAssistantPart,
) -> Result<PersistedAssistantPart, String> {
    Ok(match part {
        HookAssistantPart::Text { text } => PersistedAssistantPart::Text {
            text,
            metadata: None,
        },
        HookAssistantPart::Reasoning { text } => PersistedAssistantPart::Reasoning {
            text,
            metadata: None,
        },
        HookAssistantPart::ToolCall {
            id,
            name,
            arguments,
        } => PersistedAssistantPart::ToolCall {
            id: ModelCallId::new(id).map_err(|error| format!("invalid tool call ID: {error}"))?,
            provider_item_id: None,
            name: SafeCode::new(name).map_err(|error| format!("invalid tool name: {error}"))?,
            input: arguments,
            raw_input: None,
            metadata: None,
        },
        HookAssistantPart::File { artifact } => original
            .iter()
            .find(|part| {
                matches!(
                    part,
                    PersistedAssistantPart::File { file }
                        if matches!(
                            &file.source,
                            PersistedFileSource::Artifact { sha256, .. }
                                if sha256.as_str() == artifact.sha256
                        )
                )
            })
            .cloned()
            .ok_or_else(|| {
                format!(
                    "file {} is not in the message; a replacement may not add files",
                    artifact.sha256
                )
            })?,
    })
}

/// The input a `user_before_input` transform names. It may change or remove
/// parts, but every file it holds must be a file of `input`, and it must not
/// be blank.
pub(crate) fn transformed_input(
    input: &UserContent,
    transformed: UserContent,
) -> Result<UserContent, String> {
    if transformed.is_blank() {
        return Err("plugin transformed user input to empty content".into());
    }
    for (index, part) in transformed.parts().iter().enumerate() {
        match part {
            Open::Known(UserContentPart::Text { .. }) => {}
            Open::Known(UserContentPart::File { artifact }) => {
                if !input.files().any(|file| file == artifact) {
                    return Err(format!(
                        "content[{index}]: file {} is not in the user input; a transform may not add files",
                        artifact.display_name()
                    ));
                }
            }
            Open::Unknown { tag, .. } => {
                return Err(format!("content[{index}]: unknown type `{tag}`"));
            }
        }
    }
    Ok(transformed)
}

#[cfg(test)]
mod tests;
