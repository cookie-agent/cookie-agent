//! The daemon's management panels, as the TUI has them: `/runtime` (the
//! session's runtimes, a new session's draft runtimes, and the machines),
//! `/mcp`, `/connect`, `/config`, `/memory`, and `/plugins`. Their rules
//! come from `cookie_agent_client_state`; this is the calls and the state
//! the sheets show.

use std::time::Duration;

use cookie_agent_client_state::{
    config::{ConfigApplyOutcome, document_text, new_agent_refusal, reload_status, restart_note},
    mcp::{
        McpFields, PersistChoice, edit_refusal, is_global, mutation_runtime, new_definition,
        sort_servers, toggle_tool,
    },
    memory::{
        MemoryParts, MemoryStamps, MemoryWriteOutcome, NEW_MEMORY, delete_refusal, editor_text,
        memory_parts, new_memory_parts, new_memory_refusal, saved_status,
    },
    providers::{
        ProviderAction, ProviderOperation, action_name, retry_store_contention_once,
        store_contention_message,
    },
    runtimes::{
        AcpAgentFields, MachineAction, MachineRequest, RuntimeRow, connection_request,
        connection_status, reconnect_status, runtime_rows,
    },
};
use cookie_agent_protocol::{
    AgentId, AttachmentName, CapabilityName, ClientConnectId, ClientDelivery, ClientRequestId,
    ConfigApplyParams, ConfigChange, ConfigRevision, McpServerAddParams, McpServerEditParams,
    McpServerInfo, McpServerNameParams, McpServerPersistParams, McpServerReconnectParams,
    McpServerRenameParams, McpServerScope, McpServerSetEnabledParams, MemoryListParams,
    ProviderConnectParams, ProviderDescriptor, ProviderDisconnectParams, RuntimeAgentDeclaration,
    RuntimeId, RuntimesAgentsListParams, RuntimesAgentsRemoveParams, RuntimesAgentsRenameParams,
    RuntimesAgentsRenameResult, RuntimesAgentsSetParams, RuntimesConnectParams,
    RuntimesReconnectParams, SessionRuntimeAttachParams, SessionRuntimeDetachParams, errors,
};
use leptos::{prelude::*, task::spawn_local};

use super::{Panel, Workspace};
use crate::{
    layout::{SettingsSection, View},
    machines::{SshTemplate, has_template},
    memory::{folder_key, project_delete_status, project_memories, project_strays},
};

/// How often an open `/runtime` or `/mcp` panel reads its list again, as the
/// TUI polls.
pub const PANEL_POLL: Duration = Duration::from_secs(1);

/// Each runtime's ACP agent declarations, as the Machines view last read
/// them.
pub type RuntimeAgentsSignal =
    RwSignal<std::collections::BTreeMap<RuntimeId, Result<Vec<RuntimeAgentDeclaration>, String>>>;

/// What renaming declaration `old` to `new` did to its documents.
fn rename_status(old: &AgentId, new: &AgentId, renamed: &RuntimesAgentsRenameResult) -> String {
    let mut status = format!("ACP agent `{old}` is `{new}` now");
    if !renamed.moved.is_empty() {
        status.push_str(&format!("; moved {}", renamed.moved.join(", ")));
    }
    if !renamed.stale_templates.is_empty() {
        status.push_str(&format!(
            "; {} still name `{old}` in acp.agent",
            renamed.stale_templates.join(", ")
        ));
    }
    status
}

/// A config document open in the editor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigEdit {
    pub path: String,
    pub text: String,
    /// The document's text on disk; `None` for a new one.
    pub original: Option<String>,
    pub expected_fingerprint: Option<String>,
    pub revision: ConfigRevision,
}

/// A memory open in the editor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryEdit {
    pub url: String,
    /// The text, without the stamp lines a daemon that owns them rewrites
    /// on every save.
    pub text: String,
    /// The text as read, likewise; the template for a new memory.
    pub original: String,
    /// The revision read; `None` for a new memory.
    pub revision: Option<String>,
    /// Its stamps and size as read, shown read-only; `None` for a new
    /// memory.
    pub stamps: Option<MemoryStamps>,
    /// Its parts as read, when the editor shows them as fields; `None`
    /// when it shows the text whole.
    pub read_parts: Option<MemoryParts>,
    /// Its parts as last typed, which `text` writes; the fields open with
    /// them.
    pub parts: Option<MemoryParts>,
}

/// Why a memory write was not saved, as the panel offers to go on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryPending {
    /// The text is not a valid memory: edit it again, or drop it.
    Invalid { edit: MemoryEdit, message: String },
    /// The memory changed, appeared, or went away since it was read:
    /// reload it, overwrite it with this text, or drop the text.
    Changed { edit: MemoryEdit },
}

impl Workspace {
    /// Reads again what the open panel shows after a `*.changed`
    /// notification.
    pub(super) fn refresh_open_panel(self, delivery: &ClientDelivery) {
        let runtimes = self.panel.get_untracked() == Some(Panel::Runtimes)
            || self.showing(View::Machines, None)
            || (self.new_session.get_untracked() && self.showing(View::Chat, None));
        match delivery {
            ClientDelivery::RuntimesChanged(_) if runtimes => self.poll_runtimes(),
            ClientDelivery::MachinesChanged(_) => {
                if runtimes {
                    self.poll_runtimes();
                }
                // Renaming or merging a machine moves its memories too.
                if self.showing(View::Memory, None) {
                    self.poll_memory();
                }
            }
            ClientDelivery::McpChanged(_)
                if runtimes || self.showing(View::Settings, Some(SettingsSection::Mcp)) =>
            {
                self.poll_mcp();
            }
            ClientDelivery::MemoryChanged(_) if self.showing(View::Memory, None) => {
                self.poll_memory();
            }
            ClientDelivery::PluginsChanged(_)
                if self.showing(View::Settings, Some(SettingsSection::Plugins)) =>
            {
                self.poll_plugins();
            }
            _ => {}
        }
    }

    // ---- /runtime ---------------------------------------------------------

    /// The session's runtimes sheet. A new session's runtimes are its
    /// cards, above the composer: the page shows them instead.
    pub(super) fn open_runtimes(self) {
        if self.new_session.get_untracked() {
            self.panel.set(None);
            self.status
                .set("the new session's runtimes are the cards above the composer".into());
            crate::browser::reveal("#new-session");
            return;
        }
        self.panel.set(Some(Panel::Runtimes));
        self.poll_runtimes();
    }

    /// Reads `runtimes.list`, `machines.list`, and `mcp.server.list`.
    pub fn poll_runtimes(self) {
        let Some(client) = self.client() else {
            return;
        };
        let read = self.begin_read(|reads| &mut reads.runtimes);
        spawn_local(async move {
            let runtimes = client.list_runtimes().await;
            let machines = client
                .list_machines()
                .await
                .map_err(|error| crate::errors::describe_full(&error));
            if !self.read_is_current(|reads| &mut reads.runtimes, read) {
                return;
            }
            match runtimes {
                Ok(listing) => self.runtimes.set(Some(listing)),
                Err(error) => self.status.set(format!(
                    "runtimes unavailable: {}",
                    crate::errors::describe(&error)
                )),
            }
            self.machines.set(Some(machines));
        });
        // The MCP servers each runtime hosts, which its rows list.
        self.poll_mcp();
    }

    /// The attachments the panel shows: the viewed session's.
    pub fn runtime_rows(self) -> Vec<RuntimeRow> {
        let listing = self.runtimes.get();
        if self.new_session.get() {
            return Vec::new();
        }
        let Some(session_id) = self.selected.get() else {
            return Vec::new();
        };
        self.with_store(|store| {
            store
                .sessions
                .get(&session_id)
                .map(|state| {
                    runtime_rows(state.runtime_attachments.attachments(), listing.as_ref())
                })
                .unwrap_or_default()
        })
    }

    /// Attaches `runtime_id`'s `capability` at `workspace` to the viewed
    /// session.
    pub fn attach_runtime(
        self,
        runtime_id: RuntimeId,
        capability: Option<CapabilityName>,
        workspace: Option<String>,
    ) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            self.status
                .set("select a session before attaching a runtime".into());
            return;
        };
        self.status
            .set(format!("attaching runtime `{runtime_id}`…"));
        spawn_local(async move {
            let status = match client
                .attach_runtime(SessionRuntimeAttachParams {
                    session_id,
                    runtime_id,
                    capability,
                    workspace,
                })
                .await
            {
                Ok(result) => format!("attached runtime as `{}`", result.attachment.name),
                Err(error) => format!("attach failed: {}", crate::errors::describe(&error)),
            };
            self.status.set(status);
            self.poll_runtimes();
        });
    }

    /// Detaches the viewed session's attachment `name`.
    pub fn detach_runtime(self, name: AttachmentName) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        spawn_local(async move {
            let status = match client
                .detach_runtime(SessionRuntimeDetachParams {
                    session_id,
                    name: name.clone(),
                })
                .await
            {
                Ok(_) => format!("detached runtime `{name}`"),
                Err(error) => format!("detach failed: {}", crate::errors::describe(&error)),
            };
            self.status.set(status);
            self.poll_runtimes();
        });
    }

    /// Retries `row`'s instance (`runtimes.reconnect`).
    pub fn retry_runtime(self, row: RuntimeRow) {
        let Some(client) = self.client() else {
            return;
        };
        let runtimes = self
            .runtimes
            .with_untracked(|listing| listing.as_ref().map(|listing| listing.runtimes.clone()))
            .unwrap_or_default();
        let (runtime_id, capability, workspace) =
            match cookie_agent_client_state::runtimes::reconnect_request(&runtimes, &row) {
                Ok(request) => request,
                Err(error) => {
                    self.status.set(error);
                    return;
                }
            };
        self.status.set(format!("retrying runtime `{runtime_id}`…"));
        spawn_local(async move {
            let status = match client
                .reconnect_runtime(RuntimesReconnectParams {
                    runtime_id: runtime_id.clone(),
                    capability: Some(capability),
                    workspace,
                })
                .await
            {
                Ok(result) => reconnect_status(&runtime_id, &result.instance),
                Err(error) => format!("retry failed: {}", crate::errors::describe(&error)),
            };
            self.status.set(status);
            self.poll_runtimes();
        });
    }

    /// Connects runtime `runtime_id` (`runtimes.connect`), or disconnects it
    /// (`runtimes.disconnect`), when its listing allows.
    pub fn set_runtime_connected(self, runtime_id: RuntimeId, connect: bool) {
        let Some(client) = self.client() else {
            return;
        };
        let runtimes = self
            .runtimes
            .with_untracked(|listing| listing.as_ref().map(|listing| listing.runtimes.clone()))
            .unwrap_or_default();
        let action = if connect {
            MachineAction::Connect
        } else {
            MachineAction::Disconnect
        };
        if let Err(error) = connection_request(&runtimes, &runtime_id, action) {
            self.status.set(error);
            return;
        }
        if !self
            .connecting
            .try_update(|connecting| connecting.insert(runtime_id.clone()))
            .unwrap_or(false)
        {
            return;
        }
        self.status.set(if connect {
            format!("connecting runtime `{runtime_id}`…")
        } else {
            format!("disconnecting runtime `{runtime_id}`…")
        });
        spawn_local(async move {
            let params = RuntimesConnectParams {
                runtime_id: runtime_id.clone(),
            };
            let result = if connect {
                client.connect_runtime(params).await
            } else {
                client.disconnect_runtime(params).await
            };
            let status = match result {
                Ok(result) => connection_status(&result.runtime),
                Err(error) if connect => format!(
                    "runtime `{runtime_id}` did not connect: {}",
                    crate::errors::describe(&error)
                ),
                Err(error) => format!(
                    "runtime `{runtime_id}` did not disconnect: {}",
                    crate::errors::describe(&error)
                ),
            };
            self.connecting.update(|connecting| {
                connecting.remove(&runtime_id);
            });
            self.status.set(status);
            self.poll_runtimes();
        });
    }

    /// Sends a machine change; the error reads as the daemon's registry
    /// error says.
    pub fn change_machines(self, request: MachineRequest) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
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
            match result {
                Ok(machines) => {
                    self.status.set(done);
                    self.machines.set(Some(Ok(machines)));
                }
                Err(error) => self.status.set(format!(
                    "machine change failed: {}",
                    crate::errors::describe(&error)
                )),
            }
            self.poll_runtimes();
        });
    }

    /// Writes `template` into `config.toml` with a `config.apply` set of
    /// its `[runtimes.<id>]` table, unless the daemon lists a runtime of
    /// that ID or the file has a template of it. A set does not depend on
    /// the rest of the file, so a revision that moved meanwhile is read
    /// again once. `outcome` gets what the form says.
    pub fn add_runtime_template(
        self,
        template: SshTemplate,
        outcome: RwSignal<Option<Result<String, String>>>,
    ) {
        let Some(client) = self.client() else {
            return;
        };
        let id = template.id.clone();
        let listed = self.runtimes.with_untracked(|listing| {
            listing.as_ref().is_some_and(|listing| {
                listing
                    .runtimes
                    .iter()
                    .any(|runtime| runtime.runtime_id == id)
            })
        });
        if listed {
            outcome.set(Some(Err(format!(
                "The daemon has a runtime `{id}` already."
            ))));
            return;
        }
        outcome.set(None);
        self.status.set(format!("adding runtime {id}…"));
        spawn_local(async move {
            let mut retried = false;
            let result = loop {
                let config = match client.get_config().await {
                    Ok(config) => config,
                    Err(error) => break Err(crate::errors::describe(&error)),
                };
                let text = config
                    .documents
                    .iter()
                    .find(|document| document.path == "config.toml")
                    .map_or("", |document| document.text.as_str());
                if has_template(text, &id) {
                    break Err(format!(
                        "config.toml has a [runtimes.{id}] template already."
                    ));
                }
                match ConfigApplyOutcome::from_result(
                    client
                        .apply_config(ConfigApplyParams {
                            expected_revision: config.revision,
                            changes: vec![template.change()],
                        })
                        .await,
                ) {
                    ConfigApplyOutcome::Refused {
                        reason: cookie_agent_protocol::ConfigApplyErrorCode::Conflict,
                        ..
                    } if !retried => retried = true,
                    ConfigApplyOutcome::Applied(status) => {
                        break Ok(format!(
                            "Added [runtimes.{id}] to config.toml.{}",
                            restart_note(&status)
                        ));
                    }
                    ConfigApplyOutcome::Refused { detail, .. } => {
                        break Err(format!("config.toml was not changed: {detail}"));
                    }
                    ConfigApplyOutcome::Failed(error) => break Err(error),
                }
            };
            self.status.set(match &result {
                Ok(done) | Err(done) => done.clone(),
            });
            outcome.set(Some(result));
            if self.config.with_untracked(Option::is_some) {
                self.poll_config();
            }
        });
    }

    // ---- /mcp -------------------------------------------------------------

    pub fn poll_mcp(self) {
        let Some(client) = self.client() else {
            return;
        };
        let read = self.begin_read(|reads| &mut reads.mcp);
        spawn_local(async move {
            let result = client
                .list_mcp_servers()
                .await
                .map(|listing| {
                    let mut servers = listing.servers;
                    sort_servers(&mut servers);
                    servers
                })
                .map_err(|error| crate::errors::describe_full(&error));
            if self.read_is_current(|reads| &mut reads.mcp, read) {
                self.mcp_servers.set(Some(result));
            }
        });
    }

    /// Why `server` cannot be changed here, if it cannot.
    pub fn mcp_refusal(server: &McpServerInfo) -> Option<String> {
        edit_refusal(server)
    }

    /// Adds or edits a server from its form. `editing` names the edited
    /// server, whose runtime the change goes to; a new one goes to
    /// `runtime`, `global` when `None`. A changed name renames the server
    /// first. `scope` is a runtime server's, and `persist` says where a
    /// `global` one is kept.
    pub fn submit_mcp(
        self,
        fields: McpFields,
        editing: Option<McpServerInfo>,
        persist: PersistChoice,
        runtime: Option<RuntimeId>,
        scope: Option<McpServerScope>,
    ) -> Result<(), String> {
        let base = editing
            .as_ref()
            .map_or_else(new_definition, |server| server.definition.clone());
        let (name, mut definition) = fields.definition(&base)?;
        let runtime_id = match &editing {
            Some(server) => mutation_runtime(server),
            None => runtime.filter(|runtime| runtime.as_str() != RuntimeId::GLOBAL),
        };
        if runtime_id.is_some() {
            definition.scope = scope;
        }
        let renamed = editing
            .as_ref()
            .map(|server| server.name.clone())
            .filter(|old| *old != name);
        let Some(client) = self.client() else {
            return Err("not connected".into());
        };
        let target = persist.target().filter(|_| runtime_id.is_none());
        spawn_local(async move {
            let result = async {
                if let Some(old) = renamed {
                    client
                        .rename_mcp_server(McpServerRenameParams {
                            runtime_id: runtime_id.clone(),
                            name: old,
                            new_name: name.clone(),
                        })
                        .await
                        .map_err(|error| crate::errors::describe_full(&error))?;
                }
                let mutation = if editing.is_some() {
                    client
                        .edit_mcp_server(McpServerEditParams {
                            runtime_id,
                            name: name.clone(),
                            definition,
                        })
                        .await
                } else {
                    client
                        .add_mcp_server(McpServerAddParams {
                            runtime_id,
                            name: name.clone(),
                            definition,
                        })
                        .await
                }
                .map_err(|error| crate::errors::describe_full(&error))?;
                if let Some(target) = target {
                    return client
                        .persist_mcp_server(McpServerPersistParams { name, target })
                        .await
                        .map(|result| result.server)
                        .map_err(|error| crate::errors::describe_full(&error));
                }
                Ok(mutation.server)
            }
            .await;
            self.finish_mcp(result);
        });
        Ok(())
    }

    pub fn remove_mcp(self, server: &McpServerInfo) {
        let Some(client) = self.client() else {
            return;
        };
        let params = McpServerNameParams {
            runtime_id: mutation_runtime(server),
            name: server.name.clone(),
        };
        spawn_local(async move {
            let result = client
                .remove_mcp_server(params)
                .await
                .map(|result| result.server)
                .map_err(|error| crate::errors::describe_full(&error));
            self.finish_mcp(result);
        });
    }

    pub fn toggle_mcp(self, server: &McpServerInfo, enabled: bool) {
        let Some(client) = self.client() else {
            return;
        };
        let params = McpServerSetEnabledParams {
            runtime_id: mutation_runtime(server),
            name: server.name.clone(),
            enabled,
        };
        spawn_local(async move {
            let result = client
                .set_mcp_server_enabled(params)
                .await
                .map(|result| result.server)
                .map_err(|error| crate::errors::describe_full(&error));
            self.finish_mcp(result);
        });
    }

    /// Leaves `tool` of `server` out (`leave_out`) or takes it back, by
    /// editing the server's filters.
    pub fn toggle_mcp_tool(self, server: &McpServerInfo, tool: &str, leave_out: bool) {
        let definition = match toggle_tool(&server.definition, tool, leave_out) {
            Ok(definition) => definition,
            Err(refusal) => {
                self.status.set(refusal);
                return;
            }
        };
        let Some(client) = self.client() else {
            return;
        };
        let params = McpServerEditParams {
            runtime_id: mutation_runtime(server),
            name: server.name.clone(),
            definition,
        };
        spawn_local(async move {
            let result = client
                .edit_mcp_server(params)
                .await
                .map(|result| result.server)
                .map_err(|error| crate::errors::describe_full(&error));
            self.finish_mcp(result);
        });
    }

    // ---- a runtime's ACP agents (the Machines view) -------------------------

    /// Reads `runtime`'s ACP agent declarations into `into`.
    pub fn read_runtime_agents(self, runtime: RuntimeId, into: RuntimeAgentsSignal) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let result = client
                .list_runtime_agents(RuntimesAgentsListParams {
                    runtime_id: runtime.clone(),
                })
                .await
                .map(|result| result.agents)
                .map_err(|error| crate::errors::describe_full(&error));
            into.update(|agents| {
                agents.insert(runtime, result);
            });
        });
    }

    /// Adds or edits one of `runtime`'s ACP agent declarations from its form;
    /// `editing` names the edited one, which a changed key renames first.
    pub fn submit_runtime_agent(
        self,
        runtime: RuntimeId,
        fields: &AcpAgentFields,
        editing: Option<AgentId>,
        into: RuntimeAgentsSignal,
    ) -> Result<(), String> {
        let (agent, definition) = fields.definition()?;
        let renamed = editing.filter(|old| *old != agent);
        let Some(client) = self.client() else {
            return Err("not connected".into());
        };
        spawn_local(async move {
            let result = async {
                if let Some(old) = renamed {
                    let renamed = client
                        .rename_runtime_agent(RuntimesAgentsRenameParams {
                            runtime_id: runtime.clone(),
                            agent: old.clone(),
                            new_agent: agent.clone(),
                        })
                        .await
                        .map_err(|error| crate::errors::describe_full(&error))?;
                    self.status.set(rename_status(&old, &agent, &renamed));
                }
                client
                    .set_runtime_agent(RuntimesAgentsSetParams {
                        runtime_id: runtime.clone(),
                        agent,
                        definition,
                    })
                    .await
                    .map(|result| result.agents)
                    .map_err(|error| crate::errors::describe_full(&error))
            }
            .await;
            self.finish_runtime_agents(runtime, result, into);
        });
        Ok(())
    }

    /// Removes one of `runtime`'s ACP agent declarations; its documents stay.
    pub fn remove_runtime_agent(
        self,
        runtime: RuntimeId,
        agent: AgentId,
        into: RuntimeAgentsSignal,
    ) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let result = client
                .remove_runtime_agent(RuntimesAgentsRemoveParams {
                    runtime_id: runtime.clone(),
                    agent,
                })
                .await
                .map(|result| result.agents)
                .map_err(|error| crate::errors::describe_full(&error));
            self.finish_runtime_agents(runtime, result, into);
        });
    }

    fn finish_runtime_agents(
        self,
        runtime: RuntimeId,
        result: Result<Vec<RuntimeAgentDeclaration>, String>,
        into: RuntimeAgentsSignal,
    ) {
        if let Err(error) = &result {
            self.status.set(format!("ACP agent change failed: {error}"));
        }
        into.update(|agents| {
            agents.insert(runtime, result);
        });
        // The configuration changed; the Agents view reads it afresh.
        self.poll_config();
    }

    /// Opens agent document `path` in the Agents view's editor: the
    /// document when it exists, else `template` as a new one.
    pub fn open_agent_document(self, path: &str, template: String) {
        let Some(Ok(config)) = self.config.get_untracked() else {
            self.status.set("the configuration is still loading".into());
            return;
        };
        self.open_view(View::Agents, None);
        if config
            .documents
            .iter()
            .any(|document| document.path == path)
        {
            self.edit_config(path);
            return;
        }
        self.config_edit.set(Some(ConfigEdit {
            path: path.to_owned(),
            text: template,
            original: None,
            expected_fingerprint: None,
            revision: config.revision.clone(),
        }));
    }

    /// Reconnects any runtime's server; the request goes to the runtime
    /// that hosts it.
    pub fn reconnect_mcp(self, server: &McpServerInfo) {
        let Some(client) = self.client() else {
            return;
        };
        let params = McpServerReconnectParams {
            name: server.name.clone(),
            runtime_id: (!is_global(server)).then(|| server.runtime_id.clone()),
            workspace: server.workspace.clone(),
        };
        spawn_local(async move {
            let result = client
                .reconnect_mcp_server(params)
                .await
                .map(|result| result.server)
                .map_err(|error| crate::errors::describe_full(&error));
            self.finish_mcp(result);
        });
    }

    fn finish_mcp(self, result: Result<Option<McpServerInfo>, String>) {
        self.status.set(match result {
            Ok(Some(server)) => format!(
                "MCP server `{}` is {}",
                server.name,
                cookie_agent_client_state::mcp::state_label(server.state)
            ),
            Ok(None) => "MCP server removed".into(),
            Err(error) => format!("MCP change failed: {error}"),
        });
        self.poll_mcp();
    }

    // ---- /connect ---------------------------------------------------------

    /// The providers the catalog lists.
    pub fn providers(self) -> Vec<ProviderDescriptor> {
        self.with_runtime(|runtime| {
            runtime
                .snapshot()
                .map(|snapshot| snapshot.providers.clone())
                .unwrap_or_default()
        })
    }

    /// Connects `provider` with what its form holds; the credentials are
    /// read from the form once and only travel in the request.
    pub fn connect_provider(
        self,
        provider: ProviderDescriptor,
        auth_method: cookie_agent_protocol::AuthMethodId,
        setup_values: std::collections::BTreeMap<
            cookie_agent_protocol::SetupFieldId,
            cookie_agent_protocol::SafeSetupValue,
        >,
        auth_values: cookie_agent_protocol::ProviderCredentialValues,
        reconnect: bool,
    ) -> Result<(), String> {
        let Some(client) = self.client() else {
            return Err("not connected".into());
        };
        let Some(expected_revision) = self
            .runtime
            .with_value(|runtime| runtime.revision().cloned())
        else {
            return Err("Catalog revision is unavailable.".into());
        };
        let action = if reconnect {
            ProviderAction::Reconnect
        } else {
            ProviderAction::Connect
        };
        let provider_id = provider.id.clone();
        self.provider_operations.update(|operations| {
            operations.insert(provider_id.clone(), ProviderOperation::InProgress(action));
        });
        self.status
            .set(format!("{} provider {provider_id}…", action_name(action)));
        let params = ProviderConnectParams {
            client_connect_id: ClientConnectId::new(uuid::Uuid::now_v7().to_string())
                .expect("uuid-derived client connect id"),
            provider_id: provider_id.clone(),
            expected_revision,
            setup_values,
            auth_method,
            auth_values,
        };
        spawn_local(async move {
            let result = client.connect_provider_retrying(params, &provider).await;
            self.finish_provider(provider_id, action, result.map(|_| ()));
        });
        Ok(())
    }

    /// Disconnects `provider`'s stored connection.
    pub fn disconnect_provider(self, provider: ProviderDescriptor) {
        let Some(client) = self.client() else {
            return;
        };
        let Some(expected_revision) = self
            .runtime
            .with_value(|runtime| runtime.revision().cloned())
        else {
            self.status
                .set("Runtime snapshot unavailable; retry before disconnecting.".into());
            return;
        };
        let params = ProviderDisconnectParams {
            provider_id: provider.id.clone(),
            expected_revision,
            expected_connection_generation: provider
                .durable_connection
                .as_ref()
                .map(|connection| connection.connection_generation),
            client_request_id: ClientRequestId::new(uuid::Uuid::now_v7().to_string())
                .expect("uuid-derived client request id"),
        };
        let provider_id = provider.id.clone();
        self.provider_operations.update(|operations| {
            operations.insert(
                provider_id.clone(),
                ProviderOperation::InProgress(ProviderAction::Disconnect),
            );
        });
        self.status
            .set(format!("disconnect provider {provider_id}…"));
        spawn_local(async move {
            let mut attempt = 0;
            let result = loop {
                match client.disconnect_provider(params.clone()).await {
                    Ok(_) => break Ok(()),
                    Err(error) if retry_store_contention_once(attempt, &error) => attempt += 1,
                    Err(error) => break Err(error),
                }
            };
            self.finish_provider(provider_id, ProviderAction::Disconnect, result);
        });
    }

    fn finish_provider(
        self,
        provider_id: cookie_agent_protocol::ProviderId,
        action: ProviderAction,
        result: Result<(), cookie_agent_protocol::ClientError>,
    ) {
        match result {
            Ok(()) => {
                self.provider_operations.update(|operations| {
                    operations.remove(&provider_id);
                });
                self.status.set(match action {
                    ProviderAction::Disconnect => format!("Disconnected provider {provider_id}."),
                    _ => format!("Connected provider {provider_id}."),
                });
                self.resync_runtime();
            }
            Err(error) => {
                let message = store_contention_message(&error, "providers");
                self.provider_operations.update(|operations| {
                    operations.insert(
                        provider_id.clone(),
                        ProviderOperation::Error {
                            action,
                            message: message.clone(),
                        },
                    );
                });
                self.status.set(format!(
                    "{} {provider_id} failed: {message}",
                    action_name(action)
                ));
            }
        }
    }

    // ---- /config ----------------------------------------------------------

    /// Reads the configuration afresh for the Agents view or the
    /// Configuration section, with no document open.
    pub(super) fn open_config(self) {
        self.config_edit.set(None);
        self.config_error.set(None);
        self.poll_config();
    }

    pub fn poll_config(self) {
        let Some(client) = self.client() else {
            return;
        };
        let read = self.begin_read(|reads| &mut reads.config);
        let status_read = self.begin_read(|reads| &mut reads.config_status);
        spawn_local(async move {
            let result = client
                .get_config()
                .await
                .map_err(|error| crate::errors::describe_full(&error));
            if let Ok(config) = &result
                && self.read_is_current(|reads| &mut reads.config_status, status_read)
            {
                self.note_config_status(&config.status);
            }
            if !self.read_is_current(|reads| &mut reads.config, read) {
                return;
            }
            if let Err(error) = &result {
                self.status
                    .set(format!("configuration unavailable: {error}"));
            }
            self.config.set(Some(result));
        });
    }

    /// Opens `path` in the editor: a refused edit's text when there is one,
    /// else the document, else a new document's template.
    pub fn edit_config(self, path: &str) {
        if let Some(draft) = self
            .config_drafts
            .with_untracked(|drafts| drafts.get(path).cloned())
        {
            self.config_edit.set(Some(draft));
            return;
        }
        let Some(Ok(config)) = self.config.get_untracked() else {
            return;
        };
        let document = config
            .documents
            .iter()
            .find(|document| document.path == path);
        self.config_edit.set(Some(ConfigEdit {
            path: path.to_owned(),
            text: document_text(&config, path),
            original: document.map(|document| document.text.clone()),
            expected_fingerprint: document.map(|document| document.fingerprint.clone()),
            revision: config.revision.clone(),
        }));
    }

    /// Opens a new agent document at `path`, unless it cannot be one.
    pub fn new_agent(self, path: &str) {
        let path = path.trim();
        let Some(Ok(config)) = self.config.get_untracked() else {
            return;
        };
        if let Some(refusal) = new_agent_refusal(&config, path) {
            self.status.set(refusal);
            return;
        }
        self.edit_config(path);
    }

    /// Applies the editor's `text` with `config.apply` at the revision the
    /// document was read at.
    pub fn apply_config(self, text: String) {
        let (Some(client), Some(edit)) = (self.client(), self.config_edit.get_untracked()) else {
            return;
        };
        if edit.original.as_deref() == Some(text.as_str()) {
            self.config_drafts.update(|drafts| {
                drafts.remove(&edit.path);
            });
            self.config_edit.set(None);
            self.status.set(format!("No changes to {}.", edit.path));
            return;
        }
        let edit = ConfigEdit { text, ..edit };
        self.status.set(format!("applying {}…", edit.path));
        let change = ConfigChange::Put {
            path: edit.path.clone(),
            text: edit.text.clone(),
            expected_fingerprint: edit.expected_fingerprint.clone(),
        };
        spawn_local(async move {
            let outcome = ConfigApplyOutcome::from_result(
                client
                    .apply_config(ConfigApplyParams {
                        expected_revision: edit.revision.clone(),
                        changes: vec![change],
                    })
                    .await,
            );
            self.finish_config(edit.path.clone(), Some(edit), outcome);
        });
    }

    /// Deletes the agent document `path`.
    pub fn delete_config(self, path: String) {
        let (Some(client), Some(Ok(config))) = (self.client(), self.config.get_untracked()) else {
            return;
        };
        let Some(document) = config
            .documents
            .iter()
            .find(|document| document.path == path)
        else {
            return;
        };
        self.status.set(format!("deleting {path}…"));
        let change = ConfigChange::Delete {
            path: path.clone(),
            expected_fingerprint: document.fingerprint.clone(),
        };
        let revision = config.revision.clone();
        spawn_local(async move {
            let outcome = ConfigApplyOutcome::from_result(
                client
                    .apply_config(ConfigApplyParams {
                        expected_revision: revision,
                        changes: vec![change],
                    })
                    .await,
            );
            self.finish_config(path, None, outcome);
        });
    }

    fn finish_config(self, path: String, edit: Option<ConfigEdit>, outcome: ConfigApplyOutcome) {
        let report = outcome.report(&path);
        let failed = matches!(outcome, ConfigApplyOutcome::Failed(_));
        if report.keep_draft {
            if let Some(edit) = edit.filter(|_| !failed) {
                self.config_drafts.update(|drafts| {
                    drafts.insert(path.clone(), edit.clone());
                });
                // The editor stays open on the refused text, to be fixed.
                self.config_edit.set(Some(edit));
            }
        } else {
            self.config_drafts.update(|drafts| {
                drafts.remove(&path);
            });
            if self
                .config_edit
                .with_untracked(|open| open.as_ref().is_some_and(|open| open.path == path))
            {
                self.config_edit.set(None);
            }
        }
        if !failed {
            self.config_error.set(report.error);
        }
        self.status.set(report.status);
        self.poll_config();
    }

    pub fn reload_config(self) {
        let Some(client) = self.client() else {
            return;
        };
        self.status.set("reloading the configuration…".into());
        spawn_local(async move {
            match client.reload_config().await {
                Ok(result) => {
                    self.note_config_status(&result.status);
                    self.status.set(reload_status(&result.status));
                }
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
            self.poll_config();
        });
    }

    // ---- /memory ----------------------------------------------------------

    pub(super) fn open_memory(self) {
        self.memory_edit.set(None);
        self.memory_pending.set(None);
        self.memory_focus.set(None);
        self.poll_memory();
    }

    /// Opens the Memory view at `url`, from a transcript line that says a
    /// model changed it: its project's folder opens, and its row is marked
    /// and scrolled to.
    pub fn open_memory_at(self, url: String) {
        self.open_view(View::Memory, None);
        self.memory_focus.set(Some(url));
    }

    pub fn poll_memory(self) {
        let Some(client) = self.client() else {
            return;
        };
        let read = self.begin_read(|reads| &mut reads.memory);
        spawn_local(async move {
            let result = client
                .list_memory(MemoryListParams::default())
                .await
                .map_err(|error| crate::errors::describe_full(&error));
            if self.read_is_current(|reads| &mut reads.memory, read) {
                self.memory.set(Some(result));
            }
        });
    }

    /// Reads `url` into the editor; a missing memory opens the template.
    pub fn edit_memory(self, url: String) {
        let Some(client) = self.client() else {
            return;
        };
        self.memory_pending.set(None);
        self.status.set(format!("reading {url}…"));
        spawn_local(async move {
            let version = client.daemon_version();
            match client.read_memory(url.clone()).await {
                Ok(read) => {
                    self.status.set(match read.invalid.as_deref() {
                        Some(reason) => format!("{url} is not a valid memory: {reason}"),
                        None => format!("editing {url}"),
                    });
                    let last_read = self.memory.with_untracked(|listing| {
                        listing
                            .as_ref()
                            .and_then(|listing| listing.as_ref().ok())
                            .and_then(|listing| {
                                listing
                                    .memories
                                    .iter()
                                    .find(|memory| memory.url == read.url)
                            })
                            .and_then(|memory| memory.last_read)
                    });
                    // The daemon owns the stamp lines and writes its own on
                    // every save: the editor leaves them out.
                    let text = editor_text(&read.text, version);
                    let parts = self
                        .listed_parts(&client, &read.url, &read.revision, &text)
                        .await;
                    self.memory_edit.set(Some(MemoryEdit {
                        stamps: Some(MemoryStamps::of_read(&read, last_read)),
                        url: read.url,
                        text: text.clone(),
                        original: text,
                        revision: Some(read.revision),
                        read_parts: parts.clone(),
                        parts,
                    }));
                }
                Err(error) if error.is(&errors::MEMORY_NOT_FOUND) => {
                    self.memory_edit.set(Some(new_memory(url)));
                }
                Err(error) => self.status.set(format!(
                    "{url} could not be read: {}",
                    crate::errors::describe(&error)
                )),
            }
        });
    }

    /// The parts of `url`, read at `revision` as `text`, from the listing:
    /// the panel's when it lists that revision, else a fresh one, since a
    /// save's refresh may not have landed yet.
    async fn listed_parts(
        self,
        client: &cookie_agent_protocol::Client,
        url: &str,
        revision: &str,
        text: &str,
    ) -> Option<MemoryParts> {
        let listed = self.memory.with_untracked(|listing| {
            let listing = listing.as_ref()?.as_ref().ok()?;
            let memory = listing.memories.iter().find(|memory| memory.url == url)?;
            (memory.revision == revision).then(|| memory_parts(memory, revision, text))
        });
        if let Some(parts) = listed {
            return parts;
        }
        let listing = client.list_memory(MemoryListParams::default()).await.ok()?;
        let memory = listing.memories.iter().find(|memory| memory.url == url)?;
        memory_parts(memory, revision, text)
    }

    /// Opens a new memory at `url`, unless it cannot be one.
    pub fn new_memory(self, url: &str) {
        let url = url.trim().to_owned();
        if let Some(Ok(listing)) = self.memory.get_untracked()
            && let Some(refusal) = new_memory_refusal(&listing, &url)
        {
            self.status.set(refusal);
            return;
        }
        self.memory_pending.set(None);
        self.memory_edit.set(Some(new_memory(url)));
    }

    /// Saves the editor's `text`, typed as `parts` when it shows them, at
    /// the revision read.
    pub fn save_memory(self, text: String, parts: Option<MemoryParts>) {
        let Some(edit) = self.memory_edit.get_untracked() else {
            return;
        };
        if text == edit.original {
            self.memory_edit.set(None);
            self.status.set(if edit.revision.is_some() {
                format!("No changes to {}.", edit.url)
            } else {
                format!("No changes; {} was not created.", edit.url)
            });
            return;
        }
        self.write_memory(MemoryEdit {
            text,
            parts: parts.or(edit.parts),
            ..edit
        });
    }

    fn write_memory(self, edit: MemoryEdit) {
        let Some(client) = self.client() else {
            return;
        };
        self.status.set(format!("saving {}…", edit.url));
        spawn_local(async move {
            let outcome = MemoryWriteOutcome::from_result(
                client
                    .write_memory(edit.url.clone(), edit.text.clone(), edit.revision.clone())
                    .await,
            );
            match outcome {
                MemoryWriteOutcome::Saved { notice } => {
                    self.memory_pending.set(None);
                    self.memory_edit.set(None);
                    self.status.set(saved_status(
                        &edit.url,
                        edit.revision.is_none(),
                        notice.as_deref(),
                    ));
                }
                MemoryWriteOutcome::Invalid(message) => {
                    self.status.set(format!("Not saved: {message}"));
                    self.memory_edit.set(None);
                    self.memory_pending
                        .set(Some(MemoryPending::Invalid { edit, message }));
                }
                MemoryWriteOutcome::Changed => {
                    self.status.set(format!(
                        "Not saved: {} changed since it was opened.",
                        edit.url
                    ));
                    self.memory_edit.set(None);
                    self.memory_pending
                        .set(Some(MemoryPending::Changed { edit }));
                }
                MemoryWriteOutcome::Failed(error) => {
                    self.status
                        .set(format!("{} was not saved: {error}", edit.url));
                }
            }
            self.poll_memory();
        });
    }

    /// Answers a pending write: edit it again, reload the current memory,
    /// overwrite it with this text, or drop it.
    pub fn resolve_memory(self, choice: MemoryChoice) {
        let Some(pending) = self.memory_pending.get_untracked() else {
            return;
        };
        self.memory_pending.set(None);
        match (choice, pending) {
            (MemoryChoice::EditAgain, MemoryPending::Invalid { edit, .. }) => {
                self.memory_edit.set(Some(edit));
            }
            (MemoryChoice::Reload, MemoryPending::Changed { edit }) => self.edit_memory(edit.url),
            (MemoryChoice::Overwrite, MemoryPending::Changed { edit }) => {
                let Some(client) = self.client() else {
                    return;
                };
                spawn_local(async move {
                    let revision = client
                        .read_memory(edit.url.clone())
                        .await
                        .ok()
                        .map(|read| read.revision);
                    self.write_memory(MemoryEdit { revision, ..edit });
                });
            }
            (_, MemoryPending::Invalid { edit, .. } | MemoryPending::Changed { edit }) => {
                self.status
                    .set(format!("Your edit of {} was dropped.", edit.url));
            }
        }
    }

    pub fn delete_memory(self, url: String, revision: String) {
        let Some(client) = self.client() else {
            return;
        };
        self.status.set(format!("deleting {url}…"));
        spawn_local(async move {
            self.status
                .set(match client.delete_memory(url.clone(), revision).await {
                    Ok(_) => format!("Deleted {url}."),
                    Err(error) => delete_refusal(&url, &error),
                });
            self.poll_memory();
        });
    }

    /// Deletes every memory of `project` with `memory.delete`, each at the
    /// revision listed; the folder's other files stay, and so does the
    /// folder when there are any. A memory that is refused does not stop
    /// the others.
    pub fn delete_project(self, project: String) {
        let (Some(client), Some(Ok(listing))) = (self.client(), self.memory.get_untracked()) else {
            return;
        };
        let memories = project_memories(&listing, &project);
        let strays = project_strays(&listing, &project);
        self.status.set(format!("deleting project {project}…"));
        spawn_local(async move {
            let mut deleted = 0;
            let mut failure = None;
            for (url, revision) in &memories {
                match client.delete_memory(url.clone(), revision.clone()).await {
                    Ok(_) => deleted += 1,
                    Err(error) => {
                        failure.get_or_insert_with(|| delete_refusal(url, &error));
                    }
                }
            }
            let mut status =
                project_delete_status(&project, deleted, memories.len(), failure.as_deref());
            if strays > 0 {
                status.push_str(&format!(
                    " Its folder keeps {} that {} not memories.",
                    if strays == 1 {
                        "1 file".to_owned()
                    } else {
                        format!("{strays} files")
                    },
                    if strays == 1 { "is" } else { "are" }
                ));
            } else if failure.is_none() {
                crate::browser::forget(&folder_key(&project));
            }
            self.status.set(status);
            self.poll_memory();
        });
    }

    // ---- /plugins ---------------------------------------------------------

    pub fn poll_plugins(self) {
        let Some(client) = self.client() else {
            return;
        };
        let read = self.begin_read(|reads| &mut reads.plugins);
        spawn_local(async move {
            let result = client
                .list_plugins()
                .await
                .map(|listing| listing.plugins)
                .map_err(|error| crate::errors::describe_full(&error));
            if !self.read_is_current(|reads| &mut reads.plugins, read) {
                return;
            }
            if let Err(error) = &result {
                self.status.set(format!("plugin refresh failed: {error}"));
            }
            self.plugins.set(Some(result));
        });
    }

    pub fn restart_plugin(self, name: String) {
        let Some(client) = self.client() else {
            return;
        };
        self.status.set(format!("restarting plugin `{name}`…"));
        spawn_local(async move {
            self.status
                .set(match client.restart_plugin(name.clone()).await {
                    Ok(_) => format!("plugin `{name}` is restarting"),
                    Err(error) => crate::errors::describe(&error),
                });
            self.poll_plugins();
        });
    }
}

/// How the user answers a pending memory write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryChoice {
    EditAgain,
    Reload,
    Overwrite,
    Drop,
}

fn new_memory(url: String) -> MemoryEdit {
    let parts = new_memory_parts(&url);
    MemoryEdit {
        url,
        text: NEW_MEMORY.to_owned(),
        original: NEW_MEMORY.to_owned(),
        revision: None,
        stamps: None,
        read_parts: parts.clone(),
        parts,
    }
}
