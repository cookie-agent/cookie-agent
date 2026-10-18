//! Official Rust SDK for cookie-agent out-of-process plugins.
//!
//! Plugins exchange newline-delimited JSON-RPC 2.0 messages with the engine over standard I/O,
//! with the same JSON-RPC peer the runtime protocol uses. Build a server by registering tools,
//! hooks, event handlers, and publishing targets; its capabilities are derived automatically.
//! Tools run over the runtime protocol's `tool/*` messages: the SDK prepares each call's
//! approval identity, and a tool handler gets progress, a detach point, and cancellation.

#![deny(missing_docs)]

mod error;
mod server;

pub use cookie_agent_protocol::{
    ExtensionAgentBeforeStartParams, ExtensionAgentBeforeStartResult, ExtensionAllowBlockAction,
    ExtensionAllowBlockResult, ExtensionBusEventParams, ExtensionEmitStatus, ExtensionEventParams,
    ExtensionHookOnError, ExtensionInjectedMessage, ExtensionIntercept, ExtensionInterceptionHook,
    ExtensionMessageEndAction, ExtensionMessageEndParams, ExtensionMessageEndResult,
    ExtensionMessageRole, ExtensionModelBeforeRequestAction, ExtensionModelBeforeRequestParams,
    ExtensionModelBeforeRequestResult, ExtensionModelBeforeSelectParams, ExtensionModelMessage,
    ExtensionModelParamsAdjustments, ExtensionSessionBeforeCompactParams,
    ExtensionSessionBeforeCompactResult, ExtensionSessionBeforeForkParams,
    ExtensionSessionBeforeForkResult, ExtensionSessionBeforeRevertAction,
    ExtensionSessionBeforeRevertParams, ExtensionSessionBeforeRevertResult,
    ExtensionToolAfterResultAction, ExtensionToolAfterResultParams, ExtensionToolAfterResultResult,
    ExtensionToolBeforeCallAction, ExtensionToolBeforeCallParams, ExtensionToolBeforeCallResult,
    ExtensionToolOrigin, ExtensionUserBeforeInputAction, ExtensionUserBeforeInputParams,
    ExtensionUserBeforeInputResult, PLUGIN_PROTOCOL_VERSION, PermissionAction,
    ProducerDeliveryMode, ProducerId, ProducerIdempotencyKey, ProducerMessageId, ProducerName,
    RunId, SemVer, SessionId, ToolCallId, ToolOutputDeclaration,
    runtime_protocol::{ToolCollision, ToolConcurrency, ToolResultTruncationPolicy},
};
pub use error::{PluginError, ToolFailure};
pub use server::{
    DaemonInfo, PluginContext, PluginServer, PluginServerBuilder, ProducerHandle, RecoveryFailure,
    RecoveryResult, ToolContext, UnknownMessage,
};

/// A tool declaration sent to the engine during initialization: the runtime
/// protocol's tool declaration.
pub type ToolDecl = cookie_agent_protocol::runtime_protocol::ToolDeclaration;

/// What a plugin tool returns to the model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolOutput {
    /// The text the model reads.
    pub text: String,
    /// The tool reports a failure the model should see as one.
    pub failed: bool,
}

impl ToolOutput {
    /// A successful result.
    #[must_use]
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            failed: false,
        }
    }

    /// A result that reports a tool-level failure.
    #[must_use]
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            failed: true,
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
            append_to_system_prompt: value.into_option(),
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
        append_to_system_prompt: Some(text.into()),
        replace_system_prompt: None,
        inject_message: None,
    }
}

/// Replaces the current system prompt.
#[must_use]
pub fn replace_system_prompt(text: impl Into<String>) -> ExtensionAgentBeforeStartResult {
    ExtensionAgentBeforeStartResult {
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
