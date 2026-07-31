//! OpenAI Chat Completions and Responses API adapter.

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
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

const DEFAULT_BASE_URL: &str = "https://llm-api.quantumcookie.xyz/v1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const OVERALL_TIMEOUT: Duration = Duration::from_secs(120);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

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
    opaque_protocol: ProviderProtocol,
}

impl OpenAiProvider {
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
            default_endpoint: OpenAiEndpoint::ChatCompletions,
            model_endpoints: HashMap::new(),
            opaque_protocol: ProviderProtocol::OpenAiChatCompletions,
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

    pub(crate) fn with_opaque_protocol(mut self, protocol: ProviderProtocol) -> Self {
        self.opaque_protocol = protocol;
        self
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
            reasoning_replayable: true,
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
            .json(&chat_request(&request, self.opaque_protocol))
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
        let state = Arc::new(Mutex::new(ChatStreamState::default()));
        let protocol = self.opaque_protocol;
        let map_state = state.clone();
        let end_state = state.clone();
        let events = response
            .bytes_stream()
            .eventsource()
            .map(move |item| match item {
                Ok(event) if event.data == "[DONE]" => finalize_chat(&map_state, protocol),
                Ok(event) if event.data.trim().is_empty() => Vec::new(),
                Ok(event) => parse_chat_event(&event.data, &map_state, protocol),
                Err(error) => vec![Err(ProviderError::network(format!(
                    "dropped OpenAI stream: {error}"
                )))],
            })
            .chain(stream::once(
                async move { finalize_chat(&end_state, protocol) },
            ))
            .boxed();
        let timed = stream::unfold(Some(events), |events| async move {
            let mut events = events?;
            match tokio::time::timeout(STREAM_IDLE_TIMEOUT, events.next()).await {
                Ok(Some(item)) => Some((item, Some(events))),
                Ok(None) => None,
                Err(_) => Some((
                    vec![Err(ProviderError::network("OpenAI stream idle timeout"))],
                    None,
                )),
            }
        });
        Ok(timed.flat_map(stream::iter).boxed())
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
        let state = Arc::new(Mutex::new(ResponseStreamState::default()));
        let events = response
            .bytes_stream()
            .eventsource()
            .map(move |item| match item {
                Ok(event) if event.data == "[DONE]" || event.data.trim().is_empty() => Vec::new(),
                Ok(event) => parse_response_event(&event.event, &event.data, &state),
                Err(error) => vec![Err(ProviderError::network(format!(
                    "dropped Responses stream: {error}"
                )))],
            })
            .boxed();
        let timed = stream::unfold(Some(events), |events| async move {
            let mut events = events?;
            match tokio::time::timeout(STREAM_IDLE_TIMEOUT, events.next()).await {
                Ok(Some(item)) => Some((item, Some(events))),
                Ok(None) => None,
                Err(_) => Some((
                    vec![Err(ProviderError::network("Responses stream idle timeout"))],
                    None,
                )),
            }
        });
        Ok(timed.flat_map(stream::iter).boxed())
    }
}

#[derive(Default)]
struct ChatStreamState {
    calls: BTreeMap<(u64, u64), String>,
    raw_calls: BTreeMap<(u64, u64), Value>,
    content: Option<String>,
    reasoning_content: String,
    extra: BTreeMap<String, Value>,
    chunks: Vec<Value>,
    pending_finish: Option<String>,
    saw_output: bool,
    terminal_emitted: bool,
}

#[derive(Default)]
struct ResponseStreamState {
    calls: HashSet<String>,
    completed_calls: HashSet<String>,
    items: Vec<Value>,
    call_ids_by_item: HashMap<String, String>,
}

/// Rebuilds Chat Completions history using exact assistant echoes when present.
#[must_use]
pub fn encode_chat_history(turns: &[PersistedTurn], protocol: ProviderProtocol) -> EncodedHistory {
    let mut history = EncodedHistory::default();
    for turn in turns {
        if let Some(AssistantTurnOpaque { provider, payload }) = &turn.opaque
            && *provider == protocol
            && let Some(message) = payload.get("message")
        {
            history.messages.push(message.clone());
            continue;
        }
        if turn.opaque.is_some() {
            history.discarded_opaque = true;
        }
        history.messages.push(chat_message(&turn.message));
    }
    history
}

/// Rebuilds the selected OpenAI wire-format history.
#[must_use]
pub fn encode_history(turns: &[PersistedTurn], endpoint: OpenAiEndpoint) -> EncodedHistory {
    match endpoint {
        OpenAiEndpoint::ChatCompletions => {
            encode_chat_history(turns, ProviderProtocol::OpenAiChatCompletions)
        }
        OpenAiEndpoint::Responses => encode_responses_history(turns),
    }
}

/// Rebuilds Responses API input. Opaque response output items are replayed
/// verbatim, including encrypted reasoning and hosted-tool items.
#[must_use]
pub fn encode_responses_history(turns: &[PersistedTurn]) -> EncodedHistory {
    let mut history = EncodedHistory::default();
    for turn in turns {
        if let Some(AssistantTurnOpaque {
            provider: ProviderProtocol::OpenAiResponses,
            payload,
        }) = &turn.opaque
            && let Some(items) = payload.get("items").and_then(Value::as_array)
        {
            history.messages.extend(items.iter().cloned());
            continue;
        }
        if turn.opaque.is_some() {
            history.discarded_opaque = true;
        }
        history.messages.extend(response_items(&turn.message));
    }
    history
}

fn chat_request(request: &ProviderRequest, protocol: ProviderProtocol) -> Value {
    let messages = if request.persisted_turns.is_empty() {
        request.messages.iter().map(chat_message).collect()
    } else {
        encode_chat_history(&request.persisted_turns, protocol).messages
    };
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
    let input = if request.persisted_turns.is_empty() {
        request.messages.iter().flat_map(response_items).collect()
    } else {
        encode_responses_history(&request.persisted_turns).messages
    };
    let mut body = json!({
        "model": request.model.0,
        "input": input,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
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

fn response_items(message: &ProviderMessage) -> Vec<Value> {
    match message {
        ProviderMessage::System { content } => {
            vec![
                json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": content}]}),
            ]
        }
        ProviderMessage::User { content } => {
            vec![json!({"type": "message", "role": "user", "content": response_content(content)})]
        }
        ProviderMessage::Assistant {
            content,
            tool_calls,
        } => {
            let mut items = Vec::new();
            if !content.is_empty() {
                items.push(json!({
                    "type": "message", "role": "assistant", "content": response_output_content(content)
                }));
            }
            items.extend(tool_calls.iter().map(|call| {
                json!({"type": "function_call", "call_id": call.id, "name": call.name,
                "arguments": call.arguments.to_string()})
            }));
            items
        }
        ProviderMessage::Tool { result } => {
            vec![
                json!({"type": "function_call_output", "call_id": result.tool_call_id, "output": result.content}),
            ]
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

fn response_output_content(content: &[ContentPart]) -> Vec<Value> {
    content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => json!({"type": "output_text", "text": text}),
            ContentPart::Image { media_type, data } => {
                json!({"type": "output_image", "image_url": format!("data:{media_type};base64,{data}")})
            }
            ContentPart::Pdf { media_type, data } => {
                json!({"type": "output_file", "filename": "document.pdf", "file_data": format!("data:{media_type};base64,{data}")})
            }
        })
        .collect()
}

fn parse_chat_event(
    data: &str,
    state: &Arc<Mutex<ChatStreamState>>,
    protocol: ProviderProtocol,
) -> Vec<Result<NormalizedEvent, ProviderError>> {
    let value: Value = match serde_json::from_str(data) {
        Ok(value) => value,
        Err(error) => {
            return vec![Err(ProviderError::network(format!(
                "invalid OpenAI SSE JSON: {error}"
            )))];
        }
    };
    if value.get("error").is_some() {
        return vec![Err(ProviderError::from_sse(&value))];
    }
    let mut events = Vec::new();
    {
        let mut state = state.lock().expect("stream state lock");
        state.chunks.push(value.clone());
        for choice in value["choices"].as_array().into_iter().flatten() {
            let choice_index = choice["index"].as_u64().unwrap_or_default();
            let delta = &choice["delta"];
            state.saw_output |= !delta.is_null();
            capture_chat_extra(&mut state, delta, choice);
            if let Some(text) = delta["content"].as_str().filter(|text| !text.is_empty()) {
                state.content.get_or_insert_with(String::new).push_str(text);
                events.push(Ok(NormalizedEvent::TextDelta {
                    text: text.to_owned(),
                }));
            }
            if let Some(text) = delta["reasoning_content"]
                .as_str()
                .filter(|text| !text.is_empty())
            {
                state.reasoning_content.push_str(text);
                events.push(Ok(NormalizedEvent::ReasoningDelta {
                    text: text.to_owned(),
                }));
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                let key = (choice_index, call["index"].as_u64().unwrap_or_default());
                let id = call["id"].as_str().map(ToOwned::to_owned);
                let name = call["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                if let Some(id) = id {
                    state.calls.insert(key, id.clone());
                    let raw = state.raw_calls.entry(key).or_insert_with(|| {
                        json!({"id": id, "type": "function", "function": {"name": name, "arguments": ""}})
                    });
                    if let Some(raw_id) = call.get("id") {
                        raw["id"] = raw_id.clone();
                    }
                    if let Some(raw_type) = call.get("type") {
                        raw["type"] = raw_type.clone();
                    }
                    if !name.is_empty() {
                        raw["function"]["name"] = json!(name);
                    }
                    events.push(Ok(NormalizedEvent::ToolCallStart {
                        tool_call_id: id,
                        tool: name.clone(),
                    }));
                }
                let known_id = state.calls.get(&key).cloned();
                if let Some(arguments) = call["function"]["arguments"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                    && let Some(id) = known_id
                {
                    let raw = state.raw_calls.entry(key).or_insert_with(|| {
                        json!({"id": id, "type": "function", "function": {"name": name, "arguments": ""}})
                    });
                    let current = raw["function"]["arguments"].as_str().unwrap_or_default();
                    raw["function"]["arguments"] = json!(format!("{current}{arguments}"));
                    events.push(Ok(NormalizedEvent::ToolArgsDelta {
                        tool_call_id: id,
                        delta: arguments.to_owned(),
                    }));
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                state.pending_finish = Some(reason.to_owned());
            }
        }
    }
    if !value["usage"].is_null() {
        events.push(Ok(NormalizedEvent::Usage {
            input_tokens: number(&value["usage"], "prompt_tokens"),
            output_tokens: number(&value["usage"], "completion_tokens"),
            cache_read_tokens: number(&value["usage"]["prompt_tokens_details"], "cached_tokens"),
        }));
        events.extend(finalize_chat(state, protocol));
    }
    events
}

fn capture_chat_extra(state: &mut ChatStreamState, delta: &Value, choice: &Value) {
    for (key, value) in delta.as_object().into_iter().flatten() {
        if !matches!(
            key.as_str(),
            "role" | "content" | "reasoning_content" | "tool_calls"
        ) {
            merge_extra(state.extra.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
    for key in ["refusal", "content_filter_results"] {
        if let Some(value) = choice.get(key).filter(|value| !value.is_null()) {
            merge_extra(
                state.extra.entry(key.to_owned()).or_insert(Value::Null),
                value,
            );
        }
    }
}

fn merge_extra(existing: &mut Value, fragment: &Value) {
    if existing.is_null() {
        *existing = fragment.clone();
        return;
    }
    match (existing, fragment) {
        (Value::String(existing), Value::String(fragment)) => existing.push_str(fragment),
        (Value::Array(existing), Value::Array(fragment)) => {
            existing.extend(fragment.iter().cloned())
        }
        (Value::Object(existing), Value::Object(fragment)) => {
            for (key, value) in fragment {
                merge_extra(existing.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (existing, fragment) => *existing = fragment.clone(),
    }
}

fn finalize_chat(
    state: &Arc<Mutex<ChatStreamState>>,
    protocol: ProviderProtocol,
) -> Vec<Result<NormalizedEvent, ProviderError>> {
    let mut state = state.lock().expect("stream state lock");
    if state.terminal_emitted || (!state.saw_output && state.pending_finish.is_none()) {
        return vec![];
    }
    state.terminal_emitted = true;
    for call in state.raw_calls.values() {
        if let Some(arguments) = call["function"]["arguments"].as_str()
            && serde_json::from_str::<Value>(arguments).is_err()
        {
            return vec![Err(ProviderError::EntryTerminal {
                message: format!("malformed model tool arguments for {}", call["id"]),
            })];
        }
    }
    let reason = state
        .pending_finish
        .clone()
        .unwrap_or_else(|| "stop".into());
    let mut message = json!({"role": "assistant", "content": state.content.clone()});
    if !state.reasoning_content.is_empty() {
        message["reasoning_content"] = json!(state.reasoning_content);
    }
    if !state.raw_calls.is_empty() {
        message["tool_calls"] = Value::Array(state.raw_calls.values().cloned().collect());
    }
    for (key, value) in &state.extra {
        message[key] = value.clone();
    }
    let mut events: Vec<_> = state
        .calls
        .values()
        .map(|id| {
            Ok(NormalizedEvent::ToolCallEnd {
                tool_call_id: id.clone(),
            })
        })
        .collect();
    events.push(Ok(NormalizedEvent::TurnOpaque {
        state: AssistantTurnOpaque {
            provider: protocol,
            payload: json!({"message": message, "finish_reason": reason, "chunks": state.chunks}),
        },
    }));
    events.push(Ok(NormalizedEvent::Stop {
        reason: StopReason::from_provider(&reason),
    }));
    events
}

fn parse_response_event(
    event_name: &str,
    data: &str,
    state: &Arc<Mutex<ResponseStreamState>>,
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
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            let text = value["delta"].as_str().unwrap_or_default();
            if text.is_empty() {
                vec![]
            } else {
                append_reasoning_delta(state, &value, text);
                vec![Ok(NormalizedEvent::ReasoningDelta {
                    text: text.to_owned(),
                })]
            }
        }
        "response.output_item.added" => {
            let item = value["item"].clone();
            state
                .lock()
                .expect("stream state lock")
                .items
                .push(item.clone());
            if let (Some(item_id), Some(call_id)) = (item["id"].as_str(), item["call_id"].as_str())
            {
                state
                    .lock()
                    .expect("stream state lock")
                    .call_ids_by_item
                    .insert(item_id.to_owned(), call_id.to_owned());
            }
            response_tool_start(&item, state)
        }
        "response.function_call_arguments.delta" => {
            let item_id = value["item_id"].as_str().unwrap_or_default();
            let id = value["call_id"]
                .as_str()
                .map(ToOwned::to_owned)
                .or_else(|| {
                    state
                        .lock()
                        .expect("stream state lock")
                        .call_ids_by_item
                        .get(item_id)
                        .cloned()
                })
                .unwrap_or_default();
            let delta = value["delta"].as_str().unwrap_or_default();
            if item_id.is_empty() || id.is_empty() || delta.is_empty() {
                vec![]
            } else {
                append_response_arguments(state, item_id, delta);
                vec![Ok(NormalizedEvent::ToolArgsDelta {
                    tool_call_id: id,
                    delta: delta.to_owned(),
                })]
            }
        }
        "response.function_call_arguments.done" => {
            let item_id = value["item_id"].as_str().unwrap_or_default();
            let id = value["call_id"]
                .as_str()
                .map(ToOwned::to_owned)
                .or_else(|| {
                    state
                        .lock()
                        .expect("stream state lock")
                        .call_ids_by_item
                        .get(item_id)
                        .cloned()
                })
                .unwrap_or_default();
            complete_response_tool_call(state, id)
                .into_iter()
                .map(Ok)
                .collect()
        }
        "response.output_item.done" => {
            let item = &value["item"];
            let id = item["call_id"].as_str().unwrap_or_default();
            let had_argument_deltas = state
                .lock()
                .expect("stream state lock")
                .items
                .iter()
                .find(|existing| existing["call_id"] == id)
                .and_then(|existing| existing["arguments"].as_str())
                .is_some_and(|arguments| !arguments.is_empty());
            replace_response_item(state, item.clone());
            if let (Some(item_id), Some(call_id)) = (item["id"].as_str(), item["call_id"].as_str())
            {
                state
                    .lock()
                    .expect("stream state lock")
                    .call_ids_by_item
                    .insert(item_id.to_owned(), call_id.to_owned());
            }
            if item["type"] == "function_call" && !id.is_empty() {
                let mut events = response_tool_start(item, state);
                if let Some(arguments) = item["arguments"]
                    .as_str()
                    .filter(|arguments| !arguments.is_empty())
                    .filter(|_| !had_argument_deltas)
                {
                    events.push(Ok(NormalizedEvent::ToolArgsDelta {
                        tool_call_id: id.to_owned(),
                        delta: arguments.to_owned(),
                    }));
                }
                if let Some(event) = complete_response_tool_call(state, id.to_owned()) {
                    events.push(Ok(event));
                }
                events
            } else {
                vec![]
            }
        }
        "response.completed" => {
            let response = &value["response"];
            let usage = &response["usage"];
            let items = response["output"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| state.lock().expect("stream state lock").items.clone());
            vec![
                Ok(NormalizedEvent::TurnOpaque {
                    state: AssistantTurnOpaque {
                        provider: ProviderProtocol::OpenAiResponses,
                        payload: json!({
                            "items": items,
                            "store": false,
                            "status": response["status"],
                            "usage": usage,
                        }),
                    },
                }),
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
        "response.incomplete" => incomplete_response(&value, state),
        "error" | "response.error" | "response.failed" => {
            vec![Err(ProviderError::from_sse(&value))]
        }
        _ => vec![], // response.created and hosted-tool items are intentionally not model events.
    }
}

fn response_tool_start(
    item: &Value,
    state: &Arc<Mutex<ResponseStreamState>>,
) -> Vec<Result<NormalizedEvent, ProviderError>> {
    if item["type"] != "function_call" {
        return vec![];
    } // Ignore image_generation and other hosted tools.
    let id = item["call_id"].as_str().unwrap_or_default();
    let name = item["name"].as_str().unwrap_or_default();
    if id.is_empty()
        || name.is_empty()
        || !state
            .lock()
            .expect("stream state lock")
            .calls
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

fn complete_response_tool_call(
    state: &Arc<Mutex<ResponseStreamState>>,
    call_id: String,
) -> Option<NormalizedEvent> {
    if call_id.is_empty()
        || !state
            .lock()
            .expect("stream state lock")
            .completed_calls
            .insert(call_id.clone())
    {
        None
    } else {
        Some(NormalizedEvent::ToolCallEnd {
            tool_call_id: call_id,
        })
    }
}

fn append_response_arguments(state: &Arc<Mutex<ResponseStreamState>>, item_id: &str, delta: &str) {
    let mut state = state.lock().expect("stream state lock");
    if let Some(item) = state.items.iter_mut().find(|item| item["id"] == item_id) {
        let current = item["arguments"].as_str().unwrap_or_default();
        item["arguments"] = json!(format!("{current}{delta}"));
    }
}

fn append_reasoning_delta(state: &Arc<Mutex<ResponseStreamState>>, value: &Value, delta: &str) {
    let item_id = value["item_id"].as_str().unwrap_or_default();
    if item_id.is_empty() {
        return;
    }
    let summary_index = value["summary_index"].as_u64().unwrap_or_default() as usize;
    let mut state = state.lock().expect("stream state lock");
    let Some(item) = state.items.iter_mut().find(|item| item["id"] == item_id) else {
        return;
    };
    if item["summary"].is_null() {
        item["summary"] = Value::Array(Vec::new());
    }
    let summaries = item["summary"].as_array_mut().expect("summary array");
    while summaries.len() <= summary_index {
        summaries.push(json!({"type": "summary_text", "text": ""}));
    }
    let current = summaries[summary_index]["text"]
        .as_str()
        .unwrap_or_default();
    summaries[summary_index]["text"] = json!(format!("{current}{delta}"));
}

fn incomplete_response(
    value: &Value,
    state: &Arc<Mutex<ResponseStreamState>>,
) -> Vec<Result<NormalizedEvent, ProviderError>> {
    let response = &value["response"];
    let items = response["output"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| state.lock().expect("stream state lock").items.clone());
    let reason = response["incomplete_details"]["reason"]
        .as_str()
        .unwrap_or_default();
    let mut events = vec![Ok(NormalizedEvent::TurnOpaque {
        state: AssistantTurnOpaque {
            provider: ProviderProtocol::OpenAiResponses,
            payload: json!({"items": items, "store": false, "status": "incomplete", "incomplete_details": response["incomplete_details"]}),
        },
    })];
    if reason == "max_output_tokens" {
        let usage = &response["usage"];
        if !usage.is_null() {
            events.push(Ok(NormalizedEvent::Usage {
                input_tokens: number(usage, "input_tokens"),
                output_tokens: number(usage, "output_tokens"),
                cache_read_tokens: number(&usage["input_tokens_details"], "cached_tokens"),
            }));
        }
        events.push(Ok(NormalizedEvent::Stop {
            reason: StopReason::Length,
        }));
    } else {
        events.push(Err(ProviderError::from_sse(value)));
    }
    events
}

fn replace_response_item(state: &Arc<Mutex<ResponseStreamState>>, item: Value) {
    let mut state = state.lock().expect("stream state lock");
    let item_id = item.get("id").and_then(Value::as_str);
    let call_id = item.get("call_id").and_then(Value::as_str);
    let existing = if let Some(item_id) = item_id {
        state
            .items
            .iter_mut()
            .find(|existing| existing.get("id").and_then(Value::as_str) == Some(item_id))
    } else if item["type"] == "function_call" {
        call_id.and_then(|call_id| {
            state.items.iter_mut().find(|existing| {
                existing["type"] == "function_call"
                    && existing.get("call_id").and_then(Value::as_str) == Some(call_id)
            })
        })
    } else {
        None
    };
    if let Some(existing) = existing {
        *existing = item;
    } else {
        state.items.push(item);
    }
}

fn number(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or_default()
}
