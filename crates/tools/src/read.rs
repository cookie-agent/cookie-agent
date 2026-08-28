use std::path::PathBuf;

use async_trait::async_trait;
use cookie_agent_engine::{
    AttachmentGate, PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError,
    ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec, approved_media_type,
    attachment_gate_error, gate_attachment,
};
use cookie_agent_protocol::{
    PermissionAction, PersistedToolResult as ToolResult, ToolEmittedContent, ToolEmittedMessage,
    ToolEmittedMessageRole,
};
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
            result_truncation: cookie_agent_engine::ToolResultTruncationPolicy::OptOut,
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
                additional_messages: Vec::new(),
            });
        }
        let bytes = self.target.verified_bytes()?;
        if let Some(mime) = approved_media_type(&self.target.display_path, &bytes)? {
            let gate = gate_attachment(
                context.turn_context.adapter,
                &context.turn_context.capabilities,
                mime,
                &bytes,
            );
            if let Some(error) = attachment_gate_error(
                gate,
                mime,
                &context.turn_context.model,
                context.turn_context.adapter,
            ) {
                return Err(ToolError::execution(error));
            }
            let attachment = context.retain_validated_attachment(
                mime,
                self.target
                    .display_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned()),
                &bytes,
            )?;
            let sha256 = attachment.sha256.clone();
            let (output, attachments, additional_messages) = match gate {
                AttachmentGate::AttachToolResult => (
                    format!("Attached {mime} ({} bytes).", bytes.len()),
                    vec![attachment],
                    Vec::new(),
                ),
                AttachmentGate::DeliverViaUserTurn => (
                    format!(
                        "Attached {mime} ({} bytes), delivered in the following message.",
                        bytes.len()
                    ),
                    Vec::new(),
                    vec![
                        ToolEmittedMessage::new(
                            ToolEmittedMessageRole::User,
                            vec![ToolEmittedContent::File(attachment)],
                        )
                        .map_err(|error| ToolError::execution(error.to_string()))?,
                    ],
                ),
                AttachmentGate::RejectUnsupportedModel
                | AttachmentGate::RejectUnsupportedFamily
                | AttachmentGate::RejectTooLarge { .. } => {
                    unreachable!("rejected attachment gates returned an error")
                }
            };
            return Ok(ToolResult {
                title: crate::safe_title(format!(
                    "Read attachment {}",
                    self.target.display_path.display()
                )),
                output,
                metadata: serde_json::json!({"kind":"attachment","mime_type":mime,"sha256":sha256}),
                truncation: None,
                attachments,
                additional_messages,
            });
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| ToolError::execution("read supports UTF-8 text or approved media"))?;
        Ok(text_result(
            &self.target.display_path,
            text,
            self.offset,
            self.limit,
        ))
    }
}

fn text_result(path: &std::path::Path, text: &str, offset: usize, limit: usize) -> ToolResult {
    let lines = text.lines().collect::<Vec<_>>();
    let mut output = format!(
        "<path>{}</path>\n<type>file</type>\n<content>\n",
        path.display()
    );
    for (index, line) in text_page(&lines, offset, limit) {
        output.push_str(&format!("{}: {line}\n", index + 1));
    }
    output.push_str("</content>");
    ToolResult {
        title: crate::safe_title(format!("Read file {}", path.display())),
        output,
        metadata: serde_json::json!({"kind":"text","offset":offset,"limit":limit,"total_lines":lines.len()}),
        truncation: None,
        attachments: Vec::new(),
        additional_messages: Vec::new(),
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
    use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

    use cookie_agent_engine::{
        SessionToolContext, ToolCall, ToolError, ToolExecutionContext, ToolPreparationContext,
        ToolProvider, ToolResultTruncationPolicy, TurnAgentContext,
    };
    use cookie_agent_protocol::{
        AdaptorId, MediaCapability, MediaKind, MimeType, Modality, OperationFingerprint,
        PermissionAction, RunId, SessionId, ToolCallId, ToolEmittedContent,
    };

    use super::{ReadTool, directory_page, text_page, text_result};

    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn pdf() -> Vec<u8> {
        let mut bytes = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for object in [
            "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
            "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
            "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] >>\nendobj\n",
        ] {
            offsets.push(bytes.len());
            bytes.extend_from_slice(object.as_bytes());
        }
        let xref = bytes.len();
        bytes.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
        for offset in offsets {
            bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        bytes.extend_from_slice(
            format!("trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        bytes
    }

    fn turn_context(
        adapter: AdaptorId,
        media: Option<(MediaKind, Modality, &str, u64)>,
    ) -> Arc<TurnAgentContext> {
        let base = crate::test_turn_context();
        let mut capabilities = base.capabilities.clone();
        if let Some((kind, modality, mime_type, max_bytes)) = media {
            capabilities.input.insert(modality);
            capabilities.media = BTreeMap::from([(
                kind,
                MediaCapability {
                    mime_types: [MimeType::new(mime_type).unwrap()].into_iter().collect(),
                    max_bytes,
                    max_count: 1,
                },
            )]);
        }
        Arc::new(TurnAgentContext {
            agent: base.agent.clone(),
            model: base.model.clone(),
            adapter,
            capabilities,
        })
    }

    #[test]
    fn large_single_line_read_is_full_and_declares_absolute_opt_out() {
        let root = tempfile::tempdir().unwrap();
        let text = "x".repeat(60 * 1024);
        fs::write(root.path().join("large.txt"), &text).unwrap();
        let target = crate::fs_cap::prepare_existing(root.path(), Path::new("large.txt")).unwrap();
        let bytes = target.verified_bytes().unwrap();
        let result = text_result(
            &target.display_path,
            std::str::from_utf8(&bytes).unwrap(),
            0,
            super::DEFAULT_LIMIT,
        );
        assert!(result.output.contains(&text));
        assert!(result.output.len() > 50 * 1024);
        assert!(result.truncation.is_none());
        let spec = ReadTool::new("/tmp")
            .tools_for_session(&SessionToolContext {
                session: SessionId::new_v7(),
            })
            .unwrap()
            .remove(0);
        assert_eq!(spec.result_truncation, ToolResultTruncationPolicy::OptOut);
    }

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
        let workspace = tempfile::tempdir().expect("workspace");
        let tool = ReadTool::new(workspace.path());
        assert_eq!(
            tool.get_display_argument("read", &serde_json::json!({"filePath":"src/lib.rs"}))
                .expect("relative"),
            "src/lib.rs"
        );
        assert_eq!(
            tool.get_display_argument(
                "read",
                &serde_json::json!({"filePath":workspace.path().join("src/lib.rs"),"offset":0,"limit":100})
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
            arguments: serde_json::json!({"filePath":workspace.path().join("src/lib.rs")}),
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

    fn context_with_turn(
        root: &Path,
        turn_context: Arc<TurnAgentContext>,
    ) -> ToolPreparationContext {
        ToolPreparationContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            cwd: root.to_owned(),
            workspace_root: root.to_owned(),
            turn_context,
        }
    }

    fn context(root: &Path) -> ToolPreparationContext {
        context_with_turn(root, crate::test_turn_context())
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

    async fn execute_media(
        root: &Path,
        path: &str,
        turn_context: Arc<TurnAgentContext>,
    ) -> Result<cookie_agent_protocol::PersistedToolResult, ToolError> {
        let prepared = ReadTool::new(root)
            .prepare(
                context_with_turn(root, Arc::clone(&turn_context)),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "read".into(),
                    arguments: serde_json::json!({"filePath":path}),
                },
            )
            .await?;
        prepared
            .execute_for_test(ToolExecutionContext::for_test(
                root.join("artifacts"),
                turn_context,
            )?)
            .await
    }

    #[tokio::test]
    async fn media_reads_follow_capability_family_and_size_gates() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("pixel.png"), PNG).unwrap();
        let pdf = pdf();
        fs::write(root.path().join("page.pdf"), &pdf).unwrap();
        let mut video = 16_u32.to_be_bytes().to_vec();
        video.extend_from_slice(b"ftypisom");
        video.extend_from_slice(&[0; 4]);
        fs::write(root.path().join("clip.mp4"), &video).unwrap();

        let image_capable = turn_context(
            AdaptorId::Anthropic,
            Some((
                MediaKind::Image,
                Modality::Image,
                "image/png",
                PNG.len() as u64,
            )),
        );
        let image = execute_media(root.path(), "pixel.png", image_capable)
            .await
            .unwrap();
        assert_eq!(image.attachments.len(), 1);
        assert_eq!(image.attachments[0].mime_type.as_str(), "image/png");

        let image_incapable = execute_media(
            root.path(),
            "pixel.png",
            turn_context(AdaptorId::Anthropic, None),
        )
        .await
        .unwrap_err();
        assert_eq!(
            image_incapable.message(),
            "Cannot attach image/png: the active model \"test/model\" does not accept image inputs"
        );

        let pdf_capable = turn_context(
            AdaptorId::Anthropic,
            Some((
                MediaKind::Pdf,
                Modality::Pdf,
                "application/pdf",
                pdf.len() as u64,
            )),
        );
        let document = execute_media(root.path(), "page.pdf", pdf_capable)
            .await
            .unwrap();
        assert_eq!(document.attachments.len(), 1);
        assert_eq!(
            document.attachments[0].mime_type.as_str(),
            "application/pdf"
        );

        let pdf_incapable = execute_media(
            root.path(),
            "page.pdf",
            turn_context(AdaptorId::Anthropic, None),
        )
        .await
        .unwrap_err();
        assert_eq!(
            pdf_incapable.message(),
            "Cannot attach application/pdf: the active model \"test/model\" does not accept PDF inputs"
        );

        let family_rejected = execute_media(
            root.path(),
            "pixel.png",
            turn_context(
                AdaptorId::OpenaiCompatible,
                Some((
                    MediaKind::Image,
                    Modality::Image,
                    "image/png",
                    PNG.len() as u64,
                )),
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(
            family_rejected.message(),
            "Cannot attach image/png: not deliverable in tool results via the openai-compatible family API"
        );

        let size_rejected = execute_media(
            root.path(),
            "pixel.png",
            turn_context(
                AdaptorId::Anthropic,
                Some((
                    MediaKind::Image,
                    Modality::Image,
                    "image/png",
                    PNG.len() as u64 - 1,
                )),
            ),
        )
        .await
        .unwrap_err();
        assert!(
            size_rejected
                .message()
                .contains("inline limit for this provider")
        );

        for family in [
            AdaptorId::OpenaiCompatible,
            AdaptorId::Anthropic,
            AdaptorId::GoogleGemini,
            AdaptorId::GoogleVertexGemini,
        ] {
            let result = execute_media(
                root.path(),
                "clip.mp4",
                turn_context(
                    family,
                    Some((
                        MediaKind::Video,
                        Modality::Video,
                        "video/mp4",
                        video.len() as u64,
                    )),
                ),
            )
            .await
            .unwrap();
            assert!(result.attachments.is_empty(), "{family:?}");
            assert_eq!(
                result.output,
                format!(
                    "Attached video/mp4 ({} bytes), delivered in the following message.",
                    video.len()
                )
            );
            assert!(matches!(
                result.additional_messages[0].content.as_slice(),
                [ToolEmittedContent::File(attachment)]
                    if attachment.mime_type.as_str() == "video/mp4"
            ));
        }

        let video_incapable = execute_media(
            root.path(),
            "clip.mp4",
            turn_context(AdaptorId::OpenaiCompatible, None),
        )
        .await
        .unwrap_err();
        assert_eq!(
            video_incapable.message(),
            "Cannot attach video/mp4: the active model \"test/model\" does not accept media inputs"
        );
    }

    #[tokio::test]
    async fn malformed_media_still_errors_and_text_path_is_unchanged() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("bad.png"), b"not a PNG").unwrap();
        let malformed = execute_media(
            root.path(),
            "bad.png",
            turn_context(
                AdaptorId::Anthropic,
                Some((MediaKind::Image, Modality::Image, "image/png", 1024)),
            ),
        )
        .await
        .unwrap_err();
        assert!(
            malformed
                .message()
                .contains("malformed image, PDF, or video")
        );
        assert!(!malformed.message().contains("Cannot attach"));

        fs::write(root.path().join("note.txt"), "plain text\n").unwrap();
        let text = execute_media(
            root.path(),
            "note.txt",
            turn_context(AdaptorId::OpenaiCompatible, None),
        )
        .await
        .unwrap();
        assert!(text.output.contains("1: plain text"));
        assert!(text.attachments.is_empty());
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
        let expected = root
            .path()
            .join(".env")
            .canonicalize()
            .expect("canonical fixture path");
        assert_eq!(Path::new(canonical), expected);
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
        let expected_label = external
            .path()
            .canonicalize()
            .expect("canonical external path")
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches("//?/")
            .to_owned();
        assert_eq!(prepared.operation().capabilities().len(), 1);
        assert_eq!(
            prepared.operation().resources()[0].capability,
            PermissionAction::Read
        );
        assert_eq!(prepared.policy_labels(), [Some(expected_label.clone())]);
        assert_eq!(
            ReadTool::new(workspace.path())
                .get_permission_resource("read", prepared.normalized_arguments())
                .expect("resource from prepared args"),
            ("read", Some(expected_label))
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
