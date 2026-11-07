//! What a log's readers need of all its events, kept up to date as it grows
//! (`docs/site/specs/forever-sessions.md`, sections 3.1, 3.2, and 3.4):
//! opening a log folds it once, an append folds one event, and a revert folds
//! what stays again. A reader takes [`EventLog::state`](super::EventLog::state)
//! rather than passing over the list.
//!
//! Each fold has its reference in the `reference` module, the scan it
//! replaced, and tests compare the two at every event.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
};

use cookie_agent_protocol::{
    AcpCommand, AcpContextUsage, AcpDetachReason, AcpDetached, AcpOptionChoice, AcpSessionRecord,
    AcpSessionStart, AgentId, ApprovalFinalOutcome, ApprovalId, ApprovalRecord, ApprovalRequest,
    ApprovalStatus, ApprovalStep, ApprovalUserDecision, AttachmentName, AttemptId, CacheTtlStamp,
    ClientResponseId, DelegatedContextTurn, DelegationStage, EventOrigin, EventPayload,
    InternalAgentInvocationId, InternalAgentKind, InternalAgentRunId, InvocationId, Open,
    OperationFingerprint, ProducerMessageId, ProducerOwner, QueuedInputs, ResolvedModelRef,
    RevisionFold, RunId, RunOutcome, SafeErrorMessage, SessionId, SessionModelState, SessionTitle,
    SessionTitleChange, Sha256Digest, StoredEvent, ToolCallId, ToolTerminationOutcome,
    TreeApprovalGrant, UserContent,
};
use jiff::Timestamp;

use crate::goal_projection::{GoalProducerFold, GoalProducerProjection};

#[cfg(any(test, feature = "test-support"))]
pub mod reference;

/// How many of a log's first submitted inputs [`LogState::opening_inputs`] holds.
pub const OPENING_INPUTS: usize = 16;

/// The folds of a log's events. Shared with the readers handed it; an append
/// copies it only when one still holds it.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct LogState {
    acp: AcpState,
    model: SessionModelState,
    queue: QueuedInputs,
    producers: GoalProducerFold,
    inputs: InputFold,
    run: RunFold,
    #[serde(with = "crate::events::sorted::map")]
    calls: HashMap<ToolCallId, Arc<StoredEvent>>,
    approvals: ApprovalFold,
    once: OnceFacts,
    overlay_epoch: u64,
    #[serde(with = "crate::events::sorted::map")]
    internal_agents: HashMap<(InternalAgentInvocationId, InternalAgentRunId), OpenInternalAgent>,
    doom: DoomFold,
    streak: RunStreak,
    latest_turn: Option<(RunId, u64)>,
    fallback: Option<(RunId, usize)>,
    request: RequestFold,
    opening_inputs: Vec<Arc<StoredEvent>>,
    latest_run_input: Option<Arc<StoredEvent>>,
    harvest: HarvestFold,
}

impl LogState {
    /// Folds the next event of the log.
    pub fn observe(&mut self, event: &Arc<StoredEvent>) {
        self.harvest.observe(event);
        self.acp.observe(&event.payload);
        self.model.apply(event.run_id, &event.payload);
        self.queue.apply(event);
        self.run.observe(event, &self.producers);
        self.producers.observe_letting_go(event);
        self.inputs.observe(event);
        self.approvals.observe(event);
        self.once.observe(event);
        self.doom.observe(event);
        self.streak.observe(event);
        self.request.observe(event);
        match &event.payload {
            EventPayload::ToolCallStarted { start } => {
                self.calls.insert(start.tool_call_id, Arc::clone(event));
            }
            EventPayload::ToolCallTerminated { termination } => {
                self.calls.remove(&termination.tool_call_id);
            }
            EventPayload::SessionPermissionOverlaySet { .. } => self.overlay_epoch = event.seq,
            EventPayload::RunStarted { .. } => self.latest_run_input = None,
            EventPayload::UserInputSubmitted { .. } => {
                if self.opening_inputs.len() < OPENING_INPUTS {
                    self.opening_inputs.push(Arc::clone(event));
                }
                if self.latest_run_input.is_none() && event.run_id == self.run_started_id() {
                    self.latest_run_input = Some(Arc::clone(event));
                }
            }
            EventPayload::InternalAgentStarted {
                invocation_id,
                internal_run_id,
                kind,
                ..
            } => {
                self.internal_agents.insert(
                    (*invocation_id, *internal_run_id),
                    OpenInternalAgent {
                        kind: *kind,
                        run: event.run_id,
                    },
                );
            }
            EventPayload::InternalAgentFinished {
                invocation_id,
                internal_run_id,
                ..
            } => {
                self.internal_agents
                    .remove(&(*invocation_id, *internal_run_id));
            }
            EventPayload::ModelTurnCommitted {
                input_through_seq, ..
            } => {
                if let Some(run) = event.run_id {
                    self.latest_turn = Some((run, *input_through_seq));
                }
            }
            EventPayload::ModelAttemptStarted { fallback_index, .. } => {
                if let Some(run) = event.run_id {
                    self.fallback = Some((run, *fallback_index as usize));
                }
            }
            EventPayload::ModelFallback {
                to_fallback_index, ..
            } => {
                if let Some(run) = event.run_id {
                    self.fallback = Some((run, *to_fallback_index as usize));
                }
            }
            _ => {}
        }
    }

    /// What the log says of the ACP agent `agent`: its latest ACP session,
    /// and what left that one behind.
    #[must_use]
    pub fn acp_agent(&self, agent: &AgentId) -> Option<&AcpAgentState> {
        self.acp
            .agents
            .iter()
            .find(|known| &known.record.agent == agent)
    }

    /// The option values the session last had with agent `agent` over
    /// declaration `runtime_agent`: the latest `acp_session_updated.options`
    /// before the ACP session that just started (the log's latest
    /// `acp_session_started`), as long as no earlier ACP session of another
    /// agent or declaration comes between. Empty when there are none.
    #[must_use]
    pub fn recorded_option_values(
        &self,
        agent: &AgentId,
        runtime_agent: &AgentId,
    ) -> &[(String, String)] {
        match &self.acp.options.latest {
            Some((latest, declaration)) if latest == agent && declaration == runtime_agent => {
                &self.acp.options.before_latest
            }
            _ => &[],
        }
    }

    /// The starting values the session was created with, the latest
    /// `acp_session_updated.starting_options`.
    #[must_use]
    pub fn acp_starting_options(&self) -> &[AcpOptionChoice] {
        self.acp.starting_options.as_deref().unwrap_or_default()
    }

    /// The commands the agent offered last.
    #[must_use]
    pub fn acp_commands(&self) -> Option<&[AcpCommand]> {
        self.acp.commands.as_deref()
    }

    /// The context use the log stored last.
    #[must_use]
    pub fn acp_context(&self) -> Option<AcpContextUsage> {
        self.acp.context
    }

    /// The context `agent`'s latest ACP session was given, frozen for the life
    /// of that session (`docs/site/specs/forever-sessions.md` section 3.4);
    /// empty when it was given none.
    #[must_use]
    pub fn acp_prompt_context(&self, agent: &AgentId) -> &[String] {
        self.acp_agent(agent)
            .and_then(|known| known.prompt_context.as_deref())
            .map_or(&[], Vec::as_slice)
    }

    /// The ACP sessions the log records that are the cookie session's own, each
    /// once, as first recorded: every one but those a fork left behind, which
    /// stay their source's.
    pub fn acp_own_records(&self) -> impl Iterator<Item = &AcpSessionRecord> {
        self.acp
            .records
            .iter()
            .filter(|record| !self.acp.inherited.contains(&record.acp_session_id))
    }

    /// Whether the log records an ACP session at all.
    #[must_use]
    pub fn has_acp_sessions(&self) -> bool {
        !self.acp.records.is_empty()
    }

    /// The session's accepted model selection, as committed turns advanced it.
    #[must_use]
    pub fn model(&self) -> &SessionModelState {
        &self.model
    }

    /// The session's queued inputs.
    #[must_use]
    pub fn queue(&self) -> &QueuedInputs {
        &self.queue
    }

    /// The session's goal and producer messages.
    #[must_use]
    pub fn producers(&self) -> Arc<GoalProducerProjection> {
        self.producers.projection()
    }

    /// Whether a producer message waits that no run admitted yet.
    #[must_use]
    pub fn producer_message_unadmitted(&self) -> bool {
        self.producers.has_unadmitted()
    }

    /// `input_through_seq` of the latest committed turn: what no later prompt
    /// carries again. [`Self::inputs_since`] reads from here on.
    #[must_use]
    pub fn input_boundary(&self) -> u64 {
        self.inputs.boundary
    }

    /// The input the session took after `boundary`, which is at or after
    /// [`Self::input_boundary`], in log order: user input applied, and
    /// producer messages admitted.
    #[must_use]
    pub fn inputs_since(&self, boundary: u64) -> Vec<TakenInput<'_>> {
        debug_assert!(boundary >= self.inputs.boundary);
        self.inputs
            .taken
            .iter()
            .filter(|taken| taken.seq() > boundary)
            .filter_map(|taken| match taken {
                Taken::User { submission, .. } => match &submission.payload {
                    EventPayload::UserInputSubmitted { content } => Some(TakenInput::User {
                        user_input_seq: submission.seq,
                        content,
                    }),
                    _ => None,
                },
                Taken::Producer { seq, message_id } => {
                    let message = self.producers.message(message_id)?;
                    (!message.discarded && message.admission.is_some_and(|(_, at)| at == *seq))
                        .then_some(TakenInput::Producer {
                            message_id: *message_id,
                            body: &message.body,
                            wake: matches!(message.producer_owner, ProducerOwner::AcpAgent { .. }),
                        })
                }
            })
            .collect()
    }

    /// Whether the input after `boundary` holds a wake of the agent's own (an
    /// admitted `acp_agent` producer message).
    #[must_use]
    pub fn woke_since(&self, boundary: u64) -> bool {
        self.inputs_since(boundary)
            .iter()
            .any(|input| matches!(input, TakenInput::Producer { wake: true, .. }))
    }

    /// The `user_input_submitted` records of `run` no `user_input_applied`
    /// answered yet, in log order, while the run is unfinished.
    pub fn unapplied_submissions(&self, run: RunId) -> impl Iterator<Item = u64> + '_ {
        self.inputs
            .submitted
            .values()
            .filter(move |event| event.run_id == Some(run))
            .map(|event| event.seq)
    }

    /// What a tree load takes from the log.
    #[must_use]
    pub fn harvest(&self) -> &HarvestFold {
        &self.harvest
    }

    /// The log's first `user_input_submitted` records, up to [`OPENING_INPUTS`]
    /// of them: what a session's title is generated from.
    #[must_use]
    pub fn opening_inputs(&self) -> &[Arc<StoredEvent>] {
        &self.opening_inputs
    }

    /// The first `user_input_submitted` of `run` when it is the log's latest
    /// run, which may have none yet; `None` when it is not.
    #[must_use]
    pub fn first_input_of_run(&self, run: RunId) -> Option<Option<&Arc<StoredEvent>>> {
        self.run_started(run)?;
        Some(self.latest_run_input.as_ref())
    }

    fn run_started_id(&self) -> Option<RunId> {
        self.run.started.as_ref().and_then(|started| started.run_id)
    }

    /// The `run_started` of the log's latest run.
    #[must_use]
    pub fn latest_run_started(&self) -> Option<&Arc<StoredEvent>> {
        self.run.started.as_ref()
    }

    /// The `run_started` of `run`, when it is the log's latest.
    #[must_use]
    pub fn run_started(&self, run: RunId) -> Option<&Arc<StoredEvent>> {
        self.run
            .started
            .as_ref()
            .filter(|event| event.run_id == Some(run))
    }

    /// Whether the user's goal was active or paused as the latest run started.
    #[must_use]
    pub fn goal_open_at_run_start(&self) -> bool {
        self.run.goal_open_at_start
    }

    /// Whether runtimes came or went between the run before `run` and `run`,
    /// its latest; `None` when no run ended before it.
    #[must_use]
    pub fn machines_changed_before(&self, run: RunId) -> Option<bool> {
        self.run_started(run)?;
        self.run.machines_changed_before_start
    }

    /// The attachments the log attached since the latest run's end; `None`
    /// when no run ended.
    #[must_use]
    pub fn attached_since_run_end(&self) -> Option<&HashSet<AttachmentName>> {
        self.run.ended.then_some(&self.run.attached_since_end)
    }

    /// The `user_input_admitted` records of `run`, the log's latest, it has
    /// yet to submit.
    #[must_use]
    pub fn pending_inputs(&self, run: RunId) -> &[Arc<StoredEvent>] {
        if self.run_started(run).is_some() {
            &self.run.pending
        } else {
            &[]
        }
    }

    /// The `user_input_admitted` records outside a run since the latest
    /// run's end that nothing recalled.
    #[must_use]
    pub fn runless_pending_inputs(&self) -> &[Arc<StoredEvent>] {
        &self.run.runless
    }

    /// Whether `run_finished` recorded `run` as cancelled.
    #[must_use]
    pub fn run_cancelled(&self, run: RunId) -> bool {
        self.run.cancelled.contains(&run)
    }

    /// The model of `run`'s last attempt, when it is the latest run to try one.
    #[must_use]
    pub fn last_attempt_model(&self, run: RunId) -> Option<&ResolvedModelRef> {
        self.run
            .last_attempt
            .as_ref()
            .filter(|(attempted, _)| *attempted == run)
            .map(|(_, model)| model)
    }

    /// When the latest run ended, by the log's records.
    #[must_use]
    pub fn last_run_end(&self) -> Option<Timestamp> {
        self.run.last_end
    }

    /// The `tool_call_started` of the call `id`, while no termination
    /// recorded its end.
    #[must_use]
    pub fn open_call(&self, id: ToolCallId) -> Option<&Arc<StoredEvent>> {
        self.calls.get(&id)
    }

    /// The approval `id` while it is pending or escalated.
    #[must_use]
    pub fn open_approval(&self, id: ApprovalId) -> Option<&OpenApproval> {
        self.approvals.open.get(&id)
    }

    /// Every approval pending or escalated.
    pub fn open_approvals(&self) -> impl Iterator<Item = &OpenApproval> {
        self.approvals.open.values()
    }

    /// The approval that settled last, with the run that asked: a response
    /// reads back what it just settled.
    #[must_use]
    pub fn settled_approval(&self, id: ApprovalId) -> Option<&OpenApproval> {
        self.approvals
            .settled
            .as_ref()
            .filter(|settled| settled.record.request.approval_id() == id)
    }

    /// The first user decision recorded under `response`.
    #[must_use]
    pub fn response(&self, response: &ClientResponseId) -> Option<&Arc<UserDecision>> {
        self.approvals.responses.get(response)
    }

    /// The user's latest decision on the approval `id`.
    #[must_use]
    pub fn user_decision(&self, id: ApprovalId) -> Option<&Arc<UserDecision>> {
        self.approvals.decisions.get(&id)
    }

    /// How many times `run`, the log's latest, asked to approve `fingerprint`
    /// since the last reset: user input, or a different operation completing.
    #[must_use]
    pub fn doom_loop_repetitions(&self, run: RunId, fingerprint: &OperationFingerprint) -> u32 {
        if self.doom.run == Some(run) {
            self.doom.requests.get(fingerprint).copied().unwrap_or(0)
        } else {
            0
        }
    }

    /// How the latest runs ended, as far back as anything cleared them.
    #[must_use]
    pub fn run_streak(&self) -> &RunStreak {
        &self.streak
    }

    /// The internal agents that started and did not finish.
    pub fn open_internal_agents(
        &self,
    ) -> impl Iterator<
        Item = (
            &(InternalAgentInvocationId, InternalAgentRunId),
            &OpenInternalAgent,
        ),
    > {
        self.internal_agents.iter()
    }

    /// The run of the latest committed turn and its `input_through_seq`.
    #[must_use]
    pub fn latest_turn(&self) -> Option<(RunId, u64)> {
        self.latest_turn
    }

    /// The latest run to try a model, and the fallback it is on.
    #[must_use]
    pub fn latest_fallback(&self) -> Option<(RunId, usize)> {
        self.fallback
    }

    /// The session's latest request; `None` when it has none with a cache TTL,
    /// or a checkpoint came after it.
    #[must_use]
    pub fn latest_request(&self) -> Option<&LatestRequest> {
        self.request.latest.as_ref()
    }

    /// The seq of the latest `session_permission_overlay_set`; 0 without one.
    #[must_use]
    pub fn permission_overlay_epoch(&self) -> u64 {
        self.overlay_epoch
    }

    /// What the log holds of a seed of `invocation`'s context.
    #[must_use]
    pub fn seed(&self, invocation: InvocationId, turns: &[DelegatedContextTurn]) -> Recorded {
        let Some(seeded) = self.once.seeds.get(&invocation) else {
            return Recorded::Absent;
        };
        let digest = seed_digest(turns);
        if seeded.iter().all(|known| *known == digest) {
            Recorded::Same
        } else {
            Recorded::Conflicting
        }
    }

    /// What the log holds of a delegated title `title` of `invocation`.
    #[must_use]
    pub fn delegated_title(
        &self,
        invocation: InvocationId,
        title: &SessionTitle,
    ) -> DelegatedTitle {
        let mut same = false;
        for (recorded, recorded_title) in &self.once.delegated_titles {
            if *recorded != invocation {
                return DelegatedTitle::OtherTitle;
            }
            if recorded_title != title {
                return DelegatedTitle::OtherOrigin;
            }
            same = true;
        }
        if self.once.other_title {
            DelegatedTitle::OtherTitle
        } else if same {
            DelegatedTitle::Same
        } else {
            DelegatedTitle::Absent
        }
    }

    /// Whether the log holds a `skill_loaded` of `name` with `body`.
    #[must_use]
    pub fn skill_loaded(&self, name: &str, body: &[u8]) -> bool {
        self.once
            .skills
            .contains(&(name.to_owned(), Sha256Digest::of_bytes(body)))
    }

    /// Whether the log links the call to the child session.
    #[must_use]
    pub fn call_linked(&self, call: ToolCallId, child: SessionId) -> bool {
        self.once.links.contains(&(call, child))
    }

    /// Whether the log accepted the completion of the delegation `invocation`.
    #[must_use]
    pub fn delegation_completion_accepted(&self, invocation: InvocationId) -> bool {
        self.once.delegations_accepted.contains(&invocation)
    }
}

/// What the input of a turn came as ([`LogState::inputs_since`]).
#[derive(Clone, Debug, PartialEq)]
pub enum TakenInput<'a> {
    User {
        user_input_seq: u64,
        content: &'a UserContent,
    },
    Producer {
        message_id: ProducerMessageId,
        body: &'a UserContent,
        /// The agent made its wake's message: never sent to it.
        wake: bool,
    },
}

/// Whether a value the log may hold once is there ([`LogState::seed`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Recorded {
    Absent,
    Same,
    Conflicting,
}

/// What a delegated child's log holds of its title
/// ([`LogState::delegated_title`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegatedTitle {
    Absent,
    Same,
    /// The same delegation set another title.
    OtherOrigin,
    /// Something other than this delegation or the user set a title.
    OtherTitle,
}

/// One agent's ACP session as the log last recorded it.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct AcpAgentState {
    pub record: AcpSessionRecord,
    /// What left that session behind since it started, in log order.
    pub detached: Vec<AcpDetached>,
    /// What that session was given: the latest `prompt_context` recorded while
    /// it was the log's latest. It goes on the record after its session starts,
    /// so it is never another agent's. A resume or a fork carries it on, and a
    /// session that starts afresh has been told nothing.
    pub prompt_context: Option<Arc<Vec<String>>>,
}

/// Latest values of `acp_session_started` and `acp_session_updated`.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct AcpState {
    /// In the order the agents first appear.
    agents: Vec<AcpAgentState>,
    options: RecordedOptions,
    starting_options: Option<Vec<AcpOptionChoice>>,
    commands: Option<Vec<AcpCommand>>,
    context: Option<AcpContextUsage>,
    /// Every ACP session, each once, as first recorded.
    records: Vec<AcpSessionRecord>,
    /// The ACP sessions a fork left behind.
    #[serde(with = "crate::events::sorted::set")]
    inherited: HashSet<String>,
}

/// The chain of option values [`LogState::recorded_option_values`] reads.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct RecordedOptions {
    /// The agent and declaration of the log's latest ACP session.
    latest: Option<(AgentId, AgentId)>,
    /// The values in force as that session started.
    before_latest: Vec<(String, String)>,
    /// The latest values recorded after it, or before any session.
    since_latest: Option<Vec<(String, String)>>,
}

impl AcpState {
    fn observe(&mut self, payload: &EventPayload) {
        match payload {
            EventPayload::AcpSessionStarted { record } => {
                if !self
                    .records
                    .iter()
                    .any(|known| known.acp_session_id == record.acp_session_id)
                {
                    self.records.push(record.clone());
                }
                let key = (record.agent.clone(), record.runtime_agent.clone());
                self.options.before_latest = match self.options.since_latest.take() {
                    Some(options) => options,
                    None if self.options.latest.as_ref() == Some(&key) => {
                        std::mem::take(&mut self.options.before_latest)
                    }
                    None => Vec::new(),
                };
                self.options.latest = Some(key);
                match self
                    .agents
                    .iter_mut()
                    .find(|known| known.record.agent == record.agent)
                {
                    Some(known) => {
                        known.record = record.clone();
                        known.detached.clear();
                        if !continues_a_session(record.how) {
                            known.prompt_context = None;
                        }
                    }
                    None => self.agents.push(AcpAgentState {
                        record: record.clone(),
                        detached: Vec::new(),
                        prompt_context: None,
                    }),
                }
            }
            EventPayload::AcpSessionUpdated { update } => {
                if let Some(marker) = &update.detached {
                    if marker.reason != AcpDetachReason::Reverted {
                        self.inherited.insert(marker.acp_session_id.clone());
                    }
                    for known in &mut self.agents {
                        if marker.acp_session_id == known.record.acp_session_id {
                            known.detached.push(marker.clone());
                        }
                    }
                }
                if let Some(options) = &update.options {
                    self.options.since_latest = Some(
                        options
                            .iter()
                            .map(|option| (option.id.clone(), option.current_value.clone()))
                            .collect(),
                    );
                }
                if let Some(options) = &update.starting_options {
                    self.starting_options = Some(options.clone());
                }
                if let Some(commands) = &update.commands {
                    self.commands = Some(commands.clone());
                }
                if update.context.is_some() {
                    self.context = update.context;
                }
                if let Some(context) = &update.prompt_context
                    && let Some((latest, _)) = &self.options.latest
                    && let Some(known) = self
                        .agents
                        .iter_mut()
                        .find(|known| &known.record.agent == latest)
                {
                    known.prompt_context = Some(Arc::new(context.clone()));
                }
            }
            _ => {}
        }
    }
}

/// Whether an ACP session started this way goes on from an earlier one, and so
/// keeps what that one was given.
fn continues_a_session(how: AcpSessionStart) -> bool {
    matches!(
        how,
        AcpSessionStart::Resumed | AcpSessionStart::Loaded | AcpSessionStart::Forked
    )
}

/// The input after the latest committed turn's `input_through_seq`, which is
/// as far back as a prompt reads, with the body of each.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct InputFold {
    boundary: u64,
    /// `user_input_submitted` records no `user_input_applied` answered, until
    /// their run ends.
    submitted: BTreeMap<u64, Arc<StoredEvent>>,
    taken: Vec<Taken>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
enum Taken {
    User {
        applied_seq: u64,
        submission: Arc<StoredEvent>,
    },
    Producer {
        seq: u64,
        message_id: ProducerMessageId,
    },
}

impl Taken {
    fn seq(&self) -> u64 {
        match self {
            Self::User { applied_seq, .. } => *applied_seq,
            Self::Producer { seq, .. } => *seq,
        }
    }
}

impl InputFold {
    fn observe(&mut self, event: &Arc<StoredEvent>) {
        match &event.payload {
            EventPayload::UserInputSubmitted { .. } => {
                self.submitted.insert(event.seq, Arc::clone(event));
            }
            EventPayload::UserInputApplied { user_input_seq } => {
                if let Some(submission) = self.submitted.remove(user_input_seq) {
                    self.taken.push(Taken::User {
                        applied_seq: event.seq,
                        submission,
                    });
                }
            }
            EventPayload::ProducerMessageAdmitted { message_id } => {
                self.taken.push(Taken::Producer {
                    seq: event.seq,
                    message_id: *message_id,
                });
            }
            EventPayload::ModelTurnCommitted {
                input_through_seq, ..
            } => {
                self.boundary = *input_through_seq;
                self.taken.retain(|taken| taken.seq() > *input_through_seq);
            }
            EventPayload::RunFinished { .. } => self.submitted.clear(),
            _ => {}
        }
    }
}

/// The latest run, and what stands around it.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct RunFold {
    started: Option<Arc<StoredEvent>>,
    goal_open_at_start: bool,
    /// A run ended.
    ended: bool,
    last_end: Option<Timestamp>,
    machines_changed_since_end: bool,
    machines_changed_before_start: Option<bool>,
    #[serde(with = "crate::events::sorted::set")]
    attached_since_end: HashSet<AttachmentName>,
    /// The latest run's admitted inputs not yet submitted.
    pending: Vec<Arc<StoredEvent>>,
    /// Whether the latest run's first input went in.
    initial_input_submitted: bool,
    runless: Vec<Arc<StoredEvent>>,
    #[serde(with = "crate::events::sorted::set")]
    cancelled: HashSet<RunId>,
    last_attempt: Option<(RunId, ResolvedModelRef)>,
}

impl RunFold {
    fn observe(&mut self, event: &Arc<StoredEvent>, producers: &GoalProducerFold) {
        let current = self.started.as_ref().and_then(|started| started.run_id);
        let own = event.run_id.is_some() && event.run_id == current;
        match &event.payload {
            EventPayload::RunStarted { .. } => {
                if event.run_id.is_some() {
                    self.started = Some(Arc::clone(event));
                    self.goal_open_at_start = producers.goal_is_open();
                    self.machines_changed_before_start =
                        self.ended.then_some(self.machines_changed_since_end);
                    self.pending = self.runless.clone();
                    self.initial_input_submitted = false;
                }
            }
            EventPayload::RunFinished { outcome } => {
                self.ended = true;
                self.machines_changed_since_end = false;
                self.attached_since_end.clear();
                self.runless.clear();
                if own {
                    self.pending.clear();
                }
                if event.run_id.is_some() {
                    self.last_end = self.last_end.max(Some(event.timestamp));
                }
                if let (Some(run), Open::Known(RunOutcome::Cancelled { .. })) =
                    (event.run_id, outcome)
                {
                    self.cancelled.insert(run);
                }
            }
            EventPayload::RuntimeAttached { name, .. } => {
                self.machines_changed_since_end = true;
                self.attached_since_end.insert(name.clone());
            }
            EventPayload::RuntimeDetached { .. } => self.machines_changed_since_end = true,
            EventPayload::UserInputAdmitted { .. } => {
                if event.run_id.is_none() {
                    self.runless.push(Arc::clone(event));
                } else if own {
                    self.pending.push(Arc::clone(event));
                }
            }
            EventPayload::UserInputRecalled { user_input_seq, .. } => {
                if event.run_id.is_none() {
                    self.runless.retain(|input| input.seq != *user_input_seq);
                } else if own {
                    self.pending.retain(|input| input.seq != *user_input_seq);
                }
            }
            EventPayload::UserInputSubmitted { .. } if own => {
                if self.initial_input_submitted {
                    if !self.pending.is_empty() {
                        self.pending.remove(0);
                    }
                } else {
                    self.initial_input_submitted = true;
                }
            }
            EventPayload::ProducerMessageAdmitted { .. } if own => {
                self.initial_input_submitted = true;
            }
            EventPayload::ModelAttemptStarted { resolved_model, .. } => {
                if let Some(run) = event.run_id {
                    self.last_attempt = Some((run, resolved_model.clone()));
                }
            }
            _ => {}
        }
    }
}

/// An approval still pending or escalated, with the run that asked.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct OpenApproval {
    pub record: ApprovalRecord,
    pub run: Option<RunId>,
}

/// A user's decision on an approval, as an `approval_step` records it.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct UserDecision {
    pub approval_id: ApprovalId,
    pub decision: ApprovalUserDecision,
    pub feedback: Option<cookie_agent_protocol::ApprovalFeedback>,
    pub agent_option: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct ApprovalFold {
    #[serde(with = "crate::events::sorted::map")]
    open: HashMap<ApprovalId, OpenApproval>,
    /// By `client_response_id`, the first of each.
    #[serde(with = "crate::events::sorted::map")]
    responses: HashMap<ClientResponseId, Arc<UserDecision>>,
    /// By approval, the latest of each.
    #[serde(with = "crate::events::sorted::map")]
    decisions: HashMap<ApprovalId, Arc<UserDecision>>,
    /// The approval that settled last.
    settled: Option<OpenApproval>,
}

impl ApprovalFold {
    fn observe(&mut self, event: &Arc<StoredEvent>) {
        if let EventPayload::ApprovalRequested { request } = &event.payload {
            if self.settled.as_ref().is_some_and(|settled| {
                settled.record.request.approval_id() == request.approval_id()
            }) {
                self.settled = None;
            }
            self.open.insert(
                request.approval_id(),
                OpenApproval {
                    record: pending_record(event.session_id, request),
                    run: event.run_id,
                },
            );
            return;
        }
        if let EventPayload::ApprovalStep {
            approval_id,
            step:
                Open::Known(ApprovalStep::UserDecision {
                    client_response_id,
                    decision,
                    feedback,
                    agent_option,
                    ..
                }),
        } = &event.payload
        {
            let recorded = Arc::new(UserDecision {
                approval_id: *approval_id,
                decision: *decision,
                feedback: feedback.clone(),
                agent_option: agent_option.clone(),
            });
            self.responses
                .entry(client_response_id.clone())
                .or_insert_with(|| Arc::clone(&recorded));
            self.decisions.insert(*approval_id, recorded);
        }
        let Some(approval_id) = approval_event_id(&event.payload) else {
            return;
        };
        if let Some(open) = self.open.get_mut(&approval_id) {
            apply_approval_event(&mut open.record, &event.payload);
            if !matches!(
                open.record.status,
                ApprovalStatus::Pending | ApprovalStatus::Escalated
            ) {
                self.settled = self.open.remove(&approval_id);
            }
        } else if let Some(settled) = self
            .settled
            .as_mut()
            .filter(|settled| settled.record.request.approval_id() == approval_id)
        {
            apply_approval_event(&mut settled.record, &event.payload);
        }
    }
}

/// An approval as its request left it.
#[must_use]
pub fn pending_record(session_id: SessionId, request: &ApprovalRequest) -> ApprovalRecord {
    ApprovalRecord {
        session_id,
        request: request.clone(),
        status: ApprovalStatus::Pending,
        internal_decision: None,
        user_decision: None,
        final_decision: None,
    }
}

/// The approval a post-request lifecycle event belongs to.
#[must_use]
pub fn approval_event_id(payload: &EventPayload) -> Option<ApprovalId> {
    match payload {
        EventPayload::ApprovalStep { approval_id, .. }
        | EventPayload::ApprovalFinalized { approval_id, .. } => Some(*approval_id),
        _ => None,
    }
}

/// Folds a post-request lifecycle event into the approval it belongs to.
pub fn apply_approval_event(record: &mut ApprovalRecord, payload: &EventPayload) {
    match payload {
        EventPayload::ApprovalStep {
            step: Open::Known(step),
            ..
        } => match step {
            ApprovalStep::Evaluated { decision } => {
                record.internal_decision = Some(decision.clone());
            }
            ApprovalStep::Escalated { expires_at, .. } => {
                record.status = ApprovalStatus::Escalated;
                // The window an escalation opened is the record's.
                if expires_at.is_some() {
                    record.request = record.request.clone().with_expires_at(*expires_at);
                }
            }
            ApprovalStep::UserDecision { decision, .. } => {
                record.user_decision = Some(*decision);
            }
            ApprovalStep::DoomLoop { .. } => {}
        },
        // An outcome this build does not know is not an approval.
        EventPayload::ApprovalFinalized { outcome, .. } => match outcome.known() {
            Some(outcome) => {
                let decision = outcome.decision();
                record.status = match decision.outcome {
                    ApprovalFinalOutcome::Approved => ApprovalStatus::Approved,
                    ApprovalFinalOutcome::Rejected => ApprovalStatus::Rejected,
                    ApprovalFinalOutcome::Cancelled => ApprovalStatus::Cancelled,
                    ApprovalFinalOutcome::Expired => ApprovalStatus::Expired,
                };
                record.final_decision = Some(decision);
            }
            None => record.status = ApprovalStatus::Cancelled,
        },
        _ => {}
    }
}

/// An internal agent that started and did not finish.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct OpenInternalAgent {
    pub kind: InternalAgentKind,
    pub run: Option<RunId>,
}

/// What the doom-loop guard counts, for the latest run. It applies to every
/// approval of a session: an ACP agent's go through it too.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct DoomFold {
    run: Option<RunId>,
    /// The approvals asked for since the last reset, by operation.
    #[serde(with = "crate::events::sorted::map")]
    requests: HashMap<OperationFingerprint, u32>,
    /// The operation of each call started since the latest input and not ended.
    #[serde(with = "crate::events::sorted::map")]
    starts: HashMap<ToolCallId, OperationFingerprint>,
}

impl DoomFold {
    fn observe(&mut self, event: &StoredEvent) {
        if let EventPayload::RunStarted { .. } = &event.payload {
            if event.run_id.is_some() {
                *self = Self {
                    run: event.run_id,
                    ..Self::default()
                };
            }
            return;
        }
        if event.run_id.is_none() || event.run_id != self.run {
            return;
        }
        match &event.payload {
            EventPayload::UserInputSubmitted { .. } | EventPayload::UserInputApplied { .. } => {
                self.requests.clear();
                self.starts.clear();
            }
            EventPayload::ApprovalRequested { request } => {
                let count = self
                    .requests
                    .entry(request.operation_fingerprint().clone())
                    .or_default();
                *count = count.saturating_add(1);
            }
            EventPayload::ToolCallStarted { start } => {
                self.starts
                    .insert(start.tool_call_id, start.operation_fingerprint.clone());
            }
            EventPayload::ToolCallTerminated { termination } => {
                if let Some(operation) = self.starts.remove(&termination.tool_call_id)
                    && termination.outcome == ToolTerminationOutcome::Completed
                {
                    // A different operation completing resets the others.
                    self.requests.retain(|requested, _| *requested == operation);
                }
            }
            _ => {}
        }
    }
}

/// How the latest runs ended, as far back as input, a resume, or a run's
/// success cleared them: what decides whether a producer's message may wake the
/// session.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct RunStreak {
    /// The session's automatic runs were held, and nothing cleared that.
    pub held: bool,
    /// The user stopped the latest run that did not fail.
    pub stopped: bool,
    /// The runs that failed since.
    pub failures: u32,
    /// The latest of their errors.
    pub last_error: Option<SafeErrorMessage>,
}

impl RunStreak {
    fn observe(&mut self, event: &StoredEvent) {
        match &event.payload {
            EventPayload::AutomaticRunsHeld { .. } => {
                *self = Self {
                    held: true,
                    ..Self::default()
                };
            }
            EventPayload::AutomaticRunsResumed {} | EventPayload::UserInputSubmitted { .. } => {
                *self = Self::default();
            }
            EventPayload::RunFinished { outcome } => match outcome {
                Open::Known(RunOutcome::Cancelled { .. })
                    if event.origin.as_ref().is_some_and(stopped_by_user) =>
                {
                    *self = Self {
                        stopped: true,
                        ..Self::default()
                    };
                }
                Open::Known(RunOutcome::Failed { error, .. }) => {
                    self.stopped = false;
                    self.failures += 1;
                    self.last_error = Some(error.clone());
                }
                // A run a restart interrupted resumes; it neither counts nor
                // clears.
                Open::Known(RunOutcome::Interrupted { .. }) => {}
                _ => *self = Self::default(),
            },
            _ => {}
        }
    }
}

/// Whether `origin` is the user's: a stop by the user holds a session.
fn stopped_by_user(origin: &EventOrigin) -> bool {
    origin.as_str() == "user" || origin.as_str().starts_with("client:")
}

/// What the rule of a compaction before cache expiry reads of a session's latest
/// request. An ACP agent's usage carries a cache TTL too, so this serves ACP
/// sessions.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct LatestRequest {
    /// The sequence of its usage event.
    pub usage_seq: u64,
    /// When it was sent: its prepared request, else its usage event.
    pub sent_at: Timestamp,
    pub stamp: CacheTtlStamp,
    /// Its reported input plus output tokens.
    pub tokens: u64,
    pub model: ResolvedModelRef,
}

/// [`LatestRequest`], with the few recent attempts and turns that finding when it
/// was sent reads.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct RequestFold {
    latest: Option<LatestRequest>,
    /// When each recent attempt's request went out.
    attempts: VecDeque<(AttemptId, Timestamp)>,
    /// When each recently committed turn's request went out, by `model_turn_seq`.
    turns: VecDeque<(u64, Option<Timestamp>)>,
}

impl RequestFold {
    /// How many attempts and turns it remembers: a usage event follows its
    /// turn's commit closely, and a commit its attempt.
    const REMEMBERS: usize = 16;

    fn observe(&mut self, event: &StoredEvent) {
        match &event.payload {
            EventPayload::ContextCheckpointCommitted { .. } => self.latest = None,
            EventPayload::ModelRequestPrepared { attempt_id, .. }
            | EventPayload::ModelAttemptStarted { attempt_id, .. } => {
                remember(&mut self.attempts, (*attempt_id, event.timestamp));
            }
            EventPayload::ModelTurnCommitted {
                model_turn_seq,
                attempt_id,
                ..
            } => {
                let sent = self
                    .attempts
                    .iter()
                    .rev()
                    .find(|(attempt, _)| attempt == attempt_id)
                    .map(|(_, sent)| *sent);
                remember(&mut self.turns, (*model_turn_seq, sent));
            }
            EventPayload::ModelUsageRecorded {
                model_turn_seq,
                resolved_model,
                usage,
                cache_ttl,
                ..
            } => {
                self.latest = cache_ttl.map(|stamp| LatestRequest {
                    usage_seq: event.seq,
                    sent_at: self
                        .turns
                        .iter()
                        .rev()
                        .find(|(turn, _)| turn == model_turn_seq)
                        .and_then(|(_, sent)| *sent)
                        .unwrap_or(event.timestamp),
                    stamp,
                    tokens: usage
                        .input_tokens
                        .unwrap_or(0)
                        .saturating_add(usage.output_tokens.unwrap_or(0)),
                    model: resolved_model.clone(),
                });
            }
            _ => {}
        }
    }
}

fn remember<T>(list: &mut VecDeque<T>, item: T) {
    if list.len() == RequestFold::REMEMBERS {
        list.pop_front();
    }
    list.push_back(item);
}

/// Facts a log records once: a set of keys, which grows with the log but
/// stays small (section 3.1).
/// A delegation record a tree load hands over, with the revisions it was frozen
/// against.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct HarvestedDelegation {
    pub seq: u64,
    pub run: Option<RunId>,
    pub payload: EventPayload,
}

/// What a tree load takes from a log, whoever holds it in the tree: its grants,
/// its delegation records, and what the registry needs of it as a delegation's
/// parent (`docs/site/specs/forever-sessions.md` section 4.1). So neither a load
/// nor a registry rebuild reads the log for them.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct HarvestFold {
    /// Every grant, restart-stable or not.
    grants: Vec<TreeApprovalGrant>,
    /// Whether one of them is tainted is asked of the log when it is handed over,
    /// as damage found later can excuse or condemn an earlier record.
    delegations: Vec<HarvestedDelegation>,
    revisions: RevisionFold,
    /// The child each invocation started.
    #[serde(with = "crate::events::sorted::map")]
    started: HashMap<InvocationId, SessionId>,
    /// `(parent run, child)` of each queued delegation.
    queued: Vec<(RunId, SessionId)>,
    /// Delegations whose delivery a producer message accepted.
    producer_accepted: Vec<InvocationId>,
}

impl HarvestFold {
    fn observe(&mut self, event: &StoredEvent) {
        self.revisions.observe(&event.payload);
        match &event.payload {
            EventPayload::TreeApprovalGrantCommitted { grant } => self.grants.push(grant.clone()),
            EventPayload::DelegationProgressed {
                invocation_id,
                stage: Open::Known(DelegationStage::Started { child_session_id }),
            } => {
                self.started.insert(*invocation_id, *child_session_id);
            }
            EventPayload::DelegationProgressed {
                invocation_id,
                stage: Open::Known(DelegationStage::Queued { .. }),
            } => {
                if let (Some(run), Some(child)) = (event.run_id, self.started.get(invocation_id)) {
                    self.queued.push((run, *child));
                }
            }
            EventPayload::ProducerMessageAccepted {
                producer_owner: ProducerOwner::Delegation { invocation_id },
                ..
            } => self.producer_accepted.push(*invocation_id),
            _ => {}
        }
        if crate::delegation_events::is_delegation_payload(&event.payload) {
            self.delegations.push(HarvestedDelegation {
                seq: event.seq,
                run: event.run_id,
                payload: crate::delegation_events::harvested(&event.payload, &self.revisions),
            });
        }
    }

    /// Every grant of the log, in order.
    #[must_use]
    pub fn grants(&self) -> &[TreeApprovalGrant] {
        &self.grants
    }

    /// Every delegation record of the log, in order.
    #[must_use]
    pub fn delegations(&self) -> &[HarvestedDelegation] {
        &self.delegations
    }

    /// `(parent run, child)` of each queued delegation, in order.
    #[must_use]
    pub fn queued(&self) -> &[(RunId, SessionId)] {
        &self.queued
    }

    /// The delegations whose delivery a producer message accepted, in order.
    #[must_use]
    pub fn producer_accepted(&self) -> &[InvocationId] {
        &self.producer_accepted
    }
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct OnceFacts {
    #[serde(with = "crate::events::sorted::map")]
    seeds: HashMap<InvocationId, Vec<Sha256Digest>>,
    /// The delegated titles up to the first title anything else set, which
    /// [`LogState::delegated_title`] reads no further than.
    delegated_titles: Vec<(InvocationId, SessionTitle)>,
    other_title: bool,
    #[serde(with = "crate::events::sorted::set")]
    skills: HashSet<(String, Sha256Digest)>,
    #[serde(with = "crate::events::sorted::set")]
    links: HashSet<(ToolCallId, SessionId)>,
    #[serde(with = "crate::events::sorted::set")]
    delegations_accepted: HashSet<InvocationId>,
}

impl OnceFacts {
    fn observe(&mut self, event: &StoredEvent) {
        match &event.payload {
            EventPayload::DelegatedContextSeeded {
                invocation_id,
                turns,
            } => {
                self.seeds
                    .entry(*invocation_id)
                    .or_default()
                    .push(seed_digest(turns));
            }
            EventPayload::SessionTitleCommitted { change, .. } => match change {
                SessionTitleChange::DelegatedSet {
                    title,
                    invocation_id,
                } => {
                    if !self.other_title {
                        self.delegated_titles.push((*invocation_id, title.clone()));
                    }
                }
                SessionTitleChange::UserSet { .. }
                | SessionTitleChange::UserClear { .. }
                | SessionTitleChange::UserReset { .. } => {}
                _ => self.other_title = true,
            },
            EventPayload::SkillLoaded {
                name,
                rendered_body,
                ..
            } => {
                self.skills.insert((
                    name.clone(),
                    Sha256Digest::of_bytes(rendered_body.as_bytes()),
                ));
            }
            EventPayload::ToolCallLinked {
                tool_call_id,
                child_session_id,
            } => {
                self.links.insert((*tool_call_id, *child_session_id));
            }
            EventPayload::ProducerMessageAccepted {
                producer_owner: ProducerOwner::Delegation { invocation_id },
                ..
            } => {
                self.delegations_accepted.insert(*invocation_id);
            }
            _ => {}
        }
    }
}

fn seed_digest(turns: &[DelegatedContextTurn]) -> Sha256Digest {
    Sha256Digest::of_bytes(&serde_json::to_vec(turns).expect("a seed's turns serialize"))
}

#[cfg(test)]
mod tests;
