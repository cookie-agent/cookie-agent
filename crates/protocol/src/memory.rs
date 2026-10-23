//! The memory snapshot a session's model sees without searching
//! (`docs/site/specs/memory.md` section 7), as the `memory_loaded` event
//! stores it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{AgentId, AttachmentName, EventSchemaError, MachineName, ProjectName, Sha256Digest};

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
///
/// Since client protocol 1.12.0 a snapshot holds only what the run's
/// `memory` rules let it read: a memory they allow is shown as before, one
/// they ask for is listed by URL and description only, and one they deny
/// or do not match is left out and not counted. A snapshot that holds no
/// memory at all after that renders no memory turn either.
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
    /// Categories the run's rules ask before reading, listed by URL and
    /// description only, in name order (client protocol 1.12.0).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_listed: Vec<MemorySnapshotIndexEntry>,
    /// The run's agent and its memory; absent in an empty snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<MemorySnapshotAgent>,
    /// One entry per machine of the session, in the order the session's
    /// attachments first name them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub machines: Vec<MemorySnapshotMachine>,
    /// The session's project: its overview whole and an index of its other
    /// memories (client protocol 1.8.0). Only root runs of a session in a
    /// project have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<MemorySnapshotProject>,
    /// A fingerprint of the `memory` rules reaching `global` (the agent's,
    /// then the session overlay's) the snapshot was taken under; a run whose
    /// rules differ takes a new snapshot (client protocol 1.12.0). Absent in
    /// an empty snapshot and in one an earlier daemon took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Sha256Digest>,
    /// When the snapshot was taken; the ages it shows are measured to this
    /// time, so equal snapshots render equal text (client protocol
    /// 1.14.0). Absent in an empty snapshot and in one an earlier daemon
    /// took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken_at: Option<jiff::Timestamp>,
}

/// How full one memory document was when a snapshot showed it, and when
/// it was last modified (client protocol 1.14.0). Every member is absent
/// in a snapshot an earlier daemon took. A document whose `size` exceeds
/// its `max_size` is over its limit: only writes that make it smaller are
/// accepted until it fits.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemoryDocumentStatus {
    /// When the memory was last modified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<jiff::Timestamp>,
    /// The file's size in bytes, frontmatter included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The `[memory]` limit of its kind, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u64>,
}

impl MemoryDocumentStatus {
    /// Whether the document was over its limit.
    #[must_use]
    pub fn over_limit(&self) -> bool {
        matches!((self.size, self.max_size), (Some(size), Some(max)) if size > max)
    }
}

/// The session's project and what the snapshot holds of its memories.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemorySnapshotProject {
    /// The project's name when the snapshot was taken.
    pub project: ProjectName,
    /// `project://<project>/overview`, whole; absent when it is empty,
    /// missing, invalid, or larger than what the snapshot had room for, in
    /// which case `memories` lists it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<MemorySnapshotMemory>,
    /// The project's other memories that fit, newest file first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<MemorySnapshotIndexEntry>,
    /// How many more memories the project has.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted: u64,
}

/// One user category file, whole.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemorySnapshotCategory {
    pub category: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    pub body: String,
    #[serde(flatten)]
    pub status: MemoryDocumentStatus,
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
    /// The file, listed by URL and description only, because the run's
    /// rules ask before reading it (client protocol 1.12.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed: Option<MemorySnapshotIndexEntry>,
}

/// A memory file's content.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemorySnapshotMemory {
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    pub body: String,
    #[serde(flatten)]
    pub status: MemoryDocumentStatus,
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

/// One line of a memory index: a machine's, a project's, or a memory the
/// run's rules ask before reading.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemorySnapshotIndexEntry {
    /// The memory URL, such as `machine://<machine>/<name>` or
    /// `project://<project>/<name>`.
    pub url: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    #[serde(flatten)]
    pub status: MemoryDocumentStatus,
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
        let holds_more = !self.user.is_empty()
            || !self.user_omitted.is_empty()
            || !self.user_listed.is_empty()
            || !self.machines.is_empty()
            || self.project.is_some();
        match &self.agent {
            None if holds_more
                || self.delegated
                || self.writable
                || self.rules.is_some()
                || self.taken_at.is_some() =>
            {
                return invalid;
            }
            None => {}
            Some(_) if self.delegated && holds_more => return invalid,
            Some(agent) if agent.omitted && agent.memory.is_some() => return invalid,
            Some(agent) if agent.listed.is_some() && (agent.omitted || agent.memory.is_some()) => {
                return invalid;
            }
            Some(_) => {}
        }
        let blank = |value: &str| value.trim().is_empty();
        if self
            .user
            .iter()
            .any(|category| blank(&category.category) || blank(&category.description))
            || self.user_omitted.iter().any(|category| blank(category))
            || self
                .user_listed
                .iter()
                .chain(self.agent.as_ref().and_then(|agent| agent.listed.as_ref()))
                .any(|memory| blank(&memory.url) || blank(&memory.description))
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
            || self.project.as_ref().is_some_and(|project| {
                project
                    .overview
                    .as_ref()
                    .is_some_and(|overview| blank(&overview.description))
                    || project
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

crate::open_enum! {
    /// What a memory is about (`docs/site/specs/memory.md` section 4).
    #[derive(
        Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
    )]
    #[serde(rename_all = "snake_case")]
    pub enum MemoryLayer {
        /// `user://<category>`: the user, in every task.
        User,
        /// `agent://<agent>`: one agent's way of doing its job.
        Agent,
        /// `machine://<machine>/<name>`: one machine.
        Machine,
        /// `project://<project>/<name>`: one body of work.
        Project,
    }
}

impl MemoryLayer {
    /// The four layers, in the order listings give them.
    pub const ALL: [Self; 4] = [Self::User, Self::Agent, Self::Machine, Self::Project];

    /// The layer's name, which is also its URL scheme; `unknown` for a
    /// layer this build does not know.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Machine => "machine",
            Self::Project => "project",
            Self::Unknown => "unknown",
        }
    }
}

/// List memory files. With no member, everything; `layer` keeps one layer,
/// and at most one of `agent`, `machine`, and `project` keeps one agent's
/// file, one machine's memories, or one project's (`layer`, when also
/// given, must be that one's layer).
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<MemoryLayer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<MachineName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// The memory files asked for, valid or not, and what else lies in their
/// folders.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemoryListResult {
    /// Memory files in layer order (`user`, `agent`, `machine`, `project`),
    /// then by URL.
    pub memories: Vec<MemoryInfo>,
    /// Entries of the memory folder that are not memory files of its
    /// layout, by path; search and the snapshot never read them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<MemoryIgnoredEntry>,
}

/// One memory file.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemoryInfo {
    /// The memory URL.
    pub url: String,
    pub layer: MemoryLayer,
    /// The machine of a `machine://` memory or the project of a
    /// `project://` one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The first 16 hex digits of the SHA-256 of the file's bytes; a write
    /// or delete sends it back.
    pub revision: String,
    /// The file's size in bytes.
    pub size: u64,
    /// The memory's description; absent for an invalid file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// Why the file is not a valid memory; search and the snapshot skip it
    /// until it is fixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<String>,
    /// An `agent://` file whose agent the agent registry no longer has.
    #[serde(default, skip_serializing_if = "is_false")]
    pub orphaned: bool,
    /// When the memory was created: its stamp, or the file's modification
    /// time before it has one (client protocol 1.14.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<jiff::Timestamp>,
    /// When the memory was last modified, likewise (client protocol
    /// 1.14.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<jiff::Timestamp>,
    /// When a model last read the memory, if the daemon knows (client
    /// protocol 1.14.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read: Option<jiff::Timestamp>,
    /// The `[memory]` limit of its kind, in bytes; a `size` above it is
    /// over its limit (client protocol 1.14.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u64>,
}

/// Something in the memory folder that is not a memory file of its layout.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemoryIgnoredEntry {
    /// The path relative to the memory folder, with `/` separators.
    pub path: String,
    /// Why it is not a memory file.
    pub reason: String,
}

/// Read one memory whole.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryReadParams {
    pub url: String,
}

/// A memory file's whole text, frontmatter included, and its revision.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemoryReadResult {
    pub url: String,
    pub text: String,
    pub revision: String,
    /// Why the file is not a valid memory, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<String>,
    /// When the memory was created and last modified, as in
    /// [`MemoryInfo`] (client protocol 1.14.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<jiff::Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<jiff::Timestamp>,
    /// The file's size and the `[memory]` limit of its kind, in bytes
    /// (client protocol 1.14.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u64>,
}

/// Create or replace one memory from its whole text, frontmatter included.
/// Without `revision` the memory must not exist; with it, it must exist at
/// that revision.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryWriteParams {
    pub url: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// The memory's new revision.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemoryWriteResult {
    pub url: String,
    pub revision: String,
    /// What the daemon kept in place of what the text gave, for the user
    /// to read: an `agent://` file keeps the description and the two
    /// keywords the daemon owns. Absent when the text was written as
    /// given. (Since 1.6.0.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// Delete one memory at the revision last read.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryDeleteParams {
    pub url: String,
    pub revision: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemoryDeleteResult {}

crate::open_enum! {
    /// What a model did to a memory (`docs/site/specs/memory.md` section
    /// 16.9.10).
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum MemoryChangeKind {
        /// `memory_write` created or replaced it.
        Saved,
        /// `memory_edit` changed it.
        Edited,
        /// `memory_delete` deleted it.
        Deleted,
    }
}

impl MemoryChangeKind {
    /// The verb a transcript line starts with: `Saved`, `Edited`, or
    /// `Deleted`; `Changed` for a kind this build does not know.
    #[must_use]
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Saved => "Saved",
            Self::Edited => "Edited",
            Self::Deleted => "Deleted",
            Self::Unknown => "Changed",
        }
    }
}

/// A memory file was created, changed, or deleted through this daemon;
/// refetch `memory.list`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MemoryChangedNotification {}

/// How long ago `then` was at `now`, as memory reads it out (memory spec
/// sections 16.9.4 and 16.9.7): whole units of the largest unit that
/// fits, from minutes to years (7 days a week, 30 a month, 365 a year),
/// such as `3 weeks`; `less than a minute` for anything shorter, a time
/// after `now` included.
#[must_use]
pub fn memory_age(then: jiff::Timestamp, now: jiff::Timestamp) -> String {
    let seconds = now.as_second().saturating_sub(then.as_second());
    let units: [(i64, &str); 6] = [
        (365 * 86_400, "year"),
        (30 * 86_400, "month"),
        (7 * 86_400, "week"),
        (86_400, "day"),
        (3_600, "hour"),
        (60, "minute"),
    ];
    units
        .iter()
        .find(|(length, _)| seconds >= *length)
        .map_or_else(
            || "less than a minute".to_owned(),
            |(length, unit)| {
                let count = seconds / length;
                format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
            },
        )
}

/// A memory's size for a reader, such as `812 bytes` or `1.4 KB`: bytes
/// under 1 KB, otherwise KB of 1024 bytes with at most one decimal.
#[must_use]
pub fn memory_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} {}", if bytes == 1 { "byte" } else { "bytes" });
    }
    let tenths = (bytes * 10 + 512) / 1024;
    if tenths.is_multiple_of(10) {
        format!("{} KB", tenths / 10)
    } else {
        format!("{}.{} KB", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod format_tests {
    use super::{memory_age, memory_size};

    fn at(seconds: i64) -> jiff::Timestamp {
        jiff::Timestamp::from_second(seconds).unwrap()
    }

    #[test]
    fn ages_use_the_largest_whole_unit() {
        let now = at(1_000_000_000);
        for (ago, expected) in [
            (-5, "less than a minute"),
            (59, "less than a minute"),
            (60, "1 minute"),
            (3_599, "59 minutes"),
            (3_600, "1 hour"),
            (2 * 86_400, "2 days"),
            (6 * 86_400 + 86_399, "6 days"),
            (7 * 86_400, "1 week"),
            (21 * 86_400, "3 weeks"),
            (30 * 86_400, "1 month"),
            (364 * 86_400, "12 months"),
            (365 * 86_400, "1 year"),
            (800 * 86_400, "2 years"),
        ] {
            assert_eq!(memory_age(at(1_000_000_000 - ago), now), expected, "{ago}");
        }
    }

    #[test]
    fn sizes_switch_to_kilobytes_at_1024_bytes() {
        for (bytes, expected) in [
            (0, "0 bytes"),
            (1, "1 byte"),
            (812, "812 bytes"),
            (1023, "1023 bytes"),
            (1024, "1 KB"),
            (1434, "1.4 KB"),
            (2048, "2 KB"),
            (2611, "2.5 KB"),
        ] {
            assert_eq!(memory_size(bytes), expected, "{bytes}");
        }
    }
}
