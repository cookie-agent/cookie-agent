//! Tool layers and runtimes: where a tool comes from and which workspace it
//! sees.
//!
//! Not to be confused with `crate::runtime` (the engine internals: sessions,
//! runs, the model loop) or [`crate::PublishedRuntime`] (the compiled model
//! runtime published to clients). Here a *runtime* is a source of tools and
//! workspace context, as specified in `docs/site/specs/runtimes.md`.
//!
//! The engine composes a session's tool list from:
//!
//! 1. The **base layer** ([`ToolLayer::Base`]): tools the daemon implements
//!    for its own orchestration (delegation, messaging, skills, goals,
//!    `read_artifact`). It is not a runtime and has no workspace.
//! 2. The session's **attachments**, in attach order: capability instances
//!    that the [`RuntimeRegistry`] opens from [`RuntimeTemplate`]s, one per
//!    (runtime ID, capability, canonical workspace), shared by every session
//!    attached to them. The built-in templates are the **global runtime** (ID `global`):
//!    top-level MCP servers, plugins, and a `webfetch` declared
//!    `ToolCollision::Fallback`, no workspace, called in-process; and the
//!    **local coding runtime** (ID `local`): `read`, `write`, `edit`, `bash`,
//!    and `webfetch` in the daemon's working directory, reached over the
//!    runtime protocol (`remote`) in production.
//!
//! [`compose`] turns the sources' tools into published names, prefixing
//! collisions with the attachment name. Every tool is traced to its
//! [`ToolOrigin`]. Preparation, permission decisions, and visibility read the
//! workspace of that origin, never the daemon's working directory, which only
//! locates the session store.
//!
//! The runtime identity types ([`ToolOrigin`], `RuntimeInfo`,
//! `WorkspaceContext`) live in `cookie_agent_runtime_api`, which tool
//! providers depend on; this module composes them.

use std::sync::Arc;

use cookie_agent_runtime_api::{ToolOrigin, ToolProvider};

mod builtin;
mod composition;
mod dial_in;
mod liveness;
mod mcp_hub;
mod process;
mod registry;
mod remote;

#[cfg(test)]
pub(crate) use builtin::coding_capability;
pub(crate) use builtin::daemon_home;
pub(crate) use builtin::{GlobalTemplate, LocalTemplate};
pub(crate) use composition::{ComposedTool, Composition, compose};
pub(crate) use cookie_agent_runtime_api::reconnect::{ReconnectOutcome, ReconnectPolicy};
pub use dial_in::DialInRefusal;
pub(crate) use dial_in::{DialInTemplate, not_connected};
pub(crate) use liveness::Liveness;
pub(crate) use mcp_hub::McpHub;
pub use mcp_hub::McpServerFailure;
pub(crate) use process::ProcessConnector;
pub use registry::{
    RuntimeFailure, RuntimeInstanceStatus, RuntimeInstanceSummary, RuntimeListingState,
    RuntimeTemplate, RuntimeTemplateSummary, StartError, StartedRuntime,
};
pub(crate) use registry::{RuntimeInstance, RuntimeRegistry};
pub(crate) use remote::{
    ClientExtension, HostedMcpServer, InstanceConnection, ProtocolTemplate, RuntimeClient,
    RuntimeProducerCall, RuntimeProducerHost, register_configured_templates, runtime_settings,
};

/// One-line description of where a tool runs, for the approval reviewer.
pub(crate) fn describe_origin(origin: &ToolOrigin) -> String {
    match origin {
        ToolOrigin::Base => "the daemon's base layer (no workspace)".to_owned(),
        ToolOrigin::Runtime(runtime) => match runtime.workspace() {
            Some(workspace) => format!(
                "the `{}` {} runtime with workspace {}",
                runtime.id(),
                runtime.kind().as_str(),
                workspace.root().display()
            ),
            None => format!(
                "the `{}` {} runtime (no workspace)",
                runtime.id(),
                runtime.kind().as_str()
            ),
        },
    }
}

/// Where [`crate::Engine::try_register_tool_provider`] adds a provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolLayer {
    /// The daemon's base layer.
    Base,
    /// The global runtime instance.
    Global,
    /// The local coding runtime instance.
    Local,
}

/// A provider together with the layer or runtime attachment that owns it.
#[derive(Clone)]
pub(crate) struct SourcedProvider {
    /// The session's attachment name; `None` for the base layer.
    pub(crate) attachment: Option<cookie_agent_protocol::AttachmentName>,
    /// The instance generation; `0` for the base layer.
    pub(crate) generation: u64,
    pub(crate) origin: ToolOrigin,
    pub(crate) provider: Arc<dyn ToolProvider>,
}
