use async_trait::async_trait;
use cookie_agent_engine::{
    Engine, PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError,
    ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::{
    ApprovalResourceSource, PermissionAction, PersistedToolResult as ToolResult,
    PreparedBindingLifetime, ToolCallId,
};
use serde::{Deserialize, Serialize};

use crate::{fs_cap, parse_args, prepared_operation, prepared_resource, safe_title};

const DEFAULT_LIMIT: u64 = 2_000;
const MAX_LIMIT: u64 = 2_000;

pub struct ReadToolResultProvider {
    engine: Engine,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ResultStream {
    Stdout,
    Stderr,
}

impl ResultStream {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadToolResultArgs {
    tool_call_id: ToolCallId,
    #[serde(default)]
    offset: u64,
    limit: Option<u64>,
    stream: Option<ResultStream>,
}

struct ReadToolResultExecutor {
    engine: Engine,
    session: cookie_agent_protocol::SessionId,
    args: ReadToolResultArgs,
}

impl ReadToolResultProvider {
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self { engine }
    }

    fn permission_resource(tool_call_id: ToolCallId) -> String {
        format!("tool_result:{tool_call_id}")
    }

    pub(crate) fn tool_spec() -> ToolSpec {
        ToolSpec {
            result_truncation: cookie_agent_engine::ToolResultTruncationPolicy::OptOut,
            name: "read_tool_result".into(),
            permission_name: "read_tool_result".into(),
            description: "Read a line page from a prior tool result in this session.".into(),
            parameters: serde_json::json!({
                "type":"object",
                "additionalProperties":false,
                "properties":{
                    "tool_call_id":{"type":"string","format":"uuid"},
                    "offset":{"type":"integer","minimum":0,"default":0},
                    "limit":{"type":"integer","minimum":1,"maximum":MAX_LIMIT,"default":DEFAULT_LIMIT},
                    "stream":{"type":"string","enum":["stdout","stderr"]}
                },
                "required":["tool_call_id"]
            }),
        }
    }
}

#[async_trait]
impl ToolProvider for ReadToolResultProvider {
    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![Self::tool_spec()])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "read_tool_result" => Ok("read_tool_result"),
            _ => Err(ToolError::execution(
                "read tool result provider received another tool",
            )),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let permission = Self::get_permission_name(name)?;
        let args: ReadToolResultArgs = parse_args(name, arguments.clone())?;
        Ok((
            permission,
            Some(Self::permission_resource(args.tool_call_id)),
        ))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        let args: ReadToolResultArgs = parse_args(name, arguments.clone())?;
        Ok(args.tool_call_id.to_string())
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Self::get_permission_name(&call.name)?;
        let mut args: ReadToolResultArgs = parse_args(&call.name, call.arguments)?;
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        if limit == 0 {
            return Err(ToolError::execution("limit must be positive"));
        }
        args.limit = Some(limit);
        let label = Self::permission_resource(args.tool_call_id);
        let resource = prepared_resource(
            PermissionAction::Read,
            "tool_result",
            label.as_bytes(),
            serde_json::to_string(&args)
                .map_err(|error| ToolError::execution(error.to_string()))?
                .as_bytes(),
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = prepared_operation(
            "read_tool_result",
            &args,
            vec![(PermissionAction::Read, "read")],
            vec![resource],
            &fs_cap::cwd_context_bytes(&ctx.cwd)?,
        )?;
        let normalized =
            serde_json::to_value(&args).map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            normalized,
            None,
            Box::new(ReadToolResultExecutor {
                engine: self.engine.clone(),
                session: ctx.session,
                args,
            }),
        )?
        .with_policy_labels(vec![label])
    }
}

#[async_trait]
impl PreparedExecutor for ReadToolResultExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        if context.session != self.session {
            return Err(ToolError::operation_changed("session changed"));
        }
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution("tool result read was cancelled"));
        }
        let page = self.engine.read_tool_result(
            context.session,
            self.args.tool_call_id,
            self.args.stream.map(ResultStream::as_str),
            self.args.offset,
            self.args.limit.expect("normalized limit"),
        )?;
        let metadata = serde_json::json!({
            "tool_call_id":self.args.tool_call_id,
            "offset":self.args.offset,
            "limit":self.args.limit,
            "next_offset":page.next_offset_lines,
            "source":page.source,
            "stream":self.args.stream.map(ResultStream::as_str),
        });
        Ok(ToolResult {
            title: safe_title("Tool result page"),
            output: page.content,
            metadata,
            truncation: None,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn readback_declares_absolute_truncation_opt_out() {
        assert_eq!(
            super::ReadToolResultProvider::tool_spec().result_truncation,
            cookie_agent_engine::ToolResultTruncationPolicy::OptOut
        );
    }
}
