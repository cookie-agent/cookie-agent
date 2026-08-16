use std::collections::BTreeMap;

use cookie_agent_identity::ProviderId;
use cookie_agent_models::ProviderDefinition;
use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::toml_values::{SensitiveProviderValues, zeroize_toml_value};
use zeroize::Zeroize;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 7419;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "McpOAuthConfig::is_auto")]
    pub oauth: McpOAuthConfig,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub lazy: bool,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpOAuthSettings {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub client_metadata_url: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for McpOAuthSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpOAuthSettings")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("client_metadata_url", &self.client_metadata_url)
            .field("scopes", &self.scopes)
            .finish()
    }
}

#[derive(Clone, Default, Eq, PartialEq)]
pub enum McpOAuthConfig {
    #[default]
    Auto,
    Enabled,
    Disabled,
    Settings(McpOAuthSettings),
}

impl McpOAuthConfig {
    #[must_use]
    pub const fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    #[must_use]
    pub fn settings(&self) -> McpOAuthSettings {
        match self {
            Self::Settings(settings) => settings.clone(),
            Self::Auto | Self::Enabled | Self::Disabled => McpOAuthSettings::default(),
        }
    }
}

impl std::fmt::Debug for McpOAuthConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => formatter.write_str("Auto"),
            Self::Enabled => formatter.write_str("Enabled"),
            Self::Disabled => formatter.write_str("Disabled"),
            Self::Settings(settings) => formatter
                .debug_struct("Settings")
                .field("client_id", &settings.client_id)
                .field(
                    "client_secret",
                    &settings.client_secret.as_ref().map(|_| "[REDACTED]"),
                )
                .field("client_metadata_url", &settings.client_metadata_url)
                .field("scopes", &settings.scopes)
                .finish(),
        }
    }
}

impl Serialize for McpOAuthConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Auto | Self::Enabled => serializer.serialize_bool(true),
            Self::Disabled => serializer.serialize_bool(false),
            Self::Settings(settings) => settings.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for McpOAuthConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OAuthValue {
            Bool(bool),
            Settings(McpOAuthSettings),
        }

        Ok(match OAuthValue::deserialize(deserializer)? {
            OAuthValue::Bool(true) => Self::Enabled,
            OAuthValue::Bool(false) => Self::Disabled,
            OAuthValue::Settings(settings) => Self::Settings(settings),
        })
    }
}

impl McpServerConfig {
    pub fn validate(&self, name: &str) -> Result<(), ConfigError> {
        let invalid_name =
            name.is_empty() || name.len() > 128 || name.chars().any(char::is_control);
        let transport_count = usize::from(self.command.is_some()) + usize::from(self.url.is_some());
        let invalid_stdio = self.command.as_ref().is_some_and(String::is_empty)
            || self.cwd.as_ref().is_some_and(String::is_empty);
        let invalid_remote = self.url.as_ref().is_some_and(|url| {
            url::Url::parse(url)
                .map(|parsed| !matches!(parsed.scheme(), "http" | "https"))
                .unwrap_or(true)
        });
        let mixed = self.command.is_some() && (!self.headers.is_empty() || !self.oauth.is_auto())
            || self.url.is_some()
                && (!self.args.is_empty() || !self.env.is_empty() || self.cwd.is_some());
        let invalid_oauth = match &self.oauth {
            McpOAuthConfig::Settings(settings) => {
                settings.client_id.as_ref().is_some_and(String::is_empty)
                    || settings
                        .client_secret
                        .as_ref()
                        .is_some_and(String::is_empty)
                    || settings.client_secret.is_some() && settings.client_id.is_none()
                    || settings.scopes.iter().any(String::is_empty)
                    || settings.client_metadata_url.as_ref().is_some_and(|value| {
                        url::Url::parse(value)
                            .map(|url| {
                                url.scheme() != "https"
                                    || url.host_str().is_none()
                                    || url.path() == "/"
                            })
                            .unwrap_or(true)
                    })
            }
            McpOAuthConfig::Auto | McpOAuthConfig::Enabled | McpOAuthConfig::Disabled => false,
        };
        if invalid_name
            || transport_count != 1
            || invalid_stdio
            || invalid_remote
            || mixed
            || invalid_oauth
            || self.timeout_ms == Some(0)
        {
            return Err(ConfigError::McpServer {
                server: name.to_owned(),
                reason: "configure exactly one of command or url; stdio-only and remote-only fields may not be mixed; OAuth client settings must be valid; and timeout_ms must be positive".into(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
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
    pub delegation: DelegationConfig,
    #[serde(default)]
    pub providers: BTreeMap<ProviderId, ProviderDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRuntimeLayer {
    pub(crate) server: Option<ServerConfig>,
    pub(crate) tool_output: Option<ToolOutputConfig>,
    pub(crate) approval: Option<ApprovalConfig>,
    pub(crate) context_compaction: Option<ContextCompactionConfig>,
    pub(crate) session_title: Option<SessionTitleConfig>,
    pub(crate) delegation: Option<DelegationConfig>,
    pub(crate) mcp: Option<McpConfig>,
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

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationConfig {
    #[serde(default = "default_delegation_depth")]
    pub max_depth: u32,
    #[serde(default = "default_delegation_concurrency")]
    pub max_concurrency: Option<u32>,
}
impl Default for DelegationConfig {
    fn default() -> Self {
        Self {
            max_depth: default_delegation_depth(),
            max_concurrency: default_delegation_concurrency(),
        }
    }
}
const fn default_delegation_depth() -> u32 {
    3
}
const fn default_delegation_concurrency() -> Option<u32> {
    Some(4)
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

#[derive(Clone, Debug)]
pub struct ContextCompactionConfig {
    pub auto_compaction: bool,
    pub trigger: ContextCompactionTrigger,
    pub max_summary_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ContextCompactionTrigger {
    Percent { percent: u8 },
    BufferTokens { buffer_tokens: u64 },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawContextCompactionConfig {
    #[serde(default = "yes", rename = "auto")]
    auto_compaction: bool,
    trigger: Option<ContextCompactionTrigger>,
    buffer_tokens: Option<u64>,
    #[serde(default = "default_summary")]
    max_summary_bytes: usize,
}

impl<'de> Deserialize<'de> for ContextCompactionConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawContextCompactionConfig::deserialize(deserializer)?;
        let trigger = match (raw.trigger, raw.buffer_tokens) {
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "trigger and buffer_tokens cannot both be set",
                ));
            }
            (Some(trigger), None) => trigger,
            (None, Some(buffer_tokens)) => ContextCompactionTrigger::BufferTokens { buffer_tokens },
            (None, None) => default_compaction_trigger(),
        };
        Ok(Self {
            auto_compaction: raw.auto_compaction,
            trigger,
            max_summary_bytes: raw.max_summary_bytes,
        })
    }
}

impl Default for ContextCompactionConfig {
    fn default() -> Self {
        Self {
            auto_compaction: true,
            trigger: default_compaction_trigger(),
            max_summary_bytes: default_summary(),
        }
    }
}
const fn default_compaction_trigger() -> ContextCompactionTrigger {
    ContextCompactionTrigger::Percent { percent: 70 }
}
const fn default_summary() -> usize {
    256 * 1024
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
    if let Some(value) = &layer.delegation {
        runtime.delegation = value.clone();
    }
}

pub(crate) fn validate_runtime(runtime: &RuntimeConfig) -> Result<(), ConfigError> {
    if runtime.server.host.is_empty()
        || runtime.server.host.len() > 255
        || runtime.tool_output.max_lines == 0
        || runtime.tool_output.max_bytes == 0
        || runtime.approval.timeout_ms == 0
        || runtime.delegation.max_depth == 0
        || runtime.delegation.max_concurrency == Some(0)
    {
        return Err(ConfigError::InvalidRuntime);
    }
    let context = &runtime.context_compaction;
    let invalid_trigger = match &context.trigger {
        ContextCompactionTrigger::Percent { percent } => !(1..=99).contains(percent),
        ContextCompactionTrigger::BufferTokens { buffer_tokens } => *buffer_tokens == 0,
    };
    if invalid_trigger
        || context.max_summary_bytes == 0
        || context.max_summary_bytes > 2 * 1024 * 1024
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
