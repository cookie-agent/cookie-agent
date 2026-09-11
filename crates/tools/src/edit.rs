use std::path::PathBuf;

use async_trait::async_trait;
use cookie_agent_engine::{
    PreparedExecutor, PreparedSerializationKey, PreparedTool, SessionToolContext, ToolCall,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::{PermissionAction, PersistedToolResult as ToolResult, Sha256Digest};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{fs_cap, parse_args, prepared_operation, prepared_path_resources, schema};

#[derive(Debug)]
pub struct EditTool {
    workspace: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
struct EditArgs {
    #[serde(
        rename = "filePath",
        deserialize_with = "crate::path_args::deserialize"
    )]
    file_path: String,
    #[serde(rename = "oldString")]
    old_string: String,
    #[serde(rename = "newString")]
    new_string: String,
    #[serde(rename = "replaceAll", default)]
    replace_all: bool,
}

struct EditExecutor {
    target: EditTarget,
    new_bytes: Vec<u8>,
}

enum EditTarget {
    Existing(fs_cap::PreparedExisting),
    Chained(fs_cap::PreparedChained),
}

impl EditTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }

    /// The per-call front half of preparation: parse arguments and resolve
    /// the target. Shared by `prepare` and `prepare_parallel`.
    fn prepare_target(
        &self,
        ctx: &ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PendingEdit, ToolError> {
        let args: EditArgs = parse_args("edit", call.arguments)?;
        fs_cap::ensure_atomic_write_supported()?;
        if args.old_string.is_empty() {
            return Err(ToolError::execution("oldString must not be empty"));
        }
        let target = fs_cap::prepare_existing(&ctx.cwd, std::path::Path::new(&args.file_path))?;
        if target.directory {
            return Err(ToolError::unsupported_security(
                "edit target is a directory",
            ));
        }
        let bytes = target.read_bytes()?;
        let text =
            String::from_utf8(bytes).map_err(|_| ToolError::execution("edit requires UTF-8"))?;
        Ok(PendingEdit { args, target, text })
    }

    /// Match `oldString` against `text` and build the prepared call. When
    /// `predecessor` is set, the call belongs to a same-target chain: the
    /// fingerprint binds the predecessor's output digest (the content this
    /// edit matched against) and the executor validates that digest instead
    /// of the original target identity, which an earlier call in the chain
    /// atomically replaces. Returns the replacement text for chain
    /// continuation.
    fn prepare_edit(
        &self,
        ctx: &ToolPreparationContext,
        args: EditArgs,
        target: fs_cap::PreparedExisting,
        text: &str,
        predecessor: Option<Sha256Digest>,
    ) -> Result<(PreparedTool, String), ToolError> {
        let count = text.matches(&args.old_string).count();
        if count == 0 {
            return Err(ToolError::execution("oldString was not found"));
        }
        if !args.replace_all && count != 1 {
            return Err(ToolError::execution(format!(
                "oldString matched {count} times"
            )));
        }
        let replaced = if args.replace_all {
            text.replace(&args.old_string, &args.new_string)
        } else {
            text.replacen(&args.old_string, &args.new_string, 1)
        };
        let new_bytes = replaced.clone().into_bytes();
        let input_digest = predecessor
            .clone()
            .unwrap_or_else(|| target.content_digest.clone());
        let mut binding = target.identity.canonical_bytes();
        binding.extend_from_slice(input_digest.as_str().as_bytes());
        binding.extend_from_slice(
            Sha256Digest::of_bytes(args.old_string.as_bytes())
                .as_str()
                .as_bytes(),
        );
        binding.extend_from_slice(
            Sha256Digest::of_bytes(args.new_string.as_bytes())
                .as_str()
                .as_bytes(),
        );
        binding.extend_from_slice(&(count as u64).to_be_bytes());
        binding.extend_from_slice(Sha256Digest::of_bytes(&new_bytes).as_str().as_bytes());
        binding.extend_from_slice(&target.manifest_bytes()?);
        let (resources, policy_labels) = prepared_path_resources(
            PermissionAction::Write,
            "file",
            &target.display_path,
            &self.workspace,
            &binding,
        )?;
        let context = fs_cap::cwd_context_bytes(&ctx.cwd)?;
        let operation = prepared_operation(
            "edit",
            &args,
            vec![(PermissionAction::Write, "edit")],
            resources,
            &context,
        )?;
        let normalized_arguments = serde_json::json!({
            "filePath": target.display_path,
            "oldString": args.old_string,
            "newString": args.new_string,
            "replaceAll": args.replace_all,
        });
        let mut serialization_key = target.identity.device.to_be_bytes().to_vec();
        serialization_key.extend_from_slice(&target.identity.inode.to_be_bytes());
        let executor_target = match predecessor {
            Some(predecessor_digest) => EditTarget::Chained(fs_cap::PreparedChained {
                original_digest: target.content_digest.clone(),
                predecessor_digest,
                target,
            }),
            None => EditTarget::Existing(target),
        };
        let prepared = PreparedTool::new(
            operation,
            normalized_arguments,
            Some(PreparedSerializationKey::new(serialization_key)),
            Box::new(EditExecutor {
                target: executor_target,
                new_bytes,
            }),
        )?
        .with_policy_labels(policy_labels)?;
        Ok((prepared, replaced))
    }
}

struct PendingEdit {
    args: EditArgs,
    target: fs_cap::PreparedExisting,
    text: String,
}
impl Default for EditTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

#[async_trait]
impl ToolProvider for EditTool {
    fn provider_id(&self) -> &'static str {
        "builtin.edit"
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            output: Default::default(),
            concurrency: cookie_agent_engine::ToolConcurrency::Parallel,
            result_truncation: Default::default(),
            name: "edit".into(),
            permission_name: Self::get_permission_name("edit")?.into(),
            description: "Apply a precomputed semantic replacement atomically.".into(),
            parameters: schema::<EditArgs>(),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "edit" => Ok("write"),
            _ => Err(ToolError::execution("edit provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let permission_name = Self::get_permission_name(name)?;
        let args: EditArgs = parse_args("edit", arguments.clone())?;
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
            return Err(ToolError::execution("edit permission resource is missing"));
        };
        Ok(crate::abbreviated_display_path(&path, &self.workspace))
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let args: EditArgs = parse_args("edit", call.arguments)?;
        fs_cap::ensure_atomic_write_supported()?;
        if args.old_string.is_empty() {
            return Err(ToolError::execution("oldString must not be empty"));
        }
        let target = fs_cap::prepare_existing(&ctx.cwd, std::path::Path::new(&args.file_path))?;
        if target.directory {
            return Err(ToolError::unsupported_security(
                "edit target is a directory",
            ));
        }
        let bytes = target.read_bytes()?;
        let text =
            String::from_utf8(bytes).map_err(|_| ToolError::execution("edit requires UTF-8"))?;
        let (prepared, _) = self.prepare_edit(&ctx, args, target, &text, None)?;
        Ok(prepared)
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
        // Prepare each call's target individually, then group by target
        // identity: same-file calls chain in call order, with every edit
        // matching against the content produced by the previous edit.
        let mut pending = Vec::with_capacity(calls.len());
        for call in calls {
            pending.push(Some(self.prepare_target(&ctx, call)));
        }
        let mut results: Vec<Option<Result<PreparedTool, ToolError>>> =
            (0..pending.len()).map(|_| None).collect();
        let mut groups: Vec<((u64, u64), Vec<usize>)> = Vec::new();
        for (index, entry) in pending.iter_mut().enumerate() {
            match entry {
                Some(Ok(pending_edit)) => {
                    let key = (
                        pending_edit.target.identity.device,
                        pending_edit.target.identity.inode,
                    );
                    if let Some((_, indexes)) =
                        groups.iter_mut().find(|(candidate, _)| *candidate == key)
                    {
                        indexes.push(index);
                    } else {
                        groups.push((key, vec![index]));
                    }
                }
                // Early per-call failures (invalid arguments, missing target,
                // directory, non-UTF-8) stay isolated in their own slot.
                Some(Err(_)) => {
                    let Err(error) = entry.take().expect("each edit call is visited once") else {
                        unreachable!("error entry");
                    };
                    results[index] = Some(Err(error));
                }
                None => unreachable!("each edit call is visited once"),
            }
        }
        for (_, indexes) in groups {
            let base_digest = pending[indexes[0]]
                .as_ref()
                .and_then(|entry| entry.as_ref().ok())
                .expect("grouped edit prepared its target")
                .target
                .content_digest
                .clone();
            // Current chained content, advanced by every successful edit. A
            // failed edit is skipped: later edits continue from the last good
            // state. The first successful edit prepares standalone (it
            // validates the original target identity); later edits validate
            // their predecessor's output digest.
            let mut chain_text: Option<String> = None;
            let mut chain_started = false;
            for index in indexes {
                let pending_edit = pending[index]
                    .take()
                    .expect("grouped edit pending")
                    .expect("grouped edit prepared");
                // A member that observed different content than the chain base
                // (external writer between preparations) prepares standalone.
                let chained = pending_edit.target.content_digest == base_digest;
                let (input_text, predecessor) = if chained {
                    let predecessor = if chain_started {
                        chain_text.as_deref().map(|text| {
                            cookie_agent_protocol::Sha256Digest::of_bytes(text.as_bytes())
                        })
                    } else {
                        None
                    };
                    let text = chain_text
                        .clone()
                        .unwrap_or_else(|| pending_edit.text.clone());
                    (text, predecessor)
                } else {
                    (pending_edit.text.clone(), None)
                };
                match self.prepare_edit(
                    &ctx,
                    pending_edit.args,
                    pending_edit.target,
                    &input_text,
                    predecessor,
                ) {
                    Ok((prepared, new_text)) => {
                        if chained {
                            chain_text = Some(new_text);
                            chain_started = true;
                        }
                        results[index] = Some(Ok(prepared));
                    }
                    Err(error) => {
                        results[index] = Some(Err(error));
                    }
                }
            }
        }
        results
            .into_iter()
            .map(|result| result.expect("every edit call is grouped"))
            .collect()
    }
}

#[async_trait]
impl PreparedExecutor for EditExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        match &self.target {
            EditTarget::Existing(target) => target.revalidate(),
            EditTarget::Chained(target) => target.revalidate(),
        }
    }
    async fn revalidate_for_execution(&self) -> Result<(), ToolError> {
        match &self.target {
            EditTarget::Existing(target) => target.revalidate(),
            EditTarget::Chained(target) => target.revalidate_for_execution(),
        }
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_engine::ToolCompletion, ToolError> {
        let result: Result<ToolResult, ToolError> = async move {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution(
                "prepared edit cancelled before commit",
            ));
        }
        let path = match &self.target {
            EditTarget::Existing(target) => target.display_path.clone(),
            EditTarget::Chained(target) => target.target.display_path.clone(),
        };
        let outcome = match &self.target {
            EditTarget::Existing(target) => target.replace_atomically(&self.new_bytes)?,
            EditTarget::Chained(target) => {
                // Direct executions (tests) bypass the engine's serialization
                // lock; the strict predecessor check must hold regardless.
                target.revalidate_for_execution()?;
                target.replace_atomically(&self.new_bytes)?
            }
        };
        Ok(ToolResult {
display: None,
retained_output: None,
            title: crate::safe_title(format!("Edited {}", path.display())),
            output: "Edit applied atomically".into(),
            metadata: serde_json::json!({"new_sha256":Sha256Digest::of_bytes(&self.new_bytes),"cleanup_warning":outcome.cleanup_warning}),
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
    use std::fs;

    #[cfg(any(unix, windows))]
    use cookie_agent_engine::ToolExecutionContext;
    use cookie_agent_engine::{ToolCall, ToolError, ToolPreparationContext, ToolProvider};
    #[cfg(any(unix, windows))]
    use cookie_agent_protocol::OperationFingerprint;
    use cookie_agent_protocol::{RunId, SessionId, ToolCallId};

    use super::EditTool;

    #[test]
    fn permission_resource_is_the_file_path() {
        let tool = EditTool::new("/tmp");
        assert_eq!(
            tool.get_permission_resource(
                "edit",
                &serde_json::json!({
                    "filePath":"value.txt",
                    "oldString":"a",
                    "newString":"b"
                })
            )
            .expect("permission resource"),
            ("write", Some("value.txt".into()))
        );
        assert!(matches!(
            tool.get_permission_resource(
                "edit",
                &serde_json::json!({"oldString":"a","newString":"b"})
            ),
            Err(ToolError::Failed(_))
        ));
    }

    #[test]
    fn display_argument_abbreviates_workspace_and_home_paths() {
        let workspace = tempfile::tempdir().expect("workspace");
        let tool = EditTool::new(workspace.path());
        let args = serde_json::json!({
            "filePath":workspace.path().join("value.txt"),
            "oldString":"a",
            "newString":"b"
        });
        assert_eq!(
            tool.get_display_argument("edit", &args).expect("workspace"),
            "value.txt"
        );
        let home = cookie_agent_protocol::paths::home_dir().expect("home directory");
        assert_eq!(
            tool.get_display_argument(
                "edit",
                &serde_json::json!({
                    "filePath": home.join("value.txt"),
                    "oldString":"a",
                    "newString":"b"
                })
            )
            .expect("home"),
            "~/value.txt"
        );
    }

    #[tokio::test]
    async fn ambiguous_single_replacement_is_rejected_before_approval() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "same same").expect("fixture");
        let result = EditTool::new(root.path())
            .prepare(
                ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: root.path().to_owned(),
                    workspace_root: root.path().to_owned(),
                    turn_context: crate::test_turn_context(),
                },
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "edit".into(),
                    arguments: serde_json::json!({
                        "filePath":"value.txt",
                        "oldString":"same",
                        "newString":"new"
                    }),
                },
            )
            .await;
        assert!(matches!(result, Err(ToolError::Failed(_))));
    }

    #[cfg(any(unix, windows))]
    fn context(root: &std::path::Path) -> ToolPreparationContext {
        ToolPreparationContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            cwd: root.to_owned(),
            workspace_root: root.to_owned(),
            turn_context: crate::test_turn_context(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_routes_keep_lexical_edit_contract_and_preserve_links() {
        use std::os::unix::fs::symlink;

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
            fs::write(external.join(name), "old value").expect("fixture");
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

        let tool = EditTool::new(&workspace);
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
                        name: "edit".into(),
                        arguments: serde_json::json!({
                            "filePath":requested,
                            "oldString":"old",
                            "newString":"new"
                        }),
                    },
                )
                .await
                .expect("prepare symlink edit");
            assert_eq!(prepared.operation().resources().len(), 1);
            assert_eq!(prepared.policy_labels(), [Some(requested.to_owned())]);
            assert_eq!(
                prepared.normalized_arguments()["filePath"],
                workspace.join(requested).to_string_lossy().as_ref()
            );
            assert_eq!(
                tool.get_permission_resource("edit", prepared.normalized_arguments())
                    .expect("normalized permission resource"),
                ("write", Some(requested.to_owned()))
            );
            crate::assert_workspace_rule_allows(
                &prepared,
                &workspace,
                cookie_agent_protocol::PermissionAction::Write,
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
                .expect("execute symlink edit");
            assert_eq!(fs::read_to_string(destination).unwrap(), "new value");
            assert_eq!(fs::read_link(link).unwrap(), original_link);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dangling_edit_errors_and_ancestor_swap_is_operation_changed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        fs::create_dir(root.path().join("destination")).expect("destination");
        fs::write(root.path().join("destination/value.txt"), "old").expect("fixture");
        symlink("missing.txt", root.path().join("dangling")).expect("dangling route");
        let dangling = EditTool::new(root.path())
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "edit".into(),
                    arguments: serde_json::json!({
                        "filePath":"dangling",
                        "oldString":"old",
                        "newString":"new"
                    }),
                },
            )
            .await;
        assert!(matches!(dangling, Err(ToolError::Failed(_))));

        symlink("destination", root.path().join("ancestor")).expect("ancestor route");
        let prepared = EditTool::new(root.path())
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "edit".into(),
                    arguments: serde_json::json!({
                        "filePath":"ancestor/value.txt",
                        "oldString":"old",
                        "newString":"new"
                    }),
                },
            )
            .await
            .expect("prepare edit");
        fs::remove_file(root.path().join("ancestor")).expect("remove ancestor");
        symlink("destination", root.path().join("ancestor")).expect("replace ancestor");
        let error = prepared
            .execute_for_test(
                ToolExecutionContext::for_test(
                    root.path().join("artifacts"),
                    crate::test_turn_context(),
                )
                .expect("execution context"),
            )
            .await
            .expect_err("ancestor swap must fail");
        assert!(matches!(error, ToolError::OperationChanged(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_routes_to_the_same_target_have_distinct_fingerprints() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "old").expect("fixture");
        symlink("value.txt", root.path().join("left")).expect("left route");
        symlink(root.path().join("value.txt"), root.path().join("right")).expect("right route");
        let tool = EditTool::new(root.path());
        let prepare = |path: &str| {
            tool.prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "edit".into(),
                    arguments: serde_json::json!({
                        "filePath":path,
                        "oldString":"old",
                        "newString":"new"
                    }),
                },
            )
        };
        let left = prepare("left").await.expect("left");
        let right = prepare("right").await.expect("right");
        assert_ne!(
            left.operation().resources()[0].binding_digest,
            right.operation().resources()[0].binding_digest
        );
        assert_ne!(
            OperationFingerprint::from_prepared_operation(left.operation()),
            OperationFingerprint::from_prepared_operation(right.operation())
        );
    }

    #[cfg(any(unix, windows))]
    fn edit_call(path: &str, old: &str, new: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new_v7(),
            name: "edit".into(),
            arguments: serde_json::json!({
                "filePath": path,
                "oldString": old,
                "newString": new
            }),
        }
    }

    #[cfg(any(unix, windows))]
    fn execution_context(root: &std::path::Path) -> ToolExecutionContext {
        ToolExecutionContext::for_test(root.join("artifacts"), crate::test_turn_context())
            .expect("execution context")
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn parallel_same_file_edits_chain_in_call_order() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "alpha beta gamma").expect("fixture");
        let prepared = EditTool::new(root.path())
            .prepare_parallel(
                context(root.path()),
                vec![
                    edit_call("value.txt", "alpha", "ALPHA"),
                    edit_call("value.txt", "beta", "BETA"),
                ],
            )
            .await;
        let mut prepared = prepared.into_iter();
        let first = prepared.next().expect("first").expect("first edit");
        let second = prepared.next().expect("second").expect("second edit");
        assert!(prepared.next().is_none());
        assert_ne!(
            OperationFingerprint::from_prepared_operation(first.operation()),
            OperationFingerprint::from_prepared_operation(second.operation())
        );
        first
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("first edit");
        second
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("chained edit");
        assert_eq!(
            fs::read_to_string(root.path().join("value.txt")).unwrap(),
            "ALPHA BETA gamma"
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn chained_edit_matches_predecessor_output() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "a").expect("fixture");
        let prepared = EditTool::new(root.path())
            .prepare_parallel(
                context(root.path()),
                vec![
                    edit_call("value.txt", "a", "b"),
                    edit_call("value.txt", "b", "c"),
                ],
            )
            .await;
        let mut prepared = prepared.into_iter();
        let first = prepared.next().expect("first").expect("first edit");
        // The second edit's oldString only exists after the first edit ran.
        let second = prepared.next().expect("second").expect("chained edit");
        first
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("first edit");
        second
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("chained edit");
        assert_eq!(
            fs::read_to_string(root.path().join("value.txt")).unwrap(),
            "c"
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn failed_chain_member_skips_without_breaking_the_chain() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "a b c").expect("fixture");
        let prepared = EditTool::new(root.path())
            .prepare_parallel(
                context(root.path()),
                vec![
                    edit_call("value.txt", "a", "A"),
                    edit_call("value.txt", "missing", "X"),
                    edit_call("value.txt", "c", "C"),
                ],
            )
            .await;
        let mut prepared = prepared.into_iter();
        let first = prepared.next().expect("first").expect("first edit");
        let middle = prepared.next().expect("middle");
        assert!(matches!(middle, Err(ToolError::Failed(_))));
        // The third edit chains from the last good state (after the first).
        let third = prepared.next().expect("third").expect("third edit");
        first
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("first edit");
        third
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("third edit");
        assert_eq!(
            fs::read_to_string(root.path().join("value.txt")).unwrap(),
            "A b C"
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn early_preparation_errors_stay_isolated_in_a_batch() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "alpha beta").expect("fixture");
        let prepared = EditTool::new(root.path())
            .prepare_parallel(
                context(root.path()),
                vec![
                    edit_call("missing.txt", "alpha", "ALPHA"),
                    edit_call("value.txt", "beta", "BETA"),
                ],
            )
            .await;
        let mut prepared = prepared.into_iter();
        assert!(matches!(
            prepared.next().expect("first"),
            Err(ToolError::Failed(_))
        ));
        let second = prepared.next().expect("second").expect("second edit");
        second
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("valid edit");
        assert_eq!(
            fs::read_to_string(root.path().join("value.txt")).unwrap(),
            "alpha BETA"
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn chained_edit_requires_predecessor_and_detects_external_changes() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "a b").expect("fixture");
        let tool = EditTool::new(root.path());
        let prepared = tool
            .prepare_parallel(
                context(root.path()),
                vec![
                    edit_call("value.txt", "a", "A"),
                    edit_call("value.txt", "b", "B"),
                ],
            )
            .await;
        let mut prepared = prepared.into_iter();
        let first = prepared.next().expect("first").expect("first edit");
        let second = prepared.next().expect("second").expect("second edit");

        // Executing the chained edit before its predecessor must fail.
        let error = second
            .revalidate_for_execution_for_test()
            .await
            .expect_err("predecessor has not run yet");
        assert!(matches!(error, ToolError::OperationChanged(_)));

        first
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("first edit");

        // An external change after the predecessor must fail the chain.
        fs::write(root.path().join("value.txt"), "tampered").expect("external write");
        let error = second
            .execute_for_test(execution_context(root.path()))
            .await
            .expect_err("external change must fail the chained edit");
        assert!(matches!(error, ToolError::OperationChanged(_)));
    }
}
