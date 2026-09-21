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

/// A by-name reader must never observe the target absent while it is replaced.
/// The session metadata cache is rewritten on every persisted append and
/// discovery reads it by name, so a replacement window that leaves the name
/// unresolvable surfaces as a spurious `NotFound` during listing.
#[test]
fn windows_replacement_is_never_observed_absent_by_a_concurrent_reader() {
    use cookie_agent_models::secure_store::{
        create_windows_private_dir_all, create_windows_private_file, replace_windows_path,
    };
    use std::{
        io::Write as _,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    const REPLACEMENTS: usize = 2_000;
    const EVEN: &[u8] = br#"{"generation":"even","payload":"aaaaaaaaaaaaaaaaaaaaaaaa"}"#;
    const ODD: &[u8] = br#"{"generation":"odd","payload":"bbbbbbbbbbbbbbbbbbbbbbbbbb"}"#;

    fn stage(directory: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let staging = directory.join(name);
        let mut file = create_windows_private_file(&staging).expect("staging file");
        file.write_all(bytes).expect("write staging payload");
        file.sync_all().expect("flush staging payload");
        drop(file);
        staging
    }

    let temporary = tempfile::tempdir().expect("temporary root");
    let directory = temporary.path().join("store");
    create_windows_private_dir_all(&directory).expect("private directory");
    let target = directory.join("metadata");
    let seed = stage(&directory, ".metadata.seed.tmp", EVEN);
    replace_windows_path(&seed, &target).expect("seed the replaced target");

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let reader = std::thread::spawn({
        let target = target.clone();
        let stop = Arc::clone(&stop);
        let reads = Arc::clone(&reads);
        move || {
            while !stop.load(Ordering::Relaxed) {
                let bytes = std::fs::read(&target).unwrap_or_else(|error| {
                    panic!(
                        "replaced target stopped resolving by name: {error} (kind {:?})",
                        error.kind()
                    )
                });
                assert!(
                    bytes == EVEN || bytes == ODD,
                    "reader observed a partial payload of {} bytes",
                    bytes.len()
                );
                reads.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    for generation in 0..REPLACEMENTS {
        let payload = if generation % 2 == 0 { EVEN } else { ODD };
        let staging = stage(&directory, &format!(".metadata.{generation}.tmp"), payload);
        replace_windows_path(&staging, &target).expect("replace the target");
    }
    stop.store(true, Ordering::Relaxed);
    reader
        .join()
        .expect("reader never observed a missing or partial target");
    assert!(
        reads.load(Ordering::Relaxed) > 0,
        "reader never completed a read"
    );
    assert_eq!(
        std::fs::read(&target).expect("final read"),
        ODD,
        "the last replacement wins"
    );
}
