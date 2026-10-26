//! Runtime, tree, and delivery refresh scheduling for [`App`].

use super::*;

impl App {
    /// Record the authoritative title sequence from a session meta patch.
    pub(in crate::ui) fn note_title_sequence(&mut self, session: &SessionMeta) {
        let known = self.title_sequences.entry(session.session_id).or_insert(0);
        *known = (*known).max(session.title_updated_seq);
    }

    /// Merge one session meta patch: strictly newer title sequences win; a
    /// stale patch retains the newer known title and never overwrites it.
    pub(in crate::ui) fn merge_session_meta(&mut self, session: SessionMeta) -> SessionMeta {
        let known = self
            .title_sequences
            .get(&session.session_id)
            .copied()
            .unwrap_or(0);
        let mut session = session;
        let known_status = self
            .sessions
            .iter()
            .filter(|existing| existing.session_id == session.session_id)
            .map(|existing| (existing.last_event_seq, existing.status))
            .chain(
                self.tree
                    .as_ref()
                    .and_then(|tree| find_session(tree, session.session_id))
                    .map(|existing| (existing.last_event_seq, existing.status)),
            )
            .max_by_key(|(seq, _)| *seq);
        if session.title_updated_seq < known
            && let Some(current) = self
                .sessions
                .iter()
                .find(|existing| existing.session_id == session.session_id)
                .or_else(|| {
                    self.tree
                        .as_ref()
                        .and_then(|tree| find_session(tree, session.session_id))
                })
                .cloned()
        {
            session.title = current.title;
            session.title_updated_seq = known;
        }
        if let Some((seq, status)) = known_status
            && session.last_event_seq < seq
        {
            session.last_event_seq = seq;
            session.status = status;
        }
        self.note_title_sequence(&session);
        session
    }

    pub(in crate::ui) async fn drain_replay(&mut self, session_id: SessionId) {
        loop {
            let delivery = match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.deliveries
                    .as_mut()
                    .expect("app delivery receiver attached")
                    .recv(),
            )
            .await
            {
                Ok(Some(delivery)) => delivery,
                Ok(None) => return,
                Err(_) => {
                    for replay_session in self.store.abandon_replays() {
                        self.recover_session(replay_session);
                    }
                    self.status = "replay timed out; retrying recovery".into();
                    return;
                }
            };
            let finished = matches!(
                &delivery,
                ClientDelivery::ReplayEnd { session_id: replay_session, .. } if *replay_session == session_id
            );
            self.handle_delivery(delivery).await;
            if finished {
                return;
            }
        }
    }

    pub(in crate::ui) async fn refresh_lists(&mut self) {
        self.refresh_session_list().await;
        self.refresh_coherent_lists().await;
        self.poll_runtime_homes();
        self.poll_projects();
    }

    /// Fetches the runtimes' homes in the background, from a daemon that
    /// reports them (client protocol 1.7.0). A failure keeps the homes
    /// known so far: paths then show as they did.
    pub(in crate::ui) fn poll_runtime_homes(&mut self) {
        if !cookie_agent_client_state::paths::daemon_reports_runtime_homes(
            self.client.daemon_version(),
        ) || !self.runtime_homes_refresh.start()
        {
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let homes = client.list_runtimes().await.ok().map(|listing| {
                cookie_agent_client_state::paths::RuntimeHomes::from_listing(&listing)
            });
            let _ = updates.send(RpcUpdate::RuntimeHomesRefreshed { homes });
        });
    }

    /// Installs the runtimes' homes; the session picker's rows name
    /// directories with them, so a change rebuilds them.
    pub(in crate::ui) fn install_runtime_homes(
        &mut self,
        homes: cookie_agent_client_state::paths::RuntimeHomes,
    ) {
        if self.runtime_homes != homes {
            self.runtime_homes = homes;
            self.note_sessions_changed();
        }
    }

    /// Lists this directory's root sessions, or every directory's when the
    /// picker shows all of them.
    pub(in crate::ui) async fn refresh_session_list(&mut self) {
        let cwd_identity = if self.session_scope_all {
            None
        } else {
            self.cwd
                .as_deref()
                .and_then(|cwd| cookie_agent_protocol::CwdIdentity::new(cwd).ok())
        };
        match self
            .client
            .list_sessions(SessionListParams {
                cwd_identity,
                ..SessionListParams::default()
            })
            .await
        {
            Ok(result) => {
                self.sessions = result
                    .sessions
                    .into_iter()
                    .map(|session| self.merge_session_meta(session))
                    .collect();
                self.note_sessions_changed();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    /// Fetch and install the sole protocol-10 discovery object.
    pub(in crate::ui) async fn refresh_coherent_lists(&mut self) {
        match self.client.runtime_snapshot().await {
            Ok(result) => self.install_initial_runtime(result.snapshot),
            Err(error) => {
                self.runtime.set_error(error.to_string());
                self.status =
                    format!("Runtime snapshot unavailable: {error}. Press Enter to retry.");
            }
        }
    }

    pub(in crate::ui) fn install_initial_runtime(
        &mut self,
        snapshot: cookie_agent_protocol::ModelCatalogView,
    ) {
        if self.runtime.snapshot().is_some() {
            return;
        }
        self.runtime.install_initial(snapshot.clone());
        self.install_runtime_projection(snapshot);
    }

    pub(in crate::ui) fn install_runtime_response(
        &mut self,
        baseline: Option<&cookie_agent_protocol::ViewRevision>,
        snapshot: cookie_agent_protocol::ModelCatalogView,
    ) -> bool {
        if !self.runtime.install_response(baseline, snapshot.clone()) {
            return false;
        }
        self.install_runtime_projection(snapshot);
        true
    }

    /// Applies a daemon-wide session index change, which also covers
    /// sessions this client does not watch: another client's new sessions,
    /// status and title changes, approvals anywhere in the viewed tree, and
    /// permission-mode changes.
    pub(in crate::ui) async fn apply_sessions_change(
        &mut self,
        change: cookie_agent_protocol::SessionsChangedNotification,
    ) {
        use cookie_agent_protocol::SessionsChangedNotification as Change;
        match change {
            Change::Upserted { session } => {
                let session = self.merge_session_meta(*session);
                if let Some(existing) = self
                    .sessions
                    .iter_mut()
                    .find(|existing| existing.session_id == session.session_id)
                {
                    *existing = session;
                } else if matches!(session.origin, cookie_agent_protocol::SessionOrigin::Root)
                    && (self.session_scope_all
                        || self
                            .cwd
                            .as_deref()
                            .is_none_or(|cwd| session.cwd_identity.as_str() == cwd))
                {
                    // A root another client created; children join the cache
                    // when the Agents tree loads them.
                    self.sessions.push(session);
                } else {
                    return;
                }
                self.note_sessions_changed();
            }
            // A revert's deleted subagent or a deleted tree, by any client.
            Change::Removed { session_id } => self.forget_removed_session(session_id),
            Change::ApprovalsChanged {
                root_session_id, ..
            } => {
                if let Some(selected) = self.selected
                    && self.permission_mode_root(selected) == root_session_id
                {
                    self.refresh_approvals(selected);
                }
            }
            Change::PermissionModeChanged {
                root_session_id,
                mode,
                changed_by,
            } => {
                // Supersede any in-flight load, which may predate this change.
                self.next_permission_mode_generation(root_session_id);
                self.permission_modes.insert(root_session_id, mode);
                if self.client.origin().as_ref() != Some(&changed_by)
                    && self.selected.is_some_and(|selected| {
                        self.permission_mode_root(selected) == root_session_id
                    })
                {
                    self.status = format!(
                        "Permission mode: {} — changed by another client",
                        permission_mode_label(mode)
                    );
                }
            }
            Change::Resync => {
                // Changes were dropped for this connection: reload what they
                // would have updated.
                self.refresh_lists().await;
                if let Some(selected) = self.selected {
                    self.refresh_approvals(selected);
                    self.refresh_permission_mode_for_session(selected);
                }
            }
        }
    }

    pub(in crate::ui) fn request_runtime_resync(&mut self) {
        if !self.runtime_resync.start() {
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .runtime_snapshot()
                .await
                .map(|result| Box::new(result.snapshot))
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::RuntimeResynced { result });
        });
    }

    pub(in crate::ui) fn install_runtime_notification(
        &mut self,
        changed: cookie_agent_protocol::RuntimeChangedNotification,
    ) -> bool {
        let snapshot = changed.snapshot.clone();
        if !self.runtime.apply_notification(changed) {
            return false;
        }
        self.install_runtime_projection(snapshot);
        true
    }

    pub(super) fn install_runtime_projection(
        &mut self,
        snapshot: cookie_agent_protocol::ModelCatalogView,
    ) {
        self.catalog_view_revision = Some(snapshot.revision);
        self.providers = snapshot.providers;
        self.models = snapshot.models;
        self.agents = snapshot.agents;
        self.runtime_panel.agents.clone_from(&self.agents);
        if self
            .selected_preset
            .as_ref()
            .is_some_and(|selected| !self.preset_names().contains(selected))
        {
            self.selected_preset = None;
        }
        self.revalidate_draft();
        self.revalidate_new_session_draft();
        if self.runtime.is_empty() && self.watching_root_session() {
            self.draft = None;
            self.status = EMPTY_RUNTIME_GUIDANCE.into();
        } else if self.draft.is_none() {
            self.draft = self.default_draft_selection();
        }
    }

    pub(in crate::ui) fn refresh_tree_background(&mut self) {
        let Some(root) = self.tree_root else {
            return;
        };
        self.refresh_tree_background_for(root, self.selection_generation);
    }

    pub(super) fn refresh_tree_background_for(&mut self, session_id: SessionId, generation: u64) {
        if self.tree_refresh_in_flight.is_some() {
            self.tree_refresh_pending = true;
            return;
        }
        self.next_tree_refresh_id = self.next_tree_refresh_id.wrapping_add(1);
        let request_id = self.next_tree_refresh_id;
        self.tree_refresh_in_flight = Some((generation, request_id));
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            match tokio::time::timeout(
                TREE_REFRESH_TIMEOUT,
                client.session_tree(SessionTreeParams { session_id }),
            )
            .await
            {
                Ok(Ok(result)) => {
                    let _ = updates.send(RpcUpdate::Tree {
                        session_id,
                        generation,
                        request_id,
                        tree: Box::new(result.tree),
                    });
                }
                Ok(Err(error)) => {
                    let _ = updates.send(RpcUpdate::TreeFailed {
                        session_id,
                        generation,
                        request_id,
                        error: error.to_string(),
                    });
                }
                Err(_) => {
                    let _ = updates.send(RpcUpdate::TreeFailed {
                        session_id,
                        generation,
                        request_id,
                        error: "tree refresh timed out".into(),
                    });
                }
            }
        });
    }

    pub(in crate::ui) fn handle_rpc_update(&mut self, update: RpcUpdate) {
        self.apply_rpc_update(update);
        // A provider connect the onboarding panel handed over to may just
        // have finished and closed its panel.
        self.resume_onboarding();
    }

    fn apply_rpc_update(&mut self, update: RpcUpdate) {
        match update {
            RpcUpdate::Status(status) => {
                self.session_errors.record(&status);
                self.status = status;
            }
            RpcUpdate::Failure { status, detail } => {
                self.session_errors.record(&status);
                if let (Some(detail), Some(session_id)) = (detail, self.selected) {
                    self.store.push_error_details(session_id, &status, &detail);
                }
                self.status = status;
            }
            RpcUpdate::Notice(status) => self.status = status,
            RpcUpdate::Reconnected { .. } => {
                // The event loop adopts the client, which needs async work.
            }
            RpcUpdate::PublishDraft {
                session_id,
                generation,
            } => self.finish_draft_publish(session_id, generation),
            RpcUpdate::RetrySubscription { session_id } => {
                if self.selected == Some(session_id)
                    && !self.subscriptions.contains(session_id)
                    && !self.reconnecting
                {
                    self.resubscribe_from_scratch(session_id);
                }
            }
            RpcUpdate::RuntimeResynced { result } => {
                let again = self.runtime_resync.landed();
                match result {
                    Ok(snapshot) => {
                        if self.runtime.install_current((*snapshot).clone()) {
                            self.install_runtime_projection(*snapshot);
                        }
                    }
                    Err(error) => self.status = format!("runtime refresh failed: {error}"),
                }
                if again {
                    self.request_runtime_resync();
                }
            }
            RpcUpdate::FullContentLoaded {
                session_id,
                call_id,
                block,
                state,
            } => {
                if let Some(session) = self.store.sessions.get_mut(&session_id) {
                    session.finish_full_content(call_id, block, state);
                }
            }
            RpcUpdate::InputNotStarted {
                client_run_id,
                notice,
            } => {
                self.pending_fallback_resets.remove(&client_run_id);
                if let Some(notice) = notice {
                    self.status = notice;
                }
            }
            RpcUpdate::RunStartFinished {
                session_id,
                client_run_id,
                draft_generation,
                reset_fallback,
                input,
                result,
            } => {
                match result {
                    Ok(()) => self.acknowledge_fallback_reset(
                        session_id,
                        &client_run_id,
                        Some(draft_generation),
                        false,
                        None,
                    ),
                    Err(error) => {
                        // A failed response is not admission proof. Restore the
                        // submitted prompt while retaining the draft selection.
                        if self.selected == Some(session_id)
                            && self.draft_generation == draft_generation
                            && (!reset_fallback
                                || self.pending_fallback_resets.contains_key(&client_run_id))
                        {
                            self.restore_composer(vec![input]);
                            self.session_errors.record(&error);
                            self.status =
                                format!("run failed to start ({error}); restored to the composer");
                        } else {
                            self.store.park_voided_input(session_id, input);
                        }
                    }
                }
            }
            RpcUpdate::GoalFinished { session_id, result } => {
                self.finish_goal_command(session_id, *result)
            }
            RpcUpdate::SessionOwnershipClassified {
                session_id,
                generation,
                outcome,
            } => self.apply_ownership_classification(session_id, generation, outcome),
            RpcUpdate::SubscriptionFinished {
                session_id,
                attempt,
                outcome,
            } => self.finish_subscription(session_id, attempt, outcome),
            RpcUpdate::BackgroundCalls {
                session_id,
                mark,
                calls,
            } => {
                self.store
                    .install_background_listing(session_id, mark, calls);
            }
            RpcUpdate::SteerFailed {
                session_id,
                input,
                error,
            } => {
                if self.selected == Some(session_id) {
                    self.restore_composer(vec![input]);
                    self.status = format!("message not sent ({error}); restored to the composer");
                } else {
                    self.store.park_voided_input(session_id, input);
                    self.status =
                        format!("a message failed to send ({error}); kept for its session");
                }
            }
            RpcUpdate::AttachmentUploaded {
                session_id,
                name,
                result,
            } => self.finish_attachment(session_id, name, result),
            RpcUpdate::SteerRecalled { session_id, input } => {
                if self.selected == Some(session_id) {
                    self.restore_composer(vec![input]);
                    self.status = "recalled message restored to the composer".into();
                } else {
                    // The composer belongs to another session right now;
                    // park the text so it is restored when that session is
                    // viewed rather than leaking across sessions.
                    self.store.park_voided_input(session_id, input);
                    self.status = "recalled message kept for its session".into();
                }
            }
            RpcUpdate::Reverted { session_id, text } => {
                // The transcript rebuild rides the `rewound` notification;
                // here only the composer's share and the tree's
                // branch-derived rows (title/status/usage) need attention.
                if self.selected == Some(session_id) {
                    self.restore_composer(vec![crate::state::ComposerInput::text(text)]);
                    self.status = "reverted; the message text is back in the composer".into();
                } else {
                    self.store
                        .park_voided_input(session_id, crate::state::ComposerInput::text(text));
                    self.status = "reverted; the message text is kept for its session".into();
                }
                self.refresh_tree_background();
            }
            RpcUpdate::SessionDeleteSubagents {
                session_id,
                subagents,
            } => self.note_session_delete_subagents(session_id, subagents),
            RpcUpdate::SessionRenamed { change, result } => {
                self.finish_session_rename(change, *result);
            }
            RpcUpdate::SessionDeleted { title, outcome } => {
                self.finish_session_delete(title, outcome);
            }
            RpcUpdate::Forked { forked } => {
                self.status = "forked the session; switching to it".into();
                self.refresh_tree_background();
                // This update is emitted only after the fork RPC confirms success.
                self.ownership_classifications.remove(&forked);
                self.owned_sessions.insert(forked);
                self.read_only_sessions.remove(&forked);
                self.reroot_tree(forked);
                self.subscribe_session(forked, SubscribeIntent::Follow);
            }
            RpcUpdate::Tree {
                session_id,
                generation,
                request_id,
                tree,
            } if self.tree_refresh_in_flight == Some((generation, request_id)) => {
                self.tree_refresh_in_flight = None;
                if self.tree_root == Some(session_id) && self.selection_generation == generation {
                    let mut tree = *tree;
                    self.patch_tree_titles(&mut tree);
                    cookie_agent_client_state::rows::order_tree(&mut tree);
                    self.subscribe_tree_sessions(&tree);
                    self.tree = Some(tree);
                    self.clamp_tree_view();
                }
                self.refresh_pending_tree();
            }
            RpcUpdate::TreeFailed {
                session_id,
                generation,
                request_id,
                error,
            } if self.tree_refresh_in_flight == Some((generation, request_id)) => {
                self.tree_refresh_in_flight = None;
                if self.tree_root == Some(session_id) && self.selection_generation == generation {
                    self.status = error;
                }
                self.refresh_pending_tree();
            }
            RpcUpdate::Tree { .. } => {}
            RpcUpdate::TreeFailed { .. } => {}
            RpcUpdate::ProviderMutationFinished { outcome } => {
                let (ProviderMutationOutcome::Failed { provider_id, .. }
                | ProviderMutationOutcome::Connected { provider_id, .. }
                | ProviderMutationOutcome::Disconnected { provider_id, .. }) = &outcome;
                self.connect_tasks.remove(provider_id);
                self.apply_provider_mutation_outcome(outcome);
            }
            RpcUpdate::ApprovalResponse {
                request_id,
                approval_id,
                result,
            } => {
                self.finish_approval_submission(request_id, approval_id, result);
            }
            RpcUpdate::ApprovalList {
                root_session_id,
                generation,
                request_id,
                result,
            } if self.approval_refresh_in_flight
                == Some((root_session_id, generation, request_id)) =>
            {
                self.approval_refresh_in_flight = None;
                let current_root = self.tree_root.or(self.selected);
                if current_root != Some(root_session_id) || self.selection_generation != generation
                {
                    return;
                }
                match result {
                    Ok(result) => {
                        self.apply_approval_list(root_session_id, result);
                        self.reconcile_pending_approval();
                    }
                    Err(error) => self.status = format!("approval list refresh failed: {error}"),
                }
            }
            RpcUpdate::ApprovalList { .. } => {}
            RpcUpdate::AgentOptionSet {
                session_id,
                label,
                result,
            } => self.finish_agent_option_set(session_id, label, result),
            RpcUpdate::QuestionAnswered {
                session_id,
                elicitation_id,
                result,
            } => self.finish_question_answer(session_id, elicitation_id, result),
            RpcUpdate::PermissionModeMutationFinished {
                session_id,
                generation,
                result,
            } => {
                if self.permission_mode_generations.get(&session_id).copied() != Some(generation) {
                    return;
                }
                if let Err(error) = result {
                    self.permission_modes.remove(&session_id);
                    self.status = format!("permission mode update failed: {error}");
                    self.refresh_permission_mode_for_session(session_id);
                }
            }
            RpcUpdate::PermissionModeLoaded {
                session_id,
                generation,
                result,
            } => {
                if self.permission_mode_generations.get(&session_id).copied() != Some(generation) {
                    return;
                }
                match result {
                    Ok(Some(mode)) => {
                        self.permission_modes.insert(session_id, mode);
                    }
                    Ok(None) => {}
                    Err(error) => self.status = format!("permission mode load failed: {error}"),
                }
            }
            RpcUpdate::McpRefreshed { result } => {
                let again = self.mcp_panel.refresh.landed();
                match result {
                    Ok(servers) => {
                        self.mcp_panel.install(servers.servers);
                    }
                    Err(error) => self.status = format!("MCP refresh failed: {error}"),
                }
                if again {
                    self.poll_mcp();
                }
            }
            RpcUpdate::PluginsRefreshed { result } => self.install_plugins(result),
            RpcUpdate::ConfigLoaded { result } => self.install_config(result),
            RpcUpdate::OnboardingStatus { read, status } => {
                self.install_onboarding_read(read, status)
            }
            RpcUpdate::OnboardingPreview { read, result } => {
                self.install_onboarding_preview(read, result)
            }
            RpcUpdate::OnboardingRun { result } => self.install_onboarding_run(result),
            RpcUpdate::ConfigApplied {
                path,
                edit,
                outcome,
            } => self.install_config_apply(path, edit, outcome),
            RpcUpdate::ConfigReloaded { status } => self.install_config_reload(status),
            RpcUpdate::MemoryListed { result } => self.install_memory_listing(result),
            RpcUpdate::MemoryRead {
                url,
                purpose,
                result,
            } => self.install_memory_read(url, purpose, result),
            RpcUpdate::MemoryWritten { request, outcome } => {
                self.install_memory_write(request, outcome);
            }
            RpcUpdate::MemoryDeleted { result } => self.install_memory_delete(result),
            RpcUpdate::CronsListed { result } => self.install_cron_listing(result),
            RpcUpdate::ConfigStatusFetched(status) => self.install_config_status(status),
            RpcUpdate::RestartAnswered(outcome) => self.finish_restart_request(outcome),
            RpcUpdate::CronSaved { request, result } => {
                self.install_cron_save(*request, *result);
            }
            RpcUpdate::CronChanged { result } => self.install_cron_change(result),
            RpcUpdate::MachinesRefreshed { result } => self.install_machine_listing(result),
            RpcUpdate::MachineChanged { result } => self.install_machine_change(result),
            RpcUpdate::RuntimesRefreshed { result } => match result {
                Ok(listing) => {
                    self.install_runtime_homes(
                        cookie_agent_client_state::paths::RuntimeHomes::from_listing(&listing),
                    );
                    self.runtime_panel.install(listing);
                }
                Err(error) => self.status = format!("runtime refresh failed: {error}"),
            },
            RpcUpdate::RuntimeMcpRefreshed { result } => {
                let again = self.runtime_panel.refresh.landed();
                match result {
                    Ok(listing) => self.runtime_panel.install_mcp_servers(listing.servers),
                    Err(error) => self.status = format!("MCP refresh failed: {error}"),
                }
                if again {
                    self.poll_runtime_listing();
                }
            }
            RpcUpdate::RuntimeHomesRefreshed { homes } => {
                let again = self.runtime_homes_refresh.landed();
                // A failure keeps the homes known so far.
                if let Some(homes) = homes {
                    self.install_runtime_homes(homes);
                }
                if again {
                    self.poll_runtime_homes();
                }
            }
            RpcUpdate::ProjectsRefreshed { projects } => {
                let again = self.projects_refresh.landed();
                if let Some(projects) = projects {
                    self.install_projects(projects);
                }
                if again {
                    self.poll_projects();
                }
            }
            RpcUpdate::SessionProjectSet { label, result } => {
                self.finish_session_project_set(label, *result);
            }
            RpcUpdate::ProjectCreated { result } => self.finish_project_create(*result),
            RpcUpdate::RuntimeAttachmentChanged { result } => {
                self.status = match result {
                    Ok(message) | Err(message) => message,
                };
                self.poll_runtimes();
            }
            RpcUpdate::DraftRuntimeOpened { id, result } => {
                self.install_draft_runtime(id, result);
                self.poll_runtimes();
            }
            RpcUpdate::McpMutation { result } => match *result {
                Ok(_) => {
                    self.status = "MCP server state updated.".into();
                    self.poll_mcp();
                }
                Err(error) => self.status = format!("MCP update failed: {error}"),
            },
            RpcUpdate::McpAuthBegan { result } => match result {
                Ok(result) => {
                    self.mcp_panel.auth = Some(McpAuthView {
                        server: result.server,
                        authorization_url: result.authorization_url,
                    });
                    self.status = "waiting for MCP OAuth authorization".into();
                }
                Err(error) => {
                    self.mcp_panel.auth_listener = None;
                    self.status = format!("MCP authentication failed: {error}");
                }
            },
            RpcUpdate::McpAuthCompleted { server, result } => {
                match result {
                    Ok(()) => {
                        if self
                            .mcp_panel
                            .auth
                            .as_ref()
                            .is_some_and(|auth| auth.server == server)
                        {
                            self.mcp_panel.clear_auth();
                        }
                        self.status = format!("MCP server `{server}` authorized; connecting.");
                    }
                    Err(error) => self.status = format!("MCP authentication failed: {error}"),
                }
                self.poll_mcp();
            }
            RpcUpdate::McpAuthCancelled { result } => match result {
                Ok(server) => {
                    if self
                        .mcp_panel
                        .auth
                        .as_ref()
                        .is_some_and(|auth| auth.server == server)
                    {
                        self.mcp_panel.clear_auth();
                    }
                    self.status = "MCP authentication cancelled.".into();
                    self.poll_mcp();
                }
                Err(error) => {
                    self.mcp_panel.clear_auth();
                    self.status = format!("MCP authentication cancel failed: {error}");
                    self.poll_mcp();
                }
            },
            RpcUpdate::PermissionPreviewLoaded { generation, result } => {
                if !self.permission_panel.draft
                    || generation != self.draft_permissions.preview_generation
                {
                    return;
                }
                match result {
                    Ok(permissions) => self.permission_panel.install(SessionPermissionGetResult {
                        permissions,
                        current_mode: None,
                    }),
                    Err(error) => {
                        self.permission_panel.notice =
                            Some(format!("Could not preview permissions: {error}"));
                    }
                }
            }
            RpcUpdate::PermissionsLoaded { session_id, result } => {
                if self.selected != Some(session_id) || self.permission_panel.draft {
                    return;
                }
                match result {
                    Ok(result) => {
                        self.permission_panel.install(result);
                        self.load_skills_for_session(session_id);
                    }
                    Err(error) => self.status = format!("permission update failed: {error}"),
                }
            }
            RpcUpdate::SkillsLoaded {
                session_id,
                read,
                result,
            } => {
                // An older read's answer may predate a permission change
                // the newer one saw.
                if self.selected != Some(session_id) || !self.skills_reads.is_current(read) {
                    return;
                }
                match result {
                    Ok(result) => self.skills = result.skills,
                    Err(error) => self.status = format!("skill discovery failed: {error}"),
                }
            }
            RpcUpdate::UsageLoaded {
                generation,
                session_id,
                session,
                tree,
            } => {
                if generation != self.usage_load_generation {
                    return;
                }
                self.usage_panel.loading = false;
                if self.selected == session_id {
                    match session {
                        Ok(result) => self.usage_panel.session = result,
                        Err(error) => self.status = format!("session usage failed: {error}"),
                    }
                    match tree {
                        Ok(result) => self.usage_panel.tree = result,
                        Err(error)
                            if error.is(
                                &cookie_agent_protocol::errors::TREE_USAGE_DELEGATION_CORRUPT,
                            ) =>
                        {
                            self.usage_panel.tree_corrupt = true;
                        }
                        Err(error) => self.status = format!("session tree usage failed: {error}"),
                    }
                }
            }
            RpcUpdate::SessionCostLoaded {
                session_id,
                request_id,
                result,
            } => {
                let Some(refresh) = self.cost_refreshes.get_mut(&session_id) else {
                    return;
                };
                if !refresh.in_flight || refresh.request_id != request_id {
                    return;
                }
                refresh.in_flight = false;
                let dirty = std::mem::take(&mut refresh.dirty);
                if let Ok(result) = result
                    && let Some(state) = self.store.sessions.get_mut(&session_id)
                {
                    state.estimated_cost_usd = result.usage.estimated_cost_usd;
                }
                if dirty {
                    self.schedule_session_cost_refresh(session_id);
                }
            }
            RpcUpdate::SessionCostDebounceElapsed {
                session_id,
                generation,
            } => {
                let launch = self
                    .cost_refreshes
                    .get_mut(&session_id)
                    .is_some_and(|refresh| {
                        if !refresh.scheduled || refresh.debounce_generation != generation {
                            return false;
                        }
                        refresh.scheduled = false;
                        true
                    });
                if launch {
                    self.start_session_cost_refresh(session_id);
                }
            }
        }
    }

    /// Apply event-sequence staleness rules to a fresh tree response so title
    /// and run-status event patches cannot be undone by an older RPC result.
    pub(in crate::ui) fn patch_tree_titles(&mut self, tree: &mut SessionTree) {
        let mut known_titles = HashMap::new();
        collect_known_titles(
            self.tree.as_ref(),
            &self.sessions,
            &self.title_sequences,
            &mut known_titles,
        );
        patch_tree_node_titles(tree, &self.title_sequences, &known_titles);
        let mut known_statuses = HashMap::new();
        collect_known_statuses(self.tree.as_ref(), &self.sessions, &mut known_statuses);
        patch_tree_node_statuses(tree, &known_statuses);
    }

    pub(in crate::ui) fn apply_provider_mutation_outcome(
        &mut self,
        outcome: ProviderMutationOutcome,
    ) {
        match outcome {
            ProviderMutationOutcome::Failed {
                provider_id,
                action,
                error,
            } => {
                self.provider_operations.insert(
                    provider_id.clone(),
                    ProviderOperation::Error {
                        action,
                        message: error.clone(),
                    },
                );
                if matches!(action, ProviderAction::Connect | ProviderAction::Reconnect)
                    && let Some(form) = &mut self.provider_form
                    && form.provider.id == provider_id
                {
                    form.error = Some(error.clone());
                    self.modal = Modal::ConnectError;
                    self.status = format!("Provider {} failed: {error}", action_name(action));
                } else {
                    self.status = format!(
                        "Provider {} failed: {error}. Enter the row to retry.",
                        action_name(action)
                    );
                }
            }
            ProviderMutationOutcome::Connected {
                provider_id,
                baseline,
                runtime,
            } => {
                self.provider_operations.remove(&provider_id);
                self.install_runtime_response(baseline.as_ref(), *runtime);
                self.clear_connect_secrets();
                self.modal = Modal::None;
                self.status = if self.runtime.is_empty() {
                    EMPTY_RUNTIME_GUIDANCE.into()
                } else {
                    format!("Connected provider {provider_id}.")
                };
            }
            ProviderMutationOutcome::Disconnected {
                provider_id,
                baseline,
                runtime,
            } => {
                self.provider_operations.remove(&provider_id);
                self.install_runtime_response(baseline.as_ref(), *runtime);
                self.status = if self.runtime.is_empty() {
                    EMPTY_RUNTIME_GUIDANCE.into()
                } else {
                    format!("Disconnected provider {provider_id}.")
                };
            }
        }
    }

    pub(super) fn refresh_pending_tree(&mut self) {
        if std::mem::take(&mut self.tree_refresh_pending) {
            self.refresh_tree_background();
        }
    }

    /// Subscribe the tree's live nodes (running or idle) up front, so their
    /// status, titles and warnings stay current. A finished node's history
    /// is replayed only when it is opened ([`Self::watch_session`]): a
    /// resumed tree can hold many long finished logs, and replaying them all
    /// on attach is what floods the daemon. A finished node that is woken
    /// again reports `Running` on the next tree refresh and is picked up then.
    /// Nodes already subscribed on this connection are left alone, and a
    /// node whose ownership is being classified is subscribed by that
    /// classification.
    pub(super) fn subscribe_tree_sessions(&mut self, tree: &SessionTree) {
        let mut sessions = Vec::new();
        collect_live_tree_sessions(tree, &mut sessions);
        for session_id in sessions {
            if !self.ownership_classifications.contains_key(&session_id) {
                self.subscribe_session(session_id, SubscribeIntent::Follow);
            }
        }
    }

    pub(in crate::ui) async fn refresh_tree(&mut self) {
        if let Some(root) = self.tree_root {
            let generation = self.selection_generation;
            match tokio::time::timeout(
                TREE_REFRESH_TIMEOUT,
                self.client
                    .session_tree(SessionTreeParams { session_id: root }),
            )
            .await
            {
                Ok(Ok(result))
                    if self.tree_root == Some(root) && self.selection_generation == generation =>
                {
                    let mut tree = result.tree;
                    self.patch_tree_titles(&mut tree);
                    cookie_agent_client_state::rows::order_tree(&mut tree);
                    self.subscribe_tree_sessions(&tree);
                    self.tree = Some(tree);
                    self.clamp_tree_view();
                }
                Ok(Err(error)) => self.status = error.to_string(),
                Err(_) => self.status = "tree refresh timed out".into(),
                Ok(Ok(_)) => {}
            }
        }
    }

    pub(in crate::ui) async fn handle_delivery(&mut self, delivery: ClientDelivery) {
        if let ClientDelivery::Disconnected { error } = &delivery {
            self.status = format!("connection failed: {error}");
            self.reset_subscriptions();
        }
        if let ClientDelivery::SessionsChanged(change) = &delivery {
            self.apply_sessions_change((**change).clone()).await;
            return;
        }
        if matches!(delivery, ClientDelivery::McpChanged(_)) {
            // An open MCP panel refreshes now instead of at its next poll.
            self.poll_mcp();
        }
        if matches!(
            delivery,
            ClientDelivery::RuntimesChanged(_) | ClientDelivery::MachinesChanged(_)
        ) {
            self.poll_runtimes();
            if self.modal != Modal::Runtimes {
                // An open panel's refresh brings the homes as well.
                self.poll_runtime_homes();
            }
        }
        // Renaming or merging a machine moves its memories too.
        if matches!(
            delivery,
            ClientDelivery::MemoryChanged(_) | ClientDelivery::MachinesChanged(_)
        ) {
            self.poll_memory();
        }
        if matches!(delivery, ClientDelivery::CronsChanged(_)) {
            self.poll_crons();
        }
        if let ClientDelivery::OnboardingChanged(status) = &delivery {
            self.note_onboarding_changed(status.clone());
        }
        if matches!(delivery, ClientDelivery::PluginsChanged(_)) {
            self.poll_plugins();
        }
        if matches!(delivery, ClientDelivery::ProjectsChanged(_)) {
            self.poll_projects();
        }
        if let ClientDelivery::DaemonRestarting(restarting) = &delivery {
            self.note_daemon_restarting(restarting.clone());
            return;
        }
        if let ClientDelivery::RuntimeChanged(changed) = &delivery {
            // A new connection, or a configuration change: the banner's
            // status may have changed.
            if changed.reasons.iter().any(|reason| {
                matches!(
                    reason,
                    cookie_agent_protocol::RuntimeChangeReason::Startup
                        | cookie_agent_protocol::RuntimeChangeReason::ConfigReloaded
                        | cookie_agent_protocol::RuntimeChangeReason::Resync
                )
            }) {
                self.poll_config_status();
            }
            let duplicate = self.runtime.revision() == Some(&changed.snapshot.revision);
            if !self.install_runtime_notification((**changed).clone()) && !duplicate {
                // Outside the predecessor chain: this client missed a
                // notification (the daemon drops them for a lagging
                // connection) or got an older one. Every later notification
                // would be rejected too, so fetch the current snapshot.
                self.request_runtime_resync();
            }
            return;
        }
        if let ClientDelivery::RecoveryFailed { session_id, error } = &delivery {
            self.note_recovery_failed(*session_id);
            self.schedule_subscription_retry(*session_id);
            self.status = match session_id {
                Some(session_id) => format!("recovery for {session_id} failed: {error}"),
                None => format!("recovery failed: {error}"),
            };
            return;
        }
        if let ClientDelivery::PluginEvent(event) = &delivery {
            if Some(event.session_id) == self.selected {
                self.status = cookie_agent_client_state::plugins::plugin_event_status(event);
            }
            return;
        }
        let event = match &delivery {
            ClientDelivery::Live { message, .. } => match message.as_ref() {
                cookie_agent_protocol::EventSubscriptionMessage::Event { event } => {
                    Some(event.as_ref())
                }
                cookie_agent_protocol::EventSubscriptionMessage::Transient { .. }
                | cookie_agent_protocol::EventSubscriptionMessage::Gap { .. }
                | cookie_agent_protocol::EventSubscriptionMessage::Rewound { .. } => None,
            },
            ClientDelivery::ReplayEvent { event, .. } => Some(event.as_ref()),
            _ => None,
        };
        let reset_admission = event.and_then(|event| match &event.payload {
            EventPayload::RunStarted { client_run_id, .. } => Some((
                event.session_id,
                client_run_id.clone(),
                match &delivery {
                    ClientDelivery::ReplayEvent { generation, .. } => Some(*generation),
                    _ => None,
                },
            )),
            _ => None,
        });
        let reset_replay_end = match &delivery {
            ClientDelivery::ReplayEnd {
                session_id,
                generation,
                ..
            } => Some((*session_id, *generation)),
            _ => None,
        };
        let linked = event
            .is_some_and(|event| matches!(&event.payload, EventPayload::ToolCallLinked { .. }));
        // Another client picked the viewed session's model: this view's own
        // unsent pick yields, so the draft follows the latest pick.
        let foreign_model_pick = cookie_agent_client_state::model_draft::is_foreign_model_pick(
            &delivery,
            self.selected,
            self.client.origin().as_ref(),
        );
        if foreign_model_pick {
            self.set_draft_reset_intent(false);
            self.draft_publish.yield_to_foreign_pick();
        }
        // A run that ended in a session outside the view no longer needs its
        // live tail: releasing it lets the daemon evict the session.
        let run_ended = event.and_then(|event| {
            (matches!(&delivery, ClientDelivery::Live { .. })
                && matches!(&event.payload, EventPayload::RunFinished { .. }))
            .then_some(event.session_id)
        });
        let terminalized = event.is_some_and(|event| {
            matches!(
                &event.payload,
                EventPayload::RunFinished { .. }
                    | EventPayload::DelegationFinished { .. }
                    | EventPayload::DelegateChildTerminated { .. }
            )
        });
        // A parent's delegation lifecycle is how an unsubscribed child's
        // status change reaches the tree: re-read it so a woken child is
        // shown running and gets subscribed.
        let delegation_changed = event.is_some_and(|event| {
            matches!(
                &event.payload,
                EventPayload::DelegationProgressed { .. } | EventPayload::DelegationFinished { .. }
            )
        });
        let title_change = event.and_then(title_change_from_event);
        let goal_completed = event.and_then(|event| {
            let EventPayload::GoalLifecycleChanged {
                goal_id,
                status: cookie_agent_protocol::GoalStatus::Completed,
                revision,
                ..
            } = &event.payload
            else {
                return None;
            };
            (matches!(&delivery, ClientDelivery::Live { .. })
                && self
                    .store
                    .sessions
                    .get(&event.session_id)
                    .is_none_or(|state| state.last_seq < event.seq))
            .then_some((event.session_id, *goal_id, *revision))
        });
        let status_change = event.and_then(status_change_from_event);
        let refresh_skills = event.and_then(|event| {
            (Some(event.session_id) == self.selected
                && matches!(
                    event.payload,
                    EventPayload::RunStarted { .. }
                        | EventPayload::SessionPermissionOverlaySet { .. }
                ))
            .then(|| event.clone())
        });
        let refresh_cost = event.and_then(|event| {
            matches!(
                event.payload,
                EventPayload::ModelUsageRecorded { .. }
                    | EventPayload::InternalAgentUsageRecorded { .. }
            )
            .then_some(event.session_id)
        });
        if let Some((session_id, title, seq)) = title_change {
            self.apply_title_patch(session_id, title, seq);
        }
        if let Some((session_id, status, seq)) = status_change {
            self.apply_status_patch(session_id, status, seq);
        }
        let replay_finished = matches!(
            &delivery,
            ClientDelivery::ReplayEnd { session_id, .. } if Some(*session_id) == self.selected
        );
        let replay_ended_session = match &delivery {
            ClientDelivery::ReplayEnd { session_id, .. } => Some(*session_id),
            _ => None,
        };
        let revert_rebuild = matches!(
            &delivery,
            ClientDelivery::Live { message, .. } if matches!(
                message.as_ref(),
                cookie_agent_protocol::EventSubscriptionMessage::Rewound { session_id, .. }
                    if Some(*session_id) == self.selected
            )
        );
        // A descendant's warning or error never mutates the viewed session's
        // projection (a replay could not reproduce that): the transcript
        // splices it in at render time by its durable time instead.
        let outcome = self.store.apply_delivery(delivery);
        if foreign_model_pick {
            self.sync_session_model_draft();
            if let Some(draft) = &self.draft {
                self.status = format!(
                    "Model: {} — picked by another client",
                    super::pickers::draft_title(draft)
                );
            }
        }
        if let Some(session_id) = run_ended
            && !self.is_recently_viewed(session_id)
            && self.is_settled(session_id)
        {
            self.release_subscription(session_id);
        }
        if let Some(session_id) = replay_ended_session {
            self.note_replay_end(session_id);
        }
        if (revert_rebuild || replay_finished)
            && matches!(self.selection, Some(TextSelection::Conversation { .. }))
        {
            // The viewed transcript was replaced — a revert rebuilds the
            // visible branch, a recovery replay swaps in a whole new
            // projection — so the conversation leg's content coordinates
            // address lines that no longer exist and would paint and copy
            // the wrong text. The composer leg survives: the draft buffer
            // is untouched by either rebuild (its own restoring mutations
            // retire it separately).
            self.selection = None;
        }
        if matches!(outcome, DeliveryOutcome::Applied) {
            if let Some((session_id, client_run_id, replay_generation)) = reset_admission {
                self.acknowledge_fallback_reset(
                    session_id,
                    &client_run_id,
                    None,
                    true,
                    replay_generation,
                );
            }
            if let Some((session_id, generation)) = reset_replay_end {
                self.finish_fallback_reset_replay(session_id, generation);
            }
            self.sync_session_model_draft();
            if let Some((session_id, goal_id, revision)) = goal_completed {
                self.notify_goal_completed(session_id, goal_id, revision);
            }
            // The pending lane itself is a pure event reduction (admitted,
            // promoted, recalled, replayed identically); only the composer's
            // share needs a hook here: run-end events void pending inputs
            // without per-entry events, so drain whatever the viewed session
            // is owed back into the composer.
            self.restore_voided_inputs();
            if let Some(event) = &refresh_skills {
                self.refresh_skills_for_event(event);
            }
            if let Some(session_id) = refresh_cost {
                self.refresh_session_cost(session_id);
            }
        }
        match outcome {
            DeliveryOutcome::Applied => {}
            DeliveryOutcome::Gap { cursor, .. } => {
                self.status = format!("event gap after sequence {cursor}; replaying");
            }
            DeliveryOutcome::ReplayFailed { session_id } => {
                self.status = "incomplete replay; retrying recovery".into();
                self.recover_session(session_id);
            }
        }
        self.reconcile_pending_approval();
        if linked || terminalized || delegation_changed || replay_finished {
            self.refresh_tree_background();
        }
    }

    /// Apply a strictly-newer title event patch immediately: the Agents
    /// panel rows and session list update without waiting for a tree
    /// refresh, and older tree/list responses cannot undo it.
    pub(in crate::ui) fn apply_title_patch(
        &mut self,
        session_id: SessionId,
        title: Option<SessionTitle>,
        seq: u64,
    ) {
        let known = self.title_sequences.entry(session_id).or_insert(0);
        if seq < *known {
            return;
        }
        *known = seq;
        let mut session_changed = false;
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.session_id == session_id)
        {
            session.title = title.clone();
            session.title_updated_seq = seq;
            session_changed = true;
        }
        if let Some(tree) = &mut self.tree
            && let Some(node) = find_node_mut(tree, session_id)
            && seq >= node.session.title_updated_seq
        {
            node.session.title = title;
            node.session.title_updated_seq = seq;
        }
        if session_changed {
            self.note_sessions_changed();
        }
    }

    /// Apply a run lifecycle status immediately to both panel metadata
    /// sources without waiting for a session-list or tree RPC response.
    pub(in crate::ui) fn apply_status_patch(
        &mut self,
        session_id: SessionId,
        status: SessionStatus,
        seq: u64,
    ) {
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.session_id == session_id)
            && seq >= session.last_event_seq
        {
            session.status = status;
            session.last_event_seq = seq;
        }
        if let Some(tree) = &mut self.tree
            && let Some(node) = find_node_mut(tree, session_id)
            && seq >= node.session.last_event_seq
        {
            node.session.status = status;
            node.session.last_event_seq = seq;
            // A subagent that starts or ends a run moves in the order.
            cookie_agent_client_state::rows::order_tree(tree);
        }
    }

    /// Retry replays past their deadline and drop a stale pending approval.
    /// Returns whether any visible state changed.
    pub(in crate::ui) fn recover_timed_out_replays(&mut self) -> bool {
        let timed_out = self.store.abandon_timed_out_replays();
        for &session_id in &timed_out {
            self.status = "replay timed out; retrying recovery".into();
            self.recover_session(session_id);
        }
        let reconciled = self.reconcile_pending_approval();
        !timed_out.is_empty() || reconciled
    }

    /// The drawn state that changes with wall-clock time alone: the approval
    /// on top (hidden once it expires) and the pending strip's coarse age
    /// label. The replay watchdog redraws only when this differs from what
    /// was last drawn.
    pub(in crate::ui) fn wall_clock_view(&self) -> WallClockView {
        let current = self.current_approval();
        let approval = current.map(|approval| (approval.approval_id, approval.request_revision));
        let approval_seconds_left = current.and_then(super::approvals::seconds_left);
        let queue_age = self.selected_queue_entries().first().map(|oldest| {
            queue_age_label(
                jiff::Timestamp::now()
                    .duration_since(oldest.accepted_at)
                    .as_secs()
                    .max(0),
            )
        });
        WallClockView {
            approval,
            approval_seconds_left,
            queue_age,
        }
    }

    pub(in crate::ui) fn poll_runtimes(&mut self) {
        self.poll_machines();
        self.poll_runtime_listing();
    }

    /// Fetches `runtimes.list`, then `mcp.server.list`, for the open
    /// `/runtime` panel; the read is on its way until the second answers.
    fn poll_runtime_listing(&mut self) {
        if self.modal != Modal::Runtimes || !self.runtime_panel.refresh.start() {
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_runtimes()
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::RuntimesRefreshed { result });
            // The MCP servers each runtime hosts, for the rows' details.
            let result = client
                .list_mcp_servers()
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::RuntimeMcpRefreshed { result });
        });
    }

    pub(in crate::ui) fn poll_mcp(&mut self) {
        if self.modal != Modal::Mcp || !self.mcp_panel.refresh.start() {
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_mcp_servers()
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpRefreshed { result });
        });
    }

    /// Preview the new-session draft's effective permissions: its agent's
    /// rules with the draft's overlay rules applied.
    pub(super) fn load_permission_preview(&mut self) {
        let Some(selection) = self.new_session_draft.clone() else {
            return;
        };
        self.draft_permissions.preview_generation += 1;
        let generation = self.draft_permissions.preview_generation;
        let rules = self.draft_permissions.rules.clone();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .preview_session_permissions(SessionPermissionPreviewParams { selection, rules })
                .await
                .map(|result| result.permissions)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PermissionPreviewLoaded { generation, result });
        });
    }

    pub(super) fn load_permissions(&mut self) {
        let Some(session_id) = self.selected else {
            self.status = "select a session before editing permissions".into();
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .get_session_permissions(SessionPermissionGetParams { session_id })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PermissionsLoaded { session_id, result });
        });
    }

    pub(super) fn refresh_permission_mode_for_session(&mut self, session_id: SessionId) {
        let session_id = self.permission_mode_root(session_id);
        let generation = self.next_permission_mode_generation(session_id);
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .get_session_permissions(SessionPermissionGetParams { session_id })
                .await
                .map(|result| result.current_mode)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PermissionModeLoaded {
                session_id,
                generation,
                result,
            });
        });
    }

    pub(super) fn load_skills_for_session(&mut self, session_id: SessionId) {
        #[cfg(test)]
        self.skill_refresh_requests.push(session_id);
        self.skills.clear();
        let read = self.skills_reads.begin();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_skills(cookie_agent_protocol::SkillsListParams {
                    session_id: Some(session_id),
                })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::SkillsLoaded {
                session_id,
                read,
                result,
            });
        });
    }

    pub(super) fn refresh_skills_for_event(&mut self, event: &StoredEvent) {
        if Some(event.session_id) == self.selected
            && matches!(
                event.payload,
                EventPayload::RunStarted { .. } | EventPayload::SessionPermissionOverlaySet { .. }
            )
        {
            self.load_skills_for_session(event.session_id);
        }
    }

    pub(super) fn load_usage(&mut self) {
        let session_id = self.selected;
        self.usage_load_generation = self
            .usage_load_generation
            .checked_add(1)
            .expect("usage load generation exhausted");
        let generation = self.usage_load_generation;
        self.usage_panel.begin_load();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let session = async {
                match session_id {
                    Some(session_id) => client
                        .session_usage(SessionUsageParams { session_id })
                        .await
                        .map(Some)
                        .map_err(|error| error.to_string()),
                    None => Ok(None),
                }
            };
            let tree = async {
                match session_id {
                    Some(session_id) => client
                        .session_tree_usage(SessionUsageParams { session_id })
                        .await
                        .map(Some),
                    None => Ok(None),
                }
            };
            let (session, tree) = tokio::join!(session, tree);
            let _ = updates.send(RpcUpdate::UsageLoaded {
                generation,
                session_id,
                session,
                tree,
            });
        });
    }

    pub(super) fn refresh_session_cost(&mut self, session_id: SessionId) {
        let refresh = self.cost_refreshes.entry(session_id).or_default();
        if refresh.in_flight {
            refresh.dirty = true;
            return;
        }
        if refresh.scheduled {
            self.schedule_session_cost_refresh(session_id);
            return;
        }
        self.start_session_cost_refresh(session_id);
    }

    pub(super) fn schedule_session_cost_refresh(&mut self, session_id: SessionId) {
        let refresh = self.cost_refreshes.entry(session_id).or_default();
        refresh.debounce_generation = refresh.debounce_generation.wrapping_add(1);
        refresh.scheduled = true;
        let generation = refresh.debounce_generation;
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            tokio::time::sleep(SESSION_COST_DEBOUNCE).await;
            let _ = updates.send(RpcUpdate::SessionCostDebounceElapsed {
                session_id,
                generation,
            });
        });
    }

    pub(super) fn start_session_cost_refresh(&mut self, session_id: SessionId) {
        self.next_cost_refresh_request_id = self.next_cost_refresh_request_id.wrapping_add(1);
        let request_id = self.next_cost_refresh_request_id;
        let refresh = self.cost_refreshes.entry(session_id).or_default();
        refresh.scheduled = false;
        refresh.in_flight = true;
        refresh.dirty = false;
        refresh.request_id = request_id;
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .session_usage(SessionUsageParams { session_id })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::SessionCostLoaded {
                session_id,
                request_id,
                result,
            });
        });
    }

    #[cfg(test)]
    pub(crate) fn session_cost_refresh_idle_for_test(&self, session_id: SessionId) -> bool {
        self.cost_refreshes
            .get(&session_id)
            .is_some_and(|refresh| !refresh.scheduled && !refresh.in_flight && !refresh.dirty)
    }

    #[cfg(test)]
    pub(crate) fn session_cost_request_id_for_test(&self, session_id: SessionId) -> Option<u64> {
        self.cost_refreshes
            .get(&session_id)
            .filter(|refresh| refresh.in_flight)
            .map(|refresh| refresh.request_id)
    }
}
