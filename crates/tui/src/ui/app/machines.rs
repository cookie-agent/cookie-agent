//! The `/runtime` panel's machines view (`docs/site/specs/memory.md`
//! section 3.3): the daemon's machines with their runtimes, and renaming,
//! merging, and forgetting machines and moving runtimes between them over
//! the `machines.*` methods.

use crossterm::event::{KeyCode, KeyEvent};

use crate::ui::management::{MachineAction, MachineRequest, RuntimeView};

use super::{App, Modal, RpcUpdate, keys::edit_plain_input};

impl App {
    /// Switches the `/runtime` panel to `view`, refreshing what it shows.
    pub(super) fn show_runtime_view(&mut self, view: RuntimeView) {
        self.runtime_panel.view = view;
        self.runtime_panel.machine_form = None;
        self.poll_runtimes();
    }

    pub(super) fn handle_machine_key(&mut self, key: KeyEvent) {
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
        match self.runtime_panel.begin_machine_action(action) {
            Ok(Some(request)) => self.dispatch_machine_request(request),
            Ok(None) => {}
            Err(error) => self.status = error,
        }
    }

    fn dispatch_machine_request(&mut self, request: MachineRequest) {
        self.status = "changing machines...".into();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let (done, result) = match request {
                MachineRequest::Rename { machine, name } => (
                    format!("renamed machine {machine} to {name}"),
                    client.rename_machine(machine, name).await,
                ),
                MachineRequest::Assign { runtime, machine } => (
                    format!("pinned runtime {runtime} to machine {machine}"),
                    client.assign_machine(runtime, machine).await,
                ),
                MachineRequest::Unassign { runtime } => (
                    format!("unpinned runtime {runtime}"),
                    client.unassign_machine(runtime).await,
                ),
                MachineRequest::Merge { from, into } => (
                    format!("merged machine {from} into {into}"),
                    client.merge_machines(from, into).await,
                ),
                MachineRequest::Forget { machine } => (
                    format!("forgot machine {machine}"),
                    client.forget_machine(machine).await,
                ),
            };
            let result = result
                .map(|machines| (done, machines))
                .map_err(|error| format!("machine change failed: {error}"));
            let _ = updates.send(RpcUpdate::MachineChanged { result });
        });
    }

    /// Fetches `machines.list` for the open `/runtime` panel.
    pub(super) fn poll_machines(&mut self) {
        if self.modal != Modal::Runtimes || self.runtime_panel.machines_in_flight {
            return;
        }
        self.runtime_panel.machines_in_flight = true;
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
}
