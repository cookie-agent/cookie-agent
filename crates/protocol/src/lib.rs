//! Versioned JSON-RPC wire types shared by CookieCode clients and the daemon.

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, TS)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub protocol_version: u32,
    pub id: JsonRpcId,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, TS)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub protocol_version: u32,
    pub id: JsonRpcId,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, TS)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, TS)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub protocol_version: u32,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionId(pub Uuid);

impl SessionId {
    #[must_use]
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
pub struct RunId(pub Uuid);

impl RunId {
    #[must_use]
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, TS)]
pub struct PersistedEvent {
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub sequence: u64,
    pub timestamp: Timestamp,
    pub event: Event,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionCreated {
        origin: SessionOrigin,
    },
    RunStarted,
    RunCompleted,
    RunFailed {
        message: String,
    },
    RunCancelled,
    RunInterrupted,
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallStarted {
        tool_call_id: String,
        tool: String,
    },
    ToolCallProgress {
        tool_call_id: String,
        message: String,
    },
    ToolCallCompleted {
        tool_call_id: String,
    },
    ToolCallFailed {
        tool_call_id: String,
        message: String,
    },
    ApprovalRequested {
        approval_id: String,
    },
    ApprovalResolved {
        approval_id: String,
        approved: bool,
    },
    ToolStdinSubmitted {
        tool_call_id: String,
        byte_count: u64,
    },
    ToolCallLinked {
        tool_call_id: String,
        child_session_id: SessionId,
    },
    ModelFallback {
        from: String,
        to: String,
        reason: String,
        attempts: u32,
    },
    UsageReported {
        model: String,
    },
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionOrigin {
    Root,
    Delegated {
        root_session_id: SessionId,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: String,
        invocation_id: String,
        depth: u32,
    },
    Forked {
        source_session_id: SessionId,
        source_event_seq: u64,
    },
}
