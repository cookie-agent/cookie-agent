use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::{
    fs,
    io::{Read, Write},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
#[cfg(windows)]
use cookie_agent_models::secure_store::{SecureDirectory, SecureStoreError};
use cookie_agent_protocol::paths;
use thiserror::Error;

const TOKEN_FILE: &str = "token-v1";
const TOKEN_BYTES: usize = 32;
pub(crate) const TOKEN_ENCODED_BYTES: usize = 43;

pub(crate) fn standard_token_path() -> Option<PathBuf> {
    paths::user_data_root()
        .ok()
        .map(|root| root.join("daemon").join(TOKEN_FILE))
}

/// Loads the same private bearer token used by the localhost WebSocket daemon.
pub fn load_auth_token() -> Result<String, TokenError> {
    let path = standard_token_path().ok_or(TokenError::HomeUnavailable)?;
    load_or_create_token(&path)
}

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("home directory is unavailable")]
    HomeUnavailable,
    #[error("token path is unsafe")]
    UnsafePath,
    #[error("token file is invalid")]
    InvalidToken,
    #[error("token storage failed")]
    Io(#[source] io::Error),
}

pub(crate) fn load_or_create_token(path: &Path) -> Result<String, TokenError> {
    #[cfg(unix)]
    {
        load_or_create_token_unix(path)
    }
    #[cfg(windows)]
    {
        load_or_create_token_windows(path)
    }
}

#[cfg(windows)]
fn load_or_create_token_windows(path: &Path) -> Result<String, TokenError> {
    if path.as_os_str().is_empty()
        || path.file_name().and_then(|name| name.to_str()) != Some(TOKEN_FILE)
    {
        return Err(TokenError::UnsafePath);
    }
    let parent = path.parent().ok_or(TokenError::UnsafePath)?;
    let directory = SecureDirectory::open(parent).map_err(token_store_error)?;
    let lock = directory
        .lock(".token-v1.lock")
        .map_err(token_store_error)?;
    if let Some(bytes) = lock
        .read(TOKEN_FILE, (TOKEN_ENCODED_BYTES + 1) as u64)
        .map_err(token_store_error)?
    {
        return decode_token(bytes);
    }
    let token = generate_token()?;
    lock.atomic_replace(TOKEN_FILE, token.as_bytes())
        .map_err(token_store_error)?;
    Ok(token)
}

#[cfg(windows)]
fn token_store_error(error: SecureStoreError) -> TokenError {
    match error {
        SecureStoreError::Io(error) => TokenError::Io(error),
        SecureStoreError::TooLarge => TokenError::InvalidToken,
        SecureStoreError::HomeUnavailable | SecureStoreError::UnsafePath => TokenError::UnsafePath,
    }
}

#[cfg(windows)]
fn decode_token(bytes: Vec<u8>) -> Result<String, TokenError> {
    let token = String::from_utf8(bytes).map_err(|_| TokenError::InvalidToken)?;
    validate_token(token)
}

#[cfg(unix)]
fn load_or_create_token_unix(path: &Path) -> Result<String, TokenError> {
    if path.as_os_str().is_empty()
        || path.file_name().and_then(|name| name.to_str()) != Some(TOKEN_FILE)
    {
        return Err(TokenError::UnsafePath);
    }
    let root = paths::user_data_root().map_err(|_| TokenError::HomeUnavailable)?;
    let home = root.parent().ok_or(TokenError::UnsafePath)?;
    let root_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(TokenError::UnsafePath)?;
    let expected = root.join("daemon").join(TOKEN_FILE);
    #[cfg(not(test))]
    if path != expected {
        return Err(TokenError::UnsafePath);
    }
    let parent = if path == expected {
        let home = open_trusted_token_anchor(home)?;
        let root = open_or_create_private_dir(&home, root_name)?;
        open_or_create_private_dir(&root, "daemon")?
    } else {
        #[cfg(test)]
        {
            let parent_path = path.parent().ok_or(TokenError::UnsafePath)?;
            fs::create_dir_all(parent_path).map_err(TokenError::Io)?;
            let directory = fs::File::open(parent_path).map_err(TokenError::Io)?;
            rustix::fs::fchmod(&directory, rustix::fs::Mode::RWXU)
                .map_err(|error| TokenError::Io(error.into()))?;
            directory
        }
        #[cfg(not(test))]
        unreachable!()
    };
    load_or_create_token_from_parent(&parent)
}

#[cfg(unix)]
fn load_or_create_token_from_parent(parent: &fs::File) -> Result<String, TokenError> {
    if let Some(mut file) = open_token_file(parent)? {
        return read_token(&mut file);
    }
    let token = generate_token()?;
    let temporary = format!(".token-v1.tmp-{}", &token[..12]);
    let mut file = create_token_file(parent, &temporary)?;
    let result = (|| {
        file.write_all(token.as_bytes()).map_err(TokenError::Io)?;
        file.sync_all().map_err(TokenError::Io)?;
        match rustix::fs::renameat_with(
            parent,
            temporary.as_str(),
            parent,
            TOKEN_FILE,
            rustix::fs::RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {
                rustix::fs::unlinkat(parent, temporary.as_str(), rustix::fs::AtFlags::empty())
                    .map_err(|error| TokenError::Io(error.into()))?;
                let mut existing = open_token_file(parent)?.ok_or(TokenError::UnsafePath)?;
                return read_token(&mut existing);
            }
            Err(error) => return Err(token_path_error(error)),
        }
        rustix::fs::fsync(parent).map_err(|error| TokenError::Io(error.into()))?;
        Ok(token)
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(parent, temporary.as_str(), rustix::fs::AtFlags::empty());
    }
    result
}

#[cfg(unix)]
fn open_trusted_token_anchor(path: &Path) -> Result<fs::File, TokenError> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(TokenError::UnsafePath);
    }
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let mut current = fs::File::from(
        rustix::fs::open("/", flags, rustix::fs::Mode::empty()).map_err(token_path_error)?,
    );
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = fs::File::from(
                    rustix::fs::openat(&current, name, flags, rustix::fs::Mode::empty())
                        .map_err(token_path_error)?,
                );
            }
            _ => return Err(TokenError::UnsafePath),
        }
    }
    validate_safe_anchor(&current.metadata().map_err(TokenError::Io)?)?;
    Ok(current)
}

#[cfg(unix)]
fn open_or_create_private_dir(parent: &fs::File, name: &str) -> Result<fs::File, TokenError> {
    match open_directory_at(parent, name) {
        Ok(directory) => {
            validate_private_directory(&directory.metadata().map_err(TokenError::Io)?)?;
            Ok(directory)
        }
        Err(TokenError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            match rustix::fs::mkdirat(parent, name, rustix::fs::Mode::RWXU) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(TokenError::Io(error.into())),
            }
            let directory = open_directory_at(parent, name)?;
            rustix::fs::fchmod(&directory, rustix::fs::Mode::RWXU)
                .map_err(|error| TokenError::Io(error.into()))?;
            validate_private_directory(&directory.metadata().map_err(TokenError::Io)?)?;
            Ok(directory)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_directory_at(parent: &fs::File, name: &str) -> Result<fs::File, TokenError> {
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty())
        .map(fs::File::from)
        .map_err(token_path_error)
}

#[cfg(unix)]
fn open_token_file(parent: &fs::File) -> Result<Option<fs::File>, TokenError> {
    let flags =
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC;
    match rustix::fs::openat(parent, TOKEN_FILE, flags, rustix::fs::Mode::empty()) {
        Ok(file) => {
            let file = fs::File::from(file);
            validate_token_file(&file.metadata().map_err(TokenError::Io)?)?;
            Ok(Some(file))
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(token_path_error(error)),
    }
}

#[cfg(unix)]
fn create_token_file(parent: &fs::File, name: &str) -> Result<fs::File, TokenError> {
    let flags = rustix::fs::OFlags::WRONLY
        | rustix::fs::OFlags::CREATE
        | rustix::fs::OFlags::EXCL
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let mode = rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR;
    let file =
        fs::File::from(rustix::fs::openat(parent, name, flags, mode).map_err(token_path_error)?);
    rustix::fs::fchmod(&file, mode).map_err(|error| TokenError::Io(error.into()))?;
    Ok(file)
}

#[cfg(unix)]
fn validate_safe_anchor(metadata: &fs::Metadata) -> Result<(), TokenError> {
    use std::os::unix::fs::MetadataExt as _;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        Err(TokenError::UnsafePath)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn validate_private_directory(metadata: &fs::Metadata) -> Result<(), TokenError> {
    use std::os::unix::fs::MetadataExt as _;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        Err(TokenError::UnsafePath)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn validate_token_file(metadata: &fs::Metadata) -> Result<(), TokenError> {
    use std::os::unix::fs::MetadataExt as _;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != TOKEN_ENCODED_BYTES as u64
    {
        Err(TokenError::UnsafePath)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn token_path_error(error: rustix::io::Errno) -> TokenError {
    let error: io::Error = error.into();
    if matches!(error.raw_os_error(), Some(code) if code == libc::ELOOP || code == libc::ENOTDIR) {
        TokenError::UnsafePath
    } else {
        TokenError::Io(error)
    }
}

#[cfg(unix)]
fn read_token(file: &mut fs::File) -> Result<String, TokenError> {
    let mut token = String::new();
    file.take((TOKEN_ENCODED_BYTES + 1) as u64)
        .read_to_string(&mut token)
        .map_err(TokenError::Io)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(token.as_bytes())
        .map_err(|_| TokenError::InvalidToken)?;
    if token.len() != TOKEN_ENCODED_BYTES || decoded.len() != TOKEN_BYTES {
        return Err(TokenError::InvalidToken);
    }
    Ok(token)
}

fn generate_token() -> Result<String, TokenError> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::getrandom(&mut bytes).map_err(|error| TokenError::Io(error.into()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(windows)]
fn validate_token(token: String) -> Result<String, TokenError> {
    let decoded = URL_SAFE_NO_PAD
        .decode(token.as_bytes())
        .map_err(|_| TokenError::InvalidToken)?;
    if token.len() != TOKEN_ENCODED_BYTES || decoded.len() != TOKEN_BYTES {
        return Err(TokenError::InvalidToken);
    }
    Ok(token)
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn token_storage_is_acl_protected_and_stable() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let path = temporary.path().join("daemon").join(TOKEN_FILE);
        let first = load_or_create_token(&path).expect("first token");
        let second = load_or_create_token(&path).expect("second token");
        assert_eq!(first, second);
        cookie_agent_models::secure_store::validate_windows_path_acl(&path).expect("token ACL");
    }
}
