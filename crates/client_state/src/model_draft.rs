//! A session's model-selection draft, shared with the daemon's other
//! clients: a pick is published as the session's selection once it settles,
//! [`DRAFT_PUBLISH_DELAY`] after the last change, so cycling through agents
//! or variants publishes only where it stops. A pick another client makes
//! meanwhile wins: the unsent pick yields, and the draft follows the
//! session's latest selection.
//!
//! The client owns the clock: it waits the delay itself and then asks
//! whether its publish is still the latest.

use std::time::Duration;

use cookie_agent_protocol::{
    ClientDelivery, EventOrigin, EventPayload, EventSubscriptionMessage, SessionId,
};

/// How long a draft must stay unchanged before it is published.
pub const DRAFT_PUBLISH_DELAY: Duration = Duration::from_millis(300);

/// Which pick of the draft may still be published.
#[derive(Clone, Copy, Debug, Default)]
pub struct DraftPublish {
    generation: u64,
}

impl DraftPublish {
    /// The draft changed: returns the generation its publish, due after
    /// [`DRAFT_PUBLISH_DELAY`], carries. An earlier pending publish no
    /// longer goes out.
    pub fn schedule(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Whether the publish of `generation` is still the latest pick.
    pub fn is_current(&self, generation: u64) -> bool {
        generation == self.generation
    }

    /// Another client picked: the pending publish, if any, yields.
    pub fn yield_to_foreign_pick(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

/// Whether `delivery` is a live model pick in the `viewed` session by a
/// client other than the one whose origin is `own_origin`.
pub fn is_foreign_model_pick(
    delivery: &ClientDelivery,
    viewed: Option<SessionId>,
    own_origin: Option<&EventOrigin>,
) -> bool {
    let ClientDelivery::Live { message, .. } = delivery else {
        return false;
    };
    let EventSubscriptionMessage::Event { event } = message.as_ref() else {
        return false;
    };
    matches!(&event.payload, EventPayload::SessionModelSelected { .. })
        && Some(event.session_id) == viewed
        && event.origin.as_ref() != own_origin
}

#[cfg(test)]
mod tests {
    use super::DraftPublish;

    #[test]
    fn only_the_last_pick_publishes_and_a_foreign_pick_cancels_it() {
        let mut publish = DraftPublish::default();
        let first = publish.schedule();
        let second = publish.schedule();
        assert!(!publish.is_current(first));
        assert!(publish.is_current(second));
        publish.yield_to_foreign_pick();
        assert!(!publish.is_current(second));
        let third = publish.schedule();
        assert!(publish.is_current(third));
    }
}
