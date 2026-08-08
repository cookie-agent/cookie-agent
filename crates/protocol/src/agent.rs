use std::{borrow::Cow, collections::BTreeSet, fmt};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{AgentId, FrozenModelBinding, ModelKey, ModelSelection, Sha256Digest, WildcardPattern};

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, TS)]
#[ts(type = "3")]
pub struct AgentSchemaVersion(());
impl AgentSchemaVersion {
    #[must_use]
    pub const fn current() -> Self {
        Self(())
    }
    #[must_use]
    pub const fn value(self) -> u32 {
        3
    }
}
impl Serialize for AgentSchemaVersion {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_u32(3)
    }
}
impl<'de> Deserialize<'de> for AgentSchemaVersion {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = u32::deserialize(d)?;
        if v == 3 {
            Ok(Self::current())
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported exact agent schema {v}; expected 3"
            )))
        }
    }
}
impl JsonSchema for AgentSchemaVersion {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("AgentSchemaVersion")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"integer","const":3})
    }
}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    Primary,
    Subagent,
    All,
    Internal,
}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    Read,
    Write,
    Edit,
    Bash,
    Grep,
    Glob,
}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    Read,
    Write,
    Bash,
    Grep,
    Glob,
    Delegate,
    ExternalDirectory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PermissionEffect {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PermissionRule {
    pub action: PermissionAction,
    pub resource: WildcardPattern,
    pub effect: PermissionEffect,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum AgentDocumentSource {
    BuiltIn,
    User,
    Workspace,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct FrozenDelegationPolicy {
    #[schemars(length(min = 1, max = 256))]
    #[ts(type = "Array<AgentId>")]
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
        #[serde(deny_unknown_fields)]
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

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AgentSnapshot {
    #[ts(type = "AgentId")]
    pub agent: AgentId,
    pub schema: AgentSchemaVersion,
    pub mode: AgentMode,
    #[schemars(with = "AgentDescriptionSchema")]
    pub description: String,
    pub document_source: AgentDocumentSource,
    pub document_fingerprint: Sha256Digest,
    #[schemars(with = "AgentPromptSchema")]
    pub composed_prompt: String,
    pub prompt_fingerprint: Sha256Digest,
    #[schemars(length(max = 256))]
    pub tools: Vec<ToolName>,
    #[schemars(length(max = 256))]
    pub permissions: Vec<PermissionRule>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<FrozenDelegationPolicy>", required)]
    pub delegation: Option<FrozenDelegationPolicy>,
    #[schemars(length(min = 1, max = 256))]
    pub fallback_chain: Vec<FrozenModelBinding>,
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
        if self.tools.len() > 256
            || self.permissions.len() > 256
            || self.fallback_chain.is_empty()
            || self.fallback_chain.len() > 256
        {
            return Err(AgentSchemaError::InvalidListBounds);
        }
        let mut tools = BTreeSet::new();
        if !self.tools.iter().all(|tool| tools.insert(*tool)) {
            return Err(AgentSchemaError::DuplicateTool);
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

    pub fn validate_selection(&self, selection: &RunSelection) -> Result<(), AgentSchemaError> {
        self.validate()?;
        let start = self.selected_suffix_start as usize;
        if selection.agent != self.agent
            || self.fallback_chain[start].selection.model != selection.model.model
        {
            return Err(AgentSchemaError::SelectionMismatch);
        }
        Ok(())
    }

    pub fn validate_selected_suffix(
        &self,
        selection: &RunSelection,
        selected_suffix: &[FrozenModelBinding],
    ) -> Result<(), AgentSchemaError> {
        self.validate_selection(selection)?;
        let start = self.selected_suffix_start as usize;
        let expected = &self.fallback_chain[start..];
        if selected_suffix.len() != expected.len() || selected_suffix.is_empty() {
            return Err(AgentSchemaError::SelectedSuffixMismatch);
        }
        let selected_head = &selected_suffix[0];
        let fallback_head = &expected[0];
        if selected_head.selection != selection.model
            || selected_head.descriptor != fallback_head.descriptor
        {
            return Err(AgentSchemaError::SelectedSuffixMismatch);
        }
        if selection.model.variant == fallback_head.selection.variant
            && selected_head != fallback_head
        {
            return Err(AgentSchemaError::SelectedSuffixMismatch);
        }
        if selected_suffix[1..] != expected[1..] {
            return Err(AgentSchemaError::SelectedSuffixMismatch);
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for AgentSnapshot {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            agent: AgentId,
            schema: AgentSchemaVersion,
            mode: AgentMode,
            description: String,
            document_source: AgentDocumentSource,
            document_fingerprint: Sha256Digest,
            composed_prompt: String,
            prompt_fingerprint: Sha256Digest,
            tools: Vec<ToolName>,
            permissions: Vec<PermissionRule>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            delegation: Option<FrozenDelegationPolicy>,
            fallback_chain: Vec<FrozenModelBinding>,
            selected_suffix_start: u32,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            agent: w.agent,
            schema: w.schema,
            mode: w.mode,
            description: w.description,
            document_source: w.document_source,
            document_fingerprint: w.document_fingerprint,
            composed_prompt: w.composed_prompt,
            prompt_fingerprint: w.prompt_fingerprint,
            tools: w.tools,
            permissions: w.permissions,
            delegation: w.delegation,
            fallback_chain: w.fallback_chain,
            selected_suffix_start: w.selected_suffix_start,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunSelection {
    #[ts(type = "AgentId")]
    pub agent: AgentId,
    #[serde(deserialize_with = "crate::deserialize_required_model_selection")]
    #[schemars(with = "crate::RequiredModelSelectionSchema")]
    #[ts(type = "ModelSelection")]
    pub model: ModelSelection,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AgentDescriptor {
    #[ts(type = "AgentId")]
    pub id: AgentId,
    #[schemars(with = "AgentDescriptionSchema")]
    pub description: String,
    pub mode: AgentMode,
    pub enabled: bool,
    pub runnable_as_root: bool,
    #[schemars(with = "Vec<crate::RequiredModelSelectionSchema>")]
    #[schemars(length(max = 256))]
    #[ts(type = "Array<ModelSelection>")]
    pub resolved_fallback: Vec<ModelSelection>,
    #[schemars(length(max = 256))]
    pub tools: Vec<ToolName>,
    #[schemars(length(max = 256))]
    #[ts(type = "Array<AgentId>")]
    pub delegation_targets: Vec<AgentId>,
}

struct AgentDescriptionSchema;
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
        if self.description.trim().is_empty()
            || self.description.len() > 512
            || self.description.chars().any(char::is_control)
        {
            return Err(AgentSchemaError::InvalidDescription);
        }
        if self.resolved_fallback.len() > 256
            || self.tools.len() > 256
            || self.delegation_targets.len() > 256
        {
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
impl<'de> Deserialize<'de> for AgentDescriptor {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            id: AgentId,
            description: String,
            mode: AgentMode,
            enabled: bool,
            runnable_as_root: bool,
            #[serde(deserialize_with = "crate::deserialize_required_model_selections")]
            resolved_fallback: Vec<ModelSelection>,
            tools: Vec<ToolName>,
            delegation_targets: Vec<AgentId>,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            id: w.id,
            description: w.description,
            mode: w.mode,
            enabled: w.enabled,
            runnable_as_root: w.runnable_as_root,
            resolved_fallback: w.resolved_fallback,
            tools: w.tools,
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
    InvalidPrompt,
    InvalidListBounds,
    DuplicateTool,
    DuplicateFallbackModel,
    InvalidModelBinding,
    InvalidSuffixStart,
    SelectionMismatch,
    SelectedSuffixMismatch,
    InvalidRootRunnable,
}
impl fmt::Display for AgentSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self{Self::InvalidDelegationTargets=>"delegation targets must be a nonempty, strictly sorted unique list of at most 256 IDs",Self::InvalidDescription=>"agent description must be nonblank, control-free, and 1..=512 bytes",Self::InvalidPrompt=>"composed prompt must be nonblank and at most 128 KiB",Self::InvalidListBounds=>"agent list exceeds bounds or frozen fallback is empty",Self::DuplicateTool=>"agent tools must be unique",Self::DuplicateFallbackModel=>"fallback chains may contain each model key at most once",Self::InvalidModelBinding=>"agent snapshot contains an invalid frozen model binding",Self::InvalidSuffixStart=>"selected_suffix_start must index the frozen fallback chain",Self::SelectionMismatch=>"run selection does not match the snapshot agent and selected fallback start",Self::SelectedSuffixMismatch=>"selected suffix does not exactly match the frozen fallback order and selected head",Self::InvalidRootRunnable=>"runnable_as_root contradicts enabled mode or fallback state"})
    }
}
impl std::error::Error for AgentSchemaError {}
