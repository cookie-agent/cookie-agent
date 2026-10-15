//! The `runtimes.list` and `runtimes.reconnect` views of the engine's
//! runtime registry.

use cookie_agent_engine::{Engine, RuntimeInstanceStatus, RuntimeInstanceSummary};
use cookie_agent_protocol::{
    AttachmentName, RuntimeId, RuntimeInstanceInfo, RuntimeTemplateInfo, RuntimesListResult,
};

pub(crate) fn runtimes_list(engine: &Engine) -> RuntimesListResult {
    RuntimesListResult {
        templates: engine
            .runtime_templates()
            .into_iter()
            .filter_map(|template| {
                Some(RuntimeTemplateInfo {
                    runtime_id: RuntimeId::new(template.runtime_id).ok()?,
                    takes_workspace: template.takes_workspace,
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
