//! Buffered/durable session event logs and ephemeral tool-output hubs.

use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[cfg(unix)]
use std::fs::File;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::{
    AssistantToolCallRef, AttemptId, EventOrigin, EventPayload, ModelCallId, OutputDelta,
    OutputGap, OutputSnapshot, OutputStream, ProviderItemId, RunId, SessionId, StoredEvent,
    StoredEventEnvelope, ToolCallId, ToolCallStart, deserialize_event_payload_best_effort,
};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::ownership::{WriteAuthority, WriteCapability};

#[derive(Debug, Error)]
pub enum EventLogError {
    #[error("event log IO failure at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid JSONL record in {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("event log {0} has no SessionCreated record")]
    MissingCreation(PathBuf),
    #[error("corrupt event log at {path}: {message}")]
    Corrupt { path: PathBuf, message: String },
    #[error("event log {0} is read-only")]
    ReadOnly(PathBuf),
}

#[derive(Debug)]
pub struct EventLog {
    path: PathBuf,
    session_id: SessionId,
    append: Mutex<()>,
    events: Mutex<EventStorage>,
    diagnostics: Vec<EventLoadDiagnostic>,
    /// Damage found at load. Truncation drops what it no longer reaches.
    initial_validation_taint: Mutex<ValidationTaint>,
    validation: Mutex<ValidationState>,
    next_seq: AtomicU64,
    persisted: AtomicBool,
    read_only: bool,
    write_capability: Option<WriteCapability>,
    #[cfg(test)]
    _test_authority: Option<WriteAuthority>,
    #[cfg(test)]
    append_authorization_hook: Mutex<Option<AppendAuthorizationHook>>,
    writer: Mutex<Option<EventLogWriter>>,
}

#[derive(Clone, Copy)]
enum TornTail {
    Truncate,
    Ignore,
}

/// The open append handle of a persisted log. Every append is synced before
/// it is published, so nothing is ever buffered here: no flush, no deferred
/// sync, no background failure to report later.
#[derive(Debug)]
struct EventLogWriter {
    path: PathBuf,
    file: fs::File,
    directory_sync_pending: bool,
    #[cfg(test)]
    before_sync: Option<SyncHook>,
}

#[cfg(test)]
#[derive(Debug)]
struct SyncHook {
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
#[derive(Debug)]
struct AppendAuthorizationHook {
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[derive(Debug, Default)]
struct EventIndex {
    last_run_started: Option<(u64, RunId, cookie_agent_protocol::ModelSelection)>,
    last_checkpoint_seq: u64,
    last_checkpoint_input_through_seq: u64,
    last_recorded_usage: Option<(u64, u64)>,
    last_turn_usage: Option<(u64, u64)>,
    /// Visible `ModelAttemptStarted` records per run.
    run_attempts: HashMap<RunId, u32>,
    /// Tool names of the latest visible `ModelToolsPublished` record.
    last_model_tools: Option<Vec<String>>,
}

impl EventIndex {
    fn observe(&mut self, event: &StoredEvent) {
        match &event.payload {
            EventPayload::RunStarted { selection, .. } => {
                if let Some(run) = event.run_id {
                    self.last_run_started = Some((event.seq, run, selection.model.clone()));
                }
            }
            EventPayload::ContextCheckpointCommitted { commit } => {
                self.last_checkpoint_seq = event.seq;
                self.last_checkpoint_input_through_seq = commit.boundaries.input_through_seq;
            }
            EventPayload::ModelUsageRecorded { usage, .. } => {
                if let Some(usage) = usage_total(event.seq, usage) {
                    self.last_recorded_usage = Some(usage);
                }
            }
            EventPayload::ModelTurnCommitted { turn, .. } => {
                if let Some(usage) = usage_total(event.seq, &turn.usage) {
                    self.last_turn_usage = Some(usage);
                }
            }
            EventPayload::ModelAttemptStarted { .. } => {
                if let Some(run) = event.run_id {
                    *self.run_attempts.entry(run).or_default() += 1;
                }
            }
            EventPayload::ModelToolsPublished { tool_names, .. } => {
                self.last_model_tools = Some(tool_names.clone());
            }
            _ => {}
        }
    }

    fn latest_real_usage(&self) -> Option<(u64, u64)> {
        self.last_recorded_usage.or(self.last_turn_usage)
    }
}

/// A shared, immutable view of a log's events. Taking one is a reference
/// count bump; the log copies the pointer list only when it appends while an
/// older view is still alive, and never deep-copies an event.
pub type EventSnapshot = Arc<Vec<Arc<StoredEvent>>>;

#[derive(Debug)]
struct EventStorage {
    /// Every physical record, including ones hidden by a later revert.
    all: EventSnapshot,
    /// The currently visible branch, in log order.
    visible: EventSnapshot,
    index: EventIndex,
    /// Physical `ModelTurnCommitted` records. Model turn sequences are
    /// session-global and stay contiguous across reverts, so this counts
    /// hidden records too.
    physical_model_turns: u64,
}

fn is_model_turn(event: &StoredEvent) -> bool {
    matches!(event.payload, EventPayload::ModelTurnCommitted { .. })
}

impl EventStorage {
    fn new(all: Vec<Arc<StoredEvent>>) -> Self {
        let physical_model_turns = all.iter().filter(|event| is_model_turn(event)).count() as u64;
        let mut storage = Self {
            all: Arc::new(all),
            visible: Arc::default(),
            index: EventIndex::default(),
            physical_model_turns,
        };
        storage.rebuild_visible();
        storage
    }

    fn push(&mut self, event: impl Into<Arc<StoredEvent>>) {
        let event = event.into();
        self.physical_model_turns += u64::from(is_model_turn(&event));
        Arc::make_mut(&mut self.all).push(event.clone());
        if let EventPayload::SessionReverted { through_seq } = &event.payload {
            let through_seq = *through_seq;
            let visible = Arc::make_mut(&mut self.visible);
            visible.retain(|candidate| candidate.seq <= through_seq);
            visible.push(event);
            self.rebuild_index();
        } else {
            self.index.observe(&event);
            Arc::make_mut(&mut self.visible).push(event);
        }
    }

    /// Legacy `SessionReverted` markers each hide what follows their target
    /// on the branch visible when they were written. Reverts no longer write
    /// markers; they truncate the log (see [`Self::truncate`]).
    fn rebuild_visible(&mut self) {
        let mut visible: Vec<Arc<StoredEvent>> = Vec::with_capacity(self.all.len());
        for event in self.all.iter() {
            if let EventPayload::SessionReverted { through_seq } = &event.payload {
                visible.retain(|candidate| candidate.seq <= *through_seq);
            }
            visible.push(event.clone());
        }
        self.visible = Arc::new(visible);
        self.rebuild_index();
    }

    /// Drops every physical event after `through_seq`.
    fn truncate(&mut self, through_seq: u64) {
        Arc::make_mut(&mut self.all).retain(|event| event.seq <= through_seq);
        self.physical_model_turns =
            self.all.iter().filter(|event| is_model_turn(event)).count() as u64;
        self.rebuild_visible();
    }

    fn rebuild_index(&mut self) {
        self.index = EventIndex::default();
        for event in self.visible.iter() {
            self.index.observe(event);
        }
    }

    fn snapshot(&self) -> EventSnapshot {
        self.visible.clone()
    }
}

/// The branch of `events` that its revert markers leave visible.
pub(crate) fn visible_events(events: Vec<Arc<StoredEvent>>) -> EventSnapshot {
    EventStorage::new(events).snapshot()
}

/// Borrows each event of a slice of owned or shared events.
pub(crate) fn event_refs<E: Borrow<StoredEvent>>(events: &[E]) -> Vec<&StoredEvent> {
    events.iter().map(Borrow::borrow).collect()
}

fn usage_total(seq: u64, usage: &cookie_agent_protocol::Usage) -> Option<(u64, u64)> {
    let input = usage.input_tokens;
    let output = usage.output_tokens;
    (input.is_some() || output.is_some()).then(|| {
        (
            seq,
            input
                .unwrap_or_default()
                .saturating_add(output.unwrap_or_default()),
        )
    })
}

impl EventLogWriter {
    fn open(path: &Path) -> Result<Self, EventLogError> {
        let created = !path.exists();
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;

            const FILE_SHARE_READ: u32 = 0x1;
            const FILE_SHARE_WRITE: u32 = 0x2;
            const FILE_SHARE_DELETE: u32 = 0x4;
            options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        }
        let file = options.open(path).map_err(|source| EventLogError::Io {
            path: path.to_owned(),
            source,
        })?;
        Ok(Self {
            path: path.to_owned(),
            file,
            directory_sync_pending: created,
            #[cfg(test)]
            before_sync: None,
        })
    }

    /// Writes one newline-terminated record and syncs it (and, after the
    /// file was created, its directory entry).
    fn append(&mut self, record: &[u8]) -> Result<(), EventLogError> {
        #[cfg(test)]
        if let Some(hook) = self.before_sync.take() {
            let _ = hook.reached.send(());
            let _ = hook.release.recv();
        }
        self.file
            .write_all(record)
            .map_err(|source| self.io_error(source))?;
        self.file
            .sync_data()
            .map_err(|source| self.io_error(source))?;
        if self.directory_sync_pending
            && let Some(parent) = self.path.parent()
        {
            fsync_directory(parent)?;
            self.directory_sync_pending = false;
        }
        Ok(())
    }

    fn io_error(&self, source: io::Error) -> EventLogError {
        EventLogError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventLoadDiagnostic {
    pub seq: u64,
    pub reason: String,
    pub engine_version: Option<String>,
    pub skipped: bool,
}

impl EventLog {
    #[cfg(test)]
    pub fn create(
        path: PathBuf,
        session_id: SessionId,
        origin: EventOrigin,
        creation: EventPayload,
    ) -> Result<Arc<Self>, EventLogError> {
        let authority = WriteAuthority::new();
        let capability = authority.capability();
        Self::create_owned(
            path,
            session_id,
            origin,
            creation,
            capability,
            true,
            Some(authority),
        )
    }

    fn create_owned(
        path: PathBuf,
        session_id: SessionId,
        origin: EventOrigin,
        creation: EventPayload,
        capability: WriteCapability,
        persisted: bool,
        _test_authority: Option<WriteAuthority>,
    ) -> Result<Arc<Self>, EventLogError> {
        let log = Arc::new(Self {
            path,
            session_id,
            append: Mutex::new(()),
            events: Mutex::new(EventStorage::new(Vec::new())),
            diagnostics: Vec::new(),
            initial_validation_taint: Mutex::default(),
            validation: Mutex::new(ValidationState::default()),
            next_seq: AtomicU64::new(1),
            persisted: AtomicBool::new(persisted),
            read_only: false,
            write_capability: Some(capability),
            #[cfg(test)]
            _test_authority,
            #[cfg(test)]
            append_authorization_hook: Mutex::new(None),
            writer: Mutex::new(None),
        });
        if !matches!(creation, EventPayload::SessionCreated { .. }) {
            return Err(EventLogError::MissingCreation(log.path.clone()));
        }
        log.append_inner(None, None, origin, creation)?;
        Ok(log)
    }

    #[cfg(test)]
    pub fn create_buffered(
        path: PathBuf,
        session_id: SessionId,
        origin: EventOrigin,
        creation: EventPayload,
    ) -> Result<Arc<Self>, EventLogError> {
        let authority = WriteAuthority::new();
        let capability = authority.capability();
        Self::create_owned(
            path,
            session_id,
            origin,
            creation,
            capability,
            false,
            Some(authority),
        )
    }

    pub(crate) fn create_buffered_owned(
        path: PathBuf,
        session_id: SessionId,
        origin: EventOrigin,
        creation: EventPayload,
        capability: WriteCapability,
    ) -> Result<Arc<Self>, EventLogError> {
        Self::create_owned(path, session_id, origin, creation, capability, false, None)
    }

    #[cfg(test)]
    pub fn open(path: PathBuf, session_id: SessionId) -> Result<Arc<Self>, EventLogError> {
        let authority = WriteAuthority::new();
        let capability = authority.capability();
        Self::open_with_mode(path, session_id, false, Some(capability), Some(authority))
    }

    pub(crate) fn open_owned(
        path: PathBuf,
        session_id: SessionId,
        capability: WriteCapability,
    ) -> Result<Arc<Self>, EventLogError> {
        Self::open_with_mode(path, session_id, false, Some(capability), None)
    }

    pub fn open_read_only(
        path: PathBuf,
        session_id: SessionId,
    ) -> Result<Arc<Self>, EventLogError> {
        Self::open_with_mode(path, session_id, true, None, None)
    }

    fn open_with_mode(
        path: PathBuf,
        session_id: SessionId,
        read_only: bool,
        write_capability: Option<WriteCapability>,
        _test_authority: Option<WriteAuthority>,
    ) -> Result<Arc<Self>, EventLogError> {
        let loaded = load_event_jsonl(
            &path,
            if read_only {
                TornTail::Ignore
            } else {
                TornTail::Truncate
            },
        )?;
        let records = loaded.records.into_iter().map(Arc::new).collect::<Vec<_>>();
        if !matches!(
            records.first().map(|record| &record.payload),
            Some(EventPayload::SessionCreated { .. })
        ) {
            return Err(EventLogError::MissingCreation(path));
        }
        let validation =
            validate_records(&path, session_id, &records, &loaded.validation_taint, None)?;
        Ok(Arc::new(Self {
            path,
            session_id,
            append: Mutex::new(()),
            events: Mutex::new(EventStorage::new(records)),
            diagnostics: loaded.diagnostics,
            initial_validation_taint: Mutex::new(loaded.validation_taint),
            validation: Mutex::new(validation),
            next_seq: AtomicU64::new(loaded.next_seq),
            persisted: AtomicBool::new(true),
            read_only,
            write_capability,
            #[cfg(test)]
            _test_authority,
            #[cfg(test)]
            append_authorization_hook: Mutex::new(None),
            writer: Mutex::new(None),
        }))
    }

    pub(crate) fn append_owned(
        &self,
        capability: &WriteCapability,
        run_id: Option<RunId>,
        origin: EventOrigin,
        payload: EventPayload,
    ) -> Result<StoredEvent, EventLogError> {
        if !self
            .write_capability
            .as_ref()
            .is_some_and(|expected| capability.authorizes(expected))
        {
            return Err(EventLogError::ReadOnly(self.path.clone()));
        }
        #[cfg(test)]
        if let Some(hook) = self
            .append_authorization_hook
            .lock()
            .expect("append authorization hook lock poisoned")
            .take()
        {
            let _ = hook.reached.send(());
            let _ = hook.release.recv();
        }
        self.append_inner(Some(capability), run_id, origin, payload)
    }

    #[cfg(test)]
    pub fn append(
        &self,
        run_id: Option<RunId>,
        origin: EventOrigin,
        payload: EventPayload,
    ) -> Result<StoredEvent, EventLogError> {
        let capability = self
            .write_capability
            .as_ref()
            .ok_or_else(|| EventLogError::ReadOnly(self.path.clone()))?;
        self.append_owned(capability, run_id, origin, payload)
    }

    fn append_inner(
        &self,
        capability: Option<&WriteCapability>,
        run_id: Option<RunId>,
        origin: EventOrigin,
        payload: EventPayload,
    ) -> Result<StoredEvent, EventLogError> {
        if self.read_only {
            return Err(EventLogError::ReadOnly(self.path.clone()));
        }
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if capability.is_some_and(|capability| {
            !self
                .write_capability
                .as_ref()
                .is_some_and(|expected| capability.authorizes(expected))
        }) {
            return Err(EventLogError::ReadOnly(self.path.clone()));
        }
        let events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let event = StoredEvent {
            engine_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            origin: Some(origin),
            session_id: self.session_id,
            run_id,
            seq: self.next_seq.load(Ordering::Acquire),
            timestamp: Timestamp::now(),
            payload,
        };
        event.validate().map_err(|error| EventLogError::Corrupt {
            path: self.path.clone(),
            message: error.to_string(),
        })?;
        let mut record = serde_json::to_vec(&event).map_err(|source| EventLogError::Json {
            path: self.path.clone(),
            source,
        })?;
        record.push(b'\n');
        let mut validation = self
            .validation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) =
            validate_record_incremental(&self.path, self.session_id, &event, &mut validation, true)
        {
            *validation = validate_records(
                &self.path,
                self.session_id,
                &events.all,
                &self.initial_taint(),
                None,
            )?;
            return Err(error);
        }
        drop(validation);
        drop(events);
        let write_result = if self.persisted.load(Ordering::Acquire) {
            self.write_record(&record)
        } else {
            Ok(())
        };
        if let Err(error) = write_result {
            let events = self
                .events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut validation = self
                .validation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *validation = validate_records(
                &self.path,
                self.session_id,
                &events.all,
                &self.initial_taint(),
                None,
            )?;
            return Err(error);
        }
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let event = Arc::new(event);
        events.push(event.clone());
        self.next_seq.store(event.seq + 1, Ordering::Release);
        Ok(Arc::unwrap_or_clone(event))
    }

    /// Owned copies of the visible events. Deep-copies every payload; hot
    /// paths use [`Self::event_snapshot`].
    #[must_use]
    pub fn events(&self) -> Vec<StoredEvent> {
        self.event_snapshot()
            .iter()
            .map(|event| event.as_ref().clone())
            .collect()
    }

    /// The visible events. O(1): a shared view, not a copy.
    #[must_use]
    pub fn event_snapshot(&self) -> EventSnapshot {
        self.storage().snapshot()
    }

    /// Every physical event, including ones a revert hid. O(1).
    #[must_use]
    pub fn all_events(&self) -> EventSnapshot {
        self.storage().all.clone()
    }

    fn storage(&self) -> std::sync::MutexGuard<'_, EventStorage> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Physical `ModelTurnCommitted` records, including reverted ones.
    #[must_use]
    pub(crate) fn physical_model_turns(&self) -> u64 {
        self.storage().physical_model_turns
    }

    /// Visible `ModelAttemptStarted` records of `run`.
    #[must_use]
    pub(crate) fn visible_run_attempts(&self, run: RunId) -> u32 {
        self.storage()
            .index
            .run_attempts
            .get(&run)
            .copied()
            .unwrap_or_default()
    }

    /// Physical events after `cursor`, at most `limit` of them. Only the
    /// returned events are cloned, so paging through a long log costs one
    /// page per call rather than the whole log.
    #[must_use]
    pub fn events_after(
        &self,
        cursor: Option<u64>,
        limit: Option<std::num::NonZeroU32>,
    ) -> cookie_agent_protocol::EventsSubscribeResult {
        let events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let limit = limit.map_or(usize::MAX, |limit| limit.get() as usize);
        let mut after = events
            .all
            .iter()
            .filter(|event| cursor.is_none_or(|cursor| event.seq > cursor));
        let page = after
            .by_ref()
            .take(limit)
            .map(|event| event.as_ref().clone())
            .collect();
        cookie_agent_protocol::EventsSubscribeResult {
            events: page,
            has_more: after.next().is_some(),
            stale_cursor: false,
        }
    }

    /// Whether the event at `seq` is the one a reader saw at `timestamp`. A
    /// revert truncates the log and reuses sequences, so a cursor alone
    /// cannot tell the replaced event from its replacement.
    #[must_use]
    pub fn event_matches(&self, seq: u64, timestamp: Timestamp) -> bool {
        let storage = self.storage();
        storage
            .all
            .binary_search_by_key(&seq, |event| event.seq)
            .is_ok_and(|index| storage.all[index].timestamp == timestamp)
    }

    #[must_use]
    pub fn last_event(&self) -> Option<Arc<StoredEvent>> {
        self.storage().all.last().cloned()
    }

    pub(crate) fn last_run_started(
        &self,
    ) -> Option<(u64, RunId, cookie_agent_protocol::ModelSelection)> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .last_run_started
            .clone()
    }

    /// Whether the latest recorded tool names sent to the model are `names`.
    pub(crate) fn last_model_tools_match(&self, names: &[String]) -> bool {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .last_model_tools
            .as_deref()
            == Some(names)
    }

    pub(crate) fn latest_checkpoint_seq(&self) -> u64 {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .last_checkpoint_seq
    }

    pub(crate) fn latest_real_usage(&self) -> Option<(u64, u64)> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .latest_real_usage()
    }

    pub(crate) fn checkpoint_covers_input(&self, input_through_seq: u64) -> bool {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .last_checkpoint_input_through_seq
            >= input_through_seq
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[EventLoadDiagnostic] {
        &self.diagnostics
    }

    #[must_use]
    pub(crate) fn delegation_event_tainted(&self, event: &StoredEvent) -> bool {
        delegation_invocation_from_event(&event.payload).is_some_and(|invocation_id| {
            self.validation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .taint
                .delegation_before(invocation_id, event.seq)
        })
    }

    #[must_use]
    pub fn physical_tip_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Acquire).saturating_sub(1)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn is_persisted(&self) -> bool {
        self.persisted.load(Ordering::Acquire)
    }

    pub fn mark_persisted(&self) {
        self.persisted.store(true, Ordering::Release);
    }

    /// Closes the append handle; the next append reopens it.
    pub(crate) fn suspend_writer(&self) {
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }

    fn initial_taint(&self) -> ValidationTaint {
        self.initial_validation_taint
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Removes every event after `through_seq`, from the file (cut at the
    /// first later record and synced) and from memory. The removed sequence
    /// numbers are reused by the next appends. `SessionCreated` always stays.
    pub(crate) fn truncate_after(
        &self,
        capability: &WriteCapability,
        through_seq: u64,
    ) -> Result<(), EventLogError> {
        if self.read_only
            || !self
                .write_capability
                .as_ref()
                .is_some_and(|expected| capability.authorizes(expected))
        {
            return Err(EventLogError::ReadOnly(self.path.clone()));
        }
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if through_seq == 0 {
            return Err(EventLogError::Corrupt {
                path: self.path.clone(),
                message: "truncation must keep SessionCreated".into(),
            });
        }
        if through_seq >= self.physical_tip_seq() {
            return Ok(());
        }
        if self.persisted.load(Ordering::Acquire) {
            // Close the append handle first so nothing writes past the cut.
            self.writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            truncate_event_file(&self.path, through_seq)?;
        }
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events.truncate(through_seq);
        let taint = {
            let mut taint = self
                .initial_validation_taint
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            taint.truncate(through_seq);
            taint.clone()
        };
        *self
            .validation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            validate_records(&self.path, self.session_id, &events.all, &taint, None)?;
        self.next_seq.store(through_seq + 1, Ordering::Release);
        Ok(())
    }

    /// Appends one record and syncs it before returning. The caller holds
    /// `append`.
    fn write_record(&self, record: &[u8]) -> Result<(), EventLogError> {
        let mut writer = self.open_writer()?;
        writer
            .as_mut()
            .expect("event log writer initialized")
            .append(record)
    }

    fn open_writer(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Option<EventLogWriter>>, EventLogError> {
        if self.read_only {
            return Err(EventLogError::ReadOnly(self.path.clone()));
        }
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if writer.is_none() {
            *writer = Some(EventLogWriter::open(&self.path)?);
        }
        Ok(writer)
    }

    /// Blocks the next record's sync until the returned sender releases it.
    #[cfg(test)]
    pub(crate) fn install_sync_hook_for_test(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, reached_receiver) = std::sync::mpsc::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        self.open_writer()
            .expect("open event log writer")
            .as_mut()
            .expect("event log writer initialized")
            .before_sync = Some(SyncHook {
            reached,
            release: release_receiver,
        });
        (reached_receiver, release)
    }

    #[cfg(test)]
    pub(crate) fn install_append_authorization_hook_for_test(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, reached_receiver) = std::sync::mpsc::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        *self
            .append_authorization_hook
            .lock()
            .expect("append authorization hook lock poisoned") = Some(AppendAuthorizationHook {
            reached,
            release: release_receiver,
        });
        (reached_receiver, release)
    }

    #[cfg(test)]
    pub(crate) fn writer_is_open_for_test(&self) -> bool {
        self.writer
            .lock()
            .expect("event log writer lock poisoned")
            .is_some()
    }

    #[cfg(test)]
    fn snapshot_lock_available_for_test(&self) -> bool {
        self.events.try_lock().is_ok()
    }
}

#[derive(Clone, Debug, PartialEq)]
struct RunAttribution {
    start_seq: u64,
    agent_id: cookie_agent_protocol::AgentId,
    prompt_fingerprint: cookie_agent_protocol::Sha256Digest,
    selected_suffix: Vec<cookie_agent_protocol::ResolvedModelRef>,
    active_fallback_index: usize,
    next_attempt_ordinal: u32,
    attempts_on_active: u32,
    active_attempt: Option<AttemptId>,
    ordering_tainted: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct AttemptAttribution {
    run_id: RunId,
    resolved_model: cookie_agent_protocol::ResolvedModelRef,
    finished: bool,
    /// A `ModelTurnCommitted` closed this attempt.
    committed: bool,
    /// An `AttemptAbandoned` closed this attempt.
    abandoned: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct InternalRunAttribution {
    start_seq: u64,
    invocation_id: cookie_agent_protocol::InternalAgentInvocationId,
    kind: cookie_agent_protocol::InternalAgentKind,
    run_id: RunId,
    active_model: Option<cookie_agent_protocol::ResolvedModelRef>,
    usage_recorded_in_phase: bool,
    model_phase_taint_seen: u64,
    usage_phase_taint_seen: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct DelegationAttribution {
    parent_run_id: RunId,
    child_session_id: SessionId,
    resume: bool,
    started: bool,
    child_run_id: Option<RunId>,
    finished: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct ValidationState {
    taint: ValidationTaint,
    runs: HashMap<RunId, RunAttribution>,
    approval_owners: HashMap<cookie_agent_protocol::ApprovalId, RunId>,
    attempts: HashMap<AttemptId, AttemptAttribution>,
    turns: HashMap<u64, (RunId, cookie_agent_protocol::PersistedModelTurn)>,
    turn_models: HashMap<u64, cookie_agent_protocol::ResolvedModelRef>,
    usage_turns: HashSet<u64>,
    internal_runs: HashMap<cookie_agent_protocol::InternalAgentRunId, InternalRunAttribution>,
    delegations: HashMap<cookie_agent_protocol::InvocationId, DelegationAttribution>,
    model_call_owners: HashMap<(RunId, ModelCallId), AssistantToolCallRef>,
    provider_item_owners: HashMap<(RunId, ProviderItemId), AssistantToolCallRef>,
    tool_starts: HashMap<ToolCallId, (RunId, ToolCallStart)>,
    terminated_tools: HashSet<ToolCallId>,
    elided_tools: HashSet<ToolCallId>,
    admissions: HashMap<u64, (RunId, String)>,
    next_model_turn_seq: u64,
    previous_seq: Option<u64>,
    previous_timestamp: Option<Timestamp>,
    active_run: Option<RunId>,
    record_count: usize,
}

impl ValidationState {
    fn new(taint: ValidationTaint) -> Self {
        Self {
            taint,
            runs: HashMap::new(),
            approval_owners: HashMap::new(),
            attempts: HashMap::new(),
            turns: HashMap::new(),
            turn_models: HashMap::new(),
            usage_turns: HashSet::new(),
            internal_runs: HashMap::new(),
            delegations: HashMap::new(),
            model_call_owners: HashMap::new(),
            provider_item_owners: HashMap::new(),
            tool_starts: HashMap::new(),
            terminated_tools: HashSet::new(),
            elided_tools: HashSet::new(),
            admissions: HashMap::new(),
            next_model_turn_seq: 1,
            previous_seq: None,
            previous_timestamp: None,
            active_run: None,
            record_count: 0,
        }
    }

    fn finish_record(&mut self, record: &StoredEvent) {
        self.previous_seq = Some(record.seq);
        self.previous_timestamp = Some(record.timestamp);
        self.record_count += 1;
    }
}

impl Default for ValidationState {
    fn default() -> Self {
        Self::new(ValidationTaint::default())
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ValidationTaint {
    broad: Vec<(u64, u64)>,
    runs: HashMap<RunId, u64>,
    attempts: HashMap<AttemptId, u64>,
    attempt_runs: HashMap<RunId, u64>,
    turns: HashMap<u64, u64>,
    tools: HashMap<ToolCallId, u64>,
    approvals: HashMap<cookie_agent_protocol::ApprovalId, u64>,
    internal_runs: HashMap<cookie_agent_protocol::InternalAgentRunId, u64>,
    internal_model_phases: HashMap<cookie_agent_protocol::InternalAgentRunId, Vec<u64>>,
    internal_usage_phases: HashMap<cookie_agent_protocol::InternalAgentRunId, Vec<u64>>,
    tool_terminals: HashMap<ToolCallId, u64>,
    admissions: HashMap<u64, u64>,
    delegations: HashMap<cookie_agent_protocol::InvocationId, u64>,
    delegation_repairs: HashMap<cookie_agent_protocol::InvocationId, u64>,
    run_ordering: HashMap<RunId, u64>,
    turn_ordering: Option<u64>,
    active_run_ordering: Option<u64>,
}

const MAX_INTERNAL_PHASE_TAINTS_PER_RUN: usize = 64;
const INTERNAL_PHASE_TAINT_LIMIT_MESSAGE: &str =
    "internal-agent phase taint history exceeds the 64-transition per-run limit";

impl ValidationTaint {
    /// Forgets damage recorded after `through_seq`, which a truncation removed.
    fn truncate(&mut self, through_seq: u64) {
        let kept = |seq: &u64| *seq <= through_seq;
        self.broad.retain(|(start, _)| kept(start));
        for (_, end) in &mut self.broad {
            *end = (*end).min(through_seq);
        }
        self.runs.retain(|_, seq| kept(seq));
        self.attempts.retain(|_, seq| kept(seq));
        self.attempt_runs.retain(|_, seq| kept(seq));
        self.turns.retain(|_, seq| kept(seq));
        self.tools.retain(|_, seq| kept(seq));
        self.approvals.retain(|_, seq| kept(seq));
        self.internal_runs.retain(|_, seq| kept(seq));
        for phases in self
            .internal_model_phases
            .values_mut()
            .chain(self.internal_usage_phases.values_mut())
        {
            phases.retain(kept);
        }
        self.internal_model_phases
            .retain(|_, phases| !phases.is_empty());
        self.internal_usage_phases
            .retain(|_, phases| !phases.is_empty());
        self.tool_terminals.retain(|_, seq| kept(seq));
        self.admissions.retain(|_, seq| kept(seq));
        self.delegations.retain(|_, seq| kept(seq));
        self.delegation_repairs.retain(|_, seq| kept(seq));
        self.run_ordering.retain(|_, seq| kept(seq));
        self.turn_ordering = self.turn_ordering.filter(kept);
        self.active_run_ordering = self.active_run_ordering.filter(kept);
    }

    fn broad_before(&self, seq: u64) -> bool {
        self.broad.iter().any(|(start, _)| *start < seq)
    }

    fn keyed_before<K: Eq + std::hash::Hash>(
        &self,
        taints: &HashMap<K, u64>,
        key: &K,
        seq: u64,
    ) -> bool {
        self.broad_before(seq) || taints.get(key).is_some_and(|tainted| *tainted < seq)
    }

    fn run_before(&self, run_id: RunId, seq: u64) -> bool {
        self.keyed_before(&self.runs, &run_id, seq)
    }

    fn attempt_before(&self, attempt_id: AttemptId, seq: u64) -> bool {
        self.keyed_before(&self.attempts, &attempt_id, seq)
    }

    fn turn_before(&self, model_turn_seq: u64, seq: u64) -> bool {
        self.keyed_before(&self.turns, &model_turn_seq, seq)
    }

    fn tool_before(&self, tool_call_id: ToolCallId, seq: u64) -> bool {
        self.keyed_before(&self.tools, &tool_call_id, seq)
    }

    fn approval_before(&self, approval_id: cookie_agent_protocol::ApprovalId, seq: u64) -> bool {
        self.keyed_before(&self.approvals, &approval_id, seq)
    }

    fn internal_run_before(
        &self,
        internal_run_id: cookie_agent_protocol::InternalAgentRunId,
        seq: u64,
    ) -> bool {
        self.keyed_before(&self.internal_runs, &internal_run_id, seq)
    }

    fn latest_broad_before(&self, seq: u64) -> Option<u64> {
        self.broad
            .iter()
            .filter(|(start, _)| *start < seq)
            .map(|(_, end)| *end)
            .max()
    }

    fn internal_model_phase_taint_before(
        &self,
        internal_run_id: cookie_agent_protocol::InternalAgentRunId,
        seq: u64,
    ) -> Option<u64> {
        self.latest_broad_before(seq).max(
            self.internal_model_phases
                .get(&internal_run_id)
                .into_iter()
                .flatten()
                .copied()
                .filter(|tainted| *tainted < seq)
                .max(),
        )
    }

    fn internal_usage_phase_taint_before(
        &self,
        internal_run_id: cookie_agent_protocol::InternalAgentRunId,
        seq: u64,
    ) -> Option<u64> {
        self.latest_broad_before(seq).max(
            self.internal_usage_phases
                .get(&internal_run_id)
                .into_iter()
                .flatten()
                .copied()
                .filter(|tainted| *tainted < seq)
                .max(),
        )
    }

    fn tool_terminal_before(&self, tool_call_id: ToolCallId, seq: u64) -> bool {
        self.keyed_before(&self.tool_terminals, &tool_call_id, seq)
    }

    fn admission_before(&self, admission_seq: u64, seq: u64) -> bool {
        self.keyed_before(&self.admissions, &admission_seq, seq)
    }

    fn delegation_before(
        &self,
        invocation_id: cookie_agent_protocol::InvocationId,
        seq: u64,
    ) -> bool {
        self.broad_before(seq)
            || self.delegations.get(&invocation_id).is_some_and(|tainted| {
                *tainted < seq
                    && !self
                        .delegation_repairs
                        .get(&invocation_id)
                        .is_some_and(|repair| *repair >= *tainted && *repair <= seq)
            })
    }

    fn delegation_unrepaired_before(
        &self,
        invocation_id: cookie_agent_protocol::InvocationId,
        seq: u64,
    ) -> bool {
        self.broad_before(seq)
            || self
                .delegations
                .get(&invocation_id)
                .is_some_and(|tainted| *tainted < seq)
    }

    fn run_ordering_between(&self, run_id: RunId, start_seq: u64, seq: u64) -> bool {
        self.broad
            .iter()
            .any(|(start, end)| *start < seq && *end > start_seq)
            || self
                .run_ordering
                .get(&run_id)
                .is_some_and(|tainted| *tainted > start_seq && *tainted < seq)
            || self
                .attempt_runs
                .get(&run_id)
                .is_some_and(|tainted| *tainted > start_seq && *tainted < seq)
    }

    fn turn_ordering_before(&self, seq: u64) -> bool {
        self.broad_before(seq) || self.turn_ordering.is_some_and(|tainted| tainted < seq)
    }

    fn active_run_ordering_before(&self, seq: u64) -> bool {
        self.broad_before(seq)
            || self
                .active_run_ordering
                .is_some_and(|tainted| tainted < seq)
    }

    fn mark_broad(&mut self, start: u64, end: u64) {
        self.broad.push((start, end));
    }

    fn mark_record(
        &mut self,
        seq: u64,
        run_id: Option<RunId>,
        payload: &Value,
    ) -> Result<(), &'static str> {
        match payload.get("type").and_then(Value::as_str) {
            Some("run_started") => {
                if let Some(run_id) = run_id {
                    self.runs.entry(run_id).or_insert(seq);
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
                self.active_run_ordering.get_or_insert(seq);
            }
            Some("model_attempt_started") => {
                if let Some(attempt_id) = json_field(payload, "attempt_id") {
                    self.attempts.entry(attempt_id).or_insert(seq);
                }
                if let Some(run_id) = run_id {
                    self.attempt_runs.entry(run_id).or_insert(seq);
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
            }
            Some("attempt_abandoned") | Some("model_fallback") => {
                if let Some(run_id) = run_id {
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
            }
            Some("model_turn_committed") => {
                if let Some(model_turn_seq) = payload.get("model_turn_seq").and_then(Value::as_u64)
                {
                    self.turns.entry(model_turn_seq).or_insert(seq);
                }
                self.turn_ordering.get_or_insert(seq);
                if let Some(run_id) = run_id {
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
            }
            Some("tool_call_started") => {
                if let Some(tool_call_id) = json_field(payload, "tool_call_id") {
                    self.tools.entry(tool_call_id).or_insert(seq);
                }
            }
            Some("tool_call_terminated") => {
                if let Some(tool_call_id) = json_field(payload, "tool_call_id") {
                    self.tool_terminals.entry(tool_call_id).or_insert(seq);
                }
            }
            Some("approval_requested") => {
                if let Some(approval_id) = payload
                    .get("request")
                    .and_then(|request| json_field(request, "approval_id"))
                {
                    self.approvals.entry(approval_id).or_insert(seq);
                }
            }
            Some("internal_agent_started") => {
                if let Some(internal_run_id) = json_field(payload, "internal_run_id") {
                    self.internal_runs.entry(internal_run_id).or_insert(seq);
                }
            }
            Some("internal_agent_fallback") => {
                if let Some(internal_run_id) = json_field(payload, "internal_run_id") {
                    self.mark_internal_fallback(internal_run_id, seq)?;
                }
            }
            Some("internal_agent_usage_recorded") => {
                if let Some(internal_run_id) = json_field(payload, "internal_run_id") {
                    self.mark_internal_usage(internal_run_id, seq)?;
                }
            }
            Some("user_input_admitted") => {
                self.admissions.entry(seq).or_insert(seq);
            }
            Some(
                "delegation_reserved"
                | "delegation_started"
                | "delegation_run_started"
                | "delegation_run_attached"
                | "delegation_finished",
            ) => {
                if let Some(invocation_id) = delegation_invocation_from_value(payload) {
                    self.delegations.entry(invocation_id).or_insert(seq);
                }
            }
            Some("run_completed" | "run_failed" | "run_cancelled" | "run_interrupted") => {
                self.active_run_ordering.get_or_insert(seq);
            }
            _ => {}
        }
        Ok(())
    }

    fn mark_event(&mut self, event: &StoredEvent) -> Result<(), &'static str> {
        let seq = event.seq;
        match &event.payload {
            EventPayload::RunStarted { .. } => {
                if let Some(run_id) = event.run_id {
                    self.runs.entry(run_id).or_insert(seq);
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
                self.active_run_ordering.get_or_insert(seq);
            }
            EventPayload::ModelAttemptStarted { attempt_id, .. } => {
                self.attempts.entry(*attempt_id).or_insert(seq);
                if let Some(run_id) = event.run_id {
                    self.attempt_runs.entry(run_id).or_insert(seq);
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
            }
            EventPayload::AttemptAbandoned { .. } | EventPayload::ModelFallback { .. } => {
                if let Some(run_id) = event.run_id {
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
            }
            EventPayload::ModelTurnCommitted { model_turn_seq, .. } => {
                self.turns.entry(*model_turn_seq).or_insert(seq);
                self.turn_ordering.get_or_insert(seq);
                if let Some(run_id) = event.run_id {
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
            }
            EventPayload::ToolCallStarted { start } => {
                self.tools.entry(start.tool_call_id).or_insert(seq);
            }
            EventPayload::ToolCallTerminated { termination } => {
                self.tool_terminals
                    .entry(termination.tool_call_id)
                    .or_insert(seq);
            }
            EventPayload::ApprovalRequested { request } => {
                self.approvals.entry(request.approval_id()).or_insert(seq);
            }
            EventPayload::InternalAgentStarted {
                internal_run_id, ..
            } => {
                self.internal_runs.entry(*internal_run_id).or_insert(seq);
            }
            EventPayload::InternalAgentFallback {
                internal_run_id, ..
            } => {
                self.mark_internal_fallback(*internal_run_id, seq)?;
            }
            EventPayload::InternalAgentUsageRecorded {
                internal_run_id, ..
            } => {
                self.mark_internal_usage(*internal_run_id, seq)?;
            }
            EventPayload::UserInputAdmitted { .. } => {
                self.admissions.entry(seq).or_insert(seq);
            }
            EventPayload::DelegationReserved { reservation, .. } => {
                self.delegations
                    .entry(reservation.invocation_id)
                    .or_insert(seq);
            }
            EventPayload::DelegationStarted { invocation_id, .. }
            | EventPayload::DelegationRunStarted { invocation_id, .. }
            | EventPayload::DelegationRunAttached { invocation_id, .. }
            | EventPayload::DelegationFinished { invocation_id, .. } => {
                self.delegations.entry(*invocation_id).or_insert(seq);
            }
            EventPayload::RunCompleted { .. }
            | EventPayload::RunFailed { .. }
            | EventPayload::RunCancelled { .. }
            | EventPayload::RunInterrupted { .. } => {
                self.active_run_ordering.get_or_insert(seq);
            }
            _ => {}
        }
        Ok(())
    }

    fn mark_internal_fallback(
        &mut self,
        internal_run_id: cookie_agent_protocol::InternalAgentRunId,
        seq: u64,
    ) -> Result<(), &'static str> {
        let model_len = self
            .internal_model_phases
            .get(&internal_run_id)
            .map_or(0, Vec::len);
        let usage_len = self
            .internal_usage_phases
            .get(&internal_run_id)
            .map_or(0, Vec::len);
        if model_len >= MAX_INTERNAL_PHASE_TAINTS_PER_RUN
            || usage_len >= MAX_INTERNAL_PHASE_TAINTS_PER_RUN
        {
            return Err(INTERNAL_PHASE_TAINT_LIMIT_MESSAGE);
        }
        self.internal_model_phases
            .entry(internal_run_id)
            .or_default()
            .push(seq);
        self.internal_usage_phases
            .entry(internal_run_id)
            .or_default()
            .push(seq);
        Ok(())
    }

    fn mark_internal_usage(
        &mut self,
        internal_run_id: cookie_agent_protocol::InternalAgentRunId,
        seq: u64,
    ) -> Result<(), &'static str> {
        let usage = self
            .internal_usage_phases
            .entry(internal_run_id)
            .or_default();
        if usage.len() >= MAX_INTERNAL_PHASE_TAINTS_PER_RUN {
            return Err(INTERNAL_PHASE_TAINT_LIMIT_MESSAGE);
        }
        usage.push(seq);
        Ok(())
    }
}

fn json_field<T: for<'de> Deserialize<'de>>(value: &Value, field: &str) -> Option<T> {
    serde_json::from_value(value.get(field)?.clone()).ok()
}

fn delegation_invocation_from_value(
    payload: &Value,
) -> Option<cookie_agent_protocol::InvocationId> {
    match payload.get("type").and_then(Value::as_str) {
        Some("delegation_reserved") => payload
            .get("reservation")
            .and_then(|reservation| json_field(reservation, "invocation_id")),
        Some(
            "delegation_started"
            | "delegation_run_started"
            | "delegation_run_attached"
            | "delegation_finished",
        ) => json_field(payload, "invocation_id"),
        _ => None,
    }
}

fn delegation_invocation_from_event(
    payload: &EventPayload,
) -> Option<cookie_agent_protocol::InvocationId> {
    match payload {
        EventPayload::DelegationReserved { reservation, .. } => Some(reservation.invocation_id),
        EventPayload::DelegationStarted { invocation_id, .. }
        | EventPayload::DelegationRunStarted { invocation_id, .. }
        | EventPayload::DelegationRunAttached { invocation_id, .. }
        | EventPayload::DelegationFinished { invocation_id, .. } => Some(*invocation_id),
        _ => None,
    }
}

fn validate_observed_duplicates<E: Borrow<StoredEvent>>(
    path: &Path,
    records: &[E],
) -> Result<(), EventLogError> {
    let mut runs = HashSet::new();
    let mut attempts = HashSet::new();
    let mut turns = HashSet::new();
    let mut usage_turns = HashSet::new();
    let mut internal_runs = HashSet::new();
    let mut tool_starts = HashSet::new();
    let mut tool_terminations = HashSet::new();
    let mut approvals = HashSet::new();
    let mut model_calls = HashSet::new();
    let mut provider_items = HashSet::new();
    for record in records {
        let record: &StoredEvent = record.borrow();
        match &record.payload {
            EventPayload::RunStarted { .. } => {
                let Some(run_id) = record.run_id else {
                    return corrupt(path, "RunStarted is missing run_id");
                };
                if !runs.insert(run_id) {
                    return corrupt(path, "run_id has more than one RunStarted event");
                }
            }
            EventPayload::ModelAttemptStarted { attempt_id, .. } => {
                if !attempts.insert(*attempt_id) {
                    return corrupt(path, "attempt_id has more than one ModelAttemptStarted");
                }
            }
            EventPayload::ModelTurnCommitted {
                model_turn_seq,
                turn,
                ..
            } => {
                if !turns.insert(*model_turn_seq) {
                    return corrupt(path, "model turn sequence is duplicated");
                }
                if let Some(run_id) = record.run_id {
                    for part in &turn.content {
                        if let cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                            id,
                            provider_item_id,
                            ..
                        } = part
                        {
                            if !model_calls.insert((run_id, id.clone())) {
                                return corrupt(path, "model call id is reused within a run");
                            }
                            if let Some(provider_item_id) = provider_item_id
                                && !provider_items.insert((run_id, provider_item_id.clone()))
                            {
                                return corrupt(path, "provider item id is reused within a run");
                            }
                        }
                    }
                }
            }
            EventPayload::ModelUsageRecorded { model_turn_seq, .. } => {
                if !usage_turns.insert(*model_turn_seq) {
                    return corrupt(path, "usage ownership does not match its model turn");
                }
            }
            EventPayload::InternalAgentStarted {
                internal_run_id, ..
            } => {
                if !internal_runs.insert(*internal_run_id) {
                    return corrupt(path, "internal_run_id has more than one start");
                }
            }
            EventPayload::ToolCallStarted { start } => {
                if !tool_starts.insert(start.tool_call_id) {
                    return corrupt(path, "tool_call_id has more than one start");
                }
            }
            EventPayload::ToolCallTerminated { termination } => {
                if !tool_terminations.insert(termination.tool_call_id) {
                    return corrupt(path, "tool call has more than one terminal event");
                }
            }
            EventPayload::ApprovalRequested { request }
                if !approvals.insert(request.approval_id()) =>
            {
                return corrupt(
                    path,
                    "approval_id has more than one ApprovalRequested event",
                );
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_record_local(path: &Path, record: &StoredEvent) -> Result<(), EventLogError> {
    if record.payload.is_transient() {
        return corrupt(path, "live-only stream output is never stored");
    }
    match &record.payload {
        EventPayload::SessionReverted { through_seq } => {
            if record.run_id.is_some() || *through_seq == 0 || *through_seq >= record.seq {
                return corrupt(
                    path,
                    "SessionReverted target is not an existing prior event",
                );
            }
        }
        EventPayload::SessionPermissionOverlaySet { .. } if record.run_id.is_some() => {
            return corrupt(path, "SessionPermissionOverlaySet must not have run_id");
        }
        EventPayload::DelegateChildTerminated { .. } if record.run_id.is_some() => {
            return corrupt(path, "DelegateChildTerminated must not have run_id");
        }
        EventPayload::DelegatedContextSeeded { .. } if record.run_id.is_some() => {
            return corrupt(path, "DelegatedContextSeeded must be runless");
        }
        EventPayload::PluginEventAdded { .. } | EventPayload::PluginDiagnostic { .. }
            if record.run_id.is_some() =>
        {
            return corrupt(path, "plugin events must be runless");
        }
        EventPayload::SessionTitleCommitted { change, .. } => {
            let runless = matches!(
                change,
                cookie_agent_protocol::SessionTitleChange::UserSet { .. }
                    | cookie_agent_protocol::SessionTitleChange::UserClear { .. }
                    | cookie_agent_protocol::SessionTitleChange::UserReset { .. }
                    | cookie_agent_protocol::SessionTitleChange::DelegatedSet { .. }
            );
            if runless != record.run_id.is_none() {
                return corrupt(path, "SessionTitleCommitted has inconsistent run ownership");
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_records<E: Borrow<StoredEvent>>(
    path: &Path,
    session_id: SessionId,
    records: &[E],
    initial_taint: &ValidationTaint,
    strict_from_seq: Option<u64>,
) -> Result<ValidationState, EventLogError> {
    validate_observed_duplicates(path, records)?;
    let mut state = ValidationState::new(initial_taint.clone());
    for record in records {
        let record: &StoredEvent = record.borrow();
        let strict = strict_from_seq.is_some_and(|from| record.seq >= from);
        validate_record_incremental(path, session_id, record, &mut state, strict)?;
    }
    Ok(state)
}

fn validate_record_incremental(
    path: &Path,
    session_id: SessionId,
    record: &StoredEvent,
    state: &mut ValidationState,
    strict: bool,
) -> Result<(), EventLogError> {
    if state
        .previous_seq
        .is_some_and(|previous| record.seq <= previous)
    {
        return corrupt(
            path,
            format!(
                "event sequence {} is not strictly greater than {}",
                record.seq,
                state.previous_seq.expect("checked previous sequence")
            ),
        );
    }
    if record.session_id != session_id {
        return corrupt(
            path,
            "event envelope session ID does not match its directory",
        );
    }
    if state
        .previous_timestamp
        .is_some_and(|timestamp| record.timestamp < timestamp)
    {
        return corrupt(path, "event timestamps are not monotonic");
    }
    if state.record_count == 0 {
        let EventPayload::SessionCreated { .. } = &record.payload else {
            return Err(EventLogError::MissingCreation(path.to_owned()));
        };
        if record.run_id.is_some() {
            return corrupt(path, "invalid initial SessionCreated record");
        }
        state.finish_record(record);
        return Ok(());
    }
    if matches!(record.payload, EventPayload::SessionCreated { .. }) {
        return corrupt(path, "SessionCreated appeared after sequence 1");
    }
    validate_record_local(path, record)?;
    let ValidationState {
        taint,
        runs,
        approval_owners,
        attempts,
        turns,
        turn_models,
        usage_turns,
        internal_runs,
        delegations,
        model_call_owners,
        provider_item_owners,
        tool_starts,
        terminated_tools,
        elided_tools,
        admissions,
        next_model_turn_seq,
        active_run,
        ..
    } = state;
    if let EventPayload::UserInputAdmitted { input } = &record.payload
        && let Some(run_id) = record.run_id
    {
        admissions.insert(record.seq, (run_id, input.clone()));
    }
    let missing_admission = |user_input_seq: u64, run_id: RunId, input: &str| {
        !admissions
            .get(&user_input_seq)
            .is_some_and(|(owner, admitted)| *owner == run_id && admitted == input)
    };
    let tainted_prerequisite = if strict {
        false
    } else {
        match &record.payload {
            EventPayload::SkillLoaded { .. } | EventPayload::SkillInvocationNoted { .. }
                if record.run_id.is_some() =>
            {
                record.run_id.is_some_and(|run_id| {
                    !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                })
            }
            EventPayload::SessionTitleCommitted { .. } if record.run_id.is_some() => {
                record.run_id.is_some_and(|run_id| {
                    !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                })
            }
            EventPayload::UserInputRecalledV2 {
                user_input_seq,
                input,
            } => record.run_id.is_some_and(|run_id| {
                missing_admission(*user_input_seq, run_id, input)
                    && taint.admission_before(*user_input_seq, record.seq)
            }),
            EventPayload::ModelAttemptStarted { .. }
            | EventPayload::InternalAgentStarted { .. }
            | EventPayload::ModelFallback { .. }
            | EventPayload::ApprovalRequested { .. } => record.run_id.is_some_and(|run_id| {
                !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
            }),
            EventPayload::ModelRequestPrepared { attempt_id, .. }
            | EventPayload::ModelToolsPublished { attempt_id, .. }
            | EventPayload::ModelOutputStarted { attempt_id, .. }
            | EventPayload::AttemptAbandoned { attempt_id, .. }
            | EventPayload::ModelReplayEvaluated { attempt_id, .. }
            | EventPayload::ModelTurnCommitted { attempt_id, .. } => {
                (!attempts.contains_key(attempt_id)
                    && taint.attempt_before(*attempt_id, record.seq))
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            EventPayload::ModelUsageRecorded { model_turn_seq, .. } => {
                (!turns.contains_key(model_turn_seq)
                    && taint.turn_before(*model_turn_seq, record.seq))
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            EventPayload::InternalAgentFallback {
                internal_run_id, ..
            }
            | EventPayload::InternalAgentUsageRecorded {
                internal_run_id, ..
            } => {
                (!internal_runs.contains_key(internal_run_id)
                    && taint.internal_run_before(*internal_run_id, record.seq))
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            EventPayload::ToolCallStarted { start } => {
                (!turns.contains_key(&start.owner.model_turn_seq)
                    && taint.turn_before(start.owner.model_turn_seq, record.seq))
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            EventPayload::ToolCallTerminated { termination } => {
                !tool_starts.contains_key(&termination.tool_call_id)
                    && taint.tool_before(termination.tool_call_id, record.seq)
            }
            EventPayload::ToolOutputElided { tool_call_id, .. }
            | EventPayload::ToolStdinSubmitted { tool_call_id, .. }
            | EventPayload::ToolCallLinked { tool_call_id, .. } => {
                !tool_starts.contains_key(tool_call_id)
                    && taint.tool_before(*tool_call_id, record.seq)
            }
            EventPayload::ApprovalEvaluated { approval_id, .. }
            | EventPayload::ApprovalEscalated { approval_id, .. }
            | EventPayload::ApprovalUserDecisionRecorded { approval_id, .. }
            | EventPayload::ApprovalFinalized { approval_id, .. }
            | EventPayload::ApprovalCancelled { approval_id, .. }
            | EventPayload::ApprovalDoomLoopDetected { approval_id, .. } => {
                !approval_owners.contains_key(approval_id)
                    && taint.approval_before(*approval_id, record.seq)
            }
            EventPayload::TreeApprovalGrantCommitted { grant } => {
                !approval_owners.contains_key(&grant.approval_id)
                    && taint.approval_before(grant.approval_id, record.seq)
            }
            EventPayload::DelegationReserved { reservation, .. } => {
                taint.delegation_before(reservation.invocation_id, record.seq)
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            EventPayload::DelegationStarted { invocation_id, .. }
            | EventPayload::DelegationRunStarted { invocation_id, .. }
            | EventPayload::DelegationRunAttached { invocation_id, .. } => {
                taint.delegation_before(*invocation_id, record.seq)
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            EventPayload::DelegationFinished {
                invocation_id,
                child_session_id,
                child_run_id,
                ..
            } => {
                let repair_matches = delegations.get(invocation_id).is_some_and(|delegation| {
                    record.run_id == Some(delegation.parent_run_id)
                        && *child_session_id == delegation.child_session_id
                        && *child_run_id == delegation.child_run_id
                        && !delegation.finished
                });
                (taint.delegation_before(*invocation_id, record.seq) && !repair_matches)
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            _ => record.run_id.is_some_and(|run_id| {
                !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
            }),
        }
    };
    if tainted_prerequisite {
        taint
            .mark_event(record)
            .map_err(|message| EventLogError::Corrupt {
                path: path.to_owned(),
                message: message.into(),
            })?;
        state.finish_record(record);
        return Ok(());
    }
    match &record.payload {
        EventPayload::SessionReverted { through_seq } => {
            if record.run_id.is_some() || *through_seq == 0 || *through_seq >= record.seq {
                return corrupt(
                    path,
                    "SessionReverted target is not an existing prior event",
                );
            }
        }
        EventPayload::SessionPermissionOverlaySet { .. } => {
            if record.run_id.is_some() {
                return corrupt(path, "SessionPermissionOverlaySet must not have run_id");
            }
        }
        EventPayload::DelegationReserved {
            reservation,
            request,
            ..
        } => {
            if record.run_id != Some(reservation.parent_run_id)
                || record.session_id != reservation.parent_session_id
                || !runs.contains_key(&reservation.parent_run_id)
                || delegations
                    .insert(
                        reservation.invocation_id,
                        DelegationAttribution {
                            parent_run_id: reservation.parent_run_id,
                            child_session_id: reservation.child_session_id,
                            resume: request.resume_session_id.is_some(),
                            started: false,
                            child_run_id: None,
                            finished: false,
                        },
                    )
                    .is_some()
            {
                return corrupt(path, "delegation reservation ownership is invalid");
            }
        }
        EventPayload::DelegationStarted {
            invocation_id,
            child_session_id,
        } => {
            let Some(delegation) = delegations.get_mut(invocation_id) else {
                return corrupt(path, "delegation start appeared before its reservation");
            };
            if record.run_id != Some(delegation.parent_run_id)
                || *child_session_id != delegation.child_session_id
                || delegation.started
                || delegation.finished
            {
                return corrupt(path, "delegation start ownership is invalid");
            }
            delegation.started = true;
        }
        EventPayload::DelegationRunStarted {
            invocation_id,
            child_run_id,
        }
        | EventPayload::DelegationRunAttached {
            invocation_id,
            child_run_id,
        } => {
            let attached = matches!(&record.payload, EventPayload::DelegationRunAttached { .. });
            let Some(delegation) = delegations.get_mut(invocation_id) else {
                return corrupt(path, "delegation run appeared before its reservation");
            };
            if record.run_id != Some(delegation.parent_run_id)
                || delegation.child_run_id.is_some()
                || delegation.finished
                || (attached && !delegation.resume)
            {
                return corrupt(path, "delegation run ownership is invalid");
            }
            delegation.child_run_id = Some(*child_run_id);
        }
        EventPayload::DelegationFinished {
            invocation_id,
            child_session_id,
            child_run_id,
            ..
        } => {
            let Some(delegation) = delegations.get_mut(invocation_id) else {
                return corrupt(path, "delegation finish appeared before its reservation");
            };
            if record.run_id != Some(delegation.parent_run_id)
                || *child_session_id != delegation.child_session_id
                || *child_run_id != delegation.child_run_id
                || delegation.finished
            {
                return corrupt(path, "delegation finish ownership is invalid");
            }
            if taint.delegation_unrepaired_before(*invocation_id, record.seq) {
                taint.delegation_repairs.insert(*invocation_id, record.seq);
            }
            delegation.finished = true;
        }
        EventPayload::SkillLoaded { .. } | EventPayload::SkillInvocationNoted { .. } => {
            if record.run_id.is_some() {
                require_started_run(path, runs, record.run_id)?;
            }
        }
        EventPayload::DelegateChildTerminated { .. } => {
            if record.run_id.is_some() {
                return corrupt(path, "DelegateChildTerminated must not have run_id");
            }
        }
        EventPayload::UserInputAdmitted { .. } | EventPayload::UserInputRecalled { .. }
            if record.run_id.is_none() =>
        {
            if active_run.is_some() && !taint.active_run_ordering_before(record.seq) {
                return corrupt(path, "runless UserInputAdmitted requires no active run");
            }
            if taint.active_run_ordering_before(record.seq) {
                *active_run = None;
            }
        }
        EventPayload::UserInputRecalledV2 {
            user_input_seq,
            input,
        } => {
            let Some(run_id) = record.run_id else {
                return corrupt(path, "UserInputRecalledV2 is missing run_id");
            };
            if missing_admission(*user_input_seq, run_id, input) {
                return corrupt(path, "UserInputRecalledV2 target is not a prior admission");
            }
        }
        EventPayload::DelegatedContextSeeded { .. } => {
            if record.run_id.is_some() || !runs.is_empty() {
                return corrupt(
                    path,
                    "DelegatedContextSeeded must be runless and precede the first run",
                );
            }
        }
        EventPayload::RunStarted {
            agent,
            selected_suffix,
            ..
        } => {
            let Some(run_id) = record.run_id else {
                return corrupt(path, "RunStarted is missing run_id");
            };
            let attribution = RunAttribution {
                start_seq: record.seq,
                agent_id: agent.agent.clone(),
                prompt_fingerprint: agent.prompt_fingerprint.clone(),
                selected_suffix: selected_suffix
                    .iter()
                    .map(crate::policy::wire_resolved)
                    .collect(),
                active_fallback_index: 0,
                next_attempt_ordinal: 1,
                attempts_on_active: 0,
                active_attempt: None,
                ordering_tainted: false,
            };
            if runs.insert(run_id, attribution).is_some() {
                return corrupt(path, "run_id has more than one RunStarted event");
            }
            *active_run = Some(run_id);
        }
        EventPayload::SessionTitleCommitted { change, .. } => {
            let user = matches!(
                change,
                cookie_agent_protocol::SessionTitleChange::UserSet { .. }
                    | cookie_agent_protocol::SessionTitleChange::UserClear { .. }
                    | cookie_agent_protocol::SessionTitleChange::UserReset { .. }
                    | cookie_agent_protocol::SessionTitleChange::DelegatedSet { .. }
            );
            if user != record.run_id.is_none() {
                return corrupt(path, "SessionTitleCommitted has inconsistent run ownership");
            }
            if let Some(run_id) = record.run_id
                && !runs.contains_key(&run_id)
            {
                return corrupt(path, "session title references a run before RunStarted");
            }
        }
        EventPayload::ModelAttemptStarted {
            attempt_id,
            attempt_ordinal,
            fallback_index,
            retry_ordinal,
            resolved_model,
            prompt_fingerprint,
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            if attempts.contains_key(attempt_id) {
                return corrupt(path, "attempt_id has more than one ModelAttemptStarted");
            }
            let run = runs.get_mut(&run_id).expect("started run is indexed");
            run.ordering_tainted |= taint.run_ordering_between(run_id, run.start_seq, record.seq);
            if strict && run.ordering_tainted {
                return corrupt(
                    path,
                    "cannot strictly append an attempt after missing run-order prerequisites",
                );
            }
            if !run.ordering_tainted && run.active_attempt.is_some() {
                return corrupt(
                    path,
                    "ModelAttemptStarted appeared before the prior attempt ended",
                );
            }
            if !run.ordering_tainted && *attempt_ordinal != run.next_attempt_ordinal {
                return corrupt(path, "attempt_ordinal is not contiguous within its run");
            }
            let Ok(fallback_index) = usize::try_from(*fallback_index) else {
                return corrupt(path, "fallback_index does not index the frozen suffix");
            };
            if !run.ordering_tainted && fallback_index != run.active_fallback_index {
                return corrupt(
                    path,
                    "attempt fallback_index is not the active frozen suffix entry",
                );
            }
            let Some(expected_model) = run.selected_suffix.get(fallback_index) else {
                return corrupt(path, "fallback_index does not index the frozen suffix");
            };
            if resolved_model != expected_model {
                return corrupt(
                    path,
                    "attempt resolved model does not match its frozen suffix entry",
                );
            }
            if prompt_fingerprint != &run.prompt_fingerprint {
                return corrupt(path, "attempt prompt fingerprint does not match RunStarted");
            }
            if !run.ordering_tainted && *retry_ordinal != run.attempts_on_active {
                return corrupt(
                    path,
                    "retry_ordinal is not contiguous for the active fallback entry",
                );
            }
            run.next_attempt_ordinal = attempt_ordinal.saturating_add(1);
            run.active_fallback_index = fallback_index;
            run.attempts_on_active = retry_ordinal.saturating_add(1);
            run.active_attempt = Some(*attempt_id);
            attempts.insert(
                *attempt_id,
                AttemptAttribution {
                    run_id,
                    resolved_model: resolved_model.clone(),
                    finished: false,
                    committed: false,
                    abandoned: false,
                },
            );
        }
        EventPayload::ModelRequestPrepared { attempt_id, .. }
        | EventPayload::ModelToolsPublished { attempt_id, .. }
        | EventPayload::ModelOutputStarted { attempt_id, .. } => {
            validate_attempt_owner(path, attempts, *attempt_id, record.run_id)?;
        }
        EventPayload::AttemptAbandoned { attempt_id, .. } => {
            let run_id =
                validate_abandoned_attempt_owner(path, attempts, *attempt_id, record.run_id)?;
            let run = runs.get_mut(&run_id).expect("started run is indexed");
            run.ordering_tainted |= taint.run_ordering_between(run_id, run.start_seq, record.seq);
            if strict && run.ordering_tainted {
                return corrupt(
                    path,
                    "cannot strictly append an attempt terminal after missing prerequisites",
                );
            }
            finish_attempt(path, runs, attempts, run_id, *attempt_id, false)?;
        }
        EventPayload::ModelReplayEvaluated {
            attempt_id,
            resolved_model,
            base_attempt_id,
            ..
        } => {
            let run =
                validate_attempt_model(path, attempts, *attempt_id, record.run_id, resolved_model)?;
            if let Some(base) = base_attempt_id {
                validate_replay_base(path, attempts, *attempt_id, *base, run, resolved_model)?;
            }
        }
        EventPayload::ModelTurnCommitted {
            attempt_id,
            model_turn_seq,
            resolved_model,
            turn,
            ..
        } => {
            let run_id =
                validate_attempt_model(path, attempts, *attempt_id, record.run_id, resolved_model)?;
            let run = runs.get_mut(&run_id).expect("started run is indexed");
            run.ordering_tainted |= taint.run_ordering_between(run_id, run.start_seq, record.seq);
            if strict && run.ordering_tainted {
                return corrupt(
                    path,
                    "cannot strictly append a model turn after missing prerequisites",
                );
            }
            finish_attempt(path, runs, attempts, run_id, *attempt_id, true)?;
            let turn_ordering_tainted = taint.turn_ordering_before(record.seq);
            if *model_turn_seq != *next_model_turn_seq && (strict || !turn_ordering_tainted) {
                return corrupt(path, "model_turn_seq is not contiguous");
            }
            *next_model_turn_seq = model_turn_seq.saturating_add(1);
            for (content_index, part) in turn.content.iter().enumerate() {
                if let cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                    id,
                    provider_item_id,
                    ..
                } = part
                {
                    let owner = AssistantToolCallRef {
                        model_turn_seq: *model_turn_seq,
                        content_index: content_index as u32,
                        model_call_id: id.clone(),
                        provider_item_id: provider_item_id.clone(),
                    };
                    if model_call_owners
                        .insert((run_id, id.clone()), owner.clone())
                        .is_some()
                    {
                        return corrupt(path, "model call id is reused within a run");
                    }
                    if let Some(provider_item_id) = provider_item_id
                        && provider_item_owners
                            .insert((run_id, provider_item_id.clone()), owner)
                            .is_some()
                    {
                        return corrupt(path, "provider item id is reused within a run");
                    }
                }
            }
            if turns
                .insert(*model_turn_seq, (run_id, turn.clone()))
                .is_some()
            {
                return corrupt(path, "model turn sequence is duplicated");
            }
            turn_models.insert(*model_turn_seq, resolved_model.clone());
        }
        EventPayload::ModelUsageRecorded {
            model_turn_seq,
            agent_id,
            resolved_model,
            ..
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            let Some((turn_run, _)) = turns.get(model_turn_seq) else {
                return corrupt(path, "usage references an unknown committed model turn");
            };
            if *turn_run != run_id
                || turn_models.get(model_turn_seq) != Some(resolved_model)
                || runs.get(&run_id).map(|run| &run.agent_id) != Some(agent_id)
                || !usage_turns.insert(*model_turn_seq)
            {
                return corrupt(path, "usage ownership does not match its model turn");
            }
        }
        EventPayload::InternalAgentStarted {
            invocation_id,
            internal_run_id,
            kind,
            backend,
            ..
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            let active_model = match backend {
                cookie_agent_protocol::InternalAgentBackend::Model { resolved_model } => {
                    Some(resolved_model.clone())
                }
                cookie_agent_protocol::InternalAgentBackend::Builtin { .. } => None,
            };
            if internal_runs
                .insert(
                    *internal_run_id,
                    InternalRunAttribution {
                        start_seq: record.seq,
                        invocation_id: *invocation_id,
                        kind: *kind,
                        run_id,
                        active_model,
                        usage_recorded_in_phase: false,
                        model_phase_taint_seen: 0,
                        usage_phase_taint_seen: 0,
                    },
                )
                .is_some()
            {
                return corrupt(path, "internal_run_id has more than one start");
            }
        }
        EventPayload::InternalAgentFallback {
            invocation_id,
            internal_run_id,
            kind,
            from,
            to,
            ..
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            let Some(internal) = internal_runs.get_mut(internal_run_id) else {
                return corrupt(path, "internal fallback appeared before its start");
            };
            let from_model = match from {
                cookie_agent_protocol::InternalAgentBackend::Model { resolved_model } => {
                    Some(resolved_model)
                }
                cookie_agent_protocol::InternalAgentBackend::Builtin { .. } => None,
            };
            let model_phase_taint = taint
                .internal_model_phase_taint_before(*internal_run_id, record.seq)
                .filter(|tainted| {
                    *tainted > internal.start_seq && *tainted > internal.model_phase_taint_seen
                });
            if strict && model_phase_taint.is_some() {
                return corrupt(
                    path,
                    "cannot strictly append internal fallback after a missing phase transition",
                );
            }
            if internal.invocation_id != *invocation_id
                || internal.kind != *kind
                || internal.run_id != run_id
                || (model_phase_taint.is_none() && internal.active_model.as_ref() != from_model)
            {
                return corrupt(path, "internal fallback ownership does not match its run");
            }
            internal.active_model = match to {
                cookie_agent_protocol::InternalAgentBackend::Model { resolved_model } => {
                    Some(resolved_model.clone())
                }
                cookie_agent_protocol::InternalAgentBackend::Builtin { .. } => None,
            };
            internal.usage_recorded_in_phase = false;
            if let Some(tainted) = model_phase_taint {
                internal.model_phase_taint_seen = tainted;
            }
            if let Some(tainted) = taint
                .internal_usage_phase_taint_before(*internal_run_id, record.seq)
                .filter(|tainted| *tainted > internal.start_seq)
            {
                internal.usage_phase_taint_seen = tainted;
            }
        }
        EventPayload::InternalAgentUsageRecorded {
            internal_run_id,
            kind,
            agent_id,
            resolved_model,
            ..
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            let Some(internal) = internal_runs.get_mut(internal_run_id) else {
                return corrupt(path, "internal usage appeared before its start");
            };
            let expected_agent = cookie_agent_protocol::AgentId::new(match kind {
                cookie_agent_protocol::InternalAgentKind::Approval => {
                    cookie_agent_config::BUILT_IN_APPROVAL_AGENT_ID
                }
                cookie_agent_protocol::InternalAgentKind::ContextCompaction => {
                    cookie_agent_config::BUILT_IN_COMPACTION_AGENT_ID
                }
                cookie_agent_protocol::InternalAgentKind::SessionTitle => {
                    cookie_agent_config::BUILT_IN_TITLE_AGENT_ID
                }
            })
            .expect("built-in internal agent IDs are valid");
            let model_phase_taint = taint
                .internal_model_phase_taint_before(*internal_run_id, record.seq)
                .filter(|tainted| {
                    *tainted > internal.start_seq && *tainted > internal.model_phase_taint_seen
                });
            let usage_phase_taint = taint
                .internal_usage_phase_taint_before(*internal_run_id, record.seq)
                .filter(|tainted| {
                    *tainted > internal.start_seq && *tainted > internal.usage_phase_taint_seen
                });
            if strict && (model_phase_taint.is_some() || usage_phase_taint.is_some()) {
                return corrupt(
                    path,
                    "cannot strictly append internal usage after a missing phase transition",
                );
            }
            if internal.kind != *kind
                || internal.run_id != run_id
                || (model_phase_taint.is_none()
                    && internal.active_model.as_ref() != Some(resolved_model))
                || *agent_id != expected_agent
                || (usage_phase_taint.is_none() && internal.usage_recorded_in_phase)
            {
                return corrupt(path, "internal usage ownership does not match its run");
            }
            internal.active_model = Some(resolved_model.clone());
            internal.usage_recorded_in_phase = true;
            if let Some(tainted) = model_phase_taint {
                internal.model_phase_taint_seen = tainted;
            }
            if let Some(tainted) = usage_phase_taint {
                internal.usage_phase_taint_seen = tainted;
            }
        }
        EventPayload::ModelFallback {
            from,
            to,
            from_fallback_index,
            to_fallback_index,
            attempts_on_from,
            ..
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            let run = runs.get_mut(&run_id).expect("started run is indexed");
            run.ordering_tainted |= taint.run_ordering_between(run_id, run.start_seq, record.seq);
            if strict && run.ordering_tainted {
                return corrupt(
                    path,
                    "cannot strictly append fallback after missing run-order prerequisites",
                );
            }
            if !run.ordering_tainted && run.active_attempt.is_some() {
                return corrupt(
                    path,
                    "ModelFallback appeared before the active attempt ended",
                );
            }
            let Ok(from_index) = usize::try_from(*from_fallback_index) else {
                return corrupt(path, "ModelFallback index does not index the frozen suffix");
            };
            let Ok(to_index) = usize::try_from(*to_fallback_index) else {
                return corrupt(path, "ModelFallback index does not index the frozen suffix");
            };
            let Some(adjacent_index) = from_index.checked_add(1) else {
                return corrupt(path, "ModelFallback source index cannot advance");
            };
            if (!run.ordering_tainted && from_index != run.active_fallback_index)
                || to_index != adjacent_index
            {
                return corrupt(
                    path,
                    "ModelFallback transition is not adjacent from the active entry",
                );
            }
            let Some(expected_from) = run.selected_suffix.get(from_index) else {
                return corrupt(
                    path,
                    "ModelFallback source does not index the frozen suffix",
                );
            };
            let Some(expected_to) = run.selected_suffix.get(to_index) else {
                return corrupt(
                    path,
                    "ModelFallback target does not index the frozen suffix",
                );
            };
            if from != expected_from || to != expected_to {
                return corrupt(
                    path,
                    "ModelFallback models do not match the frozen suffix transition",
                );
            }
            if *attempts_on_from == 0
                || (!run.ordering_tainted && *attempts_on_from != run.attempts_on_active)
            {
                return corrupt(
                    path,
                    "ModelFallback attempt count does not match started attempts",
                );
            }
            run.active_fallback_index = to_index;
            run.attempts_on_active = 0;
        }
        EventPayload::ToolCallStarted { start } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            validate_tool_owner(
                path,
                run_id,
                turns,
                model_call_owners,
                provider_item_owners,
                &start.owner,
            )?;
            if tool_starts
                .insert(start.tool_call_id, (run_id, start.clone()))
                .is_some()
            {
                return corrupt(path, "tool_call_id has more than one start");
            }
        }
        EventPayload::ToolCallTerminated { termination } => {
            let Some((run_id, start)) = tool_starts.get(&termination.tool_call_id) else {
                return corrupt(path, "tool termination appeared before its start");
            };
            if record.run_id != Some(*run_id) || !termination.matches_start(start) {
                return corrupt(path, "tool termination ownership does not match its start");
            }
            if strict && taint.tool_terminal_before(termination.tool_call_id, record.seq) {
                return corrupt(
                    path,
                    "cannot strictly append tool termination after a missing terminal transition",
                );
            }
            if !terminated_tools.insert(termination.tool_call_id) {
                return corrupt(path, "tool call has more than one terminal event");
            }
        }
        EventPayload::ToolOutputElided { tool_call_id, .. } => {
            let Some((run_id, _)) = tool_starts.get(tool_call_id) else {
                return corrupt(path, "tool elision appeared before its start");
            };
            let terminal_tainted = taint.tool_terminal_before(*tool_call_id, record.seq);
            if record.run_id != Some(*run_id)
                || (!terminated_tools.contains(tool_call_id) && !terminal_tainted)
                || (strict && terminal_tainted)
                || !elided_tools.insert(*tool_call_id)
            {
                return corrupt(path, "tool elision ownership or ordering is invalid");
            }
        }
        EventPayload::ToolStdinSubmitted { tool_call_id, .. }
        | EventPayload::ToolCallLinked { tool_call_id, .. } => {
            let Some((run_id, _)) = tool_starts.get(tool_call_id) else {
                return corrupt(path, "tool lifecycle event appeared before its start");
            };
            let terminal_tainted = taint.tool_terminal_before(*tool_call_id, record.seq);
            if record.run_id != Some(*run_id)
                || terminated_tools.contains(tool_call_id)
                || (strict && terminal_tainted)
            {
                return corrupt(
                    path,
                    "tool lifecycle event has invalid ownership or ordering",
                );
            }
        }
        EventPayload::ApprovalRequested { request } => {
            let Some(run_id) = record.run_id else {
                return corrupt(path, "ApprovalRequested is missing run_id");
            };
            if !runs.contains_key(&run_id) {
                return corrupt(path, "approval references a run before RunStarted");
            }
            if approval_owners
                .insert(request.approval_id(), run_id)
                .is_some()
            {
                return corrupt(
                    path,
                    "approval_id has more than one ApprovalRequested event",
                );
            }
        }
        EventPayload::ApprovalEvaluated { approval_id, .. }
        | EventPayload::ApprovalEscalated { approval_id, .. }
        | EventPayload::ApprovalUserDecisionRecorded { approval_id, .. }
        | EventPayload::ApprovalFinalized { approval_id, .. }
        | EventPayload::ApprovalCancelled { approval_id, .. }
        | EventPayload::ApprovalDoomLoopDetected { approval_id, .. } => {
            validate_approval_owner(path, approval_owners, *approval_id, record.run_id)?;
        }
        EventPayload::TreeApprovalGrantCommitted { grant } => {
            validate_approval_owner(path, approval_owners, grant.approval_id, record.run_id)?;
        }
        EventPayload::PluginEventAdded { .. } | EventPayload::PluginDiagnostic { .. } => {
            if record.run_id.is_some() {
                return corrupt(path, "plugin events must be runless");
            }
        }
        EventPayload::GoalActivated { .. }
        | EventPayload::GoalChecklistRevised { .. }
        | EventPayload::GoalLifecycleChanged { .. }
        | EventPayload::ProducerMessageAccepted { .. }
        | EventPayload::ProducerMessageDiscarded { .. } => {
            if record.run_id.is_some() {
                require_started_run(path, runs, record.run_id)?;
            }
        }
        _ => {
            require_started_run(path, runs, record.run_id)?;
        }
    }
    if matches!(
        record.payload,
        EventPayload::RunCompleted { .. }
            | EventPayload::RunFailed { .. }
            | EventPayload::RunCancelled { .. }
            | EventPayload::RunInterrupted { .. }
    ) && record.run_id == *active_run
    {
        *active_run = None;
    }
    state.finish_record(record);
    Ok(())
}

struct LoadedEvents {
    records: Vec<StoredEvent>,
    diagnostics: Vec<EventLoadDiagnostic>,
    validation_taint: ValidationTaint,
    next_seq: u64,
}

/// Wire tags of the live-only payloads ([`EventPayload::is_transient`]).
const TRANSIENT_PAYLOAD_TAGS: [&str; 3] = ["text_delta", "reasoning_delta", "tool_call_progress"];

/// The sequence of a record an older engine stored for live-only stream
/// output, found without decoding its payload. The envelope's `payload` is
/// its only object-valued field and the payload's `type` tag is serialized
/// first, so the first `"payload":{"type":"` in the line is the envelope's.
/// Anything else, including a record this cannot parse, is `None` and goes
/// through the ordinary readers.
fn legacy_transient_record_seq(line: &[u8]) -> Option<u64> {
    const PAYLOAD_TAG: &[u8] = br#""payload":{"type":""#;
    #[derive(Deserialize)]
    struct Sequence {
        seq: u64,
    }
    let tag_start = line
        .windows(PAYLOAD_TAG.len())
        .position(|window| window == PAYLOAD_TAG)?
        + PAYLOAD_TAG.len();
    let tag = &line[tag_start..];
    TRANSIENT_PAYLOAD_TAGS
        .iter()
        .any(|transient| {
            tag.strip_prefix(transient.as_bytes())
                .is_some_and(|rest| rest.first() == Some(&b'"'))
        })
        .then(|| serde_json::from_slice::<Sequence>(line).ok())
        .flatten()
        .map(|record| record.seq)
}

/// Checks that a record's sequence advances the log and records any gap
/// before it.
fn observe_sequence(
    path: &Path,
    seq: u64,
    last_observed_seq: &mut u64,
    diagnostics: &mut Vec<EventLoadDiagnostic>,
    validation_taint: &mut ValidationTaint,
) -> Result<(), EventLogError> {
    if seq <= *last_observed_seq {
        return corrupt_value(path, "event sequences are not strictly increasing");
    }
    if *last_observed_seq > 0 && seq > *last_observed_seq + 1 {
        let start = *last_observed_seq + 1;
        let end = seq - 1;
        diagnostics.push(EventLoadDiagnostic {
            seq: start,
            reason: if start == end {
                "event sequence is absent from the physical log".into()
            } else {
                format!("event sequences {start}..={end} are absent from the physical log")
            },
            engine_version: None,
            skipped: true,
        });
        validation_taint.mark_broad(start, end);
    }
    *last_observed_seq = seq;
    Ok(())
}

fn load_event_jsonl(path: &Path, torn_tail: TornTail) -> Result<LoadedEvents, EventLogError> {
    let bytes = read_complete_jsonl(path, torn_tail)?;
    let mut records = Vec::new();
    let mut diagnostics = Vec::new();
    let mut validation_taint = ValidationTaint::default();
    let mut last_observed_seq = 0_u64;
    for (index, line) in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let line_number = index as u64 + 1;
        // Older engines stored live-only stream output. It is not history:
        // its sequence still orders the log, but the record is dropped
        // without decoding its payload.
        if index > 0
            && let Some(seq) = legacy_transient_record_seq(line)
        {
            observe_sequence(
                path,
                seq,
                &mut last_observed_seq,
                &mut diagnostics,
                &mut validation_taint,
            )?;
            continue;
        }
        // Nearly every record is intact: type it straight from the bytes and
        // leave the untyped, repairing reader to the rest.
        if let Some(event) = StoredEvent::decode_strict(line) {
            observe_sequence(
                path,
                event.seq,
                &mut last_observed_seq,
                &mut diagnostics,
                &mut validation_taint,
            )?;
            records.push(event);
            continue;
        }
        let mut value = match serde_json::from_slice::<serde_json::Value>(line) {
            Ok(value) => value,
            Err(error) => {
                if index == 0 {
                    return corrupt_value(
                        path,
                        format!("SessionCreated line is unreadable: {error}"),
                    );
                }
                diagnostics.push(EventLoadDiagnostic {
                    seq: line_number,
                    reason: format!("corrupt JSON line: {error}"),
                    engine_version: None,
                    skipped: true,
                });
                validation_taint.mark_broad(line_number, line_number);
                continue;
            }
        };
        let Some(object) = value.as_object_mut() else {
            if index == 0 {
                return corrupt_value(path, "SessionCreated line is not a JSON object");
            }
            diagnostics.push(EventLoadDiagnostic {
                seq: line_number,
                reason: "event envelope is not a JSON object".into(),
                engine_version: None,
                skipped: true,
            });
            validation_taint.mark_broad(line_number, line_number);
            continue;
        };
        let seq = object
            .get("seq")
            .and_then(Value::as_u64)
            .unwrap_or(line_number);
        let engine_version = object
            .get("engine_version")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let envelope_run_id = object
            .get("run_id")
            .and_then(|value| RunId::deserialize(value).ok());
        if object.get("seq").and_then(Value::as_u64).is_some() {
            observe_sequence(
                path,
                seq,
                &mut last_observed_seq,
                &mut diagnostics,
                &mut validation_taint,
            )?;
        }
        if index > 0
            && object
                .get("payload")
                .and_then(|payload| payload.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|tag| TRANSIENT_PAYLOAD_TAGS.contains(&tag))
        {
            continue;
        }
        let unknown = object
            .keys()
            .filter(|key| {
                !matches!(
                    key.as_str(),
                    "engine_version"
                        | "origin"
                        | "event_schema_version"
                        | "session_id"
                        | "run_id"
                        | "seq"
                        | "timestamp"
                        | "payload"
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            if index == 0 {
                return corrupt_value(
                    path,
                    format!(
                        "SessionCreated envelope has unknown fields: {}",
                        unknown.join(", ")
                    ),
                );
            }
            diagnostics.push(EventLoadDiagnostic {
                seq,
                reason: format!("unknown envelope fields: {}", unknown.join(", ")),
                engine_version,
                skipped: true,
            });
            if let Some(payload) = object.get("payload") {
                validation_taint
                    .mark_record(seq, envelope_run_id, payload)
                    .map_err(|message| EventLogError::Corrupt {
                        path: path.to_owned(),
                        message: message.into(),
                    })?;
            } else {
                validation_taint.mark_broad(seq, seq);
            }
            continue;
        }
        let mut degraded = Vec::new();
        if object
            .get("engine_version")
            .is_some_and(|version| !version.is_null() && !version.is_string())
        {
            object.remove("engine_version");
            degraded.push("engine_version".to_owned());
        }
        let Some(payload_value) = object.remove("payload") else {
            if index == 0 {
                return corrupt_value(path, "SessionCreated payload is absent");
            }
            diagnostics.push(EventLoadDiagnostic {
                seq,
                reason: "required payload is absent".into(),
                engine_version,
                skipped: true,
            });
            validation_taint.mark_broad(seq, seq);
            continue;
        };
        let payload = match deserialize_event_payload_best_effort(&payload_value) {
            Ok(read) => {
                degraded.extend(
                    read.degraded_fields
                        .into_iter()
                        .map(|field| format!("payload.{field}")),
                );
                read.payload
            }
            Err(reason) => {
                if index == 0 {
                    return corrupt_value(
                        path,
                        format!("SessionCreated is unsupported or corrupt: {reason}"),
                    );
                }
                diagnostics.push(EventLoadDiagnostic {
                    seq,
                    reason,
                    engine_version,
                    skipped: true,
                });
                validation_taint
                    .mark_record(seq, envelope_run_id, &payload_value)
                    .map_err(|message| EventLogError::Corrupt {
                        path: path.to_owned(),
                        message: message.into(),
                    })?;
                continue;
            }
        };
        drop(payload_value);
        // The envelope is decoded without its payload, which is already typed:
        // no line is re-serialized or decoded a second time.
        let event = match StoredEventEnvelope::deserialize(&value) {
            Ok(envelope) => {
                let event = envelope.with_payload(payload);
                match event.validate() {
                    Ok(()) => Ok(event),
                    Err(error) => Err((error.to_string(), event.payload)),
                }
            }
            Err(error) => Err((error.to_string(), payload)),
        };
        match event {
            Ok(event) => {
                if !degraded.is_empty() {
                    diagnostics.push(EventLoadDiagnostic {
                        seq,
                        reason: format!("degraded optional fields: {}", degraded.join(", ")),
                        engine_version: event.engine_version.clone(),
                        skipped: false,
                    });
                }
                records.push(event);
            }
            Err((error, payload)) => {
                if index == 0 {
                    return corrupt_value(
                        path,
                        format!("SessionCreated is unsupported or corrupt: {error}"),
                    );
                }
                diagnostics.push(EventLoadDiagnostic {
                    seq,
                    reason: error,
                    engine_version,
                    skipped: true,
                });
                let payload = serde_json::to_value(payload).expect("event payload serializes");
                validation_taint
                    .mark_record(seq, envelope_run_id, &payload)
                    .map_err(|message| EventLogError::Corrupt {
                        path: path.to_owned(),
                        message: message.into(),
                    })?;
            }
        }
    }
    let observed_tip = last_observed_seq.max(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.skipped)
            .map(|diagnostic| diagnostic.seq)
            .max()
            .unwrap_or(0),
    );
    Ok(LoadedEvents {
        records,
        diagnostics,
        validation_taint,
        next_seq: observed_tip.saturating_add(1).max(1),
    })
}

fn require_started_run(
    path: &Path,
    runs: &HashMap<RunId, RunAttribution>,
    run_id: Option<RunId>,
) -> Result<RunId, EventLogError> {
    let Some(run_id) = run_id else {
        return corrupt_value(path, "run-owned event is missing run_id");
    };
    if !runs.contains_key(&run_id) {
        return corrupt_value(path, "event references a run before RunStarted");
    }
    Ok(run_id)
}

fn validate_attempt_owner(
    path: &Path,
    attempts: &HashMap<AttemptId, AttemptAttribution>,
    attempt_id: AttemptId,
    run_id: Option<RunId>,
) -> Result<RunId, EventLogError> {
    let Some(attempt) = attempts.get(&attempt_id) else {
        return corrupt_value(path, "attempt event appeared before ModelAttemptStarted");
    };
    if run_id != Some(attempt.run_id) {
        return corrupt_value(path, "attempt event uses a non-owning run_id");
    }
    if attempt.finished {
        return corrupt_value(
            path,
            "attempt lifecycle event appeared after its terminal event",
        );
    }
    Ok(attempt.run_id)
}

/// `AttemptAbandoned` may follow the commit that already closed its attempt.
///
/// An interrupted model attempt commits its partial turn (`ModelTurnCommitted`,
/// which closes the attempt) and then records the abandonment, so the abandon
/// of an already-committed attempt is terminal-isolation for the commit rather
/// than a second terminal event. Every other lifecycle event stays strict.
fn validate_abandoned_attempt_owner(
    path: &Path,
    attempts: &HashMap<AttemptId, AttemptAttribution>,
    attempt_id: AttemptId,
    run_id: Option<RunId>,
) -> Result<RunId, EventLogError> {
    let Some(attempt) = attempts.get(&attempt_id) else {
        return corrupt_value(path, "attempt event appeared before ModelAttemptStarted");
    };
    if run_id != Some(attempt.run_id) {
        return corrupt_value(path, "attempt event uses a non-owning run_id");
    }
    if attempt.finished && (attempt.abandoned || !attempt.committed) {
        return corrupt_value(
            path,
            "attempt lifecycle event appeared after its terminal event",
        );
    }
    Ok(attempt.run_id)
}

fn validate_attempt_model(
    path: &Path,
    attempts: &HashMap<AttemptId, AttemptAttribution>,
    attempt_id: AttemptId,
    run_id: Option<RunId>,
    resolved_model: &cookie_agent_protocol::ResolvedModelRef,
) -> Result<RunId, EventLogError> {
    let owner = validate_attempt_owner(path, attempts, attempt_id, run_id)?;
    if attempts
        .get(&attempt_id)
        .is_none_or(|attempt| &attempt.resolved_model != resolved_model)
    {
        return corrupt_value(path, "attempt resolved model changed within its lifecycle");
    }
    Ok(owner)
}

/// A delta replay evaluation must extend an earlier attempt of its own run
/// that used the same model.
fn validate_replay_base(
    path: &Path,
    attempts: &HashMap<AttemptId, AttemptAttribution>,
    attempt_id: AttemptId,
    base: AttemptId,
    run_id: RunId,
    resolved_model: &cookie_agent_protocol::ResolvedModelRef,
) -> Result<(), EventLogError> {
    if base == attempt_id
        || attempts
            .get(&base)
            .is_none_or(|base| base.run_id != run_id || &base.resolved_model != resolved_model)
    {
        return corrupt_value(
            path,
            "replay evaluation base is not an earlier attempt of its run and model",
        );
    }
    Ok(())
}

fn finish_attempt(
    path: &Path,
    runs: &mut HashMap<RunId, RunAttribution>,
    attempts: &mut HashMap<AttemptId, AttemptAttribution>,
    run_id: RunId,
    attempt_id: AttemptId,
    committed: bool,
) -> Result<(), EventLogError> {
    let attempt = attempts
        .get_mut(&attempt_id)
        .expect("validated attempt is indexed");
    let already_committed = attempt.committed;
    attempt.finished = true;
    attempt.committed |= committed;
    attempt.abandoned |= !committed;
    let run = runs.get_mut(&run_id).expect("started run is indexed");
    if !run.ordering_tainted && run.active_attempt != Some(attempt_id) {
        // `ModelTurnCommitted` already closed a committed attempt and cleared
        // `active_attempt`, so the abandon an interrupt writes afterwards is
        // expected rather than an ordering violation.
        let abandoned_after_commit =
            !committed && already_committed && run.active_attempt.is_none();
        if !abandoned_after_commit {
            return corrupt(
                path,
                "attempt terminal event is inconsistent with the active attempt",
            );
        }
    }
    run.active_attempt = None;
    if committed {
        run.attempts_on_active = 0;
    }
    Ok(())
}

fn validate_tool_owner(
    path: &Path,
    run_id: RunId,
    turns: &HashMap<u64, (RunId, cookie_agent_protocol::PersistedModelTurn)>,
    model_call_owners: &HashMap<(RunId, ModelCallId), AssistantToolCallRef>,
    provider_item_owners: &HashMap<(RunId, ProviderItemId), AssistantToolCallRef>,
    owner: &AssistantToolCallRef,
) -> Result<(), EventLogError> {
    let Some((turn_run, turn)) = turns.get(&owner.model_turn_seq) else {
        return corrupt(
            path,
            "tool owner references an unknown committed model turn",
        );
    };
    let Some(cookie_agent_protocol::PersistedAssistantPart::ToolCall {
        id,
        provider_item_id,
        ..
    }) = turn.content.get(owner.content_index as usize)
    else {
        return corrupt(
            path,
            "tool owner content index is not a committed tool call",
        );
    };
    if *turn_run != run_id
        || id != &owner.model_call_id
        || provider_item_id != &owner.provider_item_id
        || model_call_owners.get(&(run_id, id.clone())) != Some(owner)
        || provider_item_id.as_ref().is_some_and(|provider_id| {
            provider_item_owners.get(&(run_id, provider_id.clone())) != Some(owner)
        })
    {
        return corrupt(
            path,
            "tool owner does not match the committed model content",
        );
    }
    Ok(())
}

fn validate_approval_owner(
    path: &Path,
    owners: &HashMap<cookie_agent_protocol::ApprovalId, cookie_agent_protocol::RunId>,
    approval_id: cookie_agent_protocol::ApprovalId,
    event_run_id: Option<cookie_agent_protocol::RunId>,
) -> Result<(), EventLogError> {
    let Some(owner) = owners.get(&approval_id) else {
        return corrupt(
            path,
            "approval lifecycle event appeared before ApprovalRequested",
        );
    };
    if event_run_id != Some(*owner) {
        return corrupt(path, "approval lifecycle event uses a non-owning run_id");
    }
    Ok(())
}

fn corrupt<T>(path: &Path, message: impl Into<String>) -> Result<T, EventLogError> {
    Err(EventLogError::Corrupt {
        path: path.to_owned(),
        message: message.into(),
    })
}

fn corrupt_value<T>(path: &Path, message: impl Into<String>) -> Result<T, EventLogError> {
    Err(EventLogError::Corrupt {
        path: path.to_owned(),
        message: message.into(),
    })
}

/// Reads a JSONL file after removing a crash-torn final record.  A malformed
/// complete record is corruption, not a torn tail, and is rejected.
pub fn load_jsonl<T>(path: &Path) -> Result<Vec<T>, EventLogError>
where
    T: for<'de> Deserialize<'de>,
{
    load_jsonl_with_policy(path, TornTail::Truncate)
}

pub fn load_jsonl_shared<T>(path: &Path) -> Result<Vec<T>, EventLogError>
where
    T: for<'de> Deserialize<'de>,
{
    load_jsonl_with_policy(path, TornTail::Ignore)
}

fn load_jsonl_with_policy<T>(path: &Path, torn_tail: TornTail) -> Result<Vec<T>, EventLogError>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = read_complete_jsonl(path, torn_tail)?;
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line).map_err(|source| EventLogError::Json {
                path: path.to_owned(),
                source,
            })
        })
        .collect()
}

fn read_complete_jsonl(path: &Path, torn_tail: TornTail) -> Result<Vec<u8>, EventLogError> {
    let mut bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(EventLogError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        let length = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        bytes.truncate(length);
        if matches!(torn_tail, TornTail::Truncate) {
            let file = OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|source| EventLogError::Io {
                    path: path.to_owned(),
                    source,
                })?;
            file.set_len(length as u64)
                .and_then(|()| file.sync_all())
                .map_err(|source| EventLogError::Io {
                    path: path.to_owned(),
                    source,
                })?;
        }
    }
    Ok(bytes)
}

/// Cuts `path` at the first record whose sequence is past `through_seq` and
/// syncs it. Records keep their bytes; lines without a sequence before the cut
/// stay too, as the loader already tolerated them.
fn truncate_event_file(path: &Path, through_seq: u64) -> Result<(), EventLogError> {
    #[derive(Deserialize)]
    struct Sequenced {
        seq: u64,
    }
    let io_error = |source| EventLogError::Io {
        path: path.to_owned(),
        source,
    };
    let bytes = fs::read(path).map_err(io_error)?;
    let mut length = 0_usize;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if serde_json::from_slice::<Sequenced>(line).is_ok_and(|record| record.seq > through_seq) {
            break;
        }
        length += line.len();
    }
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(io_error)?;
    file.set_len(length as u64)
        .and_then(|()| file.sync_all())
        .map_err(io_error)
}

pub fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<(), EventLogError> {
    let bytes = serde_json::to_vec(value).map_err(|source| EventLogError::Json {
        path: path.to_owned(),
        source,
    })?;
    let created = !path.exists();
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| EventLogError::Io {
        path: path.to_owned(),
        source,
    })?;
    file.write_all(&bytes)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_data())
        .map_err(|source| EventLogError::Io {
            path: path.to_owned(),
            source,
        })?;
    if created && let Some(parent) = path.parent() {
        fsync_directory(parent)?;
    }
    Ok(())
}

pub(crate) fn append_copied_event_jsonl(
    path: &Path,
    event: &StoredEvent,
) -> Result<(), EventLogError> {
    append_jsonl(path, event)
}

#[cfg(unix)]
pub fn fsync_directory(path: &Path) -> Result<(), EventLogError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| EventLogError::Io {
            path: path.to_owned(),
            source,
        })
}

#[cfg(windows)]
pub fn fsync_directory(path: &Path) -> Result<(), EventLogError> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .and_then(|directory| match directory.sync_all() {
            Ok(()) => Ok(()),
            Err(error) if matches!(error.raw_os_error(), Some(1 | 5 | 6 | 50)) => Ok(()),
            Err(error) => Err(error),
        })
        .map_err(|source| EventLogError::Io {
            path: path.to_owned(),
            source,
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputMessage {
    Delta(OutputDelta),
    Gap(OutputGap),
}

#[derive(Clone, Debug)]
struct Chunk {
    offset: u64,
    data: Vec<u8>,
}

#[derive(Debug, Default)]
struct StreamBuffer {
    start: u64,
    end: u64,
    chunks: VecDeque<Chunk>,
}

#[derive(Debug)]
struct Subscriber {
    sender: mpsc::Sender<OutputMessage>,
    gap: Option<u64>,
}

#[derive(Debug)]
struct HubState {
    streams: std::collections::HashMap<String, StreamBuffer>,
    subscribers: std::collections::HashMap<String, Vec<Subscriber>>,
    declaration: Vec<OutputStream>,
    finalized: bool,
}

/// Bounded retained output with atomic snapshot-to-live subscription handoff.
#[derive(Clone, Debug)]
pub struct OutputHub {
    call_id: ToolCallId,
    limit: usize,
    state: Arc<Mutex<HubState>>,
}

impl OutputHub {
    #[must_use]
    pub fn new(call_id: ToolCallId, retention_bytes: usize) -> Self {
        Self {
            call_id,
            limit: retention_bytes,
            state: Arc::new(Mutex::new(HubState {
                streams: std::collections::HashMap::new(),
                subscribers: std::collections::HashMap::new(),
                declaration: vec![OutputStream::Stdout, OutputStream::Stderr],
                finalized: false,
            })),
        }
    }

    pub fn declare(&self, declaration: &cookie_agent_protocol::ToolOutputDeclaration) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.declaration = declaration
            .channels()
            .iter()
            .map(|name| OutputStream::from_channel(name.as_deref()))
            .collect();
    }

    pub fn streams(&self) -> Vec<OutputStream> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .declaration
            .clone()
    }

    pub fn emit(&self, stream: OutputStream, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.finalized {
            return;
        }
        let index = stream.name().to_owned();
        let (delta, end) = {
            let buffer = state.streams.entry(index.clone()).or_default();
            let delta = OutputDelta {
                call_id: self.call_id,
                stream: stream.clone(),
                byte_offset: buffer.end,
                data: STANDARD.encode(data),
            };
            buffer.chunks.push_back(Chunk {
                offset: buffer.end,
                data: data.to_vec(),
            });
            buffer.end += data.len() as u64;
            while buffer
                .chunks
                .iter()
                .map(|chunk| chunk.data.len())
                .sum::<usize>()
                > self.limit
            {
                if let Some(chunk) = buffer.chunks.pop_front() {
                    buffer.start = chunk.offset + chunk.data.len() as u64;
                }
            }
            (delta, buffer.end)
        };
        let subscribers = state.subscribers.entry(index).or_default();
        subscribers.retain_mut(|subscriber| {
            if let Some(next_offset) = subscriber.gap {
                match subscriber.sender.try_send(OutputMessage::Gap(OutputGap {
                    call_id: self.call_id,
                    stream: stream.clone(),
                    next_offset,
                })) {
                    Ok(()) => subscriber.gap = None,
                    Err(mpsc::error::TrySendError::Full(_)) => return true,
                    Err(mpsc::error::TrySendError::Closed(_)) => return false,
                }
            }
            match subscriber
                .sender
                .try_send(OutputMessage::Delta(delta.clone()))
            {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    subscriber.gap = Some(end);
                    true
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }

    /// Holds the same lock while taking the snapshot and registering the
    /// receiver, so subsequent deltas cannot fall into a handoff gap.
    #[must_use]
    pub fn subscribe(
        &self,
        stream: OutputStream,
        queue: usize,
    ) -> (OutputSnapshot, mpsc::Receiver<OutputMessage>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = stream.name().to_owned();
        let buffer = state.streams.entry(index.clone()).or_default();
        let snapshot = OutputSnapshot {
            call_id: self.call_id,
            start_offset: buffer.start,
            end_offset: buffer.end,
            chunks: buffer
                .chunks
                .iter()
                .map(|chunk| OutputDelta {
                    call_id: self.call_id,
                    stream: stream.clone(),
                    byte_offset: chunk.offset,
                    data: STANDARD.encode(&chunk.data),
                })
                .collect(),
        };
        let (sender, receiver) = mpsc::channel(queue);
        // A nonzero retained start is an explicit loss boundary for a new
        // snapshot consumer. Queue the same marker used for lagging live
        // subscribers before any later deltas can be registered.
        let gap = (buffer.start > 0).then_some(buffer.start);
        let gap = match gap {
            Some(next_offset) => match sender.try_send(OutputMessage::Gap(OutputGap {
                call_id: self.call_id,
                stream: stream.clone(),
                next_offset,
            })) {
                Ok(()) => None,
                Err(mpsc::error::TrySendError::Full(_)) => Some(next_offset),
                Err(mpsc::error::TrySendError::Closed(_)) => None,
            },
            None => None,
        };
        // A retained finalized hub is a snapshot-only resource. Do not retain a
        // sender for a receiver which can never receive another delta.
        if !state.finalized {
            state
                .subscribers
                .entry(index)
                .or_default()
                .push(Subscriber { sender, gap });
        }
        (snapshot, receiver)
    }

    /// Prevents late producer clones from publishing after the owning call has
    /// been committed as complete and closes every live subscription.
    pub fn finalize(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.finalized = true;
        state.subscribers.clear();
    }
}

#[cfg(test)]
mod tests;
