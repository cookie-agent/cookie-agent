use std::{fs, io::Read, path::PathBuf};

use async_trait::async_trait;
use cookie_agent_engine::{
    SessionToolContext, ToolCall, ToolError, ToolInvocationContext, ToolProvider, ToolSpec,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    canonical_path, result, schema, tool_error, truncate_text, workspace_for, workspace_path,
};

const READ_LIMIT: usize = 24 * 1024;

#[derive(Debug)]
pub struct ReadTool {
    workspace: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[schemars(range(min = 1))]
    start_line: Option<usize>,
    #[schemars(range(min = 1))]
    end_line: Option<usize>,
}

#[derive(Serialize)]
struct ReadOutput {
    path: String,
    start_line: usize,
    content: String,
    total_bytes: usize,
    truncated: bool,
}

impl ReadTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace_path(workspace),
        }
    }
}
impl Default for ReadTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

#[async_trait]
impl ToolProvider for ReadTool {
    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "read".into(),
            description: "Read a text file, optionally by line range.".into(),
            parameters: schema::<ReadArgs>(),
        }])
    }
    async fn invoke(
        &self,
        ctx: ToolInvocationContext,
        call: ToolCall,
    ) -> Result<cookie_agent_engine::ToolResult, ToolError> {
        if call.name != "read" {
            return Err(tool_error("read tool received another tool name"));
        }
        let args: ReadArgs = serde_json::from_value(call.arguments).map_err(tool_error)?;
        if args.start_line == Some(0) || args.end_line == Some(0) {
            return Err(tool_error("line numbers start at 1"));
        }
        if args
            .end_line
            .is_some_and(|end| args.start_line.is_some_and(|start| end < start))
        {
            return Err(tool_error("end_line must not precede start_line"));
        }
        let path =
            canonical_path(workspace_for(&ctx, &self.workspace), &args.path).map_err(tool_error)?;
        let total_bytes =
            usize::try_from(fs::metadata(&path).map_err(tool_error)?.len()).unwrap_or(usize::MAX);
        let mut bytes = Vec::with_capacity(READ_LIMIT);
        fs::File::open(&path)
            .map_err(tool_error)?
            .take(READ_LIMIT as u64)
            .read_to_end(&mut bytes)
            .map_err(tool_error)?;
        if let Err(error) = std::str::from_utf8(&bytes)
            && error.error_len().is_none()
        {
            bytes.truncate(error.valid_up_to());
        }
        let text = String::from_utf8_lossy(&bytes);
        let start = args.start_line.unwrap_or(1);
        let end = args.end_line.unwrap_or(usize::MAX);
        let mut content = String::new();
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            if number >= start && number <= end {
                content.push_str(line);
                content.push('\n');
            }
        }
        let mut truncated = total_bytes > bytes.len();
        truncated |= truncate_text(&mut content, READ_LIMIT);
        Ok(result(
            &ReadOutput {
                path: path.display().to_string(),
                start_line: start,
                content,
                total_bytes,
                truncated,
            },
            truncated,
        ))
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_engine::{ProgressSink, ToolCall, ToolInvocationContext, ToolProvider};
    use cookie_agent_protocol::{RunId, SessionId, ToolCallId};
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    use super::{READ_LIMIT, ReadTool};

    #[tokio::test]
    async fn capped_valid_utf8_file_discards_only_the_partial_codepoint() {
        let directory = tempdir().expect("temporary directory");
        let prefix = "a".repeat(READ_LIMIT - 2);
        std::fs::write(directory.path().join("emoji.txt"), format!("{prefix}😀"))
            .expect("write UTF-8 fixture");
        let (progress, _) = mpsc::channel(1);
        let result = ReadTool::new(directory.path())
            .invoke(
                ToolInvocationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: directory.path().to_owned(),
                    workspace_root: directory.path().to_owned(),
                    progress: ProgressSink::new(
                        progress,
                        cookie_agent_engine::events::OutputHub::new(ToolCallId::new_v7(), 1),
                    ),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                    stdin: None,
                },
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path":"emoji.txt"}),
                },
            )
            .await
            .expect("read result");
        let output: serde_json::Value = serde_json::from_str(&result.content).expect("read JSON");

        assert_eq!(output["content"], format!("{prefix}\n"));
        assert!(
            !output["content"]
                .as_str()
                .expect("content")
                .contains('\u{fffd}')
        );
        assert!(result.truncated);
    }
}
