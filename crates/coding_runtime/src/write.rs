use std::path::PathBuf;

use async_trait::async_trait;
use cookie_agent_protocol::{PermissionAction, PersistedToolResult as ToolResult};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedSerializationKey, PreparedTool, SessionToolContext, ToolCall,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use cookie_agent_runtime_api::WorkspaceContext;

use crate::{fs_cap, parse_args, prepared_operation, prepared_path_resources, schema};

#[derive(Clone, Debug)]
pub struct WriteTool {
    workspace: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
struct WriteArgs {
    #[serde(
        rename = "filePath",
        deserialize_with = "crate::path_args::deserialize"
    )]
    /// Path to the file. Relative paths resolve against the session working directory.
    file_path: String,
    content: String,
}

struct WriteExecutor {
    target: fs_cap::PreparedTarget,
    bytes: Vec<u8>,
}

impl WriteTool {
    /// A tool working in `workspace`, the workspace of the coding runtime
    /// that offers it.
    #[must_use]
    pub fn new(workspace: &WorkspaceContext) -> Self {
        Self {
            workspace: workspace.root().to_owned(),
        }
    }
}

#[async_trait]
impl ToolProvider for WriteTool {
    fn provider_id(&self) -> &'static str {
        "builtin.write"
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: cookie_agent_runtime_api::ToolConcurrency::Parallel,
            result_truncation: Default::default(),
            name: "write".into(),
            permission_name: Self::get_permission_name("write")?.map(Into::into),
            description: "Write content to a file atomically, creating missing parent directories."
                .into(),
            parameters: schema::<WriteArgs>(),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        match tool_name {
            "write" => Ok(Some("write")),
            _ => Err(ToolError::execution("write provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        let permission_name = Self::get_permission_name(name)?;
        let args: WriteArgs = parse_args("write", arguments.clone())?;
        if args.file_path.is_empty() {
            return Err(ToolError::execution("filePath must not be empty"));
        }
        Ok((
            permission_name,
            Some(crate::permission_path_label(
                &args.file_path,
                &self.workspace,
            )),
        ))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        let (_, Some(path)) = self.get_permission_resource(name, arguments)? else {
            return Err(ToolError::execution("write permission resource is missing"));
        };
        Ok(crate::abbreviated_display_path(&path, &self.workspace))
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let args: WriteArgs = parse_args("write", call.arguments)?;
        let workspace = self.workspace.clone();
        crate::blocking(move || prepare_write(&workspace, args)).await
    }
}

fn prepare_write(workspace: &std::path::Path, args: WriteArgs) -> Result<PreparedTool, ToolError> {
    let target = fs_cap::prepare_target(workspace, std::path::Path::new(&args.file_path))?;
    if matches!(&target, fs_cap::PreparedTarget::Existing(existing) if existing.directory) {
        return Err(ToolError::unsupported_security(
            "write target is a directory",
        ));
    }
    let (resources, policy_labels) = prepared_path_resources(
        PermissionAction::WRITE,
        "file",
        target.display_path(),
        workspace,
        &target.binding_bytes(),
    )?;
    let context = crate::workspace_context_bytes(workspace);
    let operation = prepared_operation(
        "write",
        &args,
        vec![(PermissionAction::WRITE, "replace")],
        resources,
        &context,
    )?;
    let normalized_arguments = serde_json::json!({
        "filePath": target.display_path(),
        "content": args.content,
    });
    // Writes to one location run one at a time, in call order.
    let serialization_key = PreparedSerializationKey::new(target.serialization_key());
    PreparedTool::new(
        operation,
        normalized_arguments,
        Some(serialization_key),
        Box::new(WriteExecutor {
            target,
            bytes: args.content.into_bytes(),
        }),
    )?
    .with_policy_labels(policy_labels)
}

#[async_trait]
impl PreparedExecutor for WriteExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_runtime_api::ToolCompletion, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution(
                "prepared write cancelled before commit",
            ));
        }
        let (path, bytes) = crate::blocking(move || {
            self.target.write(&self.bytes)?;
            Ok((self.target.display_path().to_owned(), self.bytes.len()))
        })
        .await?;
        let path = path.display();
        Ok(cookie_agent_runtime_api::ToolCompletion::single(
            ToolResult {
                display: None,
                retained_output: None,
                title: crate::safe_title(format!("Wrote {path}")),
                output: format!("Wrote {bytes} bytes to {path}"),
                metadata: serde_json::json!({ "bytes": bytes }),
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        OperationFingerprint, PermissionAction, RunId, SessionId, ToolCallId,
    };
    #[cfg(any(unix, windows))]
    use cookie_agent_runtime_api::ToolExecutionContext;
    use cookie_agent_runtime_api::{ToolCall, ToolError, ToolPreparationContext, ToolProvider};

    use super::WriteTool;

    #[test]
    fn permission_resource_is_the_file_path() {
        let tool = WriteTool::new(&crate::test_workspace(std::env::temp_dir()));
        assert_eq!(
            tool.get_permission_resource(
                "write",
                &serde_json::json!({"filePath":"out.txt","content":"x"})
            )
            .expect("permission resource"),
            (Some("write"), Some("out.txt".into()))
        );
        assert!(matches!(
            tool.get_permission_resource("write", &serde_json::json!({"content":"x"})),
            Err(ToolError::Failed(_))
        ));
    }

    #[test]
    fn display_argument_abbreviates_workspace_and_home_paths() {
        let workspace = tempfile::tempdir().expect("workspace");
        let tool = WriteTool::new(&crate::test_workspace(workspace.path()));
        assert_eq!(
            tool.get_display_argument(
                "write",
                &serde_json::json!({"filePath":"out.txt","content":"x"})
            )
            .expect("relative"),
            "out.txt"
        );
        assert_eq!(
            tool.get_display_argument(
                "write",
                &serde_json::json!({"filePath":workspace.path().join("out.txt"),"content":"x"})
            )
            .expect("workspace"),
            "out.txt"
        );
        let home = cookie_agent_protocol::paths::home_dir().expect("home directory");
        assert_eq!(
            tool.get_display_argument(
                "write",
                &serde_json::json!({"filePath": home.join("notes.txt"),"content":"x"})
            )
            .expect("home"),
            "~/notes.txt"
        );
    }

    fn context(root: &std::path::Path) -> ToolPreparationContext {
        ToolPreparationContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            origin: crate::test_coding_origin(root),
            turn_context: crate::test_turn_context(),
        }
    }

    #[tokio::test]
    async fn distinct_write_content_has_distinct_fingerprint() {
        let root = tempfile::tempdir().expect("root");
        let tool = WriteTool::new(&crate::test_workspace(root.path()));
        let first = tool
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "write".into(),
                    arguments: serde_json::json!({"filePath":"value.txt","content":"one"}),
                },
            )
            .await
            .expect("first");
        let second = tool
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "write".into(),
                    arguments: serde_json::json!({"filePath":"value.txt","content":"two"}),
                },
            )
            .await
            .expect("second");
        assert_ne!(
            OperationFingerprint::from_prepared_operation(first.operation()),
            OperationFingerprint::from_prepared_operation(second.operation())
        );
    }

    #[tokio::test]
    async fn missing_subtree_write_prepares_without_creating_components() {
        let root = tempfile::tempdir().expect("root");
        let prepared = WriteTool::new(&crate::test_workspace(root.path()))
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "write".into(),
                    arguments: serde_json::json!({"filePath":"a/b/value.txt","content":"value"}),
                },
            )
            .await
            .expect("prepare");
        assert!(!root.path().join("a").exists());
        assert_eq!(
            prepared.operation().resources()[0].capability,
            PermissionAction::WRITE
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_routes_keep_lexical_write_contract_and_preserve_links() {
        use std::{fs, os::unix::fs::symlink};

        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let external = root.path().join("external");
        fs::create_dir(&workspace).expect("workspace");
        fs::create_dir(workspace.join("routes")).expect("routes");
        fs::create_dir(&external).expect("external");
        for name in [
            "relative-leaf.txt",
            "absolute-leaf.txt",
            "relative-ancestor.txt",
            "absolute-ancestor.txt",
        ] {
            fs::write(external.join(name), "old").expect("fixture");
        }
        symlink(
            "../../external/relative-leaf.txt",
            workspace.join("routes/relative-leaf"),
        )
        .expect("relative leaf");
        symlink(
            external.join("absolute-leaf.txt"),
            workspace.join("routes/absolute-leaf"),
        )
        .expect("absolute leaf");
        symlink(
            "absolute-ancestor",
            workspace.join("routes/relative-ancestor"),
        )
        .expect("relative ancestor");
        symlink(&external, workspace.join("routes/absolute-ancestor")).expect("absolute ancestor");

        let tool = WriteTool::new(&crate::test_workspace(&workspace));
        for (requested, destination, link) in [
            (
                "routes/relative-leaf",
                external.join("relative-leaf.txt"),
                workspace.join("routes/relative-leaf"),
            ),
            (
                "routes/absolute-leaf",
                external.join("absolute-leaf.txt"),
                workspace.join("routes/absolute-leaf"),
            ),
            (
                "routes/relative-ancestor/relative-ancestor.txt",
                external.join("relative-ancestor.txt"),
                workspace.join("routes/relative-ancestor"),
            ),
            (
                "routes/absolute-ancestor/absolute-ancestor.txt",
                external.join("absolute-ancestor.txt"),
                workspace.join("routes/absolute-ancestor"),
            ),
        ] {
            let original_link = fs::read_link(&link).expect("link target");
            let prepared = tool
                .prepare(
                    context(&workspace),
                    ToolCall {
                        id: ToolCallId::new_v7(),
                        name: "write".into(),
                        arguments: serde_json::json!({"filePath":requested,"content":"new"}),
                    },
                )
                .await
                .expect("prepare symlink write");
            assert_eq!(prepared.operation().resources().len(), 1);
            assert_eq!(prepared.policy_labels(), [Some(requested.to_owned())]);
            assert_eq!(
                prepared.normalized_arguments()["filePath"],
                workspace.join(requested).to_string_lossy().as_ref()
            );
            assert_eq!(
                tool.get_permission_resource("write", prepared.normalized_arguments())
                    .expect("normalized permission resource"),
                (Some("write"), Some(requested.to_owned()))
            );
            crate::assert_workspace_rule_allows(
                &prepared,
                &workspace,
                PermissionAction::WRITE,
                "routes/*",
            );
            prepared
                .execute_for_test(
                    cookie_agent_engine::testing::tool_execution_context(
                        root.path().join("artifacts"),
                        crate::test_turn_context(),
                    )
                    .expect("execution context"),
                )
                .await
                .expect("execute symlink write");
            assert_eq!(fs::read_to_string(destination).unwrap(), "new");
            assert_eq!(fs::read_link(link).unwrap(), original_link);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_follows_dangling_leaf_for_creation_without_replacing_link() {
        use std::{fs, os::unix::fs::symlink};

        let root = tempfile::tempdir().expect("root");
        symlink("created.txt", root.path().join("route")).expect("dangling route");
        let prepared = WriteTool::new(&crate::test_workspace(root.path()))
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "write".into(),
                    arguments: serde_json::json!({"filePath":"route","content":"created"}),
                },
            )
            .await
            .expect("prepare dangling write");
        prepared
            .execute_for_test(
                cookie_agent_engine::testing::tool_execution_context(
                    root.path().join("artifacts"),
                    crate::test_turn_context(),
                )
                .expect("execution context"),
            )
            .await
            .expect("execute dangling write");

        assert_eq!(
            fs::read_link(root.path().join("route")).unwrap(),
            std::path::Path::new("created.txt")
        );
        assert_eq!(
            fs::read_to_string(root.path().join("created.txt")).unwrap(),
            "created"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retargeted_write_route_does_not_redirect_and_routes_affect_fingerprint() {
        use std::{fs, os::unix::fs::symlink};

        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "old").expect("fixture");
        symlink("value.txt", root.path().join("left")).expect("left route");
        symlink("value.txt", root.path().join("right")).expect("right route");
        let tool = WriteTool::new(&crate::test_workspace(root.path()));
        let prepare = |path: &str| {
            tool.prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "write".into(),
                    arguments: serde_json::json!({"filePath":path,"content":"new"}),
                },
            )
        };
        let left = prepare("left").await.expect("prepare left");
        let right = prepare("right").await.expect("prepare right");
        assert_ne!(
            left.operation().resources()[0].binding_digest,
            right.operation().resources()[0].binding_digest
        );
        assert_ne!(
            OperationFingerprint::from_prepared_operation(left.operation()),
            OperationFingerprint::from_prepared_operation(right.operation())
        );

        fs::write(root.path().join("other.txt"), "other").expect("other fixture");
        fs::remove_file(root.path().join("left")).expect("remove route");
        symlink("other.txt", root.path().join("left")).expect("retarget route");
        left.execute_for_test(
            cookie_agent_engine::testing::tool_execution_context(
                root.path().join("artifacts"),
                crate::test_turn_context(),
            )
            .expect("execution context"),
        )
        .await
        .expect("write the approved destination");
        assert_eq!(
            fs::read_to_string(root.path().join("value.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("other.txt")).unwrap(),
            "other"
        );
    }

    #[cfg(any(unix, windows))]
    fn write_call(path: &str, content: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new_v7(),
            name: "write".into(),
            arguments: serde_json::json!({"filePath": path, "content": content}),
        }
    }

    #[cfg(any(unix, windows))]
    fn execution_context(root: &std::path::Path) -> ToolExecutionContext {
        cookie_agent_engine::testing::tool_execution_context(
            root.join("artifacts"),
            crate::test_turn_context(),
        )
        .expect("execution context")
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn parallel_same_file_writes_apply_in_call_order() {
        use std::fs;

        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "old").expect("fixture");
        let prepared = WriteTool::new(&crate::test_workspace(root.path()))
            .prepare_parallel(
                context(root.path()),
                vec![
                    write_call("value.txt", "one"),
                    write_call("value.txt", "two"),
                ],
            )
            .await;
        let mut prepared = prepared.into_iter();
        let first = prepared.next().expect("first").expect("first write");
        let second = prepared.next().expect("second").expect("second write");
        assert!(prepared.next().is_none());
        assert_ne!(
            OperationFingerprint::from_prepared_operation(first.operation()),
            OperationFingerprint::from_prepared_operation(second.operation())
        );

        first
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("first write");
        second
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("second write");
        // The last write in call order wins deterministically.
        assert_eq!(
            fs::read_to_string(root.path().join("value.txt")).unwrap(),
            "two"
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn repeated_creation_writes_apply_in_call_order() {
        let root = tempfile::tempdir().expect("root");
        let prepared = WriteTool::new(&crate::test_workspace(root.path()))
            .prepare_parallel(
                context(root.path()),
                vec![
                    write_call("nested/created.txt", "one"),
                    write_call("nested/created.txt", "two"),
                ],
            )
            .await;
        for prepared in prepared {
            prepared
                .expect("prepare write")
                .execute_for_test(execution_context(root.path()))
                .await
                .expect("write");
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("nested/created.txt")).unwrap(),
            "two"
        );
    }
}
