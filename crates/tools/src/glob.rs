use crate::{
    fs_cap, parse_args, prepared_operation, prepared_path_resources, prepared_resource, schema,
};
use async_trait::async_trait;
use cookie_agent_engine::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolResult, ToolSpec,
};
use cookie_agent_protocol::{
    ActionKind, ApprovalResourceSource, PreparedBindingLifetime, Sha256Digest,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct GlobTool {
    workspace: PathBuf,
}
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
}
struct GlobExecutor {
    result: ToolResult,
    bindings: Vec<fs_cap::PreparedExisting>,
}
impl GlobTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
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
            description: "List a prepared filesystem snapshot matching a wildcard.".into(),
            parameters: schema::<GlobArgs>(),
        }])
    }
    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let args: GlobArgs = parse_args("glob", call.arguments)?;
        let root = args.path.as_ref().map_or_else(
            || ctx.cwd.clone(),
            |path| {
                if Path::new(path).is_absolute() {
                    PathBuf::from(path)
                } else {
                    ctx.cwd.join(path)
                }
            },
        );
        let mut paths = collect(&root, &args.pattern)?;
        if paths.len() > 1024 {
            return Err(ToolError::resource_limit(
                "glob snapshot exceeds the 1024-object capability limit",
            ));
        }
        paths.sort();
        let mut bindings = vec![fs_cap::prepare_existing(Path::new("/"), &root)?];
        let canonical_root = bindings[0].display_path.clone();
        bindings.extend(
            paths
                .iter()
                .map(|path| fs_cap::prepare_existing(Path::new("/"), Path::new(path)))
                .collect::<Result<Vec<_>, ToolError>>()?,
        );
        let snapshot =
            serde_json::to_vec(&paths).map_err(|error| ToolError::execution(error.to_string()))?;
        let mut complete_binding = snapshot.clone();
        for binding in &bindings {
            complete_binding.extend_from_slice(&binding.manifest_bytes()?);
        }
        let (mut resources, mut policy_labels, external) = prepared_path_resources(
            ActionKind::Glob,
            "path",
            &canonical_root,
            &self.workspace,
            &complete_binding,
        )?;
        resources.push(prepared_resource(
            ActionKind::Glob,
            "glob",
            args.pattern.as_bytes(),
            &complete_binding,
            PreparedBindingLifetime::ProcessLocal,
            ApprovalResourceSource::SecondaryOperation,
        )?);
        policy_labels.push(args.pattern.clone());
        let context = fs_cap::cwd_context_bytes(&ctx.cwd)?;
        let operation = prepared_operation(
            "glob",
            &args,
            if external {
                vec![
                    (ActionKind::Glob, "list"),
                    (ActionKind::ExternalDirectory, "guard"),
                ]
            } else {
                vec![(ActionKind::Glob, "list")]
            },
            resources,
            &context,
        )?;
        let result = ToolResult {
            title: format!("Glob {}", args.pattern),
            output: paths.join("\n"),
            metadata: serde_json::json!({"matches":paths.len(),"snapshot_sha256":Sha256Digest::of_bytes(&snapshot)}),
            truncation: None,
            attachments: Vec::new(),
        };
        PreparedTool::new(operation, None, Box::new(GlobExecutor { result, bindings }))
            .with_policy_labels(policy_labels)
    }
}
fn collect(root: &Path, pattern: &str) -> Result<Vec<String>, ToolError> {
    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    overrides
        .add(pattern)
        .map_err(|error| ToolError::execution(error.to_string()))?;
    let overrides = overrides
        .build()
        .map_err(|error| ToolError::execution(error.to_string()))?;
    let mut output = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .follow_links(false)
        .require_git(false)
        .build()
    {
        let entry = entry.map_err(|error| ToolError::execution(error.to_string()))?;
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && overrides.matched(relative, false).is_whitelist()
        {
            output.push(entry.path().display().to_string());
        }
    }
    Ok(output)
}
#[async_trait]
impl PreparedExecutor for GlobExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        for binding in &self.bindings {
            binding.revalidate()?;
        }
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution("prepared glob cancelled"));
        }
        for binding in &self.bindings {
            binding.revalidate()?;
        }
        Ok(self.result)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::symlink};

    use cookie_agent_engine::{ToolCall, ToolPreparationContext, ToolProvider};
    use cookie_agent_protocol::{OperationFingerprint, RunId, SessionId, ToolCallId};

    use super::{GlobTool, collect};

    fn context(root: &std::path::Path) -> ToolPreparationContext {
        ToolPreparationContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            cwd: root.to_owned(),
            workspace_root: root.to_owned(),
        }
    }

    #[test]
    fn glob_respects_gitignore() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join(".gitignore"), "ignored.rs\n").expect("ignore");
        fs::write(root.path().join("visible.rs"), "visible").expect("visible");
        fs::write(root.path().join("ignored.rs"), "ignored").expect("ignored");
        let paths = collect(root.path(), "*.rs").expect("glob");
        assert!(paths.iter().any(|path| path.ends_with("visible.rs")));
        assert!(!paths.iter().any(|path| path.ends_with("ignored.rs")));
    }

    #[test]
    fn recursive_glob_traverses_nested_directories() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir_all(root.path().join("src/nested")).expect("tree");
        fs::write(root.path().join("src/nested/lib.rs"), "value").expect("file");
        let paths = collect(root.path(), "**/*.rs").expect("glob");
        assert!(paths.iter().any(|path| path.ends_with("src/nested/lib.rs")));
    }

    #[test]
    fn glob_does_not_follow_symlinked_directories() {
        let root = tempfile::tempdir().expect("root");
        let external = tempfile::tempdir().expect("external");
        fs::write(external.path().join("secret.rs"), "secret").expect("secret");
        symlink(external.path(), root.path().join("linked")).expect("link");
        let paths = collect(root.path(), "**/*.rs").expect("glob");
        assert!(!paths.iter().any(|path| path.ends_with("secret.rs")));
    }

    #[tokio::test]
    async fn prepared_manifest_retains_path_and_glob_labels() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.rs"), "value").expect("file");
        let prepared = GlobTool::new(root.path())
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "glob".into(),
                    arguments: serde_json::json!({"pattern":"*.rs"}),
                },
            )
            .await
            .expect("prepare");
        let labels = prepared
            .policy_labels()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert!(labels.contains(&"*.rs"));
        assert!(
            labels
                .iter()
                .any(|label| *label == root.path().to_string_lossy())
        );
    }

    #[tokio::test]
    async fn distinct_globs_have_distinct_fingerprints() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("a.rs"), "a").expect("rs");
        fs::write(root.path().join("a.txt"), "a").expect("txt");
        let tool = GlobTool::new(root.path());
        let rs = tool
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "glob".into(),
                    arguments: serde_json::json!({"pattern":"*.rs"}),
                },
            )
            .await
            .expect("rs");
        let txt = tool
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "glob".into(),
                    arguments: serde_json::json!({"pattern":"*.txt"}),
                },
            )
            .await
            .expect("txt");
        assert_ne!(
            OperationFingerprint::from_prepared_operation(rs.operation()),
            OperationFingerprint::from_prepared_operation(txt.operation())
        );
    }
}
