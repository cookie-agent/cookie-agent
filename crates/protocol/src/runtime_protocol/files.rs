//! The `fs/*` requests of a transfer (runtime protocol 1.13.0,
//! `docs/site/specs/machines-on-demand.md` section 6.3). The daemon makes
//! them of a runtime that reported `files` in its `runtime/initialize`
//! result and relays the bytes between two runtimes a chunk at a time,
//! storing none of them.
//!
//! A path is prepared once (`fs/prepare`): the runtime normalizes it as the
//! file tools do and resolves its route, which later requests use, so a link
//! retargeted after approval does not redirect the transfer. A folder moves
//! as a tar stream: regular files, folders, and symbolic links, with mode
//! bits and modification times and no owners.

use serde::{Deserialize, Serialize};

/// The most bytes one `fs/read_chunk` returns, and one `fs/write_chunk`
/// takes.
pub const MAX_FS_CHUNK_BYTES: u64 = 1024 * 1024;

/// What a transfer does with a prepared path.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FsDirection {
    /// The source: the path must exist.
    Read,
    /// The destination: the path may exist, and its parents need not.
    Write,
}

/// A file, or a folder moved with everything below it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FsKind {
    File,
    Folder,
}

impl FsKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Folder => "folder",
        }
    }
}

/// `fs/prepare`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsPrepareParams {
    /// The open instance whose workspace a relative path resolves against.
    pub instance: String,
    /// As the model wrote it; a leading `~` is the runtime's home.
    pub path: String,
    pub direction: FsDirection,
}

/// What `fs/prepare` resolved.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsPrepareResult {
    /// The route, until `fs/close` or an open consumes it.
    pub route: u64,
    /// The path made absolute and normalized lexically, as a result shows
    /// it.
    pub path: String,
    /// The path as a `read` or `write` rule matches it: as written,
    /// normalized lexically against the workspace, never resolved through
    /// symbolic links.
    pub label: String,
    /// Identifies the location the route resolved to, for approval
    /// bindings.
    pub binding: String,
    /// What is at the path: the source's kind, or what already exists at
    /// the destination (absent when nothing does).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<FsKind>,
    /// A file's size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// `fs/read_open`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsReadOpenParams {
    /// A route `fs/prepare` returned for reading; opening consumes it.
    pub route: u64,
}

/// What `fs/read_open` opened.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsReadOpenResult {
    pub handle: u64,
    pub kind: FsKind,
    /// A file's size in bytes; a folder's is known at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// `fs/read_chunk`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsReadChunkParams {
    pub handle: u64,
    /// At most [`MAX_FS_CHUNK_BYTES`].
    pub max_bytes: u64,
}

/// The next bytes of a read handle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsReadChunkResult {
    /// Standard base64; empty at the end.
    pub data: String,
    /// No bytes follow these.
    pub eof: bool,
    /// At the end: what the stream held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<FsStreamSummary>,
}

/// What a read stream held, at its end.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsStreamSummary {
    /// SHA-256 of the stream, lowercase hex: a file's own digest, or the
    /// tar stream's for a folder.
    pub sha256: String,
    /// The file's bytes, or the folder's files' bytes together.
    pub bytes: u64,
    /// Files in it: 1 for a file.
    pub files: u64,
    /// Entries of a folder left out: devices, sockets, and FIFOs.
    #[serde(default)]
    pub skipped: u64,
}

/// `fs/write_open`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsWriteOpenParams {
    /// A route `fs/prepare` returned for writing; opening consumes it.
    pub route: u64,
    pub kind: FsKind,
    /// Replace an existing file, or merge into an existing folder; without
    /// it an existing destination fails the open.
    #[serde(default)]
    pub overwrite: bool,
    /// The bytes to come, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// `fs/close`, and the result of `fs/write_open`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsHandle {
    /// A route or an open handle.
    pub handle: u64,
}

/// `fs/write_chunk`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsWriteChunkParams {
    pub handle: u64,
    /// Bytes the handle has taken before these: chunks come in order.
    pub offset: u64,
    /// Standard base64, at most [`MAX_FS_CHUNK_BYTES`] decoded.
    pub data: String,
}

/// `fs/write_commit`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsWriteCommitParams {
    pub handle: u64,
    /// The source's [`FsStreamSummary::sha256`]; a mismatch fails the
    /// commit and removes what was written.
    pub sha256: String,
}

/// What a commit or a copy put in place.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsWritten {
    /// The destination, as the runtime spells it.
    pub path: String,
    /// The file's bytes, or the folder's files' bytes together.
    pub bytes: u64,
    /// Files written: 1 for a file.
    pub files: u64,
    /// The stream's SHA-256, lowercase hex.
    pub sha256: String,
    /// Entries of a folder left out: links that may lead outside it, and,
    /// for `fs/copy`, the source's devices, sockets, and FIFOs.
    #[serde(default)]
    pub skipped: u64,
}

/// `fs/copy`: both routes of one runtime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct FsCopyParams {
    /// A route prepared for reading.
    pub from: u64,
    /// A route prepared for writing.
    pub to: u64,
    #[serde(default)]
    pub overwrite: bool,
}
