//! Where the ACP agent relay of every runtime connection meets the engine
//! (`docs/site/specs/acp-agents.md` section 7).
//!
//! A runtime reports the ACP agents it hosts in its initialize reply and in
//! `runtime/agents_changed`; its connection keeps them, and each report, as
//! each connection's start and end, bumps [`AcpHub::changed`], so the
//! engine recomputes its agent registry. The `agent/*` notifications a
//! runtime sends reach the [`AcpRelayHandler`] the engine installs.

use std::sync::{Arc, OnceLock};

use cookie_agent_runtime_api::protocol::{
    AgentExitedParams, AgentMcpClosedParams, AgentMcpParams, AgentMessageParams,
};

use super::RuntimeClient;

/// What the engine does with the `agent/*` notifications of a runtime
/// connection.
pub(crate) trait AcpRelayHandler: Send + Sync {
    /// One ACP message from an agent's process.
    fn message(&self, client: &RuntimeClient, params: AgentMessageParams);
    /// An agent's process ended.
    fn exited(&self, client: &RuntimeClient, params: AgentExitedParams);
    /// One MCP message of a bridge connection.
    fn mcp(&self, client: &RuntimeClient, params: AgentMcpParams);
    /// A bridge connection ended.
    fn mcp_closed(&self, client: &RuntimeClient, params: AgentMcpClosedParams);
    /// The connection ended: every agent process it relayed is gone.
    fn connection_lost(&self, client: &RuntimeClient);
}

/// The ACP relay of every runtime connection.
#[derive(Default)]
pub(crate) struct AcpHub {
    changes: tokio::sync::watch::Sender<u64>,
    handler: OnceLock<Arc<dyn AcpRelayHandler>>,
}

impl AcpHub {
    /// The runtimes' ACP agents, or whether one is connected, changed.
    pub(crate) fn changed(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Installs the engine's handler, once.
    pub(crate) fn set_handler(&self, handler: Arc<dyn AcpRelayHandler>) {
        let _ = self.handler.set(handler);
    }

    pub(crate) fn handler(&self) -> Option<Arc<dyn AcpRelayHandler>> {
        self.handler.get().cloned()
    }
}
