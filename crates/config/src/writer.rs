//! Writing configuration documents: each one is replaced atomically and
//! owner-only, only while it still holds what was read, and a set of
//! documents is written together or not at all.

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::Path,
};

use toml_edit::{DocumentMut, Item, Table};

use crate::{ConfigError, ConfigPath, ConfigTree, load_from_tree};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitStep {
    TemporarySynced,
    IdentityChecked,
    Renamed,
    ParentSynced,
}

/// Set `[server] token` to `token` while preserving every unrelated item and
/// comment, as the daemon does when it has none. The file is written
/// owner-only.
pub fn write_server_token(path: &Path, token: &str) -> Result<(), ConfigError> {
    if !crate::valid_daemon_token(token) {
        return Err(ConfigError::InvalidRuntime(
            "server.token must be 43 base64url characters",
        ));
    }
    let original = read_original(path)?;
    let text = original
        .as_deref()
        .map(std::str::from_utf8)
        .transpose()
        .map_err(|_| ConfigError::Toml(format!("{}: content is not UTF-8", path.display())))?
        .unwrap_or_default();
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Toml(format!("{}: {error}", path.display())))?;
    if !document.as_table().contains_key("server") {
        document["server"] = Item::Table(Table::new());
    }
    document["server"]["token"] = toml_edit::value(token);
    let candidate = document.to_string();
    let mut tree = ConfigTree::read(path.parent())?;
    tree.put(ConfigPath::config(), candidate.as_bytes().to_vec())?;
    load_from_tree(&tree)?;
    commit(
        path,
        original.as_deref(),
        Some(candidate.as_bytes()),
        |_| {},
    )
}

/// Writes the documents of `candidate` that differ from `original`, which
/// must be what is on disk: each document is checked against its original
/// before it is replaced, and when one fails, the ones already written are
/// put back. A preset directory left without documents is removed. The
/// caller validates `candidate` first.
///
/// # Errors
///
/// [`ConfigError::ChangedOnDisk`] when a document no longer holds what was
/// read, and I/O errors.
pub fn write_tree(original: &ConfigTree, candidate: &ConfigTree) -> Result<(), ConfigError> {
    let root = candidate.root().ok_or(ConfigError::UnsafePath)?;
    let paths = original
        .documents()
        .map(|(path, _)| path)
        .chain(candidate.documents().map(|(path, _)| path))
        .filter(|path| original.get(path) != candidate.get(path))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut written = Vec::new();
    for path in &paths {
        let file = root.join(path.as_str());
        let result = file
            .parent()
            .ok_or(ConfigError::UnsafePath)
            .and_then(create_private_directories)
            .and_then(|()| commit(&file, original.get(path), candidate.get(path), |_| {}));
        if let Err(error) = result {
            for path in written.iter().rev() {
                let file = root.join(ConfigPath::as_str(path));
                let _ = commit(&file, candidate.get(path), original.get(path), |_| {});
            }
            return Err(error);
        }
        written.push(path.clone());
    }
    for path in &paths {
        if let crate::ConfigDocument::Agent {
            preset: Some(preset),
            ..
        } = path.document()
            && candidate.get(path).is_none()
            && !candidate.has_preset(preset)
        {
            match fs::remove_dir(root.join("agents").join(preset)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(ConfigError::Io(error)),
            }
        }
    }
    Ok(())
}

/// Writes `bytes` beside the document `path` of the user root `root`, as
/// `<path>.new`, owner-only and atomically, replacing an earlier `.new`.
/// The configuration never reads it: it is for the user to compare and
/// merge. Returns the file written.
///
/// # Errors
///
/// I/O errors.
pub fn write_beside(
    root: &Path,
    path: &str,
    bytes: &[u8],
) -> Result<std::path::PathBuf, ConfigError> {
    let relative = format!("{path}.new");
    let original = read_original(&root.join(&relative))?;
    write_user_file(root, &relative, original.as_deref(), bytes)
}

/// Writes `bytes` to `relative` below the user root `root`, owner-only and
/// atomically, while it still holds `original` (`None`: does not exist), as
/// configuration documents are written. Returns the file written.
///
/// # Errors
///
/// [`ConfigError::UnsafePath`] for a path that leaves the root,
/// [`ConfigError::ChangedOnDisk`] when the file no longer holds `original`,
/// and I/O errors.
fn write_user_file(
    root: &Path,
    relative: &str,
    original: Option<&[u8]>,
    bytes: &[u8],
) -> Result<std::path::PathBuf, ConfigError> {
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ConfigError::UnsafePath);
    }
    let file = root.join(relative);
    file.parent()
        .ok_or(ConfigError::UnsafePath)
        .and_then(create_private_directories)?;
    commit(&file, original, Some(bytes), |_| {})?;
    Ok(file)
}

fn create_private_directories(path: &Path) -> Result<(), ConfigError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path).map_err(ConfigError::Io)
}

fn read_original(path: &Path) -> Result<Option<Vec<u8>>, ConfigError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ConfigError::Io(error)),
    }
}

/// `path` with every symbolic link it names followed, so a write replaces
/// the file a linked `config.toml` points to, beside it, and keeps the link.
/// A link to a file that does not exist yet resolves to where it will be.
fn resolve_links(path: &Path) -> Result<std::path::PathBuf, ConfigError> {
    /// As many links as Linux follows before `ELOOP`.
    const MOST_LINKS: usize = 40;
    let mut path = path.to_owned();
    for _ in 0..MOST_LINKS {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(&path).map_err(ConfigError::Io)?;
                path = match path.parent() {
                    Some(parent) if target.is_relative() => parent.join(target),
                    _ => target,
                };
            }
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(path),
            Err(error) => return Err(ConfigError::Io(error)),
        }
    }
    Err(ConfigError::Io(std::io::Error::other(format!(
        "{} names more than {MOST_LINKS} symbolic links",
        path.display()
    ))))
}

/// Replaces `path` with `candidate`, or removes it when `candidate` is
/// `None`, while `path` still holds `original` (`None`: does not exist).
/// A linked `path` changes the file the link points to.
fn commit(
    path: &Path,
    original: Option<&[u8]>,
    candidate: Option<&[u8]>,
    mut observe: impl FnMut(CommitStep),
) -> Result<(), ConfigError> {
    let path = &resolve_links(path)?;
    let parent = path.parent().ok_or(ConfigError::UnsafePath)?;
    let Some(candidate) = candidate else {
        if read_original(path)?.as_deref() != original {
            return Err(ConfigError::ChangedOnDisk(path.to_owned()));
        }
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ConfigError::Io(error)),
        }
        return sync_directory(parent).map_err(ConfigError::Io);
    };
    fs::create_dir_all(parent).map_err(ConfigError::Io)?;
    #[cfg(unix)]
    let (temporary, mut temporary_file) = create_unix_temporary(parent, path)?;
    #[cfg(windows)]
    let (temporary, mut temporary_file) = create_windows_temporary(parent, path)?;
    #[cfg(unix)]
    {
        // The umask may have narrowed the mode further; force it.
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = temporary_file.set_permissions(fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&temporary);
            return Err(ConfigError::Io(error));
        }
    }
    if let Err(error) = temporary_file
        .write_all(candidate)
        .and_then(|()| temporary_file.sync_all())
    {
        let _ = fs::remove_file(&temporary);
        return Err(ConfigError::Io(error));
    }
    drop(temporary_file);
    observe(CommitStep::TemporarySynced);

    let unchanged = match read_original(path) {
        Ok(current) => current.as_deref() == original,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    };
    if !unchanged {
        let _ = fs::remove_file(&temporary);
        return Err(ConfigError::ChangedOnDisk(path.to_owned()));
    }
    observe(CommitStep::IdentityChecked);
    if let Err(error) = replace_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(ConfigError::Io(error));
    }
    observe(CommitStep::Renamed);
    sync_directory(parent).map_err(ConfigError::Io)?;
    observe(CommitStep::ParentSynced);
    Ok(())
}

/// A new owner-only temporary file beside `destination`, under a name no
/// other write uses, in this process or another.
#[cfg(unix)]
fn create_unix_temporary(
    parent: &Path,
    destination: &Path,
) -> Result<(std::path::PathBuf, fs::File), ConfigError> {
    use std::{
        os::unix::fs::OpenOptionsExt as _,
        sync::atomic::{AtomicU64, Ordering},
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let base = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    for _ in 0..128 {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{base}.{}.{sequence}.tmp", std::process::id()));
        // `create_new` never follows or truncates a stale file or link; a
        // collision moves on to the next name.
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(ConfigError::Io(error)),
        }
    }
    Err(ConfigError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique private configuration temporary file",
    )))
}

#[cfg(windows)]
fn create_windows_temporary(
    parent: &Path,
    destination: &Path,
) -> Result<(std::path::PathBuf, fs::File), ConfigError> {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let base = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..128 {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{base}.{}.{epoch:x}.{sequence:x}.tmp",
            std::process::id()
        ));
        match cookie_agent_models::secure_store::create_windows_private_file(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // CREATE_NEW never follows or truncates a stale file/reparse
                // point; a collision advances to a fresh unique name.
            }
            Err(error) => return Err(ConfigError::Io(error)),
        }
    }
    Err(ConfigError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique private configuration temporary file",
    )))
}

#[cfg(unix)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    // One shared primitive for every durable Windows replacement: a superseding
    // POSIX rename, so a concurrent reader opening the destination by name never
    // finds it absent mid-replacement, with the classic write-through
    // `MoveFileExW` as the fallback where that rename is unavailable.
    cookie_agent_models::secure_store::replace_windows_path(source, destination)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .and_then(|directory| match directory.sync_all() {
            Ok(()) => Ok(()),
            Err(error) if matches!(error.raw_os_error(), Some(1 | 5 | 6 | 50)) => {
                // `replace_file` already committed the replacement, and Windows
                // does not consistently support flushing a directory handle
                // across filesystems and host policies. This second flush keeps
                // the `ParentSynced` commit step observable on hosts that do.
                Ok(())
            }
            Err(error) => Err(error),
        })
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, fs};

    use super::{CommitStep, commit};
    use crate::ConfigError;

    const CANDIDATE: &[u8] = b"[mcp.servers.demo]\ncommand = \"new\"\n";

    #[test]
    fn durable_replace_orders_sync_identity_rename_and_directory_sync() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("config.toml");
        fs::write(&path, "# retained\n").expect("initial config");
        let steps = RefCell::new(Vec::new());

        let result = commit(&path, Some(b"# retained\n"), Some(CANDIDATE), |step| {
            steps.borrow_mut().push(step);
            #[cfg(windows)]
            if step == CommitStep::TemporarySynced {
                let temporary = fs::read_dir(directory.path())
                    .expect("list temporary directory")
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| {
                        path.file_name()
                            .is_some_and(|name| name.to_string_lossy().ends_with(".tmp"))
                    })
                    .expect("unique temporary config");
                cookie_agent_models::secure_store::verify_windows_private_creation(&temporary)
                    .expect("temporary config ACL");
            }
        });
        if let Err(error) = result {
            panic!(
                "durable write failed: {error}; source: {:?}",
                std::error::Error::source(&error)
            );
        }

        assert_eq!(
            *steps.borrow(),
            [
                CommitStep::TemporarySynced,
                CommitStep::IdentityChecked,
                CommitStep::Renamed,
                CommitStep::ParentSynced,
            ]
        );
        assert_eq!(fs::read(&path).expect("replaced configuration"), CANDIDATE);
    }

    #[test]
    fn external_edit_before_commit_is_rejected_without_overwrite() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("config.toml");
        fs::write(&path, "# original\n").expect("initial config");

        let error = commit(&path, Some(b"# original\n"), Some(CANDIDATE), |step| {
            if step == CommitStep::TemporarySynced {
                fs::write(&path, "# external edit\n").expect("external edit");
            }
        })
        .expect_err("concurrent edit must conflict");

        assert!(matches!(error, ConfigError::ChangedOnDisk(changed) if changed == path));
        assert_eq!(
            fs::read_to_string(&path).expect("read preserved external edit"),
            "# external edit\n"
        );
    }

    /// A linked config.toml, as a dotfiles checkout keeps it, stays a link:
    /// the write replaces the file it points to, and the temporary file is
    /// made beside that file.
    #[cfg(unix)]
    #[test]
    fn a_linked_file_is_written_through_its_link() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let dotfiles = directory.path().join("dotfiles");
        let config = directory.path().join("config");
        fs::create_dir(&dotfiles).expect("dotfiles");
        fs::create_dir(&config).expect("config directory");
        let target = dotfiles.join("config.toml");
        fs::write(&target, "# original\n").expect("initial config");
        let link = config.join("config.toml");
        std::os::unix::fs::symlink("../dotfiles/config.toml", &link).expect("link");

        let temporaries = RefCell::new(Vec::new());
        commit(&link, Some(b"# original\n"), Some(CANDIDATE), |step| {
            if step == CommitStep::TemporarySynced {
                temporaries.borrow_mut().extend(
                    fs::read_dir(&dotfiles)
                        .expect("list dotfiles")
                        .filter_map(Result::ok)
                        .map(|entry| entry.file_name().to_string_lossy().into_owned())
                        .filter(|name| name.ends_with(".tmp")),
                );
            }
        })
        .expect("write through the link");

        assert!(
            fs::symlink_metadata(&link)
                .expect("link metadata")
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).expect("target"), CANDIDATE);
        assert_eq!(temporaries.borrow().len(), 1, "made beside the target");
        assert_eq!(fs::read_dir(&config).expect("list config").count(), 1);

        // A link to a file that does not exist yet creates it there.
        let dangling = config.join("other.toml");
        std::os::unix::fs::symlink(dotfiles.join("other.toml"), &dangling).expect("link");
        commit(&dangling, None, Some(CANDIDATE), |_| {}).expect("create through the link");
        assert_eq!(
            fs::read(dotfiles.join("other.toml")).expect("created"),
            CANDIDATE
        );
        assert!(
            fs::symlink_metadata(&dangling)
                .expect("link metadata")
                .file_type()
                .is_symlink()
        );
    }

    /// Two writes of one file at once in one process never share a
    /// temporary file: one that fails cleaning up its own leaves the other's
    /// whole.
    #[test]
    fn overlapping_commits_use_their_own_temporary_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("config.toml");
        fs::write(&path, "# original\n").expect("initial config");

        commit(&path, Some(b"# original\n"), Some(CANDIDATE), |step| {
            if step == CommitStep::TemporarySynced {
                // A second write, from what it read earlier, fails and
                // removes its temporary file.
                let stale = commit(&path, Some(b"# stale\n"), Some(b"# other\n"), |_| {});
                assert!(
                    matches!(stale, Err(ConfigError::ChangedOnDisk(_))),
                    "{stale:?}"
                );
            }
        })
        .expect("the first write is untouched");

        assert_eq!(fs::read(&path).expect("config"), CANDIDATE);
        assert_eq!(fs::read_dir(directory.path()).expect("list").count(), 1);
    }

    #[test]
    fn a_removal_checks_the_original_too() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("worker.md");
        fs::write(&path, "edited").expect("document");
        assert!(matches!(
            commit(&path, Some(b"original"), None, |_| {}),
            Err(ConfigError::ChangedOnDisk(_))
        ));
        commit(&path, Some(b"edited"), None, |_| {}).expect("remove");
        assert!(!path.exists());
    }
}
