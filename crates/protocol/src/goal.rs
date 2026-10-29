use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::GoalId;

/// A value this build does not know reads as `cancelled`: a cancelled goal,
/// which allows no lifecycle change.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum GoalStatus {
    Active,
    Paused,
    Completed,
    #[serde(other)]
    Cancelled,
}

/// User-only transitions. Completion is derived from a nonempty finished checklist.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalLifecycleAction {
    Pause,
    Resume,
    Cancel,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct GoalItem {
    pub description: String,
    pub finished: bool,
}

/// Root-session projection, rebuilt from goal events independently of compaction.
/// Activation starts with an empty checklist; each subsequent event advances revision.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct GoalState {
    pub goal_id: GoalId,
    pub objective: String,
    pub status: GoalStatus,
    pub items: Vec<GoalItem>,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoalGetParams {}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoalGetResult {
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<GoalState>", required)]
    pub goal: Option<GoalState>,
}

/// `goal_update` replaces the current root goal's checklist at actor execution,
/// even if the goal changed since the calling run started. It is never a lifecycle
/// command. The engine rejects absent/terminal goals and blank descriptions.
/// Replacements are serialized; the last accepted update wins. Duplicate descriptions
/// are allowed. Empty items preserve active/paused status. A nonempty
/// all-finished update completes even while paused, without scheduling a wake.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoalUpdateParams {
    pub items: Vec<GoalItem>,
}

/// `goal_set` starts a goal on the agent's own initiative, as the user's
/// `/goal` does, when the root session has no active or paused goal. It may
/// set the first checklist in the same call.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoalSetParams {
    /// What the goal achieves, in a sentence or two.
    pub objective: String,
    /// The first checklist; leave it out to set it later with goal_update.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<GoalItem>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoalUpdateResult {
    pub goal: GoalState,
}

/// Whether a reminder introduces the objective or continues previously consumed work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalReminderKind {
    Started,
    Continuation,
}

/// Pending reminder metadata, NOT a durable send idempotency key.
/// Coalescing uses the goal ID and revision, independently of the presentation kind.
/// After consumption another continuation at the same revision uses a fresh send.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
pub struct GoalReminderIdentity {
    pub goal_id: GoalId,
    pub revision: u64,
    pub kind: GoalReminderKind,
}
