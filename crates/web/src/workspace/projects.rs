//! Projects (`docs/site/specs/projects.md` section 10.1): `projects.list`,
//! the sidebar's sections and what the browser keeps of their folding,
//! moving a session, the project a new session starts in (with its default
//! attachments, agent, and preset), the project editor, and deleting a
//! project.

use cookie_agent_client_state::projects::{
    ProjectAgent, defaults_attach, editor_defaults, find_project, project_agent, running_sessions,
    running_sessions_message, section_key,
};
use cookie_agent_protocol::{
    AgentDescriptor, AgentId, AgentMode, ModelSelection, ProjectCreateParams, ProjectId,
    ProjectInfo, ProjectName, ProjectUpdateParams, SessionId, SessionsChangedNotification, errors,
};
use leptos::{prelude::*, task::spawn_local};

use super::{Panel, Workspace};
use crate::{browser, cards::Selection};

/// The `localStorage` key of whether the section `key` is open.
pub fn section_storage_key(key: &str) -> String {
    format!("cookie.project_section:{key}")
}

/// What the editor holds, compared with what it opened with to tell
/// unsaved edits.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EditorFields {
    pub name: String,
    pub emoji: Option<String>,
    pub selection: Selection,
    pub agent: Option<AgentId>,
    pub preset: Option<String>,
    pub agents_md: String,
}

/// The project editor: a project's fields, or a new project's.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectEditor {
    /// The project as last read; `None` for a new one.
    pub project: Option<ProjectInfo>,
    /// The fields as they opened (or were last saved).
    pub initial: EditorFields,
    /// The fields but the attachments, which live in
    /// [`Workspace::editor_selection`] for the cards.
    pub name: String,
    pub emoji: Option<String>,
    pub agent: Option<AgentId>,
    pub preset: Option<String>,
    pub agents_md: String,
    /// The `AGENTS.md` read; until then it cannot be edited.
    pub agents_md_loaded: bool,
    pub error: Option<String>,
    /// The project changed since the editor read it: offer to reload.
    pub conflict: bool,
    pub saving: bool,
    /// Closing waits for the user to discard the unsaved edits.
    pub confirm_close: bool,
    /// Picked for the new session once created (New project… of its
    /// picker).
    pub for_new_session: bool,
}

impl ProjectEditor {
    fn new(project: Option<ProjectInfo>, selection: Selection, for_new_session: bool) -> Self {
        let defaults = project
            .as_ref()
            .and_then(|project| project.defaults.clone());
        let initial = EditorFields {
            name: project
                .as_ref()
                .map(|project| project.name.to_string())
                .unwrap_or_default(),
            emoji: project.as_ref().and_then(|project| project.emoji.clone()),
            selection,
            agent: defaults
                .as_ref()
                .and_then(|defaults| defaults.agent.clone()),
            preset: defaults.and_then(|defaults| defaults.preset),
            agents_md: String::new(),
        };
        Self {
            agents_md_loaded: project
                .as_ref()
                .is_none_or(|project| !project.has_agents_md),
            project,
            name: initial.name.clone(),
            emoji: initial.emoji.clone(),
            agent: initial.agent.clone(),
            preset: initial.preset.clone(),
            agents_md: String::new(),
            initial,
            error: None,
            conflict: false,
            saving: false,
            confirm_close: false,
            for_new_session,
        }
    }

    /// The fields now, with the cards' `selection`.
    pub fn fields(&self, selection: &Selection) -> EditorFields {
        EditorFields {
            name: self.name.clone(),
            emoji: self.emoji.clone(),
            selection: selection.clone(),
            agent: self.agent.clone(),
            preset: self.preset.clone(),
            agents_md: self.agents_md.clone(),
        }
    }

    /// Whether anything changed since the editor opened or saved.
    pub fn dirty(&self, selection: &Selection) -> bool {
        self.fields(selection) != self.initial
    }
}

impl Workspace {
    /// Reads `projects.list`, from a daemon that keeps projects.
    pub fn refresh_projects(self) {
        let Some(client) = self.client() else {
            return;
        };
        if !self.has_projects.get_untracked() {
            return;
        }
        let read = self.begin_read(|reads| &mut reads.projects);
        spawn_local(async move {
            let result = client.list_projects().await;
            if !self.read_is_current(|reads| &mut reads.projects, read) {
                return;
            }
            match result {
                Ok(result) => self.projects.set(result.projects),
                Err(error) => self.status.set(format!(
                    "projects unavailable: {}",
                    crate::errors::describe(&error)
                )),
            }
        });
    }

    /// The project `id` names.
    pub fn project(self, id: Option<&ProjectId>) -> Option<ProjectInfo> {
        self.projects
            .with(|projects| find_project(projects, id).cloned())
    }

    /// Whether the section `key` shows its sessions: as this page or the
    /// browser left it, open at first.
    pub fn section_open(self, key: &str) -> bool {
        self.sections_open
            .with(|open| open.get(key).copied())
            .unwrap_or_else(|| {
                browser::load(&section_storage_key(key)).as_deref() != Some("folded")
            })
    }

    /// Folds or opens the section `key`, as the browser remembers.
    pub fn toggle_section(self, key: &str) {
        let open = !untrack(|| self.section_open(key));
        browser::store(
            &section_storage_key(key),
            if open { "open" } else { "folded" },
        );
        self.sections_open.update(|sections| {
            sections.insert(key.to_owned(), open);
        });
    }

    /// Moves the root session `session_id` into `project` (`None`: out of
    /// any). Its attachments and agent stay.
    pub fn move_session(self, session_id: SessionId, project: Option<ProjectId>) {
        let Some(client) = self.client() else {
            return;
        };
        let current = self.sessions.with_untracked(|sessions| {
            sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map(|session| session.project.clone())
        });
        if current.as_ref() == Some(&project) {
            return;
        }
        let label = cookie_agent_client_state::projects::project_label(
            untrack(|| self.project(project.as_ref())).as_ref(),
        );
        // The section it goes to opens, so the row stays in sight.
        let key = section_key(project.as_ref());
        if !untrack(|| self.section_open(&key)) {
            self.toggle_section(&key);
        }
        spawn_local(async move {
            match client.set_session_project(session_id, project).await {
                Ok(result) => {
                    self.apply_sessions_change(SessionsChangedNotification::Upserted {
                        session: Box::new(result.session),
                    });
                    self.status.set(format!("moved the session to {label}"));
                }
                Err(error) => self.status.set(format!(
                    "the session did not move: {}",
                    crate::errors::describe(&error)
                )),
            }
        });
    }

    /// "Move to project…" of `session_id`.
    pub fn ask_move(self, session_id: SessionId) {
        self.moving.set(Some(session_id));
        self.sidebar_open.set(false);
        self.panel.set(Some(Panel::MoveSession));
    }

    /// A new session in `project`: the cards and the agent take its
    /// defaults.
    pub fn new_session_in(self, project: Option<ProjectId>) {
        self.start_new_session();
        self.choose_new_project(project);
    }

    /// Picks the project the new session is created in. A project whose
    /// defaults name attachments puts them on the cards; its default agent
    /// and preset become the draft's, with the agent's own model, unless
    /// the agent cannot start a session here, which the status line says.
    pub fn choose_new_project(self, project: Option<ProjectId>) {
        // Cards still as the previous project's defaults left them follow
        // the project; cards the user picked stay.
        let homes = untrack(|| self.homes());
        let kept_defaults = untrack(|| self.draft_project_defaults()).is_some_and(|defaults| {
            self.selection
                .with_untracked(|selection| *selection == Selection::of_project(&defaults, &homes))
        });
        self.new_project.set(project.clone());
        let info = untrack(|| self.project(project.as_ref()));
        match info
            .as_ref()
            .and_then(|info| info.defaults.as_ref())
            .filter(|defaults| defaults_attach(defaults))
        {
            Some(defaults) => {
                self.selection.set(Selection::of_project(defaults, &homes));
                self.card_errors.set(Default::default());
            }
            None if kept_defaults => {
                self.selection.set(Selection::global_only());
                self.card_errors.set(Default::default());
            }
            None => {}
        }
        let Some(info) = info else {
            return;
        };
        let agents = untrack(|| self.root_agents());
        let models = untrack(|| self.models());
        let live = |selection: &ModelSelection| {
            models.iter().any(|model| {
                model.key == selection.model
                    && selection.variant.as_ref().is_none_or(|variant| {
                        model
                            .variants
                            .iter()
                            .any(|candidate| candidate.id == *variant)
                    })
            })
        };
        let fallback = models.first().map(|model| ModelSelection {
            model: model.key.clone(),
            variant: model.default_variant.clone(),
        });
        match project_agent(&info, &agents, live, fallback) {
            ProjectAgent::Unset => {}
            ProjectAgent::Selected(selection) => {
                self.preset.set(selection.preset.clone());
                self.draft.set(Some(selection));
            }
            ProjectAgent::Unavailable(message) => self.status.set(message),
        }
    }

    /// The default attachments of the draft's project, when they attach
    /// anything: what a draft without runtimes of its own is created with.
    pub fn draft_project_defaults(self) -> Option<cookie_agent_protocol::ProjectDefaults> {
        if !self.has_projects.get() {
            return None;
        }
        let project = self.new_project.get()?;
        self.project(Some(&project))?
            .defaults
            .filter(defaults_attach)
    }

    /// Every agent that can start a session, of every preset: what a
    /// project's default agent is chosen from.
    pub fn root_agents(self) -> Vec<AgentDescriptor> {
        self.with_runtime(|runtime| {
            runtime
                .snapshot()
                .map(|snapshot| {
                    snapshot
                        .agents
                        .iter()
                        .filter(|agent| agent.runnable_as_root && agent.mode != AgentMode::Internal)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// Opens the editor on `project` (`None`: a new project, picked for the
    /// new session once created when `for_new_session`).
    pub fn open_project_editor(self, project: Option<ProjectId>, for_new_session: bool) {
        let info = untrack(|| self.project(project.as_ref()));
        let selection = info
            .as_ref()
            .and_then(|info| info.defaults.as_ref())
            .map_or_else(
                || Selection {
                    global: true,
                    picks: Vec::new(),
                },
                |defaults| Selection::of_project(defaults, &untrack(|| self.homes())),
            );
        self.editor_selection.set(selection.clone());
        self.project_editor.set(Some(ProjectEditor::new(
            info.clone(),
            selection,
            for_new_session,
        )));
        self.project_error.set(None);
        self.poll_runtimes();
        // The sessions shown as the page give way to the editor.
        self.sidebar_open.set(false);
        self.panel.set(Some(Panel::Project));
        if let Some(info) = info {
            self.load_editor(info.id);
        }
    }

    /// Reads the edited project again with its `AGENTS.md`: on opening,
    /// and to reload after a conflict, which drops the edits.
    pub fn load_editor(self, project: ProjectId) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let result = client.get_project(project).await;
            let still_open = self.project_editor.with_untracked(|editor| {
                editor
                    .as_ref()
                    .and_then(|editor| editor.project.as_ref())
                    .is_some()
            });
            if !still_open {
                return;
            }
            match result {
                Ok(result) => {
                    let text = result
                        .agents_md
                        .map(|agents_md| agents_md.text)
                        .unwrap_or_default();
                    let selection = result.project.defaults.as_ref().map_or_else(
                        || Selection {
                            global: true,
                            picks: Vec::new(),
                        },
                        |defaults| Selection::of_project(defaults, &untrack(|| self.homes())),
                    );
                    let for_new_session = self.project_editor.with_untracked(|editor| {
                        editor.as_ref().is_some_and(|editor| editor.for_new_session)
                    });
                    // The list follows what the editor read, a change made
                    // on disk included.
                    let read = result.project.clone();
                    self.projects.update(|projects| {
                        if let Some(known) = projects.iter_mut().find(|known| known.id == read.id) {
                            *known = read;
                        }
                    });
                    let mut editor = ProjectEditor::new(
                        Some(result.project),
                        selection.clone(),
                        for_new_session,
                    );
                    editor.agents_md.clone_from(&text);
                    editor.initial.agents_md = text;
                    editor.agents_md_loaded = true;
                    self.editor_selection.set(selection);
                    self.project_editor.set(Some(editor));
                }
                Err(error) => self.edit_editor(|editor| {
                    editor.error = Some(crate::errors::describe(&error));
                }),
            }
        });
    }

    /// Changes the open editor.
    pub fn edit_editor(self, change: impl FnOnce(&mut ProjectEditor)) {
        self.project_editor.update(|editor| {
            if let Some(editor) = editor.as_mut() {
                change(editor);
            }
        });
    }

    /// Whether the editor holds unsaved edits.
    pub fn editor_dirty(self) -> bool {
        self.project_editor.with(|editor| {
            editor.as_ref().is_some_and(|editor| {
                self.editor_selection
                    .with(|selection| editor.dirty(selection))
            })
        })
    }

    /// Closes the open sheet; the project editor with unsaved edits asks
    /// first.
    pub fn close_panel(self) {
        if self.panel.get_untracked() == Some(Panel::Project) && untrack(|| self.editor_dirty()) {
            self.edit_editor(|editor| editor.confirm_close = true);
            return;
        }
        self.discard_panel();
    }

    /// Closes the open sheet, dropping the editor's edits.
    pub fn discard_panel(self) {
        self.panel.set(None);
        self.project_editor.set(None);
        self.moving.set(None);
    }

    /// Uses the open session's attachments as the edited defaults.
    pub fn editor_use_session(self) {
        let Some(session_id) = self
            .tree_root
            .get_untracked()
            .or(self.selected.get_untracked())
        else {
            return;
        };
        if let Some(attachments) = self.root_attachments(session_id) {
            self.editor_selection.set(Selection::of_session(
                &attachments,
                &untrack(|| self.homes()),
            ));
        }
    }

    /// Saves the editor: one `project.update` at the revision it read, or
    /// `project.create` for a new project (then its `AGENTS.md`, when it
    /// has one, with a `project.update`).
    pub fn save_editor(self) {
        let Some(client) = self.client() else {
            return;
        };
        let Some(editor) = self.project_editor.get_untracked() else {
            return;
        };
        let selection = self.editor_selection.get_untracked();
        let fields = editor.fields(&selection);
        let name = match ProjectName::new(fields.name.trim()) {
            Ok(name) => name,
            Err(_) => {
                self.edit_editor(|editor| {
                    editor.error = Some(
                        "A project name is a lowercase letter or digit, then up to 63 lowercase letters, digits, or -."
                            .into(),
                    );
                });
                return;
            }
        };
        if let Some(Err(reason)) = fields
            .emoji
            .as_deref()
            .map(cookie_agent_client_state::projects::validate_emoji)
        {
            self.edit_editor(|editor| editor.error = Some(format!("The emoji: {reason}.")));
            return;
        }
        let defaults = |fields: &EditorFields| {
            let (global, requests) = fields.selection.project_requests();
            editor_defaults(
                global,
                &requests,
                fields.agent.as_ref(),
                fields.preset.as_deref(),
            )
        };
        self.edit_editor(|editor| {
            editor.saving = true;
            editor.error = None;
            editor.confirm_close = false;
        });
        let initial = editor.initial.clone();
        spawn_local(async move {
            let saved = match &editor.project {
                None => {
                    let created = client
                        .create_project(ProjectCreateParams {
                            name,
                            emoji: fields.emoji.clone(),
                            defaults: defaults(&fields),
                        })
                        .await;
                    match created {
                        Ok(created) if !fields.agents_md.trim().is_empty() => {
                            let mut update = ProjectUpdateParams::new(
                                created.project.id.clone(),
                                created.project.revision.clone(),
                            );
                            update.agents_md = Some(fields.agents_md.clone());
                            client
                                .update_project(update)
                                .await
                                .map(|result| result.project)
                        }
                        Ok(created) => Ok(created.project),
                        Err(error) => Err(error),
                    }
                }
                Some(project) => {
                    let mut update =
                        ProjectUpdateParams::new(project.id.clone(), project.revision.clone());
                    if fields.name != initial.name {
                        update.name = Some(name);
                    }
                    if fields.emoji != initial.emoji {
                        update.emoji = Some(fields.emoji.clone());
                    }
                    let (now, before) = (defaults(&fields), defaults(&initial));
                    if now != before {
                        update.defaults = Some(now);
                    }
                    if fields.agents_md != initial.agents_md {
                        update.agents_md = Some(fields.agents_md.clone());
                    }
                    client
                        .update_project(update)
                        .await
                        .map(|result| result.project)
                }
            };
            match saved {
                Ok(project) => {
                    self.projects.update(|projects| {
                        match projects.iter_mut().find(|known| known.id == project.id) {
                            Some(known) => *known = project.clone(),
                            // The newest project goes last, as `projects.list`
                            // has it.
                            None => projects.push(project.clone()),
                        }
                    });
                    self.status.set(format!(
                        "saved project {}",
                        cookie_agent_client_state::projects::project_label(Some(&project))
                    ));
                    self.discard_panel();
                    if editor.for_new_session {
                        self.choose_new_project(Some(project.id));
                    }
                }
                Err(error) => {
                    let conflict = error.is(&errors::PROJECT_CHANGED);
                    self.edit_editor(|editor| {
                        editor.saving = false;
                        editor.conflict = conflict;
                        editor.error = Some(if conflict {
                            "The project changed since this editor read it. Reload it to see the change; your edits here are dropped.".into()
                        } else {
                            crate::errors::describe(&error)
                        });
                    });
                }
            }
        });
    }

    /// Deletes `project` at the revision listed, with its sessions and
    /// memories; a refusal shows under its header.
    pub fn remove_project(self, project: ProjectId) {
        let Some(client) = self.client() else {
            return;
        };
        let Some(info) = untrack(|| self.project(Some(&project))) else {
            return;
        };
        let key = section_key(Some(&project));
        self.project_error.set(None);
        self.status.set(format!("deleting project {}…", info.name));
        spawn_local(async move {
            match client
                .delete_project(project.clone(), info.revision.clone())
                .await
            {
                Ok(deleted) => {
                    browser::forget(&section_storage_key(&key));
                    browser::forget(&crate::memory::folder_key(info.name.as_str()));
                    self.projects
                        .update(|projects| projects.retain(|known| known.id != project));
                    if self.new_project.get_untracked().as_ref() == Some(&project) {
                        self.new_project.set(None);
                    }
                    let plural = |count: u64, one: &str, many: &str| match count {
                        1 => format!("1 {one}"),
                        count => format!("{count} {many}"),
                    };
                    self.status.set(format!(
                        "deleted project {} with {} and {}",
                        info.name,
                        plural(deleted.deleted_sessions, "session", "sessions"),
                        plural(deleted.deleted_memories, "memory", "memories"),
                    ));
                }
                Err(error) => {
                    let running = running_sessions(&error);
                    let message = if error.is(&errors::PROJECT_CHANGED) {
                        // Read it again, so that a second try deletes what
                        // the list then shows.
                        self.refresh_projects();
                        "The project changed since the list was read; it shows the project as it is now. Delete again to delete it."
                            .to_owned()
                    } else if running.is_empty() {
                        crate::errors::describe(&error)
                    } else {
                        self.sessions
                            .with_untracked(|sessions| running_sessions_message(&running, sessions))
                    };
                    self.status.set(String::new());
                    self.project_error.set(Some((key, message)));
                }
            }
        });
    }
}
