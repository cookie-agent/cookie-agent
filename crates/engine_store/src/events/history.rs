//! A pass over a log's records (`docs/site/specs/forever-sessions.md` section
//! 3.3), for a reader whose answer can lie anywhere in the log: one record at a
//! time, keeping nothing, so that its memory does not depend on the log's length.
//!
//! Its time does: a pass is a read of the log, once a log no longer holds its
//! events. So a reader keeps only what it needs, and a question asked every turn
//! belongs in a fold ([`super::LogState`]).

use std::{
    collections::VecDeque,
    fs::File,
    io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Take},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use cookie_agent_protocol::{
    EventsView, StoredEvent,
    display_view::{self, CacheBoundary, OlderPageReader, TailWindowReader, Window},
};

use super::{EventLogError, EventSnapshot, LineReader, blobs};

/// How much of the file a pass reads ahead on each visit off the async worker,
/// so a pass over small records does not leave the worker once per record. A
/// larger line is read whole, alone.
const READ_AHEAD_BYTES: usize = 64 * 1024;

/// The records of a log as it stood when the pass was asked for, in order. It
/// holds no lock of the log, and may be read for as long as it takes.
///
/// Each record is fallible: it is read when it is asked for, so damage in old
/// records is found here (section 11, decision 1). Nothing else may be assumed
/// of where the records come from: while a log holds all its events this reads
/// them from there, and it is the only way to read past a log's window.
///
/// What precedes the window is read from the file, which the pass is bounded in
/// at the window it saw, so what the log appends later does not reach it. A
/// revert that cuts the file meanwhile ends the pass with
/// [`EventLogError::HistoryCut`]: the records it asked for are gone, and the
/// sequences they had may name others by then. What it already gave stays true
/// of the log as it stood.
#[derive(Debug)]
pub struct History {
    /// The log's first record, while the pass has yet to give it and it is not
    /// among `events`.
    creation: Option<Arc<StoredEvent>>,
    /// What the pass has left to read from the file.
    file: Option<FilePass>,
    /// What it read of the file and has not given yet.
    read_ahead: VecDeque<Result<Arc<StoredEvent>, EventLogError>>,
    events: EventSnapshot,
    next: usize,
    end: usize,
}

/// Where in a log's file a pass reads, as a log worked it out from its window.
#[derive(Clone, Debug)]
pub(super) struct FileRange {
    pub(super) path: PathBuf,
    /// Where the first line of the range starts, or an earlier one.
    pub(super) offset: u64,
    /// Where the range's lines end, when the log knows its file's layout.
    pub(super) end: Option<u64>,
    /// The sequences of the records it gives: from one, up to and not including
    /// the other.
    pub(super) from_seq: u64,
    pub(super) until_seq: u64,
    /// Records of the file the log left out, which a pass leaves out too.
    pub(super) refused: Arc<Vec<u64>>,
    /// The log's count of the cuts to its file, and what it was when asked.
    pub(super) cuts: Arc<AtomicU64>,
    pub(super) seen_cuts: u64,
    /// The load noted no damage in the file: one it did not read, as a log that
    /// opened from its `state.json` did not, is found here (section 11, decision
    /// 1). A log that knows of damage skips it as its load did.
    pub(super) undamaged: bool,
    /// A read-only log's first held record: its owner can cut the file with no
    /// word to this process, so a pass checks the line is still there at its end.
    pub(super) held_from: Option<HeldFrom>,
}

/// Where a read-only log's window starts in its file, and the record there.
#[derive(Clone, Copy, Debug)]
pub(super) struct HeldFrom {
    pub(super) offset: u64,
    pub(super) seq: u64,
    pub(super) timestamp: jiff::Timestamp,
}

impl HeldFrom {
    /// Whether the file at `path` still holds the record, whole, where it was.
    fn still_there(&self, path: &std::path::Path) -> bool {
        #[derive(serde::Deserialize)]
        struct Line {
            seq: u64,
            timestamp: jiff::Timestamp,
        }
        let read = || -> std::io::Result<Vec<u8>> {
            let mut file = File::open(path)?;
            file.seek(SeekFrom::Start(self.offset))?;
            let mut line = Vec::new();
            BufReader::new(file).read_until(b'\n', &mut line)?;
            Ok(line)
        };
        read().is_ok_and(|line| {
            line.ends_with(b"\n")
                && serde_json::from_slice::<Line>(&line)
                    .is_ok_and(|line| line.seq == self.seq && line.timestamp == self.timestamp)
        })
    }
}

/// A [`FileRange`] being read.
#[derive(Debug)]
struct FilePass {
    range: FileRange,
    reader: Option<BufReader<Take<File>>>,
    /// Where the next line starts.
    position: u64,
    line: Vec<u8>,
    done: bool,
}

impl FilePass {
    fn new(range: FileRange) -> Self {
        let position = range.offset;
        Self {
            range,
            reader: None,
            position,
            line: Vec::new(),
            done: false,
        }
    }

    /// Reads on, off the async worker, into `read_ahead`.
    fn fill(&mut self, read_ahead: &mut VecDeque<Result<Arc<StoredEvent>, EventLogError>>) {
        crate::blocking::off_worker(|| {
            cookie_agent_protocol::read_stored_history(|| {
                if let Err(error) = self.read_ahead_into(read_ahead) {
                    self.done = true;
                    read_ahead.push_back(Err(error));
                }
            });
        });
    }

    fn read_ahead_into(
        &mut self,
        read_ahead: &mut VecDeque<Result<Arc<StoredEvent>, EventLogError>>,
    ) -> Result<(), EventLogError> {
        let path = self.range.path.clone();
        let io_error = |source| EventLogError::Io {
            path: path.clone(),
            source,
        };
        if self.reader.is_none() {
            let mut file = File::open(&path).map_err(&io_error)?;
            file.seek(SeekFrom::Start(self.position))
                .map_err(&io_error)?;
            let length = self.range.end.map_or(u64::MAX, |end| end - self.position);
            self.reader = Some(BufReader::with_capacity(
                READ_AHEAD_BYTES,
                file.take(length),
            ));
        }
        let blob_dir = blobs::tree_blob_dir(&path);
        let mut fetch = |digest: &str| blobs::read(&blob_dir, digest);
        // It keeps what it notes of damage for one visit: the load reported it.
        let mut lines = LineReader::new(&path);
        let mut bytes = 0_usize;
        while bytes < READ_AHEAD_BYTES && !self.done {
            if self.range.end == Some(self.position) {
                self.done = true;
                if self
                    .range
                    .held_from
                    .is_some_and(|held_from| !held_from.still_there(&path))
                {
                    return Err(EventLogError::HistoryCut { path });
                }
                break;
            }
            self.line.clear();
            let reader = self.reader.as_mut().expect("opened above");
            let read = reader.read_until(b'\n', &mut self.line);
            // Checked after the read, so what passes was read before a cut:
            // the log counts one before it cuts the file.
            if self.range.cuts.load(Ordering::Acquire) != self.range.seen_cuts {
                return Err(EventLogError::HistoryCut { path });
            }
            let length = read.map_err(&io_error)?;
            // The range's end lies within the file's complete lines, and a log
            // without its layout ends the range at a record, so a file that
            // ends first was cut.
            if !self.line.ends_with(b"\n") {
                return Err(EventLogError::HistoryCut { path });
            }
            self.position += length as u64;
            bytes += length;
            let line = &self.line[..length - 1];
            if line.is_empty() {
                continue;
            }
            // Never the log's first line by position: its creation is held,
            // and a pass starting there reads it only to skip it.
            let noted = lines.diagnostics.len();
            let read = lines.read(1, line, &mut fetch)?;
            if self.range.undamaged
                && lines.diagnostics[noted..]
                    .iter()
                    .any(|diagnostic| diagnostic.skipped)
            {
                return Err(EventLogError::Corrupt {
                    path,
                    message: format!(
                        "the line at byte {} cannot be read",
                        self.position - length as u64
                    ),
                });
            }
            let until = self.range.until_seq;
            if read.stop || read.sequenced.is_some_and(|(seq, _)| seq >= until) {
                self.done = true;
                break;
            }
            let Some(record) = read.record else {
                continue;
            };
            if record.seq >= self.range.from_seq
                && self.range.refused.binary_search(&record.seq).is_err()
            {
                read_ahead.push_back(Ok(Arc::new(record)));
            }
        }
        Ok(())
    }
}

/// The newest-first window of a log, read from where its view cache starts when
/// the log knows that (`docs/site/specs/forever-sessions.md` section 12).
#[derive(Debug)]
pub struct NewestPass {
    pub(super) from_cache_start: Option<(CacheBoundary, Vec<Arc<StoredEvent>>, History)>,
    pub(super) whole: History,
}

impl NewestPass {
    /// The newest-first window of the log.
    pub fn tail_window(self, view: EventsView, budget: usize) -> Result<Window, EventLogError> {
        let Some((start, header, suffix)) = self.from_cache_start else {
            return self.whole.tail_window(view, budget);
        };
        let suffix = suffix.collect::<Result<Vec<_>, _>>()?;
        debug_assert_eq!(suffix.first().map(|first| first.seq), Some(start.seq));
        let Some(window) = display_view::tail_window_after(&header, &suffix, view, budget) else {
            // A budget larger than the cache's own.
            return self.whole.tail_window(view, budget);
        };
        #[cfg(any(test, feature = "test-support"))]
        assert_eq!(
            self.whole.read_tail_window(view, budget)?,
            window,
            "a window read from the cache's start differs"
        );
        Ok(window)
    }
}

/// Where a view cache starts: [`History::view_cache_start`].
pub type ViewCacheStart = Option<(Arc<StoredEvent>, Vec<Arc<StoredEvent>>)>;

/// An older page, read by [`History::older_page`].
#[derive(Debug)]
pub struct OlderPage {
    pub window: Window,
    /// [`display_view::state_events`] of every record of the pass, when asked
    /// for: the header of what starts where the page ends.
    pub header_at_end: Option<Vec<Arc<StoredEvent>>>,
}

impl History {
    /// The records of `events`, in order, from position `start` to `end`: none
    /// when `start` is not before `end`, as for a range that ends before what a
    /// window holds after its pinned records.
    pub(super) fn new(events: EventSnapshot, start: usize, end: usize) -> Self {
        Self {
            creation: None,
            file: None,
            read_ahead: VecDeque::new(),
            events,
            next: start.min(end),
            end,
        }
    }

    /// The records of `events` from position `start` to `end`, after `creation`
    /// and what `file` names of what precedes them in the log.
    pub(super) fn with_file(
        creation: Option<Arc<StoredEvent>>,
        file: Option<FileRange>,
        events: EventSnapshot,
        start: usize,
        end: usize,
    ) -> Self {
        Self {
            creation,
            file: file.map(FilePass::new),
            ..Self::new(events, start, end)
        }
    }

    /// Test-only: where in the log this pass starts reading the file, if it does.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn reads_the_file_from(&self) -> Option<u64> {
        self.file.as_ref().map(|file| file.range.from_seq)
    }

    /// The records of this pass, when the log holds them all: a window need
    /// measure only what fits, walking back from the end, and a pass must
    /// measure every record to know that (section 2: a log not yet windowed
    /// costs nothing). `None` for what comes from the file.
    fn held(&self) -> Option<&[Arc<StoredEvent>]> {
        if self.creation.is_some() || self.file.is_some() {
            return None;
        }
        Some(&self.events[self.next..self.end])
    }

    /// The newest-first window of this pass, which covers a whole log.
    pub fn tail_window(self, view: EventsView, budget: usize) -> Result<Window, EventLogError> {
        let Some(events) = self.held() else {
            return self.read_tail_window(view, budget);
        };
        let window = display_view::tail_window(events, view, budget);
        #[cfg(any(test, feature = "test-support"))]
        assert_eq!(
            self.read_tail_window(view, budget)?,
            window,
            "a window read as a pass differs"
        );
        Ok(window)
    }

    fn read_tail_window(self, view: EventsView, budget: usize) -> Result<Window, EventLogError> {
        let mut reader = TailWindowReader::new(view, budget);
        for event in self {
            reader.observe(event?);
        }
        Ok(reader.window())
    }

    /// Where a view cache of this pass, which covers a whole log, starts: the
    /// record, with the header of what precedes it. `None` when it all fits.
    pub fn view_cache_start(self, budget: usize) -> Result<ViewCacheStart, EventLogError> {
        let Some(events) = self.held() else {
            return self.read_view_cache_start(budget);
        };
        let start = display_view::cache_start(events, budget).map(|start| {
            (
                Arc::clone(&events[start]),
                display_view::state_events(&events[..start]),
            )
        });
        #[cfg(any(test, feature = "test-support"))]
        assert_eq!(
            self.read_view_cache_start(budget)?,
            start,
            "a view cache's start read as a pass differs"
        );
        Ok(start)
    }

    fn read_view_cache_start(self, budget: usize) -> Result<ViewCacheStart, EventLogError> {
        let mut reader = TailWindowReader::new(EventsView::Display, budget);
        for event in self {
            reader.observe(event?);
        }
        Ok(reader.cache_start())
    }

    /// The page ending where this pass does, which starts at the log's start.
    pub fn older_page(
        self,
        view: EventsView,
        budget: usize,
        want_header_at_end: bool,
    ) -> Result<OlderPage, EventLogError> {
        let Some(events) = self.held() else {
            return self.read_older_page(view, budget, want_header_at_end);
        };
        let before = events.last().map_or(0, |event| event.seq + 1);
        let page = OlderPage {
            window: display_view::older_page(events, before, view, budget),
            header_at_end: want_header_at_end.then(|| display_view::state_events(events)),
        };
        #[cfg(any(test, feature = "test-support"))]
        {
            let read = self.read_older_page(view, budget, want_header_at_end)?;
            assert_eq!(read.window, page.window, "a page read as a pass differs");
            assert_eq!(
                read.header_at_end, page.header_at_end,
                "a page's header read as a pass differs"
            );
        }
        Ok(page)
    }

    fn read_older_page(
        self,
        view: EventsView,
        budget: usize,
        want_header_at_end: bool,
    ) -> Result<OlderPage, EventLogError> {
        let mut reader = OlderPageReader::new(view, budget);
        for event in self {
            reader.observe(event?);
        }
        let header_at_end = want_header_at_end.then(|| reader.header_at_end());
        Ok(OlderPage {
            window: reader.finish(),
            header_at_end,
        })
    }

    /// The first answer `find` gives, stopping there.
    pub fn find_map_ok<T>(
        self,
        mut find: impl FnMut(&StoredEvent) -> Option<T>,
    ) -> Result<Option<T>, EventLogError> {
        for event in self {
            if let Some(found) = find(&*event?) {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// The last answer `find` gives: the one a scan from the log's tip would
    /// meet first. It reads to the end, keeping only the latest answer.
    pub fn last_map_ok<T>(
        self,
        mut find: impl FnMut(&StoredEvent) -> Option<T>,
    ) -> Result<Option<T>, EventLogError> {
        let mut latest = None;
        for event in self {
            if let Some(found) = find(&*event?) {
                latest = Some(found);
            }
        }
        Ok(latest)
    }

    /// Calls `visit` with every record.
    pub fn for_each_ok(
        self,
        mut visit: impl FnMut(&Arc<StoredEvent>),
    ) -> Result<(), EventLogError> {
        for event in self {
            visit(&event?);
        }
        Ok(())
    }
}

impl Iterator for History {
    type Item = Result<Arc<StoredEvent>, EventLogError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(creation) = self.creation.take() {
            return Some(Ok(creation));
        }
        while let Some(file) = &mut self.file {
            match self.read_ahead.pop_front() {
                Some(Ok(event)) => return Some(Ok(event)),
                Some(Err(error)) => {
                    // Nothing follows a pass that failed.
                    self.file = None;
                    self.next = self.end;
                    return Some(Err(error));
                }
                None if file.done => self.file = None,
                None => file.fill(&mut self.read_ahead),
            }
        }
        if self.next >= self.end {
            return None;
        }
        let event = Arc::clone(&self.events[self.next]);
        self.next += 1;
        Some(Ok(event))
    }
}
