//! Strict Oven stream collection plus Tokio cancellation bridging.

use std::collections::{BTreeMap, BTreeSet};

use oven_sdk::{
    AbortRegistration, AbortSignal, AssistantMessage, AssistantPart, CompletedTurn, Finish,
    FinishReason, ModelError, ReasoningPart, StreamPart, TextPart, is_semantic_text,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A Tokio cancellation token exposed to Oven through its runtime-neutral signal.
pub(crate) struct AbortBridge {
    signal: AbortSignal,
    registration: AbortRegistration,
    waiter: JoinHandle<()>,
}

impl AbortBridge {
    pub(crate) fn new(cancellation: CancellationToken) -> Self {
        let (signal, registration) = AbortSignal::new();
        let abort = registration.clone();
        let waiter = tokio::spawn(async move {
            cancellation.cancelled().await;
            abort.abort();
        });
        Self {
            signal,
            registration,
            waiter,
        }
    }

    #[must_use]
    pub(crate) fn signal(&self) -> AbortSignal {
        self.signal.clone()
    }

    pub(crate) fn abort(&self) {
        self.registration.abort();
    }
}

impl Drop for AbortBridge {
    fn drop(&mut self) {
        self.waiter.abort();
    }
}

/// Client-visible output and retry-guard information from one accepted part.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PartEffect {
    pub(crate) text_delta: Option<String>,
    pub(crate) reasoning_delta: Option<String>,
    pub(crate) meaningful: bool,
}

struct OpenBlock {
    slot: usize,
    text: String,
    metadata: oven_sdk::PartMetadata,
}

struct OpenToolBlock {
    slot: usize,
    name: String,
    arguments: String,
}

struct EndedToolBlock {
    slot: usize,
    name: String,
    arguments: String,
}

/// Stateful equivalent of `LanguageModel::complete` that exposes deltas while
/// retaining Oven's strict lifecycle and ordering validation.
pub(crate) struct TurnAccumulator {
    content: Vec<Option<AssistantPart>>,
    text: BTreeMap<String, OpenBlock>,
    reasoning: BTreeMap<String, OpenBlock>,
    tools: BTreeMap<String, OpenToolBlock>,
    ended_tools: BTreeMap<String, EndedToolBlock>,
    finalized_tool_ids: BTreeSet<String>,
    tool_result_ids: BTreeSet<String>,
    approval_ids: BTreeSet<String>,
    warnings: Vec<String>,
    terminal: Option<Finish>,
    in_band_error: Option<ModelError>,
    stream_started: bool,
    first_item: bool,
}

impl Default for TurnAccumulator {
    fn default() -> Self {
        Self {
            content: Vec::new(),
            text: BTreeMap::new(),
            reasoning: BTreeMap::new(),
            tools: BTreeMap::new(),
            ended_tools: BTreeMap::new(),
            finalized_tool_ids: BTreeSet::new(),
            tool_result_ids: BTreeSet::new(),
            approval_ids: BTreeSet::new(),
            warnings: Vec::new(),
            terminal: None,
            in_band_error: None,
            stream_started: false,
            first_item: true,
        }
    }
}

impl TurnAccumulator {
    pub(crate) fn push(&mut self, part: StreamPart) -> Result<PartEffect, Box<ModelError>> {
        if self.first_item {
            self.first_item = false;
            if !matches!(part, StreamPart::StreamStart { .. }) {
                return Err(invalid("stream must begin with stream_start"));
            }
        }
        if self.terminal.is_some() {
            return Err(invalid("stream emitted a part after finish"));
        }
        if self.in_band_error.is_some() && !matches!(part, StreamPart::Finish { .. }) {
            return Err(invalid(
                "in-band error was not followed immediately by finish",
            ));
        }

        let mut effect = PartEffect::default();
        match part {
            StreamPart::StreamStart { warnings } => {
                if self.stream_started {
                    return Err(invalid("stream emitted multiple stream_start parts"));
                }
                self.stream_started = true;
                self.warnings.extend(warnings);
            }
            StreamPart::Raw { .. } | StreamPart::ProviderEvent { .. } => {}
            StreamPart::TextStart { id, metadata } => {
                self.ensure_fresh_block(&id)?;
                let slot = self.content.len();
                self.content.push(None);
                self.text.insert(
                    id,
                    OpenBlock {
                        slot,
                        text: String::new(),
                        metadata,
                    },
                );
            }
            StreamPart::TextDelta { id, delta, .. } => {
                self.text
                    .get_mut(&id)
                    .ok_or_else(|| invalid("text delta without matching start"))?
                    .text
                    .push_str(&delta);
                effect.text_delta = Some(delta);
                effect.meaningful = true;
            }
            StreamPart::TextEnd { id, .. } => {
                let block = self
                    .text
                    .remove(&id)
                    .ok_or_else(|| invalid("text end without matching start"))?;
                self.content[block.slot] = Some(AssistantPart::Text(TextPart {
                    text: block.text,
                    metadata: block.metadata,
                }));
            }
            StreamPart::ReasoningStart { id, metadata } => {
                self.ensure_fresh_block(&id)?;
                let slot = self.content.len();
                self.content.push(None);
                self.reasoning.insert(
                    id,
                    OpenBlock {
                        slot,
                        text: String::new(),
                        metadata,
                    },
                );
            }
            StreamPart::ReasoningDelta { id, delta, .. } => {
                self.reasoning
                    .get_mut(&id)
                    .ok_or_else(|| invalid("reasoning delta without matching start"))?
                    .text
                    .push_str(&delta);
                effect.reasoning_delta = Some(delta);
                effect.meaningful = true;
            }
            StreamPart::ReasoningEnd { id, .. } => {
                let block = self
                    .reasoning
                    .remove(&id)
                    .ok_or_else(|| invalid("reasoning end without matching start"))?;
                self.content[block.slot] = Some(AssistantPart::Reasoning(ReasoningPart {
                    text: block.text,
                    metadata: block.metadata,
                }));
            }
            StreamPart::ToolCallStart { id, name, .. } => {
                self.ensure_fresh_tool_block(&id)?;
                let slot = self.content.len();
                self.content.push(None);
                self.tools.insert(
                    id,
                    OpenToolBlock {
                        slot,
                        name,
                        arguments: String::new(),
                    },
                );
                effect.meaningful = true;
            }
            StreamPart::ToolCallDelta { id, delta, .. } => {
                self.tools
                    .get_mut(&id)
                    .ok_or_else(|| invalid("tool-call delta without matching start"))?
                    .arguments
                    .push_str(&delta);
                effect.meaningful = true;
            }
            StreamPart::ToolCallEnd { id, .. } => {
                let block = self
                    .tools
                    .remove(&id)
                    .ok_or_else(|| invalid("tool-call end without matching start"))?;
                self.ended_tools.insert(
                    id,
                    EndedToolBlock {
                        slot: block.slot,
                        name: block.name,
                        arguments: block.arguments,
                    },
                );
                effect.meaningful = true;
            }
            StreamPart::ToolCall { mut tool_call } => {
                if !self.finalized_tool_ids.insert(tool_call.id.clone()) {
                    return Err(invalid("stream emitted a duplicate finalized tool call ID"));
                }
                if self.tools.contains_key(&tool_call.id) {
                    return Err(invalid("finalized tool call arrived before tool-call end"));
                }
                if let Some(block) = self.ended_tools.remove(&tool_call.id) {
                    if block.name != tool_call.name {
                        return Err(invalid(
                            "finalized tool call name does not match its stream block",
                        ));
                    }
                    if !block.arguments.is_empty() {
                        let parsed = serde_json::from_str::<serde_json::Value>(&block.arguments);
                        let parsed_object = parsed.as_ref().ok().filter(|value| value.is_object());
                        if let Some(input) = parsed_object {
                            if input != &tool_call.input {
                                return Err(invalid(
                                    "finalized tool call input does not match streamed arguments",
                                ));
                            }
                            if tool_call
                                .raw_input
                                .as_ref()
                                .is_some_and(|raw| raw != &block.arguments)
                            {
                                return Err(invalid(
                                    "finalized tool call raw input does not match streamed arguments",
                                ));
                            }
                        } else if tool_call.input.is_null()
                            && tool_call.raw_input.as_deref() == Some(block.arguments.as_str())
                        {
                            // Adapters mark argument text that is not a JSON
                            // object (unparseable, or a bare
                            // array/number/string/null) with `input: null` plus
                            // the verbatim raw bytes. Mirror the Oven collector:
                            // tolerate the marker, surface a warning, and keep
                            // the raw bytes; anything else is still a hard error.
                            self.warnings.push(format!(
                                "tool call `{}` finalized with arguments that are not a valid JSON object; input surfaced as null",
                                tool_call.id
                            ));
                        } else if let Ok(input) = &parsed {
                            // Preserve the pre-existing acceptance of a valid
                            // non-object value that exactly matches the finalized
                            // input (unmarked legacy shape).
                            if input != &tool_call.input {
                                return Err(invalid(
                                    "finalized tool call input does not match streamed arguments",
                                ));
                            }
                            if tool_call
                                .raw_input
                                .as_ref()
                                .is_some_and(|raw| raw != &block.arguments)
                            {
                                return Err(invalid(
                                    "finalized tool call raw input does not match streamed arguments",
                                ));
                            }
                        } else {
                            return Err(invalid("tool-call argument stream is not valid JSON"));
                        }
                        tool_call.raw_input = Some(block.arguments);
                    }
                    self.content[block.slot] = Some(AssistantPart::ToolCall(tool_call));
                } else {
                    self.content.push(Some(AssistantPart::ToolCall(tool_call)));
                }
                effect.meaningful = true;
            }
            StreamPart::ToolResult { tool_result } => {
                if !self
                    .tool_result_ids
                    .insert(tool_result.tool_call_id.clone())
                {
                    return Err(invalid("stream emitted duplicate tool result IDs"));
                }
                self.content
                    .push(Some(AssistantPart::ToolResult(tool_result)));
            }
            StreamPart::Source { source } => {
                self.content.push(Some(AssistantPart::Source(source)));
            }
            StreamPart::File { file } => self.content.push(Some(AssistantPart::File(file))),
            StreamPart::ApprovalRequested { approval } => {
                if !self.approval_ids.insert(approval.tool_call_id.clone()) {
                    return Err(invalid("stream emitted duplicate tool approval IDs"));
                }
                self.content
                    .push(Some(AssistantPart::ToolApproval(approval)));
            }
            StreamPart::Custom { part } => {
                self.content.push(Some(AssistantPart::Custom(part)));
            }
            StreamPart::Error { error } => self.in_band_error = Some(error),
            StreamPart::Finish { finish } => {
                if !self.text.is_empty()
                    || !self.reasoning.is_empty()
                    || !self.tools.is_empty()
                    || !self.ended_tools.is_empty()
                {
                    return Err(invalid(
                        "finish emitted with unclosed or unfinalized blocks",
                    ));
                }
                if self.in_band_error.is_some() && finish.finish_reason != FinishReason::Error {
                    return Err(invalid("in-band error requires finish(error)"));
                }
                if self.in_band_error.is_none() && finish.finish_reason == FinishReason::Error {
                    return Err(invalid("finish(error) requires an in-band error"));
                }
                self.terminal = Some(finish);
            }
        }
        Ok(effect)
    }

    pub(crate) fn finish(mut self) -> Result<CompletedTurn, Box<ModelError>> {
        if !self.stream_started {
            return Err(invalid("stream is missing stream_start"));
        }
        let finish = self
            .terminal
            .take()
            .ok_or_else(|| Box::new(ModelError::unexpected_eof("stream ended before finish")))?;
        if let Some(error) = self.in_band_error.take() {
            return Err(Box::new(error));
        }
        for approval_id in &self.approval_ids {
            if !self.finalized_tool_ids.contains(approval_id) {
                return Err(invalid(
                    "tool approval does not match a finalized tool call",
                ));
            }
        }
        for tool_call_id in &self.tool_result_ids {
            if !self.finalized_tool_ids.contains(tool_call_id) {
                self.warnings.push(format!(
                    "tool result for `{tool_call_id}` has no normalized in-stream tool call"
                ));
            }
        }
        let content = self
            .content
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| invalid("stream ended with an unfilled content slot"))?;
        // Providers sometimes close a turn with a whitespace-only text block
        // (leading newlines before a tool call). It renders as blank rows in
        // every surface, so drop it — but only when the turn retains other
        // parts, never leaving the committed turn empty. The rule itself is
        // oven-sdk's single definition of semantic text (`is_semantic_text`),
        // so canonical turns and native replay validation stay consistent.
        let filtered: Vec<_> = content
            .iter()
            .filter(|part| match part {
                AssistantPart::Text(text) => is_semantic_text(&text.text),
                _ => true,
            })
            .collect();
        let content = if filtered.is_empty() {
            content
        } else {
            filtered.into_iter().cloned().collect()
        };
        let mut turn = CompletedTurn::new(AssistantMessage::new(content), finish);
        turn.warnings = self.warnings;
        Ok(turn)
    }

    fn ensure_fresh_block(&self, id: &str) -> Result<(), Box<ModelError>> {
        if self.text.contains_key(id)
            || self.reasoning.contains_key(id)
            || self.tools.contains_key(id)
            || self.ended_tools.contains_key(id)
        {
            Err(invalid("duplicate or mismatched block start"))
        } else {
            Ok(())
        }
    }

    fn ensure_fresh_tool_block(&self, id: &str) -> Result<(), Box<ModelError>> {
        if self.finalized_tool_ids.contains(id) {
            Err(invalid("duplicate or mismatched block start"))
        } else {
            self.ensure_fresh_block(id)
        }
    }
}

fn invalid(message: &'static str) -> Box<ModelError> {
    Box::new(ModelError::invalid_response(message))
}

#[cfg(test)]
mod tests {
    use oven_sdk::{
        AbortSignal, AdapterId, Finish, FinishReason, LanguageModel, LanguageModelDescriptor,
        ModelCapabilities, ModelId, ModelIdentity, ProviderId, Request, StreamPart,
        ToolApprovalPart, ToolCallPart, Usage,
    };

    use cookie_agent_models::{ScriptedModel, ScriptedStep};

    use super::TurnAccumulator;

    fn parts() -> Vec<StreamPart> {
        vec![
            StreamPart::StreamStart { warnings: vec![] },
            StreamPart::TextStart {
                id: "text".into(),
                metadata: None,
            },
            StreamPart::TextDelta {
                id: "text".into(),
                delta: "hello".into(),
                metadata: None,
            },
            StreamPart::TextEnd {
                id: "text".into(),
                metadata: None,
            },
            StreamPart::ToolCall {
                tool_call: ToolCallPart::new("call", "read", serde_json::json!({})),
            },
            StreamPart::ApprovalRequested {
                approval: ToolApprovalPart::new("call"),
            },
            StreamPart::Finish {
                finish: Finish::new(Usage::default(), FinishReason::ToolCalls),
            },
        ]
    }

    #[tokio::test]
    async fn accumulator_matches_oven_complete() {
        let descriptor = LanguageModelDescriptor::new(
            ModelIdentity::new(ProviderId::new("test"), ModelId::new("scripted"))
                .expect("identity"),
            AdapterId::new("test.scripted"),
            ModelCapabilities::conservative(),
        )
        .expect("descriptor");
        let model = ScriptedModel::new(
            descriptor,
            [
                ScriptedStep::stream(parts().into_iter().map(Ok)),
                ScriptedStep::stream(parts().into_iter().map(Ok)),
            ],
        );
        let expected = model
            .complete(Request::new(vec![]), AbortSignal::default())
            .await
            .expect("Oven complete")
            .turn;
        let response = model
            .stream(Request::new(vec![]), AbortSignal::default())
            .await
            .expect("stream");
        let mut stream = response.stream;
        let mut accumulator = TurnAccumulator::default();
        while let Some(part) = futures_util::StreamExt::next(&mut stream).await {
            accumulator.push(part.expect("part")).expect("accumulate");
        }
        assert_eq!(accumulator.finish().expect("finish"), expected);
    }

    #[test]
    fn marked_invalid_tool_arguments_are_accepted_with_warning() {
        use oven_sdk::{AssistantPart, ToolCallPart};

        let mut call = ToolCallPart::new("call", "read", serde_json::Value::Null);
        call.raw_input = Some("[".into());
        let mut accumulator = TurnAccumulator::default();
        for part in [
            StreamPart::StreamStart { warnings: vec![] },
            StreamPart::ToolCallStart {
                id: "call".into(),
                name: "read".into(),
                metadata: None,
            },
            StreamPart::ToolCallDelta {
                id: "call".into(),
                delta: "[".into(),
                metadata: None,
            },
            StreamPart::ToolCallEnd {
                id: "call".into(),
                metadata: None,
            },
            StreamPart::ToolCall { tool_call: call },
            StreamPart::Finish {
                finish: Finish::new(Usage::default(), FinishReason::ToolCalls),
            },
        ] {
            accumulator.push(part).expect("part");
        }
        let turn = accumulator
            .finish()
            .expect("marked-invalid call is accepted");
        assert_eq!(
            turn.warnings,
            vec![
                "tool call `call` finalized with arguments that are not a valid JSON object; input surfaced as null"
            ]
        );
        assert!(matches!(
            &turn.message.content[0],
            AssistantPart::ToolCall(ToolCallPart {
                input: serde_json::Value::Null,
                raw_input: Some(raw),
                ..
            }) if raw == "["
        ));
    }

    #[test]
    fn marked_invalid_non_object_tool_arguments_are_accepted_with_warning() {
        use oven_sdk::{AssistantPart, ToolCallPart};

        for arguments in ["[]", "123", "\"x\"", "null"] {
            let mut call = ToolCallPart::new("call", "read", serde_json::Value::Null);
            call.raw_input = Some(arguments.into());
            let mut accumulator = TurnAccumulator::default();
            for part in [
                StreamPart::StreamStart { warnings: vec![] },
                StreamPart::ToolCallStart {
                    id: "call".into(),
                    name: "read".into(),
                    metadata: None,
                },
                StreamPart::ToolCallDelta {
                    id: "call".into(),
                    delta: arguments.into(),
                    metadata: None,
                },
                StreamPart::ToolCallEnd {
                    id: "call".into(),
                    metadata: None,
                },
                StreamPart::ToolCall { tool_call: call },
                StreamPart::Finish {
                    finish: Finish::new(Usage::default(), FinishReason::ToolCalls),
                },
            ] {
                accumulator.push(part).expect("part");
            }
            let turn = accumulator
                .finish()
                .unwrap_or_else(|error| panic!("{arguments} must be accepted: {error:?}"));
            assert_eq!(
                turn.warnings,
                vec![
                    "tool call `call` finalized with arguments that are not a valid JSON object; input surfaced as null"
                ],
                "{arguments} must carry the marked-invalid warning"
            );
            assert!(matches!(
                &turn.message.content[0],
                AssistantPart::ToolCall(ToolCallPart {
                    input: serde_json::Value::Null,
                    raw_input: Some(raw),
                    ..
                }) if raw == arguments
            ));
        }
    }

    #[test]
    fn unmarked_equal_non_object_tool_arguments_are_accepted() {
        use oven_sdk::{AssistantPart, ToolCallPart};

        let mut accumulator = TurnAccumulator::default();
        for part in [
            StreamPart::StreamStart { warnings: vec![] },
            StreamPart::ToolCallStart {
                id: "call".into(),
                name: "read".into(),
                metadata: None,
            },
            StreamPart::ToolCallDelta {
                id: "call".into(),
                delta: "[]".into(),
                metadata: None,
            },
            StreamPart::ToolCallEnd {
                id: "call".into(),
                metadata: None,
            },
            StreamPart::ToolCall {
                tool_call: ToolCallPart::new("call", "read", serde_json::json!([])),
            },
            StreamPart::Finish {
                finish: Finish::new(Usage::default(), FinishReason::ToolCalls),
            },
        ] {
            accumulator.push(part).expect("part");
        }
        let turn = accumulator
            .finish()
            .expect("unmarked non-object equal to finalized input is accepted");
        assert!(turn.warnings.is_empty());
        assert!(matches!(
            &turn.message.content[0],
            AssistantPart::ToolCall(ToolCallPart {
                input: serde_json::Value::Array(values),
                raw_input: Some(raw),
                ..
            }) if values.is_empty() && raw == "[]"
        ));
    }

    #[test]
    fn unmarked_unparseable_tool_arguments_error() {
        use oven_sdk::ToolCallPart;

        let mut accumulator = TurnAccumulator::default();
        let mut error = None;
        for part in [
            StreamPart::StreamStart { warnings: vec![] },
            StreamPart::ToolCallStart {
                id: "call".into(),
                name: "read".into(),
                metadata: None,
            },
            StreamPart::ToolCallDelta {
                id: "call".into(),
                delta: "[".into(),
                metadata: None,
            },
            StreamPart::ToolCallEnd {
                id: "call".into(),
                metadata: None,
            },
            StreamPart::ToolCall {
                tool_call: ToolCallPart::new("call", "read", serde_json::Value::Null),
            },
        ] {
            if let Err(pushed) = accumulator.push(part) {
                error = Some(pushed);
                break;
            }
        }
        assert!(
            error.is_some_and(|error| error.message.contains("not valid JSON")),
            "unmarked garbage must still fail strict collection"
        );
    }

    #[test]
    fn approval_must_match_a_finalized_call() {
        let mut accumulator = TurnAccumulator::default();
        for part in [
            StreamPart::StreamStart { warnings: vec![] },
            StreamPart::ApprovalRequested {
                approval: ToolApprovalPart::new("missing"),
            },
            StreamPart::Finish {
                finish: Finish::new(Usage::default(), FinishReason::Stop),
            },
        ] {
            accumulator.push(part).expect("part");
        }
        assert!(accumulator.finish().is_err());
    }

    #[test]
    fn whitespace_only_text_is_dropped_when_other_parts_remain() {
        use oven_sdk::AssistantPart;

        let mut accumulator = TurnAccumulator::default();
        for part in [
            StreamPart::StreamStart { warnings: vec![] },
            StreamPart::ReasoningStart {
                id: "reasoning".into(),
                metadata: None,
            },
            StreamPart::ReasoningDelta {
                id: "reasoning".into(),
                delta: "thinking".into(),
                metadata: None,
            },
            StreamPart::ReasoningEnd {
                id: "reasoning".into(),
                metadata: None,
            },
            StreamPart::TextStart {
                id: "text".into(),
                metadata: None,
            },
            StreamPart::TextDelta {
                id: "text".into(),
                delta: "\n\n\n".into(),
                metadata: None,
            },
            StreamPart::TextEnd {
                id: "text".into(),
                metadata: None,
            },
            StreamPart::ToolCall {
                tool_call: ToolCallPart::new("call", "read", serde_json::json!({})),
            },
            StreamPart::Finish {
                finish: Finish::new(Usage::default(), FinishReason::ToolCalls),
            },
        ] {
            accumulator.push(part).expect("part");
        }
        let turn = accumulator.finish().expect("finish");
        let kinds: Vec<_> = turn
            .message
            .content
            .iter()
            .map(|part| match part {
                AssistantPart::Text(_) => "text",
                AssistantPart::Reasoning(_) => "reasoning",
                AssistantPart::ToolCall(_) => "tool_call",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["reasoning", "tool_call"]);
    }

    #[test]
    fn whitespace_only_turn_is_kept_when_it_is_the_only_content() {
        use oven_sdk::AssistantPart;

        let mut accumulator = TurnAccumulator::default();
        for part in [
            StreamPart::StreamStart { warnings: vec![] },
            StreamPart::TextStart {
                id: "text".into(),
                metadata: None,
            },
            StreamPart::TextDelta {
                id: "text".into(),
                delta: "  \n ".into(),
                metadata: None,
            },
            StreamPart::TextEnd {
                id: "text".into(),
                metadata: None,
            },
            StreamPart::Finish {
                finish: Finish::new(Usage::default(), FinishReason::Stop),
            },
        ] {
            accumulator.push(part).expect("part");
        }
        let turn = accumulator.finish().expect("finish");
        assert!(
            matches!(turn.message.content.first(), Some(AssistantPart::Text(_))),
            "a whitespace-only turn must not be emptied"
        );
    }
}
