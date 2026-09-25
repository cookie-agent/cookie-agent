//! Event-log folds that build session projections and summaries.

use super::*;

/// Whether the projection fold consumes this payload. Fold-ignored payloads
/// (the streaming hot path: TextDelta/ReasoningDelta per token,
/// ToolCallProgress per output chunk) only advance the metadata tip and are
/// applied incrementally by `append_with_mode`; consumed payloads trigger a
/// full rebuild.
///
/// Direction is deliberate: consumed variants are an explicit whitelist so a
/// future `EventPayload` variant defaults to rebuild (safe-slow), never to
/// incremental (wrong). Keep this in sync with the fold body below.
pub(super) fn fold_consumed(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::SessionCreated { .. }
            | EventPayload::SessionReverted { .. }
            | EventPayload::SessionPermissionOverlaySet { .. }
            | EventPayload::SessionTitleCommitted { .. }
            | EventPayload::DelegateChildTerminated { .. }
            | EventPayload::RunStarted { .. }
            | EventPayload::UserInputSubmitted { .. }
            | EventPayload::RunCompleted { .. }
            | EventPayload::RunFailed { .. }
            | EventPayload::RunCancelled { .. }
            | EventPayload::RunInterrupted { .. }
            | EventPayload::ToolCallStarted { .. }
            | EventPayload::ToolCallTerminated { .. }
            | EventPayload::ModelTurnCommitted { .. }
            | EventPayload::ModelUsageRecorded { .. }
            | EventPayload::InternalAgentUsageRecorded { .. }
    )
}

#[cfg(test)]
pub(super) fn projection_fold_count() -> u64 {
    PROJECTION_FOLDS.with(std::cell::Cell::get)
}

/// Asserts that two projections of the same log are field-identical. The log
/// itself is compared by identity/tip rather than by folding the events.
#[cfg(test)]
pub(super) fn assert_projection_equivalent(
    actual: &SessionProjection,
    expected: &SessionProjection,
) {
    assert_eq!(actual.meta, expected.meta, "meta");
    assert_eq!(
        actual.creation_agent, expected.creation_agent,
        "creation_agent"
    );
    assert_eq!(actual.status, expected.status, "status");
    assert_eq!(actual.usage, expected.usage, "usage");
    assert_eq!(actual.usage_rollup, expected.usage_rollup, "usage_rollup");
    assert_eq!(actual.runs, expected.runs, "runs");
    assert_eq!(
        actual.rename_records, expected.rename_records,
        "rename_records"
    );
    assert_eq!(
        actual.permission_overlay, expected.permission_overlay,
        "permission_overlay"
    );
    let logs_match = Arc::ptr_eq(&actual.log, &expected.log)
        || (actual.log.physical_tip_seq() == expected.log.physical_tip_seq()
            && actual.log.event_snapshot().len() == expected.log.event_snapshot().len());
    assert!(logs_match, "log tip/length");
}

pub(crate) fn projection(log: Arc<EventLog>) -> Result<SessionProjection, SessionError> {
    #[cfg(test)]
    PROJECTION_FOLDS.with(|count| count.set(count.get() + 1));
    projection_fold(log)
}

pub(super) fn projection_fold(log: Arc<EventLog>) -> Result<SessionProjection, SessionError> {
    let events = log.event_snapshot();
    let physical_tip = log.last_event().expect("creation checked by EventLog");
    let (
        origin,
        short_id,
        cwd_identity,
        creation_selection,
        creation_agent,
        runtime_revision,
        catalog_revision,
        provider_state_revision,
        model_revision,
        agent_revision,
        recipe_registry_revision,
        manifest_revision,
    ) = match &events
        .first()
        .expect("creation checked by EventLog")
        .payload
    {
        EventPayload::SessionCreated {
            origin,
            short_id,
            cwd_identity,
            creation_selection,
            creation_agent,
            runtime_revision,
            catalog_revision,
            provider_state_revision,
            model_revision,
            agent_revision,
            recipe_registry_revision,
            manifest_revision,
        } => (
            origin.clone(),
            short_id.clone(),
            cwd_identity.clone(),
            creation_selection.clone(),
            creation_agent.as_ref().clone(),
            runtime_revision.clone(),
            catalog_revision.clone(),
            provider_state_revision.clone(),
            model_revision.clone(),
            agent_revision.clone(),
            recipe_registry_revision.clone(),
            manifest_revision.clone(),
        ),
        _ => unreachable!(),
    };
    let mut meta = SessionMeta {
        session_id: events[0].session_id,
        origin,
        short_id,
        cwd_identity,
        creation_selection,
        runtime_revision,
        catalog_revision,
        provider_state_revision,
        model_revision,
        agent_revision,
        recipe_registry_revision,
        manifest_revision,
        title: None,
        title_updated_seq: 0,
        last_event_seq: log.physical_tip_seq(),
        last_activity: physical_tip.timestamp,
        status: SessionStatus::Idle,
        skipped_events: log
            .diagnostics()
            .iter()
            .filter(|diagnostic| diagnostic.skipped)
            .map(|diagnostic| cookie_agent_protocol::SkippedEvent {
                seq: diagnostic.seq,
                reason: diagnostic.reason.clone(),
            })
            .collect(),
    };
    let mut runs = HashMap::<RunId, RunProjection>::new();
    let mut status = SessionStatus::Idle;
    let mut usage = None;
    let mut usage_rollup = UsageRollup::default();
    let mut rename_records = HashMap::new();
    let mut permission_overlay = SessionPermissionOverlay::default();
    let mut automatic_title = None;
    let mut delegated_title = None;
    let mut user_title: Option<Option<cookie_agent_protocol::SessionTitle>> = None;
    let recorded_usage_turns = events
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::ModelUsageRecorded { model_turn_seq, .. } => Some(model_turn_seq),
            _ => None,
        })
        .collect::<HashSet<_>>();
    for envelope in events.iter() {
        if let EventPayload::SessionPermissionOverlaySet { overlay } = &envelope.payload {
            permission_overlay = overlay.clone();
        }
        if let EventPayload::SessionTitleCommitted { change, .. } = &envelope.payload {
            match change {
                SessionTitleChange::UserSet { title, .. } => {
                    user_title = Some(Some(title.clone()));
                }
                SessionTitleChange::UserClear { .. } => user_title = Some(None),
                SessionTitleChange::UserReset { .. } => user_title = None,
                SessionTitleChange::DelegatedSet { title, .. } => {
                    delegated_title = Some(title.clone());
                }
                SessionTitleChange::InternalAgentSet { title, .. }
                | SessionTitleChange::FallbackSet { title } => {
                    automatic_title = Some(title.clone());
                }
            }
            meta.title = user_title
                .clone()
                .unwrap_or_else(|| delegated_title.clone().or_else(|| automatic_title.clone()));
            meta.title_updated_seq = envelope.seq;
            if let Some(record) = change.user_rename_record() {
                rename_records.insert(record.client_rename_id.clone(), record);
            }
        }
        if let EventPayload::DelegateChildTerminated {
            status: terminal, ..
        } = &envelope.payload
        {
            status = *terminal;
            continue;
        }
        if matches!(envelope.payload, EventPayload::SessionReverted { .. }) {
            status = SessionStatus::Idle;
            for run in runs.values_mut() {
                if run.status == SessionStatus::Running {
                    run.status = SessionStatus::Interrupted;
                    run.pending_calls.clear();
                }
            }
            continue;
        }
        let Some(run_id) = envelope.run_id else {
            continue;
        };
        match &envelope.payload {
            EventPayload::RunStarted {
                client_run_id,
                selection,
                agent,
                ..
            } => {
                status = SessionStatus::Running;
                runs.insert(
                    run_id,
                    RunProjection {
                        id: run_id,
                        client_run_id: client_run_id.clone(),
                        input: String::new(),
                        selection: selection.clone(),
                        agent: agent.as_ref().clone(),
                        status: SessionStatus::Running,
                        final_text: None,
                        pending_calls: HashMap::new(),
                    },
                );
            }
            // User input is prompt history, not a lifecycle transition.
            EventPayload::UserInputSubmitted { input } => {
                if let Some(run) = runs.get_mut(&run_id)
                    && run.input.is_empty()
                {
                    run.input = input.clone();
                }
            }
            EventPayload::UserInputApplied { .. } => {}
            EventPayload::RunCompleted { final_text } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Completed;
                    run.final_text = final_text.clone();
                    status = SessionStatus::Completed;
                }
            }
            EventPayload::RunFailed { .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Failed;
                    status = SessionStatus::Failed;
                }
            }
            EventPayload::RunCancelled { .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Cancelled;
                    status = SessionStatus::Cancelled;
                }
            }
            EventPayload::RunInterrupted { .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Interrupted;
                    status = SessionStatus::Interrupted;
                }
            }
            EventPayload::ToolCallStarted { start } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    let tool = turns_tool_name(&events, &start.owner).unwrap_or_default();
                    run.pending_calls.insert(start.tool_call_id, tool);
                }
            }
            EventPayload::ToolCallTerminated { termination } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.pending_calls.remove(&termination.tool_call_id);
                }
            }
            EventPayload::ModelTurnCommitted {
                model_turn_seq,
                resolved_model,
                turn,
                ..
            } => {
                let reported = &turn.usage;
                let total = usage.get_or_insert_with(Usage::default);
                add_usage(&mut total.input_tokens, reported.input_tokens);
                add_usage(
                    &mut total.input_tokens_no_cache,
                    reported.input_tokens_no_cache,
                );
                add_usage(
                    &mut total.input_tokens_cache_read,
                    reported.input_tokens_cache_read,
                );
                add_usage(
                    &mut total.input_tokens_cache_write,
                    reported.input_tokens_cache_write,
                );
                add_usage(&mut total.output_tokens, reported.output_tokens);
                add_usage(&mut total.output_tokens_text, reported.output_tokens_text);
                add_usage(
                    &mut total.output_tokens_reasoning,
                    reported.output_tokens_reasoning,
                );
                // An interrupted turn without observed tokens is not a
                // request the provider finished; every other turn counts.
                let interrupted_without_usage = turn.finish_reason
                    == cookie_agent_protocol::ModelFinishReason::Aborted
                    && !crate::usage::has_observed_tokens(reported);
                if !recorded_usage_turns.contains(model_turn_seq) && !interrupted_without_usage {
                    crate::usage::record_stamped(&mut usage_rollup, resolved_model, reported, None);
                }
            }
            EventPayload::ModelUsageRecorded {
                resolved_model,
                usage: reported,
                estimated_cost_pico_usd,
                ..
            }
            | EventPayload::InternalAgentUsageRecorded {
                resolved_model,
                usage: reported,
                estimated_cost_pico_usd,
                ..
            } => {
                crate::usage::record_stamped(
                    &mut usage_rollup,
                    resolved_model,
                    reported,
                    *estimated_cost_pico_usd,
                );
            }
            _ => {}
        }
    }
    meta.status = status;
    Ok(SessionProjection {
        meta,
        creation_agent,
        status,
        usage,
        usage_rollup,
        runs,
        rename_records,
        permission_overlay,
        log,
    })
}

/// Only restart-stable grants are folded into the approval store (§4.3): a grant
/// whose binding cannot survive a restart must not outlive the process that
/// earned it.
pub(crate) fn restart_stable_grant(grant: &cookie_agent_protocol::TreeApprovalGrant) -> bool {
    !grant.resources.is_empty()
        && grant.resources.iter().all(|resource| {
            resource.binding_lifetime
                == cookie_agent_protocol::PreparedBindingLifetime::RestartStable
        })
}

/// Whether a run status ends a run.
pub(super) fn is_terminal_status(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Completed
            | SessionStatus::Failed
            | SessionStatus::Interrupted
            | SessionStatus::Cancelled
    )
}

pub(super) fn terminal_run_of(
    run: Option<RunId>,
    payload: &EventPayload,
) -> Option<(RunId, SessionStatus)> {
    let status = match payload {
        EventPayload::RunCompleted { .. } => SessionStatus::Completed,
        EventPayload::RunFailed { .. } => SessionStatus::Failed,
        EventPayload::RunCancelled { .. } => SessionStatus::Cancelled,
        EventPayload::RunInterrupted { .. } => SessionStatus::Interrupted,
        _ => return None,
    };
    Some((run?, status))
}

pub(super) fn summary_from_projection(session: &SessionProjection) -> SessionSummary {
    SessionSummary {
        meta: session.meta.clone(),
        usage: session.usage.clone(),
        usage_rollup: session.usage_rollup.clone(),
    }
}

pub(super) fn fork_title(title: Option<&SessionTitle>) -> Result<SessionTitle, SessionError> {
    const SUFFIX: &str = " (fork)";
    let base = title.map_or("Untitled", SessionTitle::as_str);
    let max_base = SessionTitle::MAX_BYTES.saturating_sub(SUFFIX.len());
    let mut boundary = base.len().min(max_base);
    while !base.is_char_boundary(boundary) {
        boundary -= 1;
    }
    SessionTitle::new(format!("{}{SUFFIX}", &base[..boundary]))
        .map_err(|error| SessionError::InvalidForkTitle(error.to_string()))
}

pub(super) fn add_usage(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or_default().saturating_add(value));
    }
}

pub(super) fn turns_tool_name(
    events: &[cookie_agent_protocol::StoredEvent],
    owner: &cookie_agent_protocol::AssistantToolCallRef,
) -> Option<String> {
    events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::ModelTurnCommitted {
            model_turn_seq,
            turn,
            ..
        } if *model_turn_seq == owner.model_turn_seq => {
            match turn.content.get(owner.content_index as usize) {
                Some(cookie_agent_protocol::PersistedAssistantPart::ToolCall { name, .. }) => {
                    Some(name.as_str().to_owned())
                }
                _ => None,
            }
        }
        _ => None,
    })
}

#[cfg(test)]
thread_local! {
    pub(super) static PROJECTION_FOLDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
