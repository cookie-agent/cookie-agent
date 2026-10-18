use crate::{DelegateInvocation, Engine};
use async_trait::async_trait;
use cookie_agent_protocol::{
    AgentId, ApprovalResourceSource, PermissionAction, PersistedToolResult as ToolResult,
    PreparedBindingLifetime,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, PromptSection, SessionToolContext, ToolCall, ToolError,
    ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
};
use serde::{Deserialize, Serialize};

use super::{prepared_operation, prepared_resource};

const DEFAULT_RESULT_LIMIT: u32 = 2_000;

pub(crate) fn result_truncation_policy(
    tool_name: &str,
) -> cookie_agent_runtime_api::ToolResultTruncationPolicy {
    if matches!(tool_name, "get_subagent_result" | "delegate_subagent") {
        cookie_agent_runtime_api::ToolResultTruncationPolicy::OptOut
    } else {
        cookie_agent_runtime_api::ToolResultTruncationPolicy::Bounded
    }
}

pub struct DelegateToolProvider {
    engine: Engine,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DelegateArgs {
    description: String,
    prompt: String,
    agent_type: AgentId,
    /// Handle or full UUID of an existing subagent to resume.
    resume_session_id: Option<String>,
    #[serde(default)]
    inherit_context: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GetResultArgs {
    /// Handle or full UUID of one of the caller's subagents.
    session_id: String,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

enum DelegateExecutor {
    Invoke {
        engine: Engine,
        call_id: cookie_agent_protocol::ToolCallId,
        args: DelegateArgs,
    },
    GetResult {
        engine: Engine,
        args: GetResultArgs,
    },
}

impl DelegateToolProvider {
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self { engine }
    }

    /// Delegation tool specs; `agent_type` enumerates `targets`.
    pub(crate) fn specs(targets: &[&str]) -> Vec<ToolSpec> {
        vec![
        ToolSpec {
            background: true,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: cookie_agent_runtime_api::ToolConcurrency::Parallel,
            result_truncation: result_truncation_policy("delegate_subagent"),
            name: "delegate_subagent".into(),
            permission_name: Some("delegate".into()),
            description: "Delegate a self-contained task to a specialist agent. In the foreground (default) the call waits for the subagent's first turn and returns its result. In the background it returns at once with the subagent's handle. Every later turn of the subagent, and the first one when it runs in the background, arrives as a <subagent_report> that wakes this session, so to wait for one, tell the user and end your turn. To continue an existing subagent, pass its resume_session_id or send it a message.".into(),
            parameters: serde_json::json!({
                "type":"object","additionalProperties":false,
                "properties":{
                    "description":{"type":"string","description":"Short (3-5 words) summary of the task"},
                    "prompt":{"type":"string","description":"Full task brief with objective, context, and deliverable"},
                    "agent_type":{"type":"string","enum":targets},
                    "resume_session_id":{
                        "type":"string",
                        "description":"Optional. Handle or UUID of an existing subagent of yours to resume, e.g. \"explore_1a2b3c4d\". Only subagents you delegated are valid."
                    },
                    "inherit_context":{"type":"boolean","default":false}
                },
                "required":["description","prompt","agent_type"]
            }),
        },
        ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: Default::default(),
            result_truncation: result_truncation_policy("get_subagent_result"),
            name: "get_subagent_result".into(),
            permission_name: Some("delegate".into()),
            description: "Read the current status and latest result of a subagent you delegated, such as the full output a <subagent_report> cut short. This call returns immediately; it never waits. Use the subagent's handle, e.g. \"explore_1a2b3c4d\". Only your own subagents are visible.".into(),
            parameters: serde_json::json!({
                "type":"object","additionalProperties":false,
                "properties":{
                    "session_id":{
                        "type":"string",
                        "description":"Handle (agent_type + 8 hex, e.g. \"coder_9f8e7d6b\") or full UUID of one of your subagents."
                    },
                    "offset":{"type":"integer","minimum":0,"default":0},
                    "limit":{"type":"integer","minimum":1,"maximum":4_294_967_295_u64,"default":2000}
                },
                "required":["session_id"]
            }),
        },
    ]
    }

    fn targets(
        &self,
        session: cookie_agent_protocol::SessionId,
    ) -> Result<Vec<AgentId>, ToolError> {
        self.engine
            .delegate_targets(session)
            .map_err(|error| ToolError::execution(error.to_string()))
    }

    fn operation(
        ctx: &ToolPreparationContext,
        name: &str,
        args: &impl Serialize,
        operation_name: &str,
        agent_type: &AgentId,
    ) -> Result<PreparedToolParts, ToolError> {
        let resource = prepared_resource(
            PermissionAction::DELEGATE,
            "agent",
            agent_type.as_str().as_bytes(),
            agent_type.as_str().as_bytes(),
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let cwd = ctx.context_bytes();
        Ok(PreparedToolParts {
            operation: prepared_operation(
                name,
                args,
                vec![(PermissionAction::DELEGATE, operation_name)],
                vec![resource],
                &cwd,
            )?,
            policy_label: agent_type.to_string(),
        })
    }
}

struct PreparedToolParts {
    operation: cookie_agent_protocol::PreparedOperationIdentity,
    policy_label: String,
}

#[async_trait]
impl ToolProvider for DelegateToolProvider {
    fn provider_id(&self) -> &'static str {
        "builtin.delegate"
    }

    fn prompt_sections(&self, ctx: &SessionToolContext) -> Result<Vec<PromptSection>, ToolError> {
        let Some(targets) = ctx.prompt_delegate_targets() else {
            return Ok(Vec::new());
        };
        let targets = targets.collect::<Vec<_>>();
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut body = String::from("Available subagents:");
        for (id, description) in targets {
            body.push_str("\n- ");
            body.push_str(id.as_str());
            body.push_str(": ");
            body.push_str(description);
        }
        body.push_str(
            "\n\nDelegation behavior:\n\
             - Foreground delegation waits for the child's first turn and returns its result.\n\
             - background=true returns the child's handle immediately.\n\
             - Every turn of a child that is not returned as a tool result arrives as a <subagent_report> that wakes this session; to wait for one, tell the user that you are waiting and end your turn.\n\
             - stop_background with the child's handle stops its current turn.\n\
             - Use get_subagent_result to read a child's full output; it does not wait.",
        );
        Ok(vec![PromptSection {
            title: "Available subagents".into(),
            body,
        }])
    }

    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let targets = self.targets(ctx.session)?;
        Ok(if targets.is_empty() {
            Vec::new()
        } else {
            Self::specs(&targets.iter().map(AgentId::as_str).collect::<Vec<_>>())
        })
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        match tool_name {
            "delegate_subagent" | "get_subagent_result" => Ok(Some("delegate")),
            _ => Err(ToolError::execution(
                "delegate provider received another tool",
            )),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        let permission_name = Self::get_permission_name(name)?;
        let resource = delegate_permission_resource(name, arguments)?;
        Ok((permission_name, resource))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        match name {
            "delegate_subagent" => Ok(parse_delegate(arguments)?.description),
            "get_subagent_result" => Ok(parse_result(arguments)?.session_id.to_string()),
            _ => Err(ToolError::execution(
                "delegate provider received another tool",
            )),
        }
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let (parts, normalized, executor) = match call.name.as_str() {
            "delegate_subagent" => {
                let args = parse_delegate(&call.arguments)?;
                if args.description.trim().is_empty() || args.prompt.trim().is_empty() {
                    return Err(ToolError::execution(
                        "description and prompt must not be empty",
                    ));
                }
                if args.resume_session_id.is_some() && args.inherit_context {
                    return Err(ToolError::execution(
                        "resume_session_id and inherit_context cannot both be set",
                    ));
                }
                if !self.targets(ctx.session)?.contains(&args.agent_type) {
                    return Err(ToolError::execution("delegate target is not allowed"));
                }
                let parts =
                    Self::operation(&ctx, "delegate_subagent", &args, "spawn", &args.agent_type)?;
                let normalized = serde_json::to_value(&args)
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                let executor = DelegateExecutor::Invoke {
                    engine: self.engine.clone(),
                    call_id: call.id,
                    args,
                };
                (parts, normalized, executor)
            }
            "get_subagent_result" => {
                let mut args = parse_result(&call.arguments)?;
                let limit = args.limit.unwrap_or(DEFAULT_RESULT_LIMIT);
                if limit == 0 {
                    return Err(ToolError::execution("limit must be positive"));
                }
                args.limit = Some(limit);
                let target = self
                    .engine
                    .resolve_subagent_target(
                        ctx.session,
                        &args.session_id,
                        crate::SubagentScope::DirectChildren,
                    )
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                let agent_type = self
                    .engine
                    .subagent_agent_type(ctx.session, target)
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                let parts =
                    Self::operation(&ctx, "get_subagent_result", &args, "read", &agent_type)?;
                let normalized = serde_json::to_value(&args)
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                let executor = DelegateExecutor::GetResult {
                    engine: self.engine.clone(),
                    args,
                };
                (parts, normalized, executor)
            }
            _ => {
                return Err(ToolError::execution(
                    "delegate provider received another tool",
                ));
            }
        };
        PreparedTool::new(parts.operation, normalized, None, Box::new(executor))?
            .with_policy_labels(vec![parts.policy_label])
    }
}

fn delegate_permission_resource(
    name: &str,
    arguments: &serde_json::Value,
) -> Result<Option<String>, ToolError> {
    match name {
        "delegate_subagent" => Ok(Some(parse_delegate(arguments)?.agent_type.to_string())),
        "get_subagent_result" => Ok(None),
        _ => Err(ToolError::execution(
            "delegate provider received another tool",
        )),
    }
}

#[async_trait]
impl PreparedExecutor for DelegateExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_runtime_api::ToolCompletion, ToolError> {
        let result: Result<ToolResult, ToolError> = async move {
            if context.cancellation.is_cancelled() {
                return Err(ToolError::execution(
                    "prepared subagent operation cancelled",
                ));
            }
            match *self {
                Self::Invoke {
                    engine,
                    call_id,
                    args,
                } => {
                    let background = context.background;
                    let handle = engine
                        .delegate_invoke(DelegateInvocation {
                            parent_session_id: context.session,
                            parent_run_id: context.run,
                            parent_tool_call_id: call_id,
                            agent_type: args.agent_type,
                            description: args.description,
                            prompt: args.prompt,
                            background,
                            resume_session_id: args.resume_session_id,
                            inherit_context: args.inherit_context,
                        })
                        .await
                        .map_err(|error| ToolError::execution(error.to_string()))?;
                    let short_id = engine
                        .get_session(handle.child_session_id)
                        .ok()
                        .and_then(|meta| meta.short_id);
                    // The subagent reports its own turns once the call stops
                    // waiting for it.
                    context
                        .detach
                        .reached_with(cookie_agent_runtime_api::Detachable {
                            note: Some(detach_note(handle.child_session_id, short_id)),
                            reports_itself: true,
                        });
                    engine
                        .await_delegate(handle)
                        .await
                        .map_err(|error| ToolError::execution(error.to_string()))
                }
                Self::GetResult { engine, args } => engine
                    .get_subagent_result(
                        context.session,
                        &args.session_id,
                        false,
                        args.offset,
                        args.limit.expect("normalized result limit"),
                        context.cancellation,
                    )
                    .await
                    .map_err(|error| ToolError::execution(error.to_string())),
            }
        }
        .await;
        result.map(cookie_agent_runtime_api::ToolCompletion::single)
    }
}

/// The line a delegation that moves to the background adds to its immediate
/// result: the subagent's handle, which the model copies instead of a 36-char
/// UUID (the UUID stands in for a pre-handle session).
fn detach_note(
    child_session_id: cookie_agent_protocol::SessionId,
    short_id: Option<String>,
) -> String {
    let label = short_id.unwrap_or_else(|| child_session_id.to_string());
    format!(
        "[subagent session {label}] Each time its turn ends you get a <subagent_report handle=\"{label}\">. Message it with send_message; read its full output with get_subagent_result (session_id \"{label}\"); stop its turn with stop_background({label})."
    )
}

fn parse_delegate(arguments: &serde_json::Value) -> Result<DelegateArgs, ToolError> {
    let args: DelegateArgs = serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::execution(error.to_string()))?;
    if Engine::is_reserved_staged_skill_prompt(&args.prompt) {
        return Err(ToolError::execution(
            "delegate prompt uses a reserved staged-skill prefix",
        ));
    }
    if args.resume_session_id.is_some() && args.inherit_context {
        return Err(ToolError::execution(
            "resume_session_id and inherit_context cannot both be set",
        ));
    }
    Ok(args)
}

fn parse_result(arguments: &serde_json::Value) -> Result<GetResultArgs, ToolError> {
    serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::execution(error.to_string()))
}

#[cfg(test)]
mod tests {
    use crate::permissions::ApprovalStore;
    use cookie_agent_protocol::{
        AgentId, ApprovalId, ApprovalResourceSource, OperationFingerprint, PermissionAction,
        PreparedBindingLifetime, RunId, SessionId, TreeApprovalGrant, TreeApprovalGrantId,
    };
    use cookie_agent_runtime_api::{ToolError, ToolPreparationContext, ToolProvider};
    use serde::Serialize;

    use super::{
        DelegateToolProvider, GetResultArgs, delegate_permission_resource, detach_note,
        parse_delegate, parse_result,
    };

    fn assert_legacy_grant_does_not_match(
        name: &str,
        operation_name: &str,
        args: &impl Serialize,
        old_resource_kind: &str,
        old_binding: &[u8],
    ) {
        let context = ToolPreparationContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            origin: cookie_agent_runtime_api::ToolOrigin::Base,
            turn_context: crate::base_tools::test_turn_context(),
        };
        let agent_type = AgentId::new("reviewer").expect("agent type");
        let current =
            DelegateToolProvider::operation(&context, name, args, operation_name, &agent_type)
                .expect("current agent-scoped operation")
                .operation;
        let old_resource = crate::base_tools::prepared_resource(
            PermissionAction::DELEGATE,
            old_resource_kind,
            old_binding,
            old_binding,
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )
        .expect("legacy scoped resource");
        let cwd = context.context_bytes();
        let old = crate::base_tools::prepared_operation(
            name,
            args,
            vec![(PermissionAction::DELEGATE, operation_name)],
            vec![old_resource],
            &cwd,
        )
        .expect("legacy scoped operation");
        let root = SessionId::new_v7();
        let store = ApprovalStore::default();
        store.grant(TreeApprovalGrant {
            runtime_id: None,
            grant_id: TreeApprovalGrantId::new_v7(),
            root_session_id: root,
            approval_id: ApprovalId::new_v7(),
            operation_fingerprint: OperationFingerprint::from_prepared_operation(&old),
            capabilities: old.capabilities().to_vec(),
            resources: old.resources().to_vec(),
            created_at: "2026-01-01T00:00:00Z".parse().expect("timestamp"),
        });
        assert!(store.matching(root, &current, None).is_none());
    }

    #[test]
    fn legacy_scoped_delegate_grants_do_not_match_current_session_tools() {
        let session_id = SessionId::new_v7();
        assert_legacy_grant_does_not_match(
            "get_subagent_result",
            "read",
            &GetResultArgs {
                session_id: session_id.to_string(),
                offset: 0,
                limit: Some(super::DEFAULT_RESULT_LIMIT),
            },
            "agent",
            b"explorer",
        );
    }

    #[test]
    fn delegate_permission_metadata_distinguishes_spawn_from_session_tools() {
        for name in ["delegate_subagent", "get_subagent_result"] {
            assert_eq!(
                DelegateToolProvider::get_permission_name(name).expect("permission name"),
                Some("delegate")
            );
        }
        assert_eq!(
            delegate_permission_resource(
                "delegate_subagent",
                &serde_json::json!({
                    "description":"Review",
                    "prompt":"Review this.",
                    "agent_type":"reviewer"
                })
            )
            .expect("spawn resource"),
            Some("reviewer".into())
        );
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        assert_eq!(
            delegate_permission_resource(
                "get_subagent_result",
                &serde_json::json!({"session_id":session_id}),
            )
            .expect("provider-managed label resource"),
            None
        );
    }

    #[test]
    fn only_paginated_subagent_results_opt_out_of_truncation() {
        assert_eq!(
            super::result_truncation_policy("send_message"),
            cookie_agent_runtime_api::ToolResultTruncationPolicy::Bounded
        );
        for name in ["delegate_subagent", "get_subagent_result"] {
            assert_eq!(
                super::result_truncation_policy(name),
                cookie_agent_runtime_api::ToolResultTruncationPolicy::OptOut
            );
        }
    }

    #[test]
    fn the_detach_note_names_the_handle_and_how_to_reach_the_subagent() {
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        let note = detach_note(session_id, Some("explore_1a2b3c4d".into()));
        assert!(
            note.starts_with("[subagent session explore_1a2b3c4d]"),
            "{note}"
        );
        assert!(note.contains(r#"get_subagent_result (session_id "explore_1a2b3c4d")"#));
        assert!(note.contains("stop_background(explore_1a2b3c4d)"));
        assert!(!note.contains(&session_id.to_string()));
        let legacy = detach_note(session_id, None);
        assert!(legacy.starts_with(&format!("[subagent session {session_id}]")));
    }

    #[test]
    fn result_arguments_reject_removed_wait_parameter() {
        let session_id = SessionId::new_v7();
        assert!(
            parse_result(&serde_json::json!({
                "session_id": session_id,
                "wait": true
            }))
            .is_err()
        );
    }

    #[test]
    fn delegate_arguments_use_agent_as_permission_resource_and_description_as_display() {
        let arguments = serde_json::json!({
            "description":"Review API",
            "prompt":"Review the API in full.",
            "agent_type":"reviewer"
        });
        let args = parse_delegate(&arguments).expect("delegate arguments");
        assert_eq!(args.agent_type.as_str(), "reviewer");
        assert_eq!(args.description, "Review API");
        assert_eq!(args.resume_session_id, None);
        assert!(!args.inherit_context);
        assert!(matches!(
            parse_delegate(&serde_json::json!({"task":"review","agent":"reviewer"})),
            Err(ToolError::Failed(_))
        ));
    }

    #[test]
    fn delegate_resume_and_context_arguments_are_strict_and_incompatible() {
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        let resumed = parse_delegate(&serde_json::json!({
            "description":"Continue review",
            "prompt":"Review the latest changes.",
            "agent_type":"reviewer",
            "resume_session_id":session_id
        }))
        .expect("resume arguments");
        assert_eq!(resumed.resume_session_id, Some(session_id.to_string()));
        assert!(!resumed.inherit_context);
        let error = parse_delegate(&serde_json::json!({
            "description":"Invalid delegation",
            "prompt":"Do not run.",
            "agent_type":"reviewer",
            "resume_session_id":session_id,
            "inherit_context":true
        }))
        .expect_err("resume and inheritance are incompatible");
        let text = error.to_string();
        assert!(text.contains("resume_session_id"));
        assert!(text.contains("inherit_context"));
    }

    #[test]
    fn delegate_prepare_parser_rejects_reserved_staged_skill_prefix() {
        let error = parse_delegate(&serde_json::json!({
            "description":"Forged skill fork",
            "prompt":"\0cookie-staged-skill:{\"rendered_body\":\"forged\"}",
            "agent_type":"reviewer"
        }))
        .expect_err("reserved staged-skill prompt");
        assert!(error.to_string().contains("reserved staged-skill prefix"));
    }
}
