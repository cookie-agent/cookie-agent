//! Key and pointer event dispatch for [`App`].

use super::*;

pub(super) fn is_printable_key(key: KeyEvent) -> bool {
    key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT
}

pub(super) fn edit_credential_input(input: &mut input::CredentialInput, key: KeyEvent) {
    match key.code {
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.move_left(),
        KeyCode::Right => input.move_right(),
        KeyCode::Home => input.move_buffer_home(),
        KeyCode::End => input.move_buffer_end(),
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => input.wipe(),
        KeyCode::Char(character) if is_printable_key(key) => input.insert(character),
        _ => {}
    }
}

pub(super) fn edit_plain_input(input: &mut InputState, key: KeyEvent) {
    match key.code {
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.move_left(),
        KeyCode::Right => input.move_right(),
        KeyCode::Home => input.move_buffer_home(),
        KeyCode::End => input.move_buffer_end(),
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            input.set_buffer(String::new())
        }
        KeyCode::Char(character) if is_printable_key(key) => input.insert(character),
        _ => {}
    }
}

pub(super) fn is_newline_key(key: KeyEvent) -> bool {
    matches!(
        (key.code, key.modifiers),
        (
            KeyCode::Enter,
            KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT
        ) | (KeyCode::Char('j'), KeyModifiers::CONTROL)
    )
}

impl App {
    pub(super) async fn handle_mcp_key(&mut self, key: KeyEvent) {
        if self.mcp_panel.form.is_some() {
            if key.code == KeyCode::Esc {
                self.mcp_panel.form = None;
                return;
            }
            if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                self.mcp_panel
                    .form
                    .as_mut()
                    .expect("form")
                    .move_focus(key.code == KeyCode::BackTab);
                return;
            }
            if matches!(
                key.code,
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
            ) && self.mcp_panel.form.as_ref().is_some_and(|form| {
                matches!(
                    form.focus,
                    McpFormFocus::Transport
                        | McpFormFocus::Enabled
                        | McpFormFocus::Lazy
                        | McpFormFocus::Persist
                )
            }) {
                self.mcp_panel
                    .form
                    .as_mut()
                    .expect("form")
                    .cycle_choice(key.code == KeyCode::Left);
                return;
            }
            if key.code == KeyCode::Enter {
                self.submit_mcp_form();
                return;
            }
            if let Some(input) = self
                .mcp_panel
                .form
                .as_mut()
                .and_then(McpForm::focused_input)
            {
                edit_plain_input(input, key);
            }
            return;
        }
        if let Some(auth) = self.mcp_panel.auth.as_ref() {
            match key.code {
                KeyCode::Esc => self.dispatch_mcp_auth_cancel(auth.server.clone()),
                KeyCode::Char('c') => {
                    self.copy_to_clipboard(auth.authorization_url.clone());
                }
                _ => {}
            }
            return;
        }
        let count = self.mcp_panel.servers.len();
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Up => move_picker_selection(&mut self.mcp_panel.selection, count, true),
            KeyCode::Down => move_picker_selection(&mut self.mcp_panel.selection, count, false),
            KeyCode::Char('n') => self.mcp_panel.form = Some(McpForm::add()),
            KeyCode::Char('e') => {
                if let Some(server) = self.mcp_panel.selected().cloned() {
                    self.mcp_panel.form = Some(McpForm::edit(&server));
                }
            }
            KeyCode::Char('d') => {
                if let Some(name) = self.mcp_panel.selected().map(|server| server.name.clone()) {
                    self.dispatch_mcp_remove(name);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(server) = self.mcp_panel.selected().cloned() {
                    self.dispatch_mcp_toggle(server.name, !server.definition.enabled);
                }
            }
            KeyCode::Char('r') => {
                if let Some(name) = self.mcp_panel.selected().map(|server| server.name.clone()) {
                    self.dispatch_mcp_reconnect(name);
                }
            }
            KeyCode::Char('a') => {
                if let Some(server) = self.mcp_panel.selected().cloned()
                    && server.state == McpServerState::NeedsAuth
                {
                    self.dispatch_mcp_auth_begin(server.name);
                }
            }
            _ => {}
        }
    }

    pub(super) fn submit_mcp_form(&mut self) {
        let Some(form) = self.mcp_panel.form.take() else {
            return;
        };
        let persist = form.persist.target();
        let editing = form.editing;
        let original = form.original_name.clone();
        let (name, definition) = match form.definition() {
            Ok(value) => value,
            Err(error) => {
                self.status = error;
                self.mcp_panel.form = Some(form);
                return;
            }
        };
        if editing && original.as_deref() != Some(name.as_str()) {
            self.status = "editing cannot rename a server; remove it and add the new name".into();
            self.mcp_panel.form = Some(form);
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = async {
                let mutation = if editing {
                    client
                        .edit_mcp_server(McpServerEditParams {
                            name: name.clone(),
                            definition,
                        })
                        .await
                } else {
                    client
                        .add_mcp_server(McpServerAddParams {
                            name: name.clone(),
                            definition,
                        })
                        .await
                }
                .map_err(|error| error.to_string())?;
                if let Some(target) = persist {
                    return client
                        .persist_mcp_server(McpServerPersistParams { name, target })
                        .await
                        .map(|result| result.server)
                        .map_err(|error| error.to_string());
                }
                Ok(mutation.server)
            }
            .await;
            let _ = updates.send(RpcUpdate::McpMutation {
                result: Box::new(result),
            });
        });
    }

    pub(super) fn dispatch_mcp_remove(&self, name: String) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .remove_mcp_server(McpServerNameParams { name })
                .await
                .map(|result| result.server)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpMutation {
                result: Box::new(result),
            });
        });
    }

    pub(super) fn dispatch_mcp_toggle(&self, name: String, enabled: bool) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .set_mcp_server_enabled(McpServerSetEnabledParams { name, enabled })
                .await
                .map(|result| result.server)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpMutation {
                result: Box::new(result),
            });
        });
    }

    pub(super) fn dispatch_mcp_reconnect(&self, name: String) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .reconnect_mcp_server(McpServerNameParams { name })
                .await
                .map(|result| result.server)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpMutation {
                result: Box::new(result),
            });
        });
    }

    pub(super) fn dispatch_mcp_auth_begin(&self, server: String) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .begin_mcp_auth(McpAuthBeginParams { server })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpAuthBegan { result });
        });
    }

    pub(super) fn dispatch_mcp_auth_cancel(&self, server: String) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .cancel_mcp_auth(McpAuthCancelParams {
                    server: server.clone(),
                })
                .await
                .map(|_| server)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpAuthCancelled { result });
        });
    }

    pub(super) async fn handle_permissions_key(&mut self, key: KeyEvent) {
        if let Some(form) = &mut self.permission_panel.form {
            match key.code {
                KeyCode::Esc => self.permission_panel.form = None,
                KeyCode::Tab | KeyCode::BackTab => form.focus_pattern = !form.focus_pattern,
                KeyCode::Up if !form.focus_pattern => form.cycle_action(true),
                KeyCode::Down if !form.focus_pattern => form.cycle_action(false),
                KeyCode::Left if !form.focus_pattern => {
                    form.effect = cycle_effect(form.effect, true)
                }
                KeyCode::Right | KeyCode::Char(' ') if !form.focus_pattern => {
                    form.effect = cycle_effect(form.effect, false)
                }
                KeyCode::Enter => self.submit_permission_form(),
                _ if form.focus_pattern => edit_plain_input(&mut form.pattern, key),
                _ => {}
            }
            return;
        }
        let rows = self.permission_panel.rows();
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Up => {
                move_picker_selection(&mut self.permission_panel.selection, rows.len(), true)
            }
            KeyCode::Down => {
                move_picker_selection(&mut self.permission_panel.selection, rows.len(), false)
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                if let Some(row) = self.permission_panel.selected() {
                    let effect = cycle_effect(row.effect, key.code == KeyCode::Left);
                    self.dispatch_permission_set(row.action, row.resource, effect);
                }
            }
            KeyCode::Char('n') => {
                let action = self
                    .permission_panel
                    .selected()
                    .map_or(PermissionAction::Read, |row| row.action);
                self.permission_panel.form = Some(PermissionForm::new(action));
            }
            KeyCode::Char('d') => {
                if let Some(row) = self.permission_panel.selected() {
                    if row.source == PermissionRuleSource::SessionOverlay {
                        self.dispatch_permission_clear(row.action, row.resource);
                    } else {
                        self.status = "only session overlay rules can be cleared".into();
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn submit_permission_form(&mut self) {
        let Some(form) = self.permission_panel.form.take() else {
            return;
        };
        let pattern = form.pattern.as_str().trim();
        let resource = match cookie_agent_protocol::WildcardPattern::new(pattern) {
            Ok(resource) => resource.to_string(),
            Err(error) => {
                self.status = format!("invalid permission pattern: {error}");
                self.permission_panel.form = Some(form);
                return;
            }
        };
        self.dispatch_permission_set(form.action, resource, form.effect);
    }

    pub(super) fn dispatch_permission_set(
        &self,
        action: PermissionAction,
        resource: String,
        effect: PermissionEffect,
    ) {
        let Some(session_id) = self.selected else {
            return;
        };
        let Ok(resource) = cookie_agent_protocol::WildcardPattern::new(resource) else {
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .set_session_permission(SessionPermissionSetParams {
                    session_id,
                    action,
                    resource,
                    effect,
                })
                .await
                .map(|result| SessionPermissionGetResult {
                    permissions: result.permissions,
                    current_mode: None,
                })
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PermissionsLoaded { session_id, result });
        });
    }

    pub(super) fn dispatch_permission_clear(&self, action: PermissionAction, resource: String) {
        let Some(session_id) = self.selected else {
            return;
        };
        let Ok(resource) = cookie_agent_protocol::WildcardPattern::new(resource) else {
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .clear_session_permission(SessionPermissionClearParams {
                    session_id,
                    action,
                    resource,
                })
                .await
                .map(|result| SessionPermissionGetResult {
                    permissions: result.permissions,
                    current_mode: None,
                })
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PermissionsLoaded { session_id, result });
        });
    }

    pub(in crate::ui) async fn handle_key(&mut self, key: KeyEvent) {
        if key.code != KeyCode::Esc {
            self.last_escape = None;
        }
        if !(key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
            self.last_quit_press = None;
        }
        if self.modal == Modal::None
            && self.current_approval().is_none()
            && !self.command_palette_visible()
        {
            if !self.goal_bar_visible() {
                self.goal_focus = None;
            } else if key.code == KeyCode::F(6) {
                self.goal_focus = if self.goal_focus.is_some() {
                    None
                } else {
                    self.status = "Goal details".into();
                    Some(GoalBarAction::Details)
                };
                return;
            }
            if let Some(action) = self.goal_focus {
                match key.code {
                    KeyCode::Esc => {
                        self.goal_focus = None;
                        self.last_escape = None;
                        return;
                    }
                    KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                        self.cycle_goal_focus(
                            matches!(key.code, KeyCode::BackTab | KeyCode::Left)
                                || key.modifiers.contains(KeyModifiers::SHIFT),
                        );
                        return;
                    }
                    KeyCode::Enter => {
                        self.activate_goal_action(action);
                        return;
                    }
                    _ => {
                        self.goal_focus = None;
                    }
                }
            }
        }
        match self.modal {
            Modal::GoalDetail => self.handle_goal_detail_key(key),
            Modal::Sessions => self.handle_session_picker(key).await,
            Modal::Presets => self.handle_selection_picker(key).await,
            Modal::Agents => self.handle_agent_picker_key(key).await,
            Modal::Models => self.handle_model_picker_key(key).await,
            Modal::Variants => self.handle_variant_picker_key(key).await,
            Modal::ConnectProviders => self.handle_connect_provider_key(key),
            Modal::ConnectDetails => self.handle_connect_details_key(key),
            Modal::ConnectSetup => self.handle_connect_setup_key(key),
            Modal::ConnectError => self.handle_connect_error_key(key),
            Modal::DisconnectConfirm => self.handle_disconnect_confirm_key(key),
            Modal::UserMessage => self.handle_user_menu_key(key).await,
            Modal::RevertConfirm => self.handle_revert_confirm_key(key).await,
            Modal::Mcp => self.handle_mcp_key(key).await,
            Modal::Permissions => self.handle_permissions_key(key).await,
            Modal::Usage => match key.code {
                KeyCode::Esc => self.modal = Modal::None,
                KeyCode::Up => self.usage_panel.scroll_up(1),
                KeyCode::Down => self.usage_panel.scroll_down(1),
                KeyCode::PageUp => self.usage_panel.page_up(),
                KeyCode::PageDown => self.usage_panel.page_down(),
                _ => {}
            },
            Modal::None
                if self.current_approval().is_none()
                    && !self.command_palette_visible()
                    && agent_cycle_backward(key).is_some() =>
            {
                self.cycle_agent(agent_cycle_backward(key).expect("agent cycle key"));
            }
            Modal::None
                if self.current_approval().is_none()
                    && !self.command_palette_visible()
                    && key.code == KeyCode::Char('t')
                    && key.modifiers == KeyModifiers::CONTROL =>
            {
                // Ctrl-T cycles the draft model's variants, like clicking the
                // bracketed variant suffix in the composer title.
                if self.draft_variants().len() > 1 {
                    self.cycle_draft_variant();
                } else {
                    self.status = "this model has no other variants".into();
                }
            }
            Modal::None if self.command_palette_visible() => self.handle_palette_key(key).await,
            Modal::None if self.current_approval().is_some() => self.handle_approval_key(key).await,
            Modal::None if key.code == KeyCode::Esc && self.selection.is_some() => {
                // Esc retires a selection before it ever counts toward the
                // double-Esc run cancel.
                self.selection = None;
                self.last_escape = None;
            }
            Modal::None if key.code == KeyCode::Esc => {
                if self.register_escape(Instant::now()) {
                    self.cancel_active_run();
                }
            }
            Modal::None
                if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // With an active selection ctrl+c is copy; without one it
                // cancels the active run, and with nothing to interrupt a
                // second press inside the quit window exits.
                if let Some(text) = self.selected_text() {
                    self.selection = None;
                    self.last_quit_press = None;
                    if text.is_empty() {
                        self.status = "nothing to copy in the selection".into();
                    } else {
                        self.copy_to_clipboard(text);
                    }
                } else if self.selected_active_run().is_some() {
                    self.last_quit_press = None;
                    self.cancel_active_run();
                } else if self.register_quit_press(Instant::now()) {
                    self.should_quit = true;
                } else {
                    self.status = "press ctrl+c again to quit".into();
                }
            }
            Modal::None
                if key.code == KeyCode::Char('x')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(self.selection, Some(TextSelection::Composer { .. })) =>
            {
                // Composer-only cut: copy the selected draft text, then
                // remove it from the buffer.
                if let Some(text) = self.selected_text()
                    && !text.is_empty()
                {
                    let (start, end) = self
                        .selection
                        .map(|selection| selection.byte_range())
                        .unwrap_or((0, 0));
                    self.selection = None;
                    self.copy_to_clipboard(text);
                    self.input.delete_byte_range(start, end);
                }
            }
            Modal::None => self.handle_input_key(key).await,
        }
    }

    /// Handle one mouse event. Returns whether a redraw is needed: every
    /// button/wheel event redraws, while pointer motion redraws only when it
    /// actually changed the hovered element.
    pub(in crate::ui) async fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_press(mouse.column, mouse.row).await;
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.handle_pointer_drag(mouse.column, mouse.row);
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.handle_release().await;
                true
            }
            MouseEventKind::ScrollUp => {
                self.handle_wheel(mouse.column, mouse.row, true);
                true
            }
            MouseEventKind::ScrollDown => {
                self.handle_wheel(mouse.column, mouse.row, false);
                true
            }
            MouseEventKind::Moved => self.update_hover(mouse.column, mouse.row),
            _ => false,
        }
    }

    /// Left-button press: a plain click anywhere clears a finished
    /// selection. Presses inside the conversation viewport or the composer
    /// text rect (with no overlay owning the pointer) are held as pending
    /// presses until motion decides click-or-drag; every other press is an
    /// immediate click, preserving scrollbar capture and panel behavior.
    pub(super) async fn handle_press(&mut self, column: u16, row: u16) {
        self.selection = None;
        self.pending_press = None;
        // Overlay ownership comes from state, not hit geometry: a modal or
        // approval that opened since the last frame still owns its presses.
        let overlay = self.command_palette_visible()
            || self.modal != Modal::None
            || self.current_approval().is_some();
        if !overlay {
            if self
                .hit_map
                .conversation
                .is_some_and(|viewport| contains(viewport, column, row))
            {
                self.pending_press = Some(PendingPress {
                    column,
                    row,
                    target: PressTarget::Conversation,
                });
                return;
            }
            if let Some(hit) = self
                .hit_map
                .input
                .filter(|hit| contains(hit.rect, column, row))
            {
                // The composer's scrollbar column keeps its immediate
                // behavior (page/thumb capture); only text cells can start
                // a selection.
                let on_scrollbar = hit
                    .scrollbar
                    .is_some_and(|geometry| contains(geometry.track, column, row));
                if !on_scrollbar && contains(hit.text_rect, column, row) {
                    self.pending_press = Some(PendingPress {
                        column,
                        row,
                        target: PressTarget::Composer,
                    });
                    return;
                }
            }
        }
        self.handle_click(column, row).await;
    }

    /// Pointer motion with the button held: scrollbar drags stay scrollbar
    /// drags; a pending press moved past the threshold becomes a selection
    /// whose head follows the pointer.
    pub(super) fn handle_pointer_drag(&mut self, column: u16, row: u16) {
        if self.scrollbar_drag.is_some() {
            self.handle_drag(column, row);
            return;
        }
        let Some(press) = self.pending_press else {
            return;
        };
        match press.target {
            PressTarget::Conversation => {
                let Some(viewport) = self.hit_map.conversation else {
                    self.pending_press = None;
                    return;
                };
                let point = self.conversation_point(viewport, column, row);
                if let Some(TextSelection::Conversation { head, .. }) = &mut self.selection {
                    *head = point;
                    return;
                }
                if column.abs_diff(press.column) > DRAG_THRESHOLD_CELLS
                    || row.abs_diff(press.row) > DRAG_THRESHOLD_CELLS
                {
                    let anchor = self.conversation_point(viewport, press.column, press.row);
                    self.selection = Some(TextSelection::Conversation {
                        anchor,
                        head: point,
                    });
                }
            }
            PressTarget::Composer => {
                let Some(hit) = self.hit_map.input else {
                    self.pending_press = None;
                    return;
                };
                let point = self.composer_point(hit, column, row);
                if let Some(TextSelection::Composer { head, .. }) = &mut self.selection {
                    *head = point;
                    return;
                }
                if column.abs_diff(press.column) > DRAG_THRESHOLD_CELLS
                    || row.abs_diff(press.row) > DRAG_THRESHOLD_CELLS
                {
                    let anchor = self.composer_point(hit, press.column, press.row);
                    self.selection = Some(TextSelection::Composer {
                        anchor,
                        head: point,
                    });
                }
            }
        }
    }

    /// Button release: a finished drag keeps its selection; a pending press
    /// that never became a drag dispatches its click at the press position.
    pub(super) async fn handle_release(&mut self) {
        self.scrollbar_drag = None;
        if self.selection.is_some() {
            self.pending_press = None;
            return;
        }
        let Some(press) = self.pending_press.take() else {
            return;
        };
        self.handle_click(press.column, press.row).await;
    }

    /// A conversation cell in content coordinates: `(logical line, display
    /// column)` inside the rendered lines, so the selection survives
    /// scrolling while it is held.
    pub(super) fn conversation_point(&self, viewport: Rect, column: u16, row: u16) -> (usize, u16) {
        let line = self
            .conversation_scroll
            .offset
            .saturating_add(usize::from(row.saturating_sub(viewport.y)));
        let column = column.saturating_sub(viewport.x).min(viewport.width);
        (line, column)
    }

    /// A composer cell as the nearest draft byte offset.
    pub(super) fn composer_point(&self, hit: InputHit, column: u16, row: u16) -> usize {
        self.input.byte_at_display_position(
            row.saturating_sub(hit.text_rect.y)
                .min(hit.text_rect.height.saturating_sub(1)),
            column
                .saturating_sub(hit.text_rect.x)
                .min(hit.text_rect.width),
        )
    }

    /// Recompute the hovered element from the current hit map. Returns true
    /// when the hover target changed (the only case needing a redraw). A
    /// captured scrollbar drag freezes hover until the press is released.
    pub(super) fn update_hover(&mut self, column: u16, row: u16) -> bool {
        if self.scrollbar_drag.is_some() {
            return false;
        }
        let next = self.hover_target_at(column, row);
        if next == self.hover {
            return false;
        }
        self.hover = next;
        match next {
            Some(HoverTarget::GoalAction(action)) => {
                self.status = format!("Goal: {}", goal::goal_action_label(action));
            }
            Some(HoverTarget::GoalClose) => self.status = "Close goal details".into(),
            _ => {}
        }
        true
    }

    /// Resolve the interactive element under a point using the exact same
    /// state-owned overlay priority as click handling: an open overlay owns
    /// the pointer even before its geometry renders.
    pub(in crate::ui) fn hover_target_at(&self, column: u16, row: u16) -> Option<HoverTarget> {
        let over = |rect: Rect| contains(rect, column, row);
        if self.command_palette_visible() {
            return self
                .hit_map
                .palette_rows
                .iter()
                .find(|hit| over(hit.rect))
                .map(|hit| HoverTarget::PaletteRow(hit.index));
        }
        if self.modal != Modal::None {
            if self.modal == Modal::GoalDetail {
                return self
                    .hit_map
                    .goal_close
                    .filter(|rect| over(*rect))
                    .map(|_| HoverTarget::GoalClose);
            }
            if self.hit_map.provider_submit.is_some_and(over) {
                return Some(HoverTarget::ProviderSubmit);
            }
            if self.hit_map.provider_cancel.is_some_and(over) {
                return Some(HoverTarget::ProviderCancel);
            }
            if let Some(hit) = self
                .hit_map
                .provider_fields
                .iter()
                .find(|hit| over(hit.rect))
            {
                return Some(HoverTarget::ProviderField(hit.focus));
            }
            return self
                .hit_map
                .picker_rows
                .iter()
                .find(|hit| over(hit.rect))
                .map(|hit| HoverTarget::PickerRow(hit.index));
        }
        if self.current_approval().is_some() {
            return self
                .hit_map
                .approval_actions
                .iter()
                .find(|hit| over(hit.rect))
                .map(|hit| HoverTarget::ApprovalAction(hit.decision));
        }
        if self.goal_bar_visible()
            && let Some((_, action)) = self
                .hit_map
                .goal_actions
                .iter()
                .find(|(rect, _)| over(*rect))
        {
            return Some(HoverTarget::GoalAction(*action));
        }
        if self.hit_map.permission_mode.is_some_and(over) {
            return Some(HoverTarget::PermissionMode);
        }
        if self.hit_map.session_cost.is_some_and(over) {
            return Some(HoverTarget::SessionCost);
        }
        if self.hit_map.event_level_filter.is_some_and(over) {
            return Some(HoverTarget::EventLevelFilter);
        }
        if let Some(hit) = self
            .hit_map
            .title_segments
            .iter()
            .find(|hit| over(hit.rect))
        {
            return Some(HoverTarget::TitleSegment(hit.segment));
        }
        if let Some(hit) = self
            .hit_map
            .queue_entries
            .iter()
            .find(|hit| hit.kind == QueueEntryKind::User && over(hit.rect))
        {
            return Some(HoverTarget::QueueEntry(hit.index));
        }
        if let Some(hit) = self.hit_map.tree_rows.iter().find(|hit| over(hit.rect)) {
            return Some(HoverTarget::TreeRow(hit.session_id));
        }
        if self.hit_map.conversation.is_some_and(over) {
            return self
                .hit_map
                .blocks
                .iter()
                .rev()
                .find(|hit| hit.toggle_rect.is_some_and(over))
                .map(|hit| HoverTarget::TranscriptBlock(hit.id));
        }
        None
    }

    /// Patch the hover affordance onto the resolved target's cells. Text
    /// targets get the glaze text style; approval buttons get the fill-only
    /// variant so their glyphs stay put.
    /// Patch the mouse text selection over the already-rendered cells,
    /// exactly like hover: a pure style pass that can never change layout.
    /// The selection background leaves cell foregrounds (code highlighting)
    /// intact, and keyboard selection keeps its own distinct style.
    pub(super) fn apply_selection(&self, frame: &mut ratatui::Frame) {
        let Some(selection) = self.selection else {
            return;
        };
        let style = self.theme.text_selection();
        match selection {
            TextSelection::Conversation { .. } => {
                let (start, end) = selection.ordered();
                let Some(viewport) = self.hit_map.conversation else {
                    return;
                };
                let offset = self.conversation_scroll.offset;
                for line in start.0..=end.0 {
                    let Some(row_in_view) = line.checked_sub(offset) else {
                        continue;
                    };
                    if row_in_view >= usize::from(viewport.height) {
                        break;
                    }
                    let y = viewport.y.saturating_add(row_in_view as u16);
                    let column_start = if line == start.0 { start.1 } else { 0 };
                    let column_end = if line == end.0 { end.1 } else { viewport.width };
                    let column_start = column_start.min(viewport.width);
                    let column_end = column_end.min(viewport.width);
                    for x in column_start..column_end {
                        let cell = &mut frame.buffer_mut()[(viewport.x.saturating_add(x), y)];
                        cell.set_style(style);
                    }
                }
            }
            TextSelection::Composer { .. } => {
                let (start, end) = selection.byte_range();
                let Some(hit) = self.hit_map.input else {
                    return;
                };
                for (row, column_start, column_end) in self.input.selection_cells(start, end) {
                    if row >= hit.text_rect.height {
                        continue;
                    }
                    let y = hit.text_rect.y.saturating_add(row);
                    let column_end = column_end.min(hit.text_rect.width);
                    for x in column_start..column_end {
                        let cell = &mut frame.buffer_mut()[(hit.text_rect.x.saturating_add(x), y)];
                        cell.set_style(style);
                    }
                }
            }
        }
    }

    pub(super) fn apply_hover(&self, frame: &mut ratatui::Frame) {
        let Some(hover) = self.hover else {
            return;
        };
        let text_style = self.theme.hover();
        let fill_style = self.theme.hover_fill();
        let patch = |frame: &mut ratatui::Frame, rect: Rect, style: ratatui::style::Style| {
            for y in rect.y..rect.y.saturating_add(rect.height) {
                for x in rect.x..rect.x.saturating_add(rect.width) {
                    if frame.area().contains(Position::new(x, y)) {
                        let cell = &mut frame.buffer_mut()[(x, y)];
                        cell.set_style(style);
                    }
                }
            }
        };
        match hover {
            HoverTarget::TranscriptBlock(id) => {
                // The block's own content rect — clamped past its leading
                // gutter by `block_hit`, then intersected with the conversation
                // viewport so no highlight can reach the reserved scrollbar
                // columns — is the paint target; the wider hit rect stays the
                // click/toggle target.
                let rect = self
                    .hit_map
                    .blocks
                    .iter()
                    .find(|hit| hit.id == id)
                    .and_then(|hit| hit.hover_rect.map(|rect| rect.intersection(hit.rect)))
                    .map(|rect| match self.hit_map.conversation {
                        Some(viewport) => rect.intersection(viewport),
                        None => rect,
                    });
                if let Some(rect) = rect {
                    patch(frame, rect, self.theme.block_hover());
                }
            }
            HoverTarget::GoalAction(action) => {
                if let Some((rect, _)) = self
                    .hit_map
                    .goal_actions
                    .iter()
                    .find(|(_, candidate)| *candidate == action)
                {
                    let style = if action == GoalBarAction::Details {
                        self.theme.block_hover()
                    } else {
                        text_style
                    };
                    patch(frame, *rect, style);
                }
            }
            HoverTarget::GoalClose => {
                if let Some(rect) = self.hit_map.goal_close {
                    patch(frame, rect, text_style);
                }
            }
            HoverTarget::PaletteRow(index) => {
                if let Some(hit) = self
                    .hit_map
                    .palette_rows
                    .iter()
                    .find(|hit| hit.index == index)
                {
                    patch(frame, hit.rect, text_style);
                }
            }
            HoverTarget::PickerRow(index) => {
                if let Some(hit) = self
                    .hit_map
                    .picker_rows
                    .iter()
                    .find(|hit| hit.index == index)
                {
                    patch(frame, hit.rect, text_style);
                }
            }
            HoverTarget::ProviderField(focus) => {
                if let Some(hit) = self
                    .hit_map
                    .provider_fields
                    .iter()
                    .find(|hit| hit.focus == focus)
                {
                    patch(frame, hit.text_rect, fill_style);
                }
            }
            HoverTarget::ProviderSubmit => {
                if let Some(rect) = self.hit_map.provider_submit {
                    patch(frame, rect, fill_style);
                }
            }
            HoverTarget::ProviderCancel => {
                if let Some(rect) = self.hit_map.provider_cancel {
                    patch(frame, rect, fill_style);
                }
            }
            HoverTarget::ApprovalAction(decision) => {
                if let Some(hit) = self
                    .hit_map
                    .approval_actions
                    .iter()
                    .find(|hit| hit.decision == decision)
                {
                    patch(frame, hit.rect, fill_style);
                }
            }
            HoverTarget::TitleSegment(segment) => {
                if let Some(hit) = self
                    .hit_map
                    .title_segments
                    .iter()
                    .find(|hit| hit.segment == segment)
                {
                    patch(frame, hit.rect, text_style);
                }
            }
            HoverTarget::PermissionMode => {
                if let Some(rect) = self.hit_map.permission_mode {
                    patch(frame, rect, text_style);
                }
            }
            HoverTarget::SessionCost => {
                if let Some(rect) = self.hit_map.session_cost {
                    patch(frame, rect, text_style);
                }
            }
            HoverTarget::EventLevelFilter => {
                if let Some(rect) = self.hit_map.event_level_filter {
                    patch(frame, rect, text_style);
                }
            }
            HoverTarget::QueueEntry(index) => {
                if let Some(hit) = self
                    .hit_map
                    .queue_entries
                    .iter()
                    .find(|hit| hit.index == index && hit.kind == QueueEntryKind::User)
                {
                    patch(frame, hit.rect, text_style);
                }
            }
            HoverTarget::TreeRow(session_id) => {
                if let Some(hit) = self
                    .hit_map
                    .tree_rows
                    .iter()
                    .find(|hit| hit.session_id == session_id)
                {
                    patch(frame, hit.rect, text_style);
                }
            }
        }
    }

    /// The animation bucket (0–3) for the streaming "thinking…" ellipsis:
    /// one step per twelve 33ms frames ≈ 400ms.
    pub(in crate::ui) fn clock_bucket(&self) -> u8 {
        u8::try_from((self.animation_ticks / 12) % 4).unwrap_or(0)
    }

    /// Animate live runs, streaming parts, tools, and waiting producers.
    pub(in crate::ui) fn animation_active(&self) -> bool {
        self.selected
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .is_some_and(|state| {
                state.active_run.is_some()
                    || crate::state::SessionState::has_open_thinking(state)
                    || crate::state::SessionState::has_running_tool(state)
                    || state.has_pending_producers()
            })
    }

    pub(super) fn working_state(&self) -> Option<WorkingState> {
        let state = self.selected.and_then(|id| self.store.sessions.get(&id))?;
        if state.active_run.is_some() {
            return Some(WorkingState::Working);
        }
        let count = state.pending_inputs.len()
            + state
                .transcript
                .iter()
                .filter(|item| {
                    matches!(
                        item,
                        TranscriptItem::ProducerMessage {
                            status: crate::state::ProducerMessageStatus::Pending
                                | crate::state::ProducerMessageStatus::Admitted,
                            ..
                        }
                    )
                })
                .count();
        (count > 0).then_some(WorkingState::Queued(count))
    }

    pub(in crate::ui) fn animation_tick(&mut self) {
        self.animation_ticks = self.animation_ticks.wrapping_add(1);
    }

    /// A captured scrollbar thumb drag keeps its grab anchor and resolves
    /// against the original geometry even when the pointer leaves the track.
    pub(super) fn handle_drag(&mut self, column: u16, row: u16) {
        let Some(drag) = self.scrollbar_drag else {
            return;
        };
        let _ = column;
        match drag.target {
            ScrollbarTarget::Conversation => {
                let Some(geometry) = self.scrollbar_geometry else {
                    self.scrollbar_drag = None;
                    return;
                };
                let offset = geometry.offset_for_thumb_anchor(row, drag.grab_row);
                self.conversation_scroll
                    .scroll_to(geometry.clamp_offset(offset));
            }
            ScrollbarTarget::Input => {
                let Some(geometry) = self.hit_map.input.and_then(|hit| hit.scrollbar) else {
                    self.scrollbar_drag = None;
                    return;
                };
                let offset = geometry.offset_for_thumb_anchor(row, drag.grab_row);
                self.input.scroll_to(geometry.clamp_offset(offset));
            }
        }
    }

    pub(in crate::ui) async fn handle_click(&mut self, column: u16, row: u16) {
        // Overlay ownership comes from current state, never from hit-map
        // geometry: an overlay that opened since the last frame still owns
        // its presses (its geometry may not exist yet), and one that just
        // closed leaves content geometry underneath intact and clickable.
        // Each overlay branch therefore always returns — geometry only
        // picks the element within the panel, never whether the panel owns
        // the click.
        if self.command_palette_visible() {
            if let Some(hit) = self
                .hit_map
                .palette_rows
                .iter()
                .find(|hit| contains(hit.rect, column, row))
                .copied()
            {
                self.activate_palette_row(hit.index).await;
            }
            return;
        }
        if self.modal != Modal::None {
            if self.modal == Modal::GoalDetail {
                if self
                    .hit_map
                    .goal_close
                    .is_some_and(|rect| contains(rect, column, row))
                {
                    self.handle_goal_detail_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                }
                return;
            }
            if let Some(hit) = self
                .hit_map
                .picker_input
                .filter(|hit| contains(hit.rect, column, row))
            {
                let search = match self.modal {
                    Modal::Sessions => &mut self.session_search,
                    Modal::Agents => &mut self.agent_search,
                    Modal::Models => &mut self.model_search,
                    Modal::ConnectProviders => &mut self.provider_search,
                    _ => return,
                };
                search.focus_input();
                search.input_mut().set_cursor_from_display_position(
                    row.saturating_sub(hit.text_rect.y)
                        .min(hit.text_rect.height.saturating_sub(1)),
                    column
                        .saturating_sub(hit.text_rect.x)
                        .min(hit.text_rect.width),
                );
                return;
            }
            if let Some(hit) = self
                .hit_map
                .picker_rows
                .iter()
                .find(|hit| contains(hit.rect, column, row))
                .copied()
            {
                self.choose_picker_entry(hit.index).await;
            }
            if self
                .hit_map
                .provider_submit
                .is_some_and(|rect| contains(rect, column, row))
            {
                self.dispatch_provider_connect();
                return;
            }
            if self
                .hit_map
                .provider_cancel
                .is_some_and(|rect| contains(rect, column, row))
            {
                self.cancel_connect_form();
                return;
            }
            if let Some(hit) = self
                .hit_map
                .provider_fields
                .iter()
                .find(|hit| contains(hit.rect, column, row))
                .copied()
            {
                if hit.focus == ProviderFormFocus::AuthMethod {
                    // Clicking the selector mirrors pressing Enter on it:
                    // cycle to the next method, wiping its stale secrets.
                    if let Some(form) = self.provider_form.as_mut() {
                        form.cycle_auth_method(false);
                    }
                } else {
                    self.focus_provider_field(hit, column, row);
                }
                return;
            }
            return;
        }
        if self.current_approval().is_some() {
            if let Some(hit) = self
                .hit_map
                .approval_actions
                .iter()
                .find(|hit| contains(hit.rect, column, row))
                .copied()
            {
                self.answer_approval(hit.decision).await;
            }
            return;
        }
        if let Some((_, action)) = self
            .hit_map
            .goal_actions
            .iter()
            .find(|(rect, _)| contains(*rect, column, row))
            .copied()
        {
            self.activate_goal_action(action);
            return;
        }
        self.goal_focus = None;
        if self
            .hit_map
            .permission_mode
            .is_some_and(|rect| contains(rect, column, row))
        {
            self.cycle_permission_mode();
            return;
        }
        if self
            .hit_map
            .session_cost
            .is_some_and(|rect| contains(rect, column, row))
        {
            self.modal = Modal::Usage;
            self.load_usage();
            return;
        }
        if self
            .hit_map
            .event_level_filter
            .is_some_and(|rect| contains(rect, column, row))
        {
            self.cycle_event_level_filter();
            return;
        }
        // Agent and model title segments open selectors. The bracketed variant
        // suffix cycles in place and never opens a separate panel.
        if let Some(hit) = self
            .hit_map
            .title_segments
            .iter()
            .find(|hit| contains(hit.rect, column, row))
            .copied()
        {
            match hit.segment {
                TitleSegment::Agent => self.open_selection_modal(Modal::Agents),
                TitleSegment::Model => self.open_selection_modal(Modal::Models),
                TitleSegment::Variant => self.cycle_draft_variant(),
            }
            return;
        }
        if let Some(hit) = self
            .hit_map
            .input
            .filter(|hit| contains(hit.rect, column, row))
        {
            // The composer's reserved scrollbar column mirrors the
            // conversation's exactly: a press on the thumb captures a drag,
            // a press on the bare track pages to the matching offset — all
            // without moving the text cursor.
            if let Some(geometry) = hit
                .scrollbar
                .filter(|geometry| contains(geometry.track, column, row))
            {
                if contains(geometry.thumb, column, row) {
                    self.scrollbar_drag = Some(ScrollbarDrag {
                        grab_row: row.saturating_sub(geometry.thumb.y),
                        target: ScrollbarTarget::Input,
                    });
                } else {
                    let offset = geometry.clamp_offset(geometry.offset_for_track_row(row));
                    self.input.scroll_to(offset);
                }
                return;
            }
            self.input.set_cursor_from_display_position(
                row.saturating_sub(hit.text_rect.y)
                    .min(hit.text_rect.height.saturating_sub(1)),
                column
                    .saturating_sub(hit.text_rect.x)
                    .min(hit.text_rect.width),
            );
            return;
        }
        if let Some(hit) = self
            .hit_map
            .queue_entries
            .iter()
            .find(|hit| contains(hit.rect, column, row))
        {
            if hit.kind == QueueEntryKind::User {
                self.recall_newest_pending();
            }
            return;
        }
        // The scrollbar column is reserved from content and block hit regions;
        // presses there page (track) or capture the thumb for dragging.
        if let Some(track) = self
            .hit_map
            .scrollbar
            .filter(|track| contains(*track, column, row))
            && let Some(geometry) = self.scrollbar_geometry
        {
            if contains(geometry.thumb, column, row) {
                self.scrollbar_drag = Some(ScrollbarDrag {
                    grab_row: row.saturating_sub(geometry.thumb.y),
                    target: ScrollbarTarget::Conversation,
                });
            } else {
                let offset = geometry.clamp_offset(geometry.offset_for_track_row(row));
                self.conversation_scroll.scroll_to(offset);
            }
            let _ = track;
            return;
        }
        // Past user-message rows open the copy/revert/fork menu — never
        // assistant/tool rows, which keep their expand/collapse toggle.
        if let Some(hit) = self
            .hit_map
            .user_messages
            .iter()
            .find(|hit| contains(hit.rect, column, row))
            .copied()
        {
            self.open_user_menu(hit);
            return;
        }
        if let Some(hit) = self
            .hit_map
            .blocks
            .iter()
            .rev()
            .find(|hit| {
                hit.toggle_rect
                    .is_some_and(|rect| contains(rect, column, row))
            })
            .copied()
        {
            self.toggle_block(hit.id);
            return;
        }
        if let Some(hit) = self
            .hit_map
            .tree_rows
            .iter()
            .find(|hit| contains(hit.rect, column, row))
            .copied()
        {
            if hit
                .expand_rect
                .is_some_and(|rect| contains(rect, column, row))
            {
                self.toggle_tree_session(hit.session_id);
            } else {
                self.tree_cursor = Some(hit.session_id);
                // Watching a descendant changes the conversation/highlight
                // only; the tree root snapshot is retained.
                self.watch_session(hit.session_id);
            }
        }
    }

    pub(in crate::ui) fn handle_wheel(&mut self, column: u16, row: u16, up: bool) {
        // Wheel ownership comes from current state, never stale geometry:
        // during an overlay transition the hit map can describe a panel
        // that is already gone (or miss one that just opened), so a rect
        // only targets the scroll *within* a surface state says is open.
        // Ownership order matches the render stacking and click/hover
        // routing exactly — palette, modal, approval, content — so a wheel
        // over overlapping panels reaches the topmost one, never the panel
        // it obscures.
        if self.command_palette_visible()
            && self
                .hit_map
                .palette
                .is_some_and(|rect| contains(rect, column, row))
        {
            self.move_palette_selection(up);
            return;
        }
        if self.modal != Modal::None {
            if self.modal == Modal::GoalDetail {
                self.scroll_goal_detail(up);
                return;
            }
            if self.modal == Modal::Usage {
                if up {
                    self.usage_panel.scroll_up(3);
                } else {
                    self.usage_panel.scroll_down(3);
                }
                return;
            }
            if self
                .hit_map
                .picker
                .is_some_and(|rect| contains(rect, column, row))
            {
                let len = match self.modal {
                    Modal::Sessions => {
                        self.session_search.focus_list();
                        self.session_search_ids().len()
                    }
                    Modal::Models => {
                        self.model_search.focus_list();
                        self.picker_entry_count()
                    }
                    Modal::Agents => {
                        self.agent_search.focus_list();
                        self.picker_entry_count()
                    }
                    Modal::Presets | Modal::Variants => self.picker_entry_count(),
                    Modal::ConnectProviders => self.filtered_providers().len(),
                    Modal::UserMessage => USER_MENU_ITEMS.len(),
                    Modal::ConnectDetails
                    | Modal::ConnectSetup
                    | Modal::ConnectError
                    | Modal::DisconnectConfirm
                    | Modal::RevertConfirm
                    | Modal::Mcp
                    | Modal::Permissions
                    | Modal::Usage
                    | Modal::GoalDetail
                    | Modal::None => 0,
                };
                move_picker_selection(&mut self.picker_state, len, up);
            }
            return;
        }
        // A visible approval panel swallows the wheel wherever it lands,
        // owned by state so the panel claims the gesture even before its
        // geometry renders; its rect only targets the panel scroll.
        if self.current_approval().is_some() {
            if self
                .hit_map
                .approval
                .is_some_and(|rect| contains(rect, column, row))
            {
                self.scroll_approval(up, 3);
            }
            return;
        }
        if self
            .hit_map
            .input
            .is_some_and(|hit| contains(hit.rect, column, row))
        {
            // The composer wheel-scrolls only when its content overflows
            // the (ceiling-height) box; a fitting draft has nothing to
            // scroll, and the gesture must not leak through to the
            // conversation beneath.
            if self.input.has_overflow() {
                self.input.move_wheel(up);
            }
            return;
        }
        // The reserved scrollbar column keeps wheel priority over content.
        if self
            .hit_map
            .scrollbar
            .is_some_and(|rect| contains(rect, column, row))
        {
            let page = usize::from(
                self.hit_map
                    .conversation
                    .map_or(3, |rect| rect.height.max(1)),
            );
            if up {
                self.conversation_scroll.up(3);
            } else {
                self.conversation_scroll.down(page.min(20));
            }
            return;
        }
        if self
            .hit_map
            .conversation
            .is_some_and(|rect| contains(rect, column, row))
        {
            if up {
                self.conversation_scroll.up(3);
            } else {
                self.conversation_scroll.down(3);
            }
            return;
        }
        if self
            .hit_map
            .tree
            .is_some_and(|rect| contains(rect, column, row))
        {
            if up {
                self.move_tree_selection(true);
            } else {
                self.move_tree_selection(false);
            }
        }
    }
}
