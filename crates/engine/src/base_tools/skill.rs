use crate::Engine;
use async_trait::async_trait;
use cookie_agent_protocol::{
    ApprovalResourceSource, PermissionAction, PersistedToolResult as ToolResult,
    PreparedBindingLifetime,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec,
};
use serde::{Deserialize, Serialize};

use super::{
    base_presentation, markdown_display, parse_args, prepared_operation, prepared_resource,
    safe_title, schema, tool_error,
};

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
    /// The skill's name, as `<available_skills>` lists it.
    name: String,
    /// Arguments for the skill, as its instructions take them.
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
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_runtime_api::ToolCompletion, ToolError> {
        let result: Result<ToolResult, ToolError> = async move {
            if self
                .engine
                .skill_invocation_context(context.session, &self.args.name)
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
                )
                .await
                .map_err(tool_error)?;
            Ok(ToolResult {
                display: markdown_display(&invocation.rendered),
                background: None,
                retained_output: None,
                title: safe_title(format!("Loaded skill {}", invocation.name)),
                output: invocation.rendered,
                metadata: serde_json::json!({"skill": invocation.name}),
                attachments: Vec::new(),
                additional_messages: Vec::new(),
                loaded_tools: Vec::new(),
            })
        }
        .await;
        result.map(cookie_agent_runtime_api::ToolCompletion::single)
    }
}

impl SkillTool {
    pub(crate) fn spec() -> ToolSpec {
        ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            name: "skill".into(),
            // Skills take no permission: the session's skill choices
            // decide which the model may load (session tool groups spec,
            // section 7).
            permission_name: None,
            description: "Load a skill listed in `<available_skills>` by its name, with optional `args`. Its instructions come back as the result.".into(),
            parameters: schema::<SkillArgs>(),
        }
    }
}

impl SkillTool {
    /// The group this provider's tools are announced under when deferred.
    pub const GROUP: cookie_agent_runtime_api::ToolGroup = cookie_agent_runtime_api::ToolGroup::new(
        "skills",
        "Skills: packaged instructions for a kind of task, loaded by name.",
    );
}
#[async_trait]
impl ToolProvider for SkillTool {
    fn provider_id(&self) -> &'static str {
        crate::base_tools::SKILL_PROVIDER_ID
    }

    fn tool_group(&self, _tool: &str) -> Option<cookie_agent_runtime_api::ToolGroup> {
        Some(Self::GROUP)
    }

    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        if !self
            .engine
            .skill_tool_available(ctx.session)
            .map_err(tool_error)?
        {
            return Ok(Vec::new());
        }
        Ok(vec![Self::spec()])
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        if tool_name == "skill" {
            Ok(None)
        } else {
            Err(ToolError::execution("skill provider received another tool"))
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        Self::get_permission_name(name)?;
        let args: SkillArgs = serde_json::from_value(arguments.clone()).map_err(tool_error)?;
        Ok((None, Some(args.name)))
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

    fn presentation(&self, call: &ToolCall) -> cookie_agent_protocol::PreparedPresentation {
        match serde_json::from_value::<SkillArgs>(call.arguments.clone()) {
            Ok(args) if call.name == "skill" => {
                let mut entries = vec![("name", args.name.clone())];
                if !args.args.is_empty() {
                    entries.push(("arguments", args.args));
                }
                base_presentation(
                    &format!("Load skill {}", args.name),
                    cookie_agent_protocol::ToolHint::SKILL,
                    &args.name,
                    entries,
                )
            }
            _ => cookie_agent_runtime_api::tool_title_only(&call.name),
        }
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
        // No rule is evaluated; the capability only names the operation.
        let action = PermissionAction::new("skill").map_err(tool_error)?;
        let resource = prepared_resource(
            action.clone(),
            "skill",
            args.name.as_bytes(),
            args.name.as_bytes(),
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = prepared_operation(
            "skill",
            &args,
            vec![(action, "load")],
            vec![resource],
            &ctx.context_bytes(),
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
