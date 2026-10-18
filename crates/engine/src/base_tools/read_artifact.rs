//! `read_artifact`: pages through output the session tree retained.
//!
//! A base-layer tool: it reads only artifacts stored in the calling session's
//! own tree, so it needs no permission rule. It is always published and every
//! call is allowed: the tool declares no permission.

use async_trait::async_trait;
use cookie_agent_protocol::{
    ApprovalResourceSource, ArtifactReadTarget, PermissionAction,
    PersistedToolResult as ToolResult, PreparedBindingLifetime, SessionId,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{parse_args, prepared_operation, prepared_resource, safe_title, schema, tool_error};

const TOOL_NAME: &str = "read_artifact";
/// Most lines one page returns; larger requests are clamped to it.
const MAX_LIMIT: usize = 2_000;
/// Hex digits of the digest shown in the tool call header.
const DISPLAY_DIGEST_CHARS: usize = 12;

#[derive(Debug, Default)]
pub struct ReadArtifactTool;

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadArtifactArgs {
    /// The artifact's 64-character lowercase SHA-256 digest, as a truncation hint names it.
    artifact: String,
    /// Named stream of a multi-stream output, such as `stdout`, as a truncation hint names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stream: Option<String>,
    /// Zero-based line offset. Defaults to 0.
    offset: Option<usize>,
    /// Maximum number of lines to return, at most 2000. Defaults to 2000.
    limit: Option<usize>,
}

impl ReadArtifactArgs {
    fn target(&self) -> Result<ArtifactReadTarget, ToolError> {
        ArtifactReadTarget::parse(&self.artifact, self.stream.as_deref())
            .map_err(ToolError::execution)
    }

    /// `<digest>` or `<digest>/<stream>`: the permission resource and policy label.
    fn label(&self) -> String {
        match &self.stream {
            Some(stream) => format!("{}/{stream}", self.artifact),
            None => self.artifact.clone(),
        }
    }
}

impl ReadArtifactTool {
    #[must_use]
    pub fn spec() -> ToolSpec {
        ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: cookie_agent_runtime_api::ToolConcurrency::Parallel,
            result_truncation: cookie_agent_runtime_api::ToolResultTruncationPolicy::OptOut,
            name: TOOL_NAME.into(),
            permission_name: None,
            description: "Read retained tool output by line, using a zero-based offset. Truncated results and elided history name the artifact (and stream) to read in a read_artifact hint; continue from the offset it gives.".into(),
            parameters: schema::<ReadArtifactArgs>(),
        }
    }
}

#[async_trait]
impl ToolProvider for ReadArtifactTool {
    fn provider_id(&self) -> &'static str {
        "builtin.read_artifact"
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![Self::spec()])
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        match tool_name {
            TOOL_NAME => Ok(None),
            _ => Err(ToolError::execution(
                "read_artifact provider received another tool",
            )),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        let permission_name = Self::get_permission_name(name)?;
        let args: ReadArtifactArgs = parse_args(TOOL_NAME, arguments.clone())?;
        args.target()?;
        Ok((permission_name, Some(args.label())))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Self::get_permission_name(name)?;
        let args: ReadArtifactArgs = parse_args(TOOL_NAME, arguments.clone())?;
        let target = args.target()?;
        let mut display = target.digest.as_str()[..DISPLAY_DIGEST_CHARS].to_owned();
        if let Some(stream) = &target.stream {
            display.push('/');
            display.push_str(stream);
        }
        let window = match (args.offset, args.limit) {
            (Some(offset), Some(limit)) => Some(format!("offset={offset}, limit={limit}")),
            (Some(offset), None) => Some(format!("offset={offset}")),
            (None, Some(limit)) => Some(format!("limit={limit}")),
            (None, None) => None,
        };
        if let Some(window) = window {
            display.push_str(" [");
            display.push_str(&window);
            display.push(']');
        }
        Ok(display)
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Self::get_permission_name(&call.name)?;
        let mut args: ReadArtifactArgs = parse_args(TOOL_NAME, call.arguments)?;
        let target = args.target()?;
        let limit = args.limit.unwrap_or(MAX_LIMIT);
        if limit == 0 {
            return Err(ToolError::execution("limit must be positive"));
        }
        args.offset = Some(args.offset.unwrap_or(0));
        args.limit = Some(limit.min(MAX_LIMIT));
        let label = args.label();
        let binding = serde_json::to_vec(&args).map_err(tool_error)?;
        let resource = prepared_resource(
            PermissionAction::READ,
            "artifact",
            label.as_bytes(),
            &binding,
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = prepared_operation(
            TOOL_NAME,
            &args,
            vec![(PermissionAction::READ, "read")],
            vec![resource],
            ctx.session.to_string().as_bytes(),
        )?;
        let normalized = serde_json::to_value(&args).map_err(tool_error)?;
        PreparedTool::new(
            operation,
            normalized,
            None,
            Box::new(ReadArtifactExecutor {
                target,
                offset: args.offset.expect("normalized offset"),
                limit: args.limit.expect("normalized limit"),
                session: ctx.session,
            }),
        )?
        .with_policy_labels(vec![label])
    }
}

struct ReadArtifactExecutor {
    target: ArtifactReadTarget,
    offset: usize,
    limit: usize,
    session: SessionId,
}

#[async_trait]
impl PreparedExecutor for ReadArtifactExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_runtime_api::ToolCompletion, ToolError> {
        if context.session != self.session {
            return Err(ToolError::operation_changed("session changed"));
        }
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution("artifact read cancelled"));
        }
        let (offset, limit) = (self.offset as u64, self.limit as u64);
        let artifact = self.target.digest.to_string();
        let stream = self.target.stream.clone();
        let page = context.read_artifact(self.target, offset, limit).await?;
        Ok(cookie_agent_runtime_api::ToolCompletion::single(
            ToolResult {
                title: safe_title("Artifact page"),
                output: page.content,
                display: Vec::new(),
                background: None,
                retained_output: None,
                metadata: serde_json::json!({
                    "artifact": artifact,
                    "stream": stream,
                    "offset": offset,
                    "limit": limit,
                    "next_offset": page.next_offset_lines,
                    "source": page.source,
                }),
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{RunId, ToolCallId};

    use super::*;

    fn context(session: SessionId, run: RunId) -> ToolPreparationContext {
        ToolPreparationContext {
            session,
            run,
            origin: cookie_agent_runtime_api::ToolOrigin::Base,
            turn_context: crate::base_tools::test_turn_context(),
        }
    }

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new_v7(),
            name: TOOL_NAME.into(),
            arguments,
        }
    }

    #[test]
    fn read_artifact_needs_no_permission() {
        assert_eq!(ReadArtifactTool::spec().permission_name, None);
        assert_eq!(
            ReadArtifactTool::get_permission_name(TOOL_NAME).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn read_artifact_pages_the_session_trees_artifact_without_retaining_output() {
        let root = tempfile::tempdir().unwrap();
        let producer = crate::testing::tool_execution_context(
            root.path().join("artifacts"),
            crate::base_tools::test_turn_context(),
        )
        .unwrap();
        let artifact = producer
            .retain_validated_attachment("text/plain", None, b"zero\none\ntwo\n")
            .unwrap();
        let reader = crate::testing::tool_execution_context(
            root.path().join("artifacts"),
            crate::base_tools::test_turn_context(),
        )
        .unwrap();
        let digest = artifact.sha256.to_string();
        let tool = ReadArtifactTool;
        let args = serde_json::json!({"artifact": digest, "offset": 1, "limit": 1});
        assert_eq!(
            tool.get_permission_resource(TOOL_NAME, &args).unwrap(),
            (None, Some(digest.clone()))
        );
        let prepared = tool
            .prepare(context(reader.session, reader.run), call(args))
            .await
            .unwrap();
        assert_eq!(prepared.policy_labels(), [Some(digest.clone())]);
        let result = prepared.execute_for_test(reader).await.unwrap();
        assert_eq!(result.output, "one\n");
        assert_eq!(result.metadata["next_offset"], 2);
        assert_eq!(result.metadata["artifact"], digest);
        assert!(result.retained_output.is_none());
    }

    #[tokio::test]
    async fn read_artifact_rejects_malformed_targets_and_limits() {
        let digest = "a".repeat(64);
        for arguments in [
            serde_json::json!({"artifact": format!("artifact://{digest}")}),
            serde_json::json!({"artifact": format!("sha256/{digest}")}),
            serde_json::json!({"artifact": "A".repeat(64)}),
            serde_json::json!({"artifact": digest, "stream": "../bad"}),
            serde_json::json!({"artifact": digest, "stream": "out/more"}),
            serde_json::json!({"artifact": digest, "limit": 0}),
            serde_json::json!({"artifact": digest, "filePath": "x"}),
        ] {
            assert!(
                ReadArtifactTool
                    .prepare(
                        context(SessionId::new_v7(), RunId::new_v7()),
                        call(arguments.clone())
                    )
                    .await
                    .is_err(),
                "{arguments}"
            );
        }
    }

    #[tokio::test]
    async fn read_artifact_clamps_the_page_limit() {
        let digest = "a".repeat(64);
        let prepared = ReadArtifactTool
            .prepare(
                context(SessionId::new_v7(), RunId::new_v7()),
                call(serde_json::json!({"artifact": digest, "stream": "stdout", "limit": 5_000})),
            )
            .await
            .unwrap();
        assert_eq!(prepared.normalized_arguments()["limit"], MAX_LIMIT);
        assert_eq!(prepared.normalized_arguments()["offset"], 0);
        assert_eq!(prepared.policy_labels(), [Some(format!("{digest}/stdout"))]);
    }
}
