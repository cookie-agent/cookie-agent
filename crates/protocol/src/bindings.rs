//! The method registry of every surface and the JSON Schema export the
//! compatibility checker (`crates/protocol/scripts/check-compat.py`) reads,
//! as specified in `docs/site/specs/compatibility-v1.md` section 7.

use std::{collections::BTreeMap, fmt, fs, path::Path};

use schemars::{JsonSchema, Schema, schema_for};
use serde::Serialize;
use serde_json::{Value, json};

use crate::runtime_protocol::{
    AgentExitedParams, AgentMcpBridgeRef, AgentMcpClosedParams, AgentMcpOpenResult, AgentMcpParams,
    AgentMessageParams, AgentRef, AgentsChangedParams, CapabilityOpenParams, CapabilityOpenResult,
    CapabilityToolsChangedParams, ContextParams, ContextResult, DetachableParams, Empty,
    ExecuteParams, ExecuteResult, InitializeParams, InitializeResult, McpChangedParams,
    McpReconnectParams, Outcome, PrepareParams, PrepareResult, PreparedRef,
    RuntimeMcpChangedParams, RuntimeMcpReconnectParams, RuntimeSettingsChangedParams, RuntimeTools,
    ToolProgressParams, method as runtime_method,
};
use crate::*;

#[derive(Debug)]
pub enum BindingExportError {
    Arguments(String),
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for BindingExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments(error) => {
                write!(formatter, "invalid binding generator arguments: {error}")
            }
            Self::Io(error) => write!(formatter, "binding output failed: {error}"),
            Self::Json(error) => write!(formatter, "schema encoding failed: {error}"),
        }
    }
}

impl std::error::Error for BindingExportError {}

impl From<std::io::Error> for BindingExportError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<serde_json::Error> for BindingExportError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

/// One versioned contract, plus the session events the client and plugin
/// protocols both deliver.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Client,
    Plugin,
    Runtime,
    /// The `EventPayload` set. It has no version of its own: a change to it
    /// changes the client and plugin protocols, and its baseline records
    /// the client version.
    Events,
}

impl Surface {
    pub const ALL: [Self; 4] = [Self::Client, Self::Plugin, Self::Runtime, Self::Events];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Plugin => "plugin",
            Self::Runtime => "runtime",
            Self::Events => "events",
        }
    }

    /// The version this build speaks on the surface.
    #[must_use]
    pub const fn version(self) -> SemVer {
        match self {
            Self::Client | Self::Events => CLIENT_PROTOCOL_VERSION,
            Self::Plugin => PLUGIN_PROTOCOL_VERSION,
            Self::Runtime => RUNTIME_PROTOCOL_VERSION,
        }
    }
}

/// Which way a message goes, seen from the daemon. Params sent to the
/// daemon are commands, except declarations ([`Role`]); params it sends, and
/// every result, are information.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    ToDaemon,
    FromDaemon,
}

/// What a message's params are to their receiver
/// (`docs/site/specs/compatibility-v1.md` section 2): a command asks it to
/// act and refuses what it does not know; information, such as a peer's
/// declaration of its tools, ignores it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Command,
    Information,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Request,
    Notification,
}

/// A schema root: a Rust type named by its file in a baseline.
#[derive(Clone, Copy)]
pub struct RootType {
    pub name: &'static str,
    pub schema: fn() -> Schema,
}

impl fmt::Debug for RootType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name)
    }
}

fn schema_of<T: JsonSchema>() -> Schema {
    schema_for!(T)
}

/// One method or notification of a surface.
#[derive(Clone, Copy, Debug)]
pub struct MethodSpec {
    pub method: &'static str,
    pub direction: Direction,
    pub kind: MessageKind,
    pub params: RootType,
    /// What the params are to their receiver.
    pub params_role: Role,
    /// The result of a request; `None` for a notification.
    pub result: Option<RootType>,
}

macro_rules! root {
    ($type:ident) => {
        RootType {
            name: stringify!($type),
            schema: schema_of::<$type>,
        }
    };
}

const fn params_role(direction: Direction) -> Role {
    match direction {
        Direction::ToDaemon => Role::Command,
        Direction::FromDaemon => Role::Information,
    }
}

macro_rules! request {
    ($method:expr, $direction:ident, $params:ident => $result:ident) => {
        MethodSpec {
            method: $method,
            direction: Direction::$direction,
            kind: MessageKind::Request,
            params: root!($params),
            params_role: params_role(Direction::$direction),
            result: Some(root!($result)),
        }
    };
}

macro_rules! notification {
    ($method:expr, $direction:ident, $params:ident) => {
        MethodSpec {
            method: $method,
            direction: Direction::$direction,
            kind: MessageKind::Notification,
            params: root!($params),
            params_role: params_role(Direction::$direction),
            result: None,
        }
    };
}

/// A notification a peer sends the daemon that declares what it has, such as
/// its tools: information, though it goes to the daemon.
macro_rules! declaration {
    ($method:expr, $params:ident) => {
        MethodSpec {
            method: $method,
            direction: Direction::ToDaemon,
            kind: MessageKind::Notification,
            params: root!($params),
            params_role: Role::Information,
            result: None,
        }
    };
}

/// The `tool/*` messages: shared by the plugin and runtime protocols.
fn tool_methods() -> Vec<MethodSpec> {
    vec![
        request!(runtime_method::TOOL_PREPARE, FromDaemon, PrepareParams => PrepareResult),
        request!(runtime_method::TOOL_REVALIDATE, FromDaemon, PreparedRef => Outcome),
        request!(runtime_method::TOOL_EXECUTE, FromDaemon, ExecuteParams => ExecuteResult),
        notification!(runtime_method::TOOL_RELEASE, FromDaemon, PreparedRef),
        notification!(runtime_method::TOOL_CANCEL, FromDaemon, PreparedRef),
        request!(runtime_method::TOOL_PROGRESS, ToDaemon, ToolProgressParams => Outcome),
        notification!(runtime_method::TOOL_DETACHABLE, ToDaemon, DetachableParams),
    ]
}

/// The `producer/*` messages: shared by the plugin and runtime protocols.
fn producer_methods() -> Vec<MethodSpec> {
    vec![
        request!(PLUGIN_PRODUCER_REGISTER_METHOD, ToDaemon, ProducerRegisterParams => ProducerRegisterResult),
        request!(PLUGIN_PRODUCER_SEND_METHOD, ToDaemon, ProducerSendParams => ProducerSendResult),
        request!(PLUGIN_PRODUCER_UNREGISTER_METHOD, ToDaemon, ProducerUnregisterParams => ProducerUnregisterResult),
        request!(PLUGIN_PRODUCER_DISCARD_METHOD, ToDaemon, ProducerDiscardParams => ProducerDiscardResult),
        notification!(
            PLUGIN_RECOVERY_START_METHOD,
            FromDaemon,
            RecoveryStartParams
        ),
        request!(PLUGIN_RECOVERY_COMPLETE_METHOD, ToDaemon, RecoveryCompleteParams => RecoveryCompleteResult),
    ]
}

/// Every method and notification of `surface`, in name order.
#[must_use]
pub fn surface_methods(surface: Surface) -> Vec<MethodSpec> {
    let mut methods = match surface {
        Surface::Client => client_methods(),
        Surface::Plugin => {
            let mut methods = plugin_methods();
            methods.extend(tool_methods());
            methods.extend(producer_methods());
            methods
        }
        Surface::Runtime => {
            let mut methods = runtime_methods();
            methods.extend(tool_methods());
            methods.extend(producer_methods());
            methods
        }
        Surface::Events => Vec::new(),
    };
    methods.sort_by_key(|method| method.method);
    methods
}

/// The roots of `surface` that no method carries but that flow as
/// information: the stored event set. Error data is described by the error
/// registry (`crate::errors`), not by schemas.
#[must_use]
pub fn surface_information_roots(surface: Surface) -> Vec<RootType> {
    match surface {
        Surface::Client => Vec::new(),
        // A reader accepts an event type it does not know (skipping it, or
        // opening the log render-only), so the set is open: a new type is an
        // addition.
        Surface::Events => vec![
            RootType {
                name: "EventPayload",
                schema: schema_of::<Open<EventPayload>>,
            },
            root!(StoredEvent),
        ],
        Surface::Plugin | Surface::Runtime => Vec::new(),
    }
}

#[allow(clippy::too_many_lines)]
fn client_methods() -> Vec<MethodSpec> {
    vec![
        request!("handshake", ToDaemon, ClientHello => ServerHello),
        request!("session.create", ToDaemon, SessionCreateParams => SessionCreateResult),
        request!("session.list", ToDaemon, SessionListParams => SessionListResult),
        request!("session.get", ToDaemon, SessionGetParams => SessionGetResult),
        request!(SESSION_GOAL_GET_METHOD, ToDaemon, SessionGoalGetParams => SessionGoalGetResult),
        request!(SESSION_GOAL_SET_METHOD, ToDaemon, SessionGoalSetParams => SessionGoalSetResult),
        request!(SESSION_GOAL_LIFECYCLE_METHOD, ToDaemon, SessionGoalLifecycleParams => SessionGoalLifecycleResult),
        request!(SESSION_PRODUCERS_METHOD, ToDaemon, SessionProducersParams => SessionProducersResult),
        request!("session.usage", ToDaemon, SessionUsageParams => SessionUsageResult),
        request!("session.tree_usage", ToDaemon, SessionUsageParams => SessionTreeUsageResult),
        request!("session.children", ToDaemon, SessionChildrenParams => SessionChildrenResult),
        request!("session.tree", ToDaemon, SessionTreeParams => SessionTreeResult),
        request!("session.resume", ToDaemon, SessionResumeParams => SessionResumeResult),
        request!("session.rename", ToDaemon, SessionRenameParams => SessionRenameResult),
        request!("session.set_permission_mode", ToDaemon, SessionSetPermissionModeParams => SessionSetPermissionModeResult),
        request!("session.permission.get", ToDaemon, SessionPermissionGetParams => SessionPermissionGetResult),
        request!("session.permission.set", ToDaemon, SessionPermissionSetParams => SessionPermissionMutationResult),
        request!("session.permission.clear", ToDaemon, SessionPermissionClearParams => SessionPermissionMutationResult),
        request!("session.permission.preview", ToDaemon, SessionPermissionPreviewParams => SessionPermissionPreviewResult),
        request!("skills.list", ToDaemon, SkillsListParams => SkillsListResult),
        request!("skills.get", ToDaemon, SkillsGetParams => SkillsGetResult),
        request!(SKILLS_UPDATE_METHOD, ToDaemon, SkillsUpdateParams => SkillsUpdateResult),
        request!("session.compact", ToDaemon, SessionCompactParams => SessionCompactResult),
        request!("session.revert", ToDaemon, SessionRevertParams => SessionRevertResult),
        request!(SESSION_DELETE_METHOD, ToDaemon, SessionDeleteParams => SessionDeleteResult),
        request!(SESSION_COMPACT_CANCEL_METHOD, ToDaemon, SessionCompactCancelParams => SessionCompactCancelResult),
        request!(SESSION_HELD_RESUME_METHOD, ToDaemon, SessionHeldResumeParams => SessionHeldResumeResult),
        request!("session.fork", ToDaemon, SessionForkParams => SessionForkResult),
        request!("run.start", ToDaemon, RunStartParams => RunStartResult),
        request!("run.steer", ToDaemon, RunSteerParams => RunSteerResult),
        request!(SESSION_SELECT_MODEL_METHOD, ToDaemon, SessionSelectModelParams => SessionSelectModelResult),
        request!(EVENTS_UNSUBSCRIBE_METHOD, ToDaemon, EventsUnsubscribeParams => EventsUnsubscribeResult),
        request!(SESSION_SUBMIT_INPUT_METHOD, ToDaemon, SessionSubmitInputParams => SessionSubmitInputResult),
        request!(SESSION_RECALL_INPUT_METHOD, ToDaemon, SessionRecallInputParams => SessionRecallInputResult),
        request!("run.cancel", ToDaemon, RunCancelParams => RunCancelResult),
        request!("events.subscribe", ToDaemon, EventsSubscribeParams => EventsSubscribeResult),
        request!("approval.respond", ToDaemon, ApprovalRespondParams => ApprovalRespondResult),
        request!("approval.list", ToDaemon, ApprovalListParams => ApprovalListResult),
        request!(ELICITATION_RESPOND_METHOD, ToDaemon, ElicitationRespondParams => ElicitationRespondResult),
        request!("mcp.auth.begin", ToDaemon, McpAuthBeginParams => McpAuthBeginResult),
        request!("mcp.auth.complete", ToDaemon, McpAuthCompleteParams => McpAuthCompleteResult),
        request!("mcp.auth.cancel", ToDaemon, McpAuthCancelParams => McpAuthCancelResult),
        request!(SESSION_RUNTIME_ATTACH_METHOD, ToDaemon, SessionRuntimeAttachParams => SessionRuntimeAttachResult),
        request!(SESSION_RUNTIME_DETACH_METHOD, ToDaemon, SessionRuntimeDetachParams => SessionRuntimeDetachResult),
        request!(RUNTIMES_LIST_METHOD, ToDaemon, RuntimesListParams => RuntimesListResult),
        request!(RUNTIMES_RECONNECT_METHOD, ToDaemon, RuntimesReconnectParams => RuntimesReconnectResult),
        request!(RUNTIMES_AGENTS_LIST_METHOD, ToDaemon, RuntimesAgentsListParams => RuntimesAgentsResult),
        request!(RUNTIMES_AGENTS_SET_METHOD, ToDaemon, RuntimesAgentsSetParams => RuntimesAgentsResult),
        request!(RUNTIMES_AGENTS_REMOVE_METHOD, ToDaemon, RuntimesAgentsRemoveParams => RuntimesAgentsResult),
        request!(RUNTIMES_AGENTS_RENAME_METHOD, ToDaemon, RuntimesAgentsRenameParams => RuntimesAgentsRenameResult),
        request!(MACHINES_LIST_METHOD, ToDaemon, MachinesListParams => MachinesListResult),
        request!(MACHINES_RENAME_METHOD, ToDaemon, MachinesRenameParams => MachinesListResult),
        request!(MACHINES_ASSIGN_METHOD, ToDaemon, MachinesAssignParams => MachinesListResult),
        request!(MACHINES_UNASSIGN_METHOD, ToDaemon, MachinesUnassignParams => MachinesListResult),
        request!(MACHINES_MERGE_METHOD, ToDaemon, MachinesMergeParams => MachinesListResult),
        request!(MACHINES_FORGET_METHOD, ToDaemon, MachinesForgetParams => MachinesListResult),
        request!(MACHINES_FORGET_RUNTIME_METHOD, ToDaemon, MachinesForgetRuntimeParams => MachinesListResult),
        request!(MEMORY_LIST_METHOD, ToDaemon, MemoryListParams => MemoryListResult),
        request!(MEMORY_READ_METHOD, ToDaemon, MemoryReadParams => MemoryReadResult),
        request!(MEMORY_WRITE_METHOD, ToDaemon, MemoryWriteParams => MemoryWriteResult),
        request!(MEMORY_DELETE_METHOD, ToDaemon, MemoryDeleteParams => MemoryDeleteResult),
        request!(PROJECTS_LIST_METHOD, ToDaemon, ProjectsListParams => ProjectsListResult),
        request!(PROJECT_GET_METHOD, ToDaemon, ProjectGetParams => ProjectGetResult),
        request!(PROJECT_CREATE_METHOD, ToDaemon, ProjectCreateParams => ProjectResult),
        request!(PROJECT_UPDATE_METHOD, ToDaemon, ProjectUpdateParams => ProjectResult),
        request!(PROJECT_DELETE_METHOD, ToDaemon, ProjectDeleteParams => ProjectDeleteResult),
        request!(SESSION_PROJECT_SET_METHOD, ToDaemon, SessionProjectSetParams => SessionProjectSetResult),
        request!(SESSION_AGENT_OPTION_SET_METHOD, ToDaemon, SessionAgentOptionSetParams => SessionAgentOptionSetResult),
        request!(CRONS_LIST_METHOD, ToDaemon, CronsListParams => CronsListResult),
        request!(CRONS_CREATE_METHOD, ToDaemon, CronCreateParams => CronJobResult),
        request!(CRONS_UPDATE_METHOD, ToDaemon, CronUpdateParams => CronJobResult),
        request!(CRONS_DELETE_METHOD, ToDaemon, CronDeleteParams => CronDeleteResult),
        request!(CRONS_RUN_METHOD, ToDaemon, CronRunParams => CronRunResult),
        request!(NOTIFICATIONS_GET_METHOD, ToDaemon, NotificationsGetParams => NotificationsGetResult),
        request!(NOTIFICATIONS_SUBSCRIBE_METHOD, ToDaemon, NotificationsSubscribeParams => NotificationsSubscribeResult),
        request!(NOTIFICATIONS_UNSUBSCRIBE_METHOD, ToDaemon, NotificationsUnsubscribeParams => NotificationsUnsubscribeResult),
        request!(NOTIFICATIONS_TEST_METHOD, ToDaemon, NotificationsTestParams => NotificationsTestResult),
        request!(CLIENT_PRESENCE_METHOD, ToDaemon, ClientPresenceParams => ClientPresenceResult),
        request!(PLUGINS_LIST_METHOD, ToDaemon, PluginsListParams => PluginsListResult),
        request!(PLUGINS_RESTART_METHOD, ToDaemon, PluginsRestartParams => PluginsRestartResult),
        request!(CONFIG_RELOAD_METHOD, ToDaemon, ConfigReloadParams => ConfigReloadResult),
        request!(CONFIG_GET_METHOD, ToDaemon, ConfigGetParams => ConfigGetResult),
        request!(CONFIG_APPLY_METHOD, ToDaemon, ConfigApplyParams => ConfigApplyResult),
        request!(DAEMON_RESTART_METHOD, ToDaemon, DaemonRestartParams => DaemonRestartResult),
        request!(ONBOARDING_GET_METHOD, ToDaemon, OnboardingGetParams => OnboardingStatus),
        request!(ONBOARDING_PREVIEW_METHOD, ToDaemon, OnboardingPreviewParams => OnboardingPreviewResult),
        request!(ONBOARDING_RUN_METHOD, ToDaemon, OnboardingRunParams => OnboardingRunResult),
        request!(RUN_TOOL_BACKGROUND_METHOD, ToDaemon, RunToolBackgroundParams => RunToolBackgroundResult),
        request!(SESSION_BACKGROUND_LIST_METHOD, ToDaemon, SessionBackgroundListParams => SessionBackgroundListResult),
        request!(SESSION_BACKGROUND_STOP_METHOD, ToDaemon, SessionBackgroundStopParams => SessionBackgroundStopResult),
        request!("mcp.server.list", ToDaemon, McpServerListParams => McpServerListResult),
        request!("mcp.server.add", ToDaemon, McpServerAddParams => McpServerMutationResult),
        request!("mcp.server.edit", ToDaemon, McpServerEditParams => McpServerMutationResult),
        request!("mcp.server.remove", ToDaemon, McpServerNameParams => McpServerMutationResult),
        request!("mcp.server.set_enabled", ToDaemon, McpServerSetEnabledParams => McpServerMutationResult),
        request!(MCP_SERVER_RENAME_METHOD, ToDaemon, McpServerRenameParams => McpServerMutationResult),
        request!("mcp.server.reconnect", ToDaemon, McpServerReconnectParams => McpServerMutationResult),
        request!("mcp.server.persist", ToDaemon, McpServerPersistParams => McpServerMutationResult),
        request!(ARTIFACT_UPLOAD_BEGIN_METHOD, ToDaemon, ArtifactUploadBeginParams => ArtifactUploadBeginResult),
        request!(ARTIFACT_UPLOAD_CHUNK_METHOD, ToDaemon, ArtifactUploadChunkParams => ArtifactUploadChunkResult),
        request!(ARTIFACT_UPLOAD_FINISH_METHOD, ToDaemon, ArtifactUploadFinishParams => ArtifactUploadFinishResult),
        request!(ARTIFACT_UPLOAD_CANCEL_METHOD, ToDaemon, ArtifactUploadCancelParams => ArtifactUploadCancelResult),
        request!(ARTIFACT_READ_METHOD, ToDaemon, ArtifactReadParams => ArtifactReadResult),
        request!(RUNTIME_SNAPSHOT_GET_METHOD, ToDaemon, RuntimeSnapshotGetParams => RuntimeSnapshotResult),
        request!(PROVIDER_CONNECT_METHOD, ToDaemon, ProviderConnectParams => ProviderConnectResult),
        request!(PROVIDER_DISCONNECT_METHOD, ToDaemon, ProviderDisconnectParams => ProviderDisconnectResult),
        notification!("events.subscription", FromDaemon, EventSubscriptionMessage),
        notification!("events.plugin", FromDaemon, ExtensionBusEventParams),
        notification!(
            RUNTIME_CHANGED_METHOD,
            FromDaemon,
            RuntimeChangedNotification
        ),
        notification!(
            SESSIONS_CHANGED_METHOD,
            FromDaemon,
            SessionsChangedNotification
        ),
        notification!(MCP_CHANGED_METHOD, FromDaemon, McpChangedNotification),
        notification!(
            RUNTIMES_CHANGED_METHOD,
            FromDaemon,
            RuntimesChangedNotification
        ),
        notification!(
            BACKGROUND_CHANGED_METHOD,
            FromDaemon,
            BackgroundChangedNotification
        ),
        notification!(
            PLUGINS_CHANGED_METHOD,
            FromDaemon,
            PluginsChangedNotification
        ),
        notification!(
            MACHINES_CHANGED_METHOD,
            FromDaemon,
            MachinesChangedNotification
        ),
        notification!(MEMORY_CHANGED_METHOD, FromDaemon, MemoryChangedNotification),
        notification!(
            PROJECTS_CHANGED_METHOD,
            FromDaemon,
            ProjectsChangedNotification
        ),
        notification!(CRONS_CHANGED_METHOD, FromDaemon, CronsChangedNotification),
        notification!(
            NOTIFICATIONS_CHANGED_METHOD,
            FromDaemon,
            NotificationsChangedNotification
        ),
        notification!(
            DAEMON_RESTARTING_METHOD,
            FromDaemon,
            DaemonRestartingNotification
        ),
        notification!(ONBOARDING_CHANGED_METHOD, FromDaemon, OnboardingStatus),
    ]
}

fn plugin_methods() -> Vec<MethodSpec> {
    vec![
        request!(PLUGIN_INITIALIZE_METHOD, FromDaemon, ExtensionInitializeParams => ExtensionInitializeResult),
        notification!(PLUGIN_SHUTDOWN_METHOD, FromDaemon, ExtensionShutdownParams),
        notification!(PLUGIN_EVENT_METHOD, FromDaemon, ExtensionEventParams),
        notification!(PLUGIN_BUS_EVENT_METHOD, FromDaemon, ExtensionBusEventParams),
        notification!(PLUGIN_EMIT_METHOD, ToDaemon, ExtensionEmitParams),
        notification!(
            PLUGIN_EMIT_RESULT_METHOD,
            FromDaemon,
            ExtensionEmitResultParams
        ),
        declaration!(runtime_method::TOOLS_CHANGED, RuntimeTools),
        request!(PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD, FromDaemon, ExtensionToolBeforeCallParams => ExtensionToolBeforeCallResult),
        request!(PLUGIN_INTERCEPT_TOOL_AFTER_RESULT_METHOD, FromDaemon, ExtensionToolAfterResultParams => ExtensionToolAfterResultResult),
        request!(PLUGIN_INTERCEPT_AGENT_BEFORE_START_METHOD, FromDaemon, ExtensionAgentBeforeStartParams => ExtensionAgentBeforeStartResult),
        request!(PLUGIN_INTERCEPT_SESSION_BEFORE_COMPACT_METHOD, FromDaemon, ExtensionSessionBeforeCompactParams => ExtensionSessionBeforeCompactResult),
        request!(PLUGIN_INTERCEPT_USER_BEFORE_INPUT_METHOD, FromDaemon, ExtensionUserBeforeInputParams => ExtensionUserBeforeInputResult),
        request!(PLUGIN_INTERCEPT_MODEL_BEFORE_REQUEST_METHOD, FromDaemon, ExtensionModelBeforeRequestParams => ExtensionModelBeforeRequestResult),
        request!(PLUGIN_INTERCEPT_MESSAGE_END_METHOD, FromDaemon, ExtensionMessageEndParams => ExtensionMessageEndResult),
        request!(PLUGIN_INTERCEPT_MODEL_BEFORE_SELECT_METHOD, FromDaemon, ExtensionModelBeforeSelectParams => ExtensionAllowBlockResult),
        request!(PLUGIN_INTERCEPT_SESSION_BEFORE_FORK_METHOD, FromDaemon, ExtensionSessionBeforeForkParams => ExtensionAllowBlockResult),
        request!(PLUGIN_INTERCEPT_SESSION_BEFORE_REVERT_METHOD, FromDaemon, ExtensionSessionBeforeRevertParams => ExtensionSessionBeforeRevertResult),
    ]
}

fn runtime_methods() -> Vec<MethodSpec> {
    vec![
        request!(runtime_method::INITIALIZE, FromDaemon, InitializeParams => InitializeResult),
        request!(runtime_method::CAPABILITY_OPEN, FromDaemon, CapabilityOpenParams => CapabilityOpenResult),
        declaration!(
            runtime_method::CAPABILITY_TOOLS_CHANGED,
            CapabilityToolsChangedParams
        ),
        request!(runtime_method::CONTEXT, FromDaemon, ContextParams => ContextResult),
        request!(runtime_method::SHUTDOWN, FromDaemon, Empty => Empty),
        request!(runtime_method::PING, FromDaemon, Empty => Empty),
        notification!(
            runtime_method::SETTINGS_CHANGED,
            FromDaemon,
            RuntimeSettingsChangedParams
        ),
        declaration!(runtime_method::MCP_CHANGED, McpChangedParams),
        request!(runtime_method::MCP_RECONNECT, FromDaemon, McpReconnectParams => Outcome),
        declaration!(runtime_method::TOOLS_CHANGED, RuntimeTools),
        declaration!(runtime_method::RUNTIME_MCP_CHANGED, RuntimeMcpChangedParams),
        request!(runtime_method::RUNTIME_MCP_RECONNECT, FromDaemon, RuntimeMcpReconnectParams => Outcome),
        declaration!(runtime_method::AGENTS_CHANGED, AgentsChangedParams),
        request!(runtime_method::AGENT_START, FromDaemon, AgentRef => Empty),
        request!(runtime_method::AGENT_STOP, FromDaemon, AgentRef => Empty),
        notification!(
            runtime_method::AGENT_MESSAGE,
            FromDaemon,
            AgentMessageParams
        ),
        declaration!(runtime_method::AGENT_MESSAGE, AgentMessageParams),
        declaration!(runtime_method::AGENT_EXITED, AgentExitedParams),
        request!(runtime_method::AGENT_MCP_OPEN, FromDaemon, AgentRef => AgentMcpOpenResult),
        notification!(
            runtime_method::AGENT_MCP_CLOSE,
            FromDaemon,
            AgentMcpBridgeRef
        ),
        notification!(runtime_method::AGENT_MCP, FromDaemon, AgentMcpParams),
        declaration!(runtime_method::AGENT_MCP, AgentMcpParams),
        declaration!(runtime_method::AGENT_MCP_CLOSED, AgentMcpClosedParams),
    ]
}

/// Every schema root of `surface` by name: the params and results of its
/// methods and its information roots.
#[must_use]
pub fn surface_roots(surface: Surface) -> BTreeMap<&'static str, RootType> {
    let methods = surface_methods(surface);
    methods
        .iter()
        .flat_map(|method| std::iter::once(method.params).chain(method.result))
        .chain(surface_information_roots(surface))
        .map(|root| (root.name, root))
        .collect()
}

/// The `methods.json` of `surface`.
#[must_use]
pub fn methods_document(surface: Surface) -> Value {
    let methods = surface_methods(surface)
        .into_iter()
        .map(|method| {
            json!({
                "method": method.method,
                "direction": method.direction,
                "kind": method.kind,
                "params": method.params.name,
                "params_role": method.params_role,
                "result": method.result.map(|result| result.name),
            })
        })
        .collect::<Vec<_>>();
    let information = surface_information_roots(surface)
        .into_iter()
        .map(|root| root.name)
        .collect::<Vec<_>>();
    json!({
        "surface": surface.name(),
        "methods": methods,
        "information": information,
    })
}

/// Every root of every surface, as `(file name, schema)`.
#[must_use]
pub fn json_schema_documents() -> Vec<(String, Schema)> {
    let mut roots = BTreeMap::new();
    for surface in Surface::ALL {
        roots.extend(surface_roots(surface));
    }
    roots
        .into_values()
        .map(|root| (format!("{}.schema.json", root.name), (root.schema)()))
        .collect()
}

/// Writes the compatibility set: `<output>/<surface>/version`,
/// `methods.json`, and `schemas/<Root>.schema.json` for every surface, the
/// layout of `crates/protocol/compat/`. Each surface's directory is
/// replaced; anything else in `output` is left alone.
pub fn export_compat_set(output: &Path) -> Result<(), BindingExportError> {
    for surface in Surface::ALL {
        let directory = output.join(surface.name());
        if directory.exists() {
            fs::remove_dir_all(&directory)?;
        }
        let schemas = directory.join("schemas");
        fs::create_dir_all(&schemas)?;
        fs::write(
            directory.join("version"),
            format!("{}\n", surface.version()),
        )?;
        fs::write(
            directory.join("methods.json"),
            pretty(&methods_document(surface))?,
        )?;
        for (name, root) in surface_roots(surface) {
            let schema = serde_json::to_value((root.schema)())?;
            fs::write(
                schemas.join(format!("{name}.schema.json")),
                pretty(&schema)?,
            )?;
        }
    }
    Ok(())
}

fn pretty(value: &Value) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(value).map(|mut text| {
        text.push('\n');
        text
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn method_names_are_unique_per_surface_and_direction() {
        for surface in Surface::ALL {
            let mut seen = BTreeSet::new();
            for method in surface_methods(surface) {
                assert!(
                    seen.insert((method.method, method.direction == Direction::ToDaemon)),
                    "{surface:?}: {}",
                    method.method
                );
                assert_eq!(
                    method.result.is_some(),
                    method.kind == MessageKind::Request,
                    "{}",
                    method.method
                );
            }
        }
    }

    #[test]
    fn a_root_name_names_one_schema_per_surface() {
        for surface in Surface::ALL {
            let methods = surface_methods(surface);
            let mut schemas = BTreeMap::new();
            for root in methods
                .iter()
                .flat_map(|method| std::iter::once(method.params).chain(method.result))
                .chain(surface_information_roots(surface))
            {
                let schema = serde_json::to_value((root.schema)()).unwrap();
                if let Some(previous) = schemas.insert(root.name, schema.clone()) {
                    assert_eq!(previous, schema, "{surface:?}: {}", root.name);
                }
            }
        }
    }

    #[test]
    fn the_shared_messages_are_on_both_surfaces_that_carry_them() {
        let names = |surface| {
            surface_methods(surface)
                .into_iter()
                .map(|method| method.method)
                .collect::<BTreeSet<_>>()
        };
        let (plugin, runtime) = (names(Surface::Plugin), names(Surface::Runtime));
        for method in tool_methods().into_iter().chain(producer_methods()) {
            assert!(plugin.contains(method.method) && runtime.contains(method.method));
        }
        assert!(names(Surface::Client).contains(CONFIG_APPLY_METHOD));
        assert!(names(Surface::Client).contains(PROVIDER_CONNECT_METHOD));
        assert!(names(Surface::Client).contains(PLUGINS_RESTART_METHOD));
        assert!(names(Surface::Events).is_empty());
        let events = surface_roots(Surface::Events);
        assert!(events.contains_key("EventPayload"));
    }
}

#[cfg(test)]
mod reference_tests {
    use std::collections::BTreeSet;

    use super::*;

    /// The names the first table of a section of the protocol reference
    /// lists.
    fn documented(heading: &str) -> BTreeSet<String> {
        let reference = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/site/reference/protocol.md"
        ))
        .expect("protocol reference");
        let start = reference
            .find(&format!("\n## {heading}\n"))
            .unwrap_or_else(|| panic!("a {heading} section"));
        reference[start + 1..]
            .lines()
            .skip(1)
            .take_while(|line| !line.starts_with("## "))
            .skip_while(|line| !line.starts_with('|'))
            .take_while(|line| line.starts_with('|'))
            .filter_map(|line| line.strip_prefix("| `"))
            .filter_map(|line| line.split_once('`'))
            .map(|(name, _)| name.to_owned())
            .collect()
    }

    /// Every client method and notification of the registry has a row in
    /// the protocol reference, and every row names one of them.
    #[test]
    fn the_protocol_reference_lists_every_client_method() {
        let methods = surface_methods(Surface::Client);
        let names = |kind| {
            methods
                .iter()
                .filter(|method| method.kind == kind)
                .map(|method| method.method.to_owned())
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(documented("Methods"), names(MessageKind::Request));
        assert_eq!(
            documented("Notifications"),
            names(MessageKind::Notification)
        );
    }
}
