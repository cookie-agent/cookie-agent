use std::{borrow::Cow, collections::BTreeMap, fmt};

use jiff::Timestamp;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::*;

pub const CATALOG_PROVIDER_LIST_METHOD: &str = "catalog.provider.list";
pub const CATALOG_MODEL_LIST_METHOD: &str = "catalog.model.list";
pub const MODEL_LIST_METHOD: &str = "model.list";
pub const AGENT_LIST_METHOD: &str = "agent.list";

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

#[derive(Clone, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderCredentials {
    #[schemars(extend("writeOnly"=true))]
    #[schemars(with = "CredentialValuesSchema")]
    pub values: BTreeMap<CredentialFieldName, String>,
}
struct CredentialValuesSchema;
impl JsonSchema for CredentialValuesSchema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CredentialValues")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type":"object",
            "minProperties":0,
            "maxProperties":32,
            "additionalProperties":false,
            "propertyNames":{"type":"string","minLength":1,"maxLength":1024,"pattern":"^[A-Z0-9_]+$"},
            "patternProperties":{
                "^[A-Z0-9_]+$":{"type":"string","minLength":1,"maxLength":16384,"writeOnly":true}
            },
            "writeOnly":true
        })
    }
}
impl fmt::Debug for ProviderCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderCredentials")
            .field("values", &"<redacted>")
            .finish()
    }
}
impl<'de> Deserialize<'de> for ProviderCredentials {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(deserialize_with = "deserialize_credential_values")]
            values: BTreeMap<CredentialFieldName, String>,
        }
        let w = Wire::deserialize(d)?;
        if w.values.len() > 32
            || w.values
                .values()
                .any(|value| value.is_empty() || value.len() > 16 * 1024)
        {
            return Err(serde::de::Error::custom(
                "credentials must contain at most 32 strict bounded nonempty fields",
            ));
        }
        Ok(Self { values: w.values })
    }
}

fn deserialize_credential_values<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<CredentialFieldName, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = BTreeMap<CredentialFieldName, String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map of unique credential fields")
        }

        fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((field, value)) = access.next_entry::<CredentialFieldName, String>()? {
                if values.len() >= 32 {
                    return Err(serde::de::Error::custom("credentials exceed 32 fields"));
                }
                if values.insert(field, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate credential field"));
                }
            }
            Ok(values)
        }
    }
    deserializer.deserialize_map(Visitor)
}

#[derive(Clone, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderConnectParams {
    pub client_connect_id: ClientConnectId,
    #[ts(type = "ProviderId")]
    pub provider_id: ProviderId,
    pub catalog_revision: CatalogRevision,
    pub credentials: ProviderCredentials,
}
impl fmt::Debug for ProviderConnectParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConnectParams")
            .field("client_connect_id", &self.client_connect_id)
            .field("provider_id", &self.provider_id)
            .field("catalog_revision", &self.catalog_revision)
            .field("credentials", &"<redacted>")
            .finish()
    }
}
impl<'de> Deserialize<'de> for ProviderConnectParams {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            client_connect_id: ClientConnectId,
            provider_id: ProviderId,
            catalog_revision: CatalogRevision,
            credentials: ProviderCredentials,
        }
        let w = Wire::deserialize(d)?;
        Ok(Self {
            client_connect_id: w.client_connect_id,
            provider_id: w.provider_id,
            catalog_revision: w.catalog_revision,
            credentials: w.credentials,
        })
    }
}
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderConnection {
    #[ts(type = "ProviderId")]
    pub provider_id: ProviderId,
    #[schemars(length(min = 1, max = 32))]
    pub credential_fields: Vec<CredentialFieldName>,
    pub connected_at: Timestamp,
    pub catalog_revision: CatalogRevision,
}
impl ProviderConnection {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.credential_fields.is_empty()
            || self.credential_fields.len() > 32
            || self
                .credential_fields
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err("credential fields must be strictly sorted and unique");
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for ProviderConnection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            provider_id: ProviderId,
            credential_fields: Vec<CredentialFieldName>,
            connected_at: Timestamp,
            catalog_revision: CatalogRevision,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            provider_id: wire.provider_id,
            credential_fields: wire.credential_fields,
            connected_at: wire.connected_at,
            catalog_revision: wire.catalog_revision,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderConnectResult {
    pub client_connect_id: ClientConnectId,
    pub connection: ProviderConnection,
    pub model_revision: SnapshotRevision,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProviderConnectErrorCode {
    UnknownProvider,
    UnsupportedProvider,
    CatalogRevisionConflict,
    MissingCredential,
    InvalidCredential,
    CredentialStorageFailed,
    IdempotencyConflict,
}
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderConnectError {
    pub code: ProviderConnectErrorCode,
    #[ts(type = "ProviderId")]
    pub provider_id: ProviderId,
    pub client_connect_id: ClientConnectId,
    #[schemars(length(max = 32))]
    pub missing_credential_fields: Vec<CredentialFieldName>,
}
impl ProviderConnectError {
    pub fn validate(&self) -> Result<(), &'static str> {
        let fields_are_exact = self.missing_credential_fields.len() <= 32
            && !self
                .missing_credential_fields
                .windows(2)
                .any(|pair| pair[0] >= pair[1]);
        if !fields_are_exact
            || (self.code == ProviderConnectErrorCode::MissingCredential)
                == self.missing_credential_fields.is_empty()
        {
            return Err(
                "missing credential fields must be sorted, unique, bounded, and code-exact",
            );
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for ProviderConnectError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            code: ProviderConnectErrorCode,
            provider_id: ProviderId,
            client_connect_id: ClientConnectId,
            missing_credential_fields: Vec<CredentialFieldName>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            code: wire.code,
            provider_id: wire.provider_id,
            client_connect_id: wire.client_connect_id,
            missing_credential_fields: wire.missing_credential_fields,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ModelListParams {}
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ModelListResult {
    pub revision: SnapshotRevision,
    pub generated_at: Timestamp,
    pub catalog_revision: CatalogRevision,
    #[schemars(length(max = 4096))]
    pub models: Vec<AvailableModelDescriptor>,
}
impl<'de> Deserialize<'de> for ModelListResult {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            revision: SnapshotRevision,
            generated_at: Timestamp,
            catalog_revision: CatalogRevision,
            models: Vec<AvailableModelDescriptor>,
        }
        let w = Wire::deserialize(d)?;
        if w.models.len() > 4096 || w.models.windows(2).any(|pair| pair[0].key >= pair[1].key) {
            return Err(serde::de::Error::custom(
                "models must be a strictly sorted unique list of at most 4096 entries",
            ));
        }
        Ok(Self {
            revision: w.revision,
            generated_at: w.generated_at,
            catalog_revision: w.catalog_revision,
            models: w.models,
        })
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ModelListErrorCode {
    ModelSnapshotUnavailable,
    ModelSnapshotInvalid,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ModelListError {
    pub code: ModelListErrorCode,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AgentListParams {}
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AgentListResult {
    pub revision: SnapshotRevision,
    pub model_revision: SnapshotRevision,
    pub generated_at: Timestamp,
    #[schemars(length(max = 4096))]
    pub agents: Vec<AgentDescriptor>,
}
impl<'de> Deserialize<'de> for AgentListResult {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            revision: SnapshotRevision,
            model_revision: SnapshotRevision,
            generated_at: Timestamp,
            agents: Vec<AgentDescriptor>,
        }
        let w = Wire::deserialize(d)?;
        if w.agents.len() > 4096 || w.agents.windows(2).any(|pair| pair[0].id >= pair[1].id) {
            return Err(serde::de::Error::custom(
                "agents must be a strictly sorted unique list of at most 4096 entries",
            ));
        }
        Ok(Self {
            revision: w.revision,
            model_revision: w.model_revision,
            generated_at: w.generated_at,
            agents: w.agents,
        })
    }
}
