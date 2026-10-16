//! Engine-backed host services for testing tool providers outside a running
//! engine.

use std::{path::PathBuf, sync::Arc};

use cookie_agent_protocol::{RunId, SessionId, ToolOutputDeclaration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    ProgressSink, ToolError, ToolExecutionContext, ToolProgress, TurnAgentContext,
    events::OutputHub,
    runtime::{ArtifactRouter, OutputCapture},
};

/// An execution context for a fresh session whose artifacts live in
/// `artifact_directory`. It has no output capture and no background jobs.
pub fn tool_execution_context(
    artifact_directory: impl Into<PathBuf>,
    turn_context: Arc<TurnAgentContext>,
) -> Result<ToolExecutionContext, ToolError> {
    let (progress, _receiver) = mpsc::channel(1);
    Ok(ToolExecutionContext {
        session: SessionId::new_v7(),
        run: RunId::new_v7(),
        progress: ProgressSink::new(progress),
        cancellation: CancellationToken::new(),
        turn_context,
        artifacts: ArtifactRouter::open_flat(artifact_directory.into())
            .map_err(|error| ToolError::execution(error.to_string()))?,
        background: None,
    })
}

/// A progress sink that captures output as the engine does, retaining it in
/// `directory` and publishing it live on `output`.
pub async fn capturing_progress_sink(
    sender: mpsc::Sender<ToolProgress>,
    output: OutputHub,
    directory: PathBuf,
    declaration: ToolOutputDeclaration,
) -> Result<ProgressSink, ToolError> {
    let router =
        ArtifactRouter::open_flat(directory).map_err(|e| ToolError::execution(e.to_string()))?;
    let capture =
        OutputCapture::new(router, SessionId::new_v7(), declaration, 2_000, 16 * 1024).await?;
    Ok(capture.progress_sink(sender, output))
}
