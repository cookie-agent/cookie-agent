//! Durable goal and producer-message projection.

use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

use cookie_agent_protocol::{
    EventPayload, GoalId, GoalReminderIdentity, GoalReminderKind, GoalState, GoalStatus,
    ProducerDeliveryMode, ProducerIdempotencyKey, ProducerMessageId, ProducerOwner, RunId,
    RunSelection, SafeDisplayText, StoredEvent,
};

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProducerMessageRecord {
    pub message_id: ProducerMessageId,
    pub producer_owner: ProducerOwner,
    pub mode: ProducerDeliveryMode,
    pub idempotency_key: ProducerIdempotencyKey,
    pub description: SafeDisplayText,
    pub body: cookie_agent_protocol::UserContent,
    pub reminder: Option<GoalReminderIdentity>,
    /// `send_message` hop depth stamped by the guard; present exactly on an
    /// Agent producer's message.
    pub agent_hop: Option<u32>,
    pub accepted_seq: u64,
    pub admission: Option<(RunId, u64)>,
    #[serde(with = "crate::events::sorted::set")]
    pub claims: HashSet<u64>,
    pub consumed: bool,
    pub discarded: bool,
    pub discarded_seq: Option<u64>,
    pub consumption_recorded: bool,
    pub consumed_run: Option<RunId>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProducerClaimRecord {
    pub run_id: RunId,
    pub message_ids: Vec<ProducerMessageId>,
}

/// Whether a run is offered the goal tools: every root run, so its agent can
/// start a goal itself (stable prefix spec, section 3), except an ACP
/// agent's, which sees them only while the user's goal is active or paused.
pub fn goal_tools_offered<E: Borrow<StoredEvent>>(root: bool, acp: bool, events: &[E]) -> bool {
    root && (!acp || GoalProducerProjection::from_events(events).goal_is_open())
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GoalProducerProjection {
    pub goal: Option<GoalState>,
    pub selection: Option<RunSelection>,
    pub messages: Vec<ProducerMessageRecord>,
    #[serde(with = "crate::events::sorted::map")]
    pub claims: HashMap<u64, ProducerClaimRecord>,
    pub invalid: Vec<(u64, String)>,
    /// What is left of the messages a log's own fold let go of.
    pub settled: SettledMessages,
}

/// What a log's producer fold keeps of a message that settled, once it lets go of
/// the message (`docs/site/specs/forever-sessions.md` section 12): a message
/// consumed and recorded, or discarded, whose run ended, with no claim on it.
/// Nothing names such a message again but a producer's retry of its key, so this
/// keeps that, and whether a goal's reminder was consumed. Both are kept for as
/// long as the log, each 8 bytes and an ID.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SettledMessages {
    /// By the digest of each message's owner and idempotency key, its ID.
    #[serde(with = "crate::events::sorted::map")]
    keys: HashMap<String, ProducerMessageId>,
    /// The goals a consumed reminder of continued.
    #[serde(with = "crate::events::sorted::set")]
    continued_goals: HashSet<GoalId>,
}

/// The digest [`SettledMessages`] keeps a message's key as.
fn key_digest(owner: &ProducerOwner, key: &ProducerIdempotencyKey) -> String {
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    digest.update(serde_json::to_vec(owner).unwrap_or_default());
    digest.update([0]);
    digest.update(key.as_str().as_bytes());
    digest.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl GoalProducerProjection {
    /// Whether the user's goal is active or paused.
    #[must_use]
    pub fn goal_is_open(&self) -> bool {
        self.goal
            .as_ref()
            .is_some_and(|goal| matches!(goal.status, GoalStatus::Active | GoalStatus::Paused))
    }

    pub fn next_reminder_kind(&self, goal_id: GoalId) -> GoalReminderKind {
        // Acceptance and claims are retractable; only committed input coverage introduces a goal.
        if self.settled.continued_goals.contains(&goal_id)
            || self.messages.iter().any(|message| {
                message.consumed
                    && message
                        .reminder
                        .is_some_and(|reminder| reminder.goal_id == goal_id)
            })
        {
            GoalReminderKind::Continuation
        } else {
            GoalReminderKind::Started
        }
    }

    /// The message accepted from `owner` under `key`, whether this holds it or let
    /// go of it as settled: a producer's retry of the key is answered with it.
    #[must_use]
    pub fn accepted(
        &self,
        owner: &ProducerOwner,
        key: &ProducerIdempotencyKey,
    ) -> Option<ProducerMessageId> {
        self.messages
            .iter()
            .find(|message| &message.producer_owner == owner && &message.idempotency_key == key)
            .map(|message| message.message_id)
            .or_else(|| self.settled.keys.get(&key_digest(owner, key)).copied())
    }

    /// Hop depth a `send_message` from `run` must carry: one more than the
    /// deepest Agent-owned mail this session's log shows `run` has seen, or `0`
    /// when the run has seen no agent mail.
    ///
    /// The basis is durable observation — admission, a live claim, or committed
    /// consumption — not payload heuristics, so a chain cannot be reset by
    /// releasing a claim or by restarting the process. A message with no hop
    /// metadata is a pre-guard send and counts as depth `0`.
    ///
    /// This over-approximates on purpose: any send from a run that consumed
    /// hop-`N` mail inherits `N + 1`, even when the model treats the send as an
    /// unrelated topic. The guard's bound, not per-message linkage, is the
    /// control operators tune.
    #[must_use]
    pub fn inherited_agent_hop(&self, run: RunId) -> u32 {
        self.messages
            .iter()
            .filter(|message| {
                matches!(message.producer_owner, ProducerOwner::Agent { .. })
                    && (message.consumed_run == Some(run)
                        || message
                            .admission
                            .is_some_and(|(admitted_run, _)| admitted_run == run)
                        || self.claims.values().any(|claim| {
                            claim.run_id == run && claim.message_ids.contains(&message.message_id)
                        }))
            })
            .filter_map(|message| message.agent_hop)
            .max()
            .map_or(0, |deepest| deepest.saturating_add(1))
    }

    #[must_use]
    pub fn from_events<E: Borrow<StoredEvent>>(events: &[E]) -> Self {
        let mut fold = GoalProducerFold::default();
        for event in events {
            fold.observe(event.borrow());
        }
        Arc::unwrap_or_clone(fold.projection)
    }

    /// Test-only: whether this, a log's own fold, which let go of messages that
    /// settled, agrees with `whole`, the same log folded keeping every message:
    /// what it holds is `whole`'s, in order; what it let go of had settled, and
    /// its key answers with its ID; and the rest is the same.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn agrees_with_whole(&self, whole: &Self) -> bool {
        let held = self
            .messages
            .iter()
            .map(|message| message.message_id)
            .collect::<HashSet<_>>();
        let let_go_settled = whole
            .messages
            .iter()
            .filter(|message| !held.contains(&message.message_id))
            .all(|message| {
                message.claims.is_empty()
                    && ((message.consumed && message.consumption_recorded) || message.discarded)
                    && self.accepted(&message.producer_owner, &message.idempotency_key)
                        == Some(message.message_id)
            });
        let continued = whole
            .messages
            .iter()
            .filter(|message| message.consumed)
            .filter_map(|message| message.reminder.map(|reminder| reminder.goal_id))
            .collect::<HashSet<_>>();
        self.goal == whole.goal
            && self.selection == whole.selection
            && self.claims == whole.claims
            && self.invalid == whole.invalid
            && whole
                .messages
                .iter()
                .filter(|message| held.contains(&message.message_id))
                .eq(self.messages.iter())
            && let_go_settled
            && self.settled.continued_goals == continued
    }

    fn reject(&mut self, seq: u64, reason: &str) {
        self.invalid.push((seq, reason.to_owned()));
    }
}

/// [`GoalProducerProjection`] folded one event at a time, so a log that
/// grew is folded only through what it gained.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct GoalProducerFold {
    /// Shared with the readers handed it; copied only when an event changes
    /// it while one still holds it.
    projection: Arc<GoalProducerProjection>,
    goals: GoalFold,
    #[serde(with = "crate::events::sorted::map")]
    message_indexes: HashMap<ProducerMessageId, usize>,
    #[serde(with = "crate::events::sorted::map")]
    dedup_indexes: HashMap<(ProducerOwner, ProducerIdempotencyKey), usize>,
    #[serde(with = "crate::events::sorted::set")]
    terminal_runs: HashSet<RunId>,
    /// The messages accepted and neither admitted nor discarded yet.
    #[serde(with = "crate::events::sorted::set")]
    unadmitted: HashSet<usize>,
    /// The runs that ended, oldest first, of those `terminal_runs` keeps: the
    /// newest [`SETTLED_KEPT`], and any a message it holds names.
    terminal_order: VecDeque<RunId>,
}

/// How many settled messages, and ended runs, a log's own fold keeps beyond what
/// it must: what settles is read back for a moment, as a cron job reads where its
/// message landed once that run ended.
const SETTLED_KEPT: usize = 16;

impl GoalProducerFold {
    /// The projection through the events observed so far.
    #[must_use]
    pub fn projection(&self) -> Arc<GoalProducerProjection> {
        Arc::clone(&self.projection)
    }

    /// The message `id`.
    #[must_use]
    pub fn message(&self, id: &ProducerMessageId) -> Option<&ProducerMessageRecord> {
        self.message_indexes
            .get(id)
            .map(|index| &self.projection.messages[*index])
    }

    /// Whether the user's goal is active or paused.
    #[must_use]
    pub fn goal_is_open(&self) -> bool {
        self.projection.goal_is_open()
    }

    /// Whether a message waits that no run admitted yet.
    #[must_use]
    pub fn has_unadmitted(&self) -> bool {
        !self.unadmitted.is_empty()
    }

    fn projection_mut(&mut self) -> &mut GoalProducerProjection {
        Arc::make_mut(&mut self.projection)
    }

    /// Folds the next event of the log.
    pub fn observe(&mut self, event: &StoredEvent) {
        match &event.payload {
            EventPayload::GoalActivated { selection, .. } => match self.goals.observe(event) {
                Ok(()) => {
                    let goal = self.goals.goal.clone();
                    let projection = self.projection_mut();
                    projection.selection = selection.clone();
                    projection.goal = goal;
                }
                Err(reason) => self.projection_mut().reject(event.seq, reason),
            },
            EventPayload::GoalChecklistRevised { .. } => match self.goals.observe(event) {
                Ok(()) => self.projection_mut().goal = self.goals.goal.clone(),
                Err(reason) => self.projection_mut().reject(event.seq, reason),
            },
            EventPayload::GoalLifecycleChanged { selection, .. } => {
                match self.goals.observe(event) {
                    Ok(()) => {
                        let goal = self.goals.goal.clone();
                        let projection = self.projection_mut();
                        if let Some(selection) = selection {
                            projection.selection = Some(selection.clone());
                        }
                        projection.goal = goal;
                    }
                    Err(reason) => self.projection_mut().reject(event.seq, reason),
                }
            }
            EventPayload::ProducerMessageAccepted {
                message_id,
                producer_owner,
                mode,
                idempotency_key,
                description,
                body,
                reminder,
                agent_hop,
                ..
            } => {
                if !valid_reminder_owner(producer_owner, reminder.as_ref()) {
                    self.projection_mut()
                        .reject(event.seq, "invalid producer reminder ownership");
                    return;
                }
                if !valid_hop_owner(producer_owner, *agent_hop) {
                    self.projection_mut()
                        .reject(event.seq, "invalid producer hop ownership");
                    return;
                }
                // A retry of a key whose message settled and went: as an exact
                // retry of one held, it changes nothing.
                if !self.projection.settled.keys.is_empty()
                    && self
                        .projection
                        .settled
                        .keys
                        .contains_key(&key_digest(producer_owner, idempotency_key))
                {
                    return;
                }
                let dedup_key = (producer_owner.clone(), idempotency_key.clone());
                if let Some(index) = self.dedup_indexes.get(&dedup_key).copied() {
                    let prior = &self.projection.messages[index];
                    // `agent_hop` is guard metadata, not payload: a replayed
                    // acceptance for the same key stays idempotent even when
                    // the stamped depth differs from the stored record.
                    let exact = prior.message_id == *message_id
                        && prior.mode == *mode
                        && prior.description == *description
                        && prior.body == *body
                        && prior.reminder == *reminder;
                    if exact {
                        return;
                    }
                    self.projection_mut()
                        .reject(event.seq, "conflicting producer idempotency key");
                    return;
                }
                if self.message_indexes.contains_key(message_id) {
                    self.projection_mut()
                        .reject(event.seq, "conflicting producer message id");
                    return;
                }
                let index = self.projection.messages.len();
                self.projection_mut().messages.push(ProducerMessageRecord {
                    message_id: *message_id,
                    producer_owner: producer_owner.clone(),
                    mode: *mode,
                    idempotency_key: idempotency_key.clone(),
                    description: description.clone(),
                    body: body.clone(),
                    reminder: *reminder,
                    agent_hop: *agent_hop,
                    accepted_seq: event.seq,
                    admission: None,
                    claims: HashSet::new(),
                    consumed: false,
                    discarded: false,
                    discarded_seq: None,
                    consumption_recorded: false,
                    consumed_run: None,
                });
                self.message_indexes.insert(*message_id, index);
                self.dedup_indexes.insert(dedup_key, index);
                self.unadmitted.insert(index);
            }
            EventPayload::ProducerMessageAdmitted { message_id } => {
                let Some(run_id) = event.run_id else {
                    self.projection_mut()
                        .reject(event.seq, "producer admission is missing run id");
                    return;
                };
                if self.terminal_runs.contains(&run_id) {
                    self.projection_mut()
                        .reject(event.seq, "producer admission targets a terminal run");
                    return;
                }
                let Some(index) = self.message_indexes.get(message_id).copied() else {
                    self.projection_mut()
                        .reject(event.seq, "producer admission has no accepted message");
                    return;
                };
                let message = &self.projection.messages[index];
                let replaceable = message.admission.is_some_and(|(prior_run, _)| {
                    prior_run != run_id && self.terminal_runs.contains(&prior_run)
                });
                if message.consumed
                    || message.discarded_seq.is_some()
                    || (message.admission.is_some() && !replaceable)
                {
                    self.projection_mut()
                        .reject(event.seq, "invalid producer admission");
                    return;
                }
                self.projection_mut().messages[index].admission = Some((run_id, event.seq));
                self.unadmitted.remove(&index);
            }
            EventPayload::ProducerMessagesClaimed { message_ids } => {
                let Some(run_id) = event.run_id else {
                    self.projection_mut()
                        .reject(event.seq, "producer claim is missing run id");
                    return;
                };
                let unique = message_ids.iter().copied().collect::<HashSet<_>>();
                let indexes = message_ids
                    .iter()
                    .filter_map(|message_id| self.message_indexes.get(message_id).copied())
                    .collect::<Vec<_>>();
                let valid = !message_ids.is_empty()
                    && unique.len() == message_ids.len()
                    && indexes.len() == message_ids.len()
                    && !self.projection.claims.contains_key(&event.seq)
                    && indexes.iter().all(|index| {
                        let message = &self.projection.messages[*index];
                        message.accepted_seq < event.seq
                            && !message.consumed
                            && message.discarded_seq.is_none()
                            && message
                                .admission
                                .is_some_and(|(admission_run, admission_seq)| {
                                    admission_run == run_id && admission_seq < event.seq
                                })
                    });
                if !valid {
                    self.projection_mut()
                        .reject(event.seq, "invalid producer claim");
                    return;
                }
                for index in indexes {
                    self.projection_mut().messages[index]
                        .claims
                        .insert(event.seq);
                }
                self.projection_mut().claims.insert(
                    event.seq,
                    ProducerClaimRecord {
                        run_id,
                        message_ids: message_ids.clone(),
                    },
                );
            }
            EventPayload::ProducerMessagesReleased { claim_seq } => {
                let Some(run_id) = event.run_id else {
                    self.projection_mut()
                        .reject(event.seq, "producer release is missing run id");
                    return;
                };
                let Some(claim) = self.projection.claims.get(claim_seq) else {
                    self.projection_mut()
                        .reject(event.seq, "producer release has no claim");
                    return;
                };
                if *claim_seq == 0 || claim.run_id != run_id {
                    self.projection_mut()
                        .reject(event.seq, "producer release ownership is invalid");
                    return;
                }
                let message_ids = claim.message_ids.clone();
                for message_id in &message_ids {
                    if let Some(index) = self.message_indexes.get(message_id).copied() {
                        self.projection_mut().messages[index]
                            .claims
                            .remove(claim_seq);
                    }
                }
                self.projection_mut().claims.remove(claim_seq);
            }
            EventPayload::ModelTurnCommitted {
                input_through_seq, ..
            } => {
                let Some(run_id) = event.run_id else { return };
                let covered = |message: &ProducerMessageRecord| {
                    !message.consumed
                        && message
                            .admission
                            .is_some_and(|(admission_run, admission_seq)| {
                                admission_run == run_id && admission_seq <= *input_through_seq
                            })
                        && message
                            .discarded_seq
                            .is_none_or(|discarded_seq| discarded_seq > *input_through_seq)
                };
                // Most turns consume nothing; they leave a shared projection
                // unshared.
                if !self.projection.messages.iter().any(covered) {
                    return;
                }
                let projection = self.projection_mut();
                for message in &mut projection.messages {
                    if covered(message) {
                        message.discarded = false;
                        message.discarded_seq = None;
                        message.consumed = true;
                        message.consumed_run = Some(run_id);
                        if let Some(reminder) = message.reminder {
                            projection.settled.continued_goals.insert(reminder.goal_id);
                        }
                    }
                }
            }
            EventPayload::ProducerMessageConsumed { message_id, run_id } => {
                let Some(index) = self.message_indexes.get(message_id).copied() else {
                    self.projection_mut()
                        .reject(event.seq, "producer consumption has no accepted message");
                    return;
                };
                let message = &self.projection.messages[index];
                if event.run_id != Some(*run_id)
                    || message.admission.map(|(run, _)| run) != Some(*run_id)
                    || message.consumed_run != Some(*run_id)
                    || message.consumption_recorded
                {
                    self.projection_mut()
                        .reject(event.seq, "invalid producer consumption marker");
                    return;
                }
                self.projection_mut().messages[index].consumption_recorded = true;
            }
            EventPayload::ProducerMessageDiscarded {
                message_id,
                reminder,
                producer_owner,
            } => {
                let Some(index) = self.message_indexes.get(message_id).copied() else {
                    self.projection_mut()
                        .reject(event.seq, "producer discard has no accepted message");
                    return;
                };
                let message = &self.projection.messages[index];
                let identity_matches = producer_owner == &message.producer_owner
                    && reminder
                        .as_ref()
                        .is_none_or(|identity| message.reminder.as_ref() == Some(identity));
                if !identity_matches || message.consumed || !message.claims.is_empty() {
                    self.projection_mut()
                        .reject(event.seq, "invalid producer message discard");
                    return;
                }
                if message.discarded_seq.is_none() {
                    self.projection_mut().messages[index].discarded = true;
                    self.projection_mut().messages[index].discarded_seq = Some(event.seq);
                    self.unadmitted.remove(&index);
                }
            }
            EventPayload::RunFinished { .. } => {
                if let Some(run_id) = event.run_id
                    && self.terminal_runs.insert(run_id)
                {
                    self.terminal_order.push_back(run_id);
                }
            }
            _ => {}
        }
    }

    /// [`Self::observe`] for a log's own fold, which lets go of what settled, at
    /// each committed turn and run's end, so it holds what is open and not what
    /// the log grew to (`docs/site/specs/forever-sessions.md` section 6). A fold
    /// for a model's history ([`GoalProducerProjection::from_events`]) keeps every
    /// message, whose bodies are the history.
    pub fn observe_letting_go(&mut self, event: &StoredEvent) {
        self.observe(event);
        if matches!(
            event.payload,
            EventPayload::RunFinished { .. } | EventPayload::ModelTurnCommitted { .. }
        ) {
            self.let_go_settled();
        }
    }

    /// Lets go of the messages that settled, but for the newest [`SETTLED_KEPT`]:
    /// those consumed and recorded, or discarded, with no claim, whose run ended,
    /// so no record can change them. Their keys stay, as a digest, and so does
    /// whether one was a goal's consumed reminder. Then of the ended runs, those
    /// no message held names, but for the newest [`SETTLED_KEPT`]: a message is
    /// admitted to the run that starts, so only a held message asks of an older one.
    fn let_go_settled(&mut self) {
        let ended = |run: &RunId| self.terminal_runs.contains(run);
        let mut settled = self
            .projection
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                message.claims.is_empty()
                    && ((message.consumed
                        && message.consumption_recorded
                        && message.consumed_run.as_ref().is_none_or(ended))
                        || (message.discarded
                            && message.admission.as_ref().is_none_or(|(run, _)| ended(run))))
            })
            .map(|(index, message)| (message.accepted_seq, index))
            .collect::<Vec<_>>();
        if settled.len() > SETTLED_KEPT {
            settled.sort_unstable();
            settled.truncate(settled.len() - SETTLED_KEPT);
            let gone = settled
                .into_iter()
                .map(|(_, index)| index)
                .collect::<HashSet<_>>();
            let projection = self.projection_mut();
            let mut index = 0;
            let mut keys = Vec::new();
            projection.messages.retain(|message| {
                let kept = !gone.contains(&index);
                if !kept {
                    keys.push((
                        key_digest(&message.producer_owner, &message.idempotency_key),
                        message.message_id,
                    ));
                }
                index += 1;
                kept
            });
            projection.settled.keys.extend(keys);
            self.message_indexes.clear();
            self.dedup_indexes.clear();
            self.unadmitted.clear();
            for (index, message) in self.projection.messages.iter().enumerate() {
                self.message_indexes.insert(message.message_id, index);
                self.dedup_indexes.insert(
                    (
                        message.producer_owner.clone(),
                        message.idempotency_key.clone(),
                    ),
                    index,
                );
                if message.admission.is_none() && message.discarded_seq.is_none() {
                    self.unadmitted.insert(index);
                }
            }
        }
        if self.terminal_order.len() > SETTLED_KEPT {
            let named = self
                .projection
                .messages
                .iter()
                .flat_map(|message| {
                    message
                        .admission
                        .map(|(run, _)| run)
                        .into_iter()
                        .chain(message.consumed_run)
                })
                .collect::<HashSet<_>>();
            let beyond = self.terminal_order.len() - SETTLED_KEPT;
            let mut kept = VecDeque::with_capacity(self.terminal_order.len());
            for (position, run) in self.terminal_order.drain(..).enumerate() {
                if position >= beyond || named.contains(&run) {
                    kept.push_back(run);
                } else {
                    self.terminal_runs.remove(&run);
                }
            }
            self.terminal_order = kept;
        }
    }
}

/// The goal part of a session's log: its current goal, after the
/// activations, checklist revisions, and lifecycle changes that were valid
/// where they stand. Shared by the producer projection and the session
/// projection, which reports an active goal in `SessionMeta::waiting_on`.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GoalFold {
    pub goal: Option<GoalState>,
    /// The latest revision of every goal the log activated.
    #[serde(with = "crate::events::sorted::map")]
    revisions: HashMap<GoalId, u64>,
}

impl GoalFold {
    /// Applies `event` when it is a goal event, or says why it is invalid
    /// and leaves the fold unchanged. Any other event is ignored.
    pub fn observe(&mut self, event: &StoredEvent) -> Result<(), &'static str> {
        match &event.payload {
            EventPayload::GoalActivated {
                goal_id,
                objective,
                revision,
                ..
            } => {
                let replaceable = self
                    .goal
                    .as_ref()
                    .is_none_or(|goal| is_terminal(goal.status));
                let distinct = self
                    .goal
                    .as_ref()
                    .is_none_or(|goal| goal.goal_id != *goal_id);
                let unseen = !self.revisions.contains_key(goal_id);
                if objective.trim().is_empty() || !replaceable || !distinct || !unseen {
                    return Err("invalid goal activation");
                }
                self.goal = Some(GoalState {
                    goal_id: *goal_id,
                    objective: objective.clone(),
                    status: GoalStatus::Active,
                    items: Vec::new(),
                    revision: *revision,
                });
                self.revisions.insert(*goal_id, *revision);
            }
            EventPayload::GoalChecklistRevised {
                goal_id,
                items,
                revision,
            } => {
                let valid_items = items.iter().all(|item| !item.description.trim().is_empty());
                let valid = valid_items
                    && self.goal.as_ref().is_some_and(|goal| {
                        goal.goal_id == *goal_id
                            && !is_terminal(goal.status)
                            && *revision > goal.revision
                    });
                if !valid {
                    return Err("invalid goal checklist revision");
                }
                let goal = self.goal.as_mut().expect("validated current goal");
                goal.items = items.clone();
                goal.revision = *revision;
                self.revisions.insert(*goal_id, *revision);
            }
            EventPayload::GoalLifecycleChanged {
                goal_id,
                status,
                revision,
                selection,
            } => {
                let valid = self.goal.as_ref().is_some_and(|goal| {
                    goal.goal_id == *goal_id
                        && *revision > goal.revision
                        && valid_lifecycle_change(goal, *status)
                        && (selection.is_none() || *status == GoalStatus::Active)
                });
                if !valid {
                    return Err("invalid goal lifecycle transition");
                }
                let goal = self.goal.as_mut().expect("validated current goal");
                goal.status = *status;
                goal.revision = *revision;
                self.revisions.insert(*goal_id, *revision);
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether the current goal is active, so goal mode keeps scheduling
    /// runs. A paused, completed, or cancelled goal schedules none.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.goal
            .as_ref()
            .is_some_and(|goal| goal.status == GoalStatus::Active)
    }
}

fn is_terminal(status: GoalStatus) -> bool {
    matches!(status, GoalStatus::Completed | GoalStatus::Cancelled)
}

fn valid_lifecycle_change(goal: &GoalState, status: GoalStatus) -> bool {
    match (goal.status, status) {
        (GoalStatus::Active, GoalStatus::Paused | GoalStatus::Cancelled)
        | (GoalStatus::Paused, GoalStatus::Active | GoalStatus::Cancelled) => true,
        (GoalStatus::Active | GoalStatus::Paused, GoalStatus::Completed) => {
            !goal.items.is_empty() && goal.items.iter().all(|item| item.finished)
        }
        _ => false,
    }
}

fn valid_reminder_owner(owner: &ProducerOwner, reminder: Option<&GoalReminderIdentity>) -> bool {
    match (owner, reminder) {
        (ProducerOwner::Goal { goal_id }, Some(reminder)) => *goal_id == reminder.goal_id,
        (ProducerOwner::Goal { .. }, None) | (_, Some(_)) => false,
        (_, None) => true,
    }
}

/// Hop metadata belongs exclusively to agent mail. A malformed durable event
/// that stamps it onto plugin, goal, or delegation mail is quarantined so it
/// cannot inflate an agent chain it never joined.
const fn valid_hop_owner(owner: &ProducerOwner, agent_hop: Option<u32>) -> bool {
    agent_hop.is_some() == matches!(owner, ProducerOwner::Agent { .. })
}

/// Whether a log's producer state needs reconciliation after a restart: an active goal, an unread message, or a message consumed without its
/// consumption record.
pub fn producer_state_pending(projection: &GoalProducerProjection) -> bool {
    projection.goal.is_some()
        || projection
            .messages
            .iter()
            .any(|message| pending(message) || (message.consumed && !message.consumption_recorded))
}

/// Whether a message is accepted but neither consumed nor discarded: mail
/// addressed to the session that it still owes a wake.
///
/// This is the race-free "a wake is owed" signal: `ProducerMessageAccepted` is
/// durable before a send reports success, and a message stays pending through
/// the whole window before its consumer's `RunStarted` flips session status to
/// `Running` (it clears only when a committed model turn has actually consumed
/// it). Liveness checks therefore cannot miss a steered finished child in the
/// gap between acceptance and run admission.
pub fn pending(message: &ProducerMessageRecord) -> bool {
    !message.consumed && !message.discarded
}

#[cfg(test)]
mod tests;
