use std::{borrow::Cow, collections::BTreeMap, fmt};

use jiff::Timestamp;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_path_to_error::Segment;
use ts_rs::TS;

use crate::*;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct SessionTitle(String);
impl SessionTitle {
    pub const MAX_BYTES: usize = 512;
    pub fn new(value: impl Into<String>) -> Result<Self, EventSchemaError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(EventSchemaError::EmptyTitle);
        }
        if value.len() > Self::MAX_BYTES {
            return Err(EventSchemaError::TitleTooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(EventSchemaError::TitleControlCharacter);
        }
        Ok(Self(value))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for SessionTitle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl Serialize for SessionTitle {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for SessionTitle {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for SessionTitle {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SessionTitle")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":512,"description":"Nonblank control-free UTF-8 session title."})
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct EventOrigin(String);
impl EventOrigin {
    pub fn new(value: impl Into<String>) -> Result<Self, EventSchemaError> {
        let value = value.into();
        if value == "user" {
            return Ok(Self(value));
        }
        let Some((class, slug)) = value.split_once(':') else {
            return Err(EventSchemaError::InvalidOrigin);
        };
        if !matches!(class, "engine" | "plugin" | "client")
            || slug.is_empty()
            || slug.len() > 64
            || !slug.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && byte == b'-')
            })
        {
            return Err(EventSchemaError::InvalidOrigin);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn plugin_name(&self) -> Option<&str> {
        self.0.strip_prefix("plugin:")
    }
}
impl fmt::Display for EventOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl Serialize for EventOrigin {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for EventOrigin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for EventOrigin {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("EventOrigin")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type":"string",
            "pattern":"^(user|(engine|plugin|client):[a-z0-9][a-z0-9-]{0,63})$",
            "description":"Validated event author origin."
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionOrigin {
    Root,
    Delegated {
        root_session_id: SessionId,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        invocation_id: InvocationId,
        depth: u32,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DelegationReservation {
    pub invocation_id: InvocationId,
    pub parent_session_id: SessionId,
    pub parent_run_id: RunId,
    pub parent_tool_call_id: ToolCallId,
    pub child_session_id: SessionId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum DelegatedContextRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DelegatedContextTurn {
    pub role: DelegatedContextRole,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum StagedSkillProvenance {
    SkillFork,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct StagedSkillPayload {
    pub provenance: StagedSkillProvenance,
    pub name: String,
    pub args: String,
    pub rendered_body: String,
    pub source_path: String,
    pub base_dir: String,
    #[schemars(length(max = 10))]
    pub supporting_files: Vec<String>,
    #[schemars(length(max = 256))]
    pub grants: Vec<PermissionRule>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ModelKey>", required)]
    #[ts(type = "ModelKey | null")]
    pub model: Option<ModelKey>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DelegateRequestPayload {
    pub description: String,
    pub prompt: String,
    pub title: SessionTitle,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SessionId>", required)]
    pub resume_session_id: Option<SessionId>,
    pub inherit_context: bool,
    #[schemars(length(max = 65536))]
    pub seeded_context: Vec<DelegatedContextTurn>,
    pub background: bool,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<StagedSkillPayload>", required)]
    pub staged_skill: Option<StagedSkillPayload>,
}
impl DelegateRequestPayload {
    fn validate(&self) -> Result<(), EventSchemaError> {
        if self.description.is_empty()
            || self.prompt.is_empty()
            || (self.resume_session_id.is_some() && self.inherit_context)
            || (!self.inherit_context && !self.seeded_context.is_empty())
            || self.seeded_context.len() > 65_536
            || self.seeded_context.iter().any(|turn| turn.text.is_empty())
            || self
                .seeded_context
                .iter()
                .map(|turn| turn.text.len())
                .sum::<usize>()
                > 65_536
        {
            return Err(EventSchemaError::InvalidDelegationLifecycle);
        }
        if self.staged_skill.as_ref().is_some_and(|skill| {
            skill.provenance != StagedSkillProvenance::SkillFork
                || skill.name.is_empty()
                || skill.rendered_body.is_empty()
                || skill.source_path.is_empty()
                || skill.base_dir.is_empty()
                || skill.supporting_files.len() > 10
                || skill.grants.len() > 256
                || self.resume_session_id.is_some()
        }) {
            return Err(EventSchemaError::InvalidDelegationLifecycle);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionMeta {
    pub session_id: SessionId,
    pub origin: SessionOrigin,
    pub cwd_identity: CwdIdentity,
    pub creation_selection: RunSelection,
    #[ts(type = "RuntimeRevision")]
    pub runtime_revision: RuntimeRevision,
    #[ts(type = "CatalogRevision")]
    pub catalog_revision: CatalogRevision,
    #[ts(type = "ProviderStateRevision")]
    pub provider_state_revision: ProviderStateRevision,
    #[ts(type = "ModelRevision")]
    pub model_revision: ModelRevision,
    #[ts(type = "AgentRevision")]
    pub agent_revision: AgentRevision,
    #[ts(type = "RecipeRegistryRevision")]
    pub recipe_registry_revision: RecipeRegistryRevision,
    #[ts(type = "ModelSnapshotRevision")]
    pub manifest_revision: ModelSnapshotRevision,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SessionTitle>", required)]
    pub title: Option<SessionTitle>,
    pub title_updated_seq: u64,
    #[schemars(range(min = 1))]
    pub last_event_seq: u64,
    pub last_activity: Timestamp,
    pub status: SessionStatus,
    pub skipped_events: Vec<SkippedEvent>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct SkippedEvent {
    pub seq: u64,
    pub reason: String,
}
impl SessionMeta {
    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if self.last_event_seq == 0 || self.title_updated_seq > self.last_event_seq {
            return Err(EventSchemaError::InvalidSessionMetaSequence);
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for SessionMeta {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            session_id: SessionId,
            origin: SessionOrigin,
            cwd_identity: CwdIdentity,
            creation_selection: RunSelection,
            runtime_revision: RuntimeRevision,
            catalog_revision: CatalogRevision,
            provider_state_revision: ProviderStateRevision,
            model_revision: ModelRevision,
            agent_revision: AgentRevision,
            recipe_registry_revision: RecipeRegistryRevision,
            manifest_revision: ModelSnapshotRevision,
            #[serde(deserialize_with = "deserialize_required_option")]
            title: Option<SessionTitle>,
            title_updated_seq: u64,
            last_event_seq: u64,
            last_activity: Timestamp,
            status: SessionStatus,
            #[serde(default)]
            skipped_events: Vec<SkippedEvent>,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            session_id: w.session_id,
            origin: w.origin,
            cwd_identity: w.cwd_identity,
            creation_selection: w.creation_selection,
            runtime_revision: w.runtime_revision,
            catalog_revision: w.catalog_revision,
            provider_state_revision: w.provider_state_revision,
            model_revision: w.model_revision,
            agent_revision: w.agent_revision,
            recipe_registry_revision: w.recipe_registry_revision,
            manifest_revision: w.manifest_revision,
            title: w.title,
            title_updated_seq: w.title_updated_seq,
            last_event_seq: w.last_event_seq,
            last_activity: w.last_activity,
            status: w.status,
            skipped_events: w.skipped_events,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionTitleChange {
    UserSet {
        title: SessionTitle,
        client_rename_id: ClientRenameId,
    },
    UserClear {
        client_rename_id: ClientRenameId,
    },
    UserReset {
        client_rename_id: ClientRenameId,
    },
    InternalAgentSet {
        title: SessionTitle,
        invocation_id: InternalAgentInvocationId,
    },
    DelegatedSet {
        title: SessionTitle,
        invocation_id: InvocationId,
    },
    FallbackSet {
        title: SessionTitle,
    },
}
impl SessionTitleChange {
    #[must_use]
    pub fn user_rename_record(&self) -> Option<SessionRenameRecord> {
        match self {
            Self::UserSet {
                title,
                client_rename_id,
            } => Some(SessionRenameRecord {
                client_rename_id: client_rename_id.clone(),
                change: SessionRenameChange::Set {
                    title: title.clone(),
                },
            }),
            Self::UserClear { client_rename_id } => Some(SessionRenameRecord {
                client_rename_id: client_rename_id.clone(),
                change: SessionRenameChange::Clear,
            }),
            Self::UserReset { client_rename_id } => Some(SessionRenameRecord {
                client_rename_id: client_rename_id.clone(),
                change: SessionRenameChange::Reset,
            }),
            Self::InternalAgentSet { .. }
            | Self::DelegatedSet { .. }
            | Self::FallbackSet { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReference {
    pub uri: String,
}
impl ArtifactReference {
    pub const MAX_URI_BYTES: usize = 1024;
    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if self.uri.is_empty()
            || self.uri.len() > Self::MAX_URI_BYTES
            || self.uri.chars().any(char::is_control)
            || !self.uri.starts_with("artifact://")
        {
            Err(EventSchemaError::InvalidArtifactReference)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ToolAttachment {
    pub mime_type: MimeType,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub filename: Option<String>,
    pub byte_length: u64,
    pub sha256: Sha256Digest,
    pub reference: ArtifactReference,
}
impl ToolAttachment {
    pub fn validate(&self) -> Result<(), EventSchemaError> {
        self.reference.validate()?;
        if self.filename.as_ref().is_some_and(|name| {
            name.is_empty() || name.len() > 512 || name.chars().any(char::is_control)
        }) {
            return Err(EventSchemaError::InvalidAttachmentFilename);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ToolEmittedMessageRole {
    System,
    User,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolEmittedContent {
    Text(String),
    File(ToolAttachment),
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ToolEmittedMessage {
    pub role: ToolEmittedMessageRole,
    #[schemars(length(min = 1))]
    pub content: Vec<ToolEmittedContent>,
}
impl ToolEmittedMessage {
    pub const MAX_TEXT_BYTES: usize = 64 * 1024;

    pub fn new(
        role: ToolEmittedMessageRole,
        content: Vec<ToolEmittedContent>,
    ) -> Result<Self, EventSchemaError> {
        let value = Self { role, content };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if self.content.is_empty() {
            return Err(EventSchemaError::EmptyToolEmittedMessage);
        }
        let mut text_bytes = 0_usize;
        for part in &self.content {
            match part {
                ToolEmittedContent::Text(text) => {
                    if text.trim().is_empty() {
                        return Err(EventSchemaError::EmptyToolEmittedText);
                    }
                    text_bytes = text_bytes
                        .checked_add(text.len())
                        .ok_or(EventSchemaError::ToolEmittedTextTooLarge)?;
                    if text_bytes > Self::MAX_TEXT_BYTES {
                        return Err(EventSchemaError::ToolEmittedTextTooLarge);
                    }
                }
                ToolEmittedContent::File(attachment) => attachment.validate()?,
            }
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for ToolEmittedMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            role: ToolEmittedMessageRole,
            content: Vec<ToolEmittedContent>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.role, wire.content).map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputTruncation {
    pub original_bytes: u64,
    pub original_lines: u64,
    pub retained: ArtifactReference,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PersistedToolResult {
    pub title: SafeDisplayText,
    pub output: String,
    pub metadata: Value,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ToolOutputTruncation>", required)]
    pub truncation: Option<ToolOutputTruncation>,
    #[schemars(
        description = "Direct result files. Together with files in additional_messages, at most 256 are allowed."
    )]
    #[schemars(length(max = 256))]
    pub attachments: Vec<ToolAttachment>,
    #[schemars(
        description = "Emitted model-history messages. Their files share the 256-file limit with attachments."
    )]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 4))]
    pub additional_messages: Vec<ToolEmittedMessage>,
}
impl PersistedToolResult {
    pub const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
    pub const MAX_METADATA_BYTES: usize = 2 * 1024 * 1024;
    pub const MAX_ADDITIONAL_MESSAGES: usize = 4;
    pub const MAX_ATTACHMENTS: usize = 256;

    pub fn all_attachments(&self) -> impl Iterator<Item = &ToolAttachment> {
        self.attachments.iter().chain(
            self.additional_messages
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(|part| match part {
                    ToolEmittedContent::Text(_) => None,
                    ToolEmittedContent::File(attachment) => Some(attachment),
                }),
        )
    }

    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if self.output.len() > Self::MAX_OUTPUT_BYTES {
            return Err(EventSchemaError::ToolOutputTooLarge);
        }
        if serde_json::to_vec(&self.metadata)
            .map_err(|_| EventSchemaError::InvalidJson)?
            .len()
            > Self::MAX_METADATA_BYTES
        {
            return Err(EventSchemaError::ToolMetadataTooLarge);
        }
        let emitted_attachment_count = self
            .additional_messages
            .iter()
            .flat_map(|message| &message.content)
            .filter(|part| matches!(part, ToolEmittedContent::File(_)))
            .count();
        if self
            .attachments
            .len()
            .saturating_add(emitted_attachment_count)
            > Self::MAX_ATTACHMENTS
        {
            return Err(EventSchemaError::TooManyAttachments);
        }
        if self.additional_messages.len() > Self::MAX_ADDITIONAL_MESSAGES {
            return Err(EventSchemaError::TooManyToolEmittedMessages);
        }
        for message in &self.additional_messages {
            message.validate()?;
        }
        for attachment in self.all_attachments() {
            attachment.validate()?;
        }
        if let Some(truncation) = &self.truncation {
            truncation.retained.validate()?;
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for PersistedToolResult {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            title: SafeDisplayText,
            output: String,
            metadata: Value,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            truncation: Option<ToolOutputTruncation>,
            attachments: Vec<ToolAttachment>,
            #[serde(default)]
            additional_messages: Vec<ToolEmittedMessage>,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            title: w.title,
            output: w.output,
            metadata: w.metadata,
            truncation: w.truncation,
            attachments: w.attachments,
            additional_messages: w.additional_messages,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SafeToolError {
    pub code: SafeCode,
    pub message: SafeErrorMessage,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AssistantToolCallRef {
    #[schemars(range(min = 1))]
    pub model_turn_seq: u64,
    pub content_index: u32,
    pub model_call_id: ModelCallId,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ProviderItemId>", required)]
    pub provider_item_id: Option<ProviderItemId>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ToolCallPresentation {
    pub title: SafeDisplayText,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeDisplayText>", required)]
    pub primary_argument: Option<SafeDisplayText>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ToolCallStart {
    pub tool_call_id: ToolCallId,
    pub owner: AssistantToolCallRef,
    pub presentation: ToolCallPresentation,
    pub operation_fingerprint: OperationFingerprint,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ToolTerminationOutcome {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ToolCallTermination {
    pub tool_call_id: ToolCallId,
    pub owner: AssistantToolCallRef,
    pub outcome: ToolTerminationOutcome,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<PersistedToolResult>", required)]
    pub result: Option<PersistedToolResult>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeToolError>", required)]
    pub error: Option<SafeToolError>,
}
impl ToolCallTermination {
    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if self.owner.model_turn_seq == 0 {
            return Err(EventSchemaError::ZeroModelTurnSequence);
        }
        match self.outcome {
            ToolTerminationOutcome::Completed if self.result.is_some() && self.error.is_none() => {}
            ToolTerminationOutcome::Failed
            | ToolTerminationOutcome::Cancelled
            | ToolTerminationOutcome::Interrupted
                if self.error.is_some() => {}
            _ => return Err(EventSchemaError::InvalidToolTermination),
        }
        if let Some(result) = &self.result {
            result.validate()?;
        }
        Ok(())
    }
    #[must_use]
    pub fn matches_start(&self, start: &ToolCallStart) -> bool {
        self.tool_call_id == start.tool_call_id && self.owner == start.owner
    }
}
impl<'de> Deserialize<'de> for ToolCallTermination {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            tool_call_id: ToolCallId,
            owner: AssistantToolCallRef,
            outcome: ToolTerminationOutcome,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            result: Option<PersistedToolResult>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            error: Option<SafeToolError>,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            tool_call_id: w.tool_call_id,
            owner: w.owner,
            outcome: w.outcome,
            result: w.result,
            error: w.error,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u64>", required)]
    pub input_tokens: Option<u64>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u64>", required)]
    pub input_tokens_no_cache: Option<u64>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u64>", required)]
    pub input_tokens_cache_read: Option<u64>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u64>", required)]
    pub input_tokens_cache_write: Option<u64>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u64>", required)]
    pub output_tokens: Option<u64>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u64>", required)]
    pub output_tokens_text: Option<u64>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u64>", required)]
    pub output_tokens_reasoning: Option<u64>,
}

pub type SafeModelMetadata = BTreeMap<String, Value>;

#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct NativeContextScope {
    #[ts(type = "ProviderId")]
    pub provider_id: ProviderId,
    #[ts(type = "ProviderModelId")]
    pub model_id: ProviderModelId,
    pub resource_id: SafeDisplayText,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NativeArtifactError {
    PayloadTooLarge { size: usize, maximum: usize },
    InvalidScope,
    Serialization,
}
impl fmt::Display for NativeArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge { size, maximum } => write!(
                f,
                "native payload is {size} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::InvalidScope => f.write_str("native artifact scope does not match its selection"),
            Self::Serialization => f.write_str("native payload could not be serialized"),
        }
    }
}
impl std::error::Error for NativeArtifactError {}

#[derive(Clone, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct NativeReplayArtifact {
    adapter_id: SafeCode,
    selection_fingerprint: Sha256Digest,
    scope: NativeContextScope,
    #[schemars(extend("x-max-serialized-bytes"=2_097_152))]
    payload: Value,
}
impl NativeReplayArtifact {
    pub const MAX_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
    pub fn new(
        adapter_id: SafeCode,
        selection_fingerprint: Sha256Digest,
        scope: NativeContextScope,
        payload: Value,
    ) -> Result<Self, NativeArtifactError> {
        validate_payload(&payload, Self::MAX_PAYLOAD_BYTES)?;
        Ok(Self {
            adapter_id,
            selection_fingerprint,
            scope,
            payload,
        })
    }
    #[must_use]
    pub const fn selection_fingerprint(&self) -> &Sha256Digest {
        &self.selection_fingerprint
    }
    #[must_use]
    pub const fn adapter_id(&self) -> &SafeCode {
        &self.adapter_id
    }
    #[must_use]
    pub const fn scope(&self) -> &NativeContextScope {
        &self.scope
    }
    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
    }

    pub fn validate_for(
        &self,
        resolved: &ResolvedModelRef,
        expected_scope: &NativeContextScope,
    ) -> Result<(), NativeArtifactError> {
        validate_native_identity(
            &self.adapter_id,
            &self.selection_fingerprint,
            &self.scope,
            resolved,
            expected_scope,
        )
    }

    pub fn validate_resolved(
        &self,
        resolved: &ResolvedModelRef,
    ) -> Result<(), NativeArtifactError> {
        self.validate_for(resolved, &self.scope)
    }

    pub fn validate_for_binding(
        &self,
        binding: &FrozenModelBinding,
        expected_scope: &NativeContextScope,
    ) -> Result<(), NativeArtifactError> {
        binding
            .validate()
            .map_err(|_| NativeArtifactError::InvalidScope)?;
        if self.adapter_id.as_str() != binding.descriptor.adapter_id.as_str()
            || self.selection_fingerprint != binding.blueprint_fingerprint
            || self.scope != *expected_scope
            || self.scope.provider_id != binding.selection.model.provider_id()
            || self.scope.model_id != binding.selection.model.model_id()
        {
            return Err(NativeArtifactError::InvalidScope);
        }
        Ok(())
    }
}
impl fmt::Debug for NativeReplayArtifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeReplayArtifact")
            .field("adapter_id", &self.adapter_id)
            .field("selection_fingerprint", &self.selection_fingerprint)
            .field("scope", &self.scope)
            .field("payload", &"<redacted>")
            .finish()
    }
}
impl<'de> Deserialize<'de> for NativeReplayArtifact {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            adapter_id: SafeCode,
            selection_fingerprint: Sha256Digest,
            scope: NativeContextScope,
            payload: Value,
        }
        let w = Wire::deserialize(d)?;
        Self::new(w.adapter_id, w.selection_fingerprint, w.scope, w.payload)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct NativeContextWindow {
    adapter_id: SafeCode,
    selection_fingerprint: Sha256Digest,
    scope: NativeContextScope,
    #[schemars(extend("x-max-serialized-bytes"=33_554_432))]
    payload: Value,
}
impl NativeContextWindow {
    pub const MAX_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;
    pub fn new(
        adapter_id: SafeCode,
        selection_fingerprint: Sha256Digest,
        scope: NativeContextScope,
        payload: Value,
    ) -> Result<Self, NativeArtifactError> {
        validate_payload(&payload, Self::MAX_PAYLOAD_BYTES)?;
        Ok(Self {
            adapter_id,
            selection_fingerprint,
            scope,
            payload,
        })
    }
    #[must_use]
    pub const fn selection_fingerprint(&self) -> &Sha256Digest {
        &self.selection_fingerprint
    }
    #[must_use]
    pub const fn adapter_id(&self) -> &SafeCode {
        &self.adapter_id
    }
    #[must_use]
    pub const fn scope(&self) -> &NativeContextScope {
        &self.scope
    }
    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
    }

    pub fn validate_for(
        &self,
        resolved: &ResolvedModelRef,
        expected_scope: &NativeContextScope,
    ) -> Result<(), NativeArtifactError> {
        validate_native_identity(
            &self.adapter_id,
            &self.selection_fingerprint,
            &self.scope,
            resolved,
            expected_scope,
        )
    }

    pub fn validate_for_binding(
        &self,
        binding: &FrozenModelBinding,
        expected_scope: &NativeContextScope,
    ) -> Result<(), NativeArtifactError> {
        binding
            .validate()
            .map_err(|_| NativeArtifactError::InvalidScope)?;
        if self.adapter_id.as_str() != binding.descriptor.adapter_id.as_str()
            || self.selection_fingerprint != binding.blueprint_fingerprint
            || self.scope != *expected_scope
            || self.scope.provider_id != binding.selection.model.provider_id()
            || self.scope.model_id != binding.selection.model.model_id()
        {
            return Err(NativeArtifactError::InvalidScope);
        }
        Ok(())
    }
}
impl fmt::Debug for NativeContextWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeContextWindow")
            .field("adapter_id", &self.adapter_id)
            .field("selection_fingerprint", &self.selection_fingerprint)
            .field("scope", &self.scope)
            .field("payload", &"<redacted>")
            .finish()
    }
}
impl<'de> Deserialize<'de> for NativeContextWindow {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            adapter_id: SafeCode,
            selection_fingerprint: Sha256Digest,
            scope: NativeContextScope,
            payload: Value,
        }
        let w = Wire::deserialize(d)?;
        Self::new(w.adapter_id, w.selection_fingerprint, w.scope, w.payload)
            .map_err(serde::de::Error::custom)
    }
}
fn validate_payload(payload: &Value, maximum: usize) -> Result<(), NativeArtifactError> {
    let size = serde_json::to_vec(payload)
        .map_err(|_| NativeArtifactError::Serialization)?
        .len();
    if size > maximum {
        Err(NativeArtifactError::PayloadTooLarge { size, maximum })
    } else {
        Ok(())
    }
}

fn validate_native_identity(
    _adapter_id: &SafeCode,
    selection_fingerprint: &Sha256Digest,
    scope: &NativeContextScope,
    resolved: &ResolvedModelRef,
    expected_scope: &NativeContextScope,
) -> Result<(), NativeArtifactError> {
    resolved
        .validate()
        .map_err(|_| NativeArtifactError::InvalidScope)?;
    if selection_fingerprint != &resolved.selection_fingerprint
        || scope != expected_scope
        || scope.provider_id != resolved.provider_id
        || scope.model_id != resolved.model_id
    {
        return Err(NativeArtifactError::InvalidScope);
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ModelFinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Cancelled,
    Error,
    Aborted,
    Timeout,
    Refused,
    Unknown,
    Other(String),
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistedFileSource {
    Artifact {
        byte_length: u64,
        sha256: Sha256Digest,
        reference: ArtifactReference,
    },
    Url {
        url: String,
    },
    ProviderReference {
        #[ts(type = "ProviderId")]
        provider_id: ProviderId,
        id: SafeDisplayText,
    },
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PersistedFilePart {
    pub media_type: MimeType,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub filename: Option<String>,
    pub source: PersistedFileSource,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeModelMetadata>", required)]
    pub metadata: Option<SafeModelMetadata>,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistedContentValue {
    Text { text: String },
    File { file: PersistedFilePart },
    Json { value: Value },
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistedToolContent {
    Text {
        text: String,
    },
    Json {
        value: Value,
    },
    Mixed {
        values: Vec<PersistedContentValue>,
    },
    Denied {
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<String>", required)]
        reason: Option<String>,
    },
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistedAssistantPart {
    Text {
        text: String,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeModelMetadata>", required)]
        metadata: Option<SafeModelMetadata>,
    },
    Reasoning {
        text: String,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeModelMetadata>", required)]
        metadata: Option<SafeModelMetadata>,
    },
    ToolCall {
        id: ModelCallId,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<ProviderItemId>", required)]
        provider_item_id: Option<ProviderItemId>,
        name: SafeCode,
        input: Value,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<String>", required)]
        raw_input: Option<String>,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeModelMetadata>", required)]
        metadata: Option<SafeModelMetadata>,
    },
    ToolResult {
        tool_call_id: ModelCallId,
        content: PersistedToolContent,
        is_error: bool,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeModelMetadata>", required)]
        metadata: Option<SafeModelMetadata>,
    },
    File {
        file: PersistedFilePart,
    },
    Source {
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<String>", required)]
        id: Option<String>,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<String>", required)]
        url: Option<String>,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<String>", required)]
        title: Option<String>,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<String>", required)]
        media_type: Option<String>,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<String>", required)]
        excerpt: Option<String>,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeModelMetadata>", required)]
        metadata: Option<SafeModelMetadata>,
    },
    ToolApproval {
        tool_call_id: ModelCallId,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<String>", required)]
        message: Option<String>,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeModelMetadata>", required)]
        metadata: Option<SafeModelMetadata>,
    },
    Custom {
        kind: SafeCode,
        data: Value,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeModelMetadata>", required)]
        metadata: Option<SafeModelMetadata>,
    },
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PersistedModelTurn {
    #[schemars(length(max = 4096))]
    pub content: Vec<PersistedAssistantPart>,
    pub provider_options: BTreeMap<String, Value>,
    pub finish_reason: ModelFinishReason,
    pub usage: Usage,
    pub response_metadata: SafeModelMetadata,
    pub provider_metadata: SafeModelMetadata,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<NativeReplayArtifact>", required)]
    pub native_replay: Option<NativeReplayArtifact>,
}
impl PersistedModelTurn {
    pub const MAX_SERIALIZED_BYTES: usize = 8 * 1024 * 1024;
    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if self.content.len() > 4096 {
            return Err(EventSchemaError::TooManyModelParts);
        }
        if serde_json::to_vec(self)
            .map_err(|_| EventSchemaError::InvalidJson)?
            .len()
            > Self::MAX_SERIALIZED_BYTES
        {
            return Err(EventSchemaError::ModelTurnTooLarge);
        }
        Ok(())
    }

    pub fn validate_for(&self, resolved: &ResolvedModelRef) -> Result<(), EventSchemaError> {
        self.validate()?;
        if let Some(native_replay) = &self.native_replay {
            native_replay
                .validate_resolved(resolved)
                .map_err(|_| EventSchemaError::NativeSelectionMismatch)?;
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for PersistedModelTurn {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            content: Vec<PersistedAssistantPart>,
            provider_options: BTreeMap<String, Value>,
            finish_reason: ModelFinishReason,
            usage: Usage,
            response_metadata: SafeModelMetadata,
            provider_metadata: SafeModelMetadata,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            native_replay: Option<NativeReplayArtifact>,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            content: w.content,
            provider_options: w.provider_options,
            finish_reason: w.finish_reason,
            usage: w.usage,
            response_metadata: w.response_metadata,
            provider_metadata: w.provider_metadata,
            native_replay: w.native_replay,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplayDisposition {
    Replayed,
    NoArtifact,
    DiscardedForeignAdapter {
        found: SafeCode,
        expected: SafeCode,
    },
    DiscardedForeignModelSelection {
        #[serde(deserialize_with = "crate::deserialize_required_model_selection")]
        #[schemars(with = "crate::RequiredModelSelectionSchema")]
        #[ts(type = "ModelSelection")]
        found: ModelSelection,
        #[serde(deserialize_with = "crate::deserialize_required_model_selection")]
        #[schemars(with = "crate::RequiredModelSelectionSchema")]
        #[ts(type = "ModelSelection")]
        expected: ModelSelection,
    },
    DiscardedForeignVariant {
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<VariantId>", required)]
        #[ts(type = "VariantId | null")]
        found: Option<VariantId>,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<VariantId>", required)]
        #[ts(type = "VariantId | null")]
        expected: Option<VariantId>,
    },
    DiscardedInvalidPayload {
        reason: SafeErrorMessage,
    },
    ReconstructedNormalizedHistory,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ReplayDecision {
    pub history_index: u64,
    pub disposition: ReplayDisposition,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorStage {
    #[default]
    Unknown,
    RequestValidation,
    RequestEncoding,
    Connect,
    ResponseHeaders,
    ResponseBody,
    StreamRead,
    StreamDecode,
    StreamEvent,
    StreamFinalize,
    ReplayEncode,
    ReplayDecode,
    NativeContextEncode,
    NativeContextDecode,
    Middleware,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    Transport,
    Timeout,
    RateLimited,
    Auth,
    PermissionDenied,
    InvalidRequest,
    ModelNotFound,
    ContextLength,
    Quota,
    Overload,
    Unsupported,
    UnexpectedEof,
    InvalidResponse,
    InvalidToolInput,
    ContentFilter,
    Replay,
    NativeContext,
    Provider,
    Abort,
    Unknown,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ModelErrorSummary {
    pub kind: ModelErrorKind,
    pub message: SafeErrorMessage,
    pub retryable: bool,
    pub stage: ModelErrorStage,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u16>", required)]
    pub http_status: Option<u16>,
    pub bytes_received: u64,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeCode>", required)]
    pub vendor_code: Option<SafeCode>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeDisplayText>", required)]
    pub request_id: Option<SafeDisplayText>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u64>", required)]
    pub retry_after_ms: Option<u64>,
}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum InternalAgentKind {
    Approval,
    ContextCompaction,
    SessionTitle,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrozenInternalAgentFallback {
    ParentModel,
    Model { binding: Box<FrozenModelBinding> },
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct FrozenInternalAgentDefinition {
    pub kind: InternalAgentKind,
    #[ts(type = "AgentId")]
    pub agent: AgentId,
    #[schemars(length(min = 1, max = 512))]
    pub description: String,
    pub document_source: AgentDocumentSource,
    pub document_fingerprint: Sha256Digest,
    #[schemars(length(min = 1, max = 131_072))]
    pub composed_prompt: String,
    pub prompt_fingerprint: Sha256Digest,
    pub enabled: bool,
    pub max_output_tokens: u64,
    pub timeout_ms: u64,
    #[schemars(length(max = 256))]
    pub fallbacks: Vec<FrozenInternalAgentFallback>,
}

impl FrozenInternalAgentDefinition {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.description.trim().is_empty()
            || self.description.len() > 512
            || self.description.chars().any(char::is_control)
            || self.composed_prompt.trim().is_empty()
            || self.composed_prompt.len() > AgentSnapshot::MAX_PROMPT_BYTES
            || self.fallbacks.len() > 256
        {
            return Err("invalid frozen internal agent definition");
        }
        for fallback in &self.fallbacks {
            if let FrozenInternalAgentFallback::Model { binding } = fallback {
                binding
                    .validate()
                    .map_err(|_| "invalid frozen internal agent binding")?;
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InternalAgentBackend {
    Model {
        resolved_model: ResolvedModelRef,
    },
    Builtin {
        name: SafeCode,
        revision: SafeDisplayText,
    },
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SafeInternalAgentCall {
    pub name: SafeCode,
    pub input_summary: SafeDisplayText,
    pub input_digest: Sha256Digest,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SafeInternalAgentResult {
    pub output_summary: SafeDisplayText,
    pub output_digest: Sha256Digest,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct InternalAgentFailure {
    pub code: SafeCode,
    pub message: SafeErrorMessage,
    pub retryable: bool,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ModelErrorSummary>", required)]
    pub model_error: Option<ModelErrorSummary>,
}

fn validate_internal_backend(backend: &InternalAgentBackend) -> Result<(), EventSchemaError> {
    match backend {
        InternalAgentBackend::Model { resolved_model } => resolved_model
            .validate()
            .map_err(|_| EventSchemaError::InvalidInternalAgentLifecycle),
        InternalAgentBackend::Builtin { .. } => Ok(()),
    }
}

fn validate_internal_failure(failure: &InternalAgentFailure) -> Result<(), EventSchemaError> {
    if failure
        .model_error
        .as_ref()
        .is_some_and(|error| error.retryable != failure.retryable)
    {
        Err(EventSchemaError::InvalidInternalAgentLifecycle)
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ContextCheckpointBoundaries {
    #[schemars(range(min = 1))]
    pub source_from_seq: u64,
    #[schemars(range(min = 1))]
    pub source_through_seq: u64,
    pub input_through_seq: u64,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u64>", required)]
    pub prior_checkpoint_seq: Option<u64>,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, TS)]
#[ts(type = "number")]
pub struct SummaryByteLimit(u64);
impl SummaryByteLimit {
    pub const GLOBAL_MAX_BYTES: u64 = 2 * 1024 * 1024;
    pub fn new(value: u64) -> Result<Self, EventSchemaError> {
        if value > Self::GLOBAL_MAX_BYTES {
            Err(EventSchemaError::SummaryLimitTooLarge)
        } else {
            Ok(Self(value))
        }
    }
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}
impl Serialize for SummaryByteLimit {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_u64(self.0)
    }
}
impl<'de> Deserialize<'de> for SummaryByteLimit {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(u64::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for SummaryByteLimit {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SummaryByteLimit")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"integer","minimum":0,"maximum":2_097_152})
    }
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ContextCheckpointBudgets {
    #[schemars(range(min = 1))]
    pub context_limit_tokens: u64,
    #[schemars(range(min = 1))]
    pub trigger_tokens: u64,
    #[schemars(range(min = 1))]
    pub input_tokens_before: u64,
    pub input_tokens_after: u64,
    pub max_summary_bytes: SummaryByteLimit,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ContextRehydratedFile {
    pub path: SafeDisplayText,
    pub content: String,
    pub byte_length: u64,
    pub sha256: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AgentMdEntry {
    pub source: SafeDisplayText,
    pub content: String,
    pub truncated: bool,
    pub original_bytes: u64,
}
impl AgentMdEntry {
    pub const MAX_CONTENT_BYTES: usize = 2 * 1024 * 1024;

    pub fn validate(&self) -> Result<(), EventSchemaError> {
        let content_bytes = self.content.len() as u64;
        if self.content.len() > Self::MAX_CONTENT_BYTES
            || self.original_bytes < content_bytes
            || self.truncated != (self.original_bytes > content_bytes)
        {
            return Err(EventSchemaError::InvalidAgentMd);
        }
        Ok(())
    }
}

impl ContextRehydratedFile {
    pub const MAX_CONTENT_BYTES: usize = 32 * 1024;

    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if self.content.len() > Self::MAX_CONTENT_BYTES
            || self.byte_length != self.content.len() as u64
            || self.sha256 != Sha256Digest::of_bytes(self.content.as_bytes())
        {
            return Err(EventSchemaError::InvalidContextRehydration);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct InternalSummaryCheckpoint {
    summary: String,
    byte_length: u64,
    sha256: Sha256Digest,
    invocation_id: InternalAgentInvocationId,
    internal_run_id: InternalAgentRunId,
}
impl InternalSummaryCheckpoint {
    pub fn new(
        summary: String,
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        max: SummaryByteLimit,
    ) -> Result<Self, EventSchemaError> {
        let byte_length = summary.len() as u64;
        if byte_length > max.get() || byte_length > SummaryByteLimit::GLOBAL_MAX_BYTES {
            return Err(EventSchemaError::SummaryTooLarge);
        }
        let sha256 = Sha256Digest::of_bytes(summary.as_bytes());
        Ok(Self {
            summary,
            byte_length,
            sha256,
            invocation_id,
            internal_run_id,
        })
    }
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }
    #[must_use]
    pub const fn byte_length(&self) -> u64 {
        self.byte_length
    }
}
impl<'de> Deserialize<'de> for InternalSummaryCheckpoint {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            summary: String,
            byte_length: u64,
            sha256: Sha256Digest,
            invocation_id: InternalAgentInvocationId,
            internal_run_id: InternalAgentRunId,
        }
        let w = Wire::deserialize(d)?;
        if w.summary.len() as u64 != w.byte_length
            || Sha256Digest::of_bytes(w.summary.as_bytes()) != w.sha256
            || w.byte_length > SummaryByteLimit::GLOBAL_MAX_BYTES
        {
            return Err(serde::de::Error::custom(
                "invalid summary length, digest, or bound",
            ));
        }
        Ok(Self {
            summary: w.summary,
            byte_length: w.byte_length,
            sha256: w.sha256,
            invocation_id: w.invocation_id,
            internal_run_id: w.internal_run_id,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContextCheckpoint {
    InternalSummary {
        #[serde(flatten)]
        checkpoint: InternalSummaryCheckpoint,
    },
    NativeWindow {
        window: NativeContextWindow,
    },
}
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ContextCheckpointCommit {
    pub checkpoint: ContextCheckpoint,
    pub boundaries: ContextCheckpointBoundaries,
    pub budgets: ContextCheckpointBudgets,
}
impl ContextCheckpointCommit {
    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if self.boundaries.source_from_seq == 0
            || self.boundaries.source_from_seq > self.boundaries.source_through_seq
            || self.boundaries.input_through_seq < self.boundaries.source_through_seq
            || self.budgets.context_limit_tokens == 0
            || self.budgets.trigger_tokens > self.budgets.context_limit_tokens
            || self.budgets.input_tokens_after >= self.budgets.input_tokens_before
        {
            return Err(EventSchemaError::InvalidCheckpointBoundaries);
        }
        match &self.checkpoint {
            ContextCheckpoint::InternalSummary { checkpoint }
                if checkpoint.byte_length() > self.budgets.max_summary_bytes.get() =>
            {
                Err(EventSchemaError::SummaryTooLarge)
            }
            _ => Ok(()),
        }
    }

    pub fn validate_for_binding(
        &self,
        binding: &FrozenModelBinding,
    ) -> Result<(), EventSchemaError> {
        self.validate()?;
        if let ContextCheckpoint::NativeWindow { window } = &self.checkpoint {
            window
                .validate_for_binding(binding, window.scope())
                .map_err(|_| EventSchemaError::NativeSelectionMismatch)?;
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for ContextCheckpointCommit {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            checkpoint: ContextCheckpoint,
            boundaries: ContextCheckpointBoundaries,
            budgets: ContextCheckpointBudgets,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            checkpoint: w.checkpoint,
            boundaries: w.boundaries,
            budgets: w.budgets,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum EventPayload {
    SessionCreated {
        origin: SessionOrigin,
        cwd_identity: CwdIdentity,
        creation_selection: RunSelection,
        creation_agent: Box<AgentSnapshot>,
        #[ts(type = "RuntimeRevision")]
        runtime_revision: RuntimeRevision,
        #[ts(type = "CatalogRevision")]
        catalog_revision: CatalogRevision,
        #[ts(type = "ProviderStateRevision")]
        provider_state_revision: ProviderStateRevision,
        #[ts(type = "ModelRevision")]
        model_revision: ModelRevision,
        #[ts(type = "AgentRevision")]
        agent_revision: AgentRevision,
        #[ts(type = "RecipeRegistryRevision")]
        recipe_registry_revision: RecipeRegistryRevision,
        #[ts(type = "ModelSnapshotRevision")]
        manifest_revision: ModelSnapshotRevision,
    },
    SessionReverted {
        #[schemars(range(min = 1))]
        through_seq: u64,
    },
    SessionPermissionOverlaySet {
        overlay: SessionPermissionOverlay,
    },
    SkillLoaded {
        name: String,
        rendered_body: String,
        source_path: String,
        args: String,
        base_dir: String,
        #[schemars(length(max = 10))]
        supporting_files: Vec<String>,
    },
    SkillInvocationNoted {
        name: String,
    },
    AgentMdLoaded {
        #[schemars(length(min = 1, max = 2))]
        entries: Vec<AgentMdEntry>,
    },
    PluginEventAdded {
        plugin: String,
        name: String,
        payload: Value,
    },
    PluginDiagnostic {
        plugin: String,
        kind: PluginDiagnosticKind,
        message: String,
        count: u64,
    },
    RunStarted {
        client_run_id: ClientRunId,
        selection: RunSelection,
        agent: Box<AgentSnapshot>,
        #[ts(type = "RuntimeRevision")]
        runtime_revision: RuntimeRevision,
        #[ts(type = "CatalogRevision")]
        catalog_revision: CatalogRevision,
        #[ts(type = "ProviderStateRevision")]
        provider_state_revision: ProviderStateRevision,
        #[ts(type = "ModelRevision")]
        model_revision: ModelRevision,
        #[ts(type = "AgentRevision")]
        agent_revision: AgentRevision,
        #[ts(type = "RecipeRegistryRevision")]
        recipe_registry_revision: RecipeRegistryRevision,
        #[ts(type = "ModelSnapshotRevision")]
        manifest_revision: ModelSnapshotRevision,
        #[schemars(length(min = 1, max = 256))]
        selected_suffix: Vec<FrozenModelBinding>,
        #[serde(default)]
        #[schemars(length(max = 3))]
        internal_agents: Vec<FrozenInternalAgentDefinition>,
        input_through_seq: u64,
    },
    MessageInjected {
        role: crate::ExtensionMessageRole,
        input: String,
    },
    UserInputAdmitted {
        input: String,
    },
    UserInputSubmitted {
        input: String,
    },
    UserInputTransformed {
        original_input: String,
        input: String,
    },
    UserInputRecalled {
        input: String,
    },
    UserInputRecalledV2 {
        user_input_seq: u64,
        input: String,
    },
    UserInputApplied {
        user_input_seq: u64,
    },
    DelegatedContextSeeded {
        invocation_id: InvocationId,
        #[schemars(length(max = 65536))]
        turns: Vec<DelegatedContextTurn>,
    },
    RunCompleted {
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<String>", required)]
        final_text: Option<String>,
    },
    RunFailed {
        error: SafeErrorMessage,
    },
    RunCancelled {
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeErrorMessage>", required)]
        reason: Option<SafeErrorMessage>,
    },
    RunInterrupted {
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeErrorMessage>", required)]
        reason: Option<SafeErrorMessage>,
    },
    ModelAttemptStarted {
        attempt_id: AttemptId,
        #[schemars(range(min = 1))]
        attempt_ordinal: u32,
        fallback_index: u32,
        retry_ordinal: u32,
        resolved_model: ResolvedModelRef,
        prompt_fingerprint: Sha256Digest,
    },
    ModelRequestPrepared {
        attempt_id: AttemptId,
        prompt_fingerprint: Sha256Digest,
    },
    TextDelta {
        attempt_id: AttemptId,
        text: String,
    },
    ReasoningDelta {
        attempt_id: AttemptId,
        text: String,
    },
    AttemptAbandoned {
        attempt_id: AttemptId,
    },
    ModelReplayEvaluated {
        attempt_id: AttemptId,
        resolved_model: ResolvedModelRef,
        #[schemars(length(max = 4096))]
        ordered_decisions: Vec<ReplayDecision>,
    },
    ModelTurnCommitted {
        attempt_id: AttemptId,
        #[schemars(range(min = 1))]
        model_turn_seq: u64,
        resolved_model: ResolvedModelRef,
        input_through_seq: u64,
        turn: PersistedModelTurn,
        #[schemars(length(max = 256))]
        warnings: Vec<SafeErrorMessage>,
    },
    ModelUsageRecorded {
        #[schemars(range(min = 1))]
        model_turn_seq: u64,
        #[ts(type = "AgentId")]
        agent_id: AgentId,
        resolved_model: ResolvedModelRef,
        usage: Usage,
        #[serde(default)]
        #[ts(optional = nullable)]
        estimated_cost_pico_usd: Option<u64>,
    },
    ModelFallback {
        from: ResolvedModelRef,
        to: ResolvedModelRef,
        from_fallback_index: u32,
        to_fallback_index: u32,
        #[schemars(range(min = 1))]
        attempts_on_from: u32,
        error: ModelErrorSummary,
    },
    ToolCallStarted {
        #[serde(flatten)]
        start: ToolCallStart,
    },
    ToolCallProgress {
        tool_call_id: ToolCallId,
        message: SafeDisplayText,
        #[serde(default)]
        #[ts(optional = nullable)]
        output_chunk: Option<SafeDisplayText>,
    },
    ToolCallTerminated {
        #[serde(flatten)]
        termination: ToolCallTermination,
    },
    ToolOutputElided {
        tool_call_id: ToolCallId,
        original_bytes: u64,
        retained: ArtifactReference,
    },
    ToolStdinSubmitted {
        tool_call_id: ToolCallId,
        byte_count: u64,
    },
    ToolCallLinked {
        tool_call_id: ToolCallId,
        child_session_id: SessionId,
    },
    DelegationReserved {
        reservation: DelegationReservation,
        child_agent: Box<AgentSnapshot>,
        #[ts(type = "ModelSnapshotRevision")]
        manifest_revision: ModelSnapshotRevision,
        #[ts(type = "RuntimeRevision")]
        runtime_revision: RuntimeRevision,
        #[ts(type = "CatalogRevision")]
        catalog_revision: CatalogRevision,
        #[ts(type = "ProviderStateRevision")]
        provider_state_revision: ProviderStateRevision,
        #[ts(type = "ModelRevision")]
        model_revision: ModelRevision,
        #[ts(type = "AgentRevision")]
        agent_revision: AgentRevision,
        #[ts(type = "RecipeRegistryRevision")]
        recipe_registry_revision: RecipeRegistryRevision,
        #[schemars(length(min = 1, max = 256))]
        selected_suffix: Vec<FrozenModelBinding>,
        #[serde(default)]
        #[schemars(length(max = 256))]
        cache_strategies: Vec<Option<FrozenCacheStrategy>>,
        request_fingerprint: Sha256Digest,
        request: DelegateRequestPayload,
    },
    DelegationStarted {
        invocation_id: InvocationId,
        child_session_id: SessionId,
    },
    DelegationRunStarted {
        invocation_id: InvocationId,
        child_run_id: RunId,
    },
    DelegationRunAttached {
        invocation_id: InvocationId,
        child_run_id: RunId,
    },
    DelegationFinished {
        invocation_id: InvocationId,
        child_session_id: SessionId,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<RunId>", required)]
        child_run_id: Option<RunId>,
        status: SessionStatus,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeErrorMessage>", required)]
        reason: Option<SafeErrorMessage>,
    },
    DelegateQueued {
        session_id: SessionId,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<u32>", required)]
        position: Option<u32>,
    },
    DelegateFinished {
        session_id: SessionId,
        status: SessionStatus,
        #[schemars(length(max = 2048))]
        preview: String,
        total_lines: u64,
    },
    DelegateFinishedV2 {
        invocation_id: InvocationId,
        session_id: SessionId,
        status: SessionStatus,
        #[schemars(length(max = 2048))]
        preview: String,
        total_lines: u64,
    },
    DelegateChildTerminated {
        status: SessionStatus,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeErrorMessage>", required)]
        reason: Option<SafeErrorMessage>,
    },
    ApprovalRequested {
        request: ApprovalRequest,
    },
    ApprovalEvaluated {
        approval_id: ApprovalId,
        decision: ApprovalInternalDecision,
    },
    ApprovalEscalated {
        approval_id: ApprovalId,
        reason_code: ApprovalReasonCode,
    },
    ApprovalUserDecisionRecorded {
        approval_id: ApprovalId,
        client_response_id: ClientResponseId,
        decision: ApprovalUserDecision,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<ApprovalFeedback>", required)]
        feedback: Option<ApprovalFeedback>,
    },
    ApprovalFinalized {
        approval_id: ApprovalId,
        decision: ApprovalFinalDecision,
    },
    ApprovalCancelled {
        approval_id: ApprovalId,
        reason_code: ApprovalReasonCode,
    },
    ApprovalDoomLoopDetected {
        approval_id: ApprovalId,
        operation_fingerprint: OperationFingerprint,
        #[schemars(range(min = 4))]
        repetitions: u32,
    },
    TreeApprovalGrantCommitted {
        grant: TreeApprovalGrant,
    },
    InternalAgentStarted {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        backend: InternalAgentBackend,
        call: SafeInternalAgentCall,
    },
    InternalAgentCompleted {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        result: SafeInternalAgentResult,
    },
    InternalAgentUsageRecorded {
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        #[ts(type = "AgentId")]
        agent_id: AgentId,
        resolved_model: ResolvedModelRef,
        usage: Usage,
        #[serde(default)]
        #[ts(optional = nullable)]
        estimated_cost_pico_usd: Option<u64>,
    },
    InternalAgentFailed {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        failure: InternalAgentFailure,
    },
    InternalAgentCancelled {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeErrorMessage>", required)]
        reason: Option<SafeErrorMessage>,
    },
    InternalAgentInterrupted {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<SafeErrorMessage>", required)]
        reason: Option<SafeErrorMessage>,
    },
    InternalAgentFallback {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        from: InternalAgentBackend,
        to: InternalAgentBackend,
        failure: InternalAgentFailure,
        #[schemars(range(min = 1))]
        attempts: u32,
    },
    ContextCheckpointCommitted {
        #[serde(flatten)]
        commit: ContextCheckpointCommit,
    },
    ContextRehydrated {
        #[schemars(length(max = 5))]
        files: Vec<ContextRehydratedFile>,
    },
    SessionTitleCommitted {
        change: SessionTitleChange,
        input_through_seq: u64,
    },
}
impl EventPayload {
    fn requires_run_id(&self) -> bool {
        !matches!(
            self,
            Self::SessionCreated { .. }
                | Self::SessionReverted { .. }
                | Self::SessionPermissionOverlaySet { .. }
                | Self::SkillLoaded { .. }
                | Self::SkillInvocationNoted { .. }
                | Self::PluginEventAdded { .. }
                | Self::PluginDiagnostic { .. }
                | Self::SessionTitleCommitted { .. }
                | Self::UserInputAdmitted { .. }
                | Self::UserInputRecalled { .. }
                | Self::DelegatedContextSeeded { .. }
                | Self::DelegateChildTerminated { .. }
        )
    }
    fn validate(&self) -> Result<(), EventSchemaError> {
        match self {
            Self::SessionCreated {
                creation_selection,
                creation_agent,
                ..
            } => {
                creation_agent
                    .validate_selection(creation_selection)
                    .map_err(|_| EventSchemaError::SelectionAgentMismatch)?;
            }
            Self::SessionReverted { through_seq } => {
                if *through_seq == 0 {
                    return Err(EventSchemaError::InvalidRevertSequence);
                }
            }
            Self::SessionPermissionOverlaySet { overlay } => overlay
                .validate()
                .map_err(|_| EventSchemaError::InvalidSessionPermissionOverlay)?,
            Self::SkillLoaded {
                name,
                rendered_body,
                source_path,
                base_dir,
                supporting_files,
                ..
            } => {
                if name.is_empty()
                    || rendered_body.is_empty()
                    || source_path.is_empty()
                    || base_dir.is_empty()
                    || supporting_files.len() > 10
                {
                    return Err(EventSchemaError::InvalidSkillEvent);
                }
            }
            Self::SkillInvocationNoted { name } if name.is_empty() => {
                return Err(EventSchemaError::InvalidSkillEvent);
            }
            Self::AgentMdLoaded { entries } => {
                if entries.is_empty() || entries.len() > 2 {
                    return Err(EventSchemaError::InvalidAgentMd);
                }
                for entry in entries {
                    entry.validate()?;
                }
            }
            Self::RunStarted {
                selection,
                agent,
                selected_suffix,
                internal_agents,
                ..
            } => {
                agent
                    .validate_selected_suffix(selection, selected_suffix)
                    .map_err(|_| EventSchemaError::InvalidSelectedSuffix)?;
                if !matches!(internal_agents.len(), 0 | 3)
                    || internal_agents
                        .windows(2)
                        .any(|pair| pair[0].kind >= pair[1].kind)
                    || internal_agents.iter().any(|definition| {
                        definition.validate().is_err()
                            || definition.agent.as_str()
                                != match definition.kind {
                                    InternalAgentKind::Approval => "approval",
                                    InternalAgentKind::ContextCompaction => "compaction",
                                    InternalAgentKind::SessionTitle => "title",
                                }
                    })
                {
                    return Err(EventSchemaError::InvalidAgentSnapshot);
                }
            }
            Self::MessageInjected { role, input }
                if input.trim().is_empty() || *role == crate::ExtensionMessageRole::Tool =>
            {
                return Err(EventSchemaError::InvalidJson);
            }
            Self::UserInputTransformed {
                original_input,
                input,
            } if original_input.trim().is_empty()
                || input.trim().is_empty()
                || original_input == input =>
            {
                return Err(EventSchemaError::InvalidJson);
            }
            Self::ModelAttemptStarted {
                attempt_ordinal,
                resolved_model,
                ..
            } => {
                if *attempt_ordinal == 0 {
                    return Err(EventSchemaError::InvalidAttemptOrdinal);
                }
                resolved_model
                    .validate()
                    .map_err(|_| EventSchemaError::InvalidResolvedModel)?;
            }
            Self::ModelReplayEvaluated {
                resolved_model,
                ordered_decisions,
                ..
            } => {
                if ordered_decisions.len() > 4096 {
                    return Err(EventSchemaError::TooManyReplayDecisions);
                }
                resolved_model
                    .validate()
                    .map_err(|_| EventSchemaError::InvalidResolvedModel)?;
            }
            Self::ModelTurnCommitted {
                model_turn_seq,
                resolved_model,
                turn,
                warnings,
                ..
            } => {
                if *model_turn_seq == 0 || warnings.len() > 256 {
                    return Err(EventSchemaError::InvalidModelTurnCommit);
                }
                resolved_model
                    .validate()
                    .map_err(|_| EventSchemaError::InvalidResolvedModel)?;
                turn.validate_for(resolved_model)?;
            }
            Self::ModelUsageRecorded {
                model_turn_seq,
                resolved_model,
                ..
            } => {
                if *model_turn_seq == 0 {
                    return Err(EventSchemaError::InvalidModelTurnCommit);
                }
                resolved_model
                    .validate()
                    .map_err(|_| EventSchemaError::InvalidResolvedModel)?;
            }
            Self::InternalAgentUsageRecorded { resolved_model, .. } => {
                resolved_model
                    .validate()
                    .map_err(|_| EventSchemaError::InvalidResolvedModel)?;
            }
            Self::ModelFallback {
                from,
                to,
                from_fallback_index,
                to_fallback_index,
                attempts_on_from,
                ..
            } => {
                if to_fallback_index <= from_fallback_index
                    || *attempts_on_from == 0
                    || from.selection.model == to.selection.model
                {
                    return Err(EventSchemaError::InvalidFallback);
                }
                from.validate()
                    .map_err(|_| EventSchemaError::InvalidResolvedModel)?;
                to.validate()
                    .map_err(|_| EventSchemaError::InvalidResolvedModel)?;
            }
            Self::ToolCallStarted { start } => {
                if start.owner.model_turn_seq == 0 {
                    return Err(EventSchemaError::ZeroModelTurnSequence);
                }
            }
            Self::ToolCallTerminated { termination } => termination.validate()?,
            Self::DelegationReserved {
                child_agent,
                selected_suffix,
                cache_strategies,
                request,
                ..
            } => {
                request.validate()?;
                if (!cache_strategies.is_empty() && cache_strategies.len() != selected_suffix.len())
                    || cache_strategies
                        .iter()
                        .flatten()
                        .any(|strategy| strategy.validate().is_err())
                {
                    return Err(EventSchemaError::InvalidDelegationLifecycle);
                }
                child_agent
                    .validate_selected_suffix(
                        &RunSelection {
                            agent: child_agent.agent.clone(),
                            model: selected_suffix
                                .first()
                                .ok_or(EventSchemaError::InvalidDelegationLifecycle)?
                                .selection
                                .clone(),
                            preset: None,
                        },
                        selected_suffix,
                    )
                    .map_err(|_| EventSchemaError::InvalidDelegationLifecycle)?;
            }
            Self::DelegationFinished { status, .. }
                if !matches!(
                    status,
                    SessionStatus::Completed
                        | SessionStatus::Failed
                        | SessionStatus::Interrupted
                        | SessionStatus::Cancelled
                ) =>
            {
                return Err(EventSchemaError::InvalidDelegationLifecycle);
            }
            Self::DelegatedContextSeeded { turns, .. } => {
                if turns.is_empty()
                    || turns.len() > 65_536
                    || turns.iter().any(|turn| turn.text.is_empty())
                    || turns.iter().map(|turn| turn.text.len()).sum::<usize>() > 65_536
                {
                    return Err(EventSchemaError::InvalidDelegatedContext);
                }
            }
            Self::DelegateFinished {
                status, preview, ..
            }
            | Self::DelegateFinishedV2 {
                status, preview, ..
            } => {
                if !matches!(
                    status,
                    SessionStatus::Completed
                        | SessionStatus::Failed
                        | SessionStatus::Interrupted
                        | SessionStatus::Cancelled
                ) || preview.len() > 2048
                {
                    return Err(EventSchemaError::InvalidDelegateFinished);
                }
            }
            Self::DelegateChildTerminated { status, .. }
                if !matches!(status, SessionStatus::Failed | SessionStatus::Cancelled) =>
            {
                return Err(EventSchemaError::InvalidDelegateFinished);
            }
            Self::ToolOutputElided {
                original_bytes,
                retained,
                ..
            } => {
                if *original_bytes == 0 {
                    return Err(EventSchemaError::InvalidToolElision);
                }
                retained.validate()?;
            }
            Self::ApprovalEvaluated { decision, .. } => {
                decision
                    .validate()
                    .map_err(|_| EventSchemaError::InvalidApprovalLifecycle)?;
            }
            Self::ApprovalEscalated { reason_code, .. }
                if *reason_code != ApprovalReasonCode::Escalated =>
            {
                return Err(EventSchemaError::InvalidApprovalLifecycle);
            }
            Self::ApprovalUserDecisionRecorded {
                decision, feedback, ..
            } if feedback.is_some() && *decision != ApprovalUserDecision::Reject => {
                return Err(EventSchemaError::InvalidApprovalLifecycle);
            }
            Self::ApprovalFinalized { decision, .. } => decision
                .validate()
                .map_err(|_| EventSchemaError::InvalidApprovalLifecycle)?,
            Self::ApprovalCancelled { reason_code, .. }
                if !matches!(
                    reason_code,
                    ApprovalReasonCode::UserCancelled
                        | ApprovalReasonCode::RequestCancelled
                        | ApprovalReasonCode::ApprovalExpired
                        | ApprovalReasonCode::OperationChanged
                        | ApprovalReasonCode::PreparedCapabilityLost
                ) =>
            {
                return Err(EventSchemaError::InvalidApprovalLifecycle);
            }
            Self::ApprovalDoomLoopDetected { repetitions, .. } if *repetitions < 4 => {
                return Err(EventSchemaError::InvalidApprovalCounter);
            }
            Self::TreeApprovalGrantCommitted { grant } => grant
                .validate()
                .map_err(|_| EventSchemaError::InvalidApprovalLifecycle)?,
            Self::InternalAgentStarted { backend, .. } => validate_internal_backend(backend)?,
            Self::InternalAgentFailed { failure, .. } => validate_internal_failure(failure)?,
            Self::InternalAgentFallback {
                from,
                to,
                failure,
                attempts,
                ..
            } => {
                if from == to || *attempts == 0 {
                    return Err(EventSchemaError::InvalidInternalAgentLifecycle);
                }
                validate_internal_backend(from)?;
                validate_internal_backend(to)?;
                validate_internal_failure(failure)?;
            }
            Self::ContextCheckpointCommitted { commit } => commit.validate()?,
            Self::ContextRehydrated { files } => {
                if files.is_empty() || files.len() > 5 {
                    return Err(EventSchemaError::InvalidContextRehydration);
                }
                for file in files {
                    file.validate()?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PluginDiagnosticKind {
    EventDrop,
    InterceptionTimeout,
    InterceptionCrash,
    HookBlocked,
    OversizedEvent,
    InvalidModification,
    UnsupportedCapability,
    ContextMismatch,
    RateLimited,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct StoredEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub engine_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub origin: Option<EventOrigin>,
    pub session_id: SessionId,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<RunId>", required)]
    pub run_id: Option<RunId>,
    #[schemars(range(min = 1))]
    pub seq: u64,
    pub timestamp: Timestamp,
    pub payload: EventPayload,
}
impl StoredEvent {
    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if let Some(origin) = &self.origin {
            EventOrigin::new(origin.as_str())?;
        }
        if self.seq == 0 {
            return Err(EventSchemaError::ZeroEventSequence);
        }
        if matches!(self.payload, EventPayload::SessionCreated { .. })
            && (self.seq != 1 || self.run_id.is_some())
        {
            return Err(EventSchemaError::InvalidSessionCreatedEnvelope);
        }
        if self.payload.requires_run_id() && self.run_id.is_none() {
            return Err(EventSchemaError::MissingRunId);
        }
        self.payload.validate()?;
        Ok(())
    }
}

/// Reduces an append-only event stream to the currently visible branch.
/// Revert markers are physical control records and are not model/transcript content.
#[must_use]
pub fn visible_events(events: &[StoredEvent]) -> Vec<StoredEvent> {
    let mut visible = Vec::new();
    let mut historical_ceiling = u64::MAX;
    for event in events {
        if let EventPayload::SessionReverted { through_seq } = &event.payload {
            historical_ceiling = historical_ceiling.min(*through_seq);
            visible.retain(|candidate: &StoredEvent| candidate.seq <= historical_ceiling);
            visible.push(event.clone());
        } else {
            visible.push(event.clone());
        }
    }
    visible
}
impl<'de> Deserialize<'de> for StoredEvent {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(default)]
            engine_version: Option<String>,
            #[serde(default)]
            origin: Option<EventOrigin>,
            #[serde(default)]
            event_schema_version: Option<Value>,
            session_id: SessionId,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            run_id: Option<RunId>,
            seq: u64,
            timestamp: Timestamp,
            payload: Value,
        }
        let w = Wire::deserialize(d)?;
        let _ = w.event_schema_version;
        let value = Self {
            engine_version: w.engine_version,
            origin: w.origin,
            session_id: w.session_id,
            run_id: w.run_id,
            seq: w.seq,
            timestamp: w.timestamp,
            payload: deserialize_event_payload_best_effort(w.payload)
                .map_err(serde::de::Error::custom)?
                .payload,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

const MAX_DEGRADED_FIELDS: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub struct EventPayloadRead {
    pub payload: EventPayload,
    pub degraded_fields: Vec<String>,
}

/// Decodes an event payload, replacing only leaves which Serde identifies as
/// nullable fields. Required-field and unknown-variant failures remain errors.
pub fn deserialize_event_payload_best_effort(mut value: Value) -> Result<EventPayloadRead, String> {
    let mut degraded_fields = Vec::new();
    let mut previous_path = None;
    for _ in 0..=MAX_DEGRADED_FIELDS {
        match serde_path_to_error::deserialize::<_, EventPayload>(value.clone().into_deserializer())
        {
            Ok(payload) => {
                return Ok(EventPayloadRead {
                    payload,
                    degraded_fields,
                });
            }
            Err(error) => {
                let path = error.path().to_string();
                if path != "." && previous_path.as_deref() == Some(path.as_str()) {
                    return Err(error.to_string());
                }
                let degraded = if path == "." {
                    replace_degradable_leaf(&mut value, &error.to_string())
                } else if replace_path_with_null(&mut value, error.path()) {
                    Some(path.clone())
                } else {
                    None
                };
                let Some(degraded) = degraded else {
                    return Err(error.to_string());
                };
                previous_path = Some(path.clone());
                degraded_fields.push(degraded);
            }
        }
    }
    Err(format!(
        "event payload exceeds the {MAX_DEGRADED_FIELDS}-field degradation limit"
    ))
}

#[derive(Clone)]
enum ValuePathSegment {
    Key(String),
    Index(usize),
}

fn replace_degradable_leaf(value: &mut Value, original_error: &str) -> Option<String> {
    let mut paths = Vec::new();
    collect_leaf_paths(value, &mut Vec::new(), &mut paths);
    for path in paths {
        let mut candidate = value.clone();
        if !set_value_path(&mut candidate, &path, Value::Null) {
            continue;
        }
        match serde_path_to_error::deserialize::<_, EventPayload>(
            candidate.clone().into_deserializer(),
        ) {
            Ok(_) => {
                *value = candidate;
                return Some(display_value_path(&path));
            }
            Err(error)
                if error.to_string() != original_error
                    && !error.inner().to_string().contains("invalid type: null") =>
            {
                *value = candidate;
                return Some(display_value_path(&path));
            }
            Err(_) => {}
        }
    }
    let missing = original_error
        .split("missing field `")
        .nth(1)
        .and_then(|suffix| suffix.split('`').next())?;
    let mut object_paths = Vec::new();
    collect_object_paths(value, &mut Vec::new(), &mut object_paths);
    for mut path in object_paths {
        path.push(ValuePathSegment::Key(missing.to_owned()));
        let mut candidate = value.clone();
        if !set_value_path(&mut candidate, &path, Value::Null) {
            continue;
        }
        if serde_json::from_value::<EventPayload>(candidate.clone()).is_ok() {
            *value = candidate;
            return Some(display_value_path(&path));
        }
    }
    None
}

fn collect_leaf_paths(
    value: &Value,
    current: &mut Vec<ValuePathSegment>,
    paths: &mut Vec<Vec<ValuePathSegment>>,
) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key == "type" || child.is_null() {
                    continue;
                }
                current.push(ValuePathSegment::Key(key.clone()));
                collect_leaf_paths(child, current, paths);
                current.pop();
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter().enumerate() {
                current.push(ValuePathSegment::Index(index));
                collect_leaf_paths(child, current, paths);
                current.pop();
            }
        }
        _ => paths.push(current.clone()),
    }
}

fn collect_object_paths(
    value: &Value,
    current: &mut Vec<ValuePathSegment>,
    paths: &mut Vec<Vec<ValuePathSegment>>,
) {
    match value {
        Value::Object(object) => {
            paths.push(current.clone());
            for (key, child) in object {
                current.push(ValuePathSegment::Key(key.clone()));
                collect_object_paths(child, current, paths);
                current.pop();
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter().enumerate() {
                current.push(ValuePathSegment::Index(index));
                collect_object_paths(child, current, paths);
                current.pop();
            }
        }
        _ => {}
    }
}

fn set_value_path(value: &mut Value, path: &[ValuePathSegment], replacement: Value) -> bool {
    let Some((leaf, parents)) = path.split_last() else {
        return false;
    };
    let mut current = value;
    for segment in parents {
        let Some(next) = (match segment {
            ValuePathSegment::Key(key) => current.get_mut(key),
            ValuePathSegment::Index(index) => current.get_mut(*index),
        }) else {
            return false;
        };
        current = next;
    }
    match leaf {
        ValuePathSegment::Key(key) => current
            .as_object_mut()
            .map(|object| object.insert(key.clone(), replacement))
            .is_some(),
        ValuePathSegment::Index(index) => current
            .as_array_mut()
            .and_then(|array| array.get_mut(*index))
            .map(|value| *value = replacement)
            .is_some(),
    }
}

fn display_value_path(path: &[ValuePathSegment]) -> String {
    let mut result = String::new();
    for segment in path {
        match segment {
            ValuePathSegment::Key(key) => {
                if !result.is_empty() {
                    result.push('.');
                }
                result.push_str(key);
            }
            ValuePathSegment::Index(index) => result.push_str(&format!("[{index}]")),
        }
    }
    result
}

fn replace_path_with_null(value: &mut Value, path: &serde_path_to_error::Path) -> bool {
    let segments = path
        .iter()
        .filter(|segment| !matches!(segment, Segment::Enum { .. }))
        .collect::<Vec<_>>();
    let Some((leaf, parents)) = segments.split_last() else {
        return false;
    };
    let mut current = value;
    for segment in parents {
        let next = match segment {
            Segment::Map { key } => current.get_mut(key),
            Segment::Seq { index } => current.get_mut(*index),
            Segment::Enum { .. } | Segment::Unknown => None,
        };
        let Some(next) = next else {
            return false;
        };
        current = next;
    }
    match leaf {
        Segment::Map { key } => {
            let Some(object) = current.as_object_mut() else {
                return false;
            };
            object.remove(key);
            object.insert(key.clone(), Value::Null);
            true
        }
        Segment::Seq { index } => current
            .as_array_mut()
            .and_then(|array| array.get_mut(*index))
            .map(|leaf| *leaf = Value::Null)
            .is_some(),
        Segment::Enum { .. } | Segment::Unknown => false,
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventSubscriptionMessage {
    Event {
        event: Box<StoredEvent>,
    },
    Gap {
        session_id: SessionId,
        last_delivered_seq: u64,
    },
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct OutputDelta {
    pub call_id: ToolCallId,
    pub stream: OutputStream,
    pub byte_offset: u64,
    pub data: String,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct OutputGap {
    pub call_id: ToolCallId,
    pub stream: OutputStream,
    pub next_offset: u64,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct OutputSnapshot {
    pub call_id: ToolCallId,
    pub start_offset: u64,
    pub end_offset: u64,
    pub chunks: Vec<OutputDelta>,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct OutputSnapshotEnvelope {
    pub stream: OutputStream,
    pub snapshot: OutputSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventSchemaError {
    InvalidOrigin,
    InvalidAgentMd,
    EmptyTitle,
    TitleTooLong,
    TitleControlCharacter,
    InvalidArtifactReference,
    InvalidToolElision,
    InvalidContextRehydration,
    ToolOutputTooLarge,
    ToolMetadataTooLarge,
    TooManyAttachments,
    InvalidAttachmentFilename,
    TooManyToolEmittedMessages,
    EmptyToolEmittedMessage,
    EmptyToolEmittedText,
    ToolEmittedTextTooLarge,
    InvalidJson,
    ZeroModelTurnSequence,
    InvalidToolTermination,
    InvalidDelegateFinished,
    InvalidDelegationLifecycle,
    InvalidDelegatedContext,
    TooManyModelParts,
    ModelTurnTooLarge,
    SummaryLimitTooLarge,
    SummaryTooLarge,
    InvalidCheckpointBoundaries,
    NativeSelectionMismatch,
    InvalidApprovalLifecycle,
    InvalidApprovalCounter,
    InvalidInternalAgentLifecycle,
    InvalidSessionMetaSequence,
    SelectionAgentMismatch,
    InvalidAgentSnapshot,
    InvalidSelectedSuffix,
    InvalidAttemptOrdinal,
    InvalidResolvedModel,
    TooManyReplayDecisions,
    InvalidModelTurnCommit,
    InvalidFallback,
    InvalidRevertSequence,
    InvalidSessionPermissionOverlay,
    InvalidSkillEvent,
    ZeroEventSequence,
    InvalidSessionCreatedEnvelope,
    MissingRunId,
}
impl fmt::Display for EventSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidOrigin => "event origin is invalid",
            Self::InvalidAgentMd => "AGENTS.md context event is invalid",
            Self::EmptyTitle => "session title must not be blank",
            Self::TitleTooLong => "session title exceeds 512 bytes",
            Self::TitleControlCharacter => "session title must not contain control characters",
            Self::InvalidArtifactReference => {
                "artifact reference must be a bounded artifact:// URI"
            }
            Self::InvalidToolElision => "tool output elision is invalid",
            Self::InvalidContextRehydration => "context rehydration is invalid",
            Self::ToolOutputTooLarge => "persisted tool output exceeds 2 MiB",
            Self::ToolMetadataTooLarge => "persisted tool metadata exceeds 2 MiB",
            Self::TooManyAttachments => "persisted tool result exceeds 256 attachments",
            Self::InvalidAttachmentFilename => "attachment filename is invalid",
            Self::TooManyToolEmittedMessages => {
                "persisted tool result exceeds 4 additional messages"
            }
            Self::EmptyToolEmittedMessage => "tool-emitted message content must not be empty",
            Self::EmptyToolEmittedText => "tool-emitted text must not be blank",
            Self::ToolEmittedTextTooLarge => "tool-emitted message text exceeds 64 KiB",
            Self::InvalidJson => "JSON value could not be serialized",
            Self::ZeroModelTurnSequence => "model_turn_seq must be positive",
            Self::InvalidToolTermination => {
                "tool termination outcome/result/error combination is invalid"
            }
            Self::InvalidDelegateFinished => "delegate completion payload is invalid",
            Self::InvalidDelegationLifecycle => "delegation lifecycle payload is invalid",
            Self::InvalidDelegatedContext => "delegated context seed is invalid",
            Self::TooManyModelParts => "persisted model turn exceeds 4096 parts",
            Self::ModelTurnTooLarge => "persisted model turn exceeds 8 MiB",
            Self::SummaryLimitTooLarge => "summary limit exceeds 2 MiB",
            Self::SummaryTooLarge => "summary exceeds its frozen byte limit",
            Self::InvalidCheckpointBoundaries => "context checkpoint boundaries are invalid",
            Self::NativeSelectionMismatch => {
                "native artifact selection fingerprint does not match resolved model"
            }
            Self::InvalidApprovalLifecycle => "approval lifecycle fields contradict",
            Self::InvalidApprovalCounter => {
                "approval lifecycle counter is below its runtime minimum"
            }
            Self::InvalidInternalAgentLifecycle => "internal-agent lifecycle fields contradict",
            Self::InvalidSessionMetaSequence => "session metadata sequence fields are inconsistent",
            Self::SelectionAgentMismatch => {
                "creation selection agent does not match creation snapshot"
            }
            Self::InvalidAgentSnapshot => "event contains an invalid agent snapshot",
            Self::InvalidSelectedSuffix => "RunStarted selected suffix is invalid",
            Self::InvalidAttemptOrdinal => "attempt ordinal must be positive",
            Self::InvalidResolvedModel => "event contains an invalid resolved model",
            Self::TooManyReplayDecisions => "replay decision list exceeds 4096 entries",
            Self::InvalidModelTurnCommit => "model turn sequence or warnings are invalid",
            Self::InvalidFallback => {
                "fallback must advance to a distinct later model after at least one attempt"
            }
            Self::InvalidRevertSequence => "session revert sequence must be positive",
            Self::InvalidSessionPermissionOverlay => "session permission overlay is invalid",
            Self::InvalidSkillEvent => "skill event is invalid",
            Self::ZeroEventSequence => "event sequence must be positive",
            Self::InvalidSessionCreatedEnvelope => {
                "SessionCreated must be sequence 1 with no run_id"
            }
            Self::MissingRunId => "run-scoped event requires run_id",
        })
    }
}
impl std::error::Error for EventSchemaError {}
