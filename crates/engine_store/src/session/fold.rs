//! Event-log folds that build session projections and summaries.

use super::*;

#[cfg(test)]
pub(super) fn projection_fold_count() -> u64 {
    PROJECTION_FOLDS.with(std::cell::Cell::get)
}

/// Test-only: skips the per-append reference fold for the rest of the
/// process, so a timing test measures the append path rather than the
/// differential check, whichever thread appends.
#[cfg(any(test, feature = "test-support"))]
pub fn skip_reference_fold() {
    SKIP_REFERENCE_FOLD.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn reference_fold_skipped() -> bool {
    SKIP_REFERENCE_FOLD.load(std::sync::atomic::Ordering::Relaxed)
}

/// Asserts that two projections of the same log are field-identical. The log
/// itself is compared by identity/tip rather than by folding the events.
#[cfg(any(test, feature = "test-support"))]
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
    assert_eq!(
        actual.permission_mode, expected.permission_mode,
        "permission_mode"
    );
    assert_eq!(
        actual.runtime_attachments, expected.runtime_attachments,
        "runtime_attachments"
    );
    assert_eq!(
        actual.automatic_title_eligible, expected.automatic_title_eligible,
        "automatic_title_eligible"
    );
    let logs_match = Arc::ptr_eq(&actual.log, &expected.log)
        || (actual.log.physical_tip_seq() == expected.log.physical_tip_seq()
            && actual.log.event_snapshot().len() == expected.log.event_snapshot().len());
    assert!(logs_match, "log tip/length");
}

/// Whether an automatic title may still be generated, folded from the
/// visible `SessionTitleCommitted` records in log order.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct AutomaticTitleEligibility {
    latest_automatic: Option<u64>,
    /// Latest user or delegated change, and whether it was a reset.
    latest_user: Option<(u64, bool)>,
}

impl AutomaticTitleEligibility {
    pub(super) fn observe(&mut self, seq: u64, change: &SessionTitleChange) {
        match change {
            SessionTitleChange::InternalAgentSet { .. }
            | SessionTitleChange::FallbackSet { .. } => {
                self.latest_automatic = Some(seq);
            }
            SessionTitleChange::DelegatedSet { .. }
            | SessionTitleChange::UserSet { .. }
            | SessionTitleChange::UserClear { .. } => self.latest_user = Some((seq, false)),
            SessionTitleChange::UserReset { .. } => self.latest_user = Some((seq, true)),
        }
    }

    #[must_use]
    pub(super) fn eligible(self) -> bool {
        match self.latest_user {
            Some((_, false)) => false,
            Some((reset_seq, true)) => self.latest_automatic.is_none_or(|seq| seq < reset_seq),
            None => self.latest_automatic.is_none(),
        }
    }
}

pub fn projection(log: Arc<EventLog>) -> Result<SessionProjection, SessionError> {
    refold(log, None)
}

/// Folds `log` again, sharing the agent snapshots `previous` already holds
/// for the same log. A `RunStarted` record is immutable and unique per run,
/// so its snapshot never changes between folds.
pub(super) fn refold(
    log: Arc<EventLog>,
    previous: Option<&SessionProjection>,
) -> Result<SessionProjection, SessionError> {
    #[cfg(any(test, feature = "test-support"))]
    PROJECTION_FOLDS.with(|count| count.set(count.get() + 1));
    let previous = previous.filter(|previous| Arc::ptr_eq(&previous.log, &log));
    projection_fold_with(log, previous)
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn projection_fold(log: Arc<EventLog>) -> Result<SessionProjection, SessionError> {
    projection_fold_with(log, None)
}

fn projection_fold_with(
    log: Arc<EventLog>,
    previous: Option<&SessionProjection>,
) -> Result<SessionProjection, SessionError> {
    let events = log.event_snapshot();
    let physical_tip = log.last_event().expect("creation checked by EventLog");
    let (origin, short_id, cwd_identity, creation_selection, creation_agent, revisions, cron) =
        match &events
            .first()
            .expect("creation checked by EventLog")
            .payload
        {
            EventPayload::SessionCreated {
                history_format: _,
                origin,
                short_id,
                cwd_identity,
                creation_selection,
                creation_agent,
                revisions,
                cron,
            } => (
                origin.clone(),
                Some(short_id.clone()),
                cwd_identity.clone(),
                creation_selection.clone(),
                previous.map_or_else(
                    || Arc::new(creation_agent.as_ref().clone()),
                    |previous| previous.creation_agent.clone(),
                ),
                revisions.clone(),
                cron.clone(),
            ),
            _ => unreachable!(),
        };
    let meta = SessionMeta {
        session_id: events[0].session_id,
        origin,
        short_id,
        cwd_identity,
        creation_selection,
        runtime_revision: revisions.runtime_revision,
        catalog_revision: revisions.catalog_revision,
        provider_state_revision: revisions.provider_state_revision,
        model_revision: revisions.model_revision,
        agent_revision: revisions.agent_revision,
        recipe_registry_revision: revisions.recipe_registry_revision,
        manifest_revision: revisions.manifest_revision,
        title: None,
        title_updated_seq: 0,
        last_event_seq: log.physical_tip_seq(),
        last_activity: physical_tip.timestamp,
        last_agent_activity: None,
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
        attachments: Vec::new(),
        project: None,
        waiting_on: Vec::new(),
        delegated_attachments: Vec::new(),
        cron,
        held: None,
        unread: false,
    };
    let mut projection = SessionProjection {
        meta,
        creation_agent,
        status: SessionStatus::Idle,
        usage: None,
        usage_rollup: UsageRollup::default(),
        runs: HashMap::new(),
        rename_records: HashMap::new(),
        permission_overlay: SessionPermissionOverlay::default(),
        permission_mode: None,
        runtime_attachments: cookie_agent_protocol::RuntimeAttachmentFold::default(),
        automatic_title_eligible: AutomaticTitleEligibility::default().eligible(),
        fold: ProjectionFoldState::default(),
        log: log.clone(),
    };
    // A full fold sees every usage record up front, so a turn whose usage is
    // recorded later never counts at its commit.
    let recorded_usage_turns = events
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::ModelUsageRecorded { model_turn_seq, .. } => Some(model_turn_seq),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let step = FoldStep::Full {
        recorded_usage_turns: &recorded_usage_turns,
        previous,
    };
    for envelope in events.iter() {
        let observed = projection.observe(envelope, step);
        debug_assert!(
            matches!(observed, Observed::Applied { .. }),
            "a full fold applies every event"
        );
    }
    Ok(projection)
}

/// How [`SessionProjection::observe`] is being driven.
#[derive(Clone, Copy)]
pub(super) enum FoldStep<'a> {
    /// Folding a whole visible log from its creation record.
    Full {
        /// `model_turn_seq`s that have a `ModelUsageRecorded` anywhere in the
        /// log.
        recorded_usage_turns: &'a HashSet<u64>,
        /// An earlier fold of the same log whose agent snapshots are reused.
        previous: Option<&'a SessionProjection>,
    },
    /// Applying one event just appended to a log the projection already
    /// folds.
    Append,
}

/// What [`SessionProjection::observe`] did with one appended event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Observed {
    /// The event was applied. `durable_meta` reports whether it changed the
    /// metadata beyond its tip (title, status, attachments, project, or an
    /// active goal), which other processes read from the `metadata` cache.
    Applied { durable_meta: bool },
    /// The event cannot be applied in place; the log needs a full fold.
    NeedsRefold,
}

/// Fold state a projection keeps beyond its public fields, so an appended
/// event can be applied in place instead of refolding the log.
#[derive(Clone, Debug, Default)]
pub struct ProjectionFoldState {
    user_title: Option<Option<SessionTitle>>,
    delegated_title: Option<SessionTitle>,
    automatic_title: Option<SessionTitle>,
    title_eligibility: AutomaticTitleEligibility,
    /// Tool name of each committed tool-call part, by
    /// `(model_turn_seq, content_index)`. A later committed turn with the
    /// same sequence replaces the names it carries.
    tool_names: HashMap<(u64, u32), String>,
    /// Committed turns whose usage counted at their commit because no
    /// `ModelUsageRecorded` for them was known, by `model_turn_seq`.
    provisional_usage: HashMap<u64, ProvisionalUsage>,
    /// The session's goal, which `meta.waiting_on` reports while active.
    goals: crate::goal_projection::GoalFold,
}

impl ProjectionFoldState {
    /// Whether the user or a delegation named the session: an automatic
    /// title no longer shows.
    pub fn titled_by_user(&self) -> bool {
        self.user_title.is_some() || self.delegated_title.is_some()
    }
}

/// Where a committed turn's provisionally counted usage sits in the rollup.
#[derive(Clone, Debug)]
struct ProvisionalUsage {
    model: cookie_agent_protocol::ModelKey,
    /// Index of the turn's observation in its model rollup.
    observation: usize,
}

impl SessionProjection {
    /// Folds one visible event into the projection. The metadata tip is the
    /// caller's: a full fold takes it from the log, an append from the event.
    ///
    /// Each step matches what a full fold of the extended log produces. The
    /// one place the two differ is a turn's usage: a full fold knows whether
    /// a `ModelUsageRecorded` for the turn follows, an append does not, so an
    /// append counts the turn at its commit and takes it back out when the
    /// usage record arrives. When that cannot be undone exactly (another
    /// observation for the model followed, or the rollup overflowed), the
    /// step reports [`Observed::NeedsRefold`] before changing anything, so
    /// readers never see a half-applied projection.
    pub(super) fn observe(&mut self, envelope: &StoredEvent, step: FoldStep<'_>) -> Observed {
        let status_before = self.status;
        let mut durable_meta = false;
        match &envelope.payload {
            EventPayload::SessionPermissionOverlaySet { overlay } => {
                self.permission_overlay = overlay.clone();
            }
            EventPayload::PermissionModeSet { mode } => {
                self.permission_mode = Some(*mode);
            }
            EventPayload::SessionProjectSet { project } => {
                self.meta.project = project.clone();
                durable_meta = true;
            }
            EventPayload::AutomaticRunsHeld { reason } => {
                self.meta.held = Some(reason.clone());
                durable_meta = true;
            }
            EventPayload::AutomaticRunsResumed {} => {
                if self.meta.held.take().is_some() {
                    durable_meta = true;
                }
            }
            EventPayload::DelegatedAttachmentsAdded { attachments, .. } => {
                for attachment in attachments {
                    if !self
                        .meta
                        .delegated_attachments
                        .iter()
                        .any(|known| same_place(known, attachment))
                    {
                        self.meta.delegated_attachments.push(attachment.clone());
                    }
                }
                durable_meta = true;
            }
            EventPayload::RuntimeAttached { .. } | EventPayload::RuntimeDetached { .. } => {
                self.runtime_attachments.observe(&envelope.payload);
                self.meta.attachments = self.runtime_attachments.attachments().to_vec();
                durable_meta = true;
            }
            EventPayload::SessionTitleCommitted { change, .. } => {
                let fold = &mut self.fold;
                fold.title_eligibility.observe(envelope.seq, change);
                match change {
                    SessionTitleChange::UserSet { title, .. } => {
                        fold.user_title = Some(Some(title.clone()));
                    }
                    SessionTitleChange::UserClear { .. } => fold.user_title = Some(None),
                    SessionTitleChange::UserReset { .. } => fold.user_title = None,
                    SessionTitleChange::DelegatedSet { title, .. } => {
                        fold.delegated_title = Some(title.clone());
                    }
                    SessionTitleChange::InternalAgentSet { title, .. }
                    | SessionTitleChange::FallbackSet { title } => {
                        fold.automatic_title = Some(title.clone());
                    }
                }
                self.meta.title = fold.user_title.clone().unwrap_or_else(|| {
                    fold.delegated_title
                        .clone()
                        .or_else(|| fold.automatic_title.clone())
                });
                self.meta.title_updated_seq = envelope.seq;
                self.automatic_title_eligible = fold.title_eligibility.eligible();
                if let Some(record) = change.user_rename_record() {
                    self.rename_records
                        .insert(record.client_rename_id.clone(), record);
                }
                durable_meta = true;
            }
            EventPayload::GoalActivated { .. }
            | EventPayload::GoalChecklistRevised { .. }
            | EventPayload::GoalLifecycleChanged { .. } => {
                let was_active = self.fold.goals.is_active();
                // An invalid goal event changes nothing, as the producer
                // projection rejects it.
                let _ = self.fold.goals.observe(envelope);
                let active = self.fold.goals.is_active();
                if active != was_active {
                    // The fold's only kind: the store adds the live ones
                    // where metadata leaves the engine.
                    self.meta.waiting_on = if active {
                        vec![WaitingOn::Goal]
                    } else {
                        Vec::new()
                    };
                    durable_meta = true;
                }
            }
            EventPayload::DelegateChildTerminated {
                status: terminal, ..
            } => {
                self.status = *terminal;
            }
            // An admission (a steer, or input queued for later) may have no
            // run.
            EventPayload::UserInputAdmitted { .. } | EventPayload::UserInputSubmitted { .. } => {
                self.meta.last_agent_activity = Some(envelope.timestamp);
                // The user's submitted input clears a hold.
                if matches!(envelope.payload, EventPayload::UserInputSubmitted { .. })
                    && self.meta.held.take().is_some()
                {
                    durable_meta = true;
                }
            }
            EventPayload::ModelTurnCommitted {
                model_turn_seq,
                turn,
                ..
            } => {
                for (index, part) in turn.content.iter().enumerate() {
                    if let cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                        name, ..
                    } = part
                    {
                        self.fold
                            .tool_names
                            .insert((*model_turn_seq, index as u32), name.as_str().to_owned());
                    }
                }
            }
            _ => {}
        }
        if let Some(run_id) = envelope.run_id
            && !self.observe_run_event(run_id, envelope, step)
        {
            return Observed::NeedsRefold;
        }
        self.meta.status = self.status;
        Observed::Applied {
            durable_meta: durable_meta || self.status != status_before,
        }
    }

    /// Folds an event just appended to this projection's log, tip included,
    /// refolding the log when the event cannot be applied in place.
    pub(super) fn observe_appended(&mut self, event: &StoredEvent) -> Result<(), SessionError> {
        match self.observe(event, FoldStep::Append) {
            Observed::Applied { .. } => {
                self.meta.last_event_seq = event.seq;
                self.meta.last_activity = event.timestamp;
            }
            Observed::NeedsRefold => *self = refold(self.log.clone(), Some(self))?,
        }
        Ok(())
    }

    /// The run-owned part of [`Self::observe`]. `false` when the step needs a
    /// full fold.
    fn observe_run_event(
        &mut self,
        run_id: RunId,
        envelope: &StoredEvent,
        step: FoldStep<'_>,
    ) -> bool {
        match &envelope.payload {
            EventPayload::RunStarted {
                client_run_id,
                selection,
                agent,
                ..
            } => {
                let previous = match step {
                    FoldStep::Full { previous, .. } => previous,
                    FoldStep::Append => None,
                };
                self.status = SessionStatus::Running;
                self.runs.insert(
                    run_id,
                    RunProjection {
                        id: run_id,
                        client_run_id: client_run_id.clone(),
                        input: None,
                        selection: selection.clone(),
                        agent: previous
                            .and_then(|previous| previous.runs.get(&run_id))
                            .map_or_else(
                                || Arc::new(agent.as_ref().clone()),
                                |run| run.agent.clone(),
                            ),
                        status: SessionStatus::Running,
                        final_text: None,
                        pending_calls: HashMap::new(),
                    },
                );
            }
            // User input is prompt history, not a lifecycle transition.
            EventPayload::UserInputSubmitted { content } => {
                if let Some(run) = self.runs.get_mut(&run_id)
                    && run.input.is_none()
                {
                    run.input = Some(content.clone());
                }
            }
            EventPayload::RunFinished { outcome } => {
                if let Some(run) = self.runs.get_mut(&run_id) {
                    let status = outcome.status();
                    run.status = status;
                    if let cookie_agent_protocol::Open::Known(
                        cookie_agent_protocol::RunOutcome::Completed { final_text },
                    ) = outcome
                    {
                        run.final_text = final_text.clone();
                    }
                    self.status = status;
                }
            }
            EventPayload::ToolCallStarted { start } => {
                if let Some(run) = self.runs.get_mut(&run_id) {
                    let tool = self
                        .fold
                        .tool_names
                        .get(&(start.owner.model_turn_seq, start.owner.content_index))
                        .cloned()
                        .unwrap_or_default();
                    if tool == "delegate_subagent" {
                        self.meta.last_agent_activity = Some(envelope.timestamp);
                    }
                    run.pending_calls.insert(start.tool_call_id, tool);
                }
            }
            EventPayload::ToolCallTerminated { termination } => {
                if let Some(run) = self.runs.get_mut(&run_id) {
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
                let total = self.usage.get_or_insert_with(Usage::default);
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
                // An ACP agent's turn is one step of its round, whose usage
                // the agent reports once, with the round's last turn: a step
                // without tokens of its own is no request of its own.
                let acp_step_without_usage = resolved_model.provider_id.as_str()
                    == cookie_agent_protocol::ACP_MODEL_PROVIDER
                    && !crate::usage::has_observed_tokens(reported);
                let recorded_later = match step {
                    FoldStep::Full {
                        recorded_usage_turns,
                        ..
                    } => recorded_usage_turns.contains(model_turn_seq),
                    FoldStep::Append => false,
                };
                if !recorded_later && !interrupted_without_usage && !acp_step_without_usage {
                    crate::usage::record_stamped(
                        &mut self.usage_rollup,
                        resolved_model,
                        reported,
                        None,
                    );
                    if let Ok(model) = cookie_agent_protocol::ModelKey::new(
                        resolved_model.provider_id.clone(),
                        resolved_model.model_id.clone(),
                    ) {
                        let observation = self
                            .usage_rollup
                            .by_model
                            .get(&model)
                            .map_or(0, |rollup| rollup.observations.len().saturating_sub(1));
                        self.fold
                            .provisional_usage
                            .insert(*model_turn_seq, ProvisionalUsage { model, observation });
                    }
                }
            }
            EventPayload::ModelUsageRecorded {
                model_turn_seq,
                resolved_model,
                usage: reported,
                estimated_cost_pico_usd,
                ..
            } => {
                if let Some(provisional) = self.fold.provisional_usage.get(model_turn_seq) {
                    if !unrecord_provisional(&mut self.usage_rollup, provisional) {
                        return false;
                    }
                    self.fold.provisional_usage.remove(model_turn_seq);
                }
                crate::usage::record_stamped(
                    &mut self.usage_rollup,
                    resolved_model,
                    reported,
                    *estimated_cost_pico_usd,
                );
            }
            EventPayload::InternalAgentUsageRecorded {
                resolved_model,
                usage: reported,
                estimated_cost_pico_usd,
                ..
            } => {
                crate::usage::record_stamped(
                    &mut self.usage_rollup,
                    resolved_model,
                    reported,
                    *estimated_cost_pico_usd,
                );
            }
            _ => {}
        }
        true
    }
}

/// Takes a provisionally counted turn back out of `rollup`, when it is still
/// the newest observation of its model and no arithmetic overflowed (which
/// would make the sums unrecoverable). `false` leaves `rollup` unchanged.
fn unrecord_provisional(rollup: &mut UsageRollup, provisional: &ProvisionalUsage) -> bool {
    if rollup.arithmetic_overflow {
        return false;
    }
    let Some(model) = rollup.by_model.get_mut(&provisional.model) else {
        return false;
    };
    if model.arithmetic_overflow || model.observations.len() != provisional.observation + 1 {
        return false;
    }
    let usage = model.observations.pop().expect("length checked above");
    model.cost_provenance.pop();
    let take = |total: &mut u64, value: Option<u64>| {
        *total -= value.unwrap_or_default();
    };
    for totals in [
        (
            &mut model.input_tokens,
            &mut model.output_tokens,
            &mut model.reasoning_tokens,
            &mut model.cache_read_tokens,
            &mut model.cache_write_tokens,
            &mut model.request_count,
        ),
        (
            &mut rollup.input_tokens,
            &mut rollup.output_tokens,
            &mut rollup.reasoning_tokens,
            &mut rollup.cache_read_tokens,
            &mut rollup.cache_write_tokens,
            &mut rollup.request_count,
        ),
    ] {
        let (input, output, reasoning, cache_read, cache_write, requests) = totals;
        take(input, usage.input_tokens);
        take(output, usage.output_tokens);
        take(reasoning, usage.output_tokens_reasoning);
        take(cache_read, usage.input_tokens_cache_read);
        take(cache_write, usage.input_tokens_cache_write);
        *requests -= 1;
    }
    if model.observations.is_empty() {
        rollup.by_model.remove(&provisional.model);
    }
    true
}

/// Only restart-stable grants are folded into the approval store (§4.3): a grant
/// whose binding cannot survive a restart must not outlive the process that
/// earned it.
pub fn restart_stable_grant(grant: &cookie_agent_protocol::TreeApprovalGrant) -> bool {
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
    let status = payload.run_end_status()?;
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

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub(super) static PROJECTION_FOLDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// See [`skip_reference_fold`].
#[cfg(any(test, feature = "test-support"))]
static SKIP_REFERENCE_FOLD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether two attachments name the same runtime capability at the same
/// workspace, whatever their names.
pub fn same_place(
    left: &cookie_agent_protocol::RuntimeAttachment,
    right: &cookie_agent_protocol::RuntimeAttachment,
) -> bool {
    left.runtime_id == right.runtime_id
        && left.capability == right.capability
        && left.workspace == right.workspace
}
