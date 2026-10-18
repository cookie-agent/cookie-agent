use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    GoalId, InvocationId, ProducerId, ProducerIdempotencyKey, ProducerName, ProducerSource,
    SessionId, ToolCallId,
};

/// Stable identity used with session + idempotency key for durable deduplication.
/// External identifies a producer outside the daemon, a plugin or a runtime:
/// the engine derives `source` from the authenticated connection, never from
/// send parameters, and the sender chooses `name` when it registers. The
/// connection epoch that authorizes live sends is runtime-only and never part
/// of this durable owner or the deduplication key.
/// GoalControl identifies engine-authored lifecycle steering, not a continuation
/// reminder; its accepted messages survive goal-controller teardown.
/// Agent identifies an in-tree agent mail sender; the session identity is
/// engine-derived from the executing tool context, never from model arguments.
/// Tool identifies the background result of one tool call that ran in the
/// background; the call identity is engine-derived from the executing tool
/// context, never from model arguments.
/// Subagent identifies a delegated session reporting its finished turns to
/// its parent.
/// The owner is durable and participates in `(session, owner, key)` dedup.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProducerOwner {
    /// A plugin's or a runtime's named producer.
    External {
        source: ProducerSource,
        name: ProducerName,
    },
    Delegation {
        invocation_id: InvocationId,
    },
    Goal {
        goal_id: GoalId,
    },
    GoalControl {
        goal_id: GoalId,
    },
    Agent {
        session_id: SessionId,
    },
    Tool {
        tool_call_id: ToolCallId,
    },
    /// A delegated session reporting a finished turn to its parent.
    Subagent {
        session_id: SessionId,
    },
}

impl ProducerOwner {
    /// The external owner `name` of `source`.
    #[must_use]
    pub fn external(source: ProducerSource, name: ProducerName) -> Self {
        Self::External { source, name }
    }

    /// The source of an external owner.
    #[must_use]
    pub fn source(&self) -> Option<&ProducerSource> {
        match self {
            Self::External { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Per-send mode. Queue remains accepted-but-deferred during an active run;
/// steer joins the next safe request without interrupting an in-flight request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProducerDeliveryMode {
    Steer,
    Queue,
}

/// Read-only runtime record. Never persist or restore a registration ID.
/// A fresh registration is required on each process start, even for retries.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerRegistration {
    pub producer_id: ProducerId,
    pub producer_owner: ProducerOwner,
    pub session_id: SessionId,
    pub age_ms: u64,
}

/// The recovery state of a producer source: whether it has restored the
/// external work it tracks since it connected.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProducerSourceStatus {
    Starting,
    Ready,
    Failed,
    Disabled,
}

/// Runtime startup readiness of a producer source (a producer-capable plugin
/// or runtime), not a durable producer record or recovery blob.
/// Failed/disabled means external work is unknown, not that work is complete.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerSourceState {
    pub source: ProducerSource,
    pub status: ProducerSourceStatus,
}

// --- producer operations, shared by the plugin and runtime protocols ----------

/// Sender-to-engine request; the connection supplies the producer's source,
/// and `name` labels this producer among the source's others. Registration is
/// allowed during recovery and does not wait for goal readiness.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerRegisterParams {
    pub session_id: SessionId,
    pub name: crate::ProducerName,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProducerRegisterResult {
    pub producer_id: ProducerId,
}

/// Send requires a live registration owned by this connection and session.
/// Identical (session, stable owner, key) retries return the original receipt;
/// different description, body, or mode is rejected. A missing ACK is commit-uncertain.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerSendParams {
    pub session_id: SessionId,
    pub producer_id: ProducerId,
    pub mode: crate::ProducerDeliveryMode,
    pub idempotency_key: ProducerIdempotencyKey,
    #[serde(deserialize_with = "deserialize_producer_description")]
    pub description: crate::SafeDisplayText,
    pub body: String,
}

fn deserialize_producer_description<'de, D>(
    deserializer: D,
) -> Result<crate::SafeDisplayText, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let description = crate::SafeDisplayText::deserialize(deserializer)?;
    if description.as_str().trim().is_empty() {
        return Err(serde::de::Error::custom("description must not be blank"));
    }
    Ok(description)
}

/// Returned only after durable ProducerMessageAccepted append. Retries return
/// this same receipt, including after consumption, with no second acceptance.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProducerSendResult {
    pub message_id: crate::ProducerMessageId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerUnregisterParams {
    pub session_id: SessionId,
    pub producer_id: ProducerId,
}

/// Unregister does not remove accepted messages; zero-send unregister is valid.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProducerUnregisterResult {}

/// Discard an owned message only while it remains waiting. Authority is the live
/// connection's source and the session, not a registration: any message of the
/// source, whatever its producer name.
///
/// A durable actor claim reserves the message and removes it from waiting before
/// request preparation and hooks. Discard rejects until that claim is released,
/// even before network activity; a claim does not prove provider receipt or execution.
/// Release after failed preparation or cancellation may return an unconsumed
/// message to waiting. Consumed messages cannot return or be discarded.
/// Discard rejection makes no exactly-once claim about execution or external effects.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerDiscardParams {
    pub session_id: SessionId,
    pub message_id: crate::ProducerMessageId,
}

/// Already-discarded owned messages succeed; consumed or currently claimed messages reject.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProducerDiscardResult {}

/// `producer/recovery/start`: the daemon to a producer source (a plugin or a
/// runtime), never a deadlineful pending request. Restore from the source's
/// own storage or services and report completion on the same connection. No
/// response, recovery deadline, replay, or eager session adoption is implied.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RecoveryStartParams {}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryOutcome {
    Ready,
    Failed { message: crate::SafeErrorMessage },
}

/// `producer/recovery/complete`: a producer source's recovery outcome. The
/// daemon owns the starting and disabled states; the connection supplies the
/// source and its epoch out of band.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryCompleteParams {
    pub outcome: RecoveryOutcome,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RecoveryCompleteResult {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn external_owners_round_trip_with_their_source_and_name() {
        let owner = ProducerOwner::external(
            ProducerSource::runtime("local").unwrap(),
            ProducerName::new("ci-watcher").unwrap(),
        );
        let wire = serde_json::to_value(&owner).unwrap();
        assert_eq!(
            wire,
            json!({"type": "external", "source": "runtime:local", "name": "ci-watcher"})
        );
        assert_eq!(
            serde_json::from_value::<ProducerOwner>(wire).unwrap(),
            owner
        );
    }

    #[test]
    fn plugin_owners_from_before_external_owners_are_rejected() {
        assert!(
            serde_json::from_value::<ProducerOwner>(json!({"type": "plugin", "plugin": "jobs"}))
                .is_err()
        );
    }

    #[test]
    fn producer_names_and_sources_are_bounded_labels() {
        for valid in ["default", "ci-watcher", "a", "0.watch_1", &"a".repeat(64)] {
            assert!(ProducerName::new(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "CI", "-ci", ".ci", "ci watcher", &"a".repeat(65)] {
            assert!(ProducerName::new(invalid).is_err(), "{invalid}");
        }
        assert_eq!(
            ProducerSource::plugin("jobs").unwrap().plugin_name(),
            Some("jobs")
        );
        assert_eq!(
            ProducerSource::runtime("local").unwrap().runtime_id(),
            Some("local")
        );
        for invalid in [
            "plugin",
            "plugin:",
            "plugin: ",
            ":jobs",
            "Plugin:jobs",
            "plugin:a\nb",
        ] {
            assert!(ProducerSource::new(invalid).is_err(), "{invalid}");
        }
    }
}
