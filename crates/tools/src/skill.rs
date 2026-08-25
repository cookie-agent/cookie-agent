use async_trait::async_trait;
use cookie_agent_engine::{
    Engine, PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError,
    ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::{
    ApprovalResourceSource, PermissionAction, PersistedToolResult as ToolResult,
    PreparedBindingLifetime,
};
use serde::{Deserialize, Serialize};

use crate::{parse_args, prepared_operation, prepared_resource, safe_title, schema, tool_error};

pub struct SkillTool {
    engine: Engine,
}

impl SkillTool {
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self { engine }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SkillArgs {
    name: String,
    #[serde(default)]
    args: String,
}

struct SkillExecutor {
    engine: Engine,
    call_id: cookie_agent_protocol::ToolCallId,
    args: SkillArgs,
}

#[async_trait]
impl PreparedExecutor for SkillExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        if self
            .engine
            .skill_invocation_context(&self.args.name)
            .map_err(tool_error)?
            == Some(cookie_agent_config::SkillContext::Fork)
        {
            return self
                .engine
                .execute_skill_fork(
                    context.session,
                    context.run,
                    self.call_id,
                    &self.args.name,
                    &self.args.args,
                )
                .await
                .map_err(tool_error);
        }
        let invocation = self
            .engine
            .invoke_skill(
                context.session,
                Some(context.run),
                &self.args.name,
                &self.args.args,
                true,
            )
            .await
            .map_err(tool_error)?;
        Ok(ToolResult {
            title: safe_title(format!("Loaded skill {}", invocation.name)),
            output: invocation.rendered,
            metadata: serde_json::json!({"skill": invocation.name}),
            truncation: None,
            attachments: Vec::new(),
        })
    }
}

#[async_trait]
impl ToolProvider for SkillTool {
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        if !self
            .engine
            .skill_tool_available(ctx.session)
            .map_err(tool_error)?
        {
            return Ok(Vec::new());
        }
        Ok(vec![ToolSpec {
            result_truncation: Default::default(),
            name: "skill".into(),
            permission_name: "skill".into(),
            description: "Load an available skill by name with optional arguments.".into(),
            parameters: schema::<SkillArgs>(),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        if tool_name == "skill" {
            Ok("skill")
        } else {
            Err(ToolError::execution("skill provider received another tool"))
        }
    }

    fn permission_for_unlisted_tool(
        &self,
        tool_name: &str,
    ) -> Result<Option<&'static str>, ToolError> {
        Ok((tool_name == "skill").then_some("skill"))
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        Self::get_permission_name(name)?;
        let args: SkillArgs = serde_json::from_value(arguments.clone()).map_err(tool_error)?;
        Ok(("skill", Some(args.name)))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Self::get_permission_name(name)?;
        Ok(serde_json::from_value::<SkillArgs>(arguments.clone())
            .map_err(tool_error)?
            .name)
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Self::get_permission_name(&call.name)?;
        let args: SkillArgs = parse_args("skill", call.arguments)?;
        if self.engine.is_direct_skill_call(call.id) {
            self.engine
                .get_user_skill(ctx.session, &args.name, &args.args)
                .map_err(tool_error)?;
        } else {
            self.engine
                .get_model_skill(ctx.session, &args.name, &args.args)
                .map_err(tool_error)?;
        }
        let resource = prepared_resource(
            PermissionAction::Skill,
            "skill",
            args.name.as_bytes(),
            args.name.as_bytes(),
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = prepared_operation(
            "skill",
            &args,
            vec![(PermissionAction::Skill, "load")],
            vec![resource],
            ctx.cwd.to_string_lossy().as_bytes(),
        )?;
        let normalized = serde_json::to_value(&args).map_err(tool_error)?;
        PreparedTool::new(
            operation,
            normalized,
            None,
            Box::new(SkillExecutor {
                engine: self.engine.clone(),
                call_id: call.id,
                args,
            }),
        )
    }
}
