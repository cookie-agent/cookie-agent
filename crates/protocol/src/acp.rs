//! What the client protocol and session history say about ACP agents
//! (`docs/site/specs/acp-agents.md` sections 5, 10, and 11; client protocol
//! 1.17.0, notices, steering, and tool presentations 1.20.0, context use
//! and boolean options 1.23.0, forked and detached sessions 1.24.0,
//! starting options 1.26.0).
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
        /// `session/fork` of the session a fork or a revert left behind
        /// ([`AcpSessionRecord::forked_from`]); client protocol 1.24.0.
        Forked,
        /// A subagent session of the agent's own, which it announced with
        /// `subagent_update` in its parent's session: the cookie session is
        /// a child of the parent's, its runs are the subagent's own work,
        /// and it takes no prompts (client protocol 1.39.0).
        Subagent,
    }
}

crate::open_enum! {
    /// Why a cookie session stopped continuing an ACP session as it
    /// stands (client protocol 1.24.0). An unknown reason reads as
    /// `forked`, which never ends the agent's session.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AcpDetachReason {
        /// The cookie session is a fork: the ACP session stays its
        /// source's.
        Forked,
        /// The cookie session was reverted to before the end of the ACP
        /// session's conversation: the agent's session holds turns the
        /// cookie session no longer has, and is deleted once the next
        /// turn has branched off it.
        Reverted,
    }
}

/// A cookie session's ACP session left behind by a fork or a revert
/// (client protocol 1.24.0): the next turn of its agent does not resume
/// it but branches off it at `message_id`, the agent's last message the
/// cookie session keeps (`session/fork`, else a new session given the
/// cookie transcript), and the session recorded before is no longer the
/// cookie session's.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpDetached {
    /// The agent's session ID ([`AcpSessionRecord::acp_session_id`]).
    pub acp_session_id: String,
    pub reason: AcpDetachReason,
    /// The ACP `messageId` of the agent's last message the cookie session
    /// keeps; none when it keeps none of the agent's messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
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

crate::open_enum! {
    /// The control an [`AcpConfigOption`] is (client protocol 1.23.0). An
    /// unknown kind reads as a select, whose `values` a client lists.
    #[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AcpConfigOptionKind {
        /// One of `values`.
        #[default]
        Select,
        /// On or off: `current_value` is `true` or `false`, and `values`
        /// lists both (`true`, named On, and `false`, named Off), so a
        /// client that reads every option as a select can still set it.
        Boolean,
    }
}

impl AcpConfigOptionKind {
    fn is_select(&self) -> bool {
        *self == Self::Select
    }
}

/// One config option an ACP agent offers in a session, such as its model,
/// mode, or reasoning effort. An agent that offers modes but no mode option
/// is shown as an option `mode` of category `mode`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpConfigOption {
    pub id: String,
    pub name: String,
    /// The option's control: a select (the default, left out) or a boolean
    /// (client protocol 1.23.0).
    #[serde(default, skip_serializing_if = "AcpConfigOptionKind::is_select")]
    pub kind: AcpConfigOptionKind,
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

/// A value chosen for one of an ACP agent's config options before its
/// session exists (client protocol 1.26.0): a draft's model or effort,
/// applied when the agent's session starts.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpOptionChoice {
    pub option_id: String,
    pub value: String,
}

/// The most starting choices one session records.
pub const MAX_ACP_OPTION_CHOICES: usize = 64;

impl AcpOptionChoice {
    /// Checks a list of choices a client sent: at most
    /// [`MAX_ACP_OPTION_CHOICES`], each option once, and every ID and value
    /// nonempty and at most [`MAX_ACP_LABEL_BYTES`].
    pub fn validate_list(choices: &[Self]) -> Result<(), String> {
        if choices.len() > MAX_ACP_OPTION_CHOICES {
            return Err(format!(
                "at most {MAX_ACP_OPTION_CHOICES} agent options can be chosen"
            ));
        }
        for (index, choice) in choices.iter().enumerate() {
            for (what, text) in [("option ID", &choice.option_id), ("value", &choice.value)] {
                if text.is_empty() || text.len() > MAX_ACP_LABEL_BYTES {
                    return Err(format!(
                        "an agent option's {what} is 1 to {MAX_ACP_LABEL_BYTES} bytes"
                    ));
                }
            }
            if choices[..index]
                .iter()
                .any(|earlier| earlier.option_id == choice.option_id)
            {
                return Err(format!(
                    "agent option `{}` is chosen more than once",
                    choice.option_id
                ));
            }
        }
        Ok(())
    }
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
    /// Whether the agent takes messages into its running work: input sent
    /// to steer reaches the work, else it waits for the agent's next turn
    /// (client protocol 1.20.0). Every ACP v2 agent does; a record of an
    /// earlier build may say otherwise (an ACP v1 agent without the
    /// steering extension).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub steering: bool,
    /// The agent's session this one was forked from, for `how: forked`
    /// (client protocol 1.24.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// Whether the agent offers `session/fork` (client protocol 1.24.0);
    /// absent in a record an older build wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<bool>,
    /// Whether the agent's `session/fork` forks at a message (the JetBrains
    /// AIR fork point), as the daemon found by asking it; absent until it
    /// has, when `acp_session_updated.fork_point` may say later (client
    /// protocol 1.24.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_point: Option<bool>,
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

crate::open_enum! {
    /// Where a compaction of the agent's own stands (client protocol
    /// 1.41.0). An unknown state reads as `completed`.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AcpCompactionState {
        /// The agent started compacting its conversation.
        InProgress,
        Completed,
        /// It failed; a warning `notice` in the same update says why.
        Failed,
        /// It was abandoned, such as by a stop.
        Cancelled,
    }
}

/// A compaction of the agent's own conversation (its `compaction_update`,
/// client protocol 1.41.0), recorded once as it starts and once as it
/// ends. Clients show it as they show the session's own compaction.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpCompaction {
    /// The agent's ID for it, the same in its start and its end.
    pub compaction_id: String,
    pub state: AcpCompactionState,
    /// The summary the agent kept, when it completed with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

crate::open_enum! {
    /// Which way an [`AcpSessionMessage`] went (client protocol 1.41.0).
    /// An unknown direction reads as `received`.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AcpMessageDirection {
        /// The session got it.
        Received,
        /// The session sent it.
        Sent,
    }
}

/// A message between two of the agent's sessions (its `session_message`,
/// client protocol 1.41.0), such as the task a subagent got or the report
/// it gave back: shown where it arrives, never sent anywhere.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpSessionMessage {
    pub direction: AcpMessageDirection,
    /// The other session, as the daemon named it when the message arrived:
    /// a subagent's title or handle, or `parent` for its parent's session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    pub text: String,
}

/// A command of the agent's that kept running in the background after its
/// tool call ended, and has now ended (client protocol 1.50.0), such as
/// Claude Code's background shell. Recorded where the end arrives, between
/// runs too, so the transcript shows it before the run it may wake.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpBackgroundEnded {
    /// The tool call that started it.
    pub tool_call_id: ToolCallId,
    /// Its tool, as the agent names it.
    pub tool: String,
    /// The call's display argument, such as the command line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// When it moved to the background.
    pub started_at: jiff::Timestamp,
    /// Its exit code, when the agent says it; Claude Code does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The signal that ended it, when the agent says it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}

/// Input the daemon delivered into the agent's running work (an ACP v2
/// prompt sent during the work; with an ACP v1 agent of an earlier build,
/// its steering extension), by the events that took it: the
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
    /// The ACP session a fork or a revert left behind (client protocol
    /// 1.24.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached: Option<AcpDetached>,
    /// Whether the agent's `session/fork` forks at a message, found after
    /// the session's record ([`AcpSessionRecord::fork_point`]; client
    /// protocol 1.24.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_point: Option<bool>,
    /// The values the session's new ACP sessions start with, on top of the
    /// agent document's `acp.config` (client protocol 1.26.0): what
    /// `session.create`'s `agent_options` chose. Recorded once, as the
    /// session is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starting_options: Option<Vec<AcpOptionChoice>>,
    /// A compaction of the agent's own starting or ending (client protocol
    /// 1.41.0; before, a `notice`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<AcpCompaction>,
    /// A message between the agent's sessions (client protocol 1.41.0;
    /// before, a `notice`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_message: Option<AcpSessionMessage>,
    /// A command of the agent's running in the background ended (client
    /// protocol 1.50.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_ended: Option<AcpBackgroundEnded>,
    /// What the session's first prompt to the agent carried ahead of the
    /// input, each part a text block as sent: the tool instructions, the
    /// skill listing, the attachments, the memory snapshot turn, and the
    /// agent document's body (client protocol 1.52.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_context: Option<Vec<String>>,
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
            && self.detached.is_none()
            && self.fork_point.is_none()
            && self.starting_options.is_none()
            && self.compaction.is_none()
            && self.session_message.is_none()
            && self.background_ended.is_none()
            && self.prompt_context.is_none()
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
    /// One of the option's `values`: `true` or `false` for a boolean
    /// option.
    pub value: String,
}

/// The session's options after the change.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionAgentOptionSetResult {
    pub options: Vec<AcpConfigOption>,
}

/// The client method that sets an ACP agent's option.
pub const SESSION_AGENT_OPTION_SET_METHOD: &str = "session.agent_option.set";
