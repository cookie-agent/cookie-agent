use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use fs2::FileExt as _;

#[cfg(any(unix, test))]
const OWNER_LOCK_FILE: &str = "owner.lock";
#[cfg(windows)]
const OWNER_LOCK_EXTENSION: &str = "owner.lock";
#[cfg(test)]
const OWNER_LOCK_SUFFIX: &str = ".owner.lock";

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

#[cfg(test)]
pub(crate) fn is_owner_lock_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if name == OWNER_LOCK_FILE {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(is_canonical_uuid);
    }
    name.strip_suffix(OWNER_LOCK_SUFFIX)
        .is_some_and(is_canonical_uuid)
}

#[cfg(test)]
fn is_canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.hyphenated().to_string() == value)
}

pub(crate) fn try_acquire(session_dir: &Path) -> std::io::Result<SessionOwnership> {
    let path = owner_lock_path(session_dir);
    #[cfg(windows)]
    let file = match cookie_agent_models::secure_store::create_windows_private_lock_file(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Denying delete sharing prevents another process from renaming or replacing
            // the pathname while this handle is the session's ownership authority.
            cookie_agent_models::secure_store::open_windows_private_lock_file(&path)?
        }
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    let file = {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
        options.open(path)?
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            #[cfg(unix)]
            repair_unix_lock_permissions(&file)?;
            #[cfg(windows)]
            repair_windows_lock_acl(&file)?;
            Ok(SessionOwnership::Owned(HeldLock { _file: file }))
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Ok(SessionOwnership::Foreign)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn repair_unix_lock_permissions(file: &fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions and does not retain pointers.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "session ownership lock has a foreign owner",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn repair_windows_lock_acl(file: &fs::File) -> std::io::Result<()> {
    // ACL operations must target the LockFileEx handle. A pathname lookup could inspect a
    // replacement while HeldLock still protects the old file object, creating two owners.
    if cookie_agent_models::secure_store::verify_windows_private_file_handle(file).is_err() {
        cookie_agent_models::secure_store::repair_windows_private_file_handle_acl(file)?;
        cookie_agent_models::secure_store::verify_windows_private_file_handle(file)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{SessionOwnership, is_owner_lock_path, owner_lock_path, try_acquire};

    #[cfg(unix)]
    #[test]
    fn owner_lock_is_inside_the_session_directory_on_unix() {
        let session_dir = Path::new("sessions").join("0198a5d4-4216-7c73-8385-6954cb683af1");
        let lock = owner_lock_path(&session_dir);
        assert_eq!(lock, session_dir.join("owner.lock"));
        assert!(is_owner_lock_path(&lock));
    }

    #[cfg(windows)]
    #[test]
    fn owner_lock_is_a_session_sidecar_on_windows() {
        let session_dir = Path::new("sessions").join("0198a5d4-4216-7c73-8385-6954cb683af1");
        let lock = owner_lock_path(&session_dir);
        assert_eq!(
            lock,
            Path::new("sessions").join("0198a5d4-4216-7c73-8385-6954cb683af1.owner.lock")
        );
        assert!(is_owner_lock_path(&lock));
    }

    #[test]
    fn owner_lock_artifacts_recognize_only_valid_cross_platform_layouts() {
        let id = "0198a5d4-4216-7c73-8385-6954cb683af1";
        assert!(is_owner_lock_path(
            &Path::new("sessions").join(id).join("owner.lock")
        ));
        assert!(is_owner_lock_path(
            &Path::new("sessions").join(format!("{id}.owner.lock"))
        ));
        for path in [
            Path::new("sessions").join("owner.lock"),
            Path::new("sessions").join("not-a-uuid").join("owner.lock"),
            Path::new("sessions").join(".owner.lock"),
            Path::new("sessions").join("not-a-uuid.owner.lock"),
            Path::new("sessions").join("0198a5d442167c7383856954cb683af1.owner.lock"),
            Path::new("sessions").join("0198A5D4-4216-7C73-8385-6954CB683AF1.owner.lock"),
            Path::new("sessions").join(format!("{id}.not-owner.lock")),
        ] {
            assert!(!is_owner_lock_path(&path), "accepted {path:?}");
        }
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

    #[cfg(unix)]
    #[test]
    fn acquiring_an_existing_unix_lock_repairs_its_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("temporary directory");
        let lock_path = owner_lock_path(directory.path());
        fs::write(&lock_path, []).expect("legacy lock file");
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o666))
            .expect("legacy lock mode");

        let lock = try_acquire(directory.path()).expect("repair and acquire");

        assert!(matches!(lock, SessionOwnership::Owned(_)));
        assert_eq!(
            fs::metadata(lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(windows)]
    #[test]
    fn acquiring_an_existing_windows_lock_repairs_its_acl() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let sessions = directory.path().join("sessions");
        cookie_agent_models::secure_store::SecureDirectory::open(&sessions)
            .expect("private sessions directory");
        let session_dir = sessions.join("0198a5d4-4216-7c73-8385-6954cb683af1");
        let lock_path = owner_lock_path(&session_dir);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .expect("legacy inherited-ACL sidecar");
        assert!(
            cookie_agent_models::secure_store::verify_windows_private_creation(&lock_path).is_err()
        );

        let lock = try_acquire(&session_dir).expect("repair and acquire");

        let SessionOwnership::Owned(lock) = lock else {
            panic!("legacy sidecar was not acquired");
        };
        cookie_agent_models::secure_store::verify_windows_private_file_handle(&lock._file)
            .expect("repaired locked-handle ACL");
        let renamed = lock_path.with_extension("renamed");
        assert!(
            fs::rename(&lock_path, renamed).is_err(),
            "held ownership handle must deny pathname replacement"
        );
    }
}
