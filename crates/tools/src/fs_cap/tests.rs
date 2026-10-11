use std::{fs, io::Read, os::unix::fs::symlink, path::Path, sync::atomic::AtomicUsize};

use cookie_agent_engine::ToolError;

use super::unix::reserve_weight;
use super::{PreparedExisting, PreparedTarget, prepare_existing, prepare_target};

fn read(target: &PreparedExisting) -> String {
    let mut text = String::new();
    target
        .open_file()
        .expect("open prepared file")
        .read_to_string(&mut text)
        .expect("read prepared file");
    text
}

#[test]
fn execution_sees_content_written_after_preparation() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("value.txt"), "alpha").unwrap();
    let target = prepare_existing(root.path(), Path::new("value.txt")).unwrap();
    fs::write(root.path().join("value.txt"), "bravo").unwrap();
    assert_eq!(read(&target), "bravo");

    // An atomic replacement swaps the inode; the prepared location still reads it.
    fs::write(root.path().join("replacement"), "charlie").unwrap();
    fs::rename(
        root.path().join("replacement"),
        root.path().join("value.txt"),
    )
    .unwrap();
    assert_eq!(read(&target), "charlie");

    let absent = prepare_target(root.path(), Path::new("later.txt")).unwrap();
    assert!(matches!(absent, PreparedTarget::Absent(_)));
    fs::write(root.path().join("later.txt"), "concurrent").unwrap();
    absent.write(b"written").unwrap();
    assert_eq!(fs::read(root.path().join("later.txt")).unwrap(), b"written");
}

#[test]
fn symlink_targets_resolve_parent_components_after_following_links() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("actual/nested")).unwrap();
    fs::write(root.path().join("actual/value"), "resolved").unwrap();
    fs::write(root.path().join("value"), "lexical").unwrap();
    symlink("actual/nested", root.path().join("directory")).unwrap();
    symlink("directory/../value", root.path().join("alias")).unwrap();
    let target = prepare_existing(root.path(), Path::new("alias")).unwrap();
    assert_eq!(read(&target), "resolved");
    target.replace(b"updated").unwrap();
    assert_eq!(
        fs::read(root.path().join("actual/value")).unwrap(),
        b"updated"
    );
    assert_eq!(fs::read(root.path().join("value")).unwrap(), b"lexical");
    assert_eq!(
        fs::read_link(root.path().join("alias")).unwrap(),
        Path::new("directory/../value")
    );

    symlink(
        "directory/../missing/deep/value",
        root.path().join("dangling"),
    )
    .unwrap();
    let absent = prepare_target(root.path(), Path::new("dangling")).unwrap();
    assert!(matches!(absent, PreparedTarget::Absent(_)));
    absent.write(b"created").unwrap();
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
fn retargeted_route_links_do_not_redirect_execution() {
    let root = tempfile::tempdir().unwrap();
    for directory in ["first", "second"] {
        fs::create_dir(root.path().join(directory)).unwrap();
        fs::write(root.path().join(directory).join("value"), directory).unwrap();
    }
    symlink("first", root.path().join("link")).unwrap();
    let existing = prepare_existing(root.path(), Path::new("link/value")).unwrap();
    let absent = prepare_target(root.path(), Path::new("link/new/value")).unwrap();
    fs::remove_file(root.path().join("link")).unwrap();
    symlink("second", root.path().join("link")).unwrap();

    assert_eq!(read(&existing), "first");
    existing.replace(b"replaced").unwrap();
    absent.write(b"created").unwrap();
    assert_eq!(
        fs::read(root.path().join("first/value")).unwrap(),
        b"replaced"
    );
    assert_eq!(
        fs::read(root.path().join("first/new/value")).unwrap(),
        b"created"
    );
    assert_eq!(
        fs::read(root.path().join("second/value")).unwrap(),
        b"second"
    );
    assert!(!root.path().join("second/new").exists());
}

#[test]
fn symlinks_planted_at_the_prepared_location_are_not_followed() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("target"), "old").unwrap();
    fs::write(root.path().join("elsewhere"), "elsewhere").unwrap();
    let existing = prepare_existing(root.path(), Path::new("target")).unwrap();
    fs::remove_file(root.path().join("target")).unwrap();
    symlink("elsewhere", root.path().join("target")).unwrap();
    assert!(matches!(
        existing.open_file(),
        Err(ToolError::OperationChanged(_))
    ));
    // Replacing renames over the link itself rather than writing through it.
    existing.replace(b"new").unwrap();
    assert_eq!(fs::read(root.path().join("target")).unwrap(), b"new");
    assert!(
        !fs::symlink_metadata(root.path().join("target"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        fs::read(root.path().join("elsewhere")).unwrap(),
        b"elsewhere"
    );

    fs::create_dir(root.path().join("outside")).unwrap();
    let absent = prepare_target(root.path(), Path::new("one/two/file")).unwrap();
    symlink("outside", root.path().join("one")).unwrap();
    assert!(matches!(
        absent.write(b"content"),
        Err(ToolError::OperationChanged(_))
    ));
    assert!(
        fs::read_dir(root.path().join("outside"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn symlink_cycles_and_missing_parent_traversal_fail_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    symlink("two", root.path().join("one")).unwrap();
    symlink("one", root.path().join("two")).unwrap();
    symlink("missing/../value", root.path().join("dangling")).unwrap();
    for path in ["one", "one/child", "dangling"] {
        assert!(prepare_target(root.path(), Path::new(path)).is_err());
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
        assert!(prepare_target(root.path(), Path::new(&alias)).is_err());
    }
    assert_eq!(fs::read(root.path().join("file")).unwrap(), b"value");
    assert!(!root.path().join("missing").exists());
}

#[test]
fn writes_publish_complete_content_and_leave_no_stage_files() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("existing"), "old").unwrap();
    prepare_target(root.path(), Path::new("existing"))
        .unwrap()
        .write(b"new-value")
        .unwrap();
    prepare_target(root.path(), Path::new("nested/deeper/absent"))
        .unwrap()
        .write(b"complete")
        .unwrap();
    assert_eq!(
        fs::read(root.path().join("existing")).unwrap(),
        b"new-value"
    );
    assert_eq!(
        fs::read(root.path().join("nested/deeper/absent")).unwrap(),
        b"complete"
    );
    let directory = prepare_target(root.path(), Path::new("nested")).unwrap();
    assert!(directory.write(b"file over directory").is_err());
    assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".cookie-agent-")
    }));
}

#[test]
fn bindings_distinguish_locations_kinds_and_presence() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a"), "same").unwrap();
    fs::hard_link(root.path().join("a"), root.path().join("b")).unwrap();
    fs::create_dir(root.path().join("d")).unwrap();
    symlink("a", root.path().join("alias")).unwrap();
    let binding = |path: &str| {
        prepare_target(root.path(), Path::new(path))
            .unwrap()
            .binding_bytes()
    };
    assert_ne!(binding("a"), binding("b"));
    assert_ne!(binding("a"), binding("c"));
    assert_ne!(binding("d"), binding("c"));
    assert_eq!(binding("a"), binding("alias"));
    let key = |path: &str| {
        prepare_target(root.path(), Path::new(path))
            .unwrap()
            .serialization_key()
    };
    assert_eq!(key("a"), key("alias"));
    assert_ne!(key("a"), key("b"));
}

#[test]
fn unsupported_paths_and_capability_drops_do_not_leak_descriptors() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("file"), "value").unwrap();
    symlink("file", root.path().join("link")).unwrap();
    symlink("loop", root.path().join("loop")).unwrap();
    let descriptors_for_root = || {
        fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|target| target.starts_with(root.path()))
            .count()
    };
    let before = descriptors_for_root();
    for _ in 0..128 {
        drop(prepare_existing(root.path(), Path::new("file")).unwrap());
        drop(prepare_existing(root.path(), Path::new("link")).unwrap());
        assert!(matches!(
            prepare_existing(root.path(), Path::new("loop")),
            Err(ToolError::UnsupportedSecurity(_))
        ));
    }
    assert_eq!(
        descriptors_for_root(),
        before,
        "prepared descriptors leaked for fixture root"
    );
}

#[test]
fn aggregate_budget_exhaustion_is_reported_without_reserving() {
    let counter = AtomicUsize::new(7);
    assert!(matches!(
        reserve_weight(&counter, 8, 2),
        Err(ToolError::ResourceLimit(_))
    ));
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 7);
    reserve_weight(&counter, 9, 2).unwrap();
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 9);
}
