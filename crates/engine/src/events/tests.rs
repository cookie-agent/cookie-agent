use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

use cookie_agent_protocol::{
    AgentId, AgentMode, AgentRevision, ApprovalReasonCode, ApprovalTrigger, ArtifactReference,
    AssistantToolCallRef, AttemptId, CatalogRevision, ClientRunId, CwdIdentity,
    DelegateRequestPayload, DelegationReservation, EventPayload, FrozenModelBinding,
    InternalAgentBackend, InternalAgentFailure, InternalAgentInvocationId, InternalAgentKind,
    InternalAgentRunId, InvocationId, ModelCallId, ModelErrorKind, ModelErrorStage,
    ModelErrorSummary, ModelFinishReason, ModelKey, ModelRevision, OutputStream, PermissionAction,
    PermissionEffect, PersistedAssistantPart, PersistedModelTurn, ProviderStateRevision,
    RecipeRegistryRevision, RunId, RunSelection, RuntimeRevision, SafeCode, SafeDisplayText,
    SafeErrorMessage, SafeInternalAgentCall, SafeToolError, SessionId, SessionOrigin,
    SessionStatus, SessionTitle, Sha256Digest, StoredEvent, ToolCallId, ToolCallPresentation,
    ToolCallStart, ToolCallTermination, ToolTerminationOutcome, Usage, VariantId,
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
    RecipeRegistryRevision::new(format!("sha256:{}", "6".repeat(64))).expect("registry revision")
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
            short_id: None,
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
        response_body: None,
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
        EventPayload::AttemptAbandoned {
            attempt_id,
            model_error: None,
        },
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
    let mut provider_item_owners = HashMap::<(RunId, ProviderItemId), AssistantToolCallRef>::new();
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
                | EventPayload::AttemptAbandoned { attempt_id, .. }
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
                        committed: false,
                        abandoned: false,
                    },
                );
            }
            EventPayload::ModelRequestPrepared { attempt_id, .. }
            | EventPayload::TextDelta { attempt_id, .. }
            | EventPayload::ReasoningDelta { attempt_id, .. } => {
                validate_attempt_owner(path, &attempts, *attempt_id, record.run_id)?;
            }
            EventPayload::AttemptAbandoned { attempt_id, .. } => {
                let run_id =
                    validate_abandoned_attempt_owner(path, &attempts, *attempt_id, record.run_id)?;
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
                if *model_turn_seq != next_model_turn_seq && (strict || !turn_ordering_tainted) {
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
                validate_approval_owner(path, &approval_owners, grant.approval_id, record.run_id)?;
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
            EventPayload::ModelUsageRecorded { usage, .. } => super::usage_total(event.seq, usage),
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

    // An interrupted attempt commits its partial turn — which closes the attempt
    // — and only then records the abandonment. The reference validator must
    // accept the same relaxation the incremental one does.
    let mut commit_then_abandon = valid.clone();
    let prompt_fingerprint = match &valid[1].payload {
        EventPayload::RunStarted { agent, .. } => agent.prompt_fingerprint.clone(),
        _ => unreachable!("run started at index 1"),
    };
    let interrupted_attempt = AttemptId(Uuid::from_u128(901));
    let interrupted_model = wire_resolved(&fallback_binding("fallback-two"));
    push_run_event(
        &mut commit_then_abandon,
        session_id,
        run_id,
        EventPayload::ModelAttemptStarted {
            attempt_id: interrupted_attempt,
            attempt_ordinal: 6,
            fallback_index: 2,
            retry_ordinal: 1,
            resolved_model: interrupted_model.clone(),
            prompt_fingerprint,
        },
    );
    push_run_event(
        &mut commit_then_abandon,
        session_id,
        run_id,
        EventPayload::ModelTurnCommitted {
            attempt_id: interrupted_attempt,
            model_turn_seq: 1,
            resolved_model: interrupted_model,
            input_through_seq: 1,
            turn: PersistedModelTurn {
                content: vec![PersistedAssistantPart::Text {
                    text: "partial answer".into(),
                    metadata: None,
                }],
                provider_options: BTreeMap::new(),
                finish_reason: ModelFinishReason::Aborted,
                usage: Usage::default(),
                response_metadata: BTreeMap::new(),
                provider_metadata: BTreeMap::new(),
                native_replay: None,
            },
            warnings: Vec::new(),
        },
    );
    push_run_event(
        &mut commit_then_abandon,
        session_id,
        run_id,
        EventPayload::AttemptAbandoned {
            attempt_id: interrupted_attempt,
            model_error: None,
        },
    );

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
            "commit_then_abandon",
            commit_then_abandon,
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
        let cache_strategies = vec![None; selected_suffix.len()];
        let request_fingerprint = crate::delegation_events::delegation_request_fingerprint(
            &child_agent,
            &selected_suffix,
            &cache_strategies,
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
                cache_strategies,
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
fn shared_jsonl_read_ignores_a_torn_tail_without_truncating() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("events.jsonl");
    let contents = b"{\"ok\":true}\n{\"partial\"";
    fs::write(&path, contents).expect("write torn log");

    let records = load_jsonl_shared::<Value>(&path).expect("read shared log");

    assert_eq!(records, vec![serde_json::json!({"ok": true})]);
    assert_eq!(fs::read(&path).expect("read unchanged log"), contents);
}

#[test]
fn read_only_event_log_rejects_appends() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("events.jsonl");
    let records = current_delegation_records();
    let bytes = records
        .iter()
        .flat_map(|record| {
            let mut line = serde_json::to_vec(record).expect("serialize record");
            line.push(b'\n');
            line
        })
        .collect::<Vec<_>>();
    fs::write(&path, bytes).expect("write event log");
    let log = EventLog::open_read_only(path, records[0].session_id).expect("open snapshot");

    let error = log
        .append(
            None,
            EventOrigin::new("engine:test").expect("origin"),
            EventPayload::SessionReverted { through_seq: 1 },
        )
        .expect_err("read-only append must fail");

    assert!(matches!(error, EventLogError::ReadOnly(_)));
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
            display: None,
        }
    ));
    assert!(event_requires_durable_barrier(
        &EventPayload::UserInputAdmitted {
            input: "steer".into(),
        }
    ));
    assert!(event_requires_durable_barrier(
        &EventPayload::ProducerMessagesClaimed {
            message_ids: vec![cookie_agent_protocol::ProducerMessageId::new_v7()],
        }
    ));
    assert!(event_requires_durable_barrier(
        &EventPayload::ProducerMessagesReleased { claim_seq: 2 }
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
                EventPayload::AttemptAbandoned {
                    attempt_id,
                    model_error: None,
                },
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
        Some(EventPayload::AttemptAbandoned { attempt_id: durable_attempt, .. })
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
            model_error: None,
            resolved_model: None,
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
        !diagnostic.skipped && diagnostic.seq == 2 && diagnostic.reason.contains("payload.reason")
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
    let artifacts = crate::ArtifactRouter::open_flat(directory.path().join("artifacts"))
        .expect("artifact store");
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
fn protocol_thirteen_project_context_event_is_skipped_without_harming_neighbors() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("events.jsonl");
    let creation = stored_event();
    let session = creation.session_id;
    let old_run = RunId(Uuid::from_u128(13));
    let old_record = serde_json::json!({
        "engine_version": "0.2.0",
        "origin": "engine:project-context",
        "session_id": session,
        "run_id": old_run,
        "seq": 2,
        "timestamp": jiff::Timestamp::new(2, 0).unwrap(),
        "payload": {
            "type": "project_context_loaded",
            "entries": [{
                "source": "AGENTS.md",
                "content": "legacy context",
                "truncated": false,
                "original_bytes": 14
            }]
        }
    });
    let later = event(
        session,
        None,
        3,
        EventPayload::UserInputAdmitted {
            input: "still readable".into(),
        },
    );
    write_event_values(
        &path,
        &[
            serde_json::to_value(creation).unwrap(),
            old_record,
            serde_json::to_value(later).unwrap(),
        ],
    );

    let log = EventLog::open(path, session).expect("open around protocol 13 event");
    assert_eq!(
        log.all_events()
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    assert!(log.diagnostics().iter().any(|diagnostic| {
        diagnostic.seq == 2 && diagnostic.skipped && !diagnostic.reason.is_empty()
    }));
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
        agent_id: AgentId::new(cookie_agent_config::BUILT_IN_APPROVAL_AGENT_ID).expect("agent id"),
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
        serde_json::to_value(event(session, Some(run), 4, usage(to_model))).expect("forged usage"),
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
                output: Default::default(),
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
fn event_log_accepts_abandoned_attempt_after_its_committed_partial_turn() {
    let base = attribution_records();
    let session = base[0].session_id;
    let run = base[1].run_id.expect("run id");
    let resolved_model = wire_resolved(&fallback_binding("fallback-zero"));
    let attempt_id = AttemptId(Uuid::from_u128(900));
    let prompt_fingerprint = match &base[1].payload {
        EventPayload::RunStarted { agent, .. } => agent.prompt_fingerprint.clone(),
        _ => unreachable!(),
    };
    let mut records = vec![base[0].clone(), base[1].clone()];
    push_run_event(
        &mut records,
        session,
        run,
        EventPayload::ModelAttemptStarted {
            attempt_id,
            attempt_ordinal: 1,
            fallback_index: 0,
            retry_ordinal: 0,
            resolved_model: resolved_model.clone(),
            prompt_fingerprint,
        },
    );
    // An interrupted attempt commits its partial turn — which closes the
    // attempt — and only then records the abandonment.
    push_run_event(
        &mut records,
        session,
        run,
        EventPayload::ModelTurnCommitted {
            attempt_id,
            model_turn_seq: 1,
            resolved_model,
            input_through_seq: 1,
            turn: PersistedModelTurn {
                content: vec![PersistedAssistantPart::Text {
                    text: "partial answer".into(),
                    metadata: None,
                }],
                provider_options: BTreeMap::new(),
                finish_reason: ModelFinishReason::Aborted,
                usage: Usage::default(),
                response_metadata: BTreeMap::new(),
                provider_metadata: BTreeMap::new(),
                native_replay: None,
            },
            warnings: Vec::new(),
        },
    );
    push_run_event(
        &mut records,
        session,
        run,
        EventPayload::AttemptAbandoned {
            attempt_id,
            model_error: None,
        },
    );
    assert_log_open(&records, true, "committed partial turned abandoned");

    // The relaxation is not an amnesty: a second terminal abandonment of the
    // same attempt stays corrupt.
    let mut repeated = records.clone();
    push_run_event(
        &mut repeated,
        session,
        run,
        EventPayload::AttemptAbandoned {
            attempt_id,
            model_error: None,
        },
    );
    assert_log_open(&repeated, false, "second abandonment after a commit");
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
    let EventPayload::ModelAttemptStarted { fallback_index, .. } = &mut forged[2].payload else {
        unreachable!()
    };
    *fallback_index = 1;
    assert_log_open(&forged, false, "wrong fallback index");

    let mut forged = records.clone();
    let EventPayload::ModelAttemptStarted { resolved_model, .. } = &mut forged[2].payload else {
        unreachable!()
    };
    *resolved_model = suffix[1].clone();
    assert_log_open(&forged, false, "wrong frozen model");

    let mut forged = records.clone();
    let EventPayload::ModelAttemptStarted { resolved_model, .. } = &mut forged[2].payload else {
        unreachable!()
    };
    resolved_model.selection.variant = Some(VariantId::new("fast").expect("variant id"));
    assert_log_open(&forged, false, "wrong frozen variant");

    let mut forged = records.clone();
    let EventPayload::ModelAttemptStarted { resolved_model, .. } = &mut forged[2].payload else {
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
