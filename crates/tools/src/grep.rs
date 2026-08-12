use std::path::{Path, PathBuf};

use async_trait::async_trait;
use cookie_agent_engine::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::{PermissionAction, PersistedToolResult as ToolResult, Sha256Digest};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{fs_cap, parse_args, prepared_operation, prepared_pattern_resources, schema};

#[derive(Debug)]
pub struct GrepTool {
    workspace: PathBuf,
}
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
}
struct GrepExecutor {
    result: ToolResult,
    bindings: Vec<fs_cap::PreparedExisting>,
}
impl GrepTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
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
            description: "Search a prepared filesystem snapshot.".into(),
            parameters: schema::<GrepArgs>(),
        }])
    }
    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let args: GrepArgs = parse_args("grep", call.arguments)?;
        let regex =
            Regex::new(&args.pattern).map_err(|error| ToolError::execution(error.to_string()))?;
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
        let mut paths = collect_files(&root)?;
        if paths.len() > 1024 {
            return Err(ToolError::resource_limit(
                "grep snapshot exceeds the 1024-object capability limit",
            ));
        }
        let include_matcher = args
            .include
            .as_ref()
            .map(|include| {
                let mut builder = ignore::overrides::OverrideBuilder::new(&root);
                builder
                    .add(include)
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                builder
                    .build()
                    .map_err(|error| ToolError::execution(error.to_string()))
            })
            .transpose()?;
        paths.sort();
        let mut matches = Vec::new();
        let mut bindings = vec![fs_cap::prepare_existing(Path::new("/"), &root)?];
        let canonical_root = bindings[0].display_path.clone();
        for path in paths {
            let relative = path.strip_prefix(&root).unwrap_or(&path);
            if include_matcher
                .as_ref()
                .is_some_and(|matcher| !matcher.matched(relative, false).is_whitelist())
            {
                continue;
            }
            let binding = fs_cap::prepare_existing(Path::new("/"), &path)?;
            let text = String::from_utf8(binding.read_bytes()?)
                .map_err(|_| ToolError::execution("grep supports UTF-8 files"))?;
            for (line, value) in text.lines().enumerate() {
                if regex.is_match(value) {
                    matches.push(format!("{}:{}:{}", path.display(), line + 1, value));
                }
            }
            bindings.push(binding);
        }
        matches.sort();
        let snapshot = serde_json::to_vec(&matches)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let mut complete_binding = snapshot.clone();
        for binding in &bindings {
            complete_binding.extend_from_slice(&binding.manifest_bytes()?);
        }
        let (resources, policy_labels, external) = prepared_pattern_resources(
            PermissionAction::Grep,
            "regex",
            &args.pattern,
            &canonical_root,
            &self.workspace,
            &complete_binding,
        )?;
        let context = fs_cap::cwd_context_bytes(&ctx.cwd)?;
        let operation = prepared_operation(
            "grep",
            &args,
            if external {
                vec![
                    (PermissionAction::Grep, "search"),
                    (PermissionAction::ExternalDirectory, "guard"),
                ]
            } else {
                vec![(PermissionAction::Grep, "search")]
            },
            resources,
            &context,
        )?;
        let result = ToolResult {
            title: crate::safe_title(format!("Grep {}", args.pattern)),
            output: matches.join("\n"),
            metadata: serde_json::json!({"matches":matches.len(),"snapshot_sha256":Sha256Digest::of_bytes(&snapshot)}),
            truncation: None,
            attachments: Vec::new(),
        };
        PreparedTool::new(
            operation,
            serde_json::json!({
                "pattern": args.pattern,
                "path": canonical_root,
            }),
            None,
            Box::new(GrepExecutor { result, bindings }),
        )?
        .with_policy_labels(policy_labels)
    }
}

fn collect_files(path: &Path) -> Result<Vec<PathBuf>, ToolError> {
    let mut output = Vec::new();
    for entry in ignore::WalkBuilder::new(path)
        .follow_links(false)
        .require_git(false)
        .build()
    {
        let entry = entry.map_err(|error| ToolError::execution(error.to_string()))?;
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            output.push(entry.into_path());
        }
    }
    Ok(output)
}

#[async_trait]
impl PreparedExecutor for GrepExecutor {
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
            return Err(ToolError::execution("prepared grep cancelled"));
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

    use super::{GrepTool, collect_files};

    fn context(root: &std::path::Path) -> ToolPreparationContext {
        ToolPreparationContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            cwd: root.to_owned(),
            workspace_root: root.to_owned(),
        }
    }

    #[test]
    fn traversal_respects_gitignore() {
        let root = tempfile::tempdir().expect("root");
        fs::write(
            root.path().join(".gitignore"),
            "ignored.txt\nignored-dir/\n",
        )
        .expect("ignore");
        fs::write(root.path().join("visible.txt"), "visible").expect("visible");
        fs::write(root.path().join("ignored.txt"), "ignored").expect("ignored");
        fs::create_dir(root.path().join("ignored-dir")).expect("ignored directory");
        fs::write(root.path().join("ignored-dir/value.txt"), "ignored").expect("ignored child");
        let paths = collect_files(root.path()).expect("walk");
        assert!(paths.iter().any(|path| path.ends_with("visible.txt")));
        assert!(!paths.iter().any(|path| path.ends_with("ignored.txt")));
        assert!(
            !paths
                .iter()
                .any(|path| path.to_string_lossy().contains("ignored-dir"))
        );
    }

    #[test]
    fn traversal_reaches_nested_nonignored_files() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir_all(root.path().join("a/b")).expect("tree");
        fs::write(root.path().join("a/b/value.rs"), "fn value() {}").expect("file");
        let paths = collect_files(root.path()).expect("walk");
        assert!(paths.iter().any(|path| path.ends_with("a/b/value.rs")));
    }

    #[test]
    fn traversal_does_not_follow_symlinked_directories() {
        let root = tempfile::tempdir().expect("root");
        let external = tempfile::tempdir().expect("external");
        fs::write(external.path().join("secret.txt"), "secret").expect("secret");
        symlink(external.path(), root.path().join("linked")).expect("symlink");
        let paths = collect_files(root.path()).expect("walk");
        assert!(!paths.iter().any(|path| path.ends_with("secret.txt")));
    }

    #[tokio::test]
    async fn prepared_manifest_exposes_only_regex_permission_resource() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("a.txt"), "needle").expect("a");
        fs::write(root.path().join("b.txt"), "needle").expect("b");
        let prepared = GrepTool::new(root.path())
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "grep".into(),
                    arguments: serde_json::json!({"pattern":"needle"}),
                },
            )
            .await
            .expect("prepare");
        let labels = prepared
            .policy_labels()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(labels, ["needle"]);
        assert_eq!(prepared.operation().resources().len(), 1);
    }

    #[tokio::test]
    async fn external_traversal_is_guarded_separately_from_regex() {
        let workspace = tempfile::tempdir().expect("workspace");
        let external = tempfile::tempdir().expect("external");
        fs::write(external.path().join("value.txt"), "needle").expect("fixture");
        let prepared = GrepTool::new(workspace.path())
            .prepare(
                context(workspace.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "grep".into(),
                    arguments: serde_json::json!({
                        "pattern":"needle",
                        "path":external.path()
                    }),
                },
            )
            .await
            .expect("prepare");
        assert_eq!(
            prepared.policy_labels(),
            [format!("{}/*", external.path().display()), "needle".into()]
        );
        assert_eq!(
            prepared.operation().resources()[0].capability,
            cookie_agent_protocol::PermissionAction::ExternalDirectory
        );
        assert_eq!(
            prepared.operation().resources()[1].capability,
            cookie_agent_protocol::PermissionAction::Grep
        );
    }

    #[tokio::test]
    async fn distinct_regexes_have_distinct_fingerprints() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("a.txt"), "alpha beta").expect("file");
        let tool = GrepTool::new(root.path());
        let first = tool
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "grep".into(),
                    arguments: serde_json::json!({"pattern":"alpha"}),
                },
            )
            .await
            .expect("first");
        let second = tool
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "grep".into(),
                    arguments: serde_json::json!({"pattern":"beta"}),
                },
            )
            .await
            .expect("second");
        assert_ne!(
            OperationFingerprint::from_prepared_operation(first.operation()),
            OperationFingerprint::from_prepared_operation(second.operation())
        );
    }
}
