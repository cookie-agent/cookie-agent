//! Durable delegation state projected from parent-session events.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{
    AgentSnapshot, DelegateRequestPayload, DelegationReservation, EventPayload,
    FrozenCacheStrategy, FrozenModelBinding, InvocationId, Open, RevisionFold, RunId,
    RuntimeRevisions, SessionId, SessionStatus, Sha256Digest, ToolCallId,
};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::session::{SessionError, SessionStore};

/// The fingerprint of a delegation request; `None` when it does not
/// serialize.
pub fn delegation_request_fingerprint(
    child_agent: &AgentSnapshot,
    cache_strategies: &[Option<FrozenCacheStrategy>],
    request: &DelegateRequestPayload,
) -> Option<Sha256Digest> {
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
        cache_strategies,
        &request.staged_skill,
    ))
    .ok()?;
    Sha256Digest::new(format!("{:x}", Sha256::digest(payload))).ok()
}

#[derive(Clone, Debug, PartialEq)]
pub struct DelegationEntry {
    pub reservation: DelegationReservation,
    pub child_agent: AgentSnapshot,
    /// The revisions the child was frozen against.
    pub revisions: RuntimeRevisions,
    pub cache_strategies: Vec<Option<FrozenCacheStrategy>>,
    pub request_fingerprint: Sha256Digest,
    pub request: DelegateRequestPayload,
    pub started: bool,
    pub child_run_id: Option<RunId>,
    pub run_attached: bool,
    pub terminal_status: Option<SessionStatus>,
    pub terminal_reason: Option<cookie_agent_protocol::SafeErrorMessage>,
}

/// What a `delegation_finished` record shows of the child's report.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FinishedTeaser {
    /// The child's handle; absent when the child session is gone.
    pub short_id: Option<String>,
    /// The report as `[subagent_output]` previews it.
    pub preview: String,
    pub total_lines: u64,
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
    /// Delegations whose stored records do not verify. Each is left out
    /// alone, so it is not recovered, and its later records are ignored.
    unrecoverable: HashSet<InvocationId>,
}

impl DelegationState {
    /// Folds one stored record. A record that does not verify makes its
    /// delegation unrecoverable instead of failing the fold.
    fn apply_stored(
        &mut self,
        parent_session_id: SessionId,
        run_id: Option<RunId>,
        payload: EventPayload,
    ) {
        let Some(invocation_id) = delegation_invocation_of(&payload) else {
            return;
        };
        if self.unrecoverable.contains(&invocation_id) {
            return;
        }
        if let Err(DelegationEventError::Corrupt(invocation_id)) =
            apply_event(self, parent_session_id, run_id, payload)
        {
            eprintln!(
                "delegation {invocation_id} of session {parent_session_id} cannot be recovered: \
                 its records do not verify"
            );
            self.entries.remove(&invocation_id);
            self.order.retain(|id| *id != invocation_id);
            self.unrecoverable.insert(invocation_id);
        }
    }
}

#[derive(Debug)]
pub struct DelegationEventStore {
    sessions: Arc<SessionStore>,
    state: Mutex<DelegationState>,
}

impl DelegationEventStore {
    /// An empty store. Records never come from a work-dir-wide scan: each
    /// tree's arrive through [`Self::extend_from_payloads`] when that tree is
    /// loaded, from its root log and every descendant log (tree-local C1).
    #[must_use]
    pub fn new(sessions: Arc<SessionStore>) -> Arc<Self> {
        Arc::new(Self {
            sessions,
            state: Mutex::new(DelegationState::default()),
        })
    }

    /// Folds delegation records harvested from child logs (§4.1.2), each as
    /// `harvested` hands it over, returning the entries that changed so the
    /// engine can rebuild only what moved.
    pub fn extend_from_payloads(
        &self,
        payloads: &[(
            SessionId,
            Option<RunId>,
            cookie_agent_protocol::EventPayload,
        )],
    ) -> Result<Vec<DelegationEntry>, DelegationEventError> {
        if payloads.is_empty() {
            return Ok(Vec::new());
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut touched = Vec::new();
        for (parent_session_id, run_id, payload) in payloads {
            let Some(invocation_id) = delegation_invocation_of(payload) else {
                continue;
            };
            let before = state.entries.get(&invocation_id).cloned();
            state.apply_stored(*parent_session_id, *run_id, payload.clone());
            match state.entries.get(&invocation_id).cloned() {
                Some(updated) if Some(&updated) != before.as_ref() => touched.push(updated),
                _ => {}
            }
        }
        Ok(touched)
    }

    /// Records a delegation reservation. `selected_suffix` holds the
    /// complete bindings of `child_agent`'s selected suffix; they are checked
    /// against it and not stored.
    #[allow(clippy::too_many_arguments)]
    pub fn reserve(
        &self,
        invocation_id: InvocationId,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        child_agent: AgentSnapshot,
        revisions: RuntimeRevisions,
        selected_suffix: Vec<FrozenModelBinding>,
        cache_strategies: Vec<Option<FrozenCacheStrategy>>,
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
                && entry.cache_strategies == cache_strategies
                && entry.request == request;
            return if same_request {
                Ok(entry.clone())
            } else {
                Err(DelegationEventError::Corrupt(invocation_id))
            };
        }
        if !valid_request(&request)
            || !request_within_caps(&request)
            || child_agent.validate().is_err()
            || !suffix_matches_snapshot(&child_agent, &selected_suffix)
            || !(cache_strategies_match_bindings(&selected_suffix, &cache_strategies)
                || (selected_suffix.is_empty() && cache_strategies.iter().all(Option::is_none)))
            || selected_suffix
                .iter()
                .any(|binding| binding.manifest_revision != revisions.manifest_revision)
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
            cache_strategies: cache_strategies.clone(),
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
            cookie_agent_protocol::EventOrigin::new("engine:delegation")
                .expect("static event origin is valid"),
            EventPayload::DelegationReserved {
                reservation,
                child_agent: Box::new(child_agent),
                // The log drops them again when they equal the latest it records.
                revisions: Some(revisions),
                cache_strategies,
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
            cookie_agent_protocol::EventOrigin::new("engine:delegation")
                .expect("static event origin is valid"),
            EventPayload::delegation_progressed(
                invocation_id,
                cookie_agent_protocol::DelegationStage::Started {
                    child_session_id: reservation.child_session_id,
                },
            ),
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
        let payload = EventPayload::delegation_progressed(
            invocation_id,
            if attached {
                cookie_agent_protocol::DelegationStage::RunAttached { child_run_id }
            } else {
                cookie_agent_protocol::DelegationStage::RunStarted { child_run_id }
            },
        );
        self.sessions.append(
            reservation.parent_session_id,
            Some(reservation.parent_run_id),
            cookie_agent_protocol::EventOrigin::new("engine:delegation")
                .expect("static event origin is valid"),
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
        teaser: FinishedTeaser,
    ) -> Result<(), DelegationEventError> {
        self.mark_finished_with_reason(invocation_id, status, None, teaser)
    }

    pub fn mark_finished_with_reason(
        &self,
        invocation_id: InvocationId,
        status: SessionStatus,
        reason: Option<cookie_agent_protocol::SafeErrorMessage>,
        teaser: FinishedTeaser,
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
            cookie_agent_protocol::EventOrigin::new("engine:delegation")
                .expect("static event origin is valid"),
            EventPayload::DelegationFinished {
                invocation_id,
                child_session_id: reservation.child_session_id,
                child_run_id,
                status,
                reason: reason.clone(),
                short_id: teaser.short_id,
                preview: teaser.preview,
                total_lines: teaser.total_lines,
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

    /// The delegation that started `child_run_id` of `child_session_id`,
    /// found without copying the others.
    #[must_use]
    pub fn find_child_run(
        &self,
        child_session_id: SessionId,
        child_run_id: RunId,
    ) -> Option<DelegationEntry> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .values()
            .find(|entry| {
                entry.reservation.child_session_id == child_session_id
                    && entry.child_run_id == Some(child_run_id)
            })
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

    /// Drops every entry a deleted session took part in, as parent or child.
    pub fn forget_sessions(&self, deleted: &std::collections::HashSet<SessionId>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.entries.retain(|_, entry| {
            !deleted.contains(&entry.reservation.parent_session_id)
                && !deleted.contains(&entry.reservation.child_session_id)
        });
        let retained = state
            .entries
            .keys()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        state
            .order
            .retain(|invocation_id| retained.contains(invocation_id));
    }

    pub fn reconcile_parent(
        &self,
        parent_session_id: SessionId,
    ) -> Result<(), DelegationEventError> {
        let parent = self.sessions.get(parent_session_id)?;
        let mut rebuilt = DelegationState::default();
        let mut revisions = RevisionFold::default();
        for envelope in parent.log.event_snapshot().iter() {
            revisions.observe(&envelope.payload);
            // Only delegation records fold; the rest are not copied.
            if delegation_invocation_of(&envelope.payload).is_none()
                || parent.log.delegation_event_tainted(envelope)
            {
                continue;
            }
            rebuilt.apply_stored(
                parent_session_id,
                envelope.run_id,
                harvested(&envelope.payload, &revisions),
            );
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .entries
            .retain(|_, entry| entry.reservation.parent_session_id != parent_session_id);
        let retained = state
            .entries
            .keys()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        state
            .order
            .retain(|invocation_id| retained.contains(invocation_id));
        for invocation_id in rebuilt.order {
            if let Some(entry) = rebuilt.entries.remove(&invocation_id) {
                state.entries.insert(invocation_id, entry);
                state.order.push(invocation_id);
            }
        }
        state.unrecoverable.extend(rebuilt.unrecoverable);
        Ok(())
    }
}

/// The invocation a delegation payload belongs to, for cache-diff bookkeeping.
fn delegation_invocation_of(payload: &EventPayload) -> Option<cookie_agent_protocol::InvocationId> {
    match payload {
        EventPayload::DelegationReserved { reservation, .. } => Some(reservation.invocation_id),
        EventPayload::DelegationProgressed { invocation_id, .. }
        | EventPayload::DelegationFinished { invocation_id, .. } => Some(*invocation_id),
        _ => None,
    }
}

/// A delegation payload as `apply_event` consumes it: a reservation that
/// recorded no revisions of its own gets the ones it was frozen against,
/// `revisions` having folded the log through it.
pub fn harvested(payload: &EventPayload, revisions: &RevisionFold) -> EventPayload {
    let mut payload = payload.clone();
    if let EventPayload::DelegationReserved {
        revisions: recorded @ None,
        ..
    } = &mut payload
    {
        *recorded = revisions.latest().cloned();
    }
    payload
}

/// The delegation lifecycle payloads `apply_event` consumes.
///
/// Lazy tree loads hand child-session payloads over in log order and must offer
/// exactly this set: a missing arm would leave an entry half-folded, and the
/// following payload would then be rejected as corrupt.
pub fn is_delegation_payload(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::DelegationReserved { .. }
            | EventPayload::DelegationProgressed { .. }
            | EventPayload::DelegationFinished { .. }
    )
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
            revisions,
            cache_strategies,
            request_fingerprint,
            request,
        } => {
            let invocation_id = reservation.invocation_id;
            // Every reader hands reservations over with their effective
            // revisions ([`harvested`]).
            let Some(revisions) = revisions else {
                return Err(DelegationEventError::Corrupt(invocation_id));
            };
            // The request fingerprint is trusted as written: computing it
            // again would tie reading to how this build serializes requests.
            if parent_session_id != reservation.parent_session_id
                || run_id != Some(reservation.parent_run_id)
                || state.entries.contains_key(&invocation_id)
                || request
                    .resume_session_id
                    .is_some_and(|id| id != reservation.child_session_id)
                || !valid_request(&request)
                || child_agent.validate().is_err()
                || cache_strategies.len() != child_agent.selected_suffix().len()
            {
                return Err(DelegationEventError::Corrupt(invocation_id));
            }
            state.entries.insert(
                invocation_id,
                DelegationEntry {
                    reservation,
                    child_agent: *child_agent,
                    revisions,
                    cache_strategies,
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
        EventPayload::DelegationProgressed {
            invocation_id,
            stage: Open::Known(cookie_agent_protocol::DelegationStage::Started { child_session_id }),
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
        EventPayload::DelegationProgressed {
            invocation_id,
            stage:
                Open::Known(
                    stage @ (cookie_agent_protocol::DelegationStage::RunStarted { .. }
                    | cookie_agent_protocol::DelegationStage::RunAttached { .. }),
                ),
        } => {
            let (child_run_id, attached) = match stage {
                cookie_agent_protocol::DelegationStage::RunStarted { child_run_id } => {
                    (child_run_id, false)
                }
                cookie_agent_protocol::DelegationStage::RunAttached { child_run_id } => {
                    (child_run_id, true)
                }
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
            ..
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

/// Whether a delegation request is well formed, within its schema's bounds.
/// Readers check this of stored reservations too; the writer-only size cap
/// is [`request_within_caps`]'s.
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
    !request
        .seeded_context
        .iter()
        .any(|turn| turn.text.is_empty())
}

/// The size cap a new reservation keeps to, which no schema states: the
/// seeded context's text in total. It bounds what a writer records, so a
/// reader never applies it to stored reservations.
fn request_within_caps(request: &DelegateRequestPayload) -> bool {
    request
        .seeded_context
        .iter()
        .map(|turn| turn.text.len())
        .sum::<usize>()
        <= 65_536
}

/// Whether `bindings` are the complete bindings of the snapshot's selected
/// suffix, in order.
fn suffix_matches_snapshot(snapshot: &AgentSnapshot, bindings: &[FrozenModelBinding]) -> bool {
    let suffix = snapshot.selected_suffix();
    // An ACP agent runs no model: its snapshot holds only its synthetic
    // binding (`docs/site/specs/acp-agents.md` section 6).
    if bindings.is_empty() {
        return matches!(suffix, [binding] if binding.adapter == crate::bindings::ACP_ADAPTER);
    }
    suffix.len() == bindings.len()
        && suffix.iter().zip(bindings).all(|(stored, binding)| {
            stored.selection == binding.selection
                && stored.selection_fingerprint == binding.selection_fingerprint
                && stored.blueprint_fingerprint == binding.blueprint_fingerprint
        })
}

fn cache_strategies_match_bindings(
    bindings: &[FrozenModelBinding],
    strategies: &[Option<FrozenCacheStrategy>],
) -> bool {
    bindings.len() == strategies.len()
        && bindings.iter().zip(strategies).all(|(binding, strategy)| {
            let Some(strategy) = strategy else {
                return true;
            };
            let Some(family) = cookie_agent_models::adapters::wire_adapter_for_protocol(
                binding.descriptor.adapter_id.as_str(),
            ) else {
                return false;
            };
            matches!(
                (family, strategy),
                (
                    cookie_agent_models::adapters::OvenAdapterFamily::Anthropic
                        | cookie_agent_models::adapters::OvenAdapterFamily::AnthropicCompatible,
                    FrozenCacheStrategy::Anthropic { .. }
                ) | (
                    cookie_agent_models::adapters::OvenAdapterFamily::AwsBedrockConverse,
                    FrozenCacheStrategy::Bedrock { .. }
                ) | (
                    cookie_agent_models::adapters::OvenAdapterFamily::GoogleGemini
                        | cookie_agent_models::adapters::OvenAdapterFamily::GoogleVertexGemini,
                    FrozenCacheStrategy::Google { .. }
                ) | (
                    cookie_agent_models::adapters::OvenAdapterFamily::OpenaiChat
                        | cookie_agent_models::adapters::OvenAdapterFamily::OpenaiResponses
                        | cookie_agent_models::adapters::OvenAdapterFamily::OpenaiCompatible
                        | cookie_agent_models::adapters::OvenAdapterFamily::AzureOpenaiChat
                        | cookie_agent_models::adapters::OvenAdapterFamily::AzureOpenaiResponses,
                    FrozenCacheStrategy::OpenAi { .. }
                )
            )
        })
}
