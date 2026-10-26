use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    fmt,
};

use jiff::Timestamp;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_path_to_error::Segment;

use crate::*;

fn producer_description_is_empty(description: &SafeDisplayText) -> bool {
    description.as_str().is_empty()
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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

crate::open_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SessionStatus {
        Idle,
        Running,
        Completed,
        Failed,
        Cancelled,
        Interrupted,
    }
}

crate::open_enum! {
    /// A producer that will start a session's next run without the user
    /// (client protocol 1.9.0): what `SessionMeta::waiting_on` lists. A
    /// value this build does not know reads as `Unknown`, still something
    /// the session waits on.
    #[derive(
        Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
    )]
    #[serde(rename_all = "snake_case")]
    pub enum WaitingOn {
        /// The root session's goal is active: goal mode keeps scheduling
        /// runs until the goal is paused, completed, or cancelled.
        Goal,
        /// A background call of the session is running; its
        /// `<background_result>` starts a run.
        BackgroundCall,
        /// A direct child is running a turn on its own, or a background
        /// delegation is queued or running; its `<subagent_report>` starts a
        /// run.
        Subagent,
        /// A plugin or runtime holds a live producer registration for the
        /// session and may send to it.
        ExternalProducer,
        /// An enabled cron job with a next fire time targets the session
        /// (client protocol 1.15.0); its fire starts a run.
        ScheduledJob,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DelegationReservation {
    pub invocation_id: InvocationId,
    pub parent_session_id: SessionId,
    pub parent_run_id: RunId,
    pub parent_tool_call_id: ToolCallId,
    pub child_session_id: SessionId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegatedContextRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DelegatedContextTurn {
    pub role: DelegatedContextRole,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StagedSkillProvenance {
    SkillFork,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
    pub model: Option<ModelKey>,
    /// The attachment that contributed a project skill; its grants apply
    /// only to that attachment's tools. Absent for a user skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentName>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
            || (writer_size_caps()
                && self
                    .seeded_context
                    .iter()
                    .map(|turn| turn.text.len())
                    .sum::<usize>()
                    > 65_536)
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

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize)]
pub struct SessionMeta {
    pub session_id: SessionId,
    pub origin: SessionOrigin,
    /// Short, tree-unique model-facing handle. Every session has one; it is
    /// null only in a projection built without its creation event.
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub short_id: Option<String>,
    pub cwd_identity: CwdIdentity,
    pub creation_selection: RunSelection,
    pub runtime_revision: RuntimeRevision,
    pub catalog_revision: CatalogRevision,
    pub provider_state_revision: ProviderStateRevision,
    pub model_revision: ModelRevision,
    pub agent_revision: AgentRevision,
    pub recipe_registry_revision: RecipeRegistryRevision,
    pub manifest_revision: ModelSnapshotRevision,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SessionTitle>", required)]
    pub title: Option<SessionTitle>,
    pub title_updated_seq: u64,
    #[schemars(range(min = 1))]
    pub last_event_seq: u64,
    pub last_activity: Timestamp,
    /// Latest user-initiated activity in the session: a user input submitted
    /// or admitted, or a `delegate_subagent` call started. Unlike
    /// `last_activity`, output and tool results never move it, so it orders
    /// sibling sessions without letting a busy one reorder itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_agent_activity: Option<Timestamp>,
    pub status: SessionStatus,
    pub skipped_events: Vec<SkippedEvent>,
    /// The session's runtime attachments now, in attach order: the fold of
    /// its `runtime_attached` and `runtime_detached` events (client protocol
    /// 1.7.0). Empty from an older daemon, and for a session whose
    /// metadata an older build wrote and that has recorded nothing since.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::RuntimeAttachment>,
    /// The project the session belongs to: for a root session the fold of
    /// its `session_project_set` events, for a delegated one its root's
    /// (client protocol 1.8.0). Absent outside a project, from an older
    /// daemon, and when the project no longer exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectId>,
    /// What will start the session's next run without the user, in this
    /// order and each once (client protocol 1.9.0): an active goal, folded
    /// from the log, and the daemon's live background calls, subagent
    /// turns, and external producer registrations. Reported whatever
    /// `status` says; a client shows a running session as running. Empty
    /// when nothing will wake the session, from an older daemon, and for a
    /// session whose metadata an older build wrote and that has recorded
    /// nothing since.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_on: Vec<WaitingOn>,
    /// For a root session, the attachments its subagents brought that its
    /// own did not record yet, in the order they brought them: the fold of
    /// its `delegated_attachments_added` events (client protocol 1.10.0).
    /// It only grows. Clients build a root's badges from `attachments`
    /// followed by these. Empty for a delegated session, from an older
    /// daemon, and for a session whose metadata an older build wrote and
    /// that has recorded nothing since.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delegated_attachments: Vec<crate::RuntimeAttachment>,
    /// The cron job whose fire created the session, and when it fired
    /// (client protocol 1.15.0). Absent for every other session and from
    /// an older daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<crate::SessionCron>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
            #[serde(deserialize_with = "deserialize_required_option")]
            short_id: Option<String>,
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
            #[serde(default)]
            last_agent_activity: Option<Timestamp>,
            status: SessionStatus,
            #[serde(default)]
            skipped_events: Vec<SkippedEvent>,
            #[serde(default)]
            attachments: Vec<crate::RuntimeAttachment>,
            #[serde(default)]
            project: Option<ProjectId>,
            #[serde(default)]
            waiting_on: Vec<WaitingOn>,
            #[serde(default)]
            delegated_attachments: Vec<crate::RuntimeAttachment>,
            #[serde(default)]
            cron: Option<crate::SessionCron>,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            session_id: w.session_id,
            origin: w.origin,
            short_id: w.short_id,
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
            last_agent_activity: w.last_agent_activity,
            status: w.status,
            skipped_events: w.skipped_events,
            attachments: w.attachments,
            project: w.project,
            waiting_on: w.waiting_on,
            delegated_attachments: w.delegated_attachments,
            cron: w.cron,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
    /// The attachment as an [`ArtifactRef`](crate::ArtifactRef), such as a
    /// `media` display block names. A filename an artifact reference cannot
    /// carry (longer than 255 bytes, or naming a directory) is left out.
    #[must_use]
    pub fn artifact_ref(&self) -> crate::ArtifactRef {
        let filename = self.filename.clone().filter(|name| {
            !name.is_empty()
                && name.len() <= crate::ArtifactRef::MAX_FILENAME_BYTES
                && !name
                    .chars()
                    .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        });
        crate::ArtifactRef {
            sha256: self.sha256.as_str().to_owned(),
            byte_length: self.byte_length,
            media_type: self.mime_type.as_str().to_ascii_lowercase(),
            filename,
        }
    }

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

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEmittedMessageRole {
    System,
    User,
}

/// One part of a tool-emitted message: `{"text": …}` or `{"file": …}`. Other
/// members are ignored.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(transform = open_object_schema)]
pub enum ToolEmittedContent {
    Text(String),
    File(ToolAttachment),
}

impl<'de> Deserialize<'de> for ToolEmittedContent {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            file: Option<ToolAttachment>,
        }
        match Wire::deserialize(d)? {
            Wire {
                text: Some(text),
                file: None,
            } => Ok(Self::Text(text)),
            Wire {
                text: None,
                file: Some(file),
            } => Ok(Self::File(file)),
            _ => Err(serde::de::Error::custom(
                "tool-emitted content holds exactly one of `text` and `file`",
            )),
        }
    }
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize)]
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
        struct Wire {
            role: ToolEmittedMessageRole,
            content: Vec<ToolEmittedContent>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.role, wire.content).map_err(serde::de::Error::custom)
    }
}
/// A tool result as sessions store it and clients receive it. `display` is
/// what clients show, a list of display blocks sharing one 64 KiB budget;
/// empty means nothing to show. A session log stores the display's text that
/// repeats `output` as byte ranges of `output` instead of a second copy
/// (`crate::stored_display`); decoding expands them, so nothing outside the
/// engine meets them.
#[derive(Clone, Debug, JsonSchema, PartialEq)]
pub struct PersistedToolResult {
    pub title: SafeDisplayText,
    pub output: String,
    pub display: Vec<Open<DisplayBlock>>,
    /// Present on the result a call committed when it moved to the
    /// background; the daemon alone sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<ToolBackground>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_output: Option<crate::RetainedToolOutput>,
    pub metadata: Value,
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
        if writer_size_caps() && self.output.len() > Self::MAX_OUTPUT_BYTES {
            return Err(EventSchemaError::ToolOutputTooLarge);
        }
        match crate::display::validate_display(&self.display) {
            Ok(()) => {}
            Err(crate::DisplayBlockError::TooLarge) if !writer_size_caps() => {}
            Err(crate::DisplayBlockError::TooLarge) => {
                return Err(EventSchemaError::ToolOutputTooLarge);
            }
            Err(_) => return Err(EventSchemaError::InvalidDisplay),
        }
        if let Some(retained) = &self.retained_output {
            retained.validate()?;
        }
        if self.background.as_ref().is_some_and(|background| {
            background.job.as_ref().is_some_and(|job| {
                job.is_empty() || job.len() > 64 || job.chars().any(char::is_control)
            }) || match background.state {
                ToolBackgroundState::Job => background.job.is_none(),
                ToolBackgroundState::SelfReporting => background.job.is_some(),
                ToolBackgroundState::Unknown => false,
            }
        }) {
            return Err(EventSchemaError::InvalidToolBackground);
        }
        if writer_size_caps()
            && json_len_exceeds(&self.metadata, Self::MAX_METADATA_BYTES)
                .map_err(|_| EventSchemaError::InvalidJson)?
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
        Ok(())
    }
}
impl Serialize for PersistedToolResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Wire<'a> {
            title: &'a SafeDisplayText,
            output: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            display: Option<DisplayWire<'a>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            display_span: Option<[usize; 2]>,
            #[serde(skip_serializing_if = "Option::is_none")]
            background: Option<&'a ToolBackground>,
            #[serde(skip_serializing_if = "Option::is_none")]
            retained_output: Option<&'a crate::RetainedToolOutput>,
            metadata: &'a Value,
            attachments: &'a [ToolAttachment],
            #[serde(skip_serializing_if = "<[_]>::is_empty")]
            additional_messages: &'a [ToolEmittedMessage],
        }
        #[derive(Serialize)]
        #[serde(untagged)]
        enum DisplayWire<'a> {
            Blocks(&'a [Open<DisplayBlock>]),
            Stored(Vec<Value>),
        }
        let stored = storage_encoding();
        let display_span = match self.display.as_slice() {
            [
                Open::Known(DisplayBlock::Text {
                    text,
                    omitted_lines: None,
                }),
            ] if stored && !text.is_empty() => self
                .output
                .find(text.as_str())
                .map(|start| [start, start + text.len()]),
            _ => None,
        };
        let display = match display_span {
            Some(_) => None,
            None if stored => Some(DisplayWire::Stored(
                crate::stored_display::encode(&self.output, &self.display)
                    .map_err(serde::ser::Error::custom)?,
            )),
            None => Some(DisplayWire::Blocks(&self.display)),
        };
        Wire {
            title: &self.title,
            output: &self.output,
            display,
            display_span,
            background: self.background.as_ref(),
            retained_output: self.retained_output.as_ref(),
            metadata: &self.metadata,
            attachments: &self.attachments,
            additional_messages: &self.additional_messages,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PersistedToolResult {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            title: SafeDisplayText,
            output: String,
            #[serde(default)]
            display: Option<Vec<Value>>,
            #[serde(default)]
            display_span: Option<Value>,
            #[serde(default)]
            background: Option<ToolBackground>,
            #[serde(default)]
            retained_output: Option<crate::RetainedToolOutput>,
            metadata: Value,
            attachments: Vec<ToolAttachment>,
            #[serde(default)]
            additional_messages: Vec<ToolEmittedMessage>,
        }
        let w = Wire::deserialize(d)?;
        let display = match (w.display, w.display_span) {
            (Some(mut display), None) => {
                crate::stored_display::decode(&w.output, &mut display)
                    .map_err(serde::de::Error::custom)?;
                display
                    .into_iter()
                    .map(serde_json::from_value)
                    .collect::<Result<Vec<Open<DisplayBlock>>, _>>()
                    .map_err(serde::de::Error::custom)?
            }
            (None, Some(span)) => vec![Open::Known(DisplayBlock::text(
                crate::stored_display::slice(&w.output, &span).map_err(serde::de::Error::custom)?,
            ))],
            (None, None) => return Err(serde::de::Error::missing_field("display")),
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "display and display_span are mutually exclusive",
                ));
            }
        };
        let value = Self {
            title: w.title,
            output: w.output,
            display,
            background: w.background,
            retained_output: w.retained_output,
            metadata: w.metadata,
            attachments: w.attachments,
            additional_messages: w.additional_messages,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

/// What became of a call that moved to the background: its work goes on and
/// reports its result later.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ToolBackground {
    pub state: ToolBackgroundState,
    /// Who moved the call there.
    pub by: BackgroundActor,
    /// The job the call runs as, present in the `job` state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 64))]
    pub job: Option<String>,
}

crate::open_enum! {
    /// How a backgrounded call's work goes on. An unknown state reads as
    /// `job` without a job: the call is backgrounded and its result arrives
    /// later.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ToolBackgroundState {
        /// It runs as background job `job`, which reports its result once.
        Job,
        /// The work reports itself, as a subagent's turns do.
        SelfReporting,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SafeToolError {
    pub code: SafeCode,
    pub message: SafeErrorMessage,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssistantToolCallRef {
    #[schemars(range(min = 1))]
    pub model_turn_seq: u64,
    pub content_index: u32,
    pub model_call_id: ModelCallId,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ProviderItemId>", required)]
    pub provider_item_id: Option<ProviderItemId>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ToolCallStart {
    pub tool_call_id: ToolCallId,
    pub output: crate::ToolOutputDeclaration,
    pub owner: AssistantToolCallRef,
    pub presentation: ToolPresentation,
    pub operation_fingerprint: OperationFingerprint,
    /// Whether the call's tool supports the background, so a client may
    /// offer to move the call there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTerminationOutcome {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
pub struct NativeContextScope {
    pub provider_id: ProviderId,
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

#[derive(Clone, JsonSchema, PartialEq, Serialize)]
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

#[derive(Clone, Eq, JsonSchema, PartialEq, Serialize)]
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

    /// Whether a request bound to `binding` may carry this window: the same
    /// adapter, provider ID, and model ID. The variant, and with it the
    /// selection fingerprint, may differ.
    pub fn validate_for_binding(
        &self,
        binding: &FrozenModelBinding,
    ) -> Result<(), NativeArtifactError> {
        binding
            .validate()
            .map_err(|_| NativeArtifactError::InvalidScope)?;
        if self.adapter_id.as_str() != binding.descriptor.adapter_id.as_str()
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
    if !writer_size_caps()
        || !json_len_exceeds(payload, maximum).map_err(|_| NativeArtifactError::Serialization)?
    {
        return Ok(());
    }
    let size = json_len(payload).map_err(|_| NativeArtifactError::Serialization)?;
    Err(NativeArtifactError::PayloadTooLarge { size, maximum })
}

thread_local! {
    static READING_HISTORY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `read` as a reader of stored history (`docs/site/specs/compatibility-v1.md`
/// section 8.5): the size caps a writer keeps to are not applied, so a
/// record reads as its writer bounded it. They are a tool result's output,
/// metadata, and display budget, a model turn's serialized size, native
/// replay payloads, and a diagnostic's detail. Every other check runs.
pub fn read_stored_history<T>(read: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            READING_HISTORY.with(|reading| reading.set(self.0));
        }
    }
    let _restore = Restore(READING_HISTORY.with(|reading| reading.replace(true)));
    read()
}

thread_local! {
    static WRITING_HISTORY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `write` as the writer of a session log: values serialize in the
/// log's storage encoding, which only the log holds. A tool result stores the
/// display text that repeats its output as ranges of it
/// (`docs/site/specs/compatibility-v1.md` section 12.3); every other value
/// serializes as it does anywhere else, and decoding reads either form.
pub fn write_stored_history<T>(write: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            WRITING_HISTORY.with(|writing| writing.set(self.0));
        }
    }
    let _restore = Restore(WRITING_HISTORY.with(|writing| writing.replace(true)));
    write()
}

/// Whether values serialize in the session log's storage encoding: only
/// inside [`write_stored_history`].
pub(crate) fn storage_encoding() -> bool {
    WRITING_HISTORY.with(std::cell::Cell::get)
}

/// Whether the writer-only size caps apply: everywhere but inside
/// [`read_stored_history`].
pub(crate) fn writer_size_caps() -> bool {
    !READING_HISTORY.with(std::cell::Cell::get)
}

/// Counts serialized bytes instead of buffering them, optionally failing
/// once a limit is passed so an oversized value stops serializing early.
struct ByteCounter {
    count: usize,
    limit: usize,
}

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.count = self.count.saturating_add(bytes.len());
        if self.count > self.limit {
            return Err(std::io::Error::other("serialized value exceeds its limit"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The length of `value` serialized as JSON, without buffering it.
fn json_len<T: Serialize + ?Sized>(value: &T) -> Result<usize, serde_json::Error> {
    let mut counter = ByteCounter {
        count: 0,
        limit: usize::MAX,
    };
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.count)
}

/// Whether `value` serializes to more than `maximum` bytes of JSON. Nothing
/// is buffered, and serialization stops as soon as the limit is passed.
pub(crate) fn json_len_exceeds<T: Serialize + ?Sized>(
    value: &T,
    maximum: usize,
) -> Result<bool, serde_json::Error> {
    let mut counter = ByteCounter {
        count: 0,
        limit: maximum,
    };
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => Ok(false),
        Err(_) if counter.count > maximum => Ok(true),
        Err(error) => Err(error),
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

/// Why a model turn ended. A reason this build does not know reads as
/// [`Self::Other`] with its name, and an `other` object's unknown members are
/// ignored.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(transform = open_object_schema)]
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

impl<'de> Deserialize<'de> for ModelFinishReason {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Name(String),
            Other { other: String },
        }
        Ok(match Wire::deserialize(d)? {
            Wire::Name(name) => match name.as_str() {
                "stop" => Self::Stop,
                "tool_calls" => Self::ToolCalls,
                "length" => Self::Length,
                "content_filter" => Self::ContentFilter,
                "cancelled" => Self::Cancelled,
                "error" => Self::Error,
                "aborted" => Self::Aborted,
                "timeout" => Self::Timeout,
                "refused" => Self::Refused,
                "unknown" => Self::Unknown,
                _ => Self::Other(name),
            },
            Wire::Other { other } => Self::Other(other),
        })
    }
}

/// Drops `additionalProperties: false` wherever schemars put it, for an
/// information type whose readers ignore unknown members.
pub(crate) fn open_object_schema(schema: &mut Schema) {
    fn open(value: &mut Value) {
        match value {
            Value::Object(object) => {
                if object.get("additionalProperties") == Some(&Value::Bool(false)) {
                    object.remove("additionalProperties");
                }
                object.values_mut().for_each(open);
            }
            Value::Array(values) => values.iter_mut().for_each(open),
            _ => {}
        }
    }
    if let Some(object) = schema.as_object_mut() {
        if object.get("additionalProperties") == Some(&Value::Bool(false)) {
            object.remove("additionalProperties");
        }
        object.values_mut().for_each(open);
    }
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
        provider_id: ProviderId,
        id: SafeDisplayText,
    },
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PersistedContentValue {
    Text { text: String },
    File { file: PersistedFilePart },
    Json { value: Value },
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize)]
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
        if writer_size_caps()
            && json_len_exceeds(self, Self::MAX_SERIALIZED_BYTES)
                .map_err(|_| EventSchemaError::InvalidJson)?
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
        found: ModelSelection,
        #[serde(deserialize_with = "crate::deserialize_required_model_selection")]
        #[schemars(with = "crate::RequiredModelSelectionSchema")]
        expected: ModelSelection,
    },
    DiscardedForeignVariant {
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<VariantId>", required)]
        found: Option<VariantId>,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<VariantId>", required)]
        expected: Option<VariantId>,
    },
    DiscardedInvalidPayload {
        reason: SafeErrorMessage,
    },
    ReconstructedNormalizedHistory,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReplayDecision {
    pub history_index: u64,
    pub disposition: ReplayDisposition,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ModelErrorSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<DiagnosticText>,
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
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum InternalAgentKind {
    Approval,
    ContextCompaction,
    SessionTitle,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrozenInternalAgentFallback {
    /// The owner run's model, with `variant` in place of its own when set
    /// (client protocol 1.22.0; `base` is the base variant).
    ParentModel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        variant: Option<cookie_agent_identity::ConfiguredVariantRef>,
    },
    Model {
        binding: Box<PersistedModelBinding>,
    },
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct FrozenInternalAgentDefinition {
    pub kind: InternalAgentKind,
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
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InternalAgentBackend {
    Model {
        resolved_model: ResolvedModelRef,
    },
    Builtin {
        name: SafeCode,
        revision: SafeDisplayText,
    },
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SafeInternalAgentCall {
    pub name: SafeCode,
    pub input_summary: SafeDisplayText,
    pub input_digest: Sha256Digest,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SafeInternalAgentResult {
    pub output_summary: SafeDisplayText,
    pub output_digest: Sha256Digest,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ContextCheckpointBoundaries {
    #[schemars(range(min = 1))]
    pub source_from_seq: u64,
    #[schemars(range(min = 1))]
    pub source_through_seq: u64,
    /// Where the kept recent tail starts; `null` when the checkpoint keeps
    /// none apart.
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u64>", required)]
    pub recent_from_seq: Option<u64>,
    pub input_through_seq: u64,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u64>", required)]
    pub prior_checkpoint_seq: Option<u64>,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ContextCheckpointBudgets {
    #[schemars(range(min = 1))]
    pub context_limit_tokens: u64,
    #[schemars(range(min = 1))]
    pub trigger_tokens: u64,
    #[schemars(range(min = 1))]
    pub input_tokens_before: u64,
    pub input_tokens_after: u64,
    pub keep_recent_tokens: u64,
    pub max_summary_bytes: SummaryByteLimit,
}

/// Most tool names one `model_tools_published` record carries.
pub const MAX_MODEL_TOOL_NAMES: usize = 1024;
/// Longest tool name a `model_tools_published` record accepts.
pub const MAX_MODEL_TOOL_NAME_BYTES: usize = 256;

/// Whether `names` fits a `model_tools_published` record: bounded, and every
/// name nonempty and free of control characters.
#[must_use]
pub fn valid_model_tool_names(names: &[String]) -> bool {
    names.len() <= MAX_MODEL_TOOL_NAMES
        && names.iter().all(|name| {
            !name.is_empty()
                && name.len() <= MAX_MODEL_TOOL_NAME_BYTES
                && !name.chars().any(char::is_control)
        })
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AgentMdEntry {
    pub source: SafeDisplayText,
    pub content: String,
    pub byte_length: u64,
}
/// The project an `agent_md_loaded` event's `AGENTS.md` belongs to.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AgentMdProject {
    pub id: ProjectId,
    /// The project's name when the file was read.
    pub name: ProjectName,
}

impl AgentMdEntry {
    pub const MAX_CONTENT_BYTES: usize = 2 * 1024 * 1024;

    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if self.content.len() > Self::MAX_CONTENT_BYTES
            || self.byte_length != self.content.len() as u64
        {
            return Err(EventSchemaError::InvalidAgentMd);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AgentMdSkipped {
    pub path: SafeDisplayText,
    pub byte_length: u64,
}
impl AgentMdSkipped {
    pub const MAX_SKIP_BYTES: u64 = 2 * 1024 * 1024;

    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if self.byte_length <= Self::MAX_SKIP_BYTES {
            return Err(EventSchemaError::InvalidAgentMd);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
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

/// The provider window a summary checkpoint carries beside its summary,
/// when native compaction succeeded alongside it (client protocol 1.29.0).
/// A request on the window's provider and model uses it; every other
/// request uses the summary.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CheckpointNativeWindow {
    pub window: NativeContextWindow,
    /// The last event the window covers; it keeps no recent tail apart.
    #[schemars(range(min = 1))]
    pub source_through_seq: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextCheckpoint {
    InternalSummary {
        #[serde(flatten)]
        checkpoint: InternalSummaryCheckpoint,
        /// The provider window compacted from the same events; absent when
        /// native compaction was not attempted or failed. A reader that
        /// ignores it uses the summary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native: Option<CheckpointNativeWindow>,
    },
    /// A provider window alone, which builds before 1.29.0 wrote; read
    /// only. A request that cannot use it compacts the covered events
    /// into a summary first.
    NativeWindow { window: NativeContextWindow },
}
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ContextCheckpointCommit {
    pub checkpoint: ContextCheckpoint,
    pub boundaries: ContextCheckpointBoundaries,
    pub budgets: ContextCheckpointBudgets,
}
impl ContextCheckpointCommit {
    pub fn validate(&self) -> Result<(), EventSchemaError> {
        let recent_outside_source =
            self.boundaries
                .recent_from_seq
                .is_some_and(|recent_from_seq| {
                    recent_from_seq < self.boundaries.source_from_seq
                        || recent_from_seq > self.boundaries.source_through_seq
                });
        let native_has_recent_tail =
            matches!(self.checkpoint, ContextCheckpoint::NativeWindow { .. })
                && (self.boundaries.recent_from_seq.is_some()
                    || self.budgets.keep_recent_tokens != 0);
        let native_outside_input = matches!(
            &self.checkpoint,
            ContextCheckpoint::InternalSummary { native: Some(native), .. }
                if native.source_through_seq < self.boundaries.source_from_seq
                    || native.source_through_seq > self.boundaries.input_through_seq
        );
        if self.boundaries.source_from_seq == 0
            || self.boundaries.source_from_seq > self.boundaries.source_through_seq
            || recent_outside_source
            || self.boundaries.input_through_seq < self.boundaries.source_through_seq
            || self.budgets.context_limit_tokens == 0
            || self.budgets.trigger_tokens > self.budgets.context_limit_tokens
            || self.budgets.input_tokens_after >= self.budgets.input_tokens_before
            || native_has_recent_tail
            || native_outside_input
        {
            return Err(EventSchemaError::InvalidCheckpointBoundaries);
        }
        match &self.checkpoint {
            ContextCheckpoint::InternalSummary { checkpoint, .. }
                if checkpoint.byte_length() > self.budgets.max_summary_bytes.get() =>
            {
                Err(EventSchemaError::SummaryTooLarge)
            }
            _ => Ok(()),
        }
    }

    /// [`Self::validate`], and for a window alone, whether a request bound
    /// to `binding` may carry it
    /// ([`NativeContextWindow::validate_for_binding`]).
    pub fn validate_for_binding(
        &self,
        binding: &FrozenModelBinding,
    ) -> Result<(), EventSchemaError> {
        self.validate()?;
        if let ContextCheckpoint::NativeWindow { window } = &self.checkpoint {
            window
                .validate_for_binding(binding)
                .map_err(|_| EventSchemaError::NativeSelectionMismatch)?;
        }
        Ok(())
    }

    /// The provider window this checkpoint holds, alone or beside its
    /// summary.
    #[must_use]
    pub fn native_window(&self) -> Option<&NativeContextWindow> {
        match &self.checkpoint {
            ContextCheckpoint::InternalSummary { native, .. } => {
                native.as_ref().map(|native| &native.window)
            }
            ContextCheckpoint::NativeWindow { window } => Some(window),
        }
    }
}
impl<'de> Deserialize<'de> for ContextCheckpointCommit {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
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

/// How a run ended: the `outcome` of [`EventPayload::RunFinished`].
///
/// Open: an outcome this build does not know reads as interrupted, a run
/// that ended without this build being able to say how.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum RunOutcome {
    Completed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_text: Option<String>,
    },
    Failed {
        error: SafeErrorMessage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_error: Option<ModelErrorSummary>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolved_model: Option<ResolvedModelRef>,
    },
    Cancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<SafeErrorMessage>,
    },
    Interrupted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<SafeErrorMessage>,
    },
}
crate::open_enum!(tagged RunOutcome, tag = "type", {
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
    Interrupted => "interrupted",
});

impl RunOutcome {
    /// The status the run ends with.
    #[must_use]
    pub const fn status(&self) -> SessionStatus {
        match self {
            Self::Completed { .. } => SessionStatus::Completed,
            Self::Failed { .. } => SessionStatus::Failed,
            Self::Cancelled { .. } => SessionStatus::Cancelled,
            Self::Interrupted { .. } => SessionStatus::Interrupted,
        }
    }
}

impl Open<RunOutcome> {
    /// The status the run ends with; an unknown outcome reads as
    /// interrupted.
    #[must_use]
    pub fn status(&self) -> SessionStatus {
        self.known()
            .map_or(SessionStatus::Interrupted, RunOutcome::status)
    }
}

/// How an internal agent invocation ended: the `outcome` of
/// [`EventPayload::InternalAgentFinished`].
///
/// Open: an outcome this build does not know reads as interrupted, with no
/// result.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InternalAgentOutcome {
    Completed {
        result: SafeInternalAgentResult,
    },
    Failed {
        failure: InternalAgentFailure,
    },
    Cancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<SafeErrorMessage>,
    },
    Interrupted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<SafeErrorMessage>,
    },
}
crate::open_enum!(tagged InternalAgentOutcome, tag = "type", {
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
    Interrupted => "interrupted",
});

/// How far a delegation got: the `stage` of
/// [`EventPayload::DelegationProgressed`].
///
/// Open: a stage this build does not know changes nothing it reads.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DelegationStage {
    /// The child session was published.
    Started { child_session_id: SessionId },
    /// A new child run started.
    RunStarted { child_run_id: RunId },
    /// The delegation attached to the already-running run of a resumed
    /// child.
    RunAttached { child_run_id: RunId },
    /// The child waits for a background slot.
    Queued {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        position: Option<u32>,
    },
}
crate::open_enum!(tagged DelegationStage, tag = "type", {
    Started => "started",
    RunStarted => "run_started",
    RunAttached => "run_attached",
    Queued => "queued",
});

/// One step of an approval before its finalization: the `step` of
/// [`EventPayload::ApprovalStep`].
///
/// Open: a step this build does not know changes nothing it reads.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalStep {
    /// The internal decision pipeline decided.
    Evaluated { decision: ApprovalInternalDecision },
    /// The approval waits for the user.
    Escalated {
        reason_code: ApprovalReasonCode,
        /// When the user's window to answer ends (client protocol 1.28.0):
        /// it starts at the escalation, so the reviewer's time never comes
        /// out of it. The request then carries no `expires_at`, and the
        /// approval's record carries this one. Absent from an earlier
        /// writer, whose request carries the window, and when the window
        /// never ends.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<Timestamp>,
    },
    /// A client answered for the user.
    UserDecision {
        client_response_id: ClientResponseId,
        decision: ApprovalUserDecision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<ApprovalFeedback>,
    },
    /// The same operation was requested too often in a row.
    DoomLoop {
        operation_fingerprint: OperationFingerprint,
        #[schemars(range(min = 4))]
        repetitions: u32,
    },
}
crate::open_enum!(tagged ApprovalStep, tag = "type", {
    Evaluated => "evaluated",
    Escalated => "escalated",
    UserDecision => "user_decision",
    DoomLoop => "doom_loop",
});

/// How an approval ended: the `outcome` of
/// [`EventPayload::ApprovalFinalized`].
///
/// Open: an outcome this build does not know reads as cancelled, so the
/// operation is not approved.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalFinalization {
    /// A decision by policy, a grant, the reviewer, or the user.
    Decided { decision: ApprovalFinalDecision },
    /// The engine withdrew the approval: the request was cancelled, expired,
    /// or its prepared operation changed or was lost.
    Cancelled { reason_code: ApprovalReasonCode },
}
crate::open_enum!(tagged ApprovalFinalization, tag = "type", {
    Decided => "decided",
    Cancelled => "cancelled",
});

impl ApprovalFinalization {
    /// The final decision this outcome records. A cancellation is the
    /// system's: `expired` for an expired approval, `cancelled` otherwise.
    #[must_use]
    pub fn decision(&self) -> ApprovalFinalDecision {
        match self {
            Self::Decided { decision } => decision.clone(),
            Self::Cancelled { reason_code } => ApprovalFinalDecision {
                outcome: if *reason_code == ApprovalReasonCode::ApprovalExpired {
                    ApprovalFinalOutcome::Expired
                } else {
                    ApprovalFinalOutcome::Cancelled
                },
                source: ApprovalDecisionSource::System,
                reason_code: *reason_code,
                feedback: None,
                tree_grant_id: None,
            },
        }
    }
}

/// The history format this build writes and reads: the major version of the
/// session log layout, stamped on `session_created`. It bumps only for a
/// change that cannot follow the additive rules of session history.
pub const HISTORY_FORMAT: u32 = 1;

/// The most bytes of a diagnostic's serialized `detail`.
pub const MAX_DIAGNOSTIC_DETAIL_BYTES: usize = 16 * 1024;

/// The revisions of the runtime a session, run, or delegation was frozen
/// against.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimeRevisions {
    pub runtime_revision: RuntimeRevision,
    pub catalog_revision: CatalogRevision,
    pub provider_state_revision: ProviderStateRevision,
    pub model_revision: ModelRevision,
    pub agent_revision: AgentRevision,
    pub recipe_registry_revision: RecipeRegistryRevision,
    pub manifest_revision: ModelSnapshotRevision,
}

/// The latest runtime revisions a log records, folded in log order.
///
/// `session_created` records them, and a later `run_started` or
/// `delegation_reserved` records them again only when they changed, so an
/// event without its own revisions was frozen against the latest before it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RevisionFold {
    latest: Option<RuntimeRevisions>,
}

impl RevisionFold {
    /// Folds one event and returns the revisions it was frozen against, for
    /// the events that record them.
    pub fn observe(&mut self, payload: &EventPayload) -> Option<&RuntimeRevisions> {
        match payload {
            EventPayload::SessionCreated { revisions, .. } => {
                self.latest = Some(revisions.clone());
            }
            EventPayload::RunStarted { revisions, .. }
            | EventPayload::DelegationReserved { revisions, .. } => {
                if let Some(revisions) = revisions {
                    self.latest = Some(revisions.clone());
                }
            }
            _ => return None,
        }
        self.latest.as_ref()
    }

    /// The latest revisions folded so far.
    #[must_use]
    pub const fn latest(&self) -> Option<&RuntimeRevisions> {
        self.latest.as_ref()
    }

    /// Drops `payload`'s own revisions when they equal the latest folded,
    /// which is how a writer records them only where they changed.
    pub fn elide_unchanged(&self, payload: &mut EventPayload) {
        if let EventPayload::RunStarted { revisions, .. }
        | EventPayload::DelegationReserved { revisions, .. } = payload
            && revisions.is_some()
            && revisions.as_ref() == self.latest.as_ref()
        {
            *revisions = None;
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum EventPayload {
    SessionCreated {
        /// The session log's [`HISTORY_FORMAT`].
        history_format: u32,
        origin: SessionOrigin,
        /// Short, tree-unique model-facing handle (`<agent_type>_<8 hex>`).
        short_id: String,
        cwd_identity: CwdIdentity,
        creation_selection: RunSelection,
        creation_agent: Box<AgentSnapshot>,
        /// The revisions the session was created against.
        revisions: RuntimeRevisions,
        /// The cron job whose fire created this session (client protocol
        /// 1.15.0); absent for every other session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cron: Option<crate::SessionCron>,
    },
    SessionPermissionOverlaySet {
        overlay: SessionPermissionOverlay,
    },
    /// The session was attached to a runtime instance under `name`.
    RuntimeAttached {
        name: AttachmentName,
        runtime_id: RuntimeId,
        /// The capability attached.
        capability: CapabilityName,
        kind: RuntimeKind,
        /// Canonical workspace the runtime reported; absent without one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    /// The attachment `name` was removed from the session.
    RuntimeDetached {
        name: AttachmentName,
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
        /// The coding-runtime attachment whose workspace the files came
        /// from; `global` for a project's `AGENTS.md`, which lives in the
        /// daemon's data directory.
        attachment: AttachmentName,
        /// The project whose `AGENTS.md` this is, when it is a project's
        /// rather than a workspace's (client protocol 1.8.0). It comes
        /// first in the `AGENTS.md` turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<AgentMdProject>,
    },
    /// The memory snapshot the session's model sees from this run on
    /// (`docs/site/specs/memory.md` section 7). An empty snapshot records
    /// that memory is no longer available.
    MemoryLoaded {
        #[serde(flatten)]
        snapshot: MemorySnapshot,
    },
    /// A model's memory tool saved, edited, or deleted the memory at `url`
    /// (`docs/site/specs/memory.md` section 16.9.10; client protocol
    /// 1.14.0). A transcript shows it as one line, such as `Saved
    /// project://cookie-agent/release`. An audit event: the memory itself
    /// lives outside the session.
    // Queued for removal at the next major (`docs/site/specs/
    // compatibility-v1.md` section 3.4): since 2026-10-24 the memory tools
    // no longer emit it, and clients only fold it for older logs.
    MemoryChanged {
        change: MemoryChangeKind,
        url: String,
    },
    /// A notice about the engine, a plugin, or a runtime: something it
    /// turned off, could not do, or dropped. `detail` is display-only.
    Diagnostic {
        source: DiagnosticSource,
        kind: DiagnosticKind,
        message: String,
        /// How many occurrences this one record coalesces; absent for one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(range(min = 1))]
        count: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Value>,
    },
    GoalActivated {
        goal_id: GoalId,
        objective: String,
        revision: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selection: Option<RunSelection>,
    },
    GoalChecklistRevised {
        goal_id: GoalId,
        items: Vec<GoalItem>,
        revision: u64,
    },
    GoalLifecycleChanged {
        goal_id: GoalId,
        status: GoalStatus,
        revision: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selection: Option<RunSelection>,
    },
    /// Sole durable message body and per-send delivery mode. Acceptance is valid
    /// during an active run without creating a runless UserInputAdmitted.
    ProducerMessageAccepted {
        message_id: ProducerMessageId,
        producer_owner: ProducerOwner,
        mode: ProducerDeliveryMode,
        idempotency_key: ProducerIdempotencyKey,
        #[serde(default, skip_serializing_if = "producer_description_is_empty")]
        description: SafeDisplayText,
        /// Text parts only.
        body: UserContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reminder: Option<GoalReminderIdentity>,
        /// Internal `send_message` guard metadata: position of this message in
        /// its agent-mail chain (`0` for mail sent by a run that has seen no
        /// agent mail). Never rendered into the agent-visible envelope and
        /// never compared for idempotency. Present exactly on Agent producers'
        /// messages.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_hop: Option<u32>,
        /// Engine-retained output the body points the agent at (a background
        /// job's full stdout/stderr). Recording it here keeps the artifacts
        /// alive for later reads; plugins cannot supply it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retained_output: Option<crate::RetainedToolOutput>,
    },
    /// Run-scoped reference, not a second body or consumption marker. Its event
    /// sequence participates in ModelTurnCommitted.input_through_seq coverage.
    ProducerMessageAdmitted {
        message_id: ProducerMessageId,
    },
    /// Run-scoped reservation of pending admitted messages. The envelope
    /// sequence is the claim token, not proof that provider dispatch occurred.
    ProducerMessagesClaimed {
        #[schemars(length(min = 1))]
        message_ids: Vec<ProducerMessageId>,
    },
    /// Releases the claim identified by a ProducerMessagesClaimed envelope.
    ProducerMessagesReleased {
        #[schemars(range(min = 1))]
        claim_seq: u64,
    },
    /// Append only after committed model input coverage includes the admission.
    /// Recovery repairs missing markers from that commit, never from API selection.
    ProducerMessageConsumed {
        message_id: ProducerMessageId,
        run_id: RunId,
    },
    /// Pending messages are discarded by stable owner identity.
    ProducerMessageDiscarded {
        message_id: ProducerMessageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reminder: Option<GoalReminderIdentity>,
        producer_owner: ProducerOwner,
    },
    RunStarted {
        client_run_id: ClientRunId,
        selection: RunSelection,
        agent: Box<AgentSnapshot>,
        /// The revisions the run was frozen against, present only when they
        /// differ from the latest the log recorded ([`RevisionFold`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revisions: Option<RuntimeRevisions>,
        /// One definition per internal agent kind, in kind order.
        #[schemars(length(min = 3, max = 3))]
        internal_agents: Vec<FrozenInternalAgentDefinition>,
        input_through_seq: u64,
    },
    /// `content` carries text parts only.
    MessageInjected {
        role: crate::ExtensionMessageRole,
        content: UserContent,
    },
    UserInputAdmitted {
        content: UserContent,
    },
    UserInputSubmitted {
        content: UserContent,
    },
    UserInputTransformed {
        original_content: UserContent,
        content: UserContent,
    },
    UserInputRecalled {
        user_input_seq: u64,
        content: UserContent,
    },
    UserInputApplied {
        user_input_seq: u64,
    },
    /// Input waiting for the running run to complete. The run the engine
    /// then starts with it carries `client_run_id`; a failed, cancelled, or
    /// interrupted run voids every queued input instead.
    UserInputQueued {
        content: UserContent,
        selection: RunSelection,
        client_run_id: ClientRunId,
    },
    /// Its author withdrew the queued input at `queued_seq`.
    UserInputQueueRecalled {
        queued_seq: u64,
    },
    /// The engine voided every queued input without starting a run, for
    /// example because the queued run could not start.
    UserInputQueueVoided {},
    /// A client picked the session's model selection: later runs use it
    /// unless a request names its own, and every client's draft follows it.
    SessionModelSelected {
        selection: RunSelection,
    },
    DelegatedContextSeeded {
        invocation_id: InvocationId,
        #[schemars(length(max = 65536))]
        turns: Vec<DelegatedContextTurn>,
    },
    /// The run ended.
    RunFinished {
        outcome: Open<RunOutcome>,
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
        // Write-only and costly to compute; dropped at the next major
        // version (compatibility-v1 spec, section 3.4).
        prompt_fingerprint: Sha256Digest,
    },
    /// Names of the tools sent with this attempt's request, after plugin
    /// rewrites. Recorded only when they differ from the session's previous
    /// record.
    ModelToolsPublished {
        attempt_id: AttemptId,
        #[schemars(length(max = 1024))]
        tool_names: Vec<String>,
    },
    /// The attempt began streaming a text or reasoning part: its first
    /// output, or a switch between the two. The deltas themselves are
    /// live-only; this durably records where in the log, and when, each
    /// streamed part began.
    ModelOutputStarted {
        attempt_id: AttemptId,
        kind: StreamedOutputKind,
    },
    /// Live-only: delivered as a [`TransientEvent`], never persisted.
    TextDelta {
        attempt_id: AttemptId,
        text: String,
    },
    /// Live-only: delivered as a [`TransientEvent`], never persisted.
    ReasoningDelta {
        attempt_id: AttemptId,
        text: String,
    },
    /// Live-only: text an internal agent is streaming, delivered as a
    /// [`TransientEvent`] and never persisted. Its terminal internal-agent
    /// event is the durable record.
    InternalAgentTextDelta {
        invocation_id: InternalAgentInvocationId,
        text: String,
    },
    /// Live-only: a provider-native compaction call began, delivered as a
    /// [`TransientEvent`] and never persisted.
    NativeCompactionStarted {
        resolved_model: ResolvedModelRef,
    },
    /// Live-only: the provider-native compaction call returned, whether or
    /// not it produced a window. A committed checkpoint is the durable record
    /// of a success.
    NativeCompactionFinished {
        resolved_model: ResolvedModelRef,
    },
    AttemptAbandoned {
        attempt_id: AttemptId,
        /// The model error that ended the attempt (retry, fallback, or
        /// recovery trigger).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_error: Option<ModelErrorSummary>,
    },
    ModelReplayEvaluated {
        attempt_id: AttemptId,
        resolved_model: ResolvedModelRef,
        /// The attempt's replay decisions. With `base_attempt_id`, only the
        /// decisions appended after that attempt's list, which is an exact
        /// prefix of this one.
        #[schemars(length(max = 4096))]
        ordered_decisions: Vec<ReplayDecision>,
        /// An earlier attempt of the same run, with the same resolved model,
        /// whose full decision list this attempt's list extends. Absent means
        /// `ordered_decisions` is the full list.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_attempt_id: Option<AttemptId>,
    },
    /// The provider refused a request because a replayed reasoning block is
    /// bound to a conversation prefix that changed since (Anthropic's
    /// thinking-binding check: its tools or system prompt): the reasoning of
    /// every turn committed at or before `through_seq` is no longer replayed
    /// (`docs/site/specs/thinking-replay-recovery.md`, client protocol
    /// 1.33.0).
    NativeReplayCut {
        #[schemars(range(min = 1))]
        through_seq: u64,
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
        agent_id: AgentId,
        resolved_model: ResolvedModelRef,
        usage: Usage,
        /// The price stamped at write time; `null` is unpriced.
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<u64>", required)]
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
    /// Live-only: delivered as a [`TransientEvent`], never persisted.
    ToolCallProgress {
        tool_call_id: ToolCallId,
        message: SafeDisplayText,
        /// A change to the call's live display.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<Open<DisplayDelta>>,
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
    ToolCallLinked {
        tool_call_id: ToolCallId,
        child_session_id: SessionId,
    },
    DelegationReserved {
        reservation: DelegationReservation,
        child_agent: Box<AgentSnapshot>,
        /// The revisions the child was frozen against, present only when
        /// they differ from the latest the log recorded ([`RevisionFold`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revisions: Option<RuntimeRevisions>,
        /// One cache strategy per entry of the child's selected suffix.
        #[schemars(length(min = 1, max = 256))]
        cache_strategies: Vec<Option<FrozenCacheStrategy>>,
        request_fingerprint: Sha256Digest,
        request: DelegateRequestPayload,
    },
    /// A delegation reached a new stage before it finished.
    DelegationProgressed {
        invocation_id: InvocationId,
        stage: Open<DelegationStage>,
    },
    /// A delegation finished. It is the durable control record and carries
    /// the teaser transcripts render: `short_id` is the child's handle, and
    /// `preview` the report as `[subagent_output]` previews it.
    DelegationFinished {
        invocation_id: InvocationId,
        child_session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_run_id: Option<RunId>,
        status: SessionStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<SafeErrorMessage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        short_id: Option<String>,
        #[schemars(length(max = 65536))]
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
    ApprovalStep {
        approval_id: ApprovalId,
        step: Open<ApprovalStep>,
    },
    ApprovalFinalized {
        approval_id: ApprovalId,
        outcome: Open<ApprovalFinalization>,
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
    InternalAgentUsageRecorded {
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        agent_id: AgentId,
        resolved_model: ResolvedModelRef,
        usage: Usage,
        /// The price stamped at write time; `null` is unpriced.
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<u64>", required)]
        estimated_cost_pico_usd: Option<u64>,
    },
    InternalAgentFinished {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        outcome: Open<InternalAgentOutcome>,
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
    SessionTitleCommitted {
        change: SessionTitleChange,
        input_through_seq: u64,
    },
    /// The root session moved into `project`, or out of any project when it
    /// is absent (client protocol 1.8.0). Only root sessions record it; a
    /// delegated session belongs to its root's project.
    SessionProjectSet {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<ProjectId>,
    },
    /// A subagent of this root session, `child_session_id`, was created with
    /// `attachments` that the root recorded neither as its own nor as
    /// delegated before (client protocol 1.10.0). Only root sessions record
    /// it; `SessionMeta.delegated_attachments` is its fold.
    DelegatedAttachmentsAdded {
        child_session_id: SessionId,
        #[schemars(length(min = 1))]
        attachments: Vec<crate::RuntimeAttachment>,
    },
    /// In place of a record of type `omitted_type` that the writer
    /// deliberately did not carry over, at that record's sequence, run, and
    /// time: a fork writes it for each of its source's delegation records,
    /// whose subagents stay the source's.
    RecordOmitted {
        omitted_type: EventType,
    },
    /// The ACP agent of this session talks to it in ACP session
    /// `acp_session_id` from now on (client protocol 1.17.0;
    /// `docs/site/specs/acp-agents.md` section 7).
    AcpSessionStarted {
        #[serde(flatten)]
        record: crate::AcpSessionRecord,
    },
    /// What the ACP agent reported about its session changed: its
    /// commands, its config options, or its plan (client protocol 1.17.0).
    AcpSessionUpdated {
        #[serde(flatten)]
        update: crate::AcpSessionUpdate,
    },
    /// An agent of the run asks the user a question: a form or a URL
    /// (client protocol 1.25.0). The run waits for `elicitation_resolved`.
    ElicitationRequested {
        #[serde(flatten)]
        request: crate::ElicitationRequest,
    },
    /// The question `elicitation_id` was answered (client protocol
    /// 1.25.0).
    ElicitationResolved {
        #[serde(flatten)]
        resolution: crate::ElicitationResolution,
    },
    /// The session tree's permission mode became `mode` (client protocol
    /// 1.28.0). Only root sessions record it, outside their runs; loading
    /// the tree restores the newest one.
    PermissionModeSet {
        mode: crate::PermissionMode,
    },
}

// Plugins receive events as `Open<EventPayload>`: an event type a plugin
// does not know reaches it as `Unknown`.
crate::open_enum!(tagged EventPayload, tag = "type", tags = [
    "acp_session_started",
    "acp_session_updated",
    "agent_md_loaded",
    "approval_finalized",
    "approval_requested",
    "approval_step",
    "attempt_abandoned",
    "context_checkpoint_committed",
    "delegate_child_terminated",
    "delegated_attachments_added",
    "delegated_context_seeded",
    "delegation_finished",
    "delegation_progressed",
    "delegation_reserved",
    "diagnostic",
    "elicitation_requested",
    "elicitation_resolved",
    "goal_activated",
    "goal_checklist_revised",
    "goal_lifecycle_changed",
    "internal_agent_fallback",
    "internal_agent_finished",
    "internal_agent_started",
    "internal_agent_text_delta",
    "internal_agent_usage_recorded",
    "memory_changed",
    "memory_loaded",
    "message_injected",
    "model_attempt_started",
    "model_fallback",
    "model_output_started",
    "model_replay_evaluated",
    "native_replay_cut",
    "model_request_prepared",
    "model_tools_published",
    "model_turn_committed",
    "model_usage_recorded",
    "native_compaction_finished",
    "native_compaction_started",
    "permission_mode_set",
    "producer_message_accepted",
    "producer_message_admitted",
    "producer_message_consumed",
    "producer_message_discarded",
    "producer_messages_claimed",
    "producer_messages_released",
    "reasoning_delta",
    "record_omitted",
    "run_finished",
    "run_started",
    "runtime_attached",
    "runtime_detached",
    "session_created",
    "session_model_selected",
    "session_permission_overlay_set",
    "session_project_set",
    "session_title_committed",
    "skill_invocation_noted",
    "skill_loaded",
    "text_delta",
    "tool_call_linked",
    "tool_call_progress",
    "tool_call_started",
    "tool_call_terminated",
    "tool_output_elided",
    "tree_approval_grant_committed",
    "user_input_admitted",
    "user_input_applied",
    "user_input_queue_recalled",
    "user_input_queue_voided",
    "user_input_queued",
    "user_input_recalled",
    "user_input_submitted",
    "user_input_transformed",
]);
impl EventPayload {
    /// A `run_finished` with `outcome`.
    #[must_use]
    pub fn run_finished(outcome: RunOutcome) -> Self {
        Self::RunFinished {
            outcome: outcome.into(),
        }
    }

    /// The status a `run_finished` ends its run with, or `None` for any
    /// other payload.
    #[must_use]
    pub fn run_end_status(&self) -> Option<SessionStatus> {
        match self {
            Self::RunFinished { outcome } => Some(outcome.status()),
            _ => None,
        }
    }

    /// An `internal_agent_finished` with `outcome`.
    #[must_use]
    pub fn internal_agent_finished(
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        outcome: InternalAgentOutcome,
    ) -> Self {
        Self::InternalAgentFinished {
            invocation_id,
            internal_run_id,
            kind,
            outcome: outcome.into(),
        }
    }

    /// A `delegation_progressed` of `invocation_id` to `stage`.
    #[must_use]
    pub fn delegation_progressed(invocation_id: InvocationId, stage: DelegationStage) -> Self {
        Self::DelegationProgressed {
            invocation_id,
            stage: stage.into(),
        }
    }

    /// An `approval_step` of `approval_id`.
    #[must_use]
    pub fn approval_step(approval_id: ApprovalId, step: ApprovalStep) -> Self {
        Self::ApprovalStep {
            approval_id,
            step: step.into(),
        }
    }

    /// An `approval_finalized` of `approval_id` with `outcome`.
    #[must_use]
    pub fn approval_finalized(approval_id: ApprovalId, outcome: ApprovalFinalization) -> Self {
        Self::ApprovalFinalized {
            approval_id,
            outcome: outcome.into(),
        }
    }

    /// The `diagnostic` an `AGENTS.md` file over the size limit records: the
    /// file in `attachment`'s workspace contributes no context.
    #[must_use]
    pub fn agent_md_skipped(skip: &AgentMdSkipped, attachment: &AttachmentName) -> Self {
        Self::Diagnostic {
            source: DiagnosticSource::engine("agent-md")
                .expect("static diagnostic source is valid"),
            kind: EngineDiagnosticKind::AgentMdTooLarge.label(),
            message: format!(
                "AGENTS.md skipped: {} is {} bytes, over the {} MiB limit",
                skip.path.as_str(),
                skip.byte_length,
                AgentMdSkipped::MAX_SKIP_BYTES / (1024 * 1024)
            ),
            count: None,
            detail: Some(serde_json::json!({
                "path": skip.path,
                "byte_length": skip.byte_length,
                "attachment": attachment,
            })),
        }
    }

    /// Whether `tag` is the `type` of an event this build knows.
    #[must_use]
    pub fn is_known_tag(tag: &str) -> bool {
        <Self as crate::OpenTagged>::known_tags().contains(tag)
    }

    /// Whether this payload is an audit event rather than a state event.
    ///
    /// A state event is one the projection, model history, delegation
    /// recovery, goals, producers, approvals, or usage read; an audit event is
    /// everything else, which a reader that does not know its tag can skip
    /// without changing any of them. Writers stamp `skippable` on the
    /// envelope of every audit event. Live-only stream output is never
    /// stored and counts as audit.
    #[must_use]
    pub fn is_audit(&self) -> bool {
        match self {
            Self::Diagnostic { .. }
            | Self::SkillInvocationNoted { .. }
            | Self::UserInputTransformed { .. }
            | Self::ModelRequestPrepared { .. }
            | Self::ModelToolsPublished { .. }
            | Self::ModelOutputStarted { .. }
            | Self::ModelReplayEvaluated { .. }
            | Self::TextDelta { .. }
            | Self::ReasoningDelta { .. }
            | Self::InternalAgentTextDelta { .. }
            | Self::NativeCompactionStarted { .. }
            | Self::NativeCompactionFinished { .. }
            | Self::ToolCallProgress { .. }
            | Self::MemoryChanged { .. }
            | Self::RecordOmitted { .. } => true,
            // The approval projection ignores the doom-loop step; an unknown
            // step reads as a state step, the safe side for its writer's
            // `skippable`.
            Self::ApprovalStep { step, .. } => {
                matches!(step, Open::Known(ApprovalStep::DoomLoop { .. }))
            }
            Self::SessionCreated { .. }
            | Self::AcpSessionStarted { .. }
            | Self::AcpSessionUpdated { .. }
            | Self::ElicitationRequested { .. }
            | Self::ElicitationResolved { .. }
            | Self::SessionPermissionOverlaySet { .. }
            | Self::PermissionModeSet { .. }
            | Self::RuntimeAttached { .. }
            | Self::RuntimeDetached { .. }
            | Self::SkillLoaded { .. }
            | Self::AgentMdLoaded { .. }
            | Self::MemoryLoaded { .. }
            | Self::SessionProjectSet { .. }
            | Self::DelegatedAttachmentsAdded { .. }
            | Self::GoalActivated { .. }
            | Self::GoalChecklistRevised { .. }
            | Self::GoalLifecycleChanged { .. }
            | Self::ProducerMessageAccepted { .. }
            | Self::ProducerMessageAdmitted { .. }
            | Self::ProducerMessagesClaimed { .. }
            | Self::ProducerMessagesReleased { .. }
            | Self::ProducerMessageConsumed { .. }
            | Self::ProducerMessageDiscarded { .. }
            | Self::RunStarted { .. }
            | Self::MessageInjected { .. }
            | Self::UserInputAdmitted { .. }
            | Self::UserInputSubmitted { .. }
            | Self::UserInputRecalled { .. }
            | Self::UserInputApplied { .. }
            | Self::UserInputQueued { .. }
            | Self::UserInputQueueRecalled { .. }
            | Self::UserInputQueueVoided {}
            | Self::SessionModelSelected { .. }
            | Self::DelegatedContextSeeded { .. }
            | Self::RunFinished { .. }
            | Self::ModelAttemptStarted { .. }
            | Self::AttemptAbandoned { .. }
            | Self::NativeReplayCut { .. }
            | Self::ModelTurnCommitted { .. }
            | Self::ModelUsageRecorded { .. }
            | Self::ModelFallback { .. }
            | Self::ToolCallStarted { .. }
            | Self::ToolCallTerminated { .. }
            | Self::ToolOutputElided { .. }
            | Self::ToolCallLinked { .. }
            | Self::DelegationReserved { .. }
            | Self::DelegationProgressed { .. }
            | Self::DelegationFinished { .. }
            | Self::DelegateChildTerminated { .. }
            | Self::ApprovalRequested { .. }
            | Self::ApprovalFinalized { .. }
            | Self::TreeApprovalGrantCommitted { .. }
            | Self::InternalAgentStarted { .. }
            | Self::InternalAgentUsageRecorded { .. }
            | Self::InternalAgentFinished { .. }
            | Self::InternalAgentFallback { .. }
            | Self::ContextCheckpointCommitted { .. }
            | Self::SessionTitleCommitted { .. } => false,
        }
    }

    /// Whether `skippable` on an envelope agrees with this payload's class.
    /// A step this build does not know may be either.
    #[must_use]
    pub fn skippable_matches(&self, skippable: bool) -> bool {
        matches!(
            self,
            Self::ApprovalStep {
                step: Open::Unknown { .. },
                ..
            }
        ) || skippable == self.is_audit()
    }

    /// Whether this payload is live-only stream output: delivered to
    /// subscribers as a [`TransientEvent`] and never appended to a session
    /// log. The committed model turn and the tool termination are the durable
    /// record of what these streamed.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::TextDelta { .. }
                | Self::ReasoningDelta { .. }
                | Self::InternalAgentTextDelta { .. }
                | Self::NativeCompactionStarted { .. }
                | Self::NativeCompactionFinished { .. }
                | Self::ToolCallProgress { .. }
        )
    }

    fn requires_run_id(&self) -> bool {
        !matches!(
            self,
            Self::SessionCreated { .. }
                | Self::SessionPermissionOverlaySet { .. }
                | Self::PermissionModeSet { .. }
                | Self::RuntimeAttached { .. }
                | Self::RuntimeDetached { .. }
                | Self::SkillLoaded { .. }
                | Self::SkillInvocationNoted { .. }
                | Self::Diagnostic { .. }
                | Self::GoalActivated { .. }
                | Self::GoalChecklistRevised { .. }
                | Self::GoalLifecycleChanged { .. }
                | Self::ProducerMessageAccepted { .. }
                | Self::ProducerMessageDiscarded { .. }
                | Self::SessionTitleCommitted { .. }
                | Self::SessionProjectSet { .. }
                | Self::DelegatedAttachmentsAdded { .. }
                | Self::UserInputAdmitted { .. }
                | Self::UserInputRecalled { .. }
                | Self::UserInputQueued { .. }
                | Self::UserInputQueueRecalled { .. }
                | Self::UserInputQueueVoided { .. }
                | Self::SessionModelSelected { .. }
                | Self::DelegatedContextSeeded { .. }
                | Self::DelegateChildTerminated { .. }
                | Self::RecordOmitted { .. }
                | Self::AcpSessionStarted { .. }
                | Self::AcpSessionUpdated { .. }
        )
    }
    fn validate(&self) -> Result<(), EventSchemaError> {
        match self {
            Self::AcpSessionStarted { record } => {
                let label = |value: &str| {
                    !value.is_empty()
                        && value.len() <= crate::MAX_ACP_LABEL_BYTES
                        && !value.chars().any(char::is_control)
                };
                if !label(&record.acp_session_id)
                    || record.forked_from.as_deref().is_some_and(|id| !label(id))
                    || record.cwd.is_empty()
                    || record.cwd.len() > 4096
                    || record.cwd.chars().any(char::is_control)
                {
                    return Err(EventSchemaError::InvalidAcpSession);
                }
            }
            Self::AcpSessionUpdated { update } => {
                let label = |value: &str| {
                    !value.is_empty()
                        && value.len() <= crate::MAX_ACP_LABEL_BYTES
                        && !value.chars().any(char::is_control)
                };
                if let Some(detached) = &update.detached
                    && (!label(&detached.acp_session_id)
                        || detached.message_id.as_deref().is_some_and(|id| !label(id)))
                {
                    return Err(EventSchemaError::InvalidAcpSession);
                }
            }
            Self::ElicitationRequested { request } if !request.is_valid() => {
                return Err(EventSchemaError::InvalidElicitation);
            }
            Self::ElicitationResolved { resolution }
                if resolution
                    .reason
                    .as_ref()
                    .is_some_and(|reason| reason.len() > crate::MAX_ELICITATION_TEXT_BYTES) =>
            {
                return Err(EventSchemaError::InvalidElicitation);
            }
            Self::GoalActivated { objective, .. } if objective.trim().is_empty() => {
                return Err(EventSchemaError::InvalidGoalEvent);
            }
            Self::GoalLifecycleChanged {
                status,
                selection: Some(_),
                ..
            } if *status != GoalStatus::Active => {
                return Err(EventSchemaError::InvalidGoalEvent);
            }
            Self::GoalChecklistRevised { items, .. } => {
                if items.iter().any(|item| item.description.trim().is_empty()) {
                    return Err(EventSchemaError::InvalidGoalEvent);
                }
            }
            Self::ProducerMessageAccepted {
                producer_owner,
                body,
                reminder,
                retained_output,
                ..
            } => {
                if body.validate_text_only().is_err() {
                    return Err(EventSchemaError::InvalidProducerMessage);
                }
                if reminder.as_ref().is_some_and(|identity| {
                        !matches!(producer_owner, ProducerOwner::Goal { goal_id } if *goal_id == identity.goal_id)
                    })
                {
                    return Err(EventSchemaError::InvalidProducerMessage);
                }
                if let Some(retained) = retained_output {
                    retained.validate()?;
                }
            }
            Self::ProducerMessagesClaimed { message_ids }
                if message_ids.is_empty()
                    || message_ids.iter().copied().collect::<HashSet<_>>().len()
                        != message_ids.len() =>
            {
                return Err(EventSchemaError::InvalidProducerMessage);
            }
            Self::ProducerMessagesReleased { claim_seq } if *claim_seq == 0 => {
                return Err(EventSchemaError::InvalidProducerMessage);
            }
            Self::SessionCreated {
                history_format,
                creation_selection,
                creation_agent,
                ..
            } => {
                if *history_format != HISTORY_FORMAT {
                    return Err(EventSchemaError::UnsupportedHistoryFormat);
                }
                creation_agent
                    .validate_selection(creation_selection)
                    .map_err(|_| EventSchemaError::SelectionAgentMismatch)?;
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
            Self::AgentMdLoaded { entries, .. } => {
                if entries.is_empty() || entries.len() > 2 {
                    return Err(EventSchemaError::InvalidAgentMd);
                }
                for entry in entries {
                    entry.validate()?;
                }
            }
            Self::MemoryLoaded { snapshot } => snapshot.validate()?,
            Self::ModelToolsPublished { tool_names, .. } if !valid_model_tool_names(tool_names) => {
                return Err(EventSchemaError::InvalidModelTools);
            }
            Self::Diagnostic {
                message,
                count,
                detail,
                ..
            } => {
                if message.trim().is_empty()
                    || *count == Some(0)
                    || detail.as_ref().is_some_and(|detail| {
                        writer_size_caps()
                            && json_len_exceeds(detail, MAX_DIAGNOSTIC_DETAIL_BYTES).unwrap_or(true)
                    })
                {
                    return Err(EventSchemaError::InvalidDiagnostic);
                }
            }
            Self::RunStarted {
                selection,
                agent,
                internal_agents,
                ..
            } => {
                agent
                    .validate_selection(selection)
                    .map_err(|_| EventSchemaError::InvalidSelectedSuffix)?;
                // One definition per kind. The agent each kind runs as is
                // stored as written, not checked against this build's.
                if internal_agents.len() != 3
                    || internal_agents
                        .windows(2)
                        .any(|pair| pair[0].kind >= pair[1].kind)
                    || internal_agents
                        .iter()
                        .any(|definition| definition.validate().is_err())
                {
                    return Err(EventSchemaError::InvalidAgentSnapshot);
                }
            }
            Self::MessageInjected { role, content } => {
                if content.is_blank()
                    || content.validate_text_only().is_err()
                    || *role == crate::ExtensionMessageRole::Tool
                {
                    return Err(EventSchemaError::InvalidJson);
                }
            }
            Self::UserInputTransformed {
                original_content,
                content,
            } => {
                if original_content.is_blank() || content.is_blank() || original_content == content
                {
                    return Err(EventSchemaError::InvalidJson);
                }
                original_content
                    .validate()
                    .and_then(|()| content.validate())
                    .map_err(|_| EventSchemaError::InvalidUserContent)?;
            }
            Self::UserInputAdmitted { content }
            | Self::UserInputSubmitted { content }
            | Self::UserInputRecalled { content, .. }
            | Self::UserInputQueued { content, .. } => content
                .validate()
                .map_err(|_| EventSchemaError::InvalidUserContent)?,
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
                start
                    .output
                    .validate()
                    .map_err(|_| EventSchemaError::InvalidToolTermination)?;
                if start.owner.model_turn_seq == 0 {
                    return Err(EventSchemaError::ZeroModelTurnSequence);
                }
                start
                    .presentation
                    .validate()
                    .map_err(|_| EventSchemaError::InvalidDisplay)?;
            }
            Self::ToolCallProgress {
                display: Some(Open::Known(delta)),
                ..
            } => delta.validate().map_err(|error| match error {
                crate::DisplayBlockError::TooLarge => EventSchemaError::ToolOutputTooLarge,
                _ => EventSchemaError::InvalidDisplay,
            })?,
            Self::ToolCallTerminated { termination } => termination.validate()?,
            Self::DelegationReserved {
                child_agent,
                cache_strategies,
                request,
                ..
            } => {
                request.validate()?;
                child_agent
                    .validate()
                    .map_err(|_| EventSchemaError::InvalidDelegationLifecycle)?;
                if cache_strategies.len() != child_agent.selected_suffix().len()
                    || cache_strategies
                        .iter()
                        .flatten()
                        .any(|strategy| strategy.validate().is_err())
                {
                    return Err(EventSchemaError::InvalidDelegationLifecycle);
                }
            }
            Self::DelegatedContextSeeded { turns, .. } => {
                if turns.is_empty()
                    || turns.len() > 65_536
                    || turns.iter().any(|turn| turn.text.is_empty())
                    || (writer_size_caps()
                        && turns.iter().map(|turn| turn.text.len()).sum::<usize>() > 65_536)
                {
                    return Err(EventSchemaError::InvalidDelegatedContext);
                }
            }
            Self::DelegationFinished {
                status, preview, ..
            } => {
                if !matches!(
                    status,
                    SessionStatus::Completed
                        | SessionStatus::Failed
                        | SessionStatus::Interrupted
                        | SessionStatus::Cancelled
                ) || preview.len() > crate::MAX_DELEGATE_PREVIEW_BYTES
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
            Self::ApprovalStep {
                step: Open::Known(step),
                ..
            } => match step {
                // A reason or source a newer build added reads as its
                // fallback, and the combination may be coherent only among
                // that build's values: as for the approval records, the
                // writer keeps coherence and a reader checks only what it
                // knows.
                ApprovalStep::Evaluated { decision } if !decision.holds_fallback() => decision
                    .validate()
                    .map_err(|_| EventSchemaError::InvalidApprovalLifecycle)?,
                ApprovalStep::Escalated { reason_code, .. }
                    if *reason_code != ApprovalReasonCode::Escalated =>
                {
                    return Err(EventSchemaError::InvalidApprovalLifecycle);
                }
                ApprovalStep::UserDecision {
                    decision, feedback, ..
                } if feedback.is_some() && *decision != ApprovalUserDecision::Reject => {
                    return Err(EventSchemaError::InvalidApprovalLifecycle);
                }
                ApprovalStep::DoomLoop { repetitions, .. } if *repetitions < 4 => {
                    return Err(EventSchemaError::InvalidApprovalCounter);
                }
                _ => {}
            },
            Self::ApprovalFinalized {
                outcome: Open::Known(outcome),
                ..
            } => match outcome {
                ApprovalFinalization::Decided { decision } => decision
                    .validate()
                    .map_err(|_| EventSchemaError::InvalidApprovalLifecycle)?,
                ApprovalFinalization::Cancelled { reason_code } => {
                    if !matches!(
                        reason_code,
                        ApprovalReasonCode::RequestCancelled
                            | ApprovalReasonCode::ApprovalExpired
                            | ApprovalReasonCode::OperationChanged
                            | ApprovalReasonCode::PreparedCapabilityLost
                    ) {
                        return Err(EventSchemaError::InvalidApprovalLifecycle);
                    }
                }
            },
            Self::TreeApprovalGrantCommitted { grant } => grant
                .validate()
                .map_err(|_| EventSchemaError::InvalidApprovalLifecycle)?,
            Self::InternalAgentStarted { backend, .. } => validate_internal_backend(backend)?,
            Self::InternalAgentFinished {
                outcome: Open::Known(InternalAgentOutcome::Failed { failure }),
                ..
            } => validate_internal_failure(failure)?,
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
            _ => {}
        }
        Ok(())
    }
}

/// The `diagnostic` kinds the plugin host records about a plugin.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PluginDiagnosticKind {
    EventDrop,
    InterceptionTimeout,
    /// The plugin was not connected or exited during the hook.
    InterceptionCrash,
    HookBlocked,
    OversizedEvent,
    InvalidModification,
    ContextMismatch,
    RateLimited,
    /// A plugin answered a hook with an error or a malformed result.
    InterceptionRejected,
}

/// Why a producer source is unavailable: the `diagnostic` kinds a plugin or
/// runtime producer source records.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProducerDiagnosticKind {
    /// The source reported that its recovery failed.
    RecoveryFailed,
    /// The source is configured off, or lost its connection before
    /// recovering.
    RecoveryDisabled,
}

/// The `diagnostic` kinds the engine records about itself.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EngineDiagnosticKind {
    /// Automatic context compaction failed repeatedly and is off for the
    /// rest of the run.
    AutoCompactionDisabled,
    /// An `AGENTS.md` file is over the size limit and contributes no
    /// context; `detail` carries its `path`, `byte_length`, and
    /// `attachment`.
    AgentMdTooLarge,
}

/// The `diagnostic` kinds the daemon records about what a plugin or runtime
/// sent; the diagnostic's source names the peer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PeerDiagnosticKind {
    /// A tool's presentation, result, or progress carried display blocks of
    /// a kind this build does not know, which it dropped.
    UnknownDisplayBlock,
}

macro_rules! diagnostic_kinds {
    ($($name:ident { $($variant:ident => $label:literal),+ $(,)? })+) => {
        $(
            impl $name {
                /// The kind's wire label.
                #[must_use]
                pub const fn as_str(self) -> &'static str {
                    match self {
                        $(Self::$variant => $label,)+
                    }
                }

                /// The kind as a `diagnostic` label.
                #[must_use]
                pub fn label(self) -> DiagnosticKind {
                    DiagnosticKind::new(self.as_str()).expect("diagnostic kind labels are valid")
                }
            }

            impl From<$name> for DiagnosticKind {
                fn from(kind: $name) -> Self {
                    kind.label()
                }
            }

            impl PartialEq<$name> for DiagnosticKind {
                fn eq(&self, kind: &$name) -> bool {
                    self.as_str() == kind.as_str()
                }
            }
        )+
    };
}

diagnostic_kinds! {
    PluginDiagnosticKind {
        EventDrop => "event_drop",
        InterceptionTimeout => "interception_timeout",
        InterceptionCrash => "interception_crash",
        HookBlocked => "hook_blocked",
        OversizedEvent => "oversized_event",
        InvalidModification => "invalid_modification",
        ContextMismatch => "context_mismatch",
        RateLimited => "rate_limited",
        InterceptionRejected => "interception_rejected",
    }
    ProducerDiagnosticKind {
        RecoveryFailed => "recovery_failed",
        RecoveryDisabled => "recovery_disabled",
    }
    EngineDiagnosticKind {
        AutoCompactionDisabled => "auto_compaction_disabled",
        AgentMdTooLarge => "agent_md_too_large",
    }
    PeerDiagnosticKind {
        UnknownDisplayBlock => "unknown_display_block",
    }
}

/// What a reader shows before a diagnostic's message, if anything: `plugin
/// <name>` for a plugin's own notice, `producer <source>` for a producer
/// source's, and nothing for the engine's.
#[must_use]
pub fn diagnostic_subject(source: &DiagnosticSource, kind: &DiagnosticKind) -> Option<String> {
    if *kind == ProducerDiagnosticKind::RecoveryFailed
        || *kind == ProducerDiagnosticKind::RecoveryDisabled
    {
        return Some(format!("producer {source}"));
    }
    if let Some(plugin) = source.plugin_name() {
        return Some(format!("plugin {plugin}"));
    }
    source
        .runtime_id()
        .map(|runtime_id| format!("runtime {runtime_id}"))
}

/// One record of a session log.
///
/// The envelope's `skippable` is not a field: it is derived from the
/// payload's class ([`EventPayload::is_audit`]), written only when true, and
/// checked against the payload when read.
///
/// The payload is an [`EventPayload`] wherever this build reads its own
/// logs. A client reads a daemon's events as [`ClientEvent`], whose payload
/// is open: an event of a type this build does not know, or one it cannot
/// read, is [`Open::Unknown`].
#[derive(Clone, Debug, JsonSchema, PartialEq)]
#[schemars(rename = "StoredEvent", transform = stored_event_schema)]
pub struct StoredEvent<P = EventPayload> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<EventOrigin>,
    pub session_id: SessionId,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<RunId>", required)]
    pub run_id: Option<RunId>,
    #[schemars(range(min = 1))]
    pub seq: u64,
    pub timestamp: Timestamp,
    /// The event. Readers accept event types they do not know.
    #[schemars(with = "Open<EventPayload>")]
    pub payload: P,
}

/// A session event as a client reads it from the daemon.
pub type ClientEvent = StoredEvent<Open<EventPayload>>;
/// Live-only stream output as a client reads it from the daemon.
pub type ClientTransientEvent = TransientEvent<Open<EventPayload>>;
/// An `events.subscription` message as a client reads it from the daemon.
pub type ClientSubscriptionMessage = EventSubscriptionMessage<Open<EventPayload>>;

/// What a stored event's envelope derives from its payload.
pub trait EventPayloadClass {
    /// Whether the envelope carries `"skippable": true`.
    fn skippable(&self) -> bool;
}

impl EventPayloadClass for EventPayload {
    fn skippable(&self) -> bool {
        self.is_audit()
    }
}

impl EventPayloadClass for Open<EventPayload> {
    /// An event of a type this build does not know is written without
    /// `skippable`: only its payload was kept.
    fn skippable(&self) -> bool {
        self.known().is_some_and(EventPayload::is_audit)
    }
}

/// A durable event a client could not read: of a type this build does not
/// know, or one it cannot decode. It still takes its place in the session's
/// sequence; a client shows nothing of it beyond, at most, its type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownEvent {
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub seq: u64,
    pub timestamp: Timestamp,
    pub origin: Option<EventOrigin>,
    /// The payload's `type`.
    pub tag: String,
}

impl ClientEvent {
    /// The event, when this build knows its type and could read it, or what
    /// a client keeps of it otherwise.
    pub fn into_known(self) -> Result<StoredEvent, UnknownEvent> {
        match self.payload {
            Open::Known(payload) => Ok(StoredEvent {
                engine_version: self.engine_version,
                origin: self.origin,
                session_id: self.session_id,
                run_id: self.run_id,
                seq: self.seq,
                timestamp: self.timestamp,
                payload,
            }),
            Open::Unknown { tag, .. } => Err(UnknownEvent {
                session_id: self.session_id,
                run_id: self.run_id,
                seq: self.seq,
                timestamp: self.timestamp,
                origin: self.origin,
                tag,
            }),
        }
    }
}

impl From<StoredEvent> for ClientEvent {
    fn from(event: StoredEvent) -> Self {
        Self {
            engine_version: event.engine_version,
            origin: event.origin,
            session_id: event.session_id,
            run_id: event.run_id,
            seq: event.seq,
            timestamp: event.timestamp,
            payload: Open::Known(event.payload),
        }
    }
}

impl From<TransientEvent> for ClientTransientEvent {
    fn from(event: TransientEvent) -> Self {
        Self {
            session_id: event.session_id,
            run_id: event.run_id,
            after_seq: event.after_seq,
            timestamp: event.timestamp,
            payload: Open::Known(event.payload),
        }
    }
}

impl From<EventSubscriptionMessage> for ClientSubscriptionMessage {
    fn from(message: EventSubscriptionMessage) -> Self {
        match message {
            EventSubscriptionMessage::Event { event } => Self::Event {
                event: std::sync::Arc::new(std::sync::Arc::unwrap_or_clone(event).into()),
            },
            EventSubscriptionMessage::Transient { event } => Self::Transient {
                event: std::sync::Arc::new(std::sync::Arc::unwrap_or_clone(event).into()),
            },
            EventSubscriptionMessage::Gap {
                session_id,
                last_delivered_seq,
            } => Self::Gap {
                session_id,
                last_delivered_seq,
            },
            EventSubscriptionMessage::Rewound {
                session_id,
                through_seq,
            } => Self::Rewound {
                session_id,
                through_seq,
            },
        }
    }
}

fn stored_event_schema(schema: &mut Schema) {
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert(
            "skippable".into(),
            serde_json::json!({
                "description": "Present and true exactly on audit events: a reader that does not know the payload's type may skip the record.",
                "type": "boolean",
            }),
        );
    }
}

impl<P: Serialize + EventPayloadClass> Serialize for StoredEvent<P> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Wire<'a, P> {
            #[serde(skip_serializing_if = "Option::is_none")]
            engine_version: Option<&'a String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            origin: Option<&'a EventOrigin>,
            session_id: SessionId,
            run_id: Option<RunId>,
            seq: u64,
            timestamp: Timestamp,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            skippable: bool,
            payload: &'a P,
        }
        Wire {
            engine_version: self.engine_version.as_ref(),
            origin: self.origin.as_ref(),
            session_id: self.session_id,
            run_id: self.run_id,
            seq: self.seq,
            timestamp: self.timestamp,
            skippable: self.payload.skippable(),
            payload: &self.payload,
        }
        .serialize(serializer)
    }
}

impl StoredEvent {
    /// Whether the envelope carries `"skippable": true`: exactly the audit
    /// events.
    #[must_use]
    pub fn skippable(&self) -> bool {
        self.payload.is_audit()
    }

    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if let Some(origin) = &self.origin {
            EventOrigin::new(origin.as_str())?;
        }
        if self.seq == 0 {
            return Err(EventSchemaError::ZeroEventSequence);
        }
        if self.payload.is_transient() {
            return Err(EventSchemaError::StoredTransientPayload);
        }
        if matches!(self.payload, EventPayload::SessionCreated { .. })
            && (self.seq != 1 || self.run_id.is_some())
        {
            return Err(EventSchemaError::InvalidSessionCreatedEnvelope);
        }
        if self.payload.requires_run_id() && self.run_id.is_none() {
            return Err(EventSchemaError::MissingRunId);
        }
        if let EventPayload::ProducerMessageConsumed { run_id, .. } = &self.payload
            && self.run_id != Some(*run_id)
        {
            return Err(EventSchemaError::InvalidProducerMessage);
        }
        self.payload.validate()?;
        Ok(())
    }
}

/// The kind of part a model attempt streams.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamedOutputKind {
    Text,
    Reasoning,
}

/// A live-only stream event ([`EventPayload::is_transient`]). It is never
/// stored, so it has no sequence of its own: it follows the durable event
/// `after_seq` and precedes the next one. Durable sequences stay gap-free and
/// replay cursors never point at it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[schemars(rename = "TransientEvent")]
pub struct TransientEvent<P = EventPayload> {
    pub session_id: SessionId,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<RunId>", required)]
    pub run_id: Option<RunId>,
    /// The session's last durable sequence when this event was published.
    #[schemars(range(min = 1))]
    pub after_seq: u64,
    pub timestamp: Timestamp,
    #[schemars(with = "Open<EventPayload>")]
    pub payload: P,
}
impl TransientEvent {
    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if self.after_seq == 0 {
            return Err(EventSchemaError::ZeroEventSequence);
        }
        if !self.payload.is_transient() {
            return Err(EventSchemaError::NonTransientPayload);
        }
        if self.payload.requires_run_id() && self.run_id.is_none() {
            return Err(EventSchemaError::MissingRunId);
        }
        self.payload.validate()
    }
}

impl StoredEvent {
    /// Decodes one JSONL record that needs no repair, typed in a single pass
    /// over its bytes. Members this build does not know are ignored, as for
    /// any information. `None` means the record has to go through the
    /// tolerant reader, which reports what it repaired or skipped.
    #[must_use]
    pub fn decode_strict(line: &[u8]) -> Option<Self> {
        #[derive(Deserialize)]
        struct Strict {
            #[serde(default)]
            engine_version: Option<String>,
            #[serde(default)]
            origin: Option<EventOrigin>,
            #[serde(default)]
            skippable: bool,
            session_id: SessionId,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            run_id: Option<RunId>,
            seq: u64,
            timestamp: Timestamp,
            payload: EventPayload,
        }
        let strict = serde_json::from_slice::<Strict>(line).ok()?;
        if !strict.payload.skippable_matches(strict.skippable) {
            return None;
        }
        let event = Self {
            engine_version: strict.engine_version,
            origin: strict.origin,
            session_id: strict.session_id,
            run_id: strict.run_id,
            seq: strict.seq,
            timestamp: strict.timestamp,
            payload: strict.payload,
        };
        event.validate().ok()?;
        Some(event)
    }
}

/// A stored event's envelope, decoded apart from its payload so a tolerant
/// reader can pair it with a payload it already decoded.
#[derive(Deserialize)]
pub struct StoredEventEnvelope {
    #[serde(default)]
    engine_version: Option<String>,
    #[serde(default)]
    origin: Option<EventOrigin>,
    #[serde(default)]
    skippable: bool,
    session_id: SessionId,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    run_id: Option<RunId>,
    seq: u64,
    timestamp: Timestamp,
}

impl StoredEventEnvelope {
    /// Whether the envelope carries `"skippable": true`.
    #[must_use]
    pub const fn skippable(&self) -> bool {
        self.skippable
    }

    /// Pairs the envelope with its payload. The result still has to pass
    /// [`StoredEvent::validate`], and the payload's class has to agree with
    /// [`Self::skippable`] ([`EventPayload::skippable_matches`]).
    #[must_use]
    pub fn with_payload(self, payload: EventPayload) -> StoredEvent {
        StoredEvent {
            engine_version: self.engine_version,
            origin: self.origin,
            session_id: self.session_id,
            run_id: self.run_id,
            seq: self.seq,
            timestamp: self.timestamp,
            payload,
        }
    }
}

impl<'de> Deserialize<'de> for StoredEvent {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            engine_version: Option<String>,
            #[serde(default)]
            origin: Option<EventOrigin>,
            #[serde(default)]
            skippable: bool,
            session_id: SessionId,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            run_id: Option<RunId>,
            seq: u64,
            timestamp: Timestamp,
            payload: Value,
        }
        let w = Wire::deserialize(d)?;
        let skippable = w.skippable;
        let value = Self {
            engine_version: w.engine_version,
            origin: w.origin,
            session_id: w.session_id,
            run_id: w.run_id,
            seq: w.seq,
            timestamp: w.timestamp,
            payload: deserialize_event_payload_best_effort(&w.payload)
                .map_err(serde::de::Error::custom)?
                .payload,
        };
        if !value.payload.skippable_matches(skippable) {
            return Err(serde::de::Error::custom(
                EventSchemaError::SkippableMismatch,
            ));
        }
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

/// A client's read of a daemon's event: the envelope must decode, and a
/// payload of a type this build does not know, or one it cannot read even
/// best-effort, is kept as [`Open::Unknown`] with its raw JSON. The daemon
/// already validated what it sends, so a client does not validate again.
impl<'de> Deserialize<'de> for ClientEvent {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            engine_version: Option<String>,
            #[serde(default)]
            origin: Option<EventOrigin>,
            session_id: SessionId,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            run_id: Option<RunId>,
            seq: u64,
            timestamp: Timestamp,
            payload: Value,
        }
        let w = Wire::deserialize(d)?;
        let tag = match w.payload.get("type") {
            Some(Value::String(tag)) => tag.clone(),
            _ => return Err(serde::de::Error::missing_field("type")),
        };
        let payload = if EventPayload::is_known_tag(&tag) {
            match deserialize_event_payload_best_effort(&w.payload) {
                Ok(read) => Open::Known(read.payload),
                Err(_) => Open::Unknown {
                    tag,
                    raw: w.payload,
                },
            }
        } else {
            Open::Unknown {
                tag,
                raw: w.payload,
            }
        };
        Ok(Self {
            engine_version: w.engine_version,
            origin: w.origin,
            session_id: w.session_id,
            run_id: w.run_id,
            seq: w.seq,
            timestamp: w.timestamp,
            payload,
        })
    }
}

crate::open_enum!(tagged EventSubscriptionMessage, tag = "type", tags = [
    "event",
    "gap",
    "rewound",
    "transient",
]);
crate::open_enum!(tagged ClientSubscriptionMessage, tag = "type", tags = [
    "event",
    "gap",
    "rewound",
    "transient",
]);

const MAX_DEGRADED_FIELDS: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub struct EventPayloadRead {
    pub payload: EventPayload,
    pub degraded_fields: Vec<String>,
}

/// Decodes an event payload, replacing only leaves which Serde identifies as
/// nullable fields. Required-field and unknown-variant failures remain errors.
/// The original event log is not rewritten.
///
/// A payload that decodes strictly is read in place; only a payload that
/// needs repair is copied. Each repair locates its leaf first and then decodes
/// the payload once more, so repairing stays linear in the payload's size:
/// `serde_path_to_error` names the leaf where it can, and where serde buffered
/// the value first (inside an internally tagged enum, which every payload is)
/// the leaf is found by the value serde's error names.
pub fn deserialize_event_payload_best_effort(value: &Value) -> Result<EventPayloadRead, String> {
    if let Ok(payload) = EventPayload::deserialize(value) {
        return Ok(EventPayloadRead {
            payload,
            degraded_fields: Vec::new(),
        });
    }
    let mut degraded_fields = Vec::new();
    let mut value = value.clone();
    let mut previous_path = None;
    for _ in 0..=MAX_DEGRADED_FIELDS {
        count_degradation_decode();
        let error = match serde_path_to_error::deserialize::<_, EventPayload>(&value) {
            Ok(payload) => {
                return Ok(EventPayloadRead {
                    payload,
                    degraded_fields,
                });
            }
            Err(error) => error,
        };
        let path = error.path().to_string();
        if path != "." && previous_path.as_deref() == Some(path.as_str()) {
            return Err(error.to_string());
        }
        let degraded = if path == "." {
            replace_named_leaf(&mut value, &error.inner().to_string())
        } else if replace_path_with_null(&mut value, error.path()) {
            Some(path.clone())
        } else {
            None
        };
        let Some(degraded) = degraded else {
            return Err(error.to_string());
        };
        previous_path = Some(path);
        degraded_fields.push(degraded);
    }
    Err(format!(
        "event payload exceeds the {MAX_DEGRADED_FIELDS}-field degradation limit"
    ))
}

#[cfg(test)]
thread_local! {
    /// Test-only: payload decodes the best-effort reader ran to degrade.
    pub(crate) static DEGRADATION_DECODES: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

fn count_degradation_decode() {
    #[cfg(test)]
    DEGRADATION_DECODES.with(|decodes| decodes.set(decodes.get() + 1));
}

/// The most candidate leaves one repair tries, each with one decode.
const MAX_LEAF_CANDIDATES: usize = 16;

#[derive(Clone)]
enum ValuePathSegment {
    Key(String),
    Index(usize),
}

/// What serde's error names as the value it could not take.
enum NamedValue {
    /// `invalid type` or `invalid value`: a leaf equal to this one.
    Leaf(Value),
    /// `invalid type` for a sequence or a map: any array or object.
    Array,
    Object,
    /// `invalid length N`: an array of that length.
    Length(usize),
    /// `missing field`: an object without it, which takes a null.
    Missing(String),
}

impl NamedValue {
    /// Reads serde's standard messages, which spell the value out with
    /// [`serde::de::Unexpected`]; a custom message names none.
    fn parse(message: &str) -> Option<Self> {
        if let Some(rest) = message.strip_prefix("missing field `") {
            return rest
                .split('`')
                .next()
                .map(|field| Self::Missing(field.to_owned()));
        }
        if let Some(rest) = message.strip_prefix("unknown variant `") {
            return rest
                .split('`')
                .next()
                .map(|variant| Self::Leaf(Value::String(variant.to_owned())));
        }
        if let Some(rest) = message.strip_prefix("invalid length ") {
            return rest
                .split(',')
                .next()
                .and_then(|length| length.trim().parse().ok())
                .map(Self::Length);
        }
        let unexpected = message
            .strip_prefix("invalid type: ")
            .or_else(|| message.strip_prefix("invalid value: "))?;
        let quoted = |prefix: &str| {
            unexpected
                .strip_prefix(prefix)
                .and_then(|rest| rest.split('`').next())
        };
        if let Some(rest) = unexpected.strip_prefix("string ") {
            // The string as `Debug` spells it; decoding it as JSON covers the
            // escapes the two share, which is every one serde emits for
            // stored text.
            let end = rest.rfind("\", expected").or_else(|| rest.rfind('"'))?;
            return serde_json::from_str::<String>(&rest[..=end])
                .ok()
                .map(|text| Self::Leaf(Value::String(text)));
        }
        if let Some(number) = quoted("integer `") {
            return serde_json::from_str::<serde_json::Number>(number)
                .ok()
                .map(|number| Self::Leaf(Value::Number(number)));
        }
        if let Some(number) = quoted("floating point `") {
            return number
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(|number| Self::Leaf(Value::Number(number)));
        }
        if let Some(boolean) = quoted("boolean `") {
            return boolean
                .parse()
                .ok()
                .map(|boolean| Self::Leaf(Value::Bool(boolean)));
        }
        if unexpected.starts_with("sequence") {
            return Some(Self::Array);
        }
        if unexpected.starts_with("map") {
            return Some(Self::Object);
        }
        None
    }

    fn matches(&self, value: &Value) -> bool {
        match (self, value) {
            (Self::Leaf(Value::Number(named)), Value::Number(number)) => {
                named == number
                    || named
                        .as_f64()
                        .is_some_and(|named| Some(named) == number.as_f64())
            }
            (Self::Leaf(named), value) => named == value,
            (Self::Array, Value::Array(_)) | (Self::Object, Value::Object(_)) => true,
            (Self::Length(length), Value::Array(items)) => items.len() == *length,
            (Self::Missing(field), Value::Object(members)) => !members.contains_key(field),
            _ => false,
        }
    }
}

/// Nulls the leaf serde's `message` names, when nulling it lets the payload
/// decode further, and returns its path. The candidates are the values equal
/// to the named one, in document order, at most [`MAX_LEAF_CANDIDATES`] of
/// them; each costs one decode.
fn replace_named_leaf(value: &mut Value, message: &str) -> Option<String> {
    let named = NamedValue::parse(message)?;
    let mut candidates = Vec::new();
    collect_named_paths(value, &named, &mut Vec::new(), &mut candidates);
    for mut path in candidates {
        if let NamedValue::Missing(field) = &named {
            path.push(ValuePathSegment::Key(field.clone()));
        }
        let mut candidate = value.clone();
        if !set_value_path(&mut candidate, &path, Value::Null) {
            continue;
        }
        count_degradation_decode();
        match serde_json::from_value::<EventPayload>(candidate.clone()) {
            Ok(_) => {
                *value = candidate;
                return Some(display_value_path(&path));
            }
            Err(error)
                if error.to_string() != message
                    && !error.to_string().contains("invalid type: null") =>
            {
                *value = candidate;
                return Some(display_value_path(&path));
            }
            Err(_) => {}
        }
    }
    None
}

/// The paths of the values `named` matches, never the payload's tag, at most
/// [`MAX_LEAF_CANDIDATES`] of them.
fn collect_named_paths(
    value: &Value,
    named: &NamedValue,
    current: &mut Vec<ValuePathSegment>,
    paths: &mut Vec<Vec<ValuePathSegment>>,
) {
    if paths.len() >= MAX_LEAF_CANDIDATES {
        return;
    }
    if !current.is_empty() && named.matches(value) {
        paths.push(current.clone());
    } else if current.is_empty() && matches!(named, NamedValue::Missing(_)) && named.matches(value)
    {
        paths.push(Vec::new());
    }
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if current.is_empty() && key == "type" {
                    continue;
                }
                current.push(ValuePathSegment::Key(key.clone()));
                collect_named_paths(child, named, current, paths);
                current.pop();
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter().enumerate() {
                current.push(ValuePathSegment::Index(index));
                collect_named_paths(child, named, current, paths);
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

/// One `events.subscription` notification. A client reads it open: a
/// message type it does not know is ignored.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(bound(
    serialize = "P: Serialize + EventPayloadClass",
    deserialize = "StoredEvent<P>: Deserialize<'de>, TransientEvent<P>: Deserialize<'de>"
))]
#[schemars(rename = "EventSubscriptionMessage", extend("x-open" = true))]
pub enum EventSubscriptionMessage<P = EventPayload> {
    /// A durable event, shared with the log and every other tail.
    Event {
        event: std::sync::Arc<StoredEvent<P>>,
    },
    /// Live-only stream output. It does not advance the subscription cursor
    /// and is never replayed.
    Transient {
        event: std::sync::Arc<TransientEvent<P>>,
    },
    Gap {
        session_id: SessionId,
        last_delivered_seq: u64,
    },
    /// The session's log was cut after `through_seq` by a revert: the events
    /// past it are gone and their sequence numbers are reused by the next
    /// appends. A follower drops what it holds past `through_seq` and replays
    /// the session in full.
    Rewound {
        session_id: SessionId,
        through_seq: u64,
    },
}
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
    Single,
    Named(#[serde(deserialize_with = "crate::tool_output::deserialize_stream_name")] String),
}

impl OutputStream {
    pub fn name(&self) -> &str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Single => "",
            Self::Named(name) => name,
        }
    }

    pub fn from_channel(name: Option<&str>) -> Self {
        match name {
            None => Self::Single,
            Some("stdout") => Self::Stdout,
            Some("stderr") => Self::Stderr,
            Some(name) => Self::Named(name.to_owned()),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputDelta {
    pub call_id: ToolCallId,
    pub stream: OutputStream,
    pub byte_offset: u64,
    pub data: String,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputGap {
    pub call_id: ToolCallId,
    pub stream: OutputStream,
    pub next_offset: u64,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSnapshot {
    pub call_id: ToolCallId,
    pub start_offset: u64,
    pub end_offset: u64,
    pub chunks: Vec<OutputDelta>,
}

impl crate::RetainedToolOutput {
    pub fn validate(&self) -> Result<(), EventSchemaError> {
        if let Some(reference) = &self.reference {
            reference.validate()?;
        }
        if self.streams.is_empty() || self.streams.len() > crate::MAX_TOOL_STREAMS {
            return Err(EventSchemaError::ToolOutputTooLarge);
        }
        for stream in &self.streams {
            if let Some(reference) = &stream.reference {
                reference.validate()?;
            }
            if !stream.artifact_matches() {
                return Err(EventSchemaError::InvalidArtifactReference);
            }
            if let Some(name) = &stream.name {
                crate::validate_tool_stream_name(name)
                    .map_err(|_| EventSchemaError::InvalidArtifactReference)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventSchemaError {
    InvalidAcpSession,
    InvalidElicitation,
    InvalidGoalEvent,
    InvalidProducerMessage,
    InvalidOrigin,
    InvalidAgentMd,
    InvalidMemorySnapshot,
    InvalidDiagnostic,
    UnsupportedHistoryFormat,
    SkippableMismatch,
    InvalidModelTools,
    EmptyTitle,
    TitleTooLong,
    TitleControlCharacter,
    InvalidArtifactReference,
    InvalidToolElision,
    ToolOutputTooLarge,
    ToolMetadataTooLarge,
    TooManyAttachments,
    InvalidAttachmentFilename,
    TooManyToolEmittedMessages,
    EmptyToolEmittedMessage,
    EmptyToolEmittedText,
    ToolEmittedTextTooLarge,
    InvalidJson,
    InvalidUserContent,
    InvalidDisplay,
    InvalidToolBackground,
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
    InvalidSessionPermissionOverlay,
    InvalidSkillEvent,
    ZeroEventSequence,
    InvalidSessionCreatedEnvelope,
    MissingRunId,
    StoredTransientPayload,
    NonTransientPayload,
}
impl fmt::Display for EventSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidAcpSession => {
                "acp session ID and cwd must be nonempty bounded control-free text"
            }
            Self::InvalidElicitation => {
                "an elicitation has at most one of a form and a URL, bounded text, and distinct field names"
            }
            Self::InvalidGoalEvent => "goal objective/items must be nonblank",
            Self::InvalidProducerMessage => {
                "producer owner, reminder identity, or consumed run is invalid"
            }
            Self::InvalidOrigin => "event origin is invalid",
            Self::InvalidAgentMd => "AGENTS.md context event is invalid",
            Self::InvalidMemorySnapshot => "memory snapshot event is invalid",
            Self::InvalidDiagnostic => "diagnostic message, count, or detail is invalid",
            Self::UnsupportedHistoryFormat => {
                "session history format is not the one this build writes"
            }
            Self::SkippableMismatch => "envelope `skippable` does not match the event's class",
            Self::InvalidModelTools => "published model tool names are invalid or too many",
            Self::EmptyTitle => "session title must not be blank",
            Self::TitleTooLong => "session title exceeds 512 bytes",
            Self::TitleControlCharacter => "session title must not contain control characters",
            Self::InvalidArtifactReference => {
                "artifact reference must be a bounded artifact:// URI"
            }
            Self::InvalidToolElision => "tool output elision is invalid",
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
            Self::InvalidUserContent => "user content names an invalid artifact",
            Self::InvalidDisplay => "a display block is invalid",
            Self::InvalidToolBackground => {
                "a background state names a job exactly in the job state"
            }
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
            Self::InvalidSessionPermissionOverlay => "session permission overlay is invalid",
            Self::InvalidSkillEvent => "skill event is invalid",
            Self::ZeroEventSequence => "event sequence must be positive",
            Self::StoredTransientPayload => "live-only stream payloads are never stored",
            Self::NonTransientPayload => "transient events carry only live-only payloads",
            Self::InvalidSessionCreatedEnvelope => {
                "SessionCreated must be sequence 1 with no run_id"
            }
            Self::MissingRunId => "run-scoped event requires run_id",
        })
    }
}
impl std::error::Error for EventSchemaError {}

impl crate::DisplayAudience {
    /// What the client receives of `payload`'s display blocks: the call's
    /// presentation, its result's display, and a progress `replace`. `None`
    /// when that is `payload` unchanged.
    #[must_use]
    pub fn payload(&self, payload: &EventPayload) -> Option<EventPayload> {
        match payload {
            EventPayload::ToolCallStarted { start } => {
                let arguments = self.blocks(&start.presentation.arguments);
                let preview = self.blocks(&start.presentation.preview);
                if arguments.is_none() && preview.is_none() {
                    return None;
                }
                let mut start = start.clone();
                if let Some(arguments) = arguments {
                    start.presentation.arguments = arguments;
                }
                if let Some(preview) = preview {
                    start.presentation.preview = preview;
                }
                Some(EventPayload::ToolCallStarted { start })
            }
            EventPayload::ToolCallTerminated { termination } => {
                let display = self.blocks(&termination.result.as_ref()?.display)?;
                let mut termination = termination.clone();
                if let Some(result) = termination.result.as_mut() {
                    result.display = display;
                }
                Some(EventPayload::ToolCallTerminated { termination })
            }
            EventPayload::ToolCallProgress {
                tool_call_id,
                message,
                display: Some(Open::Known(crate::DisplayDelta::Replace { blocks })),
            } => Some(EventPayload::ToolCallProgress {
                tool_call_id: *tool_call_id,
                message: message.clone(),
                display: Some(Open::Known(crate::DisplayDelta::Replace {
                    blocks: self.blocks(blocks)?,
                })),
            }),
            _ => None,
        }
    }

    /// What the client receives of a stored event; the same event, shared,
    /// when nothing changes.
    #[must_use]
    pub fn stored(&self, event: &std::sync::Arc<StoredEvent>) -> std::sync::Arc<StoredEvent> {
        match self.payload(&event.payload) {
            None => std::sync::Arc::clone(event),
            Some(payload) => std::sync::Arc::new(StoredEvent {
                payload,
                ..StoredEvent::clone(event)
            }),
        }
    }

    /// What the client receives of one `events.subscription` message.
    #[must_use]
    pub fn message(&self, message: EventSubscriptionMessage) -> EventSubscriptionMessage {
        match message {
            EventSubscriptionMessage::Event { event } => EventSubscriptionMessage::Event {
                event: self.stored(&event),
            },
            EventSubscriptionMessage::Transient { event } => match self.payload(&event.payload) {
                None => EventSubscriptionMessage::Transient { event },
                Some(payload) => EventSubscriptionMessage::Transient {
                    event: std::sync::Arc::new(TransientEvent {
                        payload,
                        ..TransientEvent::clone(&event)
                    }),
                },
            },
            message => message,
        }
    }
}

#[cfg(test)]
mod size_limit_tests {
    use super::*;

    #[test]
    fn json_size_limits_count_serialized_bytes_at_the_boundary() {
        // `"abcdefgh"` is ten bytes of JSON.
        let value = serde_json::json!("abcdefgh");
        assert!(!json_len_exceeds(&value, 10).unwrap());
        assert!(json_len_exceeds(&value, 9).unwrap());
        assert!(validate_payload(&value, 10).is_ok());
        assert!(matches!(
            validate_payload(&value, 9),
            Err(NativeArtifactError::PayloadTooLarge {
                size: 10,
                maximum: 9
            })
        ));
    }
}
