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
/// A stored `plugin { plugin }` owner, from before external owners, decodes
/// as `external { source: "plugin:<plugin>", name: "default" }`.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
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

/// [`ProducerOwner`] as stored, including the owner kinds it replaced. Its
/// schema is the owner's: everything a stored event may carry.
#[derive(Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum StoredProducerOwner {
    /// Before external owners; decodes as `external` with name `default`.
    /// Never written.
    Plugin {
        plugin: String,
    },
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
    Subagent {
        session_id: SessionId,
    },
}

impl JsonSchema for ProducerOwner {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ProducerOwner")
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        StoredProducerOwner::json_schema(generator)
    }
}

impl<'de> Deserialize<'de> for ProducerOwner {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match StoredProducerOwner::deserialize(deserializer)? {
            StoredProducerOwner::Plugin { plugin } => Self::External {
                source: ProducerSource::plugin(&plugin).map_err(serde::de::Error::custom)?,
                name: ProducerName::default_name(),
            },
            StoredProducerOwner::External { source, name } => Self::External { source, name },
            StoredProducerOwner::Delegation { invocation_id } => Self::Delegation { invocation_id },
            StoredProducerOwner::Goal { goal_id } => Self::Goal { goal_id },
            StoredProducerOwner::GoalControl { goal_id } => Self::GoalControl { goal_id },
            StoredProducerOwner::Agent { session_id } => Self::Agent { session_id },
            StoredProducerOwner::Tool { tool_call_id } => Self::Tool { tool_call_id },
            StoredProducerOwner::Subagent { session_id } => Self::Subagent { session_id },
        })
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ProducerDiscardResult {}

/// `producer/recovery/start`: the daemon to a producer source (a plugin or a
/// runtime), never a deadlineful pending request. Restore from the source's
/// own storage or services and report completion on the same connection. No
/// response, recovery deadline, replay, or eager session adoption is implied.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    fn stored_plugin_owners_decode_as_the_plugins_default_producer() {
        let owner: ProducerOwner =
            serde_json::from_value(json!({"type": "plugin", "plugin": "jobs"})).unwrap();
        assert_eq!(
            owner,
            ProducerOwner::external(
                ProducerSource::plugin("jobs").unwrap(),
                ProducerName::default_name()
            )
        );
        assert!(
            serde_json::from_value::<ProducerOwner>(json!({"type": "plugin", "plugin": ""}))
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
            ProducerSource::runtime("local")
                .unwrap()
                .runtime_attachment(),
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
