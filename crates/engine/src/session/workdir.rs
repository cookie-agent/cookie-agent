//! Session directory scaffolding and the workdir cwd marker.

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

/// Lowercased, `[a-z0-9._-]`-only, repeat-collapsed, 32-char-truncated basename
/// of the canonical cwd (§1.1). Empty results fall back to a hash-only key.
pub(super) fn workdir_key_suffix(cwd: &Path) -> String {
    fn trim(value: &str) -> String {
        value
            .trim_matches(|character| character == '-' || character == '_')
            .to_owned()
    }

    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned());
    let base = canonical
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut sanitized = String::with_capacity(base.len());
    for character in base.chars() {
        let mapped = if character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '_' | '-')
        {
            character
        } else {
            '-'
        };
        if mapped == '-' && sanitized.ends_with('-') {
            continue;
        }
        sanitized.push(mapped);
    }
    trim(&trim(&sanitized).chars().take(32).collect::<String>())
}

pub(super) fn write_layout_marker_if_absent(workdir_dir: &Path) -> Result<(), SessionError> {
    let path = workdir_dir.join(LAYOUT_MARKER_FILE);
    if path.exists() {
        return Ok(());
    }
    let marker = serde_json::json!({ "version": LAYOUT_VERSION });
    let temporary = workdir_dir.join(format!(".{LAYOUT_MARKER_FILE}.{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<(), SessionError> {
        #[cfg(unix)]
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|source| SessionError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            serde_json::to_writer_pretty(&mut file, &marker).map_err(|source| {
                SessionError::Json {
                    path: temporary.clone(),
                    source,
                }
            })?;
            file.sync_all().map_err(|source| SessionError::Io {
                path: temporary.clone(),
                source,
            })?;
            drop(file);
            fs::rename(&temporary, &path).map_err(|source| SessionError::Io {
                path: path.clone(),
                source,
            })?;
            fsync_directory(workdir_dir)?;
        }
        #[cfg(windows)]
        {
            let mut file =
                cookie_agent_models::secure_store::create_windows_private_file(&temporary)
                    .map_err(|source| SessionError::Io {
                        path: temporary.clone(),
                        source,
                    })?;
            serde_json::to_writer_pretty(&mut file, &marker).map_err(|source| {
                SessionError::Json {
                    path: temporary.clone(),
                    source,
                }
            })?;
            file.sync_all().map_err(|source| SessionError::Io {
                path: temporary.clone(),
                source,
            })?;
            drop(file);
            replace_windows_path_with_retry(&temporary, &path).map_err(|source| {
                SessionError::Io {
                    path: path.clone(),
                    source,
                }
            })?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
pub(super) fn write_workdir_cwd(workdir_dir: &Path, cwd: &Path) -> Result<(), SessionError> {
    let Ok(canonical) = cwd.canonicalize() else {
        return Ok(());
    };
    let bytes = canonical.as_os_str().as_bytes();
    let path = workdir_dir.join(WORKDIR_CWD_FILE);
    if workdir_cwd_is_current(&path, bytes) {
        return Ok(());
    }

    let temporary = workdir_dir.join(format!(".{WORKDIR_CWD_FILE}.{}.tmp", Uuid::now_v7()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &path)?;
        fs::File::open(workdir_dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|source| SessionError::Io { path, source })
}

#[cfg(windows)]
pub(super) fn write_workdir_cwd(workdir_dir: &Path, cwd: &Path) -> Result<(), SessionError> {
    let Ok(canonical) = cwd.canonicalize() else {
        return Ok(());
    };
    let bytes = canonical.as_os_str().as_encoded_bytes();
    let path = workdir_dir.join(WORKDIR_CWD_FILE);
    if workdir_cwd_is_current(&path, bytes) {
        return Ok(());
    }
    let temporary = workdir_dir.join(format!(".{WORKDIR_CWD_FILE}.{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<(), SessionError> {
        let mut file = cookie_agent_models::secure_store::create_windows_private_file(&temporary)
            .map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        file.write_all(bytes).map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        drop(file);
        replace_windows_path_with_retry(&temporary, &path).map_err(|source| SessionError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
pub(super) fn workdir_cwd_is_current(path: &Path, expected: &[u8]) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes == expected)
}

#[cfg(windows)]
pub(super) fn workdir_cwd_is_current(path: &Path, expected: &[u8]) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes == expected)
}

#[cfg(unix)]
pub(crate) fn create_unix_session_directory_all(path: &Path) -> Result<(), SessionError> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path).map_err(|source| SessionError::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(windows)]
pub(crate) fn create_windows_session_directory(path: &Path) -> Result<(), SessionError> {
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
