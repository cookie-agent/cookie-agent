use std::{
    collections::{HashSet, VecDeque},
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::Path,
};

use bytes::Bytes;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactPage {
    pub(crate) content: String,
    pub(crate) next_offset_lines: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ArtifactGcReport {
    pub(crate) deleted: usize,
    pub(crate) retained: usize,
}

const MAX_TRANSITIVE_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024;
const VERIFIED_FILE_CACHE_CAPACITY: usize = 32;
const VERIFIED_BYTES_CACHE_CAPACITY: usize = 8;
const TEMPORARY_ARTIFACT_GRACE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

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

#[cfg(test)]
mod temporary_cleanup_tests {
    use std::time::{Duration, SystemTime};

    use fs2::FileExt as _;

    use super::ArtifactStore;

    #[test]
    fn startup_cleanup_preserves_fresh_temporary_artifacts_and_removes_old_ones() {
        let root = tempfile::tempdir().expect("temporary root");
        let artifacts = root.path().join("artifacts");
        let store = ArtifactStore::open(artifacts.clone()).expect("artifact store");
        drop(store);
        let name = format!(".{}.{}.tmp", "a".repeat(64), uuid::Uuid::now_v7());
        let path = artifacts.join(name);
        std::fs::write(&path, b"in flight").expect("write temporary artifact");

        drop(ArtifactStore::open(artifacts.clone()).expect("reopen with fresh temporary"));
        assert!(path.is_file());

        let old = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        let active = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open temporary");
        active.lock_exclusive().expect("lock active temporary");
        active
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("age temporary");
        drop(ArtifactStore::open(artifacts.clone()).expect("reopen with active old temporary"));
        assert!(path.is_file());

        drop(active);
        // Dropping `active` releases the flock in this process, but any child
        // this test binary forked in the window between `fork` and `exec`
        // still holds a duplicate of the open file description, and an flock
        // lives on the description rather than on the descriptor. The
        // temporary therefore stays locked for a few milliseconds longer than
        // the drop, and `cleanup_temporary_artifacts` skips it with
        // `try_lock_once`. Production semantics are "an abandoned temporary is
        // removed on a later startup", so retry the startup rather than
        // assuming the very first one wins the race.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            drop(ArtifactStore::open(artifacts.clone()).expect("reopen with abandoned temporary"));
            if !path.exists() || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!path.exists());
    }
}

#[cfg(test)]
fn scan_durable_artifact_references(sessions_dir: &Path) -> std::io::Result<HashSet<String>> {
    let mut live = HashSet::new();
    'sessions: for entry in std::fs::read_dir(sessions_dir)? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let file = match File::open(entry.path().join("events.jsonl")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for line in BufReader::new(file).lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue 'sessions,
                Err(error) => return Err(error),
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            collect_artifact_references(&value, &mut live);
        }
    }
    Ok(live)
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
    use rustix::fs::{AtFlags, Dir, Mode, OFlags, fsync, openat, renameat, unlinkat};
    use serde::Serialize;
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use super::{
        ArtifactGcReport, ArtifactPage, VerifiedBytesCache, VerifiedFileCache,
        read_verified_file_paged,
    };

    #[cfg(test)]
    use super::{
        MAX_TRANSITIVE_ARTIFACT_BYTES, expand_transitive_artifact_references,
        scan_durable_artifact_references,
    };

    pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

    #[derive(Debug)]
    pub(crate) struct ArtifactStore {
        #[cfg(test)]
        pub(crate) io_test_hook: super::ArtifactIoTestHook,
        directory_handle: Arc<fs::File>,
        pub(crate) publication: Arc<tokio::sync::RwLock<()>>,
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
                publication: Arc::new(tokio::sync::RwLock::new(())),
                writes: Mutex::new(()),
                verified_reads: Mutex::new(VerifiedFileCache::default()),
                verified_attachment_bytes: Mutex::new(VerifiedBytesCache::default()),
            });
            store.cleanup_temporary_artifacts()?;
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
            if let Some(mut existing) = self.open_existing(&digest)? {
                if hash_file(&mut existing)?.0 != digest {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "artifact digest collision or corrupt retained artifact",
                    ));
                }
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

        #[cfg(test)]
        pub(crate) fn collect_garbage(
            &self,
            sessions_dir: &Path,
            grace: std::time::Duration,
        ) -> std::io::Result<ArtifactGcReport> {
            let Ok(_publication) = self.publication.try_write() else {
                return Ok(ArtifactGcReport::default());
            };
            let mut live = scan_durable_artifact_references(sessions_dir)?;
            expand_transitive_artifact_references(&mut live, |digest| {
                let Some(mut file) = self.open_existing(digest)? else {
                    return Ok(None);
                };
                if file.metadata()?.len() > MAX_TRANSITIVE_ARTIFACT_BYTES {
                    return Ok(None);
                }
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                Ok(Some(bytes))
            })?;
            self.collect_expired(&live, grace)
        }

        /// Collect against a live set the caller assembled: the router merges
        /// root scans, loaded trees and the cross-reference ledger (§5.2).
        pub(crate) fn collect_garbage_with(
            &self,
            live: &std::collections::HashSet<String>,
            grace: std::time::Duration,
        ) -> std::io::Result<ArtifactGcReport> {
            let Ok(_publication) = self.publication.try_write() else {
                return Ok(ArtifactGcReport::default());
            };
            self.collect_expired(live, grace)
        }

        fn collect_expired(
            &self,
            live: &std::collections::HashSet<String>,
            grace: std::time::Duration,
        ) -> std::io::Result<ArtifactGcReport> {
            let _write = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = std::time::SystemTime::now();
            let mut report = ArtifactGcReport::default();
            for digest in directory_names(&self.directory_handle)? {
                if !is_digest_name(&digest) {
                    continue;
                }
                if live.contains(&digest) {
                    report.retained += 1;
                    continue;
                }
                let Some(file) = self.open_existing(&digest)? else {
                    continue;
                };
                let modified = file.metadata()?.modified()?;
                let age = now
                    .duration_since(modified)
                    .unwrap_or(std::time::Duration::ZERO);
                drop(file);
                if age < grace {
                    report.retained += 1;
                    continue;
                }
                let mut verified = self
                    .verified_reads
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                verified.evict(&digest);
                self.verified_attachment_bytes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .evict(&digest);
                match unlinkat(&*self.directory_handle, &digest, AtFlags::empty()) {
                    Ok(()) => report.deleted += 1,
                    Err(error) if error == rustix::io::Errno::NOENT => {}
                    Err(error) => return Err(error.into()),
                }
            }
            if report.deleted > 0 {
                fsync(&*self.directory_handle)?;
            }
            Ok(report)
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

        fn cleanup_temporary_artifacts(&self) -> std::io::Result<()> {
            let now = std::time::SystemTime::now();
            let mut deleted = false;
            for name in directory_names(&self.directory_handle)? {
                if !valid_temporary_artifact_name(&name) {
                    continue;
                }
                let Some(file) = self.open_existing(&name)? else {
                    continue;
                };
                let age = now
                    .duration_since(file.metadata()?.modified()?)
                    .unwrap_or(std::time::Duration::ZERO);
                if age < super::TEMPORARY_ARTIFACT_GRACE {
                    continue;
                }
                match cookie_agent_models::secure_store::try_lock_once(&file) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => continue,
                }
                match unlinkat(&*self.directory_handle, &name, AtFlags::empty()) {
                    Ok(()) => deleted = true,
                    Err(error) if error == rustix::io::Errno::NOENT => {}
                    Err(error) => return Err(error.into()),
                }
            }
            if deleted {
                fsync(&*self.directory_handle)?;
            }
            Ok(())
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

        pub(crate) fn commit_capture(
            &self,
            name: &str,
            capture: &Mutex<fs::File>,
        ) -> std::io::Result<(CapturedArtifact, u64)> {
            #[cfg(test)]
            self.io_test_hook.run("capture_finalize", name)?;
            let _write = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut temporary = capture
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (digest, byte_length, newlines) = hash_file(&mut temporary)?;
            let published_at = std::time::SystemTime::now();
            temporary.set_times(std::fs::FileTimes::new().set_modified(published_at))?;
            temporary.sync_all()?;
            if let Some(mut existing) = self.open_existing(&digest)? {
                if hash_file(&mut existing)?.0 != digest {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "artifact digest collision or corrupt retained artifact",
                    ));
                }
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
                    &digest,
                )?;
                fs2::FileExt::unlock(&*temporary)?;
                self.open_existing(&digest)?
                    .ok_or_else(|| std::io::Error::other("capture artifact disappeared"))?;
            }
            fsync(&*self.directory_handle)?;
            Ok((
                CapturedArtifact {
                    reference: ArtifactReference {
                        uri: format!("artifact://sha256/{digest}"),
                    },
                    sha256: digest,
                    byte_length,
                },
                newlines,
            ))
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

    #[derive(Clone, Debug, Serialize)]
    pub(crate) struct CapturedArtifact {
        pub(crate) reference: ArtifactReference,
        pub(crate) sha256: String,
        pub(crate) byte_length: u64,
    }

    pub(super) fn directory_names(directory: &fs::File) -> std::io::Result<Vec<String>> {
        let mut names = Vec::new();
        let mut entries = Dir::read_from(directory)?;
        for entry in &mut entries {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if matches!(name, b"." | b"..") {
                continue;
            }
            if let Ok(name) = std::str::from_utf8(name) {
                names.push(name.to_owned());
            }
        }
        Ok(names)
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

    #[cfg(test)]
    use std::path::Path;

    use bytes::Bytes;
    use cookie_agent_protocol::{ArtifactReference, ToolAttachment};
    use fs2::FileExt as _;
    use serde::Serialize;
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use super::{
        ArtifactGcReport, ArtifactPage, VerifiedBytesCache, VerifiedFileCache,
        read_verified_file_paged,
    };

    #[cfg(test)]
    use super::{
        MAX_TRANSITIVE_ARTIFACT_BYTES, expand_transitive_artifact_references,
        scan_durable_artifact_references,
    };

    pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

    #[derive(Debug)]
    pub(crate) struct ArtifactStore {
        #[cfg(test)]
        pub(crate) io_test_hook: super::ArtifactIoTestHook,
        directory: PathBuf,
        pub(crate) publication: Arc<tokio::sync::RwLock<()>>,
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
                publication: Arc::new(tokio::sync::RwLock::new(())),
                writes: Mutex::new(()),
                verified_reads: Mutex::new(VerifiedFileCache::default()),
                verified_attachment_bytes: Mutex::new(VerifiedBytesCache::default()),
            });
            store.cleanup_temporary_artifacts()?;
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
            if let Some(mut existing) = self.open_existing(&digest)? {
                if hash_file(&mut existing)?.0 != digest {
                    return Err(invalid(
                        "artifact digest collision or corrupt retained artifact",
                    ));
                }
                existing.set_times(
                    std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()),
                )?;
            } else {
                let temporary_name = format!(".{digest}.{}.tmp", Uuid::now_v7());
                let temporary_path = self.directory.join(&temporary_name);
                let mut temporary = self.create_file(&temporary_name)?;
                let result = (|| {
                    temporary.write_all(content)?;
                    temporary.sync_all()?;
                    drop(temporary);
                    cookie_agent_models::secure_store::replace_windows_path(
                        &temporary_path,
                        &self.directory.join(&digest),
                    )?;
                    let mut installed = self
                        .open_existing(&digest)?
                        .ok_or_else(|| invalid("retained artifact disappeared"))?;
                    if hash_file(&mut installed)?.0 != digest {
                        return Err(invalid("retained artifact failed verification"));
                    }
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

        #[cfg(test)]
        pub(crate) fn collect_garbage(
            &self,
            sessions_dir: &Path,
            grace: std::time::Duration,
        ) -> std::io::Result<ArtifactGcReport> {
            let mut live = scan_durable_artifact_references(sessions_dir)?;
            let Ok(_publication) = self.publication.try_write() else {
                return Ok(ArtifactGcReport::default());
            };
            expand_transitive_artifact_references(&mut live, |digest| {
                let Some(mut file) = self.open_existing(digest)? else {
                    return Ok(None);
                };
                if file.metadata()?.len() > MAX_TRANSITIVE_ARTIFACT_BYTES {
                    return Ok(None);
                }
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                Ok(Some(bytes))
            })?;
            self.collect_expired(&live, grace)
        }

        /// Collect against a live set the caller assembled (§5.2).
        pub(crate) fn collect_garbage_with(
            &self,
            live: &std::collections::HashSet<String>,
            grace: std::time::Duration,
        ) -> std::io::Result<ArtifactGcReport> {
            let Ok(_publication) = self.publication.try_write() else {
                return Ok(ArtifactGcReport::default());
            };
            self.collect_expired(live, grace)
        }

        fn collect_expired(
            &self,
            live: &std::collections::HashSet<String>,
            grace: std::time::Duration,
        ) -> std::io::Result<ArtifactGcReport> {
            let _write = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = std::time::SystemTime::now();
            let mut report = ArtifactGcReport::default();
            for entry in fs::read_dir(&self.directory)? {
                let entry = entry?;
                let digest = entry.file_name().to_string_lossy().into_owned();
                if !is_digest_name(&digest) {
                    continue;
                }
                if live.contains(&digest) {
                    report.retained += 1;
                    continue;
                }
                let modified = entry.metadata()?.modified()?;
                let age = now
                    .duration_since(modified)
                    .unwrap_or(std::time::Duration::ZERO);
                if age < grace {
                    report.retained += 1;
                    continue;
                }
                let mut verified = self
                    .verified_reads
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                verified.evict(&digest);
                self.verified_attachment_bytes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .evict(&digest);
                match fs::remove_file(entry.path()) {
                    Ok(()) => report.deleted += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(report)
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

        fn cleanup_temporary_artifacts(&self) -> std::io::Result<()> {
            let now = std::time::SystemTime::now();
            for entry in fs::read_dir(&self.directory)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if !valid_temporary_artifact_name(&name) {
                    continue;
                }
                let age = now
                    .duration_since(entry.metadata()?.modified()?)
                    .unwrap_or(std::time::Duration::ZERO);
                if age < super::TEMPORARY_ARTIFACT_GRACE {
                    continue;
                }
                let Some(file) = self.open_existing(&name)? else {
                    continue;
                };
                match cookie_agent_models::secure_store::try_lock_once(&file) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => continue,
                }
                match fs::remove_file(entry.path()) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
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

        pub(crate) fn commit_capture(
            &self,
            name: &str,
            capture: &Mutex<fs::File>,
        ) -> std::io::Result<(CapturedArtifact, u64)> {
            #[cfg(test)]
            self.io_test_hook.run("capture_finalize", name)?;
            let _write = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // LockFileEx locks overlap by handle, including handles in this process.
            // Hash through the original handle, refresh and sync its mtime while
            // locked, then unlock only after a final digest exists. This permits
            // immediate verification/preview through a second handle without an
            // old-mtime GC window or a reopened temp-cleanup race.
            let mut temporary = capture
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (digest, byte_length, newlines) = hash_file(&mut temporary)?;
            let published_at = std::time::SystemTime::now();
            temporary.set_times(std::fs::FileTimes::new().set_modified(published_at))?;
            temporary.sync_all()?;
            let temporary_path = self.directory.join(name);
            if let Some(mut existing) = self.open_existing(&digest)? {
                if hash_file(&mut existing)?.0 != digest {
                    return Err(invalid(
                        "artifact digest collision or corrupt retained artifact",
                    ));
                }
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
                    &self.directory.join(&digest),
                )?;
                fs2::FileExt::unlock(&*temporary)?;
                self.open_existing(&digest)?
                    .ok_or_else(|| invalid("capture artifact disappeared"))?;
            }
            Ok((
                CapturedArtifact {
                    reference: ArtifactReference {
                        uri: format!("artifact://sha256/{digest}"),
                    },
                    sha256: digest,
                    byte_length,
                },
                newlines,
            ))
        }

        pub(crate) fn discard_capture(&self, name: &str) {
            if valid_temporary_artifact_name(name) {
                let _ = fs::remove_file(self.directory.join(name));
            }
        }
    }

    #[derive(Clone, Debug, Serialize)]
    pub(crate) struct CapturedArtifact {
        pub(crate) reference: ArtifactReference,
        pub(crate) sha256: String,
        pub(crate) byte_length: u64,
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
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{ArtifactReference, SessionId, ToolAttachment};

/// Keyed placement decision: which root tree directory owns a session's writes.
type TreeResolver = Arc<dyn Fn(SessionId) -> Option<SessionId> + Send + Sync>;

/// The store's own view of one session log — resident tip and durable length —
/// asked for on demand. A sweep needs it to decide whether data a tree load
/// harvested is still current, and only the store knows what its resident logs
/// hold in a buffered writer (§3.3, §5.2).
type LogFingerprintProbe = Arc<dyn Fn(SessionId) -> crate::session::LogFingerprint + Send + Sync>;

/// Is an enumerated path a directory? Only a vanished entry proves it is not;
/// any other stat failure is returned so a garbage-collection sweep cannot read
/// an unprovable entry as "not a session" (§5.2, fail-closed).
fn entry_is_directory(path: &std::path::Path) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Directory holding a tree's own artifacts (`<root>/artifacts`).
pub(crate) const ARTIFACTS_DIR: &str = "artifacts";

/// Child references a lazy tree load harvested, and the proof they are still
/// complete: the [`LogFingerprint`](crate::session::LogFingerprint) of every
/// child log at fold time — resident tip *and* durable length, the same pair a
/// fold is verified against. Durable size alone would not do: a resident append
/// that adds a reference does not change the file until its writer syncs, and a
/// sweep keyed on sizes would keep using a set that misses it (§3.3, §5.2).
#[derive(Clone, Debug)]
struct TreeLiveRefs {
    digests: HashSet<String>,
    child_log_fingerprints: BTreeMap<SessionId, crate::session::LogFingerprint>,
}

/// What a sweep may do with a loaded tree's harvested child references.
enum HarvestedRefs {
    /// Nothing has moved: the harvested set still names every child reference.
    Current(HashSet<String>),
    /// Something durable moved and no child is resident, so scanning the child
    /// logs again is a complete answer.
    Reharvest,
    /// A child of this tree is resident and moved since the harvest. Its newest
    /// records may still be in that log's writer buffer, so no scan of disk can
    /// establish its references: this tree must not be collected at all.
    Unprovable,
}

/// Content-addressed artifacts, each belonging to exactly one root tree
/// (tree-local sessions, D).
///
/// A work-dir router keeps every tree's artifacts in that root's `artifacts/`
/// directory: a session writes to and reads from its own tree only, a fork
/// copies what it references into its new tree, and a sweep collects one tree
/// against that tree's own logs. A flat router — a standalone tool context
/// with no sessions around it — keeps everything in the one directory it was
/// opened on.
pub(crate) struct ArtifactRouter {
    /// Set for a flat router: the one store every session uses.
    flat: Option<Arc<ArtifactStore>>,
    #[cfg(test)]
    flat_directory: Option<PathBuf>,
    /// Directory whose children are the per-root tree directories.
    sessions_dir: PathBuf,
    /// A sweep holds this while a capture publication may still be in flight,
    /// subsuming the per-store lock it replaces.
    publication: Arc<tokio::sync::RwLock<()>>,
    #[cfg(test)]
    io_test_hook: Arc<ArtifactIoTestHook>,
    roots: Mutex<HashMap<SessionId, Arc<ArtifactStore>>>,
    loaded_trees: Mutex<HashSet<SessionId>>,
    /// Per loaded tree: the child references the one bulk fold harvested, plus
    /// the log fingerprints it was taken at (§3.3(b), §5.2).
    tree_live_refs: Mutex<HashMap<SessionId, TreeLiveRefs>>,
    /// Asks the store what a session log looks like right now. Without it the
    /// harvested sets can never be proven current, so they are never reused.
    fingerprint_probe: Mutex<Option<LogFingerprintProbe>>,
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
            loaded_trees: Mutex::new(HashSet::new()),
            tree_live_refs: Mutex::new(HashMap::new()),
            fingerprint_probe: Mutex::new(None),
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
    ) -> std::io::Result<(CapturedArtifact, u64)> {
        self.write_store(session)?.commit_capture(name, capture)
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

    /// Collects each loaded tree against that tree's own logs (tree-local D4).
    ///
    /// Fail-closed per tree: a tree whose live set cannot be established
    /// completely — an unreadable log or `subagents/`, or a resident child whose
    /// newest records may still be buffered — is left untouched, and the other
    /// trees are still collected. Unloaded trees are never collected, because
    /// their children are unknown. A flat router (test contexts only) has no
    /// logs behind it, so nothing in it counts as referenced.
    pub(crate) fn collect_garbage(
        &self,
        grace: std::time::Duration,
    ) -> std::io::Result<ArtifactGcReport> {
        let _publication = match self.publication.try_write() {
            Ok(guard) => guard,
            // A capture is publishing; the next sweep will see its bytes.
            Err(_) => return Ok(ArtifactGcReport::default()),
        };
        if let Some(flat) = &self.flat {
            return flat.collect_garbage_with(&HashSet::new(), grace);
        }
        let mut report = ArtifactGcReport::default();
        let loaded = self
            .loaded_trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for tree in loaded {
            let Some(store) = self.cached_tree_store(tree) else {
                continue;
            };
            let live = match self.tree_live_set(tree, &store) {
                Ok(Some(live)) => live,
                Ok(None) => continue,
                Err(error) => {
                    eprintln!("artifact collection skipped tree {tree}: {error}");
                    continue;
                }
            };
            let tree_report = store.collect_garbage_with(&live, grace)?;
            report.deleted += tree_report.deleted;
            report.retained += tree_report.retained;
        }
        Ok(report)
    }

    /// Digests one tree's logs reference, expanded transitively through that
    /// tree's own artifacts, or `None` when the set cannot be proven complete.
    fn tree_live_set(
        &self,
        tree: SessionId,
        store: &ArtifactStore,
    ) -> std::io::Result<Option<HashSet<String>>> {
        let Some(mut live) = self.scan_tree_references(&self.tree_dir(tree), tree)? else {
            return Ok(None);
        };
        expand_transitive_artifact_references(&mut live, |digest| {
            let Some(mut file) = store.open_existing(digest)? else {
                return Ok(None);
            };
            if file.metadata()?.len() > MAX_TRANSITIVE_ARTIFACT_BYTES {
                return Ok(None);
            }
            use std::io::Read as _;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(Some(bytes))
        })?;
        Ok(Some(live))
    }

    /// The root log plus every child log of a loaded tree, or `None` when a
    /// child's references cannot be established at all.
    ///
    /// Fail-closed: an unreadable `subagents/` hides live child references, so
    /// only a missing directory counts as "no children" and every other error
    /// is returned. A tree whose harvested set may be stale is never guessed at
    /// (§5.2, review F4).
    fn scan_tree_references(
        &self,
        directory: &Path,
        tree: SessionId,
    ) -> std::io::Result<Option<HashSet<String>>> {
        let mut live =
            scan_artifact_references_in_log(&directory.join(crate::session::EVENTS_FILE))?;
        // The one bulk fold already read every child log of a loaded tree; while
        // the store still agrees with that fold, its harvested set *is* the child
        // live set and the sweep opens none of them again (§3.3, §5.2).
        match self.harvested_tree_refs(directory, tree)? {
            HarvestedRefs::Current(installed) => {
                live.extend(installed);
                return Ok(Some(live));
            }
            HarvestedRefs::Unprovable => return Ok(None),
            HarvestedRefs::Reharvest => {}
        }
        let children = match std::fs::read_dir(directory.join(crate::session::SUBAGENTS_DIR)) {
            Ok(children) => children,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Some(live)),
            Err(error) => return Err(error),
        };
        for child in children {
            let path = child?.path();
            if !entry_is_directory(&path)? {
                continue;
            }
            live.extend(scan_artifact_references_in_log(
                &path.join(crate::session::EVENTS_FILE),
            )?);
        }
        Ok(Some(live))
    }

    /// Teach the router which root tree a session belongs to; a session the
    /// resolver does not place (`None`) is treated as its own tree.
    pub(crate) fn install_tree_resolver(&self, resolver: TreeResolver) {
        *self
            .tree_resolver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(resolver);
    }

    /// Teach the router how to ask the store about a session log, which is what
    /// makes a loaded tree's harvested child references reusable at all. A router
    /// without a probe never reuses them (§3.3(b), §5.2).
    pub(crate) fn install_log_fingerprint_probe(&self, probe: LogFingerprintProbe) {
        *self
            .fingerprint_probe
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(probe);
    }

    /// The store's current fingerprint of one log, or `None` when this router has
    /// no way to ask the store.
    fn log_fingerprint_probe(&self) -> Option<LogFingerprintProbe> {
        self.fingerprint_probe
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn tree_of(&self, session: SessionId) -> Option<SessionId> {
        self.tree_resolver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(|resolve| resolve(session))
    }

    /// Gate for artifact garbage collection across every store this router owns.
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

    /// Mark a tree as loaded so its own artifact directory becomes collectable
    /// and its child references join the live set (§5.2).
    pub(crate) fn note_tree_loaded(&self, tree: SessionId) {
        self.loaded_trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(tree);
    }

    /// Installs the child-reference set one bulk tree load harvested, together
    /// with the log fingerprints it was taken at (§3.3(b), §5.2). Called when the
    /// load's products are applied, so a loaded tree always has a set that was
    /// proven against the same bytes.
    pub(crate) fn note_tree_live_refs(
        &self,
        tree: SessionId,
        digests: HashSet<String>,
        child_log_fingerprints: BTreeMap<SessionId, crate::session::LogFingerprint>,
    ) {
        self.tree_live_refs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                tree,
                TreeLiveRefs {
                    digests,
                    child_log_fingerprints,
                },
            );
    }

    /// Whether one loaded tree's harvested child references may still be reused,
    /// re-harvested from disk, or cannot be established at all.
    ///
    /// The validity proof is the store's fold-validity signal: every child must
    /// still have the fingerprint the harvest recorded, and the harvest must
    /// still cover exactly the children that exist. Durable bytes alone are not
    /// the proof — see [`HarvestedRefs::Unprovable`].
    fn harvested_tree_refs(
        &self,
        directory: &Path,
        tree: SessionId,
    ) -> std::io::Result<HarvestedRefs> {
        let installed = self
            .tree_live_refs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&tree)
            .cloned();
        let Some(installed) = installed else {
            return Ok(HarvestedRefs::Reharvest);
        };
        let Some(probe) = self.log_fingerprint_probe() else {
            // Durable bytes are all a router that cannot ask the store would
            // compare, and they are not enough to prove a set current.
            return Ok(HarvestedRefs::Reharvest);
        };
        let durable =
            self.child_log_fingerprints(&directory.join(crate::session::SUBAGENTS_DIR), &probe)?;
        // A child the harvest never saw, and one whose directory is gone, both
        // make the recorded set incomplete; the union reports either.
        let children = installed
            .child_log_fingerprints
            .keys()
            .chain(durable.keys())
            .copied()
            .collect::<BTreeSet<_>>();
        for child in children {
            let current = match durable.get(&child) {
                Some(fingerprint) => *fingerprint,
                // Off disk: only the store can say whether it is still resident.
                None => probe(child),
            };
            if installed.child_log_fingerprints.get(&child) == Some(&current) {
                continue;
            }
            return Ok(if current.resident_tip.is_some() {
                // This process is writing that log and its newest records can
                // still be in the writer's buffer: no scan of the file can
                // complete the live set for this tree.
                HarvestedRefs::Unprovable
            } else {
                HarvestedRefs::Reharvest
            });
        }
        Ok(HarvestedRefs::Current(installed.digests))
    }

    /// Current fingerprint of every child `events.jsonl` under `subagents_dir`,
    /// keyed by child id: durable bytes, plus this process's resident tip.
    /// Fingerprints only — no log content is read. Fail-closed like every other
    /// enumeration here: only a missing directory means "no children".
    fn child_log_fingerprints(
        &self,
        subagents_dir: &Path,
        probe: &LogFingerprintProbe,
    ) -> std::io::Result<BTreeMap<SessionId, crate::session::LogFingerprint>> {
        let mut fingerprints = BTreeMap::new();
        let entries = match std::fs::read_dir(subagents_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(fingerprints),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let path = entry?.path();
            if !entry_is_directory(&path)? {
                continue;
            }
            let Some(id) = path
                .file_name()
                .and_then(|name| name.to_string_lossy().parse::<SessionId>().ok())
            else {
                continue;
            };
            let size = match std::fs::metadata(path.join(crate::session::EVENTS_FILE)) {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => return Err(error),
            };
            fingerprints.insert(
                id,
                crate::session::LogFingerprint {
                    resident_tip: probe(id).resident_tip,
                    durable_len: size,
                },
            );
        }
        Ok(fingerprints)
    }
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

pub(crate) fn scan_artifact_references_in_log(path: &Path) -> std::io::Result<HashSet<String>> {
    use std::io::BufRead as _;

    let mut live = HashSet::new();
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(live),
        Err(error) => return Err(error),
    };
    for line in std::io::BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        };
        // A reference needs the bytes `artifact://` *after* JSON decoding, and
        // any one of them may arrive as a `\uXXXX` escape. So a line is only
        // skippable when it carries neither a literal `artifact` nor a single
        // escape sequence: an encoded URI cannot exist in such a line, and every
        // line that could hold one is parsed. Measured cost of the surviving
        // test, whole log, 200k lines, on this machine:
        //   no prefilter .......... 690 ms
        //   this prefilter ........ 118 ms (96% of lines skipped, zero skipped
        //                                 lines can carry a reference)
        if !line.contains("artifact") && !line.contains("\\u") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        collect_artifact_references(&value, &mut live);
    }
    Ok(live)
}

/// Harvests the same references the durable line scan finds, from envelopes an
/// in-memory fold already read (§3.3(b)). Serializing each envelope reproduces
/// exactly the JSON line on disk, so the folded view and the scanned view of one
/// log cannot disagree about what is live. A value that cannot be serialized is
/// an error: an incomplete live set must never look like an empty one.
pub(crate) fn collect_artifact_references_in_events(
    events: &[cookie_agent_protocol::StoredEvent],
    live: &mut HashSet<String>,
) -> std::io::Result<()> {
    for event in events {
        let value = serde_json::to_value(event).map_err(std::io::Error::other)?;
        collect_artifact_references(&value, live);
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
mod gc_tests {
    use std::time::{Duration, SystemTime};

    use super::ArtifactStore;

    fn age(path: &std::path::Path) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(
            std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
        )
        .unwrap();
    }

    #[test]
    fn garbage_collection_tracks_logs_manifests_grace_and_torn_lines() {
        let root = tempfile::tempdir().unwrap();
        let artifacts_dir = root.path().join("artifacts");
        let sessions_dir = root.path().join("sessions");
        let session_dir = sessions_dir.join(cookie_agent_protocol::SessionId::new_v7().to_string());
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(
            sessions_dir.join(cookie_agent_protocol::SessionId::new_v7().to_string()),
        )
        .unwrap();
        let store = ArtifactStore::open(artifacts_dir.clone()).unwrap();

        let (referenced, referenced_digest) = store.retain(b"referenced").unwrap();
        let (elided, elided_digest) = store.retain(b"elided").unwrap();
        let (persisted_file, persisted_file_digest) = store.retain(b"file").unwrap();
        let (_, unreferenced_digest) = store.retain(b"unreferenced").unwrap();
        let (_, young_digest) = store.retain(b"young").unwrap();
        let (stdout_ref, stdout_digest) = store.retain(b"stdout").unwrap();
        let (stderr_ref, stderr_digest) = store.retain(b"stderr").unwrap();
        let manifest = serde_json::to_vec(&serde_json::json!({
            "title":"bash",
            "streams":{
                "stdout":{"reference":stdout_ref,"sha256":stdout_digest},
                "stderr":{"reference":stderr_ref,"sha256":stderr_digest}
            }
        }))
        .unwrap();
        let (manifest_ref, manifest_digest) = store.retain(&manifest).unwrap();

        for digest in [
            &referenced_digest,
            &unreferenced_digest,
            &elided_digest,
            &persisted_file_digest,
            &stdout_digest,
            &stderr_digest,
            &manifest_digest,
        ] {
            age(&artifacts_dir.join(digest));
        }
        store
            .read_paged(&unreferenced_digest, 0, 1)
            .expect("cache unreferenced artifact");
        let line = serde_json::json!({
            "payload":{
                "result":{"attachments":[{"reference":referenced}]},
                "truncation":{"retained":manifest_ref},
                "tool_output_elided":{"retained":elided},
                "persisted_file":{"source":{"type":"artifact","reference":persisted_file}}
            }
        });
        std::fs::write(
            session_dir.join("events.jsonl"),
            format!("{}\n{{torn", serde_json::to_string(&line).unwrap()),
        )
        .unwrap();

        let report = store
            .collect_garbage(&sessions_dir, Duration::from_secs(60))
            .unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!artifacts_dir.join(unreferenced_digest).exists());
        for digest in [
            referenced_digest,
            elided_digest,
            persisted_file_digest,
            young_digest,
            stdout_digest,
            stderr_digest,
            manifest_digest,
        ] {
            assert!(artifacts_dir.join(digest).exists());
        }
    }

    #[test]
    fn garbage_collection_aborts_before_deletion_for_unreadable_event_log() {
        let root = tempfile::tempdir().unwrap();
        let artifacts_dir = root.path().join("artifacts");
        let sessions_dir = root.path().join("sessions");
        let session_dir = sessions_dir.join(cookie_agent_protocol::SessionId::new_v7().to_string());
        std::fs::create_dir_all(session_dir.join("events.jsonl")).unwrap();
        let store = ArtifactStore::open(artifacts_dir.clone()).unwrap();
        let (_, digest) = store.retain(b"must survive failed scan").unwrap();
        age(&artifacts_dir.join(&digest));

        let error = store
            .collect_garbage(&sessions_dir, Duration::from_secs(60))
            .unwrap_err();
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(artifacts_dir.join(digest).exists());
    }

    #[test]
    fn garbage_collection_continues_when_event_log_disappeared() {
        let root = tempfile::tempdir().unwrap();
        let artifacts_dir = root.path().join("artifacts");
        let sessions_dir = root.path().join("sessions");
        std::fs::create_dir_all(
            sessions_dir.join(cookie_agent_protocol::SessionId::new_v7().to_string()),
        )
        .unwrap();
        let store = ArtifactStore::open(artifacts_dir.clone()).unwrap();
        let (_, digest) = store.retain(b"unreferenced").unwrap();
        age(&artifacts_dir.join(&digest));

        let report = store
            .collect_garbage(&sessions_dir, Duration::from_secs(60))
            .unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!artifacts_dir.join(digest).exists());
    }
}

#[cfg(test)]
mod router_tests;
