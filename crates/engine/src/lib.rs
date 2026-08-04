//! The transport-free single-conversation cookie agent runtime.

pub mod actor;
#[cfg(test)]
mod builtin_revision_tests;
mod delegation_api;
#[cfg(test)]
mod delegation_tests;
pub mod events;
pub mod grant_journal;
pub mod journal;
mod media;
mod model_bridge;
mod model_history;
mod model_policy;
pub mod permissions;
mod policy;
#[cfg(test)]
mod prepared_tests;
#[cfg(test)]
mod responses_fixture_tests;
pub mod run;
#[cfg(test)]
mod run_selection_tests;
mod runtime;
#[cfg(test)]
mod security_tests;
pub mod session;
#[cfg(test)]
mod test_support;
mod tool_api;

pub use cookie_agent_protocol::PersistedToolResult;
pub use delegation_api::{DelegateAwait, DelegateHandle, DelegateInvocation};
pub use media::approved_media_type;
pub use runtime::{ApprovalRespondFailure, Engine, EngineClient, EngineError, EngineOptions};
pub use tool_api::{
    PreparedExecutor, PreparedSerializationKey, PreparedTool, ProgressSink, SessionToolContext,
    StdinWrite, ToolCall, ToolError, ToolExecutionContext, ToolPreparationContext, ToolProgress,
    ToolProvider, ToolSpec, ToolStdin,
};

pub(crate) use runtime::ArtifactStore;

#[cfg(test)]
pub(crate) use cookie_agent_protocol::InternalAgentKind;
#[cfg(test)]
pub(crate) use cookie_agent_protocol::PersistedToolResult as ToolResult;
#[cfg(test)]
pub(crate) use policy::FrozenRunPolicy;
#[cfg(test)]
pub(crate) use runtime::{
    ApprovalOutcome, BOUNDED_SUMMARY_BUILTIN_REVISION, InternalAgentRuntime, PendingApproval,
    ToolCallFailureCode, UNAVAILABLE_BUILTIN_REVISION, active_fallback_index, approval_records,
    completed_delegate_result, cwd_identity, doom_loop_repetitions, freeze_delegated_child_policy,
    invocation_id, protocol_digest, restart_approval_decision, restart_tool_failure,
    safe_tool_presentation, session_meta, title_regeneration_target, validate_attachment,
};
