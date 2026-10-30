use std::{fs, path::Path};

use cookie_agent_protocol::runtime_protocol::{FsDirection, FsKind};
use sha2::{Digest as _, Sha256};

use super::{Files, FsError, PipeReader, hex, stays_inside, unpack};

fn root() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = cookie_agent_protocol::paths::canonicalize(directory.path()).expect("canonical");
    (directory, path)
}

/// Copies `from` to `to` (both under `workspace`) on one runtime.
async fn copy(
    files: &Files,
    workspace: &Path,
    from: &str,
    to: &str,
    overwrite: bool,
) -> Result<cookie_agent_protocol::runtime_protocol::FsWritten, FsError> {
    let source = files
        .prepare(workspace.to_owned(), from.into(), FsDirection::Read)
        .await?;
    let destination = files
        .prepare(workspace.to_owned(), to.into(), FsDirection::Write)
        .await?;
    files.copy(source.route, destination.route, overwrite).await
}

/// Relays `from` to `to` through the requests, as the daemon does between
/// two runtimes, in chunks of `chunk` bytes.
async fn relay(
    source: &Files,
    destination: &Files,
    workspace: &Path,
    from: &str,
    to: &str,
    chunk: u64,
) -> Result<cookie_agent_protocol::runtime_protocol::FsWritten, FsError> {
    let from = source
        .prepare(workspace.to_owned(), from.into(), FsDirection::Read)
        .await?;
    let to = destination
        .prepare(workspace.to_owned(), to.into(), FsDirection::Write)
        .await?;
    let reader = source.read_open(from.route).await?;
    let writer = destination.write_open(to.route, reader.kind, false).await?;
    let mut offset = 0;
    let summary = loop {
        let piece = source.read_chunk(reader.handle, chunk).await?;
        let length = base64_length(&piece.data);
        destination.write_chunk(writer, offset, &piece.data).await?;
        offset += length;
        if let Some(summary) = piece.summary {
            break summary;
        }
    };
    source.close(reader.handle);
    destination.write_commit(writer, &summary.sha256).await
}

fn base64_length(data: &str) -> u64 {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .expect("base64")
        .len() as u64
}

fn partials(folder: &Path) -> Vec<String> {
    fs::read_dir(folder)
        .expect("folder")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.contains(".cookie-partial-"))
        .collect()
}

#[tokio::test]
async fn a_file_is_copied_with_its_digest_and_never_replaced_without_overwrite() {
    let (_directory, workspace) = root();
    fs::write(workspace.join("report.txt"), b"first").unwrap();
    let files = Files::new(None);
    let written = copy(&files, &workspace, "report.txt", "out/copy.txt", false)
        .await
        .expect("copy");
    assert_eq!(fs::read(workspace.join("out/copy.txt")).unwrap(), b"first");
    assert_eq!(written.bytes, 5);
    assert_eq!(written.files, 1);
    assert_eq!(written.sha256, hex(Sha256::digest(b"first")));
    assert!(
        Path::new(&written.path).ends_with("out/copy.txt"),
        "{}",
        written.path
    );

    fs::write(workspace.join("report.txt"), b"second").unwrap();
    let error = copy(&files, &workspace, "report.txt", "out/copy.txt", false)
        .await
        .expect_err("exists");
    assert!(
        matches!(&error, FsError::Failed(message) if message.contains("exists already")),
        "{error:?}"
    );
    copy(&files, &workspace, "report.txt", "out/copy.txt", true)
        .await
        .expect("overwrite");
    assert_eq!(fs::read(workspace.join("out/copy.txt")).unwrap(), b"second");
    assert!(partials(&workspace.join("out")).is_empty());
}

#[tokio::test]
async fn chunks_relay_a_file_in_order_and_a_missing_source_fails_at_prepare() {
    let (_directory, workspace) = root();
    let content = (0..10_000u32)
        .flat_map(u32::to_le_bytes)
        .collect::<Vec<_>>();
    fs::write(workspace.join("data.bin"), &content).unwrap();
    let (source, destination) = (Files::new(None), Files::new(None));
    let written = relay(
        &source,
        &destination,
        &workspace,
        "data.bin",
        "copy.bin",
        4096,
    )
    .await
    .expect("relay");
    assert_eq!(fs::read(workspace.join("copy.bin")).unwrap(), content);
    assert_eq!(written.bytes, content.len() as u64);

    let error = source
        .prepare(workspace.clone(), "missing.bin".into(), FsDirection::Read)
        .await
        .expect_err("missing");
    assert!(matches!(error, FsError::Failed(_)), "{error:?}");
}

#[tokio::test]
async fn a_chunk_out_of_order_or_a_digest_that_differs_fails_and_leaves_nothing() {
    let (_directory, workspace) = root();
    let files = Files::new(None);
    let route = files
        .prepare(workspace.clone(), "out.txt".into(), FsDirection::Write)
        .await
        .unwrap();
    let handle = files
        .write_open(route.route, FsKind::File, false)
        .await
        .unwrap();
    files.write_chunk(handle, 0, "aGVsbG8=").await.unwrap();
    let error = files
        .write_chunk(handle, 0, "aGVsbG8=")
        .await
        .expect_err("order");
    assert!(matches!(error, FsError::Invalid(_)), "{error:?}");
    let error = files
        .write_commit(handle, &hex(Sha256::digest(b"other")))
        .await
        .expect_err("mismatch");
    assert!(
        matches!(&error, FsError::Failed(message) if message.contains("do not match")),
        "{error:?}"
    );
    assert!(!workspace.join("out.txt").exists());
    assert!(
        partials(&workspace).is_empty(),
        "{:?}",
        partials(&workspace)
    );
    assert!(matches!(
        files.write_chunk(handle, 5, "aGVsbG8=").await,
        Err(FsError::UnknownHandle(_))
    ));
}

#[tokio::test]
async fn closing_an_uncommitted_write_removes_its_partial_copy() {
    let (_directory, workspace) = root();
    let journal = workspace.join("journal");
    let files = Files::new(Some(journal.clone()));
    for kind in [FsKind::File, FsKind::Folder] {
        let route = files
            .prepare(workspace.clone(), "target".into(), FsDirection::Write)
            .await
            .unwrap();
        let handle = files.write_open(route.route, kind, false).await.unwrap();
        assert_eq!(partials(&workspace).len(), 1, "{kind:?}");
        assert_eq!(fs::read_dir(&journal).unwrap().count(), 1, "recorded");
        files.close(handle);
        // A folder's unpacking ends at the closed stream.
        tokio::task::yield_now().await;
        assert!(partials(&workspace).is_empty(), "{kind:?}");
        assert_eq!(fs::read_dir(&journal).unwrap().count(), 0, "unrecorded");
        assert!(!workspace.join("target").exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_folder_moves_with_its_links_and_modes_and_without_devices() {
    use std::os::unix::fs::PermissionsExt as _;

    let (_directory, workspace) = root();
    let source = workspace.join("project");
    fs::create_dir_all(source.join("src/deep")).unwrap();
    fs::write(source.join("README.md"), b"hello").unwrap();
    fs::write(source.join("src/deep/main.rs"), b"fn main() {}").unwrap();
    fs::write(source.join("run.sh"), b"#!/bin/sh").unwrap();
    fs::set_permissions(source.join("run.sh"), fs::Permissions::from_mode(0o750)).unwrap();
    std::os::unix::fs::symlink("src/deep/main.rs", source.join("main")).unwrap();
    std::os::unix::fs::symlink("../README.md", source.join("src/readme")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/env", source.join("python")).unwrap();
    let pipe =
        std::ffi::CString::new(source.join("pipe").into_os_string().into_encoded_bytes()).unwrap();
    // SAFETY: `pipe` is a valid, NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);

    let (from, to) = (Files::new(None), Files::new(None));
    let written = relay(&from, &to, &workspace, "project", "elsewhere/project", 1000)
        .await
        .expect("relay");
    let copied = workspace.join("elsewhere/project");
    assert_eq!(
        fs::read(copied.join("src/deep/main.rs")).unwrap(),
        b"fn main() {}"
    );
    assert_eq!(
        fs::read_link(copied.join("main")).unwrap(),
        Path::new("src/deep/main.rs")
    );
    assert_eq!(fs::read(copied.join("src/readme")).unwrap(), b"hello");
    assert_eq!(
        fs::metadata(copied.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
    assert!(!copied.join("pipe").exists(), "a FIFO is left out");
    assert!(
        fs::symlink_metadata(copied.join("python")).is_err(),
        "a link that leads outside is left out"
    );
    assert_eq!(written.files, 3);
    assert_eq!(written.bytes, 5 + 12 + 9);
    assert_eq!(
        written.skipped, 1,
        "the outside link, counted where it was refused"
    );
    assert!(partials(&workspace.join("elsewhere")).is_empty());

    // Copying again merges with overwrite and is refused without it.
    fs::write(copied.join("mine.txt"), b"kept").unwrap();
    fs::write(source.join("README.md"), b"changed").unwrap();
    assert!(
        copy(&from, &workspace, "project", "elsewhere/project", false)
            .await
            .is_err()
    );
    let written = copy(&from, &workspace, "project", "elsewhere/project", true)
        .await
        .expect("merge");
    assert_eq!(written.skipped, 2, "the FIFO, and the outside link");
    assert_eq!(fs::read(copied.join("README.md")).unwrap(), b"changed");
    assert_eq!(fs::read(copied.join("mine.txt")).unwrap(), b"kept");
}

#[tokio::test]
async fn a_folder_cannot_be_copied_into_itself() {
    let (_directory, workspace) = root();
    fs::create_dir_all(workspace.join("folder")).unwrap();
    let error = copy(
        &Files::new(None),
        &workspace,
        "folder",
        "folder/inner",
        false,
    )
    .await
    .expect_err("into itself");
    assert!(matches!(error, FsError::Invalid(_)), "{error:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn the_label_is_lexical_and_the_route_is_resolved_once() {
    let (_directory, workspace) = root();
    fs::create_dir_all(workspace.join("real")).unwrap();
    fs::create_dir_all(workspace.join("other")).unwrap();
    fs::write(workspace.join("real/file.txt"), b"real").unwrap();
    fs::write(workspace.join("other/file.txt"), b"other").unwrap();
    std::os::unix::fs::symlink("real", workspace.join("link")).unwrap();
    let files = Files::new(None);
    let prepared = files
        .prepare(workspace.clone(), "link/file.txt".into(), FsDirection::Read)
        .await
        .unwrap();
    assert_eq!(
        prepared.label, "link/file.txt",
        "never resolved through the link"
    );
    assert_eq!(prepared.kind, Some(FsKind::File));
    assert_eq!(prepared.size, Some(4));
    // Retargeted after prepare: the transfer still reads where it led.
    fs::remove_file(workspace.join("link")).unwrap();
    std::os::unix::fs::symlink("other", workspace.join("link")).unwrap();
    let destination = files
        .prepare(workspace.clone(), "copy.txt".into(), FsDirection::Write)
        .await
        .unwrap();
    files
        .copy(prepared.route, destination.route, false)
        .await
        .unwrap();
    assert_eq!(fs::read(workspace.join("copy.txt")).unwrap(), b"real");
}

/// A tar stream of `entries`, each a name, an entry type, and a link
/// target or content.
fn stream(entries: &[(&str, tar::EntryType, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, kind, value) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*kind);
        header.set_mode(0o644);
        if *kind == tar::EntryType::Symlink {
            header.set_size(0);
            // `append_link` refuses nothing, unlike a path setter.
            builder.append_link(&mut header, name, value).unwrap();
        } else {
            header.set_size(value.len() as u64);
            let bytes = header.as_old_mut();
            bytes.name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            builder.append(&header, value.as_bytes()).unwrap();
        }
    }
    builder.into_inner().unwrap()
}

fn unpacked(bytes: Vec<u8>) -> (tempfile::TempDir, std::io::Result<super::Counted>) {
    let directory = tempfile::tempdir().unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    sender.try_send(bytes).unwrap();
    drop(sender);
    let result = unpack(PipeReader::new(receiver), directory.path());
    (directory, result)
}

#[test]
fn unpacking_refuses_names_that_climb_and_entries_below_links() {
    let (_directory, result) = unpacked(stream(&[("../escape.txt", tar::EntryType::Regular, "x")]));
    assert!(result.is_err(), "a name with `..`");
    let (_directory, result) = unpacked(stream(&[("/etc/escape", tar::EntryType::Regular, "x")]));
    assert!(result.is_err(), "an absolute name");
    let (directory, result) = unpacked(stream(&[
        ("inner", tar::EntryType::Symlink, "."),
        ("inner/file.txt", tar::EntryType::Regular, "x"),
    ]));
    assert!(result.is_err(), "an entry below a link the stream made");
    assert!(!directory.path().join("file.txt").exists());
    let (_directory, result) = unpacked(stream(&[("hard", tar::EntryType::Link, "other")]));
    assert!(result.is_err(), "a hard link");
}

#[test]
fn a_link_stays_inside_when_it_climbs_only_before_its_first_name() {
    assert!(stays_inside(Path::new("a/link"), Path::new("../b/c")));
    assert!(stays_inside(Path::new("link"), Path::new("./b")));
    assert!(!stays_inside(Path::new("link"), Path::new("../outside")));
    assert!(!stays_inside(Path::new("a/link"), Path::new("b/../../..")));
    assert!(!stays_inside(Path::new("a/link"), Path::new("b/../c")));
    assert!(!stays_inside(Path::new("link"), Path::new("/etc/passwd")));
}
