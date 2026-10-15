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

pub mod authoring;
mod error;
mod execution;
pub mod media;
mod prepared;
mod provider;
mod runtime;

pub use error::ToolError;
pub use execution::{
    AdapterFamily, ArtifactReadPage, BackgroundJob, BackgroundJobs, BackgroundLauncher,
    BackgroundReport, BackgroundTask, ProgressCapture, ProgressSink, StdinWrite, ToolArtifacts,
    ToolExecutionContext, ToolPreparationContext, ToolProgress, ToolStdin, TurnAgentContext,
};
pub use prepared::{
    PreparedExecutor, PreparedExecutorCell, PreparedSerializationKey, PreparedTool, ToolCompletion,
    bounded_tool_display, sanitize_tool_display,
};
pub use provider::{
    PromptSection, SessionToolContext, ToolCall, ToolCollision, ToolConcurrency, ToolProvider,
    ToolResultTruncationPolicy, ToolSpec, tool_presentation, tool_title_only,
};
pub use runtime::{
    CodingToolsFactory, GLOBAL_RUNTIME_ID, LOCAL_RUNTIME_ID, MAX_ATTACHMENT_NAME_BYTES,
    RuntimeInfo, RuntimeKind, RuntimeOptions, ToolOrigin, WorkspaceContext, coding_attachment_name,
};
