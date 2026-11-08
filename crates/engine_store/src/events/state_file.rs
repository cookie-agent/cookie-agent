//! `state.json`: what a windowed log's records fold to, saved beside it, so that
//! an open reads only what comes after (`docs/site/specs/forever-sessions.md`
//! section 4).
//!
//! The file names a record, the tip of the log when it was written, and holds
//! what folds give as of that record, and where the window starts. It is a
//! cache: it is written without a sync, and a missing, unreadable, or mismatched
//! one means folding the log from its start, one record at a time and keeping
//! only the window, so what that takes in memory does not depend on the log's
//! length. Only a log that is damaged, or of an older history format, reads the
//! way it always has: whole, as quarantining a record needs.
//!
//! It stays valid across builds. A change to what a fold gives leaves the file's
//! shape alone and its contents wrong, so it must raise [`STATE_VERSION`], and a
//! test holding a digest of what the folds give on the golden logs fails until
//! that happens (section 4.4).

use std::{
    io::{BufRead as _, Read as _, Seek as _},
    sync::atomic::Ordering,
};

use cookie_agent_models::secure_store;

use super::*;
use crate::session::ProjectionFolds;

/// The file beside a session's `events.jsonl`.
pub const STATE_FILE: &str = "state.json";

/// What [`STATE_FILE`]'s contents mean. Raise it when what a fold gives changes,
/// or the file's shape does: a different one means folding the log from its
/// start. The test `the_folds_of_the_golden_logs_match_their_recorded_digest`
/// says when.
pub const STATE_VERSION: u32 = 9;

/// How much of a log comes between two writes of [`STATE_FILE`] on a run's way,
/// so a long run is covered and a turn does not pay for a write.
const WRITE_AFTER_BYTES: u64 = 1024 * 1024;

/// A log shorter than this has no [`STATE_FILE`]: folding it from its start takes
/// a few milliseconds, and the file would be about as large as the log, or larger.
pub const STATE_MIN_LOG_BYTES: u64 = 1024 * 1024;

/// The shortest log a test lets have a [`STATE_FILE`]: none, so that every test's
/// logs go through one, unless a test of the rule sets it.
#[cfg(any(test, feature = "test-support"))]
static TEST_MIN_LOG_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test-only: makes logs shorter than `bytes` have no [`STATE_FILE`] in this
/// process, as [`STATE_MIN_LOG_BYTES`] does in a build without `test-support`.
#[cfg(any(test, feature = "test-support"))]
pub fn set_state_min_log_bytes_for_test(bytes: u64) {
    TEST_MIN_LOG_BYTES.store(bytes, Ordering::Release);
}

fn state_min_log_bytes() -> u64 {
    #[cfg(any(test, feature = "test-support"))]
    {
        TEST_MIN_LOG_BYTES.load(Ordering::Acquire)
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        STATE_MIN_LOG_BYTES
    }
}

/// The record a [`StateFile`] is as of.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
struct StateAt {
    seq: u64,
    timestamp: Timestamp,
    /// Where its line starts in `events.jsonl`.
    offset: u64,
}

/// What [`STATE_FILE`] holds.
#[derive(Clone, Deserialize, Serialize)]
pub struct StateFile {
    version: u32,
    /// The history format of the log it was folded from.
    history_format: u32,
    session_id: SessionId,
    at: StateAt,
    folds: LogFolds,
}

/// What a log's records fold to as of a record: [`EventLog`] without its records,
/// and what a session's projection needs of them.
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct LogFolds {
    /// Where the window starts, which is at or before the record.
    window: Option<u64>,
    /// The window is a native model's context, and `pinned` what it holds
    /// before its start ([`EventStorage::context`]).
    context: bool,
    /// The records a native model's context holds before the window's start,
    /// in order: what follows still names them (section 5.6).
    pinned: Vec<Arc<StoredEvent>>,
    view_start: Option<Box<CacheStartFold>>,
    refused: Arc<Vec<u64>>,
    index: EventIndex,
    physical_model_turns: u64,
    lines: LineIndex,
    raw: StoredRaw,
    validation: ValidationState,
    initial_taint: ValidationTaint,
    diagnostics: Vec<EventLoadDiagnostic>,
    render_only: Option<RenderOnly>,
    next_seq: u64,
    projection: ProjectionFolds,
}

/// [`RawLines`], with what the line index already says left out: every line not
/// named here is in the file, as the record of its sequence.
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
struct StoredRaw {
    /// The lines no record reproduces, by sequence, in base64 as a line need not
    /// be text.
    kept: Vec<(u64, String)>,
    /// Where lines in the file end, for those another line follows that carries
    /// no sequence.
    ends: Vec<(u64, u64)>,
    /// Only the kept lines are known: [`RawLines::partial`].
    partial: bool,
}

impl StoredRaw {
    /// `None` when `raw` does not match `lines`.
    fn of(raw: &RawLines, lines: &LineIndex) -> Option<Self> {
        let mut stored = Self {
            partial: raw.partial,
            ..Self::default()
        };
        for line in &raw.lines {
            match &line.bytes {
                RawBytes::Kept(bytes) => stored.kept.push((line.seq, STANDARD.encode(bytes))),
                RawBytes::InFile { start, end } => {
                    let index = lines
                        .starts
                        .binary_search_by_key(&line.seq, |(seq, _)| *seq)
                        .ok()?;
                    if lines.starts[index].1 != *start {
                        return None;
                    }
                    if *end != lines.line_end(index) {
                        stored.ends.push((line.seq, *end));
                    }
                }
            }
        }
        Some(stored)
    }

    fn restore(self, lines: &LineIndex, path: &Path) -> Option<RawLines> {
        if self.partial {
            return Some(RawLines {
                file: Some(Arc::from(path)),
                lines: self
                    .kept
                    .into_iter()
                    .map(|(seq, bytes)| {
                        Some(RawLine {
                            seq,
                            bytes: RawBytes::Kept(Arc::from(STANDARD.decode(bytes).ok()?)),
                        })
                    })
                    .collect::<Option<_>>()?,
                partial: true,
            });
        }
        let mut kept = self.kept.into_iter().peekable();
        let mut ends = self.ends.into_iter().peekable();
        let mut in_file = false;
        let mut raw = Vec::with_capacity(lines.starts.len());
        for (index, (seq, start)) in lines.starts.iter().enumerate() {
            let bytes = if let Some((_, bytes)) = kept.next_if(|(kept, _)| kept == seq) {
                RawBytes::Kept(Arc::from(STANDARD.decode(bytes).ok()?))
            } else {
                in_file = true;
                RawBytes::InFile {
                    start: *start,
                    end: ends
                        .next_if(|(line, _)| line == seq)
                        .map_or_else(|| lines.line_end(index), |(_, end)| end),
                }
            };
            raw.push(RawLine { seq: *seq, bytes });
        }
        (kept.peek().is_none() && ends.peek().is_none()).then(|| RawLines {
            file: in_file.then(|| Arc::from(path)),
            lines: raw,
            partial: false,
        })
    }
}

impl LineIndex {
    /// Where the line at `index` of [`Self::starts`] ends, newline excluded, when
    /// the next sequenced line, or the file's end, follows it.
    fn line_end(&self, index: usize) -> u64 {
        self.starts
            .get(index + 1)
            .map_or(self.len, |(_, start)| *start)
            .saturating_sub(1)
    }
}

/// What opening a log gave, without reading it whole.
pub(super) struct OpenedLog {
    pub(super) storage: EventStorage,
    pub(super) validation: ValidationState,
    pub(super) initial_taint: ValidationTaint,
    pub(super) diagnostics: Vec<EventLoadDiagnostic>,
    pub(super) next_seq: u64,
    pub(super) torn_tail: bool,
    /// `None` when a usage record could not be applied in place: the log needs a
    /// full fold, as a live append does then.
    pub(super) projection: Option<ProjectionFolds>,
    /// How much of the file the [`StateFile`] it opened from covers.
    pub(super) state_covers: Option<u64>,
}

/// The lines of a file from an offset on.
struct LineCursor {
    reader: io::BufReader<fs::File>,
    /// Where the next line starts.
    position: u64,
    line: Vec<u8>,
}

/// What a [`LineCursor`] found next.
#[derive(Debug, Eq, PartialEq)]
enum Next {
    /// A line, newline included, in `line`.
    Line,
    /// The file ends in part of a line.
    Torn,
    End,
}

impl LineCursor {
    fn open(path: &Path, offset: u64) -> Option<Self> {
        let mut file = fs::File::open(path).ok()?;
        file.seek(io::SeekFrom::Start(offset)).ok()?;
        Some(Self {
            reader: io::BufReader::with_capacity(64 * 1024, file),
            position: offset,
            line: Vec::new(),
        })
    }

    fn next(&mut self) -> Option<Next> {
        self.line.clear();
        let read = self.reader.read_until(b'\n', &mut self.line).ok()?;
        if read == 0 {
            return Some(Next::End);
        }
        if !self.line.ends_with(b"\n") {
            return Some(Next::Torn);
        }
        self.position += read as u64;
        Some(Next::Line)
    }
}

/// A log being read one line at a time, each record through the steps a live
/// append takes: validation, index, projection, and folds, and the window moving
/// on at each committed turn. What the window lets go of is dropped.
struct Folding<'a> {
    path: &'a Path,
    session_id: SessionId,
    storage: EventStorage,
    validation: ValidationState,
    projection: Option<ProjectionFolds>,
    /// Nothing has been folded yet, so the next record starts the projection.
    fresh: bool,
    reader: LineReader<'a>,
}

impl Folding<'_> {
    /// Reads a line, newline excluded, of `length` bytes with it, at `start`.
    /// `None` when the log is not one this can read: damage, or a record that
    /// does not validate.
    fn read(
        &mut self,
        index: usize,
        start: u64,
        line: &[u8],
        length: u64,
        fetch: &mut dyn FnMut(&str) -> io::Result<Vec<u8>>,
    ) -> Option<()> {
        let read = self.reader.read(index, line, fetch).ok()?;
        if read.stop
            || self.reader.render_only.is_some()
            || self.reader.validation_taint != ValidationTaint::default()
        {
            return None;
        }
        self.storage
            .lines
            .as_mut()?
            .push(read.sequenced.map(|(seq, _)| seq), length);
        if let Some((seq, strict)) = read.sequenced {
            let bytes = if strict && read.record.is_some() {
                RawBytes::InFile {
                    start,
                    end: start + line.len() as u64,
                }
            } else {
                RawBytes::Kept(Arc::from(line))
            };
            // A log that let go of its lines keeps only those no record reproduces.
            if !self.storage.raw.partial || matches!(bytes, RawBytes::Kept(_)) {
                Arc::make_mut(&mut self.storage.raw)
                    .lines
                    .push(RawLine { seq, bytes });
            }
        }
        let Some(record) = read.record else {
            return Some(());
        };
        let record = Arc::new(record);
        validate_record_incremental(
            self.path,
            self.session_id,
            &record,
            &mut self.validation,
            false,
        )
        .ok()?;
        self.storage.push(Arc::clone(&record));
        if std::mem::take(&mut self.fresh) {
            self.projection = ProjectionFolds::of_creation(&record, None);
        }
        if self
            .projection
            .as_mut()
            .is_some_and(|projection| !projection.observe_read(&record))
        {
            self.projection = None;
        }
        self.storage.narrow_after(&record);
        Some(())
    }

    fn observed_tip(&self) -> u64 {
        self.reader.last_observed_seq.max(
            self.reader
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.skipped)
                .map(|diagnostic| diagnostic.seq)
                .max()
                .unwrap_or(0),
        )
    }
}

/// A file that ended in part of a record: an owned open cuts it, as the loader does.
fn settle_torn_tail(
    path: &Path,
    cursor: &LineCursor,
    next: &Next,
    torn_tail: TornTail,
) -> Option<bool> {
    let torn = *next == Next::Torn;
    if torn && matches!(torn_tail, TornTail::Truncate) {
        cut_event_file(path, cursor.position).ok()?;
    }
    Some(torn)
}

/// Opens the log at `path` from its [`STATE_FILE`] and the log from the window's
/// start: the records up to the file's join the window as they are, and each
/// after it goes through the steps of a live append. `None` on any mismatch, for
/// the caller to fold the log from its start. Nothing is changed by then, so a
/// crash anywhere in this costs nothing.
pub(super) fn open_from_state(
    path: &Path,
    session_id: SessionId,
    torn_tail: TornTail,
) -> Option<OpenedLog> {
    let directory = path.parent()?;
    let bytes = secure_store::read_replaced(&directory.join(STATE_FILE)).ok()?;
    let file = serde_json::from_slice::<StateFile>(&bytes).ok()?;
    drop(bytes);
    if file.version != STATE_VERSION
        || file.history_format != cookie_agent_protocol::HISTORY_FORMAT
        || file.session_id != session_id
        || file.folds.render_only.is_some()
    {
        return None;
    }
    let StateFile { at, folds, .. } = file;
    let blob_dir = blobs::tree_blob_dir(path);
    let mut fetch = |digest: &str| blobs::read(&blob_dir, digest);
    let (creation, start, first_seq) =
        window_start(path, session_id, folds.window, &folds.lines, &mut fetch)?;
    let mut cursor = LineCursor::open(path, start)?;
    // Up to the record the file names, whole, and at the offset it names. A
    // record the file says validation refused stays out, as a pass leaves it out.
    let clean = !folds
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.skipped);
    let mut read = LineReader::new(path);
    read.last_observed_seq = first_seq.saturating_sub(1);
    let mut held = Vec::new();
    loop {
        let line_start = cursor.position;
        if line_start > at.offset || cursor.next()? != Next::Line {
            return None;
        }
        let line = &cursor.line[..cursor.line.len() - 1];
        if line.is_empty() {
            continue;
        }
        let index = usize::from(line_start != 0);
        let line = read.read(index, line, &mut fetch).ok()?;
        if line.stop || (clean && read.diagnostics.iter().any(|noted| noted.skipped)) {
            return None;
        }
        if let Some((seq, _)) = line.sequenced
            && folds
                .lines
                .starts
                .binary_search(&(seq, line_start))
                .is_err()
        {
            return None;
        }
        let at_record = line_start == at.offset;
        let Some(record) = line.record else {
            if at_record {
                return None;
            }
            continue;
        };
        if at_record && (record.seq != at.seq || record.timestamp != at.timestamp) {
            return None;
        }
        if folds.refused.binary_search(&record.seq).is_err() {
            held.push(Arc::new(record));
        }
        if at_record {
            break;
        }
    }
    if cursor.position != folds.lines.len {
        return None;
    }
    drop(read);
    let creation = creation.or_else(|| held.first().cloned())?;
    let covered = folds.lines.len;
    let raw = folds.raw.restore(&folds.lines, path)?;
    if folds.pinned.iter().any(|pinned| pinned.seq >= first_seq)
        || !folds.pinned.is_sorted_by_key(|pinned| pinned.seq)
        || (!folds.context && !folds.pinned.is_empty())
    {
        return None;
    }
    let mut all = folds.pinned;
    all.extend(held);
    let storage = EventStorage {
        all: Arc::new(all),
        creation: Some(creation),
        window: folds.window,
        context: folds.context,
        view_start: folds.view_start,
        refused: folds.refused,
        index: folds.index,
        physical_model_turns: folds.physical_model_turns,
        lines: Some(folds.lines),
        raw: Arc::new(raw),
    };
    let mut reader = LineReader::new(path);
    reader.last_observed_seq = at.seq;
    let mut folding = Folding {
        path,
        session_id,
        storage,
        validation: folds.validation,
        projection: Some(folds.projection),
        fresh: false,
        reader,
    };
    let next = loop {
        let line_start = cursor.position;
        let next = cursor.next()?;
        if next != Next::Line {
            break next;
        }
        let length = cursor.line.len() as u64;
        let line = &cursor.line[..cursor.line.len() - 1];
        if line.is_empty() {
            folding.storage.lines.as_mut()?.push(None, length);
            continue;
        }
        folding.read(1, line_start, line, length, &mut fetch)?;
        // The loader would repair this line or skip it, and report that.
        if !folding.reader.diagnostics.is_empty() {
            return None;
        }
    };
    let torn = settle_torn_tail(path, &cursor, &next, torn_tail)?;
    let next_seq = folds.next_seq.max(folding.reader.last_observed_seq + 1);
    let mut projection = folding.projection;
    if let Some(projection) = &mut projection {
        projection.meta.last_event_seq = next_seq - 1;
    }
    Some(OpenedLog {
        storage: folding.storage,
        validation: folding.validation,
        initial_taint: folds.initial_taint,
        diagnostics: folds.diagnostics,
        next_seq,
        torn_tail: torn,
        projection,
        state_covers: Some(covered),
    })
}

/// Where a pass of a window read from `lines` starts: the log's first record
/// when the window does not hold it, the offset of the window's first line, and
/// its sequence. With no window it reads the log from its start.
#[allow(clippy::type_complexity)]
fn window_start(
    path: &Path,
    session_id: SessionId,
    window: Option<u64>,
    lines: &LineIndex,
    fetch: &mut dyn FnMut(&str) -> io::Result<Vec<u8>>,
) -> Option<(Option<Arc<StoredEvent>>, u64, u64)> {
    let Some(window) = window else {
        return Some((None, 0, 0));
    };
    // A native model's context starts at a sequence, which need not be a record's.
    let index = lines.starts.partition_point(|(seq, _)| *seq < window);
    let (first, start) = *lines.starts.get(index)?;
    let mut cursor = LineCursor::open(path, 0)?;
    let creation = loop {
        if cursor.next()? != Next::Line {
            return None;
        }
        let line = &cursor.line[..cursor.line.len() - 1];
        if !line.is_empty() {
            let mut reader = LineReader::new(path);
            break reader.read(0, line, fetch).ok()?.record?;
        }
    };
    if creation.session_id != session_id {
        return None;
    }
    // The byte before the window's line ends the line before it.
    let mut before = [0_u8];
    let mut file = fs::File::open(path).ok()?;
    file.seek(io::SeekFrom::Start(start.checked_sub(1)?)).ok()?;
    file.read_exact(&mut before).ok()?;
    (before == *b"\n").then_some((Some(Arc::new(creation)), start, first))
}

/// Folds the log at `path` from its start, one record at a time, holding what the
/// window holds and no more. `None` when the log is damaged or of an older format,
/// which only the whole read deals with.
pub(super) fn fold_file(
    path: &Path,
    session_id: SessionId,
    torn_tail: TornTail,
) -> Option<OpenedLog> {
    let blob_dir = blobs::tree_blob_dir(path);
    let mut fetch = |digest: &str| blobs::read(&blob_dir, digest);
    let mut cursor = LineCursor::open(path, 0)?;
    let mut storage = EventStorage::new(Vec::new());
    storage.lines = Some(LineIndex::default());
    let mut folding = Folding {
        path,
        session_id,
        storage,
        validation: ValidationState::default(),
        projection: None,
        fresh: true,
        reader: LineReader::new(path),
    };
    let mut index = 0;
    let next = loop {
        let line_start = cursor.position;
        let next = cursor.next()?;
        if next != Next::Line {
            break next;
        }
        let length = cursor.line.len() as u64;
        let line = &cursor.line[..cursor.line.len() - 1];
        if line.is_empty() {
            folding.storage.lines.as_mut()?.push(None, length);
            continue;
        }
        folding.read(index, line_start, line, length, &mut fetch)?;
        index += 1;
    };
    folding.storage.creation.as_ref()?;
    let torn = settle_torn_tail(path, &cursor, &next, torn_tail)?;
    #[cfg(any(test, feature = "test-support"))]
    EVENT_LOG_PARSES.with(|parses| parses.set(parses.get() + 1));
    let next_seq = folding.observed_tip().saturating_add(1).max(1);
    let diagnostics = folding.reader.diagnostics;
    let mut projection = folding.projection;
    if let Some(projection) = &mut projection {
        projection.finish_read(next_seq - 1, &diagnostics);
    }
    let raw = Arc::make_mut(&mut folding.storage.raw);
    if raw
        .lines
        .iter()
        .any(|line| matches!(line.bytes, RawBytes::InFile { .. }))
    {
        raw.file = Some(Arc::from(path));
    }
    Some(OpenedLog {
        storage: folding.storage,
        validation: folding.validation,
        initial_taint: ValidationTaint::default(),
        diagnostics,
        next_seq,
        torn_tail: torn,
        projection,
        state_covers: None,
    })
}

impl EventLog {
    /// How much of the log came since [`STATE_FILE`] was last written, if this log
    /// should have one: it has a window, an ACP agent's or a native model's
    /// context (sections 4.2 and 5.6), a file this process owns, and at least
    /// [`STATE_MIN_LOG_BYTES`] of it. A native log that holds itself whole, having
    /// no checkpoint, has none.
    #[must_use]
    pub fn state_behind(&self) -> Option<u64> {
        if self.read_only || self.converted || self.render_only.is_some() || !self.is_persisted() {
            return None;
        }
        let storage = self.storage();
        storage.window?;
        if storage.index.window_start.is_none() && !storage.context {
            return None;
        }
        let len = storage.lines.as_ref()?.len;
        if len < state_min_log_bytes() {
            return None;
        }
        Some(len.saturating_sub(self.state_written.load(Ordering::Acquire)))
    }

    /// Whether a committed turn should write [`STATE_FILE`] now.
    #[must_use]
    pub fn state_due_after_turn(&self) -> bool {
        self.state_behind()
            .is_some_and(|behind| behind > WRITE_AFTER_BYTES)
    }

    /// What [`STATE_FILE`] holds as of this log's tip, with `projection`, this
    /// log's. `None` when this log should not have one, or its layout or tip do
    /// not allow it: the tip is not a record it holds, or a sequence jumped
    /// without a record of damage, which a parse would find and this log did not.
    /// The caller holds off appends, so `projection` is of the same moment.
    #[must_use]
    pub fn state_file(&self, projection: &ProjectionFolds) -> Option<StateFile> {
        self.state_behind()?;
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let storage = self.storage();
        let lines = storage.lines.as_ref()?;
        let tip = storage.all.last()?;
        let (last, offset) = *lines.starts.last()?;
        let next_seq = self.next_seq.load(Ordering::Acquire);
        if last != tip.seq || next_seq != tip.seq + 1 || projection.meta.last_event_seq != tip.seq {
            return None;
        }
        let initial_taint = self.initial_taint();
        let unexplained = lines
            .gaps
            .iter()
            .any(|gap| !initial_taint.broad.contains(gap));
        if unexplained {
            return None;
        }
        Some(StateFile {
            version: STATE_VERSION,
            history_format: cookie_agent_protocol::HISTORY_FORMAT,
            session_id: self.session_id,
            at: StateAt {
                seq: tip.seq,
                timestamp: tip.timestamp,
                offset,
            },
            folds: LogFolds::of(
                &storage,
                self.validation
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
                initial_taint,
                self.diagnostics.clone(),
                self.render_only.clone(),
                next_seq,
                projection.clone(),
            )?,
        })
    }

    /// Notes that `state`, this log's, is now in [`STATE_FILE`].
    pub fn note_state_written(&self, state: &StateFile) {
        self.state_written
            .store(state.folds.lines.len, Ordering::Release);
    }
}

impl StateFile {
    /// The file's bytes. Two equal states give the same ones.
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

impl LogFolds {
    fn of(
        storage: &EventStorage,
        validation: ValidationState,
        initial_taint: ValidationTaint,
        diagnostics: Vec<EventLoadDiagnostic>,
        render_only: Option<RenderOnly>,
        next_seq: u64,
        projection: ProjectionFolds,
    ) -> Option<Self> {
        let lines = storage.lines.clone()?;
        Some(Self {
            window: storage.window,
            context: storage.context,
            pinned: storage.all[..storage.window_position()].to_vec(),
            view_start: storage.view_start.clone(),
            refused: Arc::clone(&storage.refused),
            index: storage.index.clone(),
            physical_model_turns: storage.physical_model_turns,
            raw: StoredRaw::of(&storage.raw, &lines)?,
            lines,
            validation,
            initial_taint,
            diagnostics,
            render_only,
            next_seq,
            projection,
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
mod reference;

#[cfg(test)]
mod tests;
