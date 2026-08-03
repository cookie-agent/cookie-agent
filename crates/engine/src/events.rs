//! Durable session event logs and ephemeral tool-output hubs.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_config::PolicySnapshot;
use cookie_agent_protocol::{
    Event, EventEnvelope, EventSchemaVersion, OutputDelta, OutputGap, OutputSnapshot, OutputStream,
    SessionId, ToolCallId,
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
    #[error("corrupt event log at {path}: {message}")]
    Corrupt { path: PathBuf, message: String },
    #[error("event log {path} uses unsupported schema version {found}; expected {expected}")]
    UnsupportedSchemaVersion {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
}

/// The policy extension is deliberately stored only on the creation record.
/// `EventEnvelope` stays the protocol's event representation; unknown fields
/// are ignored by protocol-only readers.
#[derive(Clone, Debug, Serialize)]
pub struct StoredEvent {
    #[serde(flatten)]
    pub envelope: EventEnvelope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicySnapshot>,
}

impl<'de> Deserialize<'de> for StoredEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireStoredEvent {
            schema_version: EventSchemaVersion,
            session_id: SessionId,
            run_id: Option<cookie_agent_protocol::RunId>,
            seq: u64,
            timestamp: Timestamp,
            event: Event,
            policy: Option<PolicySnapshot>,
        }

        let wire = WireStoredEvent::deserialize(deserializer)?;
        Ok(Self {
            envelope: EventEnvelope {
                schema_version: wire.schema_version,
                session_id: wire.session_id,
                run_id: wire.run_id,
                seq: wire.seq,
                timestamp: wire.timestamp,
                event: wire.event,
            },
            policy: wire.policy,
        })
    }
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
        validate_records(&path, session_id, &records)?;
        Ok(Arc::new(Self {
            path,
            session_id,
            events: Mutex::new(records),
        }))
    }

    pub fn append(
        &self,
        run_id: Option<cookie_agent_protocol::RunId>,
        event: Event,
    ) -> Result<EventEnvelope, EventLogError> {
        self.append_inner(run_id, event, None)
    }

    fn append_inner(
        &self,
        run_id: Option<cookie_agent_protocol::RunId>,
        event: Event,
        policy: Option<PolicySnapshot>,
    ) -> Result<EventEnvelope, EventLogError> {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let envelope = EventEnvelope {
            schema_version: EventSchemaVersion::current(),
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

fn validate_records(
    path: &Path,
    session_id: SessionId,
    records: &[StoredEvent],
) -> Result<(), EventLogError> {
    let mut runs = HashSet::new();
    let mut approval_owners = HashMap::new();
    let mut previous_timestamp = None;
    for (index, record) in records.iter().enumerate() {
        let envelope = &record.envelope;
        let expected_seq = index as u64 + 1;
        if envelope.seq != expected_seq {
            return corrupt(
                path,
                format!(
                    "event sequence {} is not contiguous; expected {expected_seq}",
                    envelope.seq
                ),
            );
        }
        if envelope.session_id != session_id {
            return corrupt(
                path,
                "event envelope session ID does not match its directory",
            );
        }
        if previous_timestamp.is_some_and(|timestamp| envelope.timestamp < timestamp) {
            return corrupt(path, "event timestamps are not monotonic");
        }
        previous_timestamp = Some(envelope.timestamp);
        if index == 0 {
            let Event::SessionCreated { meta } = &envelope.event else {
                return Err(EventLogError::MissingCreation(path.to_owned()));
            };
            if envelope.run_id.is_some() || meta.id != session_id || record.policy.is_none() {
                return corrupt(path, "invalid initial SessionCreated record");
            }
            continue;
        }
        if record.policy.is_some() || matches!(envelope.event, Event::SessionCreated { .. }) {
            return corrupt(
                path,
                "creation policy or SessionCreated appeared after sequence 1",
            );
        }
        match &envelope.event {
            Event::RunStarted { .. } => {
                let Some(run_id) = envelope.run_id else {
                    return corrupt(path, "RunStarted is missing run_id");
                };
                if !runs.insert(run_id) {
                    return corrupt(path, "run_id has more than one RunStarted event");
                }
            }
            Event::SessionTitleCommitted { commit, .. } => {
                let user = matches!(
                    commit,
                    cookie_agent_protocol::SessionTitleCommit::UserSet { .. }
                        | cookie_agent_protocol::SessionTitleCommit::UserClear { .. }
                        | cookie_agent_protocol::SessionTitleCommit::UserReset { .. }
                );
                if user != envelope.run_id.is_none() {
                    return corrupt(path, "SessionTitleCommitted has inconsistent run ownership");
                }
                if let Some(run_id) = envelope.run_id
                    && !runs.contains(&run_id)
                {
                    return corrupt(path, "session title references a run before RunStarted");
                }
            }
            Event::ApprovalRequested { request } => {
                let Some(run_id) = envelope.run_id else {
                    return corrupt(path, "ApprovalRequested is missing run_id");
                };
                if !runs.contains(&run_id) {
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
            Event::ApprovalEvaluated { approval_id, .. }
            | Event::ApprovalEscalated { approval_id, .. }
            | Event::ApprovalUserDecisionRecorded { approval_id, .. }
            | Event::ApprovalFinalized { approval_id, .. }
            | Event::ApprovalCancelled { approval_id, .. }
            | Event::ApprovalDoomLoopDetected { approval_id, .. } => {
                validate_approval_owner(path, &approval_owners, *approval_id, envelope.run_id)?;
            }
            Event::TreeApprovalGrantCommitted { grant } => {
                validate_approval_owner(
                    path,
                    &approval_owners,
                    grant.approval_id(),
                    envelope.run_id,
                )?;
            }
            _ => {
                let Some(run_id) = envelope.run_id else {
                    return corrupt(path, "run-owned event is missing run_id");
                };
                if !runs.contains(&run_id) {
                    return corrupt(path, "event references a run before RunStarted");
                }
            }
        }
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

fn corrupt(path: &Path, message: impl Into<String>) -> Result<(), EventLogError> {
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
    use std::{collections::BTreeSet, fs};

    use cookie_agent_config::{
        AgentType, DelegationPolicy, DepthLimit, PolicySnapshot, ProfileSnapshot,
        ResolvedPermissions, ResultLimits,
    };
    use cookie_agent_protocol::{
        AgentType as WireAgentType, ApprovalReasonCode, ApprovalTrigger, DelegationSnapshot, Event,
        EventEnvelope, OutputStream, ProfileIdentity, ProfileSnapshot as WireProfileSnapshot,
        RunId, SessionId, SessionMeta, SessionOrigin, ToolCallId,
    };
    use serde_json::Value;
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::{EventLog, OutputHub, OutputMessage, StoredEvent, load_jsonl};

    fn stored_event() -> StoredEvent {
        let session_id = SessionId(Uuid::from_u128(99));
        StoredEvent {
            envelope: EventEnvelope {
                schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
                session_id,
                run_id: None,
                seq: 1,
                timestamp: jiff::Timestamp::now(),
                event: Event::SessionCreated {
                    meta: SessionMeta {
                        id: session_id,
                        origin: SessionOrigin::Root,
                        cwd: ".".into(),
                        profile: WireProfileSnapshot {
                            name: "test".into(),
                            agent_type: WireAgentType::Primary,
                            models: Vec::new(),
                            tools: Vec::new(),
                            delegation: DelegationSnapshot {
                                enabled: false,
                                allowed_profiles: Vec::new(),
                                depth_limit: cookie_agent_protocol::DepthLimit::Unlimited,
                                result_limit_bytes: 1024,
                            },
                            permission_rules: Vec::new(),
                        },
                        title: None,
                    },
                },
            },
            policy: Some(PolicySnapshot {
                profile: ProfileSnapshot {
                    name: "test".into(),
                    r#type: AgentType::Primary,
                },
                models: Vec::new(),
                tools: BTreeSet::new(),
                permissions: ResolvedPermissions { rules: Vec::new() },
                delegation: DelegationPolicy {
                    enabled: false,
                    allowed_profiles: BTreeSet::new(),
                    depth_limit: DepthLimit::Unlimited,
                },
                result_limits: ResultLimits::default(),
            }),
        }
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
    fn stored_event_rejects_unknown_fields_and_v3_records() {
        let mut value = serde_json::to_value(stored_event()).expect("serialize record");
        value
            .as_object_mut()
            .expect("record object")
            .insert("legacy".into(), Value::Bool(true));
        assert!(serde_json::from_value::<StoredEvent>(value).is_err());

        let mut value = serde_json::to_value(stored_event()).expect("serialize record");
        value["schema_version"] = Value::from(3);
        assert!(serde_json::from_value::<StoredEvent>(value).is_err());
    }

    #[test]
    fn event_log_rejects_malformed_creation_identity_and_sequence() {
        let expected = SessionId(Uuid::from_u128(99));
        let malformed = [
            {
                let mut record = stored_event();
                record.envelope.seq = 2;
                record
            },
            {
                let mut record = stored_event();
                record.envelope.session_id = SessionId(Uuid::from_u128(100));
                record
            },
            {
                let mut record = stored_event();
                let Event::SessionCreated { meta } = &mut record.envelope.event else {
                    unreachable!()
                };
                meta.id = SessionId(Uuid::from_u128(100));
                record
            },
            {
                let mut record = stored_event();
                record.envelope.run_id = Some(cookie_agent_protocol::RunId(Uuid::from_u128(1)));
                record
            },
            {
                let mut record = stored_event();
                record.policy = None;
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
    fn event_log_rejects_cross_run_approval_lifecycle() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        let creation = stored_event();
        let session = creation.envelope.session_id;
        let Event::SessionCreated { meta } = &creation.envelope.event else {
            unreachable!()
        };
        let profile = meta.profile.clone();
        let run_one = RunId(Uuid::from_u128(1));
        let run_two = RunId(Uuid::from_u128(2));
        let run_started = |seq, run_id| StoredEvent {
            envelope: EventEnvelope {
                schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
                session_id: session,
                run_id: Some(run_id),
                seq,
                timestamp: jiff::Timestamp::now(),
                event: Event::RunStarted {
                    client_run_id: format!("run-{seq}"),
                    input: "input".into(),
                    profile: profile.clone(),
                    current_profile: ProfileIdentity {
                        name: profile.name.clone(),
                        agent_type: profile.agent_type,
                    },
                },
            },
            policy: None,
        };
        let binding =
            cookie_agent_protocol::PreparedResourceDigest::from_canonical_binding_bytes(b"binding");
        let resource = cookie_agent_protocol::PreparedApprovalResource {
            capability: cookie_agent_protocol::ActionKind::Bash,
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
                action: cookie_agent_protocol::ActionKind::Bash,
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
                effect: cookie_agent_protocol::Effect::Ask,
                trace: cookie_agent_protocol::DecisionTrace {
                    action: cookie_agent_protocol::ActionKind::Bash,
                    normalized_resource: "command:test".into(),
                    candidates: Vec::new(),
                    effect: cookie_agent_protocol::Effect::Ask,
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
            StoredEvent {
                envelope: EventEnvelope {
                    schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
                    session_id: session,
                    run_id: Some(run_one),
                    seq: 4,
                    timestamp: jiff::Timestamp::now(),
                    event: Event::ApprovalRequested { request },
                },
                policy: None,
            },
            StoredEvent {
                envelope: EventEnvelope {
                    schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
                    session_id: session,
                    run_id: Some(run_two),
                    seq: 5,
                    timestamp: jiff::Timestamp::now(),
                    event: Event::ApprovalEscalated {
                        approval_id,
                        reason_code: ApprovalReasonCode::Escalated,
                    },
                },
                policy: None,
            },
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
