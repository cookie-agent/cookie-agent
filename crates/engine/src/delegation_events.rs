//! Durable delegation state projected from parent-session events.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{
    AgentRevision, AgentSnapshot, CatalogRevision, DelegateRequestPayload, DelegationReservation,
    EventPayload, FrozenModelBinding, InvocationId, ModelRevision, ModelSnapshotRevision,
    ProviderStateRevision, RecipeRegistryRevision, RunId, RuntimeRevision, SessionId,
    SessionStatus, Sha256Digest, ToolCallId,
};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::session::{SessionError, SessionStore};

pub(crate) fn delegation_request_fingerprint(
    child_agent: &AgentSnapshot,
    selected_suffix: &[FrozenModelBinding],
    request: &DelegateRequestPayload,
) -> Result<Sha256Digest, ()> {
    let payload = serde_json::to_vec(&(
        &child_agent.agent,
        &request.description,
        &request.prompt,
        &request.title,
        &request.resume_session_id,
        request.inherit_context,
        request.background,
        &request.seeded_context,
        child_agent,
        selected_suffix,
        &request.staged_skill,
    ))
    .map_err(|_| ())?;
    Sha256Digest::new(format!("{:x}", Sha256::digest(payload))).map_err(|_| ())
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
pub struct DelegationEntry {
    pub reservation: DelegationReservation,
    pub child_agent: AgentSnapshot,
    pub revisions: DelegationRuntimeRevisions,
    pub selected_suffix: Vec<FrozenModelBinding>,
    pub request_fingerprint: Sha256Digest,
    pub request: DelegateRequestPayload,
    pub started: bool,
    pub child_run_id: Option<RunId>,
    pub run_attached: bool,
    pub terminal_status: Option<SessionStatus>,
    pub terminal_reason: Option<cookie_agent_protocol::SafeErrorMessage>,
}

#[derive(Debug, Error)]
pub enum DelegationEventError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("delegation event corruption for invocation {0}")]
    Corrupt(InvocationId),
}

#[derive(Debug, Default)]
struct DelegationState {
    entries: HashMap<InvocationId, DelegationEntry>,
    order: Vec<InvocationId>,
}

#[derive(Debug)]
pub struct DelegationEventStore {
    sessions: Arc<SessionStore>,
    state: Mutex<DelegationState>,
}

impl DelegationEventStore {
    pub fn open(sessions: Arc<SessionStore>) -> Result<Arc<Self>, DelegationEventError> {
        let mut state = DelegationState::default();
        let mut parents = sessions.all();
        parents.sort_by_key(|session| session.meta.session_id);
        for parent in parents {
            for envelope in parent.log.events() {
                if parent.log.delegation_event_tainted(&envelope) {
                    continue;
                }
                apply_event(
                    &mut state,
                    parent.meta.session_id,
                    envelope.run_id,
                    envelope.payload,
                )?;
            }
        }
        Ok(Arc::new(Self {
            sessions,
            state: Mutex::new(state),
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
    ) -> Result<DelegationEntry, DelegationEventError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
                Err(DelegationEventError::Corrupt(invocation_id))
            };
        }
        if !valid_request(&request)
            || selected_suffix.is_empty()
            || selected_suffix
                .iter()
                .any(|binding| binding.manifest_revision != revisions.manifest_revision)
            || child_agent
                .validate_selected_suffix(
                    &cookie_agent_protocol::RunSelection {
                        agent: child_agent.agent.clone(),
                        model: selected_suffix[0].selection.clone(),
                        preset: None,
                    },
                    &selected_suffix,
                )
                .is_err()
        {
            return Err(DelegationEventError::Corrupt(invocation_id));
        }
        let child_session_id = request.resume_session_id.unwrap_or_else(SessionId::new_v7);
        let reservation = DelegationReservation {
            invocation_id,
            parent_session_id,
            parent_run_id,
            parent_tool_call_id,
            child_session_id,
        };
        let entry = DelegationEntry {
            reservation: reservation.clone(),
            child_agent: child_agent.clone(),
            revisions: revisions.clone(),
            selected_suffix: selected_suffix.clone(),
            request_fingerprint: request_fingerprint.clone(),
            request: request.clone(),
            started: false,
            child_run_id: None,
            run_attached: false,
            terminal_status: None,
            terminal_reason: None,
        };
        self.sessions.append(
            parent_session_id,
            Some(parent_run_id),
            EventPayload::DelegationReserved {
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
                request,
            },
        )?;
        state.entries.insert(invocation_id, entry.clone());
        state.order.push(invocation_id);
        Ok(entry)
    }

    pub fn mark_started(&self, invocation_id: InvocationId) -> Result<(), DelegationEventError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = state.entries.get(&invocation_id) else {
            return Err(DelegationEventError::Corrupt(invocation_id));
        };
        if entry.started {
            return Ok(());
        }
        let reservation = entry.reservation.clone();
        self.sessions.append(
            reservation.parent_session_id,
            Some(reservation.parent_run_id),
            EventPayload::DelegationStarted {
                invocation_id,
                child_session_id: reservation.child_session_id,
            },
        )?;
        state
            .entries
            .get_mut(&invocation_id)
            .expect("checked entry")
            .started = true;
        Ok(())
    }

    pub fn mark_run_started(
        &self,
        invocation_id: InvocationId,
        child_run_id: RunId,
    ) -> Result<(), DelegationEventError> {
        self.mark_run(invocation_id, child_run_id, false)
    }

    pub fn mark_run_attached(
        &self,
        invocation_id: InvocationId,
        child_run_id: RunId,
    ) -> Result<(), DelegationEventError> {
        self.mark_run(invocation_id, child_run_id, true)
    }

    fn mark_run(
        &self,
        invocation_id: InvocationId,
        child_run_id: RunId,
        attached: bool,
    ) -> Result<(), DelegationEventError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = state.entries.get(&invocation_id) else {
            return Err(DelegationEventError::Corrupt(invocation_id));
        };
        if entry.terminal_status.is_some()
            || (attached && entry.request.resume_session_id.is_none())
        {
            return Err(DelegationEventError::Corrupt(invocation_id));
        }
        if entry.child_run_id == Some(child_run_id) && entry.run_attached == attached {
            return Ok(());
        }
        if entry.child_run_id.is_some() {
            return Err(DelegationEventError::Corrupt(invocation_id));
        }
        let reservation = entry.reservation.clone();
        let payload = if attached {
            EventPayload::DelegationRunAttached {
                invocation_id,
                child_run_id,
            }
        } else {
            EventPayload::DelegationRunStarted {
                invocation_id,
                child_run_id,
            }
        };
        self.sessions.append(
            reservation.parent_session_id,
            Some(reservation.parent_run_id),
            payload,
        )?;
        let entry = state
            .entries
            .get_mut(&invocation_id)
            .expect("checked entry");
        entry.child_run_id = Some(child_run_id);
        entry.run_attached = attached;
        Ok(())
    }

    pub fn mark_finished(
        &self,
        invocation_id: InvocationId,
        status: SessionStatus,
    ) -> Result<(), DelegationEventError> {
        self.mark_finished_with_reason(invocation_id, status, None)
    }

    pub fn mark_finished_with_reason(
        &self,
        invocation_id: InvocationId,
        status: SessionStatus,
        reason: Option<cookie_agent_protocol::SafeErrorMessage>,
    ) -> Result<(), DelegationEventError> {
        if !matches!(
            status,
            SessionStatus::Completed
                | SessionStatus::Failed
                | SessionStatus::Cancelled
                | SessionStatus::Interrupted
        ) {
            return Err(DelegationEventError::Corrupt(invocation_id));
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = state.entries.get(&invocation_id) else {
            return Err(DelegationEventError::Corrupt(invocation_id));
        };
        if entry.terminal_status == Some(status) {
            return if entry.terminal_reason == reason {
                Ok(())
            } else {
                Err(DelegationEventError::Corrupt(invocation_id))
            };
        }
        if entry.terminal_status.is_some() {
            return Err(DelegationEventError::Corrupt(invocation_id));
        }
        let reservation = entry.reservation.clone();
        let child_run_id = entry.child_run_id;
        self.sessions.append(
            reservation.parent_session_id,
            Some(reservation.parent_run_id),
            EventPayload::DelegationFinished {
                invocation_id,
                child_session_id: reservation.child_session_id,
                child_run_id,
                status,
                reason: reason.clone(),
            },
        )?;
        let entry = state
            .entries
            .get_mut(&invocation_id)
            .expect("checked entry");
        entry.terminal_status = Some(status);
        entry.terminal_reason = reason;
        Ok(())
    }

    #[must_use]
    pub fn get(&self, invocation_id: InvocationId) -> Option<DelegationEntry> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .get(&invocation_id)
            .cloned()
    }

    #[must_use]
    pub fn entries(&self) -> Vec<DelegationEntry> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .order
            .iter()
            .filter_map(|invocation_id| state.entries.get(invocation_id).cloned())
            .collect()
    }
}

fn apply_event(
    state: &mut DelegationState,
    parent_session_id: SessionId,
    run_id: Option<RunId>,
    payload: EventPayload,
) -> Result<(), DelegationEventError> {
    match payload {
        EventPayload::DelegationReserved {
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
            request,
        } => {
            let invocation_id = reservation.invocation_id;
            let revisions = DelegationRuntimeRevisions {
                manifest_revision,
                runtime_revision,
                catalog_revision,
                provider_state_revision,
                model_revision,
                agent_revision,
                recipe_registry_revision,
            };
            let fingerprint_valid =
                delegation_request_fingerprint(&child_agent, &selected_suffix, &request)
                    .is_ok_and(|computed| computed == request_fingerprint);
            if parent_session_id != reservation.parent_session_id
                || run_id != Some(reservation.parent_run_id)
                || state.entries.contains_key(&invocation_id)
                || request
                    .resume_session_id
                    .is_some_and(|id| id != reservation.child_session_id)
                || !valid_request(&request)
                || !fingerprint_valid
                || selected_suffix.is_empty()
                || selected_suffix
                    .iter()
                    .any(|binding| binding.manifest_revision != revisions.manifest_revision)
                || child_agent
                    .validate_selected_suffix(
                        &cookie_agent_protocol::RunSelection {
                            agent: child_agent.agent.clone(),
                            model: selected_suffix[0].selection.clone(),
                            preset: None,
                        },
                        &selected_suffix,
                    )
                    .is_err()
            {
                return Err(DelegationEventError::Corrupt(invocation_id));
            }
            state.entries.insert(
                invocation_id,
                DelegationEntry {
                    reservation,
                    child_agent: *child_agent,
                    revisions,
                    selected_suffix,
                    request_fingerprint,
                    request,
                    started: false,
                    child_run_id: None,
                    run_attached: false,
                    terminal_status: None,
                    terminal_reason: None,
                },
            );
            state.order.push(invocation_id);
        }
        EventPayload::DelegationStarted {
            invocation_id,
            child_session_id,
        } => {
            let Some(entry) = state.entries.get_mut(&invocation_id) else {
                return Ok(());
            };
            if parent_session_id != entry.reservation.parent_session_id
                || run_id != Some(entry.reservation.parent_run_id)
                || child_session_id != entry.reservation.child_session_id
                || entry.started
            {
                return Err(DelegationEventError::Corrupt(invocation_id));
            }
            entry.started = true;
        }
        payload @ (EventPayload::DelegationRunStarted { .. }
        | EventPayload::DelegationRunAttached { .. }) => {
            let (invocation_id, child_run_id, attached) = match payload {
                EventPayload::DelegationRunStarted {
                    invocation_id,
                    child_run_id,
                } => (invocation_id, child_run_id, false),
                EventPayload::DelegationRunAttached {
                    invocation_id,
                    child_run_id,
                } => (invocation_id, child_run_id, true),
                _ => unreachable!(),
            };
            let Some(entry) = state.entries.get_mut(&invocation_id) else {
                return Ok(());
            };
            if parent_session_id != entry.reservation.parent_session_id
                || run_id != Some(entry.reservation.parent_run_id)
                || entry.child_run_id.is_some()
                || entry.terminal_status.is_some()
                || (attached && entry.request.resume_session_id.is_none())
            {
                return Err(DelegationEventError::Corrupt(invocation_id));
            }
            entry.child_run_id = Some(child_run_id);
            entry.run_attached = attached;
        }
        EventPayload::DelegationFinished {
            invocation_id,
            child_session_id,
            child_run_id,
            status,
            reason,
        } => {
            let Some(entry) = state.entries.get_mut(&invocation_id) else {
                return Ok(());
            };
            if parent_session_id != entry.reservation.parent_session_id
                || run_id != Some(entry.reservation.parent_run_id)
                || child_session_id != entry.reservation.child_session_id
                || child_run_id != entry.child_run_id
                || entry.terminal_status.is_some()
                || !matches!(
                    status,
                    SessionStatus::Completed
                        | SessionStatus::Failed
                        | SessionStatus::Cancelled
                        | SessionStatus::Interrupted
                )
            {
                return Err(DelegationEventError::Corrupt(invocation_id));
            }
            entry.terminal_status = Some(status);
            entry.terminal_reason = reason;
        }
        _ => {}
    }
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
    if request.staged_skill.as_ref().is_some_and(|skill| {
        skill.provenance != cookie_agent_protocol::StagedSkillProvenance::SkillFork
            || skill.name.is_empty()
            || skill.rendered_body.is_empty()
            || skill.source_path.is_empty()
            || skill.base_dir.is_empty()
            || skill.supporting_files.len() > 10
            || skill.grants.len() > 256
            || request.resume_session_id.is_some()
    }) {
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
