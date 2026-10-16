//! What a tool sees while it is prepared and executed, and the host services
//! it reaches through that context: progress capture, attachment retention,
//! and background jobs.

use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use async_trait::async_trait;
use cookie_agent_protocol::{
    AdaptorId, AgentId, ArtifactReadTarget, ArtifactReference, ModelCapabilities, ModelKey, RunId,
    SessionId, Sha256Digest, ToolAttachment, ToolCallId,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{ToolError, ToolOrigin, WorkspaceContext, media::validate_attachment};

/// Exact model adapter family of a turn. Unlike [`AdaptorId`] it keeps
/// compatible families apart, because their media delivery differs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterFamily {
    Anthropic,
    AnthropicCompatible,
    OpenaiChat,
    OpenaiResponses,
    OpenaiCompatible,
    GoogleGemini,
    GoogleVertexGemini,
    AwsBedrockConverse,
    AzureOpenaiChat,
    AzureOpenaiResponses,
    CohereV2Chat,
}

/// Immutable harness context captured for one tool preparation/execution batch.
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
    /// Exact adapter family, preserving compatible-family distinctions.
    pub adapter_family: AdapterFamily,
    /// Public capabilities of the exact model binding that produced the tool call.
    pub capabilities: ModelCapabilities,
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
    /// Present for calls the host runs; lets the call start a background job.
    pub background: Option<Arc<dyn BackgroundJobs>>,
}

impl ToolExecutionContext {
    /// Reserves a job that outlives this call and reports back to the session
    /// through a producer message when it finishes.
    pub async fn start_background(&self) -> Result<BackgroundJob, ToolError> {
        self.background
            .as_ref()
            .ok_or_else(|| ToolError::execution("background jobs are unavailable here"))?
            .reserve(self.session)
            .await
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
    pub display: Option<String>,
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
#[derive(Clone, Debug)]
pub struct ProgressSink {
    sender: mpsc::Sender<ToolProgress>,
    capture: Option<Arc<dyn ProgressCapture>>,
    display_used: Arc<std::sync::Mutex<usize>>,
}

impl ProgressSink {
    /// A sink for display-only progress: output chunks are rejected.
    #[must_use]
    pub fn new(sender: mpsc::Sender<ToolProgress>) -> Self {
        Self {
            sender,
            capture: None,
            display_used: Arc::new(std::sync::Mutex::new(0)),
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
            display_used: Arc::new(std::sync::Mutex::new(0)),
        }
    }

    pub async fn send(&self, mut progress: ToolProgress) -> Result<(), ToolError> {
        if !progress.output.is_empty() && self.capture.is_none() {
            return Err(ToolError::execution("output capture is unavailable"));
        }
        if progress.message.len() > cookie_agent_protocol::SafeDisplayText::MAX_BYTES
            || progress.display.as_ref().is_some_and(|display| {
                display.len() > cookie_agent_protocol::SafeDisplayText::MAX_BYTES
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
            let mut used = self.display_used.lock().unwrap_or_else(|p| p.into_inner());
            let remaining = cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES.saturating_sub(*used);
            progress.message = crate::sanitize_tool_display(&progress.message, remaining);
            *used += progress.message.len();
            progress.display = progress
                .display
                .as_deref()
                .map(|text| {
                    crate::sanitize_tool_display(
                        text,
                        cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES.saturating_sub(*used),
                    )
                })
                .filter(|text| !text.is_empty());
            *used += progress.display.as_ref().map_or(0, String::len);
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

/// Lets one executing call reserve background jobs. The host owns each job's
/// output capture, its producer registration, and the completion message
/// that wakes the session.
#[async_trait]
pub trait BackgroundJobs: std::fmt::Debug + Send + Sync {
    /// Reserves a job for the call in `session`.
    async fn reserve(&self, session: SessionId) -> Result<BackgroundJob, ToolError>;
}

/// The future that runs a background job to its report.
pub type BackgroundTask = Pin<Box<dyn Future<Output = BackgroundReport> + Send>>;

/// Starts a reserved job; implemented by the host.
pub trait BackgroundLauncher: Send {
    /// Runs `task` detached from the call that reserved it.
    fn launch(self: Box<Self>, task: BackgroundTask) -> Result<(), ToolError>;
}

/// What a finished background job reports to the agent. The engine renders
/// `<element attributes…>`, the body, and the job's output preview (truncated
/// like any tool result), then the closing tag.
pub struct BackgroundReport {
    /// One-line summary shown on the notification row.
    pub summary: String,
    /// Element name wrapping the agent-visible report, such as `background_bash`.
    pub element: String,
    /// Attributes on the opening tag, in order.
    pub attributes: Vec<(String, String)>,
    /// Report lines placed before the output preview.
    pub body: String,
}

/// A reserved background job. [`BackgroundJob::progress`] captures the job's
/// output like a foreground call's; [`BackgroundJob::launch`] hands over the
/// future that runs it. Dropping the job unlaunched releases the reservation.
pub struct BackgroundJob {
    progress: ProgressSink,
    launcher: Box<dyn BackgroundLauncher>,
}

impl BackgroundJob {
    /// A reserved job writing to `progress` and started by `launcher`.
    #[must_use]
    pub fn new(progress: ProgressSink, launcher: Box<dyn BackgroundLauncher>) -> Self {
        Self { progress, launcher }
    }

    /// The sink the job writes its output to.
    #[must_use]
    pub fn progress(&self) -> ProgressSink {
        self.progress.clone()
    }

    /// Runs `job` detached from the call. Its report is delivered once the
    /// call commits; the job is dropped (killing what it owns) if the call
    /// never commits, is reverted, or its session is deleted.
    pub fn launch(
        self,
        job: impl Future<Output = BackgroundReport> + Send + 'static,
    ) -> Result<(), ToolError> {
        let Self { progress, launcher } = self;
        drop(progress);
        launcher.launch(Box::pin(job))
    }
}
