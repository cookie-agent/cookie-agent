#![cfg(windows)]

use cookie_agent_models::secure_store::{SecureDirectory, verify_windows_private_creation};

#[test]
fn windows_private_store_applies_acl_and_round_trips_transactions() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let directory = SecureDirectory::open_in(temporary.path(), "private").expect("secure store");
    verify_windows_private_creation(directory.path()).expect("private directory ACL");

    let lock = directory.lock("state.lock").expect("lock");
    lock.atomic_replace("state.json", br#"{"ok":true}"#)
        .expect("replace");
    verify_windows_private_creation(&directory.path().join("state.lock")).expect("lock ACL");
    verify_windows_private_creation(&directory.path().join("state.json")).expect("file ACL");
    assert_eq!(
        lock.read("state.json", 1024).expect("read"),
        Some(br#"{"ok":true}"#.to_vec())
    );
}

#[test]
fn windows_private_store_uses_reparse_descendants() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let target = temporary.path().join("target");
    std::fs::create_dir(&target).expect("target");
    let link = temporary.path().join("link");
    if let Err(error) = std::os::windows::fs::symlink_dir(&target, &link) {
        // Creating symlinks can require Developer Mode on older runners.
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            return;
        }
        panic!("create directory symlink: {error}");
    }
    SecureDirectory::open_in(temporary.path(), "link/child").expect("symlinked store");
    assert!(target.join("child").is_dir());
}

#[test]
fn windows_private_store_uses_preexisting_untrusted_acl() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let preexisting = temporary.path().join("preexisting");
    std::fs::create_dir(&preexisting).expect("ordinary directory");
    SecureDirectory::open_in(temporary.path(), "preexisting")
        .expect("preexisting ordinary directory");
}

#[test]
fn windows_private_files_support_long_paths_and_keep_private_acls() {
    use cookie_agent_models::secure_store::{
        create_windows_private_dir_all, create_windows_private_file, replace_windows_path,
    };
    use std::{io::Write as _, os::windows::ffi::OsStrExt as _};

    let temporary = tempfile::tempdir().expect("temporary root");
    let directory = temporary
        .path()
        .join("nested-storage-".repeat(6))
        .join("child-storage-".repeat(6))
        .join("metadata-storage-".repeat(6));
    assert!(directory.as_os_str().encode_wide().count() > 260);
    create_windows_private_dir_all(&directory).expect("create long directory path");
    verify_windows_private_creation(&directory).expect("private directory ACL");
    let target = directory.join("metadata");
    for contents in [b"first".as_slice(), b"replacement".as_slice()] {
        let staging = directory.join(".metadata.tmp");
        let mut file = create_windows_private_file(&staging).expect("create long file path");
        file.write_all(contents).expect("write metadata");
        file.sync_all().expect("sync metadata");
        drop(file);
        verify_windows_private_creation(&staging).expect("private staging ACL");
        replace_windows_path(&staging, &target).expect("replace long file path");
        verify_windows_private_creation(&target).expect("private metadata ACL");
        assert_eq!(std::fs::read(&target).expect("read metadata"), contents);
    }
}
