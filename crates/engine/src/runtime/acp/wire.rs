//! The Agent Client Protocol messages the engine sends and reads
//! (`docs/site/specs/acp-v2.md`): version 2, JSON-RPC 2.0. Cookie speaks
//! no other version.
//!
//! Only what the engine uses is modelled. Everything an agent sends is read
//! tolerantly: unknown members are ignored, and an unknown update kind,
//! tool kind, or status reads as "other".

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The ACP version the engine speaks.
pub(crate) const PROTOCOL_VERSION: u32 = 2;

pub(crate) mod method {
    pub(crate) const INITIALIZE: &str = "initialize";
    pub(crate) const SESSION_NEW: &str = "session/new";
    pub(crate) const SESSION_RESUME: &str = "session/resume";
    pub(crate) const SESSION_CLOSE: &str = "session/close";
    /// Unstable: gated by `capabilities.session.fork`.
    pub(crate) const SESSION_FORK: &str = "session/fork";
    pub(crate) const SESSION_DELETE: &str = "session/delete";
    pub(crate) const SESSION_PROMPT: &str = "session/prompt";
    pub(crate) const SESSION_CANCEL: &str = "session/cancel";
    pub(crate) const SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
    pub(crate) const SESSION_UPDATE: &str = "session/update";
    pub(crate) const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeRequest {
    pub protocol_version: u32,
    pub info: Value,
    pub capabilities: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct InitializeResponse {
    pub protocol_version: u32,
    pub info: Value,
    pub capabilities: AgentCapabilities,
    /// The response's own `_meta`.
    #[serde(rename = "_meta")]
    pub meta: Value,
}

/// What the agent offers (`capabilities` of its `initialize` answer).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct AgentCapabilities {
    /// `capabilities.session`: `prompt`, `mcp`, `delete`, `fork`,
    /// `additionalDirectories`, `systemPrompt`. `session/new`, `resume`,
    /// `close`, `prompt`, and `cancel` are every v2 agent's.
    pub session: Value,
}

impl AgentCapabilities {
    /// Whether the agent offers session capability `name`
    /// (`capabilities.session.<name>` present and not `null` or `false`).
    pub(crate) fn session(&self, name: &str) -> bool {
        present(self.session.get(name))
    }

    /// What the agent takes in a prompt besides text and resource links
    /// (`capabilities.session.prompt`).
    pub(crate) fn prompt(&self) -> PromptCapabilities {
        let prompt = self.session.get("prompt");
        let offers = |kind: &str| present(prompt.and_then(|prompt| prompt.get(kind)));
        PromptCapabilities {
            image: offers("image"),
            audio: offers("audio"),
            embedded_context: offers("embeddedContext"),
        }
    }
}

/// A v2 capability marker: an object (`{}` included) offers it; absent,
/// `null`, or `false` does not.
fn present(value: Option<&Value>) -> bool {
    value.is_some_and(|value| !value.is_null() && value != &Value::Bool(false))
}

/// What the agent takes in a prompt besides text and resource links.
#[derive(Clone, Debug, Default)]
pub(crate) struct PromptCapabilities {
    pub image: bool,
    pub audio: bool,
    /// `resource` blocks: a file's content embedded in the prompt.
    pub embedded_context: bool,
}

/// One stdio MCP server handed to the agent (`type: stdio`; `command` an
/// absolute path).
#[derive(Clone, Debug, Serialize)]
pub(crate) struct McpServer {
    #[serde(rename = "type")]
    pub transport: &'static str,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<Value>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewSessionRequest<'a> {
    pub cwd: &'a str,
    /// The session's other workspace roots on the agent's machine, sent
    /// only to an agent that offers `sessionCapabilities.additionalDirectories`.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    pub additional_directories: &'a [String],
    pub mcp_servers: &'a [McpServer],
    /// [`session_meta`].
    #[serde(rename = "_meta")]
    pub meta: Value,
}

/// The `_meta` every session cookie opens carries: Claude Code's adapter
/// takes `claudeCode.options` as its SDK options, whose `extraArgs` asks
/// its CLI for summarized thinking (recent models omit the text by
/// default), which cookie shows as a model's thinking. Other agents ignore
/// it.
pub(crate) fn session_meta() -> Value {
    serde_json::json!({
        "claudeCode": {"options": {"extraArgs": {"thinking-display": "summarized"}}}
    })
}

/// `session/resume`, without a replay: the agent keeps its own history and
/// cookie never shows it again.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExistingSessionRequest<'a> {
    pub session_id: &'a str,
    pub cwd: &'a str,
    /// As in [`NewSessionRequest`].
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    pub additional_directories: &'a [String],
    pub mcp_servers: &'a [McpServer],
    /// [`session_meta`].
    #[serde(rename = "_meta")]
    pub meta: Value,
}

/// `session/fork` (unstable): a new session with the context of
/// `session_id`. `_meta` may name the fork point (the JetBrains AIR
/// extension, [`air_fork_point`]).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForkSessionRequest<'a> {
    pub session_id: &'a str,
    pub cwd: &'a str,
    /// As in [`NewSessionRequest`].
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    pub additional_directories: &'a [String],
    pub mcp_servers: &'a [McpServer],
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// The `_meta` of a `session/fork` that forks at the agent message
/// `message_id`: the JetBrains AIR extension
/// (`jetbrains.air.fork`, version 1), which Claude Code's and Codex's
/// adapters read; an agent without it forks the whole session.
pub(crate) fn air_fork_point(message_id: &str) -> Value {
    let mut meta = session_meta();
    meta["jetbrains"] =
        serde_json::json!({"air": {"fork": {"version": 1, "messageId": message_id}}});
    meta
}

/// What `session/new`, `session/resume`, and `session/fork` answer. The
/// session's commands come here rather than as a first update.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct SessionResponse {
    pub session_id: Option<String>,
    pub config_options: Option<Vec<Value>>,
    pub available_commands: Option<Vec<Value>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionRef<'a> {
    pub session_id: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PromptRequest<'a> {
    pub session_id: &'a str,
    pub prompt: &'a [Value],
}

/// What `session/prompt` answers: the prompt was inserted into the
/// conversation as the user message `message_id`. The work it starts is
/// reported by `state_update`s.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct PromptResponse {
    pub message_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetConfigOptionRequest<'a> {
    pub session_id: &'a str,
    pub config_id: &'a str,
    /// `boolean` for a boolean option's value, `id` for a select's, whose
    /// value is the chosen value's ID.
    #[serde(rename = "type")]
    pub value_type: &'a str,
    pub value: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct SetConfigOptionResponse {
    pub config_options: Option<Vec<Value>>,
}

/// `session/update`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionNotification {
    pub session_id: String,
    pub update: Value,
}

/// `session/request_permission`, with its subject's tool call (a
/// `tool_call` subject) read into [`Self::tool_call`].
#[derive(Clone, Debug)]
pub(crate) struct PermissionRequest {
    pub session_id: String,
    /// What the request asks, in the agent's words for the user.
    pub title: Option<String>,
    /// The tool call it asks about; empty for another subject.
    pub tool_call: ToolCallFields,
    pub options: Vec<PermissionOption>,
}

impl<'de> Deserialize<'de> for PermissionRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            session_id: String,
            #[serde(default)]
            title: String,
            #[serde(default)]
            subject: Option<Value>,
            #[serde(default)]
            options: Vec<PermissionOption>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let tool_call: ToolCallFields = wire
            .subject
            .filter(|subject| subject.get("type").and_then(Value::as_str) == Some("tool_call"))
            .and_then(|subject| subject.get("toolCall").cloned())
            .and_then(|call| serde_json::from_value(call).ok())
            .unwrap_or_default();
        Ok(Self {
            session_id: wire.session_id,
            title: Some(wire.title).filter(|title| !title.trim().is_empty()),
            tool_call,
            options: wire.options,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

/// The answer to a permission request: one of the agent's options, or
/// `cancelled` while the turn is being cancelled.
pub(crate) fn permission_outcome(option: Option<&str>) -> Value {
    match option {
        Some(option) => {
            serde_json::json!({"outcome": {"outcome": "selected", "optionId": option}})
        }
        None => serde_json::json!({"outcome": {"outcome": "cancelled"}}),
    }
}

/// The fields of a `tool_call_update`, which upserts a call, and of a
/// permission request's tool call; every one but the ID optional.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct ToolCallFields {
    pub tool_call_id: String,
    /// The tool's own name, such as `mcp__cookie__memory_read`.
    pub name: Option<String>,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub content: Option<Vec<Value>>,
    pub locations: Option<Vec<ToolCallLocation>>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
    /// The agent's own detail, such as Claude Code's
    /// `claudeCode.toolName`; read only to enrich what is shown.
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
}

impl ToolCallFields {
    /// The tool's own name: the call's `name`, else Claude Code's
    /// `_meta.claudeCode.toolName`.
    pub(crate) fn tool_name(&self) -> Option<&str> {
        self.name
            .as_deref()
            .or_else(|| {
                self.meta
                    .as_ref()?
                    .pointer("/claudeCode/toolName")
                    .and_then(Value::as_str)
            })
            .filter(|name| !name.trim().is_empty())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct ToolCallLocation {
    pub path: String,
    pub line: Option<u64>,
}

/// A text content block.
pub(crate) fn text_block(text: &str) -> Value {
    serde_json::json!({"type": "text", "text": text})
}

/// The text of a content block, or a short note for one that is not text.
pub(crate) fn content_text(block: &Value) -> String {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Some("resource_link") => format!(
            "[{}]",
            block
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or("resource")
        ),
        Some("resource") => block
            .get("resource")
            .and_then(|resource| resource.get("text"))
            .and_then(Value::as_str)
            .map_or_else(|| "[resource]".to_owned(), str::to_owned),
        Some(kind) => format!("[{kind}]"),
        None => String::new(),
    }
}
