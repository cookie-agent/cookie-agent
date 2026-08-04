//! Disposable UI projections reduced from protocol v7 stored events.
//!
//! Assistant attribution is derived only from the frozen `RunStarted` plus
//! `ModelAttemptStarted`/`ModelTurnCommitted` ownership — never from the
//! current picker, live agent files, or provider configuration. The visible
//! assistant header projects the exact canonical `Agent(Model[variant])`.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::{
    AgentId, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation, ApprovalFinalOutcome,
    ApprovalId, ApprovalRecord, ApprovalRequest, ApprovalStatus, ApprovalTrigger,
    AssistantToolCallRef, AttemptId, EventPayload, EventSubscriptionMessage, ModelErrorSummary,
    OperationFingerprint, OutputDelta, OutputGap, OutputSnapshotEnvelope, OutputStream,
    PersistedModelTurn, PreparedApprovalResource, PreparedCapabilityLifetime, ReplayDecision,
    ReplayDisposition, ResolvedModelRef, RunId, SessionId, SessionTitleChange, Sha256Digest,
    StoredEvent, ToolAttachment, ToolCallId, ToolTerminationOutcome, Usage,
};
use serde::Serialize;

use crate::{client::ClientDelivery, markdown::MarkdownDocument};

/// The visible state of a tool invocation, reduced from the exact v7
/// termination outcome. Failed, cancelled, and interrupted stay distinct.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

/// A tool invocation displayed inside its owning assistant item. The compact
/// title uses only the persisted, sanitized `ToolCallPresentation`; raw
/// arguments appear only in the expanded detail.
#[derive(Clone, Debug)]
pub struct ToolCallState {
    pub id: ToolCallId,
    pub owner: AssistantToolCallRef,
    pub presentation: cookie_agent_protocol::ToolCallPresentation,
    /// Durable tool input from the owning committed turn, shown expanded.
    pub arguments: String,
    pub status: ToolStatus,
    pub detail: String,
}

impl ToolCallState {
    /// The exact compact title: the persisted sanitized tool title plus its
    /// persisted sanitized primary argument, never reparsed from raw input.
    pub fn compact_title(&self) -> String {
        match &self.presentation.primary_argument {
            Some(argument) => format!("{} {argument}", self.presentation.title),
            None => self.presentation.title.to_string(),
        }
    }
}

/// One durable approval request projected for internal evaluation and, only
/// after escalation, possible user interaction.
#[derive(Clone, Debug)]
pub struct ApprovalState {
    pub session_id: SessionId,
    pub approval_id: ApprovalId,
    pub request_revision: u64,
    pub operation_fingerprint: OperationFingerprint,
    pub trigger: ApprovalTrigger,
    pub normalized_arguments_digest: Sha256Digest,
    pub execution_context_digest: Sha256Digest,
    pub capability_lifetime: PreparedCapabilityLifetime,
    pub capabilities: Vec<ApprovalCapability>,
    pub resources: Vec<PreparedApprovalResource>,
    pub evaluations: Vec<ApprovalEvaluation>,
    pub constraints: ApprovalConstraints,
    pub escalated: bool,
}

impl ApprovalState {
    /// User-visible/respondable approvals must have a durable escalation and
    /// must still be within their response lifetime.
    pub(crate) fn is_visible_user_escalation(&self) -> bool {
        self.escalated
            && self
                .constraints
                .expires_at
                .is_none_or(|expires_at| expires_at > jiff::Timestamp::now())
    }
}

/// Leveled diagnostic severity for TUI-only event rows. This is a display
/// projection classification; durable protocol events are unchanged.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EventLevel {
    Debug,
    Info,
    Warning,
    Error,
}

impl EventLevel {
    pub fn badge(self) -> &'static str {
        match self {
            Self::Debug => "[D]",
            Self::Info => "[I]",
            Self::Warning => "[W]",
            Self::Error => "[E]",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// Frozen producing identity for one assistant attempt/turn, reduced from the
/// exact v7 attempt and turn ownership events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenAssistantAttribution {
    pub agent: AgentId,
    pub resolved_model: ResolvedModelRef,
}

impl FrozenAssistantAttribution {
    /// The exact visible header `<agent-id>(<provider>/<model-id>[<variant>])`.
    pub fn header(&self) -> String {
        format!(
            "{}({}[{}])",
            self.agent,
            self.resolved_model.selection.model,
            self.variant_label()
        )
    }

    /// The variant retained in structured attribution, rendered as `base`
    /// when the frozen selection is exact base behavior.
    pub fn variant_label(&self) -> String {
        self.resolved_model
            .selection
            .variant
            .as_ref()
            .map_or_else(|| "base".to_owned(), |variant| variant.to_string())
    }
}

/// One rendered conversation item.
#[derive(Clone, Debug)]
pub enum TranscriptItem {
    User {
        id: u64,
        version: u64,
        text: String,
    },
    Assistant {
        id: u64,
        version: u64,
        attribution: FrozenAssistantAttribution,
        committed_turn_seq: Option<u64>,
        children: Vec<AssistantChild>,
    },
    /// A leveled diagnostic row (lifecycle notices, model warnings,
    /// failures). Never user/assistant/tool content; filtering these rows by
    /// level cannot hide conversation content or approvals.
    Event {
        id: u64,
        version: u64,
        level: EventLevel,
        text: String,
    },
}

/// One ordered child segment inside an assistant item, owned by the committed
/// turn/tool ownership events. There are no top-level reasoning or tool items.
#[derive(Clone, Debug)]
pub enum AssistantChild {
    Text {
        /// Sequence number of the first delta in this consecutive segment.
        id: u64,
        version: u64,
        markdown: MarkdownDocument,
    },
    Thinking {
        /// Sequence number of the first delta in this consecutive segment.
        id: u64,
        version: u64,
        text: String,
    },
    Tool {
        call_id: ToolCallId,
    },
    /// A durable tool placeholder from committed turn content, carrying the
    /// exact content index. A started tool replaces its placeholder through
    /// `owner.content_index`; an unstarted placeholder renders its committed
    /// call.
    CommittedTool {
        content_index: u32,
    },
}

impl AssistantChild {
    pub fn id(&self) -> u64 {
        match self {
            Self::Text { id, .. } | Self::Thinking { id, .. } => *id,
            Self::Tool { .. } | Self::CommittedTool { .. } => 0,
        }
    }

    pub fn version(&self) -> u64 {
        match self {
            Self::Text { version, .. } | Self::Thinking { version, .. } => *version,
            Self::Tool { .. } | Self::CommittedTool { .. } => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AssistantPartKind {
    Text,
    Thinking,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OpenAssistantProjection {
    item_id: u64,
    part_id: u64,
    kind: AssistantPartKind,
}

impl TranscriptItem {
    pub fn id(&self) -> u64 {
        match self {
            Self::User { id, .. } | Self::Assistant { id, .. } | Self::Event { id, .. } => *id,
        }
    }

    pub fn version(&self) -> u64 {
        match self {
            Self::User { version, .. }
            | Self::Assistant { version, .. }
            | Self::Event { version, .. } => *version,
        }
    }

    #[cfg(test)]
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            id: 1,
            version: 0,
            text: text.into(),
        }
    }

    #[cfg(test)]
    pub fn internal(text: impl Into<String>) -> Self {
        Self::Event {
            id: 1,
            version: 0,
            level: EventLevel::Info,
            text: text.into(),
        }
    }
}

/// One live streaming attempt, owning one assistant item.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AttemptProjection {
    item_id: u64,
}

/// A tool start buffered until its committed placeholder exists, linked by
/// the owning turn's exact content index.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingToolRow {
    turn_seq: u64,
    content_index: u32,
    call_id: ToolCallId,
}

/// Per-session projection of persisted events and live output.
#[derive(Clone, Debug, Default)]
pub struct SessionState {
    /// Changes whenever the visible projection mutates, for UI cache invalidation.
    pub version: u64,
    pub generation: u64,
    pub last_seq: u64,
    pub active_run: Option<RunId>,
    /// Frozen producing agent of the latest accepted `RunStarted`.
    pub run_agent: Option<AgentId>,
    /// The complete frozen creation snapshot from `SessionCreated`,
    /// including the exact frozen fallback chain. Delegated draft
    /// projections derive only from this, never from live descriptors.
    pub creation_agent: Option<Box<cookie_agent_protocol::AgentSnapshot>>,
    /// The complete frozen snapshot of the latest accepted `RunStarted`.
    pub run_snapshot: Option<Box<cookie_agent_protocol::AgentSnapshot>>,
    /// The authoritative exact suffix from the latest accepted `RunStarted`:
    /// after any run-selection variant override, this vector — never a
    /// reconstruction from the agent fallback chain — is what attempts and
    /// delegated pickers use.
    pub run_selected_suffix: Option<Vec<cookie_agent_protocol::FrozenModelBinding>>,
    pub transcript: Vec<TranscriptItem>,
    pub(crate) next_transcript_id: u64,
    pub(crate) open_assistant: Option<OpenAssistantProjection>,
    pub(crate) attempts: HashMap<AttemptId, AttemptProjection>,
    pub tools: HashMap<ToolCallId, ToolCallState>,
    /// Buffered tool rows awaiting their committed placeholder, keyed by
    /// the owning turn's content index so starts/completions cannot reorder.
    pub(crate) pending_tool_rows: Vec<PendingToolRow>,
    /// Durable tool input indexed from committed turn content:
    /// (model_turn_seq, model_call_id) → arguments JSON, for expanded rows.
    pub(crate) turn_tool_index: HashMap<(u64, String), String>,
    /// The assistant item owning each committed model-turn sequence.
    pub(crate) turn_items: HashMap<u64, u64>,
    pub approvals: Vec<ApprovalState>,
    pub output: HashMap<(ToolCallId, bool), OrderedOutput>,
}

impl SessionState {
    pub fn is_open_thinking(&self, item_id: u64, part_id: u64) -> bool {
        self.open_assistant.is_some_and(|open| {
            open.item_id == item_id
                && open.part_id == part_id
                && open.kind == AssistantPartKind::Thinking
        })
    }
}

/// All currently observed session projections.
#[derive(Clone, Debug, Default)]
pub struct StateStore {
    pub sessions: HashMap<SessionId, SessionState>,
    pending_output: HashMap<ToolCallId, Vec<PendingOutput>>,
    pending_output_order: VecDeque<ToolCallId>,
    lost_output: HashMap<ToolCallId, HashSet<bool>>,
    abandoned_output: HashMap<ToolCallId, SessionId>,
    tool_sessions: HashMap<ToolCallId, SessionId>,
    quarantined_sessions: HashSet<SessionId>,
    replays: HashMap<SessionId, ReplayProgress>,
}

#[derive(Clone, Debug)]
struct ReplayProgress {
    generation: u64,
    final_seq: u64,
    scratch: SessionState,
    deadline: Instant,
}

/// Result of reducing one item from the client's ordered delivery stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryOutcome {
    Applied,
    Gap { session_id: SessionId, cursor: u64 },
    ReplayFailed { session_id: SessionId },
}

const MAX_PENDING_OUTPUT_PER_CALL: usize = 128;
const MAX_PENDING_OUTPUT_CALLS: usize = 64;
const REPLAY_END_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
enum PendingOutput {
    Snapshot(OutputSnapshotEnvelope),
    Delta(OutputDelta),
    Gap(OutputGap),
}

impl StateStore {
    /// Reduce every delivery variant from the single client stream. Replay
    /// events reduce into a per-session scratch projection. The visible
    /// projection changes only through a validated `ReplayEnd` swap or live
    /// events after that replay's final sequence. Once a replay for a session
    /// is abandoned, that session's visible projection is immutable until a
    /// validated replacement replay ends; all of its output is quarantined.
    pub fn apply_delivery(&mut self, delivery: ClientDelivery) -> DeliveryOutcome {
        match delivery {
            ClientDelivery::Live {
                message,
                generation,
            } => match *message {
                EventSubscriptionMessage::Event { event } => {
                    let session_id = event.session_id;
                    if self.apply_event_for_generation(*event, generation) {
                        DeliveryOutcome::Applied
                    } else {
                        let cursor = self
                            .sessions
                            .get(&session_id)
                            .map_or(0, |state| state.last_seq);
                        DeliveryOutcome::Gap { session_id, cursor }
                    }
                }
                EventSubscriptionMessage::Gap {
                    session_id,
                    last_delivered_seq,
                } => DeliveryOutcome::Gap {
                    session_id,
                    cursor: last_delivered_seq,
                },
            },
            ClientDelivery::ReplayStart {
                session_id,
                generation,
                final_seq,
                rebuild,
            } => {
                let mut scratch = if rebuild {
                    SessionState {
                        generation,
                        ..SessionState::default()
                    }
                } else if let Some(state) = self.sessions.get(&session_id)
                    && state.generation == generation
                {
                    state.clone()
                } else {
                    self.quarantined_sessions.insert(session_id);
                    return DeliveryOutcome::ReplayFailed { session_id };
                };
                close_open_assistant(&mut scratch);
                self.replays.insert(
                    session_id,
                    ReplayProgress {
                        generation,
                        final_seq,
                        scratch,
                        deadline: Instant::now() + REPLAY_END_TIMEOUT,
                    },
                );
                DeliveryOutcome::Applied
            }
            ClientDelivery::ReplayEvent {
                session_id,
                generation,
                final_seq,
                event,
            } => {
                let event = *event;
                let started_call = match &event.payload {
                    EventPayload::ToolCallStarted { start } => Some(start.tool_call_id),
                    _ => None,
                };
                if let Some(call_id) = started_call {
                    self.tool_sessions.insert(call_id, session_id);
                }
                let valid = self.replays.get_mut(&session_id).is_some_and(|replay| {
                    if replay.generation != generation || replay.final_seq != final_seq {
                        return false;
                    }
                    if event.seq <= replay.scratch.last_seq {
                        return true;
                    }
                    if event.seq != replay.scratch.last_seq + 1 {
                        return false;
                    }
                    replay.scratch.last_seq = event.seq;
                    reduce_event(
                        &mut replay.scratch,
                        event.session_id,
                        event.run_id,
                        event.seq,
                        event.payload,
                    );
                    replay.scratch.version = replay.scratch.version.wrapping_add(1);
                    true
                });
                if !valid {
                    self.abandon_replay(session_id);
                    DeliveryOutcome::ReplayFailed { session_id }
                } else {
                    if let Some(call_id) = started_call {
                        self.flush_pending_output(call_id);
                    }
                    DeliveryOutcome::Applied
                }
            }
            ClientDelivery::ReplayEnd {
                session_id,
                generation,
                final_seq,
            } => {
                let replay = self.replays.remove(&session_id);
                let valid = replay.as_ref().is_some_and(|replay| {
                    replay.generation == generation
                        && replay.final_seq == final_seq
                        && replay.scratch.last_seq == final_seq
                });
                match replay {
                    Some(mut replay) if valid => {
                        self.quarantined_sessions.remove(&session_id);
                        self.abandoned_output
                            .retain(|_, output_session| *output_session != session_id);
                        for call_id in replay.scratch.tools.keys() {
                            self.abandoned_output.remove(call_id);
                        }
                        replay.scratch.version = self
                            .sessions
                            .get(&session_id)
                            .map(|previous| {
                                previous.version.max(replay.scratch.version).wrapping_add(1)
                            })
                            .unwrap_or(replay.scratch.version);
                        self.sessions.insert(session_id, replay.scratch);
                        DeliveryOutcome::Applied
                    }
                    Some(replay) => {
                        self.quarantine_replay_output(session_id, &replay);
                        DeliveryOutcome::ReplayFailed { session_id }
                    }
                    None => DeliveryOutcome::ReplayFailed { session_id },
                }
            }
            ClientDelivery::OutputSnapshot(snapshot) => {
                if let Some(session_id) = self.quarantined_output(snapshot.snapshot.call_id) {
                    return DeliveryOutcome::ReplayFailed { session_id };
                }
                self.apply_snapshot(snapshot);
                DeliveryOutcome::Applied
            }
            ClientDelivery::OutputDelta(delta) => {
                if let Some(session_id) = self.quarantined_output(delta.call_id) {
                    return DeliveryOutcome::ReplayFailed { session_id };
                }
                self.apply_output_delta(delta);
                DeliveryOutcome::Applied
            }
            ClientDelivery::OutputGap(gap) => {
                if let Some(session_id) = self.quarantined_output(gap.call_id) {
                    return DeliveryOutcome::ReplayFailed { session_id };
                }
                self.apply_output_gap(gap);
                DeliveryOutcome::Applied
            }
            ClientDelivery::RecoveryFailed { .. } => DeliveryOutcome::Applied,
        }
    }

    /// Discard incomplete scratch replays. Their currently visible session
    /// projections remain intact for a full recovery attempt.
    pub fn abandon_timed_out_replays(&mut self) -> Vec<SessionId> {
        let now = Instant::now();
        let expired = self
            .replays
            .iter()
            .filter_map(|(session_id, replay)| (replay.deadline <= now).then_some(*session_id))
            .collect::<Vec<_>>();
        for session_id in &expired {
            self.abandon_replay(*session_id);
        }
        expired
    }

    /// Discard all incomplete scratch replays after a connection closes.
    pub fn abandon_replays(&mut self) -> Vec<SessionId> {
        let sessions = self.replays.keys().copied().collect::<Vec<_>>();
        for session_id in &sessions {
            self.abandon_replay(*session_id);
        }
        sessions
    }

    fn quarantined_output(&self, call_id: ToolCallId) -> Option<SessionId> {
        if self
            .replays
            .values()
            .any(|replay| replay.scratch.tools.contains_key(&call_id))
        {
            return None;
        }
        self.tool_sessions
            .get(&call_id)
            .copied()
            .filter(|session_id| {
                self.quarantined_sessions.contains(session_id)
                    || self.replays.contains_key(session_id)
            })
            .or_else(|| self.abandoned_output.get(&call_id).copied())
    }

    fn abandon_replay(&mut self, session_id: SessionId) {
        self.quarantined_sessions.insert(session_id);
        if let Some(replay) = self.replays.remove(&session_id) {
            self.quarantine_replay_output(session_id, &replay);
        }
    }

    fn quarantine_replay_output(&mut self, session_id: SessionId, replay: &ReplayProgress) {
        for call_id in replay.scratch.tools.keys() {
            self.abandoned_output.insert(*call_id, session_id);
            self.pending_output.remove(call_id);
            self.pending_output_order
                .retain(|pending| pending != call_id);
        }
    }

    /// Apply a persisted event. Replayed duplicates are ignored by sequence.
    pub fn apply_event(&mut self, event: StoredEvent) -> bool {
        self.apply_event_for_generation(event, 0)
    }

    pub fn apply_event_for_generation(&mut self, event: StoredEvent, generation: u64) -> bool {
        let started_call = match &event.payload {
            EventPayload::ToolCallStarted { start } => Some(start.tool_call_id),
            _ => None,
        };
        if let Some(call_id) = started_call {
            self.tool_sessions.insert(call_id, event.session_id);
        }
        if self.quarantined_sessions.contains(&event.session_id) {
            return false;
        }
        let state = self.sessions.entry(event.session_id).or_default();
        if state.generation != generation {
            return false;
        }
        if event.seq <= state.last_seq {
            return true;
        }
        if event.seq != state.last_seq + 1 {
            return false;
        }
        state.last_seq = event.seq;
        reduce_event(
            state,
            event.session_id,
            event.run_id,
            event.seq,
            event.payload,
        );
        state.version = state.version.wrapping_add(1);
        if let Some(call_id) = started_call {
            self.flush_pending_output(call_id);
        }
        true
    }

    /// Apply a message from the event subscription stream. A gap is returned
    /// to allow callers to surface it; the client independently re-subscribes.
    pub fn apply_subscription(&mut self, message: EventSubscriptionMessage) -> Option<u64> {
        self.apply_subscription_for_generation(message, 0)
    }

    pub fn apply_subscription_for_generation(
        &mut self,
        message: EventSubscriptionMessage,
        generation: u64,
    ) -> Option<u64> {
        match message {
            EventSubscriptionMessage::Event { event } => {
                let cursor = self
                    .sessions
                    .get(&event.session_id)
                    .map_or(0, |state| state.last_seq);
                self.apply_event_for_generation(*event, generation)
                    .then_some(())
                    .map_or(Some(cursor), |_| None)
            }
            EventSubscriptionMessage::Gap {
                last_delivered_seq, ..
            } => Some(last_delivered_seq),
        }
    }

    /// Drop a session projection before a full cursor-zero rebuild.
    ///
    /// A visible projection is never reset while its replay scratch is active
    /// or its session is quarantined; callers receive `false` and must recover
    /// through the staged replay path instead.
    pub fn reset_session(&mut self, session_id: SessionId, generation: u64) -> bool {
        if self.quarantined_sessions.contains(&session_id) || self.replays.contains_key(&session_id)
        {
            return false;
        }
        self.sessions.insert(
            session_id,
            SessionState {
                generation,
                ..SessionState::default()
            },
        );
        true
    }

    /// Replace a session projection only after a complete contiguous replay is
    /// available. A failed/incomplete fetch leaves the existing projection intact.
    pub fn rebuild_session(
        &mut self,
        session_id: SessionId,
        generation: u64,
        events: Vec<StoredEvent>,
    ) -> bool {
        for (expected, event) in (1..).zip(&events) {
            if event.session_id != session_id || event.seq != expected {
                return false;
            }
        }
        if !self.reset_session(session_id, generation) {
            return false;
        }
        for event in events {
            if !self.apply_event_for_generation(event, generation) {
                return false;
            }
        }
        true
    }

    pub fn apply_snapshot(&mut self, envelope: OutputSnapshotEnvelope) {
        let call_id = envelope.snapshot.call_id;
        if self.quarantined_output(call_id).is_some() {
            return;
        }
        if !self.apply_snapshot_now(envelope.clone()) {
            self.buffer_output(call_id, PendingOutput::Snapshot(envelope));
        }
    }

    pub fn apply_output_delta(&mut self, delta: OutputDelta) {
        let call_id = delta.call_id;
        if self.quarantined_output(call_id).is_some() {
            return;
        }
        if !self.apply_delta_now(delta.clone()) {
            self.buffer_output(call_id, PendingOutput::Delta(delta));
        }
    }

    pub fn apply_output_gap(&mut self, gap: OutputGap) {
        let call_id = gap.call_id;
        if self.quarantined_output(call_id).is_some() {
            return;
        }
        if !self.apply_gap_now(gap.clone()) {
            self.buffer_output(call_id, PendingOutput::Gap(gap));
        }
    }

    fn apply_snapshot_now(&mut self, envelope: OutputSnapshotEnvelope) -> bool {
        let call_id = envelope.snapshot.call_id;
        let output = self
            .replays
            .values_mut()
            .find_map(|replay| {
                replay
                    .scratch
                    .tools
                    .contains_key(&envelope.snapshot.call_id)
                    .then_some(&mut replay.scratch)
            })
            .or_else(|| {
                self.sessions.values_mut().find_map(|state| {
                    state
                        .tools
                        .contains_key(&envelope.snapshot.call_id)
                        .then_some(state)
                })
            });
        if let Some(state) = output {
            state
                .output
                .entry((envelope.snapshot.call_id, stream_key(envelope.stream)))
                .or_default()
                .replace_snapshot(
                    envelope.snapshot.start_offset,
                    envelope.snapshot.end_offset,
                    envelope.snapshot.chunks,
                );
            bump_tool_item(state, call_id);
            state.version = state.version.wrapping_add(1);
            return true;
        }
        false
    }

    fn apply_delta_now(&mut self, delta: OutputDelta) -> bool {
        let call_id = delta.call_id;
        if let Some(state) = self
            .replays
            .values_mut()
            .find_map(|replay| {
                replay
                    .scratch
                    .tools
                    .contains_key(&delta.call_id)
                    .then_some(&mut replay.scratch)
            })
            .or_else(|| {
                self.sessions
                    .values_mut()
                    .find(|state| state.tools.contains_key(&delta.call_id))
            })
        {
            state
                .output
                .entry((delta.call_id, stream_key(delta.stream)))
                .or_default()
                .push(delta);
            bump_tool_item(state, call_id);
            state.version = state.version.wrapping_add(1);
            return true;
        }
        false
    }

    fn apply_gap_now(&mut self, gap: OutputGap) -> bool {
        let call_id = gap.call_id;
        if let Some(state) = self
            .replays
            .values_mut()
            .find_map(|replay| {
                replay
                    .scratch
                    .tools
                    .contains_key(&gap.call_id)
                    .then_some(&mut replay.scratch)
            })
            .or_else(|| {
                self.sessions
                    .values_mut()
                    .find(|state| state.tools.contains_key(&gap.call_id))
            })
        {
            state
                .output
                .entry((gap.call_id, stream_key(gap.stream)))
                .or_default()
                .mark_gap(gap.next_offset);
            bump_tool_item(state, call_id);
            state.version = state.version.wrapping_add(1);
            return true;
        }
        false
    }

    fn buffer_output(&mut self, call_id: ToolCallId, output: PendingOutput) {
        if !self.pending_output.contains_key(&call_id)
            && self.pending_output.len() == MAX_PENDING_OUTPUT_CALLS
            && let Some(oldest) = self.pending_output_order.pop_front()
            && let Some(dropped) = self.pending_output.remove(&oldest)
        {
            for output in dropped {
                self.record_lost_output(oldest, pending_stream(&output));
            }
        }
        if !self.pending_output.contains_key(&call_id) {
            self.pending_output_order.push_back(call_id);
        }
        let dropped = {
            let pending = self.pending_output.entry(call_id).or_default();
            let dropped = (pending.len() == MAX_PENDING_OUTPUT_PER_CALL).then(|| pending.remove(0));
            pending.push(output);
            dropped
        };
        if let Some(dropped) = dropped {
            self.record_lost_output(call_id, pending_stream(&dropped));
        }
    }

    fn flush_pending_output(&mut self, call_id: ToolCallId) {
        self.pending_output_order
            .retain(|pending_call| *pending_call != call_id);
        if let Some(pending) = self.pending_output.remove(&call_id) {
            for output in pending {
                match output {
                    PendingOutput::Snapshot(snapshot) => {
                        let _ = self.apply_snapshot_now(snapshot);
                    }
                    PendingOutput::Delta(delta) => {
                        let _ = self.apply_delta_now(delta);
                    }
                    PendingOutput::Gap(gap) => {
                        let _ = self.apply_gap_now(gap);
                    }
                }
            }
        }
        if let Some(streams) = self.lost_output.remove(&call_id) {
            for stream in streams {
                self.mark_output_lost(call_id, stream);
            }
        }
    }

    fn record_lost_output(&mut self, call_id: ToolCallId, stream: bool) {
        self.lost_output.entry(call_id).or_default().insert(stream);
    }

    fn mark_output_lost(&mut self, call_id: ToolCallId, stream: bool) {
        if self.quarantined_output(call_id).is_some() {
            return;
        }
        if let Some(state) = self
            .replays
            .values_mut()
            .find_map(|replay| {
                replay
                    .scratch
                    .tools
                    .contains_key(&call_id)
                    .then_some(&mut replay.scratch)
            })
            .or_else(|| {
                self.sessions
                    .values_mut()
                    .find(|state| state.tools.contains_key(&call_id))
            })
        {
            state
                .output
                .entry((call_id, stream))
                .or_default()
                .mark_gap(0);
            bump_tool_item(state, call_id);
        }
    }
}

fn reduce_event(
    state: &mut SessionState,
    session_id: SessionId,
    run_id: Option<RunId>,
    sequence: u64,
    payload: EventPayload,
) {
    match payload {
        EventPayload::RunStarted {
            agent,
            selected_suffix,
            ..
        } => {
            close_open_assistant(state);
            state.active_run = run_id;
            state.run_agent = Some(agent.agent.clone());
            state.run_snapshot = Some(agent);
            state.run_selected_suffix = Some(selected_suffix);
        }
        EventPayload::UserInputSubmitted { input } => {
            close_open_assistant(state);
            push_item(state, |id| TranscriptItem::User {
                id,
                version: 0,
                text: input,
            });
        }
        EventPayload::ModelAttemptStarted {
            attempt_id,
            resolved_model,
            ..
        } => {
            close_open_assistant(state);
            // Attempt attribution is frozen: the producing agent comes from
            // the owning `RunStarted`, the exact resolved model from this
            // event — never from the current picker or live configuration.
            let agent = state
                .run_agent
                .clone()
                .unwrap_or_else(|| AgentId::new("unknown").expect("static agent id"));
            let item_id = open_assistant_item(
                state,
                FrozenAssistantAttribution {
                    agent,
                    resolved_model,
                },
            );
            state
                .attempts
                .insert(attempt_id, AttemptProjection { item_id });
        }
        EventPayload::TextDelta { attempt_id, text } => {
            let Some(item_id) = state
                .attempts
                .get(&attempt_id)
                .map(|attempt| attempt.item_id)
            else {
                return;
            };
            append_assistant_delta(state, item_id, sequence, text, AssistantPartKind::Text);
        }
        EventPayload::ReasoningDelta { attempt_id, text } => {
            let Some(item_id) = state
                .attempts
                .get(&attempt_id)
                .map(|attempt| attempt.item_id)
            else {
                return;
            };
            append_assistant_delta(state, item_id, sequence, text, AssistantPartKind::Thinking);
        }
        EventPayload::AttemptAbandoned { attempt_id } => {
            close_open_assistant(state);
            state.attempts.remove(&attempt_id);
            push_event(state, EventLevel::Warning, "model attempt abandoned".into());
        }
        EventPayload::ModelTurnCommitted {
            attempt_id,
            model_turn_seq,
            resolved_model,
            turn,
            warnings,
            ..
        } => {
            close_open_assistant(state);
            // The committed turn is the canonical boundary: every
            // text/thinking/tool child is rebuilt in exact
            // `PersistedModelTurn.content` order, preserving multiple
            // segments and content indices. Tool parts become committed
            // placeholders linked by `owner.content_index` when their start
            // event arrives.
            if let Some(projection) = state.attempts.get(&attempt_id) {
                let item_id = projection.item_id;
                mark_committed(state, item_id, model_turn_seq, &resolved_model);
                index_turn_tool_content(state, model_turn_seq, &turn);
                rebuild_committed_children(state, item_id, sequence, &turn);
            } else {
                index_turn_tool_content(state, model_turn_seq, &turn);
            }
            place_tool_rows(state);
            push_event(
                state,
                EventLevel::Info,
                format!(
                    "model {} committed · finish {:?} · usage {}",
                    render_model(&resolved_model),
                    turn.finish_reason,
                    render_usage(&turn.usage)
                ),
            );
            for warning in warnings {
                push_event(
                    state,
                    EventLevel::Warning,
                    format!(
                        "model warning from {}: {warning}",
                        render_model(&resolved_model)
                    ),
                );
            }
        }
        EventPayload::ModelReplayEvaluated {
            resolved_model,
            ordered_decisions,
            ..
        } => {
            close_open_assistant(state);
            // Replay/cache discard and reconstruction dispositions are
            // WARNING rows with the exact model and a concise reason; routine
            // replay detail stays at DEBUG.
            for (level, decision) in render_replay(&ordered_decisions) {
                push_event(
                    state,
                    level,
                    format!(
                        "model {} replay · {decision}",
                        render_model(&resolved_model)
                    ),
                );
            }
        }
        EventPayload::ModelFallback {
            from,
            to,
            attempts_on_from,
            error,
            ..
        } => {
            close_open_assistant(state);
            push_event(
                state,
                EventLevel::Warning,
                format!(
                    "model fallback {} → {} after {attempts_on_from} attempt(s) · {}",
                    render_model(&from),
                    render_model(&to),
                    render_model_error(&error)
                ),
            );
        }
        EventPayload::ToolCallStarted { start } => {
            close_open_assistant(state);
            let mut identities = format!("model call: {}", start.owner.model_call_id);
            if let Some(provider_item_id) = &start.owner.provider_item_id {
                identities.push_str(&format!(" · provider item: {provider_item_id}"));
            }
            let arguments = find_tool_call_content(
                state,
                start.owner.model_turn_seq,
                &start.owner.model_call_id,
            )
            .unwrap_or_else(|| "{}".into());
            state.pending_tool_rows.push(PendingToolRow {
                turn_seq: start.owner.model_turn_seq,
                content_index: start.owner.content_index,
                call_id: start.tool_call_id,
            });
            state.tools.insert(
                start.tool_call_id,
                ToolCallState {
                    id: start.tool_call_id,
                    owner: start.owner.clone(),
                    presentation: start.presentation.clone(),
                    arguments,
                    status: ToolStatus::Running,
                    detail: identities,
                },
            );
            place_tool_rows(state);
        }
        EventPayload::ToolCallProgress {
            tool_call_id,
            message,
        } => {
            if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                tool.detail = message.to_string();
            }
            bump_tool_item(state, tool_call_id);
        }
        EventPayload::ToolCallTerminated { termination } => {
            let tool_call_id = termination.tool_call_id;
            let status = match termination.outcome {
                ToolTerminationOutcome::Completed => ToolStatus::Completed,
                ToolTerminationOutcome::Failed => ToolStatus::Failed,
                ToolTerminationOutcome::Cancelled => ToolStatus::Cancelled,
                ToolTerminationOutcome::Interrupted => ToolStatus::Interrupted,
            };
            let failed = !matches!(termination.outcome, ToolTerminationOutcome::Completed);
            let detail = match (termination.result, termination.error) {
                (Some(result), _) if !failed => render_tool_result(
                    result.title.as_str(),
                    &result.output,
                    &result.metadata,
                    result.truncation.as_ref().map(|truncation| {
                        (
                            truncation.retained.uri.as_str(),
                            truncation.original_bytes,
                            truncation.original_lines,
                        )
                    }),
                    &result.attachments,
                ),
                (_, Some(error)) => error.message.to_string(),
                _ => String::new(),
            };
            if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                tool.status = status;
                if !detail.is_empty() {
                    tool.detail = detail;
                }
            }
            bump_tool_item(state, tool_call_id);
        }
        EventPayload::ApprovalRequested { request } => {
            state
                .approvals
                .retain(|approval| approval.approval_id != request.approval_id());
            state
                .approvals
                .push(approval_state_from_request(session_id, request, false));
        }
        EventPayload::ApprovalEvaluated {
            approval_id,
            decision,
        } => push_event(
            state,
            EventLevel::Debug,
            format!(
                "approval {approval_id} evaluated: {:?} ({:?})",
                decision.decision, decision.reason_code
            )
            .to_lowercase(),
        ),
        EventPayload::ApprovalEscalated {
            approval_id,
            reason_code,
        } => {
            if let Some(approval) = state
                .approvals
                .iter_mut()
                .find(|approval| approval.approval_id == approval_id)
            {
                approval.escalated = true;
            }
            push_event(
                state,
                EventLevel::Warning,
                format!("approval {approval_id} escalated: {reason_code:?}").to_lowercase(),
            );
        }
        EventPayload::ApprovalUserDecisionRecorded {
            approval_id,
            decision,
            ..
        } => push_event(
            state,
            EventLevel::Info,
            format!("approval {approval_id} response recorded: {decision:?}").to_lowercase(),
        ),
        EventPayload::ApprovalFinalized {
            approval_id,
            decision,
        } => {
            state
                .approvals
                .retain(|approval| approval.approval_id != approval_id);
            push_event(
                state,
                EventLevel::Info,
                format!(
                    "approval {approval_id}: {} ({:?})",
                    approval_outcome_label(decision.outcome),
                    decision.reason_code
                )
                .to_lowercase(),
            );
        }
        EventPayload::ApprovalCancelled {
            approval_id,
            reason_code,
        } => {
            state
                .approvals
                .retain(|approval| approval.approval_id != approval_id);
            push_event(
                state,
                EventLevel::Info,
                format!("approval {approval_id} cancelled: {reason_code:?}").to_lowercase(),
            );
        }
        EventPayload::ApprovalDoomLoopDetected {
            approval_id,
            operation_fingerprint,
            repetitions,
        } => push_event(
            state,
            EventLevel::Error,
            format!(
                "approval {approval_id} doom loop: {} repeated {repetitions} times",
                operation_fingerprint.digest()
            ),
        ),
        EventPayload::TreeApprovalGrantCommitted { grant } => push_event(
            state,
            EventLevel::Debug,
            format!(
                "tree approval grant {} committed for {}",
                grant.grant_id,
                grant.operation_fingerprint.digest()
            ),
        ),
        EventPayload::RunCompleted { .. } => {
            close_open_assistant(state);
            state.active_run = None;
            state.attempts.clear();
            state.pending_tool_rows.clear();
            state.approvals.clear();
            push_event(state, EventLevel::Info, "run completed".into());
        }
        EventPayload::RunFailed { error } => {
            close_open_assistant(state);
            state.active_run = None;
            state.attempts.clear();
            state.pending_tool_rows.clear();
            state.approvals.clear();
            push_event(state, EventLevel::Error, format!("run failed: {error}"));
        }
        EventPayload::RunCancelled { reason } => {
            close_open_assistant(state);
            state.active_run = None;
            state.attempts.clear();
            state.pending_tool_rows.clear();
            state.approvals.clear();
            push_event(
                state,
                EventLevel::Info,
                reason.map_or_else(
                    || "run cancelled".into(),
                    |reason| format!("run cancelled: {reason}"),
                ),
            );
        }
        EventPayload::RunInterrupted { reason } => {
            close_open_assistant(state);
            state.active_run = None;
            state.attempts.clear();
            state.pending_tool_rows.clear();
            state.approvals.clear();
            push_event(
                state,
                EventLevel::Error,
                reason.map_or_else(
                    || "run interrupted".into(),
                    |reason| format!("run interrupted: {reason}"),
                ),
            );
        }
        EventPayload::InternalAgentStarted {
            kind,
            backend,
            call,
            ..
        } => push_event(
            state,
            EventLevel::Info,
            format!(
                "internal agent {kind:?} started via {}: {}",
                render_internal_backend(&backend),
                call.input_summary
            )
            .to_lowercase(),
        ),
        EventPayload::InternalAgentCompleted { kind, result, .. } => push_event(
            state,
            EventLevel::Info,
            format!(
                "internal agent {kind:?} completed: {}",
                result.output_summary
            )
            .to_lowercase(),
        ),
        EventPayload::InternalAgentFailed { kind, failure, .. } => push_event(
            state,
            EventLevel::Error,
            format!("internal agent {kind:?} failed: {}", failure.message).to_lowercase(),
        ),
        EventPayload::InternalAgentCancelled { kind, reason, .. } => push_event(
            state,
            EventLevel::Info,
            reason.map_or_else(
                || format!("internal agent {kind:?} cancelled").to_lowercase(),
                |reason| format!("internal agent {kind:?} cancelled: {reason}").to_lowercase(),
            ),
        ),
        EventPayload::InternalAgentInterrupted { kind, reason, .. } => push_event(
            state,
            EventLevel::Error,
            reason.map_or_else(
                || format!("internal agent {kind:?} interrupted").to_lowercase(),
                |reason| format!("internal agent {kind:?} interrupted: {reason}").to_lowercase(),
            ),
        ),
        EventPayload::InternalAgentFallback {
            kind,
            from,
            to,
            failure,
            attempts,
            ..
        } => push_event(
            state,
            EventLevel::Warning,
            format!(
                "internal agent {kind:?} fallback {} → {} after {attempts} attempt(s): {}",
                render_internal_backend(&from),
                render_internal_backend(&to),
                failure.message
            )
            .to_lowercase(),
        ),
        EventPayload::ContextCheckpointCommitted { commit } => {
            let kind = match &commit.checkpoint {
                cookie_agent_protocol::ContextCheckpoint::ProviderNative { .. } => {
                    "provider-native"
                }
                cookie_agent_protocol::ContextCheckpoint::InternalSummary { .. } => {
                    "internal summary"
                }
            };
            push_event(
                state,
                EventLevel::Info,
                format!(
                    "context checkpoint committed ({kind}, input through sequence {})",
                    commit.boundaries.input_through_seq
                ),
            );
        }
        EventPayload::SessionTitleCommitted { change, .. } => {
            push_event(state, EventLevel::Info, render_title_commit(&change));
        }
        EventPayload::UserInputApplied { .. } => close_open_assistant(state),
        EventPayload::SessionCreated { creation_agent, .. } => {
            // Before any run starts, attempts (for example title generation)
            // attribute to the creation agent's frozen identity.
            if state.run_agent.is_none() {
                state.run_agent = Some(creation_agent.agent.clone());
            }
            state.creation_agent = Some(creation_agent);
        }
        EventPayload::ToolStdinSubmitted { .. } | EventPayload::ToolCallLinked { .. } => {}
    }
}

/// Open a fresh assistant item for a streaming attempt. Attempt boundaries
/// are closed before this runs, so each attempt owns one item with ordered
/// text/thinking/tool children beneath one visible header.
fn open_assistant_item(state: &mut SessionState, attribution: FrozenAssistantAttribution) -> u64 {
    state.open_assistant = None;
    push_item(state, |id| TranscriptItem::Assistant {
        id,
        version: 0,
        attribution,
        committed_turn_seq: None,
        children: Vec::new(),
    });
    state
        .transcript
        .last()
        .expect("assistant item was just pushed")
        .id()
}

fn mark_committed(
    state: &mut SessionState,
    item_id: u64,
    model_turn_seq: u64,
    resolved_model: &ResolvedModelRef,
) {
    if let Some(TranscriptItem::Assistant {
        version,
        attribution,
        committed_turn_seq,
        ..
    }) = state
        .transcript
        .iter_mut()
        .find(|item| item.id() == item_id)
    {
        // The committed turn's exact resolved model is authoritative for the
        // visible header; streaming attempt attribution is replaced on commit.
        attribution.resolved_model = resolved_model.clone();
        *committed_turn_seq = Some(model_turn_seq);
        *version = version.wrapping_add(1);
    }
    state.turn_items.insert(model_turn_seq, item_id);
}

/// Index durable tool input from committed turn content so an ownership
/// event's expanded row can show exact arguments without the start event
/// duplicating them.
fn index_turn_tool_content(
    state: &mut SessionState,
    model_turn_seq: u64,
    turn: &PersistedModelTurn,
) {
    for part in &turn.content {
        if let cookie_agent_protocol::PersistedAssistantPart::ToolCall { id, input, .. } = part {
            state
                .turn_tool_index
                .insert((model_turn_seq, id.as_str().to_owned()), input.to_string());
        }
    }
}

/// Rebuild every text/thinking/tool child in exact
/// `PersistedModelTurn.content` order. Each content part becomes one child:
/// text and thinking parts preserve multiple segments; tool calls become
/// placeholders that started tools link by their exact `content_index`.
/// Deltas that streamed ahead of the commit are superseded by the durable
/// turn, which is the sole canonical content.
fn rebuild_committed_children(
    state: &mut SessionState,
    item_id: u64,
    sequence: u64,
    turn: &PersistedModelTurn,
) {
    state.open_assistant = None;
    let mut children = Vec::with_capacity(turn.content.len());
    for (index, part) in turn.content.iter().enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        match part {
            cookie_agent_protocol::PersistedAssistantPart::Text { text, .. }
                if !text.is_empty() =>
            {
                children.push(AssistantChild::Text {
                    id: sequence,
                    version: 0,
                    markdown: MarkdownDocument::new(text.clone()),
                });
            }
            cookie_agent_protocol::PersistedAssistantPart::Reasoning { text, .. }
                if !text.is_empty() =>
            {
                children.push(AssistantChild::Thinking {
                    id: sequence,
                    version: 0,
                    text: text.clone(),
                });
            }
            cookie_agent_protocol::PersistedAssistantPart::ToolCall { .. } => {
                children.push(AssistantChild::CommittedTool {
                    content_index: index,
                });
            }
            _ => {}
        }
    }
    // Bump the sequence-derived child id so distinct segments never share
    // one id across consecutive parts of the same kind.
    for (offset, child) in children.iter_mut().enumerate() {
        let id = sequence.wrapping_add(offset as u64).max(1);
        match child {
            AssistantChild::Text { id: existing, .. }
            | AssistantChild::Thinking { id: existing, .. } => {
                *existing = id;
            }
            AssistantChild::CommittedTool { .. } | AssistantChild::Tool { .. } => {}
        }
    }
    if let Some(TranscriptItem::Assistant {
        version,
        children: existing,
        ..
    }) = state
        .transcript
        .iter_mut()
        .find(|item| item.id() == item_id)
    {
        *existing = children;
        *version = version.wrapping_add(1);
    }
}

fn append_assistant_delta(
    state: &mut SessionState,
    item_id: u64,
    sequence: u64,
    text: String,
    kind: AssistantPartKind,
) {
    if let Some(open) = state.open_assistant
        && open.item_id == item_id
        && let Some(TranscriptItem::Assistant {
            version, children, ..
        }) = state
            .transcript
            .iter_mut()
            .find(|item| item.id() == open.item_id)
    {
        if open.kind == kind
            && let Some(part) = children.iter_mut().find(|part| part.id() == open.part_id)
        {
            match (part, kind) {
                (
                    AssistantChild::Text {
                        version, markdown, ..
                    },
                    AssistantPartKind::Text,
                ) => {
                    markdown.append(&text);
                    *version = version.wrapping_add(1);
                }
                (
                    AssistantChild::Thinking {
                        version,
                        text: existing,
                        ..
                    },
                    AssistantPartKind::Thinking,
                ) => {
                    existing.push_str(&text);
                    *version = version.wrapping_add(1);
                }
                _ => unreachable!("open assistant part kind matches its projection"),
            }
            *version = version.wrapping_add(1);
            return;
        }

        children.push(new_assistant_part(sequence, text, kind));
        *version = version.wrapping_add(1);
        state.open_assistant = Some(OpenAssistantProjection {
            item_id,
            part_id: sequence,
            kind,
        });
        return;
    }
    if let Some(TranscriptItem::Assistant {
        version, children, ..
    }) = state
        .transcript
        .iter_mut()
        .find(|item| item.id() == item_id)
    {
        children.push(new_assistant_part(sequence, text, kind));
        *version = version.wrapping_add(1);
        state.open_assistant = Some(OpenAssistantProjection {
            item_id,
            part_id: sequence,
            kind,
        });
    }
}

fn new_assistant_part(sequence: u64, text: String, kind: AssistantPartKind) -> AssistantChild {
    match kind {
        AssistantPartKind::Text => AssistantChild::Text {
            id: sequence,
            version: 0,
            markdown: MarkdownDocument::new(text),
        },
        AssistantPartKind::Thinking => AssistantChild::Thinking {
            id: sequence,
            version: 0,
            text,
        },
    }
}

/// Link started tools into their owning assistant item at the committed
/// placeholder with the exact same content index. A tool begins only after
/// its owning turn is durable; rows whose owning item or placeholder is not
/// yet known stay buffered, so out-of-order starts/completions can never
/// reorder children.
fn place_tool_rows(state: &mut SessionState) {
    if state.pending_tool_rows.is_empty() {
        return;
    }
    let pending = std::mem::take(&mut state.pending_tool_rows);
    let mut deferred = Vec::new();
    for row in pending {
        let linked = state
            .turn_items
            .get(&row.turn_seq)
            .copied()
            .is_some_and(|item_id| link_tool_child(state, item_id, row.content_index, row.call_id));
        if !linked {
            deferred.push(row);
        }
    }
    state.pending_tool_rows = deferred;
}

/// Replace the committed placeholder at `content_index` with the started
/// tool. Returns false when the owning item or placeholder is not durable
/// yet (or the index does not name a tool part).
fn link_tool_child(
    state: &mut SessionState,
    item_id: u64,
    content_index: u32,
    call_id: ToolCallId,
) -> bool {
    let Some(TranscriptItem::Assistant {
        version, children, ..
    }) = state
        .transcript
        .iter_mut()
        .find(|item| item.id() == item_id)
    else {
        return false;
    };
    let already = children.iter().any(
        |child| matches!(child, AssistantChild::Tool { call_id: existing } if *existing == call_id),
    );
    if already {
        return true;
    }
    for child in children.iter_mut() {
        if let AssistantChild::CommittedTool {
            content_index: placeholder,
        } = child
            && *placeholder == content_index
        {
            *child = AssistantChild::Tool { call_id };
            *version = version.wrapping_add(1);
            return true;
        }
    }
    false
}

fn close_open_assistant(state: &mut SessionState) {
    let Some(open) = state.open_assistant.take() else {
        return;
    };
    if let Some(TranscriptItem::Assistant { version, .. }) = state
        .transcript
        .iter_mut()
        .find(|item| item.id() == open.item_id)
    {
        *version = version.wrapping_add(1);
    }
}

fn push_item(state: &mut SessionState, item: impl FnOnce(u64) -> TranscriptItem) {
    state.next_transcript_id = state.next_transcript_id.wrapping_add(1).max(1);
    state.transcript.push(item(state.next_transcript_id));
}

fn push_event(state: &mut SessionState, level: EventLevel, text: String) {
    push_item(state, |id| TranscriptItem::Event {
        id,
        version: 0,
        level,
        text,
    });
}

fn bump_tool_item(state: &mut SessionState, tool_call_id: ToolCallId) {
    if let Some(TranscriptItem::Assistant { version, children, .. }) = state
        .transcript
        .iter_mut()
        .find(|item| {
            matches!(item, TranscriptItem::Assistant { children, .. } if children.iter().any(
                |child| matches!(child, AssistantChild::Tool { call_id } if *call_id == tool_call_id)))
        })
    {
        let _ = children;
        *version = version.wrapping_add(1);
    }
}

fn approval_outcome_label(outcome: ApprovalFinalOutcome) -> &'static str {
    match outcome {
        ApprovalFinalOutcome::Approved => "approved",
        ApprovalFinalOutcome::Rejected => "rejected",
        ApprovalFinalOutcome::Cancelled => "cancelled",
        ApprovalFinalOutcome::Expired => "expired",
    }
}

pub(crate) fn approval_state_from_record(record: ApprovalRecord) -> Option<ApprovalState> {
    match record.status {
        ApprovalStatus::Escalated => {}
        ApprovalStatus::Pending
        | ApprovalStatus::Approved
        | ApprovalStatus::Rejected
        | ApprovalStatus::Cancelled
        | ApprovalStatus::Expired => return None,
    }
    let approval = approval_state_from_request(record.session_id, record.request, true);
    approval.is_visible_user_escalation().then_some(approval)
}

fn approval_request_metadata(
    request: &cookie_agent_protocol::ApprovalRequest,
) -> (u64, ApprovalTrigger) {
    let wire = serde_json::to_value(request).expect("protocol approval request serializes");
    let revision = wire["revision"]
        .as_u64()
        .expect("protocol approval revision is an integer");
    let trigger = serde_json::from_value(wire["trigger"].clone())
        .expect("protocol approval trigger deserializes");
    (revision, trigger)
}

/// Serialized views over private approval identity fields. The wire form is
/// the exact durable protocol shape, so display projection stays honest
/// without new accessors.
fn approval_operation_parts(
    operation: &cookie_agent_protocol::PreparedOperationIdentity,
) -> (Sha256Digest, Sha256Digest, PreparedCapabilityLifetime) {
    let wire = serde_json::to_value(operation).expect("protocol operation serializes");
    let arguments = serde_json::from_value(wire["normalized_arguments_digest"].clone())
        .expect("arguments digest deserializes");
    let context = serde_json::from_value(wire["execution_context_digest"].clone())
        .expect("context digest deserializes");
    let lifetime = serde_json::from_value(wire["capability_lifetime"].clone())
        .expect("capability lifetime deserializes");
    (arguments, context, lifetime)
}

fn approval_request_parts(
    request: &ApprovalRequest,
) -> (Vec<ApprovalEvaluation>, ApprovalConstraints) {
    let wire = serde_json::to_value(request).expect("protocol approval request serializes");
    let evaluations =
        serde_json::from_value(wire["evaluations"].clone()).expect("evaluations deserialize");
    let constraints =
        serde_json::from_value(wire["constraints"].clone()).expect("constraints deserialize");
    (evaluations, constraints)
}

fn approval_state_from_request(
    session_id: SessionId,
    request: ApprovalRequest,
    escalated: bool,
) -> ApprovalState {
    let (request_revision, trigger) = approval_request_metadata(&request);
    let operation = request.operation();
    let (normalized_arguments_digest, execution_context_digest, capability_lifetime) =
        approval_operation_parts(operation);
    let (evaluations, constraints) = approval_request_parts(&request);
    ApprovalState {
        session_id,
        approval_id: request.approval_id(),
        request_revision,
        operation_fingerprint: request.operation_fingerprint().clone(),
        trigger,
        normalized_arguments_digest,
        execution_context_digest,
        capability_lifetime,
        capabilities: operation.capabilities().to_vec(),
        resources: operation.resources().to_vec(),
        evaluations,
        constraints,
        escalated,
    }
}

fn render_model(model: &ResolvedModelRef) -> String {
    let variant = model
        .selection
        .variant
        .as_ref()
        .map_or_else(|| "base".to_owned(), |variant| variant.to_string());
    format!(
        "{}/{} ({variant}, {})",
        model.provider_id,
        model.model_id,
        model.adapter_id.as_str()
    )
}

fn render_usage(usage: &Usage) -> String {
    let value = |value: Option<u64>| value.map_or_else(|| "?".into(), |value| value.to_string());
    format!(
        "in {} [direct {}, cache read {}, cache write {}], out {} [text {}, thinking {}]",
        value(usage.input_tokens),
        value(usage.input_tokens_no_cache),
        value(usage.input_tokens_cache_read),
        value(usage.input_tokens_cache_write),
        value(usage.output_tokens),
        value(usage.output_tokens_text),
        value(usage.output_tokens_reasoning)
    )
}

fn render_replay(decisions: &[ReplayDecision]) -> Vec<(EventLevel, String)> {
    if decisions.is_empty() {
        return vec![(EventLevel::Info, "no history entries".into())];
    }
    decisions
        .iter()
        .map(|decision| {
            let (level, disposition) = match &decision.disposition {
                ReplayDisposition::Replayed => (EventLevel::Debug, "replayed".into()),
                ReplayDisposition::NoArtifact => (EventLevel::Debug, "no artifact".into()),
                ReplayDisposition::DiscardedForeignAdapter { found, expected } => (
                    EventLevel::Warning,
                    format!(
                        "discarded foreign adapter {found} (expected {})",
                        expected.as_str()
                    ),
                ),
                ReplayDisposition::DiscardedForeignModelSelection { found, expected } => (
                    EventLevel::Warning,
                    format!(
                        "discarded foreign model selection {}/{} (expected {}/{})",
                        found.model,
                        found
                            .variant
                            .as_ref()
                            .map_or("base".into(), |variant| variant.to_string()),
                        expected.model,
                        expected
                            .variant
                            .as_ref()
                            .map_or("base".into(), |variant| variant.to_string())
                    ),
                ),
                ReplayDisposition::DiscardedForeignVariant { found, expected } => (
                    EventLevel::Warning,
                    format!(
                        "discarded foreign variant {} (expected {})",
                        found
                            .as_ref()
                            .map_or("base".into(), |variant| variant.to_string()),
                        expected
                            .as_ref()
                            .map_or("base".into(), |variant| variant.to_string())
                    ),
                ),
                ReplayDisposition::DiscardedInvalidPayload { reason } => (
                    EventLevel::Warning,
                    format!("discarded invalid payload: {reason}"),
                ),
                ReplayDisposition::ReconstructedNormalizedHistory => (
                    EventLevel::Warning,
                    "reconstructed normalized history".into(),
                ),
            };
            (level, format!("#{} {disposition}", decision.history_index))
        })
        .collect()
}

fn render_model_error(error: &ModelErrorSummary) -> String {
    let mut details = vec![
        wire_enum_label(error.kind),
        error.message.to_string(),
        format!("stage {}", wire_enum_label(error.stage)),
        format!("retryable {}", error.retryable),
        format!("{} bytes received", error.bytes_received),
    ];
    if let Some(status) = error.http_status {
        details.push(format!("HTTP {status}"));
    }
    if let Some(code) = &error.vendor_code {
        details.push(format!("code {code}"));
    }
    if let Some(request_id) = &error.request_id {
        details.push(format!("request {request_id}"));
    }
    if let Some(retry_after_ms) = error.retry_after_ms {
        details.push(format!("retry after {retry_after_ms}ms"));
    }
    details.join(" · ")
}

fn wire_enum_label(value: impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn render_tool_result(
    title: &str,
    output: &str,
    metadata: &serde_json::Value,
    truncation: Option<(&str, u64, u64)>,
    attachments: &[ToolAttachment],
) -> String {
    let mut lines = vec![title.to_owned(), output.to_owned()];
    if !metadata.is_null() {
        lines.push(format!("metadata: {metadata}"));
    }
    if let Some((uri, original_bytes, original_lines)) = truncation {
        lines.push(format!(
            "retained output: {uri} ({original_bytes} bytes, {original_lines} lines)"
        ));
    }
    for attachment in attachments {
        lines.push(format!(
            "attachment: {} · {} bytes · sha256:{} · {}",
            attachment.mime_type,
            attachment.byte_length,
            attachment.sha256,
            attachment.reference.uri
        ));
    }
    lines.join("\n")
}

/// Locate the durable tool input for an ownership reference from the
/// referenced committed turn's content; used only for the expanded row.
fn find_tool_call_content(
    state: &SessionState,
    model_turn_seq: u64,
    model_call_id: &cookie_agent_protocol::ModelCallId,
) -> Option<String> {
    state
        .turn_tool_index
        .get(&(model_turn_seq, model_call_id.as_str().to_owned()))
        .cloned()
}

fn render_internal_backend(backend: &cookie_agent_protocol::InternalAgentBackend) -> String {
    match backend {
        cookie_agent_protocol::InternalAgentBackend::Model { resolved_model }
        | cookie_agent_protocol::InternalAgentBackend::ProviderNative { resolved_model } => {
            render_model(resolved_model)
        }
        cookie_agent_protocol::InternalAgentBackend::Builtin { name, revision } => {
            format!("builtin {name}@{revision}")
        }
    }
}

fn render_title_commit(change: &SessionTitleChange) -> String {
    match change {
        SessionTitleChange::UserSet { title, .. } => format!("session renamed to {title}"),
        SessionTitleChange::UserClear { .. } => "session title cleared".into(),
        SessionTitleChange::UserReset { .. } => "session title reset".into(),
        SessionTitleChange::InternalAgentSet { title, .. } => {
            format!("session title set to {title}")
        }
        SessionTitleChange::FallbackSet { title } => format!("session title set to {title}"),
    }
}

fn stream_key(stream: OutputStream) -> bool {
    matches!(stream, OutputStream::Stderr)
}

fn pending_stream(output: &PendingOutput) -> bool {
    match output {
        PendingOutput::Snapshot(snapshot) => stream_key(snapshot.stream),
        PendingOutput::Delta(delta) => stream_key(delta.stream),
        PendingOutput::Gap(gap) => stream_key(gap.stream),
    }
}

/// Byte-offset ordered renderer for one stdout or stderr stream.
#[derive(Clone, Debug, Default)]
pub struct OrderedOutput {
    pub data: Vec<u8>,
    pub next_offset: u64,
    pub has_gap: bool,
    pending: BTreeMap<u64, Vec<u8>>,
}

impl OrderedOutput {
    pub fn replace_snapshot(&mut self, start: u64, end: u64, mut chunks: Vec<OutputDelta>) {
        self.data.clear();
        self.pending.clear();
        self.has_gap = start > 0;
        chunks.sort_by_key(|chunk| chunk.byte_offset);
        for chunk in chunks {
            if let Ok(bytes) = STANDARD.decode(chunk.data) {
                self.data.extend_from_slice(&bytes);
            }
        }
        self.next_offset = end;
    }

    pub fn push(&mut self, delta: OutputDelta) {
        let Ok(bytes) = STANDARD.decode(delta.data) else {
            return;
        };
        if delta.byte_offset < self.next_offset {
            return;
        }
        self.pending.entry(delta.byte_offset).or_insert(bytes);
        self.flush();
    }

    pub fn mark_gap(&mut self, next_offset: u64) {
        self.has_gap = true;
        let next_offset = self.next_offset.max(next_offset);
        self.next_offset = next_offset;
        self.pending.retain(|offset, _| *offset >= next_offset);
        self.flush();
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }

    fn flush(&mut self) {
        while let Some(bytes) = self.pending.remove(&self.next_offset) {
            self.next_offset += bytes.len() as u64;
            self.data.extend_from_slice(&bytes);
        }
    }
}
