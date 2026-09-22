//! Composer input, submission, steering queue, and session commands.

use super::*;

impl App {
    /// Retire any composer-leg selection: its byte offsets address the
    /// draft as it was when the drag happened, so a later buffer mutation
    /// or cursor move would leave it painting and cutting the wrong slice.
    /// Conversation selections address rendered lines and are unaffected.
    pub(super) fn retire_composer_selection(&mut self) {
        if matches!(self.selection, Some(TextSelection::Composer { .. })) {
            self.selection = None;
        }
    }

    /// Mutate the composer draft, retiring any composer-leg selection.
    pub(super) fn mutate_input(&mut self, mutation: impl FnOnce(&mut InputState)) {
        mutation(&mut self.input);
        self.retire_composer_selection();
    }

    /// Move the composer cursor, retiring any composer-leg selection the
    /// same way a mutation does.
    pub(super) fn navigate_input(&mut self, navigation: impl FnOnce(&mut InputState)) {
        navigation(&mut self.input);
        self.retire_composer_selection();
    }

    /// Delete a non-empty composer selection, as Backspace and Delete do in
    /// any editor. Returns whether there was one to delete.
    fn delete_composer_selection(&mut self) -> bool {
        let Some(selection @ TextSelection::Composer { .. }) = self.selection else {
            return false;
        };
        let (start, end) = selection.byte_range();
        if start == end {
            return false;
        }
        self.mutate_input(|input| input.delete_byte_range(start, end));
        true
    }

    pub(super) fn read_only_input_allowed(&self) -> bool {
        if self.new_session_draft.is_some() {
            return true;
        }
        let input = self.input.as_str().trim_start();
        !input.is_empty() && ("/new".starts_with(input) || input.starts_with("/new "))
    }

    pub(in crate::ui) async fn handle_input_key(&mut self, key: KeyEvent) {
        if self
            .selected
            .is_some_and(|session| self.read_only_sessions.contains(&session))
            && !self.read_only_input_allowed()
            && !(self.input.as_str().is_empty()
                && key.code == KeyCode::Char('/')
                && key.modifiers.is_empty())
        {
            self.input_focused = false;
            self.status = "Session is owned by another cookie process; input is disabled.".into();
            return;
        }
        if self.runtime.phase() == RuntimePhase::ErrorRetry
            && key.code == KeyCode::Enter
            && self.input.as_str().is_empty()
        {
            self.status = "Retrying runtime snapshot…".into();
            self.refresh_coherent_lists().await;
            return;
        }
        if self.runtime.phase() == RuntimePhase::Loading {
            self.status = "loading runtime snapshot".into();
            return;
        }
        if is_newline_key(key) {
            self.input_focused = true;
            self.mutate_input(|input| input.insert_newline());
            return;
        }
        if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
            self.input_focused = true;
            self.mutate_input(|input| input.set_buffer("/".into()));
            self.palette_dismissed = false;
            self.palette_state.select(Some(0));
            return;
        }
        if !self.input_focused {
            match key.code {
                KeyCode::Enter => self.input_focused = true,
                KeyCode::Char(character) if is_printable_key(key) => {
                    self.input_focused = true;
                    if self.input.as_str().is_empty() && character == '/' {
                        self.palette_dismissed = false;
                    }
                    self.mutate_input(|input| input.insert(character));
                }
                _ => {}
            }
            return;
        }
        // A selection is deleted whole, with or without Ctrl, rather than
        // retired in favor of a one-character or one-word delete.
        if matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
            && self.delete_composer_selection()
        {
            return;
        }
        match key.code {
            KeyCode::Enter => self.submit_input().await,
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mutate_input(|input| input.delete_word_left());
            }
            KeyCode::Backspace => {
                self.mutate_input(|input| input.backspace());
            }
            KeyCode::Delete if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mutate_input(|input| input.delete_word_right());
            }
            KeyCode::Delete => {
                self.mutate_input(|input| input.delete());
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_input(|input| input.move_word_left());
            }
            KeyCode::Left => self.navigate_input(|input| input.move_left()),
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_input(|input| input.move_word_right());
            }
            KeyCode::Right => self.navigate_input(|input| input.move_right()),
            KeyCode::Up => {
                // Recall gesture: with an empty composer and a non-empty
                // pending lane, Up withdraws the newest pending message back
                // for editing instead of moving a cursor that has nothing
                // to move through.
                if self.input.as_str().is_empty() && self.selected_pending_inputs().is_some() {
                    self.recall_newest_pending();
                } else {
                    self.navigate_input(|input| input.move_up());
                }
            }
            KeyCode::Down => self.navigate_input(|input| input.move_down()),
            KeyCode::PageUp => {
                let page = self
                    .hit_map
                    .conversation
                    .map_or(1, |rect| rect.height.max(1));
                self.conversation_scroll.up(usize::from(page));
            }
            KeyCode::PageDown => {
                let page = self
                    .hit_map
                    .conversation
                    .map_or(1, |rect| rect.height.max(1));
                self.conversation_scroll.down(usize::from(page));
            }
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_input(|input| input.move_buffer_home());
            }
            KeyCode::Home => self.navigate_input(|input| input.move_home()),
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_input(|input| input.move_buffer_end());
            }
            KeyCode::End => self.navigate_input(|input| input.move_end()),
            KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
                self.navigate_input(|input| input.move_home());
            }
            KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => {
                self.navigate_input(|input| input.move_end());
            }
            KeyCode::Char(character) if is_printable_key(key) => {
                if self.input.as_str().is_empty() && character == '/' {
                    self.palette_dismissed = false;
                }
                self.mutate_input(|input| input.insert(character));
            }
            _ => {}
        }
    }

    pub(in crate::ui) fn handle_paste(&mut self, text: &str) {
        if self.modal == Modal::Sessions {
            let sanitized = text.replace(['\r', '\n'], "");
            self.session_search.focus_input();
            self.session_search.input_mut().insert_text(&sanitized);
            self.session_search_changed();
            return;
        }
        if self.modal == Modal::ConnectProviders {
            let sanitized = text.replace(['\r', '\n'], "");
            self.provider_search.focus_input();
            self.provider_search.input_mut().insert_text(&sanitized);
            self.provider_search_changed();
            return;
        }
        if self.modal == Modal::Models {
            let sanitized = text.replace(['\r', '\n'], "");
            self.model_search.focus_input();
            self.model_search.input_mut().insert_text(&sanitized);
            self.model_search_changed();
            return;
        }
        if self.modal == Modal::Agents {
            let sanitized = text.replace(['\r', '\n'], "");
            self.agent_search.focus_input();
            self.agent_search.input_mut().insert_text(&sanitized);
            self.agent_search_changed();
            return;
        }
        if self.modal == Modal::ConnectSetup {
            let mut sanitized = Zeroizing::new(text.replace(['\r', '\n'], ""));
            if let Some(form) = &mut self.provider_form {
                match form.focus() {
                    ProviderFormFocus::Credential(index) => {
                        form.error = None;
                        form.secrets[index]
                            .input
                            .insert_owned(std::mem::take(&mut *sanitized));
                    }
                    ProviderFormFocus::Setup(index) => {
                        form.error = None;
                        form.setup[index]
                            .input
                            .insert_owned(std::mem::take(&mut *sanitized));
                    }
                    ProviderFormFocus::AuthMethod
                    | ProviderFormFocus::Submit
                    | ProviderFormFocus::Cancel => {}
                }
            }
            return;
        }
        if self.modal == Modal::Mcp {
            if let Some(input) = self
                .mcp_panel
                .form
                .as_mut()
                .and_then(McpForm::focused_input)
            {
                input.insert_text(&text.replace(['\r', '\n'], ""));
            }
            return;
        }
        if self.modal == Modal::Permissions {
            if let Some(form) = &mut self.permission_panel.form
                && form.focus_pattern
            {
                form.pattern.insert_text(&text.replace(['\r', '\n'], ""));
            }
            return;
        }
        if self.modal != Modal::None {
            return;
        }
        if self
            .selected
            .is_some_and(|session| self.read_only_sessions.contains(&session))
            && !self.read_only_input_allowed()
            && !text.trim_start().starts_with("/new")
        {
            self.input_focused = false;
            self.status = "Session is owned by another cookie process; input is disabled.".into();
            return;
        }
        self.input_focused = true;
        if self.input.as_str().is_empty() && text.starts_with('/') {
            self.palette_dismissed = false;
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.mutate_input(|input| input.insert_text(&normalized));
    }

    pub(in crate::ui) async fn submit_input(&mut self) {
        if self
            .selected
            .is_some_and(|session| self.read_only_sessions.contains(&session))
            && !self.read_only_input_allowed()
        {
            self.status = "Session is owned by another cookie process; input is disabled.".into();
            return;
        }
        if self.input.as_str().trim().is_empty() {
            return;
        }
        let skills = self
            .skills
            .iter()
            .filter(|skill| skill.precedence_winner && skill.user_invocable)
            .map(|skill| skill.name.clone())
            .collect::<Vec<_>>();
        let mut submission = parse_submission_with_skills(self.input.as_str(), &skills);
        let could_be_pending_skill = self
            .input
            .as_str()
            .strip_prefix('/')
            .and_then(|input| input.split_whitespace().next())
            .is_some_and(|name| {
                !COMMANDS
                    .iter()
                    .any(|spec| spec.name == name || spec.aliases.contains(&name))
            });
        if submission.is_err() && self.new_session_draft.is_some() && could_be_pending_skill {
            let selection = self
                .new_session_draft
                .clone()
                .expect("pending new-session draft");
            if self.create_root_session(selection).await {
                let Some(session_id) = self.selected else {
                    self.status = "new session was created without a selection".into();
                    return;
                };
                match self
                    .client
                    .list_skills(cookie_agent_protocol::SkillsListParams { session_id })
                    .await
                {
                    Ok(result) => {
                        self.skills = result.skills.clone();
                        self.skill_panel.install(result);
                        let skill_names = self
                            .skills
                            .iter()
                            .filter(|skill| skill.precedence_winner && skill.user_invocable)
                            .map(|skill| skill.name.clone())
                            .collect::<Vec<_>>();
                        submission =
                            parse_submission_with_skills(self.input.as_str(), &skill_names);
                    }
                    Err(error) => {
                        self.status = format!("skill discovery failed: {error}");
                        return;
                    }
                }
            } else {
                // Keep both the original slash input and pending selection for retry.
                return;
            }
        }
        let submission = match submission {
            Ok(submission) => submission,
            Err(error) => {
                self.mutate_input(|input| {
                    input.take();
                });
                self.palette_dismissed = false;
                self.status = error;
                return;
            }
        };
        match submission {
            Submission::Command(command) => {
                self.mutate_input(|input| {
                    input.take();
                });
                self.palette_dismissed = false;
                self.run_command(command).await;
            }
            Submission::Prompt(prompt) => self.submit_prompt(prompt).await,
        }
    }

    pub(in crate::ui) async fn submit_prompt(&mut self, input: String) {
        if self.runtime.phase() == RuntimePhase::Loading {
            self.status = "loading runtime snapshot".into();
            return;
        }
        if self.runtime.phase() == RuntimePhase::ErrorRetry && self.runtime.snapshot().is_none() {
            self.status = self
                .runtime
                .durable_explanation()
                .unwrap_or("runtime snapshot unavailable; retry")
                .into();
            return;
        }
        if self.runtime.is_empty() {
            self.status = EMPTY_RUNTIME_GUIDANCE.into();
            return;
        }
        if let Some(selection) = self.new_session_draft.clone() {
            // `/new` may be opened while an existing session remains
            // selected. The draft is the authoritative signal that this
            // first message belongs to a new root.
            self.create_root_session(selection).await;
            if self.new_session_draft.is_some() {
                return;
            }
        }
        let Some(session_id) = self.selected else {
            self.status = "create or select a session first".into();
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let active_run = self
            .store
            .sessions
            .get(&session_id)
            .and_then(|state| state.active_run);
        let selection = if active_run.is_none() {
            self.validated_draft_selection()
        } else {
            None
        };
        let reset_fallback = self.draft_reset_fallback;
        if active_run.is_none() && selection.is_none() {
            self.status = "select a draft agent/model before submitting".into();
            return;
        }
        self.mutate_input(|input| {
            input.take();
        });
        self.palette_dismissed = false;
        let submitted_id = client_run_id();
        let draft_generation = self.draft_generation;
        if active_run.is_none() {
            self.track_fallback_reset(session_id, &submitted_id);
        }
        self.spawn_rpc(async move {
            if let Some(run_id) = active_run {
                // The engine admits steered inputs even across compaction
                // reservations now and reports the pending lane through
                // events; only a transport failure can strand the text, and
                // that is owed back to the composer.
                match client
                    .steer_run(RunSteerParams {
                        run_id,
                        input: input.clone(),
                    })
                    .await
                {
                    Ok(result) if !result.accepted => {
                        let error = result
                            .handled_reason
                            .unwrap_or_else(|| "steer request was rejected".into());
                        let _ = updates.send(RpcUpdate::SteerFailed {
                            session_id,
                            input,
                            error,
                        });
                    }
                    Ok(result) if result.handled_reason.is_some() => {
                        let _ = updates.send(RpcUpdate::Notice(
                            result.handled_reason.expect("reason is present"),
                        ));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = updates.send(RpcUpdate::SteerFailed {
                            session_id,
                            input,
                            error: error.to_string(),
                        });
                    }
                }
            } else {
                let result = client
                    .start_run(RunStartParams {
                        reset_fallback,
                        session_id,
                        client_run_id: submitted_id.clone(),
                        selection: selection.expect("draft selection checked"),
                        input: input.clone(),
                    })
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                let _ = updates.send(RpcUpdate::RunStartFinished {
                    session_id,
                    client_run_id: submitted_id,
                    draft_generation,
                    reset_fallback,
                    input,
                    result,
                });
            }
        });
    }

    /// The viewed session's pending steered inputs, when the lane is
    /// non-empty. The lane itself is a pure event reduction inside
    /// `SessionState`; the strip is only a projection of it.
    pub(super) fn selected_pending_inputs(&self) -> Option<&VecDeque<PendingInput>> {
        self.selected
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .map(|state| &state.pending_inputs)
            .filter(|pending| !pending.is_empty())
    }

    pub(in crate::ui) fn selected_queue_entries(&self) -> Vec<PendingQueueEntry> {
        use crate::state::ProducerMessageStatus;

        let Some(state) = self.selected.and_then(|id| self.store.sessions.get(&id)) else {
            return Vec::new();
        };
        let mut entries = state
            .pending_inputs
            .iter()
            .map(|input| PendingQueueEntry {
                kind: QueueEntryKind::User,
                seq: input.admission_seq,
                accepted_at: input.admitted_at,
                preview: input.text.clone(),
            })
            .collect::<Vec<_>>();
        for item in &state.transcript {
            let TranscriptItem::ProducerMessage {
                seq,
                accepted_at,
                message_id,
                producer_owner,
                mode,
                summary,
                status,
                ..
            } = item
            else {
                continue;
            };
            match status {
                ProducerMessageStatus::Pending | ProducerMessageStatus::Admitted => {}
                ProducerMessageStatus::Claimed
                | ProducerMessageStatus::Consumed
                | ProducerMessageStatus::Discarded => continue,
            };
            entries.push(PendingQueueEntry {
                kind: QueueEntryKind::Producer(*message_id),
                seq: *seq,
                accepted_at: *accepted_at,
                preview: crate::ui::transcript::producer_summary(
                    producer_owner,
                    *mode,
                    summary.as_deref(),
                ),
            });
        }
        entries.sort_by_key(|entry| entry.seq);
        entries
    }

    /// Strip height in rows for the selected session's pending lane: zero
    /// while empty so the layout never leaves a stray border behind.
    pub(in crate::ui) fn queue_strip_height(&self) -> u16 {
        let pending = self.selected_queue_entries();
        if pending.is_empty() {
            return 0;
        }
        // Visible entries plus block borders; the "+N more" folding row
        // shares the entry budget, so the cap never grows past it.
        (pending.len().min(MAX_VISIBLE_QUEUE_ROWS) as u16).saturating_add(2)
    }

    /// Render the pending-input strip between the conversation pane and the
    /// status line. Only user rows can recall input for editing.
    pub(in crate::ui) fn render_queue_strip(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.hit_map.queue_entries.clear();
        if area.height == 0 || area.width < 3 {
            return;
        }
        let pending = self.selected_queue_entries();
        if pending.is_empty() {
            return;
        }
        let oldest_age = jiff::Timestamp::now()
            .duration_since(pending[0].accepted_at)
            .as_secs()
            .max(0);
        let title = truncate_with_ellipsis(
            &format!("Pending · oldest {}", queue_age_label(oldest_age)),
            usize::from(
                area.width
                    .saturating_sub(2 + 2 * crate::ui::PANEL_TITLE_PAD),
            ),
        );
        let block = crate::ui::panel_block()
            .title(crate::ui::panel_title(Span::styled(
                title,
                self.theme.muted(),
            )))
            .border_style(self.theme.panel_border())
            .style(self.theme.panel());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let entry_rows = inner.height as usize;
        if entry_rows == 0 {
            return;
        }
        // Entries fill the budget; one row folds the remainder into
        // "+N more" so the strip never grows past its cap.
        let shown = if pending.len() > entry_rows {
            entry_rows.saturating_sub(1)
        } else {
            pending.len()
        };
        let overflow = pending.len() - shown;
        let mut lines = Vec::new();
        for (index, entry) in pending.iter().enumerate().take(shown) {
            let prefix =
                truncate_with_ellipsis(&format!("⏳ {} ", index + 1), usize::from(inner.width));
            let available =
                usize::from(inner.width).saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
            lines.push(Line::from(vec![
                Span::styled(prefix, self.theme.muted()),
                Span::styled(
                    if available == 0 {
                        String::new()
                    } else {
                        ellipsize_single_line(&entry.preview, available)
                    },
                    self.theme.muted(),
                ),
            ]));
        }
        if overflow > 0 {
            lines.push(Line::from(Span::styled(
                truncate_with_ellipsis(&format!("+{overflow} more"), usize::from(inner.width)),
                self.theme.muted(),
            )));
        }
        let line_count = lines.len();
        frame.render_widget(Paragraph::new(lines), inner);
        self.hit_map.queue_entries = (0..line_count)
            .map(|index| QueueEntryHit {
                rect: Rect::new(
                    inner.x,
                    inner.y.saturating_add(index as u16),
                    inner.width,
                    1,
                ),
                index,
                kind: if index < shown {
                    pending[index].kind
                } else if pending
                    .iter()
                    .all(|entry| entry.kind == QueueEntryKind::User)
                {
                    QueueEntryKind::User
                } else {
                    QueueEntryKind::Overflow
                },
            })
            .collect();
    }

    /// Copy text to the system clipboard via an OSC 52 escape written to
    /// the terminal: no platform dependency, and it works over SSH (the
    /// terminal emulator owns the clipboard, not the host). Terminals that
    /// ignore OSC 52 simply leave the clipboard untouched.
    pub(super) fn copy_to_clipboard(&mut self, text: String) {
        let characters = text.chars().count();
        let result = match &self.clipboard_sink {
            ClipboardSink::Osc52 => io::stdout()
                .lock()
                .write_all(osc52_sequence(&text).as_bytes())
                .and_then(|()| io::stdout().flush()),
            #[cfg(test)]
            ClipboardSink::Capture(copied) => {
                copied.lock().expect("clipboard capture").push(text);
                Ok(())
            }
        };
        self.status = match result {
            Ok(()) => format!("copied {characters} characters to the clipboard"),
            Err(error) => format!("clipboard write failed: {error}"),
        };
    }

    /// Prepend restored text to the composer, preserving FIFO order for
    /// multiple entries. Never called with an empty batch.
    pub(super) fn restore_composer_text(&mut self, texts: Vec<String>) {
        debug_assert!(!texts.is_empty());
        let mut restored = texts.join("\n");
        let existing = self.input.as_str();
        if !existing.is_empty() {
            restored.push('\n');
            restored.push_str(existing);
        }
        self.mutate_input(|input| input.set_buffer(restored));
        self.input_focused = true;
    }

    /// Restore any voided pending inputs of the viewed session into the
    /// composer: run-end casualties and recalls that resolved while another
    /// session was being viewed. The store holds them until this drain.
    pub(super) fn restore_voided_inputs(&mut self) {
        let Some(session_id) = self.selected else {
            return;
        };
        let texts = self.store.take_voided_inputs(session_id);
        if texts.is_empty() {
            return;
        }
        self.restore_composer_text(texts);
        self.status = "unsent message restored to the composer".into();
    }

    /// Recall the engine's newest pending steered input so its text returns
    /// to the composer for editing (`run.recall_steer`). Triggered by
    /// clicking a strip entry or pressing Up in an empty composer; the
    /// `UserInputRecalled` event removes the entry from the strip itself.
    pub(in crate::ui) fn recall_newest_pending(&mut self) {
        let Some(session_id) = self.selected else {
            return;
        };
        let Some(state) = self.store.sessions.get(&session_id) else {
            return;
        };
        let Some(run_id) = state.active_run else {
            self.status = "no active run to recall from".into();
            return;
        };
        if state.pending_inputs.is_empty() {
            self.status = "no pending message to recall".into();
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let update = match client.recall_steer(RunRecallSteerParams { run_id }).await {
                Ok(result) => match result.recalled {
                    Some(text) => {
                        let _ = updates.send(RpcUpdate::SteerRecalled { session_id, text });
                        return;
                    }
                    // The lane raced ahead (a promotion landed first):
                    // nothing is owed; the strip is already catching up.
                    None => RpcUpdate::Notice("nothing pending to recall".to_owned()),
                },
                Err(error) => RpcUpdate::Status(error.to_string()),
            };
            let _ = updates.send(update);
        });
    }

    /// Open the copy/revert/fork menu for a clicked user-message row. The
    /// message text is captured now: a rebuild (e.g. a concurrent revert)
    /// can change the transcript before the action runs.
    pub(super) fn open_user_menu(&mut self, hit: UserMessageHit) {
        let Some(session_id) = self.selected else {
            return;
        };
        let text = self.store.sessions.get(&session_id).and_then(|state| {
            state.transcript.iter().find_map(|item| match item {
                TranscriptItem::User { seq, text, .. } if *seq == hit.seq => Some(text.clone()),
                _ => None,
            })
        });
        let Some(text) = text else {
            self.status = "message is no longer in the visible branch".into();
            return;
        };
        self.user_menu = Some(UserMenuState {
            session_id,
            seq: hit.seq,
            text,
        });
        self.picker_state.select(Some(0));
        self.modal = Modal::UserMessage;
    }

    pub(super) async fn handle_user_menu_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                self.user_menu = None;
            }
            KeyCode::Up => {
                move_picker_selection(&mut self.picker_state, USER_MENU_ITEMS.len(), true);
            }
            KeyCode::Down => {
                move_picker_selection(&mut self.picker_state, USER_MENU_ITEMS.len(), false);
            }
            KeyCode::Enter => {
                self.choose_picker_entry(self.picker_state.selected().unwrap_or(0))
                    .await;
            }
            KeyCode::Char('c') => self.activate_user_menu_entry(0),
            KeyCode::Char('r') => self.activate_user_menu_entry(1),
            KeyCode::Char('f') => self.activate_user_menu_entry(2),
            _ => {}
        }
    }

    /// Run one menu row: copy, revert (behind its confirm guard), or fork.
    pub(super) fn activate_user_menu_entry(&mut self, index: usize) {
        let Some(menu) = self.user_menu.clone() else {
            self.modal = Modal::None;
            return;
        };
        match index {
            0 => {
                self.modal = Modal::None;
                self.user_menu = None;
                self.copy_to_clipboard(menu.text);
            }
            1 => self.modal = Modal::RevertConfirm,
            2 => {
                self.modal = Modal::None;
                self.user_menu = None;
                self.dispatch_session_fork(menu);
            }
            _ => {}
        }
    }

    pub(super) async fn handle_revert_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n' | 'N') => self.modal = Modal::UserMessage,
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                let Some(menu) = self.user_menu.take() else {
                    self.modal = Modal::None;
                    return;
                };
                self.modal = Modal::None;
                self.dispatch_session_revert(menu);
            }
            _ => {}
        }
    }

    /// Revert the session to just before the menu's message (`through_seq =
    /// seq - 1`), voiding it and every later turn from the visible branch.
    /// The physical log is append-only; the `SessionReverted` marker drives
    /// the transcript rebuild through the normal event flow. On success the
    /// message text restores into the composer for editing and resending.
    pub(super) fn dispatch_session_revert(&mut self, menu: UserMenuState) {
        // User messages always follow `SessionCreated` (sequence 1), so
        // `seq - 1` is a positive existing physical sequence.
        let through_seq = menu.seq.saturating_sub(1).max(1);
        let session_id = menu.session_id;
        let text = menu.text;
        self.status = "reverting to before the message…".into();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let message = match client
                .revert_session(SessionRevertParams {
                    session_id,
                    through_seq,
                })
                .await
            {
                Ok(result) => {
                    let text = result.instructions_override.unwrap_or(text);
                    let _ = updates.send(RpcUpdate::Reverted { session_id, text });
                    return;
                }
                Err(error) => format!("revert failed: {error}"),
            };
            let _ = updates.send(RpcUpdate::Status(message));
        });
    }

    /// Fork the session at the menu's message (`through_seq = seq`, keeping
    /// the message in the copied prefix), then switch to the new session.
    pub(super) fn dispatch_session_fork(&mut self, menu: UserMenuState) {
        let session_id = menu.session_id;
        self.status = "forking the session from the message…".into();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let message = match client
                .fork_session(SessionForkParams {
                    session_id,
                    through_seq: menu.seq,
                })
                .await
            {
                Ok(result) => {
                    let _ = updates.send(RpcUpdate::Forked {
                        forked: result.session_id,
                    });
                    return;
                }
                Err(error) => format!("fork failed: {error}"),
            };
            let _ = updates.send(RpcUpdate::Status(message));
        });
    }

    pub(super) fn render_user_menu(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let entries = USER_MENU_ITEMS
            .iter()
            .map(|(action, description)| format!("{action} — {description}"))
            .collect();
        self.render_picker(
            frame,
            "Message actions",
            entries,
            None,
            area,
            Some("↑↓ move · enter/c/r/f: run · esc: close"),
        );
    }

    pub(super) fn render_revert_confirm(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let Some(menu) = self.user_menu.as_ref() else {
            return;
        };
        let preview = ellipsize_single_line(&menu.text, 48);
        let content = format!(
            "Revert to before \"{preview}\"?\n\nThe message and every later turn leave the visible branch; the append-only log is kept. The message text returns to the composer for editing and resending.\n\nPress Enter/Y to revert or Esc/N to go back."
        );
        frame.render_widget(
            Paragraph::new(content).wrap(Wrap { trim: false }).block(
                crate::ui::panel_block()
                    .border_style(self.theme.panel_border())
                    .title(crate::ui::panel_title("Confirm revert")),
            ),
            area,
        );
    }

    pub async fn send_stdin(&mut self, input: String, eof: bool) {
        let Some((run_id, call_id)) = self.selected_running_tool() else {
            self.status = "no running interactive tool".into();
            return;
        };
        let data = (!input.is_empty()).then(|| STANDARD.encode(input.as_bytes()));
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let lane = {
            let mut lanes = self.stdin_lanes.lock().await;
            lanes
                .entry(call_id)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        self.spawn_rpc(async move {
            let _guard = lane.lock().await;
            let update = match tokio::time::timeout(
                STDIN_RPC_TIMEOUT,
                client.tool_stdin(RunToolStdinParams {
                    run_id,
                    call_id,
                    data,
                    eof,
                }),
            )
            .await
            {
                Err(_) => RpcUpdate::Status("stdin request timed out".into()),
                Ok(Ok(result)) if !result.accepted => {
                    RpcUpdate::Status("stdin was rejected by the tool".into())
                }
                Ok(Ok(_)) => {
                    if eof {
                        RpcUpdate::Notice("tool stdin closed".into())
                    } else {
                        RpcUpdate::Notice("stdin sent".into())
                    }
                }
                Ok(Err(error)) => RpcUpdate::Status(error.to_string()),
            };
            let _ = updates.send(update);
        });
    }

    pub(in crate::ui) async fn run_command(&mut self, command: SlashCommand) {
        match command {
            SlashCommand::Quit => self.should_quit = true,
            SlashCommand::ShowAgentPanel => {
                self.agent_panel_mode = AgentPanelMode::Shown;
                self.status = "agent panel shown; manual visibility override active".into();
            }
            SlashCommand::HideAgentPanel => {
                self.agent_panel_mode = AgentPanelMode::Hidden;
                self.status = "agent panel hidden; manual visibility override active".into();
            }
            SlashCommand::New => {
                if self.runtime.is_empty() {
                    self.status = EMPTY_RUNTIME_GUIDANCE.into();
                    return;
                }
                self.new_session_draft =
                    self.draft_selection_for_preset(self.selected_preset.as_deref(), None);
                self.open_selection_modal(Modal::Agents);
                if self.modal == Modal::Agents {
                    self.status = "Select the agent for the new root session.".into();
                }
            }
            SlashCommand::Preset => {
                self.modal = Modal::Presets;
                self.picker_state.select(Some(0));
                self.status =
                    "Select the preset for the next root run and future new sessions.".into();
            }
            SlashCommand::Connect => {
                self.clear_connect_secrets();
                self.modal = Modal::ConnectProviders;
                self.provider_search.reset();
                self.picker_state.select(Some(0));
                if self.providers.is_empty() {
                    self.status = "No providers are available in the runtime snapshot.".into();
                } else {
                    self.status = "Search providers, then press Down or Tab to choose one.".into();
                }
            }
            SlashCommand::Mcp => {
                self.modal = Modal::Mcp;
                self.mcp_panel.form = None;
                self.poll_mcp();
            }
            SlashCommand::Permissions => {
                if self.selected.is_none() {
                    self.status = "select a session before editing permissions".into();
                } else {
                    self.modal = Modal::Permissions;
                    self.permission_panel.begin_load();
                    self.load_permissions();
                }
            }
            SlashCommand::Skills => {
                let Some(session_id) = self.selected else {
                    self.status = "select a session before listing skills".into();
                    return;
                };
                self.modal = Modal::Skills;
                match self
                    .client
                    .list_skills(cookie_agent_protocol::SkillsListParams { session_id })
                    .await
                {
                    Ok(result) => {
                        self.skills = result.skills.clone();
                        self.skill_panel.install(result);
                        self.status = "skills loaded".into();
                    }
                    Err(error) => self.status = format!("list skills failed: {error}"),
                }
            }
            SlashCommand::Skill { name, args } => {
                let Some(session_id) = self.selected else {
                    self.status = "select a session before invoking a skill".into();
                    return;
                };
                match self
                    .client
                    .get_skill(cookie_agent_protocol::SkillsGetParams {
                        session_id,
                        name: name.clone(),
                        args: args.clone(),
                    })
                    .await
                {
                    Ok(result) if result.skill.user_invocable => {
                        self.submit_prompt(cookie_agent_protocol::encode_skill_submission(
                            &name, &args,
                        ))
                        .await;
                    }
                    Ok(_) => self.status = "skill is not user-invocable".into(),
                    Err(error) => self.status = format!("load skill failed: {error}"),
                }
            }
            SlashCommand::Usage => {
                self.modal = Modal::Usage;
                self.load_usage();
            }
            SlashCommand::Sessions => {
                self.modal = Modal::Sessions;
                self.session_search.reset();
                self.picker_state.select(Some(0));
            }
            SlashCommand::Cancel => self.cancel_active_run(),
            SlashCommand::Goal(command) => self.run_goal_command(command),
            SlashCommand::Compact(focus) => self.compact_selected_session(focus).await,
            SlashCommand::Approve(decision) => self.answer_approval(decision).await,
            SlashCommand::Events(level) => {
                // View-only threshold change: the TOML is not rewritten and
                // hidden rows stay in the session projection.
                self.tui_config.minimum_event_level = level;
                self.status = format!("diagnostic event filter: {}", level.name());
            }
            SlashCommand::Help => self.show_help(),
        }
    }

    pub(super) async fn compact_selected_session(&mut self, focus: Option<String>) {
        let Some(session_id) = self.selected else {
            self.status = "select a session before compacting context".into();
            return;
        };
        let focus = match focus.map(SafeDisplayText::new).transpose() {
            Ok(focus) => focus,
            Err(_) => {
                self.status = "compaction focus must be control-free and at most 1024 bytes".into();
                return;
            }
        };
        self.status = "compacting context…".into();
        self.status = match self
            .client
            .compact_session(SessionCompactParams { session_id, focus })
            .await
        {
            Ok(result) if result.compacted => "context compacted".into(),
            Ok(result) if result.cancellation_reason.is_some() => {
                result.cancellation_reason.expect("reason is present")
            }
            Ok(_) => "context did not require or could not produce a smaller checkpoint".into(),
            Err(error) => format!("context compaction failed: {error}"),
        };
    }

    pub(in crate::ui) fn show_help(&mut self) {
        // One line per command in the transcript; the status line stays a
        // short pointer instead of a truncated wall of text.
        let notice = std::iter::once("Available commands:".to_owned())
            .chain(
                COMMANDS
                    .iter()
                    .filter(|spec| self.command_is_available(spec))
                    .map(|spec| format!("{} — {}", spec.usage, spec.description)),
            )
            .chain(std::iter::once(
                "Use // to send a prompt beginning with /.".to_owned(),
            ))
            .collect::<Vec<_>>()
            .join("\n");
        self.status = "commands listed in the conversation".into();
        self.transient_notices.push(notice);
        if self.transient_notices.len() > MAX_TRANSIENT_NOTICES {
            let excess = self.transient_notices.len() - MAX_TRANSIENT_NOTICES;
            self.transient_notices.drain(..excess);
        }
    }
}
