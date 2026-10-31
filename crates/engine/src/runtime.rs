use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};

use arc_swap::ArcSwap;
use cookie_agent_config::LoadedConfiguration;
use cookie_agent_identity::ModelKey;
use cookie_agent_models::{ModelManager, manifests::ManifestError};
use cookie_agent_protocol::{
    AgentId, ApprovalId, ApprovalRespondErrorCode, ApprovalStatus, EventPayload as Event,
    InvocationId, OperationFingerprint, PersistedModelTurn, PersistedToolResult as ToolResult,
    ProviderConnectParams, ProviderConnectResult, ProviderDisconnectParams,
    ProviderDisconnectResult, RunId, RunStartParams, RunStartResult, RuntimeChangeReason,
    RuntimeChangedNotification, RuntimeSnapshotResult, SafeCode, SessionId, SessionMeta,
};
use oven_sdk::{ModelError, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    sync::{broadcast, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    ToolPreparationContext,
    delegation_events::{DelegationEventError, DelegationEventStore},
    events::{self, EventLogError},
    grant_journal::{GrantJournalError, GrantJournals},
    model_history,
    model_snapshots::prepare_runtime_manifest,
    permissions::PermissionPipeline,
    policy::FrozenRunPolicy,
    runtime_snapshot::{
        AgentRegistry, PublishedRuntime, RuntimePublication, build_runtime_snapshot,
    },
    session::{SessionError, SessionStore},
};

pub(crate) mod acp;
mod admission;
mod agent_md;
mod agent_memory;
mod approval_api;
mod approval_context;
mod approval_flow;
mod approval_projection;
pub(crate) mod artifact_reads;
pub(crate) mod artifacts;
pub(crate) mod attachments;
pub(crate) mod background_tasks;
pub mod chatgpt;
mod runtime_producers;
mod subagent_reports;
pub(crate) use background_tasks::{BackgroundActor, Stopped};
mod blocking_io;
pub(crate) mod compaction;
pub(crate) mod configuration;
mod delegation;
mod get_history;
pub(crate) mod handles;
mod helpers;
mod internal_agents;
mod loop_guard;
pub(crate) mod machine_api;
pub(crate) mod machine_attach;
mod mailbox;
pub(crate) mod memory_api;
pub(crate) mod memory_runs;
pub(crate) mod messaging_api;
mod model_loop;
pub(crate) mod official_agents;
pub(crate) mod official_skills;
pub(crate) mod onboarding;
mod output_capture;
pub(crate) mod plugin_diagnostics;
mod producer_claims;
mod project_api;
pub(crate) mod runtime_delegation;
pub(crate) use project_api::EngineMemoryOwners;
pub(crate) mod cron_api;
mod cron_fire;
mod cron_run;
pub(crate) mod producers;
pub(crate) mod prompt_blocks;
pub(crate) mod push_signals;
mod recovery;
mod residency;
pub(crate) mod restart;
mod runs;
mod runtime_notes;
mod runtime_tables;
mod session_delete;
mod sessions;
mod skills;
mod submit;
/// Test-only hooks; the module body is `#![cfg(test)]`.
pub(crate) mod test_hooks;
#[cfg(test)]
pub(crate) mod tests;
mod titles;
pub(crate) mod tool_execution;
mod tool_prompts;
pub(crate) mod transfers;
mod unread;
pub(crate) mod uploads;

pub(crate) use artifact_reads::read_artifact_async;
pub(crate) use artifacts::{ArtifactRouter, SessionArtifacts};

/// Session id for artifact/capture tests that do not model tree placement.
#[cfg(test)]
pub(crate) fn test_session_id() -> cookie_agent_protocol::SessionId {
    cookie_agent_protocol::SessionId::new_v7()
}
use approval_flow::{
    ApprovalEvaluationTransition, ApprovalRuntimeState, ApprovalTerminal, ApprovalToolInput,
    ModelApprovalInput, PreparedApprovalInvalidation, ReviewOutcome,
};
pub(crate) use approval_flow::{ApprovalOutcome, PendingApproval, ReviewerDenial};
#[cfg(test)]
pub(crate) use artifacts::ArtifactStore;
#[cfg(test)]
pub(crate) use blocking_io::gate as block_artifact_io_for_test;
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use compaction::ContextTokenEstimator;
use compaction::{CompactionDeferredKind, CompactionState};
use delegation::DelegationRuntimeState;
pub use get_history::EngineHistoryView;
use helpers::safe_code;
pub(crate) use internal_agents::FrozenInternalAgentPolicy;
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use internal_agents::InternalAgentLimits;
use internal_agents::{InternalAgentExecution, InternalAgentHistoryInput};
use mailbox::SessionRuntimeState;
pub use messaging_api::{AgentMessageHandle, AgentMessageInvocation, AgentRecipientState};
pub(crate) use output_capture::OutputCapture;
use output_capture::OutputState;
pub(crate) use output_capture::finish_page;
use plugin_diagnostics::{PluginDiagnosticsState, run_plugin_diagnostic_aggregator};
pub use skills::SkillInvocation;
use skills::SkillRuntimeState;
#[cfg(test)]
pub(crate) use test_hooks::*;

// A terminal result was finalized successfully before cancellation won its commit.
pub(crate) const CANCELLED_AFTER_COMPLETION: &str = "cancelled_after_completion";

use cookie_agent_runtime_api::{
    PreparedSerializationKey, PreparedTool, ToolCall, ToolConcurrency, ToolError, ToolProvider,
    ToolSpec, TurnAgentContext,
};

#[derive(Clone)]
pub struct EngineOptions {
    pub data_dir: PathBuf,
    /// The daemon's working directory. It only locates the session store and
    /// stands in for a client directory a session is created without; tools
    /// see their runtime's workspace instead.
    pub cwd: PathBuf,
    pub config: LoadedConfiguration,
    pub model_manager: Arc<ModelManager>,
    /// The global runtime's extra tools and the local coding runtime.
    pub runtimes: cookie_agent_runtime_api::RuntimeOptions,
}

#[derive(Debug, Error)]
pub enum EngineError {
    // Not `transparent`: the manual `From<SessionError>` below unwraps lazy tree
    // load rejections back to their engine-side type, and a transparent variant
    // may not carry an explicit `#[source]`.
    #[error("{0}")]
    Session(#[source] SessionError),
    #[error(transparent)]
    DelegationEvents(#[from] DelegationEventError),
    #[error(transparent)]
    Event(#[from] EventLogError),
    #[error(transparent)]
    GrantJournal(#[from] GrantJournalError),
    #[error("tool output storage error: {0}")]
    ToolOutput(#[from] std::io::Error),
    #[error("workspace {path} is unavailable: {source}")]
    Workspace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("AGENTS.md context read failed at {path}: {source}")]
    AgentMdIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration error: {0}")]
    Config(#[source] Box<cookie_agent_config::ConfigError>),
    /// The configuration changed since the caller read it.
    #[error("configuration conflict: {0}")]
    ConfigConflict(String),
    /// A configuration change that cannot be made as asked.
    #[error("configuration change refused: {0}")]
    ConfigChangeRefused(String),
    /// `onboarding.run` would leave the daemon not set up; nothing was
    /// written.
    #[error("onboarding is incomplete: {}", onboarding::describe_missing(.0))]
    OnboardingIncomplete(Vec<onboarding::OnboardingRequirement>),
    /// `onboarding.preview` since client protocol 1.56.0: there is no
    /// starter kit; the agents are official ones the daemon keeps.
    #[error("there is no starter kit: the daemon keeps the official agents itself")]
    NoStarterKit,
    #[error("agent `{0}` is not eligible in this session origin")]
    IneligibleAgent(AgentId),
    #[error(
        "agent `{agent}` references unknown model `{model}`; refresh models.dev or choose an available model"
    )]
    UnknownAgentModel { agent: AgentId, model: ModelKey },
    #[error(
        "agent `{agent}` references catalog model `{model}`, but it is unavailable in the compiled runtime"
    )]
    UnavailableAgentModel { agent: AgentId, model: ModelKey },
    #[error("agent `{0}` is disabled")]
    DisabledAgent(AgentId),
    #[error("run {0} not found")]
    MissingRun(RunId),
    #[error("session {0} is already running")]
    SessionRunning(SessionId),
    /// A new run while a scheduled restart waits for the active ones.
    #[error("the daemon is restarting")]
    Restarting,
    /// A restart asked of an engine whose program cannot restart it.
    #[error("this daemon can't restart itself")]
    RestartUnsupported,
    #[error("session {session_id} changed since it was viewed (now at seq {current_last_seq})")]
    StaleRevert {
        session_id: SessionId,
        current_last_seq: u64,
    },
    #[error("session {session_id} is a subagent of session {root_session_id}; delete the root")]
    NotARootSession {
        session_id: SessionId,
        root_session_id: SessionId,
    },
    #[error("session {busy} of the tree rooted at {root} is running")]
    SessionTreeRunning { root: SessionId, busy: SessionId },
    #[error("session {session_id} changed since it was shown (now at seq {current_last_seq})")]
    StaleDelete {
        session_id: SessionId,
        current_last_seq: u64,
    },
    #[error("client fork id was used for a different fork")]
    ForkIdempotencyConflict,
    #[error("client run id conflicts with durable run parameters")]
    RunIdempotencyConflict,
    #[error("input was handled by plugin: {0}")]
    InputHandled(String),
    #[error("model selection was blocked by plugin: {0}")]
    ModelSelectionBlocked(String),
    #[error("session operation was blocked by plugin: {0}")]
    SessionOperationBlocked(String),
    #[error("compaction cancelled by plugin: {0}")]
    CompactionCancelled(String),
    /// A hook failed for a plugin that blocks on its failure, so the
    /// operation it guards was refused.
    #[error("{0}")]
    PluginHookFailed(crate::PluginHookFailure),
    #[error("approval `{approval_id}` is not pending for session {session_id}")]
    ApprovalNotPending {
        session_id: SessionId,
        approval_id: ApprovalId,
    },
    /// An `elicitation.respond` for a question no run waits on.
    #[error("question `{0}` is not waiting for an answer")]
    ElicitationNotPending(cookie_agent_protocol::ElicitationId),
    /// An accepted form whose values do not fit its fields.
    #[error("the answer does not fit the form: {1}")]
    ElicitationAnswerInvalid(cookie_agent_protocol::ElicitationId, String),
    #[error("approval response conflicts with durable approval state")]
    ApprovalConflict,
    #[error("approval response was rejected: {0:?}")]
    ApprovalResponse(Box<ApprovalRespondFailure>),
    #[error("client rename id conflicts with a durable rename operation")]
    RenameConflict,
    #[error("model failure: {0}")]
    Model(Box<ModelError>),
    #[error("model history failure: {0}")]
    ModelHistory(#[from] model_history::HistoryError),
    /// A tool, skill, or delegation operation was rejected; the message is
    /// self-contained and safe to surface to the model and the client.
    #[error("{0}")]
    ToolFailed(String),
    /// An ACP agent could not take a run, with the reason.
    #[error("{0}")]
    Acp(String),
    /// A revert or a fork would cut into an ACP agent's conversation where
    /// the agent cannot fork it, with the reason.
    #[error("{0}")]
    AcpUnsupported(String),
    #[error("tool prompt composition failed: {0}")]
    ToolPrompt(String),
    #[error("session actor for {0} is unavailable")]
    MissingActor(SessionId),
    #[error("session {0} is owned by another cookie process")]
    SessionOwnedByAnotherProcess(SessionId),
    #[error("session actor stopped before replying")]
    ActorStopped,
    #[error("no_runnable_model")]
    NoRunnableModel,
    /// A stored model binding is not one this build can run, so the run it
    /// belongs to cannot be continued with it.
    #[error("the stored binding of model `{0}` cannot be read by this build")]
    UnreadableModelBinding(ModelKey),
    #[error("unknown agent preset `{0}`")]
    UnknownAgentPreset(String),
    #[error("provider_store_reload_failed")]
    ProviderStoreReloadFailed,
    #[error("runtime_compile_failed")]
    RuntimeCompileFailed,
    #[error("invalid runtime agent `{0}`")]
    InvalidRuntimeAgent(AgentId),
    #[error("MCP configuration error: {0}")]
    Mcp(String),
    /// A provider sign-in failed, or a connect named a sign-in method.
    #[error("sign-in to {provider_id} failed: {reason}")]
    ProviderSignIn {
        provider_id: cookie_agent_protocol::ProviderId,
        reason: String,
    },
    #[error("cache strategy configuration error: {0}")]
    CacheStrategy(String),
    #[error("session permission error: {0}")]
    Permission(String),
    #[error("goal operation rejected: {0}")]
    Goal(String),
    #[error("producer operation rejected: {0}")]
    Producer(String),
    #[error("messaging: {0}")]
    Messaging(String),
    /// A runtime could not be attached, detached, or started.
    #[error("runtime error: {0}")]
    Runtime(String),
    /// A machine registry change was refused or failed.
    #[error(transparent)]
    Machine(#[from] crate::MachineError),
    /// The memory folder could not be read or changed.
    #[error("memory: {0}")]
    Memory(String),
    /// A `memory.*` request was refused or failed.
    #[error(transparent)]
    MemoryRequest(#[from] memory_api::MemoryRequestError),
    /// A project request was refused or failed.
    #[error(transparent)]
    Project(#[from] crate::projects::ProjectError),
    /// A cron request was refused or failed.
    #[error(transparent)]
    Cron(#[from] crate::crons::CronError),
    #[error("plugin request rejected: {0}")]
    Plugin(String),
    #[error(transparent)]
    ModelManager(#[from] cookie_agent_models::ModelManagerError),
    #[error(transparent)]
    Manifest(ManifestError),
    /// An upload, an artifact read, or an input's media was refused.
    #[error("{0}")]
    Media(uploads::MediaError),
}

/// Atomic, secret-safe rejection details produced by the serialized approval transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalRespondFailure {
    pub code: ApprovalRespondErrorCode,
    pub session_id: SessionId,
    pub approval_id: ApprovalId,
    pub client_response_id: String,
    pub current_status: Option<ApprovalStatus>,
    pub current_revision: Option<u64>,
    pub current_expires_at: Option<jiff::Timestamp>,
    pub current_operation_fingerprint: Option<OperationFingerprint>,
}

impl From<ModelError> for EngineError {
    fn from(error: ModelError) -> Self {
        Self::Model(Box::new(error))
    }
}

impl From<SessionError> for EngineError {
    fn from(error: SessionError) -> Self {
        match error {
            // A lazy tree load the engine rejected carries its own failure; keep
            // the original type so callers see what actually went wrong.
            SessionError::TreeRejected(inner) => *inner,
            other => Self::Session(other),
        }
    }
}

impl EngineError {
    /// Explicit user-facing rendering; Debug is not a diagnostic transport.
    pub fn user_message(&self) -> String {
        match self {
            Self::Model(error) => {
                cookie_agent_protocol::diagnostics::model(&crate::model_policy::summary(error))
            }
            _ => cookie_agent_protocol::diagnostics::error_chain(self),
        }
    }
}

#[derive(Debug)]
struct ActiveRun {
    session: SessionId,
    /// The direct parent of a delegated session, whose subagent turn the
    /// run is (see `Engine::background_calls`).
    parent: Option<SessionId>,
    policy: Arc<FrozenRunPolicy>,
    cancellation: CancellationToken,
    cancelled_committed: Mutex<bool>,
    /// Who cancelled the run through [`Engine::cancel_run`]; the engine
    /// itself when unset.
    cancelled_by: Mutex<Option<cookie_agent_protocol::EventOrigin>>,
    /// Why the run was cancelled, as the results of the calls it interrupts
    /// name it; the first cause recorded wins.
    cancel_cause: Mutex<Option<CancelCause>>,
    /// Executing calls of background-capable tools, and the signal that
    /// moves each to the background.
    detach_requests: Mutex<HashMap<cookie_agent_protocol::ToolCallId, Arc<tokio::sync::Notify>>>,
    /// When the run started; a subagent's running turn lists from it.
    started_at: jiff::Timestamp,
    fallback_index: AtomicU64,
    auto_compaction_failures: AtomicU8,
    auto_compaction_diagnostic_emitted: AtomicBool,
    loop_guard_digests: loop_guard::OutputDigests,
}

impl ActiveRun {
    /// The origin recorded on this run's `RunCancelled`: whoever called
    /// [`Engine::cancel_run`], or the engine when it cancelled on its own.
    fn cancel_origin(&self) -> cookie_agent_protocol::EventOrigin {
        self.cancelled_by
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| event_origin("engine:model-loop"))
    }

    /// Records why the run is being cancelled, unless a cause already is.
    /// Called before the run's cancellation trips, so the calls it
    /// interrupts read it.
    fn record_cancel_cause(&self, cause: CancelCause) {
        self.cancel_cause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_or_insert(cause);
    }

    /// Why the run was cancelled, when a cause was recorded.
    fn cancel_cause(&self) -> Option<CancelCause> {
        self.cancel_cause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// Runs `work`, which may block on the runtime mutation lock, on the
/// blocking pool and waits for it without parking an async worker.
pub(crate) async fn on_blocking_pool<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, EngineError> + Send + 'static,
) -> Result<T, EngineError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(_) => Err(EngineError::ActorStopped),
    }
}

/// Why a run was cancelled. The results of the tool calls the cancellation
/// interrupts name it (`docs/site/specs/background-tools.md`, addendum of
/// 2026-10-23).
#[derive(Clone, Debug, Eq, PartialEq)]
enum CancelCause {
    /// A client cancelled the run (`run.cancel`): the user interrupted it.
    User,
    /// The engine shut down.
    Shutdown,
    /// Something other than a client cancelled the run through
    /// [`Engine::cancel_run`].
    Origin(cookie_agent_protocol::EventOrigin),
    /// The engine cancelled the run for the reason it recorded, such as a
    /// parent stopping its subagent.
    Reason(String),
}

impl CancelCause {
    /// The cause of a cancellation `origin` asked for: the user when it is
    /// a client.
    fn from_origin(origin: &cookie_agent_protocol::EventOrigin) -> Self {
        let origin_text = origin.as_str();
        if origin_text == "user" || origin_text.starts_with("client:") {
            Self::User
        } else {
            Self::Origin(origin.clone())
        }
    }
}

struct AttemptTurn {
    turn: PersistedModelTurn,
    model_turn_seq: u64,
    turn_context: Arc<TurnAgentContext>,
    /// Persisted model call IDs whose tool-call name was normalized away from
    /// invalid provider output. Dispatch must fail these instead of executing a
    /// possibly-aliased tool.
    normalized_tool_calls: HashSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingInput {
    admission_seq: u64,
    origin: cookie_agent_protocol::EventOrigin,
    content: cookie_agent_protocol::UserContent,
}

struct DelegatedResumeAdmission {
    accepted: bool,
    admission_seq: Option<u64>,
}

pub(super) enum UserInputInterception {
    Accepted {
        content: cookie_agent_protocol::UserContent,
        /// The content before a plugin transformed it, when one did.
        original: Option<cookie_agent_protocol::UserContent>,
    },
    Handled {
        reason: String,
    },
}

struct PreparedToolCall {
    call: ToolCall,
    permission_name: Option<String>,
    presentation: cookie_agent_protocol::ToolPresentation,
    prepared: Result<PreparedTool, ToolFailure>,
    interception: Option<ToolInterceptionContext>,
    intercepted_arguments: Arc<Mutex<Value>>,
    /// For a background-capable tool, whether the model asked to run the
    /// call in the background; `None` for any other tool.
    background: Option<bool>,
}

impl PreparedToolCall {
    fn concurrency(&self) -> ToolConcurrency {
        self.interception
            .as_ref()
            .map_or(ToolConcurrency::Exclusive, |context| {
                context.spec.concurrency
            })
    }

    /// The session attachment that owns the prepared tool; `None` for the
    /// base layer or a call that did not prepare.
    fn attachment(&self) -> Option<&cookie_agent_protocol::AttachmentName> {
        self.interception
            .as_ref()
            .and_then(|context| context.attachment.as_ref())
    }

    /// Where the call's tool comes from, as hooks see it; `None` for a call
    /// that names no published tool.
    fn hook_origin(&self) -> Option<cookie_agent_protocol::ExtensionToolOrigin> {
        self.interception
            .as_ref()
            .map(ToolInterceptionContext::hook_origin)
    }

    /// Layer or runtime that owns the prepared tool.
    fn origin(&self) -> Option<&crate::ToolOrigin> {
        self.interception
            .as_ref()
            .map(|context| &context.preparation.origin)
    }

    /// Calls sharing a serialization key must execute in model call order:
    /// chained preparations build on each other's output.
    fn serialization_key(&self) -> Option<crate::PreparedSerializationKey> {
        self.prepared
            .as_ref()
            .ok()
            .and_then(|prepared| prepared.serialization_key().cloned())
    }
}

impl ToolInterceptionContext {
    /// Where the tool comes from, as hooks see it.
    fn hook_origin(&self) -> cookie_agent_protocol::ExtensionToolOrigin {
        match (&self.attachment, &self.preparation.origin) {
            (Some(attachment), crate::ToolOrigin::Runtime(runtime)) => {
                cookie_agent_protocol::ExtensionToolOrigin::Runtime {
                    attachment: attachment.to_string(),
                    runtime_id: runtime.id().to_owned(),
                }
            }
            _ => cookie_agent_protocol::ExtensionToolOrigin::Base,
        }
    }
}

struct ToolInterceptionContext {
    /// The session attachment that offers the tool; `None` for the base
    /// layer.
    attachment: Option<cookie_agent_protocol::AttachmentName>,
    /// The tool as the daemon resolved it, which a declared call's approval
    /// identity binds.
    tool: cookie_agent_protocol::ToolIdentity,
    provider: Arc<dyn ToolProvider>,
    spec: ToolSpec,
    preparation: ToolPreparationContext,
    /// `None` for a tool that needs no permission.
    permission_name: Option<String>,
    permission_resource: Option<String>,
}

struct PublishedTool {
    /// The base layer or runtime attachment that offers the tool.
    source: crate::runtimes::SourcedProvider,
    /// The tool as its source declares it, under the name the source knows.
    spec: ToolSpec,
    /// The name composition gave it, by which a call finds it again; the
    /// published name except for a routed tool's machines.
    composed: String,
    /// A routed tool's machines (`docs/site/specs/machines-on-demand.md`
    /// section 5): one published tool, a `machine` argument choosing whose.
    routes: Option<ToolRoutes>,
}

/// Where each machine's call of a routed tool goes.
struct ToolRoutes {
    /// The machine a call without `machine` runs on: the round's, as the
    /// declared definition names it; `None` when the round names none.
    default: Option<String>,
    /// Each machine that offers the tool, in attach order, with the tool
    /// it calls there.
    machines: Vec<(String, PublishedTool)>,
}

impl PublishedTool {
    /// The tool a call of this published tool runs, with `arguments`, from
    /// which a routed tool's `machine` is taken: this tool itself, or the
    /// machine's. A routed call that names no machine it offers fails with
    /// where the tool is offered.
    fn route(&self, name: &str, arguments: &mut serde_json::Value) -> Result<&Self, String> {
        let Some(routes) = &self.routes else {
            return Ok(self);
        };
        let named = arguments
            .as_object_mut()
            .and_then(|object| object.remove(crate::runtime::tool_execution::MACHINE_ARGUMENT));
        let offered = || {
            routes
                .machines
                .iter()
                .map(|(machine, _)| machine.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let machine = match named {
            Some(serde_json::Value::String(machine)) => machine,
            Some(other) => {
                return Err(format!(
                    "`machine` of `{name}` must be a machine's name, not {other}; it is offered on: {}",
                    offered()
                ));
            }
            None => match &routes.default {
                Some(default) => default.clone(),
                None => {
                    return Err(format!(
                        "`{name}` needs a `machine`: it is offered on: {}",
                        offered()
                    ));
                }
            },
        };
        routes
            .machines
            .iter()
            .find(|(offering, _)| *offering == machine)
            .map(|(_, tool)| tool)
            .ok_or_else(|| format!("`{name}` is not on `{machine}`; it is on: {}", offered()))
    }
}

struct PublishedToolSet {
    definitions: Vec<ToolDefinition>,
    tools: HashMap<String, PublishedTool>,
    /// Round tools still declared after leaving the session.
    departed: HashSet<String>,
    /// Deferred tools the request declares but no result loaded yet.
    unloaded: HashSet<String>,
    /// Deferral records to append before the request.
    pending: Vec<Event>,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolFailure {
    pub(crate) code: ToolCallFailureCode,
    pub(crate) message: String,
    pub(crate) partial_output: Option<Box<ToolResult>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ToolCallFailureCode {
    InvalidUrl,
    PermissionDenied,
    RedirectError,
    Timeout,
    TransportError,
    UnsupportedContentType,
    ExecutionFailed,
    OperationChanged,
    PreparedCapabilityLost,
    UnsupportedPlatform,
    RuntimeUnavailable,
}

impl ToolCallFailureCode {
    fn safe_code(self) -> SafeCode {
        safe_code(match self {
            Self::InvalidUrl => "invalid_url",
            Self::PermissionDenied => "permission_denied",
            Self::RedirectError => "redirect_error",
            Self::Timeout => "timeout",
            Self::TransportError => "transport_error",
            Self::UnsupportedContentType => "unsupported_content_type",
            Self::ExecutionFailed => "execution_failed",
            Self::OperationChanged => "operation_changed",
            Self::PreparedCapabilityLost => "prepared_capability_lost",
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::RuntimeUnavailable => "runtime_unavailable",
        })
    }
}

impl ToolCallFailureCode {
    const fn of(error: &ToolError) -> Self {
        match error {
            ToolError::ProgressSinkClosed | ToolError::Failed(_) => Self::ExecutionFailed,
            ToolError::OperationChanged(_) => Self::OperationChanged,
            ToolError::UnsupportedSecurity(_) | ToolError::ResourceLimit(_) => {
                Self::ExecutionFailed
            }
            ToolError::UnsupportedPlatform(_) => Self::UnsupportedPlatform,
            ToolError::InvalidUrl(_) => Self::InvalidUrl,
            ToolError::PermissionDenied(_) => Self::PermissionDenied,
            ToolError::RedirectError(_) => Self::RedirectError,
            ToolError::Timeout(_) => Self::Timeout,
            ToolError::TransportError(_) => Self::TransportError,
            ToolError::UnsupportedContentType(_) => Self::UnsupportedContentType,
            ToolError::RuntimeUnavailable(_) => Self::RuntimeUnavailable,
        }
    }
}

impl From<ToolError> for ToolFailure {
    fn from(error: ToolError) -> Self {
        Self {
            code: ToolCallFailureCode::of(&error),
            message: error.message(),
            partial_output: None,
        }
    }
}

enum PendingTool {
    Prepared {
        prepared: Box<PreparedToolCall>,
        permission: crate::permissions::PermissionDecision,
    },
    ImmediateFailure(ToolFailure),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredRuntimeRevisionMapping {
    /// Protocol the mapping was written under: the client protocol version
    /// string, or the exact number builds before semver versions wrote. A
    /// record of history that nothing reads, so a protocol bump never makes
    /// an existing index unreadable.
    protocol_version: serde_json::Value,
    runtime_revision: cookie_agent_protocol::RuntimeRevision,
    model_runtime_revision: cookie_agent_protocol::RuntimeRevision,
    /// The provider state the runtime was compiled from; absent in records
    /// written before catalog views, which a disconnect cannot resolve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_state_revision: Option<cookie_agent_protocol::ProviderStateRevision>,
    /// The catalog it was compiled from, absent like `provider_state_revision`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    catalog_revision: Option<cookie_agent_protocol::CatalogRevision>,
}

/// What a runtime revision, the text of a catalog view's revision, was
/// compiled from: what a connect or disconnect filled from that view
/// expects.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RevisionMapping {
    model_runtime_revision: cookie_agent_protocol::RuntimeRevision,
    provider_state_revision: Option<cookie_agent_protocol::ProviderStateRevision>,
    catalog_revision: Option<cookie_agent_protocol::CatalogRevision>,
}

#[derive(Debug)]
struct RuntimeRevisionIndex {
    path: PathBuf,
    mappings: HashMap<cookie_agent_protocol::RuntimeRevision, RevisionMapping>,
}

impl RuntimeRevisionIndex {
    fn open(path: PathBuf) -> Result<Self, EngineError> {
        let mut mappings = HashMap::<_, RevisionMapping>::new();
        for record in events::load_jsonl_shared::<StoredRuntimeRevisionMapping>(&path)? {
            match mappings.get_mut(&record.runtime_revision) {
                Some(existing)
                    if existing.model_runtime_revision != record.model_runtime_revision =>
                {
                    return Err(EngineError::RuntimeCompileFailed);
                }
                Some(existing) => {
                    if existing.provider_state_revision.is_none() {
                        existing.provider_state_revision = record.provider_state_revision;
                    }
                    if existing.catalog_revision.is_none() {
                        existing.catalog_revision = record.catalog_revision;
                    }
                }
                None => {
                    mappings.insert(
                        record.runtime_revision,
                        RevisionMapping {
                            model_runtime_revision: record.model_runtime_revision,
                            provider_state_revision: record.provider_state_revision,
                            catalog_revision: record.catalog_revision,
                        },
                    );
                }
            }
        }
        Ok(Self { path, mappings })
    }

    fn record(
        &mut self,
        runtime_revision: cookie_agent_protocol::RuntimeRevision,
        model_runtime_revision: cookie_agent_protocol::RuntimeRevision,
        revisions: &crate::runtime_snapshot::SnapshotRevisions,
    ) -> Result<(), EngineError> {
        if let Some(existing) = self.mappings.get(&runtime_revision) {
            if existing.model_runtime_revision != model_runtime_revision {
                return Err(EngineError::RuntimeCompileFailed);
            }
            if existing.provider_state_revision.is_some() && existing.catalog_revision.is_some() {
                return Ok(());
            }
        }
        events::append_jsonl(
            &self.path,
            &StoredRuntimeRevisionMapping {
                protocol_version: serde_json::Value::String(
                    cookie_agent_protocol::CLIENT_PROTOCOL_VERSION.to_string(),
                ),
                runtime_revision: runtime_revision.clone(),
                model_runtime_revision: model_runtime_revision.clone(),
                provider_state_revision: Some(revisions.provider_state_revision.clone()),
                catalog_revision: Some(revisions.catalog_revision.clone()),
            },
        )?;
        self.mappings.insert(
            runtime_revision,
            RevisionMapping {
                model_runtime_revision,
                provider_state_revision: Some(revisions.provider_state_revision.clone()),
                catalog_revision: Some(revisions.catalog_revision.clone()),
            },
        );
        Ok(())
    }

    fn resolve(
        &self,
        runtime_revision: &cookie_agent_protocol::RuntimeRevision,
    ) -> Option<RevisionMapping> {
        self.mappings.get(runtime_revision).cloned()
    }
}

/// How long shutdown waits, in total, for the cancelled run tasks to write
/// their terminal events.
///
/// A cancelled run only has to unwind its current tool call and append one
/// event, so this is generous; a task still running when it expires is aborted
/// so shutdown never depends on a wedged provider or tool.
const RUN_TASK_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

const SESSION_MAILBOX_CAPACITY: usize = 256;
const MAX_PENDING_PREPARED_TOOLS: usize = 64;
/// Semantic revision of the no-model builtin runtime contract.
/// This is intentionally independent of the protocol and event schema version.
pub(crate) const UNAVAILABLE_BUILTIN_REVISION: &str = "internal-agent.unavailable.runtime.1";

pub(super) fn event_origin(value: &'static str) -> cookie_agent_protocol::EventOrigin {
    cookie_agent_protocol::EventOrigin::new(value).expect("static event origin is valid")
}

/// Work run on a session actor, in mailbox order, with the engine handle the
/// actor holds.
type ActorCall = Box<dyn FnOnce(Engine) -> futures_util::future::BoxFuture<'static, ()> + Send>;

/// One message on a session actor's mailbox.
///
/// Everything the actor loop does not inspect is a [`Self::Call`]. The typed
/// variants are the commands it special-cases: `Start`, `Resume`, and the two
/// pending-input promotions are deferred while a compaction holds the session,
/// `Start` also preempts a starting producer wake, and `Append` decides from
/// its event whether producer reconciliation follows.
enum SessionCommand {
    Call {
        /// Whether producer reconciliation runs after the call. Only work that
        /// touches nothing reconciliation reads (the goal, producer,
        /// user-input and run-terminal events it folds, plus the active-run,
        /// delegation and producer registries) may skip it.
        reconcile_producers: bool,
        call: ActorCall,
    },
    Append {
        run: Option<RunId>,
        origin: cookie_agent_protocol::EventOrigin,
        event: Box<Event>,
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    Start {
        params: RunStartParams,
        origin: cookie_agent_protocol::EventOrigin,
        admission: Option<(InvocationId, u64)>,
        reply: oneshot::Sender<Result<RunStartResult, EngineError>>,
    },
    Resume {
        reply: oneshot::Sender<Result<SessionMeta, EngineError>>,
    },
    PromotePendingOrComplete {
        run: RunId,
        final_text: Option<String>,
        complete_if_empty: bool,
        reply: oneshot::Sender<Result<bool, EngineError>>,
    },
    PromotePendingInputs {
        run: RunId,
        reply: oneshot::Sender<Result<producer_claims::ClaimedPrompt, EngineError>>,
    },
}

impl SessionCommand {
    /// A [`Self::Call`] running `call` on the actor, and the receiver its
    /// result is sent to.
    fn call<T, F, Fut>(
        reconcile_producers: bool,
        call: F,
    ) -> (Self, oneshot::Receiver<Result<T, EngineError>>)
    where
        T: Send + 'static,
        F: FnOnce(Engine) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, EngineError>> + Send + 'static,
    {
        let (reply, receiver) = oneshot::channel();
        let command = Self::Call {
            reconcile_producers,
            call: Box::new(move |engine| {
                Box::pin(async move {
                    let _ = reply.send(call(engine).await);
                })
            }),
        };
        (command, receiver)
    }

    fn compaction_deferred_kind(&self) -> Option<CompactionDeferredKind> {
        match self {
            Self::Start { .. } => Some(CompactionDeferredKind::Start),
            Self::PromotePendingInputs { .. } => Some(CompactionDeferredKind::PromotePendingInputs),
            Self::PromotePendingOrComplete { .. } => {
                Some(CompactionDeferredKind::PromotePendingOrComplete)
            }
            Self::Resume { .. } => Some(CompactionDeferredKind::Resume),
            _ => None,
        }
    }

    fn reject_duplicate_during_compaction(self, session: SessionId) {
        match self {
            Self::Start { reply, .. } => {
                let _ = reply.send(Err(EngineError::SessionRunning(session)));
            }
            Self::PromotePendingInputs { reply, .. } => {
                let _ = reply.send(Err(EngineError::SessionRunning(session)));
            }
            Self::PromotePendingOrComplete { reply, .. } => {
                let _ = reply.send(Ok(false));
            }
            Self::Resume { reply } => {
                let _ = reply.send(Err(EngineError::SessionRunning(session)));
            }
            _ => unreachable!("only barrier-sensitive commands are superseded"),
        }
    }
}

const MAX_COMPACTION_DEFERRED_COMMANDS: usize = 4;

pub(crate) struct Inner {
    /// The applied configuration generation; see `runtime/configuration.rs`.
    pub(crate) config: ArcSwap<LoadedConfiguration>,
    /// The generation the engine opened with, for the parts that apply only
    /// when the daemon restarts.
    pub(crate) started_config: Arc<LoadedConfiguration>,
    /// Serializes configuration reloads and writes.
    pub(crate) config_mutation: tokio::sync::Mutex<()>,
    /// Why the last configuration reload failed, until one succeeds.
    pub(crate) config_error: Mutex<Option<String>>,
    pub(crate) config_store: Mutex<crate::config_store::ConfigStore>,
    pub(crate) artifacts: Arc<ArtifactRouter>,
    mutation_locks: Mutex<HashMap<PreparedSerializationKey, Arc<tokio::sync::Mutex<()>>>>,
    pub(crate) store: Arc<SessionStore>,
    pub(crate) delegation_events: Arc<DelegationEventStore>,
    pub(crate) grant_journals: GrantJournals,
    pub(crate) model_manager: Arc<ModelManager>,
    published_runtime: ArcSwap<PublishedRuntime>,
    runtime_mutation: Mutex<()>,
    runtime_notifications: broadcast::Sender<RuntimeChangedNotification>,
    engine_events: broadcast::Sender<crate::EngineEvent>,
    /// `provider.auth.completed` for sign-ins the daemon's own listener
    /// finished.
    provider_auth_events:
        broadcast::Sender<cookie_agent_protocol::ProviderAuthCompletedNotification>,
    /// The daemon's listener for a sign-in's redirect, while one waits.
    chatgpt_listener: Mutex<Option<tokio::task::AbortHandle>>,
    plugin_diagnostics: PluginDiagnosticsState,
    runtime_revision_index: Mutex<RuntimeRevisionIndex>,
    /// The base layer, runtime templates, and running runtime instances.
    pub(crate) runtimes: Arc<crate::runtimes::RuntimeRegistry>,
    /// The machines runtimes run on.
    pub(crate) machines: Arc<crate::machines::MachineRegistry>,
    /// The projects sessions belong to.
    pub(crate) projects: Arc<crate::projects::ProjectStore>,
    /// The cron jobs.
    pub(crate) crons: Arc<crate::crons::CronStore>,
    pub(crate) cron_scheduler: cron_fire::CronSchedulerState,
    /// A restart a client scheduled, and the hook that does it.
    pub(crate) restart: restart::RestartState,
    /// The token the daemon created at start, which the started
    /// configuration lacks.
    pub(crate) served_server_token: std::sync::OnceLock<String>,
    provider_ids: Mutex<HashSet<&'static str>>,
    pub(crate) mcp: Arc<crate::McpRegistry>,
    /// MCP changes and failures of every runtime.
    pub(crate) mcp_hub: Arc<crate::runtimes::McpHub>,
    /// The ACP agents runtimes host, and their processes.
    pub(crate) acp: acp::AcpState,
    /// The providers native compaction does not work with.
    pub(crate) native_compaction: crate::native_compaction::NativeCompactionRecord,
    /// The models that rejected deferred tools.
    pub(crate) deferred_tools_record: crate::deferred_tools::record::DeferredToolsRecord,
    /// What each session's loader tools see: its latest request's deferral.
    pub(crate) deferral_views: Mutex<HashMap<SessionId, Arc<crate::deferred_tools::DeferralView>>>,
    /// The reconnects of failed MCP servers that loaded tools come from.
    pub(crate) loaded_mcp_retries: attachments::LoadedMcpRetries,
    pub(crate) plugins: Arc<crate::PluginRegistry>,
    pub(crate) mcp_mutation: tokio::sync::Mutex<()>,
    permissions: PermissionPipeline,
    pub(crate) sessions: SessionRuntimeState,
    pub(crate) delegation: DelegationRuntimeState,
    pub(crate) approvals: ApprovalRuntimeState,
    pub(crate) skills_runtime: SkillRuntimeState,
    pub(crate) output: OutputState,
    pub(crate) background: background_tasks::BackgroundTaskState,
    /// When to push a notification (`runtime/push_signals.rs`).
    pub(crate) push: push_signals::PushSignalState,
    pub(crate) compaction: CompactionState,
    pub(crate) uploads: uploads::UploadState,
    pub(crate) onboarding: onboarding::OnboardingRuntimeState,
    pub(crate) official_skills: official_skills::OfficialSkillsState,
    pub(crate) official_agents: official_agents::OfficialAgentsState,
    /// The revision each session last saw of each memory.
    pub(crate) memory_seen: memory_runs::SeenMemories,
    runtime: Option<tokio::runtime::Handle>,
    janitor_task: Mutex<Option<JoinHandle<()>>>,
    #[cfg(test)]
    pub(crate) test_hooks: test_hooks::TestHooks,
}

/// Cloneable in-process engine handle. It contains no transport concerns and
/// is safe for tool providers to call while their parent call is executing.
#[derive(Clone)]
pub struct Engine {
    pub(crate) inner: Arc<Inner>,
}

impl Engine {
    /// Opens an engine whose local coding runtime is called in-process
    /// ([`cookie_agent_runtime_api::LocalRuntime::InProcess`]). A local
    /// runtime behind the runtime protocol starts asynchronously; open it
    /// with [`Self::open_async`].
    pub fn open(options: EngineOptions) -> Result<Self, EngineError> {
        let local_workspace = local_workspace(&options)?;
        let local_tools = match &options.runtimes.local {
            cookie_agent_runtime_api::LocalRuntime::InProcess(tools) => Arc::clone(tools),
            cookie_agent_runtime_api::LocalRuntime::Protocol(_) => {
                return Err(EngineError::Runtime(
                    "the local runtime runs behind the runtime protocol; open the engine with \
                     Engine::open_async"
                        .into(),
                ));
            }
        };
        if options.config.runtimes.iter().any(|(id, template)| {
            id.as_str() == cookie_agent_runtime_api::LOCAL_RUNTIME_ID
                && (!template.mcp_servers.is_empty() || !template.acp_agents.is_empty())
        }) {
            return Err(EngineError::Runtime(
                "the local runtime hosts [runtimes.local.mcp] servers and \
                 [runtimes.local.acp] agents only behind the runtime protocol; open the engine \
                 with Engine::open_async"
                    .into(),
            ));
        }
        let local_template = Arc::new(crate::runtimes::LocalTemplate::new(
            local_workspace,
            local_tools,
        ));
        let local = local_template.start_now().map_err(EngineError::Runtime)?;
        Self::open_started(
            options,
            local_template,
            local,
            crate::runtimes::McpHub::new(),
        )
    }

    /// Opens an engine, first starting its local coding runtime and
    /// initializing it over the runtime protocol when it runs behind it.
    pub async fn open_async(options: EngineOptions) -> Result<Self, EngineError> {
        let cookie_agent_runtime_api::LocalRuntime::Protocol(connector) = &options.runtimes.local
        else {
            return Self::open(options);
        };
        let local_workspace = local_workspace(&options)?;
        let mcp_hub = crate::runtimes::McpHub::new();
        let local_template = Arc::new(
            crate::runtimes::ProtocolTemplate::local(
                Arc::clone(connector),
                local_workspace.root(),
                crate::runtimes::runtime_settings(
                    &options.config,
                    cookie_agent_runtime_api::LOCAL_RUNTIME_ID,
                ),
            )
            .with_mcp_hub(Arc::clone(&mcp_hub)),
        );
        let local = crate::RuntimeTemplate::start(
            local_template.as_ref(),
            &cookie_agent_protocol::CapabilityName::coding(),
            Some(local_workspace.root()),
        )
        .await
        .map_err(|reason| {
            EngineError::Runtime(format!("runtime `local` failed to start: {reason}"))
        })?;
        Self::open_started(options, local_template, local, mcp_hub)
    }

    fn open_started(
        options: EngineOptions,
        local_template: Arc<dyn crate::RuntimeTemplate>,
        local: crate::StartedRuntime,
        mcp_hub: Arc<crate::runtimes::McpHub>,
    ) -> Result<Self, EngineError> {
        let mut provider_ids = HashSet::new();
        for provider in options.runtimes.global_tools.iter().chain(&local.providers) {
            reserve_provider_id(&mut provider_ids, provider.as_ref())?;
        }
        let config_store = crate::config_store::ConfigStore::new(&options.config);
        let current_models = options.model_manager.current();
        let acp = acp::AcpState::new(&options.config, &options.data_dir);
        let (agents, agent_presets) =
            resolve_agent_registries(&options.config, &current_models, &acp.runtime_agents())?;
        let current_manifest = prepare_runtime_manifest(&current_models)?;
        let (snapshot, revisions) = build_runtime_snapshot(
            &current_models,
            &agents,
            &agent_presets,
            &options.config.revision,
        )?;
        let published_runtime = Arc::new(PublishedRuntime {
            result: RuntimeSnapshotResult { snapshot },
            revisions,
            models: Arc::clone(&current_models),
            agents,
            agent_presets,
            current_manifest,
        });
        let store = SessionStore::open(&options.data_dir, &options.cwd)?;
        // The first start keys off the memory folder's absence, so it runs
        // before anything can take the memory lock or move machine folders.
        let memory = crate::memory::MemoryRoot::new(&options.data_dir);
        memory
            .create_defaults_if_absent()
            .map_err(|error| EngineError::Memory(error.to_string()))?;
        // Memories written before stamps get theirs from their files' times
        // (docs/site/specs/memory.md section 16.9.6); until then reads take
        // those times, so a failure only waits for the next start.
        if let Err(error) = memory.stamp_unstamped() {
            eprintln!("cookie-agent: memory stamps: {error}");
        }
        let machines = Arc::new(crate::machines::MachineRegistry::open(&options.data_dir)?);
        let projects = Arc::new(crate::projects::ProjectStore::new(
            &options.data_dir,
            machines.memory().clone(),
        ));
        // Every memory folder of a project becomes a project before any
        // memory write can be refused for naming none.
        if let Err(error) =
            projects.adopt_memory_folders(options.config.runtime.memory.overview_max_bytes)
        {
            eprintln!("cookie-agent: projects: {error}");
        }
        let crons = Arc::new(crate::crons::CronStore::open(&options.data_dir)?);
        let artifacts = ArtifactRouter::for_store(&store)?;
        // Every write belongs to the directory of the writing session's root.
        let placement = Arc::clone(&store);
        artifacts.install_tree_resolver(Arc::new(move |session| {
            Some(placement.root_of(session).unwrap_or(session))
        }));
        let mcp = Arc::new(
            crate::McpRegistry::new(
                options.config.mcp_servers.clone(),
                cookie_agent_mcp::McpRegistryOptions {
                    oauth_store: Some(options.data_dir.join("mcp-oauth.json")),
                    observer: Some(mcp_hub.global_observer()),
                    // The global runtime has no workspace: its servers run
                    // in the daemon user's home, not wherever the daemon
                    // was started.
                    working_dir: crate::runtimes::daemon_home().map(std::path::PathBuf::from),
                },
            )
            .map_err(|error| EngineError::ToolFailed(error.to_string()))?,
        );
        reserve_provider_id(&mut provider_ids, mcp.as_ref())?;
        // Every plugin's tools share the `plugin` provenance ID.
        provider_ids.insert(crate::plugin::PLUGIN_PROVIDER_ID);
        // Inside the global runtime, MCP tools may not take the names of its
        // other tools.
        for provider in &options.runtimes.global_tools {
            mcp.reserve_provider(provider.as_ref())
                .map_err(|error| EngineError::ToolFailed(error.to_string()))?;
        }
        // The memory tools hold the engine, which they get once it exists.
        let memory_tools = Arc::new(crate::memory_tools::MemoryTools::new());
        reserve_provider_id(&mut provider_ids, memory_tools.as_ref())?;
        mcp.reserve_provider(memory_tools.as_ref())
            .map_err(|error| EngineError::ToolFailed(error.to_string()))?;
        let runtimes_tool = Arc::new(crate::runtimes_tool::RuntimesTool::new());
        reserve_provider_id(&mut provider_ids, runtimes_tool.as_ref())?;
        mcp.reserve_provider(runtimes_tool.as_ref())
            .map_err(|error| EngineError::ToolFailed(error.to_string()))?;
        let machines_tool = Arc::new(crate::machines_tool::MachinesTool::new());
        reserve_provider_id(&mut provider_ids, machines_tool.as_ref())?;
        mcp.reserve_provider(machines_tool.as_ref())
            .map_err(|error| EngineError::ToolFailed(error.to_string()))?;
        let cron_tools = Arc::new(crate::cron_tools::CronTools::new());
        reserve_provider_id(&mut provider_ids, cron_tools.as_ref())?;
        mcp.reserve_provider(cron_tools.as_ref())
            .map_err(|error| EngineError::ToolFailed(error.to_string()))?;
        let agents_tool = Arc::new(crate::agents_tool::AgentsTool::new());
        reserve_provider_id(&mut provider_ids, agents_tool.as_ref())?;
        mcp.reserve_provider(agents_tool.as_ref())
            .map_err(|error| EngineError::ToolFailed(error.to_string()))?;
        let plugins = Arc::new(crate::PluginRegistry::new(options.config.plugins.clone()));
        let mut global_providers: Vec<Arc<dyn ToolProvider>> = vec![mcp.clone()];
        global_providers.extend(options.runtimes.global_tools.iter().cloned());
        global_providers.push(memory_tools.clone());
        global_providers.push(runtimes_tool.clone());
        global_providers.push(machines_tool.clone());
        global_providers.push(cron_tools.clone());
        global_providers.push(agents_tool.clone());
        // Each plugin's tools compose under the plugin's name.
        global_providers.extend(plugins.tool_providers());
        let runtimes = Arc::new(crate::runtimes::RuntimeRegistry::new(
            Arc::new(crate::runtimes::GlobalTemplate),
            crate::runtimes::GlobalTemplate::started(
                global_providers,
                options.config.skills.clone(),
            ),
            local_template,
            local,
        ));
        runtimes.watch_builtin();
        crate::runtimes::register_configured_templates(&runtimes, &options.config, &mcp_hub)?;
        let delegation_events = DelegationEventStore::new(Arc::clone(&store));
        let grant_journals = GrantJournals::new(store.workdir_dir_path());
        let (runtime_notifications, _) = broadcast::channel(64);
        let (engine_events, _) = broadcast::channel(256);
        let plugin_diagnostics =
            Arc::new(plugin_diagnostics::PluginDiagnosticAccumulator::default());
        let mut runtime_revision_index = RuntimeRevisionIndex::open(
            store.workdir_dir_path().join("runtime-revisions-v8.jsonl"),
        )?;
        runtime_revision_index.record(
            published_runtime.revisions.runtime_revision.clone(),
            current_models.runtime_revision().clone(),
            &published_runtime.revisions,
        )?;
        let initial_onboarding =
            onboarding::onboarding_state(&options.config, &published_runtime.models);
        let engine = Self {
            inner: Arc::new(Inner {
                started_config: Arc::new(options.config.clone()),
                config: ArcSwap::from_pointee(options.config),
                config_mutation: tokio::sync::Mutex::new(()),
                config_error: Mutex::new(None),
                config_store: Mutex::new(config_store),
                artifacts,
                mutation_locks: Mutex::new(HashMap::new()),
                store,
                delegation_events,
                grant_journals,
                model_manager: options.model_manager,
                published_runtime: ArcSwap::from(published_runtime),
                runtime_mutation: Mutex::new(()),
                runtime_notifications,
                engine_events,
                provider_auth_events: broadcast::channel(16).0,
                chatgpt_listener: Mutex::new(None),
                plugin_diagnostics: PluginDiagnosticsState {
                    accumulator: Arc::clone(&plugin_diagnostics),
                    task: Mutex::new(None),
                },
                runtime_revision_index: Mutex::new(runtime_revision_index),
                runtimes,
                machines,
                projects,
                crons,
                cron_scheduler: cron_fire::CronSchedulerState::default(),
                restart: restart::RestartState::default(),
                served_server_token: std::sync::OnceLock::new(),
                provider_ids: Mutex::new(provider_ids),
                mcp,
                mcp_hub,
                acp,
                native_compaction: crate::native_compaction::NativeCompactionRecord::open(
                    &options.data_dir,
                ),
                deferred_tools_record: crate::deferred_tools::record::DeferredToolsRecord::open(
                    &options.data_dir,
                ),
                deferral_views: Mutex::new(HashMap::new()),
                loaded_mcp_retries: attachments::LoadedMcpRetries::default(),
                plugins,
                mcp_mutation: tokio::sync::Mutex::new(()),
                permissions: PermissionPipeline::default(),
                sessions: SessionRuntimeState::default(),
                delegation: DelegationRuntimeState::default(),
                approvals: ApprovalRuntimeState::default(),
                skills_runtime: SkillRuntimeState::default(),
                output: OutputState::default(),
                background: background_tasks::BackgroundTaskState::default(),
                push: push_signals::PushSignalState::default(),
                compaction: CompactionState::default(),
                uploads: uploads::UploadState::default(),
                onboarding: onboarding::OnboardingRuntimeState::new(initial_onboarding),
                official_skills: official_skills::OfficialSkillsState::new(&options.data_dir),
                official_agents: official_agents::OfficialAgentsState::new(&options.data_dir),
                memory_seen: memory_runs::SeenMemories::default(),
                runtime: tokio::runtime::Handle::try_current().ok(),
                janitor_task: Mutex::new(None),
                #[cfg(test)]
                test_hooks: test_hooks::TestHooks::default(),
            }),
        };
        memory_tools.bind(&engine);
        runtimes_tool.bind(&engine);
        machines_tool.bind(&engine);
        cron_tools.bind(&engine);
        agents_tool.bind(&engine);
        // Engine unit tests compose their own base-layer stand-ins.
        #[cfg(not(test))]
        if let Err(error) = engine.register_base_layer() {
            // Base providers hold the engine; dropping them frees it.
            drop(engine.inner.runtimes.clear());
            return Err(error);
        }
        let weak = Arc::downgrade(&engine.inner);
        engine
            .inner
            .plugins
            .set_emit_handler(Arc::new(move |request| {
                let weak = weak.clone();
                Box::pin(async move {
                    let Some(inner) = weak.upgrade() else {
                        return crate::plugin::PluginEmitOutcome {
                            bus: cookie_agent_protocol::ExtensionEmitStatus::Dropped,
                            reason: Some("engine is shutting down".into()),
                        };
                    };
                    Engine { inner }.publish_plugin_emit(request).await
                })
            }));
        if let Some(runtime) = &engine.inner.runtime {
            let weak = Arc::downgrade(&engine.inner);
            let task = runtime.spawn(async move {
                run_plugin_diagnostic_aggregator(weak, plugin_diagnostics).await;
            });
            *engine
                .inner
                .plugin_diagnostics
                .task
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(task);
        }
        // No session state is read here: delegation records, grants, and
        // producer state arrive with each tree's load (tree-local B1, C1).
        engine.install_tree_load_observer()?;
        engine.install_runtime_producers();
        engine.install_machine_assignment();
        // After the first start above and before any run: every agent's
        // memory file exists when the first snapshot is taken.
        engine.install_agent_memory_sync();
        if let Some(runtime) = &engine.inner.runtime {
            engine.inner.mcp.start_eager(runtime);
            engine.inner.plugins.start_eager(runtime);
        }
        engine.start_subagent_janitor();
        engine.start_cron_scheduler();
        engine.install_acp();
        Ok(engine)
    }

    #[must_use]
    pub fn current_runtime(&self) -> Arc<PublishedRuntime> {
        self.inner.published_runtime.load_full()
    }

    /// The published runtime, after reloading a provider store another
    /// process changed. It never waits for a runtime change in progress,
    /// such as a catalog refresh recompiling every model, which can take
    /// seconds: it answers with the runtime published now, and the change
    /// announces its own on `runtime.changed`.
    pub fn runtime_snapshot(&self) -> Result<RuntimeSnapshotResult, EngineError> {
        let mutation = match self.inner.runtime_mutation.try_lock() {
            Ok(mutation) => Some(mutation),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        if let Some(mutation) = mutation {
            self.reconcile_provider_store_locked(&mutation)?;
        }
        Ok(self.current_runtime().result.clone())
    }

    #[must_use]
    pub fn subscribe_runtime_changes(&self) -> broadcast::Receiver<RuntimeChangedNotification> {
        self.inner.runtime_notifications.subscribe()
    }

    #[must_use]
    pub fn subscribe_engine_events(&self) -> broadcast::Receiver<crate::EngineEvent> {
        self.inner.engine_events.subscribe()
    }

    /// Session index changes for every session, including permission-mode
    /// changes. A receiver that lags has missed changes and must refetch.
    #[must_use]
    pub fn subscribe_session_changes(
        &self,
    ) -> broadcast::Receiver<cookie_agent_protocol::SessionsChangedNotification> {
        self.inner.store.subscribe_index_changes()
    }

    /// A counter bumped on every MCP server configuration or status change.
    #[must_use]
    pub fn subscribe_mcp_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.mcp_hub.subscribe_changes()
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn publish_engine_event_for_test(&self, event: crate::EngineEvent) {
        let _ = self.inner.engine_events.send(event);
    }

    pub fn connect_provider(
        &self,
        params: ProviderConnectParams,
    ) -> Result<ProviderConnectResult, EngineError> {
        use cookie_agent_models::provider_store::{
            ClientConnectId, ProviderAuthValues, ProviderStoreMutation,
        };

        if cookie_agent_models::recipes::auth_method(params.auth_method.as_str())
            .is_some_and(cookie_agent_models::recipes::AuthMethodRecipe::is_sign_in)
        {
            return Err(EngineError::ProviderSignIn {
                provider_id: params.provider_id,
                reason: "sign_in_method_not_connectable: sign in with provider.auth.begin instead"
                    .into(),
            });
        }
        let _mutation = self
            .inner
            .runtime_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let provider_id = params.provider_id.clone();
        let auth_values = params
            .auth_values
            .field_names()
            .map(|name| {
                let field = cookie_agent_protocol::AuthFieldName::new(name.to_owned())
                    .map_err(|_| EngineError::RuntimeCompileFailed)?;
                let value = params
                    .auth_values
                    .get(&field)
                    .ok_or(EngineError::RuntimeCompileFailed)?
                    .to_owned();
                Ok((field, value))
            })
            .collect::<Result<_, EngineError>>()?;
        // The form was filled from the view of `expected_revision`, whose
        // catalog the store checks (and a retry of the same request
        // replays); a view this daemon never published is a conflict.
        let conflict = || {
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(
                cookie_agent_models::provider_store::ProviderStoreError::CatalogRevisionConflict,
            ))
        };
        let expected_catalog_revision =
            cookie_agent_protocol::RuntimeRevision::new(params.expected_revision.as_str())
                .ok()
                .and_then(|revision| {
                    self.inner
                        .runtime_revision_index
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .resolve(&revision)
                })
                .and_then(|mapping| mapping.catalog_revision)
                .ok_or_else(conflict)?;
        let request = cookie_agent_models::ProviderConnectRequest {
            provider_id,
            expected_catalog_revision,
            setup_values: params
                .setup_values
                .into_iter()
                .map(|(id, value)| {
                    let value = serde_json::from_value(
                        serde_json::to_value(value)
                            .map_err(|_| EngineError::RuntimeCompileFailed)?,
                    )
                    .map_err(|_| EngineError::RuntimeCompileFailed)?;
                    Ok((id, value))
                })
                .collect::<Result<_, EngineError>>()?,
            auth_method: params.auth_method,
            auth_values: ProviderAuthValues::new(auth_values)
                .map_err(cookie_agent_models::ModelManagerError::from)?,
            client_connect_id: ClientConnectId::new(params.client_connect_id.as_str())
                .map_err(cookie_agent_models::ModelManagerError::from)?,
        };
        let previous = self.current_runtime();
        let result = self.inner.model_manager.connect(request, |candidate, _| {
            self.prepare_publication(
                &self.inner.config.load(),
                candidate,
                &previous,
                vec![RuntimeChangeReason::ProviderConnected],
            )
            .map_err(|_| cookie_agent_models::ModelManagerError::RuntimeCompileFailed)
        })?;
        let runtime = result.publication.map_or_else(
            || self.current_runtime(),
            |publication| self.publish(publication),
        );
        let durable_connection = match &result.mutation {
            ProviderStoreMutation::Connect {
                durable_connection, ..
            } => {
                crate::runtime_snapshot::projection::project_durable_connection(durable_connection)?
            }
            ProviderStoreMutation::Disconnect { .. } => {
                return Err(EngineError::RuntimeCompileFailed);
            }
        };
        Ok(ProviderConnectResult {
            durable_connection,
            effective_auth_source: crate::runtime_snapshot::projection::effective_auth_source(
                result.effective_auth,
            )?,
            runtime: runtime.result.snapshot.clone(),
            replayed: result.replayed,
        })
    }

    pub fn disconnect_provider(
        &self,
        params: ProviderDisconnectParams,
    ) -> Result<ProviderDisconnectResult, EngineError> {
        use cookie_agent_models::provider_store::{ClientRequestId, ProviderStoreMutation};

        let _mutation = self
            .inner
            .runtime_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = self.current_runtime();
        let conflict = || {
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(
                cookie_agent_models::provider_store::ProviderStoreError::RuntimeRevisionConflict,
            ))
        };
        // A view's revision is its runtime revision's text, which the index
        // maps to the model runtime it was compiled from; the store refuses
        // a model runtime that is no longer current.
        let expected_runtime_revision =
            cookie_agent_protocol::RuntimeRevision::new(params.expected_revision.as_str())
                .map_err(|_| conflict())?;
        let expected = self
            .inner
            .runtime_revision_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resolve(&expected_runtime_revision)
            .ok_or_else(conflict)?;
        let request = cookie_agent_models::ProviderDisconnectRequest {
            provider_id: params.provider_id.clone(),
            expected_runtime_revision: expected.model_runtime_revision,
            expected_provider_state_revision: expected
                .provider_state_revision
                .ok_or_else(conflict)?,
            expected_connection_generation: params
                .expected_connection_generation
                .map(|value| {
                    cookie_agent_models::provider_store::ProviderConnectionGeneration::new(
                        value.get(),
                    )
                })
                .transpose()
                .map_err(cookie_agent_models::ModelManagerError::from)?,
            client_request_id: ClientRequestId::new(params.client_request_id.as_str())
                .map_err(cookie_agent_models::ModelManagerError::from)?,
        };
        let result = self
            .inner
            .model_manager
            .disconnect(request, |candidate, _| {
                self.prepare_publication(
                    &self.inner.config.load(),
                    candidate,
                    &previous,
                    vec![RuntimeChangeReason::ProviderDisconnected],
                )
                .map_err(|_| cookie_agent_models::ModelManagerError::RuntimeCompileFailed)
            })?;
        let runtime = result.publication.map_or_else(
            || self.current_runtime(),
            |publication| self.publish(publication),
        );
        if !matches!(result.mutation, ProviderStoreMutation::Disconnect { .. }) {
            return Err(EngineError::RuntimeCompileFailed);
        }
        let receipt = result.mutation.durable_receipt();
        Ok(ProviderDisconnectResult {
            durable_receipt: cookie_agent_protocol::DurableProviderReceipt {
                receipt_id: receipt
                    .receipt_id
                    .to_string()
                    .parse()
                    .map_err(|_| EngineError::RuntimeCompileFailed)?,
                committed_at: receipt.committed_at,
            },
            provider_id: params.provider_id,
            disconnected: true,
            effective_auth_state: crate::runtime_snapshot::projection::effective_auth_state(
                result.effective_auth,
            ),
            runtime: runtime.result.clone(),
            replayed: result.replayed,
        })
    }

    /// Reloads the provider store when it changed on disk and publishes the
    /// runtime it compiles to. Waits for a runtime change in progress, so
    /// what follows sees the runtime that change publishes; async callers
    /// use `reconcile_provider_store_off_worker`.
    pub fn reconcile_provider_store(&self) -> Result<bool, EngineError> {
        let mutation = self
            .inner
            .runtime_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.reconcile_provider_store_locked(&mutation)
    }

    /// [`Self::reconcile_provider_store`] on the blocking pool. Waiting for
    /// a runtime change in progress, and the recompile a reload needs, take
    /// seconds in debug builds and must not park an async worker; the caller
    /// still waits, so a run that starts mid-change uses the runtime that
    /// change publishes.
    pub(crate) async fn reconcile_provider_store_off_worker(&self) -> Result<bool, EngineError> {
        let engine = self.clone();
        on_blocking_pool(move || engine.reconcile_provider_store()).await
    }

    /// [`Self::reconcile_provider_store`] under the runtime mutation lock
    /// the caller holds.
    fn reconcile_provider_store_locked(
        &self,
        _mutation: &std::sync::MutexGuard<'_, ()>,
    ) -> Result<bool, EngineError> {
        let previous = self.current_runtime();
        let reloaded = self
            .inner
            .model_manager
            .reload_store_if_changed(|candidate| {
                self.prepare_publication(
                    &self.inner.config.load(),
                    candidate,
                    &previous,
                    vec![
                        RuntimeChangeReason::ProviderStoreChanged,
                        RuntimeChangeReason::ProviderStoreReloaded,
                    ],
                )
                .map_err(|_| cookie_agent_models::ModelManagerError::RuntimeCompileFailed)
            })
            .map_err(|_| EngineError::ProviderStoreReloadFailed)?;
        if let Some((_, publication)) = reloaded {
            self.publish(publication);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Atomically recompiles and publishes a newly acquired catalog snapshot.
    pub fn refresh_catalog(
        &self,
        catalog: Arc<cookie_agent_models::catalog::CatalogSnapshot>,
    ) -> Result<RuntimeSnapshotResult, EngineError> {
        let _mutation = self
            .inner
            .runtime_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = self.current_runtime();
        let authored = previous.models.authored().clone();
        let reason = match catalog.source {
            cookie_agent_models::catalog::CatalogSource::Network => {
                RuntimeChangeReason::CatalogRefreshed
            }
            cookie_agent_models::catalog::CatalogSource::Cache
            | cookie_agent_models::catalog::CatalogSource::Bootstrap => {
                RuntimeChangeReason::CatalogFallback
            }
        };
        let (_, publication) =
            self.inner
                .model_manager
                .reload_inputs(authored, catalog, |candidate| {
                    self.prepare_publication(
                        &self.inner.config.load(),
                        candidate,
                        &previous,
                        vec![reason],
                    )
                    .map_err(|_| cookie_agent_models::ModelManagerError::RuntimeCompileFailed)
                })?;
        Ok(self.publish(publication).result.clone())
    }

    /// Resolves agents of `config` against `models` into the next published
    /// runtime.
    pub(crate) fn prepare_publication(
        &self,
        config: &LoadedConfiguration,
        models: &Arc<cookie_agent_models::CompiledModelRuntime>,
        previous: &Arc<PublishedRuntime>,
        mut reasons: Vec<RuntimeChangeReason>,
    ) -> Result<RuntimePublication, EngineError> {
        #[cfg(test)]
        if self
            .inner
            .test_hooks
            .publication_failure
            .swap(false, Ordering::AcqRel)
        {
            return Err(EngineError::RuntimeCompileFailed);
        }
        let (agents, agent_presets) =
            resolve_agent_registries(config, models, &self.inner.acp.runtime_agents())?;
        let current_manifest = prepare_runtime_manifest(models)?;
        let (snapshot, revisions) =
            build_runtime_snapshot(models, &agents, &agent_presets, &config.revision)?;
        self.inner
            .runtime_revision_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(
                revisions.runtime_revision.clone(),
                models.runtime_revision().clone(),
                &revisions,
            )?;
        reasons.sort();
        reasons.dedup();
        let notification = RuntimeChangedNotification {
            previous_revision: Some(previous.result.snapshot.revision.clone()),
            snapshot: snapshot.clone(),
            reasons,
        };
        Ok(RuntimePublication {
            runtime: Arc::new(PublishedRuntime {
                result: RuntimeSnapshotResult { snapshot },
                revisions,
                models: Arc::clone(models),
                agents,
                agent_presets,
                current_manifest,
            }),
            notification,
        })
    }

    pub(crate) fn publish(&self, publication: RuntimePublication) -> Arc<PublishedRuntime> {
        self.inner
            .published_runtime
            .store(Arc::clone(&publication.runtime));
        self.publish_onboarding_state(&publication.runtime.models);
        let _ = self
            .inner
            .runtime_notifications
            .send(publication.notification);
        publication.runtime
    }

    /// Receives the products of a lazy tree load and folds them into the engine
    /// singletons (§3.3(d)). Validation runs first so a rejected load cannot
    /// leave half-applied state behind, and the artifact router is told the tree
    /// is loaded last: until this function returns `Ok` the load is not a
    /// completed one, and a sweep must still treat the child logs as unproven.
    fn apply_tree_load(
        &self,
        products: Arc<crate::session::TreeLoadProducts>,
    ) -> Result<(), EngineError> {
        let extended = self
            .inner
            .delegation_events
            .extend_from_payloads(&products.delegations)?;
        if !extended.is_empty() {
            // Only this tree's records are rebuilt; no log is read to do it
            // (tree-local C3).
            self.rebuild_delegation_registry(products.root, false)?;
        }
        let invalidated = self
            .inner
            .grant_journals
            .for_root(products.root)?
            .invalidated_ids();
        for grant in &products.grants {
            if !invalidated.contains(&grant.grant_id) {
                self.inner.approvals.store.grant(grant.clone());
            }
        }
        self.reconcile_loaded_tree_producers(products.producer_projections.clone());
        Ok(())
    }

    /// Installs the store-side hook and delivers everything a load that already
    /// completed was holding back (§3.3, D5).
    fn install_tree_load_observer(&self) -> Result<(), EngineError> {
        struct Observer(std::sync::Weak<Inner>);

        impl crate::session::TreeLoadObserver for Observer {
            fn tree_loaded(
                &self,
                products: Arc<crate::session::TreeLoadProducts>,
            ) -> Result<(), EngineError> {
                let inner = self.0.upgrade().ok_or(EngineError::ActorStopped)?;
                Engine { inner }.apply_tree_load(products)
            }
        }

        self.inner
            .store
            .set_tree_load_observer(Arc::new(Observer(Arc::downgrade(&self.inner))))?;
        Ok(())
    }

    /// Completes the tree of `id` before it is used (§3.2/§3.3), then delivers
    /// any load this process finished while no hook was installed yet (D5).
    /// Cheap once the tree is loaded.
    pub(crate) fn ensure_tree_loaded(&self, id: SessionId) -> Result<(), EngineError> {
        self.inner.store.ensure_tree_for(id)?;
        self.inner.store.drain_pending_loads()?;
        Ok(())
    }

    /// Producer reconciliation for children surfaced by a tree load. Runs on the
    /// engine runtime when one is available; otherwise the periodic plugin
    /// producer scan picks them up, since loaded trees cache their hits (§4.4).
    fn reconcile_loaded_tree_producers(
        &self,
        projections: Vec<(SessionId, crate::goal_projection::GoalProducerProjection)>,
    ) {
        if projections.is_empty() {
            return;
        }
        let Some(handle) = self
            .inner
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        handle.spawn(async move {
            for (session, projection) in projections {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let _ = Engine { inner }
                    .on_actor_unreconciled(session, move |engine| {
                        engine.reconcile_producers_direct(session, Some(projection))
                    })
                    .await;
            }
        });
    }

    pub(super) fn mutation_lock(
        &self,
        key: &PreparedSerializationKey,
    ) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .inner
            .mutation_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Keyed by the file a call writes, so the map would grow with every
        // file the daemon ever edits: a lock nobody holds or waits on goes.
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Registers a tool provider after engine open; panics on a collision.
    pub fn register_tool_provider(&self, layer: crate::ToolLayer, provider: Arc<dyn ToolProvider>) {
        self.try_register_tool_provider(layer, provider)
            .expect("tool provider collision");
    }

    /// Adds `provider` to the base layer or to the `global` or `local`
    /// runtime instance. The engine registers the base layer itself when it
    /// opens and the runtimes get their providers from
    /// [`EngineOptions::runtimes`]; tests may add more.
    pub fn try_register_tool_provider(
        &self,
        layer: crate::ToolLayer,
        provider: Arc<dyn ToolProvider>,
    ) -> Result<(), EngineError> {
        let mut provider_ids = self
            .inner
            .provider_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reserve_provider_id(&mut provider_ids, provider.as_ref())?;
        match layer {
            crate::ToolLayer::Base => self.inner.runtimes.push_base(provider),
            crate::ToolLayer::Global => {
                if let Err(error) = self.inner.mcp.reserve_provider(provider.as_ref()) {
                    provider_ids.remove(provider.provider_id());
                    return Err(EngineError::ToolFailed(error.to_string()));
                }
                self.inner.runtimes.global().push_provider(provider);
            }
            crate::ToolLayer::Local => self.inner.runtimes.local().push_provider(provider),
        }
        Ok(())
    }

    #[must_use]
    pub fn mcp_statuses(&self) -> Vec<crate::McpServerStatus> {
        self.inner.mcp.statuses()
    }

    #[must_use]
    pub fn plugin_statuses(&self) -> Vec<cookie_agent_protocol::PluginInfo> {
        self.inner.plugins.statuses()
    }

    /// Starts plugin `name` again now: a failed one retries, and a connected
    /// one is stopped and restarted.
    pub fn restart_plugin(&self, name: &str) -> Result<(), EngineError> {
        self.inner
            .plugins
            .restart(name)
            .map_err(EngineError::Plugin)
    }

    /// Plugin status changes, coalesced.
    pub fn subscribe_plugin_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.plugins.subscribe_changes()
    }

    #[cfg(test)]
    pub(crate) fn block_plugin_diagnostic_appends_for_test(&self) {
        *self
            .inner
            .test_hooks
            .plugin_diagnostic_append_block
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Arc::new(tokio::sync::Notify::new()));
    }

    #[cfg(test)]
    pub(crate) fn pending_plugin_diagnostic_keys_for_test(&self) -> usize {
        self.inner.plugin_diagnostics.accumulator.key_count()
    }

    /// Stops new session mailbox traffic and cancels active work. Existing
    /// client clones may keep a session mailbox alive.
    pub async fn shutdown(&self) {
        self.inner
            .delegation
            .admission_tasks_closing
            .store(true, Ordering::Release);
        let janitor = self
            .inner
            .janitor_task
            .lock()
            .ok()
            .and_then(|mut task| task.take());
        if let Some(task) = janitor {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.take_cron_scheduler() {
            task.abort();
            let _ = task.await;
        }
        let tasks = self
            .inner
            .delegation
            .admission_tasks
            .lock()
            .map(|mut tasks| tasks.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                eprintln!("admission task stopped during shutdown: {error}");
            }
        }
        let blocking_tasks = self
            .inner
            .delegation
            .admission_blocking_tasks
            .lock()
            .map(|mut tasks| tasks.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for task in blocking_tasks {
            if let Err(error) = task.await {
                eprintln!("admission blocking task stopped during shutdown: {error}");
            }
        }
        let active: Vec<_> = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        for run in active {
            run.record_cancel_cause(CancelCause::Shutdown);
            run.cancellation.cancel();
        }
        // Joined here, before the plugin and MCP transports go away and well
        // before the actors are cleared: a run task may be mid tool call, and
        // it needs live transports to unwind and a live store to append its
        // `RunCancelled`. Anything still running when the bound expires is
        // aborted, so shutdown stays bounded.
        self.join_run_tasks(RUN_TASK_SHUTDOWN_TIMEOUT).await;
        self.inner.acp.processes.stop_all().await;
        self.inner.plugins.shutdown().await;
        self.inner.mcp.shutdown().await;
        self.inner
            .plugin_diagnostics
            .accumulator
            .shutdown
            .store(true, Ordering::Release);
        self.inner
            .plugin_diagnostics
            .accumulator
            .notify
            .notify_one();
        let diagnostic_task = self
            .inner
            .plugin_diagnostics
            .task
            .lock()
            .ok()
            .and_then(|mut task| task.take());
        if let Some(mut task) = diagnostic_task
            && tokio::time::timeout(
                plugin_diagnostics::PLUGIN_DIAGNOSTIC_SHUTDOWN_TIMEOUT,
                &mut task,
            )
            .await
            .is_err()
        {
            for plugin in self.inner.plugin_diagnostics.accumulator.offenders() {
                self.inner.plugins.note_offender_diagnostic(
                    &plugin,
                    "plugin diagnostic drain incomplete: shutdown deadline exceeded".into(),
                );
            }
            task.abort();
            let _ = task.await;
        }
        self.inner
            .sessions
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let connections = self.inner.runtimes.clear();
        futures_util::future::join_all(
            connections
                .iter()
                .map(crate::runtimes::RuntimeClient::shutdown),
        )
        .await;
        self.inner.store.release_ownership();
    }
}

fn local_workspace(options: &EngineOptions) -> Result<crate::WorkspaceContext, EngineError> {
    crate::WorkspaceContext::new(&options.runtimes.local_workspace).map_err(|source| {
        EngineError::Workspace {
            path: options.runtimes.local_workspace.clone(),
            source,
        }
    })
}

fn reserve_provider_id(
    provider_ids: &mut HashSet<&'static str>,
    provider: &dyn ToolProvider,
) -> Result<(), EngineError> {
    let provider_id = provider.provider_id();
    if !provider_ids.insert(provider_id) {
        return Err(EngineError::ToolFailed(format!(
            "tool provider ID `{provider_id}` is already registered"
        )));
    }
    Ok(())
}

type ResolvedAgentRegistries = (Arc<AgentRegistry>, BTreeMap<String, Arc<AgentRegistry>>);

fn resolve_agent_registries(
    config: &LoadedConfiguration,
    models: &Arc<cookie_agent_models::CompiledModelRuntime>,
    runtime_agents: &[crate::runtime_snapshot::RuntimeAgent],
) -> Result<ResolvedAgentRegistries, EngineError> {
    let variables = crate::runtime_snapshot::ModelVariables::of(&config.runtime);
    let default_agent = config.runtime.agents.default_agent();
    let shared = Arc::new(AgentRegistry::resolve(
        &config.agent_registry(),
        models,
        None,
        runtime_agents,
        variables,
        default_agent.clone(),
    )?);
    let presets = config
        .agent_preset_registries()
        .into_iter()
        .map(|(name, authored)| {
            AgentRegistry::resolve(
                &authored,
                models,
                Some(name.clone()),
                runtime_agents,
                variables,
                default_agent.clone(),
            )
            .map(|registry| (name, Arc::new(registry)))
        })
        .collect::<Result<_, _>>()?;
    Ok((shared, presets))
}

#[cfg(test)]
mod context_token_estimator_tests {
    use super::ContextTokenEstimator;

    #[test]
    fn learns_from_committed_usage() {
        let mut estimator = ContextTokenEstimator::default();
        estimator.record_committed_turn(200, Some(50));

        assert_eq!(estimator.tokens_per_byte, 0.25);
        assert_eq!(estimator.estimated_context_tokens(40), Some(10));
        assert_eq!(estimator.estimated_context_tokens(41), Some(11));
    }

    #[test]
    fn skips_degenerate_ratio_updates() {
        let mut estimator = ContextTokenEstimator {
            tokens_per_byte: 0.5,
        };
        estimator.record_committed_turn(0, Some(20));
        assert_eq!(estimator.tokens_per_byte, 0.5);

        estimator.record_committed_turn(100, None);
        assert_eq!(estimator.tokens_per_byte, 0.5);

        estimator.record_committed_turn(100, Some(0));
        assert_eq!(estimator.tokens_per_byte, 0.5);
    }
}
