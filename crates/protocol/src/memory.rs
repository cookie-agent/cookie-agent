//! The memory snapshot a session's model sees without searching
//! (`docs/site/specs/memory.md` section 7), as the `memory_loaded` event
//! stores it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{AgentId, AttachmentName, EventSchemaError, MachineName};

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// What a session's model was shown of memory from one point on: the user
/// categories, the run's agent's file, and an index of the session's
/// machines' memories. A snapshot without `agent` is empty: memory is not
/// available from that point on, and the model sees no memory turn.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemorySnapshot {
    /// The snapshot was taken for a delegated run: it holds only the run's
    /// agent's file.
    #[serde(default, skip_serializing_if = "is_false")]
    pub delegated: bool,
    /// The run could see the memory write tools, so the snapshot's text
    /// tells it how to save.
    #[serde(default, skip_serializing_if = "is_false")]
    pub writable: bool,
    /// Every valid user category that fits, in name order, including empty
    /// ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user: Vec<MemorySnapshotCategory>,
    /// Categories that did not fit, by name, in name order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_omitted: Vec<String>,
    /// The run's agent and its memory; absent in an empty snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<MemorySnapshotAgent>,
    /// One entry per machine of the session, in the order the session's
    /// attachments first name them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub machines: Vec<MemorySnapshotMachine>,
}

/// One user category file, whole.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemorySnapshotCategory {
    pub category: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    pub body: String,
}

/// The run's agent and its memory file.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemorySnapshotAgent {
    pub agent: AgentId,
    /// The agent's file, whole; absent when it does not exist, is invalid,
    /// or is `omitted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemorySnapshotMemory>,
    /// The file exists but its rendered text did not fit the snapshot.
    #[serde(default, skip_serializing_if = "is_false")]
    pub omitted: bool,
}

/// A memory file's content.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemorySnapshotMemory {
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    pub body: String,
}

/// One machine of the session and the index of its memories.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemorySnapshotMachine {
    pub machine: MachineName,
    /// The session's attachments that run on the machine; empty for the
    /// daemon's machine of a session with only `global`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentName>,
    /// The machine's memories that fit, newest file first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<MemorySnapshotIndexEntry>,
    /// How many more memories the machine has.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted: u64,
}

/// One line of a machine's memory index.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemorySnapshotIndexEntry {
    /// The memory URL, `machine://<machine>/<name>`.
    pub url: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
}

impl MemorySnapshot {
    /// The most bytes a snapshot's JSON may take when written. Its rendered
    /// text stays within 64 KiB; JSON escaping can lengthen text with many
    /// line breaks or quotes, so the stored form gets twice that.
    pub const MAX_JSON_BYTES: usize = 128 * 1024;

    /// The snapshot that records memory is not available.
    #[must_use]
    pub fn unavailable() -> Self {
        Self::default()
    }

    /// Whether memory was unavailable from this snapshot on.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agent.is_none()
    }

    /// Checks the snapshot's shape: an empty snapshot holds nothing, a
    /// delegated one holds only its agent, and the writer's size cap.
    pub fn validate(&self) -> Result<(), EventSchemaError> {
        let invalid = Err(EventSchemaError::InvalidMemorySnapshot);
        let holds_more =
            !self.user.is_empty() || !self.user_omitted.is_empty() || !self.machines.is_empty();
        match &self.agent {
            None if holds_more || self.delegated || self.writable => return invalid,
            None => {}
            Some(_) if self.delegated && holds_more => return invalid,
            Some(agent) if agent.omitted && agent.memory.is_some() => return invalid,
            Some(_) => {}
        }
        let blank = |value: &str| value.trim().is_empty();
        if self
            .user
            .iter()
            .any(|category| blank(&category.category) || blank(&category.description))
            || self.user_omitted.iter().any(|category| blank(category))
            || self
                .agent
                .as_ref()
                .and_then(|agent| agent.memory.as_ref())
                .is_some_and(|memory| blank(&memory.description))
            || self.machines.iter().any(|machine| {
                machine
                    .memories
                    .iter()
                    .any(|memory| blank(&memory.url) || blank(&memory.description))
            })
        {
            return invalid;
        }
        if crate::event::writer_size_caps()
            && crate::event::json_len_exceeds(self, Self::MAX_JSON_BYTES).unwrap_or(true)
        {
            return invalid;
        }
        Ok(())
    }
}
