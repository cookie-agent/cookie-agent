//! How a runtime sends producer messages to the sessions attached to it
//! (`docs/site/specs/external-producers.md` section 5). The runtime's tools
//! reach the daemon through [`ProducerChannel`]; the daemon derives the
//! producer's source from the runtime's connection.

use async_trait::async_trait;
use cookie_agent_protocol::{
    ProducerDeliveryMode, ProducerId, ProducerIdempotencyKey, ProducerMessageId, ProducerName,
    SafeDisplayText, SessionId,
};

use crate::ToolError;

/// One message a registered producer sends.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProducerMessage {
    pub mode: ProducerDeliveryMode,
    /// Identical retries return the first message's receipt.
    pub idempotency_key: ProducerIdempotencyKey,
    /// What a client shows for the message; not sent to the model.
    pub description: SafeDisplayText,
    /// What the model reads, as sent.
    pub body: String,
}

/// The daemon's producer operations, as a runtime reaches them. Every call
/// may only address a session attached to the runtime.
#[async_trait]
pub trait ProducerChannel: Send + Sync + std::fmt::Debug {
    /// Registers a producer named `name` for `session`.
    async fn register(
        &self,
        session: SessionId,
        name: ProducerName,
    ) -> Result<ProducerId, ToolError>;

    /// Sends `message` through `producer`; returns once the daemon has
    /// durably accepted it.
    async fn send(
        &self,
        session: SessionId,
        producer: ProducerId,
        message: ProducerMessage,
    ) -> Result<ProducerMessageId, ToolError>;

    /// Closes `producer`; its accepted messages stay.
    async fn unregister(&self, session: SessionId, producer: ProducerId) -> Result<(), ToolError>;

    /// Discards one of the runtime's messages while it still waits.
    async fn discard(
        &self,
        session: SessionId,
        message: ProducerMessageId,
    ) -> Result<(), ToolError>;
}
