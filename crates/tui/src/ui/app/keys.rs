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
        (KeyCode::Enter, KeyModifiers::SHIFT | KeyModifiers::CONTROL)
            | (KeyCode::Char('j'), KeyModifiers::CONTROL)
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
                    McpFormFocus::Transport | McpFormFocus::Enabled | McpFormFocus::Persist
                )
            }) {
                self.mcp_panel.form.as_mut().expect("form").cycle_choice();
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
            KeyCode::Char('e' | 'd' | ' ') => match self.mcp_panel.selected_editable() {
                Err(refusal) => self.status = refusal,
                Ok(None) => {}
                Ok(Some(server)) => {
                    let server = server.clone();
                    match key.code {
                        KeyCode::Char('e') => self.mcp_panel.form = Some(McpForm::edit(&server)),
                        KeyCode::Char('d') => self.dispatch_mcp_remove(server.name),
                        _ => self.dispatch_mcp_toggle(server.name, !server.definition.enabled),
                    }
                }
            },
            // Any runtime's server reconnects; the request goes to the
            // runtime that hosts it.
            KeyCode::Char('r') => {
                if let Some(server) = self.mcp_panel.selected() {
                    let params = McpServerReconnectParams {
                        name: server.name.clone(),
                        runtime_id: (!crate::ui::management::is_global(server))
                            .then(|| server.runtime_id.clone()),
                        workspace: server.workspace.clone(),
                    };
                    self.dispatch_mcp_reconnect(params);
                }
            }
            KeyCode::Char('a') => {
                if let Some(server) = self.mcp_panel.selected().cloned()
                    && server.state == McpServerState::NeedsAuth
                    && crate::ui::management::is_global(&server)
                {
                    self.dispatch_mcp_auth_begin(server.name);
                }
            }
            _ => {}
        }
    }

    pub(super) fn handle_runtime_key(&mut self, key: KeyEvent) {
        if let Some(choosing) = self
            .runtime_panel
            .form
            .as_ref()
            .map(|form| form.chosen.is_none())
        {
            match key.code {
                KeyCode::Esc => self.runtime_panel.back(),
                KeyCode::Up if choosing => self.runtime_panel.move_choice(true),
                KeyCode::Down if choosing => self.runtime_panel.move_choice(false),
                KeyCode::Enter if choosing => match self.runtime_panel.choose() {
                    Ok(crate::ui::management::AttachStep::Parameters) => {}
                    Ok(crate::ui::management::AttachStep::Attach(
                        runtime_id,
                        capability,
                        workspace,
                    )) => self.attach_runtime_choice(runtime_id, capability, workspace),
                    Err(error) => self.status = error,
                },
                KeyCode::Enter => match self.runtime_panel.attach_request() {
                    Ok((runtime_id, capability, workspace)) => {
                        self.attach_runtime_choice(runtime_id, capability, workspace);
                    }
                    Err(error) => self.status = error,
                },
                _ => {
                    if let Some(form) = self.runtime_panel.form.as_mut() {
                        edit_plain_input(form.input_mut(), key);
                    }
                    if choosing {
                        self.runtime_panel.clamp_choice();
                    }
                }
            }
            return;
        }
        let rows = self.runtime_rows();
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Up => {
                move_picker_selection(&mut self.runtime_panel.selection, rows.len(), true)
            }
            KeyCode::Down => {
                move_picker_selection(&mut self.runtime_panel.selection, rows.len(), false);
            }
            KeyCode::Char('a') => {
                if let Err(error) = self.runtime_panel.begin_attach() {
                    self.status = error;
                }
            }
            KeyCode::Char('d') if self.new_session_draft.is_some() => {
                if let Some(index) = self.runtime_panel.selection.selected()
                    && let Some(entries) = self.draft_runtimes.as_mut()
                    && index < entries.len()
                {
                    let entry = entries.remove(index);
                    self.status = format!(
                        "the new session will not attach runtime `{}`",
                        entry.request.runtime_id
                    );
                }
            }
            KeyCode::Char('r') if self.new_session_draft.is_some() => {
                if let Some(index) = self.runtime_panel.selection.selected()
                    && let Some(entry) = self
                        .draft_runtimes
                        .as_mut()
                        .and_then(|entries| entries.get_mut(index))
                {
                    entry.opened = None;
                    let entry = entry.clone();
                    self.status = format!("retrying runtime `{}`...", entry.request.runtime_id);
                    self.open_draft_runtime(&entry);
                }
            }
            KeyCode::Char('d') => {
                if let Some(row) = self
                    .runtime_panel
                    .selection
                    .selected()
                    .and_then(|index| rows.get(index))
                {
                    self.dispatch_runtime_detach(row.attachment.name.clone());
                }
            }
            KeyCode::Char('r') => {
                if let Some(row) = self
                    .runtime_panel
                    .selection
                    .selected()
                    .and_then(|index| rows.get(index))
                {
                    match self.runtime_panel.reconnect_request(row) {
                        Ok((runtime_id, capability, workspace)) => {
                            self.dispatch_runtime_reconnect(runtime_id, capability, workspace);
                        }
                        Err(error) => self.status = error,
                    }
                }
            }
            _ => {}
        }
    }

    fn dispatch_runtime_reconnect(
        &mut self,
        runtime_id: cookie_agent_protocol::RuntimeId,
        capability: cookie_agent_protocol::CapabilityName,
        workspace: Option<String>,
    ) {
        self.status = format!("retrying runtime `{runtime_id}`...");
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .reconnect_runtime(cookie_agent_protocol::RuntimesReconnectParams {
                    runtime_id: runtime_id.clone(),
                    capability: Some(capability),
                    workspace,
                })
                .await
                .map(|result| {
                    let status = match result.instance.status {
                        cookie_agent_protocol::RuntimeInstanceStatus::Starting => "starting",
                        cookie_agent_protocol::RuntimeInstanceStatus::Ready => "ready",
                        cookie_agent_protocol::RuntimeInstanceStatus::Reconnecting => {
                            "reconnecting"
                        }
                        cookie_agent_protocol::RuntimeInstanceStatus::Failed => "failed",
                        cookie_agent_protocol::RuntimeInstanceStatus::Stopped => "stopped",
                        cookie_agent_protocol::RuntimeInstanceStatus::Unknown => "unknown",
                    };
                    format!("runtime `{runtime_id}` is {status}")
                })
                .map_err(|error| format!("retry failed: {error}"));
            let _ = updates.send(RpcUpdate::RuntimeAttachmentChanged { result });
        });
    }

    fn attach_runtime_choice(
        &mut self,
        runtime_id: cookie_agent_protocol::RuntimeId,
        capability: Option<cookie_agent_protocol::CapabilityName>,
        workspace: Option<String>,
    ) {
        if self.new_session_draft.is_some() {
            self.add_draft_runtime(cookie_agent_protocol::RuntimeAttachRequest {
                runtime_id,
                capability,
                workspace,
            });
            return;
        }
        let Some(session_id) = self.selected else {
            self.status = "select a session before attaching a runtime".into();
            return;
        };
        self.runtime_panel.form = None;
        self.status = format!("attaching runtime `{runtime_id}`...");
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .attach_runtime(cookie_agent_protocol::SessionRuntimeAttachParams {
                    session_id,
                    runtime_id,
                    capability,
                    workspace,
                })
                .await
                .map(|result| format!("attached runtime as `{}`", result.attachment.name))
                .map_err(|error| format!("attach failed: {error}"));
            let _ = updates.send(RpcUpdate::RuntimeAttachmentChanged { result });
        });
    }

    fn dispatch_runtime_detach(&self, name: cookie_agent_protocol::AttachmentName) {
        let Some(session_id) = self.selected else {
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .detach_runtime(cookie_agent_protocol::SessionRuntimeDetachParams {
                    session_id,
                    name: name.clone(),
                })
                .await
                .map(|_| format!("detached runtime `{name}`"))
                .map_err(|error| format!("detach failed: {error}"));
            let _ = updates.send(RpcUpdate::RuntimeAttachmentChanged { result });
        });
    }

    /// The `coding` capability a new session attaches by default: the
    /// runtime this client offers, else `local`, at the client's directory.
    pub(super) fn default_coding_runtime(
        &self,
    ) -> Option<cookie_agent_protocol::RuntimeAttachRequest> {
        let runtime = self
            .client_runtime
            .as_deref()
            .unwrap_or(cookie_agent_protocol::RuntimeId::LOCAL);
        Some(cookie_agent_protocol::RuntimeAttachRequest {
            runtime_id: cookie_agent_protocol::RuntimeId::new(runtime).ok()?,
            capability: Some(cookie_agent_protocol::CapabilityName::coding()),
            workspace: self.cwd.clone(),
        })
    }

    /// Gives the new-session draft its own runtimes, starting from what
    /// `session.create` attaches by default, and opens their instances.
    pub(super) fn materialize_draft_runtimes(&mut self) {
        if self.draft_runtimes.is_some() {
            return;
        }
        self.draft_runtimes = Some(Vec::new());
        let global =
            cookie_agent_protocol::RuntimeId::new(cookie_agent_protocol::RuntimeId::GLOBAL)
                .ok()
                .map(|runtime_id| cookie_agent_protocol::RuntimeAttachRequest {
                    capability: Some(cookie_agent_protocol::default_capability(&runtime_id)),
                    runtime_id,
                    workspace: None,
                });
        for request in global.into_iter().chain(self.default_coding_runtime()) {
            let entry = self.push_draft_runtime(request);
            // `global` is always running; opening it would only retry its
            // MCP servers.
            if !entry.is_global() {
                self.open_draft_runtime(&entry);
            }
        }
    }

    fn push_draft_runtime(
        &mut self,
        request: cookie_agent_protocol::RuntimeAttachRequest,
    ) -> crate::ui::management::DraftRuntime {
        self.next_draft_runtime += 1;
        let entry = crate::ui::management::DraftRuntime {
            id: self.next_draft_runtime,
            request,
            opened: None,
        };
        self.draft_runtimes
            .get_or_insert_with(Vec::new)
            .push(entry.clone());
        entry
    }

    fn add_draft_runtime(&mut self, request: cookie_agent_protocol::RuntimeAttachRequest) {
        self.materialize_draft_runtimes();
        if self
            .draft_runtimes
            .iter()
            .flatten()
            .any(|entry| entry.same_instance(&request))
        {
            self.status = format!(
                "the new session already attaches runtime `{}` there",
                request.runtime_id
            );
            return;
        }
        self.runtime_panel.form = None;
        self.status = format!("opening runtime `{}`...", request.runtime_id);
        let entry = self.push_draft_runtime(request);
        self.open_draft_runtime(&entry);
    }

    /// Opens (or retries) the instance of a draft runtime, so the panel
    /// shows its status before the session attaches it.
    fn open_draft_runtime(&self, entry: &crate::ui::management::DraftRuntime) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let (id, request) = (entry.id, entry.request.clone());
        self.spawn_rpc(async move {
            let result = client
                .reconnect_runtime(cookie_agent_protocol::RuntimesReconnectParams {
                    runtime_id: request.runtime_id,
                    capability: request.capability,
                    workspace: request.workspace,
                })
                .await
                .map(|result| result.instance)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::DraftRuntimeOpened { id, result });
        });
    }

    /// Records what opening draft runtime `id` answered. An instance that
    /// turns out to be one the draft already attaches, under another
    /// spelling of its workspace, leaves the draft.
    pub(super) fn install_draft_runtime(
        &mut self,
        id: u64,
        result: Result<cookie_agent_protocol::RuntimeInstanceInfo, String>,
    ) {
        let Some(entries) = self.draft_runtimes.as_mut() else {
            return;
        };
        let Some(index) = entries.iter().position(|entry| entry.id == id) else {
            return;
        };
        let runtime_id = entries[index].request.runtime_id.clone();
        if let Ok(instance) = &result {
            let request = cookie_agent_protocol::RuntimeAttachRequest {
                runtime_id: instance.runtime_id.clone(),
                capability: Some(instance.capability.clone()),
                workspace: instance.workspace.clone(),
            };
            if entries
                .iter()
                .enumerate()
                .any(|(other, entry)| other != index && entry.same_instance(&request))
            {
                entries.remove(index);
                self.status =
                    format!("the new session already attaches runtime `{runtime_id}` there");
                return;
            }
        }
        if let Err(error) = &result {
            self.status = format!("runtime `{runtime_id}` cannot start: {error}");
        }
        entries[index].opened = Some(result);
    }

    /// The selected session's attachments with their instances, or the
    /// new-session draft's runtimes.
    pub(in crate::ui) fn runtime_rows(&self) -> Vec<crate::ui::management::RuntimeRow> {
        if self.new_session_draft.is_some() {
            return crate::ui::management::draft_runtime_rows(
                self.draft_runtimes.as_deref().unwrap_or_default(),
                self.runtime_panel.listing.as_ref(),
            );
        }
        let attachments = self
            .selected
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .map(|state| state.runtime_attachments.attachments());
        match attachments {
            Some(attachments) => crate::ui::management::runtime_rows(
                attachments,
                self.runtime_panel.listing.as_ref(),
            ),
            None => Vec::new(),
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

    pub(super) fn dispatch_mcp_reconnect(&self, params: McpServerReconnectParams) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .reconnect_mcp_server(params)
                .await
                .map(|result| result.server)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpMutation {
                result: Box::new(result),
            });
        });
    }

    /// Binds the loopback callback first, since its address goes into the
    /// authorization request, then forwards every redirect the browser sends
    /// to the daemon until one completes the flow.
    pub(super) fn dispatch_mcp_auth_begin(&mut self, server: String) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let task = tokio::spawn(async move {
            let listener = match OAuthCallbackListener::bind().await {
                Ok(listener) => listener,
                Err(error) => {
                    let _ = updates.send(RpcUpdate::McpAuthBegan {
                        result: Err(format!("could not listen for the OAuth callback: {error}")),
                    });
                    return;
                }
            };
            let result = client
                .begin_mcp_auth(McpAuthBeginParams {
                    server: server.clone(),
                    redirect_uri: listener.redirect_uri(),
                })
                .await
                .map_err(|error| error.to_string());
            let began = result.is_ok();
            let _ = updates.send(RpcUpdate::McpAuthBegan { result });
            if !began {
                return;
            }
            while let Ok(callback) = listener.accept().await {
                let result = client
                    .complete_mcp_auth(McpAuthCompleteParams {
                        server: server.clone(),
                        callback_url: callback.url().to_owned(),
                    })
                    .await
                    .map(drop)
                    .map_err(|error| error.to_string());
                let _ = callback.respond(result.is_ok()).await;
                let completed = result.is_ok();
                let _ = updates.send(RpcUpdate::McpAuthCompleted {
                    server: server.clone(),
                    result,
                });
                if completed {
                    return;
                }
            }
        });
        self.mcp_panel.auth_listener = Some(AuthListener(task.abort_handle()));
    }

    /// The paste fallback for a browser on another machine, whose redirect
    /// to this machine's loopback never arrives.
    pub(super) fn dispatch_mcp_auth_complete(&self, server: String, callback_url: String) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .complete_mcp_auth(McpAuthCompleteParams {
                    server: server.clone(),
                    callback_url,
                })
                .await
                .map(drop)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpAuthCompleted { server, result });
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
        if self.permission_panel.form.is_some() {
            self.handle_permission_form_key(key);
            return;
        }
        self.permission_panel.notice = None;
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Up => self.permission_panel.move_selection(-1),
            KeyCode::Down => self.permission_panel.move_selection(1),
            KeyCode::PageUp => self.permission_panel.move_selection(-10),
            KeyCode::PageDown => self.permission_panel.move_selection(10),
            KeyCode::Home => self.permission_panel.selection.select(Some(0)),
            KeyCode::End => self.permission_panel.select_last(),
            KeyCode::Left | KeyCode::Right => {
                if let Some(row) = self.permission_panel.selected() {
                    let effect = step_effect(row.effect, key.code == KeyCode::Left);
                    self.set_permission_effect(&row, effect);
                }
            }
            KeyCode::Enter => {
                if self.permission_panel.add_row_selected() {
                    self.open_permission_form();
                } else if let Some(row) = self
                    .permission_panel
                    .selected()
                    .filter(|row| row.source == PermissionRuleSource::SessionOverlay)
                {
                    self.permission_panel.form = Some(PermissionForm::edit(&row));
                }
            }
            KeyCode::Char('n') | KeyCode::Char('+') => self.open_permission_form(),
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                self.remove_selected_permission();
            }
            KeyCode::Char('m') => self.cycle_permission_mode(),
            _ => {}
        }
    }

    fn handle_permission_form_key(&mut self, key: KeyEvent) {
        let Some(form) = &mut self.permission_panel.form else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.permission_panel.form = None;
                return;
            }
            KeyCode::Tab => form.cycle_focus(false),
            KeyCode::BackTab => form.cycle_focus(true),
            KeyCode::Up => form.cycle_focus(true),
            KeyCode::Down => form.cycle_focus(false),
            KeyCode::Enter => {
                self.submit_permission_form();
                return;
            }
            KeyCode::Left | KeyCode::Right => {
                let backward = key.code == KeyCode::Left;
                match form.focus {
                    PermissionFormFocus::Action => form.cycle_action(
                        &crate::ui::management::permission_actions(
                            self.permission_panel.result.as_ref(),
                        ),
                        backward,
                    ),
                    PermissionFormFocus::Effect => form.effect = step_effect(form.effect, backward),
                    PermissionFormFocus::Pattern => edit_plain_input(&mut form.pattern, key),
                }
            }
            _ if form.focus == PermissionFormFocus::Pattern => {
                edit_plain_input(&mut form.pattern, key);
            }
            _ => {}
        }
        form.error = None;
    }

    /// Open the new-rule form for the selected row's action.
    pub(super) fn open_permission_form(&mut self) {
        let action = self
            .permission_panel
            .selected()
            .map_or(PermissionAction::BASH, |row| row.action.clone());
        self.permission_panel.form = Some(PermissionForm::new(action));
    }

    /// Change a row's effect. An agent or default rule gains a session
    /// override with the same pattern; a session rule is updated.
    pub(super) fn set_permission_effect(&mut self, row: &PermissionRow, effect: PermissionEffect) {
        if row.effect == effect {
            return;
        }
        self.dispatch_permission_set(row.action.clone(), row.resource.clone(), effect);
    }

    fn remove_selected_permission(&mut self) {
        let Some(row) = self.permission_panel.selected() else {
            return;
        };
        if row.source == PermissionRuleSource::SessionOverlay {
            self.dispatch_permission_clear(row.action, row.resource);
        } else {
            self.permission_panel.notice = Some(format!(
                "Only session rules can be removed; this one comes from the {}. \
                 Change its effect to override it for this session.",
                if row.source == PermissionRuleSource::Default {
                    "default"
                } else {
                    "agent"
                }
            ));
        }
    }

    pub(super) fn submit_permission_form(&mut self) {
        let Some(form) = &mut self.permission_panel.form else {
            return;
        };
        let pattern = form.pattern.as_str().trim();
        let resource = match cookie_agent_protocol::WildcardPattern::new(pattern) {
            Ok(resource) => resource.to_string(),
            Err(error) => {
                form.error = Some(if pattern.is_empty() {
                    "enter a pattern, or * for everything".into()
                } else {
                    format!("invalid pattern: {error}")
                });
                form.focus = PermissionFormFocus::Pattern;
                return;
            }
        };
        let form = self
            .permission_panel
            .form
            .take()
            .expect("permission form is open");
        match form.editing {
            Some((action, previous)) if action != form.action || previous != resource => {
                self.dispatch_permission_replace(
                    (action, previous),
                    form.action,
                    resource,
                    form.effect,
                );
            }
            _ => self.dispatch_permission_set(form.action, resource, form.effect),
        }
    }

    /// Replace an edited session rule: set the new one, then clear the old
    /// one, in order on one task so the panel ends on the final overlay.
    fn dispatch_permission_replace(
        &mut self,
        (previous_action, previous_resource): (PermissionAction, String),
        action: PermissionAction,
        resource: String,
        effect: PermissionEffect,
    ) {
        let (Ok(resource), Ok(previous_resource)) = (
            cookie_agent_protocol::WildcardPattern::new(resource),
            cookie_agent_protocol::WildcardPattern::new(previous_resource),
        ) else {
            return;
        };
        if self.permission_panel.draft {
            self.draft_permissions
                .remove(&previous_action, &previous_resource);
            self.draft_permissions.set(PermissionRule {
                runtime: None,
                action,
                resource,
                effect,
            });
            self.load_permission_preview();
            return;
        }
        let Some(session_id) = self.selected else {
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = async {
                client
                    .set_session_permission(SessionPermissionSetParams {
                        runtime: None,
                        session_id,
                        action,
                        resource,
                        effect,
                    })
                    .await?;
                client
                    .clear_session_permission(SessionPermissionClearParams {
                        runtime: None,
                        session_id,
                        action: previous_action,
                        resource: previous_resource,
                    })
                    .await
            }
            .await
            .map(|result| SessionPermissionGetResult {
                permissions: result.permissions,
                current_mode: None,
            })
            .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PermissionsLoaded { session_id, result });
        });
    }

    pub(super) fn dispatch_permission_set(
        &mut self,
        action: PermissionAction,
        resource: String,
        effect: PermissionEffect,
    ) {
        if self.permission_panel.draft {
            if let Ok(resource) = cookie_agent_protocol::WildcardPattern::new(resource) {
                self.draft_permissions.set(PermissionRule {
                    runtime: None,
                    action,
                    resource,
                    effect,
                });
                self.load_permission_preview();
            }
            return;
        }
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
                    runtime: None,
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

    pub(super) fn dispatch_permission_clear(&mut self, action: PermissionAction, resource: String) {
        if self.permission_panel.draft {
            if let Ok(resource) = cookie_agent_protocol::WildcardPattern::new(resource) {
                self.draft_permissions.remove(&action, &resource);
                self.load_permission_preview();
            }
            return;
        }
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
                    runtime: None,
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
        // Ctrl+Z suspends the TUI as it would any terminal program; raw
        // mode means the terminal does not do it for us. Job control is
        // Unix's only.
        if key.code == KeyCode::Char('z') && key.modifiers == KeyModifiers::CONTROL {
            if cfg!(unix) {
                self.pending_suspend = true;
            }
            return;
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
            Modal::Background => self.handle_background_key(key),
            Modal::Plugins => self.handle_plugin_key(key),
            Modal::Config => self.handle_config_key(key),
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
            Modal::AttachConfirm => self.handle_attach_offer_key(key).await,
            Modal::Mcp => self.handle_mcp_key(key).await,
            Modal::Runtimes => self.handle_runtime_key(key),
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
                } else if self.selected_active_run().is_some() || self.selected_compacting() {
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
                    || state.is_compacting()
                    || crate::state::SessionState::has_open_thinking(state)
                    || crate::state::SessionState::has_running_tool(state)
                    || state.has_pending_producers()
            })
    }

    pub(super) fn working_state(&self) -> Option<WorkingState> {
        let state = self.selected.and_then(|id| self.store.sessions.get(&id))?;
        if state.is_compacting() {
            return Some(WorkingState::Compacting);
        }
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

    /// Advance the animation one 33ms frame. Returns whether the
    /// [`Self::clock_bucket`] changed — the only per-frame input to the
    /// drawn view — so the event loop redraws once per step instead of
    /// every frame.
    pub(in crate::ui) fn animation_tick(&mut self) -> bool {
        let before = self.clock_bucket();
        self.animation_ticks = self.animation_ticks.wrapping_add(1);
        self.clock_bucket() != before
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
            if self.modal == Modal::Permissions {
                if self.permission_panel.form.is_none() {
                    self.permission_panel
                        .move_selection(if up { -1 } else { 1 });
                }
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
                    | Modal::AttachConfirm
                    | Modal::Mcp
                    | Modal::Runtimes
                    | Modal::Permissions
                    | Modal::Usage
                    | Modal::GoalDetail
                    | Modal::Background
                    | Modal::Plugins
                    | Modal::Config
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
