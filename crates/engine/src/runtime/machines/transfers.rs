//! Where a `transfer` reads and writes: one of the session's attachments on
//! the machine it names (`docs/site/specs/machines-on-demand.md` section 6).

use cookie_agent_protocol::{RuntimeId, SessionId};
use cookie_agent_runtime_api::ResourceScope;

use crate::runtime::Engine;
use crate::runtimes::InstanceConnection;

/// One end of a transfer on a machine: the attachment's runtime connection,
/// and where its path permission is checked.
pub(crate) struct TransferEndpoint {
    pub(crate) connection: InstanceConnection,
    /// The machine as the session's machines block names it.
    pub(crate) machine: String,
    pub(crate) scope: ResourceScope,
}

impl Engine {
    /// The end of a transfer on `machine`: the session's attachment there
    /// whose runtime takes transfers.
    pub(crate) fn transfer_endpoint(
        &self,
        session: SessionId,
        machine: &str,
    ) -> Result<TransferEndpoint, String> {
        let attachments = self
            .session_runtimes(session)
            .map_err(|error| error.to_string())?;
        let mut machines = std::collections::BTreeSet::new();
        let mut refusals = Vec::new();
        let root = self.root_session_of(session);
        for attachment in attachments {
            if attachment.runtime_id.as_str() == RuntimeId::GLOBAL {
                continue;
            }
            let named = self
                .runtime_machine(attachment.runtime_id.as_str())
                .map_or_else(
                    || attachment.runtime_id.to_string(),
                    |name| name.to_string(),
                );
            machines.insert(named.clone());
            if named != machine {
                continue;
            }
            let Some(instance) = self.inner.runtimes.instance(root, &attachment) else {
                refusals.push(format!(
                    "runtime `{}` is not connected",
                    attachment.runtime_id
                ));
                continue;
            };
            let Some(connection) = instance.connection() else {
                refusals.push(format!(
                    "runtime `{}` runs inside the daemon without the runtime protocol",
                    attachment.runtime_id
                ));
                continue;
            };
            if !connection.client.files() {
                refusals.push(format!(
                    "runtime `{}` speaks runtime protocol {}, which has no transfers (1.13.0 \
                     and later have them): update it",
                    attachment.runtime_id,
                    connection
                        .client
                        .protocol_version()
                        .map_or_else(|| "unknown".to_owned(), |version| version.to_string())
                ));
                continue;
            }
            let info = instance.info();
            return Ok(TransferEndpoint {
                scope: ResourceScope {
                    runtime: attachment.runtime_id.to_string(),
                    workspace: info
                        .workspace()
                        .map(|workspace| workspace.root().to_owned()),
                    home: info.home().map(str::to_owned),
                    platform: info
                        .workspace()
                        .and_then(|workspace| workspace.platform().map(str::to_owned)),
                },
                connection,
                machine: named,
            });
        }
        if refusals.is_empty() {
            let known = if machines.is_empty() {
                "none".to_owned()
            } else {
                machines
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return Err(format!(
                "machine `{machine}` is not attached to this session (its machines: {known}); \
                 attach it with attach_machine first"
            ));
        }
        Err(format!(
            "machine `{machine}` takes no transfers: {}",
            refusals.join("; ")
        ))
    }

    /// Whether `session` has a machine attached whose runtime takes
    /// transfers, which `transfer` is offered for.
    pub(crate) fn session_takes_transfers(&self, session: SessionId) -> bool {
        self.session_runtimes(session)
            .unwrap_or_default()
            .iter()
            .filter(|attachment| attachment.runtime_id.as_str() != RuntimeId::GLOBAL)
            .any(|attachment| {
                self.inner
                    .runtimes
                    .instance(self.root_session_of(session), attachment)
                    .and_then(|instance| instance.connection())
                    .is_some_and(|connection| connection.client.files())
            })
    }

    /// The most bytes one transfer moves, when `[transfer] max_bytes` sets
    /// a limit.
    pub(crate) fn transfer_limit(&self) -> Option<u64> {
        self.inner.config.load().runtime.transfer.limit()
    }
}

/// A folder artifact as the tar stream a transfer sends it as
/// (`docs/site/specs/folder-artifacts.md` section 7.4): its files in a
/// temporary file, with mode `0644` and the time it was built.
pub(crate) struct FolderTar {
    file: std::sync::Mutex<std::fs::File>,
    /// The stream's length and SHA-256.
    pub(crate) size: u64,
    pub(crate) sha256: String,
    pub(crate) files: u64,
    /// The files' bytes together.
    pub(crate) bytes: u64,
}

impl FolderTar {
    fn build(
        store: &crate::runtime::artifacts::ArtifactStore,
        manifest: &cookie_agent_protocol::FolderManifest,
    ) -> std::io::Result<Self> {
        use sha2::Digest as _;
        use std::io::{Read as _, Seek as _};

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let mut builder = tar::Builder::new(tempfile::tempfile()?);
        for entry in &manifest.entries {
            let blob = store.open_existing(&entry.sha256)?.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("the folder's file {} is missing", entry.path),
                )
            })?;
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(entry.byte_length);
            header.set_mode(0o644);
            header.set_mtime(now);
            header.set_uid(0);
            header.set_gid(0);
            builder.append_data(&mut header, &entry.path, blob.take(entry.byte_length))?;
        }
        let mut file = builder.into_inner()?;
        file.rewind()?;
        let mut hasher = sha2::Sha256::new();
        let size = std::io::copy(&mut file, &mut hasher)?;
        file.rewind()?;
        Ok(Self {
            file: std::sync::Mutex::new(file),
            size,
            sha256: format!("{:x}", hasher.finalize()),
            files: manifest.entries.len() as u64,
            bytes: manifest.byte_length(),
        })
    }

    /// `length` bytes of the stream from `offset`. Blocking.
    pub(crate) fn read(&self, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
        use std::io::{Read as _, Seek as _};

        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.seek(std::io::SeekFrom::Start(offset))?;
        let mut data = Vec::new();
        (&mut *file).take(length).read_to_end(&mut data)?;
        Ok(data)
    }
}

impl Engine {
    /// The artifact a transfer's `from` names in `session`'s tree, and its
    /// length; see [`crate::runtime::artifacts::reads::transfer_source`].
    /// A folder comes with the tar stream it is sent as, and the stream's
    /// length.
    pub(crate) async fn transfer_artifact(
        &self,
        session: SessionId,
        artifact: String,
        stream: Option<String>,
    ) -> Result<(String, u64, Option<FolderTar>), crate::ToolError> {
        let store = std::sync::Arc::clone(&self.inner.artifacts);
        crate::runtime::blocking_io::run(move || {
            let (digest, size) = crate::runtime::artifacts::reads::transfer_source(
                &store,
                session,
                &artifact,
                stream.as_deref(),
            )?;
            if stream.is_some() {
                return Ok((digest, size, None));
            }
            let read_error = |error: std::io::Error| crate::ToolError::execution(error.to_string());
            let tree = store
                .read_store(session)
                .map_err(read_error)?
                .ok_or_else(|| crate::ToolError::execution("the session's artifacts are gone"))?;
            match crate::runtime::artifacts::read_folder_manifest(&tree, &digest)
                .map_err(read_error)?
            {
                Some(manifest) => {
                    let tar = FolderTar::build(&tree, &manifest).map_err(read_error)?;
                    Ok((digest, tar.size, Some(tar)))
                }
                None => Ok((digest, size, None)),
            }
        })
        .await?
    }

    /// `length` bytes of blob `digest` of `session`'s tree from `offset`.
    pub(crate) async fn transfer_artifact_chunk(
        &self,
        session: SessionId,
        digest: String,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, crate::ToolError> {
        let store = std::sync::Arc::clone(&self.inner.artifacts);
        crate::runtime::blocking_io::run(move || {
            crate::runtime::artifacts::reads::read_transfer_chunk(
                &store, session, &digest, offset, length,
            )
        })
        .await?
    }
}
