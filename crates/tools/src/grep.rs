use std::{fs, path::PathBuf};

use async_trait::async_trait;
use cookie_agent_engine::{
    SessionToolContext, ToolCall, ToolError, ToolInvocationContext, ToolProvider, ToolSpec,
};
use ignore::WalkBuilder;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{canonical_path, result, schema, tool_error, workspace_for, workspace_path};

const MATCH_LIMIT: usize = 1_000;

#[derive(Debug)]
pub struct GrepTool {
    workspace: PathBuf,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    #[schemars(range(min = 0, max = 20))]
    context_lines: Option<usize>,
    #[schemars(range(min = 1, max = 10_000))]
    max_results: Option<usize>,
}
#[derive(Serialize)]
struct GrepMatch {
    path: String,
    line: usize,
    text: String,
    before: Vec<String>,
    after: Vec<String>,
}
#[derive(Serialize)]
struct GrepOutput {
    matches: Vec<GrepMatch>,
    truncated: bool,
}

impl GrepTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace_path(workspace),
        }
    }
}
impl Default for GrepTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

#[async_trait]
impl ToolProvider for GrepTool {
    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "grep".into(),
            description: "Search text files with a regular expression while honoring .gitignore."
                .into(),
            parameters: schema::<GrepArgs>(),
        }])
    }
    async fn invoke(
        &self,
        ctx: ToolInvocationContext,
        call: ToolCall,
    ) -> Result<cookie_agent_engine::ToolResult, ToolError> {
        if call.name != "grep" {
            return Err(tool_error("grep tool received another tool name"));
        }
        let args: GrepArgs = serde_json::from_value(call.arguments).map_err(tool_error)?;
        if args.context_lines.is_some_and(|value| value > 20)
            || args
                .max_results
                .is_some_and(|value| value == 0 || value > 10_000)
        {
            return Err(tool_error(
                "context_lines or max_results is outside its allowed range",
            ));
        }
        let expression = Regex::new(&args.pattern).map_err(tool_error)?;
        let root = match args.path {
            Some(path) => {
                canonical_path(workspace_for(&ctx, &self.workspace), &path).map_err(tool_error)?
            }
            None => workspace_for(&ctx, &self.workspace).to_owned(),
        };
        let context = args.context_lines.unwrap_or(0);
        let limit = args.max_results.unwrap_or(MATCH_LIMIT).min(MATCH_LIMIT);
        let mut matches = Vec::new();
        let mut truncated = false;
        'files: for entry in WalkBuilder::new(&root).hidden(false).build() {
            let entry = entry.map_err(tool_error)?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(tool_error)?;
            if bytes.contains(&0) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<_> = text.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if expression.is_match(line) {
                    if matches.len() == limit {
                        truncated = true;
                        break 'files;
                    }
                    matches.push(GrepMatch {
                        path: entry.path().display().to_string(),
                        line: index + 1,
                        text: (*line).to_owned(),
                        before: lines[index.saturating_sub(context)..index]
                            .iter()
                            .map(ToString::to_string)
                            .collect(),
                        after: lines[index + 1..(index + 1 + context).min(lines.len())]
                            .iter()
                            .map(ToString::to_string)
                            .collect(),
                    });
                }
            }
        }
        Ok(result(&GrepOutput { matches, truncated }, truncated))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ignore::WalkBuilder;
    use tempfile::tempdir;

    #[test]
    fn traversal_honors_gitignore() {
        let directory = tempdir().expect("temporary directory");
        fs::create_dir(directory.path().join(".git")).expect("git directory");
        fs::write(directory.path().join(".gitignore"), "hidden.txt\n").expect("gitignore");
        fs::write(directory.path().join("visible.txt"), "needle").expect("visible");
        fs::write(directory.path().join("hidden.txt"), "needle").expect("hidden");
        let paths: Vec<_> = WalkBuilder::new(directory.path())
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["visible.txt"]);
    }
}
