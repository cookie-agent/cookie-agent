//! The transport-free single-conversation Cookie Agent runtime.

pub mod actor;
mod base_tools;
mod blocking;
mod config_store;
mod delegation_api;
pub mod delegation_events;
pub mod events;
mod goal_projection;
pub mod grant_journal;
mod machines;
mod media;
mod memory;
mod memory_tools;
mod model_bridge;
mod model_history;
mod model_policy;
mod model_snapshots;
mod ownership;
pub mod permissions;
mod plugin;
mod policy;
mod preview;
mod projects;
pub(crate) mod runtime;
mod runtime_snapshot;
mod runtimes;
pub mod session;
#[cfg(test)]
mod test_support;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
mod usage;

pub use base_tools::base_layer_tool_specs;
pub use cookie_agent_mcp::{McpRegistry, McpServerState, McpServerStatus};
pub use cookie_agent_protocol::PersistedToolResult;
pub use delegation_api::{DelegateAwait, DelegateHandle, DelegateInvocation};
pub use machines::MachineError;
pub use memory_tools::memory_tool_specs;
pub use plugin::{EngineEvent, PluginHookFailure, PluginRegistry};
pub use projects::ProjectError;
pub use runtime::attachments::{DialInClaim, SessionRuntimeRequest, SessionRuntimes};
pub use runtime::configuration::{
    ConfigChange, ConfigDocumentText, ConfigDocuments, ConfigurationStatus, RestartRequiredPart,
};
/// Stable `send_message:<code>` error strings carried by
/// [`EngineError::Messaging`]. The tool layer surfaces them verbatim so sending
/// models can react predictably (retry, back off, or give up).
pub use runtime::handles::SubagentScope;
pub use runtime::memory_api::MemoryRequestError;
pub use runtime::messaging_api::{
    MESSAGE_DISABLED, MESSAGE_ENGINE_SHUTDOWN, MESSAGE_INBOX_FULL, MESSAGE_INFLIGHT_FULL,
    MESSAGE_INVALID_ARGUMENTS, MESSAGE_INVALID_BODY, MESSAGE_MAX_HOPS_EXCEEDED,
    MESSAGE_NOT_TREE_PEER, MESSAGE_SELF_SEND, MESSAGE_UNKNOWN_SESSION,
};
pub use runtime::uploads::{MAX_UPLOADS_PER_CONNECTION, MediaError};
pub use runtime::{
    AgentMessageHandle, AgentMessageInvocation, AgentRecipientState, ApprovalRespondFailure,
    Engine, EngineError, EngineHistoryView, EngineOptions, SkillInvocation,
};
pub use runtime_snapshot::PublishedRuntime;
pub use runtimes::{
    DialInRefusal, McpServerFailure, RuntimeFailure, RuntimeInstanceStatus, RuntimeInstanceSummary,
    RuntimeListingState, RuntimeTemplate, RuntimeTemplateSummary, StartError, StartedRuntime,
    ToolLayer,
};
pub use session::TailOwner;

// The tool API, imported once for the engine's own modules.
pub(crate) use cookie_agent_runtime_api::{
    PreparedSerializationKey, PreparedTool, ProgressSink, PromptSection, SessionToolContext,
    ToolCall, ToolCompletion, ToolConcurrency, ToolError, ToolExecutionContext, ToolOrigin,
    ToolPreparationContext, ToolProgress, ToolProvider, ToolResultTruncationPolicy, ToolSpec,
    TurnAgentContext, WorkspaceContext,
};

#[cfg(test)]
pub(crate) use cookie_agent_runtime_api::PreparedExecutor;
pub(crate) use runtime::{ArtifactRouter, SessionArtifacts};
#[cfg(test)]
pub(crate) use runtime::{ArtifactStore, test_session_id};
