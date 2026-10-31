//! Converting a session tree's history to this build's format on disk
//! (`docs/site/specs/compatibility-v1.md` section 8.7).

use super::*;
use crate::events::convert::{self as history, ConvertError};
use crate::ownership::{SessionOwnership, try_acquire};

/// What converting one session tree did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TreeConversion {
    /// Each log converted, with the history format it had.
    pub converted: Vec<(SessionId, u32)>,
    /// Each log left as it is because no conversion from its format exists:
    /// `None` for one written before history formats, else its format,
    /// such as a newer build's.
    pub left: Vec<(SessionId, Option<u32>)>,
}

/// What one member's log needs.
enum Stage {
    Current,
    Staged(Staged),
    Left(Option<u32>),
}

/// One log converted and written beside its original, not yet in place.
struct Staged {
    dir: PathBuf,
    format: u32,
    log: PathBuf,
    metadata: PathBuf,
}

const STAGED_SUFFIX: &str = "convert.tmp";

/// Converts every log of the tree rooted at `root_dir` (the root's, and
/// each child's under `subagents/`) whose history format is older than
/// this build's, together with each one's `metadata` cache. The caller holds
/// the tree's lock.
///
/// Each converted log and its `metadata`, folded from it, are first written
/// beside the originals and synced. Only once every log converted does each
/// original become `events.jsonl.v<N>.bak` and the new file take its place;
/// `subagents/index.json`, a cache of the old summaries, is removed and
/// rebuilt by the next tree load. A failure before that leaves every
/// original untouched. A log already current, of a newer format, or of one
/// no conversion exists from is left for its open to judge.
pub fn convert_tree(root: SessionId, root_dir: &Path) -> Result<TreeConversion, SessionError> {
    let mut members = vec![(root, root_dir.to_owned())];
    if let Ok(entries) = fs::read_dir(root_dir.join(SUBAGENTS_DIR)) {
        for entry in entries.flatten() {
            if let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<SessionId>().ok())
                && entry.path().is_dir()
            {
                members.push((id, entry.path()));
            }
        }
    }
    let mut staged = Vec::new();
    let mut conversion = TreeConversion::default();
    for (id, dir) in &members {
        match stage(*id, dir) {
            Ok(Stage::Staged(log)) => {
                conversion.converted.push((*id, log.format));
                staged.push(log);
            }
            Ok(Stage::Current) => {}
            Ok(Stage::Left(format)) => conversion.left.push((*id, format)),
            Err(error) => {
                for log in &staged {
                    let _ = fs::remove_file(&log.log);
                    let _ = fs::remove_file(&log.metadata);
                }
                return Err(error);
            }
        }
    }
    for log in &staged {
        commit(log)?;
    }
    if !staged.is_empty() {
        let index = root_dir.join(SUBAGENTS_DIR).join(SUBAGENT_INDEX_FILE);
        match fs::remove_file(&index) {
            Ok(()) => fsync_directory(&root_dir.join(SUBAGENTS_DIR))?,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(SessionError::Io {
                    path: index,
                    source,
                });
            }
        }
    }
    Ok(conversion)
}

/// The history format the log at `path` declares on its first line: `None`
/// when there is no log, `Some(None)` when it declares none.
fn log_format(path: &Path) -> Result<Option<Option<u32>>, SessionError> {
    use std::io::BufRead as _;
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SessionError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let mut first = Vec::new();
    std::io::BufReader::new(file)
        .read_until(b'\n', &mut first)
        .map_err(|source| SessionError::Io {
            path: path.to_owned(),
            source,
        })?;
    Ok(Some(
        serde_json::from_slice::<serde_json::Value>(&first)
            .ok()
            .as_ref()
            .and_then(history::history_format),
    ))
}

/// Converts one member's log beside the original, when it needs it.
fn stage(id: SessionId, dir: &Path) -> Result<Stage, SessionError> {
    let path = dir.join(EVENTS_FILE);
    let format = match log_format(&path)? {
        None => return Ok(Stage::Current),
        Some(Some(format)) if format == cookie_agent_protocol::HISTORY_FORMAT => {
            return Ok(Stage::Current);
        }
        Some(Some(format)) if history::convertible(format) => format,
        Some(format) => return Ok(Stage::Left(format)),
    };
    let bytes = fs::read(&path).map_err(|source| SessionError::Io {
        path: path.clone(),
        source,
    })?;
    // A torn final record stays in the backup only.
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    let converted =
        history::convert_bytes(format, &bytes[..complete]).map_err(|error: ConvertError| {
            SessionError::Event(EventLogError::Corrupt {
                path: path.clone(),
                message: error.to_string(),
            })
        })?;
    let log = dir.join(format!("{EVENTS_FILE}.{STAGED_SUFFIX}"));
    let metadata = dir.join(format!("{SESSION_META_FILE}.{STAGED_SUFFIX}"));
    let staged = Staged {
        dir: dir.to_owned(),
        format,
        log,
        metadata,
    };
    let result = (|| {
        write_staged(&staged.log, &converted)?;
        let meta = projection(EventLog::open_read_only(staged.log.clone(), id)?)?.meta;
        let meta = serde_json::to_vec_pretty(&meta).map_err(|source| SessionError::Json {
            path: staged.metadata.clone(),
            source,
        })?;
        write_staged(&staged.metadata, &meta)
    })();
    match result {
        Ok(()) => Ok(Stage::Staged(staged)),
        Err(error) => {
            let _ = fs::remove_file(&staged.log);
            let _ = fs::remove_file(&staged.metadata);
            Err(error)
        }
    }
}

/// Writes `bytes` as the private file at `path`, replacing what a crash left
/// there, and syncs it.
fn write_staged(path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
    let io_error = |source| SessionError::Io {
        path: path.to_owned(),
        source,
    };
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(source)),
    }
    #[cfg(unix)]
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io_error)?;
    #[cfg(windows)]
    let mut file =
        cookie_agent_models::secure_store::create_windows_private_file(path).map_err(io_error)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(io_error)
}

/// Puts one staged log in place: the original stays as its backup.
fn commit(staged: &Staged) -> Result<(), SessionError> {
    let log = staged.dir.join(EVENTS_FILE);
    let backup = staged
        .dir
        .join(format!("{EVENTS_FILE}.v{}.bak", staged.format));
    let backup_error = |source| SessionError::Io {
        path: backup.clone(),
        source,
    };
    match fs::remove_file(&backup) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(backup_error(source)),
    }
    // The backup is a second name for the original, so `events.jsonl` names
    // a complete log at every moment; a copy where links do not work.
    if fs::hard_link(&log, &backup).is_err() {
        fs::copy(&log, &backup)
            .and_then(|_| fs::File::open(&backup)?.sync_all())
            .map_err(backup_error)?;
    }
    fsync_directory(&staged.dir)?;
    replace(&staged.log, &log)?;
    replace(&staged.metadata, &meta_path(&staged.dir))?;
    fsync_directory(&staged.dir)?;
    Ok(())
}

fn replace(source: &Path, target: &Path) -> Result<(), SessionError> {
    #[cfg(unix)]
    let result = fs::rename(source, target);
    #[cfg(windows)]
    let result = replace_windows_path_with_retry(source, target);
    result.map_err(|source| SessionError::Io {
        path: target.to_owned(),
        source,
    })
}

/// What upgrading one session tree did.
#[derive(Debug)]
pub enum TreeUpgrade {
    /// Every log was already in this build's history format.
    Current,
    /// Logs were converted, or left as they are (see
    /// [`TreeConversion::left`]).
    Converted(TreeConversion),
    /// Another cookie process holds the tree; it converts it itself.
    InUse,
    Failed(SessionError),
}

/// Converts every session tree of the work dir of `cwd`, or of every work
/// dir when `cwd` is `None`, to this build's history format, as an owned
/// open would (`cookie sessions upgrade`). Returns each tree's root and work
/// dir with what happened to it.
pub fn upgrade_sessions(
    data_root: &Path,
    cwd: Option<&Path>,
) -> Result<Vec<(PathBuf, SessionId, TreeUpgrade)>, SessionError> {
    let work_dirs = match cwd {
        Some(cwd) => vec![SessionStore::resolve_workdir_dir(data_root, cwd)],
        None => {
            let sessions = data_root.join(SESSIONS_ROOT_DIR);
            match fs::read_dir(&sessions) {
                Ok(entries) => {
                    let mut dirs = entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| path.is_dir())
                        .collect::<Vec<_>>();
                    dirs.sort();
                    dirs
                }
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(source) => {
                    return Err(SessionError::Io {
                        path: sessions,
                        source,
                    });
                }
            }
        }
    };
    let mut upgrades = Vec::new();
    for work_dir in work_dirs {
        let Ok(entries) = fs::read_dir(&work_dir) else {
            continue;
        };
        let mut roots = entries
            .flatten()
            .filter_map(|entry| {
                let id = entry.file_name().to_str()?.parse::<SessionId>().ok()?;
                entry.path().join(EVENTS_FILE).is_file().then_some(id)
            })
            .collect::<Vec<_>>();
        roots.sort_by_key(ToString::to_string);
        for root in roots {
            let root_dir = work_dir.join(root.to_string());
            let upgrade = match try_acquire(&root_dir) {
                Ok(SessionOwnership::Owned(_lock)) => match convert_tree(root, &root_dir) {
                    Ok(conversion) if conversion == TreeConversion::default() => {
                        TreeUpgrade::Current
                    }
                    Ok(conversion) => TreeUpgrade::Converted(conversion),
                    Err(error) => TreeUpgrade::Failed(error),
                },
                Ok(SessionOwnership::Foreign) => TreeUpgrade::InUse,
                Err(source) => TreeUpgrade::Failed(SessionError::Io {
                    path: root_dir.clone(),
                    source,
                }),
            };
            upgrades.push((work_dir.clone(), root, upgrade));
        }
    }
    Ok(upgrades)
}
