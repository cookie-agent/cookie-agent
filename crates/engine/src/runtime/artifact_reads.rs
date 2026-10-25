use cookie_agent_protocol::{ArtifactReadTarget, SessionId, ToolOutputManifest};

use std::sync::Arc;

use super::{Engine, artifacts::ArtifactRouter, blocking_io};
use cookie_agent_runtime_api::{
    ArtifactReadPage,
    text_tools::{LineWindow, MAX_PAGE_BYTES},
};

use crate::ToolError;

impl Engine {
    /// Read an artifact, or one stream of a named-stream manifest, as
    /// `session` sees it: only content stored in that session's own tree
    /// resolves (tree-local D1). No ownership lookup is made. This synchronous
    /// API blocks; async tool executors use
    /// [`crate::ToolExecutionContext::read_artifact`].
    ///
    /// The page is the lines of `window`, numbered from 1 as `read` numbers
    /// a file's ([`cookie_agent_runtime_api::text_tools`]).
    pub fn read_artifact(
        &self,
        session: SessionId,
        artifact: &str,
        stream: Option<&str>,
        window: LineWindow,
    ) -> Result<ArtifactReadPage, ToolError> {
        let target = ArtifactReadTarget::parse(artifact, stream).map_err(ToolError::execution)?;
        read_artifact(&self.inner.artifacts, session, &target, window)
    }
}

pub(crate) fn read_artifact(
    store: &ArtifactRouter,
    session: SessionId,
    target: &ArtifactReadTarget,
    window: LineWindow,
) -> Result<ArtifactReadPage, ToolError> {
    let (digest, source) = if let Some(name) = &target.stream {
        let page = store
            .read_paged(session, target.digest.as_str(), 0, 1)
            .map_err(read_error)?;
        let manifest: ToolOutputManifest = serde_json::from_str(&page.content)
            .map_err(|_| ToolError::execution("artifact is not a named-stream manifest"))?;
        manifest.validate().map_err(ToolError::execution)?;
        let stream = manifest
            .streams
            .into_iter()
            .find(|stream| stream.name.as_ref() == Some(name))
            .ok_or_else(|| {
                ToolError::execution(format!("artifact has no declared stream {name}"))
            })?;
        let digest = stream.sha256.ok_or_else(|| {
            ToolError::execution(format!(
                "stream {name} fit its preview: the tool result already holds all of it"
            ))
        })?;
        (digest, format!("artifact.{name}"))
    } else {
        (target.digest.clone(), "artifact".into())
    };
    let page = store
        .read_numbered(session, digest.as_str(), window, MAX_PAGE_BYTES)
        .map_err(read_error)?;
    page.check_in_range(&shown(target))?;
    Ok(ArtifactReadPage { page, source })
}

/// The artifact as an out-of-range error names it: the start of its
/// digest, and its stream.
fn shown(target: &ArtifactReadTarget) -> String {
    let digest = &target.digest.as_str()[..12];
    match &target.stream {
        Some(stream) => format!("artifact {digest}/{stream}"),
        None => format!("artifact {digest}"),
    }
}

pub(crate) async fn read_artifact_async(
    store: Arc<ArtifactRouter>,
    session: SessionId,
    target: ArtifactReadTarget,
    window: LineWindow,
) -> Result<ArtifactReadPage, ToolError> {
    blocking_io::run(move || read_artifact(&store, session, &target, window)).await?
}

fn read_error(error: std::io::Error) -> ToolError {
    match error.kind() {
        std::io::ErrorKind::FileTooLarge => ToolError::resource_limit(error.to_string()),
        // Artifacts are written without fsync, so a crash can lose one a
        // durable event references: say so instead of failing obscurely.
        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData => ToolError::execution(
            format!("output unavailable ({error}): the stored artifact is missing or damaged"),
        ),
        _ => ToolError::execution(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(digest: &str) -> ArtifactReadTarget {
        ArtifactReadTarget::parse(digest, None).unwrap()
    }

    /// The first line.
    fn line_one() -> LineWindow {
        LineWindow::new(Some(1), Some(1)).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_artifact_verification_leaves_async_tasks_and_other_readers_responsive() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
        let (_, first) = store.retain(crate::test_session_id(), b"first\n").unwrap();
        let (_, second) = store.retain(crate::test_session_id(), b"second\n").unwrap();
        let (entered, release) =
            blocking_io::gate(store.io_test_hook(), "read", Some(first.clone()));
        let reader = store.clone();
        let blocked = tokio::spawn(async move {
            read_artifact_async(reader, crate::test_session_id(), target(&first), line_one()).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), entered)
            .await
            .unwrap()
            .unwrap();
        assert!(!blocked.is_finished());
        assert_eq!(tokio::spawn(async { 7 }).await.unwrap(), 7);
        let other = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read_artifact_async(store, crate::test_session_id(), target(&second), line_one()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(other.page.text, "1: second\n");
        assert!(
            !blocked.is_finished(),
            "first reader must remain behind its I/O gate"
        );
        release.send(()).unwrap();
        assert_eq!(blocked.await.unwrap().unwrap().page.text, "1: first\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn in_flight_read_does_not_reinsert_a_handle_invalidated_by_a_revert_sweep() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
        let (_, digest) = store
            .retain(crate::test_session_id(), b"verified\n")
            .unwrap();
        let path = target(&digest);
        read_artifact_async(
            store.clone(),
            crate::test_session_id(),
            path.clone(),
            line_one(),
        )
        .await
        .unwrap();
        let (entered, release) =
            blocking_io::gate(store.io_test_hook(), "read", Some(digest.clone()));
        let reader = store.clone();
        let reading_path = path.clone();
        let reading = tokio::spawn(async move {
            read_artifact_async(reader, crate::test_session_id(), reading_path, line_one()).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), entered)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .collect_reverted(
                    crate::test_session_id(),
                    [digest.clone()].into_iter().collect(),
                    || Ok(Some(std::collections::HashSet::new())),
                )
                .unwrap(),
            1
        );
        release.send(()).unwrap();
        assert_eq!(reading.await.unwrap().unwrap().page.text, "1: verified\n");
        assert!(
            read_artifact_async(store, crate::test_session_id(), path, line_one())
                .await
                .unwrap_err()
                .to_string()
                .contains("artifact missing")
        );
    }
}
