//! The `/runtime` panel's machines view (`docs/site/specs/memory.md`
//! section 3.3): the daemon's machines with their runtimes, and renaming,
//! merging, and forgetting machines and moving runtimes between them over
//! the `machines.*` methods.

use crossterm::event::{KeyCode, KeyEvent};

use cookie_agent_client_state::runtimes::{
    AcpAgentFields, MachineEntry, acp_agent_document, acp_template_document, agent_document_path,
};
use cookie_agent_protocol::{
    AgentId, RuntimeAgentDeclaration, RuntimeId, RuntimesAgentsListParams,
    RuntimesAgentsRemoveParams, RuntimesAgentsRenameParams, RuntimesAgentsSetParams,
};

use crate::ui::management::{
    AcpAgentForm, AcpFormFocus, AgentsView, MachineAction, MachineRequest, RuntimeView,
};

use super::{App, Modal, RpcUpdate, keys::edit_plain_input};

impl App {
    /// Switches the `/runtime` panel to `view`, refreshing what it shows.
    pub(super) fn show_runtime_view(&mut self, view: RuntimeView) {
        self.runtime_panel.view = view;
        self.runtime_panel.machine_form = None;
        self.poll_runtimes();
    }

    pub(super) fn handle_machine_key(&mut self, key: KeyEvent) {
        if self.runtime_panel.agents_view.is_some() {
            self.handle_agents_key(key);
            return;
        }
        if let Some(form) = self.runtime_panel.machine_form.as_mut() {
            match key.code {
                KeyCode::Esc => self.runtime_panel.machine_form = None,
                KeyCode::Enter => match self.runtime_panel.submit_machine_form() {
                    Ok(request) => self.dispatch_machine_request(request),
                    Err(error) => self.status = error,
                },
                _ => edit_plain_input(&mut form.input, key),
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Char('m') => self.show_runtime_view(RuntimeView::Attachments),
            KeyCode::Up => self.runtime_panel.move_machine_selection(true),
            KeyCode::Down => self.runtime_panel.move_machine_selection(false),
            KeyCode::Char(key) => {
                if let Some(action) = MachineAction::of_key(key) {
                    self.begin_machine_action(action);
                }
            }
            _ => {}
        }
    }

    /// Starts `action` on the selected row: sent at once, or once the form
    /// it opens has a machine name.
    pub(super) fn begin_machine_action(&mut self, action: MachineAction) {
        if action.opens_panel() {
            let runtime = match self.runtime_panel.selected_machine_entry() {
                Some(MachineEntry::Runtime { runtime, .. }) => runtime,
                _ => {
                    self.status = format!("select a runtime to open its {}", action.label());
                    return;
                }
            };
            match action {
                MachineAction::Servers => self.open_mcp_panel(Some(runtime)),
                _ if runtime.as_str() == RuntimeId::GLOBAL => {
                    self.status = "`global` hosts no ACP agents".into();
                }
                _ => {
                    self.runtime_panel.agents_view = Some(AgentsView::new(runtime.clone()));
                    self.read_runtime_agents(runtime, None);
                }
            }
            return;
        }
        match self.runtime_panel.begin_machine_action(action) {
            Ok(Some(request)) => self.dispatch_machine_request(request),
            Ok(None) => {}
            Err(error) => self.status = error,
        }
    }

    fn dispatch_machine_request(&mut self, request: MachineRequest) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let done = request.done();
            let result = match request {
                MachineRequest::Rename { machine, name } => {
                    client.rename_machine(machine, name).await
                }
                MachineRequest::Assign { runtime, machine } => {
                    client.assign_machine(runtime, machine).await
                }
                MachineRequest::Unassign { runtime } => client.unassign_machine(runtime).await,
                MachineRequest::Merge { from, into } => client.merge_machines(from, into).await,
                MachineRequest::Forget { machine } => client.forget_machine(machine).await,
                MachineRequest::ForgetRuntime { runtime } => {
                    client.forget_machine_runtime(runtime).await
                }
            };
            let result = result
                .map(|machines| (done, machines))
                .map_err(|error| format!("machine change failed: {error}"));
            let _ = updates.send(RpcUpdate::MachineChanged { result });
        });
    }

    /// Fetches `machines.list` for the open `/runtime` panel.
    pub(super) fn poll_machines(&mut self) {
        if self.modal != Modal::Runtimes || !self.runtime_panel.machines_refresh.start() {
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_machines()
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::MachinesRefreshed { result });
        });
    }

    /// Installs `machines.list`, and lists again when a change was
    /// announced while it was on its way.
    pub(super) fn install_machine_listing(
        &mut self,
        result: Result<cookie_agent_protocol::MachinesListResult, String>,
    ) {
        let again = self.runtime_panel.machines_refresh.landed();
        self.runtime_panel.install_machines(result);
        if again {
            self.poll_machines();
        }
    }

    pub(super) fn install_machine_change(
        &mut self,
        result: Result<(String, cookie_agent_protocol::MachinesListResult), String>,
    ) {
        match result {
            Ok((done, machines)) => {
                self.status = done;
                self.runtime_panel.install_machines(Ok(machines));
            }
            Err(error) => self.status = error,
        }
        self.poll_runtimes();
    }

    /// The agents view's keys: its form's while one is open.
    fn handle_agents_key(&mut self, key: KeyEvent) {
        let Some(view) = self.runtime_panel.agents_view.as_mut() else {
            return;
        };
        if let Some(form) = view.form.as_mut() {
            match key.code {
                KeyCode::Esc => view.form = None,
                KeyCode::Tab | KeyCode::BackTab => form.move_focus(key.code == KeyCode::BackTab),
                KeyCode::Char(' ') if form.focus == AcpFormFocus::Enabled => {
                    form.enabled = !form.enabled;
                }
                KeyCode::Enter => {
                    let (fields, editing) = (form.fields(), form.editing.clone());
                    let runtime = view.runtime.clone();
                    match self.submit_runtime_agent(runtime, &fields, editing) {
                        Ok(()) => {
                            if let Some(view) = self.runtime_panel.agents_view.as_mut() {
                                view.form = None;
                            }
                        }
                        Err(error) => self.status = error,
                    }
                }
                _ => {
                    if let Some(input) = form.focused_input() {
                        edit_plain_input(input, key);
                    }
                }
            }
            return;
        }
        let runtime = view.runtime.clone();
        let selected = view.selected().cloned();
        match key.code {
            KeyCode::Esc => self.runtime_panel.agents_view = None,
            KeyCode::Up => view.move_selection(true),
            KeyCode::Down => view.move_selection(false),
            KeyCode::Char('n') => view.form = Some(AcpAgentForm::new(&AcpAgentFields::add(), None)),
            KeyCode::Char('e') => {
                if let Some(declaration) = selected {
                    view.form = Some(AcpAgentForm::new(
                        &AcpAgentFields::edit(&declaration),
                        Some(declaration.agent),
                    ));
                }
            }
            KeyCode::Char('d') => {
                if let Some(declaration) = selected {
                    self.remove_runtime_agent(runtime, declaration.agent);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(declaration) = selected {
                    let mut fields = AcpAgentFields::edit(&declaration);
                    fields.enabled = !fields.enabled;
                    if let Err(error) =
                        self.submit_runtime_agent(runtime, &fields, Some(declaration.agent))
                    {
                        self.status = error;
                    }
                }
            }
            KeyCode::Char('o') => {
                if let Some(declaration) = selected {
                    let agent = declaration.agent.as_str();
                    self.fetch_agent_document(
                        agent_document_path(agent),
                        acp_agent_document(agent, &declaration.definition.description),
                    );
                }
            }
            KeyCode::Char('t') => {
                if let Some(declaration) = selected {
                    let agent = declaration.agent.as_str();
                    self.fetch_agent_document(
                        agent_document_path(&format!("{agent}-template")),
                        acp_template_document(agent),
                    );
                }
            }
            _ => {}
        }
    }

    /// Reads `runtime`'s ACP agent declarations into the agents view;
    /// `status` is what the change that led to it did.
    fn read_runtime_agents(&mut self, runtime: RuntimeId, status: Option<String>) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_runtime_agents(RuntimesAgentsListParams {
                    runtime_id: runtime.clone(),
                })
                .await
                .map(|result| result.agents)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::RuntimeAgents {
                runtime,
                result,
                status,
            });
        });
    }

    /// Adds or edits one of `runtime`'s declarations; a changed key renames
    /// it first.
    fn submit_runtime_agent(
        &mut self,
        runtime: RuntimeId,
        fields: &AcpAgentFields,
        editing: Option<AgentId>,
    ) -> Result<(), String> {
        let (agent, definition) = fields.definition()?;
        let renamed = editing.filter(|old| *old != agent);
        self.status = format!("saving ACP agent `{agent}`…");
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let mut status = format!("ACP agent `{agent}` saved");
            let result = async {
                if let Some(old) = renamed {
                    let renamed = client
                        .rename_runtime_agent(RuntimesAgentsRenameParams {
                            runtime_id: runtime.clone(),
                            agent: old.clone(),
                            new_agent: agent.clone(),
                        })
                        .await
                        .map_err(|error| error.to_string())?;
                    status = format!("ACP agent `{old}` is `{agent}` now");
                    if !renamed.moved.is_empty() {
                        status.push_str(&format!("; moved {}", renamed.moved.join(", ")));
                    }
                    if !renamed.stale_templates.is_empty() {
                        status.push_str(&format!(
                            "; {} still name `{old}` in acp.agent",
                            renamed.stale_templates.join(", ")
                        ));
                    }
                }
                client
                    .set_runtime_agent(RuntimesAgentsSetParams {
                        runtime_id: runtime.clone(),
                        agent: agent.clone(),
                        definition,
                    })
                    .await
                    .map(|result| result.agents)
                    .map_err(|error| error.to_string())
            }
            .await;
            let _ = updates.send(RpcUpdate::RuntimeAgents {
                runtime,
                result,
                status: Some(status),
            });
        });
        Ok(())
    }

    fn remove_runtime_agent(&mut self, runtime: RuntimeId, agent: AgentId) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let status = format!("ACP agent `{agent}` removed; its documents stay");
            let result = client
                .remove_runtime_agent(RuntimesAgentsRemoveParams {
                    runtime_id: runtime.clone(),
                    agent,
                })
                .await
                .map(|result| result.agents)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::RuntimeAgents {
                runtime,
                result,
                status: Some(status),
            });
        });
    }

    pub(super) fn install_runtime_agents(
        &mut self,
        runtime: &RuntimeId,
        result: Result<Vec<RuntimeAgentDeclaration>, String>,
        status: Option<String>,
    ) {
        match (&result, status) {
            (Err(error), Some(_)) => self.status = format!("ACP agent change failed: {error}"),
            (Ok(_), Some(status)) => self.status = status,
            (_, None) => {}
        }
        if let Some(view) = self
            .runtime_panel
            .agents_view
            .as_mut()
            .filter(|view| view.runtime == *runtime)
        {
            // A failed change leaves the list it had.
            if result.is_ok() || view.declarations.is_none() {
                view.install(result);
            }
        }
    }

    /// Reads the configuration to open agent document `path` in the
    /// editor, as `template` when it does not exist.
    fn fetch_agent_document(&mut self, path: String, template: String) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client.get_config().await.map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::AgentDocument {
                path,
                template,
                result,
            });
        });
    }

    pub(super) fn open_agent_document(
        &mut self,
        path: String,
        template: String,
        result: Result<cookie_agent_protocol::ConfigGetResult, String>,
    ) {
        let config = match result {
            Ok(config) => config,
            Err(error) => {
                self.status = format!("configuration unavailable: {error}");
                return;
            }
        };
        let document = config
            .documents
            .iter()
            .find(|document| document.path == path);
        self.pending_editor = Some(super::config_panel::EditorRequest {
            text: document.map_or(template, |document| document.text.clone()),
            original: document.map(|document| document.text.clone()),
            expected_fingerprint: document.map(|document| document.fingerprint.clone()),
            revision: config.revision.clone(),
            path,
        });
    }
}
