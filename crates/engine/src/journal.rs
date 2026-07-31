//! Project-scoped single-writer delegation journal.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use cookiecode_config::PolicySnapshot;
use cookiecode_protocol::{InvocationId, RunId, SessionId, ToolCallId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::events::{EventLogError, append_jsonl, load_jsonl};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DelegationReservation {
    pub invocation_id: InvocationId,
    pub parent_session_id: SessionId,
    pub parent_run_id: RunId,
    pub parent_tool_call_id: ToolCallId,
    pub child_session_id: SessionId,
}

/// Immutable delegate arguments retained so recovery can reconstruct the
/// child prompt without depending on a provider retry payload.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DelegateRequestPayload {
    pub task: String,
    #[serde(default)]
    pub context: Vec<Value>,
    #[serde(default)]
    pub success_criteria: Vec<String>,
    #[serde(default)]
    pub expected_output: Value,
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalRecord {
    DelegationStarted {
        reservation: DelegationReservation,
        child_policy: Box<PolicySnapshot>,
        request_fingerprint: String,
        #[serde(default)]
        task: String,
        #[serde(default)]
        request: DelegateRequestPayload,
    },
    DelegationLinked {
        invocation_id: InvocationId,
    },
    DelegationRunStarted {
        invocation_id: InvocationId,
        child_run_id: RunId,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct JournalEntry {
    pub reservation: DelegationReservation,
    pub child_policy: PolicySnapshot,
    pub request_fingerprint: String,
    pub task: String,
    pub request: DelegateRequestPayload,
    pub linked: bool,
    pub child_run_id: Option<RunId>,
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error(transparent)]
    Event(#[from] EventLogError),
    #[error("delegation journal corruption for invocation {0}")]
    Corrupt(InvocationId),
    #[error("delegation journal actor stopped")]
    Stopped,
    #[error("delegation journal is poisoned after an append failure; reopen required")]
    Poisoned,
}

#[derive(Default)]
struct JournalState {
    entries: HashMap<InvocationId, JournalEntry>,
    poisoned: bool,
}

#[allow(clippy::large_enum_variant)]
enum JournalCommand {
    Reserve {
        invocation_id: InvocationId,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        child_policy: PolicySnapshot,
        request_fingerprint: String,
        request: DelegateRequestPayload,
        reply: mpsc::Sender<Result<JournalEntry, JournalError>>,
    },
    MarkLinked {
        invocation_id: InvocationId,
        reply: mpsc::Sender<Result<(), JournalError>>,
    },
    MarkRunStarted {
        invocation_id: InvocationId,
        child_run_id: RunId,
        reply: mpsc::Sender<Result<(), JournalError>>,
    },
    Get {
        invocation_id: InvocationId,
        reply: mpsc::Sender<Option<JournalEntry>>,
    },
    Entries {
        reply: mpsc::Sender<Vec<JournalEntry>>,
    },
    Shutdown {
        reply: mpsc::Sender<()>,
    },
}

/// All live journal operations cross this mailbox.  The initial append is
/// performed while the reservation is present in the actor state; on append
/// failure it is poisoned before replying. A write may have reached the file
/// despite returning an error, so only reopening may safely recover it.
#[derive(Debug)]
pub struct DelegationJournal {
    path: PathBuf,
    sender: mpsc::Sender<JournalCommand>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl DelegationJournal {
    pub fn open(path: PathBuf) -> Result<Arc<Self>, JournalError> {
        let mut state = JournalState::default();
        for record in load_jsonl::<JournalRecord>(&path)? {
            apply(&mut state, record)?;
        }
        let (sender, receiver) = mpsc::channel();
        let actor_path = path.clone();
        let worker = thread::Builder::new()
            .name("cookiecode-delegation-journal".into())
            .spawn(move || run_actor(actor_path, state, receiver))
            .expect("spawn delegation journal actor");
        Ok(Arc::new(Self {
            path,
            sender,
            worker: Mutex::new(Some(worker)),
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reserve(
        &self,
        invocation_id: InvocationId,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        child_policy: PolicySnapshot,
        request_fingerprint: String,
        request: DelegateRequestPayload,
    ) -> Result<JournalEntry, JournalError> {
        let (reply, receiver) = mpsc::channel();
        self.sender
            .send(JournalCommand::Reserve {
                invocation_id,
                parent_session_id,
                parent_run_id,
                parent_tool_call_id,
                child_policy,
                request_fingerprint,
                request,
                reply,
            })
            .map_err(|_| JournalError::Stopped)?;
        receiver.recv().map_err(|_| JournalError::Stopped)?
    }

    pub fn mark_linked(&self, invocation_id: InvocationId) -> Result<(), JournalError> {
        let (reply, receiver) = mpsc::channel();
        self.sender
            .send(JournalCommand::MarkLinked {
                invocation_id,
                reply,
            })
            .map_err(|_| JournalError::Stopped)?;
        receiver.recv().map_err(|_| JournalError::Stopped)?
    }

    pub fn mark_run_started(
        &self,
        invocation_id: InvocationId,
        child_run_id: RunId,
    ) -> Result<(), JournalError> {
        let (reply, receiver) = mpsc::channel();
        self.sender
            .send(JournalCommand::MarkRunStarted {
                invocation_id,
                child_run_id,
                reply,
            })
            .map_err(|_| JournalError::Stopped)?;
        receiver.recv().map_err(|_| JournalError::Stopped)?
    }

    #[must_use]
    pub fn get(&self, invocation_id: InvocationId) -> Option<JournalEntry> {
        let (reply, receiver) = mpsc::channel();
        self.sender
            .send(JournalCommand::Get {
                invocation_id,
                reply,
            })
            .ok()?;
        receiver.recv().ok().flatten()
    }

    #[must_use]
    pub fn entries(&self) -> Vec<JournalEntry> {
        let (reply, receiver) = mpsc::channel();
        if self.sender.send(JournalCommand::Entries { reply }).is_err() {
            return Vec::new();
        }
        receiver.recv().unwrap_or_default()
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn shutdown(&self) {
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let (reply, receiver) = mpsc::channel();
            if self.sender.send(JournalCommand::Shutdown { reply }).is_ok() {
                let _ = receiver.recv();
            }
            let _ = worker.join();
        }
    }
}

fn run_actor(path: PathBuf, mut state: JournalState, receiver: mpsc::Receiver<JournalCommand>) {
    while let Ok(command) = receiver.recv() {
        match command {
            JournalCommand::Reserve {
                invocation_id,
                parent_session_id,
                parent_run_id,
                parent_tool_call_id,
                child_policy,
                request_fingerprint,
                request,
                reply,
            } => {
                let result = if state.poisoned {
                    Err(JournalError::Poisoned)
                } else {
                    reserve(
                        &path,
                        &mut state,
                        invocation_id,
                        parent_session_id,
                        parent_run_id,
                        parent_tool_call_id,
                        child_policy,
                        request_fingerprint,
                        request,
                    )
                };
                state.poisoned |= matches!(&result, Err(JournalError::Event(_)));
                let _ = reply.send(result);
            }
            JournalCommand::MarkLinked {
                invocation_id,
                reply,
            } => {
                let result = if state.poisoned {
                    Err(JournalError::Poisoned)
                } else {
                    mark_linked(&path, &mut state, invocation_id)
                };
                state.poisoned |= matches!(&result, Err(JournalError::Event(_)));
                let _ = reply.send(result);
            }
            JournalCommand::MarkRunStarted {
                invocation_id,
                child_run_id,
                reply,
            } => {
                let result = if state.poisoned {
                    Err(JournalError::Poisoned)
                } else {
                    mark_run_started(&path, &mut state, invocation_id, child_run_id)
                };
                state.poisoned |= matches!(&result, Err(JournalError::Event(_)));
                let _ = reply.send(result);
            }
            JournalCommand::Get {
                invocation_id,
                reply,
            } => {
                let _ = reply.send(state.entries.get(&invocation_id).cloned());
            }
            JournalCommand::Entries { reply } => {
                let _ = reply.send(state.entries.values().cloned().collect());
            }
            JournalCommand::Shutdown { reply } => {
                let _ = reply.send(());
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn reserve(
    path: &Path,
    state: &mut JournalState,
    invocation_id: InvocationId,
    parent_session_id: SessionId,
    parent_run_id: RunId,
    parent_tool_call_id: ToolCallId,
    child_policy: PolicySnapshot,
    request_fingerprint: String,
    request: DelegateRequestPayload,
) -> Result<JournalEntry, JournalError> {
    if let Some(entry) = state.entries.get(&invocation_id) {
        let same_request = entry.reservation.parent_session_id == parent_session_id
            && entry.reservation.parent_run_id == parent_run_id
            && entry.reservation.parent_tool_call_id == parent_tool_call_id
            && entry.request_fingerprint == request_fingerprint
            && entry.child_policy == child_policy
            && entry.request == request;
        return if same_request {
            Ok(entry.clone())
        } else {
            Err(JournalError::Corrupt(invocation_id))
        };
    }
    // The actor allocates the child ID together with its in-memory reservation,
    // before exposing either to a concurrent re-delivery.
    let reservation = DelegationReservation {
        invocation_id,
        parent_session_id,
        parent_run_id,
        parent_tool_call_id,
        child_session_id: SessionId::new_v7(),
    };
    let entry = JournalEntry {
        reservation: reservation.clone(),
        child_policy: child_policy.clone(),
        request_fingerprint: request_fingerprint.clone(),
        task: request.task.clone(),
        request: request.clone(),
        linked: false,
        child_run_id: None,
    };
    state.entries.insert(invocation_id, entry.clone());
    if let Err(error) = append_jsonl(
        path,
        &JournalRecord::DelegationStarted {
            reservation,
            child_policy: Box::new(child_policy),
            request_fingerprint,
            task: request.task.clone(),
            request,
        },
    ) {
        state.entries.remove(&invocation_id);
        return Err(error.into());
    }
    Ok(entry)
}

fn mark_linked(
    path: &Path,
    state: &mut JournalState,
    invocation_id: InvocationId,
) -> Result<(), JournalError> {
    let Some(entry) = state.entries.get(&invocation_id) else {
        return Err(JournalError::Corrupt(invocation_id));
    };
    if entry.linked {
        return Ok(());
    }
    append_jsonl(path, &JournalRecord::DelegationLinked { invocation_id })?;
    state
        .entries
        .get_mut(&invocation_id)
        .expect("checked entry")
        .linked = true;
    Ok(())
}

fn mark_run_started(
    path: &Path,
    state: &mut JournalState,
    invocation_id: InvocationId,
    child_run_id: RunId,
) -> Result<(), JournalError> {
    let Some(entry) = state.entries.get(&invocation_id) else {
        return Err(JournalError::Corrupt(invocation_id));
    };
    if entry.child_run_id == Some(child_run_id) {
        return Ok(());
    }
    if entry.child_run_id.is_some() {
        return Err(JournalError::Corrupt(invocation_id));
    }
    append_jsonl(
        path,
        &JournalRecord::DelegationRunStarted {
            invocation_id,
            child_run_id,
        },
    )?;
    state
        .entries
        .get_mut(&invocation_id)
        .expect("checked entry")
        .child_run_id = Some(child_run_id);
    Ok(())
}

fn apply(state: &mut JournalState, record: JournalRecord) -> Result<(), JournalError> {
    match record {
        JournalRecord::DelegationStarted {
            reservation,
            child_policy,
            request_fingerprint,
            task,
            mut request,
        } => {
            if let Some(previous) = state.entries.get(&reservation.invocation_id) {
                if previous.reservation != reservation
                    || previous.request_fingerprint != request_fingerprint
                    || previous.child_policy != *child_policy
                    || previous.task != task
                {
                    return Err(JournalError::Corrupt(reservation.invocation_id));
                }
                return Ok(());
            }
            if request.task.is_empty() {
                request.task = task.clone();
            }
            state.entries.insert(
                reservation.invocation_id,
                JournalEntry {
                    reservation,
                    child_policy: *child_policy,
                    request_fingerprint,
                    task,
                    request,
                    linked: false,
                    child_run_id: None,
                },
            );
        }
        JournalRecord::DelegationLinked { invocation_id } => {
            state
                .entries
                .get_mut(&invocation_id)
                .ok_or(JournalError::Corrupt(invocation_id))?
                .linked = true;
        }
        JournalRecord::DelegationRunStarted {
            invocation_id,
            child_run_id,
        } => {
            state
                .entries
                .get_mut(&invocation_id)
                .ok_or(JournalError::Corrupt(invocation_id))?
                .child_run_id = Some(child_run_id);
        }
    }
    Ok(())
}
