//! Project-scoped single-writer delegation journal.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use cookie_agent_protocol::{
    AgentRevision, AgentSnapshot, CatalogRevision, DelegationJournalRecord,
    DelegationJournalSchemaVersion, FrozenModelBinding, InvocationId, ModelRevision,
    ModelSnapshotRevision, ProviderStateRevision, RecipeRegistryRevision, RunId, RuntimeRevision,
    SessionId, Sha256Digest, StoredDelegationJournalRecord, ToolCallId,
};
use thiserror::Error;

use crate::events::{EventLogError, append_jsonl, load_jsonl};

pub use cookie_agent_protocol::{DelegateRequestPayload, DelegationReservation};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationRuntimeRevisions {
    pub manifest_revision: ModelSnapshotRevision,
    pub runtime_revision: RuntimeRevision,
    pub catalog_revision: CatalogRevision,
    pub provider_state_revision: ProviderStateRevision,
    pub model_revision: ModelRevision,
    pub agent_revision: AgentRevision,
    pub recipe_registry_revision: RecipeRegistryRevision,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JournalEntry {
    pub reservation: DelegationReservation,
    pub child_agent: AgentSnapshot,
    pub revisions: DelegationRuntimeRevisions,
    pub selected_suffix: Vec<FrozenModelBinding>,
    pub request_fingerprint: Sha256Digest,
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
        child_agent: AgentSnapshot,
        revisions: DelegationRuntimeRevisions,
        selected_suffix: Vec<FrozenModelBinding>,
        request_fingerprint: Sha256Digest,
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
        for record in load_jsonl::<StoredDelegationJournalRecord>(&path)? {
            apply(&mut state, record.record)?;
        }
        let (sender, receiver) = mpsc::channel();
        let actor_path = path.clone();
        let worker = thread::Builder::new()
            .name("cookie_agent_delegation_journal".into())
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
        child_agent: AgentSnapshot,
        revisions: DelegationRuntimeRevisions,
        selected_suffix: Vec<FrozenModelBinding>,
        request_fingerprint: Sha256Digest,
        request: DelegateRequestPayload,
    ) -> Result<JournalEntry, JournalError> {
        let (reply, receiver) = mpsc::channel();
        self.sender
            .send(JournalCommand::Reserve {
                invocation_id,
                parent_session_id,
                parent_run_id,
                parent_tool_call_id,
                child_agent,
                revisions,
                selected_suffix,
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
                child_agent,
                revisions,
                selected_suffix,
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
                        child_agent,
                        revisions,
                        selected_suffix,
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
    child_agent: AgentSnapshot,
    revisions: DelegationRuntimeRevisions,
    selected_suffix: Vec<FrozenModelBinding>,
    request_fingerprint: Sha256Digest,
    request: DelegateRequestPayload,
) -> Result<JournalEntry, JournalError> {
    if let Some(entry) = state.entries.get(&invocation_id) {
        let same_request = entry.reservation.parent_session_id == parent_session_id
            && entry.reservation.parent_run_id == parent_run_id
            && entry.reservation.parent_tool_call_id == parent_tool_call_id
            && entry.request_fingerprint == request_fingerprint
            && entry.child_agent == child_agent
            && entry.revisions == revisions
            && entry.selected_suffix == selected_suffix
            && entry.request == request;
        return if same_request {
            Ok(entry.clone())
        } else {
            Err(JournalError::Corrupt(invocation_id))
        };
    }
    if request.task.is_empty()
        || selected_suffix.is_empty()
        || selected_suffix
            .iter()
            .any(|binding| binding.manifest_revision != revisions.manifest_revision)
        || child_agent
            .validate_selected_suffix(
                &cookie_agent_protocol::RunSelection {
                    agent: child_agent.agent.clone(),
                    model: selected_suffix
                        .first()
                        .expect("checked nonempty")
                        .selection
                        .clone(),
                },
                &selected_suffix,
            )
            .is_err()
    {
        return Err(JournalError::Corrupt(invocation_id));
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
        child_agent: child_agent.clone(),
        revisions: revisions.clone(),
        selected_suffix: selected_suffix.clone(),
        request_fingerprint: request_fingerprint.clone(),
        task: request.task.clone(),
        request: request.clone(),
        linked: false,
        child_run_id: None,
    };
    state.entries.insert(invocation_id, entry.clone());
    if let Err(error) = append_jsonl(
        path,
        &StoredDelegationJournalRecord {
            delegation_journal_schema_version: DelegationJournalSchemaVersion::current(),
            record: DelegationJournalRecord::DelegationStarted {
                reservation,
                child_agent: Box::new(child_agent),
                manifest_revision: revisions.manifest_revision,
                runtime_revision: revisions.runtime_revision,
                catalog_revision: revisions.catalog_revision,
                provider_state_revision: revisions.provider_state_revision,
                model_revision: revisions.model_revision,
                agent_revision: revisions.agent_revision,
                recipe_registry_revision: revisions.recipe_registry_revision,
                selected_suffix,
                request_fingerprint,
                task: request.task.clone(),
                request,
            },
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
    append_jsonl(
        path,
        &StoredDelegationJournalRecord {
            delegation_journal_schema_version: DelegationJournalSchemaVersion::current(),
            record: DelegationJournalRecord::DelegationLinked { invocation_id },
        },
    )?;
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
        &StoredDelegationJournalRecord {
            delegation_journal_schema_version: DelegationJournalSchemaVersion::current(),
            record: DelegationJournalRecord::DelegationRunStarted {
                invocation_id,
                child_run_id,
            },
        },
    )?;
    state
        .entries
        .get_mut(&invocation_id)
        .expect("checked entry")
        .child_run_id = Some(child_run_id);
    Ok(())
}

fn apply(state: &mut JournalState, record: DelegationJournalRecord) -> Result<(), JournalError> {
    match record {
        DelegationJournalRecord::DelegationStarted {
            reservation,
            child_agent,
            manifest_revision,
            runtime_revision,
            catalog_revision,
            provider_state_revision,
            model_revision,
            agent_revision,
            recipe_registry_revision,
            selected_suffix,
            request_fingerprint,
            task,
            request,
        } => {
            if state.entries.contains_key(&reservation.invocation_id) {
                return Err(JournalError::Corrupt(reservation.invocation_id));
            }
            if task.is_empty()
                || request.task != task
                || selected_suffix.is_empty()
                || selected_suffix
                    .iter()
                    .any(|binding| binding.manifest_revision != manifest_revision)
                || child_agent
                    .validate_selected_suffix(
                        &cookie_agent_protocol::RunSelection {
                            agent: child_agent.agent.clone(),
                            model: selected_suffix[0].selection.clone(),
                        },
                        &selected_suffix,
                    )
                    .is_err()
            {
                return Err(JournalError::Corrupt(reservation.invocation_id));
            }
            state.entries.insert(
                reservation.invocation_id,
                JournalEntry {
                    reservation,
                    child_agent: *child_agent,
                    revisions: DelegationRuntimeRevisions {
                        manifest_revision,
                        runtime_revision,
                        catalog_revision,
                        provider_state_revision,
                        model_revision,
                        agent_revision,
                        recipe_registry_revision,
                    },
                    selected_suffix,
                    request_fingerprint,
                    task,
                    request,
                    linked: false,
                    child_run_id: None,
                },
            );
        }
        DelegationJournalRecord::DelegationLinked { invocation_id } => {
            let entry = state
                .entries
                .get_mut(&invocation_id)
                .ok_or(JournalError::Corrupt(invocation_id))?;
            if entry.linked {
                return Err(JournalError::Corrupt(invocation_id));
            }
            entry.linked = true;
        }
        DelegationJournalRecord::DelegationRunStarted {
            invocation_id,
            child_run_id,
        } => {
            let entry = state
                .entries
                .get_mut(&invocation_id)
                .ok_or(JournalError::Corrupt(invocation_id))?;
            if entry.child_run_id.is_some() {
                return Err(JournalError::Corrupt(invocation_id));
            }
            entry.child_run_id = Some(child_run_id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cookie_agent_protocol::InvocationId;

    use super::DelegateRequestPayload;

    #[test]
    fn delegate_request_payload_is_strict_and_has_no_serde_defaults() {
        assert!(
            serde_json::from_value::<DelegateRequestPayload>(serde_json::json!({
                "task":"report",
                "success_criteria":[],
                "expected_output":null
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<DelegateRequestPayload>(serde_json::json!({
                "task":"report",
                "context":[],
                "success_criteria":[],
                "expected_output":null,
                "legacy":true
            }))
            .is_err()
        );
    }

    #[test]
    fn journal_replay_accepts_only_schema_nine_and_strict_records() {
        let invocation_id = InvocationId::new_v7();
        for value in [
            serde_json::json!({
                "delegation_journal_schema_version":8,
                "record":{"type":"delegation_linked","invocation_id":invocation_id}
            }),
            serde_json::json!({
                "delegation_journal_schema_version":9,
                "record":{"type":"delegation_linked","invocation_id":invocation_id,"legacy":true}
            }),
        ] {
            let directory = tempfile::tempdir().expect("directory");
            let path = directory.path().join("delegation.jsonl");
            fs::write(&path, format!("{value}\n")).expect("write journal");
            assert!(super::DelegationJournal::open(path).is_err());
        }
    }
}
