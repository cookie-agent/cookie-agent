//! Projects (`docs/site/specs/projects.md`): named bodies of work the daemon
//! keeps, which sessions belong to. Client protocol 1.8.0.

use std::{borrow::Cow, fmt};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    AgentId, CapabilityName, ProjectId, ProjectName, RuntimeId, SessionId, SessionMeta,
    agent::{AgentPresetNameSchema, deserialize_optional_preset},
};

pub const PROJECTS_LIST_METHOD: &str = "projects.list";
pub const PROJECT_GET_METHOD: &str = "project.get";
pub const PROJECT_CREATE_METHOD: &str = "project.create";
pub const PROJECT_UPDATE_METHOD: &str = "project.update";
pub const PROJECT_DELETE_METHOD: &str = "project.delete";
pub const PROJECTS_CHANGED_METHOD: &str = "projects.changed";
pub const SESSION_PROJECT_SET_METHOD: &str = "session.project.set";

const fn attach_global() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn attaches_global(global: &bool) -> bool {
    *global
}

/// Reads a member that is present as `Some`, so that `Option<Option<T>>`
/// tells an absent member (`None`) from `null` (`Some(None)`).
fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// What a new session in the project starts with. `global` and `runtimes`
/// have the shape of `session.create`'s members of the same names, and a
/// `session.create` that names the project and omits `runtimes` attaches
/// them. `agent` and `preset` name an agent and its preset as a
/// [`crate::RunSelection`] does; an agent without a preset is one of the
/// shared registry. The daemon never applies them: clients put them in a
/// new session's selection.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProjectDefaults {
    #[serde(
        default,
        deserialize_with = "deserialize_optional_preset",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<AgentPresetNameSchema>")]
    pub preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    /// Whether a new session attaches the global runtime; `true` when
    /// omitted.
    #[serde(default = "attach_global", skip_serializing_if = "attaches_global")]
    pub global: bool,
    /// The runtimes a new session attaches besides `global`, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub runtimes: Vec<ProjectRuntime>,
}

impl Default for ProjectDefaults {
    fn default() -> Self {
        Self {
            preset: None,
            agent: None,
            global: true,
            runtimes: Vec::new(),
        }
    }
}

/// One runtime a project's new sessions attach, as `session.create`'s
/// `runtimes` names it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProjectRuntime {
    pub runtime_id: RuntimeId,
    /// The capability to attach; the runtime's only capability when
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<CapabilityName>,
    /// Required exactly when the capability takes a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 4096))]
    pub workspace: Option<String>,
}

/// One project.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProjectInfo {
    pub id: ProjectId,
    /// The project's name: its memory folder's name and the `<name>` of its
    /// `project://<name>/…` memories.
    pub name: ProjectName,
    /// One emoji clients show before the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    /// The first 16 hex digits of the SHA-256 of the project's
    /// `project.toml` and `AGENTS.md`; every change sends it back.
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<ProjectDefaults>,
    /// Whether the project has an `AGENTS.md`.
    #[serde(default)]
    pub has_agents_md: bool,
    /// The root sessions in the project.
    pub sessions: u64,
    /// The memory files in the project's memory folder.
    pub memories: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectsListParams {}

/// Every project, in name order.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProjectsListResult {
    pub projects: Vec<ProjectInfo>,
}

/// Read one project with its `AGENTS.md`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectGetParams {
    pub project: ProjectId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProjectGetResult {
    pub project: ProjectInfo,
    /// The project's `AGENTS.md`; absent when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md: Option<ProjectAgentsMd>,
}

/// A project's `AGENTS.md`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProjectAgentsMd {
    pub text: String,
    /// The first 16 hex digits of the SHA-256 of the file's bytes.
    pub revision: String,
}

/// Create a project, with an empty `overview` memory.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCreateParams {
    /// A name no other project has.
    pub name: ProjectName,
    /// Exactly one emoji: one extended grapheme cluster of at most 32
    /// bytes that Unicode marks as an emoji, not ASCII, whitespace, or a
    /// control character.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 32))]
    pub emoji: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<ProjectDefaults>,
}

/// Change a project at the revision last read. Only the members present
/// change.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectUpdateParams {
    pub project: ProjectId,
    /// [`ProjectInfo::revision`] as last read; a project changed since is
    /// refused with `project_changed`.
    #[schemars(length(min = 1, max = 64))]
    pub revision: String,
    /// A new name: its memory folder moves with it, and old
    /// `project://<old>/…` URLs no longer resolve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<ProjectName>,
    /// A new emoji, or `null` to clear it.
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<ProjectEmojiChangeSchema>")]
    pub emoji: Option<Option<String>>,
    /// New defaults, or `null` to clear them.
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub defaults: Option<Option<ProjectDefaults>>,
    /// The `AGENTS.md` text, which replaces the file; an empty string
    /// removes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md: Option<String>,
}

impl ProjectUpdateParams {
    /// An update of `project` at `revision` that changes nothing yet.
    #[must_use]
    pub const fn new(project: ProjectId, revision: String) -> Self {
        Self {
            project,
            revision,
            name: None,
            emoji: None,
            defaults: None,
            agents_md: None,
        }
    }
}

struct ProjectEmojiChangeSchema;

impl JsonSchema for ProjectEmojiChangeSchema {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ProjectEmojiChange")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": ["string", "null"], "minLength": 1, "maxLength": 32})
    }
}

/// The project after a create or an update.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProjectResult {
    pub project: ProjectInfo,
}

/// Delete a project: every session tree in it, its whole memory folder, its
/// `AGENTS.md`, and its defaults. Refused while a session of the project
/// runs.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDeleteParams {
    pub project: ProjectId,
    /// [`ProjectInfo::revision`] as last read.
    #[schemars(length(min = 1, max = 64))]
    pub revision: String,
}

/// What a delete removed.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProjectDeleteResult {
    /// The root sessions deleted, each with its whole tree.
    pub deleted_sessions: u64,
    /// The memory files deleted with the memory folder.
    pub deleted_memories: u64,
}

/// A project was created, changed, or deleted; refetch `projects.list`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProjectsChangedNotification {}

/// Move a root session into a project, or out of any with `null`. Allowed
/// while the session runs; it takes effect at the next root admission. The
/// session's attachments and agent never change.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionProjectSetParams {
    pub session_id: SessionId,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ProjectId>", required)]
    pub project: Option<ProjectId>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionProjectSetResult {
    pub session: SessionMeta,
}

/// Which sessions `session.list` returns by project: those of one project,
/// or (`"none"`) those in no project, a project that no longer exists
/// included.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ProjectFilter {
    /// Sessions in no project.
    None,
    Project(ProjectId),
}

impl ProjectFilter {
    /// The filter's spelling for sessions in no project.
    pub const NONE: &'static str = "none";

    /// Whether a session whose project is `project` passes.
    #[must_use]
    pub fn admits(&self, project: Option<&ProjectId>) -> bool {
        match self {
            Self::None => project.is_none(),
            Self::Project(wanted) => project == Some(wanted),
        }
    }
}

impl fmt::Display for ProjectFilter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str(Self::NONE),
            Self::Project(project) => formatter.write_str(project.as_str()),
        }
    }
}

impl Serialize for ProjectFilter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ProjectFilter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == Self::NONE {
            return Ok(Self::None);
        }
        ProjectId::new(value)
            .map(Self::Project)
            .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ProjectFilter {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ProjectFilter")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": 64,
            "pattern": "^[a-z0-9][a-z0-9-]{0,63}$",
            "description": "A project ID, or `none` for sessions in no project."
        })
    }
}
