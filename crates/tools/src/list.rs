use std::{fs, path::PathBuf};

use async_trait::async_trait;
use cookiecode_engine::{
    SessionToolContext, ToolCall, ToolError, ToolInvocationContext, ToolProvider, ToolSpec,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{canonical_path, result, schema, tool_error, workspace_for, workspace_path};

const ENTRY_LIMIT: usize = 1_000;
#[derive(Debug)]
pub struct ListTool {
    workspace: PathBuf,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    path: Option<String>,
    #[schemars(range(min = 1, max = 10_000))]
    max_results: Option<usize>,
}
#[derive(Serialize)]
struct ListEntry {
    name: String,
    kind: &'static str,
    size: u64,
}
#[derive(Serialize)]
struct ListOutput {
    path: String,
    entries: Vec<ListEntry>,
    truncated: bool,
}
impl ListTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace_path(workspace),
        }
    }
}
impl Default for ListTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

#[async_trait]
impl ToolProvider for ListTool {
    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "list".into(),
            description: "List a directory.".into(),
            parameters: schema::<ListArgs>(),
        }])
    }
    async fn invoke(
        &self,
        ctx: ToolInvocationContext,
        call: ToolCall,
    ) -> Result<cookiecode_engine::ToolResult, ToolError> {
        if call.name != "list" {
            return Err(tool_error("list tool received another tool name"));
        }
        let args: ListArgs = serde_json::from_value(call.arguments).map_err(tool_error)?;
        if args
            .max_results
            .is_some_and(|value| value == 0 || value > 10_000)
        {
            return Err(tool_error("max_results must be between 1 and 10000"));
        }
        let path = match args.path {
            Some(path) => {
                canonical_path(workspace_for(&ctx, &self.workspace), &path).map_err(tool_error)?
            }
            None => workspace_for(&ctx, &self.workspace).to_owned(),
        };
        let limit = args.max_results.unwrap_or(ENTRY_LIMIT).min(ENTRY_LIMIT);
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in fs::read_dir(&path).map_err(tool_error)? {
            if entries.len() == limit {
                truncated = true;
                break;
            }
            let entry = entry.map_err(tool_error)?;
            let metadata = entry.metadata().map_err(tool_error)?;
            let kind = if metadata.is_dir() {
                "directory"
            } else if metadata.is_file() {
                "file"
            } else {
                "other"
            };
            entries.push(ListEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
                size: metadata.len(),
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(result(
            &ListOutput {
                path: path.display().to_string(),
                entries,
                truncated,
            },
            truncated,
        ))
    }
}
