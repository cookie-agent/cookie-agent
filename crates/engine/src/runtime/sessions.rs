use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use cookie_agent_protocol::{
    AgentId, ChildSummary, EventOrigin, InvocationId, PermissionMode, PermissionRule, RunSelection,
    SessionForkResult, SessionId, SessionMeta, SessionOrigin, SessionPermissionOverlay,
    SessionRenameChange, SessionRenameParams, SessionRenameResult, SessionRevertResult,
    SessionStatus, SessionTitleChange, SessionTreeUsageResult, SessionUsageResult, UsageRollup,
};

use super::{
    Engine, EngineError, Event, SessionCommand,
    delegation::DELEGATION_RECOVERY_SETTLE_BOUND,
    helpers::{root_id, session_cwd_identity, session_depth},
};
use crate::policy::{self, freeze_root_agent_policy, resolve_agent};
use crate::session::SessionError;

impl Engine {
    /// The accepted selection, advanced only by a committed fallback model turn.
    pub fn session_model_selection(&self, session: SessionId) -> Result<RunSelection, EngineError> {
        let projection = self.inner.store.get(session)?;
        Ok(
            cookie_agent_protocol::SessionModelState::from_events(&projection.log.event_snapshot())
                .selection
                .unwrap_or_else(|| projection.meta.creation_selection.clone()),
        )
    }
    /// Rebuilds the visible grant set from the grant sets cached by loaded
    /// trees (root logs included), so it never reads a log (tree-local C4).
    pub(super) fn rebuild_visible_tree_grants(&self) {
        let invalidated = self.inner.grant_journals.invalidated_ids();
        let grants = self
            .inner
            .store
            .loaded_tree_grants()
            .into_iter()
            .filter(|grant| !invalidated.contains(&grant.grant_id))
            .collect::<Vec<_>>();
        self.inner.approvals.store.replace(grants);
    }

    /// The current runtime and the root policy a session created with
    /// `selection` would freeze. The caller reconciles the provider store
    /// first.
    pub(crate) fn freeze_selection_policy(
        &self,
        selection: &RunSelection,
    ) -> Result<
        (
            Arc<crate::runtime_snapshot::PublishedRuntime>,
            policy::FrozenRunPolicy,
        ),
        EngineError,
    > {
        let runtime = self.current_runtime();
        let agents = runtime.agents_for_preset(selection.preset.as_deref())?;
        let acp = agents
            .get(&selection.agent)
            .is_some_and(|agent| agent.acp.is_some());
        if !acp && runtime.result.snapshot.models.is_empty() {
            return Err(EngineError::NoRunnableModel);
        }
        let agent = resolve_agent(&agents, &selection.agent)?.clone();
        // An ACP agent names no model (`docs/site/specs/acp-agents.md`
        // section 4); one whose runtime is away is refused at once.
        if agent.acp.is_some() {
            if !agent.document.frontmatter.enabled
                || !matches!(
                    agent.document.frontmatter.mode,
                    cookie_agent_config::AgentMode::Primary | cookie_agent_config::AgentMode::All
                )
            {
                return Err(EngineError::DisabledAgent(agent.document.id.clone()));
            }
            let (policy, _) = policy::freeze_acp_agent_policy(
                &agent,
                agents,
                Arc::clone(&runtime),
                self.inner.config.load().runtime.delegation.max_depth,
                false,
                policy::ResultLimits::from_config(&self.inner.config.load().runtime),
                self.inner.config.load().runtime.model_retry,
                &crate::permissions::MemoryVariables::default(),
            )?;
            return Ok((runtime, policy));
        }
        let policy = freeze_root_agent_policy(
            &agent,
            agents,
            Arc::clone(&runtime),
            &selection.model,
            self.inner.config.load().runtime.delegation.max_depth,
            policy::ResultLimits::from_config(&self.inner.config.load().runtime),
            self.inner.config.load().runtime.model_retry,
            // A session that does not exist yet has no machine or project.
            &crate::permissions::MemoryVariables::default(),
        )?;
        Ok((runtime, policy))
    }

    /// [`Self::create_session`] that also commits an initial permission
    /// overlay and sets the new tree's permission mode before the session
    /// can run anything. The rules are validated before the session exists.
    ///
    /// `runtimes` names the runtimes the session attaches; instances that
    /// are not running are started first, and a failure creates nothing.
    pub async fn create_session_with_permissions(
        &self,
        selection: RunSelection,
        mode: Option<PermissionMode>,
        rules: Vec<PermissionRule>,
        runtimes: super::attachments::SessionRuntimes,
    ) -> Result<SessionMeta, EngineError> {
        self.create_session_in_project(selection, mode, rules, runtimes, None)
            .await
    }

    /// [`Self::create_session_with_permissions`] for a session that belongs
    /// to `project` (`docs/site/specs/projects.md` section 4): an unknown
    /// project is refused before anything is attached, the creation
    /// records `session_project_set`, and when `runtimes` names no runtimes
    /// the session attaches the project's default `global` and `runtimes`,
    /// if its defaults set them.
    pub async fn create_session_in_project(
        &self,
        selection: RunSelection,
        mode: Option<PermissionMode>,
        rules: Vec<PermissionRule>,
        runtimes: super::attachments::SessionRuntimes,
        project: Option<cookie_agent_protocol::ProjectId>,
    ) -> Result<SessionMeta, EngineError> {
        self.create_root_session(selection, mode, rules, runtimes, project, None)
            .await
    }

    /// [`Self::create_session_in_project`] for a session a cron job's fire
    /// creates when `cron` names the job and the session's title: its
    /// creation records the job (`SessionMeta::cron`) and the title.
    pub(crate) async fn create_root_session(
        &self,
        selection: RunSelection,
        mode: Option<PermissionMode>,
        rules: Vec<PermissionRule>,
        mut runtimes: super::attachments::SessionRuntimes,
        project: Option<cookie_agent_protocol::ProjectId>,
        cron: Option<(
            cookie_agent_protocol::SessionCron,
            cookie_agent_protocol::SessionTitle,
        )>,
    ) -> Result<SessionMeta, EngineError> {
        crate::permissions::check_rule_selectors(&rules)?;
        let overlay = SessionPermissionOverlay { rules };
        overlay
            .validate()
            .map_err(|error| EngineError::Permission(error.to_string()))?;
        if let Some(project) = &project {
            let defaults = self.project_attachment_defaults(project).await?;
            if runtimes.runtimes.is_none()
                && let Some(defaults) = defaults
            {
                runtimes.global = defaults.global;
                runtimes.runtimes = Some(
                    defaults
                        .runtimes
                        .into_iter()
                        .map(|runtime| super::attachments::SessionRuntimeRequest {
                            runtime_id: runtime.runtime_id.into_string(),
                            capability: runtime
                                .capability
                                .map(cookie_agent_protocol::CapabilityName::into_string),
                            workspace: runtime.workspace.map(std::path::PathBuf::from),
                        })
                        .collect(),
                );
            }
        }
        let attachments = self.resolve_session_runtimes(&runtimes).await?;
        let cwd = runtimes
            .cwd
            .as_deref()
            .unwrap_or_else(|| self.inner.store.cwd());
        self.reconcile_provider_store_off_worker().await?;
        let meta = self.create_session_in(selection, &attachments, cwd, project, cron)?;
        let session_id = meta.session_id;
        self.connect_session(session_id).await;
        if !overlay.rules.is_empty() {
            self.append(
                session_id,
                None,
                EventOrigin::new("user").expect("static event origin is valid"),
                Event::SessionPermissionOverlaySet { overlay },
            )
            .await?;
        }
        if let Some(mode) = mode {
            // A new tree has no other client to tell: its mode arrives with
            // the session through `session.permission.get`.
            self.store_permission_mode(session_id, mode)?;
        }
        Ok(meta)
    }

    /// Creates a root session attached to `global` and `local`.
    pub fn create_session(&self, selection: RunSelection) -> Result<SessionMeta, EngineError> {
        self.create_session_attached(selection, &self.default_attachments())
    }

    /// Creates a root session whose creation also records `attachments`.
    pub(crate) fn create_session_attached(
        &self,
        selection: RunSelection,
        attachments: &[cookie_agent_protocol::RuntimeAttachment],
    ) -> Result<SessionMeta, EngineError> {
        self.reconcile_provider_store()?;
        self.create_session_in(selection, attachments, self.inner.store.cwd(), None, None)
    }

    /// Creates a root session of the client directory `cwd` whose creation
    /// also records `attachments`, which are open. Its `cwd_identity` is the
    /// first workspace among them, else `cwd` ([`session_cwd_identity`]).
    /// A session in `project` records it with its creation.
    fn create_session_in(
        &self,
        selection: RunSelection,
        attachments: &[cookie_agent_protocol::RuntimeAttachment],
        cwd: &std::path::Path,
        project: Option<cookie_agent_protocol::ProjectId>,
        cron: Option<(
            cookie_agent_protocol::SessionCron,
            cookie_agent_protocol::SessionTitle,
        )>,
    ) -> Result<SessionMeta, EngineError> {
        let (runtime, policy) = self.freeze_selection_policy(&selection)?;
        let (cron, title) = cron.unzip();
        let id = SessionId::new_v7();
        let cwd_identity = session_cwd_identity(attachments, cwd)?;
        // A new root begins an empty tree, so its first handle is trivially
        // unique; it still goes through the same generation path so every
        // session carries a handle uniformly.
        let short_id = super::handles::generate_handle(
            selection.agent.as_str(),
            &HashSet::new(),
            super::handles::random_hex,
        )?;
        let creation = Event::SessionCreated {
            cron: cron.clone(),
            history_format: cookie_agent_protocol::HISTORY_FORMAT,
            origin: SessionOrigin::Root,
            short_id,
            cwd_identity: cwd_identity.clone(),
            creation_selection: selection.clone(),
            creation_agent: Box::new(policy.agent.clone()),
            revisions: crate::policy::runtime_revisions(
                &runtime,
                runtime.current_manifest.revision.clone(),
            ),
        };
        let mut initial = super::attachments::initial_attachment_events(attachments);
        if let Some(project) = project {
            initial.push(Event::SessionProjectSet {
                project: Some(project),
            });
        }
        // A run session's title is set with its creation, runless, as a
        // rename is, so no automatic title replaces it.
        if let (Some(title), Some(cron)) = (title, &cron) {
            initial.push(Event::SessionTitleCommitted {
                change: cookie_agent_protocol::SessionTitleChange::UserSet {
                    title,
                    client_rename_id: cookie_agent_protocol::ClientRenameId::new(format!(
                        "cron:{}:{}",
                        cron.job_id, cron.fired_at
                    ))
                    .map_err(|error| EngineError::ToolFailed(error.to_string()))?,
                },
                input_through_seq: 0,
            });
        }
        self.inner.store.create_with_initial(
            id,
            EventOrigin::new("user").expect("static event origin is valid"),
            creation,
            initial,
        )?;
        self.spawn_actor(id)?;
        Ok(self.inner.store.get(id)?.metadata())
    }

    /// Top-level session listing: delegation roots only. Delegated children are
    /// selected through their root's tree and stay reachable via
    /// [`Engine::get_session`], [`Engine::children`], [`Engine::tree`] and
    /// resume.
    ///
    /// A session whose project no longer exists lists as having none.
    #[must_use]
    pub fn list_sessions(&self) -> Vec<SessionMeta> {
        let known = self.project_ids();
        self.inner
            .store
            .list_roots()
            .into_iter()
            .filter(|meta| matches!(meta.origin, SessionOrigin::Root))
            .map(|meta| self.client_session_meta(meta, &known))
            .collect()
    }
    pub fn get_session(&self, id: SessionId) -> Result<SessionMeta, EngineError> {
        let session = self.inner.store.get(id)?;
        #[cfg(test)]
        let hook = self
            .inner
            .test_hooks
            .read_only_reopen_hook
            .lock()
            .expect("read-only reopen hook lock poisoned")
            .take();
        #[cfg(test)]
        if let Some(hook) = hook {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("read-only reopen reached lock poisoned")
                .take()
            {
                let _ = reached.send(());
            }
            let _ = hook
                .release
                .lock()
                .expect("read-only reopen release lock poisoned")
                .recv();
        }
        // A delegated session shows its root's project.
        Ok(self.client_session_meta(session.metadata(), &self.project_ids()))
    }

    #[cfg(test)]
    pub(crate) fn install_read_only_reopen_hook_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        *self
            .inner
            .test_hooks
            .read_only_reopen_hook
            .lock()
            .expect("read-only reopen hook lock poisoned") = Some(super::ReadOnlyReopenHook {
            reached: std::sync::Mutex::new(Some(reached)),
            release: std::sync::Mutex::new(release_receiver),
        });
        (receiver, release)
    }
    pub fn session_usage(&self, id: SessionId) -> Result<SessionUsageResult, EngineError> {
        let catalog = self.catalog_pricing();
        Ok(self.inner.store.session_usage(
            id,
            &self.inner.config.load().runtime.pricing,
            &catalog,
        )?)
    }
    pub fn session_tree_usage(&self, id: SessionId) -> Result<SessionTreeUsageResult, EngineError> {
        // A cold root's index is only a cache. Complete the tree before using
        // its summaries, otherwise a missing or stale index can produce a
        // plausible but incomplete rollup.
        self.inner.store.ensure_tree_for(id)?;
        let summaries = self.inner.store.tree_summaries(id)?;
        let mut children: HashMap<SessionId, Vec<(SessionId, InvocationId)>> = HashMap::new();
        let mut rollups = HashMap::new();
        for summary in summaries {
            let session_id = summary.meta.session_id;
            if let SessionOrigin::Delegated {
                root_session_id,
                parent_session_id,
                invocation_id,
                ..
            } = summary.meta.origin
                && root_session_id == id
            {
                let edge = (session_id, invocation_id);
                children.entry(parent_session_id).or_default().push(edge);
            }
            rollups.insert(session_id, summary.usage_rollup);
        }

        let mut usage = UsageRollup::default();
        let session_count = merge_session_tree_usage(id, &children, &rollups, &mut usage)?;
        Ok(SessionTreeUsageResult {
            session_id: id,
            usage: crate::usage::with_pricing(
                usage,
                &self.inner.config.load().runtime.pricing,
                &self.catalog_pricing(),
            ),
            session_count,
        })
    }
    pub(super) fn catalog_pricing(
        &self,
    ) -> BTreeMap<cookie_agent_protocol::ModelKey, cookie_agent_models::catalog::CatalogModelCost>
    {
        self.inner
            .model_manager
            .current()
            .models()
            .iter()
            .filter_map(|(model, runtime)| {
                runtime
                    .model
                    .cost
                    .as_ref()
                    .map(|cost| (model.clone(), cost.clone()))
            })
            .collect()
    }
    pub fn delegate_targets(&self, id: SessionId) -> Result<Vec<AgentId>, EngineError> {
        let session = self.inner.store.get(id)?;
        let active = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .find(|active| active.session == id)
            .cloned();
        let Some(active) = active else {
            return Ok(Vec::new());
        };
        let depth = session_depth(&session.meta.origin);
        Ok(active.policy.delegate_targets(depth))
    }

    /// Why the run in progress of session `id` may not delegate to
    /// `target`, which is not one of its targets.
    pub fn delegate_refusal(&self, id: SessionId, target: &AgentId) -> String {
        self.inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .find(|active| active.session == id)
            .map_or_else(
                || "delegate target is not allowed".to_owned(),
                |active| active.policy.delegate_refusal(target),
            )
    }
    pub fn children(
        &self,
        id: SessionId,
    ) -> Result<Vec<cookie_agent_protocol::ChildSummary>, EngineError> {
        // Listing is a use of the tree: complete it first so nested delegations
        // and their summaries are in view (§3.2.2). An incomplete tree is
        // reported, never papered over with the pre-load cache (review L14).
        self.ensure_tree_loaded(id)?;
        let known: HashSet<_> = self
            .inner
            .delegation_events
            .entries()
            .into_iter()
            .map(|entry| {
                (
                    entry.reservation.invocation_id,
                    entry.reservation.parent_session_id,
                    entry.reservation.child_session_id,
                )
            })
            .collect();
        Ok(self
            .inner
            .store
            .tree_summaries(id)?
            .into_iter()
            .filter_map(|child| match child.meta.origin {
                SessionOrigin::Delegated {
                    parent_session_id,
                    invocation_id,
                    ..
                } if parent_session_id == id
                    && known.contains(&(
                        invocation_id,
                        parent_session_id,
                        child.meta.session_id,
                    )) =>
                {
                    Some(ChildSummary {
                        session_id: child.meta.session_id,
                        agent: child.meta.creation_selection.agent.clone(),
                        title: child.meta.title.clone(),
                        title_updated_seq: child.meta.title_updated_seq,
                        status: child.meta.status,
                        usage: child.usage,
                    })
                }
                _ => None,
            })
            .collect())
    }

    pub fn tree(&self, id: SessionId) -> Result<cookie_agent_protocol::SessionTree, EngineError> {
        // The load folded every log in the tree, so the summaries below never
        // read one again, and a missing session fails there (tree-local C6).
        self.ensure_tree_loaded(id)?;
        self.tree_summary(id)
    }

    fn tree_summary(
        &self,
        id: SessionId,
    ) -> Result<cookie_agent_protocol::SessionTree, EngineError> {
        Ok(cookie_agent_protocol::SessionTree {
            session: self
                .inner
                .store
                .with_live_waiting(self.inner.store.summary(id)?.meta),
            children: self
                .children(id)?
                .into_iter()
                .map(|child| self.tree_summary(child.session_id))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
    /// Adopts `id` and resolves the work its adoption found interrupted.
    ///
    /// Adoption schedules delegation recovery instead of running it inline, so
    /// the wait below is what makes the ordering observable: when this returns,
    /// every delegation record under the session's root that the adoption
    /// classified as running without a live run here has been resolved, and the
    /// root's background-delegation capacity reads settled rather than
    /// transiently over-counted.
    ///
    /// The wait happens here, after the actor has replied, and never inside the
    /// `Resume` handler: resolving a recovered delegate goes through its parent
    /// session's mailbox, which is the very mailbox that handler occupies.
    pub async fn resume(&self, id: SessionId) -> Result<SessionMeta, EngineError> {
        let meta = self
            .request(id, |reply| SessionCommand::Resume { reply })
            .await?;
        let root = self.inner.store.root_of(id).unwrap_or(id);
        if !self
            .await_delegation_recovery(root, DELEGATION_RECOVERY_SETTLE_BOUND)
            .await
        {
            eprintln!(
                "session {id} delegation recovery still settling after {DELEGATION_RECOVERY_SETTLE_BOUND:?}"
            );
        }
        self.connect_session(id).await;
        self.drain_queued_input(id).await;
        Ok(self.inner.store.with_live_waiting(meta))
    }
    /// Delegated sessions a revert to `cut` deletes: children whose
    /// reservation the cut removes (and no kept reservation resumes), with all
    /// their descendants. A running one refuses the revert.
    fn reverted_children(
        &self,
        events: &[Arc<cookie_agent_protocol::StoredEvent>],
        cut: u64,
    ) -> Result<HashSet<SessionId>, EngineError> {
        let mut kept = HashSet::new();
        let mut removed = Vec::new();
        for event in events.iter() {
            if let Event::DelegationReserved { reservation, .. } = &event.payload {
                if event.seq <= cut {
                    kept.insert(reservation.child_session_id);
                } else {
                    removed.push(reservation.child_session_id);
                }
            }
        }
        let mut deleted = HashSet::new();
        let mut pending = removed
            .into_iter()
            .filter(|child| !kept.contains(child))
            .collect::<Vec<_>>();
        while let Some(child) = pending.pop() {
            if !deleted.insert(child) {
                continue;
            }
            match self.inner.store.get(child) {
                Ok(projection) if projection.status == SessionStatus::Running => {
                    return Err(EngineError::SessionRunning(child));
                }
                Ok(_) => {}
                // Never published, or already gone: nothing on disk to keep.
                Err(SessionError::Missing(_)) => continue,
                Err(error) => return Err(error.into()),
            }
            pending.extend(
                self.inner
                    .store
                    .children(child)?
                    .into_iter()
                    .map(|summary| summary.session_id),
            );
        }
        Ok(deleted)
    }

    /// Forgets the in-memory state of sessions a revert deletes.
    pub(super) fn forget_deleted_sessions(&self, deleted: &HashSet<SessionId>) {
        if deleted.is_empty() {
            return;
        }
        fn forget<V>(map: &std::sync::Mutex<HashMap<SessionId, V>>, deleted: &HashSet<SessionId>) {
            map.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|session, _| !deleted.contains(session));
        }
        self.kill_deleted_background_jobs(deleted);
        forget(&self.inner.sessions.actors, deleted);
        forget(&self.inner.sessions.producers, deleted);
        forget(&self.inner.sessions.producer_projections, deleted);
        forget(&self.inner.skills_runtime.grants, deleted);
        forget(&self.inner.skills_runtime.models, deleted);
        forget(&self.inner.skills_runtime.pending_child, deleted);
        forget(&self.inner.compaction.deferred, deleted);
        forget(&self.inner.compaction.context_token_estimators, deleted);
        forget(&self.inner.delegation.recovery_pending, deleted);
        self.inner
            .delegation
            .by_session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|child, record| {
                !deleted.contains(child) && !deleted.contains(&record.parent_session_id)
            });
        self.inner
            .delegation
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|child| !deleted.contains(child));
        self.inner.delegation_events.forget_sessions(deleted);
        self.forget_live_waiting(deleted);
        self.delete_crons_targeting(deleted);
    }

    /// Reverts `session_id` to `through_seq`. With `expected_last_seq`, a
    /// session whose log has moved past it is refused with
    /// [`EngineError::StaleRevert`] instead of losing what was added.
    pub async fn revert_session(
        &self,
        session_id: SessionId,
        through_seq: u64,
        _origin: EventOrigin,
        expected_last_seq: Option<u64>,
    ) -> Result<SessionRevertResult, EngineError> {
        self.ensure_appendable(session_id)?;
        let context_id = crate::plugin::plugin_context_id();
        let mut instructions = self
            .inner
            .store
            .get(session_id)?
            .log
            .event_snapshot()
            .iter()
            .find_map(|event| match &event.payload {
                cookie_agent_protocol::EventPayload::UserInputSubmitted { content }
                    if event.seq > through_seq =>
                {
                    Some(content.as_plain_text().into_owned())
                }
                _ => None,
            });
        let mut instructions_override = None;
        for hooked in self.inner.plugins.interception_plugins(
            cookie_agent_protocol::ExtensionInterceptionHook::SessionBeforeRevert,
        ) {
            let plugin = hooked.name.clone();
            let result = self
                .inner
                .plugins
                .intercept_named::<_, cookie_agent_protocol::ExtensionSessionBeforeRevertResult>(
                    &plugin,
                    cookie_agent_protocol::PLUGIN_INTERCEPT_SESSION_BEFORE_REVERT_METHOD,
                    &cookie_agent_protocol::ExtensionSessionBeforeRevertParams {
                        session_id,
                        context_id: context_id.clone(),
                        through_seq,
                        instructions: instructions.clone(),
                    },
                    Some(session_id),
                    Some(&context_id),
                )
                .await;
            match result {
                Ok(result)
                    if result.action
                        == cookie_agent_protocol::ExtensionSessionBeforeRevertAction::Block =>
                {
                    let reason = result
                        .reason
                        .unwrap_or_else(|| format!("session revert blocked by {plugin}"));
                    self.record_plugin_diagnostic(
                        session_id,
                        plugin,
                        cookie_agent_protocol::PluginDiagnosticKind::HookBlocked,
                        reason.clone(),
                    );
                    return Err(EngineError::SessionOperationBlocked(reason));
                }
                Ok(result)
                    if result.action
                        == cookie_agent_protocol::ExtensionSessionBeforeRevertAction::Override =>
                {
                    if let Some(replacement) = result
                        .instructions_override
                        .filter(|value| !value.trim().is_empty())
                    {
                        instructions = Some(replacement.clone());
                        instructions_override = Some(replacement);
                    } else {
                        self.record_plugin_diagnostic(
                            session_id,
                            plugin,
                            cookie_agent_protocol::PluginDiagnosticKind::InvalidModification,
                            "empty revert instruction override".into(),
                        );
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    if let Some(failure) = self.hook_failed(
                        session_id,
                        &hooked,
                        cookie_agent_protocol::ExtensionInterceptionHook::SessionBeforeRevert,
                        error,
                    ) {
                        return Err(EngineError::PluginHookFailed(failure));
                    }
                }
            }
        }
        let session = session_id;
        let (result, dropped) = self
            .on_actor_async(session, move |engine| async move {
                if !engine.reserve_compaction(session) {
                    return Err(EngineError::SessionRunning(session));
                }
                let result = (|| {
                    let projection = engine.inner.store.get(session)?;
                    if projection.status == SessionStatus::Running {
                        return Err(EngineError::SessionRunning(session));
                    }
                    let tip = projection.log.last_event().map_or(0, |event| event.seq);
                    if expected_last_seq.is_some_and(|expected| expected != tip) {
                        return Err(EngineError::StaleRevert {
                            session_id: session,
                            current_last_seq: tip,
                        });
                    }
                    if through_seq == 0 || through_seq > tip {
                        return Err(SessionError::InvalidSequence {
                            session_id: session,
                            through_seq,
                        }
                        .into());
                    }
                    // A revert removes the events after the cut: the log keeps no
                    // marker, and later appends reuse the freed sequences.
                    let events = projection.log.event_snapshot();
                    let cut = revert_cut(&events, through_seq);
                    let deleted = engine.reverted_children(&events, cut)?;
                    // What the cut events and deleted children referenced is all a
                    // revert can orphan; the sweep below keeps what is still used.
                    let mut dropped = HashSet::new();
                    crate::runtime::artifacts::artifact_references_in_log(
                        &projection.log,
                        cut + 1..,
                        &mut dropped,
                    )?;
                    for child in &deleted {
                        if let Ok(child) = engine.inner.store.get(*child) {
                            crate::runtime::artifacts::artifact_references_in_log(
                                &child.log,
                                ..,
                                &mut dropped,
                            )?;
                        }
                    }
                    engine.inner.store.truncate(session, cut)?;
                    // The cut can fall inside a run, with its tool calls, internal
                    // agents or approvals still open: close them as a restart would.
                    engine.reconcile_session_with(session, "session reverted")?;
                    engine
                        .inner
                        .compaction
                        .context_token_estimators
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&session);
                    engine.rebuild_visible_tree_grants();
                    engine.inner.delegation_events.reconcile_parent(session)?;
                    engine.reconcile_reverted_producers_direct(session)?;
                    engine.forget_deleted_sessions(&deleted);
                    engine
                        .inner
                        .store
                        .delete_children(&deleted.iter().copied().collect::<Vec<_>>())?;
                    Ok((
                        SessionRevertResult {
                            session: engine.inner.store.get(session)?.metadata(),
                            instructions_override,
                        },
                        dropped,
                    ))
                })();
                engine.release_compaction_direct(session).await;
                result
            })
            .await?;
        self.collect_reverted_artifacts(session, dropped).await;
        Ok(result)
    }

    /// Deletes the artifacts only the reverted events referenced, off the async
    /// workers. A failure leaves orphans behind, never a broken session.
    ///
    /// An upload still leased, or an artifact an input being admitted holds,
    /// is kept: the lease lock is held throughout, so neither can change
    /// while the sweep decides (see [`super::uploads`]).
    async fn collect_reverted_artifacts(&self, session: SessionId, dropped: HashSet<String>) {
        if dropped.is_empty() {
            return;
        }
        let engine = self.clone();
        let collected = super::blocking_io::run(move || {
            let leases = engine
                .inner
                .uploads
                .leases
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let held = leases.held(engine.inner.artifacts.tree_for(session));
            let dropped = dropped.difference(&held).cloned().collect();
            engine
                .inner
                .artifacts
                .collect_reverted(session, dropped, || {
                    engine.tree_artifact_references(session)
                })
        })
        .await;
        match collected {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => eprintln!("artifact collection after revert failed: {error}"),
            Err(error) => eprintln!("artifact collection after revert failed: {error}"),
        }
    }

    /// Every artifact reference the logs of `session`'s tree hold, or `None`
    /// while a session of the tree runs and may still add one.
    pub(super) fn tree_artifact_references(
        &self,
        session: SessionId,
    ) -> std::io::Result<Option<HashSet<String>>> {
        let store = &self.inner.store;
        let projection = store.get(session).map_err(std::io::Error::other)?;
        let mut pending = vec![root_id(&projection.meta.origin, session)];
        let mut seen = HashSet::new();
        let mut references = HashSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            let projection = match store.get(id) {
                Ok(projection) => projection,
                Err(SessionError::Missing(_)) => continue,
                Err(error) => return Err(std::io::Error::other(error)),
            };
            if projection.status == SessionStatus::Running {
                return Ok(None);
            }
            crate::runtime::artifacts::artifact_references_in_log(
                &projection.log,
                ..,
                &mut references,
            )?;
            pending.extend(
                store
                    .children(id)
                    .map_err(std::io::Error::other)?
                    .into_iter()
                    .map(|summary| summary.session_id),
            );
        }
        Ok(Some(references))
    }
    /// Forks `session_id` through `through_seq`. A retry with the same
    /// `client_fork_id` returns the fork the first request made, for as long
    /// as this engine runs; reusing the key for another fork is
    /// [`EngineError::ForkIdempotencyConflict`].
    pub async fn fork_session(
        &self,
        session_id: SessionId,
        through_seq: u64,
        origin: EventOrigin,
        client_fork_id: Option<cookie_agent_protocol::ClientForkId>,
    ) -> Result<SessionForkResult, EngineError> {
        // A render-only session cannot be continued, so it is not forked either.
        self.ensure_appendable(session_id)?;
        if let Some(client_fork_id) = &client_fork_id
            && let Some(&(forked_through, forked)) = self
                .inner
                .sessions
                .forks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&(session_id, client_fork_id.clone()))
        {
            return if forked_through == through_seq {
                Ok(SessionForkResult { session_id: forked })
            } else {
                Err(EngineError::ForkIdempotencyConflict)
            };
        }
        let context_id = crate::plugin::plugin_context_id();
        for hooked in self.inner.plugins.interception_plugins(
            cookie_agent_protocol::ExtensionInterceptionHook::SessionBeforeFork,
        ) {
            let plugin = hooked.name.clone();
            let result = self
                .inner
                .plugins
                .intercept_named::<_, cookie_agent_protocol::ExtensionSessionBeforeForkResult>(
                    &plugin,
                    cookie_agent_protocol::PLUGIN_INTERCEPT_SESSION_BEFORE_FORK_METHOD,
                    &cookie_agent_protocol::ExtensionSessionBeforeForkParams {
                        session_id,
                        context_id: context_id.clone(),
                        through_seq,
                    },
                    Some(session_id),
                    Some(&context_id),
                )
                .await;
            match result {
                Ok(result)
                    if result.action == cookie_agent_protocol::ExtensionAllowBlockAction::Block =>
                {
                    let reason = result
                        .reason
                        .unwrap_or_else(|| format!("session fork blocked by {plugin}"));
                    self.record_plugin_diagnostic(
                        session_id,
                        plugin,
                        cookie_agent_protocol::PluginDiagnosticKind::HookBlocked,
                        reason.clone(),
                    );
                    return Err(EngineError::SessionOperationBlocked(reason));
                }
                Ok(_) => {}
                Err(error) => {
                    if let Some(failure) = self.hook_failed(
                        session_id,
                        &hooked,
                        cookie_agent_protocol::ExtensionInterceptionHook::SessionBeforeFork,
                        error,
                    ) {
                        return Err(EngineError::PluginHookFailed(failure));
                    }
                }
            }
        }
        // A fork into a new tree takes the artifacts its history references
        // along, so it never resolves anything in the tree it came from
        // (tree-local D2).
        let mut referenced = HashSet::new();
        crate::runtime::artifacts::artifact_references_in_log(
            &*self.inner.store.log(session_id)?,
            ..=through_seq,
            &mut referenced,
        )?;
        let forked = self
            .inner
            .store
            .fork_with(session_id, through_seq, origin, &|tree| {
                self.inner.artifacts.link_into(
                    session_id,
                    &tree.join(crate::runtime::artifacts::ARTIFACTS_DIR),
                    &referenced,
                )
            })?;
        let producer_state = self.goal_producer_projection(forked)?;
        if producer_state.goal.is_some()
            || producer_state
                .messages
                .iter()
                .any(|message| !message.consumed && !message.discarded)
        {
            self.reconcile_producers(forked).await?;
        }
        if let Some(client_fork_id) = client_fork_id {
            self.inner
                .sessions
                .forks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert((session_id, client_fork_id), (through_seq, forked));
        }
        Ok(SessionForkResult { session_id: forked })
    }
    /// Sets the tree's permission mode and tells every connection,
    /// naming `origin` as who changed it.
    pub fn set_permission_mode(
        &self,
        session_id: SessionId,
        mode: PermissionMode,
        origin: EventOrigin,
    ) -> Result<(), EngineError> {
        let root = self.store_permission_mode(session_id, mode)?;
        self.inner.store.notify_index_change(
            cookie_agent_protocol::SessionsChangedNotification::PermissionModeChanged {
                root_session_id: root,
                mode,
                changed_by: origin,
            },
        );
        Ok(())
    }

    fn store_permission_mode(
        &self,
        session_id: SessionId,
        mode: PermissionMode,
    ) -> Result<SessionId, EngineError> {
        self.ensure_session_owned(session_id)?;
        let session = self.inner.store.get(session_id)?;
        let root = root_id(&session.meta.origin, session_id);
        self.inner
            .approvals
            .permission_modes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(root, mode);
        Ok(root)
    }
    pub(crate) fn permission_mode(&self, session_id: SessionId) -> PermissionMode {
        let root = self
            .inner
            .store
            .get(session_id)
            .map(|session| root_id(&session.meta.origin, session_id))
            .unwrap_or(session_id);
        self.inner
            .approvals
            .permission_modes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&root)
            .copied()
            .unwrap_or_default()
    }
    pub async fn rename_session(
        &self,
        params: SessionRenameParams,
        _origin: EventOrigin,
    ) -> Result<SessionRenameResult, EngineError> {
        let session_id = params.session_id;
        let reset = matches!(params.change, SessionRenameChange::Reset);
        // A rename appends only `SessionTitleCommitted`, which producer
        // reconciliation never reads.
        let mut result = self
            .on_actor_unreconciled(session_id, move |engine| {
                let projection = engine.inner.store.get(session_id)?;
                if let Some(record) = projection.rename_records.get(&params.client_rename_id) {
                    if record.conflicts_with(&params) {
                        return Err(EngineError::RenameConflict);
                    }
                    return Ok(SessionRenameResult {
                        client_rename_id: params.client_rename_id,
                        session: projection.metadata(),
                    });
                }
                let commit = match params.change {
                    SessionRenameChange::Set { title } => SessionTitleChange::UserSet {
                        title,
                        client_rename_id: params.client_rename_id.clone(),
                    },
                    SessionRenameChange::Clear => SessionTitleChange::UserClear {
                        client_rename_id: params.client_rename_id.clone(),
                    },
                    SessionRenameChange::Reset => SessionTitleChange::UserReset {
                        client_rename_id: params.client_rename_id.clone(),
                    },
                };
                let input_through_seq = projection.log.last_event().map_or(0, |event| event.seq);
                engine.append_direct(
                    session_id,
                    None,
                    super::event_origin("user"),
                    Event::SessionTitleCommitted {
                        input_through_seq,
                        change: commit,
                    },
                )?;
                Ok(SessionRenameResult {
                    client_rename_id: params.client_rename_id,
                    session: engine.inner.store.get(session_id)?.metadata(),
                })
            })
            .await?;
        if reset {
            self.generate_title_after_reset(session_id).await?;
            result.session = self.inner.store.get(session_id)?.metadata();
        }
        Ok(result)
    }
}

/// Where a revert to `through_seq` cuts the log. A run the cut would leave
/// holding nothing but its start goes too: reverting to just before a message
/// targets the event before it, which is usually that run's `RunStarted`.
fn revert_cut(events: &[Arc<cookie_agent_protocol::StoredEvent>], through_seq: u64) -> u64 {
    let mut cut = through_seq;
    loop {
        let mut open = None;
        let mut started = false;
        for event in events.iter().take_while(|event| event.seq <= cut) {
            match &event.payload {
                Event::RunStarted { .. } => {
                    open = event.run_id.map(|run| (run, event.seq));
                    started = false;
                }
                Event::RunFinished { .. }
                    if open.is_some_and(|(run, _)| event.run_id == Some(run)) =>
                {
                    open = None;
                }
                Event::UserInputSubmitted { .. }
                | Event::UserInputApplied { .. }
                | Event::ModelAttemptStarted { .. }
                | Event::ToolCallStarted { .. }
                    if open.is_some_and(|(run, _)| event.run_id == Some(run)) =>
                {
                    started = true;
                }
                _ => {}
            }
        }
        match open {
            Some((_, start)) if !started && start > 1 => cut = start - 1,
            _ => return cut,
        }
    }
}

#[cfg(test)]
fn validated_tree_usage_edge(
    origin: &SessionOrigin,
    child_session_id: SessionId,
    known: &HashSet<(InvocationId, SessionId, SessionId)>,
) -> Option<(SessionId, (SessionId, InvocationId))> {
    match origin {
        SessionOrigin::Delegated {
            parent_session_id,
            invocation_id,
            ..
        } if known.contains(&(*invocation_id, *parent_session_id, child_session_id)) => {
            Some((*parent_session_id, (child_session_id, *invocation_id)))
        }
        _ => None,
    }
}

fn merge_session_tree_usage(
    session_id: SessionId,
    children: &HashMap<SessionId, Vec<(SessionId, InvocationId)>>,
    rollups: &HashMap<SessionId, UsageRollup>,
    usage: &mut UsageRollup,
) -> Result<u64, EngineError> {
    let mut visited = HashSet::new();
    let mut pending = vec![(session_id, None)];
    let mut session_count = 0_u64;
    while let Some((current_id, incoming_invocation)) = pending.pop() {
        if !visited.insert(current_id) {
            let invocation_id = incoming_invocation.expect("only descendants can repeat");
            return Err(
                crate::delegation_events::DelegationEventError::Corrupt(invocation_id).into(),
            );
        }
        if let Some(rollup) = rollups.get(&current_id) {
            crate::usage::merge(usage, rollup);
            session_count = session_count.saturating_add(1);
        }
        if let Some(descendants) = children.get(&current_id) {
            pending.extend(
                descendants
                    .iter()
                    .map(|(child_id, invocation_id)| (*child_id, Some(*invocation_id))),
            );
        }
    }
    Ok(session_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_usage_tree_cycle_is_reported_as_delegation_corruption() {
        let first = SessionId::new_v7();
        let second = SessionId::new_v7();
        let first_to_second = InvocationId::new_v7();
        let second_to_first = InvocationId::new_v7();
        let first_run = cookie_agent_protocol::RunId::new_v7();
        let second_run = cookie_agent_protocol::RunId::new_v7();
        let first_origin = SessionOrigin::Delegated {
            root_session_id: first,
            parent_session_id: second,
            parent_run_id: second_run,
            parent_tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
            invocation_id: second_to_first,
            depth: 2,
        };
        let second_origin = SessionOrigin::Delegated {
            root_session_id: first,
            parent_session_id: first,
            parent_run_id: first_run,
            parent_tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
            invocation_id: first_to_second,
            depth: 1,
        };
        let known = HashSet::from([
            (first_to_second, first, second),
            (second_to_first, second, first),
        ]);
        let mut children: HashMap<SessionId, Vec<(SessionId, InvocationId)>> = HashMap::new();
        for (session_id, origin) in [(first, first_origin), (second, second_origin)] {
            let (parent_id, edge) = validated_tree_usage_edge(&origin, session_id, &known)
                .expect("matching origin and reservation triple");
            children.entry(parent_id).or_default().push(edge);
        }
        let rollups = HashMap::from([
            (first, UsageRollup::default()),
            (second, UsageRollup::default()),
        ]);
        let error =
            merge_session_tree_usage(first, &children, &rollups, &mut UsageRollup::default())
                .expect_err("cycle must fail");
        assert!(matches!(
            error,
            EngineError::DelegationEvents(
                crate::delegation_events::DelegationEventError::Corrupt(invocation_id)
            ) if invocation_id == second_to_first
        ));
    }
}
