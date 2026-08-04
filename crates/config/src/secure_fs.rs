use std::{
    fs, io,
    path::{Component, Path},
};

use crate::{AgentDocumentSource, ConfigError};

pub(crate) struct LayerRoot {
    pub(crate) directory: fs::File,
    pub(crate) source: AgentDocumentSource,
}

#[cfg(unix)]
pub(crate) fn regular_file_at(parent: &fs::File, name: &str) -> Result<bool, ConfigError> {
    use std::os::unix::fs::MetadataExt as _;
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC;
    let file = fs::File::from(
        rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()).map_err(path_error)?,
    );
    let metadata = file.metadata().map_err(ConfigError::Io)?;
    Ok(metadata.is_file() && !metadata.file_type().is_symlink() && metadata.nlink() == 1)
}

#[cfg(not(unix))]
pub(crate) fn regular_file_at(_parent: &fs::File, _name: &str) -> Result<bool, ConfigError> {
    Err(ConfigError::UnsupportedPlatform)
}

#[cfg(unix)]
pub(crate) fn open_layer_root(
    path: &Path,
    source: AgentDocumentSource,
) -> Result<Option<LayerRoot>, ConfigError> {
    use std::os::unix::fs::MetadataExt as _;
    if !path.is_absolute() {
        return Err(ConfigError::UnsafePath);
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
                match rustix::fs::openat(&current, name, flags, rustix::fs::Mode::empty()) {
                    Ok(fd) => current = fs::File::from(fd),
                    Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
                    Err(error) => return Err(path_error(error)),
                }
            }
            _ => return Err(ConfigError::UnsafePath),
        }
    }
    let metadata = current.metadata().map_err(ConfigError::Io)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ConfigError::UnsafePath);
    }
    if source == AgentDocumentSource::User
        && (metadata.uid() != rustix::process::getuid().as_raw()
            || metadata.mode() & 0o777 != 0o700)
    {
        return Err(ConfigError::UnsafePath);
    }
    Ok(Some(LayerRoot {
        directory: current,
        source,
    }))
}

#[cfg(not(unix))]
pub(crate) fn open_layer_root(
    _path: &Path,
    _source: AgentDocumentSource,
) -> Result<Option<LayerRoot>, ConfigError> {
    Err(ConfigError::UnsupportedPlatform)
}

#[cfg(unix)]
pub(crate) fn open_optional_directory(
    parent: &fs::File,
    name: &str,
    source: AgentDocumentSource,
) -> Result<Option<fs::File>, ConfigError> {
    use std::os::unix::fs::MetadataExt as _;
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let file = match rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()) {
        Ok(fd) => fs::File::from(fd),
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(error) => return Err(path_error(error)),
    };
    let metadata = file.metadata().map_err(ConfigError::Io)?;
    if !metadata.is_dir()
        || source == AgentDocumentSource::User
            && (metadata.uid() != rustix::process::getuid().as_raw()
                || metadata.mode() & 0o777 != 0o700)
    {
        return Err(ConfigError::UnsafePath);
    }
    Ok(Some(file))
}

#[cfg(not(unix))]
pub(crate) fn open_optional_directory(
    _parent: &fs::File,
    _name: &str,
    _source: AgentDocumentSource,
) -> Result<Option<fs::File>, ConfigError> {
    Err(ConfigError::UnsupportedPlatform)
}

pub(crate) fn read_optional_file(
    parent: &fs::File,
    name: &str,
    limit: u64,
    source: AgentDocumentSource,
) -> Result<Option<Vec<u8>>, ConfigError> {
    match read_file(parent, name, limit, source) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(ConfigError::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}
pub(crate) fn read_required_file(
    parent: &fs::File,
    name: &str,
    limit: u64,
    source: AgentDocumentSource,
) -> Result<Vec<u8>, ConfigError> {
    read_file(parent, name, limit, source)
}

#[cfg(unix)]
fn read_file(
    parent: &fs::File,
    name: &str,
    limit: u64,
    source: AgentDocumentSource,
) -> Result<Vec<u8>, ConfigError> {
    use std::{io::Read as _, os::unix::fs::MetadataExt as _};
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC;
    let file = match rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()) {
        Ok(fd) => fs::File::from(fd),
        Err(error) if error == rustix::io::Errno::NOENT => return Err(ConfigError::NotFound),
        Err(error) => return Err(path_error(error)),
    };
    let metadata = file.metadata().map_err(ConfigError::Io)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.nlink() != 1
        || source == AgentDocumentSource::User
            && (metadata.uid() != rustix::process::getuid().as_raw()
                || metadata.mode() & 0o777 != 0o600)
    {
        return Err(ConfigError::UnsafePath);
    }
    if metadata.len() > limit {
        return Err(ConfigError::TooLarge(name.to_owned()));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(ConfigError::Io)?;
    if bytes.len() as u64 > limit {
        return Err(ConfigError::TooLarge(name.to_owned()));
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_file(
    _parent: &fs::File,
    _name: &str,
    _limit: u64,
    _source: AgentDocumentSource,
) -> Result<Vec<u8>, ConfigError> {
    Err(ConfigError::UnsupportedPlatform)
}

#[cfg(unix)]
fn path_error(error: rustix::io::Errno) -> ConfigError {
    let error: io::Error = error.into();
    if matches!(
        error.raw_os_error(),
        Some(code)
            if matches!(
                code,
                libc::ELOOP
                    | libc::ENOTDIR
                    | libc::ENXIO
                    | libc::ENODEV
                    | libc::EOPNOTSUPP
            )
    ) {
        ConfigError::UnsafePath
    } else {
        ConfigError::Io(error)
    }
}
