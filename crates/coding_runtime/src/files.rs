//! The `fs/*` requests of transfers (runtime protocol 1.13.0,
//! `docs/site/specs/machines-on-demand.md` section 6.3): the daemon prepares
//! a path, opens it to read or to write, and relays chunks between two
//! runtimes, or asks one runtime to copy on its own machine.
//!
//! A path is prepared as the file tools prepare theirs: `~` expands against
//! the runtime's home, the permission label is lexical, and the route is
//! resolved once, at prepare, with every link expanded, so a link retargeted
//! after approval does not redirect the transfer.
//!
//! A folder moves as a tar stream, packed while it is read and unpacked while
//! it is written; nothing is staged on either disk. A destination is written
//! to a hidden sibling (`.<name>.cookie-partial-<id>`) and put in place at
//! commit, so nobody sees half of it, and a handle dropped uncommitted (by
//! `fs/close`, or with the connection) removes it.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use base64::Engine as _;
use cookie_agent_protocol::runtime_protocol::{
    FsDirection, FsKind, FsPrepareResult, FsReadChunkResult, FsReadOpenResult, FsStreamSummary,
    FsWritten, MAX_FS_CHUNK_BYTES,
};
use sha2::{Digest as _, Sha256};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    sync::mpsc,
};

/// What every hidden partial copy's name holds.
const PARTIAL_MARKER: &str = ".cookie-partial-";
/// How long a crash's partial copy is left before a later start removes it.
const STALE_PARTIAL_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The size of the pieces a folder's tar stream moves in between the thread
/// that packs or unpacks it and the requests.
const PIPE_CHUNK: usize = 256 * 1024;
/// Pieces buffered between that thread and the requests.
const PIPE_DEPTH: usize = 8;

/// Why an `fs/*` request failed.
#[derive(Debug)]
pub(crate) enum FsError {
    /// The request names a route or handle the runtime does not have.
    UnknownHandle(u64),
    /// The request does not fit what it names.
    Invalid(String),
    /// The file system refused.
    Failed(String),
}

impl FsError {
    fn io(what: &str, path: &Path, error: &io::Error) -> Self {
        Self::Failed(format!("{what} {}: {error}", path.display()))
    }
}

type FsResult<T> = Result<T, FsError>;

/// A path prepared for a transfer, until it is opened or closed.
struct Route {
    direction: FsDirection,
    /// The path as results show it: absolute, normalized lexically.
    shown: PathBuf,
    /// Where the route led at prepare, every link expanded.
    resolved: PathBuf,
    /// What was there at prepare.
    kind: Option<FsKind>,
}

enum Entry {
    Route(Route),
    Read(Arc<tokio::sync::Mutex<Reader>>),
    Write(Arc<tokio::sync::Mutex<Writer>>),
    /// An `fs/copy` under way, under its destination route's ID: closing
    /// that stops it.
    Copying(tokio_util::sync::CancellationToken),
}

/// The routes and open handles of one connection. Dropping it, with the
/// connection, removes every uncommitted destination.
pub(crate) struct Files {
    next: AtomicU64,
    entries: Mutex<HashMap<u64, Entry>>,
    /// Where partial copies are recorded while they exist, so a later start
    /// removes those a crash left behind; `None` keeps no record.
    journal: Option<PathBuf>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Files {
    pub(crate) fn new(journal: Option<PathBuf>) -> Self {
        Self {
            next: AtomicU64::new(1),
            entries: Mutex::default(),
            journal,
        }
    }

    fn insert(&self, entry: Entry) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.entries).insert(id, entry);
        id
    }

    fn take_route(&self, id: u64, direction: FsDirection) -> FsResult<Route> {
        let mut entries = lock(&self.entries);
        match entries.remove(&id) {
            Some(Entry::Route(route)) if route.direction == direction => Ok(route),
            Some(other) => {
                entries.insert(id, other);
                Err(FsError::Invalid(format!(
                    "{id} is no route prepared to {}",
                    match direction {
                        FsDirection::Read => "read",
                        FsDirection::Write => "write",
                    }
                )))
            }
            None => Err(FsError::UnknownHandle(id)),
        }
    }

    fn reader(&self, id: u64) -> FsResult<Arc<tokio::sync::Mutex<Reader>>> {
        match lock(&self.entries).get(&id) {
            Some(Entry::Read(reader)) => Ok(Arc::clone(reader)),
            Some(_) => Err(FsError::Invalid(format!("{id} is no read handle"))),
            None => Err(FsError::UnknownHandle(id)),
        }
    }

    fn writer(&self, id: u64) -> FsResult<Arc<tokio::sync::Mutex<Writer>>> {
        match lock(&self.entries).get(&id) {
            Some(Entry::Write(writer)) => Ok(Arc::clone(writer)),
            Some(_) => Err(FsError::Invalid(format!("{id} is no write handle"))),
            None => Err(FsError::UnknownHandle(id)),
        }
    }

    /// `fs/prepare`: `path` (`~` already expanded) against `workspace`.
    pub(crate) async fn prepare(
        &self,
        workspace: PathBuf,
        path: String,
        direction: FsDirection,
    ) -> FsResult<FsPrepareResult> {
        let label = crate::permission_path_label(&path, &workspace);
        let (route, size) =
            tokio::task::spawn_blocking(move || resolve(&workspace, &path, direction))
                .await
                .map_err(|error| {
                    FsError::Failed(format!("preparing the path failed: {error}"))
                })??;
        let result = FsPrepareResult {
            route: 0,
            path: route.shown.to_string_lossy().into_owned(),
            label,
            binding: route.resolved.to_string_lossy().into_owned(),
            kind: route.kind,
            size,
        };
        let id = self.insert(Entry::Route(route));
        Ok(FsPrepareResult {
            route: id,
            ..result
        })
    }

    /// `fs/read_open`.
    pub(crate) async fn read_open(&self, route: u64) -> FsResult<FsReadOpenResult> {
        let route = self.take_route(route, FsDirection::Read)?;
        let reader = Reader::open(&route).await?;
        let result = FsReadOpenResult {
            handle: 0,
            kind: reader.kind(),
            size: reader.size(),
        };
        let handle = self.insert(Entry::Read(Arc::new(tokio::sync::Mutex::new(reader))));
        Ok(FsReadOpenResult { handle, ..result })
    }

    /// `fs/read_chunk`.
    pub(crate) async fn read_chunk(
        &self,
        handle: u64,
        max_bytes: u64,
    ) -> FsResult<FsReadChunkResult> {
        let reader = self.reader(handle)?;
        let mut reader = reader.lock().await;
        let max = usize::try_from(max_bytes.clamp(1, MAX_FS_CHUNK_BYTES)).unwrap_or(PIPE_CHUNK);
        let (data, summary) = reader.chunk(max).await?;
        Ok(FsReadChunkResult {
            data: base64::engine::general_purpose::STANDARD.encode(&data),
            eof: summary.is_some(),
            summary,
        })
    }

    /// `fs/write_open`.
    pub(crate) async fn write_open(
        &self,
        route: u64,
        kind: FsKind,
        overwrite: bool,
    ) -> FsResult<u64> {
        let route = self.take_route(route, FsDirection::Write)?;
        let journal = self.journal.clone();
        let writer = Writer::open(route, kind, overwrite, journal).await?;
        Ok(self.insert(Entry::Write(Arc::new(tokio::sync::Mutex::new(writer)))))
    }

    /// `fs/write_chunk`.
    pub(crate) async fn write_chunk(&self, handle: u64, offset: u64, data: &str) -> FsResult<()> {
        let data = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| FsError::Invalid(format!("data is not base64: {error}")))?;
        if data.len() as u64 > MAX_FS_CHUNK_BYTES {
            return Err(FsError::Invalid(format!(
                "a chunk holds at most {MAX_FS_CHUNK_BYTES} bytes"
            )));
        }
        let writer = self.writer(handle)?;
        let mut writer = writer.lock().await;
        writer.write(offset, data).await
    }

    /// `fs/write_commit`: the handle is spent either way.
    pub(crate) async fn write_commit(&self, handle: u64, sha256: &str) -> FsResult<FsWritten> {
        let writer = self.writer(handle)?;
        let result = writer.lock().await.commit(sha256).await;
        lock(&self.entries).remove(&handle);
        result
    }

    /// `fs/close`: a route or handle the runtime no longer has is closed
    /// already.
    pub(crate) fn close(&self, handle: u64) {
        let entry = lock(&self.entries).remove(&handle);
        if let Some(Entry::Copying(stop)) = &entry {
            stop.cancel();
        }
        drop(entry);
    }

    /// `fs/copy`: both routes on this runtime, copied without the daemon.
    pub(crate) async fn copy(&self, from: u64, to: u64, overwrite: bool) -> FsResult<FsWritten> {
        let source = self.take_route(from, FsDirection::Read)?;
        let destination = self.take_route(to, FsDirection::Write)?;
        if source.kind == Some(FsKind::Folder) && destination.resolved.starts_with(&source.resolved)
        {
            return Err(FsError::Invalid(format!(
                "{} is inside {}: a folder cannot be copied into itself",
                destination.shown.display(),
                source.shown.display()
            )));
        }
        let stop = tokio_util::sync::CancellationToken::new();
        lock(&self.entries).insert(to, Entry::Copying(stop.clone()));
        let copied = tokio::select! {
            copied = Self::copy_open(source, destination, overwrite, self.journal.clone()) => copied,
            () = stop.cancelled() => Err(FsError::Failed("the copy was stopped".into())),
        };
        lock(&self.entries).remove(&to);
        copied
    }

    async fn copy_open(
        source: Route,
        destination: Route,
        overwrite: bool,
        journal: Option<PathBuf>,
    ) -> FsResult<FsWritten> {
        let mut reader = Reader::open(&source).await?;
        let mut writer = Writer::open(destination, reader.kind(), overwrite, journal).await?;
        let mut offset = 0;
        let summary = loop {
            let (data, summary) = reader.chunk(MAX_FS_CHUNK_BYTES as usize).await?;
            let length = data.len() as u64;
            if length > 0 {
                writer.write(offset, data).await?;
                offset += length;
            }
            if let Some(summary) = summary {
                break summary;
            }
        };
        let mut written = writer.commit(&summary.sha256).await?;
        written.skipped += summary.skipped;
        Ok(written)
    }
}

/// Resolves `path` for `direction` as the file tools do: the same lexical
/// normalization, the route followed once.
fn resolve(workspace: &Path, path: &str, direction: FsDirection) -> FsResult<(Route, Option<u64>)> {
    if path.is_empty() {
        return Err(FsError::Invalid("the path is empty".into()));
    }
    let tool_error =
        |error: cookie_agent_runtime_api::ToolError| FsError::Failed(error.to_string());
    let target = crate::fs_cap::prepare_target(workspace, Path::new(path)).map_err(tool_error)?;
    let shown = target.display_path().to_owned();
    let resolved = target.resolved_path().to_owned();
    let kind = match &target {
        crate::fs_cap::PreparedTarget::Existing(existing) if existing.directory => {
            Some(FsKind::Folder)
        }
        crate::fs_cap::PreparedTarget::Existing(_) => Some(FsKind::File),
        crate::fs_cap::PreparedTarget::Absent(_) => None,
    };
    drop(target);
    if direction == FsDirection::Read && kind.is_none() {
        return Err(FsError::Failed(
            cookie_agent_runtime_api::text_tools::not_found(&shown.display().to_string()),
        ));
    }
    let size = match kind {
        Some(FsKind::File) => Some(
            fs::metadata(&resolved)
                .map_err(|error| FsError::io("reading", &shown, &error))?
                .len(),
        ),
        _ => None,
    };
    if direction == FsDirection::Write && is_partial(resolved.file_name()) {
        return Err(FsError::Invalid(format!(
            "{} is the name of a transfer's partial copy",
            shown.display()
        )));
    }
    Ok((
        Route {
            direction,
            shown,
            resolved,
            kind,
        },
        size,
    ))
}

fn is_partial(name: Option<&std::ffi::OsStr>) -> bool {
    name.and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| name.starts_with('.') && name.contains(PARTIAL_MARKER))
}

/// SHA-256 as lowercase hex.
fn hex(digest: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    digest
        .as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

// --- reading -------------------------------------------------------------

/// What a folder's packing counted.
#[derive(Default)]
struct Counted {
    files: u64,
    bytes: u64,
    skipped: u64,
}

enum Reader {
    File {
        file: tokio::fs::File,
        size: u64,
        read: u64,
        hasher: Sha256,
        done: Option<FsStreamSummary>,
    },
    Folder {
        pieces: mpsc::Receiver<Vec<u8>>,
        pending: Vec<u8>,
        packing: Option<tokio::task::JoinHandle<io::Result<Counted>>>,
        hasher: Sha256,
        done: Option<FsStreamSummary>,
        shown: PathBuf,
    },
}

impl Reader {
    async fn open(route: &Route) -> FsResult<Self> {
        match route.kind {
            Some(FsKind::File) => {
                let file = tokio::fs::File::open(&route.resolved)
                    .await
                    .map_err(|error| FsError::io("opening", &route.shown, &error))?;
                let metadata = file
                    .metadata()
                    .await
                    .map_err(|error| FsError::io("reading", &route.shown, &error))?;
                if !metadata.is_file() {
                    return Err(FsError::Failed(format!(
                        "{} is no longer a file",
                        route.shown.display()
                    )));
                }
                Ok(Self::File {
                    file,
                    size: metadata.len(),
                    read: 0,
                    hasher: Sha256::new(),
                    done: None,
                })
            }
            Some(FsKind::Folder) => {
                let (sender, pieces) = mpsc::channel(PIPE_DEPTH);
                let root = route.resolved.clone();
                let packing =
                    tokio::task::spawn_blocking(move || pack(&root, PipeWriter::new(sender)));
                Ok(Self::Folder {
                    pieces,
                    pending: Vec::new(),
                    packing: Some(packing),
                    hasher: Sha256::new(),
                    done: None,
                    shown: route.shown.clone(),
                })
            }
            None => Err(FsError::Failed(
                cookie_agent_runtime_api::text_tools::not_found(&route.shown.display().to_string()),
            )),
        }
    }

    fn kind(&self) -> FsKind {
        match self {
            Self::File { .. } => FsKind::File,
            Self::Folder { .. } => FsKind::Folder,
        }
    }

    fn size(&self) -> Option<u64> {
        match self {
            Self::File { size, .. } => Some(*size),
            Self::Folder { .. } => None,
        }
    }

    /// Up to `max` bytes, and the summary once nothing follows them.
    async fn chunk(&mut self, max: usize) -> FsResult<(Vec<u8>, Option<FsStreamSummary>)> {
        match self {
            Self::File {
                file,
                read,
                hasher,
                done,
                ..
            } => {
                if let Some(summary) = done {
                    return Ok((Vec::new(), Some(summary.clone())));
                }
                let mut data = vec![0; max];
                let mut filled = 0;
                while filled < max {
                    let count = file
                        .read(&mut data[filled..])
                        .await
                        .map_err(|error| FsError::Failed(format!("reading the file: {error}")))?;
                    if count == 0 {
                        break;
                    }
                    filled += count;
                }
                data.truncate(filled);
                hasher.update(&data);
                *read += filled as u64;
                if filled < max {
                    let summary = FsStreamSummary {
                        sha256: hex(std::mem::take(hasher).finalize()),
                        bytes: *read,
                        files: 1,
                        skipped: 0,
                    };
                    *done = Some(summary.clone());
                    return Ok((data, Some(summary)));
                }
                Ok((data, None))
            }
            Self::Folder {
                pieces,
                pending,
                packing,
                hasher,
                done,
                shown,
            } => {
                if let Some(summary) = done {
                    return Ok((Vec::new(), Some(summary.clone())));
                }
                while pending.len() < max {
                    match pieces.recv().await {
                        Some(piece) => pending.extend_from_slice(&piece),
                        None => break,
                    }
                }
                let data: Vec<u8> = pending.drain(..pending.len().min(max)).collect();
                hasher.update(&data);
                if !pending.is_empty() || data.len() == max {
                    return Ok((data, None));
                }
                // The stream has ended: the packing thread has returned.
                let counted = match packing.take() {
                    Some(packing) => packing
                        .await
                        .map_err(|error| FsError::Failed(format!("packing failed: {error}")))?
                        .map_err(|error| FsError::io("reading", shown, &error))?,
                    None => Counted::default(),
                };
                let summary = FsStreamSummary {
                    sha256: hex(std::mem::take(hasher).finalize()),
                    bytes: counted.bytes,
                    files: counted.files,
                    skipped: counted.skipped,
                };
                *done = Some(summary.clone());
                Ok((data, Some(summary)))
            }
        }
    }
}

/// Writes into pieces sent to the requests that read them.
struct PipeWriter {
    sender: mpsc::Sender<Vec<u8>>,
    buffer: Vec<u8>,
}

impl PipeWriter {
    fn new(sender: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            sender,
            buffer: Vec::with_capacity(PIPE_CHUNK),
        }
    }

    fn send(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let piece = std::mem::replace(&mut self.buffer, Vec::with_capacity(PIPE_CHUNK));
        self.sender
            .blocking_send(piece)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the transfer was closed"))
    }
}

impl Write for PipeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = PIPE_CHUNK - self.buffer.len();
        let taken = bytes.len().min(room);
        self.buffer.extend_from_slice(&bytes[..taken]);
        if self.buffer.len() == PIPE_CHUNK {
            self.send()?;
        }
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

/// Packs the folder at `root` into `out` as a tar stream: regular files,
/// folders, and links as links, with mode bits and modification times and
/// no owners. Devices, sockets, FIFOs, and partial copies are left out.
fn pack(root: &Path, out: PipeWriter) -> io::Result<Counted> {
    let mut builder = tar::Builder::new(out);
    builder.follow_symlinks(false);
    let mut counted = Counted::default();
    pack_folder(&mut builder, root, Path::new(""), &mut counted)?;
    let mut out = builder.into_inner()?;
    out.flush()?;
    Ok(counted)
}

fn pack_folder(
    builder: &mut tar::Builder<PipeWriter>,
    root: &Path,
    relative: &Path,
    counted: &mut Counted,
) -> io::Result<()> {
    let mut names = fs::read_dir(root.join(relative))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    for name in names {
        if is_partial(Some(&name)) {
            continue;
        }
        let path = relative.join(&name);
        let full = root.join(&path);
        let metadata = fs::symlink_metadata(&full)?;
        let file_type = metadata.file_type();
        let mut header = tar::Header::new_gnu();
        header.set_mode(mode_of(&metadata));
        header.set_mtime(
            metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_secs()),
        );
        header.set_uid(0);
        header.set_gid(0);
        if file_type.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            builder.append_data(&mut header, &path, io::empty())?;
            pack_folder(builder, root, &path, counted)?;
        } else if file_type.is_symlink() {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            let target = fs::read_link(&full)?;
            builder.append_link(&mut header, &path, &target)?;
        } else if file_type.is_file() {
            let length = metadata.len();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(length);
            let file = fs::File::open(&full)?;
            builder.append_data(
                &mut header,
                &path,
                Exact {
                    inner: file.take(length),
                    left: length,
                },
            )?;
            counted.files += 1;
            counted.bytes += length;
        } else {
            counted.skipped += 1;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn mode_of(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(metadata: &fs::Metadata) -> u32 {
    match (metadata.is_dir(), metadata.permissions().readonly()) {
        (true, _) => 0o755,
        (false, true) => 0o444,
        (false, false) => 0o644,
    }
}

/// A file's bytes as its header announced them: one that shrank while it
/// was read fails instead of corrupting the stream.
struct Exact<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let count = self.inner.read(buffer)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "a file shrank while it was read",
            ));
        }
        self.left -= count as u64;
        Ok(count)
    }
}

// --- writing -------------------------------------------------------------

enum Sink {
    File(tokio::fs::File),
    Folder {
        pieces: Option<mpsc::Sender<Vec<u8>>>,
        unpacking: Option<tokio::task::JoinHandle<io::Result<Counted>>>,
    },
}

struct Writer {
    route: Route,
    overwrite: bool,
    /// The hidden sibling written until commit.
    partial: PathBuf,
    /// The record that lets a later start remove the partial copy.
    journal_entry: Option<PathBuf>,
    sink: Sink,
    taken: u64,
    hasher: Sha256,
    committed: bool,
}

impl Writer {
    async fn open(
        route: Route,
        kind: FsKind,
        overwrite: bool,
        journal: Option<PathBuf>,
    ) -> FsResult<Self> {
        let shown = route.shown.display().to_string();
        match (route.kind, overwrite) {
            (Some(existing), false) => {
                return Err(FsError::Failed(format!(
                    "{shown} exists already ({}); pass overwrite to replace it",
                    existing.as_str()
                )));
            }
            (Some(existing), true) if existing != kind => {
                return Err(FsError::Failed(format!(
                    "{shown} is a {}, and the source is a {}",
                    existing.as_str(),
                    kind.as_str()
                )));
            }
            _ => {}
        }
        let parent = route
            .resolved
            .parent()
            .ok_or_else(|| FsError::Invalid(format!("{shown} has no parent folder")))?
            .to_owned();
        let name = route
            .resolved
            .file_name()
            .ok_or_else(|| FsError::Invalid(format!("{shown} has no name")))?
            .to_string_lossy()
            .into_owned();
        tokio::fs::create_dir_all(&parent)
            .await
            .map_err(|error| FsError::io("creating", &parent, &error))?;
        let partial = parent.join(format!(".{name}{PARTIAL_MARKER}{}", random_id()));
        let journal_entry = journal.and_then(|journal| record_partial(&journal, &partial));
        let sink = match kind {
            FsKind::File => Sink::File(
                tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&partial)
                    .await
                    .map_err(|error| FsError::io("creating", &partial, &error))?,
            ),
            FsKind::Folder => {
                tokio::fs::create_dir(&partial)
                    .await
                    .map_err(|error| FsError::io("creating", &partial, &error))?;
                let (sender, pieces) = mpsc::channel(PIPE_DEPTH);
                let into = partial.clone();
                Sink::Folder {
                    pieces: Some(sender),
                    unpacking: Some(tokio::task::spawn_blocking(move || {
                        unpack(PipeReader::new(pieces), &into)
                    })),
                }
            }
        };
        Ok(Self {
            route,
            overwrite,
            partial,
            journal_entry,
            sink,
            taken: 0,
            hasher: Sha256::new(),
            committed: false,
        })
    }

    async fn write(&mut self, offset: u64, data: Vec<u8>) -> FsResult<()> {
        if offset != self.taken {
            return Err(FsError::Invalid(format!(
                "the chunk starts at {offset}, but {} bytes came before it",
                self.taken
            )));
        }
        self.hasher.update(&data);
        self.taken += data.len() as u64;
        match &mut self.sink {
            Sink::File(file) => file
                .write_all(&data)
                .await
                .map_err(|error| FsError::io("writing", &self.route.shown, &error)),
            Sink::Folder { pieces, unpacking } => {
                let sent = match pieces {
                    Some(pieces) => pieces.send(data).await.is_ok(),
                    None => false,
                };
                if sent {
                    return Ok(());
                }
                // Unpacking stopped: say why.
                *pieces = None;
                Err(match unpacking.take() {
                    Some(unpacking) => match unpacking.await {
                        Ok(Err(error)) => FsError::io("unpacking into", &self.route.shown, &error),
                        Ok(Ok(_)) => FsError::Invalid("bytes after the end of the folder".into()),
                        Err(error) => FsError::Failed(format!("unpacking failed: {error}")),
                    },
                    None => FsError::Failed("unpacking has already failed".into()),
                })
            }
        }
    }

    async fn commit(&mut self, sha256: &str) -> FsResult<FsWritten> {
        let digest = hex(std::mem::take(&mut self.hasher).finalize());
        let mut skipped = 0;
        let (files, bytes) = match &mut self.sink {
            Sink::File(file) => {
                file.sync_all()
                    .await
                    .map_err(|error| FsError::io("writing", &self.route.shown, &error))?;
                (1, self.taken)
            }
            Sink::Folder { pieces, unpacking } => {
                // The end of the stream: unpacking finishes.
                *pieces = None;
                let counted = match unpacking.take() {
                    Some(unpacking) => unpacking
                        .await
                        .map_err(|error| FsError::Failed(format!("unpacking failed: {error}")))?
                        .map_err(|error| {
                            FsError::io("unpacking into", &self.route.shown, &error)
                        })?,
                    None => return Err(FsError::Failed("unpacking has already failed".into())),
                };
                skipped = counted.skipped;
                (counted.files, counted.bytes)
            }
        };
        if digest != sha256 {
            return Err(FsError::Failed(format!(
                "the bytes written to {} do not match the source: SHA-256 {digest}, expected \
                 {sha256}",
                self.route.shown.display()
            )));
        }
        let partial = self.partial.clone();
        let destination = self.route.resolved.clone();
        let overwrite = self.overwrite;
        let folder = matches!(self.sink, Sink::Folder { .. });
        let shown = self.route.shown.clone();
        tokio::task::spawn_blocking(move || {
            put_in_place(&partial, &destination, folder, overwrite, &shown)
        })
        .await
        .map_err(|error| FsError::Failed(format!("putting the copy in place failed: {error}")))??;
        self.committed = true;
        if let Some(entry) = self.journal_entry.take() {
            let _ = fs::remove_file(entry);
        }
        Ok(FsWritten {
            path: self.route.shown.to_string_lossy().into_owned(),
            bytes,
            files,
            sha256: digest,
            skipped,
        })
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Sink::Folder { pieces, .. } = &mut self.sink {
            *pieces = None;
        }
        // An unpacking thread still writing into the folder ends at the
        // closed stream; whatever it wrote after this is left for the next
        // start's sweep.
        let _ = if self.partial.is_dir() {
            fs::remove_dir_all(&self.partial)
        } else {
            fs::remove_file(&self.partial)
        };
        if let Some(entry) = self.journal_entry.take() {
            let _ = fs::remove_file(entry);
        }
    }
}

/// Moves the finished partial copy to `destination`: a file replaces what is
/// there with `overwrite` and never otherwise; a folder is renamed into
/// place, or merged into the folder there with `overwrite`.
fn put_in_place(
    partial: &Path,
    destination: &Path,
    folder: bool,
    overwrite: bool,
    shown: &Path,
) -> FsResult<()> {
    let exists = fs::symlink_metadata(destination).ok();
    if let Some(existing) = &exists {
        if !overwrite {
            return Err(FsError::Failed(format!(
                "{} was created meanwhile; pass overwrite to replace it",
                shown.display()
            )));
        }
        if existing.is_dir() != folder {
            return Err(FsError::Failed(format!(
                "{} is no longer a {}",
                shown.display(),
                if folder { "folder" } else { "file" }
            )));
        }
    }
    let renamed = match (&exists, folder) {
        (Some(_), true) => merge(partial, destination).and_then(|()| fs::remove_dir_all(partial)),
        (Some(_), false) => fs::rename(partial, destination),
        // Without overwrite a file never replaces one created meanwhile:
        // a hard link fails when the name is taken.
        (None, false) => match fs::hard_link(partial, destination) {
            Ok(()) => fs::remove_file(partial),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(error),
            Err(_) => fs::rename(partial, destination),
        },
        (None, true) => fs::rename(partial, destination),
    };
    renamed.map_err(|error| FsError::io("putting in place", shown, &error))
}

/// Moves everything in `from` into the folder `into`, replacing files and
/// links of the same name and merging folders.
fn merge(from: &Path, into: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = into.join(entry.file_name());
        let source_is_folder = entry.file_type()?.is_dir();
        match fs::symlink_metadata(&target) {
            Ok(existing) if existing.is_dir() && source_is_folder => merge(&source, &target)?,
            Ok(existing) => {
                if existing.is_dir() {
                    fs::remove_dir_all(&target)?;
                } else if source_is_folder {
                    fs::remove_file(&target)?;
                }
                fs::rename(&source, &target)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::rename(&source, &target)?,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Reads the pieces the write requests send.
struct PipeReader {
    pieces: mpsc::Receiver<Vec<u8>>,
    current: io::Cursor<Vec<u8>>,
}

impl PipeReader {
    fn new(pieces: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            pieces,
            current: io::Cursor::new(Vec::new()),
        }
    }
}

impl Read for PipeReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let count = Read::read(&mut self.current, buffer)?;
            if count > 0 || buffer.is_empty() {
                return Ok(count);
            }
            match self.pieces.blocking_recv() {
                Some(piece) => self.current = io::Cursor::new(piece),
                None => return Ok(0),
            }
        }
    }
}

/// Unpacks the tar stream `input` into the empty folder `into`. Names must
/// be relative and plain (no `..`), and no entry may lie below a link the
/// stream made; a link that may lead outside `into` is left out and counted
/// as skipped. Only files, folders, and links are taken.
fn unpack(mut input: PipeReader, into: &Path) -> io::Result<Counted> {
    let refused = |what: String| io::Error::new(io::ErrorKind::InvalidData, what);
    let mut counted = Counted::default();
    let mut folders = Vec::new();
    {
        let mut archive = tar::Archive::new(&mut input);
        archive.set_preserve_mtime(true);
        let mut links = HashSet::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if path.as_os_str().is_empty()
                || !path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
            {
                return Err(refused(format!("the stream names {}", path.display())));
            }
            if path
                .ancestors()
                .skip(1)
                .any(|ancestor| links.contains(ancestor))
            {
                return Err(refused(format!(
                    "{} lies below a link the stream made",
                    path.display()
                )));
            }
            let target = into.join(&path);
            match entry.header().entry_type() {
                tar::EntryType::Directory => {
                    fs::create_dir_all(&target)?;
                    folders.push((target, entry.header().mode().unwrap_or(0o755)));
                }
                tar::EntryType::Regular | tar::EntryType::Continuous => {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let size = entry.header().size()?;
                    entry.unpack(&target)?;
                    counted.files += 1;
                    counted.bytes += size;
                }
                tar::EntryType::Symlink => {
                    let link = entry
                        .link_name()?
                        .ok_or_else(|| refused(format!("{} links nowhere", path.display())))?
                        .into_owned();
                    if !stays_inside(&path, &link) {
                        // Such as a virtual environment's absolute links:
                        // left out and counted, as the source leaves out
                        // devices.
                        counted.skipped += 1;
                        continue;
                    }
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    entry.unpack(&target)?;
                    links.insert(path);
                }
                other => {
                    return Err(refused(format!(
                        "{} is a {other:?} entry, which a transfer does not take",
                        path.display()
                    )));
                }
            }
        }
    }
    // The end-of-stream padding, so the sender is never left waiting.
    io::copy(&mut input, &mut io::sink())?;
    // Last, and deepest first, so a read-only folder was writable while it
    // was filled.
    for (folder, mode) in folders.into_iter().rev() {
        set_folder_mode(&folder, mode)?;
    }
    Ok(counted)
}

#[cfg(unix)]
fn set_folder_mode(folder: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(folder, fs::Permissions::from_mode(mode & 0o777))
}

#[cfg(not(unix))]
fn set_folder_mode(_: &Path, _: u32) -> io::Result<()> {
    Ok(())
}

/// Whether the link at `path` (relative to the folder's root) to `link`
/// stays inside the folder: relative, and climbing (`..`) only before its
/// first name and no higher than the root. No entry lies below a link, so a
/// link's own folder and its ancestors are real folders; a target that climbs
/// to one of them and then only descends stays inside even through other
/// links, which stay inside in turn.
fn stays_inside(path: &Path, link: &Path) -> bool {
    if link.is_absolute() || link.has_root() {
        return false;
    }
    let mut depth: usize = path.components().count().saturating_sub(1);
    let mut descended = false;
    for component in link.components() {
        match component {
            Component::Normal(_) => descended = true,
            Component::CurDir => {}
            Component::ParentDir if !descended => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return false,
            },
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

fn random_id() -> String {
    let mut bytes = [0; 8];
    if getrandom::getrandom(&mut bytes).is_err() {
        bytes = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos() as u64)
            .to_le_bytes();
    }
    hex(bytes)
}

// --- crash leftovers -------------------------------------------------------

/// Records `partial` in `journal`, returning the record's path.
fn record_partial(journal: &Path, partial: &Path) -> Option<PathBuf> {
    fs::create_dir_all(journal).ok()?;
    let entry = journal.join(random_id());
    fs::write(&entry, partial.as_os_str().to_string_lossy().as_bytes()).ok()?;
    Some(entry)
}

/// Removes the partial copies recorded in `journal` more than a day ago,
/// which a crash left behind, and their records.
pub(crate) fn sweep_stale_partials(journal: &Path) {
    let Ok(entries) = fs::read_dir(journal) else {
        return;
    };
    for entry in entries.flatten() {
        let record = entry.path();
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > STALE_PARTIAL_AGE);
        if !stale {
            continue;
        }
        if let Ok(partial) = fs::read_to_string(&record) {
            let partial = PathBuf::from(partial);
            // Only ever a partial copy's name.
            if is_partial(partial.file_name()) {
                let _ = if partial.is_dir() {
                    fs::remove_dir_all(&partial)
                } else {
                    fs::remove_file(&partial)
                };
            }
        }
        let _ = fs::remove_file(record);
    }
}

#[cfg(test)]
mod tests;
