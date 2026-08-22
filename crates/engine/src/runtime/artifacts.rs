#[cfg(unix)]
mod unix {
    use std::{
        fs,
        io::{Read, Seek, SeekFrom, Write},
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use cookie_agent_protocol::{
        ArtifactReference, OutputStream, PersistedToolResult as ToolResult, ToolAttachment,
        ToolOutputTruncation,
    };
    use rustix::fs::{AtFlags, Dir, Mode, OFlags, fsync, openat, renameat, unlinkat};
    use serde::Serialize;
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use super::super::tool_execution::truncate_tool_output;

    pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

    #[derive(Debug)]
    pub(crate) struct ArtifactStore {
        directory_handle: Arc<fs::File>,
        writes: Mutex<()>,
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
                directory_handle: Arc::new(handle),
                writes: Mutex::new(()),
            });
            store.cleanup_temporary_artifacts()?;
            store.verify_existing_artifact_hashes()?;
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
            for name in directory_names(&self.directory_handle)? {
                if !valid_temporary_artifact_name(&name) {
                    continue;
                }
                unlinkat(&*self.directory_handle, &name, AtFlags::empty())?;
            }
            fsync(&*self.directory_handle)?;
            Ok(())
        }

        fn verify_existing_artifact_hashes(&self) -> std::io::Result<()> {
            for name in directory_names(&self.directory_handle)? {
                if is_digest_name(&name) {
                    let mut file = self.open_existing(&name)?.ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "existing artifact disappeared during validation",
                        )
                    })?;
                    if hash_file(&mut file)?.0 != name {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "existing artifact content does not match its digest name",
                        ));
                    }
                }
            }
            Ok(())
        }

        fn create_capture_file(&self, name: &str) -> std::io::Result<fs::File> {
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
            Ok(fs::File::from(file))
        }

        fn commit_capture(&self, name: &str) -> std::io::Result<(CapturedArtifact, u64)> {
            let _write = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut temporary = self.open_existing(name)?.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "capture missing")
            })?;
            let (digest, byte_length, newlines) = hash_file(&mut temporary)?;
            temporary.sync_all()?;
            drop(temporary);
            if let Some(mut existing) = self.open_existing(&digest)? {
                if hash_file(&mut existing)?.0 != digest {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "artifact digest collision or corrupt retained artifact",
                    ));
                }
                unlinkat(&*self.directory_handle, name, AtFlags::empty())?;
            } else {
                renameat(
                    &*self.directory_handle,
                    name,
                    &*self.directory_handle,
                    &digest,
                )?;
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

        fn discard_capture(&self, name: &str) {
            if valid_temporary_artifact_name(name) {
                let _ = unlinkat(&*self.directory_handle, name, AtFlags::empty());
            }
        }

        fn preview(&self, digest: &str, max_bytes: usize) -> std::io::Result<(String, bool)> {
            let mut file = self.open_existing(digest)?.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "artifact missing")
            })?;
            let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
            std::io::Read::by_ref(&mut file)
                .take(max_bytes.saturating_add(1) as u64)
                .read_to_end(&mut bytes)?;
            let truncated = bytes.len() > max_bytes;
            bytes.truncate(max_bytes);
            Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
        }

        pub(crate) fn read_verified_attachment(
            &self,
            attachment: &ToolAttachment,
        ) -> std::io::Result<Vec<u8>> {
            if !is_digest_name(attachment.sha256.as_str())
                || attachment.reference.uri != format!("artifact://sha256/{}", attachment.sha256)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "attachment reference and digest do not match",
                ));
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

    #[derive(Clone, Debug)]
    pub(crate) struct OutputCapture {
        store: Arc<ArtifactStore>,
        stdout: Arc<CaptureStream>,
        stderr: Arc<CaptureStream>,
        _cleanup: Arc<CaptureCleanup>,
    }

    #[derive(Debug)]
    struct CaptureStream {
        name: String,
        file: Mutex<fs::File>,
        error: Mutex<Option<String>>,
    }

    #[derive(Debug)]
    struct CaptureCleanup {
        store: Arc<ArtifactStore>,
        stdout_name: String,
        stderr_name: String,
    }

    impl Drop for CaptureCleanup {
        fn drop(&mut self) {
            self.store.discard_capture(&self.stdout_name);
            self.store.discard_capture(&self.stderr_name);
        }
    }

    #[derive(Clone, Debug, Serialize)]
    struct CapturedArtifact {
        reference: ArtifactReference,
        sha256: String,
        byte_length: u64,
    }

    pub(super) fn composed_bash_output_lines(stdout_newlines: u64, stderr_newlines: u64) -> u64 {
        // The fixed labels are "stdout:\n" and "\n\nstderr:\n": four newline
        // bytes total. split('\n') line count is newline count + one.
        stdout_newlines + stderr_newlines + 5
    }

    impl OutputCapture {
        pub(crate) fn new(store: Arc<ArtifactStore>) -> std::io::Result<Self> {
            let id = Uuid::now_v7();
            let stdout_name = format!(".capture-{id}-stdout.tmp");
            let stderr_name = format!(".capture-{id}-stderr.tmp");
            let stdout = store.create_capture_file(&stdout_name)?;
            let stderr = match store.create_capture_file(&stderr_name) {
                Ok(stderr) => stderr,
                Err(error) => {
                    store.discard_capture(&stdout_name);
                    return Err(error);
                }
            };
            Ok(Self {
                store: store.clone(),
                stdout: Arc::new(CaptureStream {
                    name: stdout_name.clone(),
                    file: Mutex::new(stdout),
                    error: Mutex::new(None),
                }),
                stderr: Arc::new(CaptureStream {
                    name: stderr_name.clone(),
                    file: Mutex::new(stderr),
                    error: Mutex::new(None),
                }),
                _cleanup: Arc::new(CaptureCleanup {
                    store,
                    stdout_name,
                    stderr_name,
                }),
            })
        }

        pub(crate) fn write(&self, stream: OutputStream, data: &[u8]) {
            let capture = match stream {
                OutputStream::Stdout => &self.stdout,
                OutputStream::Stderr => &self.stderr,
            };
            if capture
                .error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
            {
                return;
            }
            if let Err(error) = capture
                .file
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .write_all(data)
            {
                *capture
                    .error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
            }
        }

        pub(crate) fn finish(
            &self,
            mut result: ToolResult,
            max_lines: usize,
            max_bytes: usize,
        ) -> std::io::Result<ToolResult> {
            for stream in [&self.stdout, &self.stderr] {
                if let Some(error) = stream
                    .error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                {
                    self.discard();
                    return Err(std::io::Error::other(format!(
                        "tool output capture failed: {error}"
                    )));
                }
                stream
                    .file
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .sync_all()?;
            }
            let (stdout, stdout_newlines) = match self.store.commit_capture(&self.stdout.name) {
                Ok(stdout) => stdout,
                Err(error) => {
                    self.discard();
                    return Err(error);
                }
            };
            let (stderr, stderr_newlines) = match self.store.commit_capture(&self.stderr.name) {
                Ok(stderr) => stderr,
                Err(error) => {
                    self.store.discard_capture(&self.stderr.name);
                    return Err(error);
                }
            };
            let original_lines = composed_bash_output_lines(stdout_newlines, stderr_newlines);
            let preview_budget = max_bytes.max(1);
            let (stdout_preview, stdout_truncated) =
                self.store.preview(&stdout.sha256, preview_budget)?;
            let (stderr_preview, stderr_truncated) =
                self.store.preview(&stderr.sha256, preview_budget)?;
            let complete_for_budget =
                format!("stdout:\n{stdout_preview}\n\nstderr:\n{stderr_preview}");
            let preview = truncate_tool_output(&complete_for_budget, max_lines, max_bytes)
                .map_or(complete_for_budget.clone(), |preview| preview.content);
            let stream_truncated = stdout_truncated || stderr_truncated;
            let output_truncated = preview != complete_for_budget || stream_truncated;
            result.output = preview;
            let streams = serde_json::json!({"stdout": stdout.clone(), "stderr": stderr.clone()});
            match &mut result.metadata {
                Value::Object(metadata) => {
                    metadata.insert("streams".into(), streams.clone());
                }
                metadata => {
                    *metadata = serde_json::json!({"tool": metadata.clone(), "streams": streams});
                }
            }
            if output_truncated {
                let manifest = serde_json::to_vec(&serde_json::json!({
                    "title": result.title,
                    "streams": streams,
                }))?;
                let (retained, _) = self.store.retain(&manifest)?;
                result.truncation = Some(ToolOutputTruncation {
                    original_bytes: stdout.byte_length + stderr.byte_length + 18,
                    original_lines,
                    retained,
                });
            }
            Ok(result)
        }

        pub(crate) fn discard(&self) {
            self.store.discard_capture(&self.stdout.name);
            self.store.discard_capture(&self.stderr.name);
        }
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
            return matches!(stream, "stdout" | "stderr") && Uuid::parse_str(id).is_ok();
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
        use cookie_agent_protocol::{
            OutputStream, PersistedToolResult as ToolResult, SafeDisplayText,
        };

        use super::{ArtifactStore, OutputCapture};

        fn result(output: String) -> ToolResult {
            ToolResult {
                title: SafeDisplayText::new("Bash").expect("result title"),
                output,
                metadata: serde_json::Value::Null,
                truncation: None,
                attachments: Vec::new(),
            }
        }

        fn capture() -> (tempfile::TempDir, OutputCapture) {
            let directory = tempfile::tempdir().expect("temporary artifact root");
            let store = ArtifactStore::open(directory.path().join("artifacts"))
                .expect("open artifact store");
            let capture = OutputCapture::new(store).expect("create output capture");
            (directory, capture)
        }

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
        fn bash_capture_composes_stdout_and_stderr_once() {
            let (_directory, capture) = capture();
            let stdout = "stdout-unique\n";
            let stderr = "stderr-unique\n";
            capture.write(OutputStream::Stdout, stdout.as_bytes());
            capture.write(OutputStream::Stderr, stderr.as_bytes());

            let result = capture
                .finish(result(format!("{stdout}{stderr}")), 100, 4096)
                .expect("finish capture");

            assert_eq!(
                result.output,
                "stdout:\nstdout-unique\n\n\nstderr:\nstderr-unique\n"
            );
            assert_eq!(result.output.matches("stdout-unique").count(), 1);
            assert_eq!(result.output.matches("stderr-unique").count(), 1);
            assert!(result.truncation.is_none());
        }

        #[test]
        fn bash_capture_truncation_counts_composed_streams_without_duplication() {
            let (_directory, capture) = capture();
            let stdout = format!("kept-prefix\n{}", "x".repeat(256));
            let stderr = "stderr-tail\n";
            capture.write(OutputStream::Stdout, stdout.as_bytes());
            capture.write(OutputStream::Stderr, stderr.as_bytes());

            let result = capture
                .finish(result(format!("{stdout}{stderr}")), 100, 80)
                .expect("finish capture");
            let truncation = result.truncation.expect("truncation metadata");
            let complete = format!("stdout:\n{stdout}\n\nstderr:\n{stderr}");

            assert!(result.output.len() <= 80);
            assert!(result.output.starts_with("stdout:\nkept-prefix\n"));
            assert_eq!(result.output.matches("kept-prefix").count(), 1);
            assert_eq!(truncation.original_bytes, complete.len() as u64);
            assert_eq!(
                truncation.original_lines,
                complete.split('\n').count() as u64
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

    use cookie_agent_protocol::{
        ArtifactReference, OutputStream, PersistedToolResult as ToolResult, ToolAttachment,
        ToolOutputTruncation,
    };
    use serde::Serialize;
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use super::super::tool_execution::truncate_tool_output;

    pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

    #[derive(Debug)]
    pub(crate) struct ArtifactStore {
        directory: PathBuf,
        writes: Mutex<()>,
    }

    impl ArtifactStore {
        pub(crate) fn open(directory: PathBuf) -> std::io::Result<Arc<Self>> {
            if !directory.exists() {
                cookie_agent_models::secure_store::create_windows_private_dir_all(&directory)?;
            }
            let store = Arc::new(Self {
                directory,
                writes: Mutex::new(()),
            });
            store.cleanup_temporary_artifacts()?;
            store.verify_existing_artifact_hashes()?;
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

        pub(crate) fn read_verified_attachment(
            &self,
            attachment: &ToolAttachment,
        ) -> std::io::Result<Vec<u8>> {
            if !is_digest_name(attachment.sha256.as_str())
                || attachment.reference.uri != format!("artifact://sha256/{}", attachment.sha256)
            {
                return Err(invalid("attachment reference and digest do not match"));
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

        fn create_file(&self, name: &str) -> std::io::Result<fs::File> {
            let path = self.directory.join(name);
            cookie_agent_models::secure_store::create_windows_private_file(&path)
        }

        fn cleanup_temporary_artifacts(&self) -> std::io::Result<()> {
            for entry in fs::read_dir(&self.directory)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if valid_temporary_artifact_name(&name) {
                    fs::remove_file(entry.path())?;
                }
            }
            Ok(())
        }

        fn verify_existing_artifact_hashes(&self) -> std::io::Result<()> {
            for entry in fs::read_dir(&self.directory)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if is_digest_name(&name) {
                    let mut file = self
                        .open_existing(&name)?
                        .ok_or_else(|| invalid("existing artifact disappeared"))?;
                    if hash_file(&mut file)?.0 != name {
                        return Err(invalid("existing artifact content does not match its name"));
                    }
                }
            }
            Ok(())
        }

        fn commit_capture(&self, name: &str) -> std::io::Result<(CapturedArtifact, u64)> {
            let _write = self
                .writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut temporary = self.open_existing(name)?.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "capture missing")
            })?;
            let (digest, byte_length, newlines) = hash_file(&mut temporary)?;
            temporary.sync_all()?;
            drop(temporary);
            let temporary_path = self.directory.join(name);
            if let Some(mut existing) = self.open_existing(&digest)? {
                if hash_file(&mut existing)?.0 != digest {
                    return Err(invalid(
                        "artifact digest collision or corrupt retained artifact",
                    ));
                }
                fs::remove_file(temporary_path)?;
            } else {
                cookie_agent_models::secure_store::replace_windows_path(
                    &temporary_path,
                    &self.directory.join(&digest),
                )?;
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

        fn discard_capture(&self, name: &str) {
            if valid_temporary_artifact_name(name) {
                let _ = fs::remove_file(self.directory.join(name));
            }
        }

        fn preview(&self, digest: &str, max_bytes: usize) -> std::io::Result<(String, bool)> {
            let mut file = self.open_existing(digest)?.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "artifact missing")
            })?;
            let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
            std::io::Read::by_ref(&mut file)
                .take(max_bytes.saturating_add(1) as u64)
                .read_to_end(&mut bytes)?;
            let truncated = bytes.len() > max_bytes;
            bytes.truncate(max_bytes);
            Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
        }
    }

    #[derive(Clone, Debug)]
    pub(crate) struct OutputCapture {
        store: Arc<ArtifactStore>,
        stdout: Arc<CaptureStream>,
        stderr: Arc<CaptureStream>,
        _cleanup: Arc<CaptureCleanup>,
    }

    #[derive(Debug)]
    struct CaptureStream {
        name: String,
        file: Mutex<fs::File>,
        error: Mutex<Option<String>>,
    }

    #[derive(Debug)]
    struct CaptureCleanup {
        store: Arc<ArtifactStore>,
        stdout_name: String,
        stderr_name: String,
    }

    impl Drop for CaptureCleanup {
        fn drop(&mut self) {
            self.store.discard_capture(&self.stdout_name);
            self.store.discard_capture(&self.stderr_name);
        }
    }

    #[derive(Clone, Debug, Serialize)]
    struct CapturedArtifact {
        reference: ArtifactReference,
        sha256: String,
        byte_length: u64,
    }

    impl OutputCapture {
        pub(crate) fn new(store: Arc<ArtifactStore>) -> std::io::Result<Self> {
            let id = Uuid::now_v7();
            let stdout_name = format!(".capture-{id}-stdout.tmp");
            let stderr_name = format!(".capture-{id}-stderr.tmp");
            let stdout = store.create_file(&stdout_name)?;
            let stderr = match store.create_file(&stderr_name) {
                Ok(stderr) => stderr,
                Err(error) => {
                    store.discard_capture(&stdout_name);
                    return Err(error);
                }
            };
            Ok(Self {
                store: store.clone(),
                stdout: Arc::new(CaptureStream {
                    name: stdout_name.clone(),
                    file: Mutex::new(stdout),
                    error: Mutex::new(None),
                }),
                stderr: Arc::new(CaptureStream {
                    name: stderr_name.clone(),
                    file: Mutex::new(stderr),
                    error: Mutex::new(None),
                }),
                _cleanup: Arc::new(CaptureCleanup {
                    store,
                    stdout_name,
                    stderr_name,
                }),
            })
        }

        pub(crate) fn write(&self, stream: OutputStream, data: &[u8]) {
            let capture = match stream {
                OutputStream::Stdout => &self.stdout,
                OutputStream::Stderr => &self.stderr,
            };
            if capture
                .error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
            {
                return;
            }
            if let Err(error) = capture
                .file
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .write_all(data)
            {
                *capture
                    .error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
            }
        }

        pub(crate) fn finish(
            &self,
            mut result: ToolResult,
            max_lines: usize,
            max_bytes: usize,
        ) -> std::io::Result<ToolResult> {
            for stream in [&self.stdout, &self.stderr] {
                if let Some(error) = stream
                    .error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                {
                    self.discard();
                    return Err(std::io::Error::other(format!(
                        "tool output capture failed: {error}"
                    )));
                }
                stream
                    .file
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .sync_all()?;
            }
            let (stdout, stdout_newlines) = self.store.commit_capture(&self.stdout.name)?;
            let (stderr, stderr_newlines) = match self.store.commit_capture(&self.stderr.name) {
                Ok(stderr) => stderr,
                Err(error) => {
                    self.store.discard_capture(&self.stderr.name);
                    return Err(error);
                }
            };
            let original_lines = stdout_newlines + stderr_newlines + 5;
            let preview_budget = max_bytes.max(1);
            let (stdout_preview, stdout_truncated) =
                self.store.preview(&stdout.sha256, preview_budget)?;
            let (stderr_preview, stderr_truncated) =
                self.store.preview(&stderr.sha256, preview_budget)?;
            let complete = format!("stdout:\n{stdout_preview}\n\nstderr:\n{stderr_preview}");
            let preview = truncate_tool_output(&complete, max_lines, max_bytes)
                .map_or(complete.clone(), |preview| preview.content);
            let output_truncated = preview != complete || stdout_truncated || stderr_truncated;
            result.output = preview;
            let streams = serde_json::json!({"stdout": stdout.clone(), "stderr": stderr.clone()});
            match &mut result.metadata {
                Value::Object(metadata) => {
                    metadata.insert("streams".into(), streams.clone());
                }
                metadata => {
                    *metadata = serde_json::json!({"tool": metadata.clone(), "streams": streams});
                }
            }
            if output_truncated {
                let manifest = serde_json::to_vec(&serde_json::json!({
                    "title": result.title,
                    "streams": streams,
                }))?;
                let (retained, _) = self.store.retain(&manifest)?;
                result.truncation = Some(ToolOutputTruncation {
                    original_bytes: stdout.byte_length + stderr.byte_length + 18,
                    original_lines,
                    retained,
                });
            }
            Ok(result)
        }

        pub(crate) fn discard(&self) {
            self.store.discard_capture(&self.stdout.name);
            self.store.discard_capture(&self.stderr.name);
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
            return matches!(stream, "stdout" | "stderr") && Uuid::parse_str(id).is_ok();
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
        use super::ArtifactStore;

        #[test]
        fn uses_preexisting_artifact_directory_without_acl_validation() {
            let temporary = tempfile::tempdir().expect("temporary root");
            let artifacts = temporary.path().join("artifacts");
            std::fs::create_dir(&artifacts).expect("ordinary artifact directory");
            ArtifactStore::open(artifacts).expect("existing artifact directory");
        }
    }
}

#[cfg(windows)]
pub(crate) use windows::*;
