use std::path::PathBuf;

use async_trait::async_trait;
use cookie_agent_engine::{
    PreparedExecutor, PreparedSerializationKey, PreparedTool, SessionToolContext, ToolCall,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::{PermissionAction, PersistedToolResult as ToolResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{fs_cap, parse_args, prepared_operation, prepared_path_resources, schema};

#[derive(Debug)]
pub struct WriteTool {
    workspace: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
struct WriteArgs {
    #[serde(
        rename = "filePath",
        deserialize_with = "crate::path_args::deserialize"
    )]
    file_path: String,
    content: String,
}

struct WriteExecutor {
    target: WriteTarget,
    bytes: Vec<u8>,
}

enum WriteTarget {
    Existing(fs_cap::PreparedExisting),
    Absent(fs_cap::PreparedAbsent),
    Chained(fs_cap::PreparedChained),
}

impl WriteTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }

    /// Build the prepared call for a resolved target. When `predecessor` is
    /// set, the call belongs to a same-target chain: the fingerprint binds
    /// the predecessor's output digest (the content this write replaces) and
    /// the executor validates that digest instead of the original target
    /// identity, which an earlier call in the chain atomically replaces.
    fn prepare_write(
        &self,
        ctx: &ToolPreparationContext,
        args: WriteArgs,
        target: fs_cap::PreparedTarget,
        predecessor: Option<cookie_agent_protocol::Sha256Digest>,
    ) -> Result<PreparedTool, ToolError> {
        let mut binding = match &target {
            fs_cap::PreparedTarget::Existing(existing) => {
                if existing.directory {
                    return Err(ToolError::unsupported_security(
                        "write target is a directory",
                    ));
                }
                let mut bytes = existing.identity.canonical_bytes();
                bytes.extend_from_slice(
                    predecessor
                        .clone()
                        .unwrap_or_else(|| existing.content_digest.clone())
                        .as_str()
                        .as_bytes(),
                );
                bytes
            }
            fs_cap::PreparedTarget::Absent(_) => b"expected-absent".to_vec(),
        };
        binding.extend_from_slice(
            cookie_agent_protocol::Sha256Digest::of_bytes(args.content.as_bytes())
                .as_str()
                .as_bytes(),
        );
        binding.extend_from_slice(&target.manifest_bytes()?);
        let display_path = match &target {
            fs_cap::PreparedTarget::Existing(target) => &target.display_path,
            fs_cap::PreparedTarget::Absent(target) => &target.display_path,
        };
        let (resources, policy_labels) = prepared_path_resources(
            PermissionAction::Write,
            "file",
            display_path,
            &self.workspace,
            &binding,
        )?;
        let serialization_key = target.serialization_bytes()?;
        let context = fs_cap::cwd_context_bytes(&ctx.cwd)?;
        let operation = prepared_operation(
            "write",
            &args,
            vec![(PermissionAction::Write, "replace")],
            resources,
            &context,
        )?;
        let normalized_arguments = serde_json::json!({
            "filePath": display_path,
            "content": args.content,
        });
        let executor_target = match (target, predecessor) {
            (fs_cap::PreparedTarget::Existing(existing), Some(predecessor_digest)) => {
                WriteTarget::Chained(fs_cap::PreparedChained {
                    original_digest: existing.content_digest.clone(),
                    predecessor_digest,
                    target: existing,
                })
            }
            (fs_cap::PreparedTarget::Existing(existing), None) => WriteTarget::Existing(existing),
            // Absent targets are not chained: only the first creation wins.
            (fs_cap::PreparedTarget::Absent(absent), _) => WriteTarget::Absent(absent),
        };
        PreparedTool::new(
            operation,
            normalized_arguments,
            Some(PreparedSerializationKey::new(serialization_key)),
            Box::new(WriteExecutor {
                target: executor_target,
                bytes: args.content.into_bytes(),
            }),
        )?
        .with_policy_labels(policy_labels)
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

#[async_trait]
impl ToolProvider for WriteTool {
    fn provider_id(&self) -> &'static str {
        "builtin.write"
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            output: Default::default(),
            concurrency: cookie_agent_engine::ToolConcurrency::Parallel,
            result_truncation: Default::default(),
            name: "write".into(),
            permission_name: Self::get_permission_name("write")?.into(),
            description: "Atomically write an exact descriptor-bound target.".into(),
            parameters: schema::<WriteArgs>(),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "write" => Ok("write"),
            _ => Err(ToolError::execution("write provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
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
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let args: WriteArgs = parse_args("write", call.arguments)?;
        fs_cap::ensure_atomic_write_supported()?;
        let target = fs_cap::prepare_target(&ctx.cwd, std::path::Path::new(&args.file_path))?;
        self.prepare_write(&ctx, args, target, None)
    }

    async fn prepare_parallel(
        &self,
        ctx: ToolPreparationContext,
        calls: Vec<ToolCall>,
    ) -> Vec<Result<PreparedTool, ToolError>> {
        if calls.len() == 1 {
            let call = calls.into_iter().next().expect("single call batch");
            return vec![self.prepare(ctx, call).await];
        }
        // Resolve each call's target individually, then group existing targets
        // by identity: same-file writes chain in call order, with every write
        // validating the content produced by the previous write. Absent
        // targets are not chained; only the first creation wins.
        let mut pending = Vec::with_capacity(calls.len());
        for call in calls {
            pending.push(Some((|| {
                let args: WriteArgs = parse_args("write", call.arguments)?;
                fs_cap::ensure_atomic_write_supported()?;
                let target =
                    fs_cap::prepare_target(&ctx.cwd, std::path::Path::new(&args.file_path))?;
                Ok((args, target))
            })()));
        }
        let mut groups: Vec<((u64, u64), Vec<usize>)> = Vec::new();
        for (index, entry) in pending.iter().enumerate() {
            let Some(Ok((_, fs_cap::PreparedTarget::Existing(existing)))) = entry else {
                continue;
            };
            let key = (existing.identity.device, existing.identity.inode);
            if let Some((_, indexes)) = groups.iter_mut().find(|(candidate, _)| *candidate == key) {
                indexes.push(index);
            } else {
                groups.push((key, vec![index]));
            }
        }
        let mut results: Vec<Option<Result<PreparedTool, ToolError>>> =
            (0..pending.len()).map(|_| None).collect();
        for (_, indexes) in groups {
            // The predecessor of each write is the previous write's output:
            // a failed preparation is skipped and the chain continues from
            // the last good write.
            let mut predecessor = None;
            for index in indexes {
                let (args, target) = pending[index]
                    .take()
                    .expect("grouped write pending")
                    .expect("grouped write prepared");
                let output_digest =
                    cookie_agent_protocol::Sha256Digest::of_bytes(args.content.as_bytes());
                let result = self.prepare_write(&ctx, args, target, predecessor.clone());
                if result.is_ok() {
                    predecessor = Some(output_digest);
                }
                results[index] = Some(result);
            }
        }
        for (index, entry) in pending.into_iter().enumerate() {
            if results[index].is_none() {
                results[index] = Some(
                    entry
                        .expect("ungrouped write pending")
                        .and_then(|(args, target)| self.prepare_write(&ctx, args, target, None)),
                );
            }
        }
        results
            .into_iter()
            .map(|result| result.expect("every write call is prepared"))
            .collect()
    }
}

#[async_trait]
impl PreparedExecutor for WriteExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        match &self.target {
            WriteTarget::Existing(target) => target.revalidate(),
            WriteTarget::Absent(target) => target.revalidate(),
            WriteTarget::Chained(target) => target.revalidate(),
        }
    }

    async fn revalidate_for_execution(&self) -> Result<(), ToolError> {
        match &self.target {
            WriteTarget::Existing(target) => target.revalidate(),
            WriteTarget::Absent(target) => target.revalidate(),
            WriteTarget::Chained(target) => target.revalidate_for_execution(),
        }
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_engine::ToolCompletion, ToolError> {
        let result: Result<ToolResult, ToolError> = async move {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution(
                "prepared write cancelled before commit",
            ));
        }
        let (path, outcome) = match &self.target {
            WriteTarget::Existing(target) => {
                let outcome = target.replace_atomically(&self.bytes)?;
                (target.display_path.clone(), outcome)
            }
            WriteTarget::Absent(target) => {
                let outcome = target.create_atomically(&self.bytes)?;
                (target.display_path.clone(), outcome)
            }
            WriteTarget::Chained(target) => {
                // Direct executions (tests) bypass the engine's serialization
                // lock; the strict predecessor check must hold regardless.
                target.revalidate_for_execution()?;
                let outcome = target.replace_atomically(&self.bytes)?;
                (target.target.display_path.clone(), outcome)
            }
        };
        Ok(ToolResult {
display: None,
retained_output: None,
            title: crate::safe_title(format!("Wrote {}", path.display())),
            output: format!("Wrote {} bytes to {}", self.bytes.len(), path.display()),
            metadata: serde_json::json!({"bytes":self.bytes.len(),"cleanup_warning":outcome.cleanup_warning}),
            truncation: None,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        })
}.await;
        result.map(cookie_agent_engine::ToolCompletion::single)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(unix, windows))]
    use cookie_agent_engine::ToolExecutionContext;
    use cookie_agent_engine::{ToolCall, ToolError, ToolPreparationContext, ToolProvider};
    use cookie_agent_protocol::{
        OperationFingerprint, PermissionAction, RunId, SessionId, ToolCallId,
    };

    use super::WriteTool;

    #[test]
    fn permission_resource_is_the_file_path() {
        let tool = WriteTool::new("/tmp");
        assert_eq!(
            tool.get_permission_resource(
                "write",
                &serde_json::json!({"filePath":"out.txt","content":"x"})
            )
            .expect("permission resource"),
            ("write", Some("out.txt".into()))
        );
        assert!(matches!(
            tool.get_permission_resource("write", &serde_json::json!({"content":"x"})),
            Err(ToolError::Failed(_))
        ));
    }

    #[test]
    fn display_argument_abbreviates_workspace_and_home_paths() {
        let workspace = tempfile::tempdir().expect("workspace");
        let tool = WriteTool::new(workspace.path());
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
            cwd: root.to_owned(),
            workspace_root: root.to_owned(),
            turn_context: crate::test_turn_context(),
        }
    }

    #[tokio::test]
    async fn distinct_write_content_has_distinct_fingerprint() {
        let root = tempfile::tempdir().expect("root");
        let tool = WriteTool::new(root.path());
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
        let prepared = WriteTool::new(root.path())
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
            PermissionAction::Write
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

        let tool = WriteTool::new(&workspace);
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
                ("write", Some(requested.to_owned()))
            );
            crate::assert_workspace_rule_allows(
                &prepared,
                &workspace,
                PermissionAction::Write,
                "routes/*",
            );
            prepared
                .execute_for_test(
                    ToolExecutionContext::for_test(
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
        let prepared = WriteTool::new(root.path())
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
                ToolExecutionContext::for_test(
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
    async fn write_route_swap_is_operation_changed_and_routes_affect_fingerprint() {
        use std::{fs, os::unix::fs::symlink};

        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "old").expect("fixture");
        symlink("value.txt", root.path().join("left")).expect("left route");
        symlink("value.txt", root.path().join("right")).expect("right route");
        let tool = WriteTool::new(root.path());
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

        fs::remove_file(root.path().join("left")).expect("remove route");
        symlink("value.txt", root.path().join("left")).expect("replace route");
        let error = left
            .execute_for_test(
                ToolExecutionContext::for_test(
                    root.path().join("artifacts"),
                    crate::test_turn_context(),
                )
                .expect("execution context"),
            )
            .await
            .expect_err("route swap must fail");
        assert!(matches!(error, ToolError::OperationChanged(_)));
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
        ToolExecutionContext::for_test(root.join("artifacts"), crate::test_turn_context())
            .expect("execution context")
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn parallel_same_file_writes_chain_in_call_order() {
        use std::fs;

        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "old").expect("fixture");
        let prepared = WriteTool::new(root.path())
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

        // The chained write must not run before its predecessor.
        let error = second
            .revalidate_for_execution_for_test()
            .await
            .expect_err("predecessor has not run yet");
        assert!(matches!(error, ToolError::OperationChanged(_)));

        first
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("first write");
        second
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("chained write");
        // The last write in call order wins deterministically.
        assert_eq!(
            fs::read_to_string(root.path().join("value.txt")).unwrap(),
            "two"
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn absent_write_targets_are_not_chained() {
        let root = tempfile::tempdir().expect("root");
        let prepared = WriteTool::new(root.path())
            .prepare_parallel(
                context(root.path()),
                vec![
                    write_call("created.txt", "one"),
                    write_call("created.txt", "two"),
                ],
            )
            .await;
        let mut prepared = prepared.into_iter();
        let first = prepared.next().expect("first").expect("first write");
        let second = prepared.next().expect("second").expect("second write");
        first
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("first creation");
        let error = second
            .execute_for_test(execution_context(root.path()))
            .await
            .expect_err("second creation must observe the first");
        assert!(matches!(error, ToolError::OperationChanged(_)));
    }
}
