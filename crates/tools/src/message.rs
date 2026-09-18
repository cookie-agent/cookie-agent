use async_trait::async_trait;
use cookie_agent_engine::{
    AgentMessageInvocation, Engine, MESSAGE_INVALID_ARGUMENTS, MESSAGE_INVALID_BODY,
    PreparedExecutor, PreparedTool, SessionToolContext, SubagentScope, ToolCall, ToolCompletion,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
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
    /// Handle or full UUID of the recipient.
    to: String,
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

    /// The frozen `send_message` surface. The recipient argument is `to`; there
    /// is deliberately no compatibility alias for the earlier
    /// `recipient_session_id` name, which would otherwise be frozen into every
    /// normalized-arguments record this call produces.
    fn spec() -> ToolSpec {
        ToolSpec {
            output: Default::default(),
            concurrency: cookie_agent_engine::ToolConcurrency::Parallel,
            result_truncation: cookie_agent_engine::ToolResultTruncationPolicy::Bounded,
            name: "send_message".into(),
            permission_name: "message".into(),
            description: "Send a message to another agent in your session tree. Recipient accepts a subagent handle or a full UUID.".into(),
            parameters: serde_json::json!({
                "type":"object","additionalProperties":false,
                "properties":{
                    "to":{"type":"string","description":"Handle or full UUID of the recipient agent."},
                    "body":{"type":"string","minLength":1},
                    "mode":{"type":"string","enum":["steer","queue"],"default":"steer"}
                },"required":["to","body"]
            }),
        }
    }

    /// Parses one call, mapping every malformed shape onto a stable code so the
    /// text a sending model sees never depends on serde's message formatting.
    fn parse(args: &serde_json::Value) -> Result<MessageArgs, ToolError> {
        let value: MessageArgs = serde_json::from_value(args.clone())
            .map_err(|_| ToolError::execution(MESSAGE_INVALID_ARGUMENTS))?;
        if value.body.trim().is_empty() {
            return Err(ToolError::execution(MESSAGE_INVALID_BODY));
        }
        Ok(value)
    }

    /// Resolves the recipient: a full UUID passes through unchanged (so the
    /// engine's `not_tree_peer` / `self_send` guards keep their stable codes);
    /// anything else is a handle resolved within the sender's tree, with the
    /// self-repairing candidate-list error on failure.
    fn resolve_recipient(
        engine: &Engine,
        sender: SessionId,
        reference: &str,
    ) -> Result<SessionId, ToolError> {
        if let Ok(id) = reference.parse::<SessionId>() {
            return Ok(id);
        }
        engine
            .resolve_subagent_target(sender, reference, SubagentScope::TreePeers)
            .map_err(|error| ToolError::execution(error.to_string()))
    }
}

#[async_trait]
impl ToolProvider for MessageToolProvider {
    fn provider_id(&self) -> &'static str {
        "builtin.message"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![Self::spec()])
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
        Ok(Self::parse(arguments)?.to.to_string())
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
        let recipient = Self::resolve_recipient(&self.engine, ctx.session, &args.to)?;
        let relationship = self
            .engine
            .message_relationship(ctx.session, recipient)
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
        let recipient =
            MessageToolProvider::resolve_recipient(&self.engine, context.session, &self.args.to)?;
        let handle = self
            .engine
            .send_agent_message(AgentMessageInvocation {
                sender_session_id: context.session,
                sender_run_id: context.run,
                sender_tool_call_id: self.call_id,
                recipient_session_id: recipient,
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

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::SessionId;

    use super::{MessageArgs, MessageToolProvider};

    fn arguments(body: &str) -> serde_json::Value {
        serde_json::json!({"to": SessionId::new_v7().to_string(), "body": body})
    }

    /// The execution-message wrapper keeps the stable code verbatim.
    fn rejection(arguments: &serde_json::Value) -> String {
        let rendered = MessageToolProvider::parse(arguments)
            .expect_err("arguments must be rejected")
            .to_string();
        rendered
            .strip_prefix("tool failed: ")
            .unwrap_or(&rendered)
            .to_owned()
    }

    #[test]
    fn surface_addresses_the_recipient_with_to() {
        let spec = MessageToolProvider::spec().parameters;
        assert_eq!(spec["properties"]["to"]["type"], "string");
        assert_eq!(spec["required"], serde_json::json!(["to", "body"]));
        assert_eq!(spec["additionalProperties"], false);
        assert!(spec["properties"].get("recipient_session_id").is_none());
    }

    #[test]
    fn parse_accepts_the_documented_shape_and_defaults_the_mode() {
        let recipient = SessionId::new_v7();
        let parsed = MessageToolProvider::parse(&serde_json::json!({
            "to": recipient.to_string(),
            "body":"chain start"
        }))
        .expect("valid arguments");
        assert_eq!(parsed.to, recipient.to_string());
        assert_eq!(parsed.body, "chain start");
        assert_eq!(
            parsed.mode,
            cookie_agent_protocol::ProducerDeliveryMode::Steer
        );
        let queued = MessageToolProvider::parse(&serde_json::json!({
            "to": recipient.to_string(),
            "body":"chain start",
            "mode":"queue"
        }))
        .expect("queued arguments");
        assert_eq!(
            queued.mode,
            cookie_agent_protocol::ProducerDeliveryMode::Queue
        );
    }

    #[test]
    fn normalized_arguments_record_the_to_field_only() {
        let recipient = SessionId::new_v7();
        let parsed = MessageToolProvider::parse(&serde_json::json!({
            "to": recipient.to_string(),
            "body":"chain start"
        }))
        .expect("valid arguments");
        let normalized = serde_json::to_value(&parsed).expect("normalized arguments");
        assert_eq!(normalized["to"], recipient.to_string());
        assert!(normalized.get("recipient_session_id").is_none());
    }

    #[test]
    fn every_malformed_shape_reports_one_stable_code() {
        let recipient = SessionId::new_v7();
        for arguments in [
            // The pre-rename argument name is unknown, not an alias.
            &serde_json::json!({"recipient_session_id": recipient.to_string(), "body":"mail"}),
            &serde_json::json!({"body":"missing recipient"}),
            // A non-UUID string is a loose-typed reference the resolver rejects
            // later, not a parse error.
            &serde_json::json!({"to":recipient.to_string(),"body":"mail","extra":true}),
            &serde_json::json!({"to":recipient.to_string(),"body":42}),
            &serde_json::json!({"to":recipient.to_string(),"body":"mail","mode":"later"}),
            &serde_json::json!([]),
        ] {
            assert_eq!(
                rejection(arguments),
                cookie_agent_engine::MESSAGE_INVALID_ARGUMENTS,
                "unstable parse error for {arguments}"
            );
        }
    }

    #[test]
    fn blank_bodies_keep_their_own_stable_code() {
        for body in ["", "   ", "\n\t"] {
            assert_eq!(
                rejection(&arguments(body)),
                cookie_agent_engine::MESSAGE_INVALID_BODY
            );
        }
    }

    #[test]
    fn argument_shape_stays_private_to_the_tool() {
        // The executor forwards `to` as the engine's recipient field; the model
        // never sees that name.
        let normalized = serde_json::to_value(MessageArgs {
            to: SessionId::new_v7().to_string(),
            body: "mail".into(),
            mode: cookie_agent_protocol::ProducerDeliveryMode::Queue,
        })
        .expect("serializable");
        let mut keys = normalized
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(keys, vec!["body", "mode", "to"]);
    }
}
