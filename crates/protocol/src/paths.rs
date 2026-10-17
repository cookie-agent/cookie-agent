//! Cross-platform per-user path resolution for Cookie Agent.
//!
//! This module lives in `cookie_agent_protocol` because models, config, engine,
//! server, TUI, and tools already depend on that low-level crate. Placing it in
//! `cookie_agent_config` would create a dependency cycle with models.

use std::path::PathBuf;

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

#[cfg(test)]
mod tests {
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
