//! The Agent Client Protocol messages the engine sends and reads
//! (`docs/site/specs/acp-agents.md` section 2): version 1, JSON-RPC 2.0.
//!
//! Only what the engine uses is modelled. Everything an agent sends is read
//! tolerantly: unknown members are ignored, and an unknown update kind,
//! tool kind, or status reads as "other".

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The ACP version the engine speaks.
pub(crate) const PROTOCOL_VERSION: u32 = 1;

pub(crate) mod method {
    pub(crate) const INITIALIZE: &str = "initialize";
    pub(crate) const SESSION_NEW: &str = "session/new";
    pub(crate) const SESSION_LOAD: &str = "session/load";
    pub(crate) const SESSION_RESUME: &str = "session/resume";
    pub(crate) const SESSION_CLOSE: &str = "session/close";
    pub(crate) const SESSION_PROMPT: &str = "session/prompt";
    pub(crate) const SESSION_CANCEL: &str = "session/cancel";
    pub(crate) const SESSION_SET_MODE: &str = "session/set_mode";
    pub(crate) const SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
    pub(crate) const SESSION_UPDATE: &str = "session/update";
    pub(crate) const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
    /// The steering extension: a message into the running turn.
    pub(crate) const SESSION_STEERING: &str = "_session/steering";
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeRequest {
    pub protocol_version: u32,
    pub client_capabilities: Value,
    pub client_info: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct InitializeResponse {
    pub protocol_version: u32,
    pub agent_capabilities: AgentCapabilities,
    pub agent_info: Value,
    /// The response's own `_meta`, where agents advertise extensions such
    /// as `steering`.
    #[serde(rename = "_meta")]
    pub meta: Value,
}

impl InitializeResponse {
    /// Whether the agent accepts [`method::SESSION_STEERING`] for a running
    /// turn: `_meta.steering.supported` is `true` (the steering extension of
    /// Claude Code's and Codex's adapters).
    pub(crate) fn steering(&self) -> bool {
        self.meta
            .pointer("/steering/supported")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct AgentCapabilities {
    pub load_session: bool,
    pub prompt_capabilities: PromptCapabilities,
    pub session_capabilities: Value,
}

impl AgentCapabilities {
    /// Whether the agent offers session capability `name`
    /// (`sessionCapabilities.<name>` present and not `false`).
    pub(crate) fn session(&self, name: &str) -> bool {
        self.session_capabilities
            .get(name)
            .is_some_and(|value| !value.is_null() && value != &Value::Bool(false))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct PromptCapabilities {
    pub image: bool,
}

/// One stdio MCP server handed to the agent.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewSessionRequest<'a> {
    pub cwd: &'a str,
    pub mcp_servers: &'a [McpServer],
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExistingSessionRequest<'a> {
    pub session_id: &'a str,
    pub cwd: &'a str,
    pub mcp_servers: &'a [McpServer],
}

/// What `session/new`, `session/load`, and `session/resume` answer.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct SessionResponse {
    pub session_id: Option<String>,
    pub modes: Option<SessionModes>,
    pub config_options: Option<Vec<Value>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct SessionModes {
    pub current_mode_id: String,
    pub available_modes: Vec<SessionMode>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct SessionMode {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
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

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct PromptResponse {
    pub stop_reason: String,
    pub usage: Option<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetModeRequest<'a> {
    pub session_id: &'a str,
    pub mode_id: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetConfigOptionRequest<'a> {
    pub session_id: &'a str,
    pub config_id: &'a str,
    /// `boolean` for a boolean option's value; left out for a select's,
    /// whose value is the chosen value's ID.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub value_type: Option<&'a str>,
    pub value: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct SetConfigOptionResponse {
    pub config_options: Option<Vec<Value>>,
}

/// `_session/steering`: `prompt` into the running turn. `idleBehavior:
/// promptRequired` asks an agent with no turn running to answer
/// `promptRequired` rather than start a turn of its own; an agent that does
/// not know it starts one (`startedNewTurn`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SteeringRequest<'a> {
    pub session_id: &'a str,
    pub prompt: &'a [Value],
    #[serde(rename = "_meta")]
    pub meta: Value,
}

/// What `_session/steering` answers: `injected`, `promptRequired`,
/// `startedNewTurn`, or (Codex) `failed`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct SteeringResponse {
    pub outcome: String,
}

/// `session/update`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionNotification {
    pub session_id: String,
    pub update: Value,
}

/// `session/request_permission`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionRequest {
    pub session_id: String,
    #[serde(default)]
    pub tool_call: ToolCallFields,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
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

/// The fields of a `tool_call`, a `tool_call_update`, and a permission
/// request's tool call; every one but the ID optional.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct ToolCallFields {
    pub tool_call_id: String,
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
    /// The tool's own name, when the agent's `_meta` gives one (Claude
    /// Code's adapter: `claudeCode.toolName`).
    pub(crate) fn tool_name(&self) -> Option<&str> {
        self.meta
            .as_ref()?
            .pointer("/claudeCode/toolName")
            .and_then(Value::as_str)
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
