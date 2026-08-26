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
}
