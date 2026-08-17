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
    SessionId, SessionTitle, Sha256Digest, StoredDelegationJournalRecord, ToolCallId,
};
use thiserror::Error;

use crate::events::{EventLogError, append_jsonl, load_jsonl};

pub use cookie_agent_protocol::DelegationReservation;

#[derive(Clone, Debug, PartialEq)]
pub struct DelegateRequestPayload {
    pub description: String,
    pub prompt: String,
    pub title: SessionTitle,
    pub resume_session_id: Option<SessionId>,
    pub inherit_context: bool,
    pub seeded_context: Vec<cookie_agent_protocol::DelegatedContextTurn>,
    pub background: Option<bool>,
}

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
    pub prompt: String,
    pub request: DelegateRequestPayload,
    pub linked: bool,
    pub child_run_id: Option<RunId>,
    pub run_attached: bool,
    pub terminal_status: Option<cookie_agent_protocol::SessionStatus>,
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error(transparent)]
    Event(#[from] EventLogError),
    #[error("delegation journal corruption for invocation {0}")]
    Corrupt(InvocationId),
    #[error(
        "schema-12 resume run transition is ambiguous for invocation {0}; move the affected project's delegations.jsonl aside and restart to discard in-flight delegation recovery state (session event logs remain intact)"
    )]
    AmbiguousSchemaTwelveResume(InvocationId),
    #[error("delegation journal actor stopped")]
    Stopped,
    #[error("delegation journal is poisoned after an append failure; reopen required")]
    Poisoned,
}

#[derive(Default)]
struct JournalState {
    entries: HashMap<InvocationId, JournalEntry>,
    order: Vec<InvocationId>,
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
    MarkRunAttached {
        invocation_id: InvocationId,
        child_run_id: RunId,
        reply: mpsc::Sender<Result<(), JournalError>>,
    },
    MarkTerminated {
        invocation_id: InvocationId,
        status: cookie_agent_protocol::SessionStatus,
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
            apply(
                &mut state,
                record.delegation_journal_schema_version.value(),
                record.record,
            )?;
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

    pub fn mark_run_attached(
        &self,
        invocation_id: InvocationId,
        child_run_id: RunId,
    ) -> Result<(), JournalError> {
        let (reply, receiver) = mpsc::channel();
        self.sender
            .send(JournalCommand::MarkRunAttached {
                invocation_id,
                child_run_id,
                reply,
            })
            .map_err(|_| JournalError::Stopped)?;
        receiver.recv().map_err(|_| JournalError::Stopped)?
    }

    pub fn mark_terminated(
        &self,
        invocation_id: InvocationId,
        status: cookie_agent_protocol::SessionStatus,
    ) -> Result<(), JournalError> {
        let (reply, receiver) = mpsc::channel();
        self.sender
            .send(JournalCommand::MarkTerminated {
                invocation_id,
                status,
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
            JournalCommand::MarkRunAttached {
                invocation_id,
                child_run_id,
                reply,
            } => {
                let result = if state.poisoned {
                    Err(JournalError::Poisoned)
                } else {
                    mark_run_attached(&path, &mut state, invocation_id, child_run_id)
                };
                state.poisoned |= matches!(&result, Err(JournalError::Event(_)));
                let _ = reply.send(result);
            }
            JournalCommand::MarkTerminated {
                invocation_id,
                status,
                reply,
            } => {
                let result = if state.poisoned {
                    Err(JournalError::Poisoned)
                } else {
                    mark_terminated(&path, &mut state, invocation_id, status)
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
                let _ = reply.send(
                    state
                        .order
                        .iter()
                        .filter_map(|invocation_id| state.entries.get(invocation_id).cloned())
                        .collect(),
                );
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
    if !valid_request(&request)
        || request.background.is_none()
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
    let child_session_id = request.resume_session_id.unwrap_or_else(SessionId::new_v7);
    let reservation = DelegationReservation {
        invocation_id,
        parent_session_id,
        parent_run_id,
        parent_tool_call_id,
        child_session_id,
    };
    let entry = JournalEntry {
        reservation: reservation.clone(),
        child_agent: child_agent.clone(),
        revisions: revisions.clone(),
        selected_suffix: selected_suffix.clone(),
        request_fingerprint: request_fingerprint.clone(),
        prompt: request.prompt.clone(),
        request: request.clone(),
        linked: false,
        child_run_id: None,
        run_attached: false,
        terminal_status: None,
    };
    state.entries.insert(invocation_id, entry.clone());
    if let Err(error) = append_jsonl(
        path,
        &StoredDelegationJournalRecord {
            delegation_journal_schema_version: DelegationJournalSchemaVersion::current(),
            record: DelegationJournalRecord::DelegationStartedV4 {
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
                prompt: request.prompt.clone(),
                request: cookie_agent_protocol::DelegateRequestPayloadV4 {
                    description: request.description,
                    prompt: request.prompt,
                    title: request.title,
                    resume_session_id: request.resume_session_id,
                    inherit_context: request.inherit_context,
                    seeded_context: request.seeded_context,
                    background: request.background.expect("checked execution mode"),
                },
            },
        },
    ) {
        state.entries.remove(&invocation_id);
        return Err(error.into());
    }
    state.order.push(invocation_id);
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
    if entry.terminal_status.is_some() {
        return Err(JournalError::Corrupt(invocation_id));
    }
    if entry.child_run_id == Some(child_run_id) && !entry.run_attached {
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

fn mark_run_attached(
    path: &Path,
    state: &mut JournalState,
    invocation_id: InvocationId,
    child_run_id: RunId,
) -> Result<(), JournalError> {
    let Some(entry) = state.entries.get(&invocation_id) else {
        return Err(JournalError::Corrupt(invocation_id));
    };
    if entry.terminal_status.is_some() || entry.request.resume_session_id.is_none() {
        return Err(JournalError::Corrupt(invocation_id));
    }
    if entry.child_run_id == Some(child_run_id) && entry.run_attached {
        return Ok(());
    }
    if entry.child_run_id.is_some() {
        return Err(JournalError::Corrupt(invocation_id));
    }
    append_jsonl(
        path,
        &StoredDelegationJournalRecord {
            delegation_journal_schema_version: DelegationJournalSchemaVersion::current(),
            record: DelegationJournalRecord::DelegationRunAttached {
                invocation_id,
                child_run_id,
            },
        },
    )?;
    let entry = state
        .entries
        .get_mut(&invocation_id)
        .expect("checked entry");
    entry.child_run_id = Some(child_run_id);
    entry.run_attached = true;
    Ok(())
}

fn mark_terminated(
    path: &Path,
    state: &mut JournalState,
    invocation_id: InvocationId,
    status: cookie_agent_protocol::SessionStatus,
) -> Result<(), JournalError> {
    let Some(entry) = state.entries.get(&invocation_id) else {
        return Err(JournalError::Corrupt(invocation_id));
    };
    if !matches!(
        status,
        cookie_agent_protocol::SessionStatus::Failed
            | cookie_agent_protocol::SessionStatus::Cancelled
    ) {
        return Err(JournalError::Corrupt(invocation_id));
    }
    if entry.terminal_status == Some(status) {
        return Ok(());
    }
    if entry.terminal_status.is_some() {
        return Err(JournalError::Corrupt(invocation_id));
    }
    if entry.child_run_id.is_some() && !entry.run_attached {
        return Err(JournalError::Corrupt(invocation_id));
    }
    append_jsonl(
        path,
        &StoredDelegationJournalRecord {
            delegation_journal_schema_version: DelegationJournalSchemaVersion::current(),
            record: DelegationJournalRecord::DelegationTerminated {
                invocation_id,
                status,
            },
        },
    )?;
    state
        .entries
        .get_mut(&invocation_id)
        .expect("checked entry")
        .terminal_status = Some(status);
    Ok(())
}

fn apply(
    state: &mut JournalState,
    schema_version: u32,
    record: DelegationJournalRecord,
) -> Result<(), JournalError> {
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
            request: legacy_request,
        } => {
            if legacy_request.task != task {
                return Err(JournalError::Corrupt(reservation.invocation_id));
            }
            let title = SessionTitle::new(task.clone())
                .map_err(|_| JournalError::Corrupt(reservation.invocation_id))?;
            apply_started(
                state,
                reservation,
                *child_agent,
                DelegationRuntimeRevisions {
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
                task.clone(),
                DelegateRequestPayload {
                    description: task.clone(),
                    prompt: task,
                    title,
                    resume_session_id: None,
                    inherit_context: false,
                    seeded_context: Vec::new(),
                    background: None,
                },
            )?;
        }
        DelegationJournalRecord::DelegationStartedV2 {
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
            prompt,
            request,
        } => {
            let request = DelegateRequestPayload {
                description: request.description,
                prompt: request.prompt,
                title: request.title,
                resume_session_id: None,
                inherit_context: false,
                seeded_context: Vec::new(),
                background: None,
            };
            apply_started(
                state,
                reservation,
                *child_agent,
                DelegationRuntimeRevisions {
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
                prompt,
                request,
            )?;
        }
        DelegationJournalRecord::DelegationStartedV3 {
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
            prompt,
            request,
        } => {
            let request = DelegateRequestPayload {
                description: request.description,
                prompt: request.prompt,
                title: request.title,
                resume_session_id: request.resume_session_id,
                inherit_context: request.inherit_context,
                seeded_context: request.seeded_context,
                background: None,
            };
            apply_started(
                state,
                reservation,
                *child_agent,
                DelegationRuntimeRevisions {
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
                prompt,
                request,
            )?;
        }
        DelegationJournalRecord::DelegationStartedV4 {
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
            prompt,
            request,
        } => {
            let request = DelegateRequestPayload {
                description: request.description,
                prompt: request.prompt,
                title: request.title,
                resume_session_id: request.resume_session_id,
                inherit_context: request.inherit_context,
                seeded_context: request.seeded_context,
                background: Some(request.background),
            };
            apply_started(
                state,
                reservation,
                *child_agent,
                DelegationRuntimeRevisions {
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
                prompt,
                request,
            )?;
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
            if schema_version == 12 && entry.request.resume_session_id.is_some() {
                return Err(JournalError::AmbiguousSchemaTwelveResume(invocation_id));
            }
            if entry.child_run_id.is_some() || entry.terminal_status.is_some() {
                return Err(JournalError::Corrupt(invocation_id));
            }
            entry.child_run_id = Some(child_run_id);
            entry.run_attached = false;
        }
        DelegationJournalRecord::DelegationRunAttached {
            invocation_id,
            child_run_id,
        } => {
            let entry = state
                .entries
                .get_mut(&invocation_id)
                .ok_or(JournalError::Corrupt(invocation_id))?;
            if entry.child_run_id.is_some()
                || entry.terminal_status.is_some()
                || entry.request.resume_session_id.is_none()
            {
                return Err(JournalError::Corrupt(invocation_id));
            }
            entry.child_run_id = Some(child_run_id);
            entry.run_attached = true;
        }
        DelegationJournalRecord::DelegationTerminated {
            invocation_id,
            status,
        } => {
            let entry = state
                .entries
                .get_mut(&invocation_id)
                .ok_or(JournalError::Corrupt(invocation_id))?;
            if entry.terminal_status.is_some()
                || (entry.child_run_id.is_some() && !entry.run_attached)
                || !matches!(
                    status,
                    cookie_agent_protocol::SessionStatus::Failed
                        | cookie_agent_protocol::SessionStatus::Cancelled
                )
            {
                return Err(JournalError::Corrupt(invocation_id));
            }
            entry.terminal_status = Some(status);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_started(
    state: &mut JournalState,
    reservation: DelegationReservation,
    child_agent: cookie_agent_protocol::AgentSnapshot,
    revisions: DelegationRuntimeRevisions,
    selected_suffix: Vec<FrozenModelBinding>,
    request_fingerprint: Sha256Digest,
    prompt: String,
    request: DelegateRequestPayload,
) -> Result<(), JournalError> {
    if state.entries.contains_key(&reservation.invocation_id)
        || !valid_request(&request)
        || request
            .resume_session_id
            .is_some_and(|session_id| session_id != reservation.child_session_id)
        || prompt.is_empty()
        || request.prompt != prompt
        || selected_suffix.is_empty()
        || selected_suffix
            .iter()
            .any(|binding| binding.manifest_revision != revisions.manifest_revision)
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
    let invocation_id = reservation.invocation_id;
    state.entries.insert(
        invocation_id,
        JournalEntry {
            reservation,
            child_agent,
            revisions,
            selected_suffix,
            request_fingerprint,
            prompt,
            request,
            linked: false,
            child_run_id: None,
            run_attached: false,
            terminal_status: None,
        },
    );
    state.order.push(invocation_id);
    Ok(())
}

fn valid_request(request: &DelegateRequestPayload) -> bool {
    if request.description.is_empty() || request.prompt.is_empty() {
        return false;
    }
    if request.resume_session_id.is_some() && request.inherit_context {
        return false;
    }
    if !request.inherit_context && !request.seeded_context.is_empty() {
        return false;
    }
    if request.seeded_context.len() > 65_536 {
        return false;
    }
    if request
        .seeded_context
        .iter()
        .any(|turn| turn.text.is_empty())
    {
        return false;
    }
    request
        .seeded_context
        .iter()
        .map(|turn| turn.text.len())
        .sum::<usize>()
        <= 65_536
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cookie_agent_protocol::{DelegateRequestPayloadV4, InvocationId};

    #[test]
    fn delegate_request_payload_is_strict_and_has_no_serde_defaults() {
        assert!(
            serde_json::from_value::<DelegateRequestPayloadV4>(serde_json::json!({
                "description":"Report",
                "title":"Report",
                "resume_session_id":null,
                "inherit_context":false,
                "seeded_context":[]
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<DelegateRequestPayloadV4>(serde_json::json!({
                "description":"Report",
                "prompt":"report",
                "title":"Report",
                "resume_session_id":null,
                "inherit_context":false,
                "seeded_context":[],
                "background":false,
                "legacy":true
            }))
            .is_err()
        );
    }

    #[test]
    fn journal_replay_accepts_schema_eleven_and_twelve_but_keeps_records_strict() {
        let invocation_id = InvocationId::new_v7();
        for value in [
            serde_json::json!({
                "delegation_journal_schema_version":11,
                "record":{"type":"delegation_linked","invocation_id":invocation_id}
            }),
            serde_json::json!({
                "delegation_journal_schema_version":12,
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
