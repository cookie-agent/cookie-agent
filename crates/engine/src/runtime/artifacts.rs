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
        drop(ArtifactStore::open(artifacts).expect("reopen with abandoned temporary"));
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
                match file.try_lock_exclusive() {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(_) => continue,
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
                match file.try_lock_exclusive() {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(_) => continue,
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
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
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
/// Workdir-wide store for cross-tree and orphaned content (§5.1).
pub(crate) const SHARED_ARTIFACTS_DIR: &str = "artifacts.shared";
/// Append-only ledger of digests one tree references from another (§5.3).
pub(crate) const CROSS_REFS_FILE: &str = "cross-refs.jsonl";

/// Index entry for a digest written into a root tree or the shared store.
fn location_of(tree: Option<SessionId>) -> ArtifactLocation {
    match tree {
        Some(tree) => ArtifactLocation::Root(tree),
        None => ArtifactLocation::Shared,
    }
}

/// Where the bytes behind an `artifact://sha256/<digest>` URI live.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArtifactLocation {
    Root(SessionId),
    Shared,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct StoredCrossReference {
    digest: String,
    tree: String,
}

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

/// Content-addressed artifacts routed to the tree that wrote them (§5.1).
///
/// Writes always land in the writing session's own root directory, which keeps
/// garbage collection tree-local. Reads resolve a digest through a process-wide
/// index and fall back to a directory-name scan, so a URI that names content
/// stored in another tree still works.
pub(crate) struct ArtifactRouter {
    /// Workdir-wide content that no single tree owns (§5.1).
    shared_directory: PathBuf,
    /// Directory whose children are the per-root tree directories.
    sessions_dir: PathBuf,
    /// Append-only ledger of digests one tree references from another.
    cross_refs_path: PathBuf,
    shared: Arc<ArtifactStore>,
    /// A sweep holds this while a capture publication may still be in flight,
    /// subsuming the per-store lock it replaces.
    publication: Arc<tokio::sync::RwLock<()>>,
    #[cfg(test)]
    io_test_hook: Arc<ArtifactIoTestHook>,
    roots: Mutex<HashMap<SessionId, Arc<ArtifactStore>>>,
    digest_index: Mutex<HashMap<String, ArtifactLocation>>,
    cross_refs: Mutex<HashSet<(String, SessionId)>>,
    /// The durable ledger is missing at least one entry this process proved;
    /// rewritten wholesale before the next sweep trusts it again.
    cross_refs_dirty: AtomicBool,
    #[cfg(test)]
    ledger_append_test_failure: AtomicBool,
    loaded_trees: Mutex<HashSet<SessionId>>,
    /// Per loaded tree: the child references the one bulk fold harvested, plus
    /// the log fingerprints it was taken at (§3.3(b), §5.2).
    tree_live_refs: Mutex<HashMap<SessionId, TreeLiveRefs>>,
    /// Asks the store what a session log looks like right now. Without it the
    /// harvested sets can never be proven current, so they are never reused.
    fingerprint_probe: Mutex<Option<LogFingerprintProbe>>,
    /// Maps a session to the root whose directory holds its writes (§5.1). The
    /// engine installs this once the store exists; unrouted callers share.
    tree_resolver: Mutex<Option<TreeResolver>>,
}

impl std::fmt::Debug for ArtifactRouter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ArtifactRouter")
            .field("shared_directory", &self.shared_directory)
            .field("cross_refs_path", &self.cross_refs_path)
            .field("sessions_dir", &self.sessions_dir)
            .finish_non_exhaustive()
    }
}

impl ArtifactRouter {
    /// Hierarchical (v2) workdir: root session directories, `artifacts.shared/`
    /// and `cross-refs.jsonl` are siblings inside `<data>/sessions/<workdirkey>`.
    pub(crate) fn open(workdir_dir: PathBuf) -> std::io::Result<Arc<Self>> {
        Self::open_layout(
            workdir_dir.clone(),
            workdir_dir.join(SHARED_ARTIFACTS_DIR),
            workdir_dir,
        )
    }

    /// Router for an open store: a v2 store partitions artifacts by tree (§5.1).
    pub(crate) fn for_store(store: &crate::session::SessionStore) -> std::io::Result<Arc<Self>> {
        Self::open(store.workdir_dir_path().to_path_buf())
    }

    /// Router for callers without a store at all: every write lands in the given
    /// directory and nothing is tree-partitioned.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn open_flat(shared_directory: PathBuf) -> std::io::Result<Arc<Self>> {
        let workdir_dir = shared_directory
            .parent()
            .map_or_else(|| PathBuf::from("."), PathBuf::from);
        let sessions_dir = workdir_dir.join(crate::session::SESSIONS_ROOT_DIR);
        Self::open_layout(workdir_dir, shared_directory, sessions_dir)
    }

    fn open_layout(
        workdir_dir: PathBuf,
        shared_directory: PathBuf,
        sessions_dir: PathBuf,
    ) -> std::io::Result<Arc<Self>> {
        let shared = ArtifactStore::open(shared_directory.clone())?;
        #[cfg(test)]
        let io_test_hook = Arc::new(ArtifactIoTestHook::default());
        #[cfg(test)]
        {
            shared.io_test_hook.attach_parent(&io_test_hook);
        }
        let cross_refs_path = workdir_dir.join(CROSS_REFS_FILE);
        // A ledger this process cannot read is not an empty ledger: reads still
        // work, and every sweep re-reads the file and aborts if it stays
        // unreadable, so no blob is freed on the strength of an unknown set.
        let cross_refs = Self::read_cross_refs(&cross_refs_path).unwrap_or_else(|error| {
            eprintln!("cross-reference ledger unreadable: {error}");
            HashSet::new()
        });
        Ok(Arc::new(Self {
            shared_directory,
            cross_refs_path,
            sessions_dir,
            shared,
            publication: Arc::new(tokio::sync::RwLock::new(())),
            #[cfg(test)]
            io_test_hook,
            roots: Mutex::new(HashMap::new()),
            digest_index: Mutex::new(HashMap::new()),
            cross_refs: Mutex::new(cross_refs),
            cross_refs_dirty: AtomicBool::new(false),
            #[cfg(test)]
            ledger_append_test_failure: AtomicBool::new(false),
            loaded_trees: Mutex::new(HashSet::new()),
            tree_live_refs: Mutex::new(HashMap::new()),
            fingerprint_probe: Mutex::new(None),
            tree_resolver: Mutex::new(None),
        }))
    }

    /// Teach the router where a session's writes belong; `None` routes to the
    /// shared store.
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

    /// Store a write targets, plus the tree it belongs to (for the digest index).
    fn write_store(
        &self,
        session: SessionId,
    ) -> std::io::Result<(Arc<ArtifactStore>, Option<SessionId>)> {
        match self.tree_of(session) {
            Some(tree) => Ok((self.tree_store(tree)?, Some(tree))),
            None => Ok((Arc::clone(&self.shared), None)),
        }
    }

    /// Gate for artifact garbage collection across every store this router owns.
    pub(crate) fn publication(&self) -> Arc<tokio::sync::RwLock<()>> {
        Arc::clone(&self.publication)
    }

    /// Where a session's blob with this digest is stored (placement visibility
    /// for tests and diagnostics).
    #[cfg(test)]
    pub(crate) fn blob_path(&self, session: SessionId, digest: &str) -> PathBuf {
        match self.tree_of(session) {
            Some(tree) => self.tree_dir(tree).join(ARTIFACTS_DIR).join(digest),
            None => self.shared_directory.join(digest),
        }
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
        if let Some(store) = self.cached_tree_store(tree) {
            return Ok(store);
        }
        let store = self.new_store(self.tree_dir(tree).join(ARTIFACTS_DIR))?;
        let mut roots = self
            .roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(Arc::clone(
            roots.entry(tree).or_insert_with(|| Arc::clone(&store)),
        ))
    }

    fn cached_tree_store(&self, tree: SessionId) -> Option<Arc<ArtifactStore>> {
        self.roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&tree)
            .map(Arc::clone)
    }

    pub(crate) fn retain(
        &self,
        session: SessionId,
        content: &[u8],
    ) -> std::io::Result<(ArtifactReference, String)> {
        let (store, tree) = self.write_store(session)?;
        let (reference, digest) = store.retain(content)?;
        self.index(tree, &digest);
        Ok((reference, digest))
    }

    pub(crate) fn create_capture_file(
        &self,
        session: SessionId,
        name: &str,
    ) -> std::io::Result<File> {
        let (store, _) = self.write_store(session)?;
        store.create_capture_file(name)
    }

    pub(crate) fn commit_capture(
        &self,
        session: SessionId,
        name: &str,
        capture: &Mutex<File>,
    ) -> std::io::Result<(CapturedArtifact, u64)> {
        let (store, tree) = self.write_store(session)?;
        let (artifact, newlines) = store.commit_capture(name, capture)?;
        self.index(tree, &artifact.sha256);
        Ok((artifact, newlines))
    }

    pub(crate) fn discard_capture(&self, session: SessionId, name: &str) {
        let store = match self.tree_of(session) {
            Some(tree) => match self.cached_tree_store(tree) {
                Some(store) => store,
                None => return,
            },
            None => Arc::clone(&self.shared),
        };
        store.discard_capture(name);
    }

    pub(crate) fn open_existing(&self, digest: &str) -> std::io::Result<Option<std::fs::File>> {
        match self.locate(digest)? {
            Some(location) => self.store_for(location)?.open_existing(digest),
            None => Ok(None),
        }
    }

    pub(crate) fn read_paged(
        &self,
        digest: &str,
        offset_lines: u64,
        limit_lines: u64,
    ) -> std::io::Result<ArtifactPage> {
        match self.locate(digest)? {
            Some(location) => {
                self.store_for(location)?
                    .read_paged(digest, offset_lines, limit_lines)
            }
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "artifact missing",
            )),
        }
    }

    pub(crate) fn read_verified_attachment(
        &self,
        attachment: &ToolAttachment,
    ) -> std::io::Result<Bytes> {
        match self.locate(attachment.sha256.as_str())? {
            Some(location) => self
                .store_for(location)?
                .read_verified_attachment(attachment),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "artifact missing",
            )),
        }
    }

    /// Mark a tree as loaded so its own artifact directory becomes collectable
    /// and its child references join the live set (§5.2).
    pub(crate) fn note_tree_loaded(&self, tree: SessionId) {
        self.loaded_trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(tree);
    }

    pub(crate) fn is_tree_loaded(&self, tree: SessionId) -> bool {
        self.loaded_trees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&tree)
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

    /// Collect expired blobs from the shared store and from every tree whose
    /// load completed in this process (§5.2). Trees that were never loaded
    /// contribute their durable references but keep their blobs in place.
    ///
    /// Fail-closed: if the live set cannot be established completely, the
    /// `Err` returns before any store is swept, so nothing is deleted. Proving a
    /// blob unreferenced requires having seen every log that could reference it,
    /// which includes a loaded tree whose child references this process can no
    /// longer prove are durable — that tree holds the whole sweep, not just its
    /// own directory (§5.2, review F4).
    pub(crate) fn collect_garbage(
        &self,
        grace: std::time::Duration,
    ) -> std::io::Result<ArtifactGcReport> {
        let _publication = match self.publication.try_write() {
            Ok(guard) => guard,
            // A capture is publishing; the next sweep will see its bytes.
            Err(_) => return Ok(ArtifactGcReport::default()),
        };
        let (live, unprovable) = self.live_references_and_unprovable()?;
        if !unprovable.is_empty() {
            // Fail closed the way an unreadable log already does: one tree whose
            // child references cannot be established makes the live set
            // incomplete for every store this sweep consults, so nothing is
            // deleted anywhere and the next sweep — once that log's records are
            // durable, or the child has been evicted — tries again (§5.2).
            return Ok(ArtifactGcReport::default());
        }
        let mut report = self.shared.collect_garbage_with(&live, grace)?;
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
            let tree_report = store.collect_garbage_with(&live, grace)?;
            report.deleted += tree_report.deleted;
            report.retained += tree_report.retained;
        }
        Ok(report)
    }

    /// Digests referenced by session logs and by the cross-tree ledger, expanded
    /// transitively through artifact contents (§5.2). Every session directory is
    /// scanned; a tree's `subagents/` logs join the set only once that tree has
    /// been loaded, because until then its children are unknown.
    ///
    /// An `Err` means the live set is *incomplete*, which the caller must never
    /// treat as an empty one: every enumeration failure aborts the sweep before
    /// a single blob is unlinked.
    /// [`Self::live_references`], plus the loaded trees whose child references
    /// this pass could not establish. The set is shared by every store a sweep
    /// consults, so an incomplete one is not safe for any of them (§5.2).
    fn live_references_and_unprovable(
        &self,
    ) -> std::io::Result<(HashSet<String>, HashSet<SessionId>)> {
        // A previous append may have failed, so repair the ledger before reading
        // it back into the live set (B2).
        self.rebuild_cross_ref_ledger_if_dirty();
        let mut live = HashSet::new();
        let mut unprovable = HashSet::new();
        for directory in self.session_directories()? {
            let tree = directory
                .file_name()
                .and_then(|name| name.to_string_lossy().parse::<SessionId>().ok());
            let referenced = self.scan_tree_references(&directory, tree, &mut unprovable)?;
            if let Some(tree) = tree {
                self.note_foreign_references(tree, &referenced);
            }
            live.extend(referenced);
        }
        live.extend(self.cross_reference_digests()?);
        expand_transitive_artifact_references(&mut live, |digest| {
            let Some(mut file) = self.open_existing(digest)? else {
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
        Ok((live, unprovable))
    }

    /// Every root session directory under the sessions root.
    ///
    /// Fail-closed: only a genuinely absent sessions root means "no sessions".
    /// Any other read or per-entry failure is returned, because a partial list
    /// would make unreachable-looking blobs look dead (§5.2).
    fn session_directories(&self) -> std::io::Result<Vec<PathBuf>> {
        let mut directories = Vec::new();
        let entries = match std::fs::read_dir(&self.sessions_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(directories),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let path = entry?.path();
            if entry_is_directory(&path)? {
                directories.push(path);
            }
        }
        directories.sort();
        Ok(directories)
    }

    /// Session log plus, for a loaded tree, every child log in its `subagents/`.
    ///
    /// Fail-closed for a loaded tree: an unreadable `subagents/` hides live
    /// child references, so only a missing directory counts as "no children" and
    /// every other error aborts the sweep. A tree whose child references cannot
    /// be established at all is reported in `unprovable` instead of being
    /// guessed at: its harvested set is never reused when it may be stale
    /// (§5.2, review F4).
    fn scan_tree_references(
        &self,
        directory: &Path,
        tree: Option<SessionId>,
        unprovable: &mut HashSet<SessionId>,
    ) -> std::io::Result<HashSet<String>> {
        let mut live =
            scan_artifact_references_in_log(&directory.join(crate::session::EVENTS_FILE))?;
        let Some(tree) = tree.filter(|tree| self.is_tree_loaded(*tree)) else {
            return Ok(live);
        };
        // The one bulk fold already read every child log of a loaded tree; while
        // the store still agrees with that fold, its harvested set *is* the child
        // live set and the sweep opens none of them again (§3.3, §5.2).
        match self.harvested_tree_refs(directory, tree)? {
            HarvestedRefs::Current(installed) => {
                live.extend(installed);
                return Ok(live);
            }
            HarvestedRefs::Unprovable => {
                unprovable.insert(tree);
                return Ok(live);
            }
            HarvestedRefs::Reharvest => {}
        }
        let children = match std::fs::read_dir(directory.join(crate::session::SUBAGENTS_DIR)) {
            Ok(children) => children,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(live),
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
        Ok(live)
    }

    /// A tree referencing content another tree stored is recorded so that
    /// owner's collection can only ever retain it (§5.3).
    fn note_foreign_references(&self, tree: SessionId, referenced: &HashSet<String>) {
        let foreign = {
            let index = self
                .digest_index
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            referenced
                .iter()
                .filter_map(|digest| match index.get(digest) {
                    Some(ArtifactLocation::Root(owner)) if *owner != tree => Some(digest.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        if foreign.is_empty() {
            return;
        }
        let mut cross_refs = self
            .cross_refs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut lines = String::new();
        for digest in foreign {
            if cross_refs.insert((digest.clone(), tree)) {
                let entry = StoredCrossReference {
                    digest,
                    tree: tree.to_string(),
                };
                let Ok(line) = serde_json::to_string(&entry) else {
                    continue;
                };
                lines.push_str(&line);
                lines.push('\n');
            }
        }
        drop(cross_refs);
        if lines.is_empty() {
            return;
        }
        use std::io::Write as _;
        let path = self.cross_refs_path.clone();
        let mut file = match std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
        {
            Ok(file) => file,
            // The ledger is rebuildable from scans, so a failed append costs a
            // later scan, not data.
            Err(error) => {
                eprintln!("cross-reference ledger append failed: {error}");
                self.cross_refs_dirty.store(true, Ordering::Relaxed);
                return;
            }
        };
        #[cfg(test)]
        if self
            .ledger_append_test_failure
            .swap(false, Ordering::Relaxed)
        {
            drop(file);
            eprintln!("cross-reference ledger append failed: injected test failure");
            self.cross_refs_dirty.store(true, Ordering::Relaxed);
            return;
        }
        // Deliberately unsynced: the file is a hint, never the source of truth.
        // A short or failed write can leave a torn tail, so the whole file is
        // rewritten from the in-memory set before the next sweep reads it.
        if let Err(error) = file.write_all(lines.as_bytes()).and_then(|()| file.flush()) {
            eprintln!("cross-reference ledger append failed: {error}");
            self.cross_refs_dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Rewrite `cross-refs.jsonl` from everything this process has proven, after
    /// an append was lost. Retain-only, so a rebuilt ledger can never free a blob
    /// the old one kept alive (B2, §5.3).
    fn rebuild_cross_ref_ledger_if_dirty(&self) {
        if !self
            .cross_refs_dirty
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let entries = self
            .cross_refs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if let Err(error) = write_cross_ref_ledger(&self.cross_refs_path, &entries) {
            eprintln!("cross-reference ledger rebuild failed: {error}");
            // Keep the flag set: the next sweep retries until the file is whole.
            self.cross_refs_dirty.store(true, Ordering::Relaxed);
        }
    }

    /// The durable ledger, as this process last wrote it.
    #[cfg(test)]
    pub(crate) fn cross_ref_ledger(&self) -> BTreeSet<(String, SessionId)> {
        self.cross_refs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Test seam: the next ledger append is lost, exactly as a short write or a
    /// full disk would lose it (B2).
    #[cfg(test)]
    pub(crate) fn fail_next_ledger_append(&self) {
        self.ledger_append_test_failure
            .store(true, Ordering::Relaxed);
    }

    /// Read the whole ledger. A missing file is an empty ledger; a file that is
    /// there but unreadable is not, so only `NotFound` is absorbed.
    fn read_cross_refs(path: &Path) -> std::io::Result<HashSet<(String, SessionId)>> {
        use std::io::BufRead as _;

        let mut refs = HashSet::new();
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(refs),
            Err(error) => return Err(error),
        };
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { continue };
            let Ok(entry) = serde_json::from_str::<StoredCrossReference>(&line) else {
                continue;
            };
            if let Ok(tree) = entry.tree.parse::<SessionId>() {
                refs.insert((entry.digest, tree));
            }
        }
        Ok(refs)
    }

    /// Every digest the ledger holds plus every one this process proved since it
    /// was last read. The file is re-read per sweep because another process may
    /// have appended, and because an unreadable ledger must abort the sweep
    /// rather than silently drop the cross-tree references it protects (§5.3).
    fn cross_reference_digests(&self) -> std::io::Result<HashSet<String>> {
        let mut digests = Self::read_cross_refs(&self.cross_refs_path)?
            .into_iter()
            .map(|(digest, _)| digest)
            .collect::<HashSet<_>>();
        digests.extend(
            self.cross_refs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .map(|(digest, _)| digest.clone()),
        );
        Ok(digests)
    }

    fn index(&self, tree: Option<SessionId>, digest: &str) {
        self.digest_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(digest.to_owned(), location_of(tree));
    }

    /// Which store holds a digest: the write index first, then a directory-name
    /// scan of the shared store and every known tree (§5.3). Hits are cached, so
    /// an external write into a tree directory is still found without a restart.
    fn locate(&self, digest: &str) -> std::io::Result<Option<ArtifactLocation>> {
        if !is_digest_name_common(digest) {
            return Ok(None);
        }
        if let Some(location) = self
            .digest_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(digest)
            .copied()
        {
            return Ok(Some(location));
        }
        let mut candidates = vec![ArtifactLocation::Shared];
        candidates.extend(
            self.roots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .keys()
                .copied()
                .map(ArtifactLocation::Root),
        );
        candidates.extend(
            self.session_directories()?
                .into_iter()
                .filter_map(|directory| {
                    directory
                        .file_name()
                        .and_then(|name| name.to_string_lossy().parse::<SessionId>().ok())
                })
                .map(ArtifactLocation::Root),
        );
        for location in candidates {
            if !self.blob_present(location, digest)? {
                continue;
            }
            self.digest_index
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(digest.to_owned(), location);
            return Ok(Some(location));
        }
        Ok(None)
    }

    /// Existence by name only; content is verified when the blob is read.
    fn blob_present(&self, location: ArtifactLocation, digest: &str) -> std::io::Result<bool> {
        match location {
            ArtifactLocation::Shared => self.shared.open_existing(digest),
            ArtifactLocation::Root(tree) => match self.cached_tree_store(tree) {
                Some(store) => {
                    return store.open_existing(digest).map(|file| file.is_some());
                }
                None => {
                    let path = self.tree_dir(tree).join(ARTIFACTS_DIR).join(digest);
                    return Ok(match std::fs::metadata(path) {
                        Ok(metadata) => metadata.is_file(),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                        Err(error) => return Err(error),
                    });
                }
            },
        }
        .map(|file| file.is_some())
    }

    fn store_for(&self, location: ArtifactLocation) -> std::io::Result<Arc<ArtifactStore>> {
        match location {
            ArtifactLocation::Shared => Ok(Arc::clone(&self.shared)),
            ArtifactLocation::Root(tree) => self.tree_store(tree),
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

/// Rewrite the cross-reference ledger wholesale when an append was lost;
/// steady-state collection appends to the file.
pub(crate) fn write_cross_ref_ledger(
    path: &Path,
    entries: &BTreeSet<(String, SessionId)>,
) -> std::io::Result<()> {
    use std::io::Write as _;

    // Unique per call: a fixed name left behind by a crash between the sync and
    // the rename would fail `create_new` for every later write.
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| CROSS_REFS_FILE.to_owned());
    let temporary = match parent {
        Some(parent) => parent.join(format!(".{name}.{}.tmp", uuid::Uuid::now_v7())),
        None => PathBuf::from(format!(".{name}.{}.tmp", uuid::Uuid::now_v7())),
    };
    let written: std::io::Result<()> = (|| {
        let mut file = std::io::BufWriter::new(
            std::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&temporary)?,
        );
        for (digest, tree) in entries {
            serde_json::to_writer(
                &mut file,
                &StoredCrossReference {
                    digest: digest.clone(),
                    tree: tree.to_string(),
                },
            )
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        file.get_ref().sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return written;
    }
    // A rebuilt ledger overwrites one that already exists. Rust's `fs::rename`
    // already replaces on Windows (`MOVEFILE_REPLACE_EXISTING`); the Windows
    // path adds `MOVEFILE_WRITE_THROUGH` and a bounded retry for transient
    // access-denied/sharing violations against a concurrently scanned file.
    #[cfg(unix)]
    let replaced = std::fs::rename(&temporary, path);
    #[cfg(windows)]
    let replaced = crate::session::replace_windows_path_with_retry(&temporary, path);
    match replaced {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
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
mod router_tests {
    use std::{collections::HashMap, fs, path::Path, sync::Arc, time::Duration};

    use cookie_agent_protocol::SessionId;

    use super::{ArtifactRouter, SHARED_ARTIFACTS_DIR};

    struct Placement {
        workdir: tempfile::TempDir,
        router: Arc<ArtifactRouter>,
        roots: [SessionId; 2],
        children: [SessionId; 2],
    }

    /// Router over two root trees, each with one child, resolved the way the
    /// engine resolves them (§5.1).
    fn placement() -> Placement {
        let workdir = tempfile::tempdir().expect("workdir");
        let roots = [SessionId::new_v7(), SessionId::new_v7()];
        let children = [SessionId::new_v7(), SessionId::new_v7()];
        let router = ArtifactRouter::open(workdir.path().to_path_buf()).expect("router");
        let mut membership: HashMap<SessionId, SessionId> = HashMap::new();
        membership.insert(children[0], roots[0]);
        membership.insert(children[1], roots[1]);
        router.install_tree_resolver(Arc::new(move |session| {
            Some(membership.get(&session).copied().unwrap_or(session))
        }));
        Placement {
            workdir,
            router,
            roots,
            children,
        }
    }

    fn tree_dir(workdir: &Path, root: SessionId) -> std::path::PathBuf {
        workdir.join(root.to_string())
    }

    fn tree_blob(workdir: &Path, root: SessionId, digest: &str) -> std::path::PathBuf {
        tree_dir(workdir, root)
            .join(super::ARTIFACTS_DIR)
            .join(digest)
    }

    fn write_root_log(workdir: &Path, root: SessionId, digests: &[&str]) -> std::path::PathBuf {
        let directory = tree_dir(workdir, root);
        fs::create_dir_all(&directory).expect("tree directory");
        let lines = digests
            .iter()
            .map(|digest| {
                serde_json::json!({
                    "payload": {"result": {"reference": format!("artifact://sha256/{digest}")}}
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let path = directory.join(crate::session::EVENTS_FILE);
        fs::write(&path, format!("{lines}\n")).expect("root log");
        path
    }

    #[test]
    fn writes_land_in_the_directory_of_the_writing_trees_root() {
        let fixture = placement();
        let (reference, first) = fixture
            .router
            .retain(fixture.children[0], b"child-zero")
            .expect("retain in child tree");
        let (_, second) = fixture
            .router
            .retain(fixture.children[1], b"child-one")
            .expect("retain in the other tree");
        assert_eq!(reference.uri, format!("artifact://sha256/{first}"));
        assert!(tree_blob(fixture.workdir.path(), fixture.roots[0], &first).is_file());
        assert!(tree_blob(fixture.workdir.path(), fixture.roots[1], &second).is_file());
        assert!(
            !fixture
                .workdir
                .path()
                .join(SHARED_ARTIFACTS_DIR)
                .join(&first)
                .exists(),
            "a routed write never uses the shared store"
        );

        // Identical content is copied into the other tree rather than shared.
        let (_, duplicate) = fixture
            .router
            .retain(fixture.roots[1], b"child-zero")
            .expect("duplicate retain");
        assert_eq!(duplicate, first);
        assert!(tree_blob(fixture.workdir.path(), fixture.roots[1], &duplicate).is_file());
    }

    #[test]
    fn unrouted_writes_fall_back_to_the_shared_store() {
        let workdir = tempfile::tempdir().expect("workdir");
        let router = ArtifactRouter::open(workdir.path().to_path_buf()).expect("router");
        let (_, digest) = router
            .retain(SessionId::new_v7(), b"orphaned")
            .expect("orphan retain");
        assert!(
            workdir
                .path()
                .join(SHARED_ARTIFACTS_DIR)
                .join(&digest)
                .is_file()
        );
        assert!(!workdir.path().join("sessions").exists());
    }

    #[test]
    fn reads_find_content_another_tree_stored() {
        let fixture = placement();
        let (reference, digest) = fixture
            .router
            .retain(fixture.children[0], b"cross-tree")
            .expect("retain");
        // A reopened router has no write index: the directory-name scan must find it.
        let reopened =
            ArtifactRouter::open(fixture.workdir.path().to_path_buf()).expect("reopened router");
        let page = reopened
            .read_paged(&digest, 0, 10)
            .expect("read content stored by another tree");
        assert_eq!(page.content, "cross-tree");
        assert_eq!(reference.uri, format!("artifact://sha256/{digest}"));
        assert!(
            reopened
                .read_paged(&"f".repeat(64), 0, 1)
                .unwrap_err()
                .to_string()
                .contains("artifact missing")
        );
    }

    #[test]
    fn collection_retains_content_another_tree_references() {
        let fixture = placement();
        let (_, digest) = fixture
            .router
            .retain(fixture.children[0], b"referenced-by-other-tree")
            .expect("retain");
        let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &digest);
        let log = write_root_log(fixture.workdir.path(), fixture.roots[1], &[&digest]);

        // First sweep records the cross-reference in the ledger.
        fixture.router.note_tree_loaded(fixture.roots[0]);
        let report = fixture
            .router
            .collect_garbage(Duration::ZERO)
            .expect("sweep");
        assert_eq!(report.deleted, 0);
        assert!(blob.is_file());

        // The referencing log disappearing must not retroactively free the blob.
        drop(fixture.router);
        fs::remove_file(log).expect("remove referencing log");
        let reopened =
            ArtifactRouter::open(fixture.workdir.path().to_path_buf()).expect("reopened router");
        reopened.note_tree_loaded(fixture.roots[0]);
        let report = reopened
            .collect_garbage(Duration::ZERO)
            .expect("second sweep");
        assert_eq!(
            report.deleted, 0,
            "the ledger keeps the foreign reference alive"
        );
        assert!(blob.is_file());
    }

    #[test]
    fn collection_only_sweeps_loaded_trees() {
        let fixture = placement();
        let (_, owned) = fixture
            .router
            .retain(fixture.roots[0], b"unreferenced-in-tree")
            .expect("retain");
        let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &owned);
        fs::write(
            fixture
                .workdir
                .path()
                .join(SHARED_ARTIFACTS_DIR)
                .join("b".repeat(64)),
            b"unreferenced-shared",
        )
        .expect("shared blob");

        let report = fixture
            .router
            .collect_garbage(Duration::ZERO)
            .expect("sweep without loading the tree");
        assert!(blob.is_file(), "an unloaded tree is never collected (§5.2)");
        assert_eq!(report.deleted, 1, "only the shared store was swept");

        fixture.router.note_tree_loaded(fixture.roots[0]);
        let report = fixture
            .router
            .collect_garbage(Duration::ZERO)
            .expect("sweep after loading");
        assert_eq!(report.deleted, 1);
        assert!(!blob.exists());
    }

    /// §5.2, review F4: reusing what the one bulk fold harvested is only sound
    /// while the store still agrees with that fold, and the store's proof is
    /// resident tip **and** durable length. A child resident in this process can
    /// hold its newest record in the log writer's buffer: the file on disk stays
    /// byte-identical while the log has moved. A sweep that compared lengths
    /// alone would call the harvested set current and delete the blob only that
    /// buffered record points at.
    #[test]
    fn a_resident_unflushed_child_append_aborts_the_sweep() {
        let fixture = placement();
        let (root, child) = (fixture.roots[0], fixture.children[0]);
        let harvested_digest = "a".repeat(64);
        let child_dir = tree_dir(fixture.workdir.path(), root)
            .join(crate::session::SUBAGENTS_DIR)
            .join(child.to_string());
        fs::create_dir_all(&child_dir).expect("child directory");
        let child_log = child_dir.join(crate::session::EVENTS_FILE);
        fs::write(
            &child_log,
            format!(
                "{}\n",
                serde_json::json!({"payload": {"result": {"reference":
                    format!("artifact://sha256/{harvested_digest}")}}})
            ),
        )
        .expect("child log");
        let durable_len = fs::metadata(&child_log).expect("child log size").len();
        let fingerprint = move |resident_tip| crate::session::LogFingerprint {
            resident_tip,
            durable_len,
        };
        fixture.router.note_tree_loaded(root);
        fixture
            .router
            .install_log_fingerprint_probe(Arc::new(move |_session| fingerprint(Some(2))));
        fixture.router.note_tree_live_refs(
            root,
            [harvested_digest.clone()].into_iter().collect(),
            [(child, fingerprint(None))]
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>(),
        );
        // Content only a record sitting in the child's writer buffer points at:
        // nothing on disk mentions its digest, and the child's file does not grow.
        let (buffered_reference, buffered_digest) = fixture
            .router
            .retain(child, b"referenced-only-from-the-buffer")
            .expect("retain");
        assert_eq!(
            buffered_reference.uri,
            format!("artifact://sha256/{buffered_digest}")
        );
        let blob = tree_blob(fixture.workdir.path(), root, &buffered_digest);
        assert!(blob.is_file());
        assert_eq!(
            fs::metadata(&child_log).expect("child log size").len(),
            durable_len,
            "the child's newest append is not in the file the sweep can scan"
        );

        let report = fixture
            .router
            .collect_garbage(Duration::ZERO)
            .expect("sweep a tree with a buffered child append");
        assert_eq!(
            (report.deleted, report.retained),
            (0, 0),
            "the live set is incomplete, so the sweep must not run at all"
        );
        assert!(
            blob.is_file(),
            "an artifact a resident child still references must survive the sweep"
        );

        // Control: with nothing buffered, the same harvested set is current and
        // the very same sweep deletes the unreferenced blob.
        let reopened_log = child_log.clone();
        fixture
            .router
            .install_log_fingerprint_probe(Arc::new(move |_session| {
                crate::session::LogFingerprint {
                    resident_tip: None,
                    durable_len: fs::metadata(&reopened_log).expect("child log size").len(),
                }
            }));
        let report = fixture
            .router
            .collect_garbage(Duration::ZERO)
            .expect("sweep the quiet tree");
        assert_eq!(report.deleted, 1, "a proven-current harvest does collect");
        assert!(!blob.exists());
    }

    /// A log line whose artifact URI is JSON-escaped. A legal encoding of
    /// `artifact://…` that a naive `contains("artifact://")` prefilter misses,
    /// which would drop a live reference out of the sweep entirely.
    fn escaped_reference_line(digest: &str) -> String {
        format!(r#"{{"payload":{{"result":{{"reference":"\u0061rtifact://sha256/{digest}"}}}}}}"#)
    }

    /// Same, with the scheme separator escaped instead of the first letter.
    fn escaped_separator_line(digest: &str) -> String {
        format!(r#"{{"payload":{{"result":{{"reference":"artifact\u003a//sha256/{digest}"}}}}}}"#)
    }

    #[test]
    fn an_escaped_artifact_uri_is_scanned_like_a_literal_one() {
        let root = tempfile::tempdir().expect("temporary root");
        let escaped = "a".repeat(64);
        let separator = "b".repeat(64);
        let literal = "c".repeat(64);
        let log = root.path().join(crate::session::EVENTS_FILE);
        fs::write(
            &log,
            format!(
                "{}\n{}\n{}\n{{\"payload\":{{\"text\":\"nothing to see\"}}}}\n",
                escaped_reference_line(&escaped),
                escaped_separator_line(&separator),
                serde_json::json!({
                    "payload": {"result": {"reference": format!("artifact://sha256/{literal}")}}
                }),
            ),
        )
        .expect("log");

        let found = super::scan_artifact_references_in_log(&log).expect("scan");
        assert_eq!(
            found,
            [escaped, separator, literal]
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
            "an escaped URI must reach the live set exactly like a literal one"
        );
    }

    #[test]
    fn collection_retains_a_blob_only_an_escaped_uri_references() {
        let fixture = placement();
        let (_, digest) = fixture
            .router
            .retain(fixture.roots[0], b"referenced-through-an-escape")
            .expect("retain");
        let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &digest);
        // The referencing log belongs to the *other* tree and uses an escape, so
        // the scanner has to decode it before it can recognise the reference.
        let directory = tree_dir(fixture.workdir.path(), fixture.roots[1]);
        fs::create_dir_all(&directory).expect("tree directory");
        fs::write(
            directory.join(crate::session::EVENTS_FILE),
            format!("{}\n", escaped_reference_line(&digest)),
        )
        .expect("escaped log");

        fixture.router.note_tree_loaded(fixture.roots[0]);
        let report = fixture
            .router
            .collect_garbage(Duration::ZERO)
            .expect("sweep");
        assert_eq!(report.deleted, 0);
        assert!(blob.is_file(), "an escaped reference is a live reference");
        assert!(
            fixture
                .router
                .cross_ref_ledger()
                .contains(&(digest.clone(), fixture.roots[1])),
            "the escaped reference is also recorded as a cross-tree one"
        );
        assert!(
            fs::read_to_string(fixture.workdir.path().join(super::CROSS_REFS_FILE))
                .expect("ledger")
                .contains(&digest),
            "the ledger on disk names the escaped digest"
        );
    }

    /// §5.2: a sweep may only delete what it has *proven* unreferenced. If the
    /// child logs of a loaded tree cannot be listed, liveness is unproven and
    /// the whole sweep must abort without unlinking anything.
    #[test]
    fn collection_aborts_without_deleting_when_a_loaded_tree_cannot_be_listed() {
        let fixture = placement();
        let (_, digest) = fixture
            .router
            .retain(fixture.roots[0], b"referenced-only-by-a-child")
            .expect("retain");
        let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &digest);
        write_root_log(fixture.workdir.path(), fixture.roots[0], &[]);
        let subagents =
            tree_dir(fixture.workdir.path(), fixture.roots[0]).join(crate::session::SUBAGENTS_DIR);
        let child = subagents.join(fixture.children[0].to_string());
        fs::create_dir_all(&child).expect("child directory");
        fs::write(
            child.join(crate::session::EVENTS_FILE),
            format!(
                "{}\n",
                serde_json::json!({
                    "payload": {"result": {"reference": format!("artifact://sha256/{digest}")}}
                })
            ),
        )
        .expect("child log");
        fixture.router.note_tree_loaded(fixture.roots[0]);
        let report = fixture
            .router
            .collect_garbage(Duration::ZERO)
            .expect("healthy sweep");
        assert_eq!(report.deleted, 0, "the child log keeps the blob alive");
        assert!(blob.is_file());

        // A sweep that would delete an expired shared blob if it ran at all.
        let shared_blob = fixture
            .workdir
            .path()
            .join(SHARED_ARTIFACTS_DIR)
            .join("d".repeat(64));
        fs::write(&shared_blob, b"expired and unreferenced").expect("shared blob");

        // Inject the enumeration failure: listing `subagents/` now errors with
        // something that is not "no such directory".
        fs::remove_dir_all(&subagents).expect("remove subagents");
        fs::write(&subagents, b"not a directory").expect("subagents becomes a file");
        let error = fixture
            .router
            .collect_garbage(Duration::ZERO)
            .expect_err("an incomplete live set must abort the sweep");
        assert_ne!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "only an absent directory may mean \"no children\": {error}"
        );
        assert!(
            blob.is_file(),
            "the sole referencer of the blob was hidden from the sweep"
        );
        assert!(
            shared_blob.is_file(),
            "the sweep deleted despite an unproven live set"
        );
    }

    /// §5.2: the same rule one level up. If the sessions root cannot be listed,
    /// nothing is known to be live, which is not the same as nothing being live.
    #[test]
    fn collection_aborts_without_deleting_when_the_sessions_root_cannot_be_listed() {
        let fixture = placement();
        let (_, digest) = fixture
            .router
            .retain(fixture.roots[0], b"referenced-by-a-root-log")
            .expect("retain");
        let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &digest);
        write_root_log(fixture.workdir.path(), fixture.roots[0], &[&digest]);
        let shared_blob = fixture
            .workdir
            .path()
            .join(SHARED_ARTIFACTS_DIR)
            .join("e".repeat(64));
        fs::write(&shared_blob, b"expired and unreferenced").expect("shared blob");

        // A router whose sessions root is a plain file: enumeration fails with
        // `NotADirectory`, and treating that as "no sessions" would free both
        // blobs below.
        let blocked = fixture.workdir.path().join("not-a-sessions-root");
        fs::write(&blocked, b"").expect("blocking file");
        let broken = ArtifactRouter::open_layout(
            fixture.workdir.path().to_path_buf(),
            fixture.workdir.path().join(SHARED_ARTIFACTS_DIR),
            blocked,
        )
        .expect("router over an unlistable sessions root");
        broken.note_tree_loaded(fixture.roots[0]);

        let error = broken
            .collect_garbage(Duration::ZERO)
            .expect_err("an unlistable sessions root must abort the sweep");
        assert_ne!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "only an absent sessions root may mean \"no sessions\": {error}"
        );
        assert!(
            blob.is_file(),
            "a tree blob was freed on an unknown live set"
        );
        assert!(
            shared_blob.is_file(),
            "the shared store was swept on an unknown live set"
        );

        // Sanity: the same fixture with a readable sessions root does collect
        // the unreferenced shared blob.
        let healthy =
            ArtifactRouter::open(fixture.workdir.path().to_path_buf()).expect("healthy router");
        healthy.note_tree_loaded(fixture.roots[0]);
        let report = healthy.collect_garbage(Duration::ZERO).expect("sweep");
        assert_eq!(report.deleted, 1);
        assert!(blob.is_file());
    }

    /// B2: a lost ledger append must never leave the durable ledger short of a
    /// reference this process already proved — the next sweep rewrites it.
    #[test]
    fn a_lost_ledger_append_is_rewritten_by_the_next_sweep() {
        let fixture = placement();
        let (_, digest) = fixture
            .router
            .retain(fixture.roots[0], b"foreign-but-for-a-lost-append")
            .expect("retain");
        let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &digest);
        write_root_log(fixture.workdir.path(), fixture.roots[1], &[&digest]);
        fixture.router.note_tree_loaded(fixture.roots[0]);

        fixture.router.fail_next_ledger_append();
        fixture
            .router
            .collect_garbage(Duration::ZERO)
            .expect("sweep with a lost append");
        let ledger_path = fixture.workdir.path().join(super::CROSS_REFS_FILE);
        assert!(
            !fs::read_to_string(&ledger_path)
                .unwrap_or_default()
                .contains(&digest),
            "the injected failure must actually lose the entry"
        );

        // The next sweep repairs the file wholesale, so a later process cannot
        // see a ledger missing a reference this one proved.
        fixture
            .router
            .collect_garbage(Duration::ZERO)
            .expect("repairing sweep");
        let text = fs::read_to_string(&ledger_path).expect("rebuilt ledger");
        assert!(
            text.contains(&digest),
            "the ledger was not rebuilt after the lost append: {text}"
        );
        assert!(blob.is_file());

        // A reopened router reads the rebuilt file back into its live set.
        drop(fixture.router);
        let reopened =
            ArtifactRouter::open(fixture.workdir.path().to_path_buf()).expect("reopened router");
        reopened.note_tree_loaded(fixture.roots[0]);
        let live = reopened
            .live_references_and_unprovable()
            .expect("live set")
            .0;
        assert!(
            live.contains(&digest),
            "the durable ledger no longer carries the reference back"
        );
        assert!(blob.is_file());
    }
}
