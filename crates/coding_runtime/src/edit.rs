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
pub struct EditTool {
    workspace: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
struct EditArgs {
    #[serde(
        rename = "filePath",
        deserialize_with = "crate::path_args::deserialize"
    )]
    /// Path to the file. Relative paths resolve against the session working directory.
    file_path: String,
    #[serde(rename = "oldString")]
    old_string: String,
    #[serde(rename = "newString")]
    new_string: String,
    #[serde(rename = "replaceAll", default)]
    replace_all: bool,
}

struct EditExecutor {
    target: fs_cap::PreparedExisting,
    args: EditArgs,
}

/// Counts `old` in `text`, failing as an edit of `text` would.
fn match_count(text: &str, args: &EditArgs) -> Result<usize, ToolError> {
    let count = text.matches(&args.old_string).count();
    if count == 0 {
        return Err(ToolError::execution("oldString was not found"));
    }
    if !args.replace_all && count != 1 {
        return Err(ToolError::execution(format!(
            "oldString matched {count} times"
        )));
    }
    Ok(count)
}

fn apply(text: &str, args: &EditArgs) -> Result<(String, usize), ToolError> {
    let count = match_count(text, args)?;
    let replaced = if args.replace_all {
        text.replace(&args.old_string, &args.new_string)
    } else {
        text.replacen(&args.old_string, &args.new_string, 1)
    };
    Ok((replaced, count))
}

fn read_text(target: &fs_cap::PreparedExisting) -> Result<String, ToolError> {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut target.open_file()?, &mut text)
        .map_err(|_| ToolError::execution("edit requires UTF-8"))?;
    Ok(text)
}

impl EditTool {
    /// A tool working in `workspace`, the workspace of the coding runtime
    /// that offers it.
    #[must_use]
    pub fn new(workspace: &WorkspaceContext) -> Self {
        Self {
            workspace: workspace.root().to_owned(),
        }
    }

    /// Parses arguments and resolves the target.
    fn prepare_target(
        &self,
        call: ToolCall,
    ) -> Result<(EditArgs, fs_cap::PreparedExisting), ToolError> {
        let args: EditArgs = parse_args("edit", call.arguments)?;
        if args.old_string.is_empty() {
            return Err(ToolError::execution("oldString must not be empty"));
        }
        let target =
            fs_cap::prepare_existing(&self.workspace, std::path::Path::new(&args.file_path))?;
        if target.directory {
            return Err(ToolError::unsupported_security(
                "edit target is a directory",
            ));
        }
        Ok((args, target))
    }

    /// Builds the prepared call. The replacement itself is computed at
    /// execution from the file's content at that moment, so an edit approved
    /// later never reverts changes made while it waited.
    fn prepare_edit(
        &self,
        args: EditArgs,
        target: fs_cap::PreparedExisting,
    ) -> Result<PreparedTool, ToolError> {
        let (resources, policy_labels) = prepared_path_resources(
            PermissionAction::WRITE,
            "file",
            &target.display_path,
            &self.workspace,
            &target.binding_bytes(),
        )?;
        let context = crate::workspace_context_bytes(&self.workspace);
        let operation = prepared_operation(
            "edit",
            &args,
            vec![(PermissionAction::WRITE, "edit")],
            resources,
            &context,
        )?;
        let normalized_arguments = serde_json::json!({
            "filePath": target.display_path,
            "oldString": args.old_string,
            "newString": args.new_string,
            "replaceAll": args.replace_all,
        });
        let serialization_key = PreparedSerializationKey::new(target.serialization_key());
        PreparedTool::new(
            operation,
            normalized_arguments,
            Some(serialization_key),
            Box::new(EditExecutor { target, args }),
        )?
        .with_policy_labels(policy_labels)
    }
}

#[async_trait]
impl ToolProvider for EditTool {
    fn provider_id(&self) -> &'static str {
        "builtin.edit"
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: cookie_agent_runtime_api::ToolConcurrency::Parallel,
            result_truncation: Default::default(),
            name: "edit".into(),
            permission_name: Self::get_permission_name("edit")?.map(Into::into),
            description: "Replace oldString with newString in a file, atomically.".into(),
            parameters: schema::<EditArgs>(),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        match tool_name {
            "edit" => Ok(Some("write")),
            _ => Err(ToolError::execution("edit provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
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
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let tool = self.clone();
        crate::blocking(move || {
            let (args, target) = tool.prepare_target(call)?;
            // Fail fast on an oldString that does not match the current content.
            match_count(&read_text(&target)?, &args)?;
            tool.prepare_edit(args, target)
        })
        .await
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
        let count = calls.len();
        let tool = self.clone();
        crate::blocking(move || Ok(tool.prepare_batch(calls)))
            .await
            .unwrap_or_else(|error| {
                (0..count)
                    .map(|_| Err(ToolError::execution(error.message())))
                    .collect()
            })
    }
}

impl EditTool {
    fn prepare_batch(&self, calls: Vec<ToolCall>) -> Vec<Result<PreparedTool, ToolError>> {
        // Same-file edits execute in call order under one serialization key,
        // each on the content its predecessor produced. Validate them the
        // same way: every edit matches against the text the previous
        // successful edit of that file would leave.
        let mut pending = Vec::with_capacity(calls.len());
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            // Early per-call failures (invalid arguments, missing target,
            // directory) stay isolated in their own slot.
            match self.prepare_target(call) {
                Ok(entry) => {
                    pending.push(Some(entry));
                    results.push(None);
                }
                Err(error) => {
                    pending.push(None);
                    results.push(Some(Err(error)));
                }
            }
        }
        let mut groups: Vec<(Vec<u8>, Vec<usize>)> = Vec::new();
        for (index, entry) in pending.iter().enumerate() {
            let Some((_, target)) = entry else { continue };
            let key = target.serialization_key();
            match groups.iter_mut().find(|(candidate, _)| *candidate == key) {
                Some((_, indexes)) => indexes.push(index),
                None => groups.push((key, vec![index])),
            }
        }
        for (_, indexes) in groups {
            let mut text: Option<Result<String, ToolError>> = None;
            for index in indexes {
                let (args, target) = pending[index].take().expect("grouped edit pending");
                let current = text.get_or_insert_with(|| read_text(&target));
                let validated = match current {
                    Ok(current) => apply(current, &args).map(|(next, _)| *current = next),
                    // Every member of the group reports the shared read failure.
                    Err(error) => Err(ToolError::execution(error.message())),
                };
                results[index] = Some(validated.and_then(|()| self.prepare_edit(args, target)));
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
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_runtime_api::ToolCompletion, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution(
                "prepared edit cancelled before commit",
            ));
        }
        let (path, count) = crate::blocking(move || {
            let (replaced, count) = apply(&read_text(&self.target)?, &self.args)?;
            self.target.replace(replaced.as_bytes())?;
            Ok((self.target.display_path.clone(), count))
        })
        .await?;
        let path = path.display();
        Ok(cookie_agent_runtime_api::ToolCompletion::single(
            ToolResult {
                display: Vec::new(),
                retained_output: None,
                title: crate::safe_title(format!("Edited {path}")),
                output: "Edit applied atomically".into(),
                metadata: serde_json::json!({ "replacements": count }),
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[cfg(any(unix, windows))]
    use cookie_agent_protocol::OperationFingerprint;
    use cookie_agent_protocol::{RunId, SessionId, ToolCallId};
    #[cfg(any(unix, windows))]
    use cookie_agent_runtime_api::ToolExecutionContext;
    use cookie_agent_runtime_api::{ToolCall, ToolError, ToolPreparationContext, ToolProvider};

    use super::EditTool;

    #[test]
    fn permission_resource_is_the_file_path() {
        let tool = EditTool::new(&crate::test_workspace(std::env::temp_dir()));
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
            (Some("write"), Some("value.txt".into()))
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
        let tool = EditTool::new(&crate::test_workspace(workspace.path()));
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
        let result = EditTool::new(&crate::test_workspace(root.path()))
            .prepare(
                ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    origin: crate::test_coding_origin(root.path()),
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
            origin: crate::test_coding_origin(root),
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

        let tool = EditTool::new(&crate::test_workspace(&workspace));
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
                (Some("write"), Some(requested.to_owned()))
            );
            crate::assert_workspace_rule_allows(
                &prepared,
                &workspace,
                cookie_agent_protocol::PermissionAction::WRITE,
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
                .expect("execute symlink edit");
            assert_eq!(fs::read_to_string(destination).unwrap(), "new value");
            assert_eq!(fs::read_link(link).unwrap(), original_link);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dangling_edit_errors_and_retargeted_ancestor_does_not_redirect() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        fs::create_dir(root.path().join("destination")).expect("destination");
        fs::create_dir(root.path().join("other")).expect("other");
        fs::write(root.path().join("destination/value.txt"), "old").expect("fixture");
        fs::write(root.path().join("other/value.txt"), "old").expect("other fixture");
        symlink("missing.txt", root.path().join("dangling")).expect("dangling route");
        let dangling = EditTool::new(&crate::test_workspace(root.path()))
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
        let prepared = EditTool::new(&crate::test_workspace(root.path()))
            .prepare(
                context(root.path()),
                edit_call("ancestor/value.txt", "old", "new"),
            )
            .await
            .expect("prepare edit");
        fs::remove_file(root.path().join("ancestor")).expect("remove ancestor");
        symlink("other", root.path().join("ancestor")).expect("retarget ancestor");
        prepared
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("edit the approved destination");
        assert_eq!(
            fs::read_to_string(root.path().join("destination/value.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("other/value.txt")).unwrap(),
            "old"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_routes_to_the_same_target_have_distinct_fingerprints() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "old").expect("fixture");
        symlink("value.txt", root.path().join("left")).expect("left route");
        symlink(root.path().join("value.txt"), root.path().join("right")).expect("right route");
        let tool = EditTool::new(&crate::test_workspace(root.path()));
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
        cookie_agent_engine::testing::tool_execution_context(
            root.join("artifacts"),
            crate::test_turn_context(),
        )
        .expect("execution context")
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn parallel_same_file_edits_chain_in_call_order() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "alpha beta gamma").expect("fixture");
        let prepared = EditTool::new(&crate::test_workspace(root.path()))
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
        let prepared = EditTool::new(&crate::test_workspace(root.path()))
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
        let prepared = EditTool::new(&crate::test_workspace(root.path()))
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
        let prepared = EditTool::new(&crate::test_workspace(root.path()))
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
    async fn edit_applies_to_content_changed_while_awaiting_execution() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "a b").expect("fixture");
        let tool = EditTool::new(&crate::test_workspace(root.path()));
        let prepared = tool
            .prepare_parallel(
                context(root.path()),
                vec![
                    edit_call("value.txt", "a", "A"),
                    edit_call("value.txt", "A", "AA"),
                ],
            )
            .await;
        let mut prepared = prepared.into_iter();
        let first = prepared.next().expect("first").expect("first edit");
        let second = prepared.next().expect("second").expect("second edit");

        fs::write(root.path().join("value.txt"), "a b c").expect("external write");
        first
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("first edit");
        second
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("second edit");
        assert_eq!(
            fs::read_to_string(root.path().join("value.txt")).unwrap(),
            "AA b c"
        );

        let stale = tool
            .prepare(context(root.path()), edit_call("value.txt", "b", "B"))
            .await
            .expect("prepare");
        fs::write(root.path().join("value.txt"), "no match").expect("external write");
        assert!(matches!(
            stale.execute_for_test(execution_context(root.path())).await,
            Err(ToolError::Failed(_))
        ));
        assert_eq!(
            fs::read_to_string(root.path().join("value.txt")).unwrap(),
            "no match"
        );
    }
}
