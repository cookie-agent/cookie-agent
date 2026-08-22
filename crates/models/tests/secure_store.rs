#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink},
    sync::mpsc,
    thread,
    time::Duration,
};

use cookie_agent_models::secure_store::{SecureDirectory, SecureStoreError};

fn directory(temporary: &tempfile::TempDir) -> SecureDirectory {
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    SecureDirectory::open_in(temporary.path(), "store").unwrap()
}

#[test]
fn creates_private_files_and_durably_replaces_them() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = directory(&temporary);
    {
        let lock = directory.lock("state.lock").unwrap();
        lock.atomic_replace("state.json", b"one").unwrap();
        lock.atomic_replace("state.json", b"two").unwrap();
    }
    assert_eq!(directory.read("state.json", 16).unwrap().unwrap(), b"two");
    let root = temporary.path().join("store");
    assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
    for name in ["state.lock", "state.json"] {
        let metadata = fs::metadata(root.join(name)).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(metadata.nlink(), 1);
    }
}

#[test]
fn uses_symlinks_hardlinks_fifos_and_weak_modes() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = directory(&temporary);
    let root = temporary.path().join("store");

    fs::write(root.join("target"), b"secret").unwrap();
    fs::set_permissions(root.join("target"), fs::Permissions::from_mode(0o600)).unwrap();
    symlink("target", root.join("link")).unwrap();
    assert_eq!(
        directory.read("link", 16).unwrap(),
        Some(b"secret".to_vec())
    );
    let lock = directory.lock("state.lock").unwrap();
    lock.atomic_replace("link", b"replacement").unwrap();
    drop(lock);
    assert_eq!(fs::read(root.join("target")).unwrap(), b"secret");
    assert_eq!(
        directory.read("link", 16).unwrap(),
        Some(b"replacement".to_vec())
    );

    fs::hard_link(root.join("target"), root.join("hard")).unwrap();
    assert_eq!(
        directory.read("hard", 16).unwrap(),
        Some(b"secret".to_vec())
    );

    let fifo = std::ffi::CString::new(root.join("fifo").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert_eq!(directory.read("fifo", 16).unwrap(), Some(Vec::new()));

    symlink("/dev/null", root.join("device")).unwrap();
    assert_eq!(directory.read("device", 16).unwrap(), Some(Vec::new()));

    fs::write(root.join("weak"), b"weak").unwrap();
    fs::set_permissions(root.join("weak"), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(directory.read("weak", 16).unwrap(), Some(b"weak".to_vec()));
}

#[test]
fn uses_loose_existing_directories_and_bounds_reads() {
    let temporary = tempfile::tempdir().unwrap();
    fs::create_dir(temporary.path().join("weak-root")).unwrap();
    fs::set_permissions(
        temporary.path().join("weak-root"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    SecureDirectory::open_in(temporary.path(), "weak-root").expect("loose directory");

    let directory = directory(&temporary);
    let lock = directory.lock("state.lock").unwrap();
    lock.atomic_replace("large", b"12345").unwrap();
    drop(lock);
    assert!(matches!(
        directory.read("large", 4),
        Err(SecureStoreError::TooLarge)
    ));
}

#[test]
fn independent_descriptors_serialize_cross_process_style_locking() {
    let temporary = tempfile::tempdir().unwrap();
    let first = directory(&temporary);
    let second = SecureDirectory::open_in(temporary.path(), "store").unwrap();
    let held = first.lock("state.lock").unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (acquired_tx, acquired_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        started_tx.send(()).unwrap();
        let _lock = second.lock("state.lock").unwrap();
        acquired_tx.send(()).unwrap();
    });
    started_rx.recv().unwrap();
    assert!(
        acquired_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    drop(held);
    acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    handle.join().unwrap();
}

#[test]
fn lock_replacement_does_not_block_atomic_publication() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = directory(&temporary);
    let root = temporary.path().join("store");
    let held = directory.lock("state.lock").unwrap();
    fs::rename(root.join("state.lock"), root.join("displaced.lock")).unwrap();
    fs::write(root.join("state.lock"), b"").unwrap();
    fs::set_permissions(root.join("state.lock"), fs::Permissions::from_mode(0o600)).unwrap();
    held.atomic_replace("state.json", b"write").unwrap();
    assert_eq!(fs::read(root.join("state.json")).unwrap(), b"write");
}

#[test]
fn wrong_owner_is_used_when_the_test_runner_can_create_it() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = directory(&temporary);
    let root = temporary.path().join("store");
    fs::write(root.join("owner"), b"owner").unwrap();
    fs::set_permissions(root.join("owner"), fs::Permissions::from_mode(0o600)).unwrap();
    if unsafe { libc::geteuid() } == 0 {
        let path =
            std::ffi::CString::new(root.join("owner").as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::chown(path.as_ptr(), 1, 1) }, 0);
        assert_eq!(
            directory.read("owner", 16).unwrap(),
            Some(b"owner".to_vec())
        );
    } else {
        assert_eq!(fs::metadata(root.join("owner")).unwrap().uid(), unsafe {
            libc::geteuid()
        });
    }
}
