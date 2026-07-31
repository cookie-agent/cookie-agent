//! Layered TOML configuration, policy snapshots, and workspace trust storage.
//!
//! Permission-rule arrays are the one exception to normal Figment merge
//! semantics. [`load_layered`] merges the TOML values itself, concatenating
//! `permissions.rules` arrays while replacing all other arrays, before using
//! Figment for environment input and Serde for typed extraction.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    hash::Hasher,
    io,
    path::{Path, PathBuf},
};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml::Value;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 7419;
const DEFAULT_DELEGATE_RESULT_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Rules and tier defaults applied to every profile.
    #[serde(default)]
    pub permissions: PermissionConfig,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentProfile>,
}

impl Default for Config {
    fn default() -> Self {
        let mut agents = BTreeMap::new();
        agents.insert(
            "compaction".to_owned(),
            AgentProfile {
                r#type: AgentType::Internal,
                ..AgentProfile::default()
            },
        );
        Self {
            server: ServerConfig::default(),
            providers: BTreeMap::new(),
            permissions: PermissionConfig::default(),
            agents,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub kind: ProviderType,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub api: Option<OpenAiApi>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProviderType {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenAiApi {
    Responses,
    Completions,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentProfile {
    #[serde(rename = "type", default)]
    pub r#type: AgentType,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DelegationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allowed_profiles: Vec<String>,
    pub limit: Option<u32>,
}

/// Defaults applied globally when a profile does not override the tier.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PermissionConfig {
    #[serde(default = "default_permission_tier")]
    pub read: String,
    #[serde(default = "default_permission_tier")]
    pub write: String,
    #[serde(default = "default_permission_tier")]
    pub exec: String,
    #[serde(default = "default_permission_tier")]
    pub delegate: String,
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            read: default_permission_tier(),
            write: default_permission_tier(),
            exec: default_permission_tier(),
            delegate: default_permission_tier(),
            rules: Vec::new(),
        }
    }
}

/// A profile's tier values are optional so global tier defaults remain effective.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AgentPermissionConfig {
    pub read: Option<String>,
    pub write: Option<String>,
    pub exec: Option<String>,
    pub delegate: Option<String>,
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

fn default_permission_tier() -> String {
    "ask".to_owned()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionRule {
    pub id: String,
    pub action: String,
    pub resource: String,
    pub effect: String,
    #[serde(default)]
    pub hard: bool,
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
    Env,
    Profile,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionEffect {
    Allow,
    Ask,
    Deny,
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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PolicySnapshot {
    pub profile: ProfileSnapshot,
    pub models: Vec<ModelConfig>,
    pub tools: BTreeSet<String>,
    pub permissions: ResolvedPermissions,
    pub delegation: DelegationPolicy,
    pub result_limits: ResultLimits,
}

/// Parent policy inputs needed to materialize a child profile.
pub struct ChildPolicyParent {
    depth_limit: DepthLimit,
    models: Option<Vec<ModelConfig>>,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProfileSnapshot {
    pub name: String,
    #[serde(rename = "type")]
    pub r#type: AgentType,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedPermissions {
    pub read: PermissionEffect,
    pub write: PermissionEffect,
    pub exec: PermissionEffect,
    pub delegate: PermissionEffect,
    pub rules: Vec<PermissionRule>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DelegationPolicy {
    pub enabled: bool,
    pub allowed_profiles: BTreeSet<String>,
    pub depth_limit: DepthLimit,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResultLimits {
    pub delegate_result_bytes: usize,
}

impl Default for ResultLimits {
    fn default() -> Self {
        Self {
            delegate_result_bytes: DEFAULT_DELEGATE_RESULT_BYTES,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
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
    #[error("configuration extraction failed: {0}")]
    Figment(#[source] Box<figment::Error>),
    #[error("merged configuration extraction failed: {0}")]
    Extract(#[source] toml::de::Error),
    #[error("could not construct built-in configuration: {0}")]
    Serialize(#[source] toml::ser::Error),
    #[error("profile `{profile}` has an empty model chain")]
    EmptyModels { profile: String },
    #[error("profile `{profile}` references unknown provider `{provider}`")]
    UnknownProvider { profile: String, provider: String },
    #[error("profile `{profile}` allows unknown profile `{allowed_profile}`")]
    UnknownAllowedProfile {
        profile: String,
        allowed_profile: String,
    },
    #[error("profile `{profile}` allows primary-only profile `{allowed_profile}`")]
    PrimaryAllowedProfile {
        profile: String,
        allowed_profile: String,
    },
    #[error("profile `{profile}` allows internal profile `{allowed_profile}`")]
    InternalAllowedProfile {
        profile: String,
        allowed_profile: String,
    },
    #[error("internal agents cannot be disabled: `{profile}`")]
    InternalAgentDisabled { profile: String },
    #[error("profile `{profile}` enables delegation with no allowed profiles")]
    EmptyAllowedProfiles { profile: String },
    #[error("invalid {tier} tier `{value}` in {scope}")]
    InvalidTier {
        scope: String,
        tier: &'static str,
        value: String,
    },
    #[error("invalid permission effect `{value}` in {scope} rule `{rule}`")]
    InvalidEffect {
        scope: String,
        rule: String,
        value: String,
    },
    #[error("unknown profile `{0}`")]
    UnknownProfile(String),
}

/// Builds the standard Figment stack. It is useful to callers that need
/// Figment-native provenance but does not perform the permission-rule append.
#[must_use]
pub fn build_figment(user_config: Option<&Path>, workspace_config: Option<&Path>) -> Figment {
    let mut figment = Figment::from(Serialized::defaults(Config::default()));
    if let Some(path) = user_config {
        figment = figment.merge(Toml::file(path));
    }
    if let Some(path) = workspace_config {
        figment = figment.merge(Toml::file(path));
    }
    figment.merge(Env::prefixed("COOKIECODE_").split("__"))
}

/// Extracts and validates a pre-built Figment stack.
pub fn load_from(figment: Figment) -> Result<Config, ConfigError> {
    let config: Config = figment
        .extract()
        .map_err(|error| ConfigError::Figment(Box::new(error)))?;
    config.validate()?;
    Ok(config)
}

/// Loads the default user configuration and a repository's workspace config.
pub fn load(workspace: &Path) -> Result<Config, ConfigError> {
    let user = user_config_path();
    let workspace_config = workspace.join(".cookiecode/config.toml");
    load_layered(Some(&user), Some(&workspace_config))
}

/// Loads config with the documented layer order.
///
/// Missing user and workspace files are simply absent layers. Environment is
/// collected through Figment's `Env` provider, then merged as the final layer.
pub fn load_layered(
    user_config: Option<&Path>,
    workspace_config: Option<&Path>,
) -> Result<Config, ConfigError> {
    let mut merged = default_value()?;

    merge_optional_file(&mut merged, user_config, RuleSource::User)?;
    merge_optional_file(&mut merged, workspace_config, RuleSource::Workspace)?;

    let environment: Value = Figment::from(Env::prefixed("COOKIECODE_").split("__"))
        .extract()
        .map_err(|error| ConfigError::Figment(Box::new(error)))?;
    merge_value(&mut merged, environment, RuleSource::Env, &[]);

    let config: Config = merged.try_into().map_err(ConfigError::Extract)?;
    config.validate()?;
    Ok(config)
}

/// Concatenates permission rules in already-established layer order.
#[must_use]
pub fn merge_permission_rules(layers: Vec<Vec<PermissionRule>>) -> Vec<PermissionRule> {
    layers.into_iter().flatten().collect()
}

impl Config {
    /// Checks cross-reference and policy constraints after typed extraction.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_permission_config(&self.permissions, "global")?;

        for (name, profile) in &self.agents {
            if profile.r#type == AgentType::Primary && profile.models.is_empty() {
                return Err(ConfigError::EmptyModels {
                    profile: name.clone(),
                });
            }
            if !profile.enabled && (profile.r#type == AgentType::Internal || name == "compaction") {
                return Err(ConfigError::InternalAgentDisabled {
                    profile: name.clone(),
                });
            }
            for model in &profile.models {
                if !self.providers.contains_key(&model.provider) {
                    return Err(ConfigError::UnknownProvider {
                        profile: name.clone(),
                        provider: model.provider.clone(),
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
                        allowed_profile: allowed_profile.clone(),
                    });
                };
                if target.r#type == AgentType::Primary {
                    return Err(ConfigError::PrimaryAllowedProfile {
                        profile: name.clone(),
                        allowed_profile: allowed_profile.clone(),
                    });
                }
                if target.r#type == AgentType::Internal {
                    return Err(ConfigError::InternalAllowedProfile {
                        profile: name.clone(),
                        allowed_profile: allowed_profile.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Resolves exactly global permissions plus the named profile's overlay.
    /// Parent profile policy is deliberately not an input to this operation.
    pub fn materialize_policy(&self, profile_name: &str) -> Result<PolicySnapshot, ConfigError> {
        self.materialize_with_parent(profile_name, DepthLimit::Unlimited, None)
    }

    /// Materializes a child profile with the parent's frozen policy.
    /// Empty child model chains inherit the parent's resolved model chain.
    pub fn materialize_child_policy(
        &self,
        profile_name: &str,
        parent: impl Into<ChildPolicyParent>,
    ) -> Result<PolicySnapshot, ConfigError> {
        let parent = parent.into();
        self.materialize_with_parent(profile_name, parent.depth_limit, parent.models.as_deref())
    }

    fn materialize_with_parent(
        &self,
        profile_name: &str,
        parent_limit: DepthLimit,
        parent_models: Option<&[ModelConfig]>,
    ) -> Result<PolicySnapshot, ConfigError> {
        self.validate()?;
        let profile = self
            .agents
            .get(profile_name)
            .ok_or_else(|| ConfigError::UnknownProfile(profile_name.to_owned()))?;

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
            profile.models.clone()
        };

        Ok(PolicySnapshot {
            profile: ProfileSnapshot {
                name: profile_name.to_owned(),
                r#type: profile.r#type,
            },
            models,
            tools: profile.tools.iter().cloned().collect(),
            permissions: ResolvedPermissions {
                read: resolve_tier(profile.permissions.read.as_deref(), &self.permissions.read),
                write: resolve_tier(
                    profile.permissions.write.as_deref(),
                    &self.permissions.write,
                ),
                exec: resolve_tier(profile.permissions.exec.as_deref(), &self.permissions.exec),
                delegate: resolve_tier(
                    profile.permissions.delegate.as_deref(),
                    &self.permissions.delegate,
                ),
                rules,
            },
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
            result_limits: ResultLimits::default(),
        })
    }
}

fn resolve_tier(overlay: Option<&str>, default: &str) -> PermissionEffect {
    parse_effect(overlay.unwrap_or(default)).expect("validated permission tier")
}

fn validate_permission_config(config: &PermissionConfig, scope: &str) -> Result<(), ConfigError> {
    validate_tier(&config.read, scope, "read")?;
    validate_tier(&config.write, scope, "write")?;
    validate_tier(&config.exec, scope, "exec")?;
    validate_tier(&config.delegate, scope, "delegate")?;
    validate_rules(&config.rules, scope)
}

fn validate_agent_permissions(
    config: &AgentPermissionConfig,
    profile: &str,
) -> Result<(), ConfigError> {
    for (tier, value) in [
        ("read", config.read.as_deref()),
        ("write", config.write.as_deref()),
        ("exec", config.exec.as_deref()),
        ("delegate", config.delegate.as_deref()),
    ] {
        if let Some(value) = value {
            validate_tier(value, profile, tier)?;
        }
    }
    validate_rules(&config.rules, profile)
}

fn validate_tier(value: &str, scope: &str, tier: &'static str) -> Result<(), ConfigError> {
    if parse_effect(value).is_none() {
        return Err(ConfigError::InvalidTier {
            scope: scope.to_owned(),
            tier,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_rules(rules: &[PermissionRule], scope: &str) -> Result<(), ConfigError> {
    for rule in rules {
        if parse_effect(&rule.effect).is_none() {
            return Err(ConfigError::InvalidEffect {
                scope: scope.to_owned(),
                rule: rule.id.clone(),
                value: rule.effect.clone(),
            });
        }
    }
    Ok(())
}

fn parse_effect(value: &str) -> Option<PermissionEffect> {
    match value {
        "allow" => Some(PermissionEffect::Allow),
        "ask" => Some(PermissionEffect::Ask),
        "deny" => Some(PermissionEffect::Deny),
        _ => None,
    }
}

fn default_value() -> Result<Value, ConfigError> {
    Value::try_from(Config::default()).map_err(ConfigError::Serialize)
}

fn merge_optional_file(
    merged: &mut Value,
    path: Option<&Path>,
    source: RuleSource,
) -> Result<(), ConfigError> {
    let Some(path) = path else {
        return Ok(());
    };
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source_error) => {
            return Err(ConfigError::Read {
                layer: source,
                path: path.to_owned(),
                source: source_error,
            });
        }
    };
    let value = contents
        .parse::<Value>()
        .map_err(|source_error| ConfigError::Toml {
            layer: source,
            path: path.to_owned(),
            source: source_error,
        })?;
    merge_value(merged, value, source, &[]);
    Ok(())
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
        RuleSource::Env => "env",
        RuleSource::Profile => "profile",
    }
}

fn user_config_path() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".config/cookiecode/config.toml")
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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TrustStore {
    #[serde(default)]
    entries: BTreeMap<String, TrustEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TrustEntry {
    content_hash: String,
}

#[derive(Debug, Error)]
pub enum TrustError {
    #[error("could not determine the home directory")]
    HomeUnavailable,
    #[error("could not canonicalize workspace {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not read trust store {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not parse trust store {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not write trust store {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not encode trust store: {0}")]
    Encode(#[source] serde_json::Error),
}

impl TrustStore {
    pub fn load(path: &Path) -> Result<Self, TrustError> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| TrustError::Parse {
                path: path.to_owned(),
                source,
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(TrustError::Read {
                path: path.to_owned(),
                source,
            }),
        }
    }

    pub fn is_trusted(&self, workspace: &Path, config_bytes: &[u8]) -> Result<bool, TrustError> {
        let workspace = canonical_workspace(workspace)?;
        Ok(self
            .entries
            .get(&workspace)
            .is_some_and(|entry| entry.content_hash == config_hash(config_bytes)))
    }

    pub fn needs_retrust(&self, workspace: &Path, config_bytes: &[u8]) -> Result<bool, TrustError> {
        Ok(!self.is_trusted(workspace, config_bytes)?)
    }

    pub fn record_trust(
        &mut self,
        workspace: &Path,
        config_bytes: &[u8],
    ) -> Result<(), TrustError> {
        self.entries.insert(
            canonical_workspace(workspace)?,
            TrustEntry {
                content_hash: config_hash(config_bytes),
            },
        );
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<(), TrustError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| TrustError::Write {
                path: path.to_owned(),
                source,
            })?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(TrustError::Encode)?;
        fs::write(path, bytes).map_err(|source| TrustError::Write {
            path: path.to_owned(),
            source,
        })
    }
}

pub fn trust_store_path() -> Result<PathBuf, TrustError> {
    let home = env::var_os("HOME").ok_or(TrustError::HomeUnavailable)?;
    Ok(PathBuf::from(home).join(".local/share/cookiecode/trust.json"))
}

pub fn is_trusted(workspace: &Path, config_bytes: &[u8]) -> Result<bool, TrustError> {
    TrustStore::load(&trust_store_path()?)?.is_trusted(workspace, config_bytes)
}

pub fn needs_retrust(workspace: &Path, config_bytes: &[u8]) -> Result<bool, TrustError> {
    TrustStore::load(&trust_store_path()?)?.needs_retrust(workspace, config_bytes)
}

pub fn record_trust(workspace: &Path, config_bytes: &[u8]) -> Result<(), TrustError> {
    let path = trust_store_path()?;
    let mut store = TrustStore::load(&path)?;
    store.record_trust(workspace, config_bytes)?;
    store.save(&path)
}

fn canonical_workspace(workspace: &Path) -> Result<String, TrustError> {
    workspace
        .canonicalize()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|source| TrustError::Canonicalize {
            path: workspace.to_owned(),
            source,
        })
}

fn config_hash(bytes: &[u8]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(bytes);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use insta::assert_json_snapshot;
    use tempfile::TempDir;

    use super::*;

    const BASE: &str = r#"
[providers.test]
type = "openai-compatible"
base_url = "http://localhost/v1"

[agents.primary]
type = "primary"
models = [{ provider = "test", model = "one" }]
tools = ["read", "bash"]

[agents.primary.delegation]
enabled = true
allowed_profiles = ["worker"]

[agents.worker]
type = "subagent"
models = [{ provider = "test", model = "two" }]
tools = ["read"]
"#;

    fn config_tree() -> (TempDir, PathBuf, PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let user = temp.path().join("user.toml");
        let workspace = temp.path().join("workspace.toml");
        (temp, user, workspace)
    }

    fn write(path: &Path, contents: &str) {
        fs::write(path, contents).expect("write config");
    }

    #[test]
    fn layers_replace_normal_arrays_and_append_rules() {
        let (_temp, user, workspace) = config_tree();
        write(
            &user,
            &format!(
                "{BASE}\n[agents.primary.permissions]\nread = \"allow\"\n[[agents.primary.permissions.rules]]\nid = \"user\"\naction = \"bash\"\nresource = \"git status *\"\neffect = \"allow\"\n"
            ),
        );
        write(
            &workspace,
            "[server]\nport = 8000\n[agents.primary]\ntools = [\"grep\"]\n[agents.primary.permissions]\n[[agents.primary.permissions.rules]]\nid = \"workspace\"\naction = \"bash\"\nresource = \"git push *\"\neffect = \"deny\"\n",
        );

        let config = load_layered(Some(&user), Some(&workspace)).expect("load config");
        assert_eq!(config.server.port, 8000);
        assert_eq!(config.agents["primary"].tools, ["grep"]);
        let rules = &config.agents["primary"].permissions.rules;
        assert_eq!(
            rules
                .iter()
                .map(|rule| rule.id.as_str())
                .collect::<Vec<_>>(),
            ["user", "workspace"]
        );
        assert_eq!(rules[0].source, RuleSource::User);
        assert_eq!(rules[1].source, RuleSource::Workspace);
    }

    #[test]
    fn validates_every_cross_reference_and_permission_failure() {
        let cases = [
            (
                "[agents.a]\ntype = \"primary\"\nmodels = []",
                "empty model chain",
            ),
            (
                "[agents.a]\nmodels = [{ provider = \"missing\", model = \"x\" }]",
                "unknown provider",
            ),
            (
                "[providers.p]\ntype = \"anthropic\"\n[agents.a]\nmodels = [{ provider = \"p\", model = \"x\" }]\n[agents.a.delegation]\nallowed_profiles = [\"missing\"]",
                "unknown profile",
            ),
            (
                "[providers.p]\ntype = \"anthropic\"\n[agents.a]\nmodels = [{ provider = \"p\", model = \"x\" }]\n[agents.b]\ntype = \"primary\"\nmodels = [{ provider = \"p\", model = \"x\" }]\n[agents.a.delegation]\nallowed_profiles = [\"b\"]",
                "primary-only",
            ),
            (
                "[providers.p]\ntype = \"anthropic\"\n[agents.a]\nmodels = [{ provider = \"p\", model = \"x\" }]\n[agents.a.delegation]\nenabled = true",
                "no allowed profiles",
            ),
            ("[permissions]\nread = \"sometimes\"", "invalid read tier"),
            (
                "[permissions]\n[[permissions.rules]]\nid = \"bad\"\naction = \"bash\"\nresource = \"x\"\neffect = \"maybe\"",
                "invalid permission effect",
            ),
        ];
        for (input, expected) in cases {
            let (_temp, user, workspace) = config_tree();
            write(&workspace, input);
            let error = load_layered(Some(&user), Some(&workspace)).expect_err("invalid config");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn snapshot_includes_global_and_own_rules_only() {
        let (_temp, user, workspace) = config_tree();
        write(
            &workspace,
            &format!(
                "{BASE}\n[[permissions.rules]]\nid = \"global\"\naction = \"read\"\nresource = \"*\"\neffect = \"allow\"\n[[agents.primary.permissions.rules]]\nid = \"primary\"\naction = \"bash\"\nresource = \"git status *\"\neffect = \"allow\"\n[[agents.worker.permissions.rules]]\nid = \"worker\"\naction = \"bash\"\nresource = \"*\"\neffect = \"deny\"\n"
            ),
        );
        let config = load_layered(Some(&user), Some(&workspace)).expect("load config");
        let snapshot = config.materialize_policy("primary").expect("snapshot");
        assert_eq!(snapshot.models[0].model, "one");
        assert_eq!(
            snapshot
                .permissions
                .rules
                .iter()
                .map(|rule| rule.id.as_str())
                .collect::<Vec<_>>(),
            ["global", "primary"]
        );
        assert_eq!(snapshot.permissions.rules[1].source, RuleSource::Profile);
        assert_eq!(snapshot.delegation.depth_limit, DepthLimit::Unlimited);
        assert_eq!(
            config
                .materialize_child_policy("worker", DepthLimit::Finite(2))
                .expect("child snapshot")
                .delegation
                .depth_limit,
            DepthLimit::Finite(1)
        );
        assert_eq!(
            DepthLimit::for_child(Some(4), DepthLimit::Finite(2)),
            DepthLimit::Finite(1)
        );
    }

    #[test]
    fn materialized_policy_snapshot_is_stable() {
        let (_temp, user, workspace) = config_tree();
        write(
            &workspace,
            &format!(
                "{BASE}\n[[permissions.rules]]\nid = \"global\"\naction = \"read\"\nresource = \"*\"\neffect = \"allow\"\n[[agents.primary.permissions.rules]]\nid = \"primary\"\naction = \"bash\"\nresource = \"git status *\"\neffect = \"allow\"\n"
            ),
        );
        let config = load_layered(Some(&user), Some(&workspace)).expect("load config");
        assert_json_snapshot!(
            config
                .materialize_policy("primary")
                .expect("policy snapshot")
        );
    }

    #[test]
    fn parses_internal_profiles_and_defaults_enabled() {
        let (_temp, user, workspace) = config_tree();
        write(&workspace, "[agents.maintenance]\ntype = \"internal\"");

        let config = load_layered(Some(&user), Some(&workspace)).expect("load config");
        let profile = &config.agents["maintenance"];
        assert_eq!(profile.r#type, AgentType::Internal);
        assert!(profile.enabled);
    }

    #[test]
    fn rejects_disabled_internal_profiles_and_internal_delegate_targets() {
        let cases = [
            (
                "[agents.maintenance]\ntype = \"internal\"\nenabled = false",
                "internal agents cannot be disabled",
            ),
            (
                "[providers.p]\ntype = \"anthropic\"\n[agents.primary]\ntype = \"primary\"\nmodels = [{ provider = \"p\", model = \"x\" }]\n[agents.primary.delegation]\nallowed_profiles = [\"compaction\"]",
                "allows internal profile",
            ),
        ];
        for (input, expected) in cases {
            let (_temp, user, workspace) = config_tree();
            write(&workspace, input);
            let error = load_layered(Some(&user), Some(&workspace)).expect_err("invalid config");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn compaction_is_builtin_and_cannot_be_disabled() {
        let (_temp, user, workspace) = config_tree();
        write(&workspace, "[agents.compaction]\ntools = [\"read\"]");
        let config = load_layered(Some(&user), Some(&workspace)).expect("load config");
        let compaction = &config.agents["compaction"];
        assert_eq!(compaction.r#type, AgentType::Internal);
        assert!(compaction.enabled);
        assert_eq!(compaction.tools, ["read"]);
        assert!(compaction.models.is_empty());

        write(&workspace, "[agents.compaction]\nenabled = false");
        let error = load_layered(Some(&user), Some(&workspace)).expect_err("invalid config");
        assert!(
            error
                .to_string()
                .contains("internal agents cannot be disabled")
        );
    }

    #[test]
    fn empty_model_chains_are_inherited_by_children_but_rejected_at_roots() {
        let (_temp, user, workspace) = config_tree();
        write(
            &workspace,
            "[providers.p]\ntype = \"anthropic\"\n[agents.root]\ntype = \"primary\"\nmodels = [{ provider = \"p\", model = \"root\" }]\n[agents.child]\ntype = \"subagent\"\n[agents.all_child]\ntype = \"all\"\n[agents.internal_child]\ntype = \"internal\"",
        );
        let config = load_layered(Some(&user), Some(&workspace)).expect("load config");
        let parent = config.materialize_policy("root").expect("root snapshot");
        let child = config
            .materialize_child_policy("child", &parent)
            .expect("child snapshot");
        assert_eq!(child.models, parent.models);
        assert!(config.materialize_policy("all_child").is_err());
        assert!(config.materialize_policy("internal_child").is_err());

        write(
            &workspace,
            "[agents.root]\ntype = \"primary\"\n[agents.child]\ntype = \"subagent\"\n[agents.all_child]\ntype = \"all\"\n[agents.internal_child]\ntype = \"internal\"",
        );
        let error = load_layered(Some(&user), Some(&workspace)).expect_err("invalid config");
        assert!(error.to_string().contains("empty model chain"));
    }

    #[test]
    fn trust_store_requires_retrust_after_config_changes() {
        let temp = TempDir::new().expect("tempdir");
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let store_path = temp.path().join("trust.json");
        let bytes = b"[server]\nport = 7419\n";

        let mut store = TrustStore::load(&store_path).expect("empty store");
        assert!(store.needs_retrust(&workspace, bytes).expect("check trust"));
        store.record_trust(&workspace, bytes).expect("record trust");
        store.save(&store_path).expect("save trust");

        let store = TrustStore::load(&store_path).expect("load trust");
        assert!(store.is_trusted(&workspace, bytes).expect("trusted"));
        assert!(
            store
                .needs_retrust(&workspace, b"changed")
                .expect("changed")
        );
    }

    #[test]
    fn wildcard_matches_documented_and_edge_cases() {
        assert!(simple_wildcard_match("git status *", "git status"));
        assert!(simple_wildcard_match(
            "git status *",
            "git status --porcelain"
        ));
        assert!(simple_wildcard_match("*", "dir/nested/file"));
        assert!(simple_wildcard_match("file?.txt", "file1.txt"));
        assert!(!simple_wildcard_match("file?.txt", "file12.txt"));
        assert!(simple_wildcard_match("", ""));
        assert!(!simple_wildcard_match("", "x"));
        assert!(!simple_wildcard_match("git status *", "git stash"));
    }
}
