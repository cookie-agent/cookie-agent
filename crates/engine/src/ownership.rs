use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use fs2::FileExt as _;

#[cfg(unix)]
const OWNER_LOCK_FILE: &str = "owner.lock";
#[cfg(windows)]
const OWNER_LOCK_EXTENSION: &str = "owner.lock";

#[derive(Debug)]
pub(crate) struct HeldLock {
    _file: fs::File,
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self._file);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WriteCapability {
    active: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(crate) struct WriteAuthority {
    active: Arc<AtomicBool>,
}

impl WriteAuthority {
    pub(crate) fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(crate) fn capability(&self) -> WriteCapability {
        WriteCapability {
            active: Arc::clone(&self.active),
        }
    }
}

impl Drop for WriteAuthority {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

impl WriteCapability {
    pub(crate) fn authorizes(&self, expected: &Self) -> bool {
        Arc::ptr_eq(&self.active, &expected.active) && self.active.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub(crate) enum SessionOwnership {
    Owned(HeldLock),
    Foreign,
}

pub(crate) fn owner_lock_path(session_dir: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        session_dir.join(OWNER_LOCK_FILE)
    }
    #[cfg(windows)]
    {
        session_dir.with_extension(OWNER_LOCK_EXTENSION)
    }
}

pub(crate) fn try_acquire(session_dir: &Path) -> std::io::Result<SessionOwnership> {
    let path = owner_lock_path(session_dir);
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    let file = options.open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(SessionOwnership::Owned(HeldLock { _file: file })),
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Ok(SessionOwnership::Foreign)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{SessionOwnership, owner_lock_path, try_acquire};

    #[cfg(unix)]
    #[test]
    fn owner_lock_is_inside_the_session_directory_on_unix() {
        let session_dir = Path::new("sessions").join("0198-session");
        assert_eq!(
            owner_lock_path(&session_dir),
            session_dir.join("owner.lock")
        );
    }

    #[cfg(windows)]
    #[test]
    fn owner_lock_is_a_session_sidecar_on_windows() {
        let session_dir = Path::new("sessions").join("0198-session");
        assert_eq!(
            owner_lock_path(&session_dir),
            Path::new("sessions").join("0198-session.owner.lock")
        );
    }

    #[test]
    fn a_second_descriptor_in_the_same_process_cannot_acquire_ownership() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = try_acquire(directory.path()).expect("first classification");
        assert!(matches!(first, SessionOwnership::Owned(_)));
        let second = try_acquire(directory.path()).expect("second classification");
        assert!(matches!(second, SessionOwnership::Foreign));
        drop(first);
        let third = try_acquire(directory.path()).expect("classification after drop");
        assert!(matches!(third, SessionOwnership::Owned(_)));
    }
}
