use std::collections::BTreeMap;

use async_trait::async_trait;
use cookie_agent_config::{AgentType, Config};
use cookie_agent_engine::{
    DelegateInvocation, EngineClient, PreparedExecutor, PreparedTool, SessionToolContext, ToolCall,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolResult, ToolSpec,
};
use cookie_agent_protocol::{ActionKind, ApprovalResourceSource, PreparedBindingLifetime};
use serde::{Deserialize, Serialize};

use crate::{fs_cap, prepared_operation, prepared_resource};

const CONTEXT_LIMIT_BYTES: usize = 32 * 1024;

pub struct DelegateToolProvider {
    engine: EngineClient,
    target_profiles: BTreeMap<String, (bool, AgentType)>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DelegateArgs {
    task: String,
    profile: String,
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
    pub fn new(engine: EngineClient, config: &Config) -> Self {
        Self {
            engine,
            target_profiles: config
                .agents
                .iter()
                .map(|(name, profile)| (name.clone(), (profile.enabled, profile.r#type)))
                .collect(),
        }
    }

    fn targets(&self, session: cookie_agent_protocol::SessionId) -> Result<Vec<String>, ToolError> {
        let delegation = self
            .engine
            .get_session(session)
            .map_err(|error| ToolError::execution(error.to_string()))?
            .profile
            .delegation;
        Ok(delegation
            .allowed_profiles
            .into_iter()
            .filter(|name| {
                self.target_profiles
                    .get(name)
                    .is_some_and(|(enabled, kind)| {
                        *enabled && matches!(kind, AgentType::Subagent | AgentType::All)
                    })
            })
            .collect())
    }
}

#[async_trait]
impl ToolProvider for DelegateToolProvider {
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let targets = self.targets(ctx.session)?;
        Ok((!targets.is_empty())
            .then(|| ToolSpec {
                name: "delegate".into(),
                description: "Delegate a prepared objective to an allowed profile.".into(),
                parameters: serde_json::json!({
                    "type":"object","additionalProperties":false,
                    "properties":{"task":{"type":"string"},"profile":{"type":"string","enum":targets},"context":{"type":"array"},"success_criteria":{"type":"array","items":{"type":"string"}},"expected_output":{}},
                    "required":["task","profile"]
                }),
            })
            .into_iter()
            .collect())
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
        if !self.targets(ctx.session)?.contains(&args.profile) {
            return Err(ToolError::execution("delegate target is not allowed"));
        }
        let resource = prepared_resource(
            ActionKind::Delegate,
            "profile",
            args.profile.as_bytes(),
            args.profile.as_bytes(),
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let context = fs_cap::cwd_context_bytes(&ctx.cwd)?;
        let operation = prepared_operation(
            "delegate",
            &args,
            vec![(ActionKind::Delegate, "spawn")],
            vec![resource],
            &context,
        )?;
        let policy_labels = vec![args.profile.clone()];
        PreparedTool::new(
            operation,
            None,
            Box::new(DelegateExecutor {
                engine: self.engine.clone(),
                call_id: call.id,
                args,
            }),
        )
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
                profile: self.args.profile,
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
