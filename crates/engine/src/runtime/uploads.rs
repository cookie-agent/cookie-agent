//! Client uploads into a session tree's artifact store, reads of any
//! artifact of a tree, and the leases that keep a finished upload until an
//! input references it (`docs/site/specs/compatibility-v1.md` section 11.3).
//!
//! An upload in progress streams into a private temporary file of its
//! tree's store, hashing as it goes, and belongs to the connection that
//! began it. Finishing it checks the digest, recognizes the content, and
//! publishes the blob under its digest with a lease of [`UPLOAD_LEASE`].
//!
//! **Leases and pins.** One lock, [`UploadState::leases`], guards three
//! things: the leases, the pins admission takes on the artifacts an input
//! names, and every deletion that could remove such an artifact (a lease
//! sweep, and a revert's sweep). A blob that is leased or pinned is never
//! deleted. Admission pins an input's artifacts and checks that they exist
//! under that lock, and keeps the pins until the event that references them
//! is in the log; a sweep deletes under the same lock only what no log of
//! the tree references. So a sweep either ran first, and admission finds
//! the artifact gone, or it runs after, and finds it pinned or referenced.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use cookie_agent_protocol::{
    ARTIFACT_CHUNK_BYTES, ArtifactRef, ArtifactUploadFinishResult, ImageSize, InputMedia, ModelKey,
    SessionId, UploadId, UserContent,
};
use cookie_agent_runtime_api::media::{
    self as runtime_media, MAX_VIDEO_ATTACHMENT_BYTES, UploadRejection,
};
use sha2::{Digest as _, Sha256};

use super::{
    Engine, EngineError, Inner,
    artifacts::{ArtifactStore, is_digest_name_common},
    blocking_io,
};

/// How long a finished upload no event references is kept.
pub(crate) const UPLOAD_LEASE: Duration = Duration::from_secs(60 * 60);
/// The most uploads one connection may have in progress. Each holds a
/// temporary file, never its bytes in memory.
pub const MAX_UPLOADS_PER_CONNECTION: usize = 8;

/// Why an upload, a read, or an input's media was refused. The server
/// answers each with its registry code.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MediaError {
    #[error("no upload of this connection has that ID")]
    UploadNotFound,
    #[error("a chunk at offset {offset}, but the upload holds {expected_offset} bytes")]
    OffsetMismatch { offset: u64, expected_offset: u64 },
    #[error("the upload began with {byte_length} bytes and would hold {received}")]
    SizeMismatch { byte_length: u64, received: u64 },
    #[error("the connection already has {max_uploads} uploads in progress")]
    TooManyUploads { max_uploads: usize },
    #[error("the uploaded bytes hash to {actual}")]
    DigestMismatch { actual: String },
    #[error("content of type {media_type} cannot be attached: {reason}")]
    TypeUnsupported { media_type: String, reason: String },
    #[error("{byte_length} bytes{}; the limit is {max_bytes} bytes{}", .width.zip(*.height).map(|(width, height)| format!(" ({width}×{height})")).unwrap_or_default(), .max_dimension.map(|side| format!(", {side} pixels a side")).unwrap_or_default())]
    TooLarge {
        kind: String,
        byte_length: u64,
        max_bytes: u64,
        width: Option<u32>,
        height: Option<u32>,
        max_dimension: Option<u32>,
        max_pixels: Option<u64>,
    },
    #[error("model {model} accepts no {kind} input")]
    Unsupported { kind: String, model: ModelKey },
    #[error("{count} {kind} parts; model accepts {max_count}")]
    LimitExceeded {
        kind: String,
        count: u32,
        max_count: u32,
    },
    #[error("artifact {sha256} is not in the session tree's store: {reason}")]
    ArtifactNotFound { sha256: String, reason: String },
    #[error("{0}")]
    InvalidRequest(String),
}

impl From<MediaError> for EngineError {
    fn from(error: MediaError) -> Self {
        Self::Media(error)
    }
}

/// The engine's uploads in progress and its leases.
#[derive(Debug, Default)]
pub(crate) struct UploadState {
    in_progress: Mutex<HashMap<UploadId, InProgress>>,
    pub(crate) leases: Mutex<Leases>,
}

/// An upload in progress and the connection it belongs to. The connection
/// is kept outside the upload's own lock, which a chunk holds while it
/// writes.
#[derive(Debug)]
struct InProgress {
    connection: u64,
    upload: Arc<Mutex<Upload>>,
}

/// One tree's artifact, by its tree's root and its digest.
type TreeArtifact = (SessionId, String);

/// Finished uploads not yet known to be referenced, and the artifacts
/// admission holds while the input naming them is written.
#[derive(Debug, Default)]
pub(crate) struct Leases {
    expiry: HashMap<TreeArtifact, Instant>,
    pins: HashMap<TreeArtifact, usize>,
}

impl Leases {
    /// The digests of `tree` no sweep may delete: leased or pinned.
    pub(crate) fn held(&self, tree: SessionId) -> HashSet<String> {
        self.expiry
            .keys()
            .chain(self.pins.keys())
            .filter(|(of, _)| *of == tree)
            .map(|(_, digest)| digest.clone())
            .collect()
    }

    fn lease(&mut self, key: TreeArtifact, until: Instant) {
        let expiry = self.expiry.entry(key).or_insert(until);
        *expiry = (*expiry).max(until);
    }
}

#[derive(Debug)]
struct Upload {
    tree: SessionId,
    store: Arc<ArtifactStore>,
    temporary: String,
    file: File,
    byte_length: u64,
    received: u64,
    hasher: Sha256,
    filename: Option<String>,
    published: bool,
}

impl Drop for Upload {
    fn drop(&mut self) {
        if !self.published {
            self.store.discard_capture(&self.temporary);
        }
    }
}

/// Artifacts admission holds for an input until the event naming them is
/// written; dropping it releases them.
#[must_use = "the artifacts are held only while the pin lives"]
pub(crate) struct PinnedArtifacts {
    inner: Weak<Inner>,
    keys: Vec<TreeArtifact>,
}

impl Drop for PinnedArtifacts {
    fn drop(&mut self) {
        if self.keys.is_empty() {
            return;
        }
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut leases = inner
            .uploads
            .leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for key in self.keys.drain(..) {
            if let Some(count) = leases.pins.get_mut(&key) {
                *count -= 1;
                if *count == 0 {
                    leases.pins.remove(&key);
                }
            }
        }
    }
}

impl Engine {
    /// Starts an upload of `byte_length` bytes into the store of `session`'s
    /// tree for `connection`.
    pub async fn begin_upload(
        &self,
        connection: u64,
        session: SessionId,
        byte_length: u64,
        filename: Option<String>,
    ) -> Result<UploadId, EngineError> {
        if byte_length == 0 {
            return Err(
                MediaError::InvalidRequest("an upload has at least one byte".into()).into(),
            );
        }
        if byte_length > MAX_VIDEO_ATTACHMENT_BYTES {
            return Err(MediaError::TooLarge {
                kind: "file".into(),
                byte_length,
                max_bytes: MAX_VIDEO_ATTACHMENT_BYTES,
                width: None,
                height: None,
                max_dimension: None,
                max_pixels: None,
            }
            .into());
        }
        if let Some(filename) = &filename {
            let reference = ArtifactRef {
                sha256: "0".repeat(64),
                byte_length,
                media_type: "application/octet-stream".into(),
                filename: Some(filename.clone()),
            };
            reference
                .validate()
                .map_err(|error| MediaError::InvalidRequest(error.to_string()))?;
        }
        self.inner.store.get(session)?;
        self.ensure_session_owned(session)?;
        let tree = self.inner.artifacts.tree_for(session);
        let temporary = format!(".upload-{}.tmp", uuid::Uuid::now_v7());
        let created = {
            let temporary = temporary.clone();
            let inner = Arc::clone(&self.inner);
            blocking_io::run(move || {
                // A session created in memory is published first: its tree's
                // directory, where the store lives, exists only from then.
                inner.store.persist_buffered_session(session)?;
                let store = inner.artifacts.write_store(session)?;
                let file = store.create_upload_file(&temporary)?;
                Ok::<_, EngineError>((store, file))
            })
            .await
            .map_err(|error| EngineError::ToolFailed(error.to_string()))?
        };
        let (store, file) = created?;
        let upload = Upload {
            tree,
            store,
            temporary,
            file,
            byte_length,
            received: 0,
            hasher: Sha256::new(),
            filename,
            published: false,
        };
        let mut in_progress = self
            .inner
            .uploads
            .in_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let held = in_progress
            .values()
            .filter(|held| held.connection == connection)
            .count();
        if held >= MAX_UPLOADS_PER_CONNECTION {
            // Dropping `upload` removes its temporary file.
            return Err(MediaError::TooManyUploads {
                max_uploads: MAX_UPLOADS_PER_CONNECTION,
            }
            .into());
        }
        let id = UploadId::new(format!("upl_{}", uuid::Uuid::now_v7().simple()))
            .expect("a UUID makes a valid upload ID");
        in_progress.insert(
            id.clone(),
            InProgress {
                connection,
                upload: Arc::new(Mutex::new(upload)),
            },
        );
        Ok(id)
    }

    /// Appends `data` at `offset`, which must be exactly the bytes the
    /// upload holds so far.
    pub async fn upload_chunk(
        &self,
        connection: u64,
        upload: &UploadId,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<(), EngineError> {
        if data.is_empty() || data.len() as u64 > ARTIFACT_CHUNK_BYTES {
            return Err(MediaError::InvalidRequest(format!(
                "a chunk carries 1 to {ARTIFACT_CHUNK_BYTES} bytes"
            ))
            .into());
        }
        let upload = self.connection_upload(connection, upload)?;
        blocking_io::run(move || {
            let mut upload = upload
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if offset != upload.received {
                return Err(MediaError::OffsetMismatch {
                    offset,
                    expected_offset: upload.received,
                }
                .into());
            }
            let received = upload.received + data.len() as u64;
            if received > upload.byte_length {
                return Err(MediaError::SizeMismatch {
                    byte_length: upload.byte_length,
                    received,
                }
                .into());
            }
            upload.file.write_all(&data).map_err(EngineError::from)?;
            upload.hasher.update(&data);
            upload.received = received;
            Ok(())
        })
        .await
        .map_err(|error| EngineError::ToolFailed(error.to_string()))?
    }

    /// Checks the upload's digest, recognizes and validates its content as a
    /// tool attachment is validated, and stores it under a lease. An upload
    /// that is not complete stays in progress; one whose digest or content
    /// is refused is dropped.
    pub async fn finish_upload(
        &self,
        connection: u64,
        id: &UploadId,
        sha256: &str,
    ) -> Result<ArtifactUploadFinishResult, EngineError> {
        let upload = self.connection_upload(connection, id)?;
        let expected = sha256.to_owned();
        let inner = Arc::clone(&self.inner);
        let id = id.clone();
        blocking_io::run(move || finish(&inner, &upload, &id, &expected, Instant::now()))
            .await
            .map_err(|error| EngineError::ToolFailed(error.to_string()))?
    }

    /// Drops one of `connection`'s uploads in progress.
    pub fn cancel_upload(&self, connection: u64, upload: &UploadId) -> Result<(), EngineError> {
        let mut in_progress = self
            .inner
            .uploads
            .in_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match in_progress.get(upload) {
            Some(held) if held.connection == connection => {
                in_progress.remove(upload);
                Ok(())
            }
            _ => Err(MediaError::UploadNotFound.into()),
        }
    }

    /// Drops every upload `connection` has in progress, when it closes.
    pub fn drop_connection_uploads(&self, connection: u64) {
        let dropped = {
            let mut in_progress = self
                .inner
                .uploads
                .in_progress
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let ids = in_progress
                .iter()
                .filter(|(_, held)| held.connection == connection)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| in_progress.remove(&id))
                .collect::<Vec<_>>()
        };
        // Each upload's temporary file goes with its last reference.
        drop(dropped);
    }

    /// Up to `length` bytes from `offset` of artifact `sha256` in the store
    /// of `session`'s tree, its length, and the media type its content is
    /// recognized as.
    pub async fn read_artifact_range(
        &self,
        session: SessionId,
        sha256: &str,
        offset: u64,
        length: u64,
    ) -> Result<(Vec<u8>, u64, String), EngineError> {
        if !is_digest_name_common(sha256) {
            return Err(
                MediaError::InvalidRequest("a digest is 64 lowercase hex digits".into()).into(),
            );
        }
        if length == 0 || length > ARTIFACT_CHUNK_BYTES {
            return Err(MediaError::InvalidRequest(format!(
                "a read is 1 to {ARTIFACT_CHUNK_BYTES} bytes"
            ))
            .into());
        }
        self.inner.store.get(session)?;
        let artifacts = Arc::clone(&self.inner.artifacts);
        let digest = sha256.to_owned();
        let read = blocking_io::run(move || {
            let Some(store) = artifacts.read_store(session)? else {
                return Ok(None);
            };
            match store.read_range(&digest, offset, length) {
                Ok(range) => Ok(Some(range)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        })
        .await
        .map_err(|error| EngineError::ToolFailed(error.to_string()))?;
        let not_found = |reason: &str| MediaError::ArtifactNotFound {
            sha256: sha256.to_owned(),
            reason: reason.to_owned(),
        };
        let range = match read {
            Ok(Some(range)) => range,
            Ok(None) => return Err(not_found("missing").into()),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Err(not_found("its stored copy is damaged").into());
            }
            Err(error) => return Err(error.into()),
        };
        if offset > range.total_bytes {
            return Err(MediaError::InvalidRequest(format!(
                "offset {offset} is past the artifact's {} bytes",
                range.total_bytes
            ))
            .into());
        }
        Ok((
            range.data,
            range.total_bytes,
            content_media_type(&range.head, range.total_bytes).to_owned(),
        ))
    }

    /// Sweeps the finished uploads whose lease ended by `now` and that no
    /// log of their tree references. A tree that is running, or a store
    /// publishing a capture, keeps its leases until a later sweep.
    pub(crate) fn sweep_upload_leases(&self, now: Instant) {
        let mut leases = self
            .inner
            .uploads
            .leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut due = HashMap::<SessionId, Vec<String>>::new();
        for ((tree, digest), expiry) in &leases.expiry {
            if *expiry <= now && !leases.pins.contains_key(&(*tree, digest.clone())) {
                due.entry(*tree).or_default().push(digest.clone());
            }
        }
        if due.is_empty() {
            return;
        }
        let Ok(_publication) = self.inner.artifacts.publication().try_write_owned() else {
            return;
        };
        for (tree, digests) in due {
            if let Err(crate::session::SessionError::Missing(_)) = self.inner.store.get(tree) {
                // The tree is gone, and its store with it.
                for digest in digests {
                    leases.expiry.remove(&(tree, digest));
                }
                continue;
            }
            let live = match self.tree_artifact_references(tree) {
                Ok(Some(live)) => live,
                // The tree runs and may still add a reference: try later.
                Ok(None) => continue,
                Err(error) => {
                    eprintln!("upload lease sweep of tree {tree} failed: {error}");
                    continue;
                }
            };
            let unreferenced = digests
                .iter()
                .filter(|digest| !live.contains(*digest))
                .collect::<Vec<_>>();
            if !unreferenced.is_empty() {
                let removed = self
                    .inner
                    .artifacts
                    .read_store(tree)
                    .and_then(|store| match store {
                        Some(store) => store.remove(unreferenced.iter().copied()).map(|_| ()),
                        None => Ok(()),
                    });
                if let Err(error) = removed {
                    eprintln!("upload lease sweep of tree {tree} failed: {error}");
                    continue;
                }
            }
            for digest in digests {
                leases.expiry.remove(&(tree, digest));
            }
        }
    }

    /// Pins every artifact `content` names in the store of `session`'s tree
    /// and checks that it is there, so no sweep deletes it before the event
    /// naming it is written; the pins last as long as the result.
    pub(crate) fn pin_input_artifacts(
        &self,
        session: SessionId,
        artifacts: &[&ArtifactRef],
    ) -> Result<PinnedArtifacts, EngineError> {
        let tree = self.inner.artifacts.tree_for(session);
        let mut pinned = PinnedArtifacts {
            inner: Arc::downgrade(&self.inner),
            keys: Vec::new(),
        };
        if artifacts.is_empty() {
            return Ok(pinned);
        }
        let store = self.inner.artifacts.read_store(session)?;
        let mut leases = self
            .inner
            .uploads
            .leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for artifact in artifacts {
            let stored = match &store {
                Some(store) => store.holds(&artifact.sha256, artifact.byte_length)?,
                None => false,
            };
            if !stored {
                return Err(MediaError::ArtifactNotFound {
                    sha256: artifact.sha256.clone(),
                    reason: format!("no blob of {} bytes", artifact.byte_length),
                }
                .into());
            }
            let key = (tree, artifact.sha256.clone());
            *leases.pins.entry(key.clone()).or_default() += 1;
            pinned.keys.push(key);
        }
        Ok(pinned)
    }

    /// Admits the file parts of `content`, an input to a run of `model`,
    /// whose accepted media are `accepted` (section 11.4): each names an
    /// artifact of the tree's store whose content is what the part says, of
    /// a kind and media type the model accepts, within its byte and image
    /// limits, and no kind outnumbers its `max_count`. The artifacts stay
    /// pinned for as long as the result lives, which the caller keeps until
    /// the event naming them is written.
    pub(crate) async fn admit_input_media(
        &self,
        session: SessionId,
        content: &UserContent,
        model: &ModelKey,
        accepted: &[InputMedia],
    ) -> Result<PinnedArtifacts, EngineError> {
        let files = content.files().collect::<Vec<_>>();
        let mut counts = BTreeMap::<&str, u32>::new();
        for artifact in &files {
            artifact
                .validate()
                .map_err(|error| MediaError::InvalidRequest(error.to_string()))?;
            let media = accepted_media(artifact, model, accepted)?;
            if artifact.byte_length > media.max_bytes {
                return Err(MediaError::TooLarge {
                    kind: media.kind.as_str().to_owned(),
                    byte_length: artifact.byte_length,
                    max_bytes: media.max_bytes,
                    width: None,
                    height: None,
                    max_dimension: None,
                    max_pixels: None,
                }
                .into());
            }
            *counts.entry(media.kind.as_str()).or_default() += 1;
        }
        for (kind, count) in counts {
            let max_count = accepted
                .iter()
                .find(|media| media.kind.as_str() == kind)
                .map_or(0, |media| media.max_count);
            if count > max_count {
                return Err(MediaError::LimitExceeded {
                    kind: kind.to_owned(),
                    count,
                    max_count,
                }
                .into());
            }
        }
        let pinned = self.pin_input_artifacts(session, &files)?;
        let artifacts = Arc::clone(&self.inner.artifacts);
        let owned = files.into_iter().cloned().collect::<Vec<_>>();
        let accepted = accepted.to_vec();
        let model = model.clone();
        blocking_io::run(move || {
            for artifact in &owned {
                check_stored_media(&artifacts, session, artifact, &model, &accepted)?;
            }
            Ok::<_, EngineError>(())
        })
        .await
        .map_err(|error| EngineError::ToolFailed(error.to_string()))??;
        Ok(pinned)
    }

    /// Finishes `id` as of `now`, for tests that move the clock.
    #[cfg(test)]
    pub(crate) fn finish_upload_at(
        &self,
        connection: u64,
        id: &UploadId,
        sha256: &str,
        now: Instant,
    ) -> Result<ArtifactUploadFinishResult, EngineError> {
        let upload = self.connection_upload(connection, id)?;
        finish(&self.inner, &upload, id, sha256, now)
    }

    fn connection_upload(
        &self,
        connection: u64,
        id: &UploadId,
    ) -> Result<Arc<Mutex<Upload>>, EngineError> {
        self.inner
            .uploads
            .in_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .filter(|held| held.connection == connection)
            .map(|held| Arc::clone(&held.upload))
            .ok_or_else(|| MediaError::UploadNotFound.into())
    }
}

/// The body of [`Engine::finish_upload`], on a blocking thread.
fn finish(
    inner: &Inner,
    upload: &Mutex<Upload>,
    id: &UploadId,
    expected: &str,
    now: Instant,
) -> Result<ArtifactUploadFinishResult, EngineError> {
    let mut upload = upload
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if upload.received != upload.byte_length {
        return Err(MediaError::SizeMismatch {
            byte_length: upload.byte_length,
            received: upload.received,
        }
        .into());
    }
    // From here the upload ends whatever happens: refused content is not
    // retried, and stored content is no longer in progress.
    inner
        .uploads
        .in_progress
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(id);
    let digest = format!("{:x}", upload.hasher.clone().finalize());
    if digest != expected {
        return Err(MediaError::DigestMismatch { actual: digest }.into());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(upload.byte_length).unwrap_or(0));
    upload.file.seek(SeekFrom::Start(0))?;
    upload.file.read_to_end(&mut bytes)?;
    if bytes.len() as u64 != upload.byte_length {
        return Err(MediaError::SizeMismatch {
            byte_length: upload.byte_length,
            received: bytes.len() as u64,
        }
        .into());
    }
    let validated =
        runtime_media::validate_upload(&bytes).map_err(|rejection| match rejection {
            UploadRejection::Unrecognized => MediaError::TypeUnsupported {
                media_type: "application/octet-stream".into(),
                reason: "not a recognized image, PDF, audio, or video file".into(),
            },
            UploadRejection::Malformed { media_type } => MediaError::TypeUnsupported {
                media_type: media_type.into(),
                reason: "the content does not parse as one".into(),
            },
            UploadRejection::TooLarge {
                kind,
                byte_length,
                max_bytes,
                image,
            } => MediaError::TooLarge {
                kind: kind.into(),
                byte_length,
                max_bytes,
                width: image.map(|(width, _)| width),
                height: image.map(|(_, height)| height),
                max_dimension: image.map(|_| runtime_media::MAX_IMAGE_DIMENSION),
                max_pixels: image.map(|_| runtime_media::MAX_IMAGE_PIXELS),
            },
        })?;
    drop(bytes);
    {
        // Publishing under the lease lock: a sweep of an earlier lease of
        // the same content cannot delete what this publishes.
        let mut leases = inner
            .uploads
            .leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        upload
            .store
            .commit_capture(&upload.temporary, &digest, upload.byte_length)?;
        upload.published = true;
        leases.lease((upload.tree, digest.clone()), now + UPLOAD_LEASE);
    }
    Ok(ArtifactUploadFinishResult {
        artifact: ArtifactRef {
            sha256: digest,
            byte_length: upload.byte_length,
            media_type: validated.media_type.into(),
            filename: upload.filename.clone(),
        },
        image: validated
            .image
            .map(|(width, height)| ImageSize { width, height }),
    })
}

/// The entry of `accepted` that takes `artifact`'s kind and media type.
fn accepted_media<'a>(
    artifact: &ArtifactRef,
    model: &ModelKey,
    accepted: &'a [InputMedia],
) -> Result<&'a InputMedia, MediaError> {
    let Some(kind) = runtime_media::media_kind(&artifact.media_type) else {
        return Err(MediaError::TypeUnsupported {
            media_type: artifact.media_type.clone(),
            reason: "not an image, PDF, audio, or video file".into(),
        });
    };
    let Some(media) = accepted.iter().find(|media| media.kind.as_str() == kind) else {
        return Err(MediaError::Unsupported {
            kind: kind.to_owned(),
            model: model.clone(),
        });
    };
    if !media
        .mime_types
        .iter()
        .any(|mime_type| runtime_media::mime_type_matches(mime_type, &artifact.media_type))
    {
        return Err(MediaError::TypeUnsupported {
            media_type: artifact.media_type.clone(),
            reason: format!("model {model} accepts {}", media.mime_types.join(", ")),
        });
    }
    Ok(media)
}

/// Reads `artifact` from the store and checks that its content is what the
/// reference says and fits the model's image limits.
fn check_stored_media(
    artifacts: &super::artifacts::ArtifactRouter,
    session: SessionId,
    artifact: &ArtifactRef,
    model: &ModelKey,
    accepted: &[InputMedia],
) -> Result<(), EngineError> {
    let not_found = |reason: String| MediaError::ArtifactNotFound {
        sha256: artifact.sha256.clone(),
        reason,
    };
    let attachment = cookie_agent_protocol::ToolAttachment {
        mime_type: cookie_agent_protocol::MimeType::new(artifact.media_type.clone())
            .map_err(|error| MediaError::InvalidRequest(error.to_string()))?,
        filename: artifact.filename.clone(),
        byte_length: artifact.byte_length,
        sha256: cookie_agent_protocol::Sha256Digest::new(artifact.sha256.clone())
            .map_err(|error| MediaError::InvalidRequest(error.to_string()))?,
        reference: cookie_agent_protocol::ArtifactReference {
            uri: format!("artifact://sha256/{}", artifact.sha256),
        },
    };
    let bytes = match artifacts.read_verified_attachment(session, &attachment) {
        Ok(bytes) => bytes,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
            ) =>
        {
            return Err(not_found(error.to_string()).into());
        }
        Err(error) => return Err(error.into()),
    };
    let validated =
        runtime_media::validate_upload(&bytes).map_err(|rejection| match rejection {
            UploadRejection::Unrecognized => MediaError::TypeUnsupported {
                media_type: artifact.media_type.clone(),
                reason: "the stored content is not a recognized media file".into(),
            },
            UploadRejection::Malformed { media_type } => MediaError::TypeUnsupported {
                media_type: media_type.into(),
                reason: "the stored content does not parse as one".into(),
            },
            UploadRejection::TooLarge {
                kind,
                byte_length,
                max_bytes,
                image,
            } => MediaError::TooLarge {
                kind: kind.into(),
                byte_length,
                max_bytes,
                width: image.map(|(width, _)| width),
                height: image.map(|(_, height)| height),
                max_dimension: image.map(|_| runtime_media::MAX_IMAGE_DIMENSION),
                max_pixels: image.map(|_| runtime_media::MAX_IMAGE_PIXELS),
            },
        })?;
    if runtime_media::canonical_video_mime_type(validated.media_type)
        != runtime_media::canonical_video_mime_type(&artifact.media_type)
    {
        return Err(not_found(format!(
            "its content is {}, not {}",
            validated.media_type, artifact.media_type
        ))
        .into());
    }
    let media = accepted_media(artifact, model, accepted)?;
    if let Some((width, height)) = validated.image {
        let too_wide = media
            .max_dimension
            .is_some_and(|side| width > side || height > side);
        let too_many = media
            .max_pixels
            .is_some_and(|pixels| u64::from(width) * u64::from(height) > pixels);
        if too_wide || too_many {
            return Err(MediaError::TooLarge {
                kind: media.kind.as_str().to_owned(),
                byte_length: artifact.byte_length,
                max_bytes: media.max_bytes,
                width: Some(width),
                height: Some(height),
                max_dimension: media.max_dimension,
                max_pixels: media.max_pixels,
            }
            .into());
        }
    }
    Ok(())
}

/// The media `model` accepts in user input, as the catalog `runtime`
/// publishes it; none for a model the catalog does not list.
pub(crate) fn catalog_input_media(
    runtime: &crate::PublishedRuntime,
    model: &ModelKey,
) -> Vec<InputMedia> {
    runtime
        .result
        .snapshot
        .models
        .iter()
        .find(|descriptor| &descriptor.key == model)
        .map(|descriptor| descriptor.input_media.clone())
        .unwrap_or_default()
}

/// What `artifact.read` says a blob is: the media type its content is
/// recognized as, `text/plain` when its head is UTF-8, and
/// `application/octet-stream` otherwise.
fn content_media_type(head: &[u8], total_bytes: u64) -> &'static str {
    if let Some(media_type) = runtime_media::recognize_media_type(head, total_bytes) {
        return media_type;
    }
    match std::str::from_utf8(head) {
        Ok(_) => "text/plain",
        // A head cut inside a character is still text.
        Err(error) if error.error_len().is_none() => "text/plain",
        Err(_) => "application/octet-stream",
    }
}
