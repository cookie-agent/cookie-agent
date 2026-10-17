//! Runtime attachments: which runtimes a session uses, as specified in
//! `docs/site/specs/runtimes.md`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{AttachmentName, CapabilityName, RuntimeId, RuntimeKind};

/// One runtime a session is attached to.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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

/// The capability an attachment recorded before capabilities refers to:
/// `global` for the global runtime, `coding` for any other.
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
    /// `None` until the log holds an attach or detach event: such a session
    /// was created before runtimes and reads as attached to `global` and
    /// `local`.
    attachments: Option<Vec<RuntimeAttachment>>,
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
                let attachments = self.attachments.get_or_insert_with(Vec::new);
                attachments.retain(|attachment| &attachment.name != name);
                attachments.push(RuntimeAttachment {
                    name: name.clone(),
                    runtime_id: runtime_id.clone(),
                    capability: capability
                        .clone()
                        .unwrap_or_else(|| default_capability(runtime_id)),
                    kind: kind.clone(),
                    workspace: workspace.clone(),
                });
            }
            crate::EventPayload::RuntimeDetached { name } => {
                self.attachments
                    .get_or_insert_with(Vec::new)
                    .retain(|attachment| &attachment.name != name);
            }
            _ => {}
        }
    }

    /// The folded attachments, or `None` for a log created before runtimes.
    #[must_use]
    pub fn attachments(&self) -> Option<&[RuntimeAttachment]> {
        self.attachments.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An attach event written before capabilities names none: the global
    /// runtime's reads as `global`, any other as `coding`.
    #[test]
    fn attach_events_without_a_capability_read_as_global_or_coding() {
        let mut fold = RuntimeAttachmentFold::default();
        for (name, runtime, kind) in [
            ("global", "global", "global"),
            ("labpc_api", "labpc", "coding"),
        ] {
            let event = serde_json::json!({
                "type": "runtime_attached",
                "name": name,
                "runtime_id": runtime,
                "kind": kind,
            });
            fold.observe(&serde_json::from_value(event).expect("old attach event"));
        }
        fold.observe(&crate::EventPayload::RuntimeAttached {
            name: AttachmentName::new("devbox_notes").unwrap(),
            runtime_id: RuntimeId::new("devbox").unwrap(),
            capability: Some(CapabilityName::new("notes").unwrap()),
            kind: RuntimeKind::new("notes").unwrap(),
            workspace: None,
        });
        let capabilities = fold
            .attachments()
            .expect("attachments")
            .iter()
            .map(|attachment| attachment.capability.as_str())
            .collect::<Vec<_>>();
        assert_eq!(capabilities, ["global", "coding", "notes"]);
    }
}
