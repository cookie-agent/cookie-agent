use std::{
    collections::{HashSet, VecDeque},
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
};

use bytes::Bytes;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactPage {
    pub(crate) content: String,
    pub(crate) next_offset_lines: Option<u64>,
}

const MAX_TRANSITIVE_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024;
const VERIFIED_FILE_CACHE_CAPACITY: usize = 32;
const VERIFIED_BYTES_CACHE_CAPACITY: usize = 8;
/// What the cached attachments of one tree hold at most together, the newest
/// kept whatever its size: an attachment can be 25 MB, and every model
/// request of the tree sends its history's attachments again.
const VERIFIED_BYTES_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Default)]
struct VerifiedFileCache {
    entries: VecDeque<(String, File)>,
    generation: u64,
}

impl VerifiedFileCache {
    fn take(&mut self, digest: &str) -> Option<File> {
        let index = self
            .entries
            .iter()
            .position(|(cached, _)| cached == digest)?;
        self.entries.remove(index).map(|(_, file)| file)
    }

    fn insert(&mut self, digest: String, file: File) {
        let _ = self.take(&digest);
        if self.entries.len() == VERIFIED_FILE_CACHE_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((digest, file));
    }

    fn evict(&mut self, digest: &str) {
        self.generation = self.generation.wrapping_add(1);
        let _ = self.take(digest);
    }

    fn restore(&mut self, digest: &str, file: File, generation: u64) {
        // GC may evict while a checked-out handle is being verified. Do not put
        // a handle back after that invalidation and resurrect a removed blob.
        if self.generation == generation {
            self.insert(digest.to_owned(), file);
        }
    }
}

#[cfg(test)]
type IoHook = std::sync::Arc<dyn Fn(&str, &str) -> std::io::Result<()> + Send + Sync>;

#[cfg(test)]
#[derive(Default)]
pub(crate) struct ArtifactIoTestHook(
    std::sync::Mutex<Option<IoHook>>,
    std::sync::Mutex<Option<std::sync::Arc<ArtifactIoTestHook>>>,
);

#[cfg(test)]
impl std::fmt::Debug for ArtifactIoTestHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ArtifactIoTestHook")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
impl ArtifactIoTestHook {
    pub(crate) fn set(&self, hook: IoHook) {
        *self.0.lock().unwrap() = Some(hook);
    }

    /// Inherit hooks registered on `parent`, which is how a router's registered
    /// gate reaches the stores it opens lazily.
    pub(crate) fn attach_parent(&self, parent: &std::sync::Arc<ArtifactIoTestHook>) {
        *self.1.lock().unwrap() = Some(Arc::clone(parent));
    }

    pub(crate) fn run(&self, operation: &str, key: &str) -> std::io::Result<()> {
        let hook = self.0.lock().unwrap().clone();
        if let Some(hook) = hook {
            return hook(operation, key);
        }
        let parent = self.1.lock().unwrap().clone();
        parent.map_or(Ok(()), |parent| parent.run(operation, key))
    }
}

#[derive(Debug, Default)]
struct VerifiedBytesCache {
    entries: VecDeque<(String, Bytes)>,
}

impl VerifiedBytesCache {
    fn get(&mut self, digest: &str) -> Option<Bytes> {
        let index = self
            .entries
            .iter()
            .position(|(cached, _)| cached == digest)?;
        let entry = self.entries.remove(index)?;
        let bytes = entry.1.clone();
        self.entries.push_back(entry);
        Some(bytes)
    }

    fn insert(&mut self, digest: String, bytes: Bytes) {
        self.evict(&digest);
        self.entries.push_back((digest, bytes));
        let mut total = self
            .entries
            .iter()
            .map(|(_, bytes)| bytes.len())
            .sum::<usize>();
        while self.entries.len() > 1
            && (self.entries.len() > VERIFIED_BYTES_CACHE_CAPACITY
                || total > VERIFIED_BYTES_CACHE_MAX_BYTES)
        {
            if let Some((_, oldest)) = self.entries.pop_front() {
                total -= oldest.len();
            }
        }
    }

    fn evict(&mut self, digest: &str) {
        if let Some(index) = self.entries.iter().position(|(cached, _)| cached == digest) {
            self.entries.remove(index);
        }
    }
}

#[cfg(test)]
mod verified_bytes_cache_tests {
    use bytes::Bytes;

    use super::VerifiedBytesCache;

    #[test]
    fn cache_returns_shared_content_by_digest() {
        let mut cache = VerifiedBytesCache::default();
        let bytes = Bytes::from_static(b"verified attachment");
        cache.insert("a".repeat(64), bytes.clone());

        let cached = cache.get(&"a".repeat(64)).expect("cached bytes");
        assert_eq!(cached, bytes);
        assert_eq!(cached.as_ptr(), bytes.as_ptr());
    }

    #[test]
    fn cache_keeps_the_newest_attachments_within_its_byte_bound() {
        let mut cache = VerifiedBytesCache::default();
        let blob = |megabytes: usize| Bytes::from(vec![0_u8; megabytes * 1024 * 1024]);
        cache.insert("a".repeat(64), blob(30));
        cache.insert("b".repeat(64), blob(30));
        // 90 MiB would pass the 64 MiB bound: the oldest goes.
        cache.insert("c".repeat(64), blob(30));
        assert!(cache.get(&"a".repeat(64)).is_none());
        assert!(cache.get(&"b".repeat(64)).is_some());
        assert!(cache.get(&"c".repeat(64)).is_some());
        // One larger than the bound is still kept, alone.
        cache.insert("d".repeat(64), blob(70));
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.get(&"d".repeat(64)).is_some());
    }
}

const ARTIFACT_URI_PREFIX: &str = "artifact://sha256/";

/// Adds every artifact `value` names, the one scan of stored JSON for what
/// keeps an artifact alive: each `artifact://sha256/<digest>` in its strings,
/// a mention inside longer text included, and each [`ArtifactRef`] object
/// (`sha256`, `byte_length`, `media_type`), such as a user file part. For
/// deciding what to keep, a superset of the references is the safe side.
///
/// [`ArtifactRef`]: cookie_agent_protocol::ArtifactRef
pub(crate) fn collect_artifact_references(value: &serde_json::Value, live: &mut HashSet<String>) {
    match value {
        serde_json::Value::String(text) => {
            for (index, _) in text.match_indices(ARTIFACT_URI_PREFIX) {
                if let Some(digest) = text
                    .get(index + ARTIFACT_URI_PREFIX.len()..)
                    .and_then(|rest| rest.get(..64))
                    .filter(|digest| is_digest_name_common(digest))
                {
                    live.insert(digest.to_owned());
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_artifact_references(value, live);
            }
        }
        serde_json::Value::Object(members) => {
            if let Some(digest) = members
                .get("sha256")
                .and_then(serde_json::Value::as_str)
                .filter(|digest| is_digest_name_common(digest))
                && members.contains_key("byte_length")
                && members.contains_key("media_type")
            {
                live.insert(digest.to_owned());
            }
            for value in members.values() {
                collect_artifact_references(value, live);
            }
        }
        _ => {}
    }
}

pub(crate) fn is_digest_name_common(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn expand_transitive_artifact_references(
    live: &mut HashSet<String>,
    mut read: impl FnMut(&str) -> std::io::Result<Option<Vec<u8>>>,
) -> std::io::Result<()> {
    let mut pending = live.iter().cloned().collect::<VecDeque<_>>();
    let mut inspected = HashSet::new();
    while let Some(digest) = pending.pop_front() {
        if !inspected.insert(digest.clone()) {
            continue;
        }
        let Some(bytes) = read(&digest)? else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let mut nested = HashSet::new();
        collect_artifact_references(&value, &mut nested);
        for digest in nested {
            if live.insert(digest.clone()) {
                pending.push_back(digest);
            }
        }
    }
    Ok(())
}

fn read_verified_file_paged(
    file: &mut File,
    digest: &str,
    offset_lines: u64,
    limit_lines: u64,
    after_first_read: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<ArtifactPage> {
    if limit_lines == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact page limit must be positive",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut after_first_read = Some(after_first_read);
    let mut line_index = 0_u64;
    let mut content = Vec::new();
    let mut read_lines = 0_u64;
    let mut has_more = false;
    let mut line_start = true;
    let mut too_large = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            break;
        }
        let length = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| index + 1);
        let bytes = &buffer[..length];
        let newline = bytes.last() == Some(&b'\n');
        hasher.update(bytes);
        if line_index >= offset_lines {
            if line_index - offset_lines < limit_lines {
                if line_start {
                    read_lines = read_lines.saturating_add(1);
                }
                if content.len().saturating_add(bytes.len())
                    > cookie_agent_protocol::PersistedToolResult::MAX_OUTPUT_BYTES
                {
                    too_large = true;
                } else if !too_large {
                    content.extend_from_slice(bytes);
                }
            } else {
                has_more = true;
            }
        }
        reader.consume(length);
        if let Some(after_first_read) = after_first_read.take() {
            after_first_read()?;
        }
        if newline {
            line_index = line_index.saturating_add(1);
        }
        line_start = newline;
    }
    if format!("{:x}", hasher.finalize()) != digest {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact content does not match its digest",
        ));
    }
    if too_large {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "artifact page exceeds the 2 MiB output limit",
        ));
    }
    Ok(ArtifactPage {
        content: String::from_utf8_lossy(&content).into_owned(),
        next_offset_lines: has_more.then_some(offset_lines.saturating_add(read_lines)),
    })
}

mod platform;

/// One tree's content-addressed blobs. Each is named by the SHA-256 of its
/// content and never changes once published; readers verify the digest.
#[derive(Debug)]
pub(crate) struct ArtifactStore {
    #[cfg(test)]
    pub(crate) io_test_hook: ArtifactIoTestHook,
    directory: platform::Directory,
    verified_reads: std::sync::Mutex<VerifiedFileCache>,
    /// Handles [`ArtifactStore::read_range`] verified once, when it opened
    /// them; ranges of a cached handle are served without hashing again.
    verified_ranges: std::sync::Mutex<VerifiedFileCache>,
    verified_attachment_bytes: std::sync::Mutex<VerifiedBytesCache>,
}

impl ArtifactStore {
    pub(crate) fn open(directory: std::path::PathBuf) -> std::io::Result<std::sync::Arc<Self>> {
        Ok(std::sync::Arc::new(Self {
            #[cfg(test)]
            io_test_hook: ArtifactIoTestHook::default(),
            directory: platform::Directory::at(directory)?,
            verified_reads: std::sync::Mutex::default(),
            verified_ranges: std::sync::Mutex::default(),
            verified_attachment_bytes: std::sync::Mutex::default(),
        }))
    }

    pub(crate) fn retain(
        &self,
        content: &[u8],
    ) -> std::io::Result<(cookie_agent_protocol::ArtifactReference, String)> {
        use std::io::Write as _;

        let digest = sha256_hex(content);
        if !self.holds(&digest, content.len() as u64)? {
            let temporary_name = format!(".{digest}.{}.tmp", uuid::Uuid::now_v7());
            let result = (|| -> std::io::Result<()> {
                self.directory.create(&temporary_name)?.write_all(content)?;
                self.publish(&temporary_name, &digest, content.len() as u64)
            })();
            if result.is_err() {
                let _ = self.directory.remove(&temporary_name);
            }
            result?;
        }
        Ok((artifact_reference(&digest), digest))
    }

    /// Whether a blob of `byte_length` bytes is stored under `digest`. Names
    /// are content addresses, so that blob is the content; a torn one (wrong
    /// length) is replaced, and readers verify digests anyway.
    pub(crate) fn holds(&self, digest: &str, byte_length: u64) -> std::io::Result<bool> {
        Ok(self.directory.len(digest)? == Some(byte_length))
    }

    /// Gives a fully written temporary its content address. There is no lock
    /// and no fsync: the rename is atomic per name, two writers of one digest
    /// write the same bytes, and a blob a crash loses reads as unavailable
    /// rather than wrong.
    fn publish(&self, temporary: &str, digest: &str, byte_length: u64) -> std::io::Result<()> {
        if self.holds(digest, byte_length)? {
            self.directory.remove(temporary)?;
            return Ok(());
        }
        match self.directory.rename(temporary, digest) {
            Ok(()) => Ok(()),
            // A concurrent writer of the same content took the name first, and
            // Windows cannot always replace a blob a reader holds open.
            Err(_) if self.holds(digest, byte_length)? => {
                self.directory.remove(temporary)?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Deletes blobs by digest, returning how many existed.
    pub(crate) fn remove<'a>(
        &self,
        digests: impl IntoIterator<Item = &'a String>,
    ) -> std::io::Result<usize> {
        let mut deleted = 0;
        for digest in digests {
            if !is_digest_name_common(digest) {
                continue;
            }
            self.verified_reads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .evict(digest);
            self.verified_ranges
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .evict(digest);
            self.verified_attachment_bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .evict(digest);
            if self.directory.remove(digest)? {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Hard-links blob `digest` into `into`; a blob this store lacks is
    /// skipped and one `into` already has is kept.
    pub(crate) fn link_to(&self, into: &Self, digest: &str) -> std::io::Result<()> {
        if !is_digest_name_common(digest) {
            return Ok(());
        }
        self.directory.link_to(&into.directory, digest)
    }

    pub(crate) fn open_existing(&self, name: &str) -> std::io::Result<Option<File>> {
        self.directory.open(name)
    }

    pub(crate) fn read_paged(
        &self,
        digest: &str,
        offset_lines: u64,
        limit_lines: u64,
    ) -> std::io::Result<ArtifactPage> {
        self.read_paged_inner(digest, offset_lines, limit_lines, true, || Ok(()))
    }

    #[cfg(test)]
    pub(super) fn read_paged_with_hook(
        &self,
        digest: &str,
        offset_lines: u64,
        limit_lines: u64,
        hook: impl FnOnce() -> std::io::Result<()>,
    ) -> std::io::Result<ArtifactPage> {
        self.read_paged_inner(digest, offset_lines, limit_lines, false, hook)
    }

    fn read_paged_inner(
        &self,
        digest: &str,
        offset_lines: u64,
        limit_lines: u64,
        #[cfg_attr(not(test), allow(unused_variables))] run_read_hook: bool,
        after_first_read: impl FnOnce() -> std::io::Result<()>,
    ) -> std::io::Result<ArtifactPage> {
        if !is_digest_name_common(digest) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid artifact digest",
            ));
        }
        let (cached, generation) = {
            let mut verified = self
                .verified_reads
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            (verified.take(digest), verified.generation)
        };
        let mut file = match cached {
            Some(file) => file,
            None => self.directory.open_reader(digest)?.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "artifact missing")
            })?,
        };
        #[cfg(test)]
        if run_read_hook {
            self.io_test_hook.run("read", digest)?;
        }
        let page = read_verified_file_paged(
            &mut file,
            digest,
            offset_lines,
            limit_lines,
            after_first_read,
        )?;
        self.verified_reads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .restore(digest, file, generation);
        Ok(page)
    }

    /// Up to `length` bytes of blob `digest` from `offset`, its length, and
    /// its first [`MEDIA_SNIFF_BYTES`] for recognizing what it is. The whole
    /// blob is verified against its digest when it is opened; a handle kept
    /// open pins the content it verified.
    ///
    /// [`MEDIA_SNIFF_BYTES`]: cookie_agent_runtime_api::media::MEDIA_SNIFF_BYTES
    pub(crate) fn read_range(
        &self,
        digest: &str,
        offset: u64,
        length: u64,
    ) -> std::io::Result<ArtifactRange> {
        use std::io::Read as _;

        if !is_digest_name_common(digest) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid artifact digest",
            ));
        }
        let (cached, generation) = {
            let mut verified = self
                .verified_ranges
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (verified.take(digest), verified.generation)
        };
        let mut file = match cached {
            Some(file) => file,
            None => {
                let mut file = self.directory.open_reader(digest)?.ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, "artifact missing")
                })?;
                file.seek(SeekFrom::Start(0))?;
                let mut hasher = Sha256::new();
                std::io::copy(&mut file, &mut hasher)?;
                if format!("{:x}", hasher.finalize()) != digest {
                    return Err(invalid("artifact content does not match its digest"));
                }
                file
            }
        };
        let total_bytes = file.metadata()?.len();
        let mut head = Vec::new();
        file.seek(SeekFrom::Start(0))?;
        (&mut file)
            .take(cookie_agent_runtime_api::media::MEDIA_SNIFF_BYTES as u64)
            .read_to_end(&mut head)?;
        let mut data = Vec::new();
        if offset < total_bytes {
            file.seek(SeekFrom::Start(offset))?;
            (&mut file).take(length).read_to_end(&mut data)?;
        }
        self.verified_ranges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .restore(digest, file, generation);
        Ok(ArtifactRange {
            data,
            total_bytes,
            head,
        })
    }

    /// Creates the private file a client's upload streams into.
    pub(crate) fn create_upload_file(&self, name: &str) -> std::io::Result<File> {
        if !valid_temporary_artifact_name(name) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid upload artifact name",
            ));
        }
        self.directory.create(name)
    }

    pub(crate) fn create_capture_file(&self, name: &str) -> std::io::Result<File> {
        #[cfg(test)]
        self.io_test_hook.run("capture_create", name)?;
        if !valid_temporary_artifact_name(name) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid capture artifact name",
            ));
        }
        self.directory.create(name)
    }

    /// Publish the capture file `name` under `digest`, which its writer
    /// hashed while it streamed the `byte_length` bytes in; the file is not
    /// read again.
    pub(crate) fn commit_capture(
        &self,
        name: &str,
        digest: &str,
        byte_length: u64,
    ) -> std::io::Result<cookie_agent_protocol::ArtifactReference> {
        #[cfg(test)]
        self.io_test_hook.run("capture_finalize", name)?;
        self.publish(name, digest, byte_length)?;
        Ok(artifact_reference(digest))
    }

    pub(crate) fn discard_capture(&self, name: &str) {
        if valid_temporary_artifact_name(name) {
            let _ = self.directory.remove(name);
        }
    }

    pub(crate) fn read_verified_attachment(
        &self,
        attachment: &ToolAttachment,
    ) -> std::io::Result<Bytes> {
        use std::io::Read as _;

        if !is_digest_name_common(attachment.sha256.as_str())
            || attachment.reference.uri != format!("artifact://sha256/{}", attachment.sha256)
        {
            return Err(invalid("attachment reference and digest do not match"));
        }
        if let Some(bytes) = self
            .verified_attachment_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(attachment.sha256.as_str())
        {
            if bytes.len() as u64 != attachment.byte_length {
                return Err(invalid("attachment artifact digest or length is corrupt"));
            }
            return Ok(bytes);
        }
        let mut file = self
            .directory
            .open(attachment.sha256.as_str())?
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "attachment artifact is missing",
                )
            })?;
        let capacity = usize::try_from(attachment.byte_length)
            .map_err(|_| invalid("attachment length does not fit in memory"))?;
        let mut bytes = Vec::with_capacity(capacity);
        // One byte past the recorded length is enough to see a longer blob.
        (&mut file)
            .take(attachment.byte_length.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != attachment.byte_length
            || sha256_hex(&bytes) != attachment.sha256.as_str()
        {
            return Err(invalid("attachment artifact digest or length is corrupt"));
        }
        let bytes = Bytes::from(bytes);
        self.verified_attachment_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(attachment.sha256.to_string(), bytes.clone());
        Ok(bytes)
    }
}

/// What [`ArtifactStore::read_range`] read.
#[derive(Debug)]
pub(crate) struct ArtifactRange {
    pub(crate) data: Vec<u8>,
    pub(crate) total_bytes: u64,
    pub(crate) head: Vec<u8>,
}

fn artifact_reference(digest: &str) -> cookie_agent_protocol::ArtifactReference {
    cookie_agent_protocol::ArtifactReference {
        uri: format!("artifact://sha256/{digest}"),
    }
}

fn valid_temporary_artifact_name(name: &str) -> bool {
    if let Some(id) = name
        .strip_prefix(".upload-")
        .and_then(|value| value.strip_suffix(".tmp"))
    {
        return uuid::Uuid::parse_str(id).is_ok();
    }
    if let Some(value) = name
        .strip_prefix(".capture-")
        .and_then(|value| value.strip_suffix(".tmp"))
    {
        let Some((id, stream)) = value.rsplit_once('-') else {
            return false;
        };
        return cookie_agent_protocol::validate_tool_stream_name(stream).is_ok()
            && uuid::Uuid::parse_str(id).is_ok();
    }
    let Some(value) = name
        .strip_prefix('.')
        .and_then(|value| value.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((digest, id)) = value.split_once('.') else {
        return false;
    };
    is_digest_name_common(digest) && uuid::Uuid::parse_str(id).is_ok()
}

fn sha256_hex(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

fn invalid(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod store_tests {
    use super::{ArtifactPage, ArtifactStore};

    #[cfg(unix)]
    #[test]
    fn artifact_store_uses_preexisting_symlinked_directory() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary artifact root");
        let actual = directory.path().join("actual");
        std::fs::create_dir(&actual).expect("actual artifact directory");
        let linked = directory.path().join("linked");
        symlink(&actual, &linked).expect("artifact directory symlink");
        let store = ArtifactStore::open(linked).expect("symlinked artifact store");
        let (_, digest) = store
            .retain(b"existing-path-policy")
            .expect("retain artifact");
        assert!(actual.join(digest).is_file());
    }

    #[test]
    fn uses_preexisting_artifact_directory() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let artifacts = temporary.path().join("artifacts");
        std::fs::create_dir(&artifacts).expect("ordinary artifact directory");
        ArtifactStore::open(artifacts).expect("existing artifact directory");
    }

    #[test]
    fn paged_reads_verify_lazily_and_report_missing_or_corrupt_blobs() {
        let directory = tempfile::tempdir().expect("temporary artifact root");
        let artifacts = directory.path().join("artifacts");
        let store = ArtifactStore::open(artifacts.clone()).expect("open artifact store");
        let (_, digest) = store
            .retain(b"zero\none\ntwo\nthree")
            .expect("retain artifact");
        assert_eq!(
            store.read_paged(&digest, 1, 2).expect("paged read"),
            ArtifactPage {
                content: "one\ntwo\n".into(),
                next_offset_lines: Some(3),
            }
        );
        assert_eq!(
            store.read_paged(&digest, 3, 2).expect("last page"),
            ArtifactPage {
                content: "three".into(),
                next_offset_lines: None,
            }
        );
        assert!(store.read_paged(&"a".repeat(64), 0, 1).is_err());

        std::fs::write(artifacts.join(&digest), b"corrupt").expect("corrupt cached artifact");
        assert_eq!(
            store.read_paged(&digest, 0, 1).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );

        drop(store);
        let reopened = ArtifactStore::open(artifacts).expect("corruption is lazy");
        assert_eq!(
            reopened.read_paged(&digest, 0, 1).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn cached_handle_pins_same_length_replacement_with_preserved_mtime() {
        let directory = tempfile::tempdir().expect("temporary artifact root");
        let artifacts = directory.path().join("artifacts");
        let store = ArtifactStore::open(artifacts.clone()).expect("open artifact store");
        let original = b"verified\ncontent\n";
        let (_, digest) = store.retain(original).expect("retain artifact");
        let path = artifacts.join(&digest);
        let modified = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .expect("artifact mtime");
        assert_eq!(
            store
                .read_paged(&digest, 0, 2)
                .expect("verify artifact")
                .content,
            String::from_utf8_lossy(original)
        );

        let replacement = artifacts.join("replacement");
        std::fs::write(&replacement, vec![b'x'; original.len()]).expect("stage replacement");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&replacement)
            .expect("open replacement")
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .expect("preserve replacement mtime");
        std::fs::rename(&replacement, &path).expect("replace artifact path");

        assert_eq!(
            store
                .read_paged(&digest, 0, 2)
                .expect("read pinned artifact")
                .content,
            String::from_utf8_lossy(original)
        );
    }

    #[cfg(windows)]
    #[test]
    fn cached_handle_pins_same_length_replacement_with_preserved_mtime() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let artifacts = temporary.path().join("artifacts");
        let store = ArtifactStore::open(artifacts.clone()).expect("artifact store");
        let original = b"verified\ncontent\n";
        let (_, digest) = store.retain(original).expect("retain artifact");
        let path = artifacts.join(&digest);
        let modified = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .expect("artifact mtime");
        assert_eq!(
            store
                .read_paged(&digest, 0, 2)
                .expect("verify artifact")
                .content,
            String::from_utf8_lossy(original)
        );

        let replacement = artifacts.join("replacement");
        std::fs::write(&replacement, vec![b'x'; original.len()]).expect("stage replacement");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&replacement)
            .expect("open replacement")
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .expect("preserve replacement mtime");
        // MoveFileExW cannot replace an open destination, even with delete
        // sharing. Rename the pinned source aside, then occupy its vacant path.
        // This simulates path substitution without requiring POSIX replacement.
        cookie_agent_models::secure_store::replace_windows_path(
            &path,
            &artifacts.join("displaced"),
        )
        .expect("move pinned artifact aside");
        cookie_agent_models::secure_store::replace_windows_path(&replacement, &path)
            .expect("replace cached artifact path");
        assert_eq!(
            std::fs::read(&path).expect("read substituted path"),
            vec![b'x'; original.len()]
        );
        assert_eq!(
            std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .expect("substituted path mtime"),
            modified
        );

        assert_eq!(
            store
                .read_paged(&digest, 0, 2)
                .expect("read pinned artifact")
                .content,
            String::from_utf8_lossy(original)
        );
        drop(store);
        let reopened = ArtifactStore::open(artifacts).expect("reopen replaced artifact");
        assert_eq!(
            reopened.read_paged(&digest, 0, 2).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
}

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{ArtifactReference, SessionId, ToolAttachment};

/// Keyed placement decision: which root tree directory owns a session's writes.
type TreeResolver = Arc<dyn Fn(SessionId) -> Option<SessionId> + Send + Sync>;

/// Directory holding a tree's own artifacts (`<root>/artifacts`).
pub(crate) const ARTIFACTS_DIR: &str = "artifacts";

/// Content-addressed artifacts, each belonging to exactly one root tree
/// (tree-local sessions, D).
///
/// A work-dir router keeps every tree's artifacts in that root's `artifacts/`
/// directory: a session writes to and reads from its own tree only, a fork
/// links what it references into its new tree, and a revert deletes from its
/// tree only what the reverted events alone referenced. A flat router — a
/// standalone tool context with no sessions around it — keeps everything in
/// the one directory it was opened on.
pub(crate) struct ArtifactRouter {
    /// Set for a flat router: the one store every session uses.
    flat: Option<Arc<ArtifactStore>>,
    #[cfg(test)]
    flat_directory: Option<PathBuf>,
    /// Directory whose children are the per-root tree directories.
    sessions_dir: PathBuf,
    /// A capture holds this until its terminal event is in the log, so a
    /// revert sweep never deletes output a log is about to reference.
    publication: Arc<tokio::sync::RwLock<()>>,
    #[cfg(test)]
    io_test_hook: Arc<ArtifactIoTestHook>,
    roots: Mutex<HashMap<SessionId, Arc<ArtifactStore>>>,
    /// Maps a session to the root whose directory holds its artifacts (§5.1).
    /// The engine installs this once the store exists; a session no resolver
    /// places is treated as its own tree.
    tree_resolver: Mutex<Option<TreeResolver>>,
}

#[async_trait::async_trait]
impl cookie_agent_runtime_api::ToolArtifacts for ArtifactRouter {
    fn retain(
        &self,
        session: SessionId,
        content: &[u8],
    ) -> Result<(ArtifactReference, String), crate::ToolError> {
        Self::retain(self, session, content)
            .map_err(|error| crate::ToolError::execution(error.to_string()))
    }

    async fn read_page(
        self: Arc<Self>,
        session: SessionId,
        target: cookie_agent_protocol::ArtifactReadTarget,
        offset: u64,
        limit: u64,
    ) -> Result<cookie_agent_runtime_api::ArtifactReadPage, crate::ToolError> {
        super::read_artifact_async(self, session, target, offset, limit).await
    }
}

impl std::fmt::Debug for ArtifactRouter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ArtifactRouter")
            .field("flat", &self.flat.is_some())
            .field("sessions_dir", &self.sessions_dir)
            .finish_non_exhaustive()
    }
}

impl ArtifactRouter {
    /// Work-dir router: root session directories are children of `workdir_dir`
    /// and each keeps its tree's artifacts in its own `artifacts/`.
    pub(crate) fn open(workdir_dir: PathBuf) -> std::io::Result<Arc<Self>> {
        Ok(Self::with_layout(None, workdir_dir))
    }

    /// Router for an open store: a v2 store partitions artifacts by tree (§5.1).
    pub(crate) fn for_store(store: &crate::session::SessionStore) -> std::io::Result<Arc<Self>> {
        Self::open(store.workdir_dir_path().to_path_buf())
    }

    /// Flat router: every session reads and writes the one store at `directory`.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn open_flat(directory: PathBuf) -> std::io::Result<Arc<Self>> {
        let store = ArtifactStore::open(directory.clone())?;
        let sessions_dir = directory
            .parent()
            .map_or_else(|| PathBuf::from("."), PathBuf::from)
            .join(crate::session::SESSIONS_ROOT_DIR);
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut router = Self::with_layout(Some(store), sessions_dir);
        #[cfg(test)]
        {
            let inner = Arc::get_mut(&mut router).expect("a new router is unshared");
            inner.flat_directory = Some(directory);
            if let Some(flat) = &inner.flat {
                flat.io_test_hook.attach_parent(&inner.io_test_hook);
            }
        }
        Ok(router)
    }

    fn with_layout(flat: Option<Arc<ArtifactStore>>, sessions_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            flat,
            #[cfg(test)]
            flat_directory: None,
            sessions_dir,
            publication: Arc::new(tokio::sync::RwLock::new(())),
            #[cfg(test)]
            io_test_hook: Arc::new(ArtifactIoTestHook::default()),
            roots: Mutex::new(HashMap::new()),
            tree_resolver: Mutex::new(None),
        })
    }

    /// The tree a session's artifacts belong to.
    pub(crate) fn tree_for(&self, session: SessionId) -> SessionId {
        self.tree_of(session).unwrap_or(session)
    }

    /// The store a session writes to, created on first write.
    pub(crate) fn write_store(&self, session: SessionId) -> std::io::Result<Arc<ArtifactStore>> {
        match &self.flat {
            Some(flat) => Ok(Arc::clone(flat)),
            None => self.tree_store(self.tree_for(session)),
        }
    }

    /// The store a session reads from, or `None` when its tree has never
    /// stored anything. A read never creates a tree directory, so it cannot
    /// race an unpublished root's publication.
    pub(crate) fn read_store(
        &self,
        session: SessionId,
    ) -> std::io::Result<Option<Arc<ArtifactStore>>> {
        if let Some(flat) = &self.flat {
            return Ok(Some(Arc::clone(flat)));
        }
        let tree = self.tree_for(session);
        if let Some(store) = self.cached_tree_store(tree) {
            return Ok(Some(store));
        }
        match std::fs::metadata(self.tree_dir(tree).join(ARTIFACTS_DIR)) {
            Ok(metadata) if metadata.is_dir() => self.tree_store(tree).map(Some),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Where a session's blob with this digest is stored (placement visibility
    /// for tests and diagnostics).
    #[cfg(test)]
    pub(crate) fn blob_path(&self, session: SessionId, digest: &str) -> PathBuf {
        match &self.flat_directory {
            Some(directory) => directory.join(digest),
            None => self
                .tree_dir(self.tree_for(session))
                .join(ARTIFACTS_DIR)
                .join(digest),
        }
    }

    pub(crate) fn retain(
        &self,
        session: SessionId,
        content: &[u8],
    ) -> std::io::Result<(ArtifactReference, String)> {
        self.write_store(session)?.retain(content)
    }

    pub(crate) fn create_capture_file(
        &self,
        session: SessionId,
        name: &str,
    ) -> std::io::Result<File> {
        self.write_store(session)?.create_capture_file(name)
    }

    pub(crate) fn commit_capture(
        &self,
        session: SessionId,
        name: &str,
        digest: &str,
        byte_length: u64,
    ) -> std::io::Result<ArtifactReference> {
        self.write_store(session)?
            .commit_capture(name, digest, byte_length)
    }

    pub(crate) fn discard_capture(&self, session: SessionId, name: &str) {
        let store = match &self.flat {
            Some(flat) => Arc::clone(flat),
            None => match self.cached_tree_store(self.tree_for(session)) {
                Some(store) => store,
                None => return,
            },
        };
        store.discard_capture(name);
    }

    /// A handle that reads as `session`, for code that walks one session's
    /// history without carrying its ID along; `None` reads nothing.
    pub(crate) fn for_session(&self, session: Option<SessionId>) -> SessionArtifacts<'_> {
        SessionArtifacts {
            router: self,
            session,
        }
    }

    /// Opens a blob of the reading session's own tree; content stored in any
    /// other tree is never found (tree-local D1).
    #[cfg(test)]
    pub(crate) fn open_existing(
        &self,
        session: SessionId,
        digest: &str,
    ) -> std::io::Result<Option<std::fs::File>> {
        match self.read_store(session)? {
            Some(store) => store.open_existing(digest),
            None => Ok(None),
        }
    }

    pub(crate) fn read_paged(
        &self,
        session: SessionId,
        digest: &str,
        offset_lines: u64,
        limit_lines: u64,
    ) -> std::io::Result<ArtifactPage> {
        match self.read_store(session)? {
            Some(store) => store.read_paged(digest, offset_lines, limit_lines),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "artifact missing",
            )),
        }
    }

    pub(crate) fn read_verified_attachment(
        &self,
        session: SessionId,
        attachment: &ToolAttachment,
    ) -> std::io::Result<Bytes> {
        match self.read_store(session)? {
            Some(store) => store.read_verified_attachment(attachment),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "artifact missing",
            )),
        }
    }

    /// Hard-links every artifact `digests` names — and every artifact those
    /// name in turn, such as a named-stream manifest's streams — from
    /// `source`'s tree into the artifact `directory` of a new tree, so a fork
    /// never depends on the tree it came from (tree-local D2). Blobs are
    /// immutable, so both trees share one copy on disk. Content the source no
    /// longer holds is skipped, exactly as a read of it would fail there; any
    /// other failure fails the fork.
    pub(crate) fn link_into(
        &self,
        source: SessionId,
        directory: &Path,
        digests: &HashSet<String>,
    ) -> std::io::Result<()> {
        if self.flat.is_some() || digests.is_empty() {
            return Ok(());
        }
        let Some(from) = self.read_store(source)? else {
            return Ok(());
        };
        let mut all = digests.clone();
        expand_transitive_artifact_references(&mut all, |digest| {
            read_small_artifact(&from, digest)
        })?;
        let into = ArtifactStore::open(directory.to_path_buf())?;
        for digest in &all {
            from.link_to(&into, digest)?;
        }
        Ok(())
    }

    /// Deletes what a revert cut away from `session`'s tree: each artifact
    /// `dropped` names, directly or through a manifest, that `live` — the
    /// references the tree's remaining logs hold — does not keep. Returns how
    /// many blobs were deleted.
    ///
    /// Nothing is deleted while a capture is publishing, because its terminal
    /// event is not in any log yet, and `live` answers `None` to skip the sweep
    /// when it cannot see every reference, such as while the tree runs.
    pub(crate) fn collect_reverted(
        &self,
        session: SessionId,
        mut dropped: HashSet<String>,
        live: impl FnOnce() -> std::io::Result<Option<HashSet<String>>>,
    ) -> std::io::Result<usize> {
        if dropped.is_empty() {
            return Ok(0);
        }
        let Ok(_publication) = self.publication.try_write() else {
            return Ok(0);
        };
        let Some(store) = self.read_store(session)? else {
            return Ok(0);
        };
        let Some(mut live) = live()? else {
            return Ok(0);
        };
        expand_transitive_artifact_references(&mut dropped, |digest| {
            read_small_artifact(&store, digest)
        })?;
        expand_transitive_artifact_references(&mut live, |digest| {
            read_small_artifact(&store, digest)
        })?;
        store.remove(dropped.difference(&live))
    }

    /// Teach the router which root tree a session belongs to; a session the
    /// resolver does not place (`None`) is treated as its own tree.
    pub(crate) fn install_tree_resolver(&self, resolver: TreeResolver) {
        *self
            .tree_resolver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(resolver);
    }

    fn tree_of(&self, session: SessionId) -> Option<SessionId> {
        self.tree_resolver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(|resolve| resolve(session))
    }

    /// Gate for revert collection across every store this router owns.
    pub(crate) fn publication(&self) -> Arc<tokio::sync::RwLock<()>> {
        Arc::clone(&self.publication)
    }

    #[cfg(test)]
    pub(crate) fn io_test_hook(&self) -> &ArtifactIoTestHook {
        &self.io_test_hook
    }

    fn new_store(&self, directory: PathBuf) -> std::io::Result<Arc<ArtifactStore>> {
        let store = ArtifactStore::open(directory)?;
        #[cfg(test)]
        store.io_test_hook.attach_parent(&self.io_test_hook);
        Ok(store)
    }

    fn tree_dir(&self, tree: SessionId) -> PathBuf {
        self.sessions_dir.join(tree.to_string())
    }

    /// The store for one root tree, created on first write.
    pub(crate) fn tree_store(&self, tree: SessionId) -> std::io::Result<Arc<ArtifactStore>> {
        // Initialization creates directories and scans scratch files. Keep it
        // inside the cache lock so concurrent first writers cannot race it.
        let mut roots = self
            .roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(store) = roots.get(&tree) {
            return Ok(Arc::clone(store));
        }
        let store = self.new_store(self.tree_dir(tree).join(ARTIFACTS_DIR))?;
        roots.insert(tree, Arc::clone(&store));
        Ok(store)
    }

    /// Drops the store handle of a tree that is being deleted, closing the
    /// files it keeps open.
    pub(crate) fn forget_tree(&self, tree: SessionId) {
        self.roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&tree);
    }

    fn cached_tree_store(&self, tree: SessionId) -> Option<Arc<ArtifactStore>> {
        self.roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&tree)
            .map(Arc::clone)
    }
}

/// A blob small enough to be a manifest that names other artifacts, or `None`.
fn read_small_artifact(store: &ArtifactStore, digest: &str) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    let Some(mut file) = store.open_existing(digest)? else {
        return Ok(None);
    };
    if file.metadata()?.len() > MAX_TRANSITIVE_ARTIFACT_BYTES {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// An [`ArtifactRouter`] bound to the session whose artifacts are read, so a
/// history walk resolves attachments in that session's tree only.
#[derive(Clone, Copy)]
pub(crate) struct SessionArtifacts<'a> {
    router: &'a ArtifactRouter,
    session: Option<SessionId>,
}

impl SessionArtifacts<'_> {
    pub(crate) fn read_verified_attachment(
        &self,
        attachment: &ToolAttachment,
    ) -> std::io::Result<Bytes> {
        match self.session {
            Some(session) => self.router.read_verified_attachment(session, attachment),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "artifact missing",
            )),
        }
    }
}

/// Adds every artifact the lines of `log` in `seqs` reference
/// ([`collect_artifact_references`]). The lines the log was read from are
/// scanned as they were read, so a record this build skipped, or a member
/// it does not know, keeps what it names alive too.
pub(crate) fn artifact_references_in_log(
    log: &crate::events::EventLog,
    seqs: impl std::ops::RangeBounds<u64>,
    references: &mut HashSet<String>,
) -> std::io::Result<()> {
    log.visit_json(seqs, |value| collect_artifact_references(value, references))
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod verified_read_cache_tests {
    use std::io::Write as _;

    use super::{ArtifactStore, VERIFIED_FILE_CACHE_CAPACITY};

    #[test]
    fn same_length_in_place_rewrite_with_preserved_mtime_is_never_served() {
        let root = tempfile::tempdir().expect("temporary root");
        let artifacts = root.path().join("artifacts");
        let store = ArtifactStore::open(artifacts.clone()).expect("artifact store");
        let original = b"verified\ncontent\n";
        let (_, digest) = store.retain(original).expect("retain artifact");
        let path = artifacts.join(&digest);
        let modified = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .expect("artifact mtime");
        store.read_paged(&digest, 0, 2).expect("verify artifact");

        std::fs::write(&path, vec![b'x'; original.len()]).expect("rewrite artifact");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open rewritten artifact")
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .expect("restore artifact mtime");

        assert_eq!(
            store.read_paged(&digest, 0, 2).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn mutation_during_paged_read_is_never_served() {
        let root = tempfile::tempdir().expect("temporary root");
        let artifacts = root.path().join("artifacts");
        let store = ArtifactStore::open(artifacts.clone()).expect("artifact store");
        let original = format!("first\n{}\n", "a".repeat(128 * 1024));
        let (_, digest) = store.retain(original.as_bytes()).expect("retain artifact");
        let path = artifacts.join(&digest);
        store.read_paged(&digest, 0, 1).expect("verify artifact");

        let replacement = vec![b'x'; original.len()];
        let error = store
            .read_paged_with_hook(&digest, 0, 2, || {
                let mut writer = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&path)?;
                writer.write_all(&replacement)
            })
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn lru_eviction_closes_handle_and_reopens_digest_path() {
        let root = tempfile::tempdir().expect("temporary root");
        let artifacts = root.path().join("artifacts");
        let store = ArtifactStore::open(artifacts.clone()).expect("artifact store");
        let mut digests = Vec::new();
        for index in 0..=VERIFIED_FILE_CACHE_CAPACITY {
            let (_, digest) = store
                .retain(format!("artifact {index}\n").as_bytes())
                .expect("retain artifact");
            digests.push(digest);
        }
        for digest in &digests {
            store.read_paged(digest, 0, 1).expect("cache artifact");
        }

        std::fs::remove_file(artifacts.join(&digests[0])).expect("remove evicted artifact");
        assert_eq!(
            store.read_paged(&digests[0], 0, 1).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
}

#[cfg(test)]
mod router_tests;
