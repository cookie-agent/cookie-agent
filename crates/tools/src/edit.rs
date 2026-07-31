use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use cookie_agent_engine::{
    SessionToolContext, ToolCall, ToolError, ToolInvocationContext, ToolProvider, ToolSpec,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use tempfile::NamedTempFile;

use crate::{canonical_path, result, schema, tool_error, workspace_for, workspace_path};

#[derive(Debug)]
pub struct EditTool {
    workspace: PathBuf,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[schemars(range(min = 1))]
    expected_count: usize,
}
#[derive(Serialize)]
struct EditOutput {
    status: &'static str,
    path: String,
    replacements: usize,
    diff: String,
}
#[derive(Serialize)]
struct ConflictOutput {
    status: &'static str,
    path: String,
    message: &'static str,
    diff: String,
}

impl EditTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace_path(workspace),
        }
    }
}
impl Default for EditTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

fn file_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}
fn occurrence_count(source: &str, needle: &str) -> usize {
    source.match_indices(needle).count()
}

fn edit_file(
    path: &Path,
    old: &str,
    new: &str,
    expected_count: usize,
    before_rename: impl FnOnce(),
) -> Result<cookie_agent_engine::ToolResult, ToolError> {
    if old.is_empty() {
        return Err(tool_error("old_string must not be empty"));
    }
    let original = fs::read(path).map_err(tool_error)?;
    let original_hash = file_hash(&original);
    let source = String::from_utf8(original.clone())
        .map_err(|_| tool_error("edit only supports UTF-8 files"))?;
    let count = occurrence_count(&source, old);
    let diff = TextDiff::from_lines(&source, source.replace(old, new))
        .unified_diff()
        .header("before", "after")
        .to_string();
    if count != expected_count {
        return Ok(result(
            &ConflictOutput {
                status: "conflict",
                path: path.display().to_string(),
                message: "exact match occurrence count changed or did not match expected_count",
                diff,
            },
            false,
        ));
    }
    let replacement = source.replace(old, new);
    let parent = path
        .parent()
        .ok_or_else(|| tool_error("edit target has no parent directory"))?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(tool_error)?;
    use std::io::Write as _;
    temporary
        .write_all(replacement.as_bytes())
        .map_err(tool_error)?;
    temporary.as_file().sync_all().map_err(tool_error)?;
    before_rename();
    let current = fs::read(path).map_err(tool_error)?;
    if file_hash(&current) != original_hash {
        return Ok(result(
            &ConflictOutput {
                status: "conflict",
                path: path.display().to_string(),
                message: "file changed before atomic rename",
                diff,
            },
            false,
        ));
    }
    temporary
        .persist(path)
        .map_err(|error| tool_error(error.error))?;
    Ok(result(
        &EditOutput {
            status: "ok",
            path: path.display().to_string(),
            replacements: count,
            diff,
        },
        false,
    ))
}

#[async_trait]
impl ToolProvider for EditTool {
    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "edit".into(),
            description:
                "Replace an exact string only when its occurrence count matches expected_count."
                    .into(),
            parameters: schema::<EditArgs>(),
        }])
    }
    async fn invoke(
        &self,
        ctx: ToolInvocationContext,
        call: ToolCall,
    ) -> Result<cookie_agent_engine::ToolResult, ToolError> {
        if call.name != "edit" {
            return Err(tool_error("edit tool received another tool name"));
        }
        let args: EditArgs = serde_json::from_value(call.arguments).map_err(tool_error)?;
        if args.expected_count == 0 {
            return Err(tool_error("expected_count must be at least 1"));
        }
        let path =
            canonical_path(workspace_for(&ctx, &self.workspace), &args.path).map_err(tool_error)?;
        edit_file(
            &path,
            &args.old_string,
            &args.new_string,
            args.expected_count,
            || {},
        )
    }
}

#[cfg(test)]
mod tests {
    use super::edit_file;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn detects_change_before_rename() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("file.txt");
        fs::write(&path, "before").expect("seed file");
        let result = edit_file(&path, "before", "after", 1, || {
            fs::write(&path, "external").expect("external write")
        })
        .expect("conflict result");
        assert!(result.content.contains("conflict"));
        assert_eq!(fs::read_to_string(path).expect("read file"), "external");
    }
}
