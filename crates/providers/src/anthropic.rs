//! Anthropic Messages adapter.

use crate::{
    AssistantTurnOpaque, CancellationSemantics, ContentPart, EncodedHistory, ModelId,
    NormalizedEvent, PersistedTurn, Provider, ProviderCapabilities, ProviderError, ProviderMessage,
    ProviderProtocol, ProviderRequest, StopReason,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, stream};
use reqwest::Client;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

const DEFAULT_BASE_URL: &str = "https://llm-api.quantumcookie.xyz/v1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const OVERALL_TIMEOUT: Duration = Duration::from_secs(120);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
enum Block {
    Text,
    Thinking,
    Tool { id: String },
}

#[derive(Clone, Debug)]
struct CapturedBlock {
    kind: Block,
    raw: Value,
    partial_json: String,
    deltas: Vec<Value>,
}

#[derive(Default)]
struct StreamState {
    blocks: BTreeMap<u64, CapturedBlock>,
    input_tokens: (u64, u64),
    input_usage: Value,
}

/// Streaming Anthropic Messages API provider.
#[derive(Clone, Debug)]
pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: String,
}

impl AnthropicProvider {
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    #[must_use]
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(OVERALL_TIMEOUT)
            .build()
            .expect("valid reqwest client configuration");
        Self::with_client(client, api_key, base_url)
    }

    #[must_use]
    pub fn with_client(
        client: Client,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            client,
            api_key: api_key.into(),
            base_url: base_url.into(),
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
        ProviderCapabilities {
            tool_calling: true,
            parallel_tool_calls: true,
            streaming_tool_argument_deltas: true,
            reasoning_deltas: true,
            reasoning_replayable: true,
            image_input: true,
            pdf_input: true,
            structured_output: false,
            prompt_caching: true,
            context_limit: None,
            output_limit: None,
            usage_reporting: true,
            cancellation: CancellationSemantics::DropStream,
        }
    }

    async fn stream(
        &self,
        request: ProviderRequest,
    ) -> Result<
        futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
        ProviderError,
    > {
        let body = anthropic_request(&request);
        let response = self
            .client
            .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::network(error.to_string()))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response
                .text()
                .await
                .unwrap_or_else(|error| error.to_string());
            return Err(ProviderError::from_http(status, text));
        }

        let state = Arc::new(Mutex::new(StreamState::default()));
        let events = response
            .bytes_stream()
            .eventsource()
            .map(move |item| match item {
                Ok(event) if event.data == "[DONE]" || event.data.trim().is_empty() => vec![],
                Ok(event) => parse_event(&event.event, &event.data, &state),
                Err(error) => vec![Err(ProviderError::network(format!(
                    "dropped Anthropic stream: {error}"
                )))],
            })
            .boxed();
        let timed = stream::unfold(Some(events), |events| async move {
            let mut events = events?;
            match tokio::time::timeout(STREAM_IDLE_TIMEOUT, events.next()).await {
                Ok(Some(item)) => Some((item, Some(events))),
                Ok(None) => None,
                Err(_) => Some((
                    vec![Err(ProviderError::network("Anthropic stream idle timeout"))],
                    None,
                )),
            }
        });
        Ok(timed.flat_map(stream::iter).boxed())
    }
}

/// Rebuilds Anthropic-native history. Artifacts from another protocol are
/// deliberately discarded and represented with the normalized skeleton.
#[must_use]
pub fn encode_history(turns: &[PersistedTurn]) -> EncodedHistory {
    let mut history = EncodedHistory::default();
    let mut tool_results = Vec::new();
    for turn in turns {
        if let ProviderMessage::Tool { result } = &turn.message {
            if turn.opaque.is_some() {
                history.discarded_opaque = true;
            }
            tool_results.push(json!({
                "type": "tool_result", "tool_use_id": result.tool_call_id,
                "content": result.content, "is_error": result.is_error,
            }));
            continue;
        }
        flush_tool_results(&mut history.messages, &mut tool_results);
        if let Some(AssistantTurnOpaque {
            provider: ProviderProtocol::AnthropicMessages,
            payload,
        }) = &turn.opaque
            && let Some(message) = payload.get("message")
        {
            history.messages.push(message.clone());
            continue;
        }
        if turn.opaque.is_some() {
            history.discarded_opaque = true;
        }
        match &turn.message {
            ProviderMessage::System { content } => {
                history
                    .system
                    .push(json!({"type": "text", "text": content}));
            }
            ProviderMessage::User { content } => {
                history
                    .messages
                    .push(json!({"role": "user", "content": anthropic_content(content)}));
            }
            ProviderMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks = anthropic_content(content);
                blocks.extend(tool_calls.iter().map(|call| {
                    json!({"type": "tool_use", "id": call.id, "name": call.name, "input": call.arguments})
                }));
                history
                    .messages
                    .push(json!({"role": "assistant", "content": blocks}));
            }
            ProviderMessage::Tool { .. } => unreachable!("tool results are batched above"),
        }
    }
    flush_tool_results(&mut history.messages, &mut tool_results);
    history
}

fn flush_tool_results(messages: &mut Vec<Value>, tool_results: &mut Vec<Value>) {
    if !tool_results.is_empty() {
        messages.push(json!({"role": "user", "content": std::mem::take(tool_results)}));
    }
}

fn anthropic_request(request: &ProviderRequest) -> Value {
    let history = if request.persisted_turns.is_empty() {
        let turns: Vec<_> = request
            .messages
            .iter()
            .cloned()
            .map(|message| PersistedTurn {
                message,
                opaque: None,
            })
            .collect();
        encode_history(&turns)
    } else {
        encode_history(&request.persisted_turns)
    };
    let mut body = json!({
        "model": request.model.0,
        "messages": history.messages,
        "stream": true,
        "max_tokens": request.max_tokens.unwrap_or(4096),
    });
    if !history.system.is_empty() {
        body["system"] = Value::Array(history.system);
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(|tool| json!({
            "name": tool.name, "description": tool.description, "input_schema": tool.input_schema,
        })).collect());
    }
    if request.reasoning.enabled {
        body["thinking"] = json!({"type": "enabled", "budget_tokens": request.reasoning.budget_tokens.unwrap_or(1024)});
    }
    body
}

fn anthropic_content(content: &[ContentPart]) -> Vec<Value> {
    content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => json!({"type": "text", "text": text}),
            ContentPart::Image { media_type, data } => json!({"type": "image", "source": {
                "type": "base64", "media_type": media_type, "data": data
            }}),
            ContentPart::Pdf { media_type, data } => json!({"type": "document", "source": {
                "type": "base64", "media_type": media_type, "data": data
            }}),
        })
        .collect()
}

fn parse_event(
    event_name: &str,
    data: &str,
    state: &Arc<Mutex<StreamState>>,
) -> Vec<Result<NormalizedEvent, ProviderError>> {
    let value: Value = match serde_json::from_str(data) {
        Ok(value) => value,
        Err(error) => {
            return vec![Err(ProviderError::network(format!(
                "invalid Anthropic SSE JSON: {error}"
            )))];
        }
    };
    match event_name {
        "message_start" => {
            let usage = &value["message"]["usage"];
            // Some Anthropic-compatible gateways report zero input_tokens and put the
            // billable input solely in cache_read_input_tokens.
            let cache = number(usage, "cache_read_input_tokens");
            state.lock().expect("stream state lock").input_tokens =
                (number(usage, "input_tokens") + cache, cache);
            state.lock().expect("stream state lock").input_usage = usage.clone();
            vec![]
        }
        "content_block_start" => {
            let index = value["index"].as_u64().unwrap_or_default();
            let block = &value["content_block"];
            let kind = block["type"].as_str().unwrap_or_default();
            let parsed = match kind {
                "thinking" | "redacted_thinking" => Block::Thinking,
                "tool_use" => Block::Tool {
                    id: block["id"].as_str().unwrap_or_default().to_owned(),
                },
                _ => Block::Text,
            };
            let captured = CapturedBlock {
                kind: parsed.clone(),
                raw: block.clone(),
                partial_json: String::new(),
                deltas: Vec::new(),
            };
            state
                .lock()
                .expect("stream state lock")
                .blocks
                .insert(index, captured);
            if let Block::Tool { id } = &parsed {
                let id = id.clone();
                let name = block["name"].as_str().unwrap_or_default().to_owned();
                vec![Ok(NormalizedEvent::ToolCallStart {
                    tool_call_id: id,
                    tool: name,
                })]
            } else {
                vec![]
            }
        }
        "content_block_delta" => {
            let index = value["index"].as_u64().unwrap_or_default();
            let delta = &value["delta"];
            let text = delta["text"]
                .as_str()
                .or_else(|| delta["thinking"].as_str())
                .or_else(|| delta["signature"].as_str())
                .unwrap_or_default()
                .to_owned();
            let mut state = state.lock().expect("stream state lock");
            let Some(block) = state.blocks.get_mut(&index) else {
                return vec![];
            };
            let delta_type = delta["type"].as_str().unwrap_or_default();
            block.deltas.push(delta.clone());
            apply_unknown_delta(&mut block.raw, delta);
            match &block.kind {
                Block::Thinking if !text.is_empty() => {
                    let key = if delta_type == "signature_delta" {
                        "signature"
                    } else {
                        "thinking"
                    };
                    append_string(&mut block.raw, key, &text);
                    if delta_type == "signature_delta" {
                        vec![]
                    } else {
                        vec![Ok(NormalizedEvent::ReasoningDelta { text })]
                    }
                }
                Block::Tool { id } => {
                    let delta = delta["partial_json"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    if delta.is_empty() {
                        vec![]
                    } else {
                        block.partial_json.push_str(&delta);
                        vec![Ok(NormalizedEvent::ToolArgsDelta {
                            tool_call_id: id.clone(),
                            delta,
                        })]
                    }
                }
                _ if !text.is_empty() => {
                    append_string(&mut block.raw, "text", &text);
                    vec![Ok(NormalizedEvent::TextDelta { text })]
                }
                _ => vec![],
            }
        }
        "content_block_stop" => {
            let index = value["index"].as_u64().unwrap_or_default();
            match state
                .lock()
                .expect("stream state lock")
                .blocks
                .get_mut(&index)
            {
                Some(CapturedBlock {
                    kind: Block::Tool { id },
                    raw,
                    partial_json,
                    ..
                }) => {
                    if !partial_json.is_empty()
                        && let Ok(input) = serde_json::from_str(partial_json)
                    {
                        raw["input"] = input;
                    }
                    vec![Ok(NormalizedEvent::ToolCallEnd {
                        tool_call_id: id.clone(),
                    })]
                }
                _ => vec![],
            }
        }
        "message_delta" => {
            let state = state.lock().expect("stream state lock");
            let (input, cache) = state.input_tokens;
            let output = number(&value["usage"], "output_tokens");
            let reason = value["delta"]["stop_reason"].as_str().unwrap_or("end_turn");
            let message = json!({
                "role": "assistant",
                "content": state.blocks.values().map(|block| block.raw.clone()).collect::<Vec<_>>(),
            });
            vec![
                Ok(NormalizedEvent::TurnOpaque {
                    state: AssistantTurnOpaque {
                        provider: ProviderProtocol::AnthropicMessages,
                        payload: json!({
                            "message": message,
                            "stop_reason": reason,
                            "stop_sequence": value["delta"]["stop_sequence"].clone(),
                            "input_usage": state.input_usage.clone(),
                            "usage": value["usage"].clone(),
                            "message_delta": value.clone(),
                            "block_deltas": state.blocks.iter().map(|(index, block)| {
                                json!({"index": index, "deltas": block.deltas})
                            }).collect::<Vec<_>>(),
                        }),
                    },
                }),
                Ok(NormalizedEvent::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_read_tokens: cache,
                }),
                Ok(NormalizedEvent::Stop {
                    reason: StopReason::from_provider(reason),
                }),
            ]
        }
        "error" => vec![Err(ProviderError::from_sse(&value))],
        _ => vec![],
    }
}

fn apply_unknown_delta(raw: &mut Value, delta: &Value) {
    let Some(fields) = delta.as_object() else {
        return;
    };
    for (key, value) in fields {
        if matches!(
            key.as_str(),
            "type" | "text" | "thinking" | "signature" | "partial_json"
        ) {
            continue;
        }
        match (raw.get(key).and_then(Value::as_str), value.as_str()) {
            (Some(existing), Some(delta)) => raw[key] = Value::String(format!("{existing}{delta}")),
            _ => raw[key] = value.clone(),
        }
    }
}

fn append_string(value: &mut Value, key: &str, delta: &str) {
    let existing = value[key].as_str().unwrap_or_default();
    value[key] = Value::String(format!("{existing}{delta}"));
}

fn number(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or_default()
}
