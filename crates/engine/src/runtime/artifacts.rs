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
        if self.entries.len() == VERIFIED_BYTES_CACHE_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((digest, bytes));
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
}

fn collect_artifact_references(value: &serde_json::Value, live: &mut HashSet<String>) {
    match value {
        serde_json::Value::String(value) => {
            if let Some(digest) = artifact_uri_digest(value) {
                live.insert(digest.to_owned());
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_artifact_references(value, live);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                collect_artifact_references(value, live);
            }
        }
        _ => {}
    }
}

fn artifact_uri_digest(value: &str) -> Option<&str> {
    value
        .strip_prefix("artifact://sha256/")
        .filter(|digest| is_digest_name_common(digest))
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

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        io::{Read, Seek, SeekFrom, Write},
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use bytes::Bytes;
    use cookie_agent_protocol::{ArtifactReference, ToolAttachment};
    use fs2::FileExt as _;
    use rustix::fs::{AtFlags, Mode, OFlags, fsync, openat, renameat, unlinkat};
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use super::{ArtifactPage, VerifiedBytesCache, VerifiedFileCache, read_verified_file_paged};

    pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

    #[derive(Debug)]
    pub(crate) struct ArtifactStore {
        #[cfg(test)]
        pub(crate) io_test_hook: super::ArtifactIoTestHook,
        directory_handle: Arc<fs::File>,
        writes: Mutex<()>,
        verified_reads: Mutex<VerifiedFileCache>,
        verified_attachment_bytes: Mutex<VerifiedBytesCache>,
    }

    impl ArtifactStore {
        pub(crate) fn open(directory: PathBuf) -> std::io::Result<Arc<Self>> {
            prepare_private_directory(&directory)?;
            let handle = rustix::fs::open(
                &directory,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let handle = fs::File::from(handle);
            let store = Arc::new(Self {
                #[cfg(test)]
                io_test_hook: super::ArtifactIoTestHook::default(),
                directory_handle: Arc::new(handle),
                writes: Mutex::new(()),
                verified_reads: Mutex::new(VerifiedFileCache::default()),
                verified_attachment_bytes: Mutex::new(VerifiedBytesCache::default()),
            });
            Ok(store)
        }

        pub(crate) fn retain(
            &self,
            content: &[u8],
        ) -> std::io::Result<(ArtifactReference, String)> {
            let digest = sha256_hex(content);
            let _write = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Names are content addresses: a blob of the right length is the
            // content. A torn one is replaced; readers verify digests anyway.
            if let Some(existing) = self.open_existing(&digest)?
                && existing.metadata()?.len() == content.len() as u64
            {
                existing.set_times(
                    std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()),
                )?;
            } else {
                let temporary_name = format!(".{digest}.{}.tmp", Uuid::now_v7());
                let result = (|| -> std::io::Result<()> {
                    let temporary = openat(
                        &*self.directory_handle,
                        &temporary_name,
                        OFlags::WRONLY
                            | OFlags::CREATE
                            | OFlags::EXCL
                            | OFlags::NOFOLLOW
                            | OFlags::CLOEXEC,
                        Mode::from_raw_mode(0o600),
                    )?;
                    let mut temporary = fs::File::from(temporary);
                    temporary.lock_exclusive()?;
                    temporary.write_all(content)?;
                    temporary.sync_all()?;
                    drop(temporary);
                    renameat(
                        &*self.directory_handle,
                        &temporary_name,
                        &*self.directory_handle,
                        &digest,
                    )?;
                    self.open_existing(&digest)?
                        .ok_or_else(|| std::io::Error::other("retained artifact disappeared"))?;
                    fsync(&*self.directory_handle)?;
                    Ok(())
                })();
                if result.is_err() {
                    let _ = unlinkat(&*self.directory_handle, &temporary_name, AtFlags::empty());
                }
                result?;
            }
            Ok((
                ArtifactReference {
                    uri: format!("artifact://sha256/{digest}"),
                },
                digest,
            ))
        }

        /// Deletes blobs by digest, returning how many existed.
        pub(crate) fn remove<'a>(
            &self,
            digests: impl IntoIterator<Item = &'a String>,
        ) -> std::io::Result<usize> {
            let _write = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut deleted = 0;
            for digest in digests {
                if !is_digest_name(digest) {
                    continue;
                }
                self.verified_reads
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .evict(digest);
                self.verified_attachment_bytes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .evict(digest);
                match unlinkat(&*self.directory_handle, digest.as_str(), AtFlags::empty()) {
                    Ok(()) => deleted += 1,
                    Err(rustix::io::Errno::NOENT) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(deleted)
        }

        pub(crate) fn open_existing(&self, name: &str) -> std::io::Result<Option<fs::File>> {
            match openat(
                &*self.directory_handle,
                name,
                OFlags::RDONLY | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(file) => Ok(Some(fs::File::from(file))),
                Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
                Err(error) => Err(error.into()),
            }
        }

        pub(crate) fn read_paged(
            &self,
            digest: &str,
            offset_lines: u64,
            limit_lines: u64,
        ) -> std::io::Result<ArtifactPage> {
            if !is_digest_name(digest) {
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
                None => self.open_existing(digest)?.ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, "artifact missing")
                })?,
            };
            #[cfg(test)]
            self.io_test_hook.run("read", digest)?;
            let page =
                read_verified_file_paged(&mut file, digest, offset_lines, limit_lines, || Ok(()))?;
            self.verified_reads
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .restore(digest, file, generation);
            Ok(page)
        }

        #[cfg(test)]
        pub(super) fn read_paged_with_hook(
            &self,
            digest: &str,
            offset_lines: u64,
            limit_lines: u64,
            hook: impl FnOnce() -> std::io::Result<()>,
        ) -> std::io::Result<ArtifactPage> {
            if !is_digest_name(digest) {
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
                None => self.open_existing(digest)?.ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, "artifact missing")
                })?,
            };
            let page =
                read_verified_file_paged(&mut file, digest, offset_lines, limit_lines, hook)?;
            self.verified_reads
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .restore(digest, file, generation);
            Ok(page)
        }

        pub(crate) fn create_capture_file(&self, name: &str) -> std::io::Result<fs::File> {
            #[cfg(test)]
            self.io_test_hook.run("capture_create", name)?;
            if !valid_temporary_artifact_name(name) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "invalid capture artifact name",
                ));
            }
            let file = openat(
                &*self.directory_handle,
                name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?;
            let file = fs::File::from(file);
            file.lock_exclusive()?;
            Ok(file)
        }

        /// Publish a capture under `digest`, which the writer hashed while it
        /// streamed the `byte_length` bytes in; the file is not read again.
        pub(crate) fn commit_capture(
            &self,
            name: &str,
            capture: &Mutex<fs::File>,
            digest: &str,
            byte_length: u64,
        ) -> std::io::Result<ArtifactReference> {
            #[cfg(test)]
            self.io_test_hook.run("capture_finalize", name)?;
            let _write = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let temporary = capture
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let published_at = std::time::SystemTime::now();
            temporary.set_times(std::fs::FileTimes::new().set_modified(published_at))?;
            temporary.sync_all()?;
            if let Some(existing) = self.open_existing(digest)?
                && existing.metadata()?.len() == byte_length
            {
                existing.set_times(std::fs::FileTimes::new().set_modified(published_at))?;
                existing.sync_all()?;
                fs2::FileExt::unlock(&*temporary)?;
                match unlinkat(&*self.directory_handle, name, AtFlags::empty()) {
                    Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                    Err(error) => return Err(error.into()),
                }
            } else {
                renameat(
                    &*self.directory_handle,
                    name,
                    &*self.directory_handle,
                    digest,
                )?;
                fs2::FileExt::unlock(&*temporary)?;
                self.open_existing(digest)?
                    .ok_or_else(|| std::io::Error::other("capture artifact disappeared"))?;
            }
            fsync(&*self.directory_handle)?;
            Ok(ArtifactReference {
                uri: format!("artifact://sha256/{digest}"),
            })
        }

        pub(crate) fn discard_capture(&self, name: &str) {
            if valid_temporary_artifact_name(name) {
                let _ = unlinkat(&*self.directory_handle, name, AtFlags::empty());
            }
        }

        pub(crate) fn read_verified_attachment(
            &self,
            attachment: &ToolAttachment,
        ) -> std::io::Result<Bytes> {
            if !is_digest_name(attachment.sha256.as_str())
                || attachment.reference.uri != format!("artifact://sha256/{}", attachment.sha256)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "attachment reference and digest do not match",
                ));
            }
            if let Some(bytes) = self
                .verified_attachment_bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(attachment.sha256.as_str())
            {
                if bytes.len() as u64 != attachment.byte_length {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "attachment artifact digest or length is corrupt",
                    ));
                }
                return Ok(bytes);
            }
            let mut file = self
                .open_existing(attachment.sha256.as_str())?
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "attachment artifact is missing",
                    )
                })?;
            let (digest, byte_length, _) = hash_file(&mut file)?;
            if digest != attachment.sha256.as_str() || byte_length != attachment.byte_length {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "attachment artifact digest or length is corrupt",
                ));
            }
            file.seek(SeekFrom::Start(0))?;
            let capacity = usize::try_from(byte_length).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "attachment length does not fit in memory",
                )
            })?;
            let mut bytes = Vec::with_capacity(capacity);
            file.read_to_end(&mut bytes)?;
            let bytes = Bytes::from(bytes);
            self.verified_attachment_bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(attachment.sha256.to_string(), bytes.clone());
            Ok(bytes)
        }
    }

    pub(super) fn hash_file(file: &mut fs::File) -> std::io::Result<(String, u64, u64)> {
        file.seek(SeekFrom::Start(0))?;
        let mut hash = Sha256::new();
        let mut total = 0_u64;
        let mut newlines = 0_u64;
        let mut buffer = [0_u8; 8192];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            total = total.saturating_add(count as u64);
            newlines = newlines.saturating_add(
                buffer[..count]
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count() as u64,
            );
        }
        file.seek(SeekFrom::Start(0))?;
        let digest = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok((digest, total, newlines))
    }

    pub(super) fn valid_temporary_artifact_name(name: &str) -> bool {
        if let Some(value) = name
            .strip_prefix(".capture-")
            .and_then(|value| value.strip_suffix(".tmp"))
        {
            let Some((id, stream)) = value.rsplit_once('-') else {
                return false;
            };
            return cookie_agent_protocol::validate_tool_stream_name(stream).is_ok()
                && Uuid::parse_str(id).is_ok();
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
        is_digest_name(digest) && Uuid::parse_str(id).is_ok()
    }

    pub(super) fn is_digest_name(name: &str) -> bool {
        name.len() == 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    pub(super) fn prepare_private_directory(directory: &Path) -> std::io::Result<()> {
        if !directory.exists() {
            use std::os::unix::fs::DirBuilderExt as _;

            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(directory)?;
        }
        Ok(())
    }

    pub(super) fn sha256_hex(content: &[u8]) -> String {
        Sha256::digest(content)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[cfg(test)]
    mod tests {

        use super::ArtifactStore;

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
        fn paged_reads_verify_lazily_and_report_missing_or_corrupt_blobs() {
            let directory = tempfile::tempdir().expect("temporary artifact root");
            let artifacts = directory.path().join("artifacts");
            let store = ArtifactStore::open(artifacts.clone()).expect("open artifact store");
            let (_, digest) = store
                .retain(b"zero\none\ntwo\nthree")
                .expect("retain artifact");
            assert_eq!(
                store.read_paged(&digest, 1, 2).expect("paged read"),
                super::ArtifactPage {
                    content: "one\ntwo\n".into(),
                    next_offset_lines: Some(3),
                }
            );
            assert_eq!(
                store.read_paged(&digest, 3, 2).expect("last page"),
                super::ArtifactPage {
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
    }
}

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows {
    use std::{
        fs,
        io::{Read, Seek, SeekFrom, Write},
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use bytes::Bytes;
    use cookie_agent_protocol::{ArtifactReference, ToolAttachment};
    use fs2::FileExt as _;
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use super::{ArtifactPage, VerifiedBytesCache, VerifiedFileCache, read_verified_file_paged};

    pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

    #[derive(Debug)]
    pub(crate) struct ArtifactStore {
        #[cfg(test)]
        pub(crate) io_test_hook: super::ArtifactIoTestHook,
        directory: PathBuf,
        writes: Mutex<()>,
        verified_reads: Mutex<VerifiedFileCache>,
        verified_attachment_bytes: Mutex<VerifiedBytesCache>,
    }

    impl ArtifactStore {
        pub(crate) fn open(directory: PathBuf) -> std::io::Result<Arc<Self>> {
            if !directory.exists() {
                cookie_agent_models::secure_store::create_windows_private_dir_all(&directory)?;
            }
            let store = Arc::new(Self {
                #[cfg(test)]
                io_test_hook: super::ArtifactIoTestHook::default(),
                directory,
                writes: Mutex::new(()),
                verified_reads: Mutex::new(VerifiedFileCache::default()),
                verified_attachment_bytes: Mutex::new(VerifiedBytesCache::default()),
            });
            Ok(store)
        }

        pub(crate) fn retain(
            &self,
            content: &[u8],
        ) -> std::io::Result<(ArtifactReference, String)> {
            let digest = sha256_hex(content);
            let _write = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Names are content addresses: a blob of the right length is the
            // content. A torn one is replaced; readers verify digests anyway.
            if let Some(existing) = self.open_existing(&digest)?
                && existing.metadata()?.len() == content.len() as u64
            {
                existing.set_times(
                    std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()),
                )?;
            } else {
                let temporary_name = format!(".{digest}.{}.tmp", Uuid::now_v7());
                let temporary_path = self.directory.join(&temporary_name);
                let mut temporary = self.create_file(&temporary_name)?;
                let result = (|| -> std::io::Result<()> {
                    temporary.write_all(content)?;
                    temporary.sync_all()?;
                    drop(temporary);
                    cookie_agent_models::secure_store::replace_windows_path(
                        &temporary_path,
                        &self.directory.join(&digest),
                    )?;
                    self.open_existing(&digest)?
                        .ok_or_else(|| invalid("retained artifact disappeared"))?;
                    Ok(())
                })();
                if result.is_err() {
                    let _ = fs::remove_file(temporary_path);
                }
                result?;
            }
            Ok((
                ArtifactReference {
                    uri: format!("artifact://sha256/{digest}"),
                },
                digest,
            ))
        }

        /// Deletes blobs by digest, returning how many existed.
        pub(crate) fn remove<'a>(
            &self,
            digests: impl IntoIterator<Item = &'a String>,
        ) -> std::io::Result<usize> {
            let _write = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut deleted = 0;
            for digest in digests {
                if !is_digest_name(digest) {
                    continue;
                }
                self.verified_reads
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .evict(digest);
                self.verified_attachment_bytes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .evict(digest);
                match fs::remove_file(self.directory.join(digest)) {
                    Ok(()) => deleted += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(deleted)
        }

        pub(crate) fn read_verified_attachment(
            &self,
            attachment: &ToolAttachment,
        ) -> std::io::Result<Bytes> {
            if !is_digest_name(attachment.sha256.as_str())
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
                .open_existing(attachment.sha256.as_str())?
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "attachment artifact is missing",
                    )
                })?;
            let (digest, byte_length, _) = hash_file(&mut file)?;
            if digest != attachment.sha256.as_str() || byte_length != attachment.byte_length {
                return Err(invalid("attachment artifact digest or length is corrupt"));
            }
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::with_capacity(
                usize::try_from(byte_length)
                    .map_err(|_| invalid("attachment length does not fit in memory"))?,
            );
            file.read_to_end(&mut bytes)?;
            let bytes = Bytes::from(bytes);
            self.verified_attachment_bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(attachment.sha256.to_string(), bytes.clone());
            Ok(bytes)
        }

        pub(crate) fn open_existing(&self, name: &str) -> std::io::Result<Option<fs::File>> {
            let path = self.directory.join(name);
            match fs::OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => Ok(Some(file)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        }

        pub(crate) fn create_capture_file(&self, name: &str) -> std::io::Result<fs::File> {
            #[cfg(test)]
            self.io_test_hook.run("capture_create", name)?;
            self.create_file(name)
        }

        fn create_file(&self, name: &str) -> std::io::Result<fs::File> {
            let path = self.directory.join(name);
            let file = cookie_agent_models::secure_store::create_windows_private_file(&path)?;
            file.lock_exclusive()?;
            Ok(file)
        }

        pub(crate) fn read_paged(
            &self,
            digest: &str,
            offset_lines: u64,
            limit_lines: u64,
        ) -> std::io::Result<ArtifactPage> {
            if !is_digest_name(digest) {
                return Err(invalid("invalid artifact digest"));
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
                None => self.open_paged(digest)?.ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, "artifact missing")
                })?,
            };
            #[cfg(test)]
            self.io_test_hook.run("read", digest)?;
            let page =
                read_verified_file_paged(&mut file, digest, offset_lines, limit_lines, || Ok(()))?;
            self.verified_reads
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .restore(digest, file, generation);
            Ok(page)
        }

        #[cfg(test)]
        pub(super) fn read_paged_with_hook(
            &self,
            digest: &str,
            offset_lines: u64,
            limit_lines: u64,
            hook: impl FnOnce() -> std::io::Result<()>,
        ) -> std::io::Result<ArtifactPage> {
            if !is_digest_name(digest) {
                return Err(invalid("invalid artifact digest"));
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
                None => self.open_paged(digest)?.ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, "artifact missing")
                })?,
            };
            let page =
                read_verified_file_paged(&mut file, digest, offset_lines, limit_lines, hook)?;
            self.verified_reads
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .restore(digest, file, generation);
            Ok(page)
        }

        fn open_paged(&self, name: &str) -> std::io::Result<Option<fs::File>> {
            use std::os::windows::fs::OpenOptionsExt as _;

            // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE. Pin the file, not
            // its path, so GC can unlink it while a checked-out reader finishes.
            const PAGED_READ_SHARE_MODE: u32 = 0x1 | 0x2 | 0x4;
            let path = self.directory.join(name);
            match fs::OpenOptions::new()
                .read(true)
                .share_mode(PAGED_READ_SHARE_MODE)
                .open(path)
            {
                Ok(file) => Ok(Some(file)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        }

        /// Publish a capture under `digest`, which the writer hashed while it
        /// streamed the `byte_length` bytes in; the file is not read again.
        pub(crate) fn commit_capture(
            &self,
            name: &str,
            capture: &Mutex<fs::File>,
            digest: &str,
            byte_length: u64,
        ) -> std::io::Result<ArtifactReference> {
            #[cfg(test)]
            self.io_test_hook.run("capture_finalize", name)?;
            let _write = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // LockFileEx locks overlap by handle, including handles in this process.
            // Refresh and sync the mtime through the original handle while locked,
            // then unlock only once the blob has its final name. This permits
            // immediate verification/preview through a second handle without an
            // old-mtime GC window or a reopened temp-cleanup race.
            let temporary = capture
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let published_at = std::time::SystemTime::now();
            temporary.set_times(std::fs::FileTimes::new().set_modified(published_at))?;
            temporary.sync_all()?;
            let temporary_path = self.directory.join(name);
            if let Some(existing) = self.open_existing(digest)?
                && existing.metadata()?.len() == byte_length
            {
                existing.set_times(std::fs::FileTimes::new().set_modified(published_at))?;
                existing.sync_all()?;
                fs2::FileExt::unlock(&*temporary)?;
                match fs::remove_file(temporary_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            } else {
                cookie_agent_models::secure_store::replace_windows_path(
                    &temporary_path,
                    &self.directory.join(digest),
                )?;
                fs2::FileExt::unlock(&*temporary)?;
                self.open_existing(digest)?
                    .ok_or_else(|| invalid("capture artifact disappeared"))?;
            }
            Ok(ArtifactReference {
                uri: format!("artifact://sha256/{digest}"),
            })
        }

        pub(crate) fn discard_capture(&self, name: &str) {
            if valid_temporary_artifact_name(name) {
                let _ = fs::remove_file(self.directory.join(name));
            }
        }
    }

    fn hash_file(file: &mut fs::File) -> std::io::Result<(String, u64, u64)> {
        file.seek(SeekFrom::Start(0))?;
        let mut hash = Sha256::new();
        let mut total = 0u64;
        let mut newlines = 0u64;
        let mut buffer = [0u8; 8192];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            total = total.saturating_add(count as u64);
            newlines = newlines.saturating_add(
                buffer[..count]
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count() as u64,
            );
        }
        file.seek(SeekFrom::Start(0))?;
        let digest = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok((digest, total, newlines))
    }

    fn sha256_hex(content: &[u8]) -> String {
        Sha256::digest(content)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn is_digest_name(name: &str) -> bool {
        name.len() == 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    fn valid_temporary_artifact_name(name: &str) -> bool {
        if let Some(value) = name
            .strip_prefix(".capture-")
            .and_then(|value| value.strip_suffix(".tmp"))
        {
            let Some((id, stream)) = value.rsplit_once('-') else {
                return false;
            };
            return cookie_agent_protocol::validate_tool_stream_name(stream).is_ok()
                && Uuid::parse_str(id).is_ok();
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
        is_digest_name(digest) && Uuid::parse_str(id).is_ok()
    }

    fn invalid(message: &'static str) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, message)
    }

    #[cfg(test)]
    mod tests {
        use super::{ArtifactPage, ArtifactStore};

        #[test]
        fn uses_preexisting_artifact_directory_without_acl_validation() {
            let temporary = tempfile::tempdir().expect("temporary root");
            let artifacts = temporary.path().join("artifacts");
            std::fs::create_dir(&artifacts).expect("ordinary artifact directory");
            ArtifactStore::open(artifacts).expect("existing artifact directory");
        }

        #[test]
        fn paged_reads_verify_lazily_and_report_missing_or_corrupt_blobs() {
            let temporary = tempfile::tempdir().expect("temporary root");
            let artifacts = temporary.path().join("artifacts");
            let store = ArtifactStore::open(artifacts.clone()).expect("artifact store");
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
}

#[cfg(windows)]
pub(crate) use windows::*;

use std::{
    collections::HashMap,
    path::PathBuf,
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
/// copies what it references into its new tree, and a revert deletes from its
/// tree only what the reverted events alone referenced. A flat router — a standalone tool context
/// with no sessions around it — keeps everything in the one directory it was
/// opened on.
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
    fn tree_for(&self, session: SessionId) -> SessionId {
        self.tree_of(session).unwrap_or(session)
    }

    /// The store a session writes to, created on first write.
    fn write_store(&self, session: SessionId) -> std::io::Result<Arc<ArtifactStore>> {
        match &self.flat {
            Some(flat) => Ok(Arc::clone(flat)),
            None => self.tree_store(self.tree_for(session)),
        }
    }

    /// The store a session reads from, or `None` when its tree has never
    /// stored anything. A read never creates a tree directory, so it cannot
    /// race an unpublished root's publication.
    fn read_store(&self, session: SessionId) -> std::io::Result<Option<Arc<ArtifactStore>>> {
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
        capture: &Mutex<File>,
        digest: &str,
        byte_length: u64,
    ) -> std::io::Result<ArtifactReference> {
        self.write_store(session)?
            .commit_capture(name, capture, digest, byte_length)
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

    /// Copies every artifact `digests` names — and every artifact those name in
    /// turn, such as a named-stream manifest's streams — from `source`'s tree
    /// into `target`'s, so a fork never depends on the tree it came from
    /// (tree-local D2). Content already present is kept; content the source no
    /// longer holds is skipped, exactly as a read of it would fail there.
    pub(crate) fn copy_into_tree(
        &self,
        source: SessionId,
        target: SessionId,
        digests: HashSet<String>,
    ) -> std::io::Result<()> {
        if self.flat.is_some()
            || self.tree_for(source) == self.tree_for(target)
            || digests.is_empty()
        {
            return Ok(());
        }
        let Some(from) = self.read_store(source)? else {
            return Ok(());
        };
        let read = |digest: &str| -> std::io::Result<Option<Vec<u8>>> {
            use std::io::Read as _;
            let Some(mut file) = from.open_existing(digest)? else {
                return Ok(None);
            };
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(Some(bytes))
        };
        let mut all = digests;
        expand_transitive_artifact_references(&mut all, |digest| {
            let Some(file) = from.open_existing(digest)? else {
                return Ok(None);
            };
            if file.metadata()?.len() > MAX_TRANSITIVE_ARTIFACT_BYTES {
                return Ok(None);
            }
            read(digest)
        })?;
        let into = self.write_store(target)?;
        for digest in all {
            if into.open_existing(&digest)?.is_some() {
                continue;
            }
            if let Some(bytes) = read(&digest)? {
                into.retain(&bytes)?;
            }
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

/// Adds every `artifact://sha256/<digest>` the events mention. The scan runs
/// over each event's serialized JSON, which spells these URIs out literally, so
/// a mention inside longer text counts too: for deciding what to keep, a
/// superset of the references is the safe side.
pub(crate) fn artifact_references_in_events<'a>(
    events: impl IntoIterator<Item = &'a cookie_agent_protocol::StoredEvent>,
    references: &mut HashSet<String>,
) -> std::io::Result<()> {
    const PREFIX: &str = "artifact://sha256/";
    for event in events {
        let line = serde_json::to_string(event).map_err(std::io::Error::other)?;
        for (index, _) in line.match_indices(PREFIX) {
            if let Some(digest) = line.get(index + PREFIX.len()..index + PREFIX.len() + 64)
                && is_digest_name_common(digest)
            {
                references.insert(digest.to_owned());
            }
        }
    }
    Ok(())
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
