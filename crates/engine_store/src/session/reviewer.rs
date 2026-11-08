//! The approval reviewer's own log (`docs/site/specs/forever-sessions.md`
//! section 5.5): a hidden child session of the session it reviews.
//!
//! It is a session log like any other, with its own id, `session_created`,
//! window, and `state.json`, filed in the reviewed session's folder as
//! `reviewer/events.jsonl`. Nothing that lists or walks sessions looks there:
//! children are filed under `subagents/` and found through its index, so the
//! tree load, the listing, the activity journal, the marks, and every client
//! never see it. It lives and goes with its folder: a delete removes it, a
//! revert of the reviewed session cuts it to the turns whose verdict the cut
//! keeps, and a fork copies those of the fork's prefix. Its records after
//! the creation are `reviewer_turn_recorded`, and a turn that starts an epoch
//! is where its window starts.
//!
//! The store holds the reviewer logs it opened beside the sessions they
//! review, and lets go of each, writing its `state.json`, when its session
//! leaves memory.

use std::io::BufRead as _;

use super::*;

/// The folder of a session's reviewer log, in the session's own folder.
pub const REVIEWER_DIR: &str = "reviewer";

/// The origin the reviewer log's records carry.
const REVIEWER_ORIGIN: &str = "engine:approvals";

/// A reviewer log the store opened, with what it folds to, which its
/// `state.json` holds.
pub(super) struct ReviewerLog {
    log: Arc<EventLog>,
    projection: Mutex<SessionProjection>,
}

/// The reviewer logs this store opened, by the session each reviews.
#[derive(Default)]
pub(super) struct ReviewerLogs(Mutex<HashMap<SessionId, Arc<ReviewerLog>>>);

impl std::fmt::Debug for ReviewerLogs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let open = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .copied()
            .collect::<Vec<_>>();
        formatter.debug_tuple("ReviewerLogs").field(&open).finish()
    }
}

impl ReviewerLogs {
    fn get(&self, parent: SessionId) -> Option<Arc<ReviewerLog>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&parent)
            .cloned()
    }

    fn insert(&self, parent: SessionId, log: Arc<ReviewerLog>) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(parent, log);
    }

    fn remove(&self, parent: SessionId) -> Option<Arc<ReviewerLog>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&parent)
    }

    fn drain(&self) -> Vec<Arc<ReviewerLog>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, log)| log)
            .collect()
    }
}

impl ReviewerLog {
    fn open(log: Arc<EventLog>) -> Result<Arc<Self>, SessionError> {
        let projection = projection(Arc::clone(&log))?;
        Ok(Arc::new(Self {
            log,
            projection: Mutex::new(projection),
        }))
    }

    /// Folds `event`, just appended, into the projection.
    fn observe(&self, event: &StoredEvent) -> Result<(), SessionError> {
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match projection.observe(event, FoldStep::Append) {
            Observed::Applied { .. } => {
                projection.meta.last_event_seq = event.seq;
                projection.meta.last_activity = event.timestamp;
            }
            Observed::NeedsRefold => *projection = refold(Arc::clone(&self.log), None)?,
        }
        Ok(())
    }

    /// Writes `state.json` when the log should have one and is behind it.
    fn write_state(&self) {
        if !self.log.state_behind().is_some_and(|behind| behind > 0) {
            return;
        }
        let state = {
            let projection = self
                .projection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.log.state_file(&projection)
        };
        let Some(state) = state else {
            return;
        };
        match view::write_state(&self.log, &state) {
            Ok(()) => self.log.note_state_written(&state),
            Err(error) => eprintln!(
                "{}: the reviewer's state.json was not written: {error}",
                self.log.path().display()
            ),
        }
    }

    /// Test-only: what a reopening of the file and a `state.json` of this
    /// moment give is what the log holds, as a session's own log is checked
    /// at each window move.
    #[cfg(any(test, feature = "test-support"))]
    fn assert_reads_as_reopened(&self) {
        self.log.assert_window_reads_as_reopened();
        let projection = self
            .projection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let reopened =
            EventLog::parsed_read_only(self.log.path().to_owned(), self.log.session_id())
                .map(|reopened| projection_fold(reopened).expect("reference fold"))
                .expect("a reviewer log reopens");
        assert_projection_equivalent(&projection, &reopened);
        if let Some(state) = self.log.state_file(&projection) {
            view::write_state(&self.log, &state).expect("state.json is written");
            let opened =
                EventLog::open_read_only(self.log.path().to_owned(), self.log.session_id())
                    .expect("a reviewer log opens from its state file");
            assert!(
                opened.opened_from_state_for_test(),
                "a state file the reviewer log just wrote matches its log"
            );
            let _ = fs::remove_file(self.log.path().with_file_name(STATE_FILE));
        }
    }
}

/// Whether `payload` starts an epoch of a reviewer log, which moves its window.
fn starts_epoch(payload: &EventPayload) -> bool {
    matches!(payload, EventPayload::ReviewerTurnRecorded { turn } if turn.epoch.is_some())
}

/// The session a log's first line names, read without the rest of the file.
fn first_record_session(path: &Path) -> Option<SessionId> {
    let file = fs::File::open(path).ok()?;
    let mut line = Vec::new();
    std::io::BufReader::new(file)
        .read_until(b'\n', &mut line)
        .ok()?;
    #[derive(serde::Deserialize)]
    struct Envelope {
        session_id: SessionId,
    }
    serde_json::from_slice::<Envelope>(&line)
        .ok()
        .map(|envelope| envelope.session_id)
}

/// Removes a reviewer log's files, so the next review starts it over.
fn remove_files(directory: &Path) {
    match fs::remove_dir_all(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => eprintln!(
            "the reviewer log at {} could not be removed: {error}",
            directory.display()
        ),
    }
}

impl SessionStore {
    /// The folder of `parent`'s reviewer log.
    fn reviewer_dir(&self, parent: SessionId) -> Result<PathBuf, SessionError> {
        Ok(self.resolve_dir(parent)?.join(REVIEWER_DIR))
    }

    /// The reviewer log of `parent`, a session this process owns, opened from
    /// its file the first time it is asked for: through its `state.json` when
    /// that matches, which reads only what the window holds. `None` when it
    /// has none, or when its file cannot be opened, which removes it: the next
    /// review starts it over from the session's log.
    pub fn reviewer_log(&self, parent: SessionId) -> Result<Option<Arc<EventLog>>, SessionError> {
        if let Some(open) = self.reviewers.get(parent) {
            return Ok(Some(Arc::clone(&open.log)));
        }
        let capability = self.write_capability(parent, false)?;
        let directory = self.reviewer_dir(parent)?;
        let path = directory.join(EVENTS_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let opened = first_record_session(&path)
            .ok_or_else(|| SessionError::Missing(parent))
            .and_then(|id| Ok(EventLog::open_owned(path, id, capability)?))
            .and_then(ReviewerLog::open);
        match opened {
            Ok(open) => {
                let log = Arc::clone(&open.log);
                self.reviewers.insert(parent, open);
                Ok(Some(log))
            }
            Err(error) => {
                eprintln!(
                    "session {parent}: the reviewer log could not be opened and starts over: {error}"
                );
                remove_files(&directory);
                Ok(None)
            }
        }
    }

    /// Starts `parent`'s reviewer log over, replacing any it had, with
    /// `records` after its creation, and syncs it. Its `session_created` is
    /// the reviewed session's, as a root of its own: the reviewer's
    /// conversation has no delegation of its own.
    pub fn create_reviewer_log(
        &self,
        parent: SessionId,
        records: Vec<EventPayload>,
    ) -> Result<Arc<EventLog>, SessionError> {
        let reviewed = self.log(parent)?;
        self.create_reviewer_log_of(parent, &reviewed, records)
    }

    /// [`Self::create_reviewer_log`] for `reviewed`, `parent`'s log.
    fn create_reviewer_log_of(
        &self,
        parent: SessionId,
        reviewed: &EventLog,
        records: Vec<EventPayload>,
    ) -> Result<Arc<EventLog>, SessionError> {
        let capability = self.write_capability(parent, false)?;
        if !reviewed.is_persisted() {
            return Err(SessionError::SessionLocked(parent));
        }
        let Some(EventPayload::SessionCreated {
            short_id,
            cwd_identity,
            creation_selection,
            creation_agent,
            revisions,
            ..
        }) = reviewed.creation().map(|creation| creation.payload.clone())
        else {
            return Err(SessionError::Missing(parent));
        };
        let creation = EventPayload::SessionCreated {
            history_format: cookie_agent_protocol::HISTORY_FORMAT,
            origin: SessionOrigin::Root,
            short_id,
            cwd_identity,
            creation_selection,
            creation_agent,
            revisions,
            cron: None,
        };
        if let Some(open) = self.reviewers.remove(parent) {
            open.log.suspend_writer();
        }
        let directory = self.reviewer_dir(parent)?;
        remove_files(&directory);
        #[cfg(unix)]
        create_unix_session_directory_all(&directory)?;
        #[cfg(windows)]
        create_windows_session_directory(&directory)?;
        let epoch = records.iter().any(starts_epoch);
        let log = EventLog::create_persisted_owned(
            directory.join(EVENTS_FILE),
            SessionId::new_v7(),
            cookie_agent_protocol::EventOrigin::new(REVIEWER_ORIGIN)
                .expect("static event origin is valid"),
            creation,
            records,
            capability,
        )?;
        fsync_directory(&directory)?;
        let open = ReviewerLog::open(Arc::clone(&log))?;
        if epoch {
            // In tests, a reopening and a state file of this moment answer as
            // the log does; the file is written after, as it removes its own.
            #[cfg(any(test, feature = "test-support"))]
            open.assert_reads_as_reopened();
            open.write_state();
        }
        self.reviewers.insert(parent, open);
        Ok(log)
    }

    /// Appends `payload` to `parent`'s reviewer log, which
    /// [`Self::reviewer_log`] or [`Self::create_reviewer_log`] opened, and syncs
    /// it. A turn that starts an epoch moves the log's window and writes its
    /// `state.json`.
    pub fn append_reviewer(
        &self,
        parent: SessionId,
        payload: EventPayload,
    ) -> Result<Arc<StoredEvent>, SessionError> {
        let capability = self.write_capability(parent, false)?;
        let open = self
            .reviewers
            .get(parent)
            .ok_or(SessionError::Missing(parent))?;
        let epoch = starts_epoch(&payload);
        let event = open.log.append_owned(
            &capability,
            None,
            cookie_agent_protocol::EventOrigin::new(REVIEWER_ORIGIN)
                .expect("static event origin is valid"),
            payload,
        )?;
        open.observe(&event)?;
        if epoch {
            // In tests, a reopening and a state file of this moment answer as
            // the log does; the file is written after, as it removes its own.
            #[cfg(any(test, feature = "test-support"))]
            open.assert_reads_as_reopened();
            open.write_state();
        }
        Ok(event)
    }

    /// Lets go of `parent`'s reviewer log as `parent` leaves memory, writing
    /// its `state.json` first.
    pub(super) fn release_reviewer(&self, parent: SessionId) {
        if let Some(open) = self.reviewers.remove(parent) {
            open.write_state();
        }
    }

    /// Lets go of every reviewer log, as the store releases its trees.
    pub(super) fn release_reviewers(&self) {
        for open in self.reviewers.drain() {
            open.write_state();
        }
    }

    /// Forgets the reviewer logs of `deleted` sessions, whose folders go with
    /// them.
    pub(super) fn forget_reviewers(&self, deleted: &HashSet<SessionId>) {
        for id in deleted {
            if let Some(open) = self.reviewers.remove(*id) {
                open.log.suspend_writer();
            }
        }
    }

    /// The reviewer turns of `parent`'s log whose verdicts `through_seq` of
    /// the reviewed session keeps, from the latest epoch among them on, and
    /// the position in the reviewer log of the last of them. `None` when it
    /// has no log; an empty list when none is kept.
    fn kept_reviewer_turns(
        &self,
        parent: SessionId,
        through_seq: u64,
    ) -> Result<Option<(Vec<EventPayload>, u64)>, SessionError> {
        let Some(log) = self.reviewer_log(parent)? else {
            return Ok(None);
        };
        let mut kept = Vec::new();
        let mut last = 1;
        for event in log.history()? {
            let event = event?;
            if let EventPayload::ReviewerTurnRecorded { turn } = &event.payload {
                if turn.evaluated_seq > through_seq {
                    break;
                }
                if turn.epoch.is_some() {
                    kept.clear();
                }
                kept.push(event.payload.clone());
                last = event.seq;
            }
        }
        Ok(Some((kept, last)))
    }

    /// Cuts `parent`'s reviewer log, as a revert of `parent` through
    /// `through_seq` cuts the session's: the turns whose verdict the cut
    /// removes go, and with them any epoch they started. A failure removes the
    /// log, so the next review starts it over rather than read a turn the
    /// session no longer has.
    pub(super) fn revert_reviewer(&self, parent: SessionId, through_seq: u64) {
        let cut = || -> Result<(), SessionError> {
            let Some((_, last)) = self.kept_reviewer_turns(parent, through_seq)? else {
                return Ok(());
            };
            let open = self
                .reviewers
                .get(parent)
                .ok_or(SessionError::Missing(parent))?;
            if last >= open.log.physical_tip_seq() {
                return Ok(());
            }
            let capability = self.write_capability(parent, false)?;
            open.log.truncate_after(&capability, last)?;
            *open
                .projection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                refold(Arc::clone(&open.log), None)?;
            let _ = fs::remove_file(open.log.path().with_file_name(STATE_FILE));
            open.write_state();
            Ok(())
        };
        if let Err(error) = cut() {
            eprintln!(
                "session {parent}: the reviewer log could not follow a revert and starts over: {error}"
            );
            if let Some(open) = self.reviewers.remove(parent) {
                open.log.suspend_writer();
            }
            if let Ok(directory) = self.reviewer_dir(parent) {
                remove_files(&directory);
            }
        }
    }

    /// Gives `fork`, a fork of `source` through `through_seq`, a reviewer log
    /// of its own holding the turns of `source`'s that the fork keeps, from the
    /// latest epoch among them. A failure leaves the fork without one, so its
    /// first review starts it over.
    pub(super) fn fork_reviewer(&self, source: SessionId, fork: SessionId, through_seq: u64) {
        let copy = || -> Result<(), SessionError> {
            let Some((kept, _)) = self.kept_reviewer_turns(source, through_seq)? else {
                return Ok(());
            };
            if kept.is_empty() {
                return Ok(());
            }
            let reviewed = self.get_resident(fork).ok_or(SessionError::Missing(fork))?;
            self.create_reviewer_log_of(fork, &reviewed.log, kept)?;
            Ok(())
        };
        if let Err(error) = copy() {
            eprintln!("session {fork}: the reviewer log of its source was not copied: {error}");
        }
    }

    /// Test-only: whether this store holds `parent`'s reviewer log open.
    #[cfg(any(test, feature = "test-support"))]
    pub fn reviewer_log_is_open_for_test(&self, parent: SessionId) -> bool {
        self.reviewers.get(parent).is_some()
    }
}
