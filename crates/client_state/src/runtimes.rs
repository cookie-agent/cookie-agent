//! The `/runtime` panel's rules every client shares: a session's
//! attachments matched to their instances and the MCP servers each hosts,
//! the runtimes a new-session draft
//! attaches, what attaching offers and asks for, and the machines view's
//! rows and changes (`machines.*`).

use std::collections::BTreeMap;

use cookie_agent_protocol::{
    AcpAgentDefinition, AgentDescriptor, AgentId, AttachmentName, CapabilityName,
    MachineAssignment, MachineName, MachinesListResult, McpServerInfo, RuntimeAgentDeclaration,
    RuntimeAttachRequest, RuntimeAttachment, RuntimeCapabilityInfo, RuntimeId, RuntimeInstanceInfo,
    RuntimeInstanceStatus, RuntimeKind, RuntimeLifetime, RuntimeListing, RuntimesListResult,
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
    /// Opens a runtime's MCP servers, to manage them.
    Servers,
    /// Opens a runtime's ACP agents, to manage them.
    Agents,
    /// Connects a runtime the daemon starts (`runtimes.connect`).
    Connect,
    /// Disconnects a runtime the daemon starts (`runtimes.disconnect`).
    Disconnect,
}

impl MachineAction {
    pub const fn key(self) -> char {
        match self {
            Self::Rename => 'n',
            Self::Move => 'v',
            Self::Unpin => 'u',
            Self::Merge => 'g',
            Self::Forget => 'f',
            Self::Servers => 's',
            Self::Agents => 'a',
            Self::Connect => 'c',
            Self::Disconnect => 'd',
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::Move => "move to machine",
            Self::Unpin => "unpin",
            Self::Merge => "merge into",
            Self::Forget => "forget",
            Self::Servers => "MCP servers",
            Self::Agents => "ACP agents",
            Self::Connect => "connect",
            Self::Disconnect => "disconnect",
        }
    }

    /// Whether the action opens a panel of the runtime rather than changing
    /// the machine registry.
    pub const fn opens_panel(self) -> bool {
        matches!(self, Self::Servers | Self::Agents)
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
            Self::Servers,
            Self::Agents,
            Self::Connect,
            Self::Disconnect,
        ]
        .into_iter()
        .find(|action| action.key() == key)
    }
}

/// An ACP agent declaration a runtime offers, as the machines views list it
/// (`docs/site/specs/runtime-management.md` section 4.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeAcpAgent {
    /// The declaration's key, which is also the agent of that ID.
    pub agent: AgentId,
    /// A session that attaches the runtime can run it now.
    pub available: bool,
    /// What the agent of the declaration's own ID says it is; empty when
    /// only templates run the declaration.
    pub description: String,
    /// The agents that run the declaration on the runtime, in ID order:
    /// the one of its own ID and every template over it (`acp.agent`).
    pub agents: Vec<AgentId>,
}

/// The client protocol minor from which `runtimes.agents.*` take `global`
/// for the ACP agents every runtime offers (`[acp.agents]`).
pub const SHARED_ACP_AGENTS_MINOR: u32 = 43;

/// Whether a daemon speaking `version` declares ACP agents for every
/// runtime.
pub fn daemon_shares_acp_agents(version: Option<cookie_agent_protocol::SemVer>) -> bool {
    version.is_some_and(|version| {
        version
            .shared_minor(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
            .is_some_and(|minor| minor >= SHARED_ACP_AGENTS_MINOR)
    })
}

/// The ACP agent declarations `runtime_id` offers, by key, from the
/// catalog's `agents`.
pub fn runtime_acp_agents(
    agents: &[AgentDescriptor],
    runtime_id: &RuntimeId,
) -> Vec<RuntimeAcpAgent> {
    let mut declarations = std::collections::BTreeMap::<AgentId, RuntimeAcpAgent>::new();
    for descriptor in agents {
        for offer in descriptor
            .runtimes
            .iter()
            .filter(|offer| offer.runtime_id == *runtime_id)
        {
            let declaration =
                declarations
                    .entry(offer.agent.clone())
                    .or_insert_with(|| RuntimeAcpAgent {
                        agent: offer.agent.clone(),
                        available: false,
                        description: String::new(),
                        agents: Vec::new(),
                    });
            declaration.available |= offer.available;
            if descriptor.id == offer.agent {
                declaration.description.clone_from(&descriptor.description);
            }
            if !declaration.agents.contains(&descriptor.id) {
                declaration.agents.push(descriptor.id.clone());
            }
        }
    }
    declarations
        .into_values()
        .map(|mut declaration| {
            declaration.agents.sort();
            declaration
        })
        .collect()
}

/// The form of an ACP agent declaration,
/// `[runtimes.<id>.acp.agents.<agent>]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcpAgentFields {
    /// The declaration's key, which is also its agent's ID.
    pub agent: String,
    /// The program and its arguments, as a JSON array.
    pub command: String,
    /// The environment, as a JSON object.
    pub environment: String,
    pub description: String,
    pub enabled: bool,
}

impl AcpAgentFields {
    /// A new declaration's form.
    #[must_use]
    pub fn add() -> Self {
        Self {
            agent: String::new(),
            command: "[]".into(),
            environment: "{}".into(),
            description: String::new(),
            enabled: true,
        }
    }

    /// `declaration`'s form.
    #[must_use]
    pub fn edit(declaration: &RuntimeAgentDeclaration) -> Self {
        Self {
            agent: declaration.agent.to_string(),
            command: serde_json::to_string(&declaration.definition.command)
                .unwrap_or_else(|_| "[]".into()),
            environment: serde_json::to_string(&declaration.definition.env)
                .unwrap_or_else(|_| "{}".into()),
            description: declaration.definition.description.clone(),
            enabled: declaration.definition.enabled,
        }
    }

    /// The key and definition the form describes, or what is wrong.
    pub fn definition(&self) -> Result<(AgentId, AcpAgentDefinition), String> {
        let agent = AgentId::new(self.agent.trim()).map_err(|_| {
            "the agent ID is lowercase letters, digits, and single inner hyphens".to_owned()
        })?;
        let command: Vec<String> = serde_json::from_str(self.command.trim())
            .map_err(|_| "the command is a JSON array of strings".to_owned())?;
        if command.first().is_none_or(String::is_empty) {
            return Err("the command names a program first".into());
        }
        let env = if self.environment.trim().is_empty() {
            BTreeMap::new()
        } else {
            serde_json::from_str(self.environment.trim())
                .map_err(|_| "the environment is a JSON object of strings".to_owned())?
        };
        Ok((
            agent,
            AcpAgentDefinition {
                command,
                env,
                description: self.description.trim().to_owned(),
                enabled: self.enabled,
            },
        ))
    }
}

/// The document of agent `agent`, as `config.get` names it.
#[must_use]
pub fn agent_document_path(agent: &str) -> String {
    format!("agents/{agent}.md")
}

/// A new document for the ACP agent of declaration `agent`: what the agent
/// is called, and the agent's own rules until the user adds cookie's.
#[must_use]
pub fn acp_agent_document(agent: &str, description: &str) -> String {
    let description = if description.is_empty() {
        format!("ACP agent {agent}")
    } else {
        description.to_owned()
    };
    format!(
        "---\ndescription: {}\nmode: all\nacp: {{}}\n---\n",
        serde_json::to_string(&description).unwrap_or_default()
    )
}

/// A new template over declaration `agent`: its own agent, named by the
/// document, that runs the declaration's process with its own settings.
#[must_use]
pub fn acp_template_document(agent: &str) -> String {
    format!(
        "---\ndescription: {}\nmode: subagent\nacp:\n  agent: {agent}\n---\n",
        serde_json::to_string(&format!("{agent}, with its own settings")).unwrap_or_default()
    )
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
            Self::Runtime {
                runtime, pinned, ..
            } => {
                let mut actions = vec![MachineAction::Move];
                if *pinned {
                    actions.push(MachineAction::Unpin);
                }
                actions.push(MachineAction::Servers);
                // `global` hosts no ACP agents.
                if runtime.as_str() != RuntimeId::GLOBAL {
                    actions.push(MachineAction::Agents);
                }
                actions
            }
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

/// What a client may do about a runtime's connection: only a runtime the
/// daemon starts from a template is connected and disconnected by hand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionControl {
    /// Not connected: connecting tries that it starts.
    Connect,
    /// Connected and idle.
    Disconnect,
    /// Connected, and a run or background call uses it: why it cannot be
    /// disconnected now.
    Busy(String),
}

impl ConnectionControl {
    /// The action the control stands for.
    #[must_use]
    pub const fn action(&self) -> MachineAction {
        match self {
            Self::Connect => MachineAction::Connect,
            Self::Disconnect | Self::Busy(_) => MachineAction::Disconnect,
        }
    }
}

/// The connection control of `listing`; `None` for a runtime that is built
/// in or dials in, which connects on its own.
#[must_use]
pub fn connection_control(listing: &RuntimeListing) -> Option<ConnectionControl> {
    if listing.lifetime != RuntimeLifetime::Spawned {
        return None;
    }
    if !listing.connected {
        return Some(ConnectionControl::Connect);
    }
    let count = |count: u32, one: &str, many: &str| match count {
        0 => None,
        1 => Some(format!("1 {one}")),
        count => Some(format!("{count} {many}")),
    };
    let uses = [
        count(listing.active_runs, "active run", "active runs"),
        count(
            listing.background_calls,
            "background call",
            "background calls",
        ),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    Some(if uses.is_empty() {
        ConnectionControl::Disconnect
    } else {
        ConnectionControl::Busy(format!("in use by {}", uses.join(" and ")))
    })
}

/// Whether `action` (connect or disconnect) applies to runtime `runtime_id`
/// of `runtimes` now; the error is the status line that says why not.
pub fn connection_request(
    runtimes: &[RuntimeListing],
    runtime_id: &RuntimeId,
    action: MachineAction,
) -> Result<(), String> {
    let listing = runtimes
        .iter()
        .find(|runtime| runtime.runtime_id == *runtime_id)
        .ok_or_else(|| format!("runtime `{runtime_id}` is not a known runtime"))?;
    match (connection_control(listing), action) {
        (None, _) => Err(format!(
            "runtime `{runtime_id}` connects on its own; only runtimes the daemon starts connect by hand"
        )),
        (Some(ConnectionControl::Connect), MachineAction::Connect)
        | (Some(ConnectionControl::Disconnect), MachineAction::Disconnect) => Ok(()),
        (Some(ConnectionControl::Busy(reason)), MachineAction::Disconnect) => Err(format!(
            "runtime `{runtime_id}` is {reason}; disconnect it once they end"
        )),
        (Some(ConnectionControl::Connect), _) => {
            Err(format!("runtime `{runtime_id}` is not connected"))
        }
        (Some(_), _) => Err(format!("runtime `{runtime_id}` is already connected")),
    }
}

/// The status line an answered `runtimes.connect` or `runtimes.disconnect`
/// gives.
#[must_use]
pub fn connection_status(listing: &RuntimeListing) -> String {
    if listing.connected {
        format!("runtime `{}` is connected", listing.runtime_id)
    } else {
        format!("runtime `{}` is disconnected", listing.runtime_id)
    }
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

    #[test]
    fn an_acp_agent_form_makes_a_declaration_or_says_what_is_wrong() {
        use super::AcpAgentFields;
        let mut fields = AcpAgentFields::add();
        fields.agent = "Claude".into();
        assert!(fields.definition().unwrap_err().contains("agent ID"));
        fields.agent = "claude".into();
        assert!(fields.definition().unwrap_err().contains("names a program"));
        fields.command = "npx -y claude".into();
        assert!(fields.definition().unwrap_err().contains("JSON array"));
        fields.command = r#"["npx", "-y", "@agentclientprotocol/claude-agent-acp"]"#.into();
        fields.environment = r#"{"CLAUDE_CONFIG_DIR": "/home/me/.claude"}"#.into();
        fields.description = " Claude Code ".into();
        let (agent, definition) = fields.definition().expect("a declaration");
        assert_eq!(agent.as_str(), "claude");
        assert_eq!(definition.command.len(), 3);
        assert_eq!(definition.env["CLAUDE_CONFIG_DIR"], "/home/me/.claude");
        assert_eq!(definition.description, "Claude Code");
        // Its form again, for an edit.
        let declaration = cookie_agent_protocol::RuntimeAgentDeclaration {
            agent,
            definition,
            available: true,
            documents: Vec::new(),
        };
        assert_eq!(
            AcpAgentFields::edit(&declaration).definition().unwrap().1,
            declaration.definition
        );
    }

    #[test]
    fn new_acp_documents_run_the_declaration() {
        let own = super::acp_agent_document("claude", "");
        assert_eq!(
            own,
            "---\ndescription: \"ACP agent claude\"\nmode: all\nacp: {}\n---\n"
        );
        let template = super::acp_template_document("claude");
        assert!(template.contains("acp:\n  agent: claude\n"), "{template}");
        assert_eq!(super::agent_document_path("claude"), "agents/claude.md");
    }

    #[test]
    fn a_runtimes_acp_agents_are_its_declarations_with_the_agents_over_them() {
        use cookie_agent_protocol::{AgentDescriptor, AgentId, AgentMode, AgentRuntime};
        let offer = |runtime: &str, declaration: &str, available| AgentRuntime {
            runtime_id: RuntimeId::new(runtime).unwrap(),
            agent: AgentId::new(declaration).unwrap(),
            available,
            options: Vec::new(),
            input_media: Vec::new(),
        };
        let agent = |id: &str, description: &str, runtimes: Vec<AgentRuntime>| AgentDescriptor {
            official: false,
            official_version: None,
            id: AgentId::new(id).unwrap(),
            preset: None,
            description: description.into(),
            mode: AgentMode::All,
            enabled: true,
            runnable_as_root: true,
            resolved_fallback: Vec::new(),
            delegation_targets: Vec::new(),
            runtime: None,
            runtimes,
            unset_models: Vec::new(),
        };
        let agents = [
            agent("build", "Builds", Vec::new()),
            agent(
                "claude",
                "Claude Code",
                vec![
                    offer("labpc", "claude", true),
                    offer("local", "claude", false),
                ],
            ),
            // A template over `claude`, and one over `codex`, which no
            // document of its own ID describes.
            agent(
                "claude-review",
                "Reviews",
                vec![
                    offer("labpc", "claude", true),
                    offer("local", "claude", false),
                ],
            ),
            agent(
                "codex-fast",
                "Fast Codex",
                vec![offer("labpc", "codex", false)],
            ),
        ];
        let listed = super::runtime_acp_agents(&agents, &RuntimeId::new("labpc").unwrap());
        assert_eq!(
            listed
                .iter()
                .map(|declaration| (
                    declaration.agent.as_str(),
                    declaration.available,
                    declaration.description.as_str(),
                    declaration
                        .agents
                        .iter()
                        .map(AgentId::as_str)
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>(),
            [
                (
                    "claude",
                    true,
                    "Claude Code",
                    vec!["claude", "claude-review"]
                ),
                ("codex", false, "", vec!["codex-fast"]),
            ]
        );
        let local = super::runtime_acp_agents(&agents, &RuntimeId::new("local").unwrap());
        assert_eq!(local.len(), 1);
        assert!(!local[0].available);
        assert!(super::runtime_acp_agents(&agents, &RuntimeId::new("global").unwrap()).is_empty());
    }

    fn listing(
        id: &str,
        lifetime: cookie_agent_protocol::RuntimeLifetime,
        connected: bool,
        active_runs: u32,
        background_calls: u32,
    ) -> RuntimeListing {
        RuntimeListing {
            runtime_id: RuntimeId::new(id).unwrap(),
            lifetime,
            connected,
            capabilities: Vec::new(),
            platform: None,
            machine: None,
            home: None,
            message: None,
            active_runs,
            background_calls,
        }
    }

    #[test]
    fn only_a_runtime_the_daemon_starts_connects_by_hand_and_one_in_use_cannot_disconnect() {
        use super::{ConnectionControl, connection_control, connection_request};
        use cookie_agent_protocol::RuntimeLifetime::{Builtin, DialIn, Spawned};
        assert_eq!(
            connection_control(&listing("local", Builtin, true, 0, 0)),
            None
        );
        assert_eq!(
            connection_control(&listing("box", DialIn, false, 0, 0)),
            None
        );
        assert_eq!(
            connection_control(&listing("labpc", Spawned, false, 0, 0)),
            Some(ConnectionControl::Connect)
        );
        assert_eq!(
            connection_control(&listing("labpc", Spawned, true, 0, 0)),
            Some(ConnectionControl::Disconnect)
        );
        assert_eq!(
            connection_control(&listing("labpc", Spawned, true, 1, 0)),
            Some(ConnectionControl::Busy("in use by 1 active run".into()))
        );
        assert_eq!(
            connection_control(&listing("labpc", Spawned, true, 2, 1)),
            Some(ConnectionControl::Busy(
                "in use by 2 active runs and 1 background call".into()
            ))
        );

        let labpc = RuntimeId::new("labpc").unwrap();
        let runtimes = [listing("labpc", Spawned, true, 0, 3)];
        assert_eq!(
            connection_request(&runtimes, &labpc, MachineAction::Disconnect),
            Err(
                "runtime `labpc` is in use by 3 background calls; disconnect it once they end"
                    .into()
            )
        );
        assert_eq!(
            connection_request(&runtimes, &labpc, MachineAction::Connect),
            Err("runtime `labpc` is already connected".into())
        );
        let runtimes = [listing("labpc", Spawned, false, 0, 0)];
        assert_eq!(
            connection_request(&runtimes, &labpc, MachineAction::Connect),
            Ok(())
        );
        assert!(
            connection_request(
                &runtimes,
                &RuntimeId::new("box").unwrap(),
                MachineAction::Connect
            )
            .is_err()
        );
        let runtimes = [listing("box", DialIn, true, 0, 0)];
        assert!(
            connection_request(
                &runtimes,
                &RuntimeId::new("box").unwrap(),
                MachineAction::Disconnect
            )
            .unwrap_err()
            .contains("connects on its own")
        );
    }
}
