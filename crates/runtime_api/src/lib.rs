//! The contract between cookie agent's engine and the tools it runs.
//!
//! A tool provider implements [`ToolProvider`]: it declares [`ToolSpec`]s,
//! prepares calls into [`PreparedTool`]s, and executes them with a
//! [`ToolExecutionContext`]. Everything a provider needs lives here, so tool
//! crates depend on this crate and the protocol types, never on the engine.
//!
//! The engine provides the host services a call uses through traits defined
//! here: [`ProgressCapture`] (output capture), [`ToolArtifacts`] (artifact
//! retention and reads), and a [`DetachPoint`] through which a
//! background-capable tool says when the host may detach it. The runtime
//! identity types ([`ToolOrigin`],
//! [`RuntimeInfo`], [`WorkspaceContext`]) say which runtime a tool belongs to,
//! as specified in `docs/site/specs/runtimes.md`.
//!
//! [`protocol`] holds the wire types of the runtime protocol that connects
//! the daemon to a runtime in another task or process, and [`rpc`] the
//! JSON-RPC peer both ends run.

pub mod authoring;
pub mod context;
mod error;
mod execution;
pub mod media;
mod prepared;
mod producers;
pub mod protocol;
mod provider;
pub mod reconnect;
pub mod rpc;
mod runtime;

pub use error::ToolError;
pub use execution::{
    ArtifactReadPage, DetachPoint, DetachWatch, Detachable, ProgressCapture, ProgressSink,
    ToolArtifacts, ToolExecutionContext, ToolPreparationContext, ToolProgress, TurnAgentContext,
    UnknownBlockReporter,
};
pub use media::AdapterFamily;
pub use prepared::{
    PreparedDeclaration, PreparedExecutor, PreparedExecutorCell, PreparedSerializationKey,
    PreparedTool, ToolCompletion, bounded_tool_display, sanitize_tool_display,
};
pub use producers::{ProducerChannel, ProducerMessage};
pub use provider::{
    PromptSection, ProvidedToolSource, SessionToolContext, ToolCall, ToolCollision,
    ToolConcurrency, ToolProvider, ToolResultTruncationPolicy, ToolSpec, tool_presentation,
    tool_title_only,
};
pub use runtime::{
    CODING_RUNTIME_KIND, CodingToolsFactory, DiagnosticTail, GLOBAL_RUNTIME_ID, LOCAL_RUNTIME_ID,
    LocalRuntime, MAX_ATTACHMENT_NAME_BYTES, RuntimeConnection, RuntimeConnector, RuntimeInfo,
    RuntimeKind, RuntimeOptions, ToolOrigin, WorkspaceContext, coding_attachment_name,
};
