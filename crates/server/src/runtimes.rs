//! The `runtimes.list` and `runtimes.reconnect` views of the engine's
//! runtime registry, and the `machines.list` view of its machines.

use cookie_agent_engine::{Engine, RuntimeInstanceStatus, RuntimeInstanceSummary};
use cookie_agent_protocol::{
    AttachmentName, CapabilityName, RuntimeId, RuntimeInstanceInfo, RuntimeListing,
    RuntimesListResult,
};

pub(crate) fn runtimes_list(engine: &Engine) -> RuntimesListResult {
    let mut activity = engine.runtime_activity();
    RuntimesListResult {
        runtimes: engine
            .runtime_templates()
            .into_iter()
            .filter_map(|template| {
                // A runtime this process has not seen connected shows what
                // the registry remembers of it.
                let remembered = (!template.listing.connected)
                    .then(|| engine.remembered_runtime(&template.runtime_id))
                    .flatten();
                let (platform, home) = match remembered {
                    Some(remembered) => (
                        template.listing.platform.or(remembered.platform),
                        template.listing.home.or(remembered.home),
                    ),
                    None => (template.listing.platform, template.listing.home),
                };
                let busy = activity.remove(&template.runtime_id).unwrap_or_default();
                Some(RuntimeListing {
                    runtime_id: RuntimeId::new(template.runtime_id.as_str()).ok()?,
                    lifetime: template.lifetime,
                    connected: template.listing.connected,
                    capabilities: template.listing.capabilities,
                    platform,
                    machine: engine.runtime_machine(&template.runtime_id),
                    home,
                    message: template.listing.message,
                    active_runs: busy.active_runs,
                    background_calls: busy.background_calls,
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
    /// Runtime `runtime_id` as `runtimes.list` reports it, for the answer of
    /// `runtimes.connect` and `runtimes.disconnect`.
    pub(crate) fn runtime_listing(&self, runtime_id: &RuntimeId) -> Option<RuntimeListing> {
        runtimes_list(&self.engine)
            .runtimes
            .into_iter()
            .find(|runtime| runtime.runtime_id == *runtime_id)
    }

    /// `machines.list`, and the answer of every `machines.*` change.
    pub(crate) fn machines_list(&self) -> cookie_agent_protocol::MachinesListResult {
        cookie_agent_protocol::MachinesListResult {
            machines: self.engine.machines(),
        }
    }
}
