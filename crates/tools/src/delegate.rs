use async_trait::async_trait;
use cookie_agent_engine::{
    DelegateInvocation, EngineClient, PreparedExecutor, PreparedTool, SessionToolContext, ToolCall,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::{
    AgentId, ApprovalResourceSource, PermissionAction, PersistedToolResult as ToolResult,
    PreparedBindingLifetime,
};
use serde::{Deserialize, Serialize};

use crate::{fs_cap, prepared_operation, prepared_resource};

const CONTEXT_LIMIT_BYTES: usize = 32 * 1024;

pub struct DelegateToolProvider {
    engine: EngineClient,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DelegateArgs {
    task: String,
    agent: AgentId,
    #[serde(default)]
    context: Vec<serde_json::Value>,
    #[serde(default)]
    success_criteria: Vec<String>,
    #[serde(default)]
    expected_output: Option<serde_json::Value>,
}

struct DelegateExecutor {
    engine: EngineClient,
    call_id: cookie_agent_protocol::ToolCallId,
    args: DelegateArgs,
}

impl DelegateToolProvider {
    #[must_use]
    pub fn new(engine: EngineClient) -> Self {
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
}

#[async_trait]
impl ToolProvider for DelegateToolProvider {
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let targets = self.targets(ctx.session)?;
        Ok((!targets.is_empty())
            .then(|| ToolSpec {
                name: "delegate".into(),
                description: "Delegate a prepared objective to an allowed agent.".into(),
                parameters: serde_json::json!({
                    "type":"object","additionalProperties":false,
                    "properties":{"task":{"type":"string"},"agent":{"type":"string","enum":targets},"context":{"type":"array"},"success_criteria":{"type":"array","items":{"type":"string"}},"expected_output":{}},
                    "required":["task","agent"]
                }),
            })
            .into_iter()
            .collect())
    }

    fn get_primary_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        delegate_argument(name, arguments)
    }

    fn get_simplified_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        delegate_argument(name, arguments)
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let args: DelegateArgs = serde_json::from_value(call.arguments)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        if serde_json::to_vec(&args.context)
            .map_err(|error| ToolError::execution(error.to_string()))?
            .len()
            > CONTEXT_LIMIT_BYTES
        {
            return Err(ToolError::execution("delegate context exceeds 32 KiB"));
        }
        if !self.targets(ctx.session)?.contains(&args.agent) {
            return Err(ToolError::execution("delegate target is not allowed"));
        }
        let resource = prepared_resource(
            PermissionAction::Delegate,
            "agent",
            args.agent.as_str().as_bytes(),
            args.agent.as_str().as_bytes(),
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let context = fs_cap::cwd_context_bytes(&ctx.cwd)?;
        let operation = prepared_operation(
            "delegate",
            &args,
            vec![(PermissionAction::Delegate, "spawn")],
            vec![resource],
            &context,
        )?;
        let policy_labels = vec![args.agent.to_string()];
        let normalized_arguments = serde_json::json!({
            "agent": args.agent,
            "task": args.task,
            "context": args.context,
            "success_criteria": args.success_criteria,
            "expected_output": args.expected_output,
        });
        PreparedTool::new(
            operation,
            normalized_arguments,
            None,
            Box::new(DelegateExecutor {
                engine: self.engine.clone(),
                call_id: call.id,
                args,
            }),
        )?
        .with_policy_labels(policy_labels)
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
            return Err(ToolError::execution("prepared delegation cancelled"));
        }
        let handle = self
            .engine
            .delegate_invoke(DelegateInvocation {
                parent_session_id: context.session,
                parent_run_id: context.run,
                parent_tool_call_id: self.call_id,
                agent: self.args.agent,
                task: self.args.task,
                context: self.args.context,
                success_criteria: self.args.success_criteria,
                expected_output: self.args.expected_output.unwrap_or(serde_json::Value::Null),
            })
            .await
            .map_err(|error| ToolError::execution(error.to_string()))?;
        self.engine
            .await_delegate(handle)
            .await
            .map_err(|error| ToolError::execution(error.to_string()))
    }
}

fn delegate_argument(name: &str, arguments: &serde_json::Value) -> Result<String, ToolError> {
    if name != "delegate" {
        return Err(ToolError::execution(
            "delegate provider received another tool",
        ));
    }
    let args: DelegateArgs = serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::execution(error.to_string()))?;
    Ok(args.agent.to_string())
}

#[cfg(test)]
mod tests {
    use cookie_agent_engine::ToolError;

    use super::delegate_argument;

    #[test]
    fn primary_and_simplified_arguments_are_the_agent_id() {
        let arguments = serde_json::json!({"task":"review","agent":"reviewer"});
        assert_eq!(
            delegate_argument("delegate", &arguments).expect("delegate argument"),
            "reviewer"
        );
        assert!(matches!(
            delegate_argument("delegate", &serde_json::json!({"task":"review"})),
            Err(ToolError::Failed(_))
        ));
    }
}
