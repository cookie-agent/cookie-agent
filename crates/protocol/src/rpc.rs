use std::{borrow::Cow, fmt};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::*;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, TS)]
#[ts(type = "\"2.0\"")]
pub struct JsonRpcVersion(());
impl JsonRpcVersion {
    #[must_use]
    pub const fn current() -> Self {
        Self(())
    }
}
impl Serialize for JsonRpcVersion {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str("2.0")
    }
}
impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(d)?;
        if value == "2.0" {
            Ok(Self::current())
        } else {
            Err(serde::de::Error::custom(
                "JSON-RPC version must be exactly 2.0",
            ))
        }
    }
}
impl JsonSchema for JsonRpcVersion {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("JsonRpcVersion")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","const":"2.0"})
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(untagged)]
pub enum JsonRpcId {
    Null,
    Number(i64),
    String(String),
}
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub params: Option<Value>,
}
impl Request {
    #[must_use]
    pub fn new(id: JsonRpcId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::current(),
            id,
            method: method.into(),
            params,
        }
    }
}
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("jsonrpc", &self.jsonrpc)
            .field("id", &self.id)
            .field("method", &self.method)
            .field("params", &self.params.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SuccessResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub result: Value,
}
impl fmt::Debug for SuccessResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuccessResponse")
            .field("jsonrpc", &self.jsonrpc)
            .field("id", &self.id)
            .field("result", &"<redacted>")
            .finish()
    }
}
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub data: Option<Value>,
}
impl fmt::Debug for JsonRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonRpcError")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("data", &self.data.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ErrorResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub error: JsonRpcError,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(untagged)]
pub enum Response {
    Success(SuccessResponse),
    Error(ErrorResponse),
}
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub params: Option<Value>,
}
impl Notification {
    #[must_use]
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::current(),
            method: method.into(),
            params,
        }
    }
}
impl fmt::Debug for Notification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notification")
            .field("jsonrpc", &self.jsonrpc)
            .field("method", &self.method)
            .field("params", &self.params.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ClientHello {
    pub protocol_version: ProtocolVersion,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ServerHello {
    pub protocol_version: ProtocolVersion,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateParams {
    pub selection: RunSelection,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateResult {
    pub session: SessionMeta,
}
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub cwd_identity: Option<CwdIdentity>,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionListResult {
    pub sessions: Vec<SessionMeta>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionGetParams {
    pub session_id: SessionId,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionGetResult {
    pub session: SessionMeta,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ChildSummary {
    pub session_id: SessionId,
    #[ts(type = "AgentId")]
    pub agent: AgentId,
    #[serde(deserialize_with = "deserialize_nullable_title")]
    #[schemars(with = "crate::NullableSchema<SessionTitle>", required)]
    pub title: Option<SessionTitle>,
    pub title_updated_seq: u64,
    pub status: SessionStatus,
    #[serde(deserialize_with = "deserialize_nullable_usage")]
    #[schemars(with = "crate::NullableSchema<Usage>", required)]
    pub usage: Option<Usage>,
}
fn deserialize_nullable_title<'de, D>(d: D) -> Result<Option<SessionTitle>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::deserialize(d)
}
fn deserialize_nullable_usage<'de, D>(d: D) -> Result<Option<Usage>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::deserialize(d)
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionTree {
    pub session: SessionMeta,
    pub children: Vec<SessionTree>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionChildrenParams {
    pub session_id: SessionId,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionChildrenResult {
    pub children: Vec<ChildSummary>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionTreeParams {
    pub session_id: SessionId,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionTreeResult {
    pub tree: SessionTree,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionResumeParams {
    pub session_id: SessionId,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionResumeResult {
    pub session: SessionMeta,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionSetPermissionModeParams {
    pub session_id: SessionId,
    pub mode: PermissionMode,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionSetPermissionModeResult {}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactParams {
    pub session_id: SessionId,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeDisplayText>", required)]
    pub focus: Option<SafeDisplayText>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactResult {
    pub compacted: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionRevertParams {
    pub session_id: SessionId,
    #[schemars(range(min = 1))]
    pub through_seq: u64,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionRevertResult {
    pub session: SessionMeta,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionForkParams {
    pub session_id: SessionId,
    #[schemars(range(min = 1))]
    pub through_seq: u64,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionForkResult {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionRenameChange {
    Set { title: SessionTitle },
    Clear,
    Reset,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameParams {
    pub session_id: SessionId,
    pub client_rename_id: ClientRenameId,
    pub change: SessionRenameChange,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameResult {
    pub client_rename_id: ClientRenameId,
    pub session: SessionMeta,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameRecord {
    pub client_rename_id: ClientRenameId,
    pub change: SessionRenameChange,
}
impl SessionRenameRecord {
    #[must_use]
    pub fn conflicts_with(&self, request: &SessionRenameParams) -> bool {
        self.client_rename_id == request.client_rename_id && self.change != request.change
    }
    #[must_use]
    pub fn matches(&self, request: &SessionRenameParams) -> bool {
        self.client_rename_id == request.client_rename_id && self.change == request.change
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SessionRenameErrorCode {
    SessionNotFound,
    InvalidTitle,
    IdempotencyConflict,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameError {
    pub code: SessionRenameErrorCode,
    pub session_id: SessionId,
    pub client_rename_id: ClientRenameId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunStartParams {
    pub session_id: SessionId,
    pub client_run_id: ClientRunId,
    pub selection: RunSelection,
    pub input: String,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RunStartConflictCode {
    IdempotencyConflict,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunStartConflict {
    pub code: RunStartConflictCode,
    pub session_id: SessionId,
    pub client_run_id: ClientRunId,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunStartResult {
    pub run_id: RunId,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunSteerParams {
    pub run_id: RunId,
    pub input: String,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunSteerResult {
    pub accepted: bool,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunRecallSteerParams {
    pub run_id: RunId,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunRecallSteerResult {
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub recalled: Option<String>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunCancelParams {
    pub run_id: RunId,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunCancelResult {
    pub cancelled: bool,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunToolStdinParams {
    pub run_id: RunId,
    pub call_id: ToolCallId,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub data: Option<String>,
    pub eof: bool,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunToolStdinResult {
    pub accepted: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct EventsSubscribeParams {
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub cursor: Option<u64>,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct EventsSubscribeResult {
    pub events: Vec<StoredEvent>,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRespondParams {
    pub session_id: SessionId,
    pub approval_id: ApprovalId,
    #[schemars(range(min = 1))]
    pub request_revision: u64,
    pub operation_fingerprint: OperationFingerprint,
    pub client_response_id: ClientResponseId,
    pub decision: ApprovalUserDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub feedback: Option<ApprovalFeedback>,
}
impl ApprovalRespondParams {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.request_revision == 0 {
            return Err("approval request revision must be positive");
        }
        if self.feedback.is_some() && self.decision != ApprovalUserDecision::Reject {
            return Err("approval feedback is allowed only with reject");
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for ApprovalRespondParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            session_id: SessionId,
            approval_id: ApprovalId,
            request_revision: u64,
            operation_fingerprint: OperationFingerprint,
            client_response_id: ClientResponseId,
            decision: ApprovalUserDecision,
            feedback: Option<ApprovalFeedback>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            session_id: wire.session_id,
            approval_id: wire.approval_id,
            request_revision: wire.request_revision,
            operation_fingerprint: wire.operation_fingerprint,
            client_response_id: wire.client_response_id,
            decision: wire.decision,
            feedback: wire.feedback,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRespondResult {
    pub client_response_id: ClientResponseId,
    pub approval: ApprovalRecord,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRespondErrorCode {
    ApprovalNotFound,
    ApprovalNotPending,
    ApprovalRevisionConflict,
    DecisionNotAllowed,
    OperationFingerprintMismatch,
    OperationChanged,
    IdempotencyConflict,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRespondError {
    pub code: ApprovalRespondErrorCode,
    pub session_id: SessionId,
    pub approval_id: ApprovalId,
    pub client_response_id: ClientResponseId,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub expected_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub found_revision: Option<u64>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalListParams {
    pub root_session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub status: Option<ApprovalStatus>,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalListResult {
    pub approvals: Vec<ApprovalRecord>,
    pub tree_grants: Vec<TreeApprovalGrant>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct McpApprovalListParams {}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct McpPendingApproval {
    pub server: String,
    pub digest: Sha256Digest,
    pub connection: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct McpApprovalListResult {
    pub approvals: Vec<McpPendingApproval>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum McpApprovalDecision {
    Approve,
    Reject,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct McpApprovalRespondParams {
    pub server: String,
    pub digest: Sha256Digest,
    pub decision: McpApprovalDecision,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct McpApprovalRespondResult {
    pub server: String,
    pub digest: Sha256Digest,
    pub decision: McpApprovalDecision,
}
