//! The transport-free single-conversation cookie agent runtime.

pub mod actor;
mod delegation_api;
pub mod events;
pub mod grant_journal;
pub mod journal;
mod media;
mod model_bridge;
mod model_history;
mod model_policy;
mod model_snapshots;
pub mod permissions;
mod policy;
pub mod run;
mod runtime;
mod runtime_snapshot;
#[cfg(test)]
mod runtime_tests;
pub mod session;
#[cfg(test)]
mod test_support;
mod tool_api;

pub use cookie_agent_protocol::PersistedToolResult;
pub use delegation_api::{DelegateAwait, DelegateHandle, DelegateInvocation};
pub use media::approved_media_type;
pub use runtime::{ApprovalRespondFailure, Engine, EngineClient, EngineError, EngineOptions};
pub use runtime_snapshot::PublishedRuntime;
pub use tool_api::{
    PreparedExecutor, PreparedSerializationKey, PreparedTool, ProgressSink, SessionToolContext,
    StdinWrite, ToolCall, ToolError, ToolExecutionContext, ToolPreparationContext, ToolProgress,
    ToolProvider, ToolSpec, ToolStdin,
};

pub(crate) use runtime::ArtifactStore;
