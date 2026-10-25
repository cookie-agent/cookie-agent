use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use cookie_agent_protocol::{
    DisplayBlock, Open, PersistedToolResult as ToolResult, PreparedPresentation,
    ToolEmittedContent, ToolEmittedMessage, ToolEmittedMessageRole, ToolHint,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec,
    media::AttachmentGate,
    media::approved_media_type,
    media::attachment_gate_error,
    media::gate_attachment,
    media::sniff_media_type,
    text_tools::{self, LineWindow, MAX_PAGE_BYTES, PageError},
    tool_presentation,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use cookie_agent_runtime_api::WorkspaceContext;

use crate::{display, fs_cap, parse_args, schema};

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
    /// The most lines, or directory entries, to return. Defaults to 2000.
    limit: Option<usize>,
    /// The 1-based line, or directory entry, to start at. Defaults to 1.
    offset: Option<usize>,
}

/// What the model reads about `read`.
const DESCRIPTION: &str = "Read a file or a directory. A text file comes back as numbered lines, `N: text`, counting from 1. `offset` is the 1-based line to start at (default 1) and `limit` the most lines to return (default 2000). A line longer than 4 KiB is cut and ends with `... [line truncated: N more bytes]`, and a page stops before it passes the result size limit (about 2 MiB). A page that leaves lines out ends with a note such as `[120 more lines; continue with offset=2001]`: read again from that offset. An offset past the last line is an error. A directory lists its entries, one per line with `/` after a folder's name, paged by the same 1-based `offset` and `limit`. An image, video, or other media file the model accepts is attached instead.";

struct ReadExecutor {
    target: fs_cap::PreparedExisting,
    window: LineWindow,
    /// The path clients see.
    shown_path: PathBuf,
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

    /// The path clients see for `args`.
    fn shown_path(&self, args: &ReadArgs) -> PathBuf {
        let path = crate::permission_path_label(&args.file_path, &self.workspace);
        PathBuf::from(crate::abbreviated_display_path(&path, &self.workspace))
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
            description: DESCRIPTION.into(),
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

    fn presentation(&self, call: &ToolCall) -> PreparedPresentation {
        let Ok(primary) = self.get_display_argument(&call.name, &call.arguments) else {
            return cookie_agent_runtime_api::tool_title_only(&call.name);
        };
        let Ok(args) = parse_args::<ReadArgs>("read", call.arguments.clone()) else {
            return cookie_agent_runtime_api::tool_title_only(&call.name);
        };
        let shown = self.shown_path(&args).display().to_string();
        // The title and primary argument already name the file.
        let mut entries = Vec::new();
        entries.extend(args.offset.map(|offset| ("offset", offset.to_string())));
        entries.extend(args.limit.map(|limit| ("limit", limit.to_string())));
        let presentation =
            tool_presentation(&format!("Read {shown}"), &primary).with_hint(ToolHint::FILE_READ);
        if entries.is_empty() {
            presentation
        } else {
            presentation.with_arguments([DisplayBlock::key_value(entries)])
        }
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        if call.name != "read" {
            return Err(ToolError::execution("read provider received another tool"));
        }
        let args: ReadArgs = parse_args("read", call.arguments)?;
        let window = LineWindow::new(args.offset, args.limit)?;
        let workspace = self.workspace.clone();
        let shown_path = self.shown_path(&args);
        crate::blocking(move || {
            let target =
                fs_cap::prepare_existing(&workspace, std::path::Path::new(&args.file_path))?;
            let normalized_arguments = serde_json::json!({
                "filePath": target.display_path,
                "offset": window.offset,
                "limit": window.limit,
            });
            PreparedTool::declared(
                normalized_arguments,
                None,
                Box::new(ReadExecutor {
                    target,
                    window,
                    shown_path,
                }),
            )
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
                let target = self.target.display_path.display().to_string();
                self.window
                    .check_in_range(entries.len(), "entry", &target)?;
                let page = directory_page(&entries, self.window).collect::<Vec<_>>();
                let mut output =
                    format!("<path>{target}</path>\n<type>directory</type>\n<entries>\n");
                for (name, directory) in &page {
                    output.push_str(name);
                    if *directory {
                        output.push('/');
                    }
                    output.push('\n');
                }
                output.push_str("</entries>");
                let marker = self.window.remaining_marker(entries.len(), "entry");
                if let Some(marker) = &marker {
                    output.push('\n');
                    output.push_str(marker);
                }
                let table = DisplayBlock::Table {
                    columns: vec!["name".into(), "type".into()],
                    rows: page
                        .iter()
                        .map(|(name, directory)| {
                            vec![
                                name.clone(),
                                if *directory { "directory" } else { "file" }.into(),
                            ]
                        })
                        .collect(),
                    omitted_rows: None,
                };
                return Ok(ToolResult {
                    display: vec![Open::Known(display::fit_block(table, None)?)],
                    background: None,
                    retained_output: None,
                    title: crate::safe_title(format!("Read {target}")),
                    output,
                    metadata: serde_json::json!({
                        "kind": "directory",
                        "offset": self.window.offset,
                        "limit": self.window.limit,
                        "shown": page.len(),
                        "total_entries": entries.len(),
                        "next_offset": marker.is_some().then(|| self.window.end_index() + 1),
                    }),
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
                let gate = gate_attachment(&context.turn_context.input_media, mime, len);
                if let Some(error) = attachment_gate_error(
                    gate,
                    mime,
                    &context.turn_context.model,
                    &context.turn_context.adapter,
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
            text_result(path, &self.shown_path, &mut reader, self.window)
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
    let media = DisplayBlock::Media {
        artifact: attachment.artifact_ref(),
        alt: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
    };
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
        display: vec![Open::Known(media)],
        background: None,
        retained_output: None,
        title: crate::safe_title(format!("Read {}", path.display())),
        output,
        metadata: serde_json::json!({"kind":"attachment","mime_type":mime,"sha256":sha256}),
        attachments,
        additional_messages,
    })
}

/// One page of numbered lines, read in a single streaming pass that also
/// counts the file's lines. Clients see the page as an excerpt of
/// `shown_path`.
fn text_result(
    path: &Path,
    shown_path: &Path,
    reader: &mut impl BufRead,
    window: LineWindow,
) -> Result<ToolResult, ToolError> {
    let target = path.display().to_string();
    let mut output = format!("<path>{target}</path>\n<type>file</type>\n<content>\n");
    let page = text_tools::page_reader(reader, window, MAX_PAGE_BYTES - output.len()).map_err(
        |error| match error {
            PageError::Io(error) => io_error(error),
            PageError::NotText => not_text(),
        },
    )?;
    page.check_in_range(&target)?;
    let excerpt = page.excerpt_block(
        shown_path.display().to_string(),
        display::language_for_path(shown_path),
    );
    output.push_str(&page.text);
    output.push_str("</content>");
    if let Some(marker) = page.continuation() {
        output.push('\n');
        output.push_str(&marker);
    }
    Ok(ToolResult {
        display: vec![Open::Known(display::fit_block(excerpt, None)?)],
        background: None,
        retained_output: None,
        title: crate::safe_title(format!("Read {target}")),
        output,
        metadata: serde_json::json!({
            "kind": "text",
            "offset": window.offset,
            "limit": window.limit,
            "total_lines": page.total_lines,
            "next_offset": page.next_offset,
        }),
        attachments: Vec::new(),
        additional_messages: Vec::new(),
    })
}

fn directory_page<T>(entries: &[T], window: LineWindow) -> impl Iterator<Item = &T> {
    entries.iter().skip(window.first_index()).take(window.limit)
}

#[cfg(test)]
mod tests;
