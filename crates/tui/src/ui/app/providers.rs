//! Provider connect and disconnect modal flows.

use super::*;

/// The provider details panel's scroll: a provider can list hundreds of
/// models.
#[derive(Debug, Default)]
pub(super) struct DetailsScroll {
    pub(super) scroll: usize,
    pub(super) max_scroll: usize,
    pub(super) page_size: usize,
}

impl DetailsScroll {
    fn scroll_by(&mut self, lines: usize, up: bool) {
        self.scroll = if up {
            self.scroll.saturating_sub(lines)
        } else {
            self.scroll.saturating_add(lines).min(self.max_scroll)
        };
    }
}

impl App {
    pub(super) fn focus_provider_field(&mut self, hit: ProviderFieldHit, column: u16, row: u16) {
        let Some(form) = self.provider_form.as_mut() else {
            return;
        };
        form.set_focus(hit.focus);
        let editor = match hit.focus {
            ProviderFormFocus::Credential(index) => {
                form.secrets.get_mut(index).map(|field| &mut field.input)
            }
            ProviderFormFocus::Setup(index) => {
                form.setup.get_mut(index).map(|field| &mut field.input)
            }
            ProviderFormFocus::AuthMethod
            | ProviderFormFocus::Submit
            | ProviderFormFocus::Cancel => None,
        };
        if let Some(editor) = editor {
            editor.state_mut().set_cursor_from_display_position(
                row.saturating_sub(hit.text_rect.y)
                    .min(hit.text_rect.height.saturating_sub(1)),
                column
                    .saturating_sub(hit.text_rect.x)
                    .min(hit.text_rect.width),
            );
        }
    }

    pub(super) fn handle_connect_provider_key(&mut self, key: KeyEvent) {
        let count = self.filtered_providers().len();
        if self.provider_search.focus() == SearchPickerFocus::Input {
            match key.code {
                KeyCode::Esc => self.close_provider_picker(),
                KeyCode::Down | KeyCode::Tab | KeyCode::Enter if count > 0 => {
                    self.provider_search.focus_list();
                    self.clamp_picker_selection();
                }
                KeyCode::Backspace => {
                    self.provider_search.input_mut().backspace();
                    self.provider_search_changed();
                }
                KeyCode::Delete => {
                    self.provider_search.input_mut().delete();
                    self.provider_search_changed();
                }
                KeyCode::Left => self.provider_search.input_mut().move_left(),
                KeyCode::Right => self.provider_search.input_mut().move_right(),
                KeyCode::Home | KeyCode::Char('a')
                    if key.code == KeyCode::Home || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.provider_search.input_mut().move_buffer_home();
                }
                KeyCode::End | KeyCode::Char('e')
                    if key.code == KeyCode::End || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.provider_search.input_mut().move_buffer_end();
                }
                KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                    self.provider_search.input_mut().set_buffer(String::new());
                    self.provider_search_changed();
                }
                KeyCode::Char(character) if is_printable_key(key) => {
                    self.provider_search.input_mut().insert(character);
                    self.provider_search_changed();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.provider_search.focus_input();
            }
            KeyCode::Up if self.picker_state.selected().unwrap_or(0) == 0 => {
                self.provider_search.focus_input();
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down | KeyCode::Tab => {
                move_picker_selection(&mut self.picker_state, count, false)
            }
            KeyCode::BackTab => self.provider_search.focus_input(),
            KeyCode::Enter => {
                let index = self.picker_state.selected().unwrap_or(0);
                if let Some(provider) = self
                    .filtered_providers()
                    .get(index)
                    .map(|provider| (*provider).clone())
                {
                    if matches!(
                        self.provider_operations.get(&provider.id),
                        Some(ProviderOperation::InProgress(_))
                    ) {
                        self.status = "Provider operation already in progress.".into();
                        return;
                    }
                    let state = row_state(
                        &provider,
                        &self.models,
                        self.provider_operations.get(&provider.id),
                    );
                    match state {
                        // Credentials from config.toml: a form could not
                        // replace them. A connected provider opens on its
                        // models; R reconnects from there.
                        ProviderRowState::Unsupported | ProviderRowState::Configured => {
                            self.open_provider_details(provider);
                        }
                        ProviderRowState::ConnectedReconnect
                            if !cookie_agent_client_state::providers::sign_in_expired(
                                &provider,
                            ) =>
                        {
                            self.open_provider_details(provider);
                        }
                        _ if cookie_agent_client_state::providers::sign_in_method(&provider)
                            .is_some() =>
                        {
                            self.begin_provider_sign_in(provider);
                        }
                        ProviderRowState::Disconnected
                        | ProviderRowState::ConnectedReconnect
                        | ProviderRowState::Removed => self.begin_provider_form(provider),
                        ProviderRowState::ErrorRetry => {
                            let failed_action = self
                                .provider_operations
                                .get(&provider.id)
                                .and_then(|operation| match operation {
                                    ProviderOperation::Error { action, .. } => Some(*action),
                                    ProviderOperation::InProgress(_) => None,
                                });
                            if failed_action == Some(ProviderAction::Disconnect) {
                                self.connect_provider = Some(provider);
                                self.modal = Modal::DisconnectConfirm;
                            } else {
                                self.begin_provider_form(provider);
                            }
                        }
                    }
                }
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.provider_search.reset();
                self.provider_search_changed();
            }
            KeyCode::Char(character) if is_printable_key(key) => {
                self.provider_search.focus_input();
                self.provider_search.input_mut().insert(character);
                self.provider_search_changed();
            }
            _ => {}
        }
    }

    pub(in crate::ui) fn open_provider_details(&mut self, provider: ProviderDescriptor) {
        self.connect_provider = Some(provider);
        self.provider_search.reset();
        self.connect_details_scroll = DetailsScroll::default();
        self.modal = Modal::ConnectDetails;
    }

    pub(super) fn scroll_connect_details(&mut self, up: bool) {
        self.connect_details_scroll.scroll_by(1, up);
    }

    /// Starts the provider's sign-in in its details: binds the loopback
    /// listener the redirect returns to, asks the daemon for the URL, and
    /// forwards every redirect the browser sends until one completes it.
    pub(in crate::ui) fn begin_provider_sign_in(&mut self, provider: ProviderDescriptor) {
        let Some(method) = cookie_agent_client_state::providers::sign_in_method(&provider) else {
            return;
        };
        let method_id = method.id.clone();
        let path = method.redirect_path.as_str().to_owned();
        self.clear_connect_secrets();
        self.provider_search.reset();
        let provider_id = provider.id.clone();
        self.connect_provider = Some(provider);
        self.connect_details_scroll = DetailsScroll::default();
        self.modal = Modal::ConnectDetails;
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let task_provider = provider_id.clone();
        let task = tokio::spawn(async move {
            let provider_id = task_provider;
            let listener = match OAuthCallbackListener::bind_on_path(path).await {
                Ok(listener) => listener,
                Err(error) => {
                    let _ = updates.send(RpcUpdate::ProviderAuthBegan {
                        provider_id,
                        result: Err(format!(
                            "could not listen for the sign-in redirect: {error}"
                        )),
                    });
                    return;
                }
            };
            let result = client
                .begin_provider_auth(cookie_agent_protocol::ProviderAuthBeginParams {
                    provider_id: provider_id.clone(),
                    method: method_id,
                    redirect_uri: Some(listener.redirect_uri()),
                })
                .await
                .map_err(|error| error.to_string());
            let began = result.is_ok();
            let _ = updates.send(RpcUpdate::ProviderAuthBegan {
                provider_id: provider_id.clone(),
                result,
            });
            if !began {
                return;
            }
            while let Ok(callback) = listener.accept().await {
                let result = client
                    .complete_provider_auth(cookie_agent_protocol::ProviderAuthCompleteParams {
                        provider_id: provider_id.clone(),
                        callback_url: callback.url().to_owned(),
                    })
                    .await
                    .map_err(|error| error.to_string());
                let _ = callback.respond(result.is_ok()).await;
                let completed = result.is_ok();
                let _ = updates.send(RpcUpdate::ProviderAuthCompleted {
                    provider_id: provider_id.clone(),
                    result,
                });
                if completed {
                    return;
                }
            }
        });
        self.provider_sign_in = Some(ProviderSignInView {
            provider_id,
            authorization_url: None,
            error: None,
            listener: Some(AuthListener(task.abort_handle())),
        });
        self.status = "Starting the sign-in…".into();
    }

    /// The paste fallback for a browser on another machine, whose redirect
    /// to this machine's loopback never arrives.
    pub(super) fn dispatch_provider_auth_complete(&self, callback_url: String) {
        let Some(sign_in) = &self.provider_sign_in else {
            return;
        };
        let provider_id = sign_in.provider_id.clone();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .complete_provider_auth(cookie_agent_protocol::ProviderAuthCompleteParams {
                    provider_id: provider_id.clone(),
                    callback_url,
                })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::ProviderAuthCompleted {
                provider_id,
                result,
            });
        });
    }

    /// Ends the sign-in on both sides.
    pub(super) fn cancel_provider_sign_in(&mut self) {
        let Some(sign_in) = self.provider_sign_in.take() else {
            return;
        };
        let client = self.client.clone();
        self.spawn_rpc(async move {
            let _ = client
                .cancel_provider_auth(cookie_agent_protocol::ProviderAuthCancelParams {
                    provider_id: sign_in.provider_id,
                })
                .await;
        });
    }

    pub(in crate::ui) fn begin_provider_form(&mut self, provider: ProviderDescriptor) {
        self.clear_connect_secrets();
        self.connect_provider = Some(provider.clone());
        let reconnect = provider.durable_connection.is_some()
            || row_state(&provider, &self.models, None) == ProviderRowState::ConnectedReconnect;
        let Some(form) = ProviderForm::new(provider, reconnect) else {
            self.connect_details_scroll = DetailsScroll::default();
            self.modal = Modal::ConnectDetails;
            self.status = "This provider has no store-backed authentication form.".into();
            return;
        };
        self.modal = Modal::ConnectSetup;
        self.provider_form = Some(form);
        self.provider_search.reset();
    }

    pub(super) fn handle_connect_details_key(&mut self, key: KeyEvent) {
        let Some(provider) = self.connect_provider.clone() else {
            self.modal = Modal::None;
            return;
        };
        if let Some(sign_in) = &self.provider_sign_in {
            match key.code {
                KeyCode::Esc => {
                    self.cancel_provider_sign_in();
                    self.modal = Modal::None;
                    self.status = "Sign-in cancelled.".into();
                }
                KeyCode::Char('c') => {
                    if let Some(url) = sign_in.authorization_url.clone() {
                        self.copy_to_clipboard(url);
                    }
                }
                KeyCode::Char('d' | 'D') if provider.durable_connection.is_some() => {
                    self.cancel_provider_sign_in();
                    self.modal = Modal::DisconnectConfirm;
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.clear_connect_secrets();
                self.modal = Modal::None;
            }
            KeyCode::Char('r' | 'R')
                if matches!(
                    row_state(
                        &provider,
                        &self.models,
                        self.provider_operations.get(&provider.id)
                    ),
                    ProviderRowState::ConnectedReconnect
                        | ProviderRowState::Removed
                        | ProviderRowState::Configured
                ) =>
            {
                if cookie_agent_client_state::providers::sign_in_method(&provider).is_some() {
                    self.begin_provider_sign_in(provider);
                } else {
                    self.begin_provider_form(provider);
                }
            }
            KeyCode::Up => self.scroll_connect_details(true),
            KeyCode::Down => self.scroll_connect_details(false),
            KeyCode::PageUp | KeyCode::PageDown => {
                let page = self.connect_details_scroll.page_size.max(1);
                self.connect_details_scroll
                    .scroll_by(page, key.code == KeyCode::PageUp);
            }
            KeyCode::Home => self.connect_details_scroll.scroll = 0,
            KeyCode::End => {
                self.connect_details_scroll.scroll = self.connect_details_scroll.max_scroll;
            }
            KeyCode::Char('d' | 'D') if provider.durable_connection.is_some() => {
                self.modal = Modal::DisconnectConfirm;
            }
            KeyCode::Enter => {
                // Unsupported providers are details-only. Enter deliberately
                // performs no mutation.
            }
            _ => {}
        }
    }

    pub(super) fn handle_connect_setup_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.cancel_connect_form();
            return;
        }
        if key.code == KeyCode::Char('d')
            && key.modifiers == KeyModifiers::CONTROL
            && self
                .provider_form
                .as_ref()
                .is_some_and(|form| form.can_disconnect)
        {
            self.connect_provider = self
                .provider_form
                .as_ref()
                .map(|form| form.provider.clone());
            self.modal = Modal::DisconnectConfirm;
            return;
        }
        let Some(form) = &mut self.provider_form else {
            self.modal = Modal::None;
            return;
        };
        match key.code {
            KeyCode::Up | KeyCode::BackTab => form.move_focus(true),
            KeyCode::Down | KeyCode::Tab => form.move_focus(false),
            // Enter activates the focused button and submits from any other
            // focus — the same path as the Submit button. Traversal is
            // Tab/Down-only; validation failures keep the modal and the
            // focus where they are.
            KeyCode::Enter => match form.focus() {
                ProviderFormFocus::Cancel => self.cancel_connect_form(),
                _ => self.dispatch_provider_connect(),
            },
            KeyCode::Left if form.focus() == ProviderFormFocus::AuthMethod => {
                form.cycle_auth_method(true);
            }
            KeyCode::Right | KeyCode::Char(' ')
                if form.focus() == ProviderFormFocus::AuthMethod =>
            {
                form.cycle_auth_method(false);
            }
            _ => match form.focus() {
                ProviderFormFocus::Credential(index) => {
                    // Any edit supersedes a stale inline validation error.
                    form.error = None;
                    edit_credential_input(&mut form.secrets[index].input, key);
                }
                ProviderFormFocus::Setup(index) => {
                    form.error = None;
                    edit_credential_input(&mut form.setup[index].input, key);
                }
                ProviderFormFocus::AuthMethod
                | ProviderFormFocus::Submit
                | ProviderFormFocus::Cancel => {}
            },
        }
    }

    /// Abort the connect form exactly like Escape: wipe every secret,
    /// dismiss the modal, and report the cancellation.
    pub(super) fn cancel_connect_form(&mut self) {
        self.clear_connect_secrets();
        self.modal = Modal::None;
        self.status = "Provider connection cancelled; credentials were cleared.".into();
    }

    pub(super) fn handle_connect_error_key(&mut self, key: KeyEvent) {
        if key.code != KeyCode::Esc {
            return;
        }
        if let Some(form) = &mut self.provider_form {
            form.error = None;
            self.modal = Modal::ConnectSetup;
            self.status = "Connect error dismissed; edit the form and submit to retry.".into();
        } else {
            self.modal = Modal::None;
        }
    }

    pub(in crate::ui) fn dispatch_provider_connect(&mut self) {
        let Some(form) = self.provider_form.as_mut() else {
            self.clear_connect_secrets();
            self.modal = Modal::None;
            return;
        };
        form.error = None;
        let provider = form.provider.clone();
        // Pre-dispatch validation failures stay inline: the form keeps the
        // modal and the focus where they are so the user can correct the
        // offending value and press Enter again. Only a failed connect RPC
        // escalates to the persistent full-message error state.
        let Some(catalog_revision) = self.catalog_view_revision.clone() else {
            let error = "Catalog revision is unavailable.".to_owned();
            form.error = Some(error.clone());
            self.status = error;
            return;
        };
        let setup_values = match form.setup_values() {
            Ok(values) => values,
            Err(error) => {
                let error = format!("Invalid public setup: {error}");
                form.error = Some(error.clone());
                self.status = error;
                return;
            }
        };
        let auth_values = match form.auth_values() {
            Ok(values) => values,
            Err(error) => {
                let error = format!("Invalid credentials: {error}");
                form.error = Some(error.clone());
                self.status = error;
                return;
            }
        };
        let auth_method = form.auth_method.clone();
        let action = if form.reconnect {
            ProviderAction::Reconnect
        } else {
            ProviderAction::Connect
        };
        let baseline = self.runtime.revision().cloned();
        // The form keeps what was typed until the connect succeeds or the
        // form closes, so a failed attempt is retried without retyping.
        self.provider_operations
            .insert(provider.id.clone(), ProviderOperation::InProgress(action));
        self.status = format!("{} provider {}…", action_name(action), provider.id);
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let params = ProviderConnectParams {
            client_connect_id: ClientConnectId::new(Uuid::now_v7().to_string())
                .expect("uuid-derived client connect id"),
            provider_id: provider.id.clone(),
            expected_revision: catalog_revision,
            setup_values,
            auth_method,
            auth_values,
        };
        let task_provider = provider.clone();
        let task = tokio::spawn(async move {
            let connect = match client
                .connect_provider_retrying(params, &task_provider)
                .await
            {
                Ok(connect) => connect,
                Err(error) => {
                    let _ = updates.send(RpcUpdate::ProviderMutationFinished {
                        outcome: ProviderMutationOutcome::Failed {
                            provider_id: task_provider.id,
                            action,
                            error: store_contention_message(&error, "providers"),
                        },
                    });
                    return;
                }
            };
            let _ = updates.send(RpcUpdate::ProviderMutationFinished {
                outcome: ProviderMutationOutcome::Connected {
                    provider_id: connect.durable_connection.provider_id,
                    baseline,
                    runtime: Box::new(connect.runtime),
                },
            });
        });
        if let Some(previous) = self.connect_tasks.insert(provider.id, task) {
            previous.abort();
        }
    }

    pub(in crate::ui) fn clear_connect_secrets(&mut self) {
        if let Some(form) = &mut self.provider_form {
            form.wipe_secrets();
        }
        self.provider_form = None;
        self.connect_provider = None;
    }

    pub(super) fn handle_disconnect_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                self.clear_connect_secrets();
                self.modal = Modal::None;
                self.status = "Provider disconnect cancelled.".into();
            }
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => self.dispatch_provider_disconnect(),
            _ => {}
        }
    }

    pub(super) fn dispatch_provider_disconnect(&mut self) {
        let Some(provider) = self.connect_provider.clone() else {
            self.modal = Modal::None;
            return;
        };
        let Some(snapshot) = self.runtime.snapshot() else {
            self.status = "Runtime snapshot unavailable; retry before disconnecting.".into();
            return;
        };
        let baseline = Some(snapshot.revision.clone());
        let params = ProviderDisconnectParams {
            provider_id: provider.id.clone(),
            expected_revision: snapshot.revision.clone(),
            expected_connection_generation: provider
                .durable_connection
                .as_ref()
                .map(|connection| connection.connection_generation),
            client_request_id: ClientRequestId::new(Uuid::now_v7().to_string())
                .expect("uuid-derived client request id"),
        };
        self.clear_connect_secrets();
        self.modal = Modal::None;
        self.provider_operations.insert(
            provider.id.clone(),
            ProviderOperation::InProgress(ProviderAction::Disconnect),
        );
        self.status = format!("disconnect provider {}…", provider.id);
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let provider_id = provider.id.clone();
        let task = tokio::spawn(async move {
            let mut attempt = 0_usize;
            let outcome = loop {
                match client.disconnect_provider(params.clone()).await {
                    Ok(result) => {
                        break ProviderMutationOutcome::Disconnected {
                            provider_id,
                            baseline,
                            runtime: Box::new(result.runtime.snapshot),
                        };
                    }
                    Err(error) => {
                        if retry_store_contention_once(attempt, &error) {
                            attempt += 1;
                            continue;
                        }
                        break ProviderMutationOutcome::Failed {
                            provider_id,
                            action: ProviderAction::Disconnect,
                            error: store_contention_message(&error, "providers"),
                        };
                    }
                }
            };
            let _ = updates.send(RpcUpdate::ProviderMutationFinished { outcome });
        });
        if let Some(previous) = self.connect_tasks.insert(provider.id, task) {
            previous.abort();
        }
    }

    pub(in crate::ui) fn abort_connect_work(&mut self) {
        for (_, task) in self.connect_tasks.drain() {
            task.abort();
        }
    }
}
