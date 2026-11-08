//! The approval reviewer's own log (`docs/site/specs/forever-sessions.md`
//! section 5.5; client protocol 1.92.0).
//!
//! The reviewer of a session keeps its conversation in a log of its own, a
//! hidden child session filed in the reviewed session's folder, which no
//! listing, tree, or client reads. Its records after the creation are
//! [`crate::EventPayload::ReviewerTurnRecorded`], one per review: the `user`
//! turn and the verdict as they were sent, what the turn holds as entries a
//! later epoch renders again, and where the next review reads the session's
//! log from.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ApprovalId, ProducerMessageId, RunId, ToolCallId};

/// One review: the `user` turn of the call under review and the verdict that
/// answered it, as sent, with what a later review needs.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReviewerTurn {
    pub approval_id: ApprovalId,
    /// The reviewed session's `approval_requested` of the call: the review's
    /// cut. The turn holds what the log positions after the previous turn's
    /// cut and up to this one.
    #[schemars(range(min = 1))]
    pub cut_seq: u64,
    /// The reviewed session's `approval_step` that recorded the verdict. A
    /// revert or fork of that session before it leaves the turn out.
    #[schemars(range(min = 1))]
    pub evaluated_seq: u64,
    /// The name of the tool the reviewed call is of, as a later epoch's
    /// carried verdicts name it.
    pub tool: String,
    /// The `user` turn as sent.
    pub request: String,
    /// The `assistant` turn as sent: the verdict as canonical JSON.
    pub verdict: String,
    /// The entries this turn renders since the previous turn, in the log's
    /// order. Empty on an epoch's first turn, whose `epoch` holds them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<ReviewerEntry>,
    /// Set on the first turn of an epoch: the conversation started over
    /// here, from what the reviewer's log kept, and holds nothing from
    /// before. The reviewer's log lets go of the turns before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<ReviewerEpoch>,
    /// What reading the reviewed session's log on from `cut_seq` needs of
    /// what came before it.
    pub cursor: ReviewerCursor,
}

/// An entry of the reviewer's conversation, as rendered, at its position in
/// the reviewed session's log: a user message, or an entry of the agent's
/// trajectory. A delegated session's inherited messages are at position 0.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReviewerEntry {
    pub seq: u64,
    pub text: String,
    /// A user message, rather than a trajectory entry.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub message: bool,
}

/// What an epoch's first turn kept for the epochs after it: the entries a
/// later fresh turn can still render within its budgets, and what was left
/// out before them.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReviewerEpoch {
    /// The user messages a later epoch can keep, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<ReviewerEntry>,
    /// The user messages an epoch left out, which no later one keeps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_omitted: Option<ReviewerOmitted>,
    /// The newest trajectory entries, up to the first that puts them over
    /// the trajectory's budget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trajectory: Vec<ReviewerEntry>,
    /// How many older trajectory entries were let go of.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub trajectory_omitted: u64,
    /// The verdicts before this turn that a later epoch carries forward, at
    /// most five, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verdicts: Vec<ReviewerVerdict>,
}

/// User messages an epoch left out: how many, their tokens by the engine's
/// estimate, and the position of the first.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReviewerOmitted {
    pub count: u64,
    pub tokens: u64,
    pub first_seq: u64,
}

/// An earlier verdict, as a new epoch carries it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReviewerVerdict {
    /// The cut of the review it answered.
    pub cut_seq: u64,
    pub tool: String,
    pub verdict: String,
}

/// What the reviewed session's log says through a cut, as far as rendering
/// what comes after it needs.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReviewerCursor {
    /// How many runs the log named through the cut, in the order each first
    /// appears: an entry says which run it came in by this count.
    pub runs: u64,
    /// The latest of those runs, at most 64, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_runs: Vec<ReviewerRun>,
    /// Calls started and not yet ended through the cut that the trajectory
    /// shows, with the entry each renders as for each way it can end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_calls: Vec<ReviewerOpenCall>,
    /// Producer messages accepted and not yet consumed or discarded that the
    /// conversation shows, at most 256.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<ReviewerMessage>,
    /// The runtimes the log attached, by attachment name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ReviewerAttachment>,
    /// The `AGENTS.md` texts of the newest run that loaded any. Empty for a
    /// delegated session, whose `inherited` holds its root's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents_md: Vec<ReviewerAgentsMd>,
    /// Set for a delegated session: what it inherited of its root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited: Option<ReviewerInherited>,
}

/// A run of the reviewed session: its number, and whether a message the
/// conversation shows came in it yet.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReviewerRun {
    pub run_id: RunId,
    pub ordinal: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_input: bool,
}

/// A call still open at a cut, with the trajectory entry it renders as for
/// each way it can end.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReviewerOpenCall {
    pub tool_call_id: ToolCallId,
    pub completed: String,
    pub failed: String,
    pub denied: String,
    pub cancelled: String,
}

/// A producer message the conversation shows once it is consumed: its
/// label and its body as the entry renders them.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReviewerMessage {
    pub message_id: ProducerMessageId,
    pub label: String,
    pub body: String,
}

/// A runtime the reviewed session attached, as the request block names
/// where a call runs.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReviewerAttachment {
    pub name: String,
    pub runtime_id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// An `AGENTS.md` record of the reviewed session: its position, its run,
/// and its text.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReviewerAgentsMd {
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub text: String,
}

/// What a delegated session's reviewer inherited of its root when its log
/// was created: the root's `AGENTS.md`, as the conversation renders it.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ReviewerInherited {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl ReviewerTurn {
    /// Whether the turn is whole: its cut precedes its verdict, and neither
    /// turn is empty.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.cut_seq >= 1
            && self.evaluated_seq > self.cut_seq
            && !self.request.is_empty()
            && !self.verdict.is_empty()
    }
}
