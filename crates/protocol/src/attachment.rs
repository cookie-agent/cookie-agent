//! Runtime attachments: which runtimes a session uses, as specified in
//! `docs/site/specs/runtimes.md`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{AttachmentName, RuntimeId};

/// What a runtime offers. A runtime reports its own kind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    /// The daemon's own runtime: MCP servers and plugins, no workspace.
    Global,
    /// A runtime that works in one workspace with file and shell tools.
    Coding,
}

impl RuntimeKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Coding => "coding",
        }
    }
}

/// One runtime a session is attached to.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeAttachment {
    /// Session-unique attachment name; the tool prefix on collisions.
    pub name: AttachmentName,
    /// Template the runtime instance was started from.
    pub runtime_id: RuntimeId,
    pub kind: RuntimeKind,
    /// Canonical workspace path the runtime reported; absent without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
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
                kind,
                workspace,
            } => {
                let attachments = self.attachments.get_or_insert_with(Vec::new);
                attachments.retain(|attachment| &attachment.name != name);
                attachments.push(RuntimeAttachment {
                    name: name.clone(),
                    runtime_id: runtime_id.clone(),
                    kind: *kind,
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
