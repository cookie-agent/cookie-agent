use std::path::PathBuf;

use async_trait::async_trait;
use cookiecode_engine::{
    SessionToolContext, ToolCall, ToolError, ToolInvocationContext, ToolProvider, ToolSpec,
};
use ignore::{Match, WalkBuilder, overrides::OverrideBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{canonical_path, result, schema, tool_error, workspace_for, workspace_path};

const MATCH_LIMIT: usize = 1_000;
#[derive(Debug)]
pub struct GlobTool {
    workspace: PathBuf,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
    #[schemars(range(min = 1, max = 10_000))]
    max_results: Option<usize>,
}
#[derive(Serialize)]
struct GlobOutput {
    paths: Vec<String>,
    truncated: bool,
}
impl GlobTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace_path(workspace),
        }
    }
}
impl Default for GlobTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

#[async_trait]
impl ToolProvider for GlobTool {
    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "glob".into(),
            description: "Find files using a glob pattern while honoring .gitignore.".into(),
            parameters: schema::<GlobArgs>(),
        }])
    }
    async fn invoke(
        &self,
        ctx: ToolInvocationContext,
        call: ToolCall,
    ) -> Result<cookiecode_engine::ToolResult, ToolError> {
        if call.name != "glob" {
            return Err(tool_error("glob tool received another tool name"));
        }
        let args: GlobArgs = serde_json::from_value(call.arguments).map_err(tool_error)?;
        if args
            .max_results
            .is_some_and(|value| value == 0 || value > 10_000)
        {
            return Err(tool_error("max_results must be between 1 and 10000"));
        }
        let root = match args.path {
            Some(path) => {
                canonical_path(workspace_for(&ctx, &self.workspace), &path).map_err(tool_error)?
            }
            None => workspace_for(&ctx, &self.workspace).to_owned(),
        };
        let mut overrides = OverrideBuilder::new(&root);
        overrides.add(&args.pattern).map_err(tool_error)?;
        let overrides = overrides.build().map_err(tool_error)?;
        let limit = args.max_results.unwrap_or(MATCH_LIMIT).min(MATCH_LIMIT);
        let mut paths = Vec::new();
        let mut truncated = false;
        for entry in WalkBuilder::new(&root).hidden(false).build() {
            let entry = entry.map_err(tool_error)?;
            if entry.path() == root || !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if !matches!(overrides.matched(entry.path(), false), Match::Whitelist(_)) {
                continue;
            }
            if paths.len() == limit {
                truncated = true;
                break;
            }
            paths.push(entry.path().display().to_string());
        }
        Ok(result(&GlobOutput { paths, truncated }, truncated))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ignore::{Match, WalkBuilder, overrides::OverrideBuilder};
    use tempfile::tempdir;

    #[test]
    fn respects_gitignore_when_matching() {
        let directory = tempdir().expect("temporary directory");
        fs::create_dir(directory.path().join(".git")).expect("git directory");
        fs::write(directory.path().join(".gitignore"), "ignored.rs\n").expect("gitignore");
        fs::write(directory.path().join("visible.rs"), "").expect("visible");
        fs::write(directory.path().join("ignored.rs"), "").expect("ignored");
        let mut overrides = OverrideBuilder::new(directory.path());
        overrides.add("*.rs").expect("glob pattern");
        let overrides = overrides.build().expect("glob overrides");
        let found: Vec<_> = WalkBuilder::new(directory.path())
            .build()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_type().is_some_and(|kind| kind.is_file())
                    && matches!(overrides.matched(entry.path(), false), Match::Whitelist(_))
            })
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(found, vec!["visible.rs"]);
    }
}
