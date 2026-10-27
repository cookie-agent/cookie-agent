use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use cookie_agent_protocol::{
    DiffAsOf, DiffBlock, DiffChange, DisplayBlock, Open, PersistedToolResult as ToolResult,
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

/// A write shows a line diff only when both its sides are UTF-8 text of at
/// most this many bytes, and otherwise only the line counts.
const MAX_DIFF_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct WriteTool {
    workspace: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    #[serde(
        rename = "filePath",
        deserialize_with = "crate::path_args::deserialize"
    )]
    /// Path to the file. Relative paths resolve against the session working directory.
    file_path: String,
    /// Everything the file will hold.
    content: String,
}

/// What the model reads about `write`.
const DESCRIPTION: &str = "Write a whole file: create it, or replace everything it holds with `content`, overwriting it without a check. Missing parent folders are created. The write is atomic: the file holds either its old content or all of the new. The result says whether the file was created or replaced. To change part of a file, use `edit`.";

struct WriteExecutor {
    target: fs_cap::PreparedTarget,
    bytes: Vec<u8>,
    /// The path clients see.
    shown_path: PathBuf,
}

/// The file a write replaces, as far as its diff needs it.
enum Replaced {
    /// No file: the write creates one.
    Nothing,
    /// UTF-8 text of at most [`MAX_DIFF_BYTES`].
    Text(String),
    /// Anything else, by its line count.
    Lines(u64),
}

impl Replaced {
    /// What `target` holds now. A file that cannot be read counts no lines.
    fn of(target: &fs_cap::PreparedTarget) -> Self {
        match target {
            fs_cap::PreparedTarget::Existing(existing) => {
                Self::read(existing).unwrap_or(Self::Lines(0))
            }
            fs_cap::PreparedTarget::Absent(_) => Self::Nothing,
        }
    }

    fn read(existing: &fs_cap::PreparedExisting) -> Result<Self, ToolError> {
        let io = |error: std::io::Error| ToolError::execution(error.to_string());
        let mut file = existing.open_file()?;
        let mut head = Vec::new();
        (&mut file)
            .take(MAX_DIFF_BYTES as u64 + 1)
            .read_to_end(&mut head)
            .map_err(io)?;
        if head.len() <= MAX_DIFF_BYTES {
            return Ok(match String::from_utf8(head) {
                Ok(text) => Self::Text(text),
                Err(error) => Self::Lines(line_count(error.as_bytes())),
            });
        }
        // Count the rest without holding it.
        let mut newlines = memchr::memchr_iter(b'\n', &head).count() as u64;
        let mut last = head.last().copied();
        let mut chunk = vec![0; 64 * 1024];
        loop {
            let read = file.read(&mut chunk).map_err(io)?;
            if read == 0 {
                break;
            }
            newlines += memchr::memchr_iter(b'\n', &chunk[..read]).count() as u64;
            last = Some(chunk[read - 1]);
        }
        Ok(Self::Lines(
            newlines + u64::from(last.is_some_and(|byte| byte != b'\n')),
        ))
    }
}

/// The lines of `bytes`, a last line without a newline included.
fn line_count(bytes: &[u8]) -> u64 {
    memchr::memchr_iter(b'\n', bytes).count() as u64
        + u64::from(bytes.last().is_some_and(|byte| *byte != b'\n'))
}

/// The diff of writing `new` over `replaced`, as of `as_of`: a line diff
/// when both are text within [`MAX_DIFF_BYTES`], else the change and its
/// line counts.
fn write_diff(path: &Path, replaced: &Replaced, new: &[u8], as_of: DiffAsOf) -> DisplayBlock {
    let change = match replaced {
        Replaced::Nothing => DiffChange::Created,
        Replaced::Text(_) | Replaced::Lines(_) => DiffChange::Modified,
    };
    let old = match replaced {
        Replaced::Nothing => Some(""),
        Replaced::Text(text) => Some(text.as_str()),
        Replaced::Lines(_) => None,
    };
    let new_text = (new.len() <= MAX_DIFF_BYTES)
        .then(|| std::str::from_utf8(new).ok())
        .flatten();
    if let (Some(old), Some(new_text)) = (old, new_text) {
        let changes = display::line_diff_changes(old, new_text);
        return DisplayBlock::Diff(display::diff_block(
            path,
            change,
            &display::raw_lines(old),
            &changes,
            as_of,
        ));
    }
    let removed = match replaced {
        Replaced::Nothing => 0,
        Replaced::Text(text) => line_count(text.as_bytes()),
        Replaced::Lines(lines) => *lines,
    };
    DisplayBlock::Diff(DiffBlock {
        path: path.display().to_string(),
        old_path: None,
        language: display::language_for_path(path),
        change,
        hunks: Vec::new(),
        added: line_count(new),
        removed,
        omitted: None,
        as_of,
        full: None,
    })
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

    /// The path clients see for `args`.
    fn shown_path(&self, args: &WriteArgs) -> PathBuf {
        let path = crate::permission_path_label(&args.file_path, &self.workspace);
        PathBuf::from(crate::abbreviated_display_path(&path, &self.workspace))
    }

    /// How clients show a call of `args`: the title and primary argument
    /// name the file, and the preview `diff` shows the content, so there are
    /// no argument blocks.
    fn presentation_of(&self, args: &WriteArgs) -> PreparedPresentation {
        let shown = self.shown_path(args).display().to_string();
        tool_presentation(&format!("Write {shown}"), &shown).with_hint(ToolHint::FILE_WRITE)
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
            description: DESCRIPTION.into(),
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

    fn presentation(&self, call: &ToolCall) -> PreparedPresentation {
        match parse_args::<WriteArgs>("write", call.arguments.clone()) {
            Ok(args) if !args.file_path.is_empty() => self.presentation_of(&args),
            _ => cookie_agent_runtime_api::tool_title_only(&call.name),
        }
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let args: WriteArgs = parse_args("write", call.arguments)?;
        let tool = self.clone();
        crate::blocking(move || prepare_write(&tool, args)).await
    }
}

/// Prepares a write, with a preview of its diff against the file there now.
fn prepare_write(tool: &WriteTool, args: WriteArgs) -> Result<PreparedTool, ToolError> {
    let target = fs_cap::prepare_target(&tool.workspace, std::path::Path::new(&args.file_path))?;
    if matches!(&target, fs_cap::PreparedTarget::Existing(existing) if existing.directory) {
        return Err(ToolError::unsupported_security(
            "write target is a directory",
        ));
    }
    let normalized_arguments = serde_json::json!({
        "filePath": target.display_path(),
        "content": args.content,
    });
    let shown_path = tool.shown_path(&args);
    let preview = display::fit_block(
        write_diff(
            &shown_path,
            &Replaced::of(&target),
            args.content.as_bytes(),
            DiffAsOf::Prepare,
        ),
        None,
    )?;
    let presentation = tool.presentation_of(&args).with_preview([preview]);
    // Writes to one location run one at a time, in call order.
    let serialization_key = PreparedSerializationKey::new(target.serialization_key());
    Ok(PreparedTool::declared(
        normalized_arguments,
        Some(serialization_key),
        Box::new(WriteExecutor {
            target,
            bytes: args.content.into_bytes(),
            shown_path,
        }),
    )?
    .with_presentation(presentation))
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
        let artifacts = Arc::clone(&context.artifacts);
        let session = context.session;
        let (path, bytes, created, diff) = crate::blocking(move || {
            let replaced = Replaced::of(&self.target);
            let created = matches!(replaced, Replaced::Nothing);
            self.target.write(&self.bytes)?;
            let diff = display::fit_block(
                write_diff(&self.shown_path, &replaced, &self.bytes, DiffAsOf::Execute),
                Some(display::Retain::diff_of(
                    artifacts.as_ref(),
                    session,
                    &self.shown_path,
                )),
            )?;
            Ok((
                self.target.display_path().to_owned(),
                self.bytes.len(),
                created,
                diff,
            ))
        })
        .await?;
        let path = path.display().to_string();
        Ok(cookie_agent_runtime_api::ToolCompletion::single(
            ToolResult {
                display: vec![Open::Known(diff)],
                background: None,
                retained_output: None,
                title: crate::safe_title(format!("Wrote {path}")),
                output: text_tools::wrote(created, &path, bytes),
                metadata: serde_json::json!({ "bytes": bytes, "created": created }),
                attachments: Vec::new(),
                additional_messages: Vec::new(),
                loaded_tools: Vec::new(),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        DiffAsOf, DiffBlock, DiffChange, DiffLine, DiffLineOp, DisplayBlock, Open,
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
        let first = crate::identified(&tool, "write", root.path(), first);
        let second = crate::identified(&tool, "write", root.path(), second);
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
        let prepared = crate::identified(
            &WriteTool::new(&crate::test_workspace(root.path())),
            "write",
            root.path(),
            prepared,
        );
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
            let prepared = crate::identified(&tool, "write", &workspace, prepared);
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
        let left = crate::identified(&tool, "write", root.path(), left);
        let right = crate::identified(&tool, "write", root.path(), right);
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

    /// The diff a call's blocks hold.
    #[cfg(any(unix, windows))]
    fn only_diff(blocks: &[Open<DisplayBlock>]) -> &DiffBlock {
        match blocks {
            [Open::Known(DisplayBlock::Diff(diff))] => diff,
            other => panic!("expected one diff, got {other:?}"),
        }
    }

    #[cfg(any(unix, windows))]
    fn numbers(diff: &DiffBlock) -> Vec<(u64, u64, u64, u64)> {
        diff.hunks
            .iter()
            .map(|hunk| {
                (
                    hunk.old_start,
                    hunk.old_lines,
                    hunk.new_start,
                    hunk.new_lines,
                )
            })
            .collect()
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn a_created_file_previews_and_reports_its_lines_as_added() {
        let root = tempfile::tempdir().expect("root");
        let tool = WriteTool::new(&crate::test_workspace(root.path()));
        let prepared = tool
            .prepare(context(root.path()), write_call("new.rs", "a\nb\n"))
            .await
            .expect("prepare");
        let presentation = prepared.presentation().expect("presentation").clone();
        assert_eq!(presentation.title.as_str(), "Write new.rs");
        assert_eq!(
            presentation.hint.as_ref().map(|hint| hint.as_str()),
            Some("file_write")
        );
        // The title names the file and the preview shows the content.
        assert!(presentation.arguments.is_empty());
        let preview = only_diff(&presentation.preview);
        assert_eq!(
            (preview.change, preview.as_of),
            (DiffChange::Created, DiffAsOf::Prepare)
        );
        assert_eq!(numbers(preview), [(0, 0, 1, 2)]);
        let result = prepared
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("write");
        let written = only_diff(&result.display);
        assert_eq!(
            (
                written.change,
                written.as_of,
                written.added,
                written.removed
            ),
            (DiffChange::Created, DiffAsOf::Execute, 2, 0)
        );
        assert_eq!(
            written.hunks[0].lines,
            [
                DiffLine {
                    op: DiffLineOp::Add,
                    text: "a".into()
                },
                DiffLine {
                    op: DiffLineOp::Add,
                    text: "b".into()
                },
            ]
        );
        // The model reads that the file was created.
        let path = crate::shown_root(root.path()).join("new.rs");
        assert_eq!(
            result.output,
            format!("Created {} (4 bytes).", path.display())
        );
        assert_eq!(
            result.metadata,
            serde_json::json!({"bytes": 4, "created": true})
        );
        assert_eq!(result.title.as_str(), format!("Wrote {}", path.display()));
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn a_write_over_a_file_says_it_replaced_it() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("value.txt"), "old").expect("fixture");
        let result = WriteTool::new(&crate::test_workspace(root.path()))
            .prepare(context(root.path()), write_call("value.txt", "x"))
            .await
            .expect("prepare")
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("write");
        let path = crate::shown_root(root.path()).join("value.txt");
        assert_eq!(
            result.output,
            format!("Replaced {} (1 byte).", path.display())
        );
        assert_eq!(result.metadata["created"], false);
    }

    #[test]
    fn unknown_arguments_are_refused() {
        let tool = WriteTool::new(&crate::test_workspace(std::env::temp_dir()));
        let Err(error) = tool.get_permission_resource(
            "write",
            &serde_json::json!({"filePath":"a.txt","content":"","mode":"append"}),
        ) else {
            panic!("an unknown argument is refused");
        };
        assert!(
            error.message().contains("unknown field `mode`"),
            "{}",
            error.message()
        );
        let spec = tool
            .tools_for_session(&cookie_agent_runtime_api::SessionToolContext::new(
                cookie_agent_protocol::SessionId::new_v7(),
            ))
            .expect("write spec")
            .remove(0);
        assert_eq!(spec.parameters["additionalProperties"], false);
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn a_modified_file_shows_a_line_diff_against_the_text_it_replaced() {
        let root = tempfile::tempdir().expect("root");
        let old = (1..=20)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        std::fs::write(root.path().join("value.txt"), &old).expect("fixture");
        let new = old.replace("line 10\n", "ten\n");
        let prepared = WriteTool::new(&crate::test_workspace(root.path()))
            .prepare(context(root.path()), write_call("value.txt", &new))
            .await
            .expect("prepare");
        let preview = only_diff(&prepared.presentation().expect("presentation").preview).clone();
        assert_eq!(preview.change, DiffChange::Modified);
        assert_eq!(numbers(&preview), [(5, 11, 5, 11)]);
        // Another change lands first: the result diffs against it.
        std::fs::write(
            root.path().join("value.txt"),
            old.replace("line 1\n", "one\n"),
        )
        .expect("change");
        let result = prepared
            .execute_for_test(execution_context(root.path()))
            .await
            .expect("write");
        let written = only_diff(&result.display);
        assert_eq!(written.as_of, DiffAsOf::Execute);
        assert_eq!(numbers(written), [(1, 15, 1, 15)]);
        assert_eq!((written.added, written.removed), (2, 2));
    }

    #[test]
    fn large_or_binary_sides_show_counts_without_hunks() {
        let path = std::path::Path::new("data.bin");
        let large = "x\n".repeat(super::MAX_DIFF_BYTES);
        let DisplayBlock::Diff(diff) = super::write_diff(
            path,
            &super::Replaced::Text("a\nb\nc".into()),
            large.as_bytes(),
            DiffAsOf::Prepare,
        ) else {
            panic!("a diff");
        };
        assert!(diff.hunks.is_empty());
        assert_eq!(
            (diff.change, diff.added, diff.removed),
            (DiffChange::Modified, super::MAX_DIFF_BYTES as u64, 3)
        );
        let DisplayBlock::Diff(diff) = super::write_diff(
            path,
            &super::Replaced::Lines(7),
            b"text\n",
            DiffAsOf::Execute,
        ) else {
            panic!("a diff");
        };
        assert!(diff.hunks.is_empty());
        assert_eq!((diff.added, diff.removed), (1, 7));
        let DisplayBlock::Diff(diff) = super::write_diff(
            path,
            &super::Replaced::Nothing,
            &[0xff, b'\n', 0xfe],
            DiffAsOf::Execute,
        ) else {
            panic!("a diff");
        };
        assert!(diff.hunks.is_empty());
        assert_eq!(
            (diff.change, diff.added, diff.removed),
            (DiffChange::Created, 2, 0)
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn a_binary_or_large_file_on_disk_counts_its_lines() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("blob"), [0xff, b'\n', 0xfe, b'\n', 0]).expect("binary");
        let mut large = "y\n".repeat(super::MAX_DIFF_BYTES / 2);
        large.push_str("tail");
        std::fs::write(root.path().join("large"), &large).expect("large");
        let tool = WriteTool::new(&crate::test_workspace(root.path()));
        for (name, removed) in [("blob", 3), ("large", super::MAX_DIFF_BYTES as u64 / 2 + 1)] {
            let prepared = tool
                .prepare(context(root.path()), write_call(name, "new\n"))
                .await
                .expect("prepare");
            let preview =
                only_diff(&prepared.presentation().expect("presentation").preview).clone();
            assert!(preview.hunks.is_empty(), "{name}");
            assert_eq!((preview.added, preview.removed), (1, removed), "{name}");
        }
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
        let tool = WriteTool::new(&crate::test_workspace(root.path()));
        let first = crate::identified(&tool, "write", root.path(), first);
        let second = crate::identified(&tool, "write", root.path(), second);
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
