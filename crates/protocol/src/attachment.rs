//! Runtime attachments: which runtimes a session uses, as specified in
//! `docs/site/specs/runtimes.md`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{AttachmentName, CapabilityName, RuntimeId, RuntimeKind};

/// One runtime a session is attached to.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
pub struct RuntimeAttachment {
    /// Session-unique attachment name; the tool prefix on collisions.
    pub name: AttachmentName,
    /// The runtime the capability instance belongs to.
    pub runtime_id: RuntimeId,
    /// The capability of the runtime the session uses.
    pub capability: CapabilityName,
    /// The capability's self-declared kind.
    pub kind: RuntimeKind,
    /// Canonical workspace path the runtime reported; absent without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// The capability a runtime's attachment most likely refers to before its
/// instance reports one: `global` for the global runtime, `coding` for any
/// other. Clients use it to label a runtime they have not opened yet.
#[must_use]
pub fn default_capability(runtime_id: &RuntimeId) -> CapabilityName {
    if runtime_id.as_str() == RuntimeId::GLOBAL {
        CapabilityName::global()
    } else {
        CapabilityName::coding()
    }
}

/// The attachment set a session's events describe: the fold of its
/// `runtime_attached` and `runtime_detached` events in log order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeAttachmentFold {
    attachments: Vec<RuntimeAttachment>,
}

impl RuntimeAttachmentFold {
    /// Folds one event payload.
    pub fn observe(&mut self, payload: &crate::EventPayload) {
        match payload {
            crate::EventPayload::RuntimeAttached {
                name,
                runtime_id,
                capability,
                kind,
                workspace,
            } => {
                self.attachments
                    .retain(|attachment| &attachment.name != name);
                self.attachments.push(RuntimeAttachment {
                    name: name.clone(),
                    runtime_id: runtime_id.clone(),
                    capability: capability.clone(),
                    kind: kind.clone(),
                    workspace: workspace.clone(),
                });
            }
            crate::EventPayload::RuntimeDetached { name } => {
                self.attachments
                    .retain(|attachment| &attachment.name != name);
            }
            _ => {}
        }
    }

    /// The folded attachments, in attach order.
    #[must_use]
    pub fn attachments(&self) -> &[RuntimeAttachment] {
        &self.attachments
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fold_follows_attach_and_detach_in_log_order() {
        let attached =
            |name: &str, runtime: &str, capability: &str| crate::EventPayload::RuntimeAttached {
                name: AttachmentName::new(name).unwrap(),
                runtime_id: RuntimeId::new(runtime).unwrap(),
                capability: CapabilityName::new(capability).unwrap(),
                kind: RuntimeKind::new(capability).unwrap(),
                workspace: None,
            };
        let mut fold = RuntimeAttachmentFold::default();
        assert!(fold.attachments().is_empty());
        fold.observe(&attached("global", "global", "global"));
        fold.observe(&attached("devbox_notes", "devbox", "notes"));
        fold.observe(&crate::EventPayload::RuntimeDetached {
            name: AttachmentName::new("global").unwrap(),
        });
        fold.observe(&attached("labpc_api", "labpc", "coding"));
        let names = fold
            .attachments()
            .iter()
            .map(|attachment| attachment.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["devbox_notes", "labpc_api"]);
    }
}
