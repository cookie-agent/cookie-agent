use std::collections::BTreeMap;

use cookie_agent_identity::ProviderId;
use cookie_agent_models::ProviderDefinition;
use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::toml_values::{SensitiveProviderValues, zeroize_toml_value};
use zeroize::Zeroize;

const CONFIG_SCHEMA: u32 = 7;
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 7419;

/// Exact schema-7 marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigSchemaVersion;

impl Serialize for ConfigSchemaVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u32(CONFIG_SCHEMA)
    }
}
impl<'de> Deserialize<'de> for ConfigSchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        if value == CONFIG_SCHEMA {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("schema_version must be exactly 7"))
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub schema_version: ConfigSchemaVersion,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub tool_output: ToolOutputConfig,
    #[serde(default)]
    pub approval: ApprovalConfig,
    #[serde(default)]
    pub context_compaction: ContextCompactionConfig,
    #[serde(default)]
    pub session_title: SessionTitleConfig,
    #[serde(default)]
    pub providers: BTreeMap<ProviderId, ProviderDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRuntimeLayer {
    pub(crate) schema_version: ConfigSchemaVersion,
    pub(crate) server: Option<ServerConfig>,
    pub(crate) tool_output: Option<ToolOutputConfig>,
    pub(crate) approval: Option<ApprovalConfig>,
    pub(crate) context_compaction: Option<ContextCompactionConfig>,
    pub(crate) session_title: Option<SessionTitleConfig>,
    #[serde(default)]
    pub(crate) providers: SensitiveProviderValues,
}

impl Drop for RawRuntimeLayer {
    fn drop(&mut self) {
        if let Some(server) = &mut self.server {
            server.host.zeroize();
        }
        for value in self.providers.values_mut() {
            zeroize_toml_value(value.value_mut());
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}
fn default_host() -> String {
    DEFAULT_HOST.into()
}
const fn default_port() -> u16 {
    DEFAULT_PORT
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputConfig {
    #[serde(default = "default_output_lines")]
    pub max_lines: usize,
    #[serde(default = "default_output_bytes")]
    pub max_bytes: usize,
}
impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            max_lines: default_output_lines(),
            max_bytes: default_output_bytes(),
        }
    }
}
const fn default_output_lines() -> usize {
    2_000
}
const fn default_output_bytes() -> usize {
    50 * 1024
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalConfig {
    #[serde(default = "default_approval_timeout")]
    pub timeout_ms: u64,
}
impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            timeout_ms: default_approval_timeout(),
        }
    }
}
const fn default_approval_timeout() -> u64 {
    30_000
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextCompactionConfig {
    #[serde(default = "default_soft")]
    pub soft_threshold_percent: u8,
    #[serde(default = "default_hard")]
    pub hard_threshold_percent: u8,
    #[serde(default = "default_target")]
    pub target_percent: u8,
    #[serde(default = "default_summary")]
    pub max_summary_bytes: usize,
    #[serde(default = "default_native")]
    pub max_native_context_bytes: usize,
}
impl Default for ContextCompactionConfig {
    fn default() -> Self {
        Self {
            soft_threshold_percent: default_soft(),
            hard_threshold_percent: default_hard(),
            target_percent: default_target(),
            max_summary_bytes: default_summary(),
            max_native_context_bytes: default_native(),
        }
    }
}
const fn default_soft() -> u8 {
    70
}
const fn default_hard() -> u8 {
    85
}
const fn default_target() -> u8 {
    50
}
const fn default_summary() -> usize {
    256 * 1024
}
const fn default_native() -> usize {
    2 * 1024 * 1024
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTitleConfig {
    #[serde(default = "default_title_chars")]
    pub max_chars: usize,
    #[serde(default = "default_title_messages")]
    pub max_input_messages: usize,
    #[serde(default = "yes")]
    pub generate_on_first_turn: bool,
    #[serde(default = "yes")]
    pub fallback_to_input_excerpt: bool,
}
impl Default for SessionTitleConfig {
    fn default() -> Self {
        Self {
            max_chars: default_title_chars(),
            max_input_messages: default_title_messages(),
            generate_on_first_turn: true,
            fallback_to_input_excerpt: true,
        }
    }
}
const fn default_title_chars() -> usize {
    80
}
const fn default_title_messages() -> usize {
    4
}
const fn yes() -> bool {
    true
}

pub(crate) fn apply_settings(runtime: &mut RuntimeConfig, layer: &RawRuntimeLayer) {
    runtime.schema_version = layer.schema_version;
    if let Some(value) = &layer.server {
        runtime.server = value.clone();
    }
    if let Some(value) = &layer.tool_output {
        runtime.tool_output = value.clone();
    }
    if let Some(value) = &layer.approval {
        runtime.approval = value.clone();
    }
    if let Some(value) = &layer.context_compaction {
        runtime.context_compaction = value.clone();
    }
    if let Some(value) = &layer.session_title {
        runtime.session_title = value.clone();
    }
}

pub(crate) fn validate_runtime(runtime: &RuntimeConfig) -> Result<(), ConfigError> {
    if runtime.server.host.is_empty()
        || runtime.server.host.len() > 255
        || runtime.tool_output.max_lines == 0
        || runtime.tool_output.max_bytes == 0
        || runtime.approval.timeout_ms == 0
    {
        return Err(ConfigError::InvalidRuntime);
    }
    let context = &runtime.context_compaction;
    if !(context.target_percent < context.soft_threshold_percent
        && context.soft_threshold_percent < context.hard_threshold_percent
        && context.hard_threshold_percent <= 100)
        || context.max_summary_bytes == 0
        || context.max_summary_bytes > 2 * 1024 * 1024
        || context.max_native_context_bytes == 0
        || context.max_native_context_bytes > 2 * 1024 * 1024
    {
        return Err(ConfigError::InvalidRuntime);
    }
    if runtime.session_title.max_chars == 0 || runtime.session_title.max_input_messages == 0 {
        return Err(ConfigError::InvalidRuntime);
    }
    for (id, provider) in &runtime.providers {
        provider
            .validate_for(id)
            .map_err(|source| ConfigError::Provider {
                provider: id.clone(),
                source,
            })?;
    }
    Ok(())
}
