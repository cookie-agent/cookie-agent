use std::path::PathBuf;

use async_trait::async_trait;
use cookie_agent_engine::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec, approved_media_type,
};
use cookie_agent_protocol::{PermissionAction, PersistedToolResult as ToolResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{fs_cap, parse_args, prepared_operation, prepared_path_resources, schema};

const DEFAULT_LIMIT: usize = 2_000;

#[derive(Debug)]
pub struct ReadTool {
    workspace: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    #[serde(rename = "filePath")]
    file_path: String,
    /// Maximum number of entries or lines to return. Defaults to 2000.
    limit: Option<usize>,
    /// Zero-based entry or line offset. Defaults to 0.
    offset: Option<usize>,
}

struct ReadExecutor {
    target: fs_cap::PreparedExisting,
    offset: usize,
    limit: usize,
}

impl ReadTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

#[async_trait]
impl ToolProvider for ReadTool {
    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "read".into(),
            permission_name: Self::get_permission_name("read")?.into(),
            description:
                "Read a descriptor-bound file or directory snapshot using a zero-based offset."
                    .into(),
            parameters: schema::<ReadArgs>(),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "read" => Ok("read"),
            _ => Err(ToolError::execution("read provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let permission_name = Self::get_permission_name(name)?;
        let args: ReadArgs = parse_args("read", arguments.clone())?;
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
        if name != "read" {
            return Err(ToolError::execution("read provider received another tool"));
        }
        let args: ReadArgs = parse_args("read", arguments.clone())?;
        if args.file_path.is_empty() {
            return Err(ToolError::execution("filePath must not be empty"));
        }
        let path = crate::permission_path_label(&args.file_path, &self.workspace);
        let mut display = crate::abbreviated_display_path(&path, &self.workspace);
        let window = match (args.offset, args.limit) {
            (Some(offset), Some(limit)) => Some(format!("offset={offset}, limit={limit}")),
            (Some(offset), None) => Some(format!("offset={offset}")),
            (None, Some(limit)) => Some(format!("limit={limit}")),
            (None, None) => None,
        };
        if let Some(window) = window {
            display.push_str(" [");
            display.push_str(&window);
            display.push(']');
        }
        Ok(display)
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        if call.name != "read" {
            return Err(ToolError::execution("read provider received another tool"));
        }
        let mut args: ReadArgs = parse_args("read", call.arguments)?;
        let offset = args.offset.unwrap_or(0);
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 {
            return Err(ToolError::execution("limit must be positive"));
        }
        args.offset = Some(offset);
        args.limit = Some(limit);
        let target = fs_cap::prepare_existing(&ctx.cwd, std::path::Path::new(&args.file_path))?;
        let binding = target.manifest_bytes()?;
        let (resources, policy_labels) = prepared_path_resources(
            PermissionAction::Read,
            if target.directory {
                "directory"
            } else {
                "file"
            },
            &target.display_path,
            &self.workspace,
            &binding,
        )?;
        let context = fs_cap::cwd_context_bytes(&ctx.cwd)?;
        let operation = prepared_operation(
            "read",
            &args,
            vec![(PermissionAction::Read, "read")],
            resources,
            &context,
        )?;
        let normalized_arguments = serde_json::json!({
            "filePath": target.display_path,
            "offset": offset,
            "limit": limit,
        });
        PreparedTool::new(
            operation,
            normalized_arguments,
            None,
            Box::new(ReadExecutor {
                target,
                offset,
                limit,
            }),
        )?
        .with_policy_labels(policy_labels)
    }
}

#[async_trait]
impl PreparedExecutor for ReadExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        self.target.revalidate()
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution(
                "prepared read cancelled before execution",
            ));
        }
        self.target.revalidate()?;
        if self.target.directory {
            let entries = self.target.directory_entries()?;
            let snapshot = serde_json::to_vec(&entries)
                .map_err(|error| ToolError::execution(error.to_string()))?;
            if cookie_agent_protocol::Sha256Digest::of_bytes(&snapshot)
                != self.target.content_digest
            {
                return Err(ToolError::operation_changed(
                    "prepared directory snapshot changed",
                ));
            }
            let page = directory_page(&entries, self.offset, self.limit).collect::<Vec<_>>();
            let mut output = format!(
                "<path>{}</path>\n<type>directory</type>\n<entries>\n",
                self.target.display_path.display()
            );
            for (name, directory) in &page {
                output.push_str(name);
                if *directory {
                    output.push('/');
                }
                output.push('\n');
            }
            output.push_str("</entries>");
            return Ok(ToolResult {
                title: crate::safe_title(format!(
                    "Read directory {}",
                    self.target.display_path.display()
                )),
                output,
                metadata: serde_json::json!({"kind":"directory","shown":page.len(),"total_entries":entries.len()}),
                truncation: None,
                attachments: Vec::new(),
            });
        }
        let bytes = self.target.verified_bytes()?;
        if let Some(mime) = approved_media_type(&self.target.display_path, &bytes)? {
            let attachment = context.retain_attachment(
                mime,
                self.target
                    .display_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned()),
                &bytes,
            )?;
            return Ok(ToolResult {
                title: crate::safe_title(format!(
                    "Read attachment {}",
                    self.target.display_path.display()
                )),
                output: format!("Attached {mime} ({} bytes).", bytes.len()),
                metadata: serde_json::json!({"kind":"attachment","mime_type":mime,"sha256":attachment.sha256}),
                truncation: None,
                attachments: vec![attachment],
            });
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| ToolError::execution("read supports UTF-8 text or approved media"))?;
        let lines = text.lines().collect::<Vec<_>>();
        let mut output = format!(
            "<path>{}</path>\n<type>file</type>\n<content>\n",
            self.target.display_path.display()
        );
        for (index, line) in text_page(&lines, self.offset, self.limit) {
            output.push_str(&format!("{}: {line}\n", index + 1));
        }
        output.push_str("</content>");
        Ok(ToolResult {
            title: crate::safe_title(format!("Read file {}", self.target.display_path.display())),
            output,
            metadata: serde_json::json!({"kind":"text","offset":self.offset,"limit":self.limit,"total_lines":lines.len()}),
            truncation: None,
            attachments: Vec::new(),
        })
    }
}

fn text_page<'a>(
    lines: &'a [&'a str],
    offset: usize,
    limit: usize,
) -> impl Iterator<Item = (usize, &'a str)> + 'a {
    lines.iter().copied().enumerate().skip(offset).take(limit)
}

fn directory_page<T>(entries: &[T], offset: usize, limit: usize) -> impl Iterator<Item = &T> {
    entries.iter().skip(offset).take(limit)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use cookie_agent_engine::{ToolCall, ToolError, ToolPreparationContext, ToolProvider};
    use cookie_agent_protocol::{
        OperationFingerprint, PermissionAction, RunId, SessionId, ToolCallId,
    };

    use super::{ReadTool, directory_page, text_page};

    #[test]
    fn permission_resource_is_the_file_path() {
        let tool = ReadTool::new("/tmp");
        assert_eq!(
            tool.get_permission_resource("read", &serde_json::json!({"filePath":"src/lib.rs"}))
                .expect("permission resource"),
            ("read", Some("src/lib.rs".into()))
        );
        assert!(matches!(
            tool.get_permission_resource("read", &serde_json::json!({})),
            Err(ToolError::Failed(_))
        ));
        assert!(matches!(
            tool.get_permission_resource(
                "read",
                &serde_json::json!({"filePath":"src/lib.rs","byteOffset":0})
            ),
            Err(ToolError::Failed(_))
        ));
    }

    #[test]
    fn display_argument_abbreviates_paths_and_includes_explicit_window() {
        let tool = ReadTool::new("/workspace");
        assert_eq!(
            tool.get_display_argument("read", &serde_json::json!({"filePath":"src/lib.rs"}))
                .expect("relative"),
            "src/lib.rs"
        );
        assert_eq!(
            tool.get_display_argument(
                "read",
                &serde_json::json!({"filePath":"/workspace/src/lib.rs","offset":0,"limit":100})
            )
            .expect("workspace"),
            "src/lib.rs [offset=0, limit=100]"
        );
        let home = cookie_agent_protocol::paths::home_dir().expect("home directory");
        assert_eq!(
            tool.get_display_argument(
                "read",
                &serde_json::json!({"filePath": home.join(".bashrc"),"offset":4})
            )
            .expect("home"),
            "~/.bashrc [offset=4]"
        );
        assert_eq!(
            tool.get_display_argument(
                "read",
                &serde_json::json!({"filePath":"src/lib.rs","limit":25})
            )
            .expect("limit"),
            "src/lib.rs [limit=25]"
        );
        assert!(matches!(
            tool.get_display_argument("read", &serde_json::json!({})),
            Err(ToolError::Failed(_))
        ));
        let presentation = tool.presentation(&ToolCall {
            id: ToolCallId::new_v7(),
            name: "read".into(),
            arguments: serde_json::json!({"filePath":"/workspace/src/lib.rs"}),
        });
        assert_eq!(presentation.title.as_str(), "read");
        assert_eq!(
            presentation
                .primary_argument
                .as_ref()
                .map(cookie_agent_protocol::SafeDisplayText::as_str),
            Some("src/lib.rs")
        );
    }

    #[test]
    fn schema_documents_zero_based_offset_and_limit_default() {
        let tool = ReadTool::new("/workspace");
        let parameters = &tool
            .tools_for_session(&cookie_agent_engine::SessionToolContext {
                session: SessionId::new_v7(),
            })
            .expect("read spec")[0]
            .parameters;
        assert_eq!(
            parameters["properties"]["offset"]["description"],
            "Zero-based entry or line offset. Defaults to 0."
        );
        assert_eq!(
            parameters["properties"]["limit"]["description"],
            "Maximum number of entries or lines to return. Defaults to 2000."
        );
    }

    #[test]
    fn text_pagination_handles_zero_based_boundaries() {
        let empty = Vec::<&str>::new();
        assert!(text_page(&empty, 0, usize::MAX).next().is_none());

        let lines = ["first", "second", "third"];
        assert_eq!(
            text_page(&lines, 0, 2).collect::<Vec<_>>(),
            [(0, "first"), (1, "second")]
        );
        assert_eq!(text_page(&lines, 1, 1).collect::<Vec<_>>(), [(1, "second")]);
        assert!(text_page(&lines, lines.len(), usize::MAX).next().is_none());
        assert!(
            text_page(&lines, lines.len() + 1, usize::MAX)
                .next()
                .is_none()
        );
        assert!(text_page(&lines, usize::MAX, usize::MAX).next().is_none());
        assert_eq!(
            text_page(&lines, 0, usize::MAX).collect::<Vec<_>>(),
            [(0, "first"), (1, "second"), (2, "third")]
        );
    }

    #[test]
    fn directory_pagination_handles_zero_based_boundaries() {
        let empty = Vec::<(String, bool)>::new();
        assert!(directory_page(&empty, 0, usize::MAX).next().is_none());

        let entries = [
            ("alpha".to_owned(), false),
            ("beta".to_owned(), true),
            ("gamma".to_owned(), false),
        ];
        assert_eq!(
            directory_page(&entries, 0, 2).collect::<Vec<_>>(),
            [&entries[0], &entries[1]]
        );
        assert_eq!(
            directory_page(&entries, 1, usize::MAX).collect::<Vec<_>>(),
            [&entries[1], &entries[2]]
        );
        assert!(
            directory_page(&entries, entries.len(), usize::MAX)
                .next()
                .is_none()
        );
        assert!(
            directory_page(&entries, entries.len() + 1, usize::MAX)
                .next()
                .is_none()
        );
        assert!(
            directory_page(&entries, usize::MAX, usize::MAX)
                .next()
                .is_none()
        );
    }

    fn context(root: &Path) -> ToolPreparationContext {
        ToolPreparationContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            cwd: root.to_owned(),
            workspace_root: root.to_owned(),
            turn_context: crate::test_turn_context(),
        }
    }

    async fn prepared(root: &Path, path: &str) -> cookie_agent_engine::PreparedTool {
        ReadTool::new(root)
            .prepare(
                context(root),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "read".into(),
                    arguments: serde_json::json!({"filePath":path}),
                },
            )
            .await
            .expect("prepare read")
    }

    #[tokio::test]
    async fn prepared_read_exposes_canonical_arguments_not_raw_traversal() {
        let root = tempfile::TempDir::new().expect("temp directory");
        std::fs::create_dir(root.path().join("safe")).expect("safe directory");
        std::fs::write(root.path().join(".env"), "secret").expect("env file");
        let prepared = prepared(root.path(), "safe/../.env").await;
        let canonical = prepared
            .normalized_arguments()
            .get("filePath")
            .and_then(serde_json::Value::as_str)
            .expect("canonical file path");
        assert_eq!(Path::new(canonical), root.path().join(".env"));
        assert!(
            !prepared
                .normalized_arguments()
                .to_string()
                .contains("safe/..")
        );
        assert_eq!(prepared.normalized_arguments()["offset"], 0);
        assert_eq!(prepared.normalized_arguments()["limit"], 2_000);
    }

    async fn fingerprint(root: &Path, path: &str) -> OperationFingerprint {
        OperationFingerprint::from_prepared_operation(prepared(root, path).await.operation())
    }

    #[tokio::test]
    async fn outside_workspace_read_has_one_absolute_labeled_resource() {
        let workspace = tempfile::tempdir().expect("workspace");
        let external = tempfile::NamedTempFile::new().expect("external file");
        fs::write(external.path(), "external").expect("fixture");
        let prepared = ReadTool::new(workspace.path())
            .prepare(
                ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: workspace.path().to_owned(),
                    workspace_root: workspace.path().to_owned(),
                    turn_context: crate::test_turn_context(),
                },
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "read".into(),
                    arguments: serde_json::json!({"filePath":external.path()}),
                },
            )
            .await
            .expect("prepare external read");
        assert_eq!(prepared.operation().capabilities().len(), 1);
        assert_eq!(
            prepared.operation().resources()[0].capability,
            PermissionAction::Read
        );
        assert_eq!(
            prepared.policy_labels(),
            [Some(external.path().display().to_string())]
        );
        assert_eq!(
            ReadTool::new(workspace.path())
                .get_permission_resource("read", prepared.normalized_arguments())
                .expect("resource from prepared args"),
            ("read", Some(external.path().display().to_string()))
        );
    }

    #[tokio::test]
    async fn workspace_read_label_is_canonical_workspace_relative() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::create_dir(workspace.path().join("nested")).expect("nested");
        fs::write(workspace.path().join("nested/value.txt"), "value").expect("fixture");
        let prepared = prepared(workspace.path(), "nested/value.txt").await;
        assert_eq!(prepared.policy_labels(), [Some("nested/value.txt".into())]);
        assert_eq!(
            ReadTool::new(workspace.path())
                .get_permission_resource("read", prepared.normalized_arguments())
                .expect("resource from prepared args"),
            ("read", Some("nested/value.txt".into()))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hard_link_through_distinct_parent_capabilities_has_distinct_fingerprint() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir(root.path().join("left")).expect("left");
        fs::create_dir(root.path().join("right")).expect("right");
        fs::write(root.path().join("left/value"), "same inode").expect("fixture");
        fs::hard_link(
            root.path().join("left/value"),
            root.path().join("right/value"),
        )
        .expect("hard link");

        let left_capability = crate::fs_cap::prepare_existing(root.path(), Path::new("left/value"))
            .expect("left capability");
        let right_capability =
            crate::fs_cap::prepare_existing(root.path(), Path::new("right/value"))
                .expect("right capability");
        assert_eq!(left_capability.identity, right_capability.identity);
        assert_ne!(
            left_capability.manifest_bytes().expect("left manifest"),
            right_capability.manifest_bytes().expect("right manifest")
        );

        let left = prepared(root.path(), "left/value").await;
        let right = prepared(root.path(), "right/value").await;
        assert_ne!(
            left.operation().resources()[0].binding_digest,
            right.operation().resources()[0].binding_digest
        );
        assert_ne!(
            OperationFingerprint::from_prepared_operation(left.operation()),
            OperationFingerprint::from_prepared_operation(right.operation())
        );
    }

    #[tokio::test]
    async fn leaf_and_parent_swaps_change_read_fingerprint() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir(root.path().join("tree")).expect("tree");
        fs::write(root.path().join("tree/value"), "same bytes").expect("fixture");
        let original = fingerprint(root.path(), "tree/value").await;

        fs::rename(
            root.path().join("tree/value"),
            root.path().join("tree/old-value"),
        )
        .expect("swap leaf");
        fs::write(root.path().join("tree/value"), "same bytes").expect("replacement leaf");
        let leaf_swapped = fingerprint(root.path(), "tree/value").await;
        assert_ne!(original, leaf_swapped);

        fs::rename(root.path().join("tree"), root.path().join("old-tree")).expect("swap parent");
        fs::create_dir(root.path().join("tree")).expect("replacement parent");
        fs::write(root.path().join("tree/value"), "same bytes").expect("replacement content");
        assert_ne!(leaf_swapped, fingerprint(root.path(), "tree/value").await);
    }
}
