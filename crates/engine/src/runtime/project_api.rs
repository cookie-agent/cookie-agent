//! Projects as clients and runs reach them (`docs/site/specs/projects.md`
//! sections 4 to 7): the `project.*` requests, a session's membership, and
//! what a run admission reads of the session's project.

use std::sync::Arc;

use cookie_agent_protocol::{
    AgentMdEntry, AgentMdProject, AgentMdSkipped, AttachmentName, ProjectAgentsMd,
    ProjectCreateParams, ProjectDefaults, ProjectDeleteParams, ProjectDeleteResult,
    ProjectGetResult, ProjectId, ProjectInfo, ProjectUpdateParams, RuntimeId, SafeDisplayText,
    SessionId, SessionMeta, SessionOrigin,
};

use super::{Engine, EngineError, Event, agent_md::AttachmentAgentMd};
use crate::{
    memory::{agents::AgentMemory, store::MemoryOwners, url::MemoryName},
    projects::{Project, ProjectChange, ProjectError, ProjectStore, short_digest},
};

/// What the memory store is told by the engine: its agent registries and
/// its projects, read from disk under the memory lock.
pub(crate) struct EngineMemoryOwners<'a>(pub(crate) &'a Engine);

impl MemoryOwners for EngineMemoryOwners<'_> {
    fn agent(&self, agent: &cookie_agent_protocol::AgentId) -> AgentMemory {
        self.0.agent_memory(agent)
    }

    fn unknown_project(&self, project: &MemoryName) -> Option<Vec<String>> {
        let names = self.0.inner.projects.names();
        (!names.iter().any(|name| name == project.as_str())).then_some(names)
    }
}

/// Whether `defaults` say which runtimes a new session attaches: they list
/// runtimes or leave out `global`. Defaults that only name an agent leave a
/// new session the usual attachments.
fn sets_attachments(defaults: &ProjectDefaults) -> bool {
    !defaults.runtimes.is_empty() || !defaults.global
}

impl Engine {
    /// Runs a project store call on a blocking thread: it reads files and
    /// may wait for the memory lock.
    async fn projects_blocking<T: Send + 'static>(
        &self,
        call: impl FnOnce(&Engine, &ProjectStore) -> Result<T, EngineError> + Send + 'static,
    ) -> Result<T, EngineError> {
        let engine = self.clone();
        tokio::task::spawn_blocking(move || {
            let store = Arc::clone(&engine.inner.projects);
            call(&engine, &store)
        })
        .await
        .map_err(|error| ProjectError::Store(format!("the request did not finish: {error}")))?
    }

    /// Signals every project change this daemon makes.
    #[must_use]
    pub fn subscribe_project_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.projects.subscribe()
    }

    /// `ProjectInfo` of `project`, its sessions counted among `roots`.
    fn project_info(&self, project: Project, roots: &[SessionMeta]) -> ProjectInfo {
        let sessions = roots
            .iter()
            .filter(|meta| meta.project.as_ref() == Some(&project.id))
            .count() as u64;
        ProjectInfo {
            memories: crate::projects::memory_count(self.inner.projects.memory(), &project),
            has_agents_md: project.agents_md.is_some(),
            id: project.id,
            name: project.name,
            emoji: project.emoji,
            revision: project.revision,
            defaults: project.defaults,
            sessions,
        }
    }

    /// The root sessions as their own metadata records them, unresolved.
    fn raw_roots(&self) -> Vec<SessionMeta> {
        self.inner
            .store
            .list_roots()
            .into_iter()
            .filter(|meta| matches!(meta.origin, SessionOrigin::Root))
            .collect()
    }

    /// `projects.list`: every project, oldest first.
    pub async fn list_projects(&self) -> Result<Vec<ProjectInfo>, EngineError> {
        self.projects_blocking(|engine, store| {
            let roots = engine.raw_roots();
            Ok(store
                .list()?
                .into_iter()
                .map(|project| engine.project_info(project, &roots))
                .collect())
        })
        .await
    }

    /// `project.get`: one project with its `AGENTS.md`.
    pub async fn get_project(&self, id: ProjectId) -> Result<ProjectGetResult, EngineError> {
        self.projects_blocking(move |engine, store| {
            let project = store.get(&id)?;
            let agents_md = project.agents_md.clone().map(|text| ProjectAgentsMd {
                revision: short_digest(text.as_bytes()),
                text,
            });
            Ok(ProjectGetResult {
                project: engine.project_info(project, &engine.raw_roots()),
                agents_md,
            })
        })
        .await
    }

    /// `project.create`.
    pub async fn create_project(
        &self,
        params: ProjectCreateParams,
    ) -> Result<ProjectInfo, EngineError> {
        if let Some(defaults) = &params.defaults {
            self.validate_default_agent(defaults)?;
        }
        self.projects_blocking(move |engine, store| {
            let project = store.create(
                params.name,
                params.emoji,
                params.defaults,
                engine.memory_limits().overview_max_bytes,
            )?;
            Ok(engine.project_info(project, &[]))
        })
        .await
    }

    /// `project.update`.
    pub async fn update_project(
        &self,
        params: ProjectUpdateParams,
    ) -> Result<ProjectInfo, EngineError> {
        if let Some(Some(defaults)) = &params.defaults {
            self.validate_default_agent(defaults)?;
        }
        self.projects_blocking(move |engine, store| {
            let project = store.update(
                &params.project,
                &params.revision,
                ProjectChange {
                    name: params.name,
                    emoji: params.emoji,
                    defaults: params.defaults,
                    agents_md: params.agents_md,
                },
                engine.memory_limits().overview_max_bytes,
            )?;
            Ok(engine.project_info(project, &engine.raw_roots()))
        })
        .await
    }

    /// Refuses a default agent the daemon does not list in its preset, or
    /// one that cannot run as root (`docs/site/specs/projects.md`
    /// section 4). A preset without an agent must exist.
    fn validate_default_agent(&self, defaults: &ProjectDefaults) -> Result<(), ProjectError> {
        let invalid = |reason: String| ProjectError::Invalid {
            field: "defaults",
            reason,
        };
        let runtime = self.current_runtime();
        let registry = runtime
            .agents_for_preset(defaults.preset.as_deref())
            .map_err(|error| invalid(error.to_string()))?;
        let Some(agent) = &defaults.agent else {
            return Ok(());
        };
        let preset = defaults.preset.as_deref().map_or_else(
            || "the shared registry".to_owned(),
            |preset| format!("preset `{preset}`"),
        );
        let resolved = registry
            .get(agent)
            .ok_or_else(|| invalid(format!("{preset} has no agent `{agent}`")))?;
        if !resolved.runnable_as_root {
            return Err(invalid(format!(
                "agent `{agent}` of {preset} cannot run as a root session"
            )));
        }
        Ok(())
    }

    /// `project.delete`: refused while any session tree of the project is
    /// busy, naming those sessions; otherwise deletes every session tree of
    /// the project as `session.delete` does, then the project's memory
    /// folder and its own folder.
    pub async fn delete_project(
        &self,
        params: ProjectDeleteParams,
    ) -> Result<ProjectDeleteResult, EngineError> {
        self.ensure_not_shutting_down()?;
        let id = params.project.clone();
        let current = self
            .projects_blocking({
                let id = id.clone();
                move |_, store| Ok(store.get(&id)?)
            })
            .await?;
        if current.revision != params.revision {
            return Err(ProjectError::Changed {
                project: id,
                revision: current.revision,
            }
            .into());
        }
        let roots = {
            let engine = self.clone();
            crate::blocking::off_worker(move || engine.raw_roots())
        }
        .into_iter()
        .filter(|meta| meta.project.as_ref() == Some(&id))
        .map(|meta| meta.session_id)
        .collect::<Vec<_>>();
        let running = roots
            .iter()
            .copied()
            .filter(|root| self.tree_busy(*root))
            .collect::<Vec<_>>();
        if !running.is_empty() {
            return Err(ProjectError::SessionsRunning {
                project: id,
                sessions: running,
            }
            .into());
        }
        // A new-session job naming the project goes with it; the jobs of
        // its sessions go with the sessions.
        self.delete_crons_of_project(id.clone()).await?;
        let mut deleted_sessions = 0;
        for root in roots {
            let last_seq = self.inner.store.get(root)?.meta.last_event_seq;
            self.delete_session(root, last_seq).await?;
            deleted_sessions += 1;
        }
        let deleted_memories = self
            .projects_blocking(move |_, store| Ok(store.delete(&id, &params.revision)?))
            .await?;
        Ok(ProjectDeleteResult {
            deleted_sessions,
            deleted_memories,
        })
    }

    /// The project `id`, when it exists.
    fn existing_project(&self, id: &ProjectId) -> Result<Project, EngineError> {
        Ok(self.inner.projects.get(id)?)
    }

    /// The project a session belongs to now, when it exists: a root's own,
    /// a delegated session's root's. Blocking: it reads the project's
    /// folder.
    pub(crate) fn session_project(&self, session: SessionId) -> Option<Project> {
        let projection = self.inner.store.get(session).ok()?;
        let project = match projection.meta.origin {
            SessionOrigin::Root => projection.meta.project.clone(),
            SessionOrigin::Delegated {
                root_session_id, ..
            } => self
                .inner
                .store
                .get(root_session_id)
                .ok()?
                .meta
                .project
                .clone(),
        }?;
        self.inner.projects.get(&project).ok()
    }

    /// `meta` as clients are shown it: a delegated session takes its root's
    /// project, a project that no longer exists reads as none, and
    /// `waiting_on` adds what the engine's live producers make the session
    /// wait on.
    pub(crate) fn client_session_meta(
        &self,
        meta: SessionMeta,
        known: impl Fn(&ProjectId) -> bool,
    ) -> SessionMeta {
        let mut meta = self.inner.store.for_clients(meta);
        if let SessionOrigin::Delegated {
            root_session_id, ..
        } = meta.origin
        {
            meta.project = self
                .inner
                .store
                .get(root_session_id)
                .ok()
                .and_then(|root| root.meta.project.clone());
        }
        if meta.project.as_ref().is_some_and(|project| !known(project)) {
            meta.project = None;
        }
        meta
    }

    /// The IDs of the projects there are.
    pub(crate) fn project_ids(&self) -> Vec<ProjectId> {
        self.inner.projects.ids()
    }

    /// Whether project `project` exists.
    pub(crate) fn project_exists(&self, project: &ProjectId) -> bool {
        self.inner.projects.exists(project)
    }

    /// The defaults a new session in `project` attaches when its creation
    /// names no runtimes; refuses a project that does not exist.
    pub(crate) async fn project_attachment_defaults(
        &self,
        project: &ProjectId,
    ) -> Result<Option<ProjectDefaults>, EngineError> {
        let project = project.clone();
        let found = self
            .projects_blocking(move |engine, _| engine.existing_project(&project))
            .await?;
        Ok(found.defaults.filter(sets_attachments))
    }

    /// `session.project.set`: moves the root session `session_id` into
    /// `project`, or out of any with `None`, by recording
    /// `session_project_set`. Allowed while the session runs: runs read the
    /// project at their admission. Moving a session where it already is
    /// records nothing.
    pub async fn set_session_project(
        &self,
        session_id: SessionId,
        project: Option<ProjectId>,
    ) -> Result<SessionMeta, EngineError> {
        let root = self.inner.store.root_of(session_id)?;
        if root != session_id {
            return Err(ProjectError::RootSessionOnly {
                session_id,
                root_session_id: root,
            }
            .into());
        }
        if let Some(project) = &project {
            let project = project.clone();
            self.projects_blocking(move |engine, _| engine.existing_project(&project).map(drop))
                .await?;
        }
        // Membership is read at admission only, never by producer
        // reconciliation.
        let meta = self
            .on_actor_unreconciled(session_id, move |engine| {
                let projection = engine.inner.store.get(session_id)?;
                if projection.meta.project != project {
                    engine.append_direct(
                        session_id,
                        None,
                        super::event_origin("user"),
                        Event::SessionProjectSet { project },
                    )?;
                }
                Ok(engine.inner.store.get(session_id)?.metadata())
            })
            .await?;
        let engine = self.clone();
        Ok(crate::blocking::off_worker(move || {
            engine.client_session_meta(meta, |project| engine.project_exists(project))
        }))
    }

    /// The project's `AGENTS.md` for a root run of `session`, as one
    /// attachment's files are given (`docs/site/specs/projects.md`
    /// section 6): recorded under `global`, where the file lives, with the
    /// project named. A file over the size cap is skipped as a workspace's
    /// would be.
    pub(crate) async fn project_agent_md(&self, session: SessionId) -> Option<AttachmentAgentMd> {
        let engine = self.clone();
        let project = tokio::task::spawn_blocking(move || engine.session_project(session))
            .await
            .ok()
            .flatten()?;
        let text = project.agents_md.clone()?;
        let source = format!("project:{}/AGENTS.md", project.name);
        let byte_length = text.len() as u64;
        let (entries, skipped) = if byte_length > AgentMdSkipped::MAX_SKIP_BYTES {
            (
                Vec::new(),
                vec![AgentMdSkipped {
                    path: SafeDisplayText::new(&source).ok()?,
                    byte_length,
                }],
            )
        } else {
            (
                vec![AgentMdEntry {
                    source: SafeDisplayText::new(&source).ok()?,
                    content: text,
                    byte_length,
                }],
                Vec::new(),
            )
        };
        Some(AttachmentAgentMd {
            attachment: AttachmentName::new(RuntimeId::GLOBAL)
                .expect("`global` is an attachment name"),
            entries,
            skipped,
            project: Some(AgentMdProject {
                id: project.id,
                name: project.name,
            }),
        })
    }
}
