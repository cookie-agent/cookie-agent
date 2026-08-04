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
struct ReadArgs {
    #[serde(rename = "filePath")]
    file_path: String,
    limit: Option<usize>,
    offset: Option<usize>,
    #[serde(rename = "byteOffset")]
    byte_offset: Option<usize>,
}

struct ReadExecutor {
    target: fs_cap::PreparedExisting,
    offset: usize,
    byte_offset: usize,
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
            description: "Read a descriptor-bound file or directory snapshot.".into(),
            parameters: schema::<ReadArgs>(),
        }])
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
        let offset = args.offset.unwrap_or(1);
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
        let byte_offset = args.byte_offset.unwrap_or(0);
        if offset == 0 || limit == 0 {
            return Err(ToolError::execution("offset and limit must be positive"));
        }
        args.offset = Some(offset);
        args.limit = Some(limit);
        args.byte_offset = Some(byte_offset);
        let target = fs_cap::prepare_existing(&ctx.cwd, std::path::Path::new(&args.file_path))?;
        let binding = target.manifest_bytes()?;
        let (resources, policy_labels, external) = prepared_path_resources(
            PermissionAction::Read,
            if target.directory {
                "directory"
            } else {
                "file"
            },
            &target.display_path,
            &self.workspace,
            &binding,
            target.directory,
        )?;
        let context = fs_cap::cwd_context_bytes(&ctx.cwd)?;
        let operation = prepared_operation(
            "read",
            &args,
            if external {
                vec![
                    (PermissionAction::Read, "read"),
                    (PermissionAction::ExternalDirectory, "guard"),
                ]
            } else {
                vec![(PermissionAction::Read, "read")]
            },
            resources,
            &context,
        )?;
        PreparedTool::new(
            operation,
            None,
            Box::new(ReadExecutor {
                target,
                offset,
                byte_offset,
                limit,
            }),
        )
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
            if self.byte_offset != 0 {
                return Err(ToolError::execution(
                    "byteOffset is invalid for directories",
                ));
            }
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
            let start = self.offset.saturating_sub(1);
            let page = entries
                .iter()
                .skip(start)
                .take(self.limit)
                .collect::<Vec<_>>();
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
        let start = self.offset.saturating_sub(1);
        let mut output = format!(
            "<path>{}</path>\n<type>file</type>\n<content>\n",
            self.target.display_path.display()
        );
        for (index, line) in lines.iter().enumerate().skip(start).take(self.limit) {
            if index == start && self.byte_offset > line.len() {
                return Err(ToolError::execution("byteOffset exceeds the starting line"));
            }
            let value = if index == start {
                line.get(self.byte_offset..).ok_or_else(|| {
                    ToolError::execution("byteOffset is not a UTF-8 character boundary")
                })?
            } else {
                line
            };
            output.push_str(&format!("{}: {value}\n", index + 1));
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

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use cookie_agent_engine::{ToolCall, ToolPreparationContext, ToolProvider};
    use cookie_agent_protocol::{
        ApprovalResourceSource, OperationFingerprint, PermissionAction, RunId, SessionId,
        ToolCallId,
    };

    use super::ReadTool;

    fn context(root: &Path) -> ToolPreparationContext {
        ToolPreparationContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            cwd: root.to_owned(),
            workspace_root: root.to_owned(),
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

    async fn fingerprint(root: &Path, path: &str) -> OperationFingerprint {
        OperationFingerprint::from_prepared_operation(prepared(root, path).await.operation())
    }

    #[tokio::test]
    async fn external_read_manifest_contains_explicit_guard() {
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
                },
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "read".into(),
                    arguments: serde_json::json!({"filePath":external.path()}),
                },
            )
            .await
            .expect("prepare external read");
        assert!(
            prepared
                .operation()
                .capabilities()
                .iter()
                .any(|capability| capability.action == PermissionAction::ExternalDirectory)
        );
        assert!(prepared.operation().resources().iter().any(|resource| {
            resource.capability == PermissionAction::ExternalDirectory
                && resource.source == ApprovalResourceSource::ExternalDirectoryGuard
        }));
        let boundary = format!(
            "{}/*",
            external.path().parent().expect("external parent").display()
        );
        assert_eq!(prepared.policy_labels()[0], boundary);
        assert_eq!(
            prepared.policy_labels()[1],
            external.path().display().to_string()
        );
    }

    #[tokio::test]
    async fn workspace_read_label_is_canonical_workspace_relative() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::create_dir(workspace.path().join("nested")).expect("nested");
        fs::write(workspace.path().join("nested/value.txt"), "value").expect("fixture");
        let prepared = prepared(workspace.path(), "nested/value.txt").await;
        assert_eq!(prepared.policy_labels(), ["nested/value.txt"]);
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
