//! The `/runtime` panel's rules every client shares: a session's
//! attachments matched to their instances and the MCP servers each hosts,
//! the runtimes a new-session draft
//! attaches, what attaching offers and asks for, and the machines view's
//! rows and changes (`machines.*`).

use cookie_agent_protocol::{
    AttachmentName, CapabilityName, MachineAssignment, MachineName, MachinesListResult,
    McpServerInfo, RuntimeAttachRequest, RuntimeAttachment, RuntimeCapabilityInfo, RuntimeId,
    RuntimeInstanceInfo, RuntimeInstanceStatus, RuntimeKind, RuntimeListing, RuntimesListResult,
};

/// One attachment row: what the session records and the instance it refers
/// to, when the daemon reports one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeRow {
    pub attachment: RuntimeAttachment,
    pub instance: Option<RuntimeInstanceInfo>,
    /// What the runtime says its capability offers, once it has connected.
    pub description: Option<String>,
}

impl RuntimeRow {
    /// The MCP servers the row's instance hosts, from `mcp.server.list`, in
    /// name order: those of its runtime at the instance's workspace (the
    /// attachment's until the instance reports one).
    pub fn mcp_servers(&self, servers: &[McpServerInfo]) -> Vec<McpServerInfo> {
        let workspace = match &self.instance {
            Some(instance) => instance.workspace.as_deref(),
            None => self.attachment.workspace.as_deref(),
        };
        crate::mcp::instance_servers(servers, &self.attachment.runtime_id, workspace)
    }
}

/// The description `listing` has for `capability` of `runtime_id`.
fn capability_description(
    listing: Option<&RuntimesListResult>,
    runtime_id: &RuntimeId,
    capability: &CapabilityName,
) -> Option<String> {
    listing?
        .runtimes
        .iter()
        .find(|runtime| runtime.runtime_id == *runtime_id)?
        .capabilities
        .iter()
        .find(|offered| offered.name == *capability)
        .map(|offered| offered.description.clone())
        .filter(|description| !description.is_empty())
}

/// A runtime a new-session draft attaches. Its instance is opened as soon as
/// it joins the draft, so its status, and why it failed, show before the
/// session exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DraftRuntime {
    /// Matches the answer of the open request to this entry.
    pub id: u64,
    pub request: RuntimeAttachRequest,
    /// The instance the open request answered, or why it failed.
    pub opened: Option<Result<RuntimeInstanceInfo, String>>,
}

impl DraftRuntime {
    pub fn is_global(&self) -> bool {
        self.request.runtime_id.as_str() == RuntimeId::GLOBAL
    }

    /// The opened instance's canonical workspace, else the one requested.
    pub fn workspace(&self) -> Option<&str> {
        match &self.opened {
            Some(Ok(instance)) => instance.workspace.as_deref(),
            _ => self.request.workspace.as_deref(),
        }
    }

    /// Whether `other` names the same instance as this entry.
    pub fn same_instance(&self, other: &RuntimeAttachRequest) -> bool {
        self.request.runtime_id == other.runtime_id
            && self.request.capability == other.capability
            && (self.request.workspace == other.workspace
                || self.workspace() == other.workspace.as_deref())
    }
}

/// One thing the attach step offers: a capability of a runtime, or a runtime
/// that has not said what it offers yet, which attaching connects and takes
/// its only capability of.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeChoice {
    pub runtime_id: RuntimeId,
    pub capability: Option<RuntimeCapabilityInfo>,
    pub connected: bool,
    /// Why the runtime last failed to connect.
    pub message: Option<String>,
}

impl RuntimeChoice {
    /// Whether attaching asks for a workspace: the capability takes one, or
    /// the runtime has not said what it offers yet (then it is optional).
    pub fn asks_workspace(&self) -> bool {
        self.capability
            .as_ref()
            .is_none_or(|capability| capability.takes_workspace)
    }

    pub fn label(&self) -> String {
        match &self.capability {
            Some(capability) => format!("{}/{}", self.runtime_id, capability.name),
            None => self.runtime_id.to_string(),
        }
    }

    /// Case-insensitive substring match over the runtime ID and the
    /// capability's name, kind, and description.
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let mut haystack = self.runtime_id.to_string();
        if let Some(capability) = &self.capability {
            haystack.push(' ');
            haystack.push_str(capability.name.as_str());
            haystack.push(' ');
            haystack.push_str(capability.kind.as_str());
            haystack.push(' ');
            haystack.push_str(&capability.description);
        }
        haystack.to_lowercase().contains(&query)
    }
}

/// A change to the machine registry the machines view offers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MachineAction {
    Rename,
    Move,
    Unpin,
    Merge,
    Forget,
}

impl MachineAction {
    pub const fn key(self) -> char {
        match self {
            Self::Rename => 'n',
            Self::Move => 'v',
            Self::Unpin => 'u',
            Self::Merge => 'g',
            Self::Forget => 'f',
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::Move => "move to machine",
            Self::Unpin => "unpin",
            Self::Merge => "merge into",
            Self::Forget => "forget",
        }
    }

    /// Whether the action asks for a machine name first.
    pub const fn asks_name(self) -> bool {
        matches!(self, Self::Rename | Self::Move | Self::Merge)
    }

    /// The action of the key `key`.
    pub fn of_key(key: char) -> Option<Self> {
        [
            Self::Rename,
            Self::Move,
            Self::Unpin,
            Self::Merge,
            Self::Forget,
        ]
        .into_iter()
        .find(|action| action.key() == key)
    }
}

/// One row of the machines view: a machine, or a runtime under it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MachineEntry {
    Machine(MachineName),
    Runtime {
        machine: MachineName,
        runtime: RuntimeId,
        /// Why it belongs to the machine while it is connected.
        assigned_by: Option<MachineAssignment>,
        pinned: bool,
    },
}

impl MachineEntry {
    /// The actions the entry offers.
    pub fn actions(&self) -> Vec<MachineAction> {
        match self {
            Self::Machine(_) => vec![
                MachineAction::Rename,
                MachineAction::Merge,
                MachineAction::Forget,
            ],
            Self::Runtime { pinned: true, .. } => vec![MachineAction::Move, MachineAction::Unpin],
            Self::Runtime { pinned: false, .. } => vec![MachineAction::Move],
        }
    }
}

/// A machine change as the daemon takes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MachineRequest {
    Rename {
        machine: MachineName,
        name: MachineName,
    },
    Assign {
        runtime: RuntimeId,
        machine: MachineName,
    },
    Unassign {
        runtime: RuntimeId,
    },
    Merge {
        from: MachineName,
        into: MachineName,
    },
    Forget {
        machine: MachineName,
    },
    /// Forget where an offline runtime ran (`machines.forget_runtime`).
    ForgetRuntime {
        runtime: RuntimeId,
    },
}

/// The rows for `attachments`, each matched to its instance in `listing`.
pub fn runtime_rows(
    attachments: &[RuntimeAttachment],
    listing: Option<&RuntimesListResult>,
) -> Vec<RuntimeRow> {
    attachments
        .iter()
        .map(|attachment| RuntimeRow {
            description: capability_description(
                listing,
                &attachment.runtime_id,
                &attachment.capability,
            ),
            attachment: attachment.clone(),
            instance: listing.and_then(|listing| {
                listing
                    .instances
                    .iter()
                    .find(|instance| {
                        instance.runtime_id == attachment.runtime_id
                            && instance.capability == attachment.capability
                            && instance.workspace == attachment.workspace
                    })
                    .cloned()
            }),
        })
        .collect()
}

/// The rows for a new-session draft: each entry with its live instance from
/// `listing`, else the one its open request answered. Until the instance
/// reports its attachment name and kind, the row shows the runtime ID and the
/// capability.
pub fn draft_runtime_rows(
    entries: &[DraftRuntime],
    listing: Option<&RuntimesListResult>,
) -> Vec<RuntimeRow> {
    entries
        .iter()
        .filter_map(|entry| {
            let capability = match (&entry.request.capability, &entry.opened) {
                (Some(capability), _) => capability.clone(),
                (None, Some(Ok(instance))) => instance.capability.clone(),
                (None, _) => cookie_agent_protocol::default_capability(&entry.request.runtime_id),
            };
            let workspace = entry.workspace().map(str::to_owned);
            let live = listing.and_then(|listing| {
                listing
                    .instances
                    .iter()
                    .find(|instance| {
                        instance.runtime_id == entry.request.runtime_id
                            && instance.capability == capability
                            && instance.workspace == workspace
                    })
                    .cloned()
            });
            let instance = live.or_else(|| match &entry.opened {
                Some(Ok(instance)) => Some(instance.clone()),
                Some(Err(message)) => Some(RuntimeInstanceInfo {
                    runtime_id: entry.request.runtime_id.clone(),
                    capability: capability.clone(),
                    attachment_name: None,
                    kind: None,
                    workspace: workspace.clone(),
                    platform: None,
                    status: RuntimeInstanceStatus::Failed,
                    message: Some(message.clone()),
                }),
                None => None,
            });
            Some(RuntimeRow {
                description: capability_description(
                    listing,
                    &entry.request.runtime_id,
                    &capability,
                ),
                attachment: RuntimeAttachment {
                    name: match instance
                        .as_ref()
                        .and_then(|instance| instance.attachment_name.clone())
                    {
                        Some(name) => name,
                        None => AttachmentName::new(entry.request.runtime_id.as_str()).ok()?,
                    },
                    kind: match instance.as_ref().and_then(|instance| instance.kind.clone()) {
                        Some(kind) => kind,
                        None => RuntimeKind::new(capability.as_str()).ok()?,
                    },
                    runtime_id: entry.request.runtime_id.clone(),
                    capability,
                    workspace,
                },
                instance,
            })
        })
        .collect()
}

pub const fn status_label(status: Option<RuntimeInstanceStatus>) -> &'static str {
    match status {
        Some(RuntimeInstanceStatus::Starting) => "starting",
        Some(RuntimeInstanceStatus::Ready) => "ready",
        Some(RuntimeInstanceStatus::Reconnecting) => "reconnecting",
        Some(RuntimeInstanceStatus::Failed) => "failed",
        Some(RuntimeInstanceStatus::Stopped) => "stopped",
        Some(RuntimeInstanceStatus::Unknown) => "unknown",
        None => "not running",
    }
}

/// Every runtime's capabilities, in listing order, that match `query`; a
/// runtime that has not said what it offers is one choice.
pub fn runtime_choices(runtimes: &[RuntimeListing], query: &str) -> Vec<RuntimeChoice> {
    runtimes
        .iter()
        .flat_map(|runtime| {
            let choice = |capability| RuntimeChoice {
                runtime_id: runtime.runtime_id.clone(),
                capability,
                connected: runtime.connected,
                message: runtime.message.clone(),
            };
            if runtime.capabilities.is_empty() {
                vec![choice(None)]
            } else {
                runtime
                    .capabilities
                    .iter()
                    .cloned()
                    .map(|capability| choice(Some(capability)))
                    .collect()
            }
        })
        .filter(|choice| choice.matches(query))
        .collect()
}

/// The attach request `choice` with the typed `workspace` describes.
pub fn attach_parameters(
    choice: &RuntimeChoice,
    workspace: &str,
) -> Result<(RuntimeId, Option<CapabilityName>, Option<String>), String> {
    let workspace = workspace.trim();
    let workspace = match (&choice.capability, workspace.is_empty()) {
        (Some(capability), true) if capability.takes_workspace => {
            return Err(format!(
                "capability `{}` of runtime `{}` needs a workspace path",
                capability.name, choice.runtime_id
            ));
        }
        (_, true) => None,
        (_, false) => Some(workspace.to_owned()),
    };
    Ok((
        choice.runtime_id.clone(),
        choice
            .capability
            .as_ref()
            .map(|capability| capability.name.clone()),
        workspace,
    ))
}

/// The `runtimes.reconnect` request that retries `row`'s instance: its
/// workspace goes along exactly when it has one.
pub fn reconnect_request(
    runtimes: &[RuntimeListing],
    row: &RuntimeRow,
) -> Result<(RuntimeId, CapabilityName, Option<String>), String> {
    if !runtimes
        .iter()
        .any(|runtime| runtime.runtime_id == row.attachment.runtime_id)
    {
        return Err(format!(
            "runtime `{}` is not a known runtime",
            row.attachment.runtime_id
        ));
    }
    Ok((
        row.attachment.runtime_id.clone(),
        row.attachment.capability.clone(),
        row.attachment.workspace.clone(),
    ))
}

/// The status line an answered `runtimes.reconnect` gives.
pub fn reconnect_status(runtime_id: &RuntimeId, instance: &RuntimeInstanceInfo) -> String {
    format!(
        "runtime `{runtime_id}` is {}",
        status_label(Some(instance.status))
    )
}

/// The machines view's rows: each machine, then its connected runtimes, the
/// runtimes pinned to it that are not connected, and the offline runtimes
/// it remembers (client protocol 1.10.0), placed by `last_seen`.
pub fn machine_entries(listing: &MachinesListResult) -> Vec<MachineEntry> {
    listing
        .machines
        .iter()
        .flat_map(|machine| {
            let mut entries = vec![MachineEntry::Machine(machine.name.clone())];
            for runtime in &machine.connected {
                entries.push(MachineEntry::Runtime {
                    machine: machine.name.clone(),
                    runtime: runtime.runtime_id.clone(),
                    assigned_by: Some(runtime.assigned_by),
                    pinned: machine.pinned.contains(&runtime.runtime_id),
                });
            }
            for runtime in machine.pinned.iter().filter(|pinned| {
                !machine
                    .connected
                    .iter()
                    .any(|connected| connected.runtime_id == **pinned)
            }) {
                entries.push(MachineEntry::Runtime {
                    machine: machine.name.clone(),
                    runtime: runtime.clone(),
                    assigned_by: None,
                    pinned: true,
                });
            }
            for runtime in machine
                .remembered
                .iter()
                .filter(|remembered| !machine.pinned.contains(&remembered.runtime_id))
            {
                entries.push(MachineEntry::Runtime {
                    machine: machine.name.clone(),
                    runtime: runtime.runtime_id.clone(),
                    assigned_by: Some(runtime.assigned_by),
                    pinned: false,
                });
            }
            entries
        })
        .collect()
}

/// `action` on `entry`: a request ready to send, `None` when it first needs
/// a machine name, or why the entry does not offer it.
pub fn machine_action_request(
    entry: &MachineEntry,
    action: MachineAction,
) -> Result<Option<MachineRequest>, String> {
    if !entry.actions().contains(&action) {
        return Err(match (entry, action) {
            (MachineEntry::Machine(_), _) => format!("select a runtime to {}", action.label()),
            (MachineEntry::Runtime { .. }, MachineAction::Unpin) => {
                "only a pinned runtime can be unpinned".to_owned()
            }
            (MachineEntry::Runtime { .. }, _) => {
                format!("select a machine to {}", action.label())
            }
        });
    }
    if action.asks_name() {
        return Ok(None);
    }
    Ok(Some(match (action, entry) {
        (MachineAction::Forget, MachineEntry::Machine(machine)) => MachineRequest::Forget {
            machine: machine.clone(),
        },
        (MachineAction::Unpin, MachineEntry::Runtime { runtime, .. }) => MachineRequest::Unassign {
            runtime: runtime.clone(),
        },
        _ => return Err("nothing to do".into()),
    }))
}

/// The request `action` on `entry` makes with the typed machine name.
pub fn machine_name_request(
    action: MachineAction,
    entry: &MachineEntry,
    typed: &str,
) -> Result<MachineRequest, String> {
    let typed = typed.trim();
    let name = MachineName::new(typed).map_err(|_| {
        format!(
            "`{typed}` is not a machine name: a lowercase letter, then up to 31 lowercase \
             letters, digits, `_`, or `-`"
        )
    })?;
    Ok(match (action, entry) {
        (MachineAction::Rename, MachineEntry::Machine(machine)) => MachineRequest::Rename {
            machine: machine.clone(),
            name,
        },
        (MachineAction::Merge, MachineEntry::Machine(machine)) => MachineRequest::Merge {
            from: machine.clone(),
            into: name,
        },
        (MachineAction::Move, MachineEntry::Runtime { runtime, .. }) => MachineRequest::Assign {
            runtime: runtime.clone(),
            machine: name,
        },
        _ => return Err("nothing to do".into()),
    })
}

/// What a machine form asks for `action` on `entry`.
pub fn machine_form_title(action: MachineAction, entry: &MachineEntry) -> String {
    match (action, entry) {
        (MachineAction::Rename, MachineEntry::Machine(machine)) => {
            format!("New name for {machine}")
        }
        (MachineAction::Merge, MachineEntry::Machine(machine)) => {
            format!("Merge {machine} into (its memories move too)")
        }
        (MachineAction::Move, MachineEntry::Runtime { runtime, .. }) => {
            format!("Machine for runtime {runtime} (a new name creates it)")
        }
        _ => "Machine name".to_owned(),
    }
}

impl MachineRequest {
    /// What the status line says once the change is made.
    pub fn done(&self) -> String {
        match self {
            Self::Rename { machine, name } => format!("renamed machine {machine} to {name}"),
            Self::Assign { runtime, machine } => {
                format!("pinned runtime {runtime} to machine {machine}")
            }
            Self::Unassign { runtime } => format!("unpinned runtime {runtime}"),
            Self::Merge { from, into } => format!("merged machine {from} into {into}"),
            Self::Forget { machine } => format!("forgot machine {machine}"),
            Self::ForgetRuntime { runtime } => format!("forgot where runtime {runtime} ran"),
        }
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        MachineAssignment, MachineInfo, MachineName, MachineRuntime, MachinesListResult, RuntimeId,
        RuntimeListing,
    };

    use super::{
        MachineAction, MachineEntry, MachineRequest, attach_parameters, machine_action_request,
        machine_entries, machine_name_request, runtime_choices,
    };

    fn machines() -> MachinesListResult {
        let runtime = |id: &str, assigned_by| MachineRuntime {
            runtime_id: RuntimeId::new(id).unwrap(),
            assigned_by,
            platform: None,
        };
        MachinesListResult {
            machines: vec![MachineInfo {
                name: MachineName::new("laptop").unwrap(),
                ids: vec!["id-laptop".into()],
                pinned: vec![RuntimeId::new("box").unwrap()],
                connected: vec![runtime("local", MachineAssignment::MachineId)],
                remembered: Vec::new(),
                platform: None,
                memories: 0,
            }],
        }
    }

    #[test]
    fn machines_list_their_connected_then_their_pinned_runtimes() {
        let entries = machine_entries(&machines());
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0],
            MachineEntry::Machine(MachineName::new("laptop").unwrap())
        );
        assert!(matches!(
            &entries[1],
            MachineEntry::Runtime { runtime, pinned: false, .. } if runtime.as_str() == "local"
        ));
        assert!(matches!(
            &entries[2],
            MachineEntry::Runtime { runtime, pinned: true, assigned_by: None, .. }
                if runtime.as_str() == "box"
        ));
    }

    #[test]
    fn machine_actions_ask_for_a_name_or_go_at_once() {
        let entries = machine_entries(&machines());
        assert_eq!(
            machine_action_request(&entries[0], MachineAction::Rename),
            Ok(None)
        );
        assert_eq!(
            machine_action_request(&entries[0], MachineAction::Forget),
            Ok(Some(MachineRequest::Forget {
                machine: MachineName::new("laptop").unwrap()
            }))
        );
        assert_eq!(
            machine_action_request(&entries[1], MachineAction::Unpin),
            Err("only a pinned runtime can be unpinned".into())
        );
        assert_eq!(
            machine_name_request(MachineAction::Move, &entries[1], " lab "),
            Ok(MachineRequest::Assign {
                runtime: RuntimeId::new("local").unwrap(),
                machine: MachineName::new("lab").unwrap()
            })
        );
        assert!(
            machine_name_request(MachineAction::Rename, &entries[0], "Not A Name")
                .unwrap_err()
                .contains("is not a machine name")
        );
    }

    #[test]
    fn a_row_lists_the_mcp_servers_of_its_instance() {
        let servers: Vec<cookie_agent_protocol::McpServerInfo> =
            serde_json::from_value(serde_json::json!([
                {"runtime_id": "global", "name": "github", "source": "user_file",
                 "definition": {"command": null, "args": [], "env": {}, "cwd": null,
                                "url": "https://example.test/mcp", "headers": {},
                                "enabled": true, "timeout_ms": null},
                 "state": "needs_auth", "tool_count": 0, "message": "sign in"},
                {"runtime_id": "labpc", "workspace": "/srv/api", "name": "lsp",
                 "source": "machine",
                 "definition": {"command": null, "args": [], "env": {}, "cwd": null,
                                "url": null, "headers": {}, "enabled": true,
                                "timeout_ms": null},
                 "state": "failed", "tool_count": 0, "message": "exited"},
                {"runtime_id": "labpc", "workspace": "/srv/web", "name": "lsp",
                 "source": "user_file",
                 "definition": {"command": "lsp", "args": [], "env": {}, "cwd": null,
                                "url": null, "headers": {}, "enabled": true,
                                "timeout_ms": null},
                 "state": "connected", "tool_count": 4, "message": null}
            ]))
            .expect("servers");
        let attachments: Vec<cookie_agent_protocol::RuntimeAttachment> =
            serde_json::from_value(serde_json::json!([
                {"name": "global", "runtime_id": "global", "capability": "global",
                 "kind": "global"},
                {"name": "labpc_api", "runtime_id": "labpc", "capability": "coding",
                 "kind": "coding", "workspace": "/srv/api"}
            ]))
            .expect("attachments");
        let rows = super::runtime_rows(&attachments, None);
        let hosted = |row: &super::RuntimeRow| {
            row.mcp_servers(&servers)
                .into_iter()
                .map(|server| (server.name, server.state))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            hosted(&rows[0]),
            [(
                "github".to_owned(),
                cookie_agent_protocol::McpServerState::NeedsAuth
            )]
        );
        assert_eq!(
            hosted(&rows[1]),
            [(
                "lsp".to_owned(),
                cookie_agent_protocol::McpServerState::Failed
            )]
        );
        assert_eq!(
            servers[1].source,
            cookie_agent_protocol::McpConfigSource::Machine
        );
    }

    #[test]
    fn choices_filter_by_runtime_and_capability_and_a_workspace_is_asked_for() {
        let runtimes: Vec<RuntimeListing> = serde_json::from_value(serde_json::json!([
            {"runtime_id": "local", "lifetime": "persistent", "connected": true, "capabilities": [
                {"name": "coding", "kind": "coding", "takes_workspace": true, "description": "edit code"}
            ]},
            {"runtime_id": "box", "lifetime": "persistent", "connected": false, "capabilities": [],
             "message": "unreachable"}
        ]))
        .expect("runtimes");
        assert_eq!(runtime_choices(&runtimes, "").len(), 2);
        let coding = runtime_choices(&runtimes, "EDIT");
        assert_eq!(coding.len(), 1);
        assert!(coding[0].asks_workspace());
        assert!(attach_parameters(&coding[0], " ").is_err());
        let box_choice = &runtime_choices(&runtimes, "box")[0];
        assert_eq!(box_choice.message.as_deref(), Some("unreachable"));
        assert_eq!(
            attach_parameters(box_choice, "").map(|(_, _, workspace)| workspace),
            Ok(None)
        );
    }
}
