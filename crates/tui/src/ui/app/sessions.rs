//! Session selection, tree navigation, titles, and search rows.

use super::*;

/// Extract an immediate title patch from a `SessionTitleCommitted` event:
/// (session, new title, authoritative sequence).
pub(super) fn title_change_from_event(
    event: &cookie_agent_protocol::StoredEvent,
) -> Option<(SessionId, Option<SessionTitle>, u64)> {
    let EventPayload::SessionTitleCommitted { change, .. } = &event.payload else {
        return None;
    };
    let title = match change {
        SessionTitleChange::UserSet { title, .. }
        | SessionTitleChange::InternalAgentSet { title, .. }
        | SessionTitleChange::DelegatedSet { title, .. }
        | SessionTitleChange::FallbackSet { title } => Some(title.clone()),
        SessionTitleChange::UserClear { .. } | SessionTitleChange::UserReset { .. } => None,
    };
    Some((event.session_id, title, event.seq))
}

/// Mirror the engine session projection's run lifecycle status derivation.
pub(in crate::ui) fn status_change_from_event(
    event: &cookie_agent_protocol::StoredEvent,
) -> Option<(SessionId, SessionStatus, u64)> {
    let status = match &event.payload {
        EventPayload::RunStarted { .. } => SessionStatus::Running,
        EventPayload::RunCompleted { .. } => SessionStatus::Completed,
        EventPayload::RunFailed { .. } => SessionStatus::Failed,
        EventPayload::RunCancelled { .. } => SessionStatus::Cancelled,
        EventPayload::RunInterrupted { .. } => SessionStatus::Interrupted,
        EventPayload::DelegateChildTerminated { status, .. } => *status,
        _ => return None,
    };
    Some((event.session_id, status, event.seq))
}

/// Collect the newest known title for each session from the live tree and
/// session list, so stale patches can be repaired with the newer value.
pub(super) fn collect_known_titles(
    tree: Option<&SessionTree>,
    sessions: &[SessionMeta],
    sequences: &HashMap<SessionId, u64>,
    titles: &mut HashMap<SessionId, (u64, Option<SessionTitle>)>,
) {
    fn walk(node: &SessionTree, titles: &mut HashMap<SessionId, (u64, Option<SessionTitle>)>) {
        titles.insert(
            node.session.session_id,
            (node.session.title_updated_seq, node.session.title.clone()),
        );
        for child in &node.children {
            walk(child, titles);
        }
    }
    if let Some(tree) = tree {
        walk(tree, titles);
    }
    for session in sessions {
        titles
            .entry(session.session_id)
            .and_modify(|entry| {
                if session.title_updated_seq > entry.0 {
                    *entry = (session.title_updated_seq, session.title.clone());
                }
            })
            .or_insert((session.title_updated_seq, session.title.clone()));
    }
    // Any session with a newer sequence but no meta patch yet keeps its
    // recorded sequence so stale tree values cannot regress it.
    for (session_id, seq) in sequences {
        titles
            .entry(*session_id)
            .and_modify(|entry| entry.0 = entry.0.max(*seq))
            .or_insert((*seq, None));
    }
}

pub(super) fn collect_known_statuses(
    tree: Option<&SessionTree>,
    sessions: &[SessionMeta],
    statuses: &mut HashMap<SessionId, (u64, SessionStatus)>,
) {
    fn record(meta: &SessionMeta, statuses: &mut HashMap<SessionId, (u64, SessionStatus)>) {
        statuses
            .entry(meta.session_id)
            .and_modify(|entry| {
                if meta.last_event_seq > entry.0 {
                    *entry = (meta.last_event_seq, meta.status);
                }
            })
            .or_insert((meta.last_event_seq, meta.status));
    }

    fn walk(node: &SessionTree, statuses: &mut HashMap<SessionId, (u64, SessionStatus)>) {
        record(&node.session, statuses);
        for child in &node.children {
            walk(child, statuses);
        }
    }

    if let Some(tree) = tree {
        walk(tree, statuses);
    }
    for session in sessions {
        record(session, statuses);
    }
}

pub(super) fn patch_tree_node_statuses(
    tree: &mut SessionTree,
    statuses: &HashMap<SessionId, (u64, SessionStatus)>,
) {
    if let Some((seq, status)) = statuses.get(&tree.session.session_id)
        && tree.session.last_event_seq < *seq
    {
        tree.session.last_event_seq = *seq;
        tree.session.status = *status;
    }
    for child in &mut tree.children {
        patch_tree_node_statuses(child, statuses);
    }
}

pub(super) fn patch_tree_node_titles(
    tree: &mut SessionTree,
    sequences: &HashMap<SessionId, u64>,
    known: &HashMap<SessionId, (u64, Option<SessionTitle>)>,
) {
    let session_id = tree.session.session_id;
    let known_seq = sequences.get(&session_id).copied().unwrap_or(0);
    if tree.session.title_updated_seq < known_seq {
        // The tree response is older than a title event already applied;
        // restore the newest known title and sequence.
        tree.session.title = known.get(&session_id).and_then(|(_, title)| title.clone());
        tree.session.title_updated_seq = known_seq;
    }
    for child in &mut tree.children {
        patch_tree_node_titles(child, sequences, known);
    }
}

pub(super) fn collect_tree_session_ids(tree: &SessionTree, session_ids: &mut Vec<SessionId>) {
    session_ids.push(tree.session.session_id);
    for child in &tree.children {
        collect_tree_session_ids(child, session_ids);
    }
}

/// Depth-first collection of a subtree's session metadata, used to attribute
/// descendant warnings to their owning session.
pub(super) fn collect_subtree_sessions(tree: &SessionTree, sessions: &mut Vec<SessionMeta>) {
    sessions.push(tree.session.clone());
    for child in &tree.children {
        collect_subtree_sessions(child, sessions);
    }
}

pub(super) fn find_session(tree: &SessionTree, session_id: SessionId) -> Option<&SessionMeta> {
    find_node(tree, session_id).map(|node| &node.session)
}

pub(super) fn find_node(tree: &SessionTree, session_id: SessionId) -> Option<&SessionTree> {
    if tree.session.session_id == session_id {
        return Some(tree);
    }
    tree.children
        .iter()
        .find_map(|child| find_node(child, session_id))
}

pub(super) fn find_node_mut(
    tree: &mut SessionTree,
    session_id: SessionId,
) -> Option<&mut SessionTree> {
    if tree.session.session_id == session_id {
        return Some(tree);
    }
    tree.children
        .iter_mut()
        .find_map(|child| find_node_mut(child, session_id))
}

impl App {
    pub(in crate::ui) async fn select_session(&mut self, session_id: SessionId) {
        self.reroot_tree(session_id);
        let cursor = self
            .store
            .sessions
            .get(&session_id)
            .map(|state| state.last_seq);
        match tokio::time::timeout(
            TREE_SUBSCRIPTION_TIMEOUT,
            self.client.subscribe_events(session_id, cursor),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.status = error.to_string(),
            Err(_) => self.status = "session subscription timed out".into(),
        }
    }

    /// Watch a session inside the current delegation tree: the conversation
    /// and highlight change, but the tree root, tree snapshot, and cursor are
    /// never cleared or rerooted. All tree refreshes keep querying the
    /// original root.
    pub(in crate::ui) fn watch_session(&mut self, session_id: SessionId) {
        let in_tree = self
            .tree
            .as_ref()
            .is_some_and(|tree| find_session(tree, session_id).is_some());
        if !in_tree {
            self.reroot_tree(session_id);
            self.classify_session_background(session_id);
            return;
        }
        self.set_selected_session(session_id);
        self.classify_session_background(session_id);
        self.tree_cursor = Some(session_id);
        let needs_subscription = self.tree_subscription_sessions.insert(session_id);
        let cursor = self
            .store
            .sessions
            .get(&session_id)
            .map(|state| state.last_seq);
        if needs_subscription {
            self.subscribe_session_background(session_id, cursor, None);
        }
    }

    /// Intentionally reroot the delegation tree at a separate session.
    pub(in crate::ui) fn reroot_tree(&mut self, session_id: SessionId) {
        let root_changed = self.tree_root != Some(session_id);
        self.set_selected_session(session_id);
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.tree_root = Some(session_id);
        self.tree = None;
        if root_changed {
            self.agent_panel_mode = AgentPanelMode::Auto;
        }
        self.tree_cursor = Some(session_id);
        self.tree_subscription_sessions.clear();
        self.tree_subscription_sessions.insert(session_id);
        self.tree_refresh_in_flight = None;
        self.tree_refresh_pending = false;
        self.tree_offset = 0;
        self.tree_viewport_height = 0;
        let cursor = self
            .store
            .sessions
            .get(&session_id)
            .map(|state| state.last_seq);
        self.subscribe_session_background(session_id, cursor, None);
        self.refresh_tree_background();
    }

    pub(in crate::ui) fn set_selected_session(&mut self, session_id: SessionId) {
        let changed = self.selected != Some(session_id);
        if changed {
            self.goal_focus = None;
            self.goal_detail = goal::GoalDetailState::default();
            if self.modal == Modal::GoalDetail {
                self.modal = Modal::None;
            }
            // Watching a different session should begin at its live tail.
            self.conversation_scroll = ConversationScroll::default();
            self.scrollbar_geometry = None;
            self.scrollbar_drag = None;
            // A conversation-leg selection addresses rendered lines of the
            // session being left; kept across the switch it would paint and
            // copy rows of the newly watched session. The composer leg
            // survives: the draft buffer persists across watches.
            if matches!(self.selection, Some(TextSelection::Conversation { .. })) {
                self.selection = None;
            }
        }
        self.selected = Some(session_id);
        if changed {
            self.set_draft_reset_intent(false);
            let root = self.permission_mode_root(session_id);
            self.permission_modes.remove(&root);
            self.refresh_permission_mode_for_session(session_id);
            self.load_skills_for_session(session_id);
        }
        self.rebind_draft_to_selected_session();
        // Voided inputs (run-end casualties, cross-session recalls) are
        // restored exactly when their session is being viewed.
        self.restore_voided_inputs();
    }

    /// Rebind the draft to the newly watched session. A root session drafts
    /// its own current creation selection when still valid against the
    /// coherent descriptors, otherwise the root default; a delegated session
    /// is pinned to its frozen child agent with the valid chain model/variant
    /// — a previous root draft is never carried into a child.
    pub(super) fn rebind_draft_to_selected_session(&mut self) {
        if self.draft_reset_fallback {
            return;
        }
        let Some(meta) = self.selected_session_meta().cloned() else {
            return;
        };
        match &meta.origin {
            cookie_agent_protocol::SessionOrigin::Delegated { .. } => {
                // The pinned child draft derives only from the persisted
                // frozen chain: the exact creation selection when it is a
                // chain member, otherwise the chain head (the inherited
                // frozen suffix head for empty-chain children).
                let creation = meta.creation_selection.clone();
                if let Some(chain) = self.persisted_chain() {
                    let model = chain
                        .iter()
                        .find(|selection| **selection == creation.model)
                        .cloned()
                        .or_else(|| chain.first().cloned())
                        .unwrap_or(creation.model);
                    self.draft = Some(RunSelection {
                        agent: creation.agent,
                        model,
                        preset: creation.preset,
                    });
                } else {
                    self.draft = Some(creation);
                }
            }
            cookie_agent_protocol::SessionOrigin::Root => {
                let creation = meta.creation_selection.clone();
                self.draft = Some(creation);
                self.revalidate_draft();
            }
        }
        self.sync_session_model_draft();
    }

    pub(super) fn set_draft_reset_intent(&mut self, armed: bool) {
        self.draft_generation = self.draft_generation.wrapping_add(1);
        self.draft_reset_fallback = armed;
        self.pending_fallback_resets.clear();
    }

    pub(super) fn track_fallback_reset(
        &mut self,
        session_id: SessionId,
        client_run_id: &ClientRunId,
    ) {
        if self.draft_reset_fallback {
            self.pending_fallback_resets.insert(
                client_run_id.clone(),
                PendingFallbackReset {
                    session_id,
                    draft_generation: self.draft_generation,
                    rpc_admitted: false,
                    replay_generation: None,
                },
            );
        }
    }

    pub(super) fn acknowledge_fallback_reset(
        &mut self,
        session_id: SessionId,
        client_run_id: &ClientRunId,
        generation: Option<u64>,
        started_event: bool,
        replay_generation: Option<u64>,
    ) {
        let Some(pending) = self.pending_fallback_resets.get_mut(client_run_id) else {
            return;
        };
        if self.selected != Some(session_id)
            || pending.session_id != session_id
            || pending.draft_generation != self.draft_generation
            || generation.is_some_and(|generation| generation != pending.draft_generation)
        {
            return;
        }
        self.draft_reset_fallback = false;
        if started_event && replay_generation.is_none() {
            // Any accepted submission of this same intent consumes it once.
            self.pending_fallback_resets.clear();
        } else {
            // Do not sync from the previous run's projection while its successor's
            // admission event is still in flight or awaiting replay.
            pending.rpc_admitted = true;
            if started_event {
                pending.replay_generation = replay_generation;
            }
        }
    }

    pub(super) fn finish_fallback_reset_replay(&mut self, session_id: SessionId, generation: u64) {
        if self.pending_fallback_resets.values().any(|pending| {
            pending.session_id == session_id
                && pending.draft_generation == self.draft_generation
                && pending.replay_generation == Some(generation)
        }) {
            self.pending_fallback_resets.clear();
        }
    }

    pub(super) fn sync_session_model_draft(&mut self) {
        if self.draft_reset_fallback
            || self.pending_fallback_resets.values().any(|pending| {
                pending.rpc_admitted
                    && pending.draft_generation == self.draft_generation
                    && Some(pending.session_id) == self.selected
            })
        {
            return;
        }
        let selection = self
            .selected
            .and_then(|session| self.store.sessions.get(&session))
            .and_then(|state| state.model_selection.selection.clone());
        if let Some(selection) = selection
            && (self.selection_is_live(&selection.model)
                || self
                    .persisted_chain()
                    .is_some_and(|chain| chain.contains(&selection.model)))
        {
            self.draft = Some(selection);
        }
    }

    pub(super) fn note_sessions_changed(&mut self) {
        self.sessions_revision = self.sessions_revision.wrapping_add(1);
    }

    /// Session picker input: root-level entries only. `self.sessions` is the
    /// general metadata cache and also carries delegated children once they are
    /// watched from the Agents tree; the picker selects a session *tree*, so
    /// those children must never surface as top-level rows.
    pub(in crate::ui) fn picker_sessions(&self) -> impl Iterator<Item = &SessionMeta> {
        self.sessions
            .iter()
            .filter(|session| matches!(session.origin, cookie_agent_protocol::SessionOrigin::Root))
    }

    pub(super) fn refresh_session_search_rows_cache(&mut self) {
        let now = jiff::Timestamp::now();
        let time_zone = jiff::tz::TimeZone::system();
        let local_day = now.to_zoned(time_zone.clone()).date();
        let query = self.session_search.query();
        if self.session_search_rows_cache.query == query
            && self.session_search_rows_cache.sessions_revision == self.sessions_revision
            && self.session_search_rows_cache.sessions_len == self.sessions.len()
            && self.session_search_rows_cache.local_day == Some(local_day)
        {
            return;
        }
        let rows = session_search_rows(self.picker_sessions(), query, now, &time_zone);
        self.session_search_rows_cache = SessionSearchRowsCache {
            query: query.to_owned(),
            sessions_revision: self.sessions_revision,
            sessions_len: self.sessions.len(),
            local_day: Some(local_day),
            rows,
        };
    }

    pub(in crate::ui) fn current_session_search_rows(&mut self) -> &[SessionSearchRow] {
        self.refresh_session_search_rows_cache();
        &self.session_search_rows_cache.rows
    }

    pub(super) fn session_search_ids(&mut self) -> Vec<SessionId> {
        self.current_session_search_rows()
            .iter()
            .filter_map(SessionSearchRow::session_id)
            .collect()
    }

    /// Providers matching the current picker query by display name or ID.
    pub(in crate::ui) fn filtered_providers(&self) -> Vec<&ProviderDescriptor> {
        self.providers
            .iter()
            .filter(|provider| provider_matches(provider, self.provider_search.query()))
            .collect()
    }

    pub(in crate::ui) fn picker_entry_count(&mut self) -> usize {
        match self.modal {
            Modal::Sessions => self.session_search_ids().len(),
            Modal::Presets => self.preset_names().len() + 1,
            Modal::Agents => self.filtered_agent_picker_candidates().len(),
            Modal::Models => self.filtered_draft_models().len(),
            Modal::Variants => self.variant_step_options().len(),
            Modal::ConnectProviders => self.filtered_providers().len(),
            Modal::UserMessage => USER_MENU_ITEMS.len(),
            Modal::Mcp | Modal::Permissions | Modal::Usage | Modal::GoalDetail => 0,
            Modal::ConnectDetails
            | Modal::ConnectSetup
            | Modal::ConnectError
            | Modal::DisconnectConfirm
            | Modal::RevertConfirm
            | Modal::None => 0,
        }
    }

    pub(super) fn clamp_picker_selection(&mut self) {
        let count = self.picker_entry_count();
        if count == 0 {
            self.picker_state.select(None);
        } else {
            self.picker_state.select(Some(
                self.picker_state.selected().unwrap_or(0).min(count - 1),
            ));
        }
    }

    pub(super) fn session_search_changed(&mut self) {
        self.picker_state.select(Some(0));
        self.clamp_picker_selection();
    }

    pub(super) fn provider_search_changed(&mut self) {
        self.picker_state.select(Some(0));
        self.clamp_picker_selection();
    }

    pub(super) fn model_search_changed(&mut self) {
        self.picker_state.select(Some(0));
        self.clamp_picker_selection();
    }

    pub(super) fn agent_search_changed(&mut self) {
        self.picker_state.select(Some(0));
        self.clamp_picker_selection();
    }

    pub(super) fn close_agent_picker(&mut self) {
        self.agent_search.reset();
        self.modal = Modal::None;
        self.new_session_draft = None;
    }

    pub(super) fn close_model_picker(&mut self) {
        self.model_search.reset();
        self.model_then_variant = false;
        self.variant_step_model = None;
        self.modal = Modal::None;
        self.new_session_draft = None;
    }

    pub(super) fn close_provider_picker(&mut self) {
        self.clear_connect_secrets();
        self.provider_search.reset();
        self.modal = Modal::None;
        self.status = "Provider connection cancelled.".into();
    }

    pub(in crate::ui) fn register_escape(&mut self, now: Instant) -> bool {
        const ESC_CANCEL_WINDOW: Duration = Duration::from_millis(500);
        let cancel = self
            .last_escape
            .and_then(|previous| now.checked_duration_since(previous))
            .is_some_and(|elapsed| elapsed <= ESC_CANCEL_WINDOW);
        self.last_escape = (!cancel).then_some(now);
        cancel
    }

    /// Records a Ctrl-C that found nothing to interrupt. Returns true when it
    /// is the second such press inside the quit window, which quits.
    pub(in crate::ui) fn register_quit_press(&mut self, now: Instant) -> bool {
        const CTRL_C_QUIT_WINDOW: Duration = Duration::from_secs(2);
        let quit = self
            .last_quit_press
            .and_then(|previous| now.checked_duration_since(previous))
            .is_some_and(|elapsed| elapsed <= CTRL_C_QUIT_WINDOW);
        self.last_quit_press = (!quit).then_some(now);
        quit
    }

    pub(in crate::ui) fn move_tree_selection(&mut self, up: bool) {
        let entries = self.tree_entries();
        if entries.is_empty() {
            self.tree_cursor = None;
            self.tree_offset = 0;
            return;
        }
        let index = self.tree_cursor_index(&entries).unwrap_or(0);
        let next = if up {
            index.saturating_sub(1)
        } else {
            (index + 1).min(entries.len() - 1)
        };
        self.tree_cursor = Some(entries[next].0);
        self.clamp_tree_view();
    }

    /// The flattened row index of the tree cursor. The cursor is retained by
    /// `SessionId` across tree refreshes; if the session disappears, the
    /// nearest surviving row is used without clearing the cursor identity.
    pub(super) fn tree_cursor_index(
        &self,
        entries: &[(SessionId, SessionMeta, usize)],
    ) -> Option<usize> {
        let cursor = self.tree_cursor?;
        entries
            .iter()
            .position(|(session_id, _, _)| *session_id == cursor)
    }

    pub(super) fn clamp_tree_view(&mut self) {
        let entries = self.tree_entries();
        self.clamp_tree_view_with(&entries);
    }

    pub(super) fn clamp_tree_view_with(&mut self, entries: &[(SessionId, SessionMeta, usize)]) {
        let mut selection = self.tree_cursor_index(entries).unwrap_or(0);
        crate::ui::pickers::clamp_tree_view(
            &mut selection,
            &mut self.tree_offset,
            entries.len(),
            self.tree_viewport_height,
        );
        if self.tree_cursor.is_none() || self.tree_cursor_index(entries).is_none() {
            self.tree_cursor = entries.first().map(|(session_id, _, _)| *session_id);
        }
    }

    pub(in crate::ui) fn toggle_tree_session(&mut self, session_id: SessionId) {
        if !self.collapsed_sessions.insert(session_id) {
            self.collapsed_sessions.remove(&session_id);
        }
        self.clamp_tree_view();
    }
}
