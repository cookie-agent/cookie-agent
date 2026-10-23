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
        background: false,
        session: SessionId::new_v7(),
        run: RunId::new_v7(),
        progress: ProgressSink::new(progress),
        cancellation: CancellationToken::new(),
        turn_context,
        artifacts: ArtifactRouter::open_flat(artifact_directory.into())
            .map_err(|error| ToolError::execution(error.to_string()))?,
        detach: cookie_agent_runtime_api::DetachPoint::channel().0,
        producers: None,
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

/// `prepared`, a call of tool `name` that `provider` prepared under
/// `origin`, identified as the daemon identifies a call of that runtime's
/// tool: its approval identity built from the permission and resource the
/// provider reports for it.
pub fn identify(
    provider: &dyn crate::ToolProvider,
    name: &str,
    origin: &crate::ToolOrigin,
    prepared: crate::PreparedTool,
) -> Result<crate::PreparedTool, ToolError> {
    let source = origin.runtime().map(|runtime| {
        cookie_agent_protocol::Open::Known(cookie_agent_protocol::ToolSource::Runtime {
            runtime_id: cookie_agent_protocol::RuntimeId::new(runtime.id())
                .expect("a test runtime has a valid ID"),
            attachment: cookie_agent_protocol::AttachmentName::new(runtime.attachment_name())
                .expect("a test runtime has a valid attachment name"),
            capability: runtime.capability().clone(),
        })
    });
    let tool = cookie_agent_protocol::ToolIdentity {
        name: name.to_owned(),
        published_name: name.to_owned(),
        source,
    };
    let permission_name = match prepared.declaration() {
        Some(declaration) => declaration.permission_name.clone(),
        None => provider
            .get_permission_resource(name, prepared.normalized_arguments())?
            .0
            .map(str::to_owned),
    };
    crate::runtime::tool_execution::apply_permission_resource(
        provider,
        name,
        permission_name.as_deref(),
        &tool,
        origin,
        prepared,
    )
    .map(|(prepared, _)| prepared)
}

/// A `.tar.gz` starter kit of `files` (path, bytes) with a `manifest.json`
/// listing them, packed as the release workflow packs one, for tests of
/// onboarding outside the engine.
#[must_use]
pub fn pack_kit(files: &[(&str, &[u8])]) -> Vec<u8> {
    use sha2::Digest as _;
    let manifest = serde_json::to_vec(&serde_json::json!({
        "kit_version": 1,
        "files": files
            .iter()
            .map(|(path, bytes)| ((*path).to_owned(), format!("{:x}", sha2::Sha256::digest(bytes))))
            .collect::<std::collections::BTreeMap<_, _>>(),
    }))
    .expect("a manifest serializes");
    let mut entries = files.to_vec();
    entries.push(("manifest.json", &manifest));
    crate::runtime::onboarding::kit::pack(&entries)
}
