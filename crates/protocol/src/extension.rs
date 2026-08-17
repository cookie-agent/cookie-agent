use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

pub const EXTENSION_PROTOCOL_VERSION: &str = "0.0.1";
pub const PLUGIN_INITIALIZE_METHOD: &str = "plugin/initialize";
pub const PLUGIN_PING_METHOD: &str = "plugin/ping";
pub const PLUGIN_SHUTDOWN_METHOD: &str = "plugin/shutdown";

// Reserved for later protocol stages: plugin/tools/call, plugin/resources/list,
// plugin/resources/read, plugin/events/subscribe, and plugin/events/publish.

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, TS)]
#[ts(type = "\"0.0.1\"")]
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
        serializer.serialize_str(EXTENSION_PROTOCOL_VERSION)
    }
}

impl<'de> Deserialize<'de> for ExtensionProtocolVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let found = String::deserialize(deserializer)?;
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
        json_schema!({"type":"string","const":"0.0.1"})
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEngineCapabilities {
    pub ping: bool,
    pub shutdown: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPluginCapabilities {
    pub tools: bool,
    pub resources: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ExtensionInitializeParams {
    pub protocol_version: ExtensionProtocolVersion,
    pub engine_version: String,
    pub capabilities: ExtensionEngineCapabilities,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ExtensionToolDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub permission_name: String,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub primary_resource_param: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ExtensionInitializeResult {
    pub protocol_version: ExtensionProtocolVersion,
    pub name: String,
    pub version: String,
    pub capabilities: ExtensionPluginCapabilities,
    pub tools: Vec<ExtensionToolDeclaration>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPingParams {}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPingResult {}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ExtensionShutdownParams {}

#[must_use]
pub fn extension_initialize_request(engine_version: impl Into<String>) -> crate::Request {
    let params = ExtensionInitializeParams {
        protocol_version: ExtensionProtocolVersion::current(),
        engine_version: engine_version.into(),
        capabilities: ExtensionEngineCapabilities {
            ping: true,
            shutdown: true,
        },
    };
    crate::Request::new(
        crate::JsonRpcId::Number(1),
        PLUGIN_INITIALIZE_METHOD,
        Some(serde_json::to_value(params).expect("extension initialize params serialize")),
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
    fn protocol_version_rejection_reports_found_value() {
        let error = serde_json::from_str::<ExtensionProtocolVersion>("\"0.0.2\"")
            .expect_err("version mismatch");
        assert!(error.to_string().contains("0.0.2"));
    }
}
