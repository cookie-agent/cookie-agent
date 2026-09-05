//! The transport-free single-conversation cookie agent runtime.

pub mod actor;
mod config_store;
mod delegation_api;
pub mod delegation_events;
pub mod events;
mod goal_projection;
pub mod grant_journal;
mod mcp;
mod media;
mod model_bridge;
mod model_history;
mod model_policy;
mod model_snapshots;
mod ownership;
pub mod permissions;
mod plugin;
mod policy;
mod runtime;
mod runtime_snapshot;
#[cfg(test)]
mod runtime_tests;
pub mod session;
#[cfg(test)]
mod test_support;
mod tool_api;
mod usage;

pub use cookie_agent_protocol::PersistedToolResult;
pub use delegation_api::{DelegateAwait, DelegateHandle, DelegateInvocation};
pub use mcp::{McpRegistry, McpServerState, McpServerStatus};
pub use media::{AttachmentGate, approved_media_type, attachment_gate_error, gate_attachment};
pub use plugin::{EngineEvent, PluginRegistry, PluginState, PluginStatus};
pub use runtime::{
    ApprovalRespondFailure, Engine, EngineError, EngineHistoryView, EngineOptions, SkillInvocation,
    ToolResultReadPage,
};
pub use runtime_snapshot::PublishedRuntime;
pub use tool_api::{
    PreparedExecutor, PreparedSerializationKey, PreparedTool, ProgressSink, PromptSection,
    SessionToolContext, StdinWrite, ToolCall, ToolConcurrency, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProgress, ToolProvider, ToolResultTruncationPolicy, ToolSpec,
    ToolStdin, TurnAgentContext,
};

pub(crate) use runtime::ArtifactStore;
