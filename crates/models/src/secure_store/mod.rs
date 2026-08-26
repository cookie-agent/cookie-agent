//! Reusable storage primitives with private-at-creation state.

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{
    create_private_dir_all as create_windows_private_dir_all,
    create_private_file as create_windows_private_file, replace_path as replace_windows_path,
    verify_private_creation as verify_windows_private_creation,
};

use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::path::Component;

use cookie_agent_protocol::paths;
use thiserror::Error;
#[cfg(unix)]
use uuid::Uuid;

/// A held storage directory; missing components are created privately.
#[derive(Debug)]
pub struct SecureDirectory {
    #[cfg(unix)]
    pub(crate) directory: fs::File,
    path: PathBuf,
}

impl SecureDirectory {
    /// Opens `~/.cookie-agent/<relative>`, creating missing components privately.
    pub fn user_data(relative: impl AsRef<Path>) -> Result<Self, SecureStoreError> {
        let root = paths::user_data_root().map_err(|_| SecureStoreError::HomeUnavailable)?;
        #[cfg(unix)]
        {
            let home = root.parent().ok_or(SecureStoreError::UnsafePath)?;
            let root_name = root.file_name().ok_or(SecureStoreError::UnsafePath)?;
            let home_directory = open_absolute_directory(home)?;
            let relative = Path::new(root_name).join(relative);
            let mut directory = Self::open_private_in(&home_directory, &relative)?;
            directory.path = home.join(relative);
            Ok(directory)
        }
        #[cfg(windows)]
        {
            let home = root.parent().ok_or(SecureStoreError::UnsafePath)?;
            let root_name = root.file_name().ok_or(SecureStoreError::UnsafePath)?;
            windows::open_private(home, &Path::new(root_name).join(relative))
        }
    }

    /// Opens a path below an existing anchor, creating missing components privately.
    pub fn open_in(
        anchor: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, SecureStoreError> {
        #[cfg(unix)]
        {
            let anchor_path = anchor.as_ref().to_path_buf();
            let anchor = open_absolute_directory(&anchor_path)?;
            let mut directory = Self::open_private_in(&anchor, relative.as_ref())?;
            directory.path = anchor_path.join(relative.as_ref());
            Ok(directory)
        }
        #[cfg(windows)]
        {
            windows::open_private(anchor.as_ref(), relative.as_ref())
        }
    }

    /// Opens a storage path below an existing project anchor.
    pub(crate) fn open_in_untrusted_project_anchor(
        anchor: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, SecureStoreError> {
        #[cfg(unix)]
        {
            let anchor_path = anchor.as_ref().to_path_buf();
            let anchor = open_absolute_directory(&anchor_path)?;
            let mut directory = Self::open_private_in(&anchor, relative.as_ref())?;
            directory.path = anchor_path.join(relative.as_ref());
            Ok(directory)
        }
        #[cfg(windows)]
        {
            windows::open_private(anchor.as_ref(), relative.as_ref())
        }
    }

    /// Opens a directory or creates it privately when missing.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SecureStoreError> {
        let path = path.as_ref();
        #[cfg(unix)]
        {
            let parent = path.parent().ok_or(SecureStoreError::UnsafePath)?;
            let name = path.file_name().ok_or(SecureStoreError::UnsafePath)?;
            Self::open_in(parent, Path::new(name))
        }
        #[cfg(windows)]
        {
            windows::open_absolute_private(path)
        }
    }

    /// Returns the diagnostic path. Security decisions never use this path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads an optional storage file from its held descriptor with a hard cap.
    pub fn read(&self, name: &str, limit: u64) -> Result<Option<Vec<u8>>, SecureStoreError> {
        validate_name(name)?;
        #[cfg(unix)]
        {
            read_file(&self.directory, name, limit)
        }
        #[cfg(windows)]
        {
            windows::validate_leaf_name(name)?;
            windows::read_file(self, name, limit)
        }
    }

    /// Acquires a cross-process exclusive lock file.
    pub fn lock(&self, name: &str) -> Result<SecureDirectoryLock<'_>, SecureStoreError> {
        validate_name(name)?;
        #[cfg(unix)]
        {
            let lock = open_or_create_file(&self.directory, name)?;
            rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
                .map_err(io_error)?;
            rustix::fs::fsync(&self.directory).map_err(io_error)?;
            Ok(SecureDirectoryLock {
                directory: self,
                lock_name: name.to_owned(),
                _lock: lock,
            })
        }
        #[cfg(windows)]
        {
            windows::validate_leaf_name(name)?;
            windows::lock(self, name)
        }
    }

    #[cfg(unix)]
    fn open_private_in(parent: &fs::File, relative: &Path) -> Result<Self, SecureStoreError> {
        let components = private_components(relative)?;
        let mut current = parent.try_clone().map_err(SecureStoreError::Io)?;
        for component in &components {
            current = open_or_create_directory(&current, Path::new(component))?;
        }
        Ok(Self {
            directory: current,
            path: relative.to_path_buf(),
        })
    }
}

/// A held lock authorizing lock/reread/replace transactions in one directory.
#[derive(Debug)]
pub struct SecureDirectoryLock<'a> {
    directory: &'a SecureDirectory,
    lock_name: String,
    _lock: fs::File,
}

impl SecureDirectoryLock<'_> {
    /// Reads bounded transaction-journal bytes stored in the held lock file.
    pub fn read_journal(&self, limit: u64) -> Result<Vec<u8>, SecureStoreError> {
        #[cfg(unix)]
        {
            use std::io::{Read as _, Seek as _, SeekFrom};

            let metadata = self._lock.metadata().map_err(SecureStoreError::Io)?;
            if metadata.len() > limit {
                return Err(SecureStoreError::TooLarge);
            }
            let mut file = self._lock.try_clone().map_err(SecureStoreError::Io)?;
            file.seek(SeekFrom::Start(0))
                .map_err(SecureStoreError::Io)?;
            let mut bytes = Vec::with_capacity(
                usize::try_from(metadata.len()).map_err(|_| SecureStoreError::TooLarge)?,
            );
            file.take(limit.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(SecureStoreError::Io)?;
            if bytes.len() as u64 > limit {
                return Err(SecureStoreError::TooLarge);
            }
            Ok(bytes)
        }
        #[cfg(windows)]
        {
            windows::read_journal(self, limit)
        }
    }

    /// Durably appends one bounded record to the held lock-file journal.
    pub fn append_journal(&self, bytes: &[u8], limit: u64) -> Result<(), SecureStoreError> {
        #[cfg(unix)]
        {
            use std::io::{Seek as _, SeekFrom, Write as _};

            let current = self._lock.metadata().map_err(SecureStoreError::Io)?.len();
            if current.saturating_add(bytes.len() as u64) > limit {
                return Err(SecureStoreError::TooLarge);
            }
            let mut file = self._lock.try_clone().map_err(SecureStoreError::Io)?;
            file.seek(SeekFrom::End(0)).map_err(SecureStoreError::Io)?;
            file.write_all(bytes).map_err(SecureStoreError::Io)?;
            file.sync_all().map_err(SecureStoreError::Io)?;
            Ok(())
        }
        #[cfg(windows)]
        {
            windows::append_journal(self, bytes, limit)
        }
    }

    /// Durably clears the held lock-file journal without replacing the lock inode.
    pub fn clear_journal(&self) -> Result<(), SecureStoreError> {
        #[cfg(unix)]
        {
            self._lock.set_len(0).map_err(SecureStoreError::Io)?;
            self._lock.sync_all().map_err(SecureStoreError::Io)?;
            Ok(())
        }
        #[cfg(windows)]
        {
            windows::clear_journal(self)
        }
    }

    /// Rereads an optional file while the cross-process lock is held.
    pub fn read(&self, name: &str, limit: u64) -> Result<Option<Vec<u8>>, SecureStoreError> {
        let result = self.directory.read(name, limit)?;
        Ok(result)
    }

    /// Durably replaces one private file using an exclusive sibling temporary.
    pub fn atomic_replace(&self, name: &str, bytes: &[u8]) -> Result<(), SecureStoreError> {
        validate_name(name)?;
        if name == self.lock_name {
            return Err(SecureStoreError::UnsafePath);
        }
        #[cfg(unix)]
        {
            use std::io::Write as _;

            let temporary = format!(".{name}.tmp-{}", Uuid::now_v7());
            let mut file = create_file(&self.directory.directory, &temporary)?;
            let result = (|| {
                file.write_all(bytes).map_err(SecureStoreError::Io)?;
                file.sync_all().map_err(SecureStoreError::Io)?;
                rustix::fs::renameat(
                    &self.directory.directory,
                    temporary.as_str(),
                    &self.directory.directory,
                    name,
                )
                .map_err(io_error)?;
                rustix::fs::fsync(&self.directory.directory).map_err(io_error)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = rustix::fs::unlinkat(
                    &self.directory.directory,
                    temporary.as_str(),
                    rustix::fs::AtFlags::empty(),
                );
            }
            result
        }
        #[cfg(windows)]
        {
            windows::validate_leaf_name(name)?;
            windows::atomic_replace(self, name, bytes)
        }
    }

    /// Removes an optional private file and fsyncs the directory.
    pub fn remove(&self, name: &str) -> Result<(), SecureStoreError> {
        validate_name(name)?;
        if name == self.lock_name {
            return Err(SecureStoreError::UnsafePath);
        }
        #[cfg(unix)]
        {
            if open_existing_file(&self.directory.directory, name, rustix::fs::OFlags::RDONLY)?
                .is_some()
            {
                rustix::fs::unlinkat(
                    &self.directory.directory,
                    name,
                    rustix::fs::AtFlags::empty(),
                )
                .map_err(io_error)?;
                rustix::fs::fsync(&self.directory.directory).map_err(io_error)?;
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            windows::validate_leaf_name(name)?;
            windows::remove(self, name)
        }
    }
}

#[cfg(windows)]
impl Drop for SecureDirectoryLock<'_> {
    fn drop(&mut self) {
        windows::unlock(&self._lock);
    }
}

/// Fail-closed storage errors with no file contents or secret values.
#[derive(Debug, Error)]
pub enum SecureStoreError {
    #[error("could not determine the home directory")]
    HomeUnavailable,
    #[error("invalid secure storage path")]
    UnsafePath,
    #[error("secure storage object exceeds its byte limit")]
    TooLarge,
    #[error("secure storage I/O failed")]
    Io(#[source] io::Error),
}

#[cfg(unix)]
fn open_absolute_directory(path: &Path) -> Result<fs::File, SecureStoreError> {
    if !path.is_absolute() {
        return Err(SecureStoreError::UnsafePath);
    }
    let flags = directory_flags();
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
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(SecureStoreError::UnsafePath);
            }
        }
    }
    Ok(current)
}

#[cfg(unix)]
fn private_components(path: &Path) -> Result<Vec<std::ffi::OsString>, SecureStoreError> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => components.push(name.to_owned()),
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(SecureStoreError::UnsafePath);
            }
        }
    }
    if components.is_empty() {
        return Err(SecureStoreError::UnsafePath);
    }
    Ok(components)
}

#[cfg(unix)]
fn open_or_create_directory(
    parent: &fs::File,
    relative: &Path,
) -> Result<fs::File, SecureStoreError> {
    let mut current = parent.try_clone().map_err(SecureStoreError::Io)?;
    for component in private_components(relative)? {
        let created = match rustix::fs::mkdirat(&current, &component, rustix::fs::Mode::RWXU) {
            Ok(()) => true,
            Err(error) if error == rustix::io::Errno::EXIST => false,
            Err(error) => return Err(io_error(error)),
        };
        let next = fs::File::from(
            rustix::fs::openat(
                &current,
                &component,
                directory_flags(),
                rustix::fs::Mode::empty(),
            )
            .map_err(path_error)?,
        );
        if created {
            rustix::fs::fchmod(&next, rustix::fs::Mode::RWXU).map_err(io_error)?;
        }
        current = next;
    }
    Ok(current)
}

#[cfg(unix)]
fn directory_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC
}

#[cfg(unix)]
fn read_file(
    directory: &fs::File,
    name: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>, SecureStoreError> {
    use std::io::Read as _;

    let Some(file) = open_existing_file(directory, name, rustix::fs::OFlags::RDONLY)? else {
        return Ok(None);
    };
    let metadata = file.metadata().map_err(SecureStoreError::Io)?;
    if metadata.len() > limit {
        return Err(SecureStoreError::TooLarge);
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| SecureStoreError::TooLarge)?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(SecureStoreError::Io)?;
    if bytes.len() as u64 > limit {
        return Err(SecureStoreError::TooLarge);
    }
    Ok(Some(bytes))
}

#[cfg(unix)]
fn open_or_create_file(directory: &fs::File, name: &str) -> Result<fs::File, SecureStoreError> {
    if let Some(file) = open_existing_file(directory, name, rustix::fs::OFlags::RDWR)? {
        return Ok(file);
    }
    match create_file(directory, name) {
        Ok(file) => Ok(file),
        Err(SecureStoreError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
            open_existing_file(directory, name, rustix::fs::OFlags::RDWR)?
                .ok_or(SecureStoreError::UnsafePath)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_existing_file(
    directory: &fs::File,
    name: &str,
    access: rustix::fs::OFlags,
) -> Result<Option<fs::File>, SecureStoreError> {
    let flags = access | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC;
    match rustix::fs::openat(directory, name, flags, rustix::fs::Mode::empty()) {
        Ok(fd) => Ok(Some(fs::File::from(fd))),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
        Err(error) => Err(path_error(error)),
    }
}

#[cfg(unix)]
fn create_file(directory: &fs::File, name: &str) -> Result<fs::File, SecureStoreError> {
    let flags = rustix::fs::OFlags::RDWR
        | rustix::fs::OFlags::CREATE
        | rustix::fs::OFlags::EXCL
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC;
    let mode = rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR;
    let file =
        fs::File::from(rustix::fs::openat(directory, name, flags, mode).map_err(path_error)?);
    rustix::fs::fchmod(&file, mode).map_err(io_error)?;
    Ok(file)
}

fn validate_name(name: &str) -> Result<(), SecureStoreError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.as_bytes().contains(&b'/')
        || name.as_bytes().contains(&0)
    {
        Err(SecureStoreError::UnsafePath)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn path_error(error: rustix::io::Errno) -> SecureStoreError {
    SecureStoreError::Io(error.into())
}

#[cfg(unix)]
fn io_error(error: rustix::io::Errno) -> SecureStoreError {
    SecureStoreError::Io(error.into())
}
