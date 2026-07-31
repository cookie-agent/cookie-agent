use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use cookie_agent_engine::{
    SessionToolContext, ToolCall, ToolError, ToolInvocationContext, ToolProvider, ToolSpec,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::{canonical_path, result, schema, tool_error, workspace_for, workspace_path};

#[derive(Debug)]
pub struct WriteTool {
    workspace: PathBuf,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}
#[derive(Serialize)]
struct WriteOutput {
    path: String,
    bytes_written: usize,
}

impl WriteTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace_path(workspace),
        }
    }
}
impl Default for WriteTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| tool_error("write target has no parent directory"))?;
    fs::create_dir_all(parent).map_err(tool_error)?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(tool_error)?;
    temporary.write_all(contents).map_err(tool_error)?;
    temporary.as_file().sync_all().map_err(tool_error)?;
    temporary
        .persist(path)
        .map_err(|error| tool_error(error.error))?;
    Ok(())
}

#[async_trait]
impl ToolProvider for WriteTool {
    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "write".into(),
            description: "Atomically write a file, creating parent directories.".into(),
            parameters: schema::<WriteArgs>(),
        }])
    }
    async fn invoke(
        &self,
        ctx: ToolInvocationContext,
        call: ToolCall,
    ) -> Result<cookie_agent_engine::ToolResult, ToolError> {
        if call.name != "write" {
            return Err(tool_error("write tool received another tool name"));
        }
        let args: WriteArgs = serde_json::from_value(call.arguments).map_err(tool_error)?;
        let path =
            canonical_path(workspace_for(&ctx, &self.workspace), &args.path).map_err(tool_error)?;
        atomic_write(&path, args.content.as_bytes())?;
        Ok(result(
            &WriteOutput {
                path: path.display().to_string(),
                bytes_written: args.content.len(),
            },
            false,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::atomic_write;

    #[test]
    fn atomically_replaces_and_creates_parents() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("nested/file.txt");
        atomic_write(&path, b"first").expect("first atomic write");
        atomic_write(&path, b"second").expect("replacement atomic write");
        assert_eq!(fs::read_to_string(path).expect("read result"), "second");
    }
}
