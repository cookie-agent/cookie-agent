//! What a plugin tool reports: how a prepared call is shown, and what a call
//! returns.

use base64::Engine as _;
use cookie_agent_protocol::{
    BoundedDisplayText, DisplayBlock, MimeType, Open, PreparedPresentation, SafeDisplayText,
    Sha256Digest, ToolAttachment, ToolCompletionOutput, ToolHint,
    runtime_protocol::{self, InlineMedia, ToolResult},
};
use serde_json::Value;

/// What a plugin tool returns: the text the model reads, and what clients
/// show of it. Build one with [`Self::success`] or [`Self::error`] and the
/// builder methods:
///
/// ```
/// use cookie_agent_plugin_sdk::{DisplayBlock, ToolOutput};
///
/// let output = ToolOutput::success("3 open issues")
///     .title("Issues of cookie-agent")
///     .display(DisplayBlock::text("#12 Crash on start\n#14 Typo\n#15 Slow"))
///     .metadata(serde_json::json!({"open": 3}));
/// assert_eq!(output.output_text(), "3 open issues");
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ToolOutput {
    text: String,
    title: Option<String>,
    display: Vec<DisplayBlock>,
    metadata: Option<Value>,
    attachments: Vec<Attachment>,
    failed: bool,
}

/// A file a tool result carries.
#[derive(Clone, Debug, PartialEq)]
struct Attachment {
    mime_type: String,
    filename: Option<String>,
    bytes: Vec<u8>,
}

impl ToolOutput {
    /// A successful result whose output is `text`. Without display blocks,
    /// clients show the text.
    #[must_use]
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            title: None,
            display: Vec::new(),
            metadata: None,
            attachments: Vec::new(),
            failed: false,
        }
    }

    /// A result that reports a tool-level failure the model reads as one.
    #[must_use]
    pub fn error(text: impl Into<String>) -> Self {
        Self::success(text).failed(true)
    }

    /// Replaces the text the model reads.
    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// The result's title, which clients show; the tool's name otherwise.
    /// Control characters become spaces, and it is cut to 1 KiB.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Adds a display block: what clients show instead of the text. Send only
    /// the kinds the daemon's version includes (see
    /// [`crate::PluginContext::daemon`]); the daemon drops others.
    #[must_use]
    pub fn display(mut self, block: DisplayBlock) -> Self {
        self.display.push(block);
        self
    }

    /// Adds display blocks, as [`Self::display`] does one.
    #[must_use]
    pub fn displays(mut self, blocks: impl IntoIterator<Item = DisplayBlock>) -> Self {
        self.display.extend(blocks);
        self
    }

    /// Structured data the model reads with the text.
    #[must_use]
    pub fn metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Attaches a file of media type `mime_type`, such as `image/png`, for
    /// the model. The SDK sends its bytes with the result.
    #[must_use]
    pub fn attachment(
        mut self,
        mime_type: impl Into<String>,
        filename: Option<String>,
        bytes: impl Into<Vec<u8>>,
    ) -> Self {
        self.attachments.push(Attachment {
            mime_type: mime_type.into(),
            filename,
            bytes: bytes.into(),
        });
        self
    }

    /// Whether the model reads the result as a failure.
    #[must_use]
    pub const fn failed(mut self, failed: bool) -> Self {
        self.failed = failed;
        self
    }

    /// The text the model reads.
    #[must_use]
    pub fn output_text(&self) -> &str {
        &self.text
    }

    /// Whether the result reports a failure.
    #[must_use]
    pub const fn is_failed(&self) -> bool {
        self.failed
    }

    /// The result of a call of tool `tool`, and the bytes of its
    /// attachments.
    pub(crate) fn into_result(self, tool: &str) -> Result<(ToolResult, Vec<InlineMedia>), String> {
        let mut attachments = Vec::with_capacity(self.attachments.len());
        let mut media = Vec::with_capacity(self.attachments.len());
        for attachment in self.attachments {
            let mime_type = MimeType::new(attachment.mime_type.clone()).map_err(|error| {
                format!(
                    "plugin tool `{tool}` attached a file of media type `{}`: {error}",
                    attachment.mime_type
                )
            })?;
            let sha256 = Sha256Digest::of_bytes(&attachment.bytes);
            attachments.push(ToolAttachment {
                mime_type,
                filename: attachment.filename,
                byte_length: attachment.bytes.len() as u64,
                reference: runtime_protocol::artifact_reference(sha256.as_str()),
                sha256: sha256.clone(),
            });
            media.push(InlineMedia {
                sha256: sha256.as_str().to_owned(),
                data: base64::engine::general_purpose::STANDARD.encode(&attachment.bytes),
            });
        }
        Ok((
            ToolResult {
                output: ToolCompletionOutput::Single { text: self.text },
                title: Some(safe_title(self.title.as_deref().unwrap_or(tool))),
                display: self.display.into_iter().map(Open::Known).collect(),
                metadata: self.metadata,
                attachments,
                additional_messages: Vec::new(),
                failed: self.failed,
            },
            media,
        ))
    }
}

/// A call a plugin prepares, as [`crate::PluginServerBuilder::prepare_presentation`]
/// sees it.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct PrepareCall<'a> {
    /// The tool's name, as the plugin declared it.
    pub name: &'a str,
    /// The call's arguments.
    pub arguments: &'a Value,
    /// The permission resource the tool derives from them, if it does.
    pub resource: Option<&'a str>,
}

/// How clients show a prepared call: a title, an optional hint and primary
/// argument, and display blocks of its arguments and of what it will do.
///
/// ```
/// use cookie_agent_plugin_sdk::{DisplayBlock, Presentation};
///
/// let presentation = Presentation::new("Close issue #12")
///     .hint("generic")
///     .primary_argument("#12")
///     .preview(DisplayBlock::text("The issue is closed as completed."));
/// # let _ = presentation;
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Presentation {
    title: String,
    hint: Option<String>,
    primary_argument: Option<String>,
    arguments: Vec<DisplayBlock>,
    preview: Vec<DisplayBlock>,
}

impl Presentation {
    /// A presentation titled `title`; control characters become spaces, and
    /// it is cut to 1 KiB.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            hint: None,
            primary_argument: None,
            arguments: Vec::new(),
            preview: Vec::new(),
        }
    }

    /// What the tool is for, for clients' icons and grouping: one of the
    /// documented hints such as `shell`, `file_read`, `fetch`, or
    /// `generic`, or a label of the tool's own (snake case, at most 32
    /// bytes), which clients show as `generic`. An invalid label is left
    /// out.
    #[must_use]
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// The argument clients show next to the title, cut to its display
    /// bound.
    #[must_use]
    pub fn primary_argument(mut self, argument: impl Into<String>) -> Self {
        self.primary_argument = Some(argument.into());
        self
    }

    /// Adds a block showing the call's arguments, such as a `code` block.
    #[must_use]
    pub fn argument(mut self, block: DisplayBlock) -> Self {
        self.arguments.push(block);
        self
    }

    /// Adds a block showing what the call will do, such as a diff.
    #[must_use]
    pub fn preview(mut self, block: DisplayBlock) -> Self {
        self.preview.push(block);
        self
    }

    /// The default presentation of `call`: the tool's name, with its
    /// permission resource as the primary argument.
    #[must_use]
    pub fn of_call(call: &PrepareCall<'_>) -> Self {
        let presentation = Self::new(call.name);
        match call.resource {
            Some(resource) => presentation.primary_argument(resource),
            None => presentation,
        }
    }

    pub(crate) fn into_prepared(self) -> PreparedPresentation {
        let mut prepared = PreparedPresentation::new(
            safe_title(&self.title),
            self.primary_argument
                .map(|argument| truncate(argument.trim(), BoundedDisplayText::MAX_BYTES))
                .filter(|argument| !argument.is_empty())
                .and_then(|argument| BoundedDisplayText::new(argument).ok()),
        );
        prepared.hint = self.hint.and_then(|hint| ToolHint::new(hint).ok());
        prepared.arguments = self.arguments.into_iter().map(Open::Known).collect();
        prepared.preview = self.preview.into_iter().map(Open::Known).collect();
        prepared
    }
}

/// `title` with control characters replaced, cut to the display bound;
/// `tool` when nothing is left.
pub(crate) fn safe_title(title: &str) -> SafeDisplayText {
    let title = truncate(
        &title.replace(char::is_control, " "),
        SafeDisplayText::MAX_BYTES,
    );
    SafeDisplayText::new(title)
        .unwrap_or_else(|_| SafeDisplayText::new("tool").expect("static title is valid"))
}

pub(crate) fn truncate(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
