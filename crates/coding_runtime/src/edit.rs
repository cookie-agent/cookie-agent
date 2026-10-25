use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use cookie_agent_protocol::{
    DiffAsOf, DiffChange, DisplayBlock, Open, PersistedToolResult as ToolResult,
    PreparedPresentation, ToolHint,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedSerializationKey, PreparedTool, SessionToolContext, ToolCall,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec, text_tools,
    tool_presentation,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use cookie_agent_runtime_api::WorkspaceContext;

use crate::{display, fs_cap, parse_args, schema};

#[derive(Clone, Debug)]
pub struct EditTool {
    workspace: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    #[serde(
        rename = "filePath",
        deserialize_with = "crate::path_args::deserialize"
    )]
    /// Path to the file. Relative paths resolve against the session working directory.
    file_path: String,
    /// The exact text to replace, whitespace and indentation included; not
    /// empty. It must occur exactly once in the file unless `replaceAll` is
    /// set.
    #[serde(rename = "oldString")]
    old_string: String,
    /// The text that replaces it.
    #[serde(rename = "newString")]
    new_string: String,
    /// Replace every occurrence of `oldString` instead of exactly one.
    /// Defaults to false.
    #[serde(rename = "replaceAll", default)]
    replace_all: bool,
}

/// What the model reads about `edit`.
const DESCRIPTION: &str = "Replace exact text in a UTF-8 text file, atomically. `oldString` is matched exactly, whitespace and indentation included, and must occur exactly once in the file: when it occurs zero or several times the edit fails before it is approved and changes nothing, so include more surrounding text to make it unique, or set `replaceAll` to replace every occurrence. The line numbers `read` shows are not part of the file. Text files only; use `write` to create a file or replace all of it.";

struct EditExecutor {
    target: fs_cap::PreparedExisting,
    args: EditArgs,
    /// The path clients see.
    shown_path: PathBuf,
}

/// Counts `old` in `text`, the file `target`, failing as an edit of `text`
/// would.
fn match_count(text: &str, args: &EditArgs, target: &Path) -> Result<usize, ToolError> {
    let count = text.matches(&args.old_string).count();
    if count == 0 || (!args.replace_all && count != 1) {
        return Err(ToolError::execution(text_tools::occurrence_error(
            &args.old_string,
            count,
            &target.display().to_string(),
            !args.replace_all,
        )));
    }
    Ok(count)
}

fn apply(text: &str, args: &EditArgs, target: &Path) -> Result<(String, usize), ToolError> {
    let count = match_count(text, args, target)?;
    let replaced = if args.replace_all {
        text.replace(&args.old_string, &args.new_string)
    } else {
        text.replacen(&args.old_string, &args.new_string, 1)
    };
    Ok((replaced, count))
}

/// The byte ranges of `text` an edit replaces, as [`apply`] finds them.
fn replaced_ranges(text: &str, args: &EditArgs) -> Vec<(usize, usize)> {
    let ranges = text
        .match_indices(&args.old_string)
        .map(|(start, matched)| (start, start + matched.len()));
    if args.replace_all {
        ranges.collect()
    } else {
        ranges.take(1).collect()
    }
}

/// The diff of applying the edit to `text`, as of `as_of`.
fn edit_diff(text: &str, args: &EditArgs, path: &Path, as_of: DiffAsOf) -> DisplayBlock {
    let changes =
        display::replacement_changes(text, &replaced_ranges(text, args), &args.new_string);
    DisplayBlock::Diff(display::diff_block(
        path,
        DiffChange::Modified,
        &display::raw_lines(text),
        &changes,
        as_of,
    ))
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

    /// The path clients see for `args`.
    fn shown_path(&self, args: &EditArgs) -> PathBuf {
        let path = crate::permission_path_label(&args.file_path, &self.workspace);
        PathBuf::from(crate::abbreviated_display_path(&path, &self.workspace))
    }

    /// How clients show a call of `args`. The title and primary argument
    /// name the file, so the arguments add only what they do not show: that
    /// every match is replaced, and the replacement as code when no preview
    /// `diff` shows it in place.
    fn presentation_of(&self, args: &EditArgs, previewed: bool) -> PreparedPresentation {
        let path = self.shown_path(args);
        let shown = path.display().to_string();
        let mut arguments = Vec::new();
        if args.replace_all {
            arguments.push(DisplayBlock::key_value([("replace all", "yes".to_owned())]));
        }
        if !previewed {
            arguments.push(DisplayBlock::code(
                args.new_string.clone(),
                display::language_for_path(&path),
            ));
        }
        tool_presentation(&format!("Edit {shown}"), &shown)
            .with_hint(ToolHint::FILE_EDIT)
            .with_arguments(arguments)
    }

    /// Builds the prepared call, with a preview of the edit applied to
    /// `text`, the file as preparation read it. The replacement itself is
    /// computed at execution from the file's content at that moment, so an
    /// edit approved later never reverts changes made while it waited.
    fn prepare_edit(
        &self,
        args: EditArgs,
        target: fs_cap::PreparedExisting,
        text: &str,
    ) -> Result<PreparedTool, ToolError> {
        let shown_path = self.shown_path(&args);
        let preview =
            display::fit_block(edit_diff(text, &args, &shown_path, DiffAsOf::Prepare), None)?;
        let presentation = self.presentation_of(&args, true).with_preview([preview]);
        let normalized_arguments = serde_json::json!({
            "filePath": target.display_path,
            "oldString": args.old_string,
            "newString": args.new_string,
            "replaceAll": args.replace_all,
        });
        let serialization_key = PreparedSerializationKey::new(target.serialization_key());
        Ok(PreparedTool::declared(
            normalized_arguments,
            Some(serialization_key),
            Box::new(EditExecutor {
                target,
                args,
                shown_path,
            }),
        )?
        .with_presentation(presentation))
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
            description: DESCRIPTION.into(),
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

    fn presentation(&self, call: &ToolCall) -> PreparedPresentation {
        match parse_args::<EditArgs>("edit", call.arguments.clone()) {
            Ok(args) if !args.file_path.is_empty() => self.presentation_of(&args, false),
            _ => cookie_agent_runtime_api::tool_title_only(&call.name),
        }
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
            let text = read_text(&target)?;
            match_count(&text, &args, &target.display_path)?;
            tool.prepare_edit(args, target, &text)
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
                results[index] = Some(match current {
                    Ok(current) => {
                        apply(current, &args, &target.display_path).and_then(|(next, _)| {
                            let prepared = self.prepare_edit(args, target, current);
                            *current = next;
                            prepared
                        })
                    }
                    // Every member of the group reports the shared read failure.
                    Err(error) => Err(ToolError::execution(error.message())),
                });
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
        let artifacts = Arc::clone(&context.artifacts);
        let session = context.session;
        let (path, count, diff) = crate::blocking(move || {
            let text = read_text(&self.target)?;
            let (replaced, count) = apply(&text, &self.args, &self.target.display_path)?;
            self.target.replace(replaced.as_bytes())?;
            let diff = display::fit_block(
                edit_diff(&text, &self.args, &self.shown_path, DiffAsOf::Execute),
                Some(display::Retain::diff_of(
                    artifacts.as_ref(),
                    session,
                    &self.shown_path,
                )),
            )?;
            Ok((self.target.display_path.clone(), count, diff))
        })
        .await?;
        let path = path.display().to_string();
        Ok(cookie_agent_runtime_api::ToolCompletion::single(
            ToolResult {
                display: vec![Open::Known(diff)],
                background: None,
                retained_output: None,
                title: crate::safe_title(format!("Edited {path}")),
                output: text_tools::edited(&path, count),
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
    use cookie_agent_protocol::{
        DiffAsOf, DiffBlock, DisplayBlock, Open, RunId, SessionId, ToolCallId,
    };
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
            let prepared = crate::identified(&tool, "edit", &workspace, prepared);
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
        let left = crate::identified(&tool, "edit", root.path(), left);
        let right = crate::identified(&tool, "edit", root.path(), right);
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
        let tool = EditTool::new(&crate::test_workspace(root.path()));
        let first = crate::identified(&tool, "edit", root.path(), first);
        let second = crate::identified(&tool, "edit", root.path(), second);
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

    /// The single diff of `blocks`.
    #[cfg(any(unix, windows))]
    fn only_diff(blocks: &[Open<DisplayBlock>]) -> &DiffBlock {
        match blocks {
            [Open::Known(DisplayBlock::Diff(diff))] => diff,
            other => panic!("expected one diff, got {other:?}"),
        }
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn an_edit_previews_its_diff_and_reports_the_one_it_applied() {
        let root = tempfile::tempdir().expect("root");
        let text = (1..=20)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(root.path().join("value.rs"), &text).expect("fixture");
        let tool = EditTool::new(&crate::test_workspace(root.path()));
        let prepared = tool
            .prepare(
                context(root.path()),
                edit_call("value.rs", "line 10\n", "ten\n"),
            )
            .await
            .expect("prepare");
        let presentation = prepared
            .presentation()
            .expect("prepared presentation")
            .clone();
        assert_eq!(presentation.title.as_str(), "Edit value.rs");
        assert_eq!(
            presentation.hint.as_ref().map(|hint| hint.as_str()),
            Some("file_edit")
        );
        // The title names the file and the preview diff shows the
        // replacement in place.
        assert!(presentation.arguments.is_empty());
        let unprepared = tool.presentation(&edit_call("value.rs", "line 10\n", "ten\n"));
        assert_eq!(
            unprepared.arguments,
            [Open::Known(DisplayBlock::code(
                "ten\n",
                Some("rust".into())
            ))],
            "without a preview the replacement shows as code"
        );
        let preview = only_diff(&presentation.preview);
        assert_eq!(preview.as_of, DiffAsOf::Prepare);
        assert_eq!(preview.path, "value.rs");
        assert_eq!(
            preview
                .hunks
                .iter()
                .map(|hunk| (
                    hunk.old_start,
                    hunk.old_lines,
                    hunk.new_start,
                    hunk.new_lines
                ))
                .collect::<Vec<_>>(),
            [(5, 11, 5, 11)]
        );

        // The file changed after preparation: the result shows what the edit
        // did to it then.
        fs::write(root.path().join("value.rs"), format!("new first\n{text}")).expect("change");
        let result = prepared
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("execute");
        let applied = only_diff(&result.display);
        assert_eq!(applied.as_of, DiffAsOf::Execute);
        assert_eq!(
            applied
                .hunks
                .iter()
                .map(|hunk| (hunk.old_start, hunk.new_start))
                .collect::<Vec<_>>(),
            [(6, 6)]
        );
        assert_eq!((applied.added, applied.removed), (1, 1));
        let path = crate::shown_root(root.path()).join("value.rs");
        assert_eq!(
            result.output,
            format!("Edited {}: 1 replacement.", path.display())
        );
        assert_eq!(result.metadata, serde_json::json!({"replacements": 1}));
        assert_eq!(result.title.as_str(), format!("Edited {}", path.display()));
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn edit_errors_name_the_file_and_the_count() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "same same").expect("fixture");
        let tool = EditTool::new(&crate::test_workspace(root.path()));
        let path = crate::shown_root(root.path()).join("value.txt");
        let error = |call| async {
            match tool.prepare(context(root.path()), call).await {
                Ok(_) => panic!("the edit cannot apply"),
                Err(error) => error.message(),
            }
        };
        assert_eq!(
            error(edit_call("value.txt", "same", "new")).await,
            format!(
                "`same` occurs 2 times in {}; include more context so it occurs once or set replaceAll",
                path.display()
            )
        );
        assert_eq!(
            error(edit_call("value.txt", "other", "new")).await,
            format!(
                "`other` occurs 0 times in {}; include more context so it occurs once or set replaceAll",
                path.display()
            )
        );
        // With replaceAll there is nothing to set.
        let mut call = edit_call("value.txt", "other", "new");
        call.arguments["replaceAll"] = true.into();
        assert_eq!(
            error(call).await,
            format!(
                "`other` occurs 0 times in {}; include more context so it occurs once",
                path.display()
            )
        );
        // A long oldString is echoed cut.
        let long = "x".repeat(300);
        assert_eq!(
            error(edit_call("value.txt", &long, "new")).await,
            format!(
                "`{}…` occurs 0 times in {}; include more context so it occurs once or set replaceAll",
                "x".repeat(200),
                path.display()
            )
        );
        assert_eq!(
            error(edit_call("missing.txt", "a", "b")).await,
            format!(
                "{} does not exist",
                crate::shown_root(root.path()).join("missing.txt").display()
            )
        );
        // replaceAll replaces every occurrence and says how many.
        let mut call = edit_call("value.txt", "same", "new");
        call.arguments["replaceAll"] = true.into();
        let result = tool
            .prepare(context(root.path()), call)
            .await
            .expect("prepare")
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("execute");
        assert_eq!(
            result.output,
            format!("Edited {}: 2 replacements.", path.display())
        );
    }

    #[test]
    fn unknown_arguments_are_refused() {
        let tool = EditTool::new(&crate::test_workspace(std::env::temp_dir()));
        let Err(error) = tool.get_permission_resource(
            "edit",
            &serde_json::json!({"filePath":"a.txt","oldString":"a","newString":"b","old":"a"}),
        ) else {
            panic!("an unknown argument is refused");
        };
        assert!(
            error.message().contains("unknown field `old`"),
            "{}",
            error.message()
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn a_diff_too_large_to_show_names_its_full_content() {
        let root = tempfile::tempdir().expect("root");
        let text = (1..=5_000)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(root.path().join("big.txt"), &text).expect("fixture");
        let prepared = EditTool::new(&crate::test_workspace(root.path()))
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "edit".into(),
                    arguments: serde_json::json!({
                        "filePath": "big.txt",
                        "oldString": "line",
                        "newString": "LINE",
                        "replaceAll": true
                    }),
                },
            )
            .await
            .expect("prepare");
        let preview = only_diff(&prepared.presentation().expect("presentation").preview).clone();
        assert!(preview.omitted.is_some_and(|omitted| omitted.hunks == 1));
        assert!(preview.full.is_none(), "a preview has nowhere to keep it");
        let result = prepared
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("execute");
        let applied = only_diff(&result.display);
        assert_eq!((applied.added, applied.removed), (5_000, 5_000));
        let full = applied.full.as_ref().expect("full diff");
        assert_eq!(full.media_type, "text/plain");
        assert_eq!(full.filename.as_deref(), Some("big.txt.diff"));
        assert!(full.byte_length > cookie_agent_protocol::DISPLAY_BUDGET_BYTES as u64);
        cookie_agent_protocol::validate_display(&result.display).expect("within the budget");
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
