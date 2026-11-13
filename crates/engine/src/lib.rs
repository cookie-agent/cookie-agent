//! The transport-free single-conversation Cookie Agent runtime.

pub mod actor;
mod agents_tool;
mod asset_tools;
mod assets;
mod base_tools;
mod config_store;
mod cron_tools;
mod crons;
mod deferred_tools;
mod delegation_api;
#[cfg(test)]
#[path = "../../../test-support/eventually.rs"]
mod eventually;
mod exposure;
pub mod grant_journal;
mod heap;
mod machines;
mod machines_tool;
mod media;
mod memory_tools;
mod model_bridge;
mod model_history;
mod model_policy;
mod model_snapshots;
mod native_compaction;
mod plugin;
mod policy;
mod prefix_fingerprint;
mod preview;
mod projects;
pub mod release;
pub(crate) mod runtime;
mod runtime_groups;
mod runtime_snapshot;
mod runtimes;
mod runtimes_tool;
mod shares;
#[cfg(test)]
#[path = "../../../test-support/model_fixtures.rs"]
mod test_support;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
mod threads;

// The storage layer, at the paths the engine's modules have always used.
pub use cookie_agent_engine_store::{TailOwner, delegation_events, events, session, status};
// The permission rules, likewise.
pub use cookie_agent_engine_permissions as permissions;
// The memory folder and its store.
use cookie_agent_engine_memory as memory;
#[cfg(test)]
pub(crate) use cookie_agent_engine_store::{bindings, ownership};
pub(crate) use cookie_agent_engine_store::{blocking, goal_projection, timing, usage};

pub use agents_tool::agents_tool_spec;
pub use asset_tools::asset_tool_specs;
pub use assets::AssetError;
pub use base_tools::{base_layer_tool_specs, builtin_tool_groups};
pub use cookie_agent_mcp::{McpRegistry, McpServerState, McpServerStatus};
pub use cookie_agent_protocol::PersistedToolResult;
pub use cron_tools::cron_tool_specs;
pub use crons::CronError;
pub use delegation_api::{DelegateAwait, DelegateHandle, DelegateInvocation, DelegatedCapability};
pub use machines::MachineError;
pub use machines_tool::machines_tool_specs;
pub use memory_tools::memory_tool_specs;
pub use plugin::{
    EngineEvent, PluginHookFailure, PluginRegistry,
    pages::{PluginPage, PluginPageTarget, percent_decode},
};
pub use projects::ProjectError;
pub use runtime::agent_documents::{AgentDocumentEntry, AgentDocumentKind, AgentDocumentOrigin};
pub use runtime::artifacts::uploads::{MAX_UPLOADS_PER_CONNECTION, MediaError};
pub use runtime::attachments::{DialInClaim, SessionRuntimeRequest, SessionRuntimes};
pub use runtime::chatgpt::CHATGPT_MAINTENANCE_INTERVAL;
pub use runtime::configuration::{
    ConfigChange, ConfigDocumentText, ConfigDocuments, ConfigurationStatus, RestartRequiredPart,
};
/// Stable `send_message:<code>` error strings carried by
/// [`EngineError::Messaging`]. The tool layer surfaces them verbatim so sending
/// models can react predictably (retry, back off, or give up).
pub use runtime::delegation::handles::SubagentScope;
pub use runtime::machines::api::RememberedRuntimeFacts;
pub use runtime::memory::api::MemoryRequestError;
pub use runtime::messaging::api::{
    MESSAGE_DISABLED, MESSAGE_ENGINE_SHUTDOWN, MESSAGE_INBOX_FULL, MESSAGE_INFLIGHT_FULL,
    MESSAGE_INVALID_ARGUMENTS, MESSAGE_INVALID_BODY, MESSAGE_MAX_HOPS_EXCEEDED,
    MESSAGE_NOT_TREE_PEER, MESSAGE_SELF_SEND, MESSAGE_UNKNOWN_SESSION,
};
pub use runtime::official::agents::{
    OFFICIAL_AGENTS_REFRESH, OFFICIAL_AGENTS_URL, OfficialAgentsReport, OfficialPresets,
    usable_presets,
};
pub use runtime::official::onboarding::{
    OnboardingAcpAgent, OnboardingAcpRequest, OnboardingField, OnboardingMcpRequest,
    OnboardingOutcome, OnboardingRequest, OnboardingRequirement, OnboardingState,
};
pub use runtime::official::skills::{
    OFFICIAL_SKILLS_REFRESH, OFFICIAL_SKILLS_URL, OfficialSkillsReport, UserSkillRoots,
    version_satisfies,
};
pub use runtime::push_signals::{
    NeedsYouItem, NeedsYouSignal, PushSignal, SettledOutcome, SettledSignal,
};
pub use runtime::restart::{RestartHook, RestartScheduled, RestartStatus};
pub use runtime::shares::{GuestLink, GuestPresence, OpenShare, OpenedLink};
pub use runtime::{
    AgentMessageHandle, AgentMessageInvocation, AgentRecipientState, ApprovalRespondFailure,
    Engine, EngineError, EngineHistoryView, EngineOptions, SkillInvocation,
};
pub use runtime_snapshot::PublishedRuntime;
pub use runtimes::{
    DialInRefusal, McpServerFailure, RuntimeActivity, RuntimeFailure, RuntimeInstanceStatus,
    RuntimeInstanceSummary, RuntimeListingState, RuntimeShellEnvironment, RuntimeTemplate,
    RuntimeTemplateSummary, StartError, StartedRuntime, ToolLayer,
};
pub use runtimes_tool::runtimes_tool_spec;
pub use shares::ShareError;

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
