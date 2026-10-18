use std::{borrow::Cow, collections::BTreeSet, fmt};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

use crate::{
    AgentId, ModelKey, ModelSelection, PersistedModelBinding, RuntimeId, Sha256Digest,
    WildcardPattern,
};

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    Primary,
    Subagent,
    All,
    Internal,
}

/// The permission action a tool is checked under: a free-form label the tool
/// declares, matching `[a-z][a-z0-9_]{0,31}`. Rules name actions; an action
/// no rule names is denied.
///
/// The constants are the actions of the tools cookie agent ships. The daemon
/// gives three groups of them meaning: [`Self::is_path`] actions take file
/// paths as resources, [`Self::is_command`] takes shell commands, and
/// [`Self::is_base_layer`] actions belong to the daemon's own tools. Every
/// other action is opaque: its resources match rules as plain wildcard
/// patterns.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PermissionAction(Cow<'static, str>);

impl PermissionAction {
    pub const READ: Self = Self(Cow::Borrowed("read"));
    pub const WRITE: Self = Self(Cow::Borrowed("write"));
    pub const BASH: Self = Self(Cow::Borrowed("bash"));
    pub const DELEGATE: Self = Self(Cow::Borrowed("delegate"));
    pub const MESSAGE: Self = Self(Cow::Borrowed("message"));
    pub const MCP: Self = Self(Cow::Borrowed("mcp"));
    pub const SKILL: Self = Self(Cow::Borrowed("skill"));
    pub const WEBFETCH: Self = Self(Cow::Borrowed("webfetch"));

    /// The actions of the tools cookie agent ships, in display order.
    pub const BUILT_IN: [Self; 8] = [
        Self::READ,
        Self::WRITE,
        Self::BASH,
        Self::DELEGATE,
        Self::MESSAGE,
        Self::MCP,
        Self::SKILL,
        Self::WEBFETCH,
    ];

    pub const MAX_LEN: usize = 32;

    pub fn new(value: impl Into<String>) -> Result<Self, PermissionActionError> {
        let value = value.into();
        let valid = value.len() <= Self::MAX_LEN
            && value.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_lowercase() || (index > 0 && (byte.is_ascii_digit() || byte == b'_'))
            })
            && !value.is_empty();
        if !valid {
            return Err(PermissionActionError::Invalid(value));
        }
        Ok(Self(Cow::Owned(value)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether resources are file paths: rules may use `${workspace_dir}`,
    /// and a relative resource resolves against the tool's workspace.
    #[must_use]
    pub fn is_path(&self) -> bool {
        *self == Self::READ || *self == Self::WRITE
    }

    /// Whether resources are shell commands, approved by command prefix and
    /// run against the shell and working directory the runtime resolves at
    /// preparation.
    #[must_use]
    pub fn is_command(&self) -> bool {
        *self == Self::BASH
    }

    /// Whether the action belongs to the daemon's base-layer tools, which no
    /// runtime selector reaches.
    #[must_use]
    pub fn is_base_layer(&self) -> bool {
        *self == Self::DELEGATE || *self == Self::MESSAGE || *self == Self::SKILL
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionActionError {
    Invalid(String),
}

impl fmt::Display for PermissionActionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(value) => write!(
                formatter,
                "invalid permission action `{value}`; expected [a-z][a-z0-9_]{{0,31}}"
            ),
        }
    }
}

impl std::error::Error for PermissionActionError {}

impl fmt::Display for PermissionAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for PermissionAction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PermissionAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for PermissionAction {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("PermissionAction")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[a-z][a-z0-9_]{0,31}$",
            "description": "Permission action a tool declares, such as `read`, `bash`, or a runtime's own action."
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionEffect {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PermissionRule {
    pub action: PermissionAction,
    /// Runtime selector (`bash@labpc`): the rule applies only to tools of
    /// runtimes with this runtime ID. Absent, it applies everywhere,
    /// including the base layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeId>,
    pub resource: WildcardPattern,
    pub effect: PermissionEffect,
}

impl PermissionRule {
    /// A rule with no runtime selector.
    #[must_use]
    pub const fn new(
        action: PermissionAction,
        resource: WildcardPattern,
        effect: PermissionEffect,
    ) -> Self {
        Self {
            action,
            runtime: None,
            resource,
            effect,
        }
    }

    /// Whether the rule applies to a tool of the runtime with ID `runtime`
    /// (`None` for the base layer).
    #[must_use]
    pub fn applies_to(&self, runtime: Option<&str>) -> bool {
        self.runtime
            .as_ref()
            .is_none_or(|selector| Some(selector.as_str()) == runtime)
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionPermissionOverlay {
    #[schemars(length(max = 256))]
    pub rules: Vec<PermissionRule>,
}

impl SessionPermissionOverlay {
    #[must_use]
    pub const fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn validate(&self) -> Result<(), AgentSchemaError> {
        if self.rules.len() > 256 {
            return Err(AgentSchemaError::InvalidListBounds);
        }
        let mut keys = BTreeSet::new();
        if !self.rules.iter().all(|rule| {
            keys.insert((
                rule.action.clone(),
                rule.runtime.clone(),
                rule.resource.as_str().to_owned(),
            ))
        }) {
            return Err(AgentSchemaError::DuplicatePermissionRule);
        }
        Ok(())
    }
}

impl Default for SessionPermissionOverlay {
    fn default() -> Self {
        Self::empty()
    }
}

impl<'de> Deserialize<'de> for SessionPermissionOverlay {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            rules: Vec<PermissionRule>,
        }
        let value = Self {
            rules: Wire::deserialize(deserializer)?.rules,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDocumentSource {
    BuiltIn,
    User,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct FrozenDelegationPolicy {
    #[schemars(length(min = 1, max = 256))]
    pub targets: Vec<AgentId>,
    pub effective_depth_ceiling: u32,
}
impl FrozenDelegationPolicy {
    pub fn validate(&self) -> Result<(), AgentSchemaError> {
        if self.targets.is_empty() || self.targets.len() > 256 {
            return Err(AgentSchemaError::InvalidDelegationTargets);
        }
        if self.targets.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(AgentSchemaError::InvalidDelegationTargets);
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for FrozenDelegationPolicy {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            targets: Vec<AgentId>,
            effective_depth_ceiling: u32,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            targets: w.targets,
            effective_depth_ceiling: w.effective_depth_ceiling,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize)]
pub struct AgentSnapshot {
    pub agent: AgentId,
    pub mode: AgentMode,
    #[schemars(with = "AgentDescriptionSchema")]
    pub description: String,
    pub document_source: AgentDocumentSource,
    pub document_fingerprint: Sha256Digest,
    #[schemars(with = "AgentPromptSchema")]
    pub composed_prompt: String,
    pub prompt_fingerprint: Sha256Digest,
    pub max_output_tokens: u64,
    #[schemars(length(max = 256))]
    pub permissions: Vec<PermissionRule>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<FrozenDelegationPolicy>", required)]
    pub delegation: Option<FrozenDelegationPolicy>,
    #[schemars(length(min = 1, max = 256))]
    pub fallback_chain: Vec<PersistedModelBinding>,
    /// Where the run's fallback starts in `fallback_chain`: the run's model
    /// is that entry, and its fallbacks are the entries after it.
    pub selected_suffix_start: u32,
}

impl AgentSnapshot {
    pub const MAX_PROMPT_BYTES: usize = 128 * 1024;
    pub fn validate(&self) -> Result<(), AgentSchemaError> {
        if self.description.trim().is_empty()
            || self.description.len() > 512
            || self.description.chars().any(char::is_control)
        {
            return Err(AgentSchemaError::InvalidDescription);
        }
        if self.composed_prompt.trim().is_empty()
            || self.composed_prompt.len() > Self::MAX_PROMPT_BYTES
        {
            return Err(AgentSchemaError::InvalidPrompt);
        }
        if self.permissions.len() > 256
            || self.fallback_chain.is_empty()
            || self.fallback_chain.len() > 256
        {
            return Err(AgentSchemaError::InvalidListBounds);
        }
        let mut models = BTreeSet::<ModelKey>::new();
        if !self
            .fallback_chain
            .iter()
            .all(|binding| models.insert(binding.selection.model.clone()))
        {
            return Err(AgentSchemaError::DuplicateFallbackModel);
        }
        for binding in &self.fallback_chain {
            binding
                .validate()
                .map_err(|_| AgentSchemaError::InvalidModelBinding)?;
        }
        if self.selected_suffix_start as usize >= self.fallback_chain.len() {
            return Err(AgentSchemaError::InvalidSuffixStart);
        }
        if let Some(delegation) = &self.delegation {
            delegation.validate()?;
        }
        Ok(())
    }

    /// Checks that `selection` is the run this snapshot was frozen for: its
    /// agent, and exactly the fallback entry the run starts at.
    pub fn validate_selection(&self, selection: &RunSelection) -> Result<(), AgentSchemaError> {
        self.validate()?;
        if selection.agent != self.agent || self.selected_suffix()[0].selection != selection.model {
            return Err(AgentSchemaError::SelectionMismatch);
        }
        Ok(())
    }

    /// The run's model and its fallbacks, in order. Nonempty for a valid
    /// snapshot.
    #[must_use]
    pub fn selected_suffix(&self) -> &[PersistedModelBinding] {
        self.fallback_chain
            .get(self.selected_suffix_start as usize..)
            .unwrap_or_default()
    }
}
impl<'de> Deserialize<'de> for AgentSnapshot {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            agent: AgentId,
            mode: AgentMode,
            description: String,
            document_source: AgentDocumentSource,
            document_fingerprint: Sha256Digest,
            composed_prompt: String,
            prompt_fingerprint: Sha256Digest,
            max_output_tokens: u64,
            permissions: Vec<PermissionRule>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            delegation: Option<FrozenDelegationPolicy>,
            fallback_chain: Vec<PersistedModelBinding>,
            selected_suffix_start: u32,
        }

        let w = Wire::deserialize(d)?;
        let value = Self {
            agent: w.agent,
            mode: w.mode,
            description: w.description,
            document_source: w.document_source,
            document_fingerprint: w.document_fingerprint,
            composed_prompt: w.composed_prompt,
            prompt_fingerprint: w.prompt_fingerprint,
            max_output_tokens: w.max_output_tokens,
            permissions: w.permissions,
            delegation: w.delegation,
            fallback_chain: w.fallback_chain,
            selected_suffix_start: w.selected_suffix_start,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RunSelection {
    pub agent: AgentId,
    #[serde(deserialize_with = "crate::deserialize_required_model_selection")]
    #[schemars(with = "crate::RequiredModelSelectionSchema")]
    pub model: ModelSelection,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_preset",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<AgentPresetNameSchema>")]
    pub preset: Option<String>,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDescriptor {
    pub id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<AgentPresetNameSchema>")]
    pub preset: Option<String>,
    #[schemars(with = "AgentDescriptionSchema")]
    pub description: String,
    pub mode: AgentMode,
    pub enabled: bool,
    pub runnable_as_root: bool,
    #[schemars(with = "Vec<crate::RequiredModelSelectionSchema>")]
    #[schemars(length(max = 256))]
    pub resolved_fallback: Vec<ModelSelection>,
    #[schemars(length(max = 256))]
    pub delegation_targets: Vec<AgentId>,
}

struct AgentDescriptionSchema;
struct AgentPresetNameSchema;
struct AgentPromptSchema;
impl JsonSchema for AgentDescriptionSchema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("AgentDescription")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":512,"pattern":"^(?=.*\\S)[^\\p{Cc}\\p{Cf}]+$"})
    }
}
impl JsonSchema for AgentPresetNameSchema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("AgentPresetName")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":64,"pattern":"^[a-z0-9]+(?:-[a-z0-9]+)*$"})
    }
}
impl JsonSchema for AgentPromptSchema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("AgentPrompt")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":131072,"pattern":"(?s)^(?=.*\\S).+$"})
    }
}
impl AgentDescriptor {
    pub fn validate(&self) -> Result<(), AgentSchemaError> {
        if self
            .preset
            .as_deref()
            .is_some_and(|name| !valid_preset_name(name))
        {
            return Err(AgentSchemaError::InvalidPreset);
        }
        if self.description.trim().is_empty()
            || self.description.len() > 512
            || self.description.chars().any(char::is_control)
        {
            return Err(AgentSchemaError::InvalidDescription);
        }
        if self.resolved_fallback.len() > 256 || self.delegation_targets.len() > 256 {
            return Err(AgentSchemaError::InvalidListBounds);
        }
        let mut models = BTreeSet::new();
        if !self
            .resolved_fallback
            .iter()
            .all(|selection| models.insert(selection.model.clone()))
        {
            return Err(AgentSchemaError::DuplicateFallbackModel);
        }
        if self
            .delegation_targets
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(AgentSchemaError::InvalidDelegationTargets);
        }
        if self.runnable_as_root
            && (!self.enabled
                || matches!(self.mode, AgentMode::Subagent | AgentMode::Internal)
                || self.resolved_fallback.is_empty())
        {
            return Err(AgentSchemaError::InvalidRootRunnable);
        }
        Ok(())
    }
}

fn valid_preset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn deserialize_optional_preset<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let preset = Option::<String>::deserialize(deserializer)?;
    if preset
        .as_deref()
        .is_some_and(|name| !valid_preset_name(name))
    {
        return Err(serde::de::Error::custom(AgentSchemaError::InvalidPreset));
    }
    Ok(preset)
}
impl<'de> Deserialize<'de> for AgentDescriptor {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            id: AgentId,
            #[serde(default)]
            preset: Option<String>,
            description: String,
            mode: AgentMode,
            enabled: bool,
            runnable_as_root: bool,
            #[serde(deserialize_with = "crate::deserialize_required_model_selections")]
            resolved_fallback: Vec<ModelSelection>,
            delegation_targets: Vec<AgentId>,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            id: w.id,
            preset: w.preset,
            description: w.description,
            mode: w.mode,
            enabled: w.enabled,
            runnable_as_root: w.runnable_as_root,
            resolved_fallback: w.resolved_fallback,
            delegation_targets: w.delegation_targets,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentSchemaError {
    InvalidDelegationTargets,
    InvalidDescription,
    InvalidPreset,
    InvalidPrompt,
    InvalidListBounds,
    DuplicatePermissionRule,
    DuplicateFallbackModel,
    InvalidModelBinding,
    InvalidSuffixStart,
    SelectionMismatch,
    InvalidRootRunnable,
}
impl fmt::Display for AgentSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self{Self::InvalidDelegationTargets=>"delegation targets must be a nonempty, strictly sorted unique list of at most 256 IDs",Self::InvalidDescription=>"agent description must be nonblank, control-free, and 1..=512 bytes",Self::InvalidPreset=>"agent preset must use lowercase alphanumeric segments separated by hyphens and contain at most 64 bytes",Self::InvalidPrompt=>"composed prompt must be nonblank and at most 128 KiB",Self::InvalidListBounds=>"agent list exceeds bounds or frozen fallback is empty",Self::DuplicatePermissionRule=>"session permission overlay rules must be unique by action and resource",Self::DuplicateFallbackModel=>"fallback chains may contain each model key at most once",Self::InvalidModelBinding=>"agent snapshot contains an invalid frozen model binding",Self::InvalidSuffixStart=>"selected_suffix_start must index the frozen fallback chain",Self::SelectionMismatch=>"run selection does not match the snapshot agent and selected fallback start",Self::InvalidRootRunnable=>"runnable_as_root contradicts enabled mode or fallback state"})
    }
}
impl std::error::Error for AgentSchemaError {}
