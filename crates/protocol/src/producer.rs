use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{GoalId, InvocationId, ProducerId, SessionId};

/// Stable identity used with session + idempotency key for durable deduplication.
/// Plugin connection authorization is runtime-only; the engine derives `plugin`
/// from the authenticated configured plugin name, never from send parameters.
/// The transport supplies name + connection epoch for live authority separately;
/// the epoch is never part of this durable owner or the deduplication key.
/// GoalControl identifies engine-authored lifecycle steering, not a continuation
/// reminder; its accepted messages survive goal-controller teardown.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProducerOwner {
    Plugin { plugin: String },
    Delegation { invocation_id: InvocationId },
    Goal { goal_id: GoalId },
    GoalControl { goal_id: GoalId },
}

/// Per-send mode. Queue remains accepted-but-deferred during an active run;
/// steer joins the next safe request without interrupting an in-flight request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProducerDeliveryMode {
    Steer,
    Queue,
}

/// Read-only runtime record. Never persist or restore a registration ID.
/// A fresh registration is required on each process start, even for retries.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProducerRegistration {
    pub producer_id: ProducerId,
    pub producer_owner: ProducerOwner,
    pub session_id: SessionId,
    pub age_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PluginRecoveryStatus {
    Starting,
    Ready,
    Failed,
    Disabled,
}

/// Runtime startup readiness, not a durable producer record or recovery blob.
/// Failed/disabled means external work is unknown, not that work is complete.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PluginRecoveryState {
    pub plugin: String,
    pub status: PluginRecoveryStatus,
}
