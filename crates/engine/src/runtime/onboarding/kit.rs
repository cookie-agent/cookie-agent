//! The starter kit: a release's `cookie-agent-defaults.tar.gz`, holding a
//! `config.toml`, agent documents, and a `manifest.json` with each file's
//! SHA-256 (`docs/site/specs/default-models-and-onboarding.md` section 6).
//!
//! Its files are configuration documents and user skills
//! (`skills/<name>/SKILL.md`). An archive is read whole in memory, checked
//! against its manifest and against this build's configuration and skill
//! parsers, and only then compared with the user root: a file that does not exist is written, an identical one
//! skipped, and a changed one left alone with the kit's version written
//! beside it as `<name>.new`.

use std::{
    collections::BTreeMap,
    io::Read as _,
    path::{Path, PathBuf},
};

use cookie_agent_config::{ConfigPath, ConfigTree, load_from_tree, parse_skill_document};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// The release asset that carries the kit.
pub(crate) const KIT_ASSET: &str = "cookie-agent-defaults.tar.gz";
/// The largest compressed archive accepted.
pub(crate) const MAX_ARCHIVE_BYTES: usize = 1024 * 1024;
/// The most bytes an archive may unpack to.
const MAX_UNPACKED_BYTES: usize = 4 * 1024 * 1024;
/// The manifest's name in the archive.
const MANIFEST: &str = "manifest.json";

/// Why a kit was refused.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum KitError {
    #[error("the kit archive is not a gzip-compressed tar archive: {0}")]
    Archive(String),
    #[error("the kit archive is larger than {MAX_ARCHIVE_BYTES} bytes")]
    TooLarge,
    #[error("the kit's manifest.json is missing or invalid: {0}")]
    Manifest(String),
    #[error("`{0}` is not a file the kit may install")]
    UnsafePath(String),
    #[error("`{0}` is in the kit but not in its manifest")]
    Unlisted(String),
    #[error("`{0}` is in the kit's manifest but not in the kit")]
    Missing(String),
    #[error("`{path}` does not match the kit's manifest: its SHA-256 is {actual}")]
    HashMismatch { path: String, actual: String },
    #[error("the kit does not load with this build: {0}")]
    Invalid(String),
    #[error("the kit could not be fetched: {0}")]
    Unavailable(String),
}

/// `manifest.json`. Fields a later kit adds are ignored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct KitManifest {
    pub kit_version: u32,
    /// The cookie version the release that packed it was built for.
    #[serde(default)]
    pub cookie_version: Option<String>,
    /// Each file's lowercase hex SHA-256, by its path in the kit.
    pub files: BTreeMap<String, String>,
}

/// A file a kit may install, by its path below the user root.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum KitPath {
    /// `config.toml` or an agent document.
    Config(ConfigPath),
    /// `skills/<name>/SKILL.md`, a user skill.
    Skill(String),
}

impl KitPath {
    /// Validates a path of a kit's manifest.
    pub fn new(path: &str) -> Result<Self, KitError> {
        let unsafe_path = || KitError::UnsafePath(path.to_owned());
        if let Some(rest) = path.strip_prefix("skills/") {
            let name = rest.strip_suffix("/SKILL.md").ok_or_else(unsafe_path)?;
            let valid = !name.is_empty()
                && name.len() <= 64
                && !name.starts_with('-')
                && !name.ends_with('-')
                && !name.contains("--")
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            return valid
                .then(|| Self::Skill(name.to_owned()))
                .ok_or_else(unsafe_path);
        }
        ConfigPath::new(path)
            .map(Self::Config)
            .map_err(|_| unsafe_path())
    }

    /// The path below the user root.
    #[must_use]
    pub fn as_string(&self) -> String {
        match self {
            Self::Config(path) => path.as_str().to_owned(),
            Self::Skill(name) => format!("skills/{name}/SKILL.md"),
        }
    }
}

impl std::fmt::Display for KitPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.as_string())
    }
}

/// A kit whose files match its manifest and load with this build.
#[derive(Clone, Debug)]
pub struct Kit {
    pub manifest: KitManifest,
    /// The files to install, by their path below the user root.
    pub files: BTreeMap<KitPath, Vec<u8>>,
}

/// What installing a kit file does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KitFileAction {
    /// The file does not exist: it is written.
    Write,
    /// The file holds exactly the kit's version: nothing is written.
    Skip,
    /// The file differs: it stays, and the kit's version goes beside it as
    /// `<name>.new`.
    WriteNew,
}

/// One kit file against the user root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KitFilePlan {
    pub path: KitPath,
    pub action: KitFileAction,
    /// For [`KitFileAction::WriteNew`], a unified diff from the file to the
    /// kit's version.
    pub diff: Option<String>,
}

impl Kit {
    /// Reads a `.tar.gz` kit archive.
    pub fn from_archive(archive: &[u8]) -> Result<Self, KitError> {
        if archive.len() > MAX_ARCHIVE_BYTES {
            return Err(KitError::TooLarge);
        }
        let mut unpacked = Vec::new();
        flate2::read::GzDecoder::new(archive)
            .take(MAX_UNPACKED_BYTES as u64 + 1)
            .read_to_end(&mut unpacked)
            .map_err(|error| KitError::Archive(error.to_string()))?;
        if unpacked.len() > MAX_UNPACKED_BYTES {
            return Err(KitError::TooLarge);
        }
        Self::from_files(untar(&unpacked)?)
    }

    /// Checks `files`, the kit's files by their path in it, `manifest.json`
    /// included.
    pub fn from_files(mut files: BTreeMap<String, Vec<u8>>) -> Result<Self, KitError> {
        let manifest = files
            .remove(MANIFEST)
            .ok_or_else(|| KitError::Manifest("the kit has no manifest.json".into()))?;
        let manifest: KitManifest = serde_json::from_slice(&manifest)
            .map_err(|error| KitError::Manifest(error.to_string()))?;
        if manifest.files.is_empty() {
            return Err(KitError::Manifest("it lists no files".into()));
        }
        if let Some(path) = files
            .keys()
            .find(|path| !manifest.files.contains_key(*path))
        {
            return Err(KitError::Unlisted(path.clone()));
        }
        let mut checked = BTreeMap::new();
        for (path, expected) in &manifest.files {
            let kit_path = KitPath::new(path)?;
            let bytes = files
                .remove(path)
                .ok_or_else(|| KitError::Missing(path.clone()))?;
            let actual = format!("{:x}", Sha256::digest(&bytes));
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(KitError::HashMismatch {
                    path: path.clone(),
                    actual,
                });
            }
            checked.insert(kit_path, bytes);
        }
        let kit = Self {
            manifest,
            files: checked,
        };
        kit.validate()?;
        Ok(kit)
    }

    /// Loads the kit alone with this build's strict configuration, agent,
    /// and skill parsers.
    fn validate(&self) -> Result<(), KitError> {
        let invalid =
            |error: cookie_agent_config::ConfigError| KitError::Invalid(error.to_string());
        let mut tree = ConfigTree::new(None);
        for (path, bytes) in &self.files {
            match path {
                KitPath::Config(path) => tree.put(path.clone(), bytes.clone()).map_err(invalid)?,
                KitPath::Skill(name) => {
                    let text = std::str::from_utf8(bytes)
                        .map_err(|_| KitError::Invalid(format!("skill `{name}` is not UTF-8")))?;
                    let directory = Path::new("skills").join(name);
                    parse_skill_document(
                        text,
                        cookie_agent_protocol::SkillSource::User,
                        &directory.join("SKILL.md"),
                        &directory,
                        Vec::new(),
                    )
                    .map_err(invalid)?;
                }
            }
        }
        load_from_tree(&tree).map(drop).map_err(invalid)
    }

    /// The configuration documents of the kit.
    pub fn config_files(&self) -> impl Iterator<Item = (&ConfigPath, &Vec<u8>)> {
        self.files.iter().filter_map(|(path, bytes)| match path {
            KitPath::Config(path) => Some((path, bytes)),
            KitPath::Skill(_) => None,
        })
    }

    /// What installing the kit over the user root `root`, whose configuration
    /// documents read as `tree`, does with each file, in path order.
    #[must_use]
    pub fn plan(&self, tree: &ConfigTree, root: Option<&Path>) -> Vec<KitFilePlan> {
        self.files
            .iter()
            .map(|(path, kit)| match current(path, tree, root).as_deref() {
                None => KitFilePlan {
                    path: path.clone(),
                    action: KitFileAction::Write,
                    diff: None,
                },
                Some(current) if current == kit.as_slice() => KitFilePlan {
                    path: path.clone(),
                    action: KitFileAction::Skip,
                    diff: None,
                },
                Some(current) => KitFilePlan {
                    path: path.clone(),
                    action: KitFileAction::WriteNew,
                    diff: Some(unified_diff(&path.as_string(), current, kit)),
                },
            })
            .collect()
    }
}

/// What `path` holds now: from `tree` for a configuration document, from
/// disk below `root` for a skill.
fn current(path: &KitPath, tree: &ConfigTree, root: Option<&Path>) -> Option<Vec<u8>> {
    match path {
        KitPath::Config(path) => tree.get(path).map(<[u8]>::to_vec),
        KitPath::Skill(_) => {
            let file: PathBuf = root?.join(path.as_string());
            std::fs::read(file).ok()
        }
    }
}

fn unified_diff(path: &str, current: &[u8], kit: &[u8]) -> String {
    let current = String::from_utf8_lossy(current);
    let kit = String::from_utf8_lossy(kit);
    similar::TextDiff::from_lines(current.as_ref(), kit.as_ref())
        .unified_diff()
        .context_radius(3)
        .header(path, &format!("{path}.new"))
        .to_string()
}

/// The regular files of a tar archive by their normalized path. Directories
/// and pax headers are understood; any other entry kind is refused.
fn untar(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, KitError> {
    const BLOCK: usize = 512;
    let invalid = |reason: &str| KitError::Archive(reason.to_owned());
    let mut files = BTreeMap::new();
    let mut offset = 0;
    let mut pax_path = None;
    while offset + BLOCK <= bytes.len() {
        let header = &bytes[offset..offset + BLOCK];
        if header.iter().all(|byte| *byte == 0) {
            return Ok(files);
        }
        let size = octal(&header[124..136]).ok_or_else(|| invalid("a malformed entry size"))?;
        let start = offset + BLOCK;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| invalid("an entry runs past the end of the archive"))?;
        let data = &bytes[start..end];
        let name = c_string(&header[0..100]);
        let path = match pax_path.take() {
            Some(path) => path,
            None if &header[257..262] == b"ustar" => {
                let prefix = c_string(&header[345..500]);
                if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                }
            }
            None => name,
        };
        match header[156] {
            b'0' | 0 => {
                if let Some(path) = normalized(&path)?
                    && files.insert(path.clone(), data.to_vec()).is_some()
                {
                    return Err(KitError::Archive(format!("`{path}` appears twice")));
                }
            }
            b'5' => {}
            b'x' => pax_path = pax_record_path(data),
            b'g' => {}
            _ => {
                return Err(KitError::Archive(format!(
                    "`{path}` is neither a file nor a directory"
                )));
            }
        }
        offset = start + size.div_ceil(BLOCK) * BLOCK;
    }
    Err(invalid("the archive ends without its end marker"))
}

fn octal(field: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(field).ok()?;
    let text = text.trim_matches(|character: char| character == '\0' || character == ' ');
    if text.is_empty() {
        return Some(0);
    }
    usize::from_str_radix(text, 8).ok()
}

fn c_string(field: &[u8]) -> String {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// The `path` record of a pax extended header: `<length> path=<value>\n`.
fn pax_record_path(data: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(data).ok()?;
    text.lines().find_map(|record| {
        let (_, rest) = record.split_once(' ')?;
        rest.strip_prefix("path=").map(str::to_owned)
    })
}

/// A member's path without a leading `./`; `None` for the archive's root.
fn normalized(path: &str) -> Result<Option<String>, KitError> {
    let trimmed = path.trim_start_matches("./");
    if trimmed.is_empty() || trimmed == "." {
        return Ok(None);
    }
    if trimmed.starts_with('/')
        || trimmed.contains('\\')
        || trimmed
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(KitError::UnsafePath(path.to_owned()));
    }
    Ok(Some(trimmed.to_owned()))
}

/// Builds a `.tar.gz` of `files` as the release workflow packs the kit, for
/// tests: one ustar entry per file, under `./`.
#[cfg(any(test, feature = "test-support"))]
pub fn pack(files: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write as _;

    let mut tar = Vec::new();
    for (path, data) in files {
        let mut header = [0_u8; 512];
        let name = format!("./{path}");
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        ");
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        tar.extend_from_slice(&header);
        tar.extend_from_slice(data);
        tar.resize(tar.len().div_ceil(512) * 512, 0);
    }
    tar.extend_from_slice(&[0; 1024]);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar).expect("gzip in memory");
    encoder.finish().expect("gzip in memory")
}
