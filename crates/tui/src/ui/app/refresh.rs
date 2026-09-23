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
                        self.client.recover_session(replay_session, true);
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
        match self
            .client
            .list_sessions(SessionListParams::default())
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
        self.refresh_coherent_lists().await;
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
        snapshot: cookie_agent_protocol::RuntimeSnapshotV1,
    ) {
        if self.runtime.snapshot().is_some() {
            return;
        }
        self.runtime.install_initial(snapshot.clone());
        self.install_runtime_projection(snapshot);
    }

    pub(in crate::ui) fn install_runtime_response(
        &mut self,
        baseline: Option<&cookie_agent_protocol::RuntimeRevision>,
        snapshot: cookie_agent_protocol::RuntimeSnapshotV1,
    ) -> bool {
        if !self.runtime.install_response(baseline, snapshot.clone()) {
            return false;
        }
        self.install_runtime_projection(snapshot);
        true
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
        snapshot: cookie_agent_protocol::RuntimeSnapshotV1,
    ) {
        self.catalog_revision = Some(snapshot.catalog_revision);
        self.model_revision = Some(snapshot.model_revision);
        self.agent_revision = Some(snapshot.agent_revision);
        self.providers = snapshot.providers;
        self.models = snapshot.models;
        self.agents = snapshot.agents;
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

    pub(super) fn subscribe_session_background(
        &self,
        session_id: SessionId,
        cursor: Option<u64>,
        live_attempt: Option<u64>,
    ) {
        // Re-subscribing is safe: the lane lock serializes it with any prior
        // subscription for the same session, and the client reconciles
        // cursors.
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let lanes = self.subscription_lanes.clone();
        self.spawn_rpc(async move {
            let lane = {
                let mut lanes = lanes.lock().await;
                lanes
                    .entry(session_id)
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                    .clone()
            };
            let _guard = lane.lock().await;
            let outcome = match tokio::time::timeout(
                TREE_SUBSCRIPTION_TIMEOUT,
                client.subscribe_events(session_id, cursor),
            )
            .await
            {
                Ok(Err(crate::client::ClientError::ReplayInProgress)) => {
                    SessionLiveSubscriptionOutcome::ReplayInProgress
                }
                Ok(Ok(())) => SessionLiveSubscriptionOutcome::Established,
                Ok(Err(error)) => SessionLiveSubscriptionOutcome::Failed(error.to_string()),
                Err(_) => {
                    SessionLiveSubscriptionOutcome::Failed("session subscription timed out".into())
                }
            };
            let _ = updates.send(RpcUpdate::SessionLiveSubscriptionFinished {
                session_id,
                live_attempt,
                outcome,
            });
        });
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
        match update {
            RpcUpdate::Status(status) => {
                self.session_errors.record(&status);
                self.status = status;
            }
            RpcUpdate::Notice(status) => self.status = status,
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
                            self.restore_composer_text(vec![input]);
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
            RpcUpdate::SessionLiveSubscriptionFinished {
                session_id,
                live_attempt,
                outcome,
            } => self.finish_live_subscription(session_id, live_attempt, outcome),
            RpcUpdate::SteerFailed {
                session_id,
                input,
                error,
            } => {
                if self.selected == Some(session_id) {
                    self.restore_composer_text(vec![input]);
                    self.status = format!("message not sent ({error}); restored to the composer");
                } else {
                    self.store.park_voided_input(session_id, input);
                    self.status =
                        format!("a message failed to send ({error}); kept for its session");
                }
            }
            RpcUpdate::SteerRecalled { session_id, text } => {
                if self.selected == Some(session_id) {
                    self.restore_composer_text(vec![text]);
                    self.status = "recalled message restored to the composer".into();
                } else {
                    // The composer belongs to another session right now;
                    // park the text so it is restored when that session is
                    // viewed rather than leaking across sessions.
                    self.store.park_voided_input(session_id, text);
                    self.status = "recalled message kept for its session".into();
                }
            }
            RpcUpdate::Reverted { session_id, text } => {
                // The transcript rebuild rides the SessionReverted event;
                // here only the composer's share and the tree's
                // branch-derived rows (title/status/usage) need attention.
                if self.selected == Some(session_id) {
                    self.restore_composer_text(vec![text]);
                    self.status = "reverted; the message text is back in the composer".into();
                } else {
                    self.store.park_voided_input(session_id, text);
                    self.status = "reverted; the message text is kept for its session".into();
                }
                self.refresh_tree_background();
            }
            RpcUpdate::Forked { forked } => {
                self.status = "forked the session; switching to it".into();
                self.refresh_tree_background();
                // This update is emitted only after the fork RPC confirms success.
                self.ownership_classifications.remove(&forked);
                self.owned_sessions.insert(forked);
                self.read_only_sessions.remove(&forked);
                self.pending_live_subscriptions.remove(&forked);
                self.live_subscription_attempts.remove(&forked);
                self.replay_ended_for_live_subscription.remove(&forked);
                self.reroot_tree(forked);
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
                self.connect_task = None;
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
                self.mcp_panel.refresh_in_flight = false;
                match result {
                    Ok(servers) => {
                        self.mcp_panel.install(servers.servers);
                    }
                    Err(error) => self.status = format!("MCP refresh failed: {error}"),
                }
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
                Err(error) => self.status = format!("MCP authentication failed: {error}"),
            },
            RpcUpdate::McpAuthCancelled { result } => match result {
                Ok(server) => {
                    if self
                        .mcp_panel
                        .auth
                        .as_ref()
                        .is_some_and(|auth| auth.server == server)
                    {
                        self.mcp_panel.auth = None;
                    }
                    self.status = "MCP authentication cancelled.".into();
                    self.poll_mcp();
                }
                Err(error) => {
                    self.mcp_panel.auth = None;
                    self.status = format!("MCP authentication cancel failed: {error}");
                    self.poll_mcp();
                }
            },
            RpcUpdate::PermissionsLoaded { session_id, result } => {
                if self.selected != Some(session_id) {
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
            RpcUpdate::SkillsLoaded { session_id, result } => {
                if self.selected != Some(session_id) {
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
                        Err(ClientError::Rpc(error))
                            if error.code == SESSION_TREE_USAGE_CORRUPT_DELEGATION_CODE =>
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

    pub(super) fn subscribe_tree_sessions(&mut self, tree: &SessionTree) {
        let mut session_ids = Vec::new();
        collect_tree_session_ids(tree, &mut session_ids);
        for session_id in session_ids {
            if !self.tree_subscription_sessions.insert(session_id) {
                continue;
            }
            let cursor = self
                .store
                .sessions
                .get(&session_id)
                .map(|state| state.last_seq);
            self.subscribe_session_background(session_id, cursor, None);
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
        }
        if let ClientDelivery::RuntimeChanged(changed) = &delivery {
            self.install_runtime_notification((**changed).clone());
            return;
        }
        if let ClientDelivery::RecoveryFailed { session_id, error } = &delivery {
            self.status = match session_id {
                Some(session_id) => format!("recovery for {session_id} failed: {error}"),
                None => format!("recovery failed: {error}"),
            };
            return;
        }
        if let ClientDelivery::PluginEvent(event) = &delivery {
            if Some(event.session_id) == self.selected {
                self.status = format!("plugin {}: {}", event.plugin, event.name);
            }
            return;
        }
        let event = match &delivery {
            ClientDelivery::Live { message, .. } => match message.as_ref() {
                cookie_agent_protocol::EventSubscriptionMessage::Event { event } => {
                    Some(event.as_ref())
                }
                cookie_agent_protocol::EventSubscriptionMessage::Gap { .. } => None,
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
        let terminalized = event.is_some_and(|event| {
            matches!(
                &event.payload,
                EventPayload::RunCompleted { .. }
                    | EventPayload::RunFailed { .. }
                    | EventPayload::RunCancelled { .. }
                    | EventPayload::RunInterrupted { .. }
                    | EventPayload::DelegateFinished { .. }
                    | EventPayload::DelegateFinishedV2 { .. }
                    | EventPayload::DelegateChildTerminated { .. }
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
        let revert_rebuild = event.is_some_and(|event| {
            Some(event.session_id) == self.selected
                && matches!(&event.payload, EventPayload::SessionReverted { .. })
        });
        let event_session_id = event.map(|event| event.session_id);
        let event_session_len_before = event_session_id.and_then(|session_id| {
            self.store
                .sessions
                .get(&session_id)
                .map(|state| state.transcript.len())
        });
        let outcome = self.store.apply_delivery(delivery);
        // A warning-or-worse row appended to a session other than the viewed
        // one interleaves into the viewed conversation; if the viewed session
        // is mid-block, its pre-warning content must finish above the break.
        if matches!(outcome, DeliveryOutcome::Applied)
            && let Some(event_session_id) = event_session_id
            && Some(event_session_id) != self.selected
            && let Some(selected) = self.selected
            && self
                .store
                .sessions
                .get(&event_session_id)
                .is_some_and(|state| {
                    Some(state.transcript.len()) > event_session_len_before
                        && matches!(
                            state.transcript.last(),
                            Some(TranscriptItem::Event { level, .. })
                                if *level >= crate::state::EventLevel::Warning
                        )
                })
            && let Some(state) = self.store.sessions.get_mut(&selected)
        {
            state.mark_event_split_pending();
        }
        if let Some(session_id) = replay_ended_session
            && self.pending_live_subscriptions.contains(&session_id)
        {
            self.replay_ended_for_live_subscription.insert(session_id);
            self.start_pending_live_subscription(session_id);
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
                self.client.recover_session(session_id, true);
            }
        }
        self.reconcile_pending_approval();
        if linked || terminalized || replay_finished {
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
        }
    }

    pub(in crate::ui) fn recover_timed_out_replays(&mut self) {
        for session_id in self.store.abandon_timed_out_replays() {
            self.status = "replay timed out; retrying recovery".into();
            self.client.recover_session(session_id, true);
        }
        self.reconcile_pending_approval();
    }

    pub(in crate::ui) fn poll_mcp(&mut self) {
        if self.modal != Modal::Mcp || self.mcp_panel.refresh_in_flight {
            return;
        }
        self.mcp_panel.refresh_in_flight = true;
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
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_skills(cookie_agent_protocol::SkillsListParams { session_id })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::SkillsLoaded { session_id, result });
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
