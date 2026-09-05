//! Official Rust SDK for cookie-agent out-of-process plugins.
//!
//! Plugins exchange newline-delimited JSON-RPC 2.0 messages with the engine over standard I/O.
//! Build a server by registering handlers and publishing targets; its extension capabilities are
//! derived automatically.

#![deny(missing_docs)]

mod error;
mod framing;
mod server;

pub use cookie_agent_protocol::{
    EXTENSION_PROTOCOL_VERSION, ExtensionAgentBeforeStartParams, ExtensionAgentBeforeStartResult,
    ExtensionAllowBlockAction, ExtensionAllowBlockResult, ExtensionBusEventParams,
    ExtensionEmitStatus, ExtensionEventParams, ExtensionInjectedMessage, ExtensionInterceptionHook,
    ExtensionMessageEndAction, ExtensionMessageEndParams, ExtensionMessageEndResult,
    ExtensionMessageRole, ExtensionModelBeforeRequestAction, ExtensionModelBeforeRequestParams,
    ExtensionModelBeforeRequestResult, ExtensionModelBeforeSelectParams, ExtensionModelMessage,
    ExtensionModelParamsAdjustments, ExtensionProviderAfterResponseParams,
    ExtensionProviderAfterResponseResult, ExtensionProviderBeforeHeadersParams,
    ExtensionProviderBeforeHeadersResult, ExtensionProviderBeforeRequestAction,
    ExtensionProviderBeforeRequestParams, ExtensionProviderBeforeRequestResult,
    ExtensionSessionBeforeCompactParams, ExtensionSessionBeforeCompactResult,
    ExtensionSessionBeforeForkParams, ExtensionSessionBeforeForkResult,
    ExtensionSessionBeforeRevertAction, ExtensionSessionBeforeRevertParams,
    ExtensionSessionBeforeRevertResult, ExtensionToolAfterResultAction,
    ExtensionToolAfterResultParams, ExtensionToolAfterResultResult, ExtensionToolBeforeCallAction,
    ExtensionToolBeforeCallParams, ExtensionToolBeforeCallResult, ExtensionToolCallParams,
    ExtensionUserBeforeInputAction, ExtensionUserBeforeInputParams, ExtensionUserBeforeInputResult,
    ProducerDeliveryMode, ProducerId, ProducerIdempotencyKey, ProducerMessageId, SessionId,
};
pub use error::{PluginError, ToolFailure};
pub use server::{
    PluginContext, PluginServer, PluginServerBuilder, ProducerHandle, RecoveryFailure,
    RecoveryResult,
};

/// A tool declaration sent to the engine during initialization.
pub type ToolDecl = cookie_agent_protocol::ExtensionToolDeclaration;

/// Successful structured output from a plugin tool handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolOutput {
    /// Text returned to the model.
    pub content: String,
    /// Whether the tool completed with a tool-level error.
    pub is_error: bool,
}

impl ToolOutput {
    /// Creates successful tool output.
    #[must_use]
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// Creates tool output marked as an error.
    #[must_use]
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

impl From<ToolOutput> for cookie_agent_protocol::ExtensionToolCallResult {
    fn from(output: ToolOutput) -> Self {
        Self {
            content: output.content,
            is_error: output.is_error,
        }
    }
}

/// A system-prompt or compaction-instruction addendum.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Addendum(Option<String>);

impl Addendum {
    /// Creates an empty addendum result.
    #[must_use]
    pub const fn none() -> Self {
        Self(None)
    }

    pub(crate) fn into_option(self) -> Option<String> {
        self.0
    }
}

impl From<Addendum> for ExtensionAgentBeforeStartResult {
    fn from(value: Addendum) -> Self {
        Self {
            addendum: value.into_option(),
            append_to_system_prompt: None,
            replace_system_prompt: None,
            inject_message: None,
        }
    }
}

impl From<Addendum> for ExtensionSessionBeforeCompactResult {
    fn from(value: Addendum) -> Self {
        Self {
            addendum: value.into_option(),
            cancel: false,
            reason: None,
            instructions_override: None,
        }
    }
}

/// Allows a tool call without changing its arguments.
#[must_use]
pub fn allow() -> ExtensionToolBeforeCallResult {
    ExtensionToolBeforeCallResult {
        action: ExtensionToolBeforeCallAction::Allow,
        reason: None,
        modified_arguments: None,
        message_to_model: None,
    }
}

/// Blocks a tool call with the supplied reason.
#[must_use]
pub fn block(reason: impl Into<String>) -> ExtensionToolBeforeCallResult {
    ExtensionToolBeforeCallResult {
        action: ExtensionToolBeforeCallAction::Block,
        reason: Some(reason.into()),
        modified_arguments: None,
        message_to_model: None,
    }
}

/// Allows a tool call with replacement arguments.
#[must_use]
pub fn modify(arguments: serde_json::Value) -> ExtensionToolBeforeCallResult {
    ExtensionToolBeforeCallResult {
        action: ExtensionToolBeforeCallAction::Allow,
        reason: None,
        modified_arguments: Some(arguments),
        message_to_model: None,
    }
}

/// Replaces a completed tool's text content.
#[must_use]
pub fn replace(content: impl Into<String>) -> ExtensionToolAfterResultResult {
    ExtensionToolAfterResultResult {
        action: ExtensionToolAfterResultAction::Replace,
        replacement_content: Some(content.into()),
        note: None,
    }
}

/// Creates a non-empty agent or compaction addendum.
#[must_use]
pub fn addendum(text: impl Into<String>) -> Addendum {
    Addendum(Some(text.into()))
}

/// Appends text to the current system prompt.
#[must_use]
pub fn append_system_prompt(text: impl Into<String>) -> ExtensionAgentBeforeStartResult {
    ExtensionAgentBeforeStartResult {
        addendum: None,
        append_to_system_prompt: Some(text.into()),
        replace_system_prompt: None,
        inject_message: None,
    }
}

/// Replaces the current system prompt.
#[must_use]
pub fn replace_system_prompt(text: impl Into<String>) -> ExtensionAgentBeforeStartResult {
    ExtensionAgentBeforeStartResult {
        addendum: None,
        append_to_system_prompt: None,
        replace_system_prompt: Some(text.into()),
        inject_message: None,
    }
}

/// Injects one durable message at run start.
#[must_use]
pub fn inject_message(
    role: ExtensionMessageRole,
    content: impl Into<String>,
) -> ExtensionAgentBeforeStartResult {
    ExtensionAgentBeforeStartResult {
        addendum: None,
        append_to_system_prompt: None,
        replace_system_prompt: None,
        inject_message: Some(ExtensionInjectedMessage {
            role,
            content: content.into(),
        }),
    }
}

/// Cancels compaction with a user-facing reason.
#[must_use]
pub fn cancel_compaction(reason: impl Into<String>) -> ExtensionSessionBeforeCompactResult {
    ExtensionSessionBeforeCompactResult {
        addendum: None,
        cancel: true,
        reason: Some(reason.into()),
        instructions_override: None,
    }
}

/// Replaces compaction instructions.
#[must_use]
pub fn override_compaction_instructions(
    instructions: impl Into<String>,
) -> ExtensionSessionBeforeCompactResult {
    ExtensionSessionBeforeCompactResult {
        addendum: None,
        cancel: false,
        reason: None,
        instructions_override: Some(instructions.into()),
    }
}
