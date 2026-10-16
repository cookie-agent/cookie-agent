//! Runtimes as producer sources (`docs/site/specs/external-producers.md`
//! section 5): a runtime produces as `runtime:<attachment name>`, only for
//! the sessions attached to it, over its live connection.

use cookie_agent_protocol::{ProducerSource, ProducerSourceState, SessionId};

use super::Engine;

impl Engine {
    /// Whether the runtime of `source` is attached to `session`.
    pub(super) fn runtime_source_attached(
        &self,
        _source: &ProducerSource,
        _session: SessionId,
    ) -> bool {
        false
    }

    /// Whether `epoch` is the live connection of the runtime of `source`.
    pub(super) fn runtime_source_is_current(&self, _source: &ProducerSource, _epoch: u64) -> bool {
        false
    }

    /// The recovery state of every producer-capable runtime attached to
    /// `session`.
    pub(super) fn runtime_producer_source_states(
        &self,
        _session: SessionId,
    ) -> Vec<ProducerSourceState> {
        Vec::new()
    }
}
