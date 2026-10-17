use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The plugin protocol version. Both ends must speak exactly this version.
pub const EXTENSION_PROTOCOL_VERSION: u32 = 1;
// Producer messaging uses the runtime protocol's method names.
pub const PLUGIN_PRODUCER_REGISTER_METHOD: &str = "producer/register";
pub const PLUGIN_PRODUCER_SEND_METHOD: &str = "producer/send";
pub const PLUGIN_PRODUCER_UNREGISTER_METHOD: &str = "producer/unregister";
pub const PLUGIN_PRODUCER_DISCARD_METHOD: &str = "producer/discard";
pub const PLUGIN_RECOVERY_START_METHOD: &str = "producer/recovery/start";
pub const PLUGIN_RECOVERY_COMPLETE_METHOD: &str = "producer/recovery/complete";
pub const PLUGIN_INITIALIZE_METHOD: &str = "plugin/initialize";
pub const PLUGIN_SHUTDOWN_METHOD: &str = "plugin/shutdown";
pub const PLUGIN_EVENT_METHOD: &str = "plugin/event";
pub const PLUGIN_BUS_EVENT_METHOD: &str = "plugin/bus_event";
pub const PLUGIN_EMIT_METHOD: &str = "plugin/emit";
pub const PLUGIN_EMIT_RESULT_METHOD: &str = "plugin/emit_result";
pub const PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD: &str = "plugin/intercept/tool_before_call";
pub const PLUGIN_INTERCEPT_TOOL_AFTER_RESULT_METHOD: &str = "plugin/intercept/tool_after_result";
pub const PLUGIN_INTERCEPT_AGENT_BEFORE_START_METHOD: &str = "plugin/intercept/agent_before_start";
pub const PLUGIN_INTERCEPT_SESSION_BEFORE_COMPACT_METHOD: &str =
    "plugin/intercept/session_before_compact";
pub const PLUGIN_INTERCEPT_USER_BEFORE_INPUT_METHOD: &str = "plugin/intercept/user_before_input";
pub const PLUGIN_INTERCEPT_MODEL_BEFORE_REQUEST_METHOD: &str =
    "plugin/intercept/model_before_request";
pub const PLUGIN_INTERCEPT_PROVIDER_BEFORE_REQUEST_METHOD: &str =
    "plugin/intercept/provider_before_request";
pub const PLUGIN_INTERCEPT_PROVIDER_AFTER_RESPONSE_METHOD: &str =
    "plugin/intercept/provider_after_response";
pub const PLUGIN_INTERCEPT_MESSAGE_END_METHOD: &str = "plugin/intercept/message_end";
pub const PLUGIN_INTERCEPT_MODEL_BEFORE_SELECT_METHOD: &str =
    "plugin/intercept/model_before_select";
pub const PLUGIN_INTERCEPT_SESSION_BEFORE_FORK_METHOD: &str =
    "plugin/intercept/session_before_fork";
pub const PLUGIN_INTERCEPT_SESSION_BEFORE_REVERT_METHOD: &str =
    "plugin/intercept/session_before_revert";

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ExtensionProtocolVersion(());

impl ExtensionProtocolVersion {
    #[must_use]
    pub const fn current() -> Self {
        Self(())
    }
}

impl Serialize for ExtensionProtocolVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u32(EXTENSION_PROTOCOL_VERSION)
    }
}

impl<'de> Deserialize<'de> for ExtensionProtocolVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let found = u32::deserialize(deserializer)?;
        if found == EXTENSION_PROTOCOL_VERSION {
            Ok(Self::current())
        } else {
            Err(serde::de::Error::custom(format!(
                "extension protocol version must be exactly {EXTENSION_PROTOCOL_VERSION}; found {found}"
            )))
        }
    }
}

impl JsonSchema for ExtensionProtocolVersion {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ExtensionProtocolVersion")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"integer","const":EXTENSION_PROTOCOL_VERSION})
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEngineCapabilities {
    /// The daemon accepts this plugin's producer messages: the plugin's
    /// configuration enables them.
    pub producer_messaging: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionInterceptionHook {
    ToolBeforeCall,
    ToolAfterResult,
    AgentBeforeStart,
    SessionBeforeCompact,
    UserBeforeInput,
    ModelBeforeRequest,
    ProviderBeforeRequest,
    ProviderAfterResponse,
    MessageEnd,
    ModelBeforeSelect,
    SessionBeforeFork,
    SessionBeforeRevert,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPluginCapabilities {
    // Explicit opt-in, off by default in plugin configuration and SDK builders.
    pub producer_messaging: bool,
    pub subscribe_events: bool,
    pub subscribe_bus: bool,
    pub publish_bus: bool,
    pub intercept: Vec<ExtensionInterceptionHook>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionInitializeParams {
    pub protocol_version: ExtensionProtocolVersion,
    pub engine_version: String,
    pub capabilities: ExtensionEngineCapabilities,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionInitializeResult {
    pub protocol_version: ExtensionProtocolVersion,
    pub name: String,
    pub version: String,
    pub capabilities: ExtensionPluginCapabilities,
    /// The plugin's tools and their prompt sections, declared as a runtime
    /// declares them; it runs them with the runtime protocol's `tool/*`
    /// messages.
    #[serde(default)]
    pub tools: crate::runtime_protocol::RuntimeTools,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionShutdownParams {}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEventParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    #[schemars(range(min = 1))]
    pub seq: u64,
    pub event: crate::EventPayload,
    pub timestamp: jiff::Timestamp,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionBusEventParams {
    pub session_id: crate::SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub plugin: String,
    pub name: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// Non-model publication only. Model-bound content must use producer.send with
/// an explicitly registered producer; emit must never implicitly register one.
pub struct ExtensionEmitParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    pub payload: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionEmitStatus {
    Published,
    Dropped,
    Rejected,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEmitResultParams {
    pub name: String,
    pub bus: ExtensionEmitStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Where a tool a hook sees comes from.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionToolOrigin {
    /// The daemon's base layer.
    Base,
    /// A runtime the session is attached to.
    Runtime {
        /// The session's attachment name.
        attachment: String,
        runtime_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionToolBeforeCallParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub tool: String,
    pub origin: ExtensionToolOrigin,
    pub arguments: Value,
    /// `null` for a tool that needs no permission.
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub permission_name: Option<String>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub resource: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionToolBeforeCallAction {
    Allow,
    Block,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionToolBeforeCallResult {
    pub action: ExtensionToolBeforeCallAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_arguments: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_to_model: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionToolAfterResultParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub tool: String,
    /// `null` for a call that names no published tool.
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ExtensionToolOrigin>", required)]
    pub origin: Option<ExtensionToolOrigin>,
    pub arguments: Value,
    pub result_content: String,
    pub is_error: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionToolAfterResultAction {
    Keep,
    Replace,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionToolAfterResultResult {
    pub action: ExtensionToolAfterResultAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_content: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionAgentBeforeStartParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub agent_path: String,
    pub prompt_context: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionAgentBeforeStartResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_to_system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_message: Option<ExtensionInjectedMessage>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionMessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionInjectedMessage {
    pub role: ExtensionMessageRole,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSessionBeforeCompactParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub checkpoint_id: String,
    pub additions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSessionBeforeCompactResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addendum: Option<String>,
    #[serde(default)]
    pub cancel: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_override: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionUserBeforeInputParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionUserBeforeInputAction {
    Allow,
    Transform,
    Handled,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionUserBeforeInputResult {
    pub action: ExtensionUserBeforeInputAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionModelMessage {
    pub role: ExtensionMessageRole,
    pub content: Value,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionModelParamsAdjustments {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionModelBeforeRequestParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub attempt_id: crate::AttemptId,
    pub messages: Vec<ExtensionModelMessage>,
    pub model: crate::ResolvedModelRef,
    pub params: ExtensionModelParamsAdjustments,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionModelBeforeRequestAction {
    Keep,
    Replace,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionModelBeforeRequestResult {
    pub action: ExtensionModelBeforeRequestAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<ExtensionModelMessage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params_adjustments: Option<ExtensionModelParamsAdjustments>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionProviderBeforeRequestParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub attempt_id: crate::AttemptId,
    pub payload: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionProviderBeforeRequestAction {
    Keep,
    Replace,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionProviderBeforeRequestResult {
    pub action: ExtensionProviderBeforeRequestAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionProviderAfterResponseParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub attempt_id: crate::AttemptId,
    pub status: u16,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionProviderAfterResponseResult {}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionMessageEndParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub attempt_id: crate::AttemptId,
    pub content: Vec<crate::PersistedAssistantPart>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionMessageEndAction {
    Keep,
    Replace,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionMessageEndResult {
    pub action: ExtensionMessageEndAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<crate::PersistedAssistantPart>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionModelSelectSource {
    User,
    Config,
    /// The engine moves to the next model of the fallback chain.
    Fallback,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionModelBeforeSelectParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<cookie_agent_identity::ModelSelection>,
    pub to: cookie_agent_identity::ModelSelection,
    pub source: ExtensionModelSelectSource,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionAllowBlockAction {
    Allow,
    Block,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionAllowBlockResult {
    pub action: ExtensionAllowBlockAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSessionBeforeForkParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub through_seq: u64,
}

pub type ExtensionSessionBeforeForkResult = ExtensionAllowBlockResult;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSessionBeforeRevertParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub through_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSessionBeforeRevertAction {
    Allow,
    Block,
    Override,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSessionBeforeRevertResult {
    pub action: ExtensionSessionBeforeRevertAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_override: Option<String>,
}

#[must_use]
pub fn extension_initialize_request(engine_version: impl Into<String>) -> crate::Request {
    let params = ExtensionInitializeParams {
        protocol_version: ExtensionProtocolVersion::current(),
        engine_version: engine_version.into(),
        capabilities: ExtensionEngineCapabilities {
            producer_messaging: true,
        },
    };
    crate::Request::new(
        crate::JsonRpcId::Number(1),
        PLUGIN_INITIALIZE_METHOD,
        Some(serde_json::to_value(params).expect("extension initialize params serialize")),
    )
}

#[must_use]
pub fn extension_recovery_start_notification() -> crate::Notification {
    crate::Notification::new(
        PLUGIN_RECOVERY_START_METHOD,
        Some(
            serde_json::to_value(crate::RecoveryStartParams {})
                .expect("recovery start params serialize"),
        ),
    )
}

#[must_use]
pub fn extension_shutdown_notification() -> crate::Notification {
    crate::Notification::new(PLUGIN_SHUTDOWN_METHOD, Some(serde_json::json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_discard_is_strict_and_registration_independent() {
        let params = crate::ProducerDiscardParams {
            session_id: crate::SessionId::new_v7(),
            message_id: crate::ProducerMessageId::new_v7(),
        };
        let wire = serde_json::to_value(params).unwrap();
        assert_eq!(
            serde_json::from_value::<crate::ProducerDiscardParams>(wire.clone()).unwrap(),
            params
        );
        assert_eq!(
            wire,
            serde_json::json!({"session_id": params.session_id, "message_id": params.message_id})
        );
        for field in ["producer_id", "plugin", "connection_epoch"] {
            let mut forged = wire.clone();
            forged[field] = serde_json::json!("foreign");
            assert!(serde_json::from_value::<crate::ProducerDiscardParams>(forged).is_err());
        }
        for field in ["session_id", "message_id"] {
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<crate::ProducerDiscardParams>(missing).is_err());
            let mut invalid = wire.clone();
            invalid[field] = serde_json::json!("not-a-uuid");
            assert!(serde_json::from_value::<crate::ProducerDiscardParams>(invalid).is_err());
        }
        assert_eq!(PLUGIN_PRODUCER_DISCARD_METHOD, "producer/discard");
        assert_eq!(
            serde_json::to_value(crate::ProducerDiscardResult {}).unwrap(),
            serde_json::json!({})
        );
        assert!(
            serde_json::from_value::<crate::ProducerDiscardResult>(serde_json::json!({})).is_ok()
        );
        assert!(
            serde_json::from_value::<crate::ProducerDiscardResult>(
                serde_json::json!({"message_id": params.message_id})
            )
            .is_err()
        );
    }

    #[test]
    fn protocol_version_rejection_reports_found_value() {
        let error =
            serde_json::from_str::<ExtensionProtocolVersion>("7").expect_err("version mismatch");
        assert!(error.to_string().contains('7'));
        assert!(serde_json::from_str::<ExtensionProtocolVersion>("\"1\"").is_err());
    }
}
