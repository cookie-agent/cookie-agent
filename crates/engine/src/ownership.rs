use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use fs2::FileExt as _;

const OWNER_LOCK_FILE: &str = "owner.lock";

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

pub(crate) fn try_acquire(session_dir: &Path) -> std::io::Result<SessionOwnership> {
    let path = session_dir.join(OWNER_LOCK_FILE);
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
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            Ok(SessionOwnership::Foreign)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionOwnership, try_acquire};

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
