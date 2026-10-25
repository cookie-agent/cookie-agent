//! Projects in the clients (`docs/site/specs/projects.md` section 10): the
//! session list's project sections, the project pickers, the emoji check
//! the daemon makes, and what choosing a project does to a new-session
//! draft: its default attachments and its default agent and preset.

use cookie_agent_protocol::{
    AgentDescriptor, AgentMode, ClientError, ModelSelection, ProjectDefaults, ProjectId,
    ProjectInfo, ProjectRuntime, RunSelection, RuntimeAttachRequest, RuntimeId, SessionId,
    SessionMeta, SessionOrigin,
};

use crate::rows::order_sessions;

/// The client protocol minor from which the daemon keeps projects:
/// `projects.list`, the `project.*` methods, `SessionMeta.project`, and
/// `session.create.project`.
pub const PROJECTS_MINOR: u32 = 8;

/// Whether a daemon whose handshake reported `version` keeps projects.
/// Before the handshake nothing is known.
#[must_use]
pub fn daemon_has_projects(version: Option<cookie_agent_protocol::SemVer>) -> bool {
    version.is_some_and(|version| {
        version
            .shared_minor(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
            .is_some_and(|minor| minor >= PROJECTS_MINOR)
    })
}

/// What a client calls a session outside every project.
pub const NO_PROJECT: &str = "no project";

/// A project as clients name it: its emoji, when it has one, then its name;
/// [`NO_PROJECT`] for none.
#[must_use]
pub fn project_label(project: Option<&ProjectInfo>) -> String {
    match project {
        Some(ProjectInfo {
            emoji: Some(emoji),
            name,
            ..
        }) => format!("{emoji} {name}"),
        Some(project) => project.name.to_string(),
        None => NO_PROJECT.to_owned(),
    }
}

/// The project `id` names among `projects`; `None` for no ID and for a
/// project that no longer exists, which lists as no project.
#[must_use]
pub fn find_project<'a>(
    projects: &'a [ProjectInfo],
    id: Option<&ProjectId>,
) -> Option<&'a ProjectInfo> {
    let id = id?;
    projects.iter().find(|project| project.id == *id)
}

/// One section of the session list: a project's root sessions, or (with no
/// project) those of no project.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectSection {
    pub project: Option<ProjectInfo>,
    /// Active sessions first, then the rest, each newest first.
    pub sessions: Vec<SessionMeta>,
}

impl ProjectSection {
    /// The key a client remembers the section's folding under.
    #[must_use]
    pub fn key(&self) -> String {
        section_key(self.project.as_ref().map(|project| &project.id))
    }
}

/// The key of the section of `project` (`None`: no project).
#[must_use]
pub fn section_key(project: Option<&ProjectId>) -> String {
    project.map_or_else(|| "none".to_owned(), |id| format!("project:{id}"))
}

/// The session list as project sections. A section per project, ordered by
/// its latest session's activity; projects without sessions follow in name
/// order; "No project" comes last and holds every root session of no
/// project or of a project that no longer exists. Each section orders its
/// sessions by [`order_sessions`]. Delegated sessions are left out.
#[must_use]
pub fn project_sections(projects: &[ProjectInfo], sessions: &[SessionMeta]) -> Vec<ProjectSection> {
    let mut sections = projects
        .iter()
        .map(|project| ProjectSection {
            project: Some(project.clone()),
            sessions: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut none = ProjectSection {
        project: None,
        sessions: Vec::new(),
    };
    for session in sessions
        .iter()
        .filter(|session| matches!(session.origin, SessionOrigin::Root))
    {
        let section = session.project.as_ref().and_then(|id| {
            sections.iter_mut().find(|section| {
                section
                    .project
                    .as_ref()
                    .is_some_and(|project| project.id == *id)
            })
        });
        section.unwrap_or(&mut none).sessions.push(session.clone());
    }
    for section in &mut sections {
        order_sessions(&mut section.sessions);
    }
    order_sessions(&mut none.sessions);
    let latest = |section: &ProjectSection| {
        section
            .sessions
            .iter()
            .map(|session| session.last_activity)
            .max()
    };
    // The stable sort keeps `projects.list`'s name order among the projects
    // without sessions.
    sections.sort_by_key(|section| std::cmp::Reverse(latest(section)));
    sections.push(none);
    sections
}

/// One line of a project picker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectChoice {
    Project(ProjectId),
    NoProject,
    /// Creates a project, then picks it.
    NewProject,
}

/// The lines of a project picker filtered by `query` (case-insensitive,
/// over the name and the emoji): the projects in `projects`' order, "No
/// project", then "New project…" (offered whatever the query says, when
/// `creates`).
#[must_use]
pub fn project_choices(projects: &[ProjectInfo], query: &str, creates: bool) -> Vec<ProjectChoice> {
    let query = query.trim().to_lowercase();
    let mut choices = projects
        .iter()
        .filter(|project| query.is_empty() || project_label(Some(project)).contains(&query))
        .map(|project| ProjectChoice::Project(project.id.clone()))
        .collect::<Vec<_>>();
    if query.is_empty() || NO_PROJECT.contains(&query) {
        choices.push(ProjectChoice::NoProject);
    }
    if creates {
        choices.push(ProjectChoice::NewProject);
    }
    choices
}

/// The longest emoji the daemon takes, in bytes.
pub const MAX_EMOJI_BYTES: usize = 32;

/// Checks `emoji` as the daemon does (`docs/site/specs/projects.md`
/// section 3): one extended grapheme cluster of at most 32 bytes, holding a
/// character Unicode marks as an emoji, made only of emoji characters and
/// emoji components, and not ASCII, whitespace, or a control character.
/// The error says why, for the field to show at once.
pub fn validate_emoji(emoji: &str) -> Result<(), String> {
    use unicode_properties::UnicodeEmoji as _;
    use unicode_segmentation::UnicodeSegmentation as _;
    if emoji.is_empty() {
        return Err("it is empty".to_owned());
    }
    if emoji.len() > MAX_EMOJI_BYTES {
        return Err(format!(
            "it is {} bytes, over the {MAX_EMOJI_BYTES}-byte limit",
            emoji.len()
        ));
    }
    if emoji.is_ascii() {
        return Err("it is not an emoji".to_owned());
    }
    if emoji
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("it holds a space or a control character".to_owned());
    }
    if emoji.graphemes(true).count() != 1 {
        return Err("it is more than one emoji".to_owned());
    }
    if !emoji.chars().any(|character| character.is_emoji_char())
        || !emoji
            .chars()
            .all(|character| character.is_emoji_char_or_emoji_component())
    {
        return Err("it is not an emoji".to_owned());
    }
    Ok(())
}

/// Whether a new session in a project with `defaults` takes their
/// attachments: they name runtimes or leave out `global`, as the daemon
/// decides. Defaults that only name an agent or a preset leave the usual
/// attachments.
#[must_use]
pub fn defaults_attach(defaults: &ProjectDefaults) -> bool {
    !defaults.runtimes.is_empty() || !defaults.global
}

/// The attachments `defaults` name: whether `global` is attached, and the
/// other runtimes as `session.create` takes them, in order.
#[must_use]
pub fn defaults_requests(defaults: &ProjectDefaults) -> (bool, Vec<RuntimeAttachRequest>) {
    (
        defaults.global,
        defaults
            .runtimes
            .iter()
            .map(|runtime| RuntimeAttachRequest {
                runtime_id: runtime.runtime_id.clone(),
                capability: runtime.capability.clone(),
                workspace: runtime.workspace.clone(),
            })
            .collect(),
    )
}

/// The defaults a project editor saves: the attachments (`global` and the
/// other runtimes, a runtime listed twice kept once), the agent, and the
/// preset. `None` when that is all the daemon's usual defaults, so the
/// project keeps none.
#[must_use]
pub fn editor_defaults(
    global: bool,
    requests: &[RuntimeAttachRequest],
    agent: Option<&cookie_agent_protocol::AgentId>,
    preset: Option<&str>,
) -> Option<ProjectDefaults> {
    let mut runtimes = Vec::<ProjectRuntime>::new();
    for request in requests
        .iter()
        .filter(|request| request.runtime_id.as_str() != RuntimeId::GLOBAL)
    {
        let runtime = ProjectRuntime {
            runtime_id: request.runtime_id.clone(),
            capability: request.capability.clone(),
            workspace: request
                .workspace
                .as_deref()
                .map(str::trim)
                .filter(|workspace| !workspace.is_empty())
                .map(str::to_owned),
        };
        if !runtimes.contains(&runtime) {
            runtimes.push(runtime);
        }
    }
    let defaults = ProjectDefaults {
        preset: preset.map(str::to_owned),
        agent: agent.cloned(),
        global,
        runtimes,
    };
    (defaults != ProjectDefaults::default()).then_some(defaults)
}

/// What a project's default agent and preset make of a new session's
/// selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectAgent {
    /// The project names neither: the draft keeps its selection.
    Unset,
    /// The agent, its preset, and the agent's own default model.
    Selected(RunSelection),
    /// The agent is gone or cannot run as root: the draft keeps its usual
    /// selection, and the message says so.
    Unavailable(String),
}

/// The selection `project`'s default agent and preset give a new session
/// (`docs/site/specs/projects.md` section 4): the agent among `agents` that
/// runs as root in that preset (with no agent named, the preset's `primary`,
/// else its first), and that agent's model: an ACP agent's synthetic one,
/// else its first `live` accepts, else `fallback_model`.
pub fn project_agent(
    project: &ProjectInfo,
    agents: &[AgentDescriptor],
    live: impl Fn(&ModelSelection) -> bool,
    fallback_model: Option<ModelSelection>,
) -> ProjectAgent {
    let Some(defaults) = project.defaults.as_ref() else {
        return ProjectAgent::Unset;
    };
    if defaults.agent.is_none() && defaults.preset.is_none() {
        return ProjectAgent::Unset;
    }
    let candidates = agents
        .iter()
        .filter(|agent| {
            agent.runnable_as_root
                && agent.mode != AgentMode::Internal
                && agent.preset == defaults.preset
        })
        .collect::<Vec<_>>();
    let agent = match &defaults.agent {
        Some(wanted) => candidates.iter().find(|agent| agent.id == *wanted).copied(),
        None => candidates
            .iter()
            .find(|agent| agent.id.as_str() == "primary")
            .or_else(|| candidates.first())
            .copied(),
    };
    let unavailable = || {
        let named = match (&defaults.agent, &defaults.preset) {
            (Some(agent), Some(preset)) => format!("agent {agent} of preset {preset}"),
            (Some(agent), None) => format!("agent {agent}"),
            (None, Some(preset)) => format!("preset {preset}"),
            (None, None) => unreachable!("checked above"),
        };
        ProjectAgent::Unavailable(format!(
            "Project {} names {named}, which cannot start a session here; using the usual agent.",
            project.name
        ))
    };
    let Some(agent) = agent else {
        return unavailable();
    };
    let Some(model) = crate::acp::acp_model(agent)
        .or_else(|| {
            agent
                .resolved_fallback
                .iter()
                .find(|selection| live(selection))
                .cloned()
        })
        .or(fallback_model)
    else {
        return unavailable();
    };
    ProjectAgent::Selected(RunSelection {
        agent: agent.id.clone(),
        model,
        preset: defaults.preset.clone(),
    })
}

/// The question a project's delete asks, with what goes.
#[must_use]
pub fn delete_question(project: &ProjectInfo) -> String {
    let sessions = match project.sessions {
        1 => "1 session".to_owned(),
        count => format!("{count} sessions"),
    };
    let memories = match project.memories {
        1 => "1 memory".to_owned(),
        count => format!("{count} memories"),
    };
    format!(
        "Delete project {} with its {sessions}, {memories}, and AGENTS.md? This cannot be undone.",
        project.name
    )
}

/// The sessions a refused delete names as running
/// (`project_sessions_running`); empty for any other error.
#[must_use]
pub fn running_sessions(error: &ClientError) -> Vec<SessionId> {
    if !error.is(&cookie_agent_protocol::errors::PROJECT_SESSIONS_RUNNING) {
        return Vec::new();
    }
    error
        .error_data()
        .and_then(|data| data.fields.get("session_ids").cloned())
        .and_then(|ids| serde_json::from_value::<Vec<SessionId>>(ids).ok())
        .unwrap_or_default()
}

/// What a client says when a delete was refused because `running` sessions
/// run: their titles (else short IDs) from `sessions`.
#[must_use]
pub fn running_sessions_message(running: &[SessionId], sessions: &[SessionMeta]) -> String {
    let names = running
        .iter()
        .map(|id| {
            sessions
                .iter()
                .find(|session| session.session_id == *id)
                .and_then(|session| {
                    session
                        .title
                        .as_ref()
                        .map(ToString::to_string)
                        .or_else(|| session.short_id.clone())
                })
                .map_or_else(|| id.to_string(), |name| format!("“{name}”"))
        })
        .collect::<Vec<_>>();
    match names.as_slice() {
        [] => "Sessions of this project are running; stop them, then delete it.".to_owned(),
        [one] => format!("{one} is running; stop it, then delete the project."),
        several => format!(
            "{} are running; stop them, then delete the project.",
            several.join(", ")
        ),
    }
}

#[cfg(test)]
#[path = "projects_tests.rs"]
mod tests;
