//! Session storage layout v3 (`docs/site/specs/session-storage-v3.md`,
//! sections 4.1, 4.4, and 5): every root filed under the UTC month of its
//! id, one store-wide `layout.json`, and the one-time migration from the
//! v2 work-dir folders that runs before a daemon accepts connections.

use std::time::Instant;

use jiff::{Timestamp, tz::TimeZone};
use serde::{Deserialize, Serialize};

use super::*;
use crate::ownership::{SessionOwnership, try_acquire};

/// Layout version written by this build.
pub const LAYOUT_VERSION: u32 = 3;
/// The activity journals' folder under `sessions/` (section 4.3).
pub const ACTIVITY_DIR: &str = "activity";
/// The runtime revision index, one for the whole store.
pub const RUNTIME_REVISIONS_FILE: &str = "runtime-revisions.jsonl";
/// Where the v2 layout kept the runtime revision index, in each work-dir
/// folder.
const V2_RUNTIME_REVISIONS_FILE: &str = "runtime-revisions-v8.jsonl";
/// The originals of the logs a migration converted, until it completes.
pub const BACKUP_DIR: &str = ".backup-v2";
/// Files a v2 work-dir folder held besides its session trees.
const V2_WORK_DIR_FILES: [&str; 3] = ["cwd", LAYOUT_MARKER_FILE, V2_RUNTIME_REVISIONS_FILE];

/// A UTC calendar month, the name of a folder of roots (`2026-10`) and of
/// an activity journal (`2026-10.jsonl`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Month {
    year: i16,
    month: i8,
}

impl Month {
    /// The month `timestamp` falls in, in UTC.
    #[must_use]
    pub fn of(timestamp: Timestamp) -> Self {
        let date = timestamp.to_zoned(TimeZone::UTC).date();
        Self {
            year: date.year(),
            month: date.month(),
        }
    }

    /// The month of a UUIDv7 id's timestamp; `None` for any other id.
    #[must_use]
    pub fn of_id(id: SessionId) -> Option<Self> {
        if id.0.get_version_num() != 7 {
            return None;
        }
        let (seconds, _) = id.0.get_timestamp()?.to_unix();
        Timestamp::from_second(i64::try_from(seconds).ok()?)
            .ok()
            .map(Self::of)
    }

    /// Parses a folder or journal name, `yyyy-mm`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let (year, month) = name.split_once('-')?;
        if year.len() != 4
            || month.len() != 2
            || !year
                .bytes()
                .chain(month.bytes())
                .all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let year = year.parse::<i16>().ok()?;
        let month = month.parse::<i8>().ok()?;
        ((1..=9999).contains(&year) && (1..=12).contains(&month)).then_some(Self { year, month })
    }

    /// The month before this one.
    #[must_use]
    pub fn previous(self) -> Self {
        if self.month == 1 {
            Self {
                year: self.year - 1,
                month: 12,
            }
        } else {
            Self {
                year: self.year,
                month: self.month - 1,
            }
        }
    }

    /// The first instant of the month.
    #[must_use]
    pub fn start(self) -> Timestamp {
        jiff::civil::Date::new(self.year, self.month, 1)
            .and_then(|date| date.to_zoned(TimeZone::UTC))
            .map_or(Timestamp::MIN, |zoned| zoned.timestamp())
    }
}

impl std::fmt::Display for Month {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:04}-{:02}", self.year, self.month)
    }
}

/// The month folders under `sessions_dir`, oldest first.
pub(super) fn month_dirs(sessions_dir: &Path) -> Vec<Month> {
    let mut months = fs::read_dir(sessions_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .filter_map(|entry| Month::parse(entry.file_name().to_str()?))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    months.sort();
    months
}

/// The session-id-named folders in one month folder: roots by construction.
pub(super) fn month_root_ids(sessions_dir: &Path, month: Month) -> Vec<SessionId> {
    session_dir_ids(&sessions_dir.join(month.to_string()))
}

/// The session-id-named folders directly inside `directory`.
fn session_dir_ids(directory: &Path) -> Vec<SessionId> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut ids = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().to_str()?.parse::<SessionId>().ok())
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

/// The folder of the root session `id` under `sessions_dir`: its id's month
/// for a UUIDv7 id. Any other id (none this build creates) is found in the
/// month folder that holds it, newest first; one that none holds would be
/// created now, in this month.
#[must_use]
pub fn root_dir_in(sessions_dir: &Path, id: SessionId) -> PathBuf {
    let month = Month::of_id(id)
        .or_else(|| find_root_month(sessions_dir, id))
        .unwrap_or_else(|| Month::of(Timestamp::now()));
    sessions_dir.join(month.to_string()).join(id.to_string())
}

/// The month folder holding the root `id`, searched newest first.
pub(super) fn find_root_month(sessions_dir: &Path, id: SessionId) -> Option<Month> {
    month_dirs(sessions_dir).into_iter().rev().find(|month| {
        sessions_dir
            .join(month.to_string())
            .join(id.to_string())
            .is_dir()
    })
}

/// `sessions/layout.json`: the layout and history format of the whole store.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StoreLayout {
    pub version: u32,
    #[serde(default)]
    pub history_format: u32,
    /// The version of the cookie build that wrote the file, which an older
    /// build refusing the store names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// The trees the migration could not move yet, retried at every start.
    #[serde(default)]
    pub pending: Vec<PendingTree>,
}

impl StoreLayout {
    fn current(pending: Vec<PendingTree>) -> Self {
        Self {
            version: LAYOUT_VERSION,
            history_format: cookie_agent_protocol::HISTORY_FORMAT,
            build: Some(env!("CARGO_PKG_VERSION").to_owned()),
            pending,
        }
    }

    fn is_current(&self) -> bool {
        self.version == LAYOUT_VERSION
            && self.history_format == cookie_agent_protocol::HISTORY_FORMAT
            && self.pending.is_empty()
    }
}

/// A tree that stays in its old folder until a migration step succeeds.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingTree {
    pub root_session_id: SessionId,
    /// Its folder, relative to `sessions/`.
    pub path: String,
    pub error: String,
}

/// What [`prepare`] found and did.
#[derive(Debug)]
pub(super) struct PreparedLayout {
    pub(super) layout: StoreLayout,
    /// Roots whose deletion stopped after its commit point, which the
    /// activity journal may still list.
    pub(super) deleted: Vec<SessionId>,
}

/// Brings the store at `sessions_dir` to this build's layout before
/// anything opens a session: refuses a newer store, migrates an older one
/// (or retries its pending trees), and removes what a finished migration or
/// deletion left behind. A current store costs one read of `layout.json`
/// and one listing of `sessions/`.
pub(super) fn prepare(sessions_dir: &Path) -> Result<PreparedLayout, SessionError> {
    // A marker that does not read is treated as missing: migrating again
    // moves nothing that moved already and rebuilds the journals.
    let existing = match read_layout(sessions_dir) {
        Err(SessionError::Json { path, source }) => {
            eprintln!(
                "cookie-agent: {} does not read ({source}); checking the store again",
                path.display()
            );
            None
        }
        existing => existing?,
    };
    if let Some(layout) = &existing {
        refuse_newer(sessions_dir, layout)?;
    }
    let layout = match existing {
        Some(layout) if layout.is_current() => {
            // Journals someone removed are rebuilt rather than listing
            // nothing.
            if !sessions_dir.join(ACTIVITY_DIR).is_dir() && !month_dirs(sessions_dir).is_empty() {
                eprintln!(
                    "cookie-agent: the session activity journals are missing; rebuilding them"
                );
                super::activity::rebuild_journals(sessions_dir)?;
            }
            layout
        }
        existing => migrate(sessions_dir, existing.as_ref())?,
    };
    let deleted = sweep_top_level(sessions_dir, &layout.pending);
    Ok(PreparedLayout { layout, deleted })
}

fn layout_path(sessions_dir: &Path) -> PathBuf {
    sessions_dir.join(LAYOUT_MARKER_FILE)
}

/// `sessions/layout.json`, or `None` before the first v3 start.
pub fn read_layout(sessions_dir: &Path) -> Result<Option<StoreLayout>, SessionError> {
    let path = layout_path(sessions_dir);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(SessionError::Io { path, source }),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| SessionError::Json { path, source })
}

/// Refuses a store a newer build laid out, naming the build it needs.
fn refuse_newer(sessions_dir: &Path, layout: &StoreLayout) -> Result<(), SessionError> {
    if layout.version <= LAYOUT_VERSION
        && layout.history_format <= cookie_agent_protocol::HISTORY_FORMAT
    {
        return Ok(());
    }
    Err(SessionError::NewerStore {
        path: sessions_dir.to_owned(),
        version: layout.version,
        history_format: layout.history_format,
        needed: layout.build.as_deref().map_or_else(
            || "a newer cookie build".to_owned(),
            |build| format!("cookie {build} or newer"),
        ),
    })
}

/// Writes `layout.json` durably: the file, then its directory. This is the
/// migration's commit point.
fn write_layout(sessions_dir: &Path, layout: &StoreLayout) -> Result<(), SessionError> {
    let path = layout_path(sessions_dir);
    let bytes = serde_json::to_vec_pretty(layout).map_err(|source| SessionError::Json {
        path: path.clone(),
        source,
    })?;
    replace_cache_file(&path, &bytes, CacheSync::Durable)?;
    // The rename above syncs the directory on Unix; Windows has no
    // directory sync to offer beyond this best effort.
    fsync_directory(sessions_dir)?;
    Ok(())
}

/// One root folder still in a v2 work-dir folder.
struct OldTree {
    root: SessionId,
    dir: PathBuf,
}

/// Every v2 work-dir folder under `sessions_dir`: a folder that is not a
/// month, not the activity journals, and not hidden.
fn old_work_dirs(sessions_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut dirs = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                Month::parse(name).is_none() && name != ACTIVITY_DIR && !name.starts_with('.')
            })
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    dirs.sort();
    dirs
}

/// Converts every tree in a month folder an older history format wrote (or,
/// for a store of this format, the pending ones), moves every root folder
/// of every v2 work-dir folder into its month, converting it too, each tree
/// one step, then writes the activity journals, moves the runtime revision
/// index up, and commits by writing `layout.json`. A step a crash cut short
/// is repeated at the next start, which finds the trees it finished current.
fn migrate(
    sessions_dir: &Path,
    existing: Option<&StoreLayout>,
) -> Result<StoreLayout, SessionError> {
    let started = Instant::now();
    // A store laid out before v3 has no journals yet; one retrying its
    // pending trees has them and only gains entries.
    let first = existing.is_none_or(|layout| layout.version < LAYOUT_VERSION);
    let work_dirs = old_work_dirs(sessions_dir);
    let trees = work_dirs
        .iter()
        .flat_map(|work_dir| {
            session_dir_ids(work_dir)
                .into_iter()
                .map(move |root| OldTree {
                    root,
                    dir: work_dir.join(root.to_string()),
                })
        })
        .collect::<Vec<_>>();
    // The trees in month folders an older history format wrote are
    // converted where they are, every one of them once; a retry converts
    // the pending ones only.
    let format = cookie_agent_protocol::HISTORY_FORMAT;
    let older_format = existing.is_none_or(|layout| layout.history_format < format);
    let in_months = if older_format {
        month_dirs(sessions_dir)
            .into_iter()
            .flat_map(|month| {
                let month_dir = sessions_dir.join(month.to_string());
                month_root_ids(sessions_dir, month)
                    .into_iter()
                    .map(move |root| OldTree {
                        root,
                        dir: month_dir.join(root.to_string()),
                    })
            })
            .collect::<Vec<_>>()
    } else {
        existing
            .map(|layout| layout.pending.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter(|tree| {
                Path::new(&tree.path)
                    .components()
                    .next()
                    .and_then(|first| Month::parse(first.as_os_str().to_str()?))
                    .is_some()
            })
            .map(|tree| OldTree {
                root: tree.root_session_id,
                dir: sessions_dir.join(&tree.path),
            })
            .filter(|tree| tree.dir.is_dir())
            .collect()
    };
    // A new store has nothing to move and says nothing.
    if !trees.is_empty() {
        eprintln!(
            "cookie-agent: moving {} session tree(s) to session storage layout {LAYOUT_VERSION}",
            trees.len()
        );
    }
    check_free_space(sessions_dir, trees.iter().chain(&in_months))?;
    let mut pending = Vec::new();
    let converting = Instant::now();
    let mut converted = 0;
    if !in_months.is_empty() {
        eprintln!(
            "cookie-agent: converting {} session tree(s) to history format {format}",
            in_months.len()
        );
    }
    for tree in &in_months {
        match convert_in_place(sessions_dir, tree) {
            Ok(true) => converted += 1,
            Ok(false) => {}
            Err(error) => pending.push(pending_tree(sessions_dir, tree, error)),
        }
    }
    if !in_months.is_empty() {
        eprintln!(
            "cookie-agent: history format {format}: converted {converted} of {} tree(s) in \
             {:.1?}, {} pending",
            in_months.len(),
            converting.elapsed(),
            pending.len()
        );
    }
    let mut moved = Vec::new();
    for tree in &trees {
        match move_tree(sessions_dir, tree) {
            Ok(dir) => moved.push((tree.root, dir)),
            Err(error) => pending.push(pending_tree(sessions_dir, tree, error)),
        }
    }
    // A tree an earlier start moved before a crash kept it from recording
    // that is in its month already, but has no journal entry yet.
    for earlier in existing
        .map(|layout| layout.pending.as_slice())
        .unwrap_or(&[])
    {
        let root = earlier.root_session_id;
        let dir = root_dir_in(sessions_dir, root);
        if !sessions_dir.join(&earlier.path).exists()
            && dir.is_dir()
            && !moved.iter().any(|(id, _)| *id == root)
        {
            moved.push((root, dir));
        }
    }
    sync_month_dirs(sessions_dir, &moved)?;
    for work_dir in &work_dirs {
        fsync_directory(work_dir)?;
    }
    if first {
        super::activity::rebuild_journals(sessions_dir)?;
    } else {
        let entries = moved
            .iter()
            .filter_map(|(root, dir)| root_meta(*root, dir))
            .collect::<Vec<_>>();
        super::activity::append_migrated(sessions_dir, &entries)?;
    }
    merge_runtime_revisions(sessions_dir, &work_dirs)?;
    let layout = StoreLayout::current(pending);
    write_layout(sessions_dir, &layout)?;
    if !trees.is_empty() {
        eprintln!(
            "cookie-agent: session storage layout {LAYOUT_VERSION}: moved {} tree(s) in {:.1?}, {} pending",
            moved.len(),
            started.elapsed(),
            layout.pending.len()
        );
    }
    Ok(layout)
}

/// Syncs every month folder a tree moved into, and `sessions/` itself, so
/// the moves are durable before the commit point.
fn sync_month_dirs(
    sessions_dir: &Path,
    moved: &[(SessionId, PathBuf)],
) -> Result<(), SessionError> {
    let months = moved
        .iter()
        .filter_map(|(_, dir)| dir.parent().map(Path::to_path_buf))
        .collect::<std::collections::BTreeSet<_>>();
    for month in months {
        fsync_directory(&month)?;
    }
    fsync_directory(sessions_dir)?;
    Ok(())
}

/// The month a root files under: its id's, or for an id that is not a
/// UUIDv7 the time its log was created, else its `metadata`'s last
/// activity, else now.
fn tree_month(root: SessionId, dir: &Path) -> Month {
    Month::of_id(root)
        .or_else(|| first_event_time(&dir.join(EVENTS_FILE)).map(Month::of))
        .or_else(|| {
            read_cache(&meta_path(dir), &dir.join(EVENTS_FILE))
                .ok()
                .map(|meta| Month::of(meta.last_activity))
        })
        .unwrap_or_else(|| Month::of(Timestamp::now()))
}

/// The timestamp of the first record in the log at `path`.
fn first_event_time(path: &Path) -> Option<Timestamp> {
    use std::io::BufRead as _;
    let mut first = Vec::new();
    std::io::BufReader::new(fs::File::open(path).ok()?)
        .read_until(b'\n', &mut first)
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&first)
        .ok()?
        .get("timestamp")?
        .as_str()?
        .parse()
        .ok()
}

/// A tree a migration step failed for, which stays where it is until a
/// later start retries it.
fn pending_tree(sessions_dir: &Path, tree: &OldTree, error: String) -> PendingTree {
    let path = tree
        .dir
        .strip_prefix(sessions_dir)
        .unwrap_or(&tree.dir)
        .to_string_lossy()
        .into_owned();
    eprintln!(
        "cookie-agent: session tree {} stays in {path}: {error}",
        tree.root
    );
    PendingTree {
        root_session_id: tree.root,
        path,
        error,
    }
}

/// One conversion step for a tree already in its month folder: takes the
/// tree's lock, converts its logs to this build's history format, and backs
/// up the originals. Returns whether anything changed; an error leaves the
/// tree as it was.
fn convert_in_place(sessions_dir: &Path, tree: &OldTree) -> Result<bool, String> {
    let _lock = match try_acquire(&tree.dir) {
        Ok(SessionOwnership::Owned(lock)) => lock,
        Ok(SessionOwnership::Foreign) => {
            return Err("another cookie process holds the tree".to_owned());
        }
        Err(error) => return Err(format!("its lock does not open: {error}")),
    };
    let conversion =
        convert::convert_tree(tree.root, &tree.dir).map_err(|error| error.to_string())?;
    back_up_converted(sessions_dir, tree);
    Ok(!conversion.converted.is_empty() || conversion.artifacts_moved)
}

/// One migration step: takes the tree's lock, converts its logs to this
/// build's history format, backs up the originals, and renames the root
/// folder into its month. An error leaves the tree where it was.
fn move_tree(sessions_dir: &Path, tree: &OldTree) -> Result<PathBuf, String> {
    let month = tree_month(tree.root, &tree.dir);
    let month_dir = sessions_dir.join(month.to_string());
    let destination = month_dir.join(tree.root.to_string());
    if destination.symlink_metadata().is_ok() {
        return Err(format!("{} already exists", destination.display()));
    }
    let lock = match try_acquire(&tree.dir) {
        Ok(SessionOwnership::Owned(lock)) => lock,
        Ok(SessionOwnership::Foreign) => {
            return Err("another cookie process holds the tree".to_owned());
        }
        Err(error) => return Err(format!("its lock does not open: {error}")),
    };
    convert::convert_tree(tree.root, &tree.dir).map_err(|error| error.to_string())?;
    back_up_converted(sessions_dir, tree);
    // Windows refuses to rename a folder with an open file; the tree is
    // this process's alone until the daemon accepts connections.
    drop(lock);
    #[cfg(windows)]
    {
        let _ = fs::remove_file(crate::ownership::owner_lock_path(&tree.dir));
    }
    #[cfg(unix)]
    create_unix_session_directory_all(&month_dir).map_err(|error| error.to_string())?;
    #[cfg(windows)]
    create_windows_session_directory(&month_dir).map_err(|error| error.to_string())?;
    fs::rename(&tree.dir, &destination).map_err(|error| error.to_string())?;
    Ok(destination)
}

/// Moves the originals of the tree's converted logs (each member's
/// `events.jsonl.v<N>.bak`, also one a conversion a crash cut short left)
/// into `sessions/.backup-v2/<root-id>/`, which the migration deletes once
/// it commits. One that does not move stays beside its log.
fn back_up_converted(sessions_dir: &Path, tree: &OldTree) {
    let mut members = vec![PathBuf::new()];
    members.extend(
        session_dir_ids(&tree.dir.join(SUBAGENTS_DIR))
            .into_iter()
            .map(|child| Path::new(SUBAGENTS_DIR).join(child.to_string())),
    );
    for relative in members {
        let Ok(entries) = fs::read_dir(tree.dir.join(&relative)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let is_backup = name.to_str().is_some_and(|name| {
                name.strip_prefix(&format!("{EVENTS_FILE}.v"))
                    .and_then(|rest| rest.strip_suffix(".bak"))
                    .is_some_and(|format| format.parse::<u32>().is_ok())
            });
            if !is_backup {
                continue;
            }
            let original = entry.path();
            let backup = sessions_dir
                .join(BACKUP_DIR)
                .join(tree.root.to_string())
                .join(&relative);
            let moved = fs::create_dir_all(&backup)
                .and_then(|()| fs::rename(&original, backup.join(&name)));
            if let Err(error) = moved {
                eprintln!(
                    "cookie-agent: the original of {} stays beside it: {error}",
                    original.display()
                );
            }
        }
    }
}

/// Refuses to migrate when the disk cannot hold the logs it converts: a
/// converted log and the blobs it names take about what the original does,
/// which stays until the migration commits.
fn check_free_space<'a>(
    sessions_dir: &Path,
    trees: impl IntoIterator<Item = &'a OldTree>,
) -> Result<(), SessionError> {
    let needed = trees
        .into_iter()
        .flat_map(|tree| {
            let mut logs = vec![tree.dir.join(EVENTS_FILE)];
            logs.extend(
                session_dir_ids(&tree.dir.join(SUBAGENTS_DIR))
                    .into_iter()
                    .map(|child| {
                        tree.dir
                            .join(SUBAGENTS_DIR)
                            .join(child.to_string())
                            .join(EVENTS_FILE)
                    }),
            );
            logs
        })
        .filter(|log| {
            matches!(
                convert::log_format(log),
                Ok(Some(Some(format)))
                    if format != cookie_agent_protocol::HISTORY_FORMAT
                        && crate::events::convert::convertible(format)
            )
        })
        .filter_map(|log| fs::metadata(log).ok().map(|metadata| metadata.len()))
        .sum::<u64>();
    if needed == 0 {
        return Ok(());
    }
    match available_space(sessions_dir) {
        Some(available) if available < needed => Err(SessionError::StoreSpace {
            path: sessions_dir.to_owned(),
            needed,
            available,
        }),
        _ => Ok(()),
    }
}

/// Bytes an unprivileged writer can still use on the filesystem of `path`.
#[cfg(unix)]
fn available_space(path: &Path) -> Option<u64> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string and `stats` is written by
    // the call before it is read.
    let stats = unsafe {
        if libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) != 0 {
            return None;
        }
        stats.assume_init()
    };
    #[allow(clippy::useless_conversion)]
    Some(u64::from(stats.f_bavail).saturating_mul(u64::from(stats.f_frsize)))
}

/// Bytes the calling user can still use on the volume of `path`.
#[cfg(windows)]
fn available_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    let mut available = 0_u64;
    // SAFETY: `wide` is NUL-terminated and outlives the call, which writes
    // only the one counter asked for; the others may be null.
    let succeeded = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (succeeded != 0).then_some(available)
}

/// The list entry of the root in `dir`: its `metadata`, or a fold of its
/// log when that does not read. `None` for a folder that is not a root
/// session at all.
pub(super) fn root_meta(root: SessionId, dir: &Path) -> Option<SessionMeta> {
    let meta = match read_cache(&meta_path(dir), &dir.join(EVENTS_FILE)) {
        Ok(meta) if meta.session_id == root => meta,
        _ => match EventLog::open_read_only(dir.join(EVENTS_FILE), root)
            .map_err(SessionError::from)
            .and_then(projection)
        {
            Ok(projection) => projection.meta,
            Err(error) => {
                eprintln!("cookie-agent: session {root} is not listed: {error}");
                return None;
            }
        },
    };
    matches!(meta.origin, SessionOrigin::Root).then_some(meta)
}

/// Writes `sessions/runtime-revisions.jsonl` from the index it already
/// holds and each work-dir folder's: every record once, and of two that
/// map one runtime revision differently, the first.
fn merge_runtime_revisions(sessions_dir: &Path, work_dirs: &[PathBuf]) -> Result<(), SessionError> {
    let target = sessions_dir.join(RUNTIME_REVISIONS_FILE);
    let sources = std::iter::once(target.clone())
        .chain(
            work_dirs
                .iter()
                .map(|dir| dir.join(V2_RUNTIME_REVISIONS_FILE)),
        )
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    if sources.iter().all(|path| *path == target) {
        return Ok(());
    }
    let mut kept = Vec::<serde_json::Value>::new();
    let mut mapped = HashMap::<String, serde_json::Value>::new();
    for source in &sources {
        let bytes = fs::read(source).map_err(|error| SessionError::Io {
            path: source.clone(),
            source: error,
        })?;
        for line in bytes.split(|byte| *byte == b'\n') {
            let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
                continue;
            };
            let Some(revision) = record.get("runtime_revision").map(ToString::to_string) else {
                continue;
            };
            let model = record
                .get("model_runtime_revision")
                .cloned()
                .unwrap_or_default();
            match mapped.get(&revision) {
                Some(known) if *known != model => continue,
                _ if kept.contains(&record) => continue,
                _ => {
                    mapped.insert(revision, model);
                    kept.push(record);
                }
            }
        }
    }
    let mut bytes = Vec::new();
    for record in &kept {
        serde_json::to_writer(&mut bytes, record).map_err(|source| SessionError::Json {
            path: target.clone(),
            source,
        })?;
        bytes.push(b'\n');
    }
    replace_cache_file(&target, &bytes, CacheSync::Durable)
}

/// Removes what a finished migration or an interrupted deletion left in
/// `sessions/`: deletion tombstones, the backups, staged files, and the
/// v2 work-dir folders no pending tree lives in. Only `sessions/` itself
/// and such leftovers are read. Returns the roots of the tombstones it
/// removed.
fn sweep_top_level(sessions_dir: &Path, pending: &[PendingTree]) -> Vec<SessionId> {
    let deleted = delete::sweep_tombstones(sessions_dir);
    let backup = sessions_dir.join(BACKUP_DIR);
    if backup.exists()
        && let Err(error) = fs::remove_dir_all(&backup)
    {
        eprintln!(
            "cookie-agent: {} could not be removed: {error}",
            backup.display()
        );
    }
    remove_staged_files(sessions_dir);
    for work_dir in old_work_dirs(sessions_dir) {
        let keeps_pending = pending
            .iter()
            .any(|tree| sessions_dir.join(&tree.path).starts_with(&work_dir));
        if !keeps_pending {
            remove_emptied_work_dir(&work_dir);
        }
    }
    deleted
}

/// Removes the temporaries a replaced file staged in `directory` and a
/// crash left behind.
fn remove_staged_files(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && name.ends_with(".tmp") && entry.path().is_file() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Removes a v2 work-dir folder whose trees all moved: the files the v2
/// layout kept there, then the folder. Anything else keeps it, and is named.
fn remove_emptied_work_dir(work_dir: &Path) {
    delete::sweep_tombstones(work_dir);
    remove_staged_files(work_dir);
    let Ok(entries) = fs::read_dir(work_dir) else {
        return;
    };
    let mut kept = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let known = V2_WORK_DIR_FILES.contains(&name.as_str())
            || name.ends_with(crate::ownership::OWNER_LOCK_SUFFIX);
        if known && entry.path().is_file() {
            let _ = fs::remove_file(entry.path());
        } else {
            kept.push(name);
        }
    }
    if kept.is_empty() {
        if let Err(error) = fs::remove_dir(work_dir) {
            eprintln!(
                "cookie-agent: {} could not be removed: {error}",
                work_dir.display()
            );
        }
    } else {
        kept.sort();
        eprintln!(
            "cookie-agent: {} stays: it still holds {}",
            work_dir.display(),
            kept.join(", ")
        );
    }
}
