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
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, fchmod, fsync, openat, renameat, statat, unlinkat,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::tool_execution::truncate_tool_output;

pub(super) const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct ArtifactStore {
    directory_handle: Arc<fs::File>,
    writes: Mutex<()>,
}

impl ArtifactStore {
    pub(crate) fn open(directory: PathBuf) -> std::io::Result<Arc<Self>> {
        prepare_private_directory(&directory)?;
        let expected = fs::symlink_metadata(&directory)?;
        let handle = rustix::fs::open(
            &directory,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let handle = fs::File::from(handle);
        ensure_same_object(&handle.metadata()?, &expected)?;
        validate_owned_directory(&handle)?;
        fchmod(&handle, Mode::from_raw_mode(0o700))?;
        let store = Arc::new(Self {
            directory_handle: Arc::new(handle),
            writes: Mutex::new(()),
        });
        store.cleanup_temporary_artifacts()?;
        store.validate_existing_artifacts()?;
        Ok(store)
    }

    pub(crate) fn retain(&self, content: &[u8]) -> std::io::Result<(ArtifactReference, String)> {
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
                validate_owned_regular_file(&temporary)?;
                temporary.write_all(content)?;
                temporary.sync_all()?;
                drop(temporary);
                renameat(
                    &*self.directory_handle,
                    &temporary_name,
                    &*self.directory_handle,
                    &digest,
                )?;
                let final_file = self
                    .open_existing(&digest)?
                    .ok_or_else(|| std::io::Error::other("retained artifact disappeared"))?;
                validate_owned_regular_file(&final_file)?;
                fchmod(&final_file, Mode::from_raw_mode(0o600))?;
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
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(file) => {
                let file = fs::File::from(file);
                validate_owned_regular_file(&file)?;
                fchmod(&file, Mode::from_raw_mode(0o600))?;
                Ok(Some(file))
            }
            Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn cleanup_temporary_artifacts(&self) -> std::io::Result<()> {
        for name in directory_names(&self.directory_handle)? {
            if !valid_temporary_artifact_name(&name) {
                continue;
            }
            let stat = statat(&*self.directory_handle, &name, AtFlags::SYMLINK_NOFOLLOW)?;
            if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                continue;
            }
            validate_stat_owner(&stat, "temporary artifact")?;
            let Some(file) = self.open_existing(&name)? else {
                continue;
            };
            validate_owned_regular_file(&file)?;
            ensure_stat_same_object(&file.metadata()?, &stat)?;
            unlinkat(&*self.directory_handle, &name, AtFlags::empty())?;
        }
        fsync(&*self.directory_handle)?;
        Ok(())
    }

    fn validate_existing_artifacts(&self) -> std::io::Result<()> {
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
        let file = fs::File::from(file);
        validate_owned_regular_file(&file)?;
        Ok(file)
    }

    fn commit_capture(&self, name: &str) -> std::io::Result<(CapturedArtifact, u64)> {
        let _write = self
            .writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut temporary = self
            .open_existing(name)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "capture missing"))?;
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
            let final_file = self
                .open_existing(&digest)?
                .ok_or_else(|| std::io::Error::other("capture artifact disappeared"))?;
            validate_owned_regular_file(&final_file)?;
            fchmod(&final_file, Mode::from_raw_mode(0o600))?;
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
        let mut file = self
            .open_existing(digest)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "artifact missing"))?;
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
    pub(super) fn new(store: Arc<ArtifactStore>) -> std::io::Result<Self> {
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

    pub(super) fn finish(
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
        let complete_for_budget = format!("stdout:\n{stdout_preview}\n\nstderr:\n{stderr_preview}");
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

    pub(super) fn discard(&self) {
        self.store.discard_capture(&self.stdout.name);
        self.store.discard_capture(&self.stderr.name);
    }
}

pub(super) fn validate_owned_directory(directory: &fs::File) -> std::io::Result<()> {
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact store root is not a directory",
        ));
    }
    validate_owner(&metadata, "artifact store root")
}

pub(super) fn validate_owned_regular_file(file: &fs::File) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact object is not a regular file",
        ));
    }
    validate_owner(&metadata, "artifact object")
}

pub(super) fn validate_owner(metadata: &fs::Metadata, object: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{object} is not owned by the current user"),
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_stat_owner(stat: &rustix::fs::Stat, object: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    if stat.st_uid != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{object} is not owned by the current user"),
        ));
    }
    Ok(())
}

pub(super) fn ensure_same_object(
    opened: &fs::Metadata,
    path: &fs::Metadata,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if opened.dev() != path.dev() || opened.ino() != path.ino() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "authorized read target changed while it was being opened",
            ));
        }
    }
    #[cfg(not(unix))]
    if opened.is_file() != path.is_file()
        || opened.is_dir() != path.is_dir()
        || opened.len() != path.len()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "authorized read target changed while it was being opened",
        ));
    }
    Ok(())
}

pub(super) fn ensure_stat_same_object(
    opened: &fs::Metadata,
    path: &rustix::fs::Stat,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if opened.dev() != path.st_dev || opened.ino() != path.st_ino {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "artifact object changed during validation",
            ));
        }
    }
    Ok(())
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
    match fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "artifact store root must be a non-symlink directory",
                ));
            }
            validate_owner(&metadata, "artifact store root")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(directory)?;
            let metadata = fs::symlink_metadata(directory)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "artifact store root must be a non-symlink directory",
                ));
            }
            validate_owner(&metadata, "artifact store root")?;
        }
        Err(error) => return Err(error),
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
    use cookie_agent_protocol::{OutputStream, PersistedToolResult as ToolResult, SafeDisplayText};

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
        let store =
            ArtifactStore::open(directory.path().join("artifacts")).expect("open artifact store");
        let capture = OutputCapture::new(store).expect("create output capture");
        (directory, capture)
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
