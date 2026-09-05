use std::{collections::BTreeMap, time::Duration};

use cookie_agent_identity::{ModelKey, ProviderId};
pub use cookie_agent_models::catalog::PicoUsdPerMillion;
use cookie_agent_models::{
    HeaderName, ProviderDefinition, SafeStaticHeaderValue, deserialize_headers,
    validate_header_limits, validate_header_ownership,
};
use indexmap::IndexMap;
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

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PluginsConfig {
    #[serde(flatten)]
    pub plugins: IndexMap<String, PluginConfig>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_plugin_interception_timeout_ms")]
    pub interception_timeout_ms: u64,
    #[serde(default = "default_plugin_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    #[serde(default = "default_plugin_shutdown_grace_ms")]
    pub shutdown_grace_ms: u64,
    #[serde(default = "default_plugin_tool_timeout_ms")]
    pub tool_timeout_ms: u64,
}

impl PluginConfig {
    pub fn validate(&self, name: &str) -> Result<(), ConfigError> {
        self.invalid_field(name).map_or(Ok(()), |field| {
            Err(ConfigError::Plugin {
                plugin: name.to_owned(),
                reason: format!("field `{field}` is invalid"),
            })
        })
    }

    pub(crate) fn invalid_field(&self, name: &str) -> Option<&'static str> {
        if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            Some("name")
        } else if self.command.as_ref().is_none_or(String::is_empty) {
            Some("command")
        } else if self.cwd.as_ref().is_some_and(String::is_empty) {
            Some("cwd")
        } else if self.interception_timeout_ms == 0 {
            Some("interception_timeout_ms")
        } else if self.startup_timeout_ms == 0 {
            Some("startup_timeout_ms")
        } else if self.shutdown_grace_ms == 0 {
            Some("shutdown_grace_ms")
        } else if self.tool_timeout_ms == 0 {
            Some("tool_timeout_ms")
        } else {
            None
        }
    }
}

const fn default_plugin_interception_timeout_ms() -> u64 {
    2_000
}

const fn default_plugin_startup_timeout_ms() -> u64 {
    10_000
}

const fn default_plugin_shutdown_grace_ms() -> u64 {
    3_000
}

const fn default_plugin_tool_timeout_ms() -> u64 {
    30_000
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
    pub agent_md: AgentMdConfig,
    #[serde(default)]
    pub approval: ApprovalConfig,
    #[serde(default)]
    pub model_retry: ModelRetryConfig,
    #[serde(default)]
    pub context_compaction: ContextCompactionConfig,
    #[serde(default)]
    pub session_title: SessionTitleConfig,
    #[serde(default)]
    pub delegation: DelegationConfig,
    #[serde(default)]
    pub pricing: PricingConfig,
    #[serde(default, deserialize_with = "deserialize_headers")]
    pub headers: BTreeMap<HeaderName, SafeStaticHeaderValue>,
    #[serde(default)]
    pub providers: BTreeMap<ProviderId, ProviderDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRuntimeLayer {
    pub(crate) server: Option<ServerConfig>,
    pub(crate) tool_output: Option<ToolOutputConfig>,
    pub(crate) agent_md: Option<AgentMdConfig>,
    pub(crate) approval: Option<ApprovalConfig>,
    pub(crate) model_retry: Option<ModelRetryConfig>,
    pub(crate) context_compaction: Option<ContextCompactionConfig>,
    pub(crate) session_title: Option<SessionTitleConfig>,
    pub(crate) delegation: Option<DelegationConfig>,
    pub(crate) pricing: Option<PricingConfig>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_headers")]
    pub(crate) headers: Option<BTreeMap<HeaderName, SafeStaticHeaderValue>>,
    pub(crate) mcp: Option<McpConfig>,
    pub(crate) plugins: Option<PluginsConfig>,
    #[serde(default)]
    pub(crate) providers: SensitiveProviderValues,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelRetryConfig {
    #[serde(default = "default_standard_retries")]
    pub standard_retries: i64,
    #[serde(default = "default_overload_retries")]
    pub overload_retries: i64,
    #[serde(default = "default_backoff_ceiling_ms")]
    pub backoff_ceiling_ms: u64,
}

impl Default for ModelRetryConfig {
    fn default() -> Self {
        Self {
            standard_retries: default_standard_retries(),
            overload_retries: default_overload_retries(),
            backoff_ceiling_ms: default_backoff_ceiling_ms(),
        }
    }
}

const fn default_standard_retries() -> i64 {
    3
}

const fn default_overload_retries() -> i64 {
    5
}

const fn default_backoff_ceiling_ms() -> u64 {
    60_000
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingConfig {
    #[serde(default)]
    pub models: BTreeMap<ModelKey, ModelPricing>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    pub input_per_million_usd: Option<PicoUsdPerMillion>,
    pub output_per_million_usd: Option<PicoUsdPerMillion>,
    pub reasoning_per_million_usd: Option<PicoUsdPerMillion>,
    pub cache_read_per_million_usd: Option<PicoUsdPerMillion>,
    pub cache_write_per_million_usd: Option<PicoUsdPerMillion>,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentMdConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_agent_md_bytes")]
    pub max_bytes: usize,
}
impl Default for AgentMdConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: default_agent_md_bytes(),
        }
    }
}
const fn default_agent_md_bytes() -> usize {
    32 * 1024
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
    #[serde(default = "default_max_resident_subagents")]
    pub max_resident_subagents: usize,
    #[serde(
        default = "default_idle_eviction_after",
        deserialize_with = "deserialize_duration"
    )]
    pub idle_eviction_after: Duration,
}
impl Default for DelegationConfig {
    fn default() -> Self {
        Self {
            max_depth: default_delegation_depth(),
            max_concurrency: default_delegation_concurrency(),
            max_resident_subagents: default_max_resident_subagents(),
            idle_eviction_after: default_idle_eviction_after(),
        }
    }
}
const fn default_delegation_depth() -> u32 {
    3
}
const fn default_delegation_concurrency() -> Option<u32> {
    Some(4)
}
const fn default_max_resident_subagents() -> usize {
    20
}
const fn default_idle_eviction_after() -> Duration {
    Duration::from_secs(60 * 60)
}

fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    parse_duration(&value).ok_or_else(|| {
        serde::de::Error::custom("duration must be an integer followed by ms, s, m, h, or d")
    })
}

fn parse_duration(value: &str) -> Option<Duration> {
    let unit_start = value.find(|character: char| !character.is_ascii_digit())?;
    let amount = value[..unit_start].parse::<u64>().ok()?;
    let unit = &value[unit_start..];
    let milliseconds = match unit {
        "ms" => amount,
        "s" => amount.checked_mul(1_000)?,
        "m" => amount.checked_mul(60_000)?,
        "h" => amount.checked_mul(3_600_000)?,
        "d" => amount.checked_mul(86_400_000)?,
        _ => return None,
    };
    Some(Duration::from_millis(milliseconds))
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
    /// Recent-history tail token budget; zero disables retention. Runtime caps it to available space.
    pub keep_recent_tokens: u64,
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
    #[serde(default = "default_keep_recent_tokens")]
    keep_recent_tokens: u64,
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
            keep_recent_tokens: raw.keep_recent_tokens,
        })
    }
}

impl Default for ContextCompactionConfig {
    fn default() -> Self {
        Self {
            auto_compaction: true,
            trigger: default_compaction_trigger(),
            max_summary_bytes: default_summary(),
            keep_recent_tokens: default_keep_recent_tokens(),
        }
    }
}
const fn default_compaction_trigger() -> ContextCompactionTrigger {
    ContextCompactionTrigger::Percent { percent: 70 }
}
const fn default_summary() -> usize {
    256 * 1024
}
const fn default_keep_recent_tokens() -> u64 {
    16_384
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
    if let Some(value) = &layer.agent_md {
        runtime.agent_md = value.clone();
    }
    if let Some(value) = &layer.approval {
        runtime.approval = value.clone();
    }
    if let Some(value) = layer.model_retry {
        runtime.model_retry = value;
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
    if let Some(value) = &layer.pricing {
        runtime.pricing = value.clone();
    }
    if let Some(values) = &layer.headers {
        for (name, value) in values {
            if value.as_str().is_empty() {
                runtime.headers.remove(name);
            } else {
                runtime.headers.insert(name.clone(), value.clone());
            }
        }
    }
}

pub(crate) fn validate_runtime(runtime: &RuntimeConfig) -> Result<(), ConfigError> {
    validate_header_ownership(&runtime.headers, "global").map_err(ConfigError::HeaderOwnership)?;
    validate_header_limits(&runtime.headers).map_err(|_| ConfigError::InvalidRuntime)?;
    if runtime.server.host.is_empty()
        || runtime.server.host.len() > 255
        || runtime.tool_output.max_lines == 0
        || runtime.tool_output.max_bytes == 0
        || runtime.agent_md.max_bytes == 0
        || runtime.agent_md.max_bytes > 2 * 1024 * 1024
        || runtime.approval.timeout_ms == 0
        || runtime.model_retry.backoff_ceiling_ms == 0
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

fn deserialize_optional_headers<'de, D>(
    deserializer: D,
) -> Result<Option<BTreeMap<HeaderName, SafeStaticHeaderValue>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_headers(deserializer).map(Some)
}
