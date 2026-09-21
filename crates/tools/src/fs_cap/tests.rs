use std::{ffi::OsString, fs, os::unix::fs::symlink, sync::atomic::AtomicUsize};

use cookie_agent_engine::ToolError;
use cookie_agent_protocol::Sha256Digest;

use super::unix::{
    ChainNode, ExchangeFailurePoint, ObjectIdentity, existing_manifest_bytes,
    inject_exchange_failure, reserve_weight,
};
use super::{PreparedTarget, prepare_existing, prepare_target};

#[test]
fn read_capability_rejects_in_place_leaf_and_ancestor_changes() {
    let root = tempfile::tempdir().expect("tempdir");
    let directory = root.path().join("tree");
    fs::create_dir(&directory).expect("directory");
    let path = directory.join("value.txt");
    fs::write(&path, "alpha").expect("fixture");

    let in_place =
        prepare_existing(root.path(), std::path::Path::new("tree/value.txt")).expect("prepare");
    fs::write(&path, "bravo").expect("change content");
    assert!(matches!(
        in_place.revalidate(),
        Err(ToolError::OperationChanged(_))
    ));

    fs::write(&path, "alpha").expect("restore");
    let leaf = prepare_existing(root.path(), std::path::Path::new("tree/value.txt"))
        .expect("prepare leaf");
    fs::rename(&path, directory.join("old.txt")).expect("rename leaf");
    fs::write(&path, "alpha").expect("replacement leaf");
    assert!(matches!(
        leaf.revalidate(),
        Err(ToolError::OperationChanged(_))
    ));

    fs::remove_file(&path).expect("remove replacement");
    fs::rename(directory.join("old.txt"), &path).expect("restore leaf");
    let ancestor = prepare_existing(root.path(), std::path::Path::new("tree/value.txt"))
        .expect("prepare ancestor");
    fs::rename(&directory, root.path().join("old-tree")).expect("rename ancestor");
    fs::create_dir(&directory).expect("replacement ancestor");
    fs::write(&path, "alpha").expect("replacement content");
    assert!(matches!(
        ancestor.revalidate(),
        Err(ToolError::OperationChanged(_))
    ));
}

#[test]
fn symlink_targets_resolve_parent_components_after_following_links() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("actual/nested")).unwrap();
    fs::write(root.path().join("actual/value"), "resolved").unwrap();
    fs::write(root.path().join("value"), "lexical").unwrap();
    symlink("actual/nested", root.path().join("directory")).unwrap();
    symlink("directory/../value", root.path().join("alias")).unwrap();
    let target = prepare_existing(root.path(), std::path::Path::new("alias")).unwrap();
    assert_eq!(target.verified_bytes().unwrap(), b"resolved");
    target.replace_atomically(b"updated").unwrap();
    assert_eq!(
        fs::read(root.path().join("actual/value")).unwrap(),
        b"updated"
    );
    assert_eq!(fs::read(root.path().join("value")).unwrap(), b"lexical");
    assert_eq!(
        fs::read_link(root.path().join("alias")).unwrap(),
        std::path::Path::new("directory/../value")
    );

    symlink(
        "directory/../missing/deep/value",
        root.path().join("dangling"),
    )
    .unwrap();
    let PreparedTarget::Absent(absent) =
        prepare_target(root.path(), std::path::Path::new("dangling")).unwrap()
    else {
        panic!("absent")
    };
    absent.create_atomically(b"created").unwrap();
    assert_eq!(
        fs::read(root.path().join("actual/missing/deep/value")).unwrap(),
        b"created"
    );
    assert!(
        fs::symlink_metadata(root.path().join("dangling"))
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn traversed_but_exited_ancestors_and_chained_links_remain_bound() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("actual/nested")).unwrap();
    fs::write(root.path().join("actual/value"), "value").unwrap();
    symlink("actual/nested", root.path().join("directory")).unwrap();
    symlink("directory/../value", root.path().join("alias")).unwrap();
    let target = prepare_existing(root.path(), std::path::Path::new("alias")).unwrap();
    fs::rename(root.path().join("actual/nested"), root.path().join("saved")).unwrap();
    fs::create_dir(root.path().join("actual/nested")).unwrap();
    assert!(matches!(
        target.revalidate(),
        Err(ToolError::OperationChanged(_))
    ));

    let target = prepare_existing(root.path(), std::path::Path::new("alias")).unwrap();
    let binding = target.manifest_bytes().unwrap();
    fs::rename(
        root.path().join("directory"),
        root.path().join("saved-link"),
    )
    .unwrap();
    symlink("actual/nested", root.path().join("directory")).unwrap();
    assert!(matches!(
        target.revalidate(),
        Err(ToolError::OperationChanged(_))
    ));
    let replacement = prepare_existing(root.path(), std::path::Path::new("alias")).unwrap();
    assert_ne!(binding, replacement.manifest_bytes().unwrap());
    assert_eq!(target.identity, replacement.identity);
}

#[test]
fn alias_retargeting_at_commit_barriers_preserves_destinations() {
    for missing in [None, Some("new"), Some("subtree/deep/new")] {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("existing"), "original").unwrap();
        fs::write(root.path().join("other"), "other").unwrap();
        let destination = missing.unwrap_or("existing");
        let alias = root.path().join("alias");
        symlink(destination, &alias).unwrap();
        let target = prepare_target(root.path(), std::path::Path::new("alias")).unwrap();
        let retarget = || {
            fs::rename(&alias, root.path().join("saved-alias")).unwrap();
            symlink("other", &alias).unwrap();
        };
        let result = match target {
            PreparedTarget::Existing(target) => target.replace_atomically_inner(
                b"new",
                retarget,
                &super::unix::DisplacedExpectation::PreparedIdentity,
            ),
            PreparedTarget::Absent(target) => target.create_atomically_inner(b"new", retarget),
        };
        assert!(matches!(result, Err(ToolError::OperationChanged(_))));
        assert_eq!(fs::read(root.path().join("existing")).unwrap(), b"original");
        assert_eq!(fs::read(root.path().join("other")).unwrap(), b"other");
        if let Some(missing) = missing {
            assert!(!root.path().join(missing).exists());
        }
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cookie-agent-")
        }));
    }
}

#[test]
fn symlink_cycles_and_missing_parent_traversal_fail_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    symlink("two", root.path().join("one")).unwrap();
    symlink("one", root.path().join("two")).unwrap();
    symlink("missing/../value", root.path().join("dangling")).unwrap();
    for path in ["one", "one/child", "dangling"] {
        assert!(prepare_target(root.path(), std::path::Path::new(path)).is_err());
    }
    assert!(!root.path().join("missing").exists());
    assert!(!root.path().join("value").exists());
}

#[test]
fn link_target_directory_suffix_cannot_be_written_as_a_file() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("file"), "value").unwrap();
    for (index, destination) in ["file/", "file/.", "missing/", "missing/."]
        .into_iter()
        .enumerate()
    {
        let alias = format!("alias-{index}");
        symlink(destination, root.path().join(&alias)).unwrap();
        assert!(prepare_target(root.path(), std::path::Path::new(&alias)).is_err());
    }
    assert_eq!(fs::read(root.path().join("file")).unwrap(), b"value");
    assert!(!root.path().join("missing").exists());
}

#[test]
fn write_capability_rejects_symlink_swaps_and_absent_target_creation() {
    let root = tempfile::tempdir().expect("tempdir");
    fs::write(root.path().join("target"), "old").expect("fixture");
    let PreparedTarget::Existing(existing) =
        prepare_target(root.path(), std::path::Path::new("target")).expect("prepare")
    else {
        panic!("existing target")
    };
    fs::rename(root.path().join("target"), root.path().join("saved")).expect("rename");
    symlink("saved", root.path().join("target")).expect("symlink");
    assert!(matches!(
        existing.replace_atomically(b"new"),
        Err(ToolError::OperationChanged(_))
    ));
    assert_eq!(
        fs::read_to_string(root.path().join("saved")).expect("saved"),
        "old"
    );

    let PreparedTarget::Absent(absent) =
        prepare_target(root.path(), std::path::Path::new("new-file")).expect("prepare absent")
    else {
        panic!("absent target")
    };
    fs::write(root.path().join("new-file"), "attacker").expect("create target");
    assert!(matches!(
        absent.create_atomically(b"new"),
        Err(ToolError::OperationChanged(_))
    ));
    assert_eq!(
        fs::read_to_string(root.path().join("new-file")).expect("attacker file"),
        "attacker"
    );
    assert!(fs::read_dir(root.path()).expect("directory").all(|entry| {
        !entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .starts_with(".cookie-agent-stage-")
    }));
}

#[test]
fn atomic_replace_and_no_replace_publish_complete_content() {
    let root = tempfile::tempdir().expect("tempdir");
    fs::write(root.path().join("existing"), "old").expect("fixture");
    let PreparedTarget::Existing(existing) =
        prepare_target(root.path(), std::path::Path::new("existing")).expect("prepare")
    else {
        panic!("existing")
    };
    existing.replace_atomically(b"new-value").expect("replace");
    assert_eq!(
        fs::read(root.path().join("existing")).expect("read"),
        b"new-value"
    );

    let PreparedTarget::Absent(absent) =
        prepare_target(root.path(), std::path::Path::new("absent")).expect("prepare")
    else {
        panic!("absent")
    };
    absent.create_atomically(b"complete").expect("create");
    assert_eq!(
        fs::read(root.path().join("absent")).expect("read"),
        b"complete"
    );
}

#[test]
fn existing_manifest_encoding_is_golden() {
    let root = tempfile::tempdir().expect("root");
    let parent_file = fs::File::open(root.path()).expect("parent");
    let chain = vec![ChainNode {
        parent: parent_file,
        name: OsString::from("ancestor"),
        _object: fs::File::open(root.path()).expect("ancestor"),
        link_target: None,
        identity: ObjectIdentity {
            device: 1,
            inode: 2,
            mode: 0o040700,
            size: 3,
        },
    }];
    let bytes = existing_manifest_bytes(
        &chain,
        &ObjectIdentity {
            device: 4,
            inode: 5,
            mode: 0o040700,
            size: 6,
        },
        std::ffi::OsStr::new("leaf"),
        &ObjectIdentity {
            device: 7,
            inode: 8,
            mode: 0o100600,
            size: 9,
        },
        false,
        &Sha256Digest::of_bytes(b"content"),
    );
    assert_eq!(
        Sha256Digest::of_bytes(&bytes).as_str(),
        "d926a6519d1bdf345feac9d76c8d7b9eaee841e843e6bffcc480ee1ec29ae6de"
    );
}

#[test]
fn unsupported_paths_and_capability_drops_do_not_leak_descriptors() {
    let root = tempfile::tempdir().expect("tempdir");
    fs::write(root.path().join("file"), "value").expect("fixture");
    symlink("file", root.path().join("link")).expect("symlink");
    drop(prepare_existing(root.path(), std::path::Path::new("link")).expect("symlink"));
    symlink("loop", root.path().join("loop")).expect("loop");
    let PreparedTarget::Absent(missing) =
        prepare_target(root.path(), std::path::Path::new("missing/subtree/file"))
            .expect("prepare missing subtree")
    else {
        panic!("missing subtree target")
    };
    missing
        .create_atomically(b"published")
        .expect("publish subtree");
    assert_eq!(
        fs::read(root.path().join("missing/subtree/file")).expect("published file"),
        b"published"
    );
    let descriptors_for_root = || {
        fs::read_dir("/proc/self/fd")
            .expect("fd directory")
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|target| target.starts_with(root.path()))
            .count()
    };
    let before = descriptors_for_root();
    for _ in 0..128 {
        drop(prepare_existing(root.path(), std::path::Path::new("file")).expect("prepare"));
        drop(prepare_existing(root.path(), std::path::Path::new("link")).expect("prepare link"));
        assert!(matches!(
            prepare_existing(root.path(), std::path::Path::new("loop")),
            Err(ToolError::UnsupportedSecurity(_))
        ));
    }
    let after = descriptors_for_root();
    assert_eq!(
        after, before,
        "prepared descriptors leaked for fixture root"
    );
}

#[test]
fn missing_subtree_fails_if_any_component_is_inserted() {
    let root = tempfile::tempdir().expect("tempdir");
    let PreparedTarget::Absent(absent) =
        prepare_target(root.path(), std::path::Path::new("one/two/file")).expect("prepare")
    else {
        panic!("absent target")
    };
    fs::create_dir(root.path().join("one")).expect("attacker insertion");
    assert!(matches!(
        absent.create_atomically(b"content"),
        Err(ToolError::OperationChanged(_))
    ));
    assert!(!root.path().join("one/two/file").exists());
}

#[test]
fn write_manifests_distinguish_state_parent_basename_and_preimage() {
    let root = tempfile::tempdir().expect("tempdir");
    fs::write(root.path().join("a"), "same").expect("a");
    fs::write(root.path().join("b"), "same").expect("b");
    let a = prepare_target(root.path(), std::path::Path::new("a")).expect("prepare a");
    let b = prepare_target(root.path(), std::path::Path::new("b")).expect("prepare b");
    let absent = prepare_target(root.path(), std::path::Path::new("c")).expect("prepare c");
    assert_ne!(
        a.manifest_bytes().expect("manifest"),
        b.manifest_bytes().expect("manifest")
    );
    assert_ne!(
        a.manifest_bytes().expect("manifest"),
        absent.manifest_bytes().expect("manifest")
    );
}

#[test]
fn aggregate_budget_exhaustion_is_reported_without_opening_descriptors() {
    let counter = AtomicUsize::new(7);
    assert!(matches!(
        reserve_weight(&counter, 8, 2),
        Err(ToolError::ResourceLimit(_))
    ));
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 7);
}

#[test]
fn exchange_rollback_preserves_racing_symlink_and_cleans_stage() {
    let root = tempfile::tempdir().expect("tempdir");
    fs::write(root.path().join("target"), "original").expect("target");
    fs::write(root.path().join("attacker"), "attacker").expect("attacker");
    let PreparedTarget::Existing(existing) =
        prepare_target(root.path(), std::path::Path::new("target")).expect("prepare")
    else {
        panic!("existing target")
    };
    let target = root.path().join("target");
    let saved = root.path().join("saved");
    let result = existing.replace_atomically_inner(
        b"new",
        || {
            fs::rename(&target, &saved).expect("move target");
            symlink("attacker", &target).expect("racing symlink");
        },
        &super::unix::DisplacedExpectation::PreparedIdentity,
    );
    assert!(matches!(result, Err(ToolError::OperationChanged(_))));
    assert_eq!(
        fs::read_link(&target).expect("symlink retained"),
        std::path::PathBuf::from("attacker")
    );
    assert_eq!(
        fs::read_to_string(&saved).expect("original retained"),
        "original"
    );
    assert!(fs::read_dir(root.path()).expect("directory").all(|entry| {
        !entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .starts_with(".cookie-agent-stage-")
    }));
}

#[test]
fn every_exchange_precommit_failure_rolls_back_and_cleans_stage() {
    for point in [
        ExchangeFailurePoint::OpenDisplaced,
        ExchangeFailurePoint::StatDisplaced,
        ExchangeFailurePoint::ReadDisplaced,
        ExchangeFailurePoint::IdentityMismatch,
        ExchangeFailurePoint::DigestMismatch,
        ExchangeFailurePoint::CommitFsync,
    ] {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("target"), "original").expect("fixture");
        let PreparedTarget::Existing(existing) =
            prepare_target(root.path(), std::path::Path::new("target")).expect("prepare")
        else {
            panic!("existing target")
        };
        inject_exchange_failure(point);
        assert!(
            existing.replace_atomically(b"replacement").is_err(),
            "{point:?}"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("target")).expect("target"),
            "original",
            "{point:?}"
        );
        assert!(fs::read_dir(root.path()).expect("directory").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".cookie-agent-stage-")
        }));
    }
}

#[test]
fn postcommit_cleanup_failures_report_committed_success() {
    for point in [
        ExchangeFailurePoint::CleanupUnlink,
        ExchangeFailurePoint::CleanupFsync,
    ] {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("target"), "original").expect("fixture");
        let PreparedTarget::Existing(existing) =
            prepare_target(root.path(), std::path::Path::new("target")).expect("prepare")
        else {
            panic!("existing target")
        };
        inject_exchange_failure(point);
        let outcome = existing
            .replace_atomically(b"replacement")
            .expect("committed success");
        assert!(outcome.cleanup_warning.is_some(), "{point:?}");
        assert_eq!(
            fs::read_to_string(root.path().join("target")).expect("target"),
            "replacement",
            "{point:?}"
        );
    }
}

#[test]
fn absent_leaf_insertion_at_publish_barrier_is_operation_changed() {
    let root = tempfile::tempdir().expect("root");
    let PreparedTarget::Absent(absent) =
        prepare_target(root.path(), std::path::Path::new("target")).expect("prepare")
    else {
        panic!("absent target")
    };
    let target = root.path().join("target");
    let result = absent.create_atomically_inner(b"new", || {
        fs::write(&target, "attacker").expect("insert target");
    });
    assert!(matches!(result, Err(ToolError::OperationChanged(_))));
    assert_eq!(fs::read_to_string(target).expect("target"), "attacker");
}

#[test]
fn missing_subtree_insertion_at_publish_barrier_is_operation_changed() {
    let root = tempfile::tempdir().expect("root");
    let PreparedTarget::Absent(absent) =
        prepare_target(root.path(), std::path::Path::new("one/two/target")).expect("prepare")
    else {
        panic!("absent target")
    };
    let inserted = root.path().join("one");
    let result = absent.create_atomically_inner(b"new", || {
        fs::create_dir(&inserted).expect("insert subtree");
        fs::write(inserted.join("attacker"), "attacker").expect("attacker");
    });
    assert!(matches!(result, Err(ToolError::OperationChanged(_))));
    assert_eq!(
        fs::read_to_string(inserted.join("attacker")).expect("attacker"),
        "attacker"
    );
    assert!(!root.path().join("one/two/target").exists());
}
