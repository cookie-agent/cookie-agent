//! Lexical path normalization, shared by the coding runtime, which prepares
//! file operations, and the daemon's permission rules, which match their
//! labels, so both read one path one way.
//!
//! [`normalize_absolute`] is the host's: it works on a [`Path`] as the
//! platform parses it, for opening files. [`path_label`] is the spelling
//! rules match and results show, as the platform of the runtime the path
//! lives on reads it ([`PathStyle`]), since the daemon decides for runtimes
//! on other machines: `/` separators, no Windows verbatim prefix, and `.` and
//! `..` collapsed. For an absolute path that stays below its root, the label
//! of the one is the label of the other.

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

/// How a platform reads a path: which characters separate names and what
/// a root is.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PathStyle {
    /// Linux, macOS, and the other Unix systems: `/` alone separates names,
    /// `\` is a character of a name, and every path that starts with `/`
    /// starts at the one root (`//etc` is `/etc`).
    Posix,
    /// Windows: `\` and `/` both separate names, and a path starts at a
    /// drive (`C:\`) or a share (`\\server\share`).
    Windows,
}

impl PathStyle {
    /// The style of the platform this process runs on.
    #[must_use]
    pub const fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }

    /// The style of `platform`, an operating system as
    /// `std::env::consts::OS` names it: `windows` reads Windows paths, every
    /// other system POSIX ones.
    #[must_use]
    pub fn of_platform(platform: &str) -> Self {
        if platform.eq_ignore_ascii_case("windows") {
            Self::Windows
        } else {
            Self::Posix
        }
    }

    /// Whether `path` reads differently in the two styles: it holds a `\`,
    /// which is a separator on Windows and a character of a name elsewhere,
    /// or starts with two separators, a share on Windows and the root
    /// elsewhere.
    #[must_use]
    pub fn ambiguous(path: &str) -> bool {
        path.contains('\\') || path.starts_with("//")
    }
}

/// `path` as permission rules match it and results show it, read in
/// `style`: every separator spelled `/`; and every `.` removed and every `..`
/// taking away the name before it, lexically, a `..` at the root staying
/// there as the platforms resolve it. A relative path stays relative,
/// keeping the `..` that leave it (`a/../../b` reads `../b`), and reads `.`
/// when nothing is left; a trailing `/` is dropped. A label is its own label
/// in the same style.
///
/// In [`PathStyle::Posix`] the root is `/`, however many `/` start the path,
/// and a `\` is part of a name. In [`PathStyle::Windows`] `\` separates too;
/// the verbatim prefix is left out (`\\?\C:\x` reads `C:/x`,
/// `\\?\UNC\server\share\x` reads `//server/share/x`); a path that starts
/// with two separators names a share, which `..` never climbs out of; and a
/// drive (`C:/`, or `C:` alone, its working directory) is a root.
#[must_use]
pub fn path_label(path: &str, style: PathStyle) -> String {
    let (root, rest): (std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>) = match style {
        PathStyle::Posix => match path.strip_prefix('/') {
            Some(rest) => ("/".into(), rest.into()),
            None => ("".into(), path.into()),
        },
        PathStyle::Windows => {
            let spelled = path.replace('\\', "/");
            let spelled = without_verbatim_prefix(&spelled);
            let (root, rest) = split_root(&spelled);
            (root.to_owned().into(), rest.to_owned().into())
        }
    };
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
        (false, true) => root.into_owned(),
        (false, false) if root.ends_with('/') || root.ends_with(':') => format!("{root}{joined}"),
        (false, false) => format!("{root}/{joined}"),
    }
}

/// Whether the label `label` ([`path_label`] in `style`) names a path from a
/// root, so that it is never read against a workspace: `/…` in either
/// style (a POSIX root, or a Windows share `//server/share`), and on Windows
/// a drive `X:…` too.
#[must_use]
pub fn label_is_absolute(label: &str, style: PathStyle) -> bool {
    label.starts_with('/') || (style == PathStyle::Windows && drive_of(label).is_some())
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

    use super::{LexicalPathError, PathStyle, label_is_absolute, normalize_absolute, path_label};

    const BOTH: [PathStyle; 2] = [PathStyle::Posix, PathStyle::Windows];

    #[test]
    fn labels_collapse_dots_lexically() {
        for style in BOTH {
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
                assert_eq!(path_label(path, style), label, "{path} {style:?}");
            }
        }
    }

    #[test]
    fn a_posix_path_has_one_root_and_names_may_hold_a_backslash() {
        for (path, label) in [
            ("//etc/passwd", "/etc/passwd"),
            ("///etc//passwd", "/etc/passwd"),
            ("//ws/../etc", "/etc"),
            ("//server/share/..", "/server"),
            // A name, not separators: the file stays under /etc.
            (r"/etc/x\..\..\ws\y", r"/etc/x\..\..\ws\y"),
            (r"\\server\share", r"\\server\share"),
            ("C:/ws/../x", "C:/x"),
            (r"\\?\C:\x", r"\\?\C:\x"),
        ] {
            assert_eq!(path_label(path, PathStyle::Posix), label, "{path}");
        }
    }

    #[test]
    fn a_windows_path_starts_at_a_drive_or_a_share() {
        for (path, label) in [
            (r"C:\ws\..\..\Windows\x", "C:/Windows/x"),
            (r"\\?\C:\ws\file.txt", "C:/ws/file.txt"),
            (r"\\?\UNC\server\share\ws\..\..\x", "//server/share/x"),
            (r"\\server\share\..", "//server/share"),
            ("//server/share/../x", "//server/share/x"),
            (r"\\server", "//server"),
            ("C:", "C:"),
            ("C:a/../../b", "C:b"),
            (r"/etc/x\..\..\ws\y", "/ws/y"),
            // Not a drive or a share after the prefix: kept.
            ("//?/etc/passwd", "//?/etc/passwd"),
        ] {
            assert_eq!(path_label(path, PathStyle::Windows), label, "{path}");
        }
    }

    #[test]
    fn a_label_is_its_own_label() {
        for style in BOTH {
            for path in [
                "/ws/../x",
                r"\\server\share\ws\..\x",
                r"\\?\C:\ws",
                "C:a/..",
                "../a/./b",
                "/",
                "///x",
                "//server/share/x",
            ] {
                let label = path_label(path, style);
                assert_eq!(path_label(&label, style), label, "{path} {style:?}");
            }
        }
    }

    #[test]
    fn only_a_windows_label_may_start_at_a_drive() {
        for label in ["/", "/etc/x", "//server/share/x"] {
            assert!(label_is_absolute(label, PathStyle::Posix), "{label}");
            assert!(label_is_absolute(label, PathStyle::Windows), "{label}");
        }
        for label in ["C:/x", "c:", "C:x"] {
            assert!(!label_is_absolute(label, PathStyle::Posix), "{label}");
            assert!(label_is_absolute(label, PathStyle::Windows), "{label}");
        }
        for label in [".", "src/lib.rs", "../x", "~/x", ":x"] {
            for style in BOTH {
                assert!(!label_is_absolute(label, style), "{label}");
            }
        }
    }

    #[test]
    fn platforms_name_their_style() {
        assert_eq!(PathStyle::of_platform("windows"), PathStyle::Windows);
        for platform in ["linux", "macos", "freebsd"] {
            assert_eq!(PathStyle::of_platform(platform), PathStyle::Posix);
        }
        assert!(PathStyle::ambiguous("//etc/passwd"));
        assert!(PathStyle::ambiguous(r"C:\x"));
        assert!(!PathStyle::ambiguous("/etc/passwd"));
        assert!(!PathStyle::ambiguous("C:/x"));
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
                path_label(&host.to_string_lossy(), PathStyle::host()),
                path_label(&path.to_string_lossy(), PathStyle::host()),
                "{rest}"
            );
        }
    }
}
