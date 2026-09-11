//! Agent-to-agent messaging: tree-peer authorization and durable
//! producer-backed delivery for the `send_message` tool surface.
//!
//! Delivery always goes through the producer mailbox path: the message is
//! durably accepted as a `ProducerMessageAccepted` event on the recipient
//! session, and the existing producer machinery admits it into the recipient's
//! run at the next safe boundary (`steer`), claims it at the next run start
//! (`queue`), or wakes an idle/finished session through post-send reconcile.
//! `SessionCommand::Steer` is intentionally not used for agent mail; using
//! both paths would double-deliver the same message.

use cookie_agent_protocol::{
    ProducerDeliveryMode, ProducerId, ProducerIdempotencyKey, ProducerMessageId, ProducerOwner,
    RunId, SessionId, SessionOrigin, SessionProducersParams, SessionStatus, ToolCallId,
};

use super::producers::{ProducerAuthority, ProducerCommand, producer_description};
use super::{Engine, EngineError, SessionCommand};

/// Stable `send_message:<code>` error codes carried by
/// [`EngineError::Messaging`]. The tool layer surfaces these strings verbatim
/// so sending models can react predictably (retry, give up, or report).
pub const MESSAGE_DISABLED: &str = "send_message:disabled";
pub const MESSAGE_INVALID_BODY: &str = "send_message:invalid_body";
pub const MESSAGE_UNKNOWN_SESSION: &str = "send_message:unknown_session";
pub const MESSAGE_NOT_TREE_PEER: &str = "send_message:not_tree_peer";
pub const MESSAGE_SELF_SEND: &str = "send_message:self_send";
pub const MESSAGE_INBOX_FULL: &str = "send_message:inbox_full";
pub const MESSAGE_ENGINE_SHUTDOWN: &str = "send_message:engine_shutdown";

/// Immutable arguments for one `send_message` tool invocation. The sender
/// triple is engine-derived tool context, never model arguments.
#[derive(Clone, Debug)]
pub struct AgentMessageInvocation {
    pub sender_session_id: SessionId,
    pub sender_run_id: RunId,
    pub sender_tool_call_id: ToolCallId,
    pub recipient_session_id: SessionId,
    pub body: String,
    pub mode: ProducerDeliveryMode,
}

/// Recipient state observed at send time. Delivery mode is never coerced: the
/// requested mode stays effective in every state (safe-boundary admission
/// covers running recipients, claim-at-start covers queued ones, and the
/// post-send producer reconcile wakes idle or finished ones).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentRecipientState {
    Running,
    Queued,
    WakingFinished,
}

impl AgentRecipientState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Queued => "queued",
            Self::WakingFinished => "waking_finished",
        }
    }
}

/// The `send_message` result contract: success means the message is durably
/// accepted (`ProducerMessageAccepted` persisted in the recipient's event
/// log), not that the recipient has consumed it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentMessageHandle {
    pub message_id: ProducerMessageId,
    pub mode: ProducerDeliveryMode,
    pub recipient_state: AgentRecipientState,
}

fn messaging_error(code: &'static str) -> EngineError {
    EngineError::Messaging(code.to_owned())
}

/// Maps a shutdown race (`ActorStopped` from actor, registration, or send
/// requests) onto the stable tool-facing code; passes others through.
fn map_shutdown(error: EngineError) -> EngineError {
    match error {
        EngineError::ActorStopped => messaging_error(MESSAGE_ENGINE_SHUTDOWN),
        other => other,
    }
}

/// Maps a missing session onto the stable `unknown_session` code. Malformed
/// IDs cannot reach this layer; the typed `SessionId` argument rejects them at
/// the tool boundary.
fn map_unknown_session(error: EngineError) -> EngineError {
    match error {
        EngineError::Session(crate::session::SessionError::Missing(_)) => {
            messaging_error(MESSAGE_UNKNOWN_SESSION)
        }
        other => other,
    }
}

/// Renders the durable agent-mail envelope, materialized as a user turn at
/// the recipient's claim point. Field order is fixed so an idempotent retry
/// renders byte-identically for the stored-body comparison in dedup.
pub(super) fn render_agent_message_envelope(
    message_id: ProducerMessageId,
    sender_session_id: SessionId,
    sender_agent_type: &str,
    body: &str,
) -> String {
    format!(
        "<agent_message>\n{{\"message_id\":{},\"from\":{{\"session_id\":{},\"agent_type\":{}}},\"body\":{}}}\n</agent_message>",
        serde_json::to_string(message_id.to_string().as_str())
            .expect("message id string serializes"),
        serde_json::to_string(sender_session_id.to_string().as_str())
            .expect("session id string serializes"),
        serde_json::to_string(sender_agent_type).expect("agent type string serializes"),
        serde_json::to_string(body).expect("message body serializes"),
    )
}

/// Relationship label for one send: the recipient's relationship to the
/// sender, derived from stored `SessionOrigin` metadata only. This is purely
/// labeling for the ordinary permission pipeline; there is no
/// relationship-based authority check anywhere in the engine.
pub(crate) fn relationship_label(
    sender_origin: &SessionOrigin,
    recipient_origin: &SessionOrigin,
    sender: SessionId,
    recipient: SessionId,
) -> &'static str {
    let parent_of = |origin: &SessionOrigin| match origin {
        SessionOrigin::Delegated {
            parent_session_id, ..
        } => Some(*parent_session_id),
        SessionOrigin::Root => None,
    };
    let sender_parent = parent_of(sender_origin);
    let recipient_parent = parent_of(recipient_origin);
    if sender_parent == Some(recipient) {
        "parent"
    } else if recipient_parent == Some(sender) {
        "child"
    } else if sender_parent.is_some() && sender_parent == recipient_parent {
        "sibling"
    } else {
        "*"
    }
}

impl Engine {
    fn messaging_projection(
        &self,
        session: SessionId,
    ) -> Result<crate::session::SessionProjection, EngineError> {
        self.inner
            .store
            .get(session)
            .map_err(|error| map_unknown_session(error.into()))
    }

    /// The root session of the delegation tree a session belongs to.
    pub(crate) fn session_root(&self, session: SessionId) -> Result<SessionId, EngineError> {
        Ok(match self.messaging_projection(session)?.meta.origin {
            SessionOrigin::Root => session,
            SessionOrigin::Delegated {
                root_session_id, ..
            } => root_session_id,
        })
    }

    /// Relationship label used as the `message` permission resource for a
    /// sender→recipient pair: `parent`, `child`, `sibling`, or `*`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Messaging`] with `send_message:unknown_session`
    /// when either session does not exist.
    pub fn message_relationship(
        &self,
        sender: SessionId,
        recipient: SessionId,
    ) -> Result<&'static str, EngineError> {
        let sender_origin = self.messaging_projection(sender)?.meta.origin;
        let recipient_origin = self.messaging_projection(recipient)?.meta.origin;
        Ok(relationship_label(
            &sender_origin,
            &recipient_origin,
            sender,
            recipient,
        ))
    }

    /// Tree-peer authorization: two sessions may exchange mail iff they carry
    /// the same stored `root_session_id`. No live registry walk, ancestry, or
    /// taint validation (spec Resolved Decision 1; ghost sessions left by a
    /// reverted branch remain reachable by design).
    pub(crate) fn ensure_tree_peer(
        &self,
        sender: SessionId,
        recipient: SessionId,
    ) -> Result<(), EngineError> {
        if sender == recipient {
            return Err(messaging_error(MESSAGE_SELF_SEND));
        }
        if self.session_root(sender)? != self.session_root(recipient)? {
            return Err(messaging_error(MESSAGE_NOT_TREE_PEER));
        }
        Ok(())
    }

    pub(crate) fn agent_recipient_state(
        &self,
        recipient: SessionId,
    ) -> Result<AgentRecipientState, EngineError> {
        let projection = self.messaging_projection(recipient)?;
        let running = projection.status == SessionStatus::Running
            || self
                .inner
                .active
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .values()
                .any(|active| active.session == recipient);
        if running {
            return Ok(AgentRecipientState::Running);
        }
        if matches!(
            projection.status,
            SessionStatus::Completed
                | SessionStatus::Failed
                | SessionStatus::Interrupted
                | SessionStatus::Cancelled
        ) {
            return Ok(AgentRecipientState::WakingFinished);
        }
        // Idle and delegation-queued recipients share one state: the durable
        // acceptance lands now and is claimed when that recipient's next run
        // starts.
        Ok(AgentRecipientState::Queued)
    }

    /// Durably accepts one agent message into the recipient's producer inbox.
    ///
    /// The sender's `ProducerOwner::Agent` registration on the recipient is
    /// found-or-registered per send; registrations are runtime-only by
    /// contract, so re-registration after a process restart is expected and
    /// cheap. The idempotency key derives from `(sender session, run, tool
    /// call)`: a retried call returns the original `message_id` and delivers
    /// once. The inbox-cap check and the acceptance run atomically inside the
    /// recipient's actor (`ProducerCommand::SendAgentMessage`).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Messaging`] stable codes for disabled messaging,
    /// invalid bodies, unknown recipients, non-tree-peer recipients, self
    /// sends, full inboxes, and shutdown races; [`EngineError::Producer`] for
    /// idempotency payload conflicts.
    pub async fn send_agent_message(
        &self,
        invocation: AgentMessageInvocation,
    ) -> Result<AgentMessageHandle, EngineError> {
        let messaging = &self.inner.config.runtime.messaging;
        if !messaging.enabled {
            return Err(messaging_error(MESSAGE_DISABLED));
        }
        if invocation.body.trim().is_empty() || invocation.body.len() > messaging.max_body_bytes {
            return Err(messaging_error(MESSAGE_INVALID_BODY));
        }
        self.ensure_tree_peer(
            invocation.sender_session_id,
            invocation.recipient_session_id,
        )?;
        let recipient_state = self.agent_recipient_state(invocation.recipient_session_id)?;
        let sender = self.messaging_projection(invocation.sender_session_id)?;
        let sender_agent_type = sender.creation_agent.agent.to_string();
        let authority = ProducerAuthority {
            owner: ProducerOwner::Agent {
                session_id: invocation.sender_session_id,
            },
            connection_epoch: None,
        };
        let producer_id = self
            .find_or_register_agent_producer(invocation.recipient_session_id, &authority)
            .await
            .map_err(map_shutdown)?;
        let key = ProducerIdempotencyKey::new(format!(
            "agent-message:{}:{}:{}",
            invocation.sender_session_id, invocation.sender_run_id, invocation.sender_tool_call_id
        ))
        .expect("agent message idempotency key fits the producer key bound");
        let description = producer_description("Agent message from ", &sender_agent_type);
        let message_id = self
            .request(invocation.recipient_session_id, |reply| {
                SessionCommand::Producer(ProducerCommand::SendAgentMessage {
                    authority,
                    producer_id,
                    mode: invocation.mode,
                    key,
                    description,
                    sender: invocation.sender_session_id,
                    sender_agent_type,
                    body: invocation.body,
                    reply,
                })
            })
            .await
            .map_err(map_shutdown)?;
        Ok(AgentMessageHandle {
            message_id,
            mode: invocation.mode,
            recipient_state,
        })
    }

    async fn find_or_register_agent_producer(
        &self,
        recipient: SessionId,
        authority: &ProducerAuthority,
    ) -> Result<ProducerId, EngineError> {
        let producers = self
            .session_producers(SessionProducersParams {
                session_id: recipient,
            })
            .await?;
        if let Some(registration) = producers
            .producers
            .into_iter()
            .find(|registration| registration.producer_owner == authority.owner)
        {
            return Ok(registration.producer_id);
        }
        self.register_producer(recipient, authority.clone()).await
    }
}
