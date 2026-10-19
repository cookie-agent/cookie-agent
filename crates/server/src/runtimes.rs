//! The `runtimes.list` and `runtimes.reconnect` views of the engine's
//! runtime registry, and the `machines.list` view of its machines.

use cookie_agent_engine::{Engine, RuntimeInstanceStatus, RuntimeInstanceSummary};
use cookie_agent_protocol::{
    AttachmentName, CapabilityName, RuntimeId, RuntimeInstanceInfo, RuntimeListing,
    RuntimesListResult,
};

pub(crate) fn runtimes_list(engine: &Engine) -> RuntimesListResult {
    RuntimesListResult {
        runtimes: engine
            .runtime_templates()
            .into_iter()
            .filter_map(|template| {
                Some(RuntimeListing {
                    runtime_id: RuntimeId::new(template.runtime_id.as_str()).ok()?,
                    lifetime: template.lifetime,
                    connected: template.listing.connected,
                    capabilities: template.listing.capabilities,
                    platform: template.listing.platform,
                    machine: engine.runtime_machine(&template.runtime_id),
                    message: template.listing.message,
                })
            })
            .collect(),
        instances: engine
            .runtime_instances()
            .into_iter()
            .filter_map(instance_info)
            .collect(),
    }
}

/// An instance as clients see it; `None` for an invalid runtime ID.
pub(crate) fn instance_info(instance: RuntimeInstanceSummary) -> Option<RuntimeInstanceInfo> {
    Some(RuntimeInstanceInfo {
        runtime_id: RuntimeId::new(instance.runtime_id).ok()?,
        capability: CapabilityName::new(instance.capability).ok()?,
        attachment_name: instance
            .attachment_name
            .and_then(|name| AttachmentName::new(name).ok()),
        kind: instance.kind,
        workspace: instance.workspace,
        platform: instance.platform,
        status: match instance.status {
            RuntimeInstanceStatus::Starting => {
                cookie_agent_protocol::RuntimeInstanceStatus::Starting
            }
            RuntimeInstanceStatus::Ready => cookie_agent_protocol::RuntimeInstanceStatus::Ready,
            RuntimeInstanceStatus::Reconnecting => {
                cookie_agent_protocol::RuntimeInstanceStatus::Reconnecting
            }
            RuntimeInstanceStatus::Failed => cookie_agent_protocol::RuntimeInstanceStatus::Failed,
            RuntimeInstanceStatus::Stopped => cookie_agent_protocol::RuntimeInstanceStatus::Stopped,
        },
        message: instance.message,
    })
}

impl crate::Server {
    /// `machines.list`, and the answer of every `machines.*` change.
    pub(crate) fn machines_list(&self) -> cookie_agent_protocol::MachinesListResult {
        cookie_agent_protocol::MachinesListResult {
            machines: self.engine.machines(),
        }
    }
}
