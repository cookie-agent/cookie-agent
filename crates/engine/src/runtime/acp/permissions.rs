//! An ACP agent's own permission requests
//! (`docs/site/specs/acp-agents.md` section 8).
//!
//! A `session/request_permission` names one of the agent's tool calls. Its
//! ACP kind maps onto one of cookie's actions, and its locations, command,
//! or URL onto that action's resources; the agent's merged rules decide
//! each, through the same pipeline as cookie's tools, grants included. An
//! `ask` goes through the approval flow. The answer is the agent's
//! allow-once or reject-once option, never an "always" one.

use cookie_agent_protocol::{
    ApprovalConstraints, ApprovalId, ApprovalRequest, ApprovalResourceSource, ApprovalTrigger,
    PermissionAction, PermissionEffect, PreparedBindingLifetime, ToolCallId,
};
use cookie_agent_runtime_api::authoring::{prepared_operation, prepared_resource};
use serde_json::Value;

use super::super::{ActiveRun, Engine};
use super::wire::{self, ToolCallFields};
use crate::runtime_snapshot::RuntimeAgent;

/// What a permission request asks: one action and the resources it needs,
/// each of which must be allowed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MappedRequest {
    pub action: PermissionAction,
    pub resources: Vec<String>,
}

/// The first string member of `input` among `names`.
fn input_string<'a>(input: Option<&'a Value>, names: &[&str]) -> Option<&'a str> {
    let input = input?;
    names
        .iter()
        .find_map(|name| input.get(*name).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
}

/// The command an `execute` call runs: its raw input's `command` (a string
/// or an argument vector), else its title.
fn command_of(call: &ToolCallFields) -> Option<String> {
    let input = call.raw_input.as_ref();
    if let Some(command) = input.and_then(|input| input.get("command")) {
        match command {
            Value::String(command) if !command.is_empty() => return Some(command.clone()),
            Value::Array(arguments) => {
                let words = arguments
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                if !words.is_empty() {
                    return Some(words.join(" "));
                }
            }
            _ => {}
        }
    }
    input_string(input, &["cmd"])
        .map(str::to_owned)
        .or_else(|| call.title.clone())
}

/// The paths a file call touches: its locations, else its raw input's path,
/// else its title.
fn paths_of(call: &ToolCallFields) -> Vec<String> {
    let locations = call
        .locations
        .iter()
        .flatten()
        .map(|location| location.path.clone())
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    if !locations.is_empty() {
        return locations;
    }
    input_string(
        call.raw_input.as_ref(),
        &["path", "file_path", "filePath", "abs_path", "absPath"],
    )
    .map(str::to_owned)
    .or_else(|| call.title.clone())
    .into_iter()
    .collect()
}

/// Maps the tool call a permission request names onto cookie's action
/// and resources: `call` as the request carries it, merged over `known`,
/// what the agent reported of the call before.
pub(crate) fn map_request(call: &ToolCallFields, known: Option<&ToolCallFields>) -> MappedRequest {
    let merged = merge(known, call);
    let kind = merged.kind.as_deref().unwrap_or("other");
    let title = merged.title.clone().unwrap_or_default();
    match kind {
        "read" | "search" => MappedRequest {
            action: PermissionAction::READ,
            resources: paths_of(&merged),
        },
        "edit" | "delete" | "move" => MappedRequest {
            action: PermissionAction::WRITE,
            resources: paths_of(&merged),
        },
        "execute" => MappedRequest {
            action: PermissionAction::BASH,
            resources: command_of(&merged).into_iter().collect(),
        },
        "fetch" => MappedRequest {
            action: PermissionAction::WEBFETCH,
            resources: input_string(merged.raw_input.as_ref(), &["url", "uri"])
                .map(str::to_owned)
                .or_else(|| merged.title.clone())
                .into_iter()
                .collect(),
        },
        other => MappedRequest {
            action: PermissionAction::ACP,
            resources: vec![format!("{other} {title}").trim().to_owned()],
        },
    }
}

/// `update` over `known`: each field the update carries replaces the known
/// one.
pub(crate) fn merge(known: Option<&ToolCallFields>, update: &ToolCallFields) -> ToolCallFields {
    let mut merged = known.cloned().unwrap_or_default();
    merged.tool_call_id.clone_from(&update.tool_call_id);
    if update.title.is_some() {
        merged.title.clone_from(&update.title);
    }
    if update.kind.is_some() {
        merged.kind.clone_from(&update.kind);
    }
    if update.status.is_some() {
        merged.status.clone_from(&update.status);
    }
    if update.content.is_some() {
        merged.content.clone_from(&update.content);
    }
    if update.locations.is_some() {
        merged.locations.clone_from(&update.locations);
    }
    if update.raw_input.is_some() {
        merged.raw_input.clone_from(&update.raw_input);
    }
    if update.raw_output.is_some() {
        merged.raw_output.clone_from(&update.raw_output);
    }
    merged
}

/// The option of `kind` (`allow_once` or `reject_once`) the agent offered.
fn option_of<'a>(options: &'a [wire::PermissionOption], kind: &str) -> Option<&'a str> {
    options
        .iter()
        .find(|option| option.kind == kind)
        .map(|option| option.option_id.as_str())
}

impl Engine {
    /// Decides ACP permission request `request` of the run `active` of
    /// `agent`, whose session works in `cwd` on the agent's runtime, home
    /// `home`: the answer to send the agent. An approval it asks for names
    /// `tool_call_id`, the cookie call the agent's call is shown as.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::runtime) async fn decide_acp_permission(
        &self,
        active: &ActiveRun,
        run: cookie_agent_protocol::RunId,
        agent: &RuntimeAgent,
        cwd: &str,
        home: Option<&str>,
        request: &wire::PermissionRequest,
        known: Option<&ToolCallFields>,
        tool_call_id: Option<ToolCallId>,
    ) -> Value {
        let allowed = self
            .acp_permission_allowed(active, run, agent, cwd, home, request, known, tool_call_id)
            .await;
        let option = if allowed {
            option_of(&request.options, "allow_once")
        } else {
            option_of(&request.options, "reject_once")
        };
        wire::permission_outcome(option)
    }

    #[allow(clippy::too_many_arguments)]
    async fn acp_permission_allowed(
        &self,
        active: &ActiveRun,
        run: cookie_agent_protocol::RunId,
        agent: &RuntimeAgent,
        cwd: &str,
        home: Option<&str>,
        request: &wire::PermissionRequest,
        known: Option<&ToolCallFields>,
        tool_call_id: Option<ToolCallId>,
    ) -> bool {
        let mapped = map_request(&request.tool_call, known);
        if mapped.resources.is_empty() {
            return false;
        }
        let tool = format!("acp:{}", agent.id);
        let resources = mapped
            .resources
            .iter()
            .map(|resource| {
                prepared_resource(
                    mapped.action.clone(),
                    mapped.action.as_str(),
                    resource.as_bytes(),
                    resource.as_bytes(),
                    PreparedBindingLifetime::RestartStable,
                    ApprovalResourceSource::PrimaryOperation,
                )
            })
            .collect::<Result<Vec<_>, _>>();
        let arguments = serde_json::json!({
            "tool_call": request.tool_call.tool_call_id,
            "action": mapped.action.as_str(),
            "resources": mapped.resources,
        });
        let Ok(resources) = resources else {
            return false;
        };
        let Ok(operation) = prepared_operation(
            &tool,
            &arguments,
            vec![(mapped.action.clone(), "request_permission")],
            resources,
            agent.id.as_str().as_bytes(),
        ) else {
            return false;
        };
        let Ok(session) = self.inner.store.get(active.session) else {
            return false;
        };
        let overlay = session.permission_overlay.clone();
        let labels = mapped
            .resources
            .iter()
            .cloned()
            .map(Some)
            .collect::<Vec<_>>();
        let workspace = std::path::Path::new(cwd);
        let decision = self.inner.permissions.decide_operation_with_grants(
            &active.policy.agent,
            Some(&overlay),
            None,
            &operation,
            &labels,
            crate::permissions::PermissionScope::runtime(agent.runtime.as_str(), Some(workspace))
                .with_home(home),
        );
        match decision.effect {
            PermissionEffect::Allow => true,
            PermissionEffect::Deny => false,
            PermissionEffect::Ask => {
                let tool_call_id = tool_call_id.unwrap_or_else(ToolCallId::new_v7);
                let request = match ApprovalRequest::new(
                    ApprovalId::new_v7(),
                    1,
                    tool_call_id,
                    ApprovalTrigger::PermissionPolicy,
                    operation,
                    decision.evaluations,
                    ApprovalConstraints {
                        allow_once: true,
                        allow_tree_grant: !mapped.action.is_path(),
                        cancellable: true,
                        expires_at: super::super::approval_flow::approval_expiry(
                            self.inner.config.load().runtime.approval.timeout_ms,
                        ),
                    },
                ) {
                    Ok(request) => request,
                    Err(_) => return false,
                };
                let executor: cookie_agent_runtime_api::PreparedExecutorCell =
                    std::sync::Arc::new(tokio::sync::Mutex::new(Some(Box::new(AnsweredExecutor))));
                self.await_user_approval(
                    active,
                    run,
                    request,
                    executor,
                    true,
                    super::super::approval_flow::ApprovalToolInput {
                        tool_call_id,
                        name: &tool,
                        origin: None,
                        normalized_parameters: &arguments,
                    },
                )
                .await
                .is_ok_and(|outcome| outcome.approved)
            }
        }
    }
}

/// The executor behind an approval of an ACP permission request: the
/// agent runs the call itself, so there is nothing to run or revalidate.
struct AnsweredExecutor;

#[async_trait::async_trait]
impl cookie_agent_runtime_api::PreparedExecutor for AnsweredExecutor {
    async fn revalidate(&self) -> Result<(), cookie_agent_runtime_api::ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        _ctx: cookie_agent_runtime_api::ToolExecutionContext,
    ) -> Result<cookie_agent_runtime_api::ToolCompletion, cookie_agent_runtime_api::ToolError> {
        Err(cookie_agent_runtime_api::ToolError::execution(
            "an ACP agent runs its own calls",
        ))
    }
}
