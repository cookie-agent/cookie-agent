//! Capability-aware, provider-neutral model streaming interface.

use async_trait::async_trait;
use futures_util::{StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc, time::Duration};
use thiserror::Error;

pub mod anthropic;
pub mod openai;
pub mod openai_compatible;

#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviderRequest {
    pub model: ModelId,
    #[serde(default)]
    pub messages: Vec<ProviderMessage>,
    /// Persisted transcript entries. When present, adapters replay matching
    /// opaque state through their native history encoder instead of rebuilding
    /// assistant turns from the normalized skeleton.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub persisted_turns: Vec<PersistedTurn>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub reasoning: ReasoningControls,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ProviderMessage {
    System {
        content: String,
    },
    User {
        content: Vec<ContentPart>,
    },
    Assistant {
        content: Vec<ContentPart>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        result: ToolResult,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    Image { media_type: String, data: String },
    Pdf { media_type: String, data: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReasoningControls {
    #[serde(default)]
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviderResponse {
    #[serde(default)]
    pub events: Vec<NormalizedEvent>,
}

/// Provider-native state needed to replay an assistant turn exactly.
///
/// `payload` deliberately has no provider-neutral schema: it is the native
/// assistant message/items captured by the adapter, including continuation
/// fields which normalized events cannot represent.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AssistantTurnOpaque {
    pub provider: ProviderProtocol,
    pub payload: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    AnthropicMessages,
    OpenAiChatCompletions,
    OpenAiResponses,
    OpenAiCompatible,
}

/// A persisted conversation entry supplied to an adapter history encoder.
///
/// An opaque artifact is only valid for the protocol that emitted it. History
/// encoders report incompatible artifacts as discarded and reconstruct that
/// entry from `message` instead.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PersistedTurn {
    pub message: ProviderMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opaque: Option<AssistantTurnOpaque>,
}

/// Provider-native history assembled for a new request.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct EncodedHistory {
    pub system: Vec<Value>,
    pub messages: Vec<Value>,
    pub discarded_opaque: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviderCapabilities {
    pub tool_calling: bool,
    pub parallel_tool_calls: bool,
    pub streaming_tool_argument_deltas: bool,
    pub reasoning_deltas: bool,
    pub reasoning_replayable: bool,
    pub image_input: bool,
    pub pdf_input: bool,
    pub structured_output: bool,
    pub prompt_caching: bool,
    pub context_limit: Option<u64>,
    pub output_limit: Option<u64>,
    pub usage_reporting: bool,
    pub cancellation: CancellationSemantics,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationSemantics {
    /// Dropping the HTTP stream stops local consumption but is not a remote abort.
    #[default]
    DropStream,
    /// The provider offers a remote cancellation endpoint.
    RemoteAbort,
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NormalizedEvent {
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallStart {
        tool_call_id: String,
        tool: String,
    },
    ToolArgsDelta {
        tool_call_id: String,
        delta: String,
    },
    ToolCallEnd {
        tool_call_id: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
    },
    Stop {
        reason: StopReason,
    },
    /// Opaque native assistant state to persist with the completed turn.
    TurnOpaque {
        state: AssistantTurnOpaque,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    Length,
    ContentFilter,
    Cancelled,
    Other(String),
}

impl StopReason {
    #[must_use]
    pub fn from_provider(value: &str) -> Self {
        match value {
            "stop" | "end_turn" | "completed" => Self::EndTurn,
            "tool_calls" | "tool_use" => Self::ToolUse,
            "length" | "max_tokens" => Self::Length,
            "content_filter" => Self::ContentFilter,
            "cancelled" | "canceled" => Self::Cancelled,
            other => Self::Other(other.to_owned()),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorClass {
    EntryRetryable,
    EntryTerminal,
    RunTerminal,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("retryable provider failure: {message}")]
    EntryRetryable { message: String },
    #[error("terminal failure for provider entry: {message}")]
    EntryTerminal { message: String },
    #[error("terminal failure for run: {message}")]
    RunTerminal { message: String },
}

impl ProviderError {
    #[must_use]
    pub const fn class(&self) -> ProviderErrorClass {
        match self {
            Self::EntryRetryable { .. } => ProviderErrorClass::EntryRetryable,
            Self::EntryTerminal { .. } => ProviderErrorClass::EntryTerminal,
            Self::RunTerminal { .. } => ProviderErrorClass::RunTerminal,
        }
    }

    #[must_use]
    pub fn from_http(status: reqwest::StatusCode, body: impl AsRef<str>) -> Self {
        let message = body.as_ref().to_owned();
        let lower = message.to_ascii_lowercase();
        if lower.contains("context_length_exceeded")
            || lower.contains("context length")
            || lower.contains("context_window")
            || lower.contains("context window")
            || lower.contains("maximum context")
            || lower.contains("maximum number of tokens")
            || lower.contains("too many tokens")
            || lower.contains("prompt is too long")
            || lower.contains("input is too long")
            || status.as_u16() == 413
        {
            return Self::RunTerminal { message };
        }
        if lower.contains("insufficient_quota") {
            return Self::EntryTerminal { message };
        }
        if has_entry_terminal_model_code(&message) {
            return Self::EntryTerminal { message };
        }
        if lower.contains("rate_limit")
            || lower.contains("rate limit")
            || lower.contains("overloaded")
            || lower.contains("capacity")
            || lower.contains("server_error")
            || lower.contains("internal_server_error")
            || status.as_u16() == 408
            || status.as_u16() == 429
            || status.as_u16() == 529
            || status.is_server_error()
        {
            Self::EntryRetryable { message }
        } else if lower.contains("authentication")
            || lower.contains("invalid_api_key")
            || lower.contains("invalid api key")
            || lower.contains("invalid_request")
            || status.as_u16() == 401
            || status.as_u16() == 403
            || status.as_u16() == 404
            || status.is_client_error()
        {
            Self::EntryTerminal { message }
        } else {
            Self::EntryRetryable { message }
        }
    }

    /// Classifies an error delivered after an HTTP 200 SSE handshake.
    #[must_use]
    pub fn from_sse(value: &Value) -> Self {
        let body = value.to_string();
        let status = value
            .get("status")
            .or_else(|| value.get("error").and_then(|error| error.get("status")))
            .or_else(|| {
                value
                    .get("response")
                    .and_then(|response| response.get("error"))
                    .and_then(|error| error.get("status"))
            })
            .and_then(status_code)
            .unwrap_or(reqwest::StatusCode::BAD_REQUEST);
        Self::from_http(status, body)
    }

    #[must_use]
    pub fn network(message: impl Into<String>) -> Self {
        Self::EntryRetryable {
            message: message.into(),
        }
    }
}

fn has_entry_terminal_model_code(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    has_entry_terminal_model_code_value(&value)
}

fn has_entry_terminal_model_code_value(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            (key.eq_ignore_ascii_case("code")
                && value.as_str().is_some_and(is_entry_terminal_model_code))
                || has_entry_terminal_model_code_value(value)
        }),
        Value::Array(values) => values.iter().any(has_entry_terminal_model_code_value),
        _ => false,
    }
}

fn is_entry_terminal_model_code(code: &str) -> bool {
    [
        "model_not_found",
        "invalid_model",
        "model_does_not_exist",
        "model_doesnt_exist",
        "model_not_exist",
    ]
    .iter()
    .any(|known_code| code.eq_ignore_ascii_case(known_code))
}

fn status_code(value: &Value) -> Option<reqwest::StatusCode> {
    let code = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))?;
    reqwest::StatusCode::from_u16(code as u16).ok()
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn capabilities(&self, model: &ModelId) -> ProviderCapabilities;

    /// The native protocol used to encode this model's persisted history.
    /// Generic test and custom providers may leave this unknown, in which case
    /// the engine uses normalized reconstruction with canonical tool IDs.
    fn protocol(&self, _model: &ModelId) -> Option<ProviderProtocol> {
        None
    }

    async fn stream(
        &self,
        request: ProviderRequest,
    ) -> Result<BoxStream<'static, Result<NormalizedEvent, ProviderError>>, ProviderError>;
}

/// A configured member of an ordered model fallback chain.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ModelRef {
    pub provider: String,
    pub model: ModelId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelFallback {
    pub from: ModelRef,
    pub to: ModelRef,
    pub reason: String,
    pub attempts: u32,
}

/// Mutable state held by an engine run.  A fresh run starts at the chain head.
#[derive(Clone, Debug, Default)]
pub struct FallbackRunState {
    entry: usize,
}

impl FallbackRunState {
    #[must_use]
    pub const fn entry(&self) -> usize {
        self.entry
    }
}

/// Executes ordered model chains while retaining the selected entry for a run.
pub struct FallbackExecutor {
    providers: HashMap<String, Arc<dyn Provider>>,
    retries: u32,
    retry_backoff: Duration,
}

#[cfg(test)]
mod tests;

impl Default for FallbackExecutor {
    fn default() -> Self {
        Self {
            providers: HashMap::new(),
            retries: 2,
            retry_backoff: Duration::from_millis(100),
        }
    }
}

impl FallbackExecutor {
    #[must_use]
    pub fn new(providers: HashMap<String, Arc<dyn Provider>>) -> Self {
        Self {
            providers,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_retry_policy(mut self, retries: u32, retry_backoff: Duration) -> Self {
        self.retries = retries;
        self.retry_backoff = retry_backoff;
        self
    }

    pub async fn execute<F, C>(
        &self,
        chain: &[ModelRef],
        assemble: F,
        callback: C,
    ) -> Result<ProviderResponse, ProviderError>
    where
        F: Fn(&ModelRef, ProviderCapabilities) -> ProviderRequest,
        C: FnMut(ModelFallback),
    {
        self.execute_with_state(chain, &mut FallbackRunState::default(), assemble, callback)
            .await
    }

    pub async fn execute_with_state<F, C>(
        &self,
        chain: &[ModelRef],
        state: &mut FallbackRunState,
        assemble: F,
        mut callback: C,
    ) -> Result<ProviderResponse, ProviderError>
    where
        F: Fn(&ModelRef, ProviderCapabilities) -> ProviderRequest,
        C: FnMut(ModelFallback),
    {
        let mut entry = state.entry;
        let mut last_error = ProviderError::EntryTerminal {
            message: "model fallback chain is empty".into(),
        };
        while entry < chain.len() {
            let model = &chain[entry];
            let Some(provider) = self.providers.get(&model.provider) else {
                last_error = ProviderError::EntryTerminal {
                    message: format!("provider '{}' is not registered", model.provider),
                };
                if let Some(next) = chain.get(entry + 1) {
                    callback(ModelFallback {
                        from: model.clone(),
                        to: next.clone(),
                        reason: last_error.to_string(),
                        attempts: 0,
                    });
                    entry += 1;
                    state.entry = entry;
                    continue;
                }
                return Err(last_error);
            };

            let mut attempts = 0;
            loop {
                let request = assemble(model, provider.capabilities(&model.model));
                let mut meaningful_output = false;
                let result = match provider.stream(request).await {
                    Ok(mut stream) => {
                        let mut events = Vec::new();
                        let mut failure = None;
                        while let Some(item) = stream.next().await {
                            match item {
                                Ok(event) => {
                                    meaningful_output |= is_meaningful(&event);
                                    events.push(event);
                                }
                                Err(error) => {
                                    failure = Some(error);
                                    break;
                                }
                            }
                        }
                        failure.map_or(Ok(ProviderResponse { events }), Err)
                    }
                    Err(error) => Err(error),
                };
                match result {
                    Ok(response) => return Ok(response),
                    Err(error) if error.class() == ProviderErrorClass::RunTerminal => {
                        return Err(error);
                    }
                    Err(error)
                        if error.class() == ProviderErrorClass::EntryRetryable
                            && attempts < self.retries =>
                    {
                        if meaningful_output {
                            last_error = error;
                            break;
                        }
                        attempts += 1;
                        let multiplier = 1_u32 << (attempts - 1);
                        tokio::time::sleep(self.retry_backoff * multiplier).await;
                    }
                    Err(error) => {
                        last_error = error;
                        break;
                    }
                }
            }
            if let Some(next) = chain.get(entry + 1) {
                callback(ModelFallback {
                    from: model.clone(),
                    to: next.clone(),
                    reason: last_error.to_string(),
                    attempts: attempts + 1,
                });
                entry += 1;
                state.entry = entry;
            } else {
                return Err(last_error);
            }
        }
        Err(last_error)
    }
}

fn is_meaningful(event: &NormalizedEvent) -> bool {
    matches!(
        event,
        NormalizedEvent::TextDelta { .. }
            | NormalizedEvent::ReasoningDelta { .. }
            | NormalizedEvent::ToolCallStart { .. }
            | NormalizedEvent::ToolArgsDelta { .. }
            | NormalizedEvent::ToolCallEnd { .. }
    )
}
