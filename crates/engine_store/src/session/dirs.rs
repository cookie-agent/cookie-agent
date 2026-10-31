//! Session directory scaffolding: private folders and files.

use super::*;

/// Every entry of a session directory, or `None` when it is not a directory at
/// all. A per-entry failure is returned instead of skipped: guessing about a
/// listing is exactly what a publish decision must not do.
pub(super) fn scaffold_listing(
    directory: &Path,
) -> Result<Option<Vec<fs::DirEntry>>, SessionError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SessionError::Io {
                path: directory.to_owned(),
                source,
            });
        }
    };
    let mut listing = Vec::new();
    for entry in entries {
        listing.push(entry.map_err(|source| SessionError::Io {
            path: directory.to_owned(),
            source,
        })?);
    }
    Ok(Some(listing))
}

#[cfg(unix)]
pub fn create_unix_session_directory_all(path: &Path) -> Result<(), SessionError> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path).map_err(|source| SessionError::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(windows)]
pub fn create_windows_session_directory(path: &Path) -> Result<(), SessionError> {
    cookie_agent_models::secure_store::SecureDirectory::open(path)
        .map(|_| ())
        .map_err(|error| match error {
            cookie_agent_models::secure_store::SecureStoreError::Io(source) => SessionError::Io {
                path: path.to_owned(),
                source,
            },
            error => SessionError::Io {
                path: path.to_owned(),
                source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, error),
            },
        })
}

#[cfg(windows)]
pub(super) fn create_windows_session_file(path: &Path) -> Result<(), SessionError> {
    cookie_agent_models::secure_store::create_windows_private_file(path)
        .map(drop)
        .map_err(|source| SessionError::Io {
            path: path.to_owned(),
            source,
        })
}
