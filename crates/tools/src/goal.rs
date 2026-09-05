use async_trait::async_trait;
use cookie_agent_engine::{
    Engine, PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError,
    ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::{
    ApprovalResourceSource, GoalGetParams, GoalUpdateParams, PermissionAction,
    PersistedToolResult as ToolResult, PreparedBindingLifetime, PreparedOperationIdentity,
    SessionId,
};

use crate::{parse_args, prepared_operation, prepared_resource, safe_title, schema, tool_error};

pub struct GoalTools {
    engine: Engine,
}

enum GoalExecutor {
    Get {
        engine: Engine,
        session: SessionId,
    },
    Update {
        engine: Engine,
        session: SessionId,
        params: GoalUpdateParams,
    },
}

impl GoalTools {
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self { engine }
    }

    fn get_spec() -> ToolSpec {
        ToolSpec {
            concurrency: cookie_agent_engine::ToolConcurrency::Parallel,
            result_truncation: cookie_agent_engine::ToolResultTruncationPolicy::OptOut,
            name: "goal_get".into(),
            permission_name: "read".into(),
            description: "Read the root session's objective, lifecycle status, revision, and full checklist. The root checklist is authoritative; verify work directly or through subagents before treating an item as finished.".into(),
            parameters: schema::<GoalGetParams>(),
        }
    }

    fn update_spec() -> ToolSpec {
        ToolSpec {
            concurrency: Default::default(),
            result_truncation: cookie_agent_engine::ToolResultTruncationPolicy::OptOut,
            name: "goal_update".into(),
            permission_name: "write".into(),
            description: "Replace the current root goal's full checklist. The current active or paused goal is selected when this update executes, even if the goal changed since this run started. The last accepted replacement wins. This cannot change the objective or issue lifecycle commands; a nonempty all-finished checklist completes the goal. Verify evidence directly or through subagents before marking items finished; the root checklist is authoritative over subagent self-reports.".into(),
            parameters: schema::<GoalUpdateParams>(),
        }
    }

    fn update_operation(
        session: SessionId,
        params: &GoalUpdateParams,
    ) -> Result<PreparedOperationIdentity, ToolError> {
        let binding = serde_json::to_vec(&(session, params)).map_err(tool_error)?;
        let resource = prepared_resource(
            PermissionAction::Write,
            "goal",
            b"goal:current",
            &binding,
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        prepared_operation(
            "goal_update",
            params,
            vec![(PermissionAction::Write, "replace_checklist")],
            vec![resource],
            session.to_string().as_bytes(),
        )
    }
}

#[async_trait]
impl ToolProvider for GoalTools {
    fn provider_id(&self) -> &'static str {
        "builtin.goal"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![Self::get_spec(), Self::update_spec()])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "goal_get" => Ok("read"),
            "goal_update" => Ok("write"),
            _ => Err(ToolError::execution("goal provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        match name {
            "goal_get" => {
                let _: GoalGetParams = parse_args(name, arguments.clone())?;
                Ok(("read", Some("goal:current".into())))
            }
            "goal_update" => {
                let _: GoalUpdateParams = parse_args(name, arguments.clone())?;
                Ok(("write", Some("goal:current".into())))
            }
            _ => Err(ToolError::execution("goal provider received another tool")),
        }
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        match name {
            "goal_get" => {
                let _: GoalGetParams = parse_args(name, arguments.clone())?;
                Ok("current goal".into())
            }
            "goal_update" => {
                let _: GoalUpdateParams = parse_args(name, arguments.clone())?;
                Ok("current goal".into())
            }
            _ => Err(ToolError::execution("goal provider received another tool")),
        }
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let (normalized_arguments, operation, policy_label, executor) = match call.name.as_str() {
            "goal_get" => {
                let params: GoalGetParams = parse_args(&call.name, call.arguments)?;
                let label = "goal:current".to_owned();
                let resource = prepared_resource(
                    PermissionAction::Read,
                    "goal",
                    label.as_bytes(),
                    ctx.session.to_string().as_bytes(),
                    PreparedBindingLifetime::RestartStable,
                    ApprovalResourceSource::PrimaryOperation,
                )?;
                let operation = prepared_operation(
                    "goal_get",
                    &params,
                    vec![(PermissionAction::Read, "get")],
                    vec![resource],
                    ctx.session.to_string().as_bytes(),
                )?;
                (
                    serde_json::to_value(params).map_err(tool_error)?,
                    operation,
                    label,
                    GoalExecutor::Get {
                        engine: self.engine.clone(),
                        session: ctx.session,
                    },
                )
            }
            "goal_update" => {
                let params: GoalUpdateParams = parse_args(&call.name, call.arguments)?;
                let label = "goal:current".to_owned();
                let normalized = serde_json::to_value(&params).map_err(tool_error)?;
                let operation = Self::update_operation(ctx.session, &params)?;
                (
                    normalized,
                    operation,
                    label,
                    GoalExecutor::Update {
                        engine: self.engine.clone(),
                        session: ctx.session,
                        params,
                    },
                )
            }
            _ => return Err(ToolError::execution("goal provider received another tool")),
        };

        PreparedTool::new(operation, normalized_arguments, None, Box::new(executor))?
            .with_policy_labels(vec![policy_label])
    }
}

#[async_trait]
impl PreparedExecutor for GoalExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution("goal tool was cancelled"));
        }
        let (title, result) = match *self {
            Self::Get { engine, session } => {
                if context.session != session {
                    return Err(ToolError::operation_changed("session changed"));
                }
                (
                    "Current goal",
                    serde_json::to_value(engine.goal_get(session).await.map_err(tool_error)?)
                        .map_err(tool_error)?,
                )
            }
            Self::Update {
                engine,
                session,
                params,
            } => {
                if context.session != session {
                    return Err(ToolError::operation_changed("session changed"));
                }
                (
                    "Updated goal checklist",
                    serde_json::to_value(
                        engine
                            .goal_update(session, params)
                            .await
                            .map_err(tool_error)?,
                    )
                    .map_err(tool_error)?,
                )
            }
        };
        Ok(ToolResult {
            title: safe_title(title),
            output: serde_json::to_string_pretty(&result).map_err(tool_error)?,
            metadata: result,
            truncation: None,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_engine::ToolProvider;

    use super::GoalTools;

    #[test]
    fn goal_specs_use_strict_protocol_schemas_and_root_authority_guidance() {
        let get = GoalTools::get_spec();
        let update = GoalTools::update_spec();
        assert_eq!(get.permission_name, "read");
        assert_eq!(update.permission_name, "write");
        assert_eq!(
            get.result_truncation,
            cookie_agent_engine::ToolResultTruncationPolicy::OptOut
        );
        assert_eq!(
            update.result_truncation,
            cookie_agent_engine::ToolResultTruncationPolicy::OptOut
        );
        assert_eq!(get.parameters["additionalProperties"], false);
        assert_eq!(update.parameters["additionalProperties"], false);
        assert_eq!(update.parameters["required"], serde_json::json!(["items"]));
        assert_eq!(
            update.parameters["properties"].as_object().unwrap().len(),
            1
        );
        assert!(!update.description.contains("expected revision"));
        assert!(!update.description.contains("goal ID"));
        assert!(
            update
                .description
                .contains("last accepted replacement wins")
        );
        assert!(get.description.contains("root checklist is authoritative"));
        assert!(update.description.contains("before marking items finished"));
    }

    #[test]
    fn strict_goal_arguments_reject_unknown_fields() {
        let get = serde_json::json!({"unexpected":true});
        assert!(
            crate::parse_args::<cookie_agent_protocol::GoalGetParams>("goal_get", get).is_err()
        );
        assert!(
            GoalTools::get_permission_name("goal_lifecycle").is_err(),
            "the model must not receive a lifecycle tool"
        );
        let goal_id = cookie_agent_protocol::GoalId::new_v7();
        for arguments in [
            serde_json::json!({"items":[],"expected_revision":0}),
            serde_json::json!({"items":[],"goal_id":goal_id}),
            serde_json::json!({"items":[],"id":"old-item"}),
            serde_json::json!({"items":[{"id":"old-item","description":"Verify","finished":false}]}),
        ] {
            assert!(
                crate::parse_args::<cookie_agent_protocol::GoalUpdateParams>(
                    "goal_update",
                    arguments
                )
                .is_err()
            );
        }
        let arguments = serde_json::json!({"items":[{"description":"Verify","finished":false},{"description":"Verify","finished":false}]});
        let parsed = crate::parse_args::<cookie_agent_protocol::GoalUpdateParams>(
            "goal_update",
            arguments.clone(),
        )
        .unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), arguments);
    }

    #[test]
    fn update_fingerprint_binds_session_and_exact_items_only() {
        use cookie_agent_protocol::{
            GoalItem, GoalUpdateParams, OperationFingerprint, SessionId, Sha256Digest,
        };

        let session = SessionId::new_v7();
        let params = GoalUpdateParams {
            items: vec![GoalItem {
                description: "Verify".into(),
                finished: false,
            }],
        };
        let operation = GoalTools::update_operation(session, &params).unwrap();
        let fingerprint = OperationFingerprint::from_prepared_operation(&operation);
        assert_eq!(
            fingerprint,
            OperationFingerprint::from_prepared_operation(
                &GoalTools::update_operation(session, &params).unwrap()
            )
        );
        assert_ne!(
            fingerprint,
            OperationFingerprint::from_prepared_operation(
                &GoalTools::update_operation(SessionId::new_v7(), &params).unwrap()
            )
        );
        let mut changed = params.clone();
        changed.items[0].finished = true;
        assert_ne!(
            fingerprint,
            OperationFingerprint::from_prepared_operation(
                &GoalTools::update_operation(session, &changed).unwrap()
            )
        );
        changed = params.clone();
        changed.items[0].description = "Verify again".into();
        assert_ne!(
            GoalTools::update_operation(session, &params)
                .unwrap()
                .resources()[0]
                .binding_digest,
            GoalTools::update_operation(session, &changed)
                .unwrap()
                .resources()[0]
                .binding_digest
        );
        let wire = serde_json::to_value(&operation).unwrap();
        assert_eq!(
            wire["normalized_arguments_digest"],
            serde_json::to_value(Sha256Digest::of_bytes(
                &serde_json::to_vec(&params).unwrap()
            ))
            .unwrap()
        );
        assert_eq!(
            wire["execution_context_digest"],
            serde_json::to_value(Sha256Digest::of_bytes(session.to_string().as_bytes())).unwrap()
        );
        assert_eq!(
            operation.resources()[0].capability,
            cookie_agent_protocol::PermissionAction::Write
        );
    }
}
