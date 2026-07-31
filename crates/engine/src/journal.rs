//! Project-scoped single-writer delegation journal.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
};

use cookiecode_config::PolicySnapshot;
use cookiecode_protocol::{InvocationId, RunId, SessionId, ToolCallId};
use serde::{Deserialize, Serialize};
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalRecord {
    DelegationStarted {
        reservation: DelegationReservation,
        child_policy: Box<PolicySnapshot>,
        request_fingerprint: String,
    },
    DelegationLinked {
        invocation_id: InvocationId,
    },
    DelegationRunStarted {
        invocation_id: InvocationId,
        child_run_id: RunId,
    },
}

#[derive(Clone, Debug)]
pub struct JournalEntry {
    pub reservation: DelegationReservation,
    pub child_policy: PolicySnapshot,
    pub request_fingerprint: String,
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
}

#[derive(Default)]
struct JournalState {
    entries: HashMap<InvocationId, JournalEntry>,
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
}

/// All live journal operations cross this mailbox.  The initial append is
/// performed while the reservation is present in the actor state; on append
/// failure it is removed before replying, preserving retry semantics.
#[derive(Debug)]
pub struct DelegationJournal {
    path: PathBuf,
    sender: mpsc::Sender<JournalCommand>,
}

impl DelegationJournal {
    pub fn open(path: PathBuf) -> Result<Arc<Self>, JournalError> {
        let mut state = JournalState::default();
        for record in load_jsonl::<JournalRecord>(&path)? {
            apply(&mut state, record)?;
        }
        let (sender, receiver) = mpsc::channel();
        let actor_path = path.clone();
        thread::Builder::new()
            .name("cookiecode-delegation-journal".into())
            .spawn(move || run_actor(actor_path, state, receiver))
            .expect("spawn delegation journal actor");
        Ok(Arc::new(Self { path, sender }))
    }

    pub fn reserve(
        &self,
        invocation_id: InvocationId,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        child_policy: PolicySnapshot,
        request_fingerprint: String,
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
                reply,
            } => {
                let result = reserve(
                    &path,
                    &mut state,
                    invocation_id,
                    parent_session_id,
                    parent_run_id,
                    parent_tool_call_id,
                    child_policy,
                    request_fingerprint,
                );
                let _ = reply.send(result);
            }
            JournalCommand::MarkLinked {
                invocation_id,
                reply,
            } => {
                let result = mark_linked(&path, &mut state, invocation_id);
                let _ = reply.send(result);
            }
            JournalCommand::MarkRunStarted {
                invocation_id,
                child_run_id,
                reply,
            } => {
                let result = mark_run_started(&path, &mut state, invocation_id, child_run_id);
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
) -> Result<JournalEntry, JournalError> {
    if let Some(entry) = state.entries.get(&invocation_id) {
        let same_request = entry.reservation.parent_session_id == parent_session_id
            && entry.reservation.parent_run_id == parent_run_id
            && entry.reservation.parent_tool_call_id == parent_tool_call_id
            && entry.request_fingerprint == request_fingerprint;
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
        } => {
            if let Some(previous) = state.entries.get(&reservation.invocation_id) {
                if previous.reservation != reservation
                    || previous.request_fingerprint != request_fingerprint
                {
                    return Err(JournalError::Corrupt(reservation.invocation_id));
                }
                return Ok(());
            }
            state.entries.insert(
                reservation.invocation_id,
                JournalEntry {
                    reservation,
                    child_policy: *child_policy,
                    request_fingerprint,
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
