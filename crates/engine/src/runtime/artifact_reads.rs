use cookie_agent_protocol::{ArtifactReadPath, PersistedToolResult, ToolOutputManifest};

use std::sync::Arc;

use super::{Engine, artifacts::ArtifactStore, blocking_io};
use crate::ToolError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactReadPage {
    pub content: String,
    pub next_offset_lines: Option<u64>,
    pub source: String,
}

impl Engine {
    /// Read a public artifact URI by possession, without a session ownership lookup.
    /// This synchronous API blocks; async tool executors use `ToolExecutionContext::read_artifact`.
    pub fn read_artifact(
        &self,
        path: &str,
        offset: u64,
        limit: u64,
    ) -> Result<ArtifactReadPage, ToolError> {
        read_artifact(&self.inner.artifacts, path, offset, limit)
    }
}

pub(crate) fn read_artifact(
    store: &ArtifactStore,
    path: &str,
    offset: u64,
    limit: u64,
) -> Result<ArtifactReadPage, ToolError> {
    let target = ArtifactReadPath::parse(path).map_err(ToolError::execution)?;
    if limit == 0 {
        return Err(ToolError::execution("artifact page limit must be positive"));
    }
    let (digest, source) = if let Some(name) = target.stream {
        let page = store
            .read_paged(target.digest.as_str(), 0, 1)
            .map_err(read_error)?;
        let manifest: ToolOutputManifest = serde_json::from_str(&page.content)
            .map_err(|_| ToolError::execution("artifact is not a named-stream manifest"))?;
        manifest.validate().map_err(ToolError::execution)?;
        let stream = manifest
            .streams
            .into_iter()
            .find(|stream| stream.name.as_deref() == Some(&name))
            .ok_or_else(|| {
                ToolError::execution(format!("artifact has no declared stream {name}"))
            })?;
        (stream.sha256, format!("artifact.{name}"))
    } else {
        (target.digest, "artifact".into())
    };
    let page = store
        .read_paged(digest.as_str(), offset, limit.min(2_000))
        .map_err(read_error)?;
    if page.content.len() > PersistedToolResult::MAX_OUTPUT_BYTES {
        return Err(ToolError::resource_limit(
            "artifact page exceeds the 2 MiB output limit; request a smaller page",
        ));
    }
    Ok(ArtifactReadPage {
        content: page.content,
        next_offset_lines: page.next_offset_lines,
        source,
    })
}

pub(crate) async fn read_artifact_async(
    store: Arc<ArtifactStore>,
    path: &str,
    offset: u64,
    limit: u64,
) -> Result<ArtifactReadPage, ToolError> {
    let path = path.to_owned();
    blocking_io::run(move || read_artifact(&store, &path, offset, limit)).await?
}

fn read_error(error: std::io::Error) -> ToolError {
    if error.kind() == std::io::ErrorKind::FileTooLarge {
        ToolError::resource_limit(error.to_string())
    } else {
        ToolError::execution(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_artifact_verification_leaves_async_tasks_and_other_readers_responsive() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(directory.path().join("artifacts")).unwrap();
        let (_, first) = store.retain(b"first\n").unwrap();
        let (_, second) = store.retain(b"second\n").unwrap();
        let (entered, release) = blocking_io::gate(&store, "read", Some(first.clone()));
        let reader = store.clone();
        let blocked = tokio::spawn(async move {
            read_artifact_async(reader, &format!("artifact://{first}"), 0, 1).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), entered)
            .await
            .unwrap()
            .unwrap();
        assert!(!blocked.is_finished());
        assert_eq!(tokio::spawn(async { 7 }).await.unwrap(), 7);
        let other = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read_artifact_async(store, &format!("artifact://{second}"), 0, 1),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(other.content, "second\n");
        assert!(
            !blocked.is_finished(),
            "first reader must remain behind its I/O gate"
        );
        release.send(()).unwrap();
        assert_eq!(blocked.await.unwrap().unwrap().content, "first\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn in_flight_read_does_not_reinsert_a_handle_invalidated_by_gc() {
        let directory = tempfile::tempdir().unwrap();
        let sessions = directory.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let store = ArtifactStore::open(directory.path().join("artifacts")).unwrap();
        let (_, digest) = store.retain(b"verified\n").unwrap();
        let path = format!("artifact://{digest}");
        read_artifact_async(store.clone(), &path, 0, 1)
            .await
            .unwrap();
        let (entered, release) = blocking_io::gate(&store, "read", Some(digest));
        let reader = store.clone();
        let reading_path = path.clone();
        let reading =
            tokio::spawn(async move { read_artifact_async(reader, &reading_path, 0, 1).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), entered)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .collect_garbage(&sessions, std::time::Duration::ZERO)
                .unwrap()
                .deleted,
            1
        );
        release.send(()).unwrap();
        assert_eq!(reading.await.unwrap().unwrap().content, "verified\n");
        assert!(
            read_artifact_async(store, &path, 0, 1)
                .await
                .unwrap_err()
                .to_string()
                .contains("artifact missing")
        );
    }
}
