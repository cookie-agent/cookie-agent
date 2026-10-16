//! `stop_background`: stops a background call, as
//! `docs/site/specs/background-tools.md` section 6 specifies.
//!
//! A base-layer tool that declares no permission: it can only reach the
//! session tree's own background calls. The engine publishes it only while
//! the model can see a background-capable tool.

use async_trait::async_trait;
use cookie_agent_protocol::{
    ApprovalResourceSource, PermissionAction, PersistedToolResult as ToolResult,
    PreparedBindingLifetime,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{parse_args, prepared_operation, prepared_resource, safe_title, schema, tool_error};
use crate::Engine;

pub(crate) const TOOL_NAME: &str = "stop_background";

pub struct StopBackgroundTool {
    engine: Engine,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct StopBackgroundArgs {
    /// The job ID from the result that moved the call to the background, such as `job_1a2b3c4d`, or a subagent's handle.
    job: String,
}

impl StopBackgroundTool {
    pub(crate) fn new(engine: Engine) -> Self {
        Self { engine }
    }

    #[must_use]
    pub fn spec() -> ToolSpec {
        ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: cookie_agent_runtime_api::ToolConcurrency::Parallel,
            result_truncation: Default::default(),
            name: TOOL_NAME.into(),
            permission_name: None,
            description: "Stop a call running in the background. It returns once the call has ended; its <background_result> still arrives, with status \"stopped\" and the output it produced. Given a subagent's handle, it cancels the turn the subagent is running; its <subagent_report> follows with status \"cancelled\", and the subagent can still be messaged or resumed.".into(),
            parameters: schema::<StopBackgroundArgs>(),
        }
    }
}

#[async_trait]
impl ToolProvider for StopBackgroundTool {
    fn provider_id(&self) -> &'static str {
        "builtin.stop_background"
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![Self::spec()])
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        match tool_name {
            TOOL_NAME => Ok(None),
            _ => Err(ToolError::execution(
                "stop_background provider received another tool",
            )),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        let permission_name = Self::get_permission_name(name)?;
        let args: StopBackgroundArgs = parse_args(TOOL_NAME, arguments.clone())?;
        Ok((permission_name, Some(args.job)))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Self::get_permission_name(name)?;
        let args: StopBackgroundArgs = parse_args(TOOL_NAME, arguments.clone())?;
        Ok(args.job)
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Self::get_permission_name(&call.name)?;
        let args: StopBackgroundArgs = parse_args(TOOL_NAME, call.arguments)?;
        // No rule is evaluated; the capability only names the operation.
        let action = PermissionAction::new(TOOL_NAME).map_err(tool_error)?;
        let binding = serde_json::to_vec(&args).map_err(tool_error)?;
        let resource = prepared_resource(
            action.clone(),
            "background-job",
            args.job.as_bytes(),
            &binding,
            PreparedBindingLifetime::ProcessLocal,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = prepared_operation(
            TOOL_NAME,
            &args,
            vec![(action, "stop")],
            vec![resource],
            ctx.session.to_string().as_bytes(),
        )?;
        let normalized = serde_json::to_value(&args).map_err(tool_error)?;
        PreparedTool::new(
            operation,
            normalized,
            None,
            Box::new(StopBackgroundExecutor {
                engine: self.engine.clone(),
                job: args.job.clone(),
            }),
        )?
        .with_policy_labels(vec![args.job])
    }
}

struct StopBackgroundExecutor {
    engine: Engine,
    job: String,
}

#[async_trait]
impl PreparedExecutor for StopBackgroundExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_runtime_api::ToolCompletion, ToolError> {
        let stopped = self
            .engine
            .stop_background(
                context.session,
                &self.job,
                crate::runtime::BackgroundActor::Model,
            )
            .await
            .map_err(tool_error)?;
        let follows = match stopped {
            crate::runtime::Stopped::Call => {
                "Its <background_result> follows with status \"stopped\"."
            }
            crate::runtime::Stopped::Subagent => {
                "Its <subagent_report> follows with status \"cancelled\"."
            }
        };
        Ok(cookie_agent_runtime_api::ToolCompletion::single(
            ToolResult {
                title: safe_title("Background call stopped"),
                output: format!("Stopped {}. {follows}", self.job),
                display: None,
                retained_output: None,
                metadata: serde_json::json!({"job": self.job}),
                truncation: None,
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            },
        ))
    }
}
