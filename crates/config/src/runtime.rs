use std::{collections::BTreeMap, time::Duration};

use cookie_agent_identity::{ConfiguredVariantRef, ModelKey, ProviderId};
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
    /// The program and its arguments.
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub producer_messaging: bool,
    #[serde(default = "default_plugin_interception_timeout_ms")]
    pub interception_timeout_ms: u64,
    #[serde(default = "default_plugin_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    #[serde(default = "default_plugin_shutdown_grace_ms")]
    pub shutdown_grace_ms: u64,
}

impl PluginConfig {
    pub(crate) fn invalid_field(&self, name: &str) -> Option<&'static str> {
        if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            Some("name")
        } else if self.command.first().is_none_or(String::is_empty) {
            Some("command")
        } else if self.cwd.as_ref().is_some_and(String::is_empty) {
            Some("cwd")
        } else if self.interception_timeout_ms == 0 {
            Some("interception_timeout_ms")
        } else if self.startup_timeout_ms == 0 {
            Some("startup_timeout_ms")
        } else if self.shutdown_grace_ms == 0 {
            Some("shutdown_grace_ms")
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
    pub timeout_ms: Option<u64>,
    /// Where a runtime hosts the server; only runtime templates' servers
    /// (`[runtimes.<id>.mcp.servers.<name>]`) take it. `None` is
    /// [`McpServerScope::Workspace`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<McpServerScope>,
    /// Patterns over the server's own tool names (`*` any characters, `?`
    /// one character): only matching tools are published. `None` publishes
    /// every tool; present, it may not be empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_tools: Option<Vec<String>>,
    /// Patterns over the server's own tool names, applied after
    /// `include_tools`: a matching tool is not published.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_tools: Vec<String>,
}

/// `scope` of a runtime template's MCP server.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerScope {
    /// One process per runtime, started when the runtime initializes and
    /// shared by every workspace it opens.
    Runtime,
    /// One process per workspace instance, started when it opens.
    #[default]
    Workspace,
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
    /// Where a runtime hosts the server.
    #[must_use]
    pub fn hosted_scope(&self) -> McpServerScope {
        self.scope.unwrap_or_default()
    }

    /// [`Self::validate`] for a top-level `[mcp.servers.<name>]` server,
    /// which the global runtime hosts once per daemon: it takes no `scope`.
    pub fn validate_top_level(&self, name: &str) -> Result<(), ConfigError> {
        self.validate(name)?;
        if self.scope.is_some() {
            return Err(ConfigError::McpServer {
                server: name.to_owned(),
                reason: "`scope` applies only to a runtime template's servers \
                         ([runtimes.<id>.mcp.servers.<name>]); a top-level [mcp] server \
                         already runs once, in the daemon's global runtime"
                    .into(),
            });
        }
        Ok(())
    }

    /// Whether the tool the server lists as `tool` is published: it matches
    /// `include_tools` (every tool when that is absent) and no
    /// `exclude_tools` pattern.
    #[must_use]
    pub fn publishes_tool(&self, tool: &str) -> bool {
        let matches = |pattern: &String| crate::simple_wildcard_match(pattern, tool);
        self.include_tools
            .as_ref()
            .is_none_or(|include| include.iter().any(matches))
            && !self.exclude_tools.iter().any(matches)
    }

    pub fn validate(&self, name: &str) -> Result<(), ConfigError> {
        if self.include_tools.as_ref().is_some_and(Vec::is_empty) {
            return Err(ConfigError::McpServer {
                server: name.to_owned(),
                reason: "`include_tools` is empty, so the server would publish no tool; \
                         remove it to publish every tool"
                    .into(),
            });
        }
        if self
            .include_tools
            .iter()
            .flatten()
            .chain(&self.exclude_tools)
            .any(String::is_empty)
        {
            return Err(ConfigError::McpServer {
                server: name.to_owned(),
                reason: "`include_tools` and `exclude_tools` patterns may not be empty".into(),
            });
        }
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

/// A model and an optional variant, as `default_model` and `small_model`
/// take them: a model key string (`"provider/model"`, its default variant),
/// or a table `{ model, variant }`. Whether the model is available is a
/// runtime matter; only the key's grammar is checked here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfiguredModel {
    pub model: ModelKey,
    /// `None` means the model's default variant; `base` its base variant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<ConfiguredVariantRef>,
}

impl<'de> Deserialize<'de> for ConfiguredModel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            model: ModelKey,
            #[serde(default)]
            variant: Option<ConfiguredVariantRef>,
        }

        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ConfiguredModel;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a model key or a table { model, variant }")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                value
                    .parse::<ModelKey>()
                    .map(|model| ConfiguredModel {
                        model,
                        variant: None,
                    })
                    .map_err(E::custom)
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let table = Table::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(ConfiguredModel {
                    model: table.model,
                    variant: table.variant,
                })
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// The decoded settings tables of `config.toml`: one field per settings table
/// (for example `[messaging]` → [`EngineConfig::messaging`]). `[mcp]` and
/// `[plugins]` decode separately onto [`crate::LoadedConfiguration`]. Exposed
/// on [`crate::LoadedConfiguration::runtime`].
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    /// `default_model`: the model and variant agent documents reach as
    /// `${default_model}`.
    #[serde(default)]
    pub default_model: Option<ConfiguredModel>,
    /// `small_model`: the model and variant agent documents reach as
    /// `${small_model}`.
    #[serde(default)]
    pub small_model: Option<ConfiguredModel>,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub tool_output: ToolOutputConfig,
    #[serde(default)]
    pub subagent_output: SubagentOutputConfig,
    #[serde(default)]
    pub agent_md: AgentMdConfig,
    #[serde(default)]
    pub loop_warning: LoopWarningConfig,
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
    pub messaging: MessagingConfig,
    #[serde(default)]
    pub producers: ProducersConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub cron: CronConfig,
    #[serde(default)]
    pub acp: AcpConfig,
    /// `[skills]`: the official skills.
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(skip)]
    pub pricing: PricingConfig,
    #[serde(default, deserialize_with = "deserialize_headers")]
    pub headers: BTreeMap<HeaderName, SafeStaticHeaderValue>,
    #[serde(default)]
    pub providers: BTreeMap<ProviderId, ProviderDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRuntimeLayer {
    pub(crate) default_model: Option<ConfiguredModel>,
    pub(crate) small_model: Option<ConfiguredModel>,
    pub(crate) server: Option<ServerConfig>,
    pub(crate) tool_output: Option<ToolOutputConfig>,
    pub(crate) subagent_output: Option<SubagentOutputConfig>,
    pub(crate) agent_md: Option<AgentMdConfig>,
    pub(crate) loop_warning: Option<LoopWarningConfig>,
    pub(crate) approval: Option<ApprovalConfig>,
    pub(crate) model_retry: Option<ModelRetryConfig>,
    pub(crate) context_compaction: Option<ContextCompactionConfig>,
    pub(crate) session_title: Option<SessionTitleConfig>,
    pub(crate) delegation: Option<DelegationConfig>,
    pub(crate) messaging: Option<MessagingConfig>,
    pub(crate) producers: Option<ProducersConfig>,
    pub(crate) memory: Option<MemoryConfig>,
    pub(crate) cron: Option<CronConfig>,
    pub(crate) acp: Option<AcpConfig>,
    pub(crate) skills: Option<SkillsConfig>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_headers")]
    pub(crate) headers: Option<BTreeMap<HeaderName, SafeStaticHeaderValue>>,
    pub(crate) mcp: Option<McpConfig>,
    pub(crate) plugins: Option<PluginsConfig>,
    pub(crate) runtimes: Option<BTreeMap<String, crate::runtime_templates::RawRuntimeTemplate>>,
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

pub use cookie_agent_models::authoring::ModelPricing;

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

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// The daemon's bearer token: 43 base64url characters. Clients and
    /// runtimes on this machine read it from here; the daemon writes a fresh
    /// one here when there is none.
    #[serde(default)]
    pub token: Option<String>,
    /// The daemon clients and runtimes on this machine connect to: a `ws://`
    /// or `wss://` URL ending in `/ws`. The daemon itself ignores it.
    #[serde(default)]
    pub url: Option<String>,
    /// How long a root session's tree sits unused (no event, no live tail,
    /// no pending work) before the daemon pages it out of memory. Using it
    /// again reloads it from disk.
    #[serde(
        default = "default_session_idle_eviction_after",
        deserialize_with = "deserialize_duration"
    )]
    pub session_idle_eviction_after: Duration,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            token: None,
            url: None,
            session_idle_eviction_after: default_session_idle_eviction_after(),
        }
    }
}
const fn default_session_idle_eviction_after() -> Duration {
    Duration::from_secs(30 * 60)
}

impl ServerConfig {
    /// The daemon clients and runtimes connect to: `url`, or the daemon
    /// this configuration describes, `ws://<host>:<port>/ws`.
    #[must_use]
    pub fn client_url(&self) -> String {
        self.url.clone().unwrap_or_else(|| self.local_url())
    }

    /// The daemon this configuration describes on this machine,
    /// `ws://<host>:<port>/ws`, whatever `url` names.
    #[must_use]
    pub fn local_url(&self) -> String {
        format!("ws://{}:{}/ws", self.host, self.port)
    }
}

/// Whether `url` names a daemon's client endpoint: `ws://` or `wss://`, a
/// host, the path `/ws`, and no credentials, query, or fragment.
#[must_use]
pub fn valid_daemon_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|url| {
        matches!(url.scheme(), "ws" | "wss")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/ws"
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("url", &self.url)
            .field(
                "session_idle_eviction_after",
                &self.session_idle_eviction_after,
            )
            .finish()
    }
}

/// Whether `token` is a daemon bearer token: 43 base64url characters, the
/// encoding of 32 random bytes.
#[must_use]
pub fn valid_daemon_token(token: &str) -> bool {
    token.len() == 43
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
fn default_host() -> String {
    DEFAULT_HOST.into()
}
const fn default_port() -> u16 {
    DEFAULT_PORT
}

/// Inline previews of tool output streams. Output within both limits is
/// shown whole; longer output shows its first and last lines (see
/// [`ToolOutputConfig::kept_lines`]), splitting `max_bytes` between them in
/// the same ratio.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputConfig {
    #[serde(default = "default_output_lines")]
    pub max_lines: usize,
    #[serde(default = "default_output_bytes")]
    pub max_bytes: usize,
    #[serde(default)]
    pub head_lines: Option<usize>,
    #[serde(default)]
    pub tail_lines: Option<usize>,
}

/// Inline previews of a subagent's final report, truncated like tool output.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentOutputConfig {
    #[serde(default = "default_subagent_output_lines")]
    pub max_lines: usize,
    #[serde(default = "default_subagent_output_bytes")]
    pub max_bytes: usize,
    #[serde(default)]
    pub head_lines: Option<usize>,
    #[serde(default)]
    pub tail_lines: Option<usize>,
}

/// Lines a truncated preview keeps from its start and its end. An omitted
/// count is `default`, capped to the room `max_lines` leaves it: the rest
/// after the other count, or its half (the start's rounded up) when both are
/// omitted.
fn kept_lines(
    max_lines: usize,
    (head, tail): (Option<usize>, Option<usize>),
    default: usize,
) -> (usize, usize) {
    match (head, tail) {
        (Some(head), Some(tail)) => (head, tail),
        (Some(head), None) => (head, default.min(max_lines.saturating_sub(head))),
        (None, Some(tail)) => (default.min(max_lines.saturating_sub(tail)), tail),
        (None, None) => (
            default.min(max_lines - max_lines / 2),
            default.min(max_lines / 2),
        ),
    }
}

impl ToolOutputConfig {
    /// Lines kept from each end of a truncated stream when not configured.
    pub const DEFAULT_KEPT_LINES: usize = 100;

    /// Lines a truncated stream keeps from its start and its end.
    #[must_use]
    pub fn kept_lines(&self) -> (usize, usize) {
        kept_lines(
            self.max_lines,
            (self.head_lines, self.tail_lines),
            Self::DEFAULT_KEPT_LINES,
        )
    }
}

impl SubagentOutputConfig {
    /// Lines kept from each end of a truncated report when not configured.
    pub const DEFAULT_KEPT_LINES: usize = 20;

    /// Lines a truncated report keeps from its start and its end.
    #[must_use]
    pub fn kept_lines(&self) -> (usize, usize) {
        kept_lines(
            self.max_lines,
            (self.head_lines, self.tail_lines),
            Self::DEFAULT_KEPT_LINES,
        )
    }
}

/// The largest `subagent_output.max_bytes`: a stored subagent preview, with
/// its omission marker, must fit the 64 KiB event bound.
pub const SUBAGENT_OUTPUT_MAX_BYTES: usize = 60 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentMdConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
}
impl Default for AgentMdConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LoopWarningConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
}
impl Default for LoopWarningConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}
impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            max_lines: default_output_lines(),
            max_bytes: default_output_bytes(),
            head_lines: None,
            tail_lines: None,
        }
    }
}
impl Default for SubagentOutputConfig {
    fn default() -> Self {
        Self {
            max_lines: default_subagent_output_lines(),
            max_bytes: default_subagent_output_bytes(),
            head_lines: None,
            tail_lines: None,
        }
    }
}
const fn default_output_lines() -> usize {
    500
}
const fn default_output_bytes() -> usize {
    12_800
}
const fn default_subagent_output_lines() -> usize {
    100
}
const fn default_subagent_output_bytes() -> usize {
    10 * 1024
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalConfig {
    #[serde(default = "default_approval_timeout")]
    pub timeout_ms: u64,
    /// The approval reviewer's prompt-cache lifetime, mapped onto its
    /// model's provider (approval reviewer spec section 5).
    #[serde(default)]
    pub cache_ttl: ApprovalCacheTtl,
}

/// `[approval] cache_ttl`: how long the approval reviewer's conversation
/// stays in its provider's prompt cache. A TTL the provider does not offer
/// becomes the nearest one it does; `provider` is the reviewer model's
/// provider's own cache configuration, and `off` writes no cache.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum ApprovalCacheTtl {
    #[default]
    #[serde(rename = "1h")]
    OneHour,
    #[serde(rename = "30m")]
    ThirtyMinutes,
    #[serde(rename = "12h")]
    TwelveHours,
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "provider")]
    Provider,
    #[serde(rename = "off")]
    Off,
}

impl ApprovalCacheTtl {
    /// The lifetime in minutes, or `None` for `provider` and `off`.
    #[must_use]
    pub const fn minutes(self) -> Option<u32> {
        match self {
            Self::FiveMinutes => Some(5),
            Self::ThirtyMinutes => Some(30),
            Self::OneHour => Some(60),
            Self::TwelveHours => Some(720),
            Self::Provider | Self::Off => None,
        }
    }

    /// The value as written in `config.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OneHour => "1h",
            Self::ThirtyMinutes => "30m",
            Self::TwelveHours => "12h",
            Self::FiveMinutes => "5m",
            Self::Provider => "provider",
            Self::Off => "off",
        }
    }
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

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessagingConfig {
    #[serde(default = "default_messaging_enabled")]
    pub enabled: bool,
    /// Deepest accepted agent-mail chain depth. `0` (or any negative value)
    /// disables the guard; there is no load-time validation beyond the type.
    #[serde(default)]
    pub max_hops: i32,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// Pending agent mail one recipient may hold at once, from any sender.
    #[serde(default = "default_max_pending_per_session")]
    pub max_pending_per_session: usize,
    /// Unacknowledged mail one directed sender→recipient pair may hold at
    /// once. `0` disables the guard.
    #[serde(default = "default_max_inflight_per_pair")]
    pub max_inflight_per_pair: u32,
}
impl Default for MessagingConfig {
    fn default() -> Self {
        Self {
            enabled: default_messaging_enabled(),
            max_hops: 0,
            max_body_bytes: default_max_body_bytes(),
            max_pending_per_session: default_max_pending_per_session(),
            max_inflight_per_pair: default_max_inflight_per_pair(),
        }
    }
}
/// `[producers]`: bounds on what external producers (plugins and runtimes)
/// may send.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducersConfig {
    /// Largest body an external producer may send, in UTF-8 bytes.
    #[serde(default = "default_producer_max_body_bytes")]
    pub max_body_bytes: usize,
}
impl Default for ProducersConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: default_producer_max_body_bytes(),
        }
    }
}
const fn default_producer_max_body_bytes() -> usize {
    64 * 1024
}
/// The ceiling of `[producers] max_body_bytes`: the 4 MiB frame a plugin
/// message travels in.
pub const PRODUCER_MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// `[cron]`: the daemon's cron jobs (`docs/site/specs/cron.md` section 10).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CronConfig {
    /// Whether the daemon fires jobs; `false` keeps the jobs, the requests,
    /// and the tools working and records each due fire as skipped.
    #[serde(default = "default_cron_enabled")]
    pub enabled: bool,
    /// Failures in a row that disable a job, unless the job sets its own.
    #[serde(default = "default_cron_max_consecutive_failures")]
    pub max_consecutive_failures: u32,
    /// Fires each job's history keeps, oldest dropped first.
    #[serde(default = "default_cron_history")]
    pub history: usize,
}
impl Default for CronConfig {
    fn default() -> Self {
        Self {
            enabled: default_cron_enabled(),
            max_consecutive_failures: default_cron_max_consecutive_failures(),
            history: default_cron_history(),
        }
    }
}
impl CronConfig {
    /// The most fires a job's history may keep.
    pub const MAX_HISTORY: usize = 1000;

    fn invalid_reason(&self) -> Option<&'static str> {
        if self.max_consecutive_failures == 0 {
            return Some("cron.max_consecutive_failures must be positive");
        }
        if self.history == 0 || self.history > Self::MAX_HISTORY {
            return Some("cron.history must be 1..1000");
        }
        None
    }
}
/// `[skills]`: whether the daemon keeps the official skills up to date and
/// offers them (`docs/site/specs/default-models-and-onboarding.md` section
/// 13).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SkillsConfig {
    /// Refresh the official skills at start and daily, and offer them below
    /// the user's own; `false` stops refreshing and leaves them unused.
    #[serde(default = "yes")]
    pub official: bool,
}
impl Default for SkillsConfig {
    fn default() -> Self {
        Self { official: true }
    }
}

/// `[acp]`: how the daemon runs ACP agents
/// (`docs/site/specs/acp-agents.md` section 7).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AcpConfig {
    /// How long an agent's process stays up with no live session before it
    /// is stopped.
    #[serde(default = "default_acp_idle_shutdown_ms")]
    pub idle_shutdown_ms: u64,
}
impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            idle_shutdown_ms: default_acp_idle_shutdown_ms(),
        }
    }
}
impl AcpConfig {
    /// [`Self::idle_shutdown_ms`] as a duration.
    #[must_use]
    pub const fn idle_shutdown(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.idle_shutdown_ms)
    }

    fn invalid_reason(&self) -> Option<&'static str> {
        (self.idle_shutdown_ms == 0).then_some("acp.idle_shutdown_ms must be positive")
    }
}
const fn default_acp_idle_shutdown_ms() -> u64 {
    5 * 60 * 1000
}
const fn default_cron_enabled() -> bool {
    true
}
const fn default_cron_max_consecutive_failures() -> u32 {
    3
}
const fn default_cron_history() -> usize {
    20
}

/// `[memory]`: size limits of memory documents and of the memory snapshot
/// (`docs/site/specs/memory.md` sections 11 and 16.9.7).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    /// Largest `user://` category, frontmatter included, in bytes.
    #[serde(default = "default_memory_document_bytes")]
    pub user_max_bytes: usize,
    /// Largest `agent://` file, frontmatter included, in bytes.
    #[serde(default = "default_memory_document_bytes")]
    pub agent_max_bytes: usize,
    /// Largest `machine://` memory, frontmatter included, in bytes.
    #[serde(default = "default_memory_document_bytes")]
    pub machine_max_bytes: usize,
    /// Largest `project://` memory other than an overview, frontmatter
    /// included, in bytes.
    #[serde(default = "default_memory_document_bytes")]
    pub project_max_bytes: usize,
    /// Largest project overview (`project://<project>/overview`),
    /// frontmatter included, in bytes.
    #[serde(default = "default_memory_document_bytes")]
    pub overview_max_bytes: usize,
    /// Room for the user categories in a snapshot, in bytes of rendered
    /// text.
    #[serde(default = "default_memory_snapshot_user_bytes")]
    pub snapshot_user_bytes: usize,
    /// Room for the machine memory index in a snapshot, in bytes of
    /// rendered text, summed over all machines.
    #[serde(default = "default_memory_snapshot_index_bytes")]
    pub snapshot_index_bytes: usize,
}
impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            user_max_bytes: default_memory_document_bytes(),
            agent_max_bytes: default_memory_document_bytes(),
            machine_max_bytes: default_memory_document_bytes(),
            project_max_bytes: default_memory_document_bytes(),
            overview_max_bytes: default_memory_document_bytes(),
            snapshot_user_bytes: default_memory_snapshot_user_bytes(),
            snapshot_index_bytes: default_memory_snapshot_index_bytes(),
        }
    }
}
impl MemoryConfig {
    /// The bound a whole memory snapshot event stays under.
    pub const EVENT_MAX_BYTES: usize = 64 * 1024;
    /// What a snapshot's fixed text, headings, element tags, and escaping
    /// may add to the snapshot's limits. The limits are validated so that
    /// `agent_max_bytes + snapshot_user_bytes + snapshot_index_bytes +
    /// RENDER_OVERHEAD_BYTES <= EVENT_MAX_BYTES`.
    pub const RENDER_OVERHEAD_BYTES: usize = 8 * 1024;
    /// The largest sum of the snapshot's limits, and the largest any one
    /// document limit may be.
    pub const MAX_LIMITS_SUM: usize = Self::EVENT_MAX_BYTES - Self::RENDER_OVERHEAD_BYTES;

    fn invalid_reason(&self) -> Option<&'static str> {
        let documents = [
            self.user_max_bytes,
            self.agent_max_bytes,
            self.machine_max_bytes,
            self.project_max_bytes,
            self.overview_max_bytes,
        ];
        if documents
            .iter()
            .chain([&self.snapshot_user_bytes, &self.snapshot_index_bytes])
            .any(|limit| *limit == 0)
        {
            return Some("every memory limit must be positive");
        }
        if documents.iter().any(|limit| *limit > Self::MAX_LIMITS_SUM) {
            return Some("a memory document limit must not exceed 57344");
        }
        let sum = self
            .agent_max_bytes
            .checked_add(self.snapshot_user_bytes)
            .and_then(|sum| sum.checked_add(self.snapshot_index_bytes));
        if sum.is_none_or(|sum| sum > Self::MAX_LIMITS_SUM) {
            return Some(
                "memory.agent_max_bytes plus snapshot_user_bytes plus snapshot_index_bytes must not exceed 57344",
            );
        }
        None
    }
}
const fn default_memory_document_bytes() -> usize {
    2048
}
const fn default_memory_snapshot_user_bytes() -> usize {
    12 * 1024
}
const fn default_memory_snapshot_index_bytes() -> usize {
    4096
}

const fn default_messaging_enabled() -> bool {
    true
}
const fn default_max_body_bytes() -> usize {
    32_768
}
const fn default_max_pending_per_session() -> usize {
    32
}
const fn default_max_inflight_per_pair() -> u32 {
    4
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
            cache_ttl: ApprovalCacheTtl::default(),
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

pub(crate) fn apply_settings(runtime: &mut EngineConfig, layer: &RawRuntimeLayer) {
    if let Some(value) = &layer.default_model {
        runtime.default_model = Some(value.clone());
    }
    if let Some(value) = &layer.small_model {
        runtime.small_model = Some(value.clone());
    }
    if let Some(value) = &layer.server {
        runtime.server = value.clone();
    }
    if let Some(value) = &layer.tool_output {
        runtime.tool_output = value.clone();
    }
    if let Some(value) = &layer.subagent_output {
        runtime.subagent_output = value.clone();
    }
    if let Some(value) = &layer.agent_md {
        runtime.agent_md = value.clone();
    }
    if let Some(value) = &layer.loop_warning {
        runtime.loop_warning = value.clone();
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
    if let Some(value) = &layer.messaging {
        runtime.messaging = value.clone();
    }
    if let Some(value) = &layer.producers {
        runtime.producers = value.clone();
    }
    if let Some(value) = &layer.memory {
        runtime.memory = value.clone();
    }
    if let Some(value) = &layer.cron {
        runtime.cron = value.clone();
    }
    if let Some(value) = &layer.acp {
        runtime.acp = value.clone();
    }
    if let Some(value) = &layer.skills {
        runtime.skills = value.clone();
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

/// Whether the authored counts alone exceed `max_lines`.
fn kept_lines_overflow(max_lines: usize, head: Option<usize>, tail: Option<usize>) -> bool {
    head.unwrap_or(0).saturating_add(tail.unwrap_or(0)) > max_lines
}

fn kept_lines_empty((head, tail): (usize, usize)) -> bool {
    head + tail == 0
}

pub(crate) fn validate_runtime(runtime: &EngineConfig) -> Result<(), ConfigError> {
    validate_header_ownership(&runtime.headers, "global").map_err(ConfigError::HeaderOwnership)?;
    validate_header_limits(&runtime.headers).map_err(|_| {
        ConfigError::InvalidRuntime("headers exceed the header count or size limits")
    })?;
    for (invalid, reason) in [
        (
            runtime.server.host.is_empty() || runtime.server.host.len() > 255,
            "server.host must contain 1..255 bytes",
        ),
        (
            runtime
                .server
                .token
                .as_deref()
                .is_some_and(|token| !valid_daemon_token(token)),
            "server.token must be 43 base64url characters",
        ),
        (
            runtime
                .server
                .url
                .as_deref()
                .is_some_and(|url| !valid_daemon_url(url)),
            "server.url must be a ws:// or wss:// URL whose path is /ws",
        ),
        (
            runtime.tool_output.max_lines == 0,
            "tool_output.max_lines must be positive",
        ),
        (
            runtime.tool_output.max_bytes == 0,
            "tool_output.max_bytes must be positive",
        ),
        (
            kept_lines_overflow(
                runtime.tool_output.max_lines,
                runtime.tool_output.head_lines,
                runtime.tool_output.tail_lines,
            ),
            "tool_output.head_lines plus tail_lines must not exceed max_lines",
        ),
        (
            kept_lines_empty(runtime.tool_output.kept_lines()),
            "tool_output.head_lines and tail_lines must keep at least one line",
        ),
        (
            runtime.subagent_output.max_lines == 0,
            "subagent_output.max_lines must be positive",
        ),
        (
            runtime.producers.max_body_bytes == 0
                || runtime.producers.max_body_bytes > PRODUCER_MAX_BODY_BYTES,
            "producers.max_body_bytes must be 1..=4194304",
        ),
        (
            runtime.subagent_output.max_bytes == 0
                || runtime.subagent_output.max_bytes > SUBAGENT_OUTPUT_MAX_BYTES,
            "subagent_output.max_bytes must be 1..=61440",
        ),
        (
            kept_lines_overflow(
                runtime.subagent_output.max_lines,
                runtime.subagent_output.head_lines,
                runtime.subagent_output.tail_lines,
            ),
            "subagent_output.head_lines plus tail_lines must not exceed max_lines",
        ),
        (
            kept_lines_empty(runtime.subagent_output.kept_lines()),
            "subagent_output.head_lines and tail_lines must keep at least one line",
        ),
        (
            runtime.approval.timeout_ms == 0,
            "approval.timeout_ms must be positive",
        ),
        (
            runtime.model_retry.backoff_ceiling_ms == 0,
            "model_retry.backoff_ceiling_ms must be positive",
        ),
        (
            runtime.delegation.max_depth == 0,
            "delegation.max_depth must be positive",
        ),
        (
            runtime.delegation.max_concurrency == Some(0),
            "delegation.max_concurrency must be positive when set",
        ),
    ] {
        if invalid {
            return Err(ConfigError::InvalidRuntime(reason));
        }
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
        return Err(ConfigError::InvalidRuntime(
            "context_compaction.trigger requires percent 1..99 or positive buffer_tokens; max_summary_bytes must be 1..2097152",
        ));
    }
    if let Some(reason) = runtime.memory.invalid_reason() {
        return Err(ConfigError::InvalidRuntime(reason));
    }
    if let Some(reason) = runtime.cron.invalid_reason() {
        return Err(ConfigError::InvalidRuntime(reason));
    }
    if let Some(reason) = runtime.acp.invalid_reason() {
        return Err(ConfigError::InvalidRuntime(reason));
    }
    if runtime.session_title.max_chars == 0 || runtime.session_title.max_input_messages == 0 {
        return Err(ConfigError::InvalidRuntime(
            "session_title.max_chars and max_input_messages must be positive",
        ));
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
