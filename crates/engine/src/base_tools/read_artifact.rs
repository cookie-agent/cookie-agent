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
    ToolPreparationContext, ToolProvider, ToolSpec, text_tools::LineWindow,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    base_presentation, parse_args, prepared_operation, prepared_resource, safe_title, schema,
    tool_error,
};

const TOOL_NAME: &str = "read_artifact";
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
    /// The 1-based line to start at. Defaults to 1.
    offset: Option<usize>,
    /// The most lines to return. Defaults to 2000.
    limit: Option<usize>,
}

/// What the model reads about `read_artifact`.
const DESCRIPTION: &str = "Read retained tool output by line, as `read` reads a file. A truncated result or elided history names the artifact (and stream) to read in a read_artifact hint, with the 1-based `offset` of the first line it left out: read from there. Lines come back numbered, `N: text`, counting from 1. `offset` is the 1-based line to start at (default 1) and `limit` the most lines to return (default 2000). A line longer than 4 KiB is cut and ends with `... [line truncated: N more bytes]`, and a page stops before it passes the result size limit (about 2 MiB). A page that leaves lines out ends with a note such as `[120 more lines; continue with offset=2001]`: read again from that offset. An offset past the last line is an error.";

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

/// The artifact as titles and errors name it: the start of its digest,
/// and its stream.
fn shown(target: &ArtifactReadTarget) -> String {
    let digest = &target.digest.as_str()[..DISPLAY_DIGEST_CHARS];
    match &target.stream {
        Some(stream) => format!("{digest}/{stream}"),
        None => digest.to_owned(),
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
            description: DESCRIPTION.into(),
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
        let mut display = shown(&target);
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

    fn presentation(&self, call: &ToolCall) -> cookie_agent_protocol::PreparedPresentation {
        let (Ok(primary), Ok(args)) = (
            self.get_display_argument(&call.name, &call.arguments),
            parse_args::<ReadArtifactArgs>(TOOL_NAME, call.arguments.clone()),
        ) else {
            return cookie_agent_runtime_api::tool_title_only(&call.name);
        };
        let mut entries = vec![("artifact", args.artifact.clone())];
        entries.extend(args.stream.clone().map(|stream| ("stream", stream)));
        entries.extend(args.offset.map(|offset| ("offset", offset.to_string())));
        entries.extend(args.limit.map(|limit| ("limit", limit.to_string())));
        let shown = match &args.stream {
            Some(stream) => format!("{}/{stream}", &args.artifact[..DISPLAY_DIGEST_CHARS]),
            None => args.artifact[..DISPLAY_DIGEST_CHARS].to_owned(),
        };
        base_presentation(
            &format!("Read artifact {shown}"),
            cookie_agent_protocol::ToolHint::FILE_READ,
            &primary,
            entries,
        )
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Self::get_permission_name(&call.name)?;
        let mut args: ReadArtifactArgs = parse_args(TOOL_NAME, call.arguments)?;
        let target = args.target()?;
        let window = LineWindow::new(args.offset, args.limit)?;
        args.offset = Some(window.offset);
        args.limit = Some(window.limit);
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
                window,
                session: ctx.session,
            }),
        )?
        .with_policy_labels(vec![label])
    }
}

struct ReadArtifactExecutor {
    target: ArtifactReadTarget,
    window: LineWindow,
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
        let artifact = self.target.digest.to_string();
        let stream = self.target.stream.clone();
        let shown = shown(&self.target);
        let window = self.window;
        let read = context.read_artifact(self.target, window).await?;
        let page = read.page;
        let display = cookie_agent_runtime_api::display::fit_block(
            page.excerpt_block(shown.clone(), None),
            None,
        )?;
        Ok(cookie_agent_runtime_api::ToolCompletion::single(
            ToolResult {
                title: safe_title(format!("Read artifact {shown}")),
                output: page.text_with_continuation(),
                display: vec![cookie_agent_protocol::Open::Known(display)],
                background: None,
                retained_output: None,
                metadata: serde_json::json!({
                    "artifact": artifact,
                    "stream": stream,
                    "offset": window.offset,
                    "limit": window.limit,
                    "total_lines": page.total_lines,
                    "next_offset": page.next_offset,
                    "source": read.source,
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
        let args = serde_json::json!({"artifact": digest, "offset": 2, "limit": 1});
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
        // Line 2, numbered as `read` numbers it, and where to continue.
        assert_eq!(
            result.output,
            "2: one\n[1 more line; continue with offset=3]"
        );
        assert_eq!(result.metadata["next_offset"], 3);
        assert_eq!(result.metadata["offset"], 2);
        assert_eq!(result.metadata["total_lines"], 3);
        assert_eq!(result.metadata["artifact"], digest);
        assert_eq!(
            result.title.as_str(),
            format!("Read artifact {}", &digest[..DISPLAY_DIGEST_CHARS])
        );
        assert_eq!(
            result.display,
            [cookie_agent_protocol::Open::Known(
                cookie_agent_protocol::DisplayBlock::FileExcerpt {
                    path: digest[..DISPLAY_DIGEST_CHARS].to_owned(),
                    language: None,
                    start_line: 2,
                    lines: vec!["one".into()],
                    total_lines: Some(3),
                    omitted_lines: None,
                    full: None,
                }
            )]
        );
        assert!(result.retained_output.is_none());
    }

    /// Reads `artifact` from line `offset`, at most `limit` lines.
    async fn page(
        root: &std::path::Path,
        digest: &str,
        offset: usize,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        let reader = crate::testing::tool_execution_context(
            root.join("artifacts"),
            crate::base_tools::test_turn_context(),
        )
        .unwrap();
        ReadArtifactTool
            .prepare(
                context(reader.session, reader.run),
                call(serde_json::json!({"artifact": digest, "offset": offset, "limit": limit})),
            )
            .await?
            .execute_for_test(reader)
            .await
    }

    #[tokio::test]
    async fn read_artifact_pages_from_line_one_to_the_last_line() {
        let root = tempfile::tempdir().unwrap();
        let producer = crate::testing::tool_execution_context(
            root.path().join("artifacts"),
            crate::base_tools::test_turn_context(),
        )
        .unwrap();
        let long = "x".repeat(cookie_agent_runtime_api::text_tools::MAX_LINE_BYTES + 3);
        let digest = producer
            .retain_validated_attachment("text/plain", None, format!("a\nb\n{long}").as_bytes())
            .unwrap()
            .sha256
            .to_string();
        let first = page(root.path(), &digest, 1, 2).await.unwrap();
        assert_eq!(
            first.output,
            "1: a\n2: b\n[1 more line; continue with offset=3]"
        );
        // The last page cuts its long line as `read` does, and ends there.
        let last = page(root.path(), &digest, 3, 2).await.unwrap();
        assert_eq!(
            last.output,
            format!(
                "3: {}... [line truncated: 3 more bytes]\n",
                "x".repeat(cookie_agent_runtime_api::text_tools::MAX_LINE_BYTES)
            )
        );
        assert!(last.metadata["next_offset"].is_null());
        assert_eq!(
            page(root.path(), &digest, 4, 2)
                .await
                .unwrap_err()
                .message(),
            format!(
                "offset=4 is past the end of artifact {}, which has 3 lines",
                &digest[..DISPLAY_DIGEST_CHARS]
            )
        );
        assert_eq!(
            page(root.path(), &digest, 0, 2)
                .await
                .unwrap_err()
                .message(),
            "offset is 1-based: the first line is offset=1"
        );
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
            serde_json::json!({"artifact": digest, "offset": 0}),
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
    async fn read_artifact_normalizes_the_window_as_read_does() {
        let digest = "a".repeat(64);
        let prepared = ReadArtifactTool
            .prepare(
                context(SessionId::new_v7(), RunId::new_v7()),
                call(serde_json::json!({"artifact": digest, "stream": "stdout", "limit": 5_000})),
            )
            .await
            .unwrap();
        // No clamp: the page budget bounds a page, as it does `read`'s.
        assert_eq!(prepared.normalized_arguments()["limit"], 5_000);
        assert_eq!(prepared.normalized_arguments()["offset"], 1);
        assert_eq!(prepared.policy_labels(), [Some(format!("{digest}/stdout"))]);
    }
}
