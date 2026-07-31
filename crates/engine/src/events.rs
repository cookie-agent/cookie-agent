//! Durable session event logs and ephemeral tool-output hubs.

use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookiecode_config::PolicySnapshot;
use cookiecode_protocol::{
    Event, EventEnvelope, OutputDelta, OutputGap, OutputSnapshot, OutputStream, SessionId,
    ToolCallId,
};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
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
}

/// The policy extension is deliberately stored only on the creation record.
/// `EventEnvelope` stays the protocol's event representation; unknown fields
/// are ignored by protocol-only readers.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoredEvent {
    #[serde(flatten)]
    pub envelope: EventEnvelope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicySnapshot>,
}

#[derive(Debug)]
pub struct EventLog {
    path: PathBuf,
    session_id: SessionId,
    events: Mutex<Vec<StoredEvent>>,
}

impl EventLog {
    pub fn create(
        path: PathBuf,
        session_id: SessionId,
        creation: Event,
        policy: PolicySnapshot,
    ) -> Result<Arc<Self>, EventLogError> {
        let log = Arc::new(Self {
            path,
            session_id,
            events: Mutex::new(Vec::new()),
        });
        log.append_inner(None, creation, Some(policy))?;
        Ok(log)
    }

    pub fn open(path: PathBuf, session_id: SessionId) -> Result<Arc<Self>, EventLogError> {
        let records = load_jsonl::<StoredEvent>(&path)?;
        if !matches!(
            records.first().map(|record| &record.envelope.event),
            Some(Event::SessionCreated { .. })
        ) {
            return Err(EventLogError::MissingCreation(path));
        }
        Ok(Arc::new(Self {
            path,
            session_id,
            events: Mutex::new(records),
        }))
    }

    pub fn append(
        &self,
        run_id: Option<cookiecode_protocol::RunId>,
        event: Event,
    ) -> Result<EventEnvelope, EventLogError> {
        self.append_inner(run_id, event, None)
    }

    fn append_inner(
        &self,
        run_id: Option<cookiecode_protocol::RunId>,
        event: Event,
        policy: Option<PolicySnapshot>,
    ) -> Result<EventEnvelope, EventLogError> {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let envelope = EventEnvelope {
            session_id: self.session_id,
            run_id,
            seq: events.last().map_or(1, |event| event.envelope.seq + 1),
            timestamp: Timestamp::now(),
            event,
        };
        let record = StoredEvent {
            envelope: envelope.clone(),
            policy,
        };
        append_jsonl(&self.path, &record)?;
        events.push(record);
        Ok(envelope)
    }

    #[must_use]
    pub fn events(&self) -> Vec<EventEnvelope> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|event| event.envelope.clone())
            .collect()
    }

    #[must_use]
    pub fn policy(&self) -> Option<PolicySnapshot> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .first()
            .and_then(|event| event.policy.clone())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Reads a JSONL file after removing a crash-torn final record.  A malformed
/// complete record is corruption, not a torn tail, and is rejected.
pub fn load_jsonl<T>(path: &Path) -> Result<Vec<T>, EventLogError>
where
    T: for<'de> Deserialize<'de>,
{
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

pub fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<(), EventLogError> {
    let bytes = serde_json::to_vec(value).map_err(|source| EventLogError::Json {
        path: path.to_owned(),
        source,
    })?;
    let created = !path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| EventLogError::Io {
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

pub fn fsync_directory(path: &Path) -> Result<(), EventLogError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
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
    use std::fs;

    use cookiecode_protocol::{OutputStream, ToolCallId};
    use serde_json::Value;
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::{OutputHub, OutputMessage, load_jsonl};

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
    async fn finalize_drains_queued_delta_before_closing() {
        let hub = OutputHub::new(ToolCallId(Uuid::from_u128(5)), 64);
        let (_, mut live) = hub.subscribe(OutputStream::Stdout, 2);
        hub.emit(OutputStream::Stdout, b"done");
        hub.finalize();
        assert!(matches!(live.recv().await, Some(OutputMessage::Delta(_))));
        assert!(live.recv().await.is_none());
    }
}
