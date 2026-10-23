//! One stream of MCP server changes and failures across runtimes.
//!
//! The global runtime hosts the top-level `[mcp]` servers in-process; a
//! coding runtime hosts its template's servers on its own machine and
//! reports them over the runtime protocol (`mcp/changed`). Clients see one
//! `mcp.changed` stream either way, so every change bumps [`McpHub`] and
//! every failure is broadcast with the runtime that hosts the server.

use std::sync::Arc;

use cookie_agent_mcp::McpObserver;
use cookie_agent_runtime_api::GLOBAL_RUNTIME_ID;

/// An MCP server whose tools are unavailable: it failed to connect, or it
/// dropped and did not reconnect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerFailure {
    /// The runtime hosting the server; `global` for top-level `[mcp]`.
    pub runtime_id: String,
    /// The hosting instance's workspace, when its runtime has one.
    pub workspace: Option<String>,
    pub server: String,
    pub reason: String,
}

/// Where MCP changes and failures of every runtime meet. It also carries
/// the runtimes' ACP relay ([`super::AcpHub`]), which reaches every
/// runtime connection the same way.
pub(crate) struct McpHub {
    changes: tokio::sync::watch::Sender<u64>,
    failures: tokio::sync::broadcast::Sender<McpServerFailure>,
    acp: Arc<super::AcpHub>,
}

impl McpHub {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            changes: tokio::sync::watch::channel(0).0,
            failures: tokio::sync::broadcast::channel(64).0,
            acp: Arc::default(),
        })
    }

    /// The runtimes' ACP relay.
    pub(crate) fn acp(&self) -> &Arc<super::AcpHub> {
        &self.acp
    }

    pub(crate) fn changed(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub(crate) fn failed(&self, failure: McpServerFailure) {
        let _ = self.failures.send(failure);
    }

    pub(crate) fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub(crate) fn subscribe_failures(&self) -> tokio::sync::broadcast::Receiver<McpServerFailure> {
        self.failures.subscribe()
    }

    /// The observer of the global runtime's registry.
    pub(crate) fn global_observer(self: &Arc<Self>) -> Arc<dyn McpObserver> {
        Arc::new(GlobalObserver(Arc::clone(self)))
    }
}

struct GlobalObserver(Arc<McpHub>);

impl McpObserver for GlobalObserver {
    fn changed(&self) {
        self.0.changed();
    }

    fn failed(&self, failure: &cookie_agent_mcp::McpServerFailure) {
        self.0.failed(McpServerFailure {
            runtime_id: GLOBAL_RUNTIME_ID.to_owned(),
            workspace: None,
            server: failure.server.clone(),
            reason: failure.reason.clone(),
        });
    }
}
