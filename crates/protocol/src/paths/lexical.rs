//! Lexical path normalization, shared by the coding runtime, which prepares
//! file operations, and the daemon's permission rules, which match their
//! labels, so both read one path one way.
//!
//! [`normalize_absolute`] is the host's: it works on a [`Path`] as the
//! platform parses it, for opening files. [`path_label`] is the spelling
//! rules match and results show, read alike on every platform, since the
//! daemon decides for runtimes on other machines: `/` separators, no Windows
//! verbatim prefix, and `.` and `..` collapsed. For an absolute path that
//! stays below its root, the label of the one is the label of the other.

use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// Why [`normalize_absolute`] refused a path.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LexicalPathError {
    /// The path is not absolute on this platform.
    #[error("filesystem path is not absolute")]
    NotAbsolute,
    /// A `..` would climb above the path's root.
    #[error("filesystem path traverses above its root")]
    AboveRoot,
}

/// `path`, absolute on this platform, with every `.` removed and every `..`
/// taking away the name before it, without consulting the file system: a
/// link is not followed, so `link/..` is the directory holding `link`. The
/// prefix (a Windows drive or share) and the root are kept as they are.
///
/// # Errors
///
/// [`LexicalPathError::NotAbsolute`] for a relative path, and
/// [`LexicalPathError::AboveRoot`] when a `..` would climb above the root.
pub fn normalize_absolute(path: &Path) -> Result<PathBuf, LexicalPathError> {
    if !path.is_absolute() {
        return Err(LexicalPathError::NotAbsolute);
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::Normal(name) => normalized.push(name),
            Component::ParentDir => {
                if normalized.file_name().is_none() || !normalized.pop() {
                    return Err(LexicalPathError::AboveRoot);
                }
            }
        }
    }
    Ok(normalized)
}

/// `path` as permission rules match it and results show it, on any
/// platform: every `\` spelled `/`; the Windows verbatim prefix left out
/// (`\\?\C:\x` reads `C:/x`, `\\?\UNC\server\share\x` reads
/// `//server/share/x`); and every `.` removed and every `..` taking away the
/// name before it, lexically, a `..` at the root staying there as the
/// platforms resolve it. A path that starts with two separators names a
/// Windows share, the root of its paths: `..` never climbs out of
/// `//server/share`. A relative path stays relative, keeping the `..` that
/// leave it (`a/../../b` reads `../b`), and reads `.` when nothing is left; a
/// trailing `/` is dropped. A label is its own label.
#[must_use]
pub fn path_label(path: &str) -> String {
    let spelled = path.replace('\\', "/");
    let spelled = without_verbatim_prefix(&spelled);
    let (root, rest) = split_root(&spelled);
    let mut names: Vec<&str> = Vec::new();
    for name in rest.split('/') {
        match name {
            "" | "." => {}
            ".." => match names.last() {
                Some(&last) if last != ".." => {
                    names.pop();
                }
                // `..` at a root is the root.
                _ if !root.is_empty() => {}
                _ => names.push(".."),
            },
            name => names.push(name),
        }
    }
    let joined = names.join("/");
    match (root.is_empty(), joined.is_empty()) {
        (true, true) => ".".to_owned(),
        (true, false) => joined,
        (false, true) => root.to_owned(),
        (false, false) if root.ends_with('/') || root.ends_with(':') => format!("{root}{joined}"),
        (false, false) => format!("{root}/{joined}"),
    }
}

/// Whether the label `label` ([`path_label`]) names a path from a root on
/// some platform, so that it is never read against a workspace: `/…`
/// (a POSIX root, or a Windows share `//server/share`), or a Windows drive
/// `X:…`.
#[must_use]
pub fn label_is_absolute(label: &str) -> bool {
    label.starts_with('/') || drive_of(label).is_some()
}

/// `spelled` (with `/` separators) without a Windows verbatim prefix before
/// a drive or a share: `//?/C:/x` is `C:/x`, `//?/UNC/server/share` is
/// `//server/share`. Any other path is kept as it is.
fn without_verbatim_prefix(spelled: &str) -> std::borrow::Cow<'_, str> {
    if let Some(share) = spelled.strip_prefix("//?/UNC/") {
        return format!("//{share}").into();
    }
    match spelled.strip_prefix("//?/") {
        Some(rest) if drive_of(rest).is_some() => rest.into(),
        _ => spelled.into(),
    }
}

/// The drive `path` starts with (`C:`), if any.
fn drive_of(path: &str) -> Option<&str> {
    let bytes = path.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':').then(|| &path[..2])
}

/// `spelled` (with `/` separators) split into its root and the names after
/// it. The root is `/`; `//server/share`, however many names follow; `X:/`;
/// or `X:` alone, a drive's working directory. A relative path has no root.
fn split_root(spelled: &str) -> (&str, &str) {
    if let Some(drive) = drive_of(spelled) {
        return if spelled[2..].starts_with('/') {
            (&spelled[..3], &spelled[3..])
        } else {
            (drive, &spelled[2..])
        };
    }
    if let Some(share) = spelled.strip_prefix("//")
        && !share.is_empty()
        && !share.starts_with('/')
    {
        let end = share
            .match_indices('/')
            .nth(1)
            .map_or(spelled.len(), |(index, _)| index + 2);
        return (&spelled[..end], &spelled[end..]);
    }
    if spelled.starts_with('/') {
        return ("/", spelled);
    }
    ("", spelled)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{LexicalPathError, label_is_absolute, normalize_absolute, path_label};

    #[test]
    fn labels_collapse_dots_lexically() {
        for (path, label) in [
            ("/ws/src/lib.rs", "/ws/src/lib.rs"),
            ("/ws/./src/../.env", "/ws/.env"),
            ("/ws/../../etc/cron.d/x", "/etc/cron.d/x"),
            ("/..", "/"),
            ("/ws/", "/ws"),
            ("/ws//src", "/ws/src"),
            ("src/../.env", ".env"),
            ("a/../../b", "../b"),
            ("./a/..", "."),
            ("", "."),
            ("~/notes/../secret", "~/secret"),
        ] {
            assert_eq!(path_label(path), label, "{path}");
        }
    }

    #[test]
    fn windows_spellings_read_alike_on_every_platform() {
        for (path, label) in [
            (r"C:\ws\..\..\Windows\x", "C:/Windows/x"),
            (r"\\?\C:\ws\file.txt", "C:/ws/file.txt"),
            (r"\\?\UNC\server\share\ws\..\..\x", "//server/share/x"),
            (r"\\server\share\..", "//server/share"),
            (r"\\server", "//server"),
            ("C:", "C:"),
            ("C:a/../../b", "C:b"),
            // Not a drive or a share after the prefix: kept.
            ("//?/etc/passwd", "//?/etc/passwd"),
        ] {
            assert_eq!(path_label(path), label, "{path}");
        }
    }

    #[test]
    fn a_label_is_its_own_label() {
        for path in [
            "/ws/../x",
            r"\\server\share\ws\..\x",
            r"\\?\C:\ws",
            "C:a/..",
            "../a/./b",
            "/",
            "///x",
        ] {
            let label = path_label(path);
            assert_eq!(path_label(&label), label, "{path}");
        }
    }

    #[test]
    fn rooted_labels_are_absolute_everywhere() {
        for label in ["/", "/etc/x", "//server/share/x", "C:/x", "c:", "C:x"] {
            assert!(label_is_absolute(label), "{label}");
        }
        for label in [".", "src/lib.rs", "../x", "~/x", ":x"] {
            assert!(!label_is_absolute(label), "{label}");
        }
    }

    #[test]
    fn absolute_paths_normalize_without_leaving_their_root() {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        let path = |rest: &str| Path::new(root).join(rest);
        assert_eq!(
            normalize_absolute(&path("ws/./src/../../etc/x")),
            Ok(path("etc/x"))
        );
        assert_eq!(normalize_absolute(&path("ws/..")), Ok(path("")));
        assert_eq!(
            normalize_absolute(&path("ws/../..")),
            Err(LexicalPathError::AboveRoot)
        );
        assert_eq!(
            normalize_absolute(Path::new("ws/x")),
            Err(LexicalPathError::NotAbsolute)
        );
    }

    #[test]
    fn the_host_normalization_and_the_label_agree() {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        for rest in ["ws/./a/../b", "ws/x/../../y", "a/b/c/../../d"] {
            let path = Path::new(root).join(rest);
            let host = normalize_absolute(&path).expect("normalizes");
            assert_eq!(
                path_label(&host.to_string_lossy()),
                path_label(&path.to_string_lossy()),
                "{rest}"
            );
        }
    }
}
