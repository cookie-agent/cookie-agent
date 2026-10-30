//! The daemon's side of transfers' `fs/*` requests (runtime protocol
//! 1.13.0, `docs/site/specs/machines-on-demand.md` section 6.3), made of a
//! runtime that reported `files`.

use std::time::Duration;

use cookie_agent_protocol::runtime_protocol::{
    FsCopyParams, FsDirection, FsHandle, FsKind, FsPrepareParams, FsPrepareResult,
    FsReadChunkParams, FsReadChunkResult, FsReadOpenParams, FsReadOpenResult, FsWriteChunkParams,
    FsWriteCommitParams, FsWriteOpenParams, FsWritten, method,
};

use super::{InstanceConnection, RuntimeClient};

/// How long one `fs/*` request may take: a chunk, an open, or a commit,
/// which may sync a large file or move a large folder into place.
const FS_REQUEST_DEADLINE: Duration = Duration::from_secs(120);

impl RuntimeClient {
    /// Whether the runtime takes part in transfers.
    pub(crate) fn files(&self) -> bool {
        self.inner.files.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The runtime's protocol version, once it initialized.
    pub(crate) fn protocol_version(&self) -> Option<cookie_agent_protocol::SemVer> {
        self.inner.protocol_version.get().copied()
    }

    async fn fs<P, R>(&self, method: &str, params: &P) -> Result<R, String>
    where
        P: serde::Serialize + Sync,
        R: serde::de::DeserializeOwned,
    {
        self.inner
            .request_within(method, params, FS_REQUEST_DEADLINE)
            .await
            .map_err(|error| fs_failure(&self.inner.runtime_id, &error))
    }

    pub(crate) async fn fs_read_open(&self, route: u64) -> Result<FsReadOpenResult, String> {
        self.fs(method::FS_READ_OPEN, &FsReadOpenParams { route })
            .await
    }

    pub(crate) async fn fs_read_chunk(
        &self,
        handle: u64,
        max_bytes: u64,
    ) -> Result<FsReadChunkResult, String> {
        self.fs(
            method::FS_READ_CHUNK,
            &FsReadChunkParams { handle, max_bytes },
        )
        .await
    }

    pub(crate) async fn fs_write_open(
        &self,
        route: u64,
        kind: FsKind,
        overwrite: bool,
        size: Option<u64>,
    ) -> Result<u64, String> {
        let opened: FsHandle = self
            .fs(
                method::FS_WRITE_OPEN,
                &FsWriteOpenParams {
                    route,
                    kind,
                    overwrite,
                    size,
                },
            )
            .await?;
        Ok(opened.handle)
    }

    pub(crate) async fn fs_write_chunk(
        &self,
        handle: u64,
        offset: u64,
        data: String,
    ) -> Result<(), String> {
        let _: cookie_agent_protocol::runtime_protocol::Empty = self
            .fs(
                method::FS_WRITE_CHUNK,
                &FsWriteChunkParams {
                    handle,
                    offset,
                    data,
                },
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn fs_write_commit(
        &self,
        handle: u64,
        sha256: String,
    ) -> Result<FsWritten, String> {
        self.fs(
            method::FS_WRITE_COMMIT,
            &FsWriteCommitParams { handle, sha256 },
        )
        .await
    }

    /// Releases a route or handle; a runtime that is gone has released it.
    pub(crate) async fn fs_close(&self, handle: u64) {
        let _: Result<cookie_agent_protocol::runtime_protocol::Empty, String> =
            self.fs(method::FS_CLOSE, &FsHandle { handle }).await;
    }

    /// Copies one route to another on the runtime's own machine. It has no
    /// deadline: a large folder takes long. Closing `to` stops it.
    pub(crate) async fn fs_copy(
        &self,
        from: u64,
        to: u64,
        overwrite: bool,
    ) -> Result<FsWritten, String> {
        self.inner
            .peer
            .request(
                method::FS_COPY,
                &FsCopyParams {
                    from,
                    to,
                    overwrite,
                },
            )
            .await
            .map_err(|error| fs_failure(&self.inner.runtime_id, &error))
    }
}

impl InstanceConnection {
    /// `fs/prepare` of `path` on the instance, for `direction`.
    pub(crate) async fn fs_prepare(
        &self,
        path: &str,
        direction: FsDirection,
    ) -> Result<FsPrepareResult, String> {
        let instance = self
            .instance
            .clone()
            .ok_or_else(|| "a plugin takes no transfers".to_owned())?;
        self.client
            .fs(
                method::FS_PREPARE,
                &FsPrepareParams {
                    instance,
                    path: path.to_owned(),
                    direction,
                },
            )
            .await
    }
}

/// What the model reads of a failed `fs/*` request: the runtime's detail
/// when it gave one.
fn fs_failure(runtime_id: &str, error: &cookie_agent_runtime_api::rpc::RpcError) -> String {
    match error {
        cookie_agent_runtime_api::rpc::RpcError::Remote { message, data, .. } => data
            .as_ref()
            .and_then(|data| data.get("detail"))
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| message.clone(), str::to_owned),
        other => format!("runtime `{runtime_id}`: {other}"),
    }
}
