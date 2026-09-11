use async_trait::async_trait;
use cookie_agent_engine::{
    AgentMessageInvocation, Engine, PreparedExecutor, PreparedTool, SessionToolContext, ToolCall,
    ToolCompletion, ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider,
    ToolSpec,
};
use cookie_agent_protocol::{
    PermissionAction, PersistedToolResult as ToolResult, ProducerDeliveryMode, SessionId,
};
use serde::{Deserialize, Serialize};

use crate::{fs_cap, prepared_operation, prepared_resource, safe_title};

fn default_mode() -> ProducerDeliveryMode {
    ProducerDeliveryMode::Steer
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MessageArgs {
    recipient_session_id: SessionId,
    body: String,
    #[serde(default = "default_mode")]
    mode: ProducerDeliveryMode,
}

pub struct MessageToolProvider {
    engine: Engine,
}

impl MessageToolProvider {
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self { engine }
    }

    fn parse(args: &serde_json::Value) -> Result<MessageArgs, ToolError> {
        let value: MessageArgs = serde_json::from_value(args.clone())
            .map_err(|error| ToolError::execution(error.to_string()))?;
        if value.body.trim().is_empty() {
            return Err(ToolError::execution("body must not be empty"));
        }
        Ok(value)
    }
}

#[async_trait]
impl ToolProvider for MessageToolProvider {
    fn provider_id(&self) -> &'static str {
        "builtin.message"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            output: Default::default(),
            concurrency: cookie_agent_engine::ToolConcurrency::Parallel,
            result_truncation: cookie_agent_engine::ToolResultTruncationPolicy::Bounded,
            name: "send_message".into(),
            permission_name: "message".into(),
            description: "Send a message to an agent in the current delegation tree.".into(),
            parameters: serde_json::json!({
                "type":"object","additionalProperties":false,
                "properties":{
                    "recipient_session_id":{"type":"string"},
                    "body":{"type":"string","minLength":1},
                    "mode":{"type":"string","enum":["steer","queue"],"default":"steer"}
                },"required":["recipient_session_id","body"]
            }),
        }])
    }

    fn get_permission_name(_name: &str) -> Result<&'static str, ToolError> {
        Ok("message")
    }

    fn get_permission_resource(
        &self,
        _name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let _ = Self::parse(arguments)?;
        Ok(("message", Some("*".into())))
    }

    fn get_display_argument(
        &self,
        _name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Ok(Self::parse(arguments)?.recipient_session_id.to_string())
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        if call.name != "send_message" {
            return Err(ToolError::execution(
                "message provider received another tool",
            ));
        }
        let args = Self::parse(&call.arguments)?;
        let relationship = self
            .engine
            .message_relationship(ctx.session, args.recipient_session_id)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let cwd = fs_cap::cwd_context_bytes(&ctx.cwd)?;
        let operation = prepared_operation(
            "send_message",
            &args,
            vec![(PermissionAction::Message, relationship)],
            vec![prepared_resource(
                PermissionAction::Message,
                relationship,
                relationship.as_bytes(),
                relationship.as_bytes(),
                cookie_agent_protocol::PreparedBindingLifetime::RestartStable,
                cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
            )?],
            &cwd,
        )?;
        let normalized =
            serde_json::to_value(&args).map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            normalized,
            None,
            Box::new(MessageExecutor {
                engine: self.engine.clone(),
                args,
                call_id: call.id,
            }),
        )?
        .with_policy_labels(vec![relationship.into()])
    }
}

struct MessageExecutor {
    engine: Engine,
    args: MessageArgs,
    call_id: cookie_agent_protocol::ToolCallId,
}

#[async_trait]
impl PreparedExecutor for MessageExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let handle = self
            .engine
            .send_agent_message(AgentMessageInvocation {
                sender_session_id: context.session,
                sender_run_id: context.run,
                sender_tool_call_id: self.call_id,
                recipient_session_id: self.args.recipient_session_id,
                body: self.args.body,
                mode: self.args.mode,
            })
            .await
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let metadata = serde_json::json!({"message_id": handle.message_id, "mode": handle.mode, "recipient_state": handle.recipient_state.as_str()});
        Ok(ToolCompletion::single(ToolResult {
            display: None,
            retained_output: None,
            title: safe_title("Message sent"),
            output: format!("Message accepted for {}.", handle.recipient_state.as_str()),
            metadata,
            truncation: None,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        }))
    }
}
