//! Fail-closed persistent provider credentials and durable idempotency receipts.

use std::{
    collections::BTreeMap,
    env, fmt, fs, io,
    path::{Path, PathBuf},
};

use cookie_agent_identity::ProviderId;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use uuid::Uuid;

const STORE_VERSION: u32 = 1;
const STORE_FILE: &str = "store-v1.json";
const LOCK_FILE: &str = "store-v1.lock";
const MAX_STORE_BYTES: u64 = 16 * 1024 * 1024;

/// One secret-bearing provider connection request.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConnectRequest {
    pub client_connect_id: String,
    pub provider_id: ProviderId,
    pub catalog_revision: String,
    pub credentials: BTreeMap<String, String>,
}

impl fmt::Debug for CredentialConnectRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialConnectRequest")
            .field("client_connect_id", &self.client_connect_id)
            .field("provider_id", &self.provider_id)
            .field("catalog_revision", &self.catalog_revision)
            .field("credentials", &"<redacted>")
            .finish()
    }
}

/// Safe durable result of one provider connection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConnectReceipt {
    pub client_connect_id: String,
    pub provider_id: ProviderId,
    pub credential_fields: Vec<String>,
    pub connected_at: String,
    pub catalog_revision: String,
    pub model_revision: String,
}

/// One stored provider connection. Debug never exposes values.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredConnection {
    provider_id: ProviderId,
    pub(crate) credentials: BTreeMap<String, String>,
    connected_at: String,
    pub(crate) catalog_revision: String,
}

impl fmt::Debug for StoredConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredConnection")
            .field("provider_id", &self.provider_id)
            .field(
                "credential_fields",
                &self.credentials.keys().collect::<Vec<_>>(),
            )
            .field("connected_at", &self.connected_at)
            .field("catalog_revision", &self.catalog_revision)
            .finish()
    }
}

/// Immutable credential-store state used to validate a candidate model snapshot.
#[derive(Clone)]
pub struct CredentialSnapshot {
    generation: Uuid,
    connections: BTreeMap<ProviderId, StoredConnection>,
}

impl CredentialSnapshot {
    #[must_use]
    pub const fn generation(&self) -> Uuid {
        self.generation
    }

    #[must_use]
    pub(crate) fn connections(&self) -> &BTreeMap<ProviderId, StoredConnection> {
        &self.connections
    }
}

impl fmt::Debug for CredentialSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialSnapshot")
            .field("generation", &self.generation)
            .field("providers", &self.connections.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Outcome of a durable connect transaction.
pub struct CredentialConnectOutcome<T> {
    pub receipt: CredentialConnectReceipt,
    pub candidate: Option<T>,
    pub replayed: bool,
}

/// Secure persistent credential store rooted at one private directory.
#[derive(Clone, Debug)]
pub struct CredentialStore {
    anchor: PathBuf,
    relative: PathBuf,
    root: PathBuf,
}

impl CredentialStore {
    /// Opens the standard per-user credential location.
    pub fn standard() -> Result<Self, CredentialStoreError> {
        let home = env::var_os("HOME").ok_or(CredentialStoreError::HomeUnavailable)?;
        let home = PathBuf::from(home);
        #[cfg(unix)]
        {
            prepare_standard_data_root(&home)?;
            let data_root = home.join(".local/share");
            Ok(Self::new_in(
                data_root,
                PathBuf::from("cookie_agent/credentials"),
            ))
        }
        #[cfg(not(unix))]
        {
            let _ = home;
            Err(CredentialStoreError::UnsupportedPlatform)
        }
    }

    /// Uses an explicit root, primarily for embedding and security tests.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        let anchor = root.parent().unwrap_or_else(|| Path::new("/")).to_owned();
        let relative = root
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            anchor,
            relative,
            root,
        }
    }

    /// Uses a trusted existing anchor and creates/traverses private relative directories.
    #[must_use]
    pub fn new_in(anchor: PathBuf, relative: PathBuf) -> Self {
        let root = anchor.join(&relative);
        Self {
            anchor,
            relative,
            root,
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Rereads the durable state while holding the cross-process lock.
    #[cfg(unix)]
    pub fn snapshot(&self) -> Result<CredentialSnapshot, CredentialStoreError> {
        self.with_locked_state(|_, state| {
            Ok(CredentialSnapshot {
                generation: state.generation,
                connections: state.connections.clone(),
            })
        })
    }

    /// Validates a full candidate under the lock, then durably writes and returns it.
    #[cfg(unix)]
    pub fn connect_with<T>(
        &self,
        request: &CredentialConnectRequest,
        validate: impl FnOnce(&CredentialSnapshot) -> Result<(String, T), CredentialStoreError>,
    ) -> Result<CredentialConnectOutcome<T>, CredentialStoreError> {
        validate_request(request)?;
        self.with_locked_state(|root, state| {
            let request_hmac = request_hmac(&state.hmac_key, request)?;
            if let Some(receipt) = state.receipts.get(&request.client_connect_id) {
                if receipt.request_hmac != request_hmac {
                    return Err(CredentialStoreError::IdempotencyConflict);
                }
                return Ok(CredentialConnectOutcome {
                    receipt: receipt.result.clone(),
                    candidate: None,
                    replayed: true,
                });
            }

            let connected_at = timestamp()?;
            let mut candidate = state.clone();
            candidate.generation = Uuid::now_v7();
            candidate.updated_at = connected_at.clone();
            candidate.connections.insert(
                request.provider_id.clone(),
                StoredConnection {
                    provider_id: request.provider_id.clone(),
                    credentials: request.credentials.clone(),
                    connected_at: connected_at.clone(),
                    catalog_revision: request.catalog_revision.clone(),
                },
            );
            let snapshot = CredentialSnapshot {
                generation: candidate.generation,
                connections: candidate.connections.clone(),
            };
            let (model_revision, validated) = validate(&snapshot)?;
            let receipt = CredentialConnectReceipt {
                client_connect_id: request.client_connect_id.clone(),
                provider_id: request.provider_id.clone(),
                credential_fields: request.credentials.keys().cloned().collect(),
                connected_at,
                catalog_revision: request.catalog_revision.clone(),
                model_revision,
            };
            candidate.receipts.insert(
                request.client_connect_id.clone(),
                StoredReceipt {
                    request_hmac,
                    result: receipt.clone(),
                },
            );
            self.write_state(root, &candidate)?;
            *state = candidate;
            Ok(CredentialConnectOutcome {
                receipt,
                candidate: Some(validated),
                replayed: false,
            })
        })
    }

    #[cfg(not(unix))]
    pub fn snapshot(&self) -> Result<CredentialSnapshot, CredentialStoreError> {
        Err(CredentialStoreError::UnsupportedPlatform)
    }

    #[cfg(not(unix))]
    pub fn connect_with<T>(
        &self,
        _request: &CredentialConnectRequest,
        _validate: impl FnOnce(&CredentialSnapshot) -> Result<(String, T), CredentialStoreError>,
    ) -> Result<CredentialConnectOutcome<T>, CredentialStoreError> {
        Err(CredentialStoreError::UnsupportedPlatform)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoreState {
    version: u32,
    generation: Uuid,
    updated_at: String,
    hmac_key: String,
    connections: BTreeMap<ProviderId, StoredConnection>,
    receipts: BTreeMap<String, StoredReceipt>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredReceipt {
    request_hmac: String,
    result: CredentialConnectReceipt,
}

impl StoreState {
    fn fresh() -> Result<Self, CredentialStoreError> {
        Ok(Self {
            version: STORE_VERSION,
            generation: Uuid::now_v7(),
            updated_at: timestamp()?,
            hmac_key: random_key(),
            connections: BTreeMap::new(),
            receipts: BTreeMap::new(),
        })
    }

    fn validate(&self) -> Result<(), CredentialStoreError> {
        if self.version != STORE_VERSION || self.hmac_key.len() != 64 {
            return Err(CredentialStoreError::InvalidStore);
        }
        decode_hex(&self.hmac_key).ok_or(CredentialStoreError::InvalidStore)?;
        for (provider, connection) in &self.connections {
            if provider != &connection.provider_id || connection.credentials.is_empty() {
                return Err(CredentialStoreError::InvalidStore);
            }
            validate_text(provider.as_str())?;
            for (name, value) in &connection.credentials {
                validate_text(name)?;
                if value.is_empty() {
                    return Err(CredentialStoreError::InvalidStore);
                }
            }
        }
        for (id, receipt) in &self.receipts {
            validate_text(id)?;
            if receipt.request_hmac.len() != 64 || decode_hex(&receipt.request_hmac).is_none() {
                return Err(CredentialStoreError::InvalidStore);
            }
        }
        Ok(())
    }
}

/// Credential storage errors deliberately exclude secret values and raw requests.
#[derive(Debug, Error)]
pub enum CredentialStoreError {
    #[error("could not determine the home directory")]
    HomeUnavailable,
    #[error("persistent provider connection is disabled on this platform")]
    UnsupportedPlatform,
    #[error("credential store path is not a current-user-owned private regular object")]
    UnsafePath,
    #[error("credential store is invalid")]
    InvalidStore,
    #[error("credential request is invalid")]
    InvalidRequest,
    #[error("provider connect id conflicts with an existing request")]
    IdempotencyConflict,
    #[error("candidate model snapshot was rejected")]
    CandidateRejected,
    #[error("credential store I/O failed")]
    Io(#[source] io::Error),
    #[error("credential store encoding failed")]
    Json(#[source] serde_json::Error),
    #[error("system clock is before the Unix epoch")]
    Clock,
}

#[cfg(unix)]
impl CredentialStore {
    fn with_locked_state<T>(
        &self,
        operation: impl FnOnce(&fs::File, &mut StoreState) -> Result<T, CredentialStoreError>,
    ) -> Result<T, CredentialStoreError> {
        let root = self.open_root()?;
        let lock = open_or_create_private_file(&root, LOCK_FILE)?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
            .map_err(|error| CredentialStoreError::Io(error.into()))?;
        let mut state = self.read_state(&root)?;
        operation(&root, &mut state)
    }

    fn open_root(&self) -> Result<fs::File, CredentialStoreError> {
        let mut current = open_trusted_anchor(&self.anchor)?;
        let components = private_components(&self.relative)?;
        for component in components {
            current = open_or_create_private_dir(&current, &component)?;
        }
        Ok(current)
    }

    fn read_state(&self, root: &fs::File) -> Result<StoreState, CredentialStoreError> {
        use std::io::Read as _;

        let file = match open_existing_file(root, STORE_FILE, rustix::fs::OFlags::RDONLY) {
            Ok(Some(file)) => file,
            Ok(None) => return StoreState::fresh(),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata().map_err(CredentialStoreError::Io)?;
        validate_private_file(&metadata)?;
        if metadata.len() > MAX_STORE_BYTES {
            return Err(CredentialStoreError::InvalidStore);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_STORE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(CredentialStoreError::Io)?;
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return Err(CredentialStoreError::InvalidStore);
        }
        let state: StoreState =
            serde_json::from_slice(&bytes).map_err(CredentialStoreError::Json)?;
        state.validate()?;
        Ok(state)
    }

    fn write_state(&self, root: &fs::File, state: &StoreState) -> Result<(), CredentialStoreError> {
        use std::io::Write as _;

        state.validate()?;
        let bytes = serde_json::to_vec_pretty(state).map_err(CredentialStoreError::Json)?;
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return Err(CredentialStoreError::InvalidStore);
        }
        let temporary = format!(".{STORE_FILE}.tmp-{}", Uuid::now_v7());
        let mut file = create_private_file(root, &temporary)?;
        let write = (|| {
            file.write_all(&bytes).map_err(CredentialStoreError::Io)?;
            file.sync_all().map_err(CredentialStoreError::Io)?;
            if let Some(destination) =
                open_existing_file(root, STORE_FILE, rustix::fs::OFlags::RDONLY)?
            {
                validate_private_file(&destination.metadata().map_err(CredentialStoreError::Io)?)?;
            }
            rustix::fs::renameat(root, temporary.as_str(), root, STORE_FILE)
                .map_err(|error| CredentialStoreError::Io(error.into()))?;
            let destination = open_existing_file(root, STORE_FILE, rustix::fs::OFlags::RDONLY)?
                .ok_or(CredentialStoreError::UnsafePath)?;
            validate_private_file(&destination.metadata().map_err(CredentialStoreError::Io)?)?;
            rustix::fs::fsync(root).map_err(|error| CredentialStoreError::Io(error.into()))?;
            Ok::<(), CredentialStoreError>(())
        })();
        if write.is_err() {
            let _ = rustix::fs::unlinkat(root, temporary.as_str(), rustix::fs::AtFlags::empty());
        }
        write
    }
}

#[cfg(unix)]
fn prepare_standard_data_root(home: &Path) -> Result<(), CredentialStoreError> {
    let home = open_trusted_anchor(home)?;
    let local = open_or_create_safe_anchor_dir(&home, ".local")?;
    let _share = open_or_create_safe_anchor_dir(&local, "share")?;
    Ok(())
}

#[cfg(unix)]
fn open_trusted_anchor(path: &Path) -> Result<fs::File, CredentialStoreError> {
    use std::path::Component;

    if !path.is_absolute() {
        return Err(CredentialStoreError::UnsafePath);
    }
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let mut current = fs::File::from(
        rustix::fs::open("/", flags, rustix::fs::Mode::empty()).map_err(path_error)?,
    );
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = fs::File::from(
                    rustix::fs::openat(&current, name, flags, rustix::fs::Mode::empty())
                        .map_err(path_error)?,
                );
                validate_directory_type(&current.metadata().map_err(CredentialStoreError::Io)?)?;
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(CredentialStoreError::UnsafePath);
            }
        }
    }
    validate_safe_anchor(&current.metadata().map_err(CredentialStoreError::Io)?)?;
    Ok(current)
}

#[cfg(unix)]
fn private_components(path: &Path) -> Result<Vec<std::ffi::OsString>, CredentialStoreError> {
    use std::path::Component;

    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => components.push(name.to_owned()),
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(CredentialStoreError::UnsafePath);
            }
        }
    }
    if components.is_empty() {
        return Err(CredentialStoreError::UnsafePath);
    }
    Ok(components)
}

#[cfg(unix)]
fn open_or_create_safe_anchor_dir(
    parent: &fs::File,
    name: &str,
) -> Result<fs::File, CredentialStoreError> {
    match open_directory_at(parent, name) {
        Ok(directory) => {
            validate_safe_anchor(&directory.metadata().map_err(CredentialStoreError::Io)?)?;
            Ok(directory)
        }
        Err(CredentialStoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            rustix::fs::mkdirat(parent, name, rustix::fs::Mode::RWXU)
                .map_err(|error| CredentialStoreError::Io(error.into()))?;
            let directory = open_directory_at(parent, name)?;
            rustix::fs::fchmod(&directory, rustix::fs::Mode::RWXU)
                .map_err(|error| CredentialStoreError::Io(error.into()))?;
            validate_private_directory(&directory.metadata().map_err(CredentialStoreError::Io)?)?;
            Ok(directory)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_or_create_private_dir(
    parent: &fs::File,
    name: &std::ffi::OsStr,
) -> Result<fs::File, CredentialStoreError> {
    match open_directory_at(parent, name) {
        Ok(directory) => {
            validate_private_directory(&directory.metadata().map_err(CredentialStoreError::Io)?)?;
            Ok(directory)
        }
        Err(CredentialStoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            let created = match rustix::fs::mkdirat(parent, name, rustix::fs::Mode::RWXU) {
                Ok(()) => true,
                Err(error) if error == rustix::io::Errno::EXIST => false,
                Err(error) => return Err(CredentialStoreError::Io(error.into())),
            };
            let directory = open_directory_at(parent, name)?;
            if created {
                rustix::fs::fchmod(&directory, rustix::fs::Mode::RWXU)
                    .map_err(|error| CredentialStoreError::Io(error.into()))?;
            }
            validate_private_directory(&directory.metadata().map_err(CredentialStoreError::Io)?)?;
            Ok(directory)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_directory_at(
    parent: &fs::File,
    name: impl rustix::path::Arg,
) -> Result<fs::File, CredentialStoreError> {
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty())
        .map(fs::File::from)
        .map_err(path_error)
}

#[cfg(unix)]
fn open_or_create_private_file(
    parent: &fs::File,
    name: &str,
) -> Result<fs::File, CredentialStoreError> {
    if let Some(file) = open_existing_file(parent, name, rustix::fs::OFlags::RDWR)? {
        return Ok(file);
    }
    match create_private_file(parent, name) {
        Ok(file) => Ok(file),
        Err(CredentialStoreError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
            open_existing_file(parent, name, rustix::fs::OFlags::RDWR)?
                .ok_or(CredentialStoreError::UnsafePath)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_existing_file(
    parent: &fs::File,
    name: &str,
    access: rustix::fs::OFlags,
) -> Result<Option<fs::File>, CredentialStoreError> {
    let flags = access | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC;
    match rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()) {
        Ok(fd) => {
            let file = fs::File::from(fd);
            validate_private_file(&file.metadata().map_err(CredentialStoreError::Io)?)?;
            Ok(Some(file))
        }
        Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
        Err(error) => Err(path_error(error)),
    }
}

#[cfg(unix)]
fn create_private_file(parent: &fs::File, name: &str) -> Result<fs::File, CredentialStoreError> {
    let flags = rustix::fs::OFlags::RDWR
        | rustix::fs::OFlags::CREATE
        | rustix::fs::OFlags::EXCL
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let file_mode = rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR;
    let file =
        fs::File::from(rustix::fs::openat(parent, name, flags, file_mode).map_err(path_error)?);
    rustix::fs::fchmod(&file, file_mode).map_err(|error| CredentialStoreError::Io(error.into()))?;
    validate_private_file(&file.metadata().map_err(CredentialStoreError::Io)?)?;
    Ok(file)
}

#[cfg(unix)]
fn validate_directory_type(metadata: &fs::Metadata) -> Result<(), CredentialStoreError> {
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(CredentialStoreError::UnsafePath)
    }
}

#[cfg(unix)]
fn validate_safe_anchor(metadata: &fs::Metadata) -> Result<(), CredentialStoreError> {
    use std::os::unix::fs::MetadataExt as _;

    validate_directory_type(metadata)?;
    if metadata.uid() != rustix::process::getuid().as_raw() || metadata.mode() & 0o022 != 0 {
        Err(CredentialStoreError::UnsafePath)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn validate_private_directory(metadata: &fs::Metadata) -> Result<(), CredentialStoreError> {
    use std::os::unix::fs::MetadataExt as _;

    validate_directory_type(metadata)?;
    if metadata.uid() != rustix::process::getuid().as_raw() || metadata.mode() & 0o777 != 0o700 {
        Err(CredentialStoreError::UnsafePath)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn validate_private_file(metadata: &fs::Metadata) -> Result<(), CredentialStoreError> {
    use std::os::unix::fs::MetadataExt as _;

    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        Err(CredentialStoreError::UnsafePath)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn path_error(error: rustix::io::Errno) -> CredentialStoreError {
    let error: io::Error = error.into();
    if matches!(error.raw_os_error(), Some(code) if code == libc::ELOOP || code == libc::ENOTDIR) {
        CredentialStoreError::UnsafePath
    } else {
        CredentialStoreError::Io(error)
    }
}

fn validate_request(request: &CredentialConnectRequest) -> Result<(), CredentialStoreError> {
    validate_text(&request.client_connect_id)?;
    validate_text(request.provider_id.as_str())?;
    validate_text(&request.catalog_revision)?;
    if request.credentials.is_empty() || request.credentials.len() > 32 {
        return Err(CredentialStoreError::InvalidRequest);
    }
    for (name, value) in &request.credentials {
        validate_text(name)?;
        if value.is_empty() || value.len() > 1024 * 1024 {
            return Err(CredentialStoreError::InvalidRequest);
        }
    }
    Ok(())
}

fn validate_text(value: &str) -> Result<(), CredentialStoreError> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        Err(CredentialStoreError::InvalidRequest)
    } else {
        Ok(())
    }
}

fn timestamp() -> Result<String, CredentialStoreError> {
    Ok(jiff::Timestamp::now().to_string())
}

fn random_key() -> String {
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    first
        .as_bytes()
        .iter()
        .chain(second.as_bytes())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn request_hmac(
    key: &str,
    request: &CredentialConnectRequest,
) -> Result<String, CredentialStoreError> {
    let key = decode_hex(key).ok_or(CredentialStoreError::InvalidStore)?;
    let canonical = serde_json::to_vec(request).map_err(CredentialStoreError::Json)?;
    Ok(hex(&hmac_sha256(&key, &canonical)))
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut normalized = [0_u8; BLOCK];
    if key.len() > BLOCK {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK];
    let mut outer_pad = [0x5c_u8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}
