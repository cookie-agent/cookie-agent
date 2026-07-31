//! Capability-aware model provider interface and adapter placeholders.

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
    pub messages: Vec<Value>,
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
    pub cancellation: bool,
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
    },
    Stop,
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
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn capabilities(&self, model: &ModelId) -> ProviderCapabilities;

    async fn stream(
        &self,
        request: ProviderRequest,
    ) -> Result<BoxStream<'static, Result<NormalizedEvent, ProviderError>>, ProviderError>;
}
