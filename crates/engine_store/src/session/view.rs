//! `view.json`: a session's newest-first view, cached
//! (`docs/site/specs/session-storage-v3.md` section 4.1).
//!
//! A cold open answers `events.subscribe` with `from: "tail"` from the cache
//! instead of parsing the whole log: the cache names a run boundary of the
//! log, where its line starts in the file, and the header of everything
//! before it ([`display_view::state_events`]). The reader parses the log
//! from that line to its end and cuts the window from those events
//! ([`display_view::tail_window_after`]); the header composes with them, so
//! the answer is the one the whole log gives. The boundary is chosen so that
//! every window up to [`VIEW_BUDGET`] starts after it, and stays so as the
//! log grows.
//!
//! The file is a cache: it is written without a sync, a missing, unreadable,
//! or stale one means parsing the log, and it is checked against the log
//! before it is used (the line at its offset must be the record it names,
//! whole, which a revert or a rewritten log no longer has there).

use cookie_agent_protocol::{EventsView, display_view};

use super::*;
use crate::events::VIEW_BUDGET;

/// The cache's file in a session's directory.
pub const VIEW_FILE: &str = "view.json";
/// The version of [`VIEW_FILE`]'s shape this build reads and writes.
const VIEW_VERSION: u32 = 1;
#[derive(serde::Deserialize, serde::Serialize)]
struct ViewCache {
    version: u32,
    /// The history format of the log the cache was cut from.
    history_format: u32,
    start: ViewStart,
    /// [`display_view::state_events`] of the log's events before `start`.
    header: Vec<Arc<StoredEvent>>,
}

/// The run boundary the cache reads the log from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(super) struct ViewStart {
    seq: u64,
    timestamp: jiff::Timestamp,
    /// Where its line starts in `events.jsonl`.
    offset: u64,
}

/// What a cache gives a reader: the header before the boundary, and the
/// log's events from the boundary to its end.
pub(super) struct CachedView {
    pub(super) header: Vec<Arc<StoredEvent>>,
    pub(super) events: Vec<Arc<StoredEvent>>,
}

/// The cache of the session in `dir`, whose log is `events_path`, with the
/// log read from its boundary on; `None` when there is none, or it does not
/// match the log.
pub(super) fn read_view(dir: &Path, events_path: &Path) -> Option<CachedView> {
    let bytes = cookie_agent_models::secure_store::read_replaced(&dir.join(VIEW_FILE)).ok()?;
    let cache = cookie_agent_protocol::read_stored_history(|| {
        serde_json::from_slice::<ViewCache>(&bytes).ok()
    })?;
    if cache.version != VIEW_VERSION
        || cache.history_format != cookie_agent_protocol::HISTORY_FORMAT
    {
        return None;
    }
    let events = crate::events::read_records_from(events_path, cache.start.offset).ok()??;
    let first = events.first()?;
    if first.seq != cache.start.seq || first.timestamp != cache.start.timestamp {
        return None;
    }
    Some(CachedView {
        header: cache.header,
        events: events.into_iter().map(Arc::new).collect(),
    })
}

/// Writes the cache of `log`, a session's whole persisted log, unless the
/// cache would start where `written` says the last one did. Returns where it
/// starts, or `None` when the log has no cache: it fits a window whole, its
/// file's layout is not known, or its load skipped or repaired a record
/// after the boundary, which the reader would not read as the loader did.
///
/// A log that holds its events walks back from their end to find where. A
/// windowed one kept up with that as it grew, so this reads none of its log.
pub(super) fn write_view(
    log: &EventLog,
    written: Option<ViewStart>,
) -> Result<Option<ViewStart>, SessionError> {
    if !log.is_persisted() || log.is_render_only() || log.is_converted() {
        return Ok(None);
    }
    let fold = log.view_start_fold();
    let (seq, timestamp, mut header) = match &fold {
        Some(fold) => {
            let Some(start) = fold.start() else {
                return Ok(None);
            };
            (start.seq, start.timestamp, None)
        }
        None => {
            let Some((first, header)) = log.history()?.view_cache_start(VIEW_BUDGET)? else {
                return Ok(None);
            };
            (first.seq, first.timestamp, Some(header))
        }
    };
    if log
        .diagnostics()
        .iter()
        .any(|diagnostic| diagnostic.seq >= seq)
    {
        return Ok(None);
    }
    let Some(offset) = log.line_start(seq) else {
        return Ok(None);
    };
    let view_start = ViewStart {
        seq,
        timestamp,
        offset,
    };
    if written == Some(view_start) {
        return Ok(Some(view_start));
    }
    let header = header
        .take()
        .or_else(|| fold.as_ref().map(|fold| fold.header().to_vec()))
        .unwrap_or_default();
    let cache = ViewCache {
        version: VIEW_VERSION,
        history_format: cookie_agent_protocol::HISTORY_FORMAT,
        start: view_start,
        header,
    };
    let Some(dir) = log.path().parent() else {
        return Ok(None);
    };
    let path = dir.join(VIEW_FILE);
    let bytes = serde_json::to_vec(&cache).map_err(|source| SessionError::Json {
        path: path.clone(),
        source,
    })?;
    replace_cache_file(&path, &bytes, CacheSync::Atomic)?;
    Ok(Some(view_start))
}

/// The answer to `events.subscribe` with `from: "tail"` for `window`.
pub(super) fn newest_first(window: display_view::Window) -> EventsSubscribeResult {
    EventsSubscribeResult {
        events: window.events,
        has_more: false,
        stale_cursor: false,
        window: Some(cookie_agent_protocol::HistoryWindow {
            header: window.header,
            older_cursor: window.older_cursor,
            gap: window.gap,
        }),
        cut: window.cut,
        spans: Vec::new(),
        live: false,
        view_events: None,
    }
}

impl SessionStore {
    /// Brings `id`'s view cache up to `log`, its whole log, when the cache
    /// would start elsewhere than the last one this process wrote. A failure
    /// is only logged: the cache is not needed.
    pub(super) fn refresh_view(&self, id: SessionId, log: &EventLog) {
        let written = self
            .views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&id)
            .copied();
        match write_view(log, written) {
            Ok(Some(start)) => {
                self.views
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id, start);
            }
            Ok(None) => {}
            Err(error) => eprintln!("session {id}: the view cache was not written: {error}"),
        }
    }

    /// Writes `id`'s view cache from its resident log, if it is resident.
    pub fn refresh_resident_view(&self, id: SessionId) {
        if let Some(resident) = self.get_resident(id) {
            self.refresh_view(id, &resident.log);
        }
    }

    /// The newest-first answer for `id` (`events.subscribe` with `from:
    /// "tail"`) with `owner`'s live tail registered where its events end.
    ///
    /// Neither `id`'s tree is completed nor `id` opened: a resident session
    /// is answered from memory, any other from its view cache or its parsed
    /// log. The handoff to the tail is exact as the final page's is: the
    /// tail is registered under the session's slot, which every append
    /// takes, after checking that what was read is still the whole log —
    /// the same resident log, or for a session not resident, whose log only
    /// grows through a resident one, a file unchanged since it was read.
    pub fn subscribe_tail(
        &self,
        id: SessionId,
        view: EventsView,
        budget: usize,
        owner: Option<TailOwner>,
    ) -> Result<
        (
            EventsSubscribeResult,
            mpsc::Receiver<EventSubscriptionMessage>,
        ),
        SessionError,
    > {
        let (answer, receiver) = self.newest_first(id, view, budget, Some(owner))?;
        Ok((answer, receiver.expect("a live answer registers its tail")))
    }

    /// [`Self::subscribe_tail`] without a live tail, for a session this
    /// process does not own.
    pub fn snapshot_tail(
        &self,
        id: SessionId,
        view: EventsView,
        budget: usize,
    ) -> Result<EventsSubscribeResult, SessionError> {
        Ok(self.newest_first(id, view, budget, None)?.0)
    }

    #[allow(clippy::type_complexity)]
    fn newest_first(
        &self,
        id: SessionId,
        view: EventsView,
        budget: usize,
        tail: Option<Option<TailOwner>>,
    ) -> Result<
        (
            EventsSubscribeResult,
            Option<mpsc::Receiver<EventSubscriptionMessage>>,
        ),
        SessionError,
    > {
        /// Reads that may lose the race with an append before one is done
        /// holding the session's slot throughout.
        const UNLOCKED_ATTEMPTS: usize = 3;
        for attempt in 0..=UNLOCKED_ATTEMPTS {
            let locked = attempt == UNLOCKED_ATTEMPTS && tail.is_some();
            let held = locked.then(|| self.lock_session(id));
            if self.get_resident(id).is_none() {
                let session_dir = self.resolve_dir(id)?;
                if !session_dir.is_dir() {
                    return Err(SessionError::Missing(id));
                }
                let path = session_dir.join(EVENTS_FILE);
                let fingerprint = ReadOnlyLogFingerprint::of(&path)?;
                let (window, tip) = self.cold_window(id, &session_dir, &path, view, budget)?;
                let Some(owner) = tail else {
                    return Ok((newest_first(window), None));
                };
                let _slot = held.unwrap_or_else(|| self.lock_session(id));
                self.ensure_open()?;
                // Under the slot throughout, nothing can have moved.
                if locked
                    || (self.get_resident(id).is_none()
                        && ReadOnlyLogFingerprint::of(&path)? == fingerprint)
                {
                    let receiver = self.register_tail(id, tip, owner, true);
                    return Ok((newest_first(window), Some(receiver)));
                }
                // Appended, or made resident, since it was read.
                continue;
            }
            let (events, receiver) = {
                let _slot = held.unwrap_or_else(|| self.lock_session(id));
                self.ensure_open()?;
                let Some(resident) = self.get_resident(id) else {
                    continue;
                };
                // Records still waiting for a sync are in it: their
                // publication skips the tail. The slot holds appends off, so
                // the pass ends where the tail starts, and it is read outside
                // the slot.
                let events = resident.log.newest_pass()?;
                let receiver = tail.map(|owner| {
                    let tip = resident.log.last_event().map_or(0, |event| event.seq);
                    self.register_tail(id, tip, owner, true)
                });
                (events, receiver)
            };
            return Ok((newest_first(events.tail_window(view, budget)?), receiver));
        }
        Err(SessionError::TreeContended(id))
    }

    /// The newest-first window of `id`, a session not resident whose log is
    /// `path`, from its view cache when that answers, else from the parsed
    /// log; with the sequence of the last event read.
    fn cold_window(
        &self,
        id: SessionId,
        session_dir: &Path,
        path: &Path,
        view: EventsView,
        budget: usize,
    ) -> Result<(display_view::Window, u64), SessionError> {
        if let Some(cached) = read_view(session_dir, path)
            && let Some(window) =
                display_view::tail_window_after(&cached.header, &cached.events, view, budget)
        {
            let tip = cached.events.last().map_or(0, |event| event.seq);
            return Ok((window, tip));
        }
        let log = self.read_only_log(id, path.to_owned())?;
        // The parse is done; a session this process owns keeps what it
        // gives the next cold open, whatever this process last wrote, since
        // the cache did not answer.
        if self.is_owned(id) {
            self.views
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            self.refresh_view(id, &log);
        }
        let tip = log.last_event().map_or(0, |event| event.seq);
        Ok((log.history()?.tail_window(view, budget)?, tip))
    }
}
