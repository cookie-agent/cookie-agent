#![cfg(windows)]

use cookie_agent_models::secure_store::{
    SecureDirectory, SecureStoreError, validate_windows_path_acl,
};

#[test]
fn windows_private_store_applies_acl_and_round_trips_transactions() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let directory = SecureDirectory::open_in(temporary.path(), "private").expect("secure store");
    validate_windows_path_acl(directory.path()).expect("private directory ACL");

    let lock = directory.lock("state.lock").expect("lock");
    lock.atomic_replace("state.json", br#"{"ok":true}"#)
        .expect("replace");
    validate_windows_path_acl(&directory.path().join("state.lock")).expect("lock ACL");
    validate_windows_path_acl(&directory.path().join("state.json")).expect("file ACL");
    assert_eq!(
        lock.read("state.json", 1024).expect("read"),
        Some(br#"{"ok":true}"#.to_vec())
    );
}

#[test]
fn windows_private_store_rejects_reparse_descendants() {
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
    assert!(matches!(
        SecureDirectory::open_in(temporary.path(), "link/child"),
        Err(SecureStoreError::UnsafePath)
    ));
}

#[test]
fn windows_private_store_rejects_preexisting_untrusted_acl() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let preexisting = temporary.path().join("preexisting");
    std::fs::create_dir(&preexisting).expect("ordinary directory");
    assert!(SecureDirectory::open_in(temporary.path(), "preexisting").is_err());
}
