use async_trait::async_trait;
use cookie_agent_engine::{
    DelegateInvocation, Engine, PreparedExecutor, PreparedTool, SessionToolContext, ToolCall,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::{
    AgentId, ApprovalResourceSource, PermissionAction, PersistedToolResult as ToolResult,
    PreparedBindingLifetime, SessionId,
};
use serde::{Deserialize, Serialize};

use crate::{fs_cap, prepared_operation, prepared_resource, safe_title};

const DEFAULT_RESULT_LIMIT: u32 = 2_000;

pub struct DelegateToolProvider {
    engine: Engine,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DelegateArgs {
    description: String,
    prompt: String,
    agent_type: AgentId,
    #[serde(default)]
    background: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GetResultArgs {
    session_id: SessionId,
    #[serde(default)]
    wait: bool,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CancelArgs {
    session_id: SessionId,
    reason: Option<String>,
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
    Cancel {
        engine: Engine,
        args: CancelArgs,
    },
}

impl DelegateToolProvider {
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self { engine }
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
        &self,
        ctx: &ToolPreparationContext,
        name: &str,
        args: &impl Serialize,
        operation_name: &str,
        agent_type: &AgentId,
    ) -> Result<PreparedToolParts, ToolError> {
        let resource = prepared_resource(
            PermissionAction::Delegate,
            "agent",
            agent_type.as_str().as_bytes(),
            agent_type.as_str().as_bytes(),
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let cwd = fs_cap::cwd_context_bytes(&ctx.cwd)?;
        Ok(PreparedToolParts {
            operation: prepared_operation(
                name,
                args,
                vec![(PermissionAction::Delegate, operation_name)],
                vec![resource],
                &cwd,
            )?,
            policy_labels: vec![agent_type.to_string()],
        })
    }
}

struct PreparedToolParts {
    operation: cookie_agent_protocol::PreparedOperationIdentity,
    policy_labels: Vec<String>,
}

#[async_trait]
impl ToolProvider for DelegateToolProvider {
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let targets = self.targets(ctx.session)?;
        Ok(if targets.is_empty() {
            Vec::new()
        } else {
            vec![
                ToolSpec {
                    name: "delegate_subagent".into(),
                    description: "Delegate a self-contained prompt to an allowed subagent.".into(),
                    parameters: serde_json::json!({
                        "type":"object","additionalProperties":false,
                        "properties":{
                            "description":{"type":"string"},
                            "prompt":{"type":"string"},
                            "agent_type":{"type":"string","enum":targets},
                            "background":{"type":"boolean","default":false}
                        },
                        "required":["description","prompt","agent_type"]
                    }),
                },
                ToolSpec {
                    name: "get_subagent_result".into(),
                    description: "Read a paginated result from an owned subagent session.".into(),
                    parameters: serde_json::json!({
                        "type":"object","additionalProperties":false,
                        "properties":{
                            "session_id":{"type":"string"},
                            "wait":{"type":"boolean","default":false},
                            "offset":{"type":"integer","minimum":0,"default":0},
                            "limit":{"type":"integer","minimum":1,"maximum":4_294_967_295_u64,"default":2000}
                        },
                        "required":["session_id"]
                    }),
                },
                ToolSpec {
                    name: "cancel_subagent".into(),
                    description: "Cancel an owned subagent session.".into(),
                    parameters: serde_json::json!({
                        "type":"object","additionalProperties":false,
                        "properties":{
                            "session_id":{"type":"string"},
                            "reason":{"type":"string"}
                        },
                        "required":["session_id"]
                    }),
                },
            ]
        })
    }

    fn get_primary_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        match name {
            "delegate_subagent" => Ok(parse_delegate(arguments)?.agent_type.to_string()),
            "get_subagent_result" => Ok(parse_result(arguments)?.session_id.to_string()),
            "cancel_subagent" => Ok(parse_cancel(arguments)?.session_id.to_string()),
            _ => Err(ToolError::execution(
                "delegate provider received another tool",
            )),
        }
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        match name {
            "delegate_subagent" => Ok(parse_delegate(arguments)?.description),
            _ => self.get_primary_argument(name, arguments),
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
                if !self.targets(ctx.session)?.contains(&args.agent_type) {
                    return Err(ToolError::execution("delegate target is not allowed"));
                }
                let parts =
                    self.operation(&ctx, "delegate_subagent", &args, "spawn", &args.agent_type)?;
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
                let agent = self
                    .engine
                    .subagent_agent_type(ctx.session, args.session_id)
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                let parts = self.operation(&ctx, "get_subagent_result", &args, "read", &agent)?;
                let normalized = serde_json::to_value(&args)
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                let executor = DelegateExecutor::GetResult {
                    engine: self.engine.clone(),
                    args,
                };
                (parts, normalized, executor)
            }
            "cancel_subagent" => {
                let args = parse_cancel(&call.arguments)?;
                let agent = self
                    .engine
                    .subagent_agent_type(ctx.session, args.session_id)
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                let parts = self.operation(&ctx, "cancel_subagent", &args, "cancel", &agent)?;
                let normalized = serde_json::to_value(&args)
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                let executor = DelegateExecutor::Cancel {
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
            .with_policy_labels(parts.policy_labels)
    }
}

#[async_trait]
impl PreparedExecutor for DelegateExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
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
                let background = args.background;
                let handle = engine
                    .delegate_invoke(DelegateInvocation {
                        parent_session_id: context.session,
                        parent_run_id: context.run,
                        parent_tool_call_id: call_id,
                        agent_type: args.agent_type,
                        description: args.description,
                        prompt: args.prompt,
                        background,
                    })
                    .await
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                if background {
                    let metadata = serde_json::json!({"session_id":handle.child_session_id});
                    Ok(ToolResult {
                        title: safe_title("Subagent started"),
                        output: metadata.to_string(),
                        metadata,
                        truncation: None,
                        attachments: Vec::new(),
                    })
                } else {
                    engine
                        .await_delegate(handle)
                        .await
                        .map_err(|error| ToolError::execution(error.to_string()))
                }
            }
            Self::GetResult { engine, args } => engine
                .get_subagent_result(
                    context.session,
                    args.session_id,
                    args.wait,
                    args.offset,
                    args.limit.expect("normalized result limit"),
                    context.cancellation,
                )
                .await
                .map_err(|error| ToolError::execution(error.to_string())),
            Self::Cancel { engine, args } => engine
                .cancel_subagent(context.session, args.session_id, args.reason)
                .await
                .map_err(|error| ToolError::execution(error.to_string())),
        }
    }
}

fn parse_delegate(arguments: &serde_json::Value) -> Result<DelegateArgs, ToolError> {
    serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::execution(error.to_string()))
}

fn parse_result(arguments: &serde_json::Value) -> Result<GetResultArgs, ToolError> {
    serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::execution(error.to_string()))
}

fn parse_cancel(arguments: &serde_json::Value) -> Result<CancelArgs, ToolError> {
    serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::execution(error.to_string()))
}

#[cfg(test)]
mod tests {
    use cookie_agent_engine::ToolError;

    use super::parse_delegate;

    #[test]
    fn delegate_arguments_use_agent_as_primary_and_description_as_display() {
        let arguments = serde_json::json!({
            "description":"Review API",
            "prompt":"Review the API in full.",
            "agent_type":"reviewer"
        });
        let args = parse_delegate(&arguments).expect("delegate arguments");
        assert_eq!(args.agent_type.as_str(), "reviewer");
        assert_eq!(args.description, "Review API");
        assert!(matches!(
            parse_delegate(&serde_json::json!({"task":"review","agent":"reviewer"})),
            Err(ToolError::Failed(_))
        ));
    }
}
