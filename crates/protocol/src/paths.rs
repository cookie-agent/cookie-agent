//! Cross-platform per-user path resolution for Cookie Agent.
//!
//! This module lives in `cookie_agent_protocol` because models, config, engine,
//! server, TUI, and tools already depend on that low-level crate. Placing it in
//! `cookie_agent_config` would create a dependency cycle with models.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Errors returned while resolving per-user paths.
#[derive(Debug, Error)]
pub enum PathError {
    #[error("home directory is unavailable")]
    HomeUnavailable,
}

/// Returns the current user's home directory.
///
/// # Errors
///
/// Returns [`PathError::HomeUnavailable`] when the standard library cannot
/// determine a home directory for the current user.
pub fn home_dir() -> Result<PathBuf, PathError> {
    std::env::home_dir().ok_or(PathError::HomeUnavailable)
}

/// Returns the unified Cookie Agent per-user data root.
///
/// # Errors
///
/// Returns [`PathError::HomeUnavailable`] when the current user's home
/// directory cannot be determined.
pub fn user_data_root() -> Result<PathBuf, PathError> {
    Ok(home_dir()?.join(".cookie-agent"))
}

/// A stable, opaque identity of this machine, so a client can tell whether a
/// daemon runs on its own machine: a SHA-256 of the operating system's machine
/// ID (`/etc/machine-id` or `/var/lib/dbus/machine-id`) or, where there is
/// none, of a random ID kept in `~/.cookie-agent/machine-id`, created on
/// first use. The fallback is per user, so two users of one machine without
/// an OS machine ID look like two machines. `None` when neither is
/// available.
#[must_use]
pub fn machine_id() -> Option<String> {
    static MACHINE_ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    MACHINE_ID
        .get_or_init(|| {
            let raw = ["/etc/machine-id", "/var/lib/dbus/machine-id"]
                .into_iter()
                .find_map(|path| {
                    std::fs::read_to_string(path)
                        .ok()
                        .map(|id| id.trim().to_owned())
                        .filter(|id| !id.is_empty())
                })
                .or_else(user_machine_id)?;
            let digest = <sha2::Sha256 as sha2::Digest>::digest(
                format!("cookie-agent machine:{raw}").as_bytes(),
            );
            Some(
                digest
                    .iter()
                    .take(16)
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            )
        })
        .clone()
}

/// This machine's host name; empty when it is unavailable.
#[must_use]
pub fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0_u8; 256];
        // SAFETY: the buffer is writable for its whole length, and
        // gethostname writes at most that many bytes.
        let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        if result != 0 {
            return String::new();
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..end]).into_owned()
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

/// The random machine ID kept in the user's data root, created when missing.
fn user_machine_id() -> Option<String> {
    let path = user_data_root().ok()?.join("machine-id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim().to_owned();
        if !id.is_empty() {
            return Some(id);
        }
    }
    let id = uuid::Uuid::now_v7().simple().to_string();
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, format!("{id}\n")).ok()?;
    Some(id)
}

/// `path` canonicalized for showing and comparing: on Windows without the
/// `\\?\` verbatim prefix `std::fs::canonicalize` adds, whenever the path
/// means the same without it (see [`simplified`]); elsewhere exactly what
/// `canonicalize` returns. Every canonical path a person or a model reads,
/// such as a workspace root or a session's directory, comes from here, so
/// they all compare equal.
///
/// # Errors
///
/// Fails as `canonicalize` does, for example when the path does not exist.
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize().map(simplified)
}

/// `path` without the Windows verbatim prefix (`\\?\C:\…` becomes `C:\…`,
/// `\\?\UNC\server\share\…` becomes `\\server\share\…`) when the path
/// means the same without it: shorter than `MAX_PATH`, and with no component
/// a non-verbatim path would read differently (a device name such as `CON`,
/// or a name ending in a dot or a space). Other paths, and every path on
/// other platforms, are returned unchanged.
#[must_use]
pub fn simplified(path: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return path;
    }
    match path.to_str().and_then(simplified_windows) {
        Some(simple) => PathBuf::from(simple),
        None => path,
    }
}

/// The non-verbatim spelling of the Windows path `path`, when it has one
/// that means the same.
fn simplified_windows(path: &str) -> Option<String> {
    const MAX_PATH: usize = 260;
    let simple = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        let rest = path.strip_prefix(r"\\?\")?;
        let bytes = rest.as_bytes();
        if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || &bytes[1..3] != b":\\" {
            return None;
        }
        rest.to_owned()
    };
    if simple.len() >= MAX_PATH || simple.contains('/') {
        return None;
    }
    let reserved = |name: &str| {
        let stem = name
            .split('.')
            .next()
            .unwrap_or(name)
            .trim_end()
            .to_ascii_uppercase();
        matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit())
    };
    let components = simple
        .trim_start_matches('\\')
        .split('\\')
        .skip(1)
        .filter(|component| !component.is_empty());
    for component in components {
        if component.ends_with('.') || component.ends_with(' ') || reserved(component) {
            return None;
        }
    }
    Some(simple)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_verbatim_path_is_simplified_when_it_means_the_same() {
        let simple = |path: &str| super::simplified_windows(path);
        assert_eq!(
            simple(r"\\?\C:\Users\me\work").as_deref(),
            Some(r"C:\Users\me\work")
        );
        assert_eq!(
            simple(r"\\?\UNC\server\share\work").as_deref(),
            Some(r"\\server\share\work")
        );
        assert_eq!(simple(r"C:\Users\me"), None, "not verbatim");
        assert_eq!(simple(r"\\?\C:\work\con.txt"), None, "a device name");
        assert_eq!(simple(r"\\?\C:\work\trailing."), None, "a trailing dot");
        assert_eq!(simple(r"\\?\Volume{0}\work"), None, "not a drive");
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(simple(&long), None, "longer than MAX_PATH");
    }

    #[test]
    fn user_data_root_is_below_home() {
        let home = super::home_dir().expect("home directory");
        assert_eq!(
            super::user_data_root().expect("user data root"),
            home.join(".cookie-agent")
        );
    }

    #[test]
    fn the_machine_id_is_a_stable_opaque_digest() {
        let Some(id) = super::machine_id() else {
            return;
        };
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()), "{id}");
        assert_eq!(super::machine_id().as_deref(), Some(id.as_str()));
    }
}
