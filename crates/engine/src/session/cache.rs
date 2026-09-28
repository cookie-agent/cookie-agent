//! Rebuildable metadata caches, the subagent index, and the mutation guard.

use super::*;

/// RAII holder for the store's durable-mutation lock. `locked: None` means this
/// thread already holds *this store's* lock further up the stack, so the guard is
/// a no-op.
pub(super) struct MutationGuard<'a> {
    pub(super) locked: Option<std::sync::MutexGuard<'a, ()>>,
    pub(super) store: Option<usize>,
}

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        let Some(store) = self.store else {
            return;
        };
        self.locked = None;
        MUTATION_DEPTH.with(|depths| {
            depths.borrow_mut().remove(&store);
        });
    }
}

/// Whether an atomic cache replacement is also made crash-durable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CacheSync {
    /// fsync the staged file before the rename and the parent directory after
    /// it. Used where the file's existence publishes something (a new session
    /// directory's `metadata`) and for `subagents/index.json`.
    Durable,
    /// Atomic rename only: readers never observe a torn file, but a power loss
    /// may leave the previous version behind. Used for the append-path
    /// `metadata` refresh, a rebuildable cache of the event log whose on-disk
    /// tip may lag the log anyway.
    Atomic,
}

/// Atomically rewrites a small JSON cache (temp file + rename + parent fsync),
/// creating its parent directory first.
pub(super) fn write_index_json<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), SessionError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| SessionError::Json {
        path: path.to_owned(),
        source,
    })?;
    let parent = cache_parent(path)?;
    #[cfg(unix)]
    create_unix_session_directory_all(parent)?;
    #[cfg(windows)]
    create_windows_session_directory(parent)?;
    replace_cache_file(path, &bytes, CacheSync::Durable)
}

fn cache_parent(path: &Path) -> Result<&Path, SessionError> {
    path.parent().ok_or_else(|| SessionError::Io {
        path: path.to_owned(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session cache has no parent",
        ),
    })
}

/// Stages `bytes` in a private temporary beside `path` and renames it over
/// `path`, so a reader sees either the old or the new file, never a partial one.
fn replace_cache_file(path: &Path, bytes: &[u8], sync: CacheSync) -> Result<(), SessionError> {
    let parent = cache_parent(path)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "cache".to_owned());
    let temporary = parent.join(format!(".{name}.{}.tmp", Uuid::now_v7()));
    let durable = sync == CacheSync::Durable;
    let result = (|| -> Result<(), SessionError> {
        #[cfg(unix)]
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|source| SessionError::Io {
                path: temporary.clone(),
                source,
            })?;
        #[cfg(windows)]
        let mut file = cookie_agent_models::secure_store::create_windows_private_file(&temporary)
            .map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        file.write_all(bytes)
            .and_then(|()| if durable { file.sync_all() } else { Ok(()) })
            .map_err(|source| SessionError::Io {
                path: temporary.clone(),
                source,
            })?;
        drop(file);
        #[cfg(unix)]
        {
            fs::rename(&temporary, path).map_err(|source| SessionError::Io {
                path: path.to_owned(),
                source,
            })?;
            if durable {
                fsync_directory(parent)?;
            }
        }
        #[cfg(windows)]
        replace_windows_path_with_retry(&temporary, path).map_err(|source| SessionError::Io {
            path: path.to_owned(),
            source,
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Session metadata cache path.
pub(crate) fn meta_path(session_dir: &Path) -> PathBuf {
    session_dir.join(SESSION_META_FILE)
}

/// Replaces a session's `metadata` cache; `sync` picks crash durability.
pub(super) fn write_cache(
    path: &Path,
    cache: &SessionMeta,
    sync: CacheSync,
) -> Result<(), SessionError> {
    let bytes = serde_json::to_value(cache)
        .and_then(|persisted| serde_json::to_vec_pretty(&persisted))
        .map_err(|source| SessionError::Json {
            path: path.to_owned(),
            source,
        })?;
    replace_cache_file(path, &bytes, sync)
}

/// Retries a replacement that lost a race for the staged source or the target.
/// `replace_windows_path` prefers a superseding POSIX rename, which tolerates an
/// open target, so the remaining contention is an antivirus scanner or indexer
/// holding the staged temporary open, plus the same open-target contention as
/// before on hosts that fall back to `MoveFileExW`. The retryable set is
/// therefore unchanged.
#[cfg(windows)]
pub(crate) fn replace_windows_path_with_retry(source: &Path, target: &Path) -> std::io::Result<()> {
    const ATTEMPTS: usize = 50;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(25);

    for attempt in 0..ATTEMPTS {
        match cookie_agent_models::secure_store::replace_windows_path(source, target) {
            Ok(()) => return Ok(()),
            Err(error) if attempt + 1 < ATTEMPTS && windows_replace_is_contended(&error) => {
                std::thread::sleep(BACKOFF);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("path replacement attempts are nonzero")
}

#[cfg(windows)]
pub(super) fn windows_replace_is_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(error.raw_os_error(), Some(5 | 32))
}

pub(super) fn read_cache(path: &Path, events_path: &Path) -> Result<SessionMeta, SessionError> {
    let bytes = fs::read(path).map_err(|source| SessionError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut value = serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|source| {
        SessionError::Json {
            path: path.to_owned(),
            source,
        }
    })?;
    if value.get("last_activity").is_none() {
        let modified = fs::metadata(events_path)
            .and_then(|metadata| metadata.modified())
            .map_err(|source| SessionError::Io {
                path: events_path.to_owned(),
                source,
            })?;
        let timestamp =
            jiff::Timestamp::try_from(modified).unwrap_or_else(|_| jiff::Timestamp::now());
        value
            .as_object_mut()
            .ok_or_else(|| SessionError::Json {
                path: path.to_owned(),
                source: serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "session metadata is not an object",
                )),
            })?
            .insert(
                "last_activity".into(),
                serde_json::to_value(timestamp).expect("timestamp serializes"),
            );
    }
    serde_json::from_value(value).map_err(|source| SessionError::Json {
        path: path.to_owned(),
        source,
    })
}

thread_local! {
    /// Which stores this thread holds a `lock_mutation` guard for. Keyed by store
    /// identity, so nesting two stores on one thread cannot make the second one
    /// skip its own mutex (review L15).
    pub(super) static MUTATION_DEPTH: std::cell::RefCell<HashMap<usize, ()>> =
        std::cell::RefCell::new(HashMap::new());
}
