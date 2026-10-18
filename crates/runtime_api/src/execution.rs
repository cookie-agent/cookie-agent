//! What a tool sees while it is prepared and executed, and the host services
//! it reaches through that context: progress capture, attachment retention,
//! and the point at which the host may detach the call.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use cookie_agent_protocol::{
    AdaptorId, AgentId, ArtifactReadTarget, ArtifactReference, ModelKey, RunId, SessionId,
    Sha256Digest, ToolAttachment, ToolCallId, runtime_protocol::ToolInputMedia,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{ToolError, ToolOrigin, WorkspaceContext, media::validate_attachment};

/// Immutable harness context captured for one tool preparation/execution batch:
/// what a tool knows about the turn, the same on either side of the runtime
/// protocol (its `turn`).
///
/// This is tool-facing but harness-private metadata. Tool providers must not forward it to
/// external systems, including MCP wrappers. Compaction rehydration uses the context of the owner
/// policy and model binding that triggered the checkpoint.
#[derive(Debug)]
pub struct TurnAgentContext {
    /// Agent that owns the tool call.
    pub agent: AgentId,
    /// Exact model selected for this turn.
    pub model: ModelKey,
    /// Frozen wire adapter family used to deliver tool results.
    pub adapter: AdaptorId,
    /// The media the model accepts from a tool, with effective limits and
    /// delivery; see [`crate::media::tool_input_media`].
    pub input_media: Vec<ToolInputMedia>,
}

#[derive(Clone, Debug)]
pub struct ToolPreparationContext {
    pub session: SessionId,
    pub run: RunId,
    /// Layer or runtime that owns the tool being prepared. Tools that work on
    /// paths take their workspace from their own runtime; this tells every
    /// tool where the call runs, for its operation context digest.
    pub origin: ToolOrigin,
    /// Static agent/model context shared with execution for this batch.
    pub turn_context: Arc<TurnAgentContext>,
}

impl ToolPreparationContext {
    /// Workspace of the runtime that owns the tool, if it has one.
    #[must_use]
    pub fn workspace(&self) -> Option<&WorkspaceContext> {
        self.origin.workspace()
    }

    /// Bytes for the prepared operation's execution-context digest; see
    /// [`ToolOrigin::context_bytes`].
    #[must_use]
    pub fn context_bytes(&self) -> Vec<u8> {
        self.origin.context_bytes()
    }
}

#[derive(Debug)]
pub struct ToolExecutionContext {
    pub session: SessionId,
    pub run: RunId,
    pub progress: ProgressSink,
    pub cancellation: CancellationToken,
    /// Static agent/model context shared with preparation for this batch.
    pub turn_context: Arc<TurnAgentContext>,
    /// The session tree's artifact store.
    pub artifacts: Arc<dyn ToolArtifacts>,
    /// Where a background-capable tool says it may be detached.
    pub detach: DetachPoint,
    /// The model asked to run this call of a background-capable tool in the
    /// background. The tool never sees the `background` argument; a tool
    /// whose admission differs in the background reads this.
    pub background: bool,
    /// Where a runtime's tool sends producer messages, such as the results
    /// of work that reports itself after detaching; `None` where the
    /// runtime does not produce.
    pub producers: Option<Arc<dyn crate::ProducerChannel>>,
}

impl ToolExecutionContext {
    /// Tells the host the call may now be detached; see [`DetachPoint`].
    pub fn detachable(&self, note: Option<String>) {
        self.detach.reached(note);
    }

    /// Reads a page of an artifact in this call's session tree.
    pub async fn read_artifact(
        &self,
        target: ArtifactReadTarget,
        offset: u64,
        limit: u64,
    ) -> Result<ArtifactReadPage, ToolError> {
        Arc::clone(&self.artifacts)
            .read_page(self.session, target, offset, limit)
            .await
    }

    pub fn retain_attachment(
        &self,
        mime_type: impl Into<String>,
        filename: Option<String>,
        bytes: &[u8],
    ) -> Result<ToolAttachment, ToolError> {
        let mime_type = mime_type.into();
        let path = filename.as_deref().map_or_else(PathBuf::new, PathBuf::from);
        validate_attachment(&mime_type, &path, bytes)?;
        self.retain_validated_attachment(mime_type, filename, bytes)
    }

    pub fn retain_validated_attachment(
        &self,
        mime_type: impl Into<String>,
        filename: Option<String>,
        bytes: &[u8],
    ) -> Result<ToolAttachment, ToolError> {
        let mime_type = mime_type.into();
        let (reference, sha256) = self.artifacts.retain(self.session, bytes)?;
        Ok(ToolAttachment {
            mime_type: cookie_agent_protocol::MimeType::new(mime_type)
                .map_err(|error| ToolError::execution(error.to_string()))?,
            filename,
            byte_length: bytes.len() as u64,
            sha256: Sha256Digest::new(sha256)
                .map_err(|error| ToolError::execution(error.to_string()))?,
            reference,
        })
    }
}

/// The session artifact store as tools see it. The host owns where and how
/// the bytes are kept; a session only reaches its own tree's artifacts.
#[async_trait]
pub trait ToolArtifacts: std::fmt::Debug + Send + Sync {
    /// Stores `content` in `session`'s tree, returning its reference and
    /// lowercase hex SHA-256.
    fn retain(
        &self,
        session: SessionId,
        content: &[u8],
    ) -> Result<(ArtifactReference, String), ToolError>;

    /// Reads up to `limit` lines from line `offset` of an artifact, or of one
    /// stream of a named-stream manifest, in `session`'s tree.
    async fn read_page(
        self: Arc<Self>,
        session: SessionId,
        target: ArtifactReadTarget,
        offset: u64,
        limit: u64,
    ) -> Result<ArtifactReadPage, ToolError>;
}

/// One page of an artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactReadPage {
    pub content: String,
    /// Line offset of the next page, when there is one.
    pub next_offset_lines: Option<u64>,
    /// `artifact`, or `artifact.<stream>` for a manifest stream.
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolProgress {
    pub tool_call_id: ToolCallId,
    pub message: String,
    /// A change to the call's live display.
    pub display: Option<cookie_agent_protocol::DisplayDelta>,
    #[serde(default)]
    pub output: Vec<cookie_agent_protocol::ToolOutputChunk>,
}

/// Where a call's validated progress goes when the host captures its output.
#[async_trait]
pub trait ProgressCapture: std::fmt::Debug + Send + Sync {
    /// Captures `progress`, which [`ProgressSink`] has already bounded and
    /// sanitized. `display` is a reserved slot on the display channel, present
    /// when the progress carries a display delta; the capture sends the
    /// progress through it once the output is accepted.
    async fn emit(
        &self,
        progress: ToolProgress,
        display: Option<mpsc::OwnedPermit<ToolProgress>>,
    ) -> Result<(), ToolError>;
}

/// A call's progress channel. It enforces the display and output bounds and
/// hands output chunks to the host's [`ProgressCapture`].
#[derive(Clone)]
pub struct ProgressSink {
    sender: mpsc::Sender<ToolProgress>,
    capture: Option<Arc<dyn ProgressCapture>>,
    budget: Arc<std::sync::Mutex<ProgressBudget>>,
    unknown_blocks: Option<UnknownBlockReporter>,
}

/// Told the tags of the display blocks of kinds this build does not know
/// that a [`ProgressSink`] dropped from one progress update.
pub type UnknownBlockReporter = Arc<dyn Fn(Vec<String>) + Send + Sync>;

impl std::fmt::Debug for ProgressSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProgressSink")
            .field("capture", &self.capture)
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

/// What a call's progress has used of its display bounds: the bytes of its
/// messages, and its live display.
#[derive(Debug, Default)]
struct ProgressBudget {
    message_bytes: usize,
    display: cookie_agent_protocol::LiveDisplay,
}

impl ProgressSink {
    /// A sink for display-only progress: output chunks are rejected.
    #[must_use]
    pub fn new(sender: mpsc::Sender<ToolProgress>) -> Self {
        Self {
            sender,
            capture: None,
            budget: Arc::default(),
            unknown_blocks: None,
        }
    }

    /// A sink whose output chunks go to `capture`.
    #[must_use]
    pub fn with_capture(
        sender: mpsc::Sender<ToolProgress>,
        capture: Arc<dyn ProgressCapture>,
    ) -> Self {
        Self {
            sender,
            capture: Some(capture),
            budget: Arc::default(),
            unknown_blocks: None,
        }
    }

    /// The sink, telling `reporter` of every display block of a kind this
    /// build does not know that it drops from a replacement, as one from a
    /// newer plugin or runtime would carry.
    #[must_use]
    pub fn with_unknown_block_reporter(mut self, reporter: UnknownBlockReporter) -> Self {
        self.unknown_blocks = Some(reporter);
        self
    }

    /// Reports display blocks of kinds this build does not know, of tags
    /// `tags`, that the host dropped from a progress update before it reached
    /// the sink, as a runtime's `tool/progress` carries them.
    pub fn report_unknown_blocks(&self, tags: Vec<String>) {
        if let Some(reporter) = self.unknown_blocks.as_ref().filter(|_| !tags.is_empty()) {
            reporter(tags);
        }
    }

    pub async fn send(&self, mut progress: ToolProgress) -> Result<(), ToolError> {
        if !progress.output.is_empty() && self.capture.is_none() {
            return Err(ToolError::execution("output capture is unavailable"));
        }
        if progress.message.len() > cookie_agent_protocol::SafeDisplayText::MAX_BYTES
            || progress
                .display
                .as_ref()
                .is_some_and(|display| match display {
                    cookie_agent_protocol::DisplayDelta::Append { text, .. } => {
                        text.len() > cookie_agent_protocol::DisplayDelta::MAX_APPEND_BYTES
                    }
                    cookie_agent_protocol::DisplayDelta::Replace { .. } => false,
                })
        {
            return Err(ToolError::resource_limit(
                "tool display delta exceeds the display event bound",
            ));
        }
        if progress.output.len() > cookie_agent_protocol::MAX_TOOL_STREAMS
            || progress.output.iter().fold(0_usize, |total, chunk| {
                total.saturating_add(chunk.text.len())
            }) > cookie_agent_protocol::MAX_TOOL_DELTA_BYTES
        {
            return Err(ToolError::resource_limit(
                "tool output delta exceeds 64 KiB or 8 chunks",
            ));
        }
        for chunk in &progress.output {
            if let Some(name) = &chunk.stream {
                cookie_agent_protocol::validate_tool_stream_name(name)
                    .map_err(ToolError::execution)?;
            }
        }
        {
            let mut budget = self.budget.lock().unwrap_or_else(|p| p.into_inner());
            let remaining =
                cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES.saturating_sub(budget.message_bytes);
            progress.message = crate::sanitize_tool_display(&progress.message, remaining);
            budget.message_bytes += progress.message.len();
            if let Some(cookie_agent_protocol::DisplayDelta::Replace { blocks }) =
                &mut progress.display
            {
                let unknown = cookie_agent_protocol::take_unknown_blocks(blocks);
                if let Some(reporter) = self.unknown_blocks.as_ref().filter(|_| !unknown.is_empty())
                {
                    reporter(unknown);
                }
            }
            progress.display = match progress.display.take().map(sanitize_delta) {
                Some(delta) => budget
                    .display
                    .admit(delta)
                    .map_err(|error| ToolError::execution(error.to_string()))?,
                None => None,
            };
        }
        let permit = if progress.message.is_empty() && progress.display.is_none() {
            None
        } else {
            Some(
                self.sender
                    .clone()
                    .reserve_owned()
                    .await
                    .map_err(|_| ToolError::ProgressSinkClosed)?,
            )
        };
        if let Some(capture) = &self.capture {
            return capture.emit(progress, permit).await;
        }
        if let Some(permit) = permit {
            permit.send(progress);
        }
        Ok(())
    }
}

/// `delta` with the control characters its text may not carry removed: an
/// append's text as [`crate::sanitize_tool_display`] cleans it (keeping SGR
/// colour when it names a terminal stream), a replacement's blocks as the
/// daemon sanitizes blocks it receives.
fn sanitize_delta(
    delta: cookie_agent_protocol::DisplayDelta,
) -> cookie_agent_protocol::DisplayDelta {
    use cookie_agent_protocol::DisplayDelta;
    match delta {
        DisplayDelta::Append {
            index,
            text,
            stream,
        } => DisplayDelta::Append {
            index,
            text: match stream {
                Some(_) => cookie_agent_protocol::sanitize_terminal_text(&text).into_owned(),
                None => crate::sanitize_tool_display(&text, usize::MAX),
            },
            stream,
        },
        DisplayDelta::Replace { blocks } => DisplayDelta::Replace {
            blocks: blocks
                .into_iter()
                .filter_map(cookie_agent_protocol::Open::into_known)
                .map(|mut block| {
                    block.sanitize();
                    cookie_agent_protocol::Open::Known(block)
                })
                .collect(),
        },
    }
}

/// The point at which an executing call of a background-capable tool may be
/// detached, as `docs/site/specs/background-tools.md` section 4 specifies.
///
/// A background-capable tool calls [`DetachPoint::reached`] once it has
/// started its work (bash right after spawning its command), with an
/// optional note for the immediate result the host commits when it detaches
/// the call. The host detaches only at or after that point: a call the model
/// started in the background detaches as soon as it reaches it, and a user's
/// request waits for it. A call that finishes first simply returns.
#[derive(Clone, Debug)]
pub struct DetachPoint {
    sender: Arc<tokio::sync::watch::Sender<Option<Detachable>>>,
}

/// What a call reports when it reaches its detach point.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Detachable {
    /// A line for the immediate result, such as a process ID.
    pub note: Option<String>,
    /// The work reports its own results once detached (a subagent reports
    /// each finished turn), so the host only stops waiting: it drops the
    /// call's execution and sends no `<background_result>`.
    pub reports_itself: bool,
}

/// The host's side of a [`DetachPoint`].
#[derive(Debug)]
pub struct DetachWatch {
    receiver: tokio::sync::watch::Receiver<Option<Detachable>>,
}

impl DetachPoint {
    /// A detach point and the watch the host reads it through.
    #[must_use]
    pub fn channel() -> (Self, DetachWatch) {
        let (sender, receiver) = tokio::sync::watch::channel(None);
        (
            Self {
                sender: Arc::new(sender),
            },
            DetachWatch { receiver },
        )
    }

    /// Marks the call detachable with `note`. Only the first call counts.
    pub fn reached(&self, note: Option<String>) {
        self.reached_with(Detachable {
            note,
            reports_itself: false,
        });
    }

    /// What the call reported at its detach point, once it reached it.
    #[must_use]
    pub fn current(&self) -> Option<Detachable> {
        self.sender.borrow().clone()
    }

    /// Marks the call detachable as `detachable`. Only the first call counts.
    pub fn reached_with(&self, detachable: Detachable) {
        self.sender.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(detachable);
            true
        });
    }
}

impl DetachWatch {
    /// The detach point, once the call has reached it.
    #[must_use]
    pub fn current(&self) -> Option<Detachable> {
        self.receiver.borrow().clone()
    }

    /// Waits until the call reaches its detach point. Pending forever if the
    /// call never does and its point is dropped.
    pub async fn reached(&mut self) -> Detachable {
        loop {
            if let Some(detachable) = self.receiver.borrow_and_update().clone() {
                return detachable;
            }
            if self.receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}
