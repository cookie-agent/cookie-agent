//! Built-in tools and the delegation tool provider.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use cookie_agent_engine::{
    SessionToolContext, ToolCall, ToolError, ToolInvocationContext, ToolProvider, ToolSpec,
};
use schemars::JsonSchema;
use serde::Serialize;

pub(crate) const RESULT_LIMIT: usize = 32 * 1024;

pub(crate) fn truncate_text(value: &mut String, limit: usize) -> bool {
    if value.len() <= limit {
        return false;
    }
    let mut boundary = limit;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    true
}

pub(crate) fn schema<T: JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("tool schemas serialize")
}

pub(crate) fn result<T: Serialize>(value: &T, truncated: bool) -> cookie_agent_engine::ToolResult {
    let content = serde_json::to_string(value).expect("tool result serializes");
    let mut was_truncated = truncated;
    let content = if content.len() > RESULT_LIMIT {
        was_truncated = true;
        serde_json::json!({ "truncated": true, "message": "result exceeded the output size cap" })
            .to_string()
    } else {
        content
    };
    cookie_agent_engine::ToolResult {
        content,
        truncated: was_truncated,
    }
}

pub(crate) fn tool_error(error: impl std::fmt::Display) -> cookie_agent_engine::ToolError {
    cookie_agent_engine::ToolError::Failed(error.to_string())
}

/// Resolves paths relative to the workspace passed when the tool provider is built.
/// Existing paths are canonicalized; new targets canonicalize their nearest existing
/// ancestor, matching the permission layer's path semantics.
pub(crate) fn canonical_path(workspace: &Path, input: &str) -> io::Result<PathBuf> {
    let requested = Path::new(input);
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };
    if absolute.exists() {
        return fs::canonicalize(absolute);
    }
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let name = ancestor.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path has no existing ancestor")
        })?;
        suffix.push(name.to_os_string());
        ancestor = ancestor.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path has no existing ancestor")
        })?;
    }
    let mut canonical = fs::canonicalize(ancestor)?;
    for component in suffix.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

pub(crate) fn workspace_path(workspace: impl Into<PathBuf>) -> PathBuf {
    let workspace = workspace.into();
    fs::canonicalize(&workspace).unwrap_or(workspace)
}

/// Uses the engine-frozen session workspace. The fallback keeps direct tool
/// construction usable in unit tests that do not create an engine context.
pub(crate) fn workspace_for<'a>(ctx: &'a ToolInvocationContext, fallback: &'a Path) -> &'a Path {
    if ctx.workspace_root.as_os_str().is_empty() {
        fallback
    } else {
        &ctx.workspace_root
    }
}

/// The single provider used to register all filesystem and process built-ins.
#[derive(Debug)]
pub struct BuiltinTools {
    read: read::ReadTool,
    write: write::WriteTool,
    edit: edit::EditTool,
    bash: bash::BashTool,
    grep: grep::GrepTool,
    glob: glob::GlobTool,
    list: list::ListTool,
}

impl BuiltinTools {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace_path(workspace);
        Self {
            read: read::ReadTool::new(workspace.clone()),
            write: write::WriteTool::new(workspace.clone()),
            edit: edit::EditTool::new(workspace.clone()),
            bash: bash::BashTool::new(workspace.clone()),
            grep: grep::GrepTool::new(workspace.clone()),
            glob: glob::GlobTool::new(workspace.clone()),
            list: list::ListTool::new(workspace),
        }
    }
}

impl Default for BuiltinTools {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

#[async_trait]
impl ToolProvider for BuiltinTools {
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let mut tools = Vec::new();
        tools.extend(self.read.tools_for_session(ctx)?);
        tools.extend(self.write.tools_for_session(ctx)?);
        tools.extend(self.edit.tools_for_session(ctx)?);
        tools.extend(self.bash.tools_for_session(ctx)?);
        tools.extend(self.grep.tools_for_session(ctx)?);
        tools.extend(self.glob.tools_for_session(ctx)?);
        tools.extend(self.list.tools_for_session(ctx)?);
        Ok(tools)
    }

    async fn invoke(
        &self,
        ctx: ToolInvocationContext,
        call: ToolCall,
    ) -> Result<cookie_agent_engine::ToolResult, ToolError> {
        match call.name.as_str() {
            "read" => self.read.invoke(ctx, call).await,
            "write" => self.write.invoke(ctx, call).await,
            "edit" => self.edit.invoke(ctx, call).await,
            "bash" => self.bash.invoke(ctx, call).await,
            "grep" => self.grep.invoke(ctx, call).await,
            "glob" => self.glob.invoke(ctx, call).await,
            "list" => self.list.invoke(ctx, call).await,
            _ => Err(tool_error(format!("unknown built-in tool `{}`", call.name))),
        }
    }
}

pub mod bash;
pub mod delegate;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod list;
pub mod read;
pub mod write;
