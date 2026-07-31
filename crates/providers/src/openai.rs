//! OpenAI Chat Completions and Responses API adapter.

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
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

const DEFAULT_BASE_URL: &str = "https://llm-api.quantumcookie.xyz/v1";

/// Selects the wire protocol for a configured model.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OpenAiEndpoint {
    #[default]
    ChatCompletions,
    Responses,
}

/// OpenAI provider whose endpoint can be selected globally or for individual models.
#[derive(Clone, Debug)]
pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    base_url: String,
    default_endpoint: OpenAiEndpoint,
    model_endpoints: HashMap<ModelId, OpenAiEndpoint>,
}

impl OpenAiProvider {
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
            default_endpoint: OpenAiEndpoint::ChatCompletions,
            model_endpoints: HashMap::new(),
        }
    }

    #[must_use]
    pub fn with_default_endpoint(mut self, endpoint: OpenAiEndpoint) -> Self {
        self.default_endpoint = endpoint;
        self
    }

    #[must_use]
    pub fn with_model_endpoint(mut self, model: ModelId, endpoint: OpenAiEndpoint) -> Self {
        self.model_endpoints.insert(model, endpoint);
        self
    }

    fn endpoint(&self, model: &ModelId) -> OpenAiEndpoint {
        self.model_endpoints
            .get(model)
            .copied()
            .unwrap_or(self.default_endpoint)
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
        ProviderCapabilities {
            tool_calling: true,
            parallel_tool_calls: true,
            streaming_tool_argument_deltas: true,
            reasoning_deltas: true,
            reasoning_replayable: false,
            image_input: true,
            pdf_input: true,
            structured_output: true,
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
        match self.endpoint(&request.model) {
            OpenAiEndpoint::ChatCompletions => self.stream_chat(request).await,
            OpenAiEndpoint::Responses => self.stream_responses(request).await,
        }
    }
}

impl OpenAiProvider {
    async fn stream_chat(
        &self,
        request: ProviderRequest,
    ) -> Result<
        futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
        ProviderError,
    > {
        let response = self
            .client
            .post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .bearer_auth(&self.api_key)
            .json(&chat_request(&request))
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
        let calls = Arc::new(Mutex::new(HashMap::<u64, String>::new()));
        Ok(response
            .bytes_stream()
            .eventsource()
            .map(move |item| match item {
                Ok(event) if event.data == "[DONE]" => Vec::new(),
                Ok(event) => parse_chat_event(&event.data, &calls),
                Err(error) => vec![Err(ProviderError::network(format!(
                    "dropped OpenAI stream: {error}"
                )))],
            })
            .flat_map(stream::iter)
            .boxed())
    }

    async fn stream_responses(
        &self,
        request: ProviderRequest,
    ) -> Result<
        futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
        ProviderError,
    > {
        let response = self
            .client
            .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
            .bearer_auth(&self.api_key)
            .json(&responses_request(&request))
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
        let calls = Arc::new(Mutex::new(HashSet::<String>::new()));
        Ok(response
            .bytes_stream()
            .eventsource()
            .map(move |item| match item {
                Ok(event) => parse_response_event(&event.event, &event.data, &calls),
                Err(error) => vec![Err(ProviderError::network(format!(
                    "dropped Responses stream: {error}"
                )))],
            })
            .flat_map(stream::iter)
            .boxed())
    }
}

fn chat_request(request: &ProviderRequest) -> Value {
    let messages: Vec<Value> = request.messages.iter().map(chat_message).collect();
    let mut body = json!({
        "model": request.model.0,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(|tool| json!({"type": "function", "function": {
            "name": tool.name, "description": tool.description, "parameters": tool.input_schema
        }})).collect());
    }
    if let Some(schema) = &request.structured_output {
        body["response_format"] = schema.clone();
    }
    body
}

fn chat_message(message: &ProviderMessage) -> Value {
    match message {
        ProviderMessage::System { content } => json!({"role": "system", "content": content}),
        ProviderMessage::User { content } => {
            json!({"role": "user", "content": chat_content(content)})
        }
        ProviderMessage::Assistant {
            content,
            tool_calls,
        } => {
            let mut value = json!({"role": "assistant", "content": chat_content(content)});
            if !tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(tool_calls.iter().map(|call| json!({
                    "id": call.id, "type": "function", "function": {"name": call.name, "arguments": call.arguments.to_string()}
                })).collect());
            }
            value
        }
        ProviderMessage::Tool { result } => json!({
            "role": "tool", "tool_call_id": result.tool_call_id, "content": result.content
        }),
    }
}

fn chat_content(content: &[ContentPart]) -> Value {
    if content
        .iter()
        .all(|part| matches!(part, ContentPart::Text { .. }))
    {
        return Value::String(
            content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
        );
    }
    Value::Array(content.iter().map(|part| match part {
        ContentPart::Text { text } => json!({"type": "text", "text": text}),
        ContentPart::Image { media_type, data } => json!({"type": "image_url", "image_url": {
            "url": format!("data:{media_type};base64,{data}")
        }}),
        ContentPart::Pdf { media_type, data } => json!({"type": "file", "file": {
            "filename": "document.pdf", "file_data": format!("data:{media_type};base64,{data}")
        }}),
    }).collect())
}

fn responses_request(request: &ProviderRequest) -> Value {
    let input: Vec<Value> = request.messages.iter().map(response_message).collect();
    let mut body = json!({"model": request.model.0, "input": input, "stream": true});
    if let Some(max_tokens) = request.max_tokens {
        body["max_output_tokens"] = json!(max_tokens);
    }
    if request.reasoning.enabled {
        body["reasoning"] =
            json!({"effort": request.reasoning.effort.as_deref().unwrap_or("medium")});
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function", "name": tool.name, "description": tool.description,
                        "parameters": tool.input_schema, "strict": false
                    })
                })
                .collect(),
        );
    }
    if let Some(schema) = &request.structured_output {
        body["text"] = json!({"format": schema});
    }
    body
}

fn response_message(message: &ProviderMessage) -> Value {
    match message {
        ProviderMessage::System { content } => {
            json!({"role": "developer", "content": [{"type": "input_text", "text": content}]})
        }
        ProviderMessage::User { content } => {
            json!({"role": "user", "content": response_content(content)})
        }
        ProviderMessage::Assistant {
            content,
            tool_calls,
        } => {
            let mut output = response_content(content);
            for call in tool_calls {
                output.push(
                    json!({"type": "function_call", "call_id": call.id, "name": call.name,
                    "arguments": call.arguments.to_string()}),
                );
            }
            json!({"role": "assistant", "content": output})
        }
        ProviderMessage::Tool { result } => {
            json!({"type": "function_call_output", "call_id": result.tool_call_id, "output": result.content})
        }
    }
}

fn response_content(content: &[ContentPart]) -> Vec<Value> {
    content.iter().map(|part| match part {
        ContentPart::Text { text } => json!({"type": "input_text", "text": text}),
        ContentPart::Image { media_type, data } => json!({"type": "input_image", "image_url": format!("data:{media_type};base64,{data}")}),
        ContentPart::Pdf { media_type, data } => json!({"type": "input_file", "filename": "document.pdf", "file_data": format!("data:{media_type};base64,{data}")}),
    }).collect()
}

fn parse_chat_event(
    data: &str,
    calls: &Arc<Mutex<HashMap<u64, String>>>,
) -> Vec<Result<NormalizedEvent, ProviderError>> {
    let value: Value = match serde_json::from_str(data) {
        Ok(value) => value,
        Err(error) => {
            return vec![Err(ProviderError::network(format!(
                "invalid OpenAI SSE JSON: {error}"
            )))];
        }
    };
    let mut events = Vec::new();
    for choice in value["choices"].as_array().into_iter().flatten() {
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str().filter(|text| !text.is_empty()) {
            events.push(Ok(NormalizedEvent::TextDelta {
                text: text.to_owned(),
            }));
        }
        if let Some(text) = delta["reasoning_content"]
            .as_str()
            .filter(|text| !text.is_empty())
        {
            events.push(Ok(NormalizedEvent::ReasoningDelta {
                text: text.to_owned(),
            }));
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64().unwrap_or_default();
            let id = call["id"].as_str().map(ToOwned::to_owned);
            let name = call["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if let Some(id) = id {
                calls
                    .lock()
                    .expect("stream state lock")
                    .insert(index, id.clone());
                events.push(Ok(NormalizedEvent::ToolCallStart {
                    tool_call_id: id,
                    tool: name,
                }));
            }
            if let Some(arguments) = call["function"]["arguments"]
                .as_str()
                .filter(|value| !value.is_empty())
                && let Some(id) = calls
                    .lock()
                    .expect("stream state lock")
                    .get(&index)
                    .cloned()
            {
                events.push(Ok(NormalizedEvent::ToolArgsDelta {
                    tool_call_id: id,
                    delta: arguments.to_owned(),
                }));
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            if reason == "tool_calls" {
                for id in calls.lock().expect("stream state lock").values() {
                    events.push(Ok(NormalizedEvent::ToolCallEnd {
                        tool_call_id: id.clone(),
                    }));
                }
            }
            events.push(Ok(NormalizedEvent::Stop {
                reason: StopReason::from_provider(reason),
            }));
        }
    }
    if !value["usage"].is_null() {
        events.push(Ok(NormalizedEvent::Usage {
            input_tokens: number(&value["usage"], "prompt_tokens"),
            output_tokens: number(&value["usage"], "completion_tokens"),
            cache_read_tokens: number(&value["usage"]["prompt_tokens_details"], "cached_tokens"),
        }));
    }
    events
}

fn parse_response_event(
    event_name: &str,
    data: &str,
    calls: &Arc<Mutex<HashSet<String>>>,
) -> Vec<Result<NormalizedEvent, ProviderError>> {
    let value: Value = match serde_json::from_str(data) {
        Ok(value) => value,
        Err(error) => {
            return vec![Err(ProviderError::network(format!(
                "invalid Responses SSE JSON: {error}"
            )))];
        }
    };
    match event_name {
        "response.output_text.delta" => value["delta"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(|text| {
                vec![Ok(NormalizedEvent::TextDelta {
                    text: text.to_owned(),
                })]
            })
            .unwrap_or_default(),
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => value["delta"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(|text| {
                vec![Ok(NormalizedEvent::ReasoningDelta {
                    text: text.to_owned(),
                })]
            })
            .unwrap_or_default(),
        "response.output_item.added" => response_tool_start(&value["item"], calls),
        "response.function_call_arguments.delta" => {
            let id = value["call_id"].as_str().unwrap_or_default();
            let delta = value["delta"].as_str().unwrap_or_default();
            if id.is_empty() || delta.is_empty() {
                vec![]
            } else {
                vec![Ok(NormalizedEvent::ToolArgsDelta {
                    tool_call_id: id.to_owned(),
                    delta: delta.to_owned(),
                })]
            }
        }
        "response.function_call_arguments.done" => {
            let id = value["call_id"].as_str().unwrap_or_default();
            if id.is_empty() {
                vec![]
            } else {
                vec![Ok(NormalizedEvent::ToolCallEnd {
                    tool_call_id: id.to_owned(),
                })]
            }
        }
        "response.output_item.done" => {
            let item = &value["item"];
            let id = item["call_id"].as_str().unwrap_or_default();
            if item["type"] == "function_call" && !id.is_empty() {
                let mut events = response_tool_start(item, calls);
                if let Some(arguments) = item["arguments"]
                    .as_str()
                    .filter(|arguments| !arguments.is_empty())
                {
                    events.push(Ok(NormalizedEvent::ToolArgsDelta {
                        tool_call_id: id.to_owned(),
                        delta: arguments.to_owned(),
                    }));
                }
                events.push(Ok(NormalizedEvent::ToolCallEnd {
                    tool_call_id: id.to_owned(),
                }));
                events
            } else {
                vec![]
            }
        }
        "response.completed" => {
            let response = &value["response"];
            let usage = &response["usage"];
            vec![
                Ok(NormalizedEvent::Usage {
                    input_tokens: number(usage, "input_tokens"),
                    output_tokens: number(usage, "output_tokens"),
                    cache_read_tokens: number(&usage["input_tokens_details"], "cached_tokens"),
                }),
                Ok(NormalizedEvent::Stop {
                    reason: StopReason::from_provider(
                        response["status"].as_str().unwrap_or("completed"),
                    ),
                }),
            ]
        }
        "error" | "response.failed" => vec![Err(ProviderError::EntryRetryable {
            message: value.to_string(),
        })],
        _ => vec![], // response.created and hosted-tool items are intentionally not model events.
    }
}

fn response_tool_start(
    item: &Value,
    calls: &Arc<Mutex<HashSet<String>>>,
) -> Vec<Result<NormalizedEvent, ProviderError>> {
    if item["type"] != "function_call" {
        return vec![];
    } // Ignore image_generation and other hosted tools.
    let id = item["call_id"].as_str().unwrap_or_default();
    let name = item["name"].as_str().unwrap_or_default();
    if id.is_empty()
        || name.is_empty()
        || !calls
            .lock()
            .expect("stream state lock")
            .insert(id.to_owned())
    {
        vec![]
    } else {
        vec![Ok(NormalizedEvent::ToolCallStart {
            tool_call_id: id.to_owned(),
            tool: name.to_owned(),
        })]
    }
}

fn number(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or_default()
}
