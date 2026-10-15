//! The contract between cookie agent's engine and the tools it runs.
//!
//! A tool provider implements [`ToolProvider`]: it declares [`ToolSpec`]s,
//! prepares calls into [`PreparedTool`]s, and executes them with a
//! [`ToolExecutionContext`]. Everything a provider needs lives here, so tool
//! crates depend on this crate and the protocol types, never on the engine.
//!
//! The engine provides the host services a call uses through traits defined
//! here: [`ProgressCapture`] (output capture), [`ToolArtifacts`] (artifact
//! retention and reads), and [`BackgroundJobs`] with [`BackgroundLauncher`]
//! (background jobs). The runtime identity types ([`ToolOrigin`],
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
pub mod protocol;
mod provider;
pub mod rpc;
mod runtime;

pub use error::ToolError;
pub use execution::{
    AdapterFamily, ArtifactReadPage, BackgroundJob, BackgroundJobs, BackgroundLauncher,
    BackgroundReport, BackgroundTask, ProgressCapture, ProgressSink, StdinWrite, ToolArtifacts,
    ToolExecutionContext, ToolPreparationContext, ToolProgress, ToolStdin, TurnAgentContext,
};
pub use prepared::{
    PreparedDeclaration, PreparedExecutor, PreparedExecutorCell, PreparedSerializationKey,
    PreparedTool, ToolCompletion, bounded_tool_display, sanitize_tool_display,
};
pub use provider::{
    PromptSection, SessionToolContext, ToolCall, ToolCollision, ToolConcurrency, ToolProvider,
    ToolResultTruncationPolicy, ToolSpec, tool_presentation, tool_title_only,
};
pub use runtime::{
    CodingToolsFactory, DiagnosticTail, GLOBAL_RUNTIME_ID, LOCAL_RUNTIME_ID, LocalRuntime,
    MAX_ATTACHMENT_NAME_BYTES, RuntimeConnection, RuntimeConnector, RuntimeInfo, RuntimeKind,
    RuntimeOptions, ToolOrigin, WorkspaceContext, coding_attachment_name,
};
