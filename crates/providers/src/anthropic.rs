//! Anthropic Messages adapter.

use crate::{
    CancellationSemantics, ContentPart, ModelId, NormalizedEvent, Provider, ProviderCapabilities,
    ProviderError, ProviderMessage, ProviderRequest, StopReason,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, stream};
use reqwest::Client;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

const DEFAULT_BASE_URL: &str = "https://llm-api.quantumcookie.xyz/v1";

#[derive(Clone, Debug)]
enum Block {
    Text,
    Thinking,
    Tool { id: String },
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
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
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
            reasoning_replayable: false,
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

        let blocks = Arc::new(Mutex::new(HashMap::<u64, Block>::new()));
        let input_tokens = Arc::new(Mutex::new((0, 0)));
        Ok(response
            .bytes_stream()
            .eventsource()
            .map(move |item| match item {
                Ok(event) => parse_event(&event.event, &event.data, &blocks, &input_tokens),
                Err(error) => vec![Err(ProviderError::network(format!(
                    "dropped Anthropic stream: {error}"
                )))],
            })
            .flat_map(stream::iter)
            .boxed())
    }
}

fn anthropic_request(request: &ProviderRequest) -> Value {
    let mut messages = Vec::new();
    let mut system = Vec::new();
    for message in &request.messages {
        match message {
            ProviderMessage::System { content } => {
                system.push(json!({"type": "text", "text": content}))
            }
            ProviderMessage::User { content } => {
                messages.push(json!({"role": "user", "content": anthropic_content(content)}))
            }
            ProviderMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks = anthropic_content(content);
                for call in tool_calls {
                    blocks.push(json!({"type": "tool_use", "id": call.id, "name": call.name, "input": call.arguments}));
                }
                messages.push(json!({"role": "assistant", "content": blocks}));
            }
            ProviderMessage::Tool { result } => messages.push(json!({"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": result.tool_call_id,
                "content": result.content, "is_error": result.is_error
            }]})),
        }
    }
    let mut body = json!({
        "model": request.model.0,
        "messages": messages,
        "stream": true,
        "max_tokens": request.max_tokens.unwrap_or(4096),
    });
    if !system.is_empty() {
        body["system"] = Value::Array(system);
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
    blocks: &Arc<Mutex<HashMap<u64, Block>>>,
    input_tokens: &Arc<Mutex<(u64, u64)>>,
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
            *input_tokens.lock().expect("stream state lock") =
                (number(usage, "input_tokens") + cache, cache);
            vec![]
        }
        "content_block_start" => {
            let index = value["index"].as_u64().unwrap_or_default();
            let block = &value["content_block"];
            let kind = block["type"].as_str().unwrap_or_default();
            let parsed = match kind {
                "thinking" => Block::Thinking,
                "tool_use" => Block::Tool {
                    id: block["id"].as_str().unwrap_or_default().to_owned(),
                },
                _ => Block::Text,
            };
            if let Block::Tool { id } = &parsed {
                let id = id.clone();
                let name = block["name"].as_str().unwrap_or_default().to_owned();
                blocks
                    .lock()
                    .expect("stream state lock")
                    .insert(index, parsed);
                vec![Ok(NormalizedEvent::ToolCallStart {
                    tool_call_id: id,
                    tool: name,
                })]
            } else {
                blocks
                    .lock()
                    .expect("stream state lock")
                    .insert(index, parsed);
                vec![]
            }
        }
        "content_block_delta" => {
            let index = value["index"].as_u64().unwrap_or_default();
            let delta = &value["delta"];
            let text = delta["text"]
                .as_str()
                .or_else(|| delta["thinking"].as_str())
                .unwrap_or_default()
                .to_owned();
            let block = blocks
                .lock()
                .expect("stream state lock")
                .get(&index)
                .cloned();
            match block {
                Some(Block::Thinking) if !text.is_empty() => {
                    vec![Ok(NormalizedEvent::ReasoningDelta { text })]
                }
                Some(Block::Tool { id }) => {
                    let delta = delta["partial_json"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    if delta.is_empty() {
                        vec![]
                    } else {
                        vec![Ok(NormalizedEvent::ToolArgsDelta {
                            tool_call_id: id,
                            delta,
                        })]
                    }
                }
                _ if !text.is_empty() => vec![Ok(NormalizedEvent::TextDelta { text })],
                _ => vec![],
            }
        }
        "content_block_stop" => {
            let index = value["index"].as_u64().unwrap_or_default();
            match blocks.lock().expect("stream state lock").remove(&index) {
                Some(Block::Tool { id }) => {
                    vec![Ok(NormalizedEvent::ToolCallEnd { tool_call_id: id })]
                }
                _ => vec![],
            }
        }
        "message_delta" => {
            let (input, cache) = *input_tokens.lock().expect("stream state lock");
            let output = number(&value["usage"], "output_tokens");
            let reason = value["delta"]["stop_reason"].as_str().unwrap_or("end_turn");
            vec![
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
        "error" => vec![Err(ProviderError::EntryRetryable {
            message: value.to_string(),
        })],
        _ => vec![],
    }
}

fn number(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or_default()
}
