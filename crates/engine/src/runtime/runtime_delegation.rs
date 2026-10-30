//! Delegating to runtimes (`docs/site/specs/runtime-delegation.md`): the
//! `capabilities` a root's `delegate_subagent` gives a child, the
//! attachments every child starts with, and the delegated attachments a
//! root records for its badges.

use std::path::PathBuf;

use cookie_agent_protocol::{
    CapabilityName, EventPayload as Event, RuntimeAttachment, RuntimeId, SessionId, SessionOrigin,
};

use super::{Engine, EngineError};
use crate::{DelegatedCapability, session::SessionProjection};

/// Whether a session's `delegate_subagent` has `capabilities` (section
/// 2.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CapabilitiesParam {
    /// A delegated session, or a root without `global`.
    Absent,
    /// A root with `global` and other attachments: leaving it out inherits.
    Optional,
    /// A root with only `global`: it has no workspace of its own.
    Required,
}

/// Whether `attachments` include the `global` runtime.
pub(crate) fn has_global(attachments: &[RuntimeAttachment]) -> bool {
    attachments
        .iter()
        .any(|attachment| attachment.runtime_id.as_str() == RuntimeId::GLOBAL)
}

/// `<runtime_id>/<capability_id>`, as `delegate_subagent` takes it.
pub(crate) fn parse_capability(
    capability: &str,
    workspace: Option<String>,
) -> Result<DelegatedCapability, String> {
    let (runtime, name) = capability.split_once('/').ok_or_else(|| {
        format!(
            "`{capability}` is not `<runtime_id>/<capability_id>`; write the runtime and one of its capabilities as list_runtimes shows them, e.g. `labpc/coding`"
        )
    })?;
    let runtime_id = RuntimeId::new(runtime)
        .map_err(|_| format!("`{runtime}` in `{capability}` is not a runtime ID"))?;
    let capability_name = CapabilityName::new(name)
        .map_err(|_| format!("`{name}` in `{capability}` is not a capability name"))?;
    if runtime_id.as_str() == RuntimeId::GLOBAL {
        return Err(format!(
            "`{capability}` cannot be listed: `global` follows this session's root, so the subagent has it exactly when the root does"
        ));
    }
    if workspace
        .as_deref()
        .is_some_and(|workspace| workspace.trim().is_empty())
    {
        return Err(format!("the workspace of `{capability}` is empty"));
    }
    Ok(DelegatedCapability {
        runtime_id,
        capability: capability_name,
        workspace,
    })
}

impl Engine {
    /// The root session of `session`'s tree.
    fn root_projection(
        &self,
        session: &SessionProjection,
    ) -> Result<std::sync::Arc<SessionProjection>, EngineError> {
        match session.meta.origin {
            SessionOrigin::Delegated {
                root_session_id, ..
            } => Ok(self.inner.store.get(root_session_id)?),
            _ => Ok(self.inner.store.get(session.meta.session_id)?),
        }
    }

    /// Whether the root of `session`'s tree has `global` attached now.
    pub(crate) fn root_has_global(&self, session: SessionId) -> Result<bool, EngineError> {
        let projection = self.inner.store.get(session)?;
        let root = self.root_projection(&projection)?;
        Ok(has_global(&self.attachments_of(&root)))
    }

    /// Whether `session`'s `delegate_subagent` takes `capabilities`.
    pub(crate) fn delegate_capabilities_param(
        &self,
        session: SessionId,
    ) -> Result<CapabilitiesParam, EngineError> {
        let projection = self.inner.store.get(session)?;
        if !matches!(projection.meta.origin, SessionOrigin::Root) {
            return Ok(CapabilitiesParam::Absent);
        }
        let attachments = self.attachments_of(&projection);
        Ok(if !has_global(&attachments) {
            CapabilitiesParam::Absent
        } else if attachments.len() == 1 {
            CapabilitiesParam::Required
        } else {
            CapabilitiesParam::Optional
        })
    }

    /// Checks `capabilities` against the daemon's runtimes now: each runtime
    /// must exist, and a runtime that has said what it offers must offer the
    /// capability, with a workspace exactly when the capability takes one.
    pub(crate) fn check_delegated_capabilities(
        &self,
        capabilities: &[DelegatedCapability],
    ) -> Result<(), String> {
        if capabilities.is_empty() {
            return Err("capabilities must name at least one runtime capability".into());
        }
        let templates = self.runtime_templates();
        for requested in capabilities {
            let written = format!("{}/{}", requested.runtime_id, requested.capability);
            let Some(template) = templates
                .iter()
                .find(|template| template.runtime_id == requested.runtime_id.as_str())
            else {
                if let Some(offline) = self.remembered_offline(requested.runtime_id.as_str()) {
                    return Err(offline);
                }
                return Err(format!(
                    "there is no runtime `{}`; call list_runtimes for the runtimes this daemon has",
                    requested.runtime_id
                ));
            };
            let offered = &template.listing.capabilities;
            if offered.is_empty() {
                // It has not connected yet: starting it tells.
                continue;
            }
            let Some(capability) = offered
                .iter()
                .find(|offered| offered.name == requested.capability)
            else {
                let names = offered
                    .iter()
                    .map(|offered| format!("`{}/{}`", requested.runtime_id, offered.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "runtime `{}` has no capability `{}`; it offers {names}",
                    requested.runtime_id, requested.capability
                ));
            };
            match (capability.takes_workspace, &requested.workspace) {
                (true, None) => {
                    return Err(format!(
                        "`{written}` takes a workspace: give the directory on that runtime's machine"
                    ));
                }
                (false, Some(_)) => {
                    return Err(format!("`{written}` takes no workspace"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The attachments a child of the root `parent` gets for
    /// `capabilities`: the root's `global` when it has it, then each listed
    /// capability in order, started first. The first that cannot start
    /// fails the whole call, naming it.
    pub(crate) async fn open_delegated_capabilities(
        &self,
        parent: &SessionProjection,
        capabilities: &[DelegatedCapability],
    ) -> Result<Vec<RuntimeAttachment>, EngineError> {
        let refused = |message: String| EngineError::ToolFailed(message);
        if !matches!(parent.meta.origin, SessionOrigin::Root) {
            return Err(refused(
                "capabilities can only be given by a root session".into(),
            ));
        }
        let root = self.attachments_of(parent);
        if !has_global(&root) {
            return Err(refused(
                "capabilities need `global` attached to this session".into(),
            ));
        }
        self.check_delegated_capabilities(capabilities)
            .map_err(refused)?;
        let mut attachments = root
            .into_iter()
            .filter(|attachment| attachment.runtime_id.as_str() == RuntimeId::GLOBAL)
            .collect::<Vec<_>>();
        for requested in capabilities {
            let written = format!("{}/{}", requested.runtime_id, requested.capability);
            let instance = self
                .ensure_runtime(
                    requested.runtime_id.as_str(),
                    Some(requested.capability.as_str()),
                    requested.workspace.as_deref().map(PathBuf::from).as_deref(),
                )
                .await
                .map_err(|error| {
                    refused(match &requested.workspace {
                        Some(workspace) => {
                            format!("`{written}` could not be opened at `{workspace}`: {error}")
                        }
                        None => format!("`{written}` could not be opened: {error}"),
                    })
                })?;
            self.refuse_overlap_with(
                &attachments,
                &instance,
                &cookie_agent_runtime_api::SessionToolContext::new(SessionId::new_v7()),
            )
            .map_err(|error| refused(format!("`{written}`: {error}")))?;
            super::attachments::push_instance(&mut attachments, &instance)
                .map_err(|error| refused(format!("`{written}`: {error}")))?;
        }
        Ok(attachments)
    }

    /// The attachments a delegated child of `parent` inherits: its
    /// parent's, with `global` exactly when the root has it now (section
    /// 2.2).
    pub(crate) fn inherited_attachments(
        &self,
        parent: &SessionProjection,
    ) -> Result<Vec<RuntimeAttachment>, EngineError> {
        let root = self.attachments_of(&*self.root_projection(parent)?);
        let mut attachments = root
            .into_iter()
            .filter(|attachment| attachment.runtime_id.as_str() == RuntimeId::GLOBAL)
            .collect::<Vec<_>>();
        attachments.extend(
            self.attachments_of(parent)
                .into_iter()
                .filter(|attachment| attachment.runtime_id.as_str() != RuntimeId::GLOBAL),
        );
        Ok(attachments)
    }

    /// Records on the root of `child`'s tree the attachments `child`
    /// started with that the root records neither as its own nor as
    /// delegated, `global` aside (section 5.3).
    pub(crate) async fn record_delegated_attachments(
        &self,
        root: SessionId,
        child: SessionId,
        attachments: Vec<RuntimeAttachment>,
    ) -> Result<(), EngineError> {
        self.on_actor(root, move |engine| {
            let projection = engine.inner.store.get(root)?;
            if !matches!(projection.meta.origin, SessionOrigin::Root) {
                return Ok(());
            }
            let own = engine.attachments_of(&projection);
            let mut added: Vec<RuntimeAttachment> = Vec::new();
            for attachment in attachments {
                let known = attachment.runtime_id.as_str() == RuntimeId::GLOBAL
                    || own
                        .iter()
                        .chain(&projection.meta.delegated_attachments)
                        .chain(&added)
                        .any(|known| crate::session::same_place(known, &attachment));
                if !known {
                    added.push(attachment);
                }
            }
            if added.is_empty() {
                return Ok(());
            }
            engine.append_direct(
                root,
                None,
                super::event_origin("engine:delegation"),
                Event::DelegatedAttachmentsAdded {
                    child_session_id: child,
                    attachments: added,
                },
            )?;
            Ok(())
        })
        .await
    }
}

/// One runtime as `list_runtimes` prints it.
struct ListedRuntime {
    id: String,
    connected: bool,
    starts_on_use: bool,
    platform: Option<String>,
    home: Option<String>,
    message: Option<String>,
    capabilities: Vec<cookie_agent_protocol::RuntimeCapabilityInfo>,
    /// The ACP agents it offers, by ID.
    acp_agents: Vec<String>,
    attachments: Vec<(RuntimeAttachment, &'static str)>,
}

/// `value` escaped for an XML attribute.
fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            character => escaped.push(character),
        }
    }
    escaped
}

/// ` name="value"` for each attribute that has a value.
fn attributes(pairs: &[(&str, Option<&str>)]) -> String {
    pairs
        .iter()
        .filter_map(|(name, value)| value.map(|value| format!(" {name}=\"{}\"", xml_escape(value))))
        .collect()
}

/// The platform every runtime of `runtimes` with one reports, when they
/// agree.
fn shared_platform(runtimes: &[ListedRuntime]) -> Option<String> {
    let mut platforms = runtimes
        .iter()
        .filter_map(|runtime| runtime.platform.as_deref());
    let first = platforms.next()?;
    platforms
        .all(|platform| platform == first)
        .then(|| first.to_owned())
}

fn write_runtime(text: &mut String, indent: &str, runtime: &ListedRuntime, platform: bool) {
    let connected = if runtime.connected { "true" } else { "false" };
    let open = format!(
        "{indent}<runtime{}",
        attributes(&[
            ("id", Some(&runtime.id)),
            ("connected", Some(connected)),
            ("starts_on_use", runtime.starts_on_use.then_some("true")),
            ("platform", runtime.platform.as_deref().filter(|_| platform),),
            ("home", runtime.home.as_deref()),
            ("message", runtime.message.as_deref()),
            (
                "acp_agents",
                Some(runtime.acp_agents.join(", "))
                    .filter(|agents| !agents.is_empty())
                    .as_deref(),
            ),
        ])
    );
    if runtime.capabilities.is_empty() && runtime.attachments.is_empty() {
        text.push_str(&open);
        text.push_str("/>\n");
        return;
    }
    text.push_str(&open);
    text.push_str(">\n");
    for capability in &runtime.capabilities {
        let takes = if capability.takes_workspace {
            "true"
        } else {
            "false"
        };
        text.push_str(&format!(
            "{indent}  <capability{}/>\n",
            attributes(&[
                ("name", Some(capability.name.as_str())),
                ("takes_workspace", Some(takes)),
                (
                    "description",
                    Some(capability.description.as_str()).filter(|text| !text.is_empty()),
                ),
            ])
        ));
    }
    for (attachment, status) in &runtime.attachments {
        text.push_str(&format!(
            "{indent}  <attachment{}/>\n",
            attributes(&[
                ("name", Some(attachment.name.as_str())),
                ("capability", Some(attachment.capability.as_str())),
                ("workspace", attachment.workspace.as_deref()),
                ("status", Some(status)),
            ])
        ));
    }
    text.push_str(&format!("{indent}</runtime>\n"));
}

impl Engine {
    /// What `list_runtimes` prints for a call of `session`: every runtime
    /// but `global`, grouped by machine (the daemon's first, then by name)
    /// and by runtime ID, with `session`'s own attachments; `<global/>`
    /// when the root of `session`'s tree has `global`.
    pub(crate) fn list_runtimes_text(&self, session: SessionId) -> Result<String, EngineError> {
        use cookie_agent_protocol::RuntimeLifetime;

        use crate::runtimes::RuntimeInstanceStatus;

        let own = self.session_runtimes(session)?;
        let instances = self.runtime_instances();
        let acp_agents = self.inner.acp.runtime_agents();
        let status_of = |attachment: &RuntimeAttachment| -> &'static str {
            let status = instances.iter().find(|instance| {
                instance.runtime_id == attachment.runtime_id.as_str()
                    && instance.capability == attachment.capability.as_str()
                    && instance.workspace == attachment.workspace
            });
            match status.map(|instance| instance.status) {
                Some(RuntimeInstanceStatus::Starting) => "starting",
                Some(RuntimeInstanceStatus::Ready) => "ready",
                Some(RuntimeInstanceStatus::Reconnecting) => "reconnecting",
                Some(RuntimeInstanceStatus::Failed) => "failed",
                Some(RuntimeInstanceStatus::Stopped) | None => "stopped",
            }
        };
        let mut machines: Vec<(cookie_agent_protocol::MachineName, Vec<ListedRuntime>)> =
            Vec::new();
        let mut unplaced = Vec::new();
        for template in self.runtime_templates() {
            if template.runtime_id == RuntimeId::GLOBAL {
                continue;
            }
            let listing = template.listing;
            let remembered = (!listing.connected)
                .then(|| self.remembered_runtime(&template.runtime_id))
                .flatten();
            let (platform, home) = match remembered {
                Some(remembered) => (
                    listing.platform.or(remembered.platform),
                    listing.home.or(remembered.home),
                ),
                None => (listing.platform, listing.home),
            };
            let runtime = ListedRuntime {
                starts_on_use: !listing.connected && template.lifetime == RuntimeLifetime::Spawned,
                connected: listing.connected,
                platform,
                home,
                message: listing.message.filter(|_| !listing.connected),
                capabilities: listing.capabilities,
                acp_agents: acp_agents
                    .iter()
                    .filter(|agent| agent.runtime.as_str() == template.runtime_id)
                    .map(|agent| agent.id.to_string())
                    .collect(),
                attachments: own
                    .iter()
                    .filter(|attachment| attachment.runtime_id.as_str() == template.runtime_id)
                    .map(|attachment| (attachment.clone(), status_of(attachment)))
                    .collect(),
                id: template.runtime_id.clone(),
            };
            match self.runtime_machine(&template.runtime_id) {
                Some(machine) => match machines.iter_mut().find(|(name, _)| *name == machine) {
                    Some((_, runtimes)) => runtimes.push(runtime),
                    None => machines.push((machine, vec![runtime])),
                },
                None => unplaced.push(runtime),
            }
        }
        let daemon = self.runtime_machine(RuntimeId::GLOBAL);
        machines.sort_by(|(left, _), (right, _)| {
            (Some(left) != daemon.as_ref(), left).cmp(&(Some(right) != daemon.as_ref(), right))
        });
        let mut text = String::from("<runtimes>\n");
        if self.root_has_global(session)? {
            text.push_str(
                "  <!-- global is attached to this session's root, so every subagent in this tree has it, whether or not its capabilities list it. -->\n  <global/>\n",
            );
        }
        for (machine, runtimes) in &machines {
            let platform = shared_platform(runtimes);
            text.push_str(&format!(
                "  <machine{}>\n",
                attributes(&[
                    ("name", Some(machine.as_str())),
                    ("platform", platform.as_deref()),
                ])
            ));
            for runtime in runtimes {
                write_runtime(&mut text, "    ", runtime, platform.is_none());
            }
            text.push_str("  </machine>\n");
        }
        if !unplaced.is_empty() {
            // Only a runtime that never connected has no machine: the daemon
            // learns it as the runtime first connects
            // (`docs/site/specs/machines-on-demand.md` section 4.1).
            text.push_str(
                "  <!-- The daemon does not know these runtimes' machines yet: they are not connected. One is placed on a machine when it connects, as attaching or delegating to it makes it do. -->\n  <not_connected_yet>\n",
            );
            for runtime in &unplaced {
                write_runtime(&mut text, "    ", runtime, true);
            }
            text.push_str("  </not_connected_yet>\n");
        }
        text.push_str("</runtimes>");
        Ok(text)
    }
}
