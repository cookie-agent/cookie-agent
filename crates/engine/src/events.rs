//! Buffered/durable session event logs and ephemeral tool-output hubs.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::{
    AssistantToolCallRef, AttemptId, EventPayload, EventSchemaVersion, ModelCallId, OutputDelta,
    OutputGap, OutputSnapshot, OutputStream, ProviderItemId, RunId, SessionId, StoredEvent,
    ToolCallId, ToolCallStart,
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
}

#[derive(Debug)]
pub struct EventLog {
    path: PathBuf,
    session_id: SessionId,
    events: Mutex<Vec<StoredEvent>>,
    persisted: AtomicBool,
}

impl EventLog {
    pub fn create(
        path: PathBuf,
        session_id: SessionId,
        creation: EventPayload,
    ) -> Result<Arc<Self>, EventLogError> {
        let log = Arc::new(Self {
            path,
            session_id,
            events: Mutex::new(Vec::new()),
            persisted: AtomicBool::new(true),
        });
        if !matches!(creation, EventPayload::SessionCreated { .. }) {
            return Err(EventLogError::MissingCreation(log.path.clone()));
        }
        log.append_inner(None, creation)?;
        Ok(log)
    }

    pub fn create_buffered(
        path: PathBuf,
        session_id: SessionId,
        creation: EventPayload,
    ) -> Result<Arc<Self>, EventLogError> {
        let log = Arc::new(Self {
            path,
            session_id,
            events: Mutex::new(Vec::new()),
            persisted: AtomicBool::new(false),
        });
        if !matches!(creation, EventPayload::SessionCreated { .. }) {
            return Err(EventLogError::MissingCreation(log.path.clone()));
        }
        log.append_inner(None, creation)?;
        Ok(log)
    }

    pub fn open(path: PathBuf, session_id: SessionId) -> Result<Arc<Self>, EventLogError> {
        let records = load_jsonl::<StoredEvent>(&path)?;
        if !matches!(
            records.first().map(|record| &record.payload),
            Some(EventPayload::SessionCreated { .. })
        ) {
            return Err(EventLogError::MissingCreation(path));
        }
        validate_records(&path, session_id, &records)?;
        Ok(Arc::new(Self {
            path,
            session_id,
            events: Mutex::new(records),
            persisted: AtomicBool::new(true),
        }))
    }

    pub fn append(
        &self,
        run_id: Option<RunId>,
        payload: EventPayload,
    ) -> Result<StoredEvent, EventLogError> {
        self.append_inner(run_id, payload)
    }

    fn append_inner(
        &self,
        run_id: Option<RunId>,
        payload: EventPayload,
    ) -> Result<StoredEvent, EventLogError> {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let event = StoredEvent {
            event_schema_version: EventSchemaVersion::current(),
            session_id: self.session_id,
            run_id,
            seq: events.last().map_or(1, |event| event.seq + 1),
            timestamp: Timestamp::now(),
            payload,
        };
        event.validate().map_err(|error| EventLogError::Corrupt {
            path: self.path.clone(),
            message: error.to_string(),
        })?;
        let mut candidate = events.clone();
        candidate.push(event.clone());
        validate_records(&self.path, self.session_id, &candidate)?;
        if self.persisted.load(Ordering::Acquire) {
            append_jsonl(&self.path, &event)?;
        }
        events.push(event.clone());
        Ok(event)
    }

    #[must_use]
    pub fn events(&self) -> Vec<StoredEvent> {
        cookie_agent_protocol::visible_events(&self.all_events())
    }

    #[must_use]
    pub fn all_events(&self) -> Vec<StoredEvent> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn last_event(&self) -> Option<StoredEvent> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last()
            .cloned()
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
}

#[derive(Debug)]
struct RunAttribution {
    prompt_fingerprint: cookie_agent_protocol::Sha256Digest,
    selected_suffix: Vec<cookie_agent_protocol::ResolvedModelRef>,
    active_fallback_index: usize,
    next_attempt_ordinal: u32,
    attempts_on_active: u32,
    active_attempt: Option<AttemptId>,
}

#[derive(Debug)]
struct AttemptAttribution {
    run_id: RunId,
    resolved_model: cookie_agent_protocol::ResolvedModelRef,
    finished: bool,
}

fn validate_records(
    path: &Path,
    session_id: SessionId,
    records: &[StoredEvent],
) -> Result<(), EventLogError> {
    let mut runs = HashMap::<RunId, RunAttribution>::new();
    let mut approval_owners = HashMap::new();
    let mut attempts = HashMap::<AttemptId, AttemptAttribution>::new();
    let mut turns = HashMap::<u64, (RunId, cookie_agent_protocol::PersistedModelTurn)>::new();
    let mut model_call_owners = HashMap::<(RunId, ModelCallId), AssistantToolCallRef>::new();
    let mut provider_item_owners = HashMap::<(RunId, ProviderItemId), AssistantToolCallRef>::new();
    let mut tool_starts = HashMap::<ToolCallId, (RunId, ToolCallStart)>::new();
    let mut terminated_tools = HashSet::<ToolCallId>::new();
    let mut elided_tools = HashSet::<ToolCallId>::new();
    let mut next_model_turn_seq = 1_u64;
    let mut previous_timestamp = None;
    for (index, record) in records.iter().enumerate() {
        let expected_seq = index as u64 + 1;
        if record.seq != expected_seq {
            return corrupt(
                path,
                format!(
                    "event sequence {} is not contiguous; expected {expected_seq}",
                    record.seq
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
        match &record.payload {
            EventPayload::SessionReverted { through_seq } => {
                if record.run_id.is_some() || *through_seq == 0 || *through_seq >= record.seq {
                    return corrupt(
                        path,
                        "SessionReverted target is not an existing prior event",
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
                    prompt_fingerprint: agent.prompt_fingerprint.clone(),
                    selected_suffix: selected_suffix
                        .iter()
                        .map(crate::policy::wire_resolved)
                        .collect(),
                    active_fallback_index: 0,
                    next_attempt_ordinal: 1,
                    attempts_on_active: 0,
                    active_attempt: None,
                };
                if runs.insert(run_id, attribution).is_some() {
                    return corrupt(path, "run_id has more than one RunStarted event");
                }
            }
            EventPayload::SessionTitleCommitted { change, .. } => {
                let user = matches!(
                    change,
                    cookie_agent_protocol::SessionTitleChange::UserSet { .. }
                        | cookie_agent_protocol::SessionTitleChange::UserClear { .. }
                        | cookie_agent_protocol::SessionTitleChange::UserReset { .. }
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
                let run_id = require_started_run(path, &runs, record.run_id)?;
                if attempts.contains_key(attempt_id) {
                    return corrupt(path, "attempt_id has more than one ModelAttemptStarted");
                }
                let run = runs.get_mut(&run_id).expect("started run is indexed");
                if run.active_attempt.is_some() {
                    return corrupt(
                        path,
                        "ModelAttemptStarted appeared before the prior attempt ended",
                    );
                }
                if *attempt_ordinal != run.next_attempt_ordinal {
                    return corrupt(path, "attempt_ordinal is not contiguous within its run");
                }
                let Ok(fallback_index) = usize::try_from(*fallback_index) else {
                    return corrupt(path, "fallback_index does not index the frozen suffix");
                };
                if fallback_index != run.active_fallback_index {
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
                if *retry_ordinal != run.attempts_on_active {
                    return corrupt(
                        path,
                        "retry_ordinal is not contiguous for the active fallback entry",
                    );
                }
                run.next_attempt_ordinal += 1;
                run.attempts_on_active += 1;
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
            EventPayload::TextDelta { attempt_id, .. }
            | EventPayload::ReasoningDelta { attempt_id, .. } => {
                validate_attempt_owner(path, &attempts, *attempt_id, record.run_id)?;
            }
            EventPayload::AttemptAbandoned { attempt_id } => {
                let run_id = validate_attempt_owner(path, &attempts, *attempt_id, record.run_id)?;
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
                finish_attempt(path, &mut runs, &mut attempts, run_id, *attempt_id, true)?;
                if *model_turn_seq != next_model_turn_seq {
                    return corrupt(path, "model_turn_seq is not contiguous");
                }
                next_model_turn_seq += 1;
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
                if run.active_attempt.is_some() {
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
                if from_index != run.active_fallback_index || to_index != adjacent_index {
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
                if run.attempts_on_active == 0 || *attempts_on_from != run.attempts_on_active {
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
                    return corrupt(path, "tool termination ownership does not match its start");
                }
                if !terminated_tools.insert(termination.tool_call_id) {
                    return corrupt(path, "tool call has more than one terminal event");
                }
            }
            EventPayload::ToolOutputElided { tool_call_id, .. } => {
                let Some((run_id, _)) = tool_starts.get(tool_call_id) else {
                    return corrupt(path, "tool elision appeared before its start");
                };
                if record.run_id != Some(*run_id)
                    || !terminated_tools.contains(tool_call_id)
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
                if record.run_id != Some(*run_id) || terminated_tools.contains(tool_call_id) {
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
                validate_approval_owner(path, &approval_owners, grant.approval_id, record.run_id)?;
            }
            _ => {
                require_started_run(path, &runs, record.run_id)?;
            }
        }
    }
    Ok(())
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
    if run.active_attempt != Some(attempt_id) {
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

fn corrupt(path: &Path, message: impl Into<String>) -> Result<(), EventLogError> {
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

    use cookie_agent_protocol::{
        AgentMode, AgentRevision, ApprovalReasonCode, ApprovalTrigger, AttemptId, CatalogRevision,
        ClientRunId, CwdIdentity, EventPayload, EventSchemaVersion, FrozenModelBinding,
        ModelErrorKind, ModelErrorStage, ModelErrorSummary, ModelKey, ModelRevision, OutputStream,
        PermissionAction, PermissionEffect, ProviderStateRevision, RecipeRegistryRevision, RunId,
        RunSelection, RuntimeRevision, SafeErrorMessage, SessionId, SessionOrigin, Sha256Digest,
        StoredEvent, ToolCallId, VariantId,
    };
    use serde_json::Value;
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::{EventLog, OutputHub, OutputMessage, load_jsonl};
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
            event_schema_version: EventSchemaVersion::current(),
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
            event_schema_version: EventSchemaVersion::current(),
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
    fn stored_event_rejects_unknown_fields_and_non_v7_records() {
        let mut value = serde_json::to_value(stored_event()).expect("serialize record");
        value
            .as_object_mut()
            .expect("record object")
            .insert("legacy".into(), Value::Bool(true));
        assert!(serde_json::from_value::<StoredEvent>(value).is_err());

        let mut value = serde_json::to_value(stored_event()).expect("serialize record");
        value["event_schema_version"] = Value::from(3);
        assert!(serde_json::from_value::<StoredEvent>(value).is_err());
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
