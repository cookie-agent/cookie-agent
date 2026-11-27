//! Folder artifacts and assets (`docs/site/specs/folder-artifacts.md`,
//! client protocol 1.104.0): a folder with a main file, stored as its files
//! and one [`FolderManifest`] in a store's content-addressed blobs.
//!
//! The folder's digest is its manifest's. The manifest is written in one
//! canonical form ([`FolderManifest::to_canonical_json`]), so the same
//! folder always has the same digest. Its entries name each file with the
//! `sha256`, `byte_length`, and `media_type` members that the engine's
//! reference walk already follows, so a fork or revert that keeps a
//! manifest keeps its files.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ArtifactRef, SessionId, human_bytes};

/// The `cookie_folder` version of the manifest written now.
pub const FOLDER_MANIFEST_VERSION: u32 = 1;
/// The most bytes a folder's files hold together (section 7.1).
pub const FOLDER_MAX_BYTES: u64 = 200 * 1024 * 1024;
/// The most files one folder holds.
pub const FOLDER_MAX_ENTRIES: usize = 2000;
/// The most bytes of one path in a folder.
pub const FOLDER_MAX_PATH_BYTES: usize = 1024;
/// The start of every canonical manifest, by which a reader tells a folder
/// from a file without parsing it.
pub const FOLDER_MANIFEST_PREFIX: &[u8] = b"{\"cookie_folder\":";
/// `artifact.manifest`.
pub const ARTIFACT_MANIFEST_METHOD: &str = "artifact.manifest";
/// `asset.manifest`.
pub const ASSET_MANIFEST_METHOD: &str = "asset.manifest";

/// What a reference to a folder says about it (section 3.1). The
/// reference's `sha256` and `byte_length` are the manifest blob's, so the
/// store checks the blob as it checks any, and `media_type` is the main
/// file's.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
pub struct FolderInfo {
    /// The main file's path in the folder.
    #[schemars(length(min = 1, max = FOLDER_MAX_PATH_BYTES))]
    pub main: String,
    /// How many files the folder holds.
    pub files: u32,
    /// The bytes of its files together.
    pub bytes: u64,
}

/// One file of a folder.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
pub struct FolderEntry {
    /// Relative and `/`-separated, with no empty, `.`, or `..` component.
    #[schemars(length(min = 1, max = FOLDER_MAX_PATH_BYTES))]
    pub path: String,
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub sha256: String,
    pub byte_length: u64,
    pub media_type: String,
}

/// A folder's list of files (section 3). Its members are in the order the
/// canonical form writes them.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct FolderManifest {
    pub cookie_folder: u32,
    pub main: String,
    pub entries: Vec<FolderEntry>,
}

/// Why a manifest or a path is not one a folder may hold.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FolderError {
    /// A path that is empty, absolute, too long, or has an empty, `.`, or
    /// `..` component, or a control character.
    Path(String),
    /// Two entries with one path.
    Duplicate(String),
    /// `main` names no entry.
    Main(String),
    /// More entries than [`FOLDER_MAX_ENTRIES`], or none.
    Entries(usize),
    /// More bytes than [`FOLDER_MAX_BYTES`].
    Bytes(u64),
    /// A digest that is not 64 lowercase hex digits.
    Digest(String),
    /// Not a manifest of a version this build reads.
    Format(String),
}

impl std::fmt::Display for FolderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(path) => write!(f, "{path:?} is not a path a folder may hold"),
            Self::Duplicate(path) => write!(f, "the folder holds {path:?} twice"),
            Self::Main(path) => write!(f, "the folder has no file {path:?}"),
            Self::Entries(count) if *count == 0 => f.write_str("the folder holds no files"),
            Self::Entries(count) => write!(
                f,
                "the folder holds {count} files, more than the {FOLDER_MAX_ENTRIES} a folder may"
            ),
            Self::Bytes(bytes) => write!(
                f,
                "the folder's files hold {}, more than the {} a folder may",
                human_bytes(*bytes),
                human_bytes(FOLDER_MAX_BYTES)
            ),
            Self::Digest(digest) => write!(f, "{digest:?} is not a SHA-256 digest"),
            Self::Format(reason) => write!(f, "not a folder manifest: {reason}"),
        }
    }
}

impl std::error::Error for FolderError {}

/// Checks `path` as a folder's entry path.
pub fn validate_folder_path(path: &str) -> Result<(), FolderError> {
    let bad = path.is_empty()
        || path.len() > FOLDER_MAX_PATH_BYTES
        || path.starts_with('/')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if bad {
        Err(FolderError::Path(path.to_owned()))
    } else {
        Ok(())
    }
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl FolderManifest {
    /// A manifest of `entries` with `main`, sorted and checked.
    pub fn new(main: String, mut entries: Vec<FolderEntry>) -> Result<Self, FolderError> {
        entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        let manifest = Self {
            cookie_folder: FOLDER_MANIFEST_VERSION,
            main,
            entries,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Reads a manifest written by [`Self::to_canonical_json`] or by an
    /// earlier build, and checks it.
    pub fn parse(bytes: &[u8]) -> Result<Self, FolderError> {
        if !bytes.starts_with(FOLDER_MANIFEST_PREFIX) {
            return Err(FolderError::Format("no cookie_folder member first".into()));
        }
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| FolderError::Format(error.to_string()))?;
        if manifest.cookie_folder != FOLDER_MANIFEST_VERSION {
            return Err(FolderError::Format(format!(
                "version {} of the manifest",
                manifest.cookie_folder
            )));
        }
        manifest.validate()?;
        Ok(manifest)
    }

    /// Checks every rule of section 3.
    pub fn validate(&self) -> Result<(), FolderError> {
        if self.entries.is_empty() || self.entries.len() > FOLDER_MAX_ENTRIES {
            return Err(FolderError::Entries(self.entries.len()));
        }
        let mut bytes = 0u64;
        let mut previous: Option<&str> = None;
        for entry in &self.entries {
            validate_folder_path(&entry.path)?;
            if !is_digest(&entry.sha256) {
                return Err(FolderError::Digest(entry.sha256.clone()));
            }
            if let Some(previous) = previous {
                match previous.as_bytes().cmp(entry.path.as_bytes()) {
                    std::cmp::Ordering::Equal => {
                        return Err(FolderError::Duplicate(entry.path.clone()));
                    }
                    std::cmp::Ordering::Greater => {
                        return Err(FolderError::Format("entries out of order".into()));
                    }
                    std::cmp::Ordering::Less => {}
                }
            }
            previous = Some(&entry.path);
            bytes = bytes.saturating_add(entry.byte_length);
        }
        if bytes > FOLDER_MAX_BYTES {
            return Err(FolderError::Bytes(bytes));
        }
        if self.entry(&self.main).is_none() {
            return Err(FolderError::Main(self.main.clone()));
        }
        Ok(())
    }

    /// The canonical form: members in declaration order, entries sorted by
    /// path as bytes, no whitespace.
    #[must_use]
    pub fn to_canonical_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a manifest serializes")
    }

    /// The entry at `path`.
    #[must_use]
    pub fn entry(&self, path: &str) -> Option<&FolderEntry> {
        self.entries
            .binary_search_by(|entry| entry.path.as_bytes().cmp(path.as_bytes()))
            .ok()
            .map(|index| &self.entries[index])
    }

    /// The main file's entry.
    #[must_use]
    pub fn main_entry(&self) -> &FolderEntry {
        self.entry(&self.main)
            .expect("a checked manifest names its main file")
    }

    /// The files' bytes together.
    #[must_use]
    pub fn byte_length(&self) -> u64 {
        self.entries
            .iter()
            .map(|entry| entry.byte_length)
            .fold(0, u64::saturating_add)
    }

    /// What a reference to this folder says about it.
    #[must_use]
    pub fn info(&self) -> FolderInfo {
        FolderInfo {
            main: self.main.clone(),
            files: u32::try_from(self.entries.len()).unwrap_or(u32::MAX),
            bytes: self.byte_length(),
        }
    }

    /// The digests of the folder's files, once each.
    #[must_use]
    pub fn digests(&self) -> Vec<&str> {
        let mut seen = BTreeMap::new();
        for entry in &self.entries {
            seen.insert(entry.sha256.as_str(), ());
        }
        seen.into_keys().collect()
    }

    /// The reference to this folder, whose canonical manifest is
    /// `manifest_bytes` long and has digest `sha256`.
    #[must_use]
    pub fn artifact_ref(
        &self,
        sha256: String,
        manifest_bytes: u64,
        filename: Option<String>,
    ) -> ArtifactRef {
        ArtifactRef {
            sha256,
            byte_length: manifest_bytes,
            media_type: self.main_entry().media_type.clone(),
            filename,
            folder: Some(self.info()),
        }
    }
}

/// `artifact.manifest`: the files of a folder artifact in the store of
/// `session_id`'s tree.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactManifestParams {
    pub session_id: SessionId,
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub sha256: String,
}

/// The files of a folder, for `artifact.manifest` and `asset.manifest`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct FolderManifestResult {
    /// The manifest's digest, the folder's.
    pub sha256: String,
    pub main: String,
    pub entries: Vec<FolderEntry>,
}

impl FolderManifestResult {
    #[must_use]
    pub fn of(sha256: String, manifest: FolderManifest) -> Self {
        Self {
            sha256,
            main: manifest.main,
            entries: manifest.entries,
        }
    }
}

/// `asset.manifest`: the files of a folder asset's current content.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetManifestParams {
    pub store: crate::AssetStore,
    pub asset: crate::AssetId,
}
