//! Buffered/durable session event logs and ephemeral tool-output hubs.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::fs::File;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::{
    AssistantToolCallRef, AttemptId, EventOrigin, EventPayload, ModelCallId, OutputDelta,
    OutputGap, OutputSnapshot, OutputStream, ProviderItemId, RunId, SessionId, StoredEvent,
    ToolCallId, ToolCallStart, deserialize_event_payload_best_effort,
};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

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
}

#[derive(Debug)]
pub struct EventLog {
    path: PathBuf,
    session_id: SessionId,
    append: Mutex<()>,
    events: Mutex<EventStorage>,
    diagnostics: Vec<EventLoadDiagnostic>,
    initial_validation_taint: ValidationTaint,
    validation: Mutex<ValidationState>,
    next_seq: AtomicU64,
    persisted: AtomicBool,
    writer: Mutex<Option<Arc<EventLogWriter>>>,
}

const EVENT_SYNC_WINDOW: Duration = Duration::from_millis(8);
const EVENT_SYNC_BYTES: usize = 32 * 1024;

#[derive(Debug)]
struct EventLogWriter {
    shared: Arc<EventLogWriterShared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug)]
struct EventLogWriterShared {
    path: PathBuf,
    state: Mutex<EventLogWriterState>,
    wake: Condvar,
    #[cfg(test)]
    before_sync: Mutex<Option<SyncHook>>,
}

#[derive(Debug)]
struct EventLogWriterState {
    output: BufWriter<fs::File>,
    unsynced_bytes: usize,
    sync_deadline: Option<Instant>,
    directory_sync_pending: bool,
    background_error: Option<WriterFailure>,
    #[cfg(test)]
    background_sync_paused: bool,
    shutdown: bool,
}

#[derive(Clone, Debug)]
struct WriterFailure {
    kind: io::ErrorKind,
    message: String,
}

#[cfg(test)]
#[derive(Debug)]
struct SyncHook {
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
            _ => {}
        }
    }

    fn latest_real_usage(&self) -> Option<(u64, u64)> {
        self.last_recorded_usage.or(self.last_turn_usage)
    }
}

#[derive(Debug)]
struct EventStorage {
    all: Vec<StoredEvent>,
    visible: Vec<usize>,
    visible_ceiling: u64,
    snapshot: Option<Arc<[StoredEvent]>>,
    index: EventIndex,
}

impl EventStorage {
    fn new(all: Vec<StoredEvent>) -> Self {
        let mut storage = Self {
            all,
            visible: Vec::new(),
            visible_ceiling: u64::MAX,
            snapshot: None,
            index: EventIndex::default(),
        };
        storage.rebuild_visible();
        storage
    }

    fn push(&mut self, event: StoredEvent) {
        let revert = match &event.payload {
            EventPayload::SessionReverted { through_seq } => Some(*through_seq),
            _ => None,
        };
        let index = self.all.len();
        self.all.push(event);
        if let Some(through_seq) = revert {
            self.visible_ceiling = self.visible_ceiling.min(through_seq);
            let all = &self.all;
            self.visible
                .retain(|candidate| all[*candidate].seq <= self.visible_ceiling);
            self.visible.push(index);
            self.rebuild_index();
        } else {
            self.index.observe(&self.all[index]);
            self.visible.push(index);
        }
        self.snapshot = None;
    }

    fn rebuild_visible(&mut self) {
        self.visible.clear();
        self.visible_ceiling = u64::MAX;
        for (index, event) in self.all.iter().enumerate() {
            if let EventPayload::SessionReverted { through_seq } = &event.payload {
                self.visible_ceiling = self.visible_ceiling.min(*through_seq);
                let all = &self.all;
                self.visible
                    .retain(|candidate| all[*candidate].seq <= self.visible_ceiling);
            }
            self.visible.push(index);
        }
        self.rebuild_index();
        self.snapshot = None;
    }

    fn rebuild_index(&mut self) {
        self.index = EventIndex::default();
        for index in &self.visible {
            self.index.observe(&self.all[*index]);
        }
    }

    fn snapshot(&mut self) -> Arc<[StoredEvent]> {
        self.snapshot
            .get_or_insert_with(|| {
                Arc::from(
                    self.visible
                        .iter()
                        .map(|index| self.all[*index].clone())
                        .collect::<Vec<_>>(),
                )
            })
            .clone()
    }
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
    fn open(path: &Path) -> Result<Arc<Self>, EventLogError> {
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
        let shared = Arc::new(EventLogWriterShared {
            path: path.to_owned(),
            state: Mutex::new(EventLogWriterState {
                output: BufWriter::new(file),
                unsynced_bytes: 0,
                sync_deadline: None,
                directory_sync_pending: created,
                background_error: None,
                #[cfg(test)]
                background_sync_paused: false,
                shutdown: false,
            }),
            wake: Condvar::new(),
            #[cfg(test)]
            before_sync: Mutex::new(None),
        });
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("event-log-sync".into())
            .spawn(move || event_log_sync_worker(&worker_shared))
            .map_err(|source| EventLogError::Io {
                path: path.to_owned(),
                source,
            })?;
        Ok(Arc::new(Self {
            shared,
            worker: Mutex::new(Some(worker)),
        }))
    }

    fn append(&self, bytes: &[u8], barrier: bool) -> Result<(), EventLogError> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(failure) = state.background_error.take() {
            return Err(self.shared.failure(failure));
        }
        state
            .output
            .write_all(bytes)
            .and_then(|()| state.output.write_all(b"\n"))
            .map_err(|source| self.shared.io_error(source))?;
        state.unsynced_bytes = state.unsynced_bytes.saturating_add(bytes.len() + 1);
        if barrier || state.unsynced_bytes >= EVENT_SYNC_BYTES {
            self.shared.sync(&mut state)?;
        } else if state.sync_deadline.is_none() {
            state.sync_deadline = Some(Instant::now() + EVENT_SYNC_WINDOW);
            self.shared.wake.notify_one();
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), EventLogError> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(failure) = state.background_error.take() {
            return Err(self.shared.failure(failure));
        }
        if state.unsynced_bytes > 0 || state.directory_sync_pending {
            self.shared.sync(&mut state)?;
        }
        Ok(())
    }

    fn shutdown(&self) {
        {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.shutdown = true;
            self.shared.wake.notify_one();
        }
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }

    #[cfg(test)]
    fn install_sync_hook(&self) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, reached_receiver) = std::sync::mpsc::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        *self
            .shared
            .before_sync
            .lock()
            .expect("event log sync hook lock poisoned") = Some(SyncHook {
            reached,
            release: release_receiver,
        });
        (reached_receiver, release)
    }

    #[cfg(test)]
    fn pause_background_sync(&self) {
        self.shared
            .state
            .lock()
            .expect("event log writer state lock poisoned")
            .background_sync_paused = true;
        self.shared.wake.notify_one();
    }
}

impl EventLogWriterShared {
    fn sync(&self, state: &mut EventLogWriterState) -> Result<(), EventLogError> {
        #[cfg(test)]
        if let Some(hook) = self
            .before_sync
            .lock()
            .expect("event log sync hook lock poisoned")
            .take()
        {
            let _ = hook.reached.send(());
            let _ = hook.release.recv();
        }
        state
            .output
            .flush()
            .and_then(|()| state.output.get_ref().sync_data())
            .map_err(|source| self.io_error(source))?;
        if state.directory_sync_pending
            && let Some(parent) = self.path.parent()
        {
            fsync_directory(parent)?;
            state.directory_sync_pending = false;
        }
        state.unsynced_bytes = 0;
        state.sync_deadline = None;
        Ok(())
    }

    fn io_error(&self, source: io::Error) -> EventLogError {
        EventLogError::Io {
            path: self.path.clone(),
            source,
        }
    }

    fn failure(&self, failure: WriterFailure) -> EventLogError {
        self.io_error(io::Error::new(failure.kind, failure.message))
    }
}

fn event_log_sync_worker(shared: &EventLogWriterShared) {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        if state.shutdown {
            if state.unsynced_bytes > 0 || state.directory_sync_pending {
                let _ = shared.sync(&mut state);
            }
            return;
        }
        #[cfg(test)]
        if state.background_sync_paused {
            state = shared
                .wake
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            continue;
        }
        let Some(deadline) = state.sync_deadline else {
            state = shared
                .wake
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            continue;
        };
        let now = Instant::now();
        if now < deadline {
            let (next_state, _) = shared
                .wake
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next_state;
            continue;
        }
        if let Err(EventLogError::Io { source, .. }) = shared.sync(&mut state) {
            state.background_error = Some(WriterFailure {
                kind: source.kind(),
                message: source.to_string(),
            });
            state.sync_deadline = None;
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
    pub fn create(
        path: PathBuf,
        session_id: SessionId,
        origin: EventOrigin,
        creation: EventPayload,
    ) -> Result<Arc<Self>, EventLogError> {
        let log = Arc::new(Self {
            path,
            session_id,
            append: Mutex::new(()),
            events: Mutex::new(EventStorage::new(Vec::new())),
            diagnostics: Vec::new(),
            initial_validation_taint: ValidationTaint::default(),
            validation: Mutex::new(ValidationState::default()),
            next_seq: AtomicU64::new(1),
            persisted: AtomicBool::new(true),
            writer: Mutex::new(None),
        });
        if !matches!(creation, EventPayload::SessionCreated { .. }) {
            return Err(EventLogError::MissingCreation(log.path.clone()));
        }
        log.append_inner(None, origin, creation)?;
        Ok(log)
    }

    pub fn create_buffered(
        path: PathBuf,
        session_id: SessionId,
        origin: EventOrigin,
        creation: EventPayload,
    ) -> Result<Arc<Self>, EventLogError> {
        let log = Arc::new(Self {
            path,
            session_id,
            append: Mutex::new(()),
            events: Mutex::new(EventStorage::new(Vec::new())),
            diagnostics: Vec::new(),
            initial_validation_taint: ValidationTaint::default(),
            validation: Mutex::new(ValidationState::default()),
            next_seq: AtomicU64::new(1),
            persisted: AtomicBool::new(false),
            writer: Mutex::new(None),
        });
        if !matches!(creation, EventPayload::SessionCreated { .. }) {
            return Err(EventLogError::MissingCreation(log.path.clone()));
        }
        log.append_inner(None, origin, creation)?;
        Ok(log)
    }

    pub fn open(path: PathBuf, session_id: SessionId) -> Result<Arc<Self>, EventLogError> {
        let loaded = load_event_jsonl(&path)?;
        let records = loaded.records;
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
            initial_validation_taint: loaded.validation_taint,
            validation: Mutex::new(validation),
            next_seq: AtomicU64::new(loaded.next_seq),
            persisted: AtomicBool::new(true),
            writer: Mutex::new(None),
        }))
    }

    pub fn append(
        &self,
        run_id: Option<RunId>,
        origin: EventOrigin,
        payload: EventPayload,
    ) -> Result<StoredEvent, EventLogError> {
        self.append_inner(run_id, origin, payload)
    }

    fn append_inner(
        &self,
        run_id: Option<RunId>,
        origin: EventOrigin,
        payload: EventPayload,
    ) -> Result<StoredEvent, EventLogError> {
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let bytes = serde_json::to_vec(&event).map_err(|source| EventLogError::Json {
            path: self.path.clone(),
            source,
        })?;
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
                &self.initial_validation_taint,
                None,
            )?;
            return Err(error);
        }
        drop(validation);
        drop(events);
        let write_result = if self.persisted.load(Ordering::Acquire) {
            self.persistent_writer().and_then(|writer| {
                writer.append(&bytes, event_requires_durable_barrier(&event.payload))
            })
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
                &self.initial_validation_taint,
                None,
            )?;
            return Err(error);
        }
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events.push(event.clone());
        self.next_seq.store(event.seq + 1, Ordering::Release);
        Ok(event)
    }

    #[must_use]
    pub fn events(&self) -> Vec<StoredEvent> {
        self.event_snapshot().to_vec()
    }

    #[must_use]
    pub fn event_snapshot(&self) -> Arc<[StoredEvent]> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot()
    }

    #[must_use]
    pub fn all_events(&self) -> Vec<StoredEvent> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .all
            .to_vec()
    }

    #[must_use]
    pub fn last_event(&self) -> Option<StoredEvent> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .all
            .last()
            .cloned()
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

    pub(crate) fn flush(&self) -> Result<(), EventLogError> {
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(writer) = writer {
            writer.flush()?;
        }
        Ok(())
    }

    pub(crate) fn suspend_writer(&self) -> Result<(), EventLogError> {
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(writer) = writer {
            let result = writer.flush();
            writer.shutdown();
            result?;
        }
        Ok(())
    }

    fn persistent_writer(&self) -> Result<Arc<EventLogWriter>, EventLogError> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if writer.is_none() {
            *writer = Some(EventLogWriter::open(&self.path)?);
        }
        Ok(writer
            .as_ref()
            .expect("event log writer initialized")
            .clone())
    }

    #[cfg(test)]
    pub(crate) fn install_sync_hook_for_test(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        self.persistent_writer()
            .expect("open event log writer")
            .install_sync_hook()
    }

    #[cfg(test)]
    pub(crate) fn pause_background_sync_for_test(&self) {
        self.persistent_writer()
            .expect("open event log writer")
            .pause_background_sync();
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

impl Drop for EventLog {
    fn drop(&mut self) {
        if let Some(writer) = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            writer.shutdown();
        }
    }
}

fn event_requires_durable_barrier(payload: &EventPayload) -> bool {
    match payload {
        EventPayload::TextDelta { .. }
        | EventPayload::ReasoningDelta { .. }
        | EventPayload::ToolCallProgress { .. } => false,
        EventPayload::SessionCreated { .. }
        | EventPayload::SessionReverted { .. }
        | EventPayload::SessionPermissionOverlaySet { .. }
        | EventPayload::SkillLoaded { .. }
        | EventPayload::SkillInvocationNoted { .. }
        | EventPayload::PluginEventAdded { .. }
        | EventPayload::PluginDiagnostic { .. }
        | EventPayload::RunStarted { .. }
        | EventPayload::MessageInjected { .. }
        | EventPayload::UserInputAdmitted { .. }
        | EventPayload::UserInputSubmitted { .. }
        | EventPayload::UserInputTransformed { .. }
        | EventPayload::UserInputRecalled { .. }
        | EventPayload::UserInputRecalledV2 { .. }
        | EventPayload::UserInputApplied { .. }
        | EventPayload::DelegatedContextSeeded { .. }
        | EventPayload::RunCompleted { .. }
        | EventPayload::RunFailed { .. }
        | EventPayload::RunCancelled { .. }
        | EventPayload::RunInterrupted { .. }
        | EventPayload::ModelAttemptStarted { .. }
        | EventPayload::ModelRequestPrepared { .. }
        | EventPayload::AttemptAbandoned { .. }
        | EventPayload::ModelReplayEvaluated { .. }
        | EventPayload::ModelTurnCommitted { .. }
        | EventPayload::ModelUsageRecorded { .. }
        | EventPayload::ModelFallback { .. }
        | EventPayload::ToolCallStarted { .. }
        | EventPayload::ToolCallTerminated { .. }
        | EventPayload::ToolOutputElided { .. }
        | EventPayload::ToolStdinSubmitted { .. }
        | EventPayload::ToolCallLinked { .. }
        | EventPayload::DelegationReserved { .. }
        | EventPayload::DelegationStarted { .. }
        | EventPayload::DelegationRunStarted { .. }
        | EventPayload::DelegationRunAttached { .. }
        | EventPayload::DelegationFinished { .. }
        | EventPayload::DelegateQueued { .. }
        | EventPayload::DelegateFinished { .. }
        | EventPayload::DelegateFinishedV2 { .. }
        | EventPayload::DelegateChildTerminated { .. }
        | EventPayload::ApprovalRequested { .. }
        | EventPayload::ApprovalEvaluated { .. }
        | EventPayload::ApprovalEscalated { .. }
        | EventPayload::ApprovalUserDecisionRecorded { .. }
        | EventPayload::ApprovalFinalized { .. }
        | EventPayload::ApprovalCancelled { .. }
        | EventPayload::ApprovalDoomLoopDetected { .. }
        | EventPayload::TreeApprovalGrantCommitted { .. }
        | EventPayload::InternalAgentStarted { .. }
        | EventPayload::InternalAgentCompleted { .. }
        | EventPayload::InternalAgentUsageRecorded { .. }
        | EventPayload::InternalAgentFailed { .. }
        | EventPayload::InternalAgentCancelled { .. }
        | EventPayload::InternalAgentInterrupted { .. }
        | EventPayload::InternalAgentFallback { .. }
        | EventPayload::ContextCheckpointCommitted { .. }
        | EventPayload::ContextRehydrated { .. }
        | EventPayload::SessionTitleCommitted { .. } => true,
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

fn validate_observed_duplicates(path: &Path, records: &[StoredEvent]) -> Result<(), EventLogError> {
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

fn validate_records(
    path: &Path,
    session_id: SessionId,
    records: &[StoredEvent],
    initial_taint: &ValidationTaint,
    strict_from_seq: Option<u64>,
) -> Result<ValidationState, EventLogError> {
    validate_observed_duplicates(path, records)?;
    let mut state = ValidationState::new(initial_taint.clone());
    for record in records {
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
            EventPayload::TextDelta { attempt_id, .. }
            | EventPayload::ReasoningDelta { attempt_id, .. }
            | EventPayload::ModelRequestPrepared { attempt_id, .. }
            | EventPayload::AttemptAbandoned { attempt_id }
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
            | EventPayload::ToolCallProgress { tool_call_id, .. }
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
                },
            );
        }
        EventPayload::ModelRequestPrepared { attempt_id, .. }
        | EventPayload::TextDelta { attempt_id, .. }
        | EventPayload::ReasoningDelta { attempt_id, .. } => {
            validate_attempt_owner(path, attempts, *attempt_id, record.run_id)?;
        }
        EventPayload::AttemptAbandoned { attempt_id } => {
            let run_id = validate_attempt_owner(path, attempts, *attempt_id, record.run_id)?;
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
            ..
        } => {
            validate_attempt_model(path, attempts, *attempt_id, record.run_id, resolved_model)?;
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
        EventPayload::ToolCallProgress { tool_call_id, .. }
        | EventPayload::ToolStdinSubmitted { tool_call_id, .. }
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

fn load_event_jsonl(path: &Path) -> Result<LoadedEvents, EventLogError> {
    let bytes = read_complete_jsonl(path)?;
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
            .and_then(|value| serde_json::from_value(value.clone()).ok());
        if object.get("seq").and_then(Value::as_u64).is_some() {
            if seq <= last_observed_seq {
                return corrupt_value(path, "event sequences are not strictly increasing");
            }
            if last_observed_seq > 0 && seq > last_observed_seq + 1 {
                let start = last_observed_seq + 1;
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
            last_observed_seq = seq;
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
        let Some(payload_value) = object.get("payload").cloned() else {
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
        match deserialize_event_payload_best_effort(payload_value.clone()) {
            Ok(read) => {
                degraded.extend(
                    read.degraded_fields
                        .into_iter()
                        .map(|field| format!("payload.{field}")),
                );
                object.insert(
                    "payload".into(),
                    serde_json::to_value(read.payload).expect("event payload serializes"),
                );
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
        }
        let final_payload = object.get("payload").cloned();
        match serde_json::from_value::<StoredEvent>(value) {
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
            Err(error) => {
                if index == 0 {
                    return corrupt_value(
                        path,
                        format!("SessionCreated is unsupported or corrupt: {error}"),
                    );
                }
                diagnostics.push(EventLoadDiagnostic {
                    seq,
                    reason: error.to_string(),
                    engine_version,
                    skipped: true,
                });
                if let Some(payload) = final_payload.as_ref() {
                    validation_taint
                        .mark_record(seq, envelope_run_id, payload)
                        .map_err(|message| EventLogError::Corrupt {
                            path: path.to_owned(),
                            message: message.into(),
                        })?;
                } else {
                    validation_taint.mark_broad(seq, seq);
                }
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
    attempt.finished = true;
    let run = runs.get_mut(&run_id).expect("started run is indexed");
    if !run.ordering_tainted && run.active_attempt != Some(attempt_id) {
        return corrupt(
            path,
            "attempt terminal event is inconsistent with the active attempt",
        );
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
    let bytes = read_complete_jsonl(path)?;
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

fn read_complete_jsonl(path: &Path) -> Result<Vec<u8>, EventLogError> {
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
    Ok(bytes)
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
    streams: [StreamBuffer; 2],
    subscribers: [Vec<Subscriber>; 2],
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
                streams: [StreamBuffer::default(), StreamBuffer::default()],
                subscribers: [Vec::new(), Vec::new()],
                finalized: false,
            })),
        }
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
        let index = stream_index(stream);
        let (delta, end) = {
            let buffer = &mut state.streams[index];
            let delta = OutputDelta {
                call_id: self.call_id,
                stream,
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
        let subscribers = &mut state.subscribers[index];
        subscribers.retain_mut(|subscriber| {
            if let Some(next_offset) = subscriber.gap {
                match subscriber.sender.try_send(OutputMessage::Gap(OutputGap {
                    call_id: self.call_id,
                    stream,
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
        let index = stream_index(stream);
        let buffer = &mut state.streams[index];
        let snapshot = OutputSnapshot {
            call_id: self.call_id,
            start_offset: buffer.start,
            end_offset: buffer.end,
            chunks: buffer
                .chunks
                .iter()
                .map(|chunk| OutputDelta {
                    call_id: self.call_id,
                    stream,
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
                stream,
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
            state.subscribers[index].push(Subscriber { sender, gap });
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
        state.subscribers = [Vec::new(), Vec::new()];
    }
}

const fn stream_index(stream: OutputStream) -> usize {
    match stream {
        OutputStream::Stdout => 0,
        OutputStream::Stderr => 1,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

    use cookie_agent_protocol::{
        AgentId, AgentMode, AgentRevision, ApprovalReasonCode, ApprovalTrigger, ArtifactReference,
        AssistantToolCallRef, AttemptId, CatalogRevision, ClientRunId, CwdIdentity,
        DelegateRequestPayload, DelegationReservation, EventPayload, FrozenModelBinding,
        InternalAgentBackend, InternalAgentFailure, InternalAgentInvocationId, InternalAgentKind,
        InternalAgentRunId, InvocationId, ModelCallId, ModelErrorKind, ModelErrorStage,
        ModelErrorSummary, ModelFinishReason, ModelKey, ModelRevision, OutputStream,
        PermissionAction, PermissionEffect, PersistedAssistantPart, PersistedModelTurn,
        ProviderStateRevision, RecipeRegistryRevision, RunId, RunSelection, RuntimeRevision,
        SafeCode, SafeDisplayText, SafeErrorMessage, SafeInternalAgentCall, SafeToolError,
        SessionId, SessionOrigin, SessionStatus, SessionTitle, Sha256Digest, StoredEvent,
        ToolCallId, ToolCallPresentation, ToolCallStart, ToolCallTermination,
        ToolTerminationOutcome, Usage, VariantId,
    };
    use serde_json::Value;
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::*;
    use crate::{
        policy::wire_resolved,
        test_support::{agent_snapshot, model_binding_named, run_selection},
    };

    fn runtime_revision() -> RuntimeRevision {
        RuntimeRevision::new(format!("sha256:{}", "1".repeat(64))).expect("runtime revision")
    }

    fn catalog_revision() -> CatalogRevision {
        CatalogRevision::new(format!("sha256:{}", "2".repeat(64))).expect("catalog revision")
    }

    fn provider_revision() -> ProviderStateRevision {
        ProviderStateRevision::new(format!("sha256:{}", "3".repeat(64))).expect("provider revision")
    }

    fn model_revision() -> ModelRevision {
        ModelRevision::new(format!("sha256:{}", "4".repeat(64))).expect("model revision")
    }

    fn agent_revision() -> AgentRevision {
        AgentRevision::new(format!("sha256:{}", "5".repeat(64))).expect("agent revision")
    }

    fn registry_revision() -> RecipeRegistryRevision {
        RecipeRegistryRevision::new(format!("sha256:{}", "6".repeat(64)))
            .expect("registry revision")
    }

    fn stored_event() -> StoredEvent {
        let session_id = SessionId(Uuid::from_u128(99));
        let agent = agent_snapshot("test", AgentMode::Primary);
        StoredEvent {
            engine_version: None,
            origin: None,
            session_id,
            run_id: None,
            seq: 1,
            timestamp: jiff::Timestamp::new(1, 0).expect("timestamp"),
            payload: EventPayload::SessionCreated {
                origin: SessionOrigin::Root,
                cwd_identity: CwdIdentity::new("workspace:test").expect("cwd identity"),
                creation_selection: run_selection("test"),
                manifest_revision: agent.fallback_chain[0].manifest_revision.clone(),
                creation_agent: Box::new(agent),
                runtime_revision: runtime_revision(),
                catalog_revision: catalog_revision(),
                provider_state_revision: provider_revision(),
                model_revision: model_revision(),
                agent_revision: agent_revision(),
                recipe_registry_revision: registry_revision(),
            },
        }
    }

    fn event(
        session_id: SessionId,
        run_id: Option<RunId>,
        seq: u64,
        payload: EventPayload,
    ) -> StoredEvent {
        StoredEvent {
            engine_version: None,
            origin: None,
            session_id,
            run_id,
            seq,
            timestamp: jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        }
    }

    fn fallback_binding(model_id: &str) -> FrozenModelBinding {
        model_binding_named(model_id)
    }

    fn fallback_error() -> ModelErrorSummary {
        ModelErrorSummary {
            kind: ModelErrorKind::RateLimited,
            message: SafeErrorMessage::new("rate limited").expect("safe error"),
            retryable: true,
            stage: ModelErrorStage::ResponseHeaders,
            http_status: Some(429),
            bytes_received: 0,
            vendor_code: None,
            request_id: None,
            retry_after_ms: Some(100),
        }
    }

    fn orphan_termination(tool_call_id: ToolCallId) -> EventPayload {
        EventPayload::ToolCallTerminated {
            termination: ToolCallTermination {
                tool_call_id,
                owner: AssistantToolCallRef {
                    model_turn_seq: 999,
                    content_index: 0,
                    model_call_id: ModelCallId::new("orphan").expect("model call id"),
                    provider_item_id: None,
                },
                outcome: ToolTerminationOutcome::Failed,
                result: None,
                error: Some(SafeToolError {
                    code: SafeCode::new("orphan").expect("safe code"),
                    message: SafeErrorMessage::new("orphan termination").expect("safe error"),
                }),
            },
        }
    }

    fn write_event_values(path: &Path, values: &[Value]) {
        let contents = values
            .iter()
            .map(|value| serde_json::to_string(value).expect("event value"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(path, contents).expect("write event values");
    }

    fn push_run_event(
        records: &mut Vec<StoredEvent>,
        session_id: SessionId,
        run_id: RunId,
        payload: EventPayload,
    ) {
        let seq = records.len() as u64 + 1;
        records.push(event(session_id, Some(run_id), seq, payload));
    }

    fn push_abandoned_attempt(
        records: &mut Vec<StoredEvent>,
        session_id: SessionId,
        run_id: RunId,
        suffix: &[FrozenModelBinding],
        prompt_fingerprint: &Sha256Digest,
        attempt: (u128, u32, usize, u32),
    ) {
        let (id, attempt_ordinal, fallback_index, retry_ordinal) = attempt;
        let attempt_id = AttemptId(Uuid::from_u128(id));
        push_run_event(
            records,
            session_id,
            run_id,
            EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal,
                fallback_index: fallback_index as u32,
                retry_ordinal,
                resolved_model: wire_resolved(&suffix[fallback_index]),
                prompt_fingerprint: prompt_fingerprint.clone(),
            },
        );
        push_run_event(
            records,
            session_id,
            run_id,
            EventPayload::AttemptAbandoned { attempt_id },
        );
    }

    fn attribution_records() -> Vec<StoredEvent> {
        let creation = stored_event();
        let session_id = creation.session_id;
        let run_id = RunId(Uuid::from_u128(200));
        let suffix = [
            fallback_binding("fallback-zero"),
            fallback_binding("fallback-one"),
            fallback_binding("fallback-two"),
        ];
        let mut agent = agent_snapshot("test", AgentMode::Primary);
        agent.fallback_chain = suffix.to_vec();
        let selection = RunSelection {
            agent: agent.agent.clone(),
            model: suffix[0].selection.clone(),
            preset: None,
        };
        let prompt_fingerprint = agent.prompt_fingerprint.clone();
        let mut records = vec![
            creation,
            event(
                session_id,
                Some(run_id),
                2,
                EventPayload::RunStarted {
                    client_run_id: ClientRunId::new("strict-attribution").expect("client run id"),
                    selection,
                    agent: Box::new(agent),
                    runtime_revision: runtime_revision(),
                    catalog_revision: catalog_revision(),
                    provider_state_revision: provider_revision(),
                    model_revision: model_revision(),
                    agent_revision: agent_revision(),
                    recipe_registry_revision: registry_revision(),
                    manifest_revision: suffix[0].manifest_revision.clone(),
                    selected_suffix: suffix.to_vec(),
                    internal_agents: Vec::new(),
                    input_through_seq: 1,
                },
            ),
        ];
        push_abandoned_attempt(
            &mut records,
            session_id,
            run_id,
            &suffix,
            &prompt_fingerprint,
            (1, 1, 0, 0),
        );
        push_abandoned_attempt(
            &mut records,
            session_id,
            run_id,
            &suffix,
            &prompt_fingerprint,
            (2, 2, 0, 1),
        );
        push_run_event(
            &mut records,
            session_id,
            run_id,
            EventPayload::ModelFallback {
                from: wire_resolved(&suffix[0]),
                to: wire_resolved(&suffix[1]),
                from_fallback_index: 0,
                to_fallback_index: 1,
                attempts_on_from: 2,
                error: fallback_error(),
            },
        );
        push_abandoned_attempt(
            &mut records,
            session_id,
            run_id,
            &suffix,
            &prompt_fingerprint,
            (3, 3, 1, 0),
        );
        push_abandoned_attempt(
            &mut records,
            session_id,
            run_id,
            &suffix,
            &prompt_fingerprint,
            (4, 4, 1, 1),
        );
        push_run_event(
            &mut records,
            session_id,
            run_id,
            EventPayload::ModelFallback {
                from: wire_resolved(&suffix[1]),
                to: wire_resolved(&suffix[2]),
                from_fallback_index: 1,
                to_fallback_index: 2,
                attempts_on_from: 2,
                error: fallback_error(),
            },
        );
        push_abandoned_attempt(
            &mut records,
            session_id,
            run_id,
            &suffix,
            &prompt_fingerprint,
            (5, 5, 2, 0),
        );
        records
    }

    // Copied from 0384899fe647c26aa54932b1ca92fc42b6d3668e, the parent of a05eedc9;
    // only the function name and rustfmt layout differ from the historical validator.
    fn reference_validate_records(
        path: &Path,
        session_id: SessionId,
        records: &[StoredEvent],
        initial_taint: &ValidationTaint,
        strict_from_seq: Option<u64>,
    ) -> Result<ValidationTaint, EventLogError> {
        validate_observed_duplicates(path, records)?;
        let mut taint = initial_taint.clone();
        let mut runs = HashMap::<RunId, RunAttribution>::new();
        let mut approval_owners = HashMap::new();
        let mut attempts = HashMap::<AttemptId, AttemptAttribution>::new();
        let mut turns = HashMap::<u64, (RunId, cookie_agent_protocol::PersistedModelTurn)>::new();
        let mut turn_models = HashMap::<u64, cookie_agent_protocol::ResolvedModelRef>::new();
        let mut usage_turns = HashSet::<u64>::new();
        let mut internal_runs =
            HashMap::<cookie_agent_protocol::InternalAgentRunId, InternalRunAttribution>::new();
        let mut delegations =
            HashMap::<cookie_agent_protocol::InvocationId, DelegationAttribution>::new();
        let mut model_call_owners = HashMap::<(RunId, ModelCallId), AssistantToolCallRef>::new();
        let mut provider_item_owners =
            HashMap::<(RunId, ProviderItemId), AssistantToolCallRef>::new();
        let mut tool_starts = HashMap::<ToolCallId, (RunId, ToolCallStart)>::new();
        let mut terminated_tools = HashSet::<ToolCallId>::new();
        let mut elided_tools = HashSet::<ToolCallId>::new();
        let mut next_model_turn_seq = 1_u64;
        let mut previous_timestamp = None;
        let mut active_run = None;
        for (index, record) in records.iter().enumerate() {
            if index > 0 && record.seq <= records[index - 1].seq {
                return corrupt(
                    path,
                    format!(
                        "event sequence {} is not strictly greater than {}",
                        record.seq,
                        records[index - 1].seq
                    ),
                );
            }
            if record.session_id != session_id {
                return corrupt(
                    path,
                    "event envelope session ID does not match its directory",
                );
            }
            if previous_timestamp.is_some_and(|timestamp| record.timestamp < timestamp) {
                return corrupt(path, "event timestamps are not monotonic");
            }
            previous_timestamp = Some(record.timestamp);
            if index == 0 {
                let EventPayload::SessionCreated { .. } = &record.payload else {
                    return Err(EventLogError::MissingCreation(path.to_owned()));
                };
                if record.run_id.is_some() {
                    return corrupt(path, "invalid initial SessionCreated record");
                }
                continue;
            }
            if matches!(record.payload, EventPayload::SessionCreated { .. }) {
                return corrupt(path, "SessionCreated appeared after sequence 1");
            }
            validate_record_local(path, record)?;
            let strict = strict_from_seq.is_some_and(|from| record.seq >= from);
            let missing_admission = |user_input_seq: u64, run_id: RunId, input: &str| {
                !records[..index].iter().any(|prior| {
                    prior.seq == user_input_seq
                        && prior.run_id == Some(run_id)
                        && matches!(
                            &prior.payload,
                            EventPayload::UserInputAdmitted { input: admitted } if admitted == input
                        )
                })
            };
            let tainted_prerequisite = if strict {
                false
            } else {
                match &record.payload {
                    EventPayload::SkillLoaded { .. }
                    | EventPayload::SkillInvocationNoted { .. }
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
                    | EventPayload::ApprovalRequested { .. } => {
                        record.run_id.is_some_and(|run_id| {
                            !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                        })
                    }
                    EventPayload::TextDelta { attempt_id, .. }
                    | EventPayload::ReasoningDelta { attempt_id, .. }
                    | EventPayload::ModelRequestPrepared { attempt_id, .. }
                    | EventPayload::AttemptAbandoned { attempt_id }
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
                    | EventPayload::ToolCallProgress { tool_call_id, .. }
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
                        let repair_matches =
                            delegations.get(invocation_id).is_some_and(|delegation| {
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
                continue;
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
                    let attached =
                        matches!(&record.payload, EventPayload::DelegationRunAttached { .. });
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
                        require_started_run(path, &runs, record.run_id)?;
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
                        active_run = None;
                    }
                }
                EventPayload::UserInputRecalledV2 {
                    user_input_seq,
                    input,
                } => {
                    let Some(run_id) = record.run_id else {
                        return corrupt(path, "UserInputRecalledV2 is missing run_id");
                    };
                    if !records[..index].iter().any(|prior| {
                        prior.seq == *user_input_seq
                            && prior.run_id == Some(run_id)
                            && matches!(
                                &prior.payload,
                                EventPayload::UserInputAdmitted { input: admitted }
                                    if admitted == input
                            )
                    }) {
                        return corrupt(
                            path,
                            "UserInputRecalledV2 target is not a prior admission",
                        );
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
                    active_run = Some(run_id);
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
                        return corrupt(
                            path,
                            "SessionTitleCommitted has inconsistent run ownership",
                        );
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
                    let run_id = require_started_run(path, &runs, record.run_id)?;
                    if attempts.contains_key(attempt_id) {
                        return corrupt(path, "attempt_id has more than one ModelAttemptStarted");
                    }
                    let run = runs.get_mut(&run_id).expect("started run is indexed");
                    run.ordering_tainted |=
                        taint.run_ordering_between(run_id, run.start_seq, record.seq);
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
                        return corrupt(
                            path,
                            "attempt prompt fingerprint does not match RunStarted",
                        );
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
                        },
                    );
                }
                EventPayload::ModelRequestPrepared { attempt_id, .. }
                | EventPayload::TextDelta { attempt_id, .. }
                | EventPayload::ReasoningDelta { attempt_id, .. } => {
                    validate_attempt_owner(path, &attempts, *attempt_id, record.run_id)?;
                }
                EventPayload::AttemptAbandoned { attempt_id } => {
                    let run_id =
                        validate_attempt_owner(path, &attempts, *attempt_id, record.run_id)?;
                    let run = runs.get_mut(&run_id).expect("started run is indexed");
                    run.ordering_tainted |=
                        taint.run_ordering_between(run_id, run.start_seq, record.seq);
                    if strict && run.ordering_tainted {
                        return corrupt(
                            path,
                            "cannot strictly append an attempt terminal after missing prerequisites",
                        );
                    }
                    finish_attempt(path, &mut runs, &mut attempts, run_id, *attempt_id, false)?;
                }
                EventPayload::ModelReplayEvaluated {
                    attempt_id,
                    resolved_model,
                    ..
                } => {
                    validate_attempt_model(
                        path,
                        &attempts,
                        *attempt_id,
                        record.run_id,
                        resolved_model,
                    )?;
                }
                EventPayload::ModelTurnCommitted {
                    attempt_id,
                    model_turn_seq,
                    resolved_model,
                    turn,
                    ..
                } => {
                    let run_id = validate_attempt_model(
                        path,
                        &attempts,
                        *attempt_id,
                        record.run_id,
                        resolved_model,
                    )?;
                    let run = runs.get_mut(&run_id).expect("started run is indexed");
                    run.ordering_tainted |=
                        taint.run_ordering_between(run_id, run.start_seq, record.seq);
                    if strict && run.ordering_tainted {
                        return corrupt(
                            path,
                            "cannot strictly append a model turn after missing prerequisites",
                        );
                    }
                    finish_attempt(path, &mut runs, &mut attempts, run_id, *attempt_id, true)?;
                    let turn_ordering_tainted = taint.turn_ordering_before(record.seq);
                    if *model_turn_seq != next_model_turn_seq && (strict || !turn_ordering_tainted)
                    {
                        return corrupt(path, "model_turn_seq is not contiguous");
                    }
                    next_model_turn_seq = model_turn_seq.saturating_add(1);
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
                    let run_id = require_started_run(path, &runs, record.run_id)?;
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
                    let run_id = require_started_run(path, &runs, record.run_id)?;
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
                    let run_id = require_started_run(path, &runs, record.run_id)?;
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
                            *tainted > internal.start_seq
                                && *tainted > internal.model_phase_taint_seen
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
                        || (model_phase_taint.is_none()
                            && internal.active_model.as_ref() != from_model)
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
                    let run_id = require_started_run(path, &runs, record.run_id)?;
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
                            *tainted > internal.start_seq
                                && *tainted > internal.model_phase_taint_seen
                        });
                    let usage_phase_taint = taint
                        .internal_usage_phase_taint_before(*internal_run_id, record.seq)
                        .filter(|tainted| {
                            *tainted > internal.start_seq
                                && *tainted > internal.usage_phase_taint_seen
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
                    let run_id = require_started_run(path, &runs, record.run_id)?;
                    let run = runs.get_mut(&run_id).expect("started run is indexed");
                    run.ordering_tainted |=
                        taint.run_ordering_between(run_id, run.start_seq, record.seq);
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
                        return corrupt(
                            path,
                            "ModelFallback index does not index the frozen suffix",
                        );
                    };
                    let Ok(to_index) = usize::try_from(*to_fallback_index) else {
                        return corrupt(
                            path,
                            "ModelFallback index does not index the frozen suffix",
                        );
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
                    let run_id = require_started_run(path, &runs, record.run_id)?;
                    validate_tool_owner(
                        path,
                        run_id,
                        &turns,
                        &model_call_owners,
                        &provider_item_owners,
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
                        return corrupt(
                            path,
                            "tool termination ownership does not match its start",
                        );
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
                EventPayload::ToolCallProgress { tool_call_id, .. }
                | EventPayload::ToolStdinSubmitted { tool_call_id, .. }
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
                    validate_approval_owner(path, &approval_owners, *approval_id, record.run_id)?;
                }
                EventPayload::TreeApprovalGrantCommitted { grant } => {
                    validate_approval_owner(
                        path,
                        &approval_owners,
                        grant.approval_id,
                        record.run_id,
                    )?;
                }
                EventPayload::PluginEventAdded { .. } | EventPayload::PluginDiagnostic { .. } => {
                    if record.run_id.is_some() {
                        return corrupt(path, "plugin events must be runless");
                    }
                }
                _ => {
                    require_started_run(path, &runs, record.run_id)?;
                }
            }
            if matches!(
                record.payload,
                EventPayload::RunCompleted { .. }
                    | EventPayload::RunFailed { .. }
                    | EventPayload::RunCancelled { .. }
                    | EventPayload::RunInterrupted { .. }
            ) && record.run_id == active_run
            {
                active_run = None;
            }
        }
        Ok(taint)
    }

    fn assert_storage_matches_full_projection(storage: &mut EventStorage) {
        let expected = cookie_agent_protocol::visible_events(&storage.all);
        let actual = storage
            .visible
            .iter()
            .map(|index| storage.all[*index].clone())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        let expected_run = expected
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                EventPayload::RunStarted { selection, .. } => event
                    .run_id
                    .map(|run| (event.seq, run, selection.model.clone())),
                _ => None,
            });
        let expected_checkpoint = expected
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                EventPayload::ContextCheckpointCommitted { commit } => {
                    Some((event.seq, commit.boundaries.input_through_seq))
                }
                _ => None,
            });
        let expected_usage = expected
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                EventPayload::ModelUsageRecorded { usage, .. } => {
                    super::usage_total(event.seq, usage)
                }
                _ => None,
            })
            .or_else(|| {
                expected
                    .iter()
                    .rev()
                    .find_map(|event| match &event.payload {
                        EventPayload::ModelTurnCommitted { turn, .. } => {
                            super::usage_total(event.seq, &turn.usage)
                        }
                        _ => None,
                    })
            });
        assert_eq!(storage.index.last_run_started, expected_run);
        assert_eq!(
            (
                storage.index.last_checkpoint_seq,
                storage.index.last_checkpoint_input_through_seq
            ),
            expected_checkpoint.unwrap_or_default()
        );
        assert_eq!(storage.index.latest_real_usage(), expected_usage);
        let first = storage.snapshot();
        let second = storage.snapshot();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.as_ref(), expected);
    }

    #[test]
    fn incremental_validation_matches_historical_reference_corpus() {
        let valid = attribution_records();
        let session_id = valid[0].session_id;
        let run_id = valid[1].run_id.expect("run id");
        let path = Path::new("differential-events.jsonl");

        let mut attempt_reordered = valid.clone();
        let first_payload = attempt_reordered[2].payload.clone();
        attempt_reordered[2].payload = attempt_reordered[3].payload.clone();
        attempt_reordered[3].payload = first_payload;

        let mut duplicate_attempt = valid.clone();
        let mut duplicate = duplicate_attempt[2].clone();
        duplicate.seq = duplicate_attempt.last().expect("last event").seq + 1;
        duplicate.timestamp = jiff::Timestamp::new(duplicate.seq as i64, 0).unwrap();
        duplicate_attempt.push(duplicate);

        let mut attempt_before_run = vec![valid[0].clone(), valid[2].clone()];
        attempt_before_run[1].seq = 2;
        attempt_before_run[1].timestamp = jiff::Timestamp::new(2, 0).unwrap();

        let mut timestamp_reversal = valid.clone();
        timestamp_reversal[2].timestamp = jiff::Timestamp::new(1, 0).unwrap();

        let mut sequence_gap = valid.clone();
        for record in &mut sequence_gap[2..] {
            record.seq += 1;
            record.timestamp = jiff::Timestamp::new(record.seq as i64, 0).unwrap();
        }

        let admitted = event(
            session_id,
            Some(run_id),
            3,
            EventPayload::UserInputAdmitted {
                input: "admitted".into(),
            },
        );
        let recalled = event(
            session_id,
            Some(run_id),
            4,
            EventPayload::UserInputRecalledV2 {
                user_input_seq: 3,
                input: "admitted".into(),
            },
        );
        let admission = vec![
            valid[0].clone(),
            valid[1].clone(),
            admitted,
            recalled.clone(),
        ];
        let missing_admission = vec![valid[0].clone(), valid[1].clone(), recalled];

        let mut revert = valid.clone();
        let revert_seq = revert.last().expect("last event").seq + 1;
        revert.push(event(
            session_id,
            None,
            revert_seq,
            EventPayload::SessionReverted { through_seq: 4 },
        ));

        let mut broad_taint = ValidationTaint::default();
        broad_taint.mark_broad(3, 3);
        let mut tainted_attempt = valid[..3].to_vec();
        tainted_attempt[2].seq = 4;
        tainted_attempt[2].timestamp = jiff::Timestamp::new(4, 0).unwrap();

        let mut admission_taint = ValidationTaint::default();
        admission_taint.admissions.insert(3, 3);
        let tainted_recall = vec![
            valid[0].clone(),
            valid[1].clone(),
            event(
                session_id,
                Some(run_id),
                4,
                EventPayload::UserInputRecalledV2 {
                    user_input_seq: 3,
                    input: "missing but tainted".into(),
                },
            ),
        ];

        let cases = vec![
            ("valid", valid, ValidationTaint::default(), true),
            (
                "attempt_reordered",
                attempt_reordered,
                ValidationTaint::default(),
                true,
            ),
            (
                "duplicate_attempt",
                duplicate_attempt,
                ValidationTaint::default(),
                true,
            ),
            (
                "attempt_before_run",
                attempt_before_run,
                ValidationTaint::default(),
                true,
            ),
            (
                "timestamp_reversal",
                timestamp_reversal,
                ValidationTaint::default(),
                true,
            ),
            (
                "sequence_gap",
                sequence_gap,
                ValidationTaint::default(),
                true,
            ),
            ("admission", admission, ValidationTaint::default(), true),
            (
                "missing_admission",
                missing_admission,
                ValidationTaint::default(),
                true,
            ),
            ("revert", revert, ValidationTaint::default(), true),
            ("broad_taint", tainted_attempt, broad_taint, false),
            ("admission_taint", tainted_recall, admission_taint, false),
        ];

        for (label, records, initial_taint, strict) in cases {
            let mut accepted = Vec::new();
            let mut incremental = ValidationState::new(initial_taint.clone());
            let mut storage = EventStorage::new(Vec::new());
            for record in records {
                let snapshot_before = storage.snapshot();
                let mut full_candidate = accepted.clone();
                full_candidate.push(record.clone());
                let reference = reference_validate_records(
                    path,
                    session_id,
                    &full_candidate,
                    &initial_taint,
                    strict.then_some(record.seq),
                );
                let mut incremental_candidate = incremental.clone();
                let one = validate_record_incremental(
                    path,
                    session_id,
                    &record,
                    &mut incremental_candidate,
                    strict,
                );
                assert_eq!(
                    one.is_ok(),
                    reference.is_ok(),
                    "{label} diverged at sequence {}: incremental={one:?}, reference={reference:?}",
                    record.seq
                );
                if let (Ok(()), Ok(reference_taint)) = (one, reference) {
                    assert_eq!(
                        incremental_candidate.taint, reference_taint,
                        "{label} taint"
                    );
                    incremental = incremental_candidate;
                    accepted.push(record.clone());
                    storage.push(record);
                    assert_storage_matches_full_projection(&mut storage);
                } else {
                    assert_eq!(storage.snapshot(), snapshot_before, "{label} snapshot");
                    let retained_reference = reference_validate_records(
                        path,
                        session_id,
                        &accepted,
                        &initial_taint,
                        if strict {
                            accepted.last().map(|event| event.seq)
                        } else {
                            None
                        },
                    )
                    .expect("retained prefix remains valid");
                    assert_eq!(
                        incremental.taint, retained_reference,
                        "{label} retained taint"
                    );
                    assert_storage_matches_full_projection(&mut storage);
                }
            }
        }
    }

    fn assert_log_rebuilt_against_reference(log: &EventLog) {
        let records = log.all_events();
        let reference = reference_validate_records(
            log.path(),
            log.session_id,
            &records,
            &log.initial_validation_taint,
            None,
        )
        .expect("reference accepts retained log");
        assert_eq!(
            log.validation.lock().unwrap().taint,
            reference,
            "rebuilt taint"
        );
        let mut events = log.events.lock().unwrap();
        assert_storage_matches_full_projection(&mut events);
    }

    #[test]
    fn rejected_append_and_persistence_failure_rebuild_incremental_state() {
        let records = attribution_records();
        let creation = records[0].clone();
        let run = records[1].run_id.expect("run id");
        let directory = tempdir().unwrap();
        let rejected = EventLog::create_buffered(
            directory.path().join("buffered.jsonl"),
            creation.session_id,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            creation.payload.clone(),
        )
        .unwrap();
        rejected
            .append(
                Some(run),
                EventOrigin::new("engine:test").unwrap(),
                records[1].payload.clone(),
            )
            .unwrap();
        let before = rejected.event_snapshot();
        assert!(
            rejected
                .append(
                    Some(run),
                    EventOrigin::new("engine:test").unwrap(),
                    records[1].payload.clone(),
                )
                .is_err()
        );
        assert_eq!(rejected.event_snapshot(), before);
        assert_log_rebuilt_against_reference(&rejected);
        rejected
            .append(
                Some(run),
                EventOrigin::new("engine:test").unwrap(),
                records[2].payload.clone(),
            )
            .expect("valid append succeeds after rejected duplicate rebuild");
        assert_log_rebuilt_against_reference(&rejected);

        let persisted_directory = tempdir().unwrap();
        let persisted_path = persisted_directory.path().join("events.jsonl");
        let persisted = EventLog::create(
            persisted_path,
            creation.session_id,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            creation.payload,
        )
        .unwrap();
        let before = persisted.event_snapshot();
        let writer = persisted.persistent_writer().expect("event log writer");
        writer.shared.state.lock().unwrap().background_error = Some(WriterFailure {
            kind: io::ErrorKind::Other,
            message: "injected persistence failure".into(),
        });
        assert!(
            persisted
                .append(
                    Some(run),
                    EventOrigin::new("engine:test").unwrap(),
                    records[1].payload.clone(),
                )
                .is_err()
        );
        assert_eq!(persisted.event_snapshot(), before);
        assert_log_rebuilt_against_reference(&persisted);
    }

    #[test]
    fn event_storage_snapshot_and_indexes_match_full_projection() {
        let mut records = attribution_records();
        let revert_seq = records.last().expect("last event").seq + 1;
        records.push(event(
            records[0].session_id,
            None,
            revert_seq,
            EventPayload::SessionReverted { through_seq: 4 },
        ));
        let mut storage = EventStorage::new(Vec::new());
        for record in records {
            storage.push(record);
            assert_storage_matches_full_projection(&mut storage);
        }
    }

    fn current_delegation_records() -> Vec<StoredEvent> {
        let mut records = attribution_records();
        records.truncate(2);
        let session_id = records[0].session_id;
        let parent_run_id = records[1].run_id.expect("parent run");
        let EventPayload::RunStarted {
            agent,
            selected_suffix,
            ..
        } = &records[1].payload
        else {
            unreachable!("second fixture event starts the run");
        };
        let child_agent = agent.as_ref().clone();
        let selected_suffix = selected_suffix.clone();
        let append_lifecycle = |records: &mut Vec<StoredEvent>,
                                invocation_id: InvocationId,
                                child_session_id: SessionId,
                                child_run_id: RunId,
                                request: DelegateRequestPayload,
                                attached: bool| {
            let request_fingerprint = crate::delegation_events::delegation_request_fingerprint(
                &child_agent,
                &selected_suffix,
                &request,
            )
            .expect("delegation fingerprint");
            let reservation = DelegationReservation {
                invocation_id,
                parent_session_id: session_id,
                parent_run_id,
                parent_tool_call_id: ToolCallId(Uuid::from_u128(invocation_id.0.as_u128() + 10)),
                child_session_id,
            };
            push_run_event(
                records,
                session_id,
                parent_run_id,
                EventPayload::DelegationReserved {
                    reservation,
                    child_agent: Box::new(child_agent.clone()),
                    manifest_revision: selected_suffix[0].manifest_revision.clone(),
                    runtime_revision: runtime_revision(),
                    catalog_revision: catalog_revision(),
                    provider_state_revision: provider_revision(),
                    model_revision: model_revision(),
                    agent_revision: agent_revision(),
                    recipe_registry_revision: registry_revision(),
                    selected_suffix: selected_suffix.clone(),
                    request_fingerprint,
                    request,
                },
            );
            push_run_event(
                records,
                session_id,
                parent_run_id,
                EventPayload::DelegationStarted {
                    invocation_id,
                    child_session_id,
                },
            );
            push_run_event(
                records,
                session_id,
                parent_run_id,
                if attached {
                    EventPayload::DelegationRunAttached {
                        invocation_id,
                        child_run_id,
                    }
                } else {
                    EventPayload::DelegationRunStarted {
                        invocation_id,
                        child_run_id,
                    }
                },
            );
            push_run_event(
                records,
                session_id,
                parent_run_id,
                EventPayload::DelegationFinished {
                    invocation_id,
                    child_session_id,
                    child_run_id: Some(child_run_id),
                    status: SessionStatus::Completed,
                    reason: None,
                },
            );
        };
        let child_session_id = SessionId(Uuid::from_u128(300));
        append_lifecycle(
            &mut records,
            InvocationId(Uuid::from_u128(301)),
            child_session_id,
            RunId(Uuid::from_u128(302)),
            DelegateRequestPayload {
                description: "Golden child".into(),
                prompt: "Inspect the current event format".into(),
                title: SessionTitle::new("Golden child").expect("title"),
                resume_session_id: None,
                inherit_context: false,
                seeded_context: Vec::new(),
                background: true,
                staged_skill: None,
            },
            false,
        );
        append_lifecycle(
            &mut records,
            InvocationId(Uuid::from_u128(303)),
            child_session_id,
            RunId(Uuid::from_u128(304)),
            DelegateRequestPayload {
                description: "Golden resumed child".into(),
                prompt: "Resume using the current event format".into(),
                title: SessionTitle::new("Golden child").expect("title"),
                resume_session_id: Some(child_session_id),
                inherit_context: false,
                seeded_context: Vec::new(),
                background: false,
                staged_skill: None,
            },
            true,
        );
        records
    }

    fn assert_log_open(records: &[StoredEvent], expected: bool, label: &str) {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        let bytes = records
            .iter()
            .flat_map(|record| {
                let mut line = serde_json::to_vec(record).expect("serialize record");
                line.push(b'\n');
                line
            })
            .collect::<Vec<_>>();
        fs::write(&path, bytes).expect("write event log");
        let result = EventLog::open(path, records[0].session_id);
        assert_eq!(result.is_ok(), expected, "{label}: {result:?}");
    }

    #[test]
    fn load_jsonl_truncates_only_a_torn_tail() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        fs::write(&path, b"{\"ok\":true}\n{\"partial\"").expect("write torn log");
        let records = load_jsonl::<Value>(&path).expect("recover log");
        assert_eq!(records, vec![serde_json::json!({"ok": true})]);
        assert_eq!(
            fs::read(&path).expect("read recovered log"),
            b"{\"ok\":true}\n"
        );
    }

    #[test]
    fn torn_tail_recovery_can_write_while_retained_writer_is_open() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        let writer = EventLogWriter::open(&path).expect("open retained writer");
        writer
            .append(br#"{"record":1}"#, true)
            .expect("append durable record");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open second write handle")
            .write_all(br#"{"torn"#)
            .expect("write torn tail");

        assert_eq!(
            load_jsonl::<Value>(&path).expect("truncate through recovery handle"),
            vec![serde_json::json!({"record": 1})]
        );
        writer
            .append(br#"{"record":2}"#, true)
            .expect("resume retained writer");
        assert_eq!(
            load_jsonl::<Value>(&path).expect("load resumed log"),
            vec![
                serde_json::json!({"record": 1}),
                serde_json::json!({"record": 2}),
            ]
        );
        writer.shutdown();
    }

    #[test]
    fn only_stream_records_skip_the_durable_barrier() {
        let attempt_id = AttemptId(Uuid::from_u128(1));
        let tool_call_id = ToolCallId(Uuid::from_u128(2));
        assert!(!event_requires_durable_barrier(&EventPayload::TextDelta {
            attempt_id,
            text: "text".into(),
        }));
        assert!(!event_requires_durable_barrier(
            &EventPayload::ReasoningDelta {
                attempt_id,
                text: "reasoning".into(),
            }
        ));
        assert!(!event_requires_durable_barrier(
            &EventPayload::ToolCallProgress {
                tool_call_id,
                message: SafeDisplayText::new("progress").expect("safe progress"),
                output_chunk: None,
            }
        ));
        assert!(event_requires_durable_barrier(
            &EventPayload::UserInputAdmitted {
                input: "steer".into(),
            }
        ));
    }

    #[test]
    fn barrier_sync_precedes_publication_without_holding_snapshot_lock() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        let creation = stored_event();
        let log = EventLog::create(
            path.clone(),
            creation.session_id,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            creation.payload,
        )
        .expect("create event log");
        let (sync_reached, release_sync) = log.install_sync_hook_for_test();
        let appending = {
            let log = log.clone();
            thread::spawn(move || {
                log.append(
                    None,
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                    EventPayload::UserInputAdmitted {
                        input: "steer".into(),
                    },
                )
            })
        };

        sync_reached.recv().expect("barrier reached sync");
        assert!(log.snapshot_lock_available_for_test());
        assert_eq!(log.all_events().len(), 1, "barrier is not published early");
        assert_eq!(
            load_jsonl::<StoredEvent>(&path)
                .expect("read durable prefix")
                .len(),
            1,
            "barrier is not durable before sync completes"
        );
        release_sync.send(()).expect("release barrier sync");
        appending
            .join()
            .expect("append thread")
            .expect("append barrier");

        assert_eq!(log.all_events().len(), 2);
        assert_eq!(
            load_jsonl::<StoredEvent>(&path)
                .expect("read durable barrier")
                .len(),
            2
        );
    }

    #[test]
    fn barrier_sync_drains_all_preceding_stream_records() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        let records = attribution_records();
        let bytes = records
            .iter()
            .flat_map(|record| {
                let mut line = serde_json::to_vec(record).expect("serialize event");
                line.push(b'\n');
                line
            })
            .collect::<Vec<_>>();
        fs::write(&path, bytes).expect("write event history");
        let log = EventLog::open(path.clone(), records[0].session_id).expect("open event log");
        let run_id = records[1].run_id.expect("run id");
        let (resolved_model, prompt_fingerprint) = records
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                EventPayload::ModelAttemptStarted {
                    resolved_model,
                    prompt_fingerprint,
                    ..
                } => Some((resolved_model.clone(), prompt_fingerprint.clone())),
                _ => None,
            })
            .expect("latest attempt");
        let attempt_id = AttemptId(Uuid::from_u128(6));
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal: 6,
                fallback_index: 2,
                retry_ordinal: 1,
                resolved_model,
                prompt_fingerprint,
            },
        )
        .expect("start attempt");
        log.pause_background_sync_for_test();
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::TextDelta {
                attempt_id,
                text: "buffered text".into(),
            },
        )
        .expect("append text delta");
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::ReasoningDelta {
                attempt_id,
                text: "buffered reasoning".into(),
            },
        )
        .expect("append reasoning delta");
        let (sync_reached, release_sync) = log.install_sync_hook_for_test();
        let barrier = {
            let log = log.clone();
            thread::spawn(move || {
                log.append(
                    Some(run_id),
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                    EventPayload::AttemptAbandoned { attempt_id },
                )
            })
        };

        sync_reached.recv().expect("barrier reached sync");
        assert_eq!(
            load_jsonl::<StoredEvent>(&path)
                .expect("read pre-barrier prefix")
                .len(),
            records.len() + 1,
            "stream records remain buffered until the barrier sync"
        );
        release_sync.send(()).expect("release barrier sync");
        barrier
            .join()
            .expect("barrier thread")
            .expect("append barrier");

        let durable = load_jsonl::<StoredEvent>(&path).expect("read barrier-synced events");
        assert!(matches!(
            &durable[durable.len() - 3].payload,
            EventPayload::TextDelta { text, .. } if text == "buffered text"
        ));
        assert!(matches!(
            &durable[durable.len() - 2].payload,
            EventPayload::ReasoningDelta { text, .. } if text == "buffered reasoning"
        ));
        assert!(matches!(
            durable.last().map(|event| &event.payload),
            Some(EventPayload::AttemptAbandoned { attempt_id: durable_attempt })
                if *durable_attempt == attempt_id
        ));
    }

    #[test]
    fn buffered_records_become_durable_on_the_sync_deadline() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        let writer = EventLogWriter::open(&path).expect("open writer");
        let (sync_reached, release_sync) = writer.install_sync_hook();

        writer
            .append(br#"{"type":"text_delta"}"#, false)
            .expect("buffer delta");
        sync_reached.recv().expect("deadline reached sync");
        assert!(fs::read(&path).expect("read pre-sync file").is_empty());
        release_sync.send(()).expect("release deadline sync");
        writer.flush().expect("wait for durable delta");

        assert_eq!(
            load_jsonl::<Value>(&path).expect("load durable delta"),
            vec![serde_json::json!({"type": "text_delta"})]
        );
        writer.shutdown();
    }

    #[test]
    fn torn_buffered_tail_does_not_remove_a_durable_barrier() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        let writer = EventLogWriter::open(&path).expect("open writer");
        writer
            .append(br#"{"type":"user_input_admitted"}"#, true)
            .expect("sync barrier");
        let (sync_reached, release_sync) = writer.install_sync_hook();
        writer
            .append(br#"{"type":"text_delta"}"#, false)
            .expect("buffer delta");
        sync_reached.recv().expect("delta reached sync");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open crash writer")
            .write_all(br#"{"type":"text_"#)
            .expect("write torn delta");

        assert_eq!(
            load_jsonl::<Value>(&path).expect("recover torn log"),
            vec![serde_json::json!({"type": "user_input_admitted"})]
        );
        assert_eq!(
            fs::read(&path).expect("read recovered log"),
            br#"{"type":"user_input_admitted"}
"#
        );
        release_sync.send(()).expect("release delta sync");
        writer.flush().expect("finish writer");
        writer.shutdown();
    }

    #[test]
    fn stored_event_rejects_unknown_envelope_fields_and_ignores_legacy_version() {
        let mut value = serde_json::to_value(stored_event()).expect("serialize record");
        value
            .as_object_mut()
            .expect("record object")
            .insert("legacy".into(), Value::Bool(true));
        assert!(serde_json::from_value::<StoredEvent>(value).is_err());

        let mut value = serde_json::to_value(stored_event()).expect("serialize record");
        value["event_schema_version"] = Value::from(3);
        assert!(serde_json::from_value::<StoredEvent>(value).is_ok());
    }

    #[test]
    fn tolerant_loader_accepts_origin_as_a_known_envelope_field() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        let mut creation = stored_event();
        creation.origin = Some(EventOrigin::new("engine:recovery").unwrap());
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&creation).unwrap()),
        )
        .expect("write originated event");

        let log = EventLog::open(path, creation.session_id).expect("load originated event");
        assert_eq!(
            log.events()[0].origin.as_ref().map(EventOrigin::as_str),
            Some("engine:recovery")
        );
        assert!(log.diagnostics().is_empty());
    }

    #[test]
    fn event_log_rejects_malformed_creation_identity_and_sequence() {
        let expected = SessionId(Uuid::from_u128(99));
        let malformed = [
            {
                let mut record = stored_event();
                record.seq = 2;
                record
            },
            {
                let mut record = stored_event();
                record.session_id = SessionId(Uuid::from_u128(100));
                record
            },
            {
                let mut record = stored_event();
                let EventPayload::SessionCreated {
                    creation_selection, ..
                } = &mut record.payload
                else {
                    unreachable!()
                };
                creation_selection.agent =
                    cookie_agent_protocol::AgentId::new("other").expect("agent id");
                record
            },
            {
                let mut record = stored_event();
                record.run_id = Some(cookie_agent_protocol::RunId(Uuid::from_u128(1)));
                record
            },
        ];

        for (index, record) in malformed.into_iter().enumerate() {
            let directory = tempdir().expect("temporary directory");
            let path = directory.path().join(format!("events-{index}.jsonl"));
            let mut bytes = serde_json::to_vec(&record).expect("serialize record");
            bytes.push(b'\n');
            fs::write(&path, bytes).expect("write event log");
            assert!(EventLog::open(path, expected).is_err());
        }
    }

    #[test]
    fn event_log_best_effort_reader_applies_all_three_tiers() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        let creation = stored_event();
        let session = creation.session_id;
        let mut values = vec![serde_json::to_value(&creation).expect("creation")];

        let mut optional = serde_json::to_value(event(
            session,
            None,
            2,
            EventPayload::DelegateChildTerminated {
                status: cookie_agent_protocol::SessionStatus::Failed,
                reason: None,
            },
        ))
        .expect("optional event");
        optional["payload"]["reason"] = serde_json::json!(42);
        values.push(optional);

        let mut unknown_tag = serde_json::to_value(event(
            session,
            None,
            3,
            EventPayload::UserInputAdmitted {
                input: "future".into(),
            },
        ))
        .expect("unknown event");
        unknown_tag["payload"]["type"] = serde_json::json!("future_event");
        values.push(unknown_tag);

        let mut broken_required = serde_json::to_value(event(
            session,
            Some(RunId(Uuid::from_u128(400))),
            4,
            EventPayload::RunFailed {
                error: SafeErrorMessage::new("failed").expect("safe error"),
            },
        ))
        .expect("broken event");
        broken_required["payload"]["error"] = serde_json::json!(42);
        values.push(broken_required);

        let mut future_field = serde_json::to_value(event(
            session,
            None,
            5,
            EventPayload::UserInputAdmitted {
                input: "accepted".into(),
            },
        ))
        .expect("future field event");
        future_field["payload"]["future_optional"] = serde_json::json!(true);
        values.push(future_field);

        let mut unknown_envelope = serde_json::to_value(event(
            session,
            None,
            6,
            EventPayload::UserInputAdmitted {
                input: "skip envelope".into(),
            },
        ))
        .expect("unknown envelope");
        unknown_envelope["future_envelope"] = serde_json::json!(true);
        values.push(unknown_envelope);

        let contents = values
            .into_iter()
            .map(|value| serde_json::to_string(&value).expect("event line"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&path, contents).expect("write events");

        let log = EventLog::open(path, session).expect("best-effort open");
        assert_eq!(
            log.all_events()
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![1, 2, 5],
            "diagnostics: {:?}",
            log.diagnostics()
        );
        assert!(log.diagnostics().iter().any(|diagnostic| {
            !diagnostic.skipped
                && diagnostic.seq == 2
                && diagnostic.reason.contains("payload.reason")
        }));
        assert_eq!(
            log.diagnostics()
                .iter()
                .filter(|diagnostic| diagnostic.skipped)
                .map(|diagnostic| diagnostic.seq)
                .collect::<Vec<_>>(),
            vec![3, 4, 6]
        );
        let events = log.events();
        let EventPayload::SessionCreated { creation_agent, .. } = &events[0].payload else {
            panic!("creation event")
        };
        let artifacts =
            crate::ArtifactStore::open(directory.path().join("artifacts")).expect("artifact store");
        crate::model_history::assemble_full_history(
            &events,
            &artifacts,
            &creation_agent.fallback_chain[0],
            "system",
        )
        .expect("model history tolerates skipped records");
        let appended = log
            .append(
                None,
                EventOrigin::new("engine:test").unwrap(),
                EventPayload::UserInputAdmitted {
                    input: "after skipped tail".into(),
                },
            )
            .expect("append after skipped tail");
        assert_eq!(appended.seq, 7);
        assert_eq!(
            appended.engine_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn unrelated_skipped_event_does_not_permit_forged_usage_or_orphan_tool_termination() {
        let records = attribution_records();
        let session = records[0].session_id;
        let run = records[1].run_id.expect("run id");
        let next_seq = records.last().expect("records").seq + 1;
        let mut unknown = serde_json::to_value(event(
            session,
            Some(run),
            next_seq,
            EventPayload::UserInputApplied { user_input_seq: 1 },
        ))
        .expect("unknown event envelope");
        unknown["payload"]["type"] = serde_json::json!("future_unrelated_event");

        let forged_usage = event(
            session,
            Some(run),
            next_seq + 1,
            EventPayload::ModelUsageRecorded {
                model_turn_seq: 999,
                agent_id: cookie_agent_protocol::AgentId::new("test").expect("agent id"),
                resolved_model: wire_resolved(&fallback_binding("fallback-zero")),
                usage: Usage::default(),
                estimated_cost_pico_usd: None,
            },
        );
        let directory = tempdir().expect("temporary directory");
        let usage_path = directory.path().join("forged-usage.jsonl");
        let mut lines = records
            .iter()
            .map(|record| serde_json::to_string(record).expect("record"))
            .collect::<Vec<_>>();
        lines.push(serde_json::to_string(&unknown).expect("unknown"));
        lines.push(serde_json::to_string(&forged_usage).expect("usage"));
        fs::write(&usage_path, lines.join("\n") + "\n").expect("write usage log");
        assert!(EventLog::open(usage_path, session).is_err());

        let orphan_path = directory.path().join("orphan-tool.jsonl");
        lines.pop();
        lines.push(
            serde_json::to_string(&event(
                session,
                Some(run),
                next_seq + 1,
                orphan_termination(ToolCallId(Uuid::from_u128(700))),
            ))
            .expect("termination"),
        );
        fs::write(&orphan_path, lines.join("\n") + "\n").expect("write orphan log");
        assert!(EventLog::open(orphan_path, session).is_err());
    }

    #[test]
    fn append_after_skipped_tool_start_still_requires_an_observed_start() {
        let creation = stored_event();
        let session = creation.session_id;
        let run = RunId(Uuid::from_u128(701));
        let tool_call_id = ToolCallId(Uuid::from_u128(702));
        let skipped_start = serde_json::json!({
            "session_id": session,
            "run_id": run,
            "seq": 2,
            "timestamp": "1970-01-01T00:00:02Z",
            "payload": {
                "type": "tool_call_started",
                "tool_call_id": tool_call_id,
                "owner": 42,
                "presentation": 42,
                "operation_fingerprint": 42
            }
        });
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("skipped-start.jsonl");
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&creation).expect("creation"),
                serde_json::to_string(&skipped_start).expect("skipped start")
            ),
        )
        .expect("write skipped start log");
        let log = EventLog::open(path, session).expect("open skipped start log");
        assert!(
            log.append(
                Some(run),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                orphan_termination(tool_call_id)
            )
            .is_err()
        );
        assert_eq!(log.all_events().len(), 1);
    }

    #[test]
    fn large_sequence_gap_uses_one_bounded_diagnostic() {
        let creation = stored_event();
        let session = creation.session_id;
        let distant_seq = 1_000_000_000;
        let distant = event(
            session,
            None,
            distant_seq,
            EventPayload::DelegateChildTerminated {
                status: cookie_agent_protocol::SessionStatus::Failed,
                reason: None,
            },
        );
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("large-gap.jsonl");
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&creation).expect("creation"),
                serde_json::to_string(&distant).expect("distant event")
            ),
        )
        .expect("write large gap log");
        let log = EventLog::open(path, session).expect("open large gap");
        assert_eq!(log.diagnostics().len(), 1);
        assert_eq!(log.diagnostics()[0].seq, 2);
        assert!(log.diagnostics()[0].reason.contains("2..=999999999"));
        assert_eq!(log.physical_tip_seq(), distant_seq);
    }

    #[test]
    fn repeated_skipped_internal_fallbacks_refresh_phase_taint_and_no_skip_stays_strict() {
        let base = attribution_records();
        let session = base[0].session_id;
        let run = base[1].run_id.expect("run id");
        let invocation_id = InternalAgentInvocationId(Uuid::from_u128(800));
        let internal_run_id = InternalAgentRunId(Uuid::from_u128(801));
        let kind = InternalAgentKind::Approval;
        let from_model = wire_resolved(&fallback_binding("fallback-zero"));
        let to_model = wire_resolved(&fallback_binding("fallback-one"));
        let final_model = wire_resolved(&fallback_binding("fallback-two"));
        let started = event(
            session,
            Some(run),
            3,
            EventPayload::InternalAgentStarted {
                invocation_id,
                internal_run_id,
                kind,
                backend: InternalAgentBackend::Model {
                    resolved_model: from_model.clone(),
                },
                call: SafeInternalAgentCall {
                    name: SafeCode::new("internal").expect("safe code"),
                    input_summary: SafeDisplayText::new("input").expect("safe display"),
                    input_digest: Sha256Digest::of_bytes(b"input"),
                },
            },
        );
        let fallback = event(
            session,
            Some(run),
            4,
            EventPayload::InternalAgentFallback {
                invocation_id,
                internal_run_id,
                kind,
                from: InternalAgentBackend::Model {
                    resolved_model: from_model,
                },
                to: InternalAgentBackend::Model {
                    resolved_model: to_model.clone(),
                },
                failure: InternalAgentFailure {
                    code: SafeCode::new("fallback").expect("safe code"),
                    message: SafeErrorMessage::new("fallback").expect("safe error"),
                    retryable: true,
                    model_error: None,
                },
                attempts: 1,
            },
        );
        let usage = |resolved_model| EventPayload::InternalAgentUsageRecorded {
            internal_run_id,
            kind,
            agent_id: AgentId::new(cookie_agent_config::BUILT_IN_APPROVAL_AGENT_ID)
                .expect("agent id"),
            resolved_model,
            usage: Usage::default(),
            estimated_cost_pico_usd: None,
        };
        let second_fallback = event(
            session,
            Some(run),
            6,
            EventPayload::InternalAgentFallback {
                invocation_id,
                internal_run_id,
                kind,
                from: InternalAgentBackend::Model {
                    resolved_model: to_model.clone(),
                },
                to: InternalAgentBackend::Model {
                    resolved_model: final_model.clone(),
                },
                failure: InternalAgentFailure {
                    code: SafeCode::new("fallback").expect("safe code"),
                    message: SafeErrorMessage::new("second fallback").expect("safe error"),
                    retryable: true,
                    model_error: None,
                },
                attempts: 2,
            },
        );
        let mut malformed_fallback = serde_json::to_value(&fallback).expect("fallback");
        malformed_fallback["payload"]["attempts"] = serde_json::json!(0);
        let mut second_malformed_fallback =
            serde_json::to_value(second_fallback).expect("second fallback");
        second_malformed_fallback["payload"]["attempts"] = serde_json::json!(0);
        let accepted = [
            serde_json::to_value(&base[0]).expect("creation"),
            serde_json::to_value(&base[1]).expect("run"),
            serde_json::to_value(&started).expect("internal start"),
            malformed_fallback,
            serde_json::to_value(event(session, Some(run), 5, usage(to_model.clone())))
                .expect("first re-anchor"),
            second_malformed_fallback,
            serde_json::to_value(event(session, Some(run), 7, usage(final_model)))
                .expect("second dependent usage"),
        ];
        let directory = tempdir().expect("temporary directory");
        let accepted_path = directory.path().join("internal-phase-tainted.jsonl");
        write_event_values(&accepted_path, &accepted);
        let log = EventLog::open(accepted_path, session).expect("phase-tainted usage loads");
        assert_eq!(
            log.all_events()
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 5, 7]
        );

        let forged = [
            serde_json::to_value(&base[0]).expect("creation"),
            serde_json::to_value(&base[1]).expect("run"),
            serde_json::to_value(&started).expect("internal start"),
            serde_json::to_value(event(session, Some(run), 4, usage(to_model)))
                .expect("forged usage"),
        ];
        let forged_path = directory.path().join("internal-phase-forged.jsonl");
        write_event_values(&forged_path, &forged);
        assert!(EventLog::open(forged_path, session).is_err());
    }

    #[test]
    fn internal_phase_taint_history_rejects_the_sixty_fifth_transition() {
        let base = attribution_records();
        let session = base[0].session_id;
        let run = base[1].run_id.expect("run id");
        let invocation_id = InternalAgentInvocationId(Uuid::from_u128(805));
        let internal_run_id = InternalAgentRunId(Uuid::from_u128(806));
        let kind = InternalAgentKind::Approval;
        let from_model = wire_resolved(&fallback_binding("fallback-zero"));
        let to_model = wire_resolved(&fallback_binding("fallback-one"));
        let started = event(
            session,
            Some(run),
            3,
            EventPayload::InternalAgentStarted {
                invocation_id,
                internal_run_id,
                kind,
                backend: InternalAgentBackend::Model {
                    resolved_model: from_model.clone(),
                },
                call: SafeInternalAgentCall {
                    name: SafeCode::new("internal").expect("safe code"),
                    input_summary: SafeDisplayText::new("input").expect("safe display"),
                    input_digest: Sha256Digest::of_bytes(b"input"),
                },
            },
        );
        let mut values = vec![
            serde_json::to_value(&base[0]).expect("creation"),
            serde_json::to_value(&base[1]).expect("run"),
            serde_json::to_value(started).expect("internal start"),
        ];
        for offset in 0..=64_u64 {
            let mut fallback = serde_json::to_value(event(
                session,
                Some(run),
                4 + offset,
                EventPayload::InternalAgentFallback {
                    invocation_id,
                    internal_run_id,
                    kind,
                    from: InternalAgentBackend::Model {
                        resolved_model: from_model.clone(),
                    },
                    to: InternalAgentBackend::Model {
                        resolved_model: to_model.clone(),
                    },
                    failure: InternalAgentFailure {
                        code: SafeCode::new("fallback").expect("safe code"),
                        message: SafeErrorMessage::new("fallback").expect("safe error"),
                        retryable: true,
                        model_error: None,
                    },
                    attempts: 1,
                },
            ))
            .expect("fallback");
            fallback["payload"]["attempts"] = serde_json::json!(0);
            values.push(fallback);
        }
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("internal-phase-limit.jsonl");
        write_event_values(&path, &values);
        let error = EventLog::open(path, session)
            .expect_err("sixty-fifth phase taint must be rejected")
            .to_string();
        assert!(error.contains("64-transition per-run limit"));
    }

    #[test]
    fn skipped_tool_termination_taints_terminal_state_but_no_skip_rejects_elision() {
        let base = attribution_records();
        let session = base[0].session_id;
        let run = base[1].run_id.expect("run id");
        let binding = fallback_binding("fallback-zero");
        let resolved_model = wire_resolved(&binding);
        let attempt_id = AttemptId(Uuid::from_u128(810));
        let tool_call_id = ToolCallId(Uuid::from_u128(811));
        let owner = AssistantToolCallRef {
            model_turn_seq: 1,
            content_index: 0,
            model_call_id: ModelCallId::new("observed-tool").expect("model call id"),
            provider_item_id: None,
        };
        let attempt = event(
            session,
            Some(run),
            3,
            EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved_model.clone(),
                prompt_fingerprint: match &base[1].payload {
                    EventPayload::RunStarted { agent, .. } => agent.prompt_fingerprint.clone(),
                    _ => unreachable!(),
                },
            },
        );
        let turn = event(
            session,
            Some(run),
            4,
            EventPayload::ModelTurnCommitted {
                attempt_id,
                model_turn_seq: 1,
                resolved_model,
                input_through_seq: 1,
                turn: PersistedModelTurn {
                    content: vec![PersistedAssistantPart::ToolCall {
                        id: owner.model_call_id.clone(),
                        provider_item_id: None,
                        name: SafeCode::new("read").expect("tool name"),
                        input: serde_json::json!({}),
                        raw_input: None,
                        metadata: None,
                    }],
                    provider_options: BTreeMap::new(),
                    finish_reason: ModelFinishReason::ToolCalls,
                    usage: Usage::default(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        );
        let start = event(
            session,
            Some(run),
            5,
            EventPayload::ToolCallStarted {
                start: ToolCallStart {
                    tool_call_id,
                    owner: owner.clone(),
                    presentation: ToolCallPresentation {
                        title: SafeDisplayText::new("Read").expect("title"),
                        primary_argument: None,
                    },
                    operation_fingerprint: serde_json::from_value(serde_json::json!({
                        "digest": Sha256Digest::of_bytes(b"operation")
                    }))
                    .expect("operation fingerprint"),
                },
            },
        );
        let termination = event(
            session,
            Some(run),
            6,
            EventPayload::ToolCallTerminated {
                termination: ToolCallTermination {
                    tool_call_id,
                    owner: owner.clone(),
                    outcome: ToolTerminationOutcome::Failed,
                    result: None,
                    error: Some(SafeToolError {
                        code: SafeCode::new("failed").expect("safe code"),
                        message: SafeErrorMessage::new("failed").expect("safe error"),
                    }),
                },
            },
        );
        let mut malformed_termination = serde_json::to_value(termination).expect("termination");
        malformed_termination["payload"]["error"] = serde_json::json!(42);
        let elision = EventPayload::ToolOutputElided {
            tool_call_id,
            original_bytes: 10,
            retained: ArtifactReference {
                uri: "artifact://retained".into(),
            },
        };
        let accepted = [
            serde_json::to_value(&base[0]).expect("creation"),
            serde_json::to_value(&base[1]).expect("run"),
            serde_json::to_value(&attempt).expect("attempt"),
            serde_json::to_value(&turn).expect("turn"),
            serde_json::to_value(&start).expect("start"),
            malformed_termination,
            serde_json::to_value(event(session, Some(run), 7, elision.clone())).expect("elision"),
        ];
        let directory = tempdir().expect("temporary directory");
        let accepted_path = directory.path().join("tool-terminal-tainted.jsonl");
        write_event_values(&accepted_path, &accepted);
        let log = EventLog::open(accepted_path, session).expect("terminal-tainted elision loads");
        assert_eq!(log.all_events().last().expect("elision").seq, 7);

        let forged = [
            serde_json::to_value(&base[0]).expect("creation"),
            serde_json::to_value(&base[1]).expect("run"),
            serde_json::to_value(attempt).expect("attempt"),
            serde_json::to_value(turn).expect("turn"),
            serde_json::to_value(start).expect("start"),
            serde_json::to_value(event(session, Some(run), 6, elision)).expect("forged elision"),
        ];
        let forged_path = directory.path().join("tool-terminal-forged.jsonl");
        write_event_values(&forged_path, &forged);
        assert!(EventLog::open(forged_path, session).is_err());
    }

    #[test]
    fn unreadable_session_created_fails_with_a_clear_diagnostic() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        fs::write(&path, b"{not-json}\n").expect("write corrupt creation");
        let error = EventLog::open(path, SessionId(Uuid::from_u128(99)))
            .expect_err("corrupt creation must fail")
            .to_string();
        assert!(error.contains("SessionCreated line is unreadable"));
    }

    #[test]
    fn gaps_are_diagnosed_and_projection_remains_coherent() {
        let records = attribution_records();
        let session = records[0].session_id;
        let removed = records
            .iter()
            .filter(|event| event.seq % 3 == 0)
            .map(|event| event.seq)
            .collect::<Vec<_>>();
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        let bytes = records
            .iter()
            .filter(|event| event.seq % 3 != 0)
            .flat_map(|event| {
                let mut line = serde_json::to_vec(event).expect("serialize event");
                line.push(b'\n');
                line
            })
            .collect::<Vec<_>>();
        fs::write(&path, bytes).expect("write gapped log");

        let log = EventLog::open(path, session).expect("open gapped log");
        assert_eq!(
            log.diagnostics()
                .iter()
                .filter(|diagnostic| diagnostic.skipped)
                .map(|diagnostic| diagnostic.seq)
                .collect::<Vec<_>>(),
            removed
        );
        let projected = crate::session::projection(log).expect("project gapped log");
        assert_eq!(projected.meta.session_id, session);
        assert_eq!(projected.meta.skipped_events.len(), removed.len());
    }

    #[test]
    fn historical_era_fixtures_open_with_stable_projection_invariants() {
        let session = SessionId(Uuid::from_u128(99));
        for schema in [18, 20, 21] {
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(format!("events-schema-{schema}.jsonl"));
            let directory = tempdir().expect("temporary directory");
            let path = directory.path().join("events.jsonl");
            fs::copy(source, &path).expect("copy era fixture");
            let log = EventLog::open(path, session).expect("open era fixture");
            assert!(log.diagnostics().is_empty(), "schema {schema}");
            let projected = crate::session::projection(log).expect("project era fixture");
            assert_eq!(projected.meta.session_id, session);
            assert_eq!(projected.meta.origin, SessionOrigin::Root);
            assert_eq!(
                projected.meta.status,
                cookie_agent_protocol::SessionStatus::Idle
            );
            assert_eq!(projected.meta.last_event_seq, 1);
            assert_eq!(projected.creation_agent.schema.value(), 7);
        }
    }

    #[test]
    fn current_delegation_event_fixture_is_stable_and_readable() {
        let records = current_delegation_records();
        let bytes = records
            .iter()
            .flat_map(|event| {
                let mut line = serde_json::to_vec(event).expect("serialize event");
                line.push(b'\n');
                line
            })
            .collect::<Vec<_>>();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/events-current-delegation.jsonl");
        if std::env::var_os("COOKIE_UPDATE_EVENT_FIXTURE").is_some() {
            fs::write(&fixture, &bytes).expect("update current delegation fixture");
        }
        assert_eq!(
            fs::read(&fixture).expect("current delegation fixture"),
            bytes
        );
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        fs::copy(fixture, &path).expect("copy current fixture");
        let log = EventLog::open(path, records[0].session_id).expect("open current fixture");
        assert!(log.diagnostics().is_empty());
        assert_eq!(log.events().len(), records.len());
    }

    #[test]
    fn event_log_rejects_cross_run_approval_lifecycle() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        let creation = stored_event();
        let session = creation.session_id;
        let run_one = RunId(Uuid::from_u128(1));
        let run_two = RunId(Uuid::from_u128(2));
        let run_started = |seq, run_id| {
            let agent = agent_snapshot("test", AgentMode::Primary);
            event(
                session,
                Some(run_id),
                seq,
                EventPayload::RunStarted {
                    client_run_id: ClientRunId::new(format!("run-{seq}")).expect("client run id"),
                    selection: run_selection("test"),
                    runtime_revision: runtime_revision(),
                    catalog_revision: catalog_revision(),
                    provider_state_revision: provider_revision(),
                    model_revision: model_revision(),
                    agent_revision: agent_revision(),
                    recipe_registry_revision: registry_revision(),
                    manifest_revision: agent.fallback_chain[0].manifest_revision.clone(),
                    selected_suffix: agent.fallback_chain.clone(),
                    internal_agents: Vec::new(),
                    agent: Box::new(agent),
                    input_through_seq: 1,
                },
            )
        };
        let binding =
            cookie_agent_protocol::PreparedResourceDigest::from_canonical_binding_bytes(b"binding");
        let resource = cookie_agent_protocol::PreparedApprovalResource {
            capability: PermissionAction::Bash,
            canonical: cookie_agent_protocol::PreparedResourceIdentity::new("command:test")
                .expect("identity"),
            binding_digest: binding.clone(),
            binding_lifetime: cookie_agent_protocol::PreparedBindingLifetime::RestartStable,
            boundary: cookie_agent_protocol::ApprovalBoundary::Exact,
            source: cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
        };
        let operation = cookie_agent_protocol::PreparedOperationIdentity::new(
            cookie_agent_protocol::Sha256Digest::of_bytes(b"args"),
            vec![cookie_agent_protocol::ApprovalCapability {
                action: PermissionAction::Bash,
                operation: cookie_agent_protocol::PreparedCapabilityOperation::new("bash:execute")
                    .expect("operation"),
            }],
            vec![resource],
            cookie_agent_protocol::Sha256Digest::of_bytes(b"context"),
        )
        .expect("prepared operation");
        let request = cookie_agent_protocol::ApprovalRequest::new(
            cookie_agent_protocol::ApprovalId::new_v7(),
            1,
            ApprovalTrigger::PermissionPolicy,
            operation,
            vec![cookie_agent_protocol::ApprovalEvaluation {
                resource_digest: binding,
                effect: PermissionEffect::Ask,
                trace: cookie_agent_protocol::DecisionTrace {
                    action: PermissionAction::Bash,
                    normalized_resource: "command:test".into(),
                    candidates: Vec::new(),
                    effect: PermissionEffect::Ask,
                    precedence_reason: "test".into(),
                },
            }],
            cookie_agent_protocol::ApprovalConstraints {
                allow_once: true,
                allow_tree_grant: true,
                cancellable: true,
                expires_at: None,
            },
        )
        .expect("approval request");
        let approval_id = request.approval_id();
        let records = [
            creation,
            run_started(2, run_one),
            run_started(3, run_two),
            event(
                session,
                Some(run_one),
                4,
                EventPayload::ApprovalRequested { request },
            ),
            event(
                session,
                Some(run_two),
                5,
                EventPayload::ApprovalEscalated {
                    approval_id,
                    reason_code: ApprovalReasonCode::Escalated,
                },
            ),
        ];
        let bytes = records
            .iter()
            .flat_map(|record| {
                let mut line = serde_json::to_vec(record).expect("serialize record");
                line.push(b'\n');
                line
            })
            .collect::<Vec<_>>();
        fs::write(&path, bytes).expect("write event log");
        assert!(EventLog::open(path, session).is_err());
    }

    #[test]
    fn event_log_accepts_valid_multi_fallback_retry_attribution() {
        assert_log_open(&attribution_records(), true, "valid fallback attribution");
    }

    #[test]
    fn event_log_rejects_forged_attempt_attribution() {
        let records = attribution_records();
        let suffix = match &records[1].payload {
            EventPayload::RunStarted {
                selected_suffix, ..
            } => selected_suffix
                .iter()
                .map(wire_resolved)
                .collect::<Vec<_>>(),
            _ => unreachable!(),
        };

        let mut forged = records.clone();
        let EventPayload::ModelAttemptStarted { fallback_index, .. } = &mut forged[2].payload
        else {
            unreachable!()
        };
        *fallback_index = 1;
        assert_log_open(&forged, false, "wrong fallback index");

        let mut forged = records.clone();
        let EventPayload::ModelAttemptStarted { resolved_model, .. } = &mut forged[2].payload
        else {
            unreachable!()
        };
        *resolved_model = suffix[1].clone();
        assert_log_open(&forged, false, "wrong frozen model");

        let mut forged = records.clone();
        let EventPayload::ModelAttemptStarted { resolved_model, .. } = &mut forged[2].payload
        else {
            unreachable!()
        };
        resolved_model.selection.variant = Some(VariantId::new("fast").expect("variant id"));
        assert_log_open(&forged, false, "wrong frozen variant");

        let mut forged = records.clone();
        let EventPayload::ModelAttemptStarted { resolved_model, .. } = &mut forged[2].payload
        else {
            unreachable!()
        };
        let model = "forged/fallback-zero"
            .parse::<ModelKey>()
            .expect("forged model key");
        resolved_model.selection.model = model.clone();
        resolved_model.provider_id = model.provider_id();
        resolved_model.model_id = model.model_id();
        assert_log_open(&forged, false, "wrong frozen provider");

        let mut forged = records.clone();
        let EventPayload::ModelAttemptStarted {
            prompt_fingerprint, ..
        } = &mut forged[2].payload
        else {
            unreachable!()
        };
        *prompt_fingerprint = Sha256Digest::of_bytes(b"forged prompt");
        assert_log_open(&forged, false, "wrong prompt fingerprint");

        let mut forged = records.clone();
        let EventPayload::ModelAttemptStarted {
            attempt_ordinal, ..
        } = &mut forged[2].payload
        else {
            unreachable!()
        };
        *attempt_ordinal = 2;
        assert_log_open(&forged, false, "noncontiguous attempt ordinal");

        let mut forged = records.clone();
        let EventPayload::ModelAttemptStarted { retry_ordinal, .. } = &mut forged[4].payload else {
            unreachable!()
        };
        *retry_ordinal = 0;
        assert_log_open(&forged, false, "noncontiguous retry ordinal");
    }

    #[test]
    fn event_log_rejects_inconsistent_fallback_transitions() {
        let records = attribution_records();
        let suffix = match &records[1].payload {
            EventPayload::RunStarted {
                selected_suffix, ..
            } => selected_suffix
                .iter()
                .map(wire_resolved)
                .collect::<Vec<_>>(),
            _ => unreachable!(),
        };

        let mut forged = records.clone();
        let EventPayload::ModelFallback {
            to,
            to_fallback_index,
            ..
        } = &mut forged[6].payload
        else {
            unreachable!()
        };
        *to = suffix[2].clone();
        *to_fallback_index = 2;
        assert_log_open(&forged, false, "skipped fallback entry");

        let mut forged = records.clone();
        let EventPayload::ModelFallback { to, .. } = &mut forged[6].payload else {
            unreachable!()
        };
        *to = suffix[2].clone();
        assert_log_open(&forged, false, "fallback target model mismatch");

        let mut forged = records.clone();
        let EventPayload::ModelFallback {
            attempts_on_from, ..
        } = &mut forged[6].payload
        else {
            unreachable!()
        };
        *attempts_on_from = 1;
        assert_log_open(&forged, false, "fallback attempt count mismatch");

        let mut forged = records[..3].to_vec();
        forged.push(event(
            forged[0].session_id,
            forged[1].run_id,
            4,
            EventPayload::ModelFallback {
                from: suffix[0].clone(),
                to: suffix[1].clone(),
                from_fallback_index: 0,
                to_fallback_index: 1,
                attempts_on_from: 1,
                error: fallback_error(),
            },
        ));
        assert_log_open(&forged, false, "fallback before attempt terminal");
    }

    #[tokio::test]
    async fn output_snapshot_handoff_has_no_duplicate_bytes() {
        let hub = OutputHub::new(ToolCallId(Uuid::from_u128(1)), 64);
        hub.emit(OutputStream::Stdout, b"one");
        let (snapshot, mut live) = hub.subscribe(OutputStream::Stdout, 4);
        hub.emit(OutputStream::Stdout, b"two");
        assert_eq!(snapshot.end_offset, 3);
        match live.recv().await.expect("live output") {
            OutputMessage::Delta(delta) => assert_eq!(delta.byte_offset, 3),
            OutputMessage::Gap(_) => panic!("unexpected gap"),
        }
    }

    #[tokio::test]
    async fn finalized_output_subscription_is_closed_after_its_snapshot() {
        let hub = OutputHub::new(ToolCallId(Uuid::from_u128(2)), 64);
        hub.emit(OutputStream::Stdout, b"done");
        hub.finalize();
        let (snapshot, mut live) = hub.subscribe(OutputStream::Stdout, 4);
        assert_eq!(snapshot.end_offset, 4);
        assert!(live.recv().await.is_none());
    }

    #[tokio::test]
    async fn live_output_subscription_closes_at_finalize() {
        let hub = OutputHub::new(ToolCallId(Uuid::from_u128(3)), 64);
        let (_, mut live) = hub.subscribe(OutputStream::Stdout, 4);
        hub.finalize();
        assert!(live.recv().await.is_none());
    }

    #[tokio::test]
    async fn evicted_snapshot_queues_an_explicit_gap_marker() {
        let hub = OutputHub::new(ToolCallId(Uuid::from_u128(4)), 3);
        hub.emit(OutputStream::Stdout, b"one");
        hub.emit(OutputStream::Stdout, b"two");
        let (snapshot, mut live) = hub.subscribe(OutputStream::Stdout, 2);
        assert_eq!(snapshot.start_offset, 3);
        match live.recv().await.expect("snapshot gap") {
            OutputMessage::Gap(gap) => assert_eq!(gap.next_offset, 3),
            OutputMessage::Delta(_) => panic!("expected gap"),
        }
    }

    #[tokio::test]
    async fn finalized_evicted_snapshot_queues_gap_before_closing() {
        let hub = OutputHub::new(ToolCallId(Uuid::from_u128(6)), 3);
        hub.emit(OutputStream::Stdout, b"one");
        hub.emit(OutputStream::Stdout, b"two");
        hub.finalize();
        let (snapshot, mut live) = hub.subscribe(OutputStream::Stdout, 2);

        assert_eq!(snapshot.start_offset, 3);
        match live.recv().await.expect("eviction gap") {
            OutputMessage::Gap(gap) => assert_eq!(gap.next_offset, 3),
            OutputMessage::Delta(_) => panic!("expected gap before the finalized receiver closes"),
        }
        assert!(live.recv().await.is_none());
    }

    #[tokio::test]
    async fn lagging_live_subscriber_receives_a_gap_before_later_delta() {
        let hub = OutputHub::new(ToolCallId(Uuid::from_u128(7)), 64);
        let (_, mut live) = hub.subscribe(OutputStream::Stdout, 2);
        hub.emit(OutputStream::Stdout, b"one");
        hub.emit(OutputStream::Stdout, b"two");
        hub.emit(OutputStream::Stdout, b"three");
        assert!(matches!(live.recv().await, Some(OutputMessage::Delta(_))));
        hub.emit(OutputStream::Stdout, b"four");
        assert!(matches!(live.recv().await, Some(OutputMessage::Delta(_))));
        match live.recv().await.expect("lagging gap") {
            OutputMessage::Gap(gap) => assert_eq!(gap.next_offset, 11),
            OutputMessage::Delta(_) => panic!("expected gap before resumed output"),
        }
        hub.emit(OutputStream::Stdout, b"five");
        match live.recv().await.expect("second lagging gap") {
            OutputMessage::Gap(gap) => assert_eq!(gap.next_offset, 15),
            OutputMessage::Delta(_) => panic!("expected retained loss boundary"),
        }
        match live.recv().await.expect("resumed output") {
            OutputMessage::Delta(delta) => assert_eq!(delta.byte_offset, 15),
            OutputMessage::Gap(_) => panic!("unexpected second gap"),
        }
    }

    #[tokio::test]
    async fn finalize_drains_queued_delta_before_closing() {
        let hub = OutputHub::new(ToolCallId(Uuid::from_u128(5)), 64);
        let (_, mut live) = hub.subscribe(OutputStream::Stdout, 2);
        hub.emit(OutputStream::Stdout, b"done");
        hub.finalize();
        assert!(matches!(live.recv().await, Some(OutputMessage::Delta(_))));
        assert!(live.recv().await.is_none());
    }
}
