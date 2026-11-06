//! The `runtimes.list` and `runtimes.reconnect` views of the engine's
//! runtime registry, and the `machines.list` view of its machines.

use cookie_agent_engine::{Engine, RuntimeInstanceStatus, RuntimeInstanceSummary};
use cookie_agent_protocol::{
    AttachmentName, CapabilityName, RuntimeId, RuntimeInstanceInfo, RuntimeListing,
    RuntimesListResult,
};

/// What one session may learn of `listing`: the runtimes of `attachments`
/// other than `global`, each saying what it is. What a runtime is doing and why
/// it last failed describe the daemon beyond the session, and a connection's
/// error can hold hosts and usernames, so they stay out.
pub(crate) fn session_runtimes_list(
    listing: RuntimesListResult,
    attachments: &[cookie_agent_protocol::RuntimeAttachment],
) -> RuntimesListResult {
    RuntimesListResult {
        runtimes: listing
            .runtimes
            .into_iter()
            .filter(|runtime| {
                runtime.runtime_id.as_str() != RuntimeId::GLOBAL
                    && attachments
                        .iter()
                        .any(|attachment| attachment.runtime_id == runtime.runtime_id)
            })
            .map(|runtime| RuntimeListing {
                capabilities: Vec::new(),
                message: None,
                active_runs: 0,
                background_calls: 0,
                ..runtime
            })
            .collect(),
        instances: Vec::new(),
    }
}

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

#[cfg(test)]
mod session_runtimes_tests {
    use cookie_agent_protocol::{
        AttachmentName, CapabilityName, MachineName, RuntimeAttachment, RuntimeCapabilityInfo,
        RuntimeKind, RuntimeLifetime,
    };

    use super::*;

    fn listed(id: &str) -> RuntimeListing {
        RuntimeListing {
            runtime_id: RuntimeId::new(id).unwrap(),
            lifetime: RuntimeLifetime::Spawned,
            connected: true,
            capabilities: vec![RuntimeCapabilityInfo {
                name: CapabilityName::coding(),
                kind: RuntimeKind::new("coding").unwrap(),
                takes_workspace: true,
                description: String::new(),
            }],
            platform: Some("linux".into()),
            machine: Some(MachineName::new(format!("{id}-box")).unwrap()),
            home: Some("/home/someone".into()),
            message: Some("ssh someone@host: refused".into()),
            active_runs: 2,
            background_calls: 1,
        }
    }

    fn attached(id: &str) -> RuntimeAttachment {
        RuntimeAttachment {
            name: AttachmentName::new(format!("{id}_work")).unwrap(),
            runtime_id: RuntimeId::new(id).unwrap(),
            capability: CapabilityName::coding(),
            kind: RuntimeKind::new("coding").unwrap(),
            workspace: None,
        }
    }

    #[test]
    fn a_session_learns_its_own_runtimes_and_what_they_are_alone() {
        let listing = RuntimesListResult {
            runtimes: vec![listed(RuntimeId::GLOBAL), listed("mine"), listed("other")],
            instances: Vec::new(),
        };
        let result =
            session_runtimes_list(listing, &[attached(RuntimeId::GLOBAL), attached("mine")]);
        assert_eq!(result.runtimes.len(), 1);
        let mine = &result.runtimes[0];
        assert_eq!(mine.runtime_id.as_str(), "mine");
        assert_eq!(mine.machine.as_ref().unwrap().as_str(), "mine-box");
        assert_eq!(mine.home.as_deref(), Some("/home/someone"));
        assert_eq!(mine.platform.as_deref(), Some("linux"));
        assert!(mine.connected);
        assert!(mine.capabilities.is_empty());
        assert_eq!(mine.message, None);
        assert_eq!((mine.active_runs, mine.background_calls), (0, 0));
        assert!(result.instances.is_empty());
    }
}
