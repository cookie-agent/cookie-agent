use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// The handshake feature the daemon sends when the plugin's configuration
/// lets it produce messages (`producer_messaging = true`).
pub const FEATURE_PRODUCER_MESSAGING: &str = "producer_messaging";

crate::open_enum! {
    /// A hook a plugin intercepts. Open: the daemon skips a hook it does not
    /// know, with a diagnostic, and the plugin still starts.
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
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionPluginCapabilities {
    // Explicit opt-in, off by default in plugin configuration and SDK builders.
    pub producer_messaging: bool,
    pub subscribe_events: bool,
    pub subscribe_bus: bool,
    pub publish_bus: bool,
    pub intercept: Vec<ExtensionInterceptionHook>,
}

/// `plugin/initialize` params: the daemon's version and what its
/// configuration enables for this plugin.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionInitializeParams {
    /// The daemon's [`crate::PLUGIN_PROTOCOL_VERSION`].
    pub protocol_version: crate::SemVer,
    /// The daemon's own version, for logs and diagnostics.
    pub engine_version: String,
    /// What the plugin's configuration enables, such as
    /// [`FEATURE_PRODUCER_MESSAGING`]. A version never implies a feature.
    #[serde(default)]
    pub features: Vec<String>,
    /// How long the daemon waits for a hook's answer, and for the plugin's
    /// `plugin/emit` to be answered with `plugin/emit_result`.
    pub interception_timeout_ms: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionInitializeResult {
    /// The plugin's [`crate::PLUGIN_PROTOCOL_VERSION`].
    pub protocol_version: crate::SemVer,
    pub name: String,
    pub version: String,
    /// What the plugin's own configuration enables. None are defined yet;
    /// the daemon ignores one it does not know, with a diagnostic.
    #[serde(default)]
    pub features: Vec<String>,
    pub capabilities: ExtensionPluginCapabilities,
    /// The plugin's tools and their prompt sections, declared as a runtime
    /// declares them; it runs them with the runtime protocol's `tool/*`
    /// messages.
    #[serde(default)]
    pub tools: crate::runtime_protocol::RuntimeTools,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionShutdownParams {}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionEventParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    #[schemars(range(min = 1))]
    pub seq: u64,
    /// The stored event's payload; an event type the plugin does not know
    /// reads as [`crate::Open::Unknown`].
    pub event: crate::Open<crate::EventPayload>,
    pub timestamp: jiff::Timestamp,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
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

/// Open: a status this build does not know reads as `dropped`: the event
/// was not published as far as the plugin can tell.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ExtensionEmitStatus {
    Published,
    Rejected,
    #[serde(other)]
    Dropped,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionEmitResultParams {
    pub name: String,
    pub bus: ExtensionEmitStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Where a tool a hook sees comes from. Hook params carry it as
/// [`crate::Open`]: an origin of a kind the plugin does not know is another
/// source it cannot name.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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

crate::open_enum!(tagged ExtensionToolOrigin, tag = "kind");

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionToolBeforeCallParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub tool: String,
    pub origin: crate::Open<ExtensionToolOrigin>,
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
pub struct ExtensionToolAfterResultParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub tool: String,
    /// `null` for a call that names no published tool.
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(
        with = "crate::NullableSchema<crate::Open<ExtensionToolOrigin>>",
        required
    )]
    pub origin: Option<crate::Open<ExtensionToolOrigin>>,
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
pub struct ExtensionToolAfterResultResult {
    pub action: ExtensionToolAfterResultAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_content: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionAgentBeforeStartParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub agent_path: String,
    pub prompt_context: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionAgentBeforeStartResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_to_system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_message: Option<ExtensionInjectedMessage>,
}

/// The role of a model message. Closed by design: a role is a part of the
/// model's history every reader must place, so a new one is a breaking
/// change. Session history stores it too (`message_injected`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionMessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionInjectedMessage {
    pub role: ExtensionMessageRole,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionSessionBeforeCompactParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub checkpoint_id: String,
    pub additions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
pub struct ExtensionUserBeforeInputResult {
    pub action: ExtensionUserBeforeInputAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionModelMessage {
    pub role: ExtensionMessageRole,
    pub content: Value,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionModelParamsAdjustments {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
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
pub struct ExtensionModelBeforeRequestResult {
    pub action: ExtensionModelBeforeRequestAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<ExtensionModelMessage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params_adjustments: Option<ExtensionModelParamsAdjustments>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
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
pub struct ExtensionProviderBeforeRequestResult {
    pub action: ExtensionProviderBeforeRequestAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionProviderAfterResponseParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub attempt_id: crate::AttemptId,
    pub status: u16,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionProviderAfterResponseResult {}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
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
pub struct ExtensionMessageEndResult {
    pub action: ExtensionMessageEndAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<crate::PersistedAssistantPart>>,
}

crate::open_enum! {
    /// Open: a source from a newer daemon reads as `Unknown`.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ExtensionModelSelectSource {
        User,
        Config,
        /// The engine moves to the next model of the fallback chain.
        Fallback,
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
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
pub struct ExtensionAllowBlockResult {
    pub action: ExtensionAllowBlockAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ExtensionSessionBeforeForkParams {
    pub session_id: crate::SessionId,
    pub context_id: String,
    pub through_seq: u64,
}

pub type ExtensionSessionBeforeForkResult = ExtensionAllowBlockResult;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
pub struct ExtensionSessionBeforeRevertResult {
    pub action: ExtensionSessionBeforeRevertAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_override: Option<String>,
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
        // A result is information: a member from a newer daemon is ignored.
        assert!(
            serde_json::from_value::<crate::ProducerDiscardResult>(
                serde_json::json!({"message_id": params.message_id})
            )
            .is_ok()
        );
    }
}
