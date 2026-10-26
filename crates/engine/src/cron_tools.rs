//! The cron tools of the `global` runtime (`docs/site/specs/cron.md`
//! section 8): `cron_create`, `cron_list`, `cron_update`, `cron_delete`,
//! and `cron_run`, provider `builtin.cron`.
//!
//! Every tool but `cron_list` is checked under the `cron` action with the
//! target kind of each job it touches as a resource (`session`, `subagent`,
//! or `new`). `cron_list` declares no permission: it is offered with the
//! other cron tools, and it shows only the jobs whose kind the rules allow
//! or ask for. Targets are resolved at preparation, against the calling
//! session: `"self"`, a session handle below it, or a new-session template
//! whose defaults are the calling session's. An agent never names a
//! permission mode or a model.
//!
//! Like the memory tools, the provider is built with the `global` runtime,
//! before the engine exists, and holds the engine through a handle bound
//! once the engine is made.

use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use cookie_agent_protocol::{
    AgentId, ApprovalResourceSource, CronAgentActor, CronCreateParams, CronJob, CronJobId,
    CronMaxRuns, CronSchedule, CronTarget, CronTargetKind, CronTemplate, CronUpdateParams,
    PermissionAction, PermissionEffect, PermissionRule, PersistedToolResult as ToolResult,
    PreparedBindingLifetime, ProducerDeliveryMode, ProjectId, ProjectRuntime, RuntimeId, SessionId,
    SessionOrigin,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolCompletion, ToolConcurrency,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider,
    ToolResultTruncationPolicy, ToolSpec,
    authoring::{parse_args, prepared_operation, prepared_resource, safe_title},
};
use serde::{Deserialize, Serialize};

use crate::{
    Engine,
    runtime::{Inner, cron_api::CronRequester},
};

/// The provider's ID.
pub(crate) const PROVIDER_ID: &str = "builtin.cron";

pub(crate) const CREATE: &str = "cron_create";
pub(crate) const LIST: &str = "cron_list";
pub(crate) const UPDATE: &str = "cron_update";
pub(crate) const DELETE: &str = "cron_delete";
pub(crate) const RUN: &str = "cron_run";

const SCHEDULE_NOTES: &str = "Schedules are in the daemon host's local time. Each fire arrives marked as a <scheduled_job>, never as the user, and is not user consent. Prefer cron_update on an existing job to creating a near duplicate. max_runs defaults to 1, so a job fires once unless told otherwise; \"unlimited\" runs until the job is deleted or disabled.";

const TARGET_NOTES: &str = "`target`: \"self\" (the default) fires into this session; a subagent's handle (e.g. \"explore_1a2b3c4d\") fires into that subagent's session, which must be below this one; {\"new\": {...}} starts a new root session per fire, whose agent, runtimes, and project default to this session's. A new session's agent must be able to run as a root (call list_agents with for=\"cron\").";

fn create_description() -> String {
    format!(
        "Schedule a prompt: once after a delay ({{\"in\": \"20m\"}}) or on a five-field cron schedule ({{\"cron\": \"0 3 * * *\"}}). {TARGET_NOTES} `delivery` (in-session jobs): \"queue\" (default) waits for the current turn, \"steer\" joins it. {SCHEDULE_NOTES}"
    )
}

fn update_description() -> String {
    format!(
        "Change a cron job: any member of cron_create, and `enabled` to disable or re-enable it (enabling resets its failure count). A new schedule or target computes the next fire from now. {SCHEDULE_NOTES}"
    )
}

const LIST_DESCRIPTION: &str = "List the daemon's cron jobs your permissions let you see: id, name, schedule, next fire (local time and UTC), target, whether enabled, and the last fire's outcome. Pass `target` (session, subagent, or new) to list one kind.";
const DELETE_DESCRIPTION: &str = "Delete a cron job. Sessions its fires started stay.";
const RUN_DESCRIPTION: &str = "Run a cron job now. An in-session job fires into its session as a scheduled fire would. A new-session job runs as your subagent with the job's agent, rules, and runtimes and the prompt as its first input: in the foreground (default) the call waits for its first turn and returns the result; in the background it returns the subagent's handle at once and its turns arrive as <subagent_report>s. A run now counts toward neither max_runs nor the failure count.";

/// `schedule` as a tool takes it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum ScheduleArg {
    Cron(String),
    In(String),
}

impl From<ScheduleArg> for CronSchedule {
    fn from(schedule: ScheduleArg) -> Self {
        match schedule {
            ScheduleArg::Cron(expression) => Self::Cron(expression),
            ScheduleArg::In(delay) => Self::In(delay),
        }
    }
}

/// `target` as a tool takes it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum TargetArg {
    /// `"self"` or a session handle.
    Session(String),
    New {
        new: NewArg,
    },
}

impl TargetArg {
    /// `"self"`: the calling session.
    fn own_session() -> Self {
        Self::Session("self".into())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NewArg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permission_rules: Option<Vec<PermissionRule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtimes: Option<Vec<ProjectRuntime>>,
    /// A project's name or ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CreateArgs {
    prompt: String,
    schedule: ScheduleArg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_runs: Option<CronMaxRuns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// `"self"` when omitted (decided 2026-10-23).
    #[serde(default = "TargetArg::own_session")]
    target: TargetArg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delivery: Option<ProducerDeliveryMode>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    id: CronJobId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    schedule: Option<ScheduleArg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_runs: Option<CronMaxRuns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<TargetArg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delivery: Option<ProducerDeliveryMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IdArgs {
    id: CronJobId,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<CronTargetKind>,
}

fn schedule_schema() -> serde_json::Value {
    serde_json::json!({
        "description": "{\"cron\": \"<minute hour day-of-month month day-of-week>\"} in host local time, or {\"in\": \"<n>s|m|h|d\"} to fire once after a delay (1s to 365d)",
        "oneOf": [
            {"type": "object", "additionalProperties": false, "required": ["cron"],
             "properties": {"cron": {"type": "string"}}},
            {"type": "object", "additionalProperties": false, "required": ["in"],
             "properties": {"in": {"type": "string"}}}
        ]
    })
}

/// `description` says what an omitted `target` means.
fn target_schema(description: &str) -> serde_json::Value {
    serde_json::json!({
        "description": description,
        "oneOf": [
            {"type": "string"},
            {"type": "object", "additionalProperties": false, "required": ["new"],
             "properties": {"new": {
                "type": "object", "additionalProperties": false,
                "properties": {
                    "agent": {"type": "string", "description": "The agent each new session runs; this session's agent when omitted"},
                    "permission_rules": {"type": "array", "description": "Rules added to each new session, e.g. {\"action\": \"bash\", \"resource\": \"git *\", \"effect\": \"allow\"}",
                        "items": {"type": "object", "required": ["action", "resource", "effect"],
                            "properties": {
                                "action": {"type": "string"},
                                "runtime": {"type": "string"},
                                "resource": {"type": "string"},
                                "effect": {"type": "string", "enum": ["allow", "ask", "deny"]}
                            }}},
                    "runtimes": {"type": "array", "description": "Runtimes besides global, as list_runtimes names them; this session's when omitted",
                        "items": {"type": "object", "required": ["runtime_id"], "additionalProperties": false,
                            "properties": {
                                "runtime_id": {"type": "string"},
                                "capability": {"type": "string"},
                                "workspace": {"type": "string"}
                            }}},
                    "project": {"type": "string", "description": "A project's name; this session's project when omitted"}
                }
             }}}
        ]
    })
}

fn max_runs_schema() -> serde_json::Value {
    serde_json::json!({
        "description": "Cron schedules only: how many fires before the job finishes, default 1, or \"unlimited\"",
        "oneOf": [{"type": "integer", "minimum": 1}, {"type": "string", "enum": ["unlimited"]}]
    })
}

fn spec(
    name: &str,
    description: String,
    permission: Option<&str>,
    background: bool,
    parameters: serde_json::Value,
) -> ToolSpec {
    ToolSpec {
        background,
        background_default: false,
        collision: Default::default(),
        output: Default::default(),
        concurrency: if name == LIST {
            ToolConcurrency::Parallel
        } else {
            ToolConcurrency::default()
        },
        result_truncation: if name == RUN {
            ToolResultTruncationPolicy::OptOut
        } else {
            ToolResultTruncationPolicy::Bounded
        },
        name: name.into(),
        permission_name: permission.map(str::to_owned),
        description,
        parameters,
    }
}

/// The cron tools as the `global` runtime publishes them, built once:
/// they are published at every model step.
pub(crate) fn specs() -> Vec<ToolSpec> {
    static SPECS: std::sync::OnceLock<Vec<ToolSpec>> = std::sync::OnceLock::new();
    SPECS.get_or_init(build_specs).clone()
}

fn build_specs() -> Vec<ToolSpec> {
    let cron = Some(PermissionAction::CRON.as_str());
    let delivery = serde_json::json!({"type": "string", "enum": ["queue", "steer"]});
    let id =
        serde_json::json!({"type": "string", "description": "The job's ID, e.g. \"k3v9q2xa\""});
    vec![
        spec(
            CREATE,
            create_description(),
            cron,
            false,
            serde_json::json!({
                "type": "object", "additionalProperties": false,
                "required": ["prompt", "schedule"],
                "properties": {
                    "prompt": {"type": "string", "description": "What to do at each fire"},
                    "schedule": schedule_schema(),
                    "max_runs": max_runs_schema(),
                    "name": {"type": "string", "description": "A short name, at most 80 characters"},
                    "target": target_schema("\"self\" (the default), a subagent's handle, or {\"new\": {...}}"),
                    "delivery": delivery,
                }
            }),
        ),
        spec(
            LIST,
            LIST_DESCRIPTION.into(),
            None,
            false,
            serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "target": {"type": "string", "enum": ["session", "subagent", "new"]}
                }
            }),
        ),
        spec(
            UPDATE,
            update_description(),
            cron,
            false,
            serde_json::json!({
                "type": "object", "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": id,
                    "prompt": {"type": "string"},
                    "schedule": schedule_schema(),
                    "max_runs": max_runs_schema(),
                    "name": {"type": "string"},
                    "target": target_schema("\"self\", a subagent's handle, or {\"new\": {...}}; unchanged when omitted"),
                    "delivery": delivery,
                    "enabled": {"type": "boolean"},
                }
            }),
        ),
        spec(
            DELETE,
            DELETE_DESCRIPTION.into(),
            cron,
            false,
            serde_json::json!({
                "type": "object", "additionalProperties": false,
                "required": ["id"],
                "properties": {"id": id}
            }),
        ),
        spec(
            RUN,
            RUN_DESCRIPTION.into(),
            cron,
            true,
            serde_json::json!({
                "type": "object", "additionalProperties": false,
                "required": ["id"],
                "properties": {"id": id}
            }),
        ),
    ]
}

/// The cron tools, for the tool reference.
#[must_use]
pub fn cron_tool_specs() -> Vec<ToolSpec> {
    specs()
}

fn permission_name(tool: &str) -> Result<Option<&'static str>, ToolError> {
    match tool {
        LIST => Ok(None),
        CREATE | UPDATE | DELETE | RUN => Ok(Some("cron")),
        other => Err(ToolError::execution(format!(
            "the cron provider received another tool, `{other}`"
        ))),
    }
}

fn refused(message: impl Into<String>) -> ToolError {
    ToolError::execution(message.into())
}

/// What a prepared cron call does.
#[derive(Clone, Debug)]
enum CronCall {
    Create(CronCreateParams),
    List(Option<CronTargetKind>),
    Update(CronUpdateParams),
    Delete(CronJobId),
    Run(CronJobId),
}

/// The cron tools, holding the engine once it is bound.
#[derive(Default)]
pub(crate) struct CronTools {
    engine: OnceLock<Weak<Inner>>,
}

impl CronTools {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Gives the provider its engine. Later calls are ignored.
    pub(crate) fn bind(&self, engine: &Engine) {
        let _ = self.engine.set(Arc::downgrade(&engine.inner));
    }

    fn engine(&self) -> Result<Engine, ToolError> {
        self.engine
            .get()
            .and_then(Weak::upgrade)
            .map(|inner| Engine { inner })
            .ok_or_else(|| ToolError::execution("the engine is gone"))
    }
}

impl Engine {
    /// The agent of the calling run, as it creates or changes jobs.
    fn cron_agent(
        &self,
        session: SessionId,
        run: cookie_agent_protocol::RunId,
    ) -> Result<CronAgentActor, ToolError> {
        // A call is prepared within its run; the session's own agent
        // stands in for a run that is no longer active.
        let agent = match self.active_run_policy(run) {
            Some(policy) => policy.agent.agent.clone(),
            None => self
                .inner
                .store
                .get(session)
                .map_err(|error| refused(error.to_string()))?
                .creation_agent
                .agent
                .clone(),
        };
        Ok(CronAgentActor { agent, session })
    }

    /// The target an agent's `target` names (section 8): its own session,
    /// a session below it, or a new-session template with the calling
    /// session's agent, runtimes, and project as defaults. Anything else is
    /// refused whatever the rules say.
    fn resolve_tool_target(
        &self,
        caller: &CronAgentActor,
        target: TargetArg,
    ) -> Result<CronTarget, ToolError> {
        let projection = self
            .inner
            .store
            .get(caller.session)
            .map_err(|error| refused(error.to_string()))?;
        match target {
            TargetArg::Session(reference) if reference == "self" => {
                Ok(match projection.meta.origin {
                    SessionOrigin::Root => CronTarget::Session(caller.session),
                    SessionOrigin::Delegated { .. } => CronTarget::Subagent(caller.session),
                })
            }
            TargetArg::Session(reference) => self
                .resolve_descendant(caller.session, &reference)
                .map(CronTarget::Subagent)
                .map_err(|error| {
                    refused(format!(
                        "target `{reference}` is not a session below this one: {error}"
                    ))
                }),
            TargetArg::New { new } => {
                let attachments = self.attachments_of(&projection);
                let global = attachments
                    .iter()
                    .any(|attachment| attachment.runtime_id.as_str() == RuntimeId::GLOBAL);
                let runtimes = new.runtimes.unwrap_or_else(|| {
                    attachments
                        .iter()
                        .filter(|attachment| attachment.runtime_id.as_str() != RuntimeId::GLOBAL)
                        .map(|attachment| ProjectRuntime {
                            runtime_id: attachment.runtime_id.clone(),
                            capability: Some(attachment.capability.clone()),
                            workspace: attachment.workspace.clone(),
                        })
                        .collect()
                });
                let project = match new.project {
                    Some(name) => Some(self.project_by_name(&name)?),
                    None => crate::blocking::off_worker(|| self.session_project(caller.session))
                        .map(|project| project.id),
                };
                Ok(CronTarget::New(CronTemplate {
                    agent: new.agent.unwrap_or_else(|| caller.agent.clone()),
                    preset: projection.meta.creation_selection.preset.clone(),
                    model: None,
                    permission_rules: new.permission_rules.unwrap_or_default(),
                    runtimes: Some(runtimes),
                    global,
                    cwd: None,
                    project,
                }))
            }
        }
    }

    /// The project named `name`, or with ID `name`.
    fn project_by_name(&self, name: &str) -> Result<ProjectId, ToolError> {
        let projects = crate::blocking::off_worker(|| self.inner.projects.list())
            .map_err(|error| refused(error.to_string()))?;
        projects
            .into_iter()
            .find(|project| project.name.as_str() == name || project.id.as_str() == name)
            .map(|project| project.id)
            .ok_or_else(|| refused(format!("there is no project `{name}`")))
    }

    /// Whether the calling run's rules allow or ask for `cron` on `kind`.
    fn cron_kind_visible(
        &self,
        session: SessionId,
        run: cookie_agent_protocol::RunId,
        kind: CronTargetKind,
    ) -> bool {
        let Some(policy) = self.active_run_policy(run) else {
            return false;
        };
        let overlay = self
            .inner
            .store
            .get(session)
            .map(|projection| projection.permission_overlay.clone())
            .unwrap_or_default();
        kind_visible(&policy.agent, &overlay, kind)
    }

    /// How a target reads in a tool result: a session by its handle.
    fn target_text(&self, target: &CronTarget) -> String {
        let handle = |session: SessionId| {
            self.inner
                .store
                .summary(session)
                .ok()
                .and_then(|summary| summary.meta.short_id)
                .unwrap_or_else(|| session.to_string())
        };
        match target {
            CronTarget::Session(session) => format!("session {}", handle(*session)),
            CronTarget::Subagent(session) => format!("subagent {}", handle(*session)),
            CronTarget::New(template) => {
                let mut text = format!("new session (agent {}", template.agent);
                if let Some(project) = &template.project {
                    let name = self
                        .inner
                        .projects
                        .get(project)
                        .map_or_else(|_| project.to_string(), |project| project.name.to_string());
                    text.push_str(&format!(", project {name}"));
                }
                text.push(')');
                text
            }
        }
    }
}

/// Whether `agent`'s rules and `overlay` allow or ask for `cron` on jobs of
/// `kind`, so that `cron_list` shows them.
fn kind_visible(
    agent: &cookie_agent_protocol::AgentSnapshot,
    overlay: &cookie_agent_protocol::SessionPermissionOverlay,
    kind: CronTargetKind,
) -> bool {
    let (effect, _) = crate::permissions::effective_permission_with_overlay(
        agent,
        Some(overlay),
        &PermissionAction::CRON,
        kind.as_str(),
        crate::permissions::PermissionScope::runtime(RuntimeId::GLOBAL, None),
    );
    matches!(effect, PermissionEffect::Allow | PermissionEffect::Ask)
}

/// `at` in local time and UTC.
fn when(at: jiff::Timestamp) -> String {
    let local = at
        .to_zoned(crate::runtime::cron_api::host_zone())
        .strftime("%Y-%m-%d %H:%M:%S %Z")
        .to_string();
    format!("{local} ({at})")
}

fn schedule_text(job: &CronJob) -> String {
    match &job.schedule {
        CronSchedule::Cron(expression) => format!("cron `{expression}`, max_runs {}", job.max_runs),
        CronSchedule::At(at) => format!("once at {}", when(*at)),
        CronSchedule::In(delay) => format!("once in {delay}"),
    }
}

fn last_fire_text(job: &CronJob) -> String {
    use cookie_agent_protocol::CronFireOutcome;
    let Some(fire) = job.history.last() else {
        return "never fired".into();
    };
    let outcome = match fire.outcome.known() {
        Some(CronFireOutcome::Pending) => "being delivered".to_owned(),
        Some(CronFireOutcome::Delivered) => match &fire.run_end {
            Some(end) => format!("delivered, run {}", status_name(end.status)),
            None => "delivered".to_owned(),
        },
        Some(CronFireOutcome::Skipped { reason }) => format!("skipped ({})", skip_text(*reason)),
        Some(CronFireOutcome::Failed { error }) => format!("failed: {error}"),
        None => "unknown".to_owned(),
    };
    format!("{outcome} at {}", when(fire.fired_at))
}

fn status_name(status: cookie_agent_protocol::SessionStatus) -> &'static str {
    use cookie_agent_protocol::SessionStatus;
    match status {
        SessionStatus::Idle => "idle",
        SessionStatus::Running => "running",
        SessionStatus::Completed => "completed",
        SessionStatus::Failed => "failed",
        SessionStatus::Cancelled => "cancelled",
        SessionStatus::Interrupted => "interrupted",
        SessionStatus::Unknown => "unknown",
    }
}

fn skip_text(reason: cookie_agent_protocol::CronSkipReason) -> &'static str {
    use cookie_agent_protocol::CronSkipReason;
    match reason {
        CronSkipReason::PreviousFireNotDelivered => "previous fire not delivered",
        CronSkipReason::PreviousRunStillRunning => "previous run still running",
        CronSkipReason::DaemonWasDown => "daemon was down",
        CronSkipReason::Interrupted => "interrupted",
        CronSkipReason::FiringDisabled => "firing disabled",
        CronSkipReason::DaemonRestarting => "daemon restarting",
        CronSkipReason::SessionHeld => "session held",
        CronSkipReason::Unknown => "unknown",
    }
}

fn enabled_text(job: &CronJob) -> String {
    use cookie_agent_protocol::CronDisabledReason;
    if job.enabled {
        return "yes".into();
    }
    match job
        .disabled_reason
        .as_ref()
        .and_then(|reason| reason.known())
    {
        Some(CronDisabledReason::Finished) => "no (finished)".into(),
        Some(CronDisabledReason::Failed { failures }) => {
            format!("no (failed {failures} times in a row)")
        }
        Some(CronDisabledReason::Disabled { by }) => match by {
            cookie_agent_protocol::CronActor::User => "no (disabled by the user)".into(),
            cookie_agent_protocol::CronActor::Agent(agent) => {
                format!("no (disabled by agent {})", agent.agent)
            }
        },
        None => "no".into(),
    }
}

/// One job as a tool result shows it.
fn job_text(engine: &Engine, job: &CronJob) -> String {
    let label = job.name.clone().unwrap_or_else(|| {
        let line = job.prompt.lines().next().unwrap_or_default();
        line.chars().take(40).collect()
    });
    let next = job.next_fire.map_or_else(|| "none".to_owned(), when);
    format!(
        "{} \"{}\"\n  schedule: {}\n  target: {}\n  next fire: {next}\n  enabled: {}\n  last fire: {}",
        job.id,
        label,
        schedule_text(job),
        engine.target_text(&job.target),
        enabled_text(job),
        last_fire_text(job),
    )
}

fn text_result(title: &str, output: String) -> ToolCompletion {
    ToolCompletion::single(ToolResult {
        title: safe_title(title),
        display: crate::base_tools::markdown_display(&format!("```text\n{output}\n```")),
        output,
        background: None,
        retained_output: None,
        metadata: serde_json::json!({}),
        attachments: Vec::new(),
        additional_messages: Vec::new(),
    })
}

#[async_trait]
impl ToolProvider for CronTools {
    fn provider_id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(specs())
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        permission_name(tool_name)
    }

    fn get_permission_resource(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        // The resources are the target kinds, labelled at preparation.
        Ok((permission_name(name)?, None))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        permission_name(name)?;
        Ok(arguments
            .get("id")
            .or_else(|| arguments.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    fn presentation(&self, call: &ToolCall) -> cookie_agent_protocol::PreparedPresentation {
        let text = |member: &str| {
            call.arguments
                .get(member)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let id = text("id").unwrap_or_default();
        let title = match call.name.as_str() {
            CREATE => format!(
                "Schedule {}",
                text("name").unwrap_or_else(|| "a prompt".to_owned())
            ),
            LIST => "List cron jobs".to_owned(),
            UPDATE => format!("Change cron job {id}"),
            DELETE => format!("Delete cron job {id}"),
            RUN => format!("Run cron job {id} now"),
            _ => return cookie_agent_runtime_api::tool_title_only(&call.name),
        };
        let mut entries = Vec::new();
        if let Some(schedule) = call.arguments.get("schedule") {
            entries.push(("schedule", schedule.to_string()));
        }
        if let Some(target) = call.arguments.get("target") {
            entries.push((
                "target",
                target
                    .as_str()
                    .map_or_else(|| target.to_string(), str::to_owned),
            ));
        }
        if let Some(prompt) = text("prompt") {
            entries.push(("prompt", prompt));
        }
        crate::base_tools::base_presentation(
            &title,
            cookie_agent_protocol::ToolHint::GENERIC,
            &id,
            entries,
        )
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let engine = self.engine()?;
        permission_name(&call.name)?;
        let (call_kind, kinds) = match call.name.as_str() {
            CREATE => {
                let args: CreateArgs = parse_args(CREATE, call.arguments.clone())?;
                let caller = engine.cron_agent(ctx.session, ctx.run)?;
                let target = engine.resolve_tool_target(&caller, args.target)?;
                let kind = target.kind();
                (
                    CronCall::Create(CronCreateParams {
                        name: args.name,
                        prompt: args.prompt,
                        schedule: args.schedule.into(),
                        max_runs: args.max_runs,
                        target,
                        delivery: args.delivery,
                        permission_mode: None,
                        max_consecutive_failures: None,
                        enabled: None,
                    }),
                    vec![kind],
                )
            }
            LIST => {
                let args: ListArgs = parse_args(LIST, call.arguments.clone())?;
                (CronCall::List(args.target), Vec::new())
            }
            UPDATE => {
                let args: UpdateArgs = parse_args(UPDATE, call.arguments.clone())?;
                let job = engine
                    .cron_store()
                    .job(&args.id)
                    .ok_or_else(|| refused(format!("there is no cron job {}", args.id)))?;
                let mut kinds = vec![job.target.kind()];
                let target = match args.target {
                    Some(target) => {
                        let caller = engine.cron_agent(ctx.session, ctx.run)?;
                        let target = engine.resolve_tool_target(&caller, target)?;
                        if target.kind() != kinds[0] {
                            kinds.push(target.kind());
                        }
                        Some(target)
                    }
                    None => None,
                };
                let mut params = CronUpdateParams::new(args.id);
                params.name = args.name.map(Some);
                params.prompt = args.prompt;
                params.schedule = args.schedule.map(Into::into);
                params.max_runs = args.max_runs;
                params.target = target;
                params.delivery = args.delivery;
                params.enabled = args.enabled;
                (CronCall::Update(params), kinds)
            }
            DELETE | RUN => {
                let args: IdArgs = parse_args(&call.name, call.arguments.clone())?;
                let job = engine
                    .cron_store()
                    .job(&args.id)
                    .ok_or_else(|| refused(format!("there is no cron job {}", args.id)))?;
                let kind = job.target.kind();
                (
                    if call.name == DELETE {
                        CronCall::Delete(args.id)
                    } else {
                        CronCall::Run(args.id)
                    },
                    vec![kind],
                )
            }
            _ => unreachable!("checked by permission_name"),
        };
        let executor = Box::new(CronExecutor {
            engine,
            call: call_kind,
            call_id: call.id,
        });
        if kinds.is_empty() {
            return PreparedTool::new(
                prepared_operation(
                    &call.name,
                    &call.arguments,
                    vec![(PermissionAction::CRON, "list")],
                    vec![prepared_resource(
                        PermissionAction::CRON,
                        "cron",
                        b"list",
                        b"list",
                        PreparedBindingLifetime::RestartStable,
                        ApprovalResourceSource::PrimaryOperation,
                    )?],
                    &ctx.context_bytes(),
                )?,
                call.arguments,
                None,
                executor,
            );
        }
        let labels = kinds
            .iter()
            .map(|kind| kind.as_str().to_owned())
            .collect::<Vec<_>>();
        let resources = labels
            .iter()
            .map(|label| {
                prepared_resource(
                    PermissionAction::CRON,
                    "cron",
                    label.as_bytes(),
                    label.as_bytes(),
                    PreparedBindingLifetime::RestartStable,
                    ApprovalResourceSource::PrimaryOperation,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let operation = prepared_operation(
            &call.name,
            &call.arguments,
            vec![(
                PermissionAction::CRON,
                call.name.trim_start_matches("cron_"),
            )],
            resources,
            &ctx.context_bytes(),
        )?;
        PreparedTool::new(operation, call.arguments, None, executor)?.with_policy_labels(labels)
    }
}

struct CronExecutor {
    engine: Engine,
    call: CronCall,
    call_id: cookie_agent_protocol::ToolCallId,
}

#[async_trait]
impl PreparedExecutor for CronExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let engine = self.engine;
        let session = context.session;
        let error = |error: crate::EngineError| refused(error.to_string());
        match self.call {
            CronCall::Create(params) => {
                let caller = engine.cron_agent(session, context.run)?;
                let job = engine
                    .create_cron_as(CronRequester::Agent(caller), params)
                    .await
                    .map_err(error)?;
                let next = job.next_fire.map_or_else(|| "none".to_owned(), when);
                Ok(text_result(
                    "Scheduled a prompt",
                    format!(
                        "Created cron job {}; next fire {next}.\n\n{}",
                        job.id,
                        job_text(&engine, &job)
                    ),
                ))
            }
            CronCall::List(filter) => {
                let jobs = engine
                    .list_crons()
                    .jobs
                    .into_iter()
                    .filter(|job| filter.is_none_or(|kind| job.target.kind() == kind))
                    .filter(|job| engine.cron_kind_visible(session, context.run, job.target.kind()))
                    .collect::<Vec<_>>();
                let mut output = if jobs.is_empty() {
                    "No cron jobs.".to_owned()
                } else {
                    jobs.iter()
                        .map(|job| job_text(&engine, job))
                        .collect::<Vec<_>>()
                        .join("\n\n")
                };
                if !engine.list_crons().firing {
                    output.push_str(
                        "\n\nFiring is off ([cron] enabled = false): due fires are skipped.",
                    );
                }
                Ok(text_result(
                    &format!(
                        "Listed {} cron {}",
                        jobs.len(),
                        if jobs.len() == 1 { "job" } else { "jobs" }
                    ),
                    output,
                ))
            }
            CronCall::Update(params) => {
                let caller = engine.cron_agent(session, context.run)?;
                let job = engine
                    .update_cron_as(CronRequester::Agent(caller), params)
                    .await
                    .map_err(error)?;
                Ok(text_result(
                    "Changed a cron job",
                    format!(
                        "Changed cron job {}.\n\n{}",
                        job.id,
                        job_text(&engine, &job)
                    ),
                ))
            }
            CronCall::Delete(id) => {
                engine.delete_cron(id.clone()).await.map_err(error)?;
                Ok(text_result(
                    "Deleted a cron job",
                    format!("Deleted cron job {id}."),
                ))
            }
            CronCall::Run(id) => engine.cron_run_tool(context, self.call_id, id).await,
        }
    }
}

#[cfg(test)]
impl Engine {
    /// What `target`, as a tool writes it, names from `session`.
    pub(crate) fn cron_target_for_test(
        &self,
        session: SessionId,
        target: serde_json::Value,
    ) -> Result<CronTarget, ToolError> {
        let caller = self.cron_agent(session, cookie_agent_protocol::RunId::new_v7())?;
        let target: TargetArg = serde_json::from_value(target).expect("a tool target");
        self.resolve_tool_target(&caller, target)
    }

    /// Whether a run of `policy` in `session` sees jobs of `kind`.
    pub(crate) fn cron_kind_visible_for_test(
        &self,
        policy: &crate::policy::FrozenRunPolicy,
        session: SessionId,
        kind: CronTargetKind,
    ) -> bool {
        let overlay = self
            .inner
            .store
            .get(session)
            .map(|projection| projection.permission_overlay.clone())
            .unwrap_or_default();
        kind_visible(&policy.agent, &overlay, kind)
    }
}

#[cfg(test)]
mod tests {
    use super::{CreateArgs, TargetArg};

    #[test]
    fn a_create_without_a_target_fires_into_its_own_session() {
        let args: CreateArgs = serde_json::from_value(serde_json::json!({
            "prompt": "check the build",
            "schedule": {"in": "10m"},
        }))
        .expect("a create without a target");
        assert!(matches!(args.target, TargetArg::Session(target) if target == "self"));
    }
}
