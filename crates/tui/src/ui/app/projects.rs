//! Projects in the TUI (`docs/site/specs/projects.md` section 10.2): the
//! project at the start of the bottom bar while no run is active, the
//! project picker its click and `/project` open (the projects, "No
//! project", and "New project…", which asks for a name), and the session
//! picker's project filter (Ctrl+P). A loaded session moves with
//! `session.project.set`; a new session's draft takes the project's
//! default attachments, agent, and preset.

use cookie_agent_client_state::projects::{
    self, ProjectChoice, defaults_attach, find_project, project_choices, project_label,
};
use cookie_agent_protocol::{ProjectCreateParams, ProjectFilter, ProjectInfo, ProjectName};

use super::*;
use crate::ui::input::InputState;

/// What the new-project field's key hint says.
const PROJECT_NAME_HINT: &str = "enter: create · esc: back";

impl App {
    /// Whether the daemon keeps projects (client protocol 1.8).
    pub(in crate::ui) fn daemon_has_projects(&self) -> bool {
        projects::daemon_has_projects(self.client.daemon_version())
    }

    /// Reads `projects.list` in the background, from a daemon that keeps
    /// projects; a change announced while it is on its way reads it again
    /// once it lands.
    pub(in crate::ui) fn poll_projects(&mut self) {
        if !self.daemon_has_projects() || !self.projects_refresh.start() {
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let projects = client
                .list_projects()
                .await
                .ok()
                .map(|listed| listed.projects);
            let _ = updates.send(RpcUpdate::ProjectsRefreshed { projects });
        });
    }

    /// Installs `projects.list`. The draft's project follows a rename, and
    /// a project that went away leaves the draft in none.
    pub(in crate::ui) fn install_projects(&mut self, projects: Vec<ProjectInfo>) {
        if let Some(draft) = self.draft_project.as_ref() {
            self.draft_project = find_project(&projects, Some(&draft.id)).cloned();
        }
        if let Some(ProjectFilter::Project(id)) = &self.session_project_filter
            && find_project(&projects, Some(id)).is_none()
        {
            self.session_project_filter = None;
        }
        if self.projects != projects {
            self.projects = projects;
            self.note_sessions_changed();
        }
    }

    /// The project the bottom bar names: the draft's, else the viewed
    /// session tree's.
    fn current_project(&self) -> Option<&ProjectInfo> {
        if self.new_session_draft.is_some() {
            return self.draft_project.as_ref();
        }
        let root = self.tree_root.or(self.selected)?;
        let project = self
            .sessions
            .iter()
            .find(|session| session.session_id == root)
            .or_else(|| self.selected_session_meta())?
            .project
            .as_ref();
        find_project(&self.projects, project)
    }

    /// The bottom bar's project, while no run is active and the daemon
    /// keeps projects: its emoji and name, or "no project".
    pub(in crate::ui) fn bar_project_label(&self) -> Option<String> {
        if !self.daemon_has_projects() || self.working_state().is_some() {
            return None;
        }
        if self.new_session_draft.is_none() && self.selected.is_none() {
            return None;
        }
        Some(project_label(self.current_project()))
    }

    /// The picker's lines: the projects, "No project", "New project…".
    pub(in crate::ui) fn project_picker_choices(&self) -> Vec<ProjectChoice> {
        project_choices(&self.projects, "", true)
    }

    /// The picker's lines as they show.
    pub(in crate::ui) fn project_picker_labels(&self) -> Vec<String> {
        let current = self.current_project().map(|project| project.id.clone());
        self.project_picker_choices()
            .into_iter()
            .map(|choice| match choice {
                ProjectChoice::Project(id) => {
                    let project = find_project(&self.projects, Some(&id));
                    let mark = if current.as_ref() == Some(&id) {
                        "  (current)"
                    } else {
                        ""
                    };
                    format!("{}{mark}", project_label(project))
                }
                ProjectChoice::NoProject if current.is_none() => "No project  (current)".to_owned(),
                ProjectChoice::NoProject => "No project".to_owned(),
                ProjectChoice::NewProject => "New project…".to_owned(),
            })
            .collect()
    }

    /// The bottom bar's project, or `/project`: the picker, on the current
    /// project. Nothing to pick from an older daemon, nor for a delegated
    /// session, which belongs to its root's project.
    pub(in crate::ui) fn open_project_picker(&mut self) {
        if !self.daemon_has_projects() {
            self.status = "This daemon keeps no projects; it needs client protocol 1.8.".into();
            return;
        }
        if self.new_session_draft.is_none() {
            if self.selected.is_none() {
                self.status =
                    "Select a session, or start one with /new, to pick its project.".into();
                return;
            }
            if !self.watching_root_session() {
                self.status =
                    "A subagent belongs to its root session's project; select the root to move it."
                        .into();
                return;
            }
        }
        self.poll_projects();
        let current = self.current_project().map(|project| project.id.clone());
        let row = self
            .project_picker_choices()
            .iter()
            .position(|choice| match (choice, &current) {
                (ProjectChoice::Project(id), Some(current)) => id == current,
                (ProjectChoice::NoProject, None) => true,
                _ => false,
            })
            .unwrap_or(0);
        self.project_create = None;
        self.picker_state.select(Some(row));
        self.modal = Modal::Projects;
    }

    /// Keys of the project picker, and of its new-project field.
    pub(in crate::ui) async fn handle_project_picker(&mut self, key: KeyEvent) {
        if let Some(input) = self.project_create.as_mut() {
            match key.code {
                KeyCode::Esc => self.project_create = None,
                KeyCode::Enter => {
                    let name = input.as_str().trim().to_owned();
                    self.create_project(&name);
                }
                _ => keys::edit_plain_input(input, key),
            }
            return;
        }
        let count = self.project_picker_choices().len();
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down | KeyCode::Tab => {
                move_picker_selection(&mut self.picker_state, count, false);
            }
            KeyCode::Enter => self.choose_project_entry(self.picker_state.selected().unwrap_or(0)),
            _ => {}
        }
    }

    /// A line of the picker chosen, by key or click.
    pub(in crate::ui) fn choose_project_entry(&mut self, index: usize) {
        let Some(choice) = self.project_picker_choices().get(index).cloned() else {
            return;
        };
        match choice {
            ProjectChoice::NewProject => {
                self.project_create = Some(InputState::default());
            }
            ProjectChoice::NoProject => {
                self.modal = Modal::None;
                self.apply_project(None);
            }
            ProjectChoice::Project(id) => {
                self.modal = Modal::None;
                let project = find_project(&self.projects, Some(&id)).cloned();
                self.apply_project(project);
            }
        }
    }

    /// Puts the loaded session in `project` (its attachments stay), or the
    /// draft: a project whose defaults name attachments gives them to the
    /// draft, and its default agent and preset become the draft's.
    fn apply_project(&mut self, project: Option<ProjectInfo>) {
        let label = project_label(project.as_ref());
        if self.new_session_draft.is_some() {
            let attaches = project
                .as_ref()
                .and_then(|project| project.defaults.as_ref())
                .is_some_and(defaults_attach);
            self.draft_project = project;
            if attaches {
                // The daemon attaches the defaults at creation; the bottom
                // bar shows them meanwhile.
                self.draft_runtimes = None;
            }
            self.status = format!("The new session starts in {label}.");
            if let Some(selection) = self.project_draft_selection() {
                self.status = format!(
                    "The new session starts in {label} with agent {}.",
                    selection.agent
                );
                self.new_session_draft = Some(selection);
            }
            return;
        }
        let Some(session_id) = self.tree_root.or(self.selected) else {
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let project = project.map(|project| project.id);
        self.spawn_rpc(async move {
            let result = client
                .set_session_project(session_id, project)
                .await
                .map(|result| result.session);
            let _ = updates.send(RpcUpdate::SessionProjectSet {
                label,
                result: Box::new(result),
            });
        });
    }

    /// Shows how a move ended, with the session's new project in the list
    /// at once; `sessions.changed` tells every other client.
    pub(super) fn finish_session_project_set(
        &mut self,
        label: String,
        result: Result<SessionMeta, ClientError>,
    ) {
        match result {
            Ok(session) => {
                let session = self.merge_session_meta(session);
                if let Some(existing) = self
                    .sessions
                    .iter_mut()
                    .find(|existing| existing.session_id == session.session_id)
                {
                    *existing = session;
                    self.note_sessions_changed();
                }
                self.status = format!("Moved the session to {label}.");
            }
            Err(error) => self.handle_rpc_update(RpcUpdate::failure(&error)),
        }
    }

    /// Enter in the new-project field: creates the project, then picks it.
    fn create_project(&mut self, name: &str) {
        let Ok(name) = ProjectName::new(name) else {
            self.status = "A project name is a lowercase letter or digit, then up to 63 lowercase letters, digits, or -.".into();
            return;
        };
        self.status = format!("Creating project {name}…");
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .create_project(ProjectCreateParams {
                    name,
                    emoji: None,
                    defaults: None,
                })
                .await
                .map(|result| result.project);
            let _ = updates.send(RpcUpdate::ProjectCreated {
                result: Box::new(result),
            });
        });
    }

    /// A created project joins the list and is picked; a refused name stays
    /// in its field to be fixed.
    pub(super) fn finish_project_create(&mut self, result: Result<ProjectInfo, ClientError>) {
        match result {
            Ok(project) => {
                if !self.projects.iter().any(|known| known.id == project.id) {
                    // The newest project goes last, as `projects.list` has it.
                    self.projects.push(project.clone());
                    self.note_sessions_changed();
                }
                self.project_create = None;
                if self.modal == Modal::Projects {
                    self.modal = Modal::None;
                }
                self.apply_project(Some(project));
            }
            Err(error) => self.handle_rpc_update(RpcUpdate::failure(&error)),
        }
    }

    /// The project picker, and its new-project field over it.
    pub(super) fn render_project_picker(&mut self, frame: &mut ratatui::Frame) {
        let area = centered(frame.area(), 48, 40);
        let target = if self.new_session_draft.is_some() {
            "Project — the new session"
        } else {
            "Project — move this session"
        };
        let entries = self.project_picker_labels();
        self.render_picker(
            frame,
            target,
            entries,
            None,
            area,
            Some("↑↓ move · enter: choose · esc: close"),
        );
        let theme = self.theme.clone();
        if let Some(input) = self.project_create.as_mut() {
            crate::ui::slash::render_text(
                frame,
                area,
                "New project",
                "A name: lowercase letters, digits, and -",
                PROJECT_NAME_HINT,
                input,
                &theme,
            );
        }
    }

    /// Paste into the new-project field as one line.
    pub(super) fn paste_into_project_create(&mut self, text: &str) -> bool {
        let Some(input) = self.project_create.as_mut() else {
            return false;
        };
        input.insert_text(text.trim());
        true
    }

    /// Ctrl+P in the session picker: all sessions, each project's, then
    /// those in no project.
    pub(in crate::ui) fn cycle_session_project_filter(&mut self) {
        if !self.daemon_has_projects() {
            self.status = "This daemon keeps no projects.".into();
            return;
        }
        let ids = self
            .projects
            .iter()
            .map(|project| project.id.clone())
            .collect::<Vec<_>>();
        self.session_project_filter = match &self.session_project_filter {
            None => ids
                .first()
                .cloned()
                .map(ProjectFilter::Project)
                .or(Some(ProjectFilter::None)),
            Some(ProjectFilter::Project(id)) => match ids.iter().position(|known| known == id) {
                Some(index) if index + 1 < ids.len() => {
                    Some(ProjectFilter::Project(ids[index + 1].clone()))
                }
                _ => Some(ProjectFilter::None),
            },
            Some(ProjectFilter::None) => None,
        };
        self.note_sessions_changed();
        self.session_search_changed();
    }

    /// What the session picker's project filter shows.
    pub(in crate::ui) fn session_project_filter_label(&self) -> String {
        match &self.session_project_filter {
            None => "all projects".to_owned(),
            Some(ProjectFilter::None) => "no project".to_owned(),
            Some(ProjectFilter::Project(id)) => {
                project_label(find_project(&self.projects, Some(id)))
            }
        }
    }

    /// Whether the session picker's project filter lets `session` through.
    pub(in crate::ui) fn session_passes_project_filter(&self, session: &SessionMeta) -> bool {
        match &self.session_project_filter {
            None => true,
            // A project that no longer exists lists as no project.
            Some(filter) => filter.admits(
                find_project(&self.projects, session.project.as_ref()).map(|project| &project.id),
            ),
        }
    }
}
