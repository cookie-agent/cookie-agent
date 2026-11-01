//! The monthly activity journals, `sessions/activity/<yyyy-mm>.jsonl`
//! (`docs/site/specs/session-storage-v3.md` section 4.3): the list entries
//! of root sessions, appended when one is created, changes its title,
//! project, or status, or is deleted, and never for every event. A listing
//! reads them newest month first instead of every session's `metadata`.

use jiff::Timestamp;

use super::layout::{ACTIVITY_DIR, Month, month_dirs, month_root_ids, root_meta};
use super::*;

/// One line of a journal: a root's list entry, or its deletion.
#[derive(Clone, Debug)]
pub(super) enum Entry {
    Listed(Box<SessionMeta>),
    Deleted,
}

/// A deletion's line.
#[derive(serde::Serialize)]
struct Tombstone {
    session_id: SessionId,
    deleted: bool,
}

/// One month's journal as read: each session's last line.
#[derive(Debug, Default)]
pub(super) struct MonthEntries(HashMap<SessionId, Entry>);

impl MonthEntries {
    fn parse(bytes: &[u8]) -> Self {
        let mut entries = HashMap::new();
        for line in bytes.split(|byte| *byte == b'\n') {
            if let Some((id, entry)) = parse_line(line) {
                entries.insert(id, entry);
            }
        }
        Self(entries)
    }
}

/// A journal line, or `None` for one that does not read (a torn final line,
/// or an entry an incompatible build wrote).
fn parse_line(line: &[u8]) -> Option<(SessionId, Entry)> {
    let value = serde_json::from_slice::<serde_json::Value>(line).ok()?;
    if value.get("deleted") == Some(&serde_json::Value::Bool(true)) {
        let id = value.get("session_id")?.as_str()?.parse().ok()?;
        return Some((id, Entry::Deleted));
    }
    let meta = serde_json::from_value::<SessionMeta>(value).ok()?;
    Some((meta.session_id, Entry::Listed(Box::new(meta))))
}

/// The session id a journal line names, without decoding the rest.
fn line_session(line: &[u8]) -> Option<SessionId> {
    #[derive(serde::Deserialize)]
    struct Named {
        session_id: SessionId,
    }
    serde_json::from_slice::<Named>(line)
        .ok()
        .map(|named| named.session_id)
}

fn journal_name(month: Month) -> String {
    format!("{month}.jsonl")
}

/// The journals of one store, with the current and the previous month's
/// held in memory.
#[derive(Debug)]
pub(super) struct ActivityJournals {
    dir: PathBuf,
    /// Months in memory, each with the journal's length when it was read or
    /// last appended to here: a journal of another length was written by
    /// another process, and is read again.
    cache: Mutex<BTreeMap<Month, (Arc<MonthEntries>, u64)>>,
}

impl ActivityJournals {
    /// The journals under `sessions_dir`. Reads nothing: a month is read
    /// when a listing first needs it.
    pub(super) fn open(sessions_dir: &Path) -> Self {
        Self {
            dir: sessions_dir.join(ACTIVITY_DIR),
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    fn path(&self, month: Month) -> PathBuf {
        self.dir.join(journal_name(month))
    }

    /// The months that have a journal, newest first.
    pub(super) fn months(&self) -> Vec<Month> {
        let mut months = fs::read_dir(&self.dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| {
                        Month::parse(entry.file_name().to_str()?.strip_suffix(".jsonl")?)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        months.sort_unstable_by(|left, right| right.cmp(left));
        months
    }

    /// One month's entries: from memory for the current and the previous
    /// month while their journals are as this process last saw them.
    pub(super) fn month(&self, month: Month) -> Arc<MonthEntries> {
        let path = self.path(month);
        let oldest_held = Month::of(Timestamp::now()).previous();
        if month < oldest_held {
            return Arc::new(read_journal(&path).0);
        }
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.retain(|held, _| *held >= oldest_held);
        let length = fs::metadata(&path).map_or(0, |metadata| metadata.len());
        if let Some((entries, held)) = cache.get(&month)
            && *held == length
        {
            return Arc::clone(entries);
        }
        let (entries, length) = read_journal(&path);
        let entries = Arc::new(entries);
        cache.insert(month, (Arc::clone(&entries), length));
        entries
    }

    /// Appends `meta`, a root's list entry, to this month's journal.
    pub(super) fn record(&self, meta: &SessionMeta) {
        match serde_json::to_vec(meta) {
            Ok(line) => self.append(meta.session_id, Entry::Listed(Box::new(meta.clone())), line),
            Err(error) => eprintln!("session {} activity entry: {error}", meta.session_id),
        }
    }

    /// Appends the deletion of the root `id` to this month's journal.
    pub(super) fn record_deleted(&self, id: SessionId) {
        let tombstone = Tombstone {
            session_id: id,
            deleted: true,
        };
        match serde_json::to_vec(&tombstone) {
            Ok(line) => self.append(id, Entry::Deleted, line),
            Err(error) => eprintln!("session {id} activity entry: {error}"),
        }
    }

    /// Appends one line. The first write of a month compacts the previous
    /// month's journal first. A failure is reported and loses only this
    /// entry: the session's own log and `metadata` are untouched.
    fn append(&self, id: SessionId, entry: Entry, mut line: Vec<u8>) {
        let month = Month::of(Timestamp::now());
        let path = self.path(month);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = (|| -> std::io::Result<()> {
            let before = match fs::metadata(&path) {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    create_journal_dir(&self.dir)?;
                    let previous = self.path(month.previous());
                    if previous.exists() {
                        // Compaction only saves space: a failure keeps the
                        // whole journal, which reads the same.
                        if let Err(error) = compact(&previous) {
                            eprintln!(
                                "activity journal {} was not compacted: {error}",
                                previous.display()
                            );
                        }
                        cache.remove(&month.previous());
                    }
                    0
                }
                Err(error) => return Err(error),
            };
            line.push(b'\n');
            append_line(&path, &line)?;
            let after = before + line.len() as u64;
            match cache.get_mut(&month) {
                Some((entries, held)) if *held == before => {
                    entries_mut(entries).0.insert(id, entry);
                    *held = after;
                }
                Some(_) => {
                    cache.remove(&month);
                }
                None if before == 0 => {
                    let mut entries = MonthEntries::default();
                    entries.0.insert(id, entry);
                    cache.insert(month, (Arc::new(entries), after));
                }
                None => {}
            }
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!(
                "session {id} activity entry was not written to {}: {error}",
                path.display()
            );
        }
    }
}

/// Copy-on-write for a month's entries a listing may still hold.
fn entries_mut(entries: &mut Arc<MonthEntries>) -> &mut MonthEntries {
    if Arc::get_mut(entries).is_none() {
        *entries = Arc::new(MonthEntries(entries.0.clone()));
    }
    Arc::get_mut(entries).expect("a fresh copy has one owner")
}

/// A journal's entries and the length read, empty when there is none.
fn read_journal(path: &Path) -> (MonthEntries, u64) {
    match fs::read(path) {
        Ok(bytes) => (MonthEntries::parse(&bytes), bytes.len() as u64),
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("activity journal {} does not read: {error}", path.display());
            }
            (MonthEntries::default(), 0)
        }
    }
}

fn create_journal_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    let created = create_unix_session_directory_all(dir);
    #[cfg(windows)]
    let created = create_windows_session_directory(dir);
    created.map_err(|error| match error {
        SessionError::Io { source, .. } => source,
        error => std::io::Error::other(error.to_string()),
    })
}

/// Appends `bytes`, whole lines, to the private file at `path` in one
/// write. Not synced: a journal is a listing aid the sessions' own files
/// can rebuild, and an append must not wait on the disk.
fn append_line(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(windows)]
    if !path.exists() {
        match cookie_agent_models::secure_store::create_windows_private_file(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)?.write_all(bytes)
}

/// Rewrites a journal keeping each session's last line, in the order of
/// those lines.
fn compact(path: &Path) -> std::io::Result<()> {
    let bytes = fs::read(path)?;
    let lines = bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| Some((line_session(line)?, line)))
        .collect::<Vec<_>>();
    let mut last = HashMap::new();
    for (index, (id, _)) in lines.iter().enumerate() {
        last.insert(*id, index);
    }
    let mut compacted = Vec::with_capacity(bytes.len());
    for (index, (id, line)) in lines.iter().enumerate() {
        if last.get(id) == Some(&index) {
            compacted.extend_from_slice(line);
            compacted.push(b'\n');
        }
    }
    replace_cache_file(path, &compacted, CacheSync::Durable).map_err(|error| match error {
        SessionError::Io { source, .. } => source,
        error => std::io::Error::other(error.to_string()),
    })
}

fn journal_bytes(entries: &[SessionMeta]) -> Result<Vec<u8>, SessionError> {
    let mut bytes = Vec::new();
    for meta in entries {
        serde_json::to_writer(&mut bytes, meta).map_err(|source| SessionError::Json {
            path: PathBuf::from(ACTIVITY_DIR),
            source,
        })?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

/// Writes every journal anew from the `metadata` of each root filed in a
/// month folder, each entry in the journal of its last activity's month.
/// The first migration runs it before its commit point, so a crash before
/// that only has the next start write them again.
pub(super) fn rebuild_journals(sessions_dir: &Path) -> Result<(), SessionError> {
    let dir = sessions_dir.join(ACTIVITY_DIR);
    let mut by_month = BTreeMap::<Month, Vec<SessionMeta>>::new();
    for month in month_dirs(sessions_dir) {
        for root in month_root_ids(sessions_dir, month) {
            let root_dir = sessions_dir.join(month.to_string()).join(root.to_string());
            if let Some(meta) = root_meta(root, &root_dir) {
                by_month
                    .entry(Month::of(meta.last_activity))
                    .or_default()
                    .push(meta);
            }
        }
    }
    create_journal_dir(&dir).map_err(|source| SessionError::Io {
        path: dir.clone(),
        source,
    })?;
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let stale = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".jsonl"))
                .and_then(Month::parse)
                .is_some_and(|month| !by_month.contains_key(&month));
            if stale {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    for (month, mut entries) in by_month {
        entries.sort_by_key(|meta| meta.last_activity);
        replace_cache_file(
            &dir.join(journal_name(month)),
            &journal_bytes(&entries)?,
            CacheSync::Durable,
        )?;
    }
    fsync_directory(&dir)?;
    Ok(())
}

/// Appends the entries of roots a retried migration step moved, each to
/// the journal of its last activity's month, and syncs them before the
/// commit point that stops retrying them.
pub(super) fn append_migrated(
    sessions_dir: &Path,
    entries: &[SessionMeta],
) -> Result<(), SessionError> {
    if entries.is_empty() {
        return Ok(());
    }
    let dir = sessions_dir.join(ACTIVITY_DIR);
    create_journal_dir(&dir).map_err(|source| SessionError::Io {
        path: dir.clone(),
        source,
    })?;
    let mut by_month = BTreeMap::<Month, Vec<SessionMeta>>::new();
    for meta in entries {
        by_month
            .entry(Month::of(meta.last_activity))
            .or_default()
            .push(meta.clone());
    }
    for (month, entries) in by_month {
        let path = dir.join(journal_name(month));
        let io_error = |source| SessionError::Io {
            path: path.clone(),
            source,
        };
        append_line(&path, &journal_bytes(&entries)?).map_err(io_error)?;
        // Opened for writing: Windows refuses to flush a read-only handle.
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|file| file.sync_all())
            .map_err(io_error)?;
    }
    fsync_directory(&dir)?;
    Ok(())
}

/// Which root sessions a `session.list` answers with (client protocol
/// `active_since` and `before`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ListWindow {
    /// Only roots last active at or after this time.
    pub active_since: Option<Timestamp>,
    /// Only roots last active before this time: the `older` cursor of the
    /// previous answer. Without `active_since`, the answer reaches back to
    /// the start of the newest month that has older entries.
    pub before: Option<Timestamp>,
}

/// One window of a listing, before the pinned roots join it.
#[derive(Default)]
struct Page {
    listed: HashMap<SessionId, SessionMeta>,
    /// Each session's newest entry in the journals read.
    seen: HashMap<SessionId, Entry>,
    older: Option<Timestamp>,
}

/// A listing of root sessions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RootList {
    pub sessions: Vec<SessionMeta>,
    /// The cursor of the next older page, or `None` when nothing is older.
    pub older: Option<Timestamp>,
}

impl SessionStore {
    /// The root sessions last active within `window`, and every root that is
    /// unread or in `busy` whatever its age (section 4.3). Entries come from
    /// the activity journals, newest month first; a root this process holds
    /// in memory answers from its projection; a pinned root no journal read
    /// so far names is read from its own `metadata`. The default window
    /// lists every root.
    #[must_use]
    pub fn list_roots_in(&self, window: ListWindow, busy: &HashSet<SessionId>) -> RootList {
        crate::blocking::off_worker(|| self.list_roots_in_place(window, busy))
    }

    /// Every root session.
    #[must_use]
    pub fn list_roots(&self) -> Vec<SessionMeta> {
        self.list_roots_in(ListWindow::default(), &HashSet::new())
            .sessions
    }

    fn list_roots_in_place(&self, window: ListWindow, busy: &HashSet<SessionId>) -> RootList {
        let months = self.activity.months();
        // Roots created here can be in memory before their folder, and their
        // first entry, exist.
        let resident = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .values()
            .filter(|session| matches!(session.meta.origin, SessionOrigin::Root))
            .map(|session| (session.meta.session_id, session.meta.clone()))
            .collect::<HashMap<_, _>>();
        let mut page = match (window.active_since, window.before) {
            (None, None) => self.window_page(&months, &resident, None, None),
            (Some(since), before) => self.window_page(&months, &resident, Some(since), before),
            // A page of older sessions reaches back to the start of the
            // newest month that holds any, passing over months that hold
            // none.
            (None, Some(mut before)) => loop {
                let Some(month) = months.iter().find(|month| month.start() < before) else {
                    break Page::default();
                };
                let page = self.window_page(&months, &resident, Some(month.start()), Some(before));
                match page.older {
                    Some(older) if page.listed.is_empty() => before = older,
                    _ => break page,
                }
            },
        };
        let pinned = self
            .unread
            .ids()
            .into_iter()
            .chain(busy.iter().copied())
            .collect::<HashSet<_>>();
        for id in pinned {
            if page.listed.contains_key(&id) {
                continue;
            }
            let meta = match (resident.get(&id), page.seen.get(&id)) {
                (Some(meta), _) => Some(meta.clone()),
                (None, Some(Entry::Listed(meta))) => Some((**meta).clone()),
                (None, Some(Entry::Deleted)) => None,
                (None, None) => self.pinned_root_meta(id),
            };
            if let Some(meta) = meta {
                page.listed.insert(id, meta);
            }
        }
        let mut sessions = page.listed.into_values().collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            right
                .last_activity
                .cmp(&left.last_activity)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        RootList {
            sessions,
            older: page.older,
        }
    }

    /// The roots last active in `[low, high)`, from the journals of the
    /// months that reach `low` (newest first, so a session's newest entry is
    /// the one kept) and the resident roots, and whether anything is older.
    fn window_page(
        &self,
        months: &[Month],
        resident: &HashMap<SessionId, SessionMeta>,
        low: Option<Timestamp>,
        high: Option<Timestamp>,
    ) -> Page {
        let floor = low.map(Month::of);
        let mut seen = HashMap::<SessionId, Entry>::new();
        for month in months
            .iter()
            .filter(|month| floor.is_none_or(|floor| **month >= floor))
        {
            for (id, entry) in &self.activity.month(*month).0 {
                seen.entry(*id).or_insert_with(|| entry.clone());
            }
        }
        let in_window = |meta: &SessionMeta| {
            low.is_none_or(|low| meta.last_activity >= low)
                && high.is_none_or(|high| meta.last_activity < high)
        };
        let mut listed = HashMap::<SessionId, SessionMeta>::new();
        for (id, entry) in &seen {
            if let Entry::Listed(meta) = entry {
                let meta = resident
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| (**meta).clone());
                if in_window(&meta) {
                    listed.insert(*id, meta);
                }
            }
        }
        for (id, meta) in resident {
            if !listed.contains_key(id)
                && !matches!(seen.get(id), Some(Entry::Deleted))
                && in_window(meta)
            {
                listed.insert(*id, meta.clone());
            }
        }
        let older = low.filter(|low| {
            months.iter().any(|month| Month::of(*low) > *month)
                || seen
                    .values()
                    .any(|entry| matches!(entry, Entry::Listed(meta) if meta.last_activity < *low))
        });
        Page {
            listed,
            seen,
            older,
        }
    }

    /// A pinned root's entry from its own `metadata`: one small read, and
    /// none for a root that is gone.
    fn pinned_root_meta(&self, id: SessionId) -> Option<SessionMeta> {
        let dir = self.root_path(id);
        match self.read_meta(id, &dir) {
            Ok(meta) if matches!(meta.origin, SessionOrigin::Root) => {
                self.record_location(id, SessionLocation::Root);
                Some(meta)
            }
            Ok(_) => None,
            Err(SessionError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                None
            }
            Err(error) => {
                eprintln!("session {id} skipped: {error}");
                None
            }
        }
    }

    /// Journals a root's list entry now. Called where its title, project,
    /// status, or attachments change, when it is first published, and when
    /// a root whose later events were not journaled leaves memory.
    pub(super) fn journal_root(&self, meta: &SessionMeta) {
        if !matches!(meta.origin, SessionOrigin::Root) {
            return;
        }
        self.residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unjournaled
            .remove(&meta.session_id);
        self.activity.record(meta);
    }

    /// Journals the deletion of the root `id`.
    pub(super) fn journal_deleted(&self, id: SessionId) {
        self.activity.record_deleted(id);
    }

    /// Journals `id` if it is a persisted root whose resident projection
    /// changed since its last entry, as before it leaves memory: the list
    /// then shows the sequence a delete is checked against.
    pub(super) fn journal_if_behind(&self, id: SessionId) {
        let meta = {
            let mut residency = self
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !residency.unjournaled.remove(&id) {
                return;
            }
            match residency.resident.get(&id) {
                Some(session) if session.log.is_persisted() => session.meta.clone(),
                _ => return,
            }
        };
        self.activity.record(&meta);
    }
}
