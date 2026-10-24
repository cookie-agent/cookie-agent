//! What the client protocol and session history say about ACP agents
//! (`docs/site/specs/acp-agents.md` sections 5, 10, and 11; client protocol
//! 1.17.0, notices, steering, and tool presentations 1.20.0, context use
//! 1.23.0).
//!
//! The daemon records the ACP session a cookie session talks to
//! (`acp_session_started`) and what the agent reports about it
//! (`acp_session_updated`): its slash commands, its config options, and its
//! plan. Clients fold the latest of each. These are information: a reader
//! ignores members it does not know.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AgentId, DisplayBlock, Open, PreparedPresentation, ProducerMessageId, RuntimeId, SessionId,
    ToolCallId,
};

/// The longest ACP session ID, command name, option ID, or value the
/// daemon records, in bytes.
pub const MAX_ACP_LABEL_BYTES: usize = 1024;

crate::open_enum! {
    /// How the daemon got the ACP session a turn runs in. An unknown way
    /// reads as `new`.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AcpSessionStart {
        /// `session/new`: the session's first, or the agent could not
        /// resume or load the one it had.
        New,
        /// `session/resume` of the session recorded before.
        Resumed,
        /// `session/load` of the session recorded before; its replay is
        /// ignored.
        Loaded,
    }
}

/// One slash command an ACP agent offers in a session.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpCommand {
    /// The command's name, without the `/`.
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// What to type after the name, when the command takes input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hint: Option<String>,
}

/// One config option an ACP agent offers in a session, such as its model,
/// mode, or reasoning effort. An agent that offers modes but no mode option
/// is shown as an option `mode` of category `mode`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpConfigOption {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The agent's label for what the option is, such as `model`, `mode`,
    /// or `thought_level`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub current_value: String,
    #[serde(default)]
    pub values: Vec<AcpConfigValue>,
}

/// One value an [`AcpConfigOption`] can take.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpConfigValue {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The payload of `acp_session_started`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct AcpSessionRecord {
    /// The ACP agent, as the daemon registers it.
    pub agent: AgentId,
    /// The runtime that hosts it.
    pub runtime_id: RuntimeId,
    /// The agent's ID on the runtime.
    pub runtime_agent: AgentId,
    /// The agent's session ID.
    pub acp_session_id: String,
    /// The session's working directory on the runtime's machine.
    pub cwd: String,
    /// The ACP protocol version the agent answered `initialize` with.
    pub protocol_version: u32,
    /// `agentInfo` as the agent sent it, when it did.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub agent_info: Value,
    pub how: AcpSessionStart,
    /// Whether the agent takes messages into its running turn (it
    /// advertises the steering extension, `_session/steering`): input sent
    /// to steer reaches the turn, else it waits for the agent's next turn
    /// (client protocol 1.20.0).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub steering: bool,
}

crate::open_enum! {
    /// How much an [`AcpNotice`] matters. An unknown severity reads as
    /// `info`.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AcpNoticeSeverity {
        Info,
        Warning,
        Error,
    }
}

/// An advisory line an ACP agent sent (its `notice` update, or what the
/// daemon makes of a compaction update), shown where it arrives (client
/// protocol 1.20.0).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpNotice {
    pub severity: AcpNoticeSeverity,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Input the daemon delivered into the agent's running turn through its
/// steering extension, by the events that took it: the
/// `user_input_submitted` events' sequence numbers and the producer
/// messages (client protocol 1.20.0).
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpSteered {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_inputs: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub producer_messages: Vec<ProducerMessageId>,
}

/// A new presentation of one of the agent's own tool calls, whose title,
/// kind, or arguments arrived after the call started (client protocol
/// 1.20.0). It replaces the call's title, hint, primary argument,
/// arguments, and preview; its tool identity stays.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct AcpToolPresentation {
    pub tool_call_id: ToolCallId,
    pub presentation: PreparedPresentation,
}

/// How much of its context window the agent's conversation uses, as its
/// latest `usage_update` reported (client protocol 1.23.0).
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpContextUsage {
    /// Tokens in the agent's context.
    pub used: u64,
    /// The agent's context window in tokens, when it reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// The payload of `acp_session_updated`: what changed, each member the
/// whole new value.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct AcpSessionUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<Vec<AcpCommand>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<AcpConfigOption>>,
    /// The agent's plan, a `plan` display block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Open<DisplayBlock>>,
    /// A notice of the agent's, shown once where it arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<AcpNotice>,
    /// Input delivered into the running turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steered: Option<AcpSteered>,
    /// A tool call's new presentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_presentation: Option<AcpToolPresentation>,
    /// The agent's context use, when it changed (client protocol 1.23.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<AcpContextUsage>,
}

impl AcpSessionUpdate {
    /// Whether it carries nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_none()
            && self.options.is_none()
            && self.plan.is_none()
            && self.notice.is_none()
            && self.steered.is_none()
            && self.tool_presentation.is_none()
            && self.context.is_none()
    }
}

/// `session.agent_option.set`: sets one of the config options of a
/// session's ACP agent (`session/set_config_option`, or `session/set_mode`
/// for an agent that offers modes but no mode option).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAgentOptionSetParams {
    pub session_id: SessionId,
    pub option_id: String,
    pub value: String,
}

/// The session's options after the change.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionAgentOptionSetResult {
    pub options: Vec<AcpConfigOption>,
}

/// The client method that sets an ACP agent's option.
pub const SESSION_AGENT_OPTION_SET_METHOD: &str = "session.agent_option.set";
