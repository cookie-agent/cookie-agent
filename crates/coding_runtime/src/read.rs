use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use cookie_agent_protocol::{
    PermissionAction, PersistedToolResult as ToolResult, ToolEmittedContent, ToolEmittedMessage,
    ToolEmittedMessageRole,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec, media::AttachmentGate,
    media::approved_media_type, media::attachment_gate_error, media::gate_attachment,
    media::sniff_media_type,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use cookie_agent_runtime_api::WorkspaceContext;

use crate::{fs_cap, parse_args, prepared_operation, prepared_path_resources, schema};

const DEFAULT_LIMIT: usize = 2_000;
/// Longest line returned in full; the rest of a longer line is replaced by a
/// marker, so one huge minified line cannot fill or fail the result.
const MAX_LINE_BYTES: usize = 4 * 1024;
/// Output budget for one page of text, under the 2 MiB result limit.
const MAX_PAGE_BYTES: usize = ToolResult::MAX_OUTPUT_BYTES - 64 * 1024;
const READ_BUFFER_BYTES: usize = 64 * 1024;
const _: () = assert!(READ_BUFFER_BYTES >= cookie_agent_runtime_api::media::MEDIA_SNIFF_BYTES);

#[derive(Debug)]
pub struct ReadTool {
    workspace: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    #[serde(
        rename = "filePath",
        deserialize_with = "crate::path_args::deserialize"
    )]
    /// Path to the file or directory. Relative paths resolve against the session working directory.
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
impl ToolProvider for ReadTool {
    fn provider_id(&self) -> &'static str {
        "builtin.read"
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: cookie_agent_runtime_api::ToolConcurrency::Parallel,
            result_truncation: cookie_agent_runtime_api::ToolResultTruncationPolicy::OptOut,
            name: "read".into(),
            permission_name: Self::get_permission_name("read")?.map(Into::into),
            description:
                "Read a text/image/video file or a directory snapshot using a zero-based offset."
                    .into(),
            parameters: schema::<ReadArgs>(),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        match tool_name {
            "read" => Ok(Some("read")),
            _ => Err(ToolError::execution("read provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
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
        _ctx: ToolPreparationContext,
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
        let workspace = self.workspace.clone();
        crate::blocking(move || {
            let target =
                fs_cap::prepare_existing(&workspace, std::path::Path::new(&args.file_path))?;
            let (resources, policy_labels) = prepared_path_resources(
                PermissionAction::READ,
                if target.directory {
                    "directory"
                } else {
                    "file"
                },
                &target.display_path,
                &workspace,
                &target.binding_bytes(),
            )?;
            let context = crate::workspace_context_bytes(&workspace);
            let operation = prepared_operation(
                "read",
                &args,
                vec![(PermissionAction::READ, "read")],
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
        })
        .await
    }
}

#[async_trait]
impl PreparedExecutor for ReadExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_runtime_api::ToolCompletion, ToolError> {
        let result = crate::blocking(move || {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution(
                "prepared read cancelled before execution",
            ));
        }
        if self.target.directory {
            let entries = self.target.directory_entries()?;
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
display: None,
retained_output: None,
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
        let file = self.target.open_file()?;
        let len = file.metadata().map_err(io_error)?.len();
        let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, file);
        let head = reader.fill_buf().map_err(io_error)?;
        let path = &self.target.display_path;
        if let Some(mime) = sniff_media_type(path, head, len)? {
            // Gate on the file size before reading the whole file.
            let gate = gate_attachment(
                context.turn_context.adapter_family,
                &context.turn_context.capabilities,
                mime,
                len,
            );
            if let Some(error) = attachment_gate_error(
                gate,
                mime,
                &context.turn_context.model,
                context.turn_context.adapter,
            ) {
                return Err(ToolError::execution(error));
            }
            let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
            reader.read_to_end(&mut bytes).map_err(io_error)?;
            let mime = approved_media_type(path, &bytes)?
                .ok_or_else(|| ToolError::execution("file changed while it was read"))?;
            return attachment_result(&context, path, mime, gate, &bytes);
        }
        if std::str::from_utf8(head).is_err_and(|error| error.error_len().is_some()) {
            return Err(not_text());
        }
        text_result(path, &mut reader, self.offset, self.limit)
        })
        .await;
        result.map(cookie_agent_runtime_api::ToolCompletion::single)
    }
}

fn io_error(error: std::io::Error) -> ToolError {
    ToolError::execution(error.to_string())
}

fn not_text() -> ToolError {
    ToolError::execution("read supports UTF-8 text or approved media")
}

fn attachment_result(
    context: &ToolExecutionContext,
    path: &Path,
    mime: &str,
    gate: AttachmentGate,
    bytes: &[u8],
) -> Result<ToolResult, ToolError> {
    let attachment = context.retain_validated_attachment(
        mime,
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        bytes,
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
    Ok(ToolResult {
        display: None,
        retained_output: None,
        title: crate::safe_title(format!("Read attachment {}", path.display())),
        output,
        metadata: serde_json::json!({"kind":"attachment","mime_type":mime,"sha256":sha256}),
        truncation: None,
        attachments,
        additional_messages,
    })
}

/// One page of numbered lines, read in a single streaming pass that also
/// counts the file's lines.
fn text_result(
    path: &Path,
    reader: &mut impl BufRead,
    offset: usize,
    limit: usize,
) -> Result<ToolResult, ToolError> {
    let mut page = TextPage {
        output: format!(
            "<path>{}</path>\n<type>file</type>\n<content>\n",
            path.display()
        ),
        line: Vec::new(),
        cut: 0,
        stopped_at: None,
    };
    let end = offset.saturating_add(limit);
    // Lines ended so far, and whether bytes follow the last newline.
    let mut lines = 0_usize;
    let mut partial = false;
    loop {
        let chunk = reader.fill_buf().map_err(io_error)?;
        if chunk.is_empty() {
            break;
        }
        let consumed = chunk.len();
        let mut rest = chunk;
        while !rest.is_empty() {
            let wanted = lines >= offset && lines < end && page.stopped_at.is_none();
            if !wanted && lines >= offset {
                // Past the page: only count what remains.
                lines += memchr::memchr_iter(b'\n', rest).count();
                partial = rest.last() != Some(&b'\n');
                break;
            }
            let newline = memchr::memchr(b'\n', rest);
            if wanted {
                page.keep(&rest[..newline.unwrap_or(rest.len())]);
            }
            match newline {
                Some(at) => {
                    if wanted {
                        page.finish_line(lines, true)?;
                    }
                    lines += 1;
                    partial = false;
                    rest = &rest[at + 1..];
                }
                None => {
                    partial = true;
                    rest = &[];
                }
            }
        }
        reader.consume(consumed);
    }
    if partial {
        if lines >= offset && lines < end && page.stopped_at.is_none() {
            page.finish_line(lines, false)?;
        }
        lines += 1;
    }
    let mut output = page.output;
    output.push_str("</content>");
    if let Some(next) = page.stopped_at {
        output.push_str(&format!(
            "\n[page cut at the result size limit; continue with offset={next}]"
        ));
    }
    Ok(ToolResult {
        display: None,
        retained_output: None,
        title: crate::safe_title(format!("Read file {}", path.display())),
        output,
        metadata: serde_json::json!({
            "kind": "text",
            "offset": offset,
            "limit": limit,
            "total_lines": lines,
            "next_offset": page.stopped_at,
        }),
        truncation: None,
        attachments: Vec::new(),
        additional_messages: Vec::new(),
    })
}

struct TextPage {
    output: String,
    /// The current line's bytes, up to [`MAX_LINE_BYTES`].
    line: Vec<u8>,
    /// Bytes of the current line beyond [`MAX_LINE_BYTES`].
    cut: usize,
    /// The first line left out because the page reached [`MAX_PAGE_BYTES`].
    stopped_at: Option<usize>,
}

impl TextPage {
    fn keep(&mut self, segment: &[u8]) {
        let take = MAX_LINE_BYTES
            .saturating_sub(self.line.len())
            .min(segment.len());
        self.line.extend_from_slice(&segment[..take]);
        self.cut += segment.len() - take;
    }

    fn finish_line(&mut self, index: usize, terminated: bool) -> Result<(), ToolError> {
        if terminated && self.cut == 0 && self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        let text = match std::str::from_utf8(&self.line) {
            Ok(text) => text,
            // A cut can split the last character; drop its leading bytes.
            Err(error) if self.cut > 0 && error.error_len().is_none() => {
                self.cut += self.line.len() - error.valid_up_to();
                std::str::from_utf8(&self.line[..error.valid_up_to()]).expect("valid prefix")
            }
            Err(_) => return Err(not_text()),
        };
        let number = index + 1;
        let rendered = if self.cut == 0 {
            format!("{number}: {text}\n")
        } else {
            format!(
                "{number}: {text}... [line truncated: {} more bytes]\n",
                self.cut
            )
        };
        if self.output.len() + rendered.len() > MAX_PAGE_BYTES {
            self.stopped_at = Some(index);
        } else {
            self.output.push_str(&rendered);
        }
        self.line.clear();
        self.cut = 0;
        Ok(())
    }
}

fn directory_page<T>(entries: &[T], offset: usize, limit: usize) -> impl Iterator<Item = &T> {
    entries.iter().skip(offset).take(limit)
}

#[cfg(test)]
mod tests;
