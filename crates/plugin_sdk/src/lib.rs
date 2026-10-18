//! Official Rust SDK for cookie-agent out-of-process plugins.
//!
//! Plugins exchange newline-delimited JSON-RPC 2.0 messages with the engine over standard I/O,
//! with the same JSON-RPC peer the runtime protocol uses. Build a server by registering tools,
//! hooks, event handlers, and publishing targets; its capabilities are derived automatically.
//! Tools run over the runtime protocol's `tool/*` messages: the SDK reports each call's
//! permission resource and presentation, from which the daemon builds its approval identity,
//! and a tool handler gets progress, a detach point, and cancellation, and returns a
//! [`ToolOutput`].
//!
//! The SDK speaks [`PLUGIN_PROTOCOL_VERSION`]; its crate version's major follows that
//! protocol's major.

#![deny(missing_docs)]

mod error;
mod output;
mod server;

pub use cookie_agent_protocol::{
    DisplayBlock, DisplayDelta, ExtensionAgentBeforeStartParams, ExtensionAgentBeforeStartResult,
    ExtensionAllowBlockAction, ExtensionAllowBlockResult, ExtensionBusEventParams,
    ExtensionEmitStatus, ExtensionEventParams, ExtensionHookOnError, ExtensionInjectedMessage,
    ExtensionIntercept, ExtensionInterceptionHook, ExtensionMessageEndAction,
    ExtensionMessageEndParams, ExtensionMessageEndResult, ExtensionMessageRole,
    ExtensionModelBeforeRequestAction, ExtensionModelBeforeRequestParams,
    ExtensionModelBeforeRequestResult, ExtensionModelBeforeSelectParams,
    ExtensionModelParamsAdjustments, ExtensionPromptContext, ExtensionSessionBeforeCompactParams,
    ExtensionSessionBeforeCompactResult, ExtensionSessionBeforeForkParams,
    ExtensionSessionBeforeForkResult, ExtensionSessionBeforeRevertAction,
    ExtensionSessionBeforeRevertParams, ExtensionSessionBeforeRevertResult,
    ExtensionToolAfterResultAction, ExtensionToolAfterResultParams, ExtensionToolAfterResultResult,
    ExtensionToolBeforeCallAction, ExtensionToolBeforeCallParams, ExtensionToolBeforeCallResult,
    ExtensionToolOrigin, ExtensionUserBeforeInputAction, ExtensionUserBeforeInputParams,
    ExtensionUserBeforeInputResult, HookAssistantPart, HookContentPart, HookHistoryMessage,
    HookModel, Open, PLUGIN_PROTOCOL_VERSION, PermissionAction, ProducerDeliveryMode, ProducerId,
    ProducerIdempotencyKey, ProducerMessageId, ProducerName, RunId, SemVer, SessionId, ToolCallId,
    ToolOutputDeclaration, UserContent, UserContentPart,
    runtime_protocol::{ToolCollision, ToolConcurrency, ToolResultTruncationPolicy},
};
pub use error::{PluginError, ToolFailure};
pub use output::{PrepareCall, Presentation, ToolOutput};
pub use server::{
    DaemonInfo, PluginContext, PluginServer, PluginServerBuilder, ProducerHandle, RecoveryFailure,
    RecoveryResult, ToolContext, UnknownMessage,
};

/// A tool declaration sent to the engine during initialization: the runtime
/// protocol's tool declaration.
pub type ToolDecl = cookie_agent_protocol::runtime_protocol::ToolDeclaration;

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

/// Transforms user input into `content`. It may change, reorder, or remove
/// parts, but each file part must be one of the input's, unchanged, and it
/// must not be blank.
#[must_use]
pub fn transform_input(content: UserContent) -> ExtensionUserBeforeInputResult {
    ExtensionUserBeforeInputResult {
        action: ExtensionUserBeforeInputAction::Transform,
        content: Some(content),
        reason: None,
    }
}

/// Transforms user input by rewriting each of its text parts with
/// `rewrite`; its file parts stay as they are. A part of a type this SDK
/// does not know is left out, since the daemon refuses one in a transform.
/// `None` when nothing would be left.
#[must_use]
pub fn rewrite_input_text(
    content: &UserContent,
    mut rewrite: impl FnMut(&str) -> String,
) -> Option<ExtensionUserBeforeInputResult> {
    let parts = content
        .parts()
        .iter()
        .filter_map(|part| match part {
            Open::Known(UserContentPart::Text { text }) => {
                Some(Open::Known(UserContentPart::Text {
                    text: rewrite(text),
                }))
            }
            Open::Known(file @ UserContentPart::File { .. }) => Some(Open::Known(file.clone())),
            Open::Unknown { .. } => None,
        })
        .collect();
    UserContent::new(parts).ok().map(transform_input)
}

/// Replaces a model request's messages with `messages`. A message returned
/// exactly as `model_before_request` sent it keeps what the view leaves out;
/// any other is built from its parts.
#[must_use]
pub fn replace_messages(messages: Vec<HookHistoryMessage>) -> ExtensionModelBeforeRequestResult {
    ExtensionModelBeforeRequestResult {
        action: ExtensionModelBeforeRequestAction::Replace,
        messages: Some(messages),
        params_adjustments: None,
    }
}

/// Replaces a model request's messages with `messages`, each text part
/// rewritten with `rewrite`. Messages whose text does not change are sent
/// back exactly, so they keep what the view leaves out; a part of a type
/// this SDK does not know is left out of a changed message.
#[must_use]
pub fn rewrite_history_text(
    messages: &[HookHistoryMessage],
    mut rewrite: impl FnMut(&str) -> String,
) -> ExtensionModelBeforeRequestResult {
    let messages = messages
        .iter()
        .map(|message| {
            let content = message
                .content
                .iter()
                .filter_map(|part| match part {
                    Open::Known(HookContentPart::Text { text }) => {
                        Some(Open::Known(HookContentPart::Text {
                            text: rewrite(text),
                        }))
                    }
                    Open::Known(other) => Some(Open::Known(other.clone())),
                    Open::Unknown { .. } => None,
                })
                .collect::<Vec<_>>();
            if content == message.content {
                message.clone()
            } else {
                HookHistoryMessage {
                    role: message.role,
                    content,
                }
            }
        })
        .collect();
    replace_messages(messages)
}

/// Replaces an assistant message's content with `content`. A part returned
/// exactly as `message_end` sent it keeps its metadata.
#[must_use]
pub fn replace_message_content(content: Vec<Open<HookAssistantPart>>) -> ExtensionMessageEndResult {
    ExtensionMessageEndResult {
        action: ExtensionMessageEndAction::Replace,
        content: Some(content),
    }
}

/// Replaces an assistant message's content with its text parts rewritten
/// with `rewrite`; every other part is returned as sent, and a part of a
/// type this SDK does not know is left out.
#[must_use]
pub fn rewrite_message_text(
    content: &[Open<HookAssistantPart>],
    mut rewrite: impl FnMut(&str) -> String,
) -> ExtensionMessageEndResult {
    replace_message_content(
        content
            .iter()
            .filter_map(|part| match part {
                Open::Known(HookAssistantPart::Text { text }) => {
                    Some(Open::Known(HookAssistantPart::Text {
                        text: rewrite(text),
                    }))
                }
                Open::Known(other) => Some(Open::Known(other.clone())),
                Open::Unknown { .. } => None,
            })
            .collect(),
    )
}
