//! Layered TOML configuration and policy snapshots.
//!
//! Permission-rule arrays are the one exception to normal Figment merge
//! semantics. [`load_layered`] merges the TOML values itself, concatenating
//! `permissions.rules` arrays while replacing all other arrays, before using
//! Figment and Serde for typed extraction.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fmt, fs, io,
    path::{Path, PathBuf},
};

use cookie_agent_models::{ConfiguredModel, FrozenModelBinding, ModelSet, build_model_set};
use figment::{
    Figment,
    error::{Actual as FigmentActual, Error as FigmentError, Kind as FigmentErrorKind},
    providers::Serialized,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml::Value;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 7419;
const DEFAULT_DELEGATE_RESULT_BYTES: usize = 32 * 1024;
const DEFAULT_TOOL_OUTPUT_MAX_LINES: usize = 2_000;
const DEFAULT_TOOL_OUTPUT_MAX_BYTES: usize = 50 * 1024;
const CONFIG_SCHEMA_VERSION: u32 = 5;
const MAX_CHECKPOINT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "current_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub models: BTreeMap<String, ConfiguredModel>,
    /// Ordered rules applied to every profile.
    #[serde(default)]
    pub permissions: PermissionConfig,
    #[serde(default)]
    pub tool_output: ToolOutputConfig,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentProfile>,
    #[serde(default)]
    pub internal_agents: InternalAgentsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: current_schema_version(),
            server: ServerConfig::default(),
            models: BTreeMap::new(),
            permissions: PermissionConfig::default(),
            tool_output: ToolOutputConfig::default(),
            agents: BTreeMap::new(),
            internal_agents: InternalAgentsConfig::default(),
        }
    }
}

const fn current_schema_version() -> u32 {
    CONFIG_SCHEMA_VERSION
}

/// Explicit engine-internal agent profiles and bounded policies.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InternalAgentsConfig {
    #[serde(default)]
    pub approval: InternalModelAgentConfig,
    #[serde(default)]
    pub context_compaction: ContextCompactionConfig,
    #[serde(default)]
    pub session_title: SessionTitleConfig,
}

/// Model chain and hard execution budgets for one internal agent.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InternalModelAgentConfig {
    /// Exact static aliases or exact `provider/model` catalog aliases.
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "default_internal_input_tokens")]
    pub max_input_tokens: u64,
    #[serde(default = "default_internal_output_tokens")]
    pub max_output_tokens: u64,
    #[serde(default = "default_internal_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for InternalModelAgentConfig {
    fn default() -> Self {
        Self {
            models: Vec::new(),
            max_input_tokens: default_internal_input_tokens(),
            max_output_tokens: default_internal_output_tokens(),
            timeout_ms: default_internal_timeout_ms(),
        }
    }
}

const fn default_internal_input_tokens() -> u64 {
    16_384
}

const fn default_internal_output_tokens() -> u64 {
    2_048
}

const fn default_internal_timeout_ms() -> u64 {
    30_000
}

/// Context thresholds and bounded summary/native persistence behavior.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextCompactionConfig {
    #[serde(default)]
    pub profile: InternalModelAgentConfig,
    #[serde(default = "default_soft_threshold_percent")]
    pub soft_threshold_percent: u8,
    #[serde(default = "default_hard_threshold_percent")]
    pub hard_threshold_percent: u8,
    #[serde(default = "default_target_percent")]
    pub target_percent: u8,
    #[serde(default = "default_summary_bytes")]
    pub max_summary_bytes: usize,
    #[serde(default = "default_native_context_bytes")]
    pub max_native_context_bytes: usize,
    #[serde(default)]
    pub persistence: CompactionPersistencePolicy,
}

impl Default for ContextCompactionConfig {
    fn default() -> Self {
        Self {
            profile: InternalModelAgentConfig::default(),
            soft_threshold_percent: default_soft_threshold_percent(),
            hard_threshold_percent: default_hard_threshold_percent(),
            target_percent: default_target_percent(),
            max_summary_bytes: default_summary_bytes(),
            max_native_context_bytes: default_native_context_bytes(),
            persistence: CompactionPersistencePolicy::default(),
        }
    }
}

const fn default_soft_threshold_percent() -> u8 {
    70
}
const fn default_hard_threshold_percent() -> u8 {
    85
}
const fn default_target_percent() -> u8 {
    50
}
const fn default_summary_bytes() -> usize {
    256 * 1024
}
const fn default_native_context_bytes() -> usize {
    MAX_CHECKPOINT_BYTES
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionPersistencePolicy {
    SummaryOnly,
    #[default]
    NativePreferred,
    NativeOnly,
}

/// Session-title model profile and deterministic fallback policy.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTitleConfig {
    #[serde(default)]
    pub profile: InternalModelAgentConfig,
    #[serde(default)]
    pub policy: SessionTitlePolicy,
}

impl Default for SessionTitleConfig {
    fn default() -> Self {
        Self {
            profile: InternalModelAgentConfig {
                max_input_tokens: 4_096,
                max_output_tokens: 128,
                timeout_ms: 10_000,
                models: Vec::new(),
            },
            policy: SessionTitlePolicy::default(),
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTitlePolicy {
    #[serde(default = "default_title_max_chars")]
    pub max_chars: usize,
    #[serde(default = "default_title_input_messages")]
    pub max_input_messages: usize,
    #[serde(default = "default_enabled")]
    pub generate_on_first_turn: bool,
    #[serde(default = "default_enabled")]
    pub fallback_to_input_excerpt: bool,
}

impl Default for SessionTitlePolicy {
    fn default() -> Self {
        Self {
            max_chars: default_title_max_chars(),
            max_input_messages: default_title_input_messages(),
            generate_on_first_turn: true,
            fallback_to_input_excerpt: true,
        }
    }
}

const fn default_title_max_chars() -> usize {
    80
}
const fn default_title_input_messages() -> usize {
    4
}

/// Model-visible tool-output preview limits. Full output is retained separately.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputConfig {
    #[serde(default = "default_tool_output_max_lines")]
    pub max_lines: usize,
    #[serde(default = "default_tool_output_max_bytes")]
    pub max_bytes: usize,
}

impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            max_lines: default_tool_output_max_lines(),
            max_bytes: default_tool_output_max_bytes(),
        }
    }
}

const fn default_tool_output_max_lines() -> usize {
    DEFAULT_TOOL_OUTPUT_MAX_LINES
}

const fn default_tool_output_max_bytes() -> usize {
    DEFAULT_TOOL_OUTPUT_MAX_BYTES
}

#[derive(Clone, Deserialize, Serialize)]
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
    DEFAULT_HOST.to_owned()
}

const fn default_port() -> u16 {
    DEFAULT_PORT
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    #[serde(rename = "type", default)]
    pub r#type: AgentType,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub delegation: DelegationConfig,
    #[serde(default)]
    pub permissions: AgentPermissionConfig,
}

impl Default for AgentProfile {
    fn default() -> Self {
        Self {
            r#type: AgentType::All,
            enabled: default_enabled(),
            models: Vec::new(),
            tools: Vec::new(),
            delegation: DelegationConfig::default(),
            permissions: AgentPermissionConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentType {
    #[default]
    All,
    Primary,
    Subagent,
    Internal,
}

const fn default_enabled() -> bool {
    true
}

#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allowed_profiles: Vec<String>,
    pub limit: Option<u32>,
}

/// Ordered permission rules applied globally.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionConfig {
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

/// A profile's ordered rules are appended after global rules.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPermissionConfig {
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRule {
    pub id: String,
    pub action: String,
    pub resource: String,
    pub effect: String,
    /// Internal merge provenance, persisted in policy snapshots for decision traces.
    #[serde(rename = "__source", default)]
    pub source: RuleSource,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleSource {
    #[default]
    Builtin,
    User,
    Workspace,
    Profile,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DepthLimit {
    Finite(u32),
    Unlimited,
}

impl DepthLimit {
    #[must_use]
    pub const fn allows_delegation(self) -> bool {
        !matches!(self, Self::Finite(0))
    }

    /// Computes a child's frozen limit from its configured limit and parent limit.
    #[must_use]
    pub fn for_child(configured: Option<u32>, parent: Self) -> Self {
        match (configured, parent) {
            (Some(child), Self::Finite(parent)) => {
                Self::Finite(child.min(parent.saturating_sub(1)))
            }
            (Some(child), Self::Unlimited) => Self::Finite(child),
            (None, Self::Finite(parent)) => Self::Finite(parent.saturating_sub(1)),
            (None, Self::Unlimited) => Self::Unlimited,
        }
    }

    #[must_use]
    pub const fn from_config(limit: Option<u32>) -> Self {
        match limit {
            Some(limit) => Self::Finite(limit),
            None => Self::Unlimited,
        }
    }
}

/// Serializable configured policy retained in a session event log.
#[derive(Clone, Deserialize, PartialEq, Serialize)]
pub struct PolicySnapshot {
    pub profile: ProfileSnapshot,
    pub models: Vec<FrozenModelBinding>,
    pub tools: BTreeSet<String>,
    pub permissions: ResolvedPermissions,
    pub delegation: DelegationPolicy,
    pub result_limits: ResultLimits,
}

/// Parent policy inputs needed to materialize a child profile.
pub struct ChildPolicyParent {
    depth_limit: DepthLimit,
    models: Option<Vec<FrozenModelBinding>>,
}

impl From<DepthLimit> for ChildPolicyParent {
    fn from(depth_limit: DepthLimit) -> Self {
        Self {
            depth_limit,
            models: None,
        }
    }
}

impl From<&PolicySnapshot> for ChildPolicyParent {
    fn from(policy: &PolicySnapshot) -> Self {
        Self {
            depth_limit: policy.delegation.depth_limit,
            models: Some(policy.models.clone()),
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProfileSnapshot {
    pub name: String,
    #[serde(rename = "type")]
    pub r#type: AgentType,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedPermissions {
    pub rules: Vec<PermissionRule>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct DelegationPolicy {
    pub enabled: bool,
    pub allowed_profiles: BTreeSet<String>,
    pub depth_limit: DepthLimit,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResultLimits {
    pub delegate_result_bytes: usize,
    pub tool_output_max_lines: usize,
    pub tool_output_max_bytes: usize,
}

impl Default for ResultLimits {
    fn default() -> Self {
        Self {
            delegate_result_bytes: DEFAULT_DELEGATE_RESULT_BYTES,
            tool_output_max_lines: DEFAULT_TOOL_OUTPUT_MAX_LINES,
            tool_output_max_bytes: DEFAULT_TOOL_OUTPUT_MAX_BYTES,
        }
    }
}

macro_rules! redacted_debug {
    ($($type:ty),+ $(,)?) => {
        $(
            impl fmt::Debug for $type {
                fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.debug_struct(stringify!($type)).finish_non_exhaustive()
                }
            }
        )+
    };
}

// Interpolated values are credentials or endpoints, so avoid exposing them
// through Debug output.
redacted_debug!(
    Config,
    ServerConfig,
    AgentProfile,
    DelegationConfig,
    PermissionConfig,
    AgentPermissionConfig,
    PermissionRule,
    PolicySnapshot,
    ProfileSnapshot,
    ResolvedPermissions,
    DelegationPolicy,
    ResultLimits,
    ToolOutputConfig,
    InternalAgentsConfig,
    InternalModelAgentConfig,
    ContextCompactionConfig,
    SessionTitleConfig,
    SessionTitlePolicy,
);

#[derive(Error)]
pub enum ConfigError {
    #[error("unsupported config schema version {found}; expected 5")]
    SchemaVersion { found: u32 },
    #[error("could not read {layer:?} configuration {path}: {source}")]
    Read {
        layer: RuleSource,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not parse {layer:?} configuration {path}: {source}")]
    Toml {
        layer: RuleSource,
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error(
        "could not resolve environment variable `{variable}` at `{key}` in {layer:?} configuration {path}: {reason}"
    )]
    EnvironmentInterpolation {
        layer: RuleSource,
        path: PathBuf,
        key: String,
        variable: String,
        reason: &'static str,
    },
    #[error("configuration extraction failed: {detail}")]
    Figment { detail: String },
    #[error("merged configuration extraction failed: {detail}")]
    Extract { detail: String },
    #[error("could not construct built-in configuration: {0}")]
    Serialize(#[source] toml::ser::Error),
    #[error("profile `{profile}` has an empty model chain")]
    EmptyModels { profile: String },
    #[error("profile `{profile}` references unknown model alias `{alias}`")]
    UnknownModelAlias { profile: String, alias: String },
    #[error("installed model set does not match the loaded model configuration")]
    InstalledModelSetMismatch,
    #[error("could not construct configured models: {0}")]
    Models(#[from] cookie_agent_models::ModelBuildError),
    #[error("profile `{profile}` allows unknown profile")]
    UnknownAllowedProfile { profile: String },
    #[error("profile `{profile}` allows a primary-only profile")]
    PrimaryAllowedProfile { profile: String },
    #[error("profile `{profile}` allows internal profile")]
    InternalAllowedProfile { profile: String },
    #[error("internal agents cannot be disabled: `{profile}`")]
    InternalAgentDisabled { profile: String },
    #[error("profile `{profile}` enables delegation with no allowed profiles")]
    EmptyAllowedProfiles { profile: String },
    #[error("invalid permission effect in {scope}")]
    InvalidEffect { scope: String },
    #[error("tool_output.{field} must be greater than zero")]
    InvalidToolOutputLimit { field: &'static str },
    #[error("unknown profile `{0}`")]
    UnknownProfile(String),
    #[error("profile `{0}` is disabled")]
    DisabledProfile(String),
    #[error("internal agent `{agent}` has an invalid {field} budget")]
    InvalidInternalBudget {
        agent: &'static str,
        field: &'static str,
    },
    #[error("context compaction thresholds must satisfy target < soft < hard <= 100")]
    InvalidCompactionThresholds,
    #[error("context compaction {field} exceeds the 2 MiB persistence ceiling")]
    CompactionPersistenceTooLarge { field: &'static str },
    #[error("session title policy requires positive max_chars and max_input_messages")]
    InvalidTitlePolicy,
}

impl fmt::Debug for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ConfigError")
            .field(&self.to_string())
            .finish()
    }
}

/// Builds the standard Figment stack, resolving TOML environment expressions.
///
/// Only model endpoints, static authentication values, and static header values are interpolated.
/// It is useful to callers that need Figment-native provenance but does not
/// perform the permission-rule append.
pub fn build_figment(
    user_config: Option<&Path>,
    workspace_config: Option<&Path>,
) -> Result<Figment, ConfigError> {
    let mut figment = Figment::from(Serialized::defaults(Config::default()));
    if let Some(value) = load_optional_file(user_config, RuleSource::User)? {
        figment = figment.merge(Serialized::defaults(value));
    }
    if let Some(value) = load_optional_file(workspace_config, RuleSource::Workspace)? {
        figment = figment.merge(Serialized::defaults(value));
    }
    Ok(figment)
}

/// Extracts and validates a pre-built Figment stack.
pub fn load_from(figment: Figment) -> Result<Config, ConfigError> {
    let config: Config = figment.extract().map_err(|error| ConfigError::Figment {
        detail: safe_figment_error(error),
    })?;
    config.validate()?;
    Ok(config)
}

/// Loads the default user configuration and a repository's workspace config.
pub fn load(workspace: &Path) -> Result<Config, ConfigError> {
    let user = user_config_path();
    let workspace_config = workspace.join(".cookie_agent/config.toml");
    load_layered(Some(&user), Some(&workspace_config))
}

/// Loads config with the documented layer order.
///
/// Missing user and workspace files are simply absent layers. TOML
/// interpolation is restricted to `models.*.endpoint`,
/// `models.*.auth` credential fields, and `models.*.headers.*`; all other
/// strings remain literal.
pub fn load_layered(
    user_config: Option<&Path>,
    workspace_config: Option<&Path>,
) -> Result<Config, ConfigError> {
    let mut merged = default_value()?;

    merge_optional_file(&mut merged, user_config, RuleSource::User)?;
    merge_optional_file(&mut merged, workspace_config, RuleSource::Workspace)?;

    let config: Config = Figment::from(Serialized::defaults(merged))
        .extract()
        .map_err(|error| ConfigError::Extract {
            detail: safe_figment_error(error),
        })?;
    config.validate()?;
    Ok(config)
}

fn safe_figment_error(error: FigmentError) -> String {
    error
        .into_iter()
        .map(|error| {
            let path = safe_error_path(&error);
            match error.kind {
                FigmentErrorKind::InvalidType(actual, expected) => format!(
                    "invalid type at `{path}`: found {}, expected {expected}",
                    safe_actual_type(&actual)
                ),
                FigmentErrorKind::InvalidValue(_, expected) => {
                    format!("invalid value at `{path}`: expected {expected}")
                }
                FigmentErrorKind::InvalidLength(_, expected) => {
                    format!("invalid length at `{path}`: expected {expected}")
                }
                FigmentErrorKind::UnknownVariant(_, expected) => format!(
                    "unknown variant at `{path}`: expected one of {}",
                    expected.join(", ")
                ),
                FigmentErrorKind::UnknownField(_, expected) => format!(
                    "unknown field at `{path}`: expected one of {}",
                    expected.join(", ")
                ),
                FigmentErrorKind::MissingField(_) => {
                    format!("missing field at `{path}`")
                }
                FigmentErrorKind::DuplicateField(_) => {
                    format!("duplicate field at `{path}`")
                }
                FigmentErrorKind::ISizeOutOfRange(_) => {
                    format!("signed integer out of range at `{path}`")
                }
                FigmentErrorKind::USizeOutOfRange(_) => {
                    format!("unsigned integer out of range at `{path}`")
                }
                FigmentErrorKind::Unsupported(actual) => {
                    format!("unsupported type {} at `{path}`", safe_actual_type(&actual))
                }
                FigmentErrorKind::UnsupportedKey(actual, expected) => format!(
                    "unsupported key type {} at `{path}`: expected {expected}",
                    safe_actual_type(&actual)
                ),
                FigmentErrorKind::Message(_) => {
                    format!("invalid configuration at `{path}`")
                }
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn safe_error_path(error: &FigmentError) -> String {
    let mut path = error.path.clone();
    let field = match &error.kind {
        FigmentErrorKind::UnknownField(field, _) => Some(field.as_str()),
        FigmentErrorKind::MissingField(field) => Some(field.as_ref()),
        FigmentErrorKind::DuplicateField(field) => Some(*field),
        _ => None,
    };
    if let Some(field) = field
        && path.last().is_none_or(|segment| segment != field)
    {
        path.push(field.to_owned());
    }
    if path.is_empty() {
        "root".to_owned()
    } else {
        path.join(".")
    }
}

const fn safe_actual_type(actual: &FigmentActual) -> &'static str {
    match actual {
        FigmentActual::Bool(_) => "boolean",
        FigmentActual::Unsigned(_) => "unsigned integer",
        FigmentActual::Signed(_) => "signed integer",
        FigmentActual::Float(_) => "floating-point number",
        FigmentActual::Char(_) => "character",
        FigmentActual::Str(_) => "string",
        FigmentActual::Bytes(_) => "bytes",
        FigmentActual::Unit => "unit",
        FigmentActual::Option => "option",
        FigmentActual::NewtypeStruct => "newtype struct",
        FigmentActual::Seq => "sequence",
        FigmentActual::Map => "map",
        FigmentActual::Enum => "enum",
        FigmentActual::UnitVariant => "unit variant",
        FigmentActual::NewtypeVariant => "newtype variant",
        FigmentActual::TupleVariant => "tuple variant",
        FigmentActual::StructVariant => "struct variant",
        FigmentActual::Other(_) => "other",
    }
}

/// Concatenates permission rules in already-established layer order.
#[must_use]
pub fn merge_permission_rules(layers: Vec<Vec<PermissionRule>>) -> Vec<PermissionRule> {
    layers.into_iter().flatten().collect()
}

impl Config {
    /// Constructs all configured aliases exactly once into an immutable model set.
    pub fn build_model_set(&self) -> Result<ModelSet, ConfigError> {
        build_model_set(&self.models).map_err(ConfigError::Models)
    }

    /// Checks cross-reference and policy constraints after typed extraction.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::SchemaVersion {
                found: self.schema_version,
            });
        }
        validate_permission_config(&self.permissions, "global")?;
        if self.tool_output.max_lines == 0 {
            return Err(ConfigError::InvalidToolOutputLimit { field: "max_lines" });
        }
        if self.tool_output.max_bytes == 0 {
            return Err(ConfigError::InvalidToolOutputLimit { field: "max_bytes" });
        }

        for (name, profile) in &self.agents {
            if profile.r#type == AgentType::Primary && profile.models.is_empty() {
                return Err(ConfigError::EmptyModels {
                    profile: name.clone(),
                });
            }
            if !profile.enabled && profile.r#type == AgentType::Internal {
                return Err(ConfigError::InternalAgentDisabled {
                    profile: name.clone(),
                });
            }
            for alias in &profile.models {
                if !is_known_model_reference(&self.models, alias) {
                    return Err(ConfigError::UnknownModelAlias {
                        profile: name.clone(),
                        alias: alias.clone(),
                    });
                }
            }
            validate_agent_permissions(&profile.permissions, name)?;
            if profile.delegation.enabled && profile.delegation.allowed_profiles.is_empty() {
                return Err(ConfigError::EmptyAllowedProfiles {
                    profile: name.clone(),
                });
            }
            for allowed_profile in &profile.delegation.allowed_profiles {
                let Some(target) = self.agents.get(allowed_profile) else {
                    return Err(ConfigError::UnknownAllowedProfile {
                        profile: name.clone(),
                    });
                };
                if target.r#type == AgentType::Primary {
                    return Err(ConfigError::PrimaryAllowedProfile {
                        profile: name.clone(),
                    });
                }
                if target.r#type == AgentType::Internal {
                    return Err(ConfigError::InternalAllowedProfile {
                        profile: name.clone(),
                    });
                }
            }
        }
        self.validate_internal_agents()?;
        Ok(())
    }

    fn validate_internal_agents(&self) -> Result<(), ConfigError> {
        validate_internal_profile("approval", &self.internal_agents.approval, &self.models)?;
        validate_internal_profile(
            "context_compaction",
            &self.internal_agents.context_compaction.profile,
            &self.models,
        )?;
        validate_internal_profile(
            "session_title",
            &self.internal_agents.session_title.profile,
            &self.models,
        )?;
        let compaction = &self.internal_agents.context_compaction;
        if !(compaction.target_percent < compaction.soft_threshold_percent
            && compaction.soft_threshold_percent < compaction.hard_threshold_percent
            && compaction.hard_threshold_percent <= 100)
        {
            return Err(ConfigError::InvalidCompactionThresholds);
        }
        for (field, value) in [
            ("max_summary_bytes", compaction.max_summary_bytes),
            (
                "max_native_context_bytes",
                compaction.max_native_context_bytes,
            ),
        ] {
            if value == 0 || value > MAX_CHECKPOINT_BYTES {
                return Err(ConfigError::CompactionPersistenceTooLarge { field });
            }
        }
        let title = &self.internal_agents.session_title.policy;
        if title.max_chars == 0 || title.max_input_messages == 0 {
            return Err(ConfigError::InvalidTitlePolicy);
        }
        Ok(())
    }

    /// Resolves exactly global permissions plus the named profile's overlay.
    /// Parent profile policy is deliberately not an input to this operation.
    pub fn materialize_policy(
        &self,
        model_set: &ModelSet,
        profile_name: &str,
    ) -> Result<PolicySnapshot, ConfigError> {
        self.materialize_with_parent(model_set, profile_name, DepthLimit::Unlimited, None)
    }

    /// Materializes a child profile with the parent's frozen policy.
    /// Empty child model chains inherit the parent's resolved model chain.
    pub fn materialize_child_policy(
        &self,
        model_set: &ModelSet,
        profile_name: &str,
        parent: impl Into<ChildPolicyParent>,
    ) -> Result<PolicySnapshot, ConfigError> {
        let parent = parent.into();
        self.materialize_with_parent(
            model_set,
            profile_name,
            parent.depth_limit,
            parent.models.as_deref(),
        )
    }

    fn materialize_with_parent(
        &self,
        model_set: &ModelSet,
        profile_name: &str,
        parent_limit: DepthLimit,
        parent_models: Option<&[FrozenModelBinding]>,
    ) -> Result<PolicySnapshot, ConfigError> {
        self.validate()?;
        let profile = self
            .agents
            .get(profile_name)
            .ok_or_else(|| ConfigError::UnknownProfile(profile_name.to_owned()))?;
        if !profile.enabled {
            return Err(ConfigError::DisabledProfile(profile_name.to_owned()));
        }
        let static_set = self.build_model_set()?;
        for (alias, expected) in static_set.entries() {
            let Some(installed) = model_set.get(alias) else {
                return Err(ConfigError::InstalledModelSetMismatch);
            };
            if installed.behavior_fingerprint() != expected.behavior_fingerprint() {
                return Err(ConfigError::InstalledModelSetMismatch);
            }
        }
        let mut rules = self.permissions.rules.clone();
        rules.extend(profile.permissions.rules.iter().cloned().map(|mut rule| {
            rule.source = RuleSource::Profile;
            rule
        }));

        let models = if profile.models.is_empty() {
            parent_models
                .map(ToOwned::to_owned)
                .ok_or_else(|| ConfigError::EmptyModels {
                    profile: profile_name.to_owned(),
                })?
        } else {
            profile
                .models
                .iter()
                .map(|alias| {
                    model_set
                        .freeze(alias)
                        .map_err(cookie_agent_models::ModelBuildError::from)
                })
                .collect::<Result<Vec<_>, _>>()?
        };

        Ok(PolicySnapshot {
            profile: ProfileSnapshot {
                name: profile_name.to_owned(),
                r#type: profile.r#type,
            },
            models,
            tools: profile.tools.iter().cloned().collect(),
            permissions: ResolvedPermissions { rules },
            delegation: DelegationPolicy {
                enabled: profile.delegation.enabled,
                allowed_profiles: profile
                    .delegation
                    .allowed_profiles
                    .iter()
                    .cloned()
                    .collect(),
                depth_limit: DepthLimit::for_child(profile.delegation.limit, parent_limit),
            },
            result_limits: ResultLimits {
                delegate_result_bytes: DEFAULT_DELEGATE_RESULT_BYTES,
                tool_output_max_lines: self.tool_output.max_lines,
                tool_output_max_bytes: self.tool_output.max_bytes,
            },
        })
    }
}

fn validate_internal_profile(
    name: &'static str,
    profile: &InternalModelAgentConfig,
    static_models: &BTreeMap<String, ConfiguredModel>,
) -> Result<(), ConfigError> {
    for (field, value) in [
        ("max_input_tokens", profile.max_input_tokens),
        ("max_output_tokens", profile.max_output_tokens),
        ("timeout_ms", profile.timeout_ms),
    ] {
        if value == 0 {
            return Err(ConfigError::InvalidInternalBudget { agent: name, field });
        }
    }
    for alias in &profile.models {
        if !is_known_model_reference(static_models, alias) {
            return Err(ConfigError::UnknownModelAlias {
                profile: name.to_owned(),
                alias: alias.clone(),
            });
        }
    }
    Ok(())
}

fn is_known_model_reference(
    static_models: &BTreeMap<String, ConfiguredModel>,
    alias: &str,
) -> bool {
    static_models.contains_key(alias) || is_exact_catalog_alias(alias)
}

fn is_exact_catalog_alias(alias: &str) -> bool {
    let Some((provider, model)) = alias.split_once('/') else {
        return false;
    };
    valid_catalog_alias_segment(provider)
        && model.split('/').all(valid_catalog_alias_segment)
        && !alias.chars().any(char::is_control)
}

fn valid_catalog_alias_segment(segment: &str) -> bool {
    !segment.is_empty() && segment != "." && segment != ".." && segment.trim() == segment
}

fn validate_permission_config(config: &PermissionConfig, scope: &str) -> Result<(), ConfigError> {
    validate_rules(&config.rules, scope)
}

fn validate_agent_permissions(
    config: &AgentPermissionConfig,
    profile: &str,
) -> Result<(), ConfigError> {
    validate_rules(&config.rules, profile)
}

fn validate_rules(rules: &[PermissionRule], scope: &str) -> Result<(), ConfigError> {
    for rule in rules {
        if !valid_effect(&rule.effect) {
            return Err(ConfigError::InvalidEffect {
                scope: scope.to_owned(),
            });
        }
    }
    Ok(())
}

fn valid_effect(value: &str) -> bool {
    matches!(value, "allow" | "ask" | "deny")
}

fn default_value() -> Result<Value, ConfigError> {
    Value::try_from(Config::default()).map_err(ConfigError::Serialize)
}

fn merge_optional_file(
    merged: &mut Value,
    path: Option<&Path>,
    source: RuleSource,
) -> Result<(), ConfigError> {
    let Some(value) = load_optional_file(path, source)? else {
        return Ok(());
    };
    merge_value(merged, value, source, &[]);
    Ok(())
}

fn load_optional_file(
    path: Option<&Path>,
    layer: RuleSource,
) -> Result<Option<Value>, ConfigError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source_error) => {
            return Err(ConfigError::Read {
                layer,
                path: path.to_owned(),
                source: source_error,
            });
        }
    };
    let value = contents
        .parse::<Value>()
        .map_err(|source_error| ConfigError::Toml {
            layer,
            path: path.to_owned(),
            source: source_error,
        })?;
    let mut value = value;
    interpolate_value(&mut value, layer, path, &mut Vec::new())?;
    Ok(Some(value))
}

fn interpolate_value(
    value: &mut Value,
    layer: RuleSource,
    source_path: &Path,
    key_path: &mut Vec<String>,
) -> Result<(), ConfigError> {
    match value {
        Value::String(value) => {
            if is_interpolation_path(key_path) {
                *value = interpolate_string(value, layer, source_path, key_path)?;
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                if let Some(last_index) = key_path.len().checked_sub(1) {
                    let key = std::mem::take(&mut key_path[last_index]);
                    key_path[last_index] = format!("{key}[{index}]");
                    interpolate_value(value, layer, source_path, key_path)?;
                    key_path[last_index] = key;
                } else {
                    key_path.push(format!("[{index}]"));
                    interpolate_value(value, layer, source_path, key_path)?;
                    key_path.pop();
                }
            }
        }
        Value::Table(values) => {
            for (key, value) in values {
                key_path.push(key.clone());
                interpolate_value(value, layer, source_path, key_path)?;
                key_path.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_interpolation_path(path: &[String]) -> bool {
    matches!(path, [models, _, endpoint]
        if models == "models" && endpoint == "endpoint")
        || matches!(path, [models, _, auth, field]
        if models == "models" && auth == "auth" && is_auth_interpolation_field(field))
        || matches!(path, [models, _, headers, _]
            if models == "models" && headers == "headers")
}

fn is_auth_interpolation_field(field: &str) -> bool {
    matches!(
        field,
        "value" | "token" | "api_key" | "access_key_id" | "secret_access_key" | "session_token"
    )
}

fn interpolate_string(
    value: &str,
    layer: RuleSource,
    source_path: &Path,
    key_path: &[String],
) -> Result<String, ConfigError> {
    let mut result = String::with_capacity(value.len());
    let mut cursor = 0;

    while let Some(offset) = value[cursor..].find('$') {
        let dollar = cursor + offset;
        result.push_str(&value[cursor..dollar]);
        let bytes = value.as_bytes();
        if bytes.get(dollar + 1) == Some(&b'$') {
            result.push('$');
            cursor = dollar + 2;
            continue;
        }
        if !value[dollar..].starts_with("${env:") {
            result.push('$');
            cursor = dollar + 1;
            continue;
        }

        let name_start = dollar + "${env:".len();
        let Some(first) = bytes.get(name_start) else {
            result.push('$');
            cursor = dollar + 1;
            continue;
        };
        if !first.is_ascii_alphabetic() && *first != b'_' {
            result.push('$');
            cursor = dollar + 1;
            continue;
        }
        let mut name_end = name_start + 1;
        while bytes
            .get(name_end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            name_end += 1;
        }
        if bytes.get(name_end) != Some(&b'}') {
            result.push('$');
            cursor = dollar + 1;
            continue;
        }

        let variable = &value[name_start..name_end];
        let resolved = match env::var_os(variable) {
            Some(value) => {
                value
                    .into_string()
                    .map_err(|_| ConfigError::EnvironmentInterpolation {
                        layer,
                        path: source_path.to_owned(),
                        key: key_path.join("."),
                        variable: variable.to_owned(),
                        reason: "value is not valid UTF-8",
                    })?
            }
            None => {
                return Err(ConfigError::EnvironmentInterpolation {
                    layer,
                    path: source_path.to_owned(),
                    key: key_path.join("."),
                    variable: variable.to_owned(),
                    reason: "variable is not set",
                });
            }
        };
        result.push_str(&resolved);
        cursor = name_end + 1;
    }
    result.push_str(&value[cursor..]);
    Ok(result)
}

fn merge_value(base: &mut Value, overlay: Value, source: RuleSource, path: &[String]) {
    match (base, overlay) {
        (Value::Table(base), Value::Table(overlay)) => {
            for (key, value) in overlay {
                let mut child_path = path.to_vec();
                child_path.push(key.clone());
                match base.get_mut(&key) {
                    Some(existing) => merge_value(existing, value, source, &child_path),
                    None => {
                        let mut value = value;
                        tag_rule_array(&mut value, source, &child_path);
                        base.insert(key, value);
                    }
                }
            }
        }
        (Value::Array(base), Value::Array(mut overlay)) if is_rule_path(path) => {
            tag_rules(&mut overlay, source);
            base.append(&mut overlay);
        }
        (base, mut overlay) => {
            tag_rule_array(&mut overlay, source, path);
            *base = overlay;
        }
    }
}

fn is_rule_path(path: &[String]) -> bool {
    path.len() >= 2 && path[path.len() - 1] == "rules" && path[path.len() - 2] == "permissions"
}

fn tag_rule_array(value: &mut Value, source: RuleSource, path: &[String]) {
    match value {
        Value::Array(rules) if is_rule_path(path) => tag_rules(rules, source),
        Value::Table(table) => {
            for (key, value) in table {
                let mut child_path = path.to_vec();
                child_path.push(key.clone());
                tag_rule_array(value, source, &child_path);
            }
        }
        _ => {}
    }
}

fn tag_rules(rules: &mut [Value], source: RuleSource) {
    for rule in rules {
        if let Value::Table(table) = rule {
            table.insert(
                "__source".to_owned(),
                Value::String(source_name(source).to_owned()),
            );
        }
    }
}

const fn source_name(source: RuleSource) -> &'static str {
    match source {
        RuleSource::Builtin => "builtin",
        RuleSource::User => "user",
        RuleSource::Workspace => "workspace",
        RuleSource::Profile => "profile",
    }
}

fn user_config_path() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".config/cookie_agent/config.toml")
}

/// `*` matches any characters (including `/`) and `?` exactly one character.
/// A terminal `" *"` also permits omitting the space and wildcard together.
#[must_use]
pub fn simple_wildcard_match(pattern: &str, resource: &str) -> bool {
    wildcard_match(pattern, resource)
        || pattern
            .strip_suffix(" *")
            .is_some_and(|prefix| wildcard_match(prefix, resource))
}

fn wildcard_match(pattern: &str, resource: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let resource: Vec<char> = resource.chars().collect();
    let (mut pattern_index, mut resource_index) = (0, 0);
    let (mut star, mut retry_resource) = (None, 0);

    while resource_index < resource.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == resource[resource_index])
        {
            pattern_index += 1;
            resource_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star = Some(pattern_index);
            pattern_index += 1;
            retry_resource = resource_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            retry_resource += 1;
            resource_index = retry_resource;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}
