//! Reusable fail-closed Unix storage primitives.

use std::{
    env, fs, io,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;
use uuid::Uuid;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

/// A held, descriptor-relative private directory.
#[derive(Debug)]
pub struct SecureDirectory {
    pub(crate) directory: fs::File,
    path: PathBuf,
}

impl SecureDirectory {
    /// Opens `~/.local/share/cookie_agent/<relative>`, creating private components.
    pub fn user_data(relative: impl AsRef<Path>) -> Result<Self, SecureStoreError> {
        let home = env::var_os("HOME").ok_or(SecureStoreError::HomeUnavailable)?;
        let home = PathBuf::from(home);
        #[cfg(unix)]
        {
            let home_directory = open_absolute_directory(&home, DirectoryPolicy::SafeAnchor)?;
            let local = open_or_create_directory(
                &home_directory,
                Path::new(".local"),
                DirectoryPolicy::SafeAnchor,
            )?;
            let share =
                open_or_create_directory(&local, Path::new("share"), DirectoryPolicy::SafeAnchor)?;
            let relative = Path::new("cookie_agent").join(relative);
            let mut directory = Self::open_private_in(&share, &relative)?;
            directory.path = home.join(".local/share").join(relative);
            Ok(directory)
        }
        #[cfg(not(unix))]
        {
            let _ = (home, relative);
            Err(SecureStoreError::UnsupportedPlatform)
        }
    }

    /// Opens a private path below a trusted existing anchor.
    pub fn open_in(
        anchor: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, SecureStoreError> {
        #[cfg(unix)]
        {
            let anchor_path = anchor.as_ref().to_path_buf();
            let anchor = open_absolute_directory(&anchor_path, DirectoryPolicy::SafeAnchor)?;
            let mut directory = Self::open_private_in(&anchor, relative.as_ref())?;
            directory.path = anchor_path.join(relative.as_ref());
            Ok(directory)
        }
        #[cfg(not(unix))]
        {
            let _ = (anchor, relative);
            Err(SecureStoreError::UnsupportedPlatform)
        }
    }

    /// Opens a private path below an existing, potentially shared project anchor.
    ///
    /// The anchor itself need not be owned by the current user or non-writable by
    /// collaborators. Such collaborators may therefore deny service by removing
    /// or replacing project entries, but cannot inject accepted storage objects:
    /// every created/opened descendant remains current-user-owned mode 0700 and
    /// all file operations retain the private, single-link, no-follow checks.
    pub(crate) fn open_in_untrusted_project_anchor(
        anchor: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, SecureStoreError> {
        #[cfg(unix)]
        {
            let anchor_path = anchor.as_ref().to_path_buf();
            let anchor =
                open_absolute_directory(&anchor_path, DirectoryPolicy::UntrustedProjectAnchor)?;
            let mut directory = Self::open_private_in(&anchor, relative.as_ref())?;
            directory.path = anchor_path.join(relative.as_ref());
            Ok(directory)
        }
        #[cfg(not(unix))]
        {
            let _ = (anchor, relative);
            Err(SecureStoreError::UnsupportedPlatform)
        }
    }

    /// Opens or creates an absolute private directory.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SecureStoreError> {
        let path = path.as_ref();
        let parent = path.parent().ok_or(SecureStoreError::UnsafePath)?;
        let name = path.file_name().ok_or(SecureStoreError::UnsafePath)?;
        Self::open_in(parent, Path::new(name))
    }

    /// Returns the diagnostic path. Security decisions never use this path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads an optional private file from its held descriptor with a hard cap.
    pub fn read(&self, name: &str, limit: u64) -> Result<Option<Vec<u8>>, SecureStoreError> {
        validate_name(name)?;
        read_file(&self.directory, name, limit)
    }

    /// Acquires and verifies a cross-process exclusive lock file.
    pub fn lock(&self, name: &str) -> Result<SecureDirectoryLock<'_>, SecureStoreError> {
        validate_name(name)?;
        #[cfg(unix)]
        {
            let lock = open_or_create_file(&self.directory, name)?;
            rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
                .map_err(io_error)?;
            verify_current_entry(&self.directory, name, &lock)?;
            rustix::fs::fsync(&self.directory).map_err(io_error)?;
            Ok(SecureDirectoryLock {
                directory: self,
                lock_name: name.to_owned(),
                _lock: lock,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            Err(SecureStoreError::UnsupportedPlatform)
        }
    }

    #[cfg(unix)]
    fn open_private_in(parent: &fs::File, relative: &Path) -> Result<Self, SecureStoreError> {
        let components = private_components(relative)?;
        let mut current = parent.try_clone().map_err(SecureStoreError::Io)?;
        for component in &components {
            current =
                open_or_create_directory(&current, Path::new(component), DirectoryPolicy::Private)?;
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

            self.verify_lock()?;
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
            self.verify_lock()?;
            Ok(bytes)
        }
        #[cfg(not(unix))]
        {
            let _ = limit;
            Err(SecureStoreError::UnsupportedPlatform)
        }
    }

    /// Durably appends one bounded record to the held lock-file journal.
    pub fn append_journal(&self, bytes: &[u8], limit: u64) -> Result<(), SecureStoreError> {
        #[cfg(unix)]
        {
            use std::io::{Seek as _, SeekFrom, Write as _};

            self.verify_lock()?;
            let current = self._lock.metadata().map_err(SecureStoreError::Io)?.len();
            if current.saturating_add(bytes.len() as u64) > limit {
                return Err(SecureStoreError::TooLarge);
            }
            let mut file = self._lock.try_clone().map_err(SecureStoreError::Io)?;
            file.seek(SeekFrom::End(0)).map_err(SecureStoreError::Io)?;
            file.write_all(bytes).map_err(SecureStoreError::Io)?;
            file.sync_all().map_err(SecureStoreError::Io)?;
            self.verify_lock()?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (bytes, limit);
            Err(SecureStoreError::UnsupportedPlatform)
        }
    }

    /// Durably clears the held lock-file journal without replacing the lock inode.
    pub fn clear_journal(&self) -> Result<(), SecureStoreError> {
        #[cfg(unix)]
        {
            self.verify_lock()?;
            self._lock.set_len(0).map_err(SecureStoreError::Io)?;
            self._lock.sync_all().map_err(SecureStoreError::Io)?;
            self.verify_lock()?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(SecureStoreError::UnsupportedPlatform)
        }
    }

    /// Rereads an optional file while the cross-process lock is held.
    pub fn read(&self, name: &str, limit: u64) -> Result<Option<Vec<u8>>, SecureStoreError> {
        self.verify_lock()?;
        let result = self.directory.read(name, limit)?;
        self.verify_lock()?;
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

            self.verify_lock()?;

            if let Some(destination) =
                open_existing_file(&self.directory.directory, name, rustix::fs::OFlags::RDONLY)?
            {
                validate_private_file(&destination.metadata().map_err(SecureStoreError::Io)?)?;
                verify_current_entry(&self.directory.directory, name, &destination)?;
            }

            let temporary = format!(".{name}.tmp-{}", Uuid::now_v7());
            let mut file = create_file(&self.directory.directory, &temporary)?;
            let result = (|| {
                file.write_all(bytes).map_err(SecureStoreError::Io)?;
                file.sync_all().map_err(SecureStoreError::Io)?;
                verify_current_entry(&self.directory.directory, &temporary, &file)?;
                self.verify_lock()?;
                rustix::fs::renameat(
                    &self.directory.directory,
                    temporary.as_str(),
                    &self.directory.directory,
                    name,
                )
                .map_err(io_error)?;
                let installed = open_existing_file(
                    &self.directory.directory,
                    name,
                    rustix::fs::OFlags::RDONLY,
                )?
                .ok_or(SecureStoreError::UnsafePath)?;
                verify_current_entry(&self.directory.directory, name, &installed)?;
                rustix::fs::fsync(&self.directory.directory).map_err(io_error)?;
                self.verify_lock()?;
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
        #[cfg(not(unix))]
        {
            let _ = (name, bytes);
            Err(SecureStoreError::UnsupportedPlatform)
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
            self.verify_lock()?;
            if let Some(file) =
                open_existing_file(&self.directory.directory, name, rustix::fs::OFlags::RDONLY)?
            {
                verify_current_entry(&self.directory.directory, name, &file)?;
                rustix::fs::unlinkat(
                    &self.directory.directory,
                    name,
                    rustix::fs::AtFlags::empty(),
                )
                .map_err(io_error)?;
                rustix::fs::fsync(&self.directory.directory).map_err(io_error)?;
            }
            self.verify_lock()?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            Err(SecureStoreError::UnsupportedPlatform)
        }
    }

    #[cfg(unix)]
    fn verify_lock(&self) -> Result<(), SecureStoreError> {
        verify_current_entry(&self.directory.directory, &self.lock_name, &self._lock)
    }
}

/// Fail-closed storage errors with no file contents or secret values.
#[derive(Debug, Error)]
pub enum SecureStoreError {
    #[error("could not determine the home directory")]
    HomeUnavailable,
    #[error("secure storage is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("secure storage path is unsafe")]
    UnsafePath,
    #[error("secure storage object exceeds its byte limit")]
    TooLarge,
    #[error("secure storage I/O failed")]
    Io(#[source] io::Error),
}

#[derive(Clone, Copy)]
enum DirectoryPolicy {
    SafeAnchor,
    UntrustedProjectAnchor,
    Private,
}

#[cfg(unix)]
fn open_absolute_directory(
    path: &Path,
    policy: DirectoryPolicy,
) -> Result<fs::File, SecureStoreError> {
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
                validate_directory_type(&current.metadata().map_err(SecureStoreError::Io)?)?;
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(SecureStoreError::UnsafePath);
            }
        }
    }
    validate_directory(&current.metadata().map_err(SecureStoreError::Io)?, policy)?;
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
    policy: DirectoryPolicy,
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
        validate_directory(&next.metadata().map_err(SecureStoreError::Io)?, policy)?;
        current = next;
    }
    Ok(current)
}

#[cfg(unix)]
fn directory_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC
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
    verify_current_entry(directory, name, &file)?;
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
    let flags = access
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC;
    match rustix::fs::openat(directory, name, flags, rustix::fs::Mode::empty()) {
        Ok(fd) => {
            let file = fs::File::from(fd);
            validate_private_file(&file.metadata().map_err(SecureStoreError::Io)?)?;
            Ok(Some(file))
        }
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
    validate_private_file(&file.metadata().map_err(SecureStoreError::Io)?)?;
    Ok(file)
}

#[cfg(unix)]
fn verify_current_entry(
    directory: &fs::File,
    name: &str,
    held: &fs::File,
) -> Result<(), SecureStoreError> {
    use std::os::unix::fs::MetadataExt as _;

    let current = open_existing_file(directory, name, rustix::fs::OFlags::RDONLY)?
        .ok_or(SecureStoreError::UnsafePath)?;
    let held_metadata = held.metadata().map_err(SecureStoreError::Io)?;
    let current_metadata = current.metadata().map_err(SecureStoreError::Io)?;
    if held_metadata.dev() == current_metadata.dev()
        && held_metadata.ino() == current_metadata.ino()
        && held_metadata.nlink() == 1
        && current_metadata.nlink() == 1
    {
        Ok(())
    } else {
        Err(SecureStoreError::UnsafePath)
    }
}

#[cfg(unix)]
fn validate_directory_type(metadata: &fs::Metadata) -> Result<(), SecureStoreError> {
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(SecureStoreError::UnsafePath)
    }
}

#[cfg(unix)]
fn validate_directory(
    metadata: &fs::Metadata,
    policy: DirectoryPolicy,
) -> Result<(), SecureStoreError> {
    use std::os::unix::fs::MetadataExt as _;

    validate_directory_type(metadata)?;
    let owner_ok = metadata.uid() == rustix::process::getuid().as_raw();
    let mode = metadata.mode() & 0o777;
    let mode_ok = match policy {
        DirectoryPolicy::SafeAnchor => mode & 0o022 == 0,
        DirectoryPolicy::UntrustedProjectAnchor => true,
        DirectoryPolicy::Private => mode == PRIVATE_DIRECTORY_MODE,
    };
    let owner_ok = matches!(policy, DirectoryPolicy::UntrustedProjectAnchor) || owner_ok;
    if owner_ok && mode_ok {
        Ok(())
    } else {
        Err(SecureStoreError::UnsafePath)
    }
}

#[cfg(unix)]
fn validate_private_file(metadata: &fs::Metadata) -> Result<(), SecureStoreError> {
    use std::os::unix::fs::MetadataExt as _;

    if metadata.file_type().is_file()
        && !metadata.file_type().is_symlink()
        && metadata.uid() == rustix::process::getuid().as_raw()
        && metadata.mode() & 0o777 == PRIVATE_FILE_MODE
        && metadata.nlink() == 1
    {
        Ok(())
    } else {
        Err(SecureStoreError::UnsafePath)
    }
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
        SecureStoreError::UnsafePath
    } else {
        SecureStoreError::Io(error)
    }
}

#[cfg(unix)]
fn io_error(error: rustix::io::Errno) -> SecureStoreError {
    SecureStoreError::Io(error.into())
}
