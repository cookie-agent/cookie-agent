//! Disposable UI projections reduced from protocol events.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::{
    ApprovalCapability, ApprovalConstraints, ApprovalEvaluation, ApprovalFinalOutcome, ApprovalId,
    ApprovalRecord, ApprovalRequest, ApprovalStatus, ApprovalTrigger, ContextCheckpoint, Event,
    EventEnvelope, EventSubscriptionMessage, ModelErrorSummary, ModelRef, OperationFingerprint,
    OutputDelta, OutputGap, OutputSnapshotEnvelope, OutputStream, PreparedApprovalResource,
    PreparedCapabilityLifetime, ProfileSnapshot, ReplayDecision, ReplayDisposition, RunId,
    SessionId, SessionTitleCommit, Sha256Digest, ToolCallId, ToolResult, Usage,
};
use serde::Serialize;

use crate::{client::ClientDelivery, markdown::MarkdownDocument};

/// The visible state of a tool invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
}

/// A tool block displayed in the conversation transcript.
#[derive(Clone, Debug)]
pub struct ToolCallState {
    pub id: ToolCallId,
    pub tool: String,
    pub arguments: String,
    pub status: ToolStatus,
    pub detail: String,
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
        parts: Vec<AssistantPart>,
    },
    Tool {
        id: u64,
        version: u64,
        call_id: ToolCallId,
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

/// One ordered child segment inside an assistant model-attempt transcript item.
#[derive(Clone, Debug)]
pub enum AssistantPart {
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
}

impl AssistantPart {
    pub fn id(&self) -> u64 {
        match self {
            Self::Text { id, .. } | Self::Thinking { id, .. } => *id,
        }
    }

    pub fn version(&self) -> u64 {
        match self {
            Self::Text { version, .. } | Self::Thinking { version, .. } => *version,
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
            Self::User { id, .. }
            | Self::Assistant { id, .. }
            | Self::Tool { id, .. }
            | Self::Event { id, .. } => *id,
        }
    }

    pub fn version(&self) -> u64 {
        match self {
            Self::User { version, .. }
            | Self::Assistant { version, .. }
            | Self::Tool { version, .. }
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
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant {
            id: 1,
            version: 0,
            parts: vec![AssistantPart::Text {
                id: 1,
                version: 0,
                markdown: MarkdownDocument::new(text.into()),
            }],
        }
    }

    #[cfg(test)]
    pub fn assistant_parts(parts: Vec<AssistantPart>) -> Self {
        Self::Assistant {
            id: 1,
            version: 0,
            parts,
        }
    }

    #[cfg(test)]
    pub fn tool(id: u64, call_id: ToolCallId) -> Self {
        Self::Tool {
            id,
            version: 0,
            call_id,
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

/// Per-session projection of persisted events and live output.
#[derive(Clone, Debug, Default)]
pub struct SessionState {
    /// Changes whenever the visible projection mutates, for UI cache invalidation.
    pub version: u64,
    pub generation: u64,
    pub last_seq: u64,
    pub active_run: Option<RunId>,
    /// Exact profile snapshot accepted by the latest `RunStarted` event.
    pub run_profile: Option<ProfileSnapshot>,
    pub transcript: Vec<TranscriptItem>,
    pub(crate) next_transcript_id: u64,
    pub(crate) open_assistant: Option<OpenAssistantProjection>,
    pub tools: HashMap<ToolCallId, ToolCallState>,
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
                    if self.apply_event_for_generation(event, generation) {
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
                let started_call = match &event.event {
                    Event::ToolCallStarted { tool_call_id, .. } => Some(*tool_call_id),
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
                        event.event,
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
    pub fn apply_event(&mut self, envelope: EventEnvelope) -> bool {
        self.apply_event_for_generation(envelope, 0)
    }

    pub fn apply_event_for_generation(&mut self, envelope: EventEnvelope, generation: u64) -> bool {
        let started_call = match &envelope.event {
            Event::ToolCallStarted { tool_call_id, .. } => Some(*tool_call_id),
            _ => None,
        };
        if let Some(call_id) = started_call {
            self.tool_sessions.insert(call_id, envelope.session_id);
        }
        if self.quarantined_sessions.contains(&envelope.session_id) {
            return false;
        }
        let state = self.sessions.entry(envelope.session_id).or_default();
        if state.generation != generation {
            return false;
        }
        if envelope.seq <= state.last_seq {
            return true;
        }
        if envelope.seq != state.last_seq + 1 {
            return false;
        }
        state.last_seq = envelope.seq;
        reduce_event(
            state,
            envelope.session_id,
            envelope.run_id,
            envelope.seq,
            envelope.event,
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
                self.apply_event_for_generation(event, generation)
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
        events: Vec<EventEnvelope>,
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
    event: Event,
) {
    match event {
        Event::RunStarted { input, profile, .. } => {
            close_open_assistant(state);
            state.active_run = run_id;
            state.run_profile = Some(profile);
            push_item(state, |id| TranscriptItem::User {
                id,
                version: 0,
                text: input,
            });
        }
        Event::UserInputSubmitted { input } => {
            close_open_assistant(state);
            push_item(state, |id| TranscriptItem::User {
                id,
                version: 0,
                text: input,
            });
        }
        Event::TextDelta { text } => {
            append_assistant_delta(state, sequence, text, AssistantPartKind::Text);
        }
        Event::ReasoningDelta { text } => {
            append_assistant_delta(state, sequence, text, AssistantPartKind::Thinking);
        }
        Event::ToolCallStarted {
            tool_call_id,
            model_call_id,
            provider_item_id,
            tool,
            arguments,
        } => {
            close_open_assistant(state);
            let mut identities = format!("model call: {model_call_id}");
            if let Some(provider_item_id) = provider_item_id {
                identities.push_str(&format!(" · provider item: {provider_item_id}"));
            }
            state.tools.insert(
                tool_call_id,
                ToolCallState {
                    id: tool_call_id,
                    tool,
                    arguments: arguments.to_string(),
                    status: ToolStatus::Running,
                    detail: identities,
                },
            );
            push_item(state, |id| TranscriptItem::Tool {
                id,
                version: 0,
                call_id: tool_call_id,
            });
        }
        Event::ToolCallProgress {
            tool_call_id,
            message,
        } => {
            if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                tool.detail = message;
            }
            bump_tool_item(state, tool_call_id);
        }
        Event::ToolCallCompleted {
            tool_call_id,
            result,
        } => {
            if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                tool.status = ToolStatus::Completed;
                tool.detail = render_tool_result(&result);
            }
            bump_tool_item(state, tool_call_id);
        }
        Event::ToolCallFailed {
            tool_call_id,
            message,
            ..
        } => {
            if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                tool.status = ToolStatus::Failed;
                tool.detail = message;
            }
            bump_tool_item(state, tool_call_id);
        }
        Event::ApprovalRequested { request } => {
            state
                .approvals
                .retain(|approval| approval.approval_id != request.approval_id());
            state
                .approvals
                .push(approval_state_from_request(session_id, request, false));
        }
        Event::ApprovalEvaluated {
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
        Event::ApprovalEscalated {
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
        Event::ApprovalUserDecisionRecorded {
            approval_id,
            decision,
            ..
        } => push_event(
            state,
            EventLevel::Info,
            format!("approval {approval_id} response recorded: {decision:?}").to_lowercase(),
        ),
        Event::ApprovalFinalized {
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
        Event::ApprovalCancelled {
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
        Event::ApprovalDoomLoopDetected {
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
        Event::TreeApprovalGrantCommitted { grant } => push_event(
            state,
            EventLevel::Debug,
            format!(
                "tree approval grant {} committed for {}",
                grant.grant_id(),
                grant.operation_fingerprint().digest()
            ),
        ),
        Event::RunCompleted { .. } => {
            close_open_assistant(state);
            state.active_run = None;
            state.approvals.clear();
            push_event(state, EventLevel::Info, "run completed".into());
        }
        Event::RunFailed { message } => {
            close_open_assistant(state);
            state.active_run = None;
            state.approvals.clear();
            push_event(state, EventLevel::Error, format!("run failed: {message}"));
        }
        Event::RunCancelled { reason } => {
            close_open_assistant(state);
            state.active_run = None;
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
        Event::RunInterrupted { reason } => {
            close_open_assistant(state);
            state.active_run = None;
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
        Event::ModelTurnCommitted { model, turn, .. } => {
            close_open_assistant(state);
            push_event(
                state,
                EventLevel::Info,
                format!(
                    "model {} committed · finish {:?} · usage {}",
                    render_model(&model),
                    turn.finish_reason,
                    render_usage(&turn.usage)
                ),
            );
            for warning in turn.warnings {
                push_event(
                    state,
                    EventLevel::Warning,
                    format!("model warning from {}: {warning}", render_model(&model)),
                );
            }
        }
        Event::ModelReplayEvaluated { model, decisions } => {
            close_open_assistant(state);
            // Replay/cache discard and reconstruction dispositions are
            // WARNING rows with the exact model and a concise reason; routine
            // replay detail stays at DEBUG.
            for (level, decision) in render_replay(&decisions) {
                push_event(
                    state,
                    level,
                    format!("model {} replay · {decision}", render_model(&model)),
                );
            }
        }
        Event::ModelFallback {
            from,
            to,
            error,
            attempts,
        } => {
            close_open_assistant(state);
            push_event(
                state,
                EventLevel::Warning,
                format!(
                    "model fallback {} → {} after {attempts} attempt(s) · {}",
                    render_model(&from),
                    render_model(&to),
                    render_model_error(&error)
                ),
            );
        }
        Event::AttemptAbandoned => {
            close_open_assistant(state);
            push_event(state, EventLevel::Warning, "model attempt abandoned".into());
        }
        Event::InternalAgentStarted {
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
        Event::InternalAgentCompleted { kind, result, .. } => push_event(
            state,
            EventLevel::Info,
            format!(
                "internal agent {kind:?} completed: {}",
                result.output_summary
            )
            .to_lowercase(),
        ),
        Event::InternalAgentFailed { kind, failure, .. } => push_event(
            state,
            EventLevel::Error,
            format!("internal agent {kind:?} failed: {}", failure.message).to_lowercase(),
        ),
        Event::InternalAgentCancelled { kind, reason, .. } => push_event(
            state,
            EventLevel::Info,
            reason.map_or_else(
                || format!("internal agent {kind:?} cancelled").to_lowercase(),
                |reason| format!("internal agent {kind:?} cancelled: {reason}").to_lowercase(),
            ),
        ),
        Event::InternalAgentInterrupted { kind, reason, .. } => push_event(
            state,
            EventLevel::Error,
            reason.map_or_else(
                || format!("internal agent {kind:?} interrupted").to_lowercase(),
                |reason| format!("internal agent {kind:?} interrupted: {reason}").to_lowercase(),
            ),
        ),
        Event::InternalAgentFallback {
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
        Event::ContextCheckpointCommitted { commit } => {
            let kind = match commit.checkpoint() {
                ContextCheckpoint::ProviderNative { .. } => "provider-native",
                ContextCheckpoint::InternalSummary { .. } => "internal summary",
            };
            push_event(
                state,
                EventLevel::Info,
                format!(
                    "context checkpoint committed ({kind}, input through sequence {})",
                    commit.boundaries().input_through_seq
                ),
            );
        }
        Event::SessionTitleCommitted { commit, .. } => {
            push_event(state, EventLevel::Info, render_title_commit(&commit));
        }
        Event::UserInputApplied { .. } => close_open_assistant(state),
        Event::SessionCreated { .. }
        | Event::ToolStdinSubmitted { .. }
        | Event::ToolCallLinked { .. } => {}
    }
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

fn approval_state_from_request(
    session_id: SessionId,
    request: ApprovalRequest,
    escalated: bool,
) -> ApprovalState {
    let (request_revision, trigger) = approval_request_metadata(&request);
    ApprovalState {
        session_id,
        approval_id: request.approval_id(),
        request_revision,
        operation_fingerprint: request.operation_fingerprint().clone(),
        trigger,
        normalized_arguments_digest: request.operation().normalized_arguments_digest().clone(),
        execution_context_digest: request.operation().execution_context_digest().clone(),
        capability_lifetime: request.operation().capability_lifetime(),
        capabilities: request.operation().capabilities().to_vec(),
        resources: request.operation().resources().to_vec(),
        evaluations: request.evaluations().to_vec(),
        constraints: request.constraints().clone(),
        escalated,
    }
}

fn render_model(model: &ModelRef) -> String {
    format!(
        "{} ({}/{}, {})",
        model.name, model.provider_id, model.model_id, model.adapter_id
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
                    format!("discarded foreign adapter {found} (expected {expected})"),
                ),
                ReplayDisposition::DiscardedForeignScope { found, expected } => (
                    EventLevel::Warning,
                    format!(
                        "discarded foreign scope {}/{}/{} (expected {}/{}/{})",
                        found.provider_id,
                        found.model_id,
                        found.resource_id,
                        expected.provider_id,
                        expected.model_id,
                        expected.resource_id
                    ),
                ),
                ReplayDisposition::DiscardedInvalidPayload { reason } => (
                    EventLevel::Warning,
                    format!("discarded invalid payload: {reason}"),
                ),
                ReplayDisposition::ReconstructedNormalized => (
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
        error.message.clone(),
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

fn render_tool_result(result: &ToolResult) -> String {
    let mut lines = vec![result.title.clone(), result.output.clone()];
    if !result.metadata.is_null() {
        lines.push(format!("metadata: {}", result.metadata));
    }
    if let Some(truncation) = &result.truncation {
        lines.push(format!(
            "retained output: {} ({} bytes, {} lines)",
            truncation.retained.uri, truncation.original_bytes, truncation.original_lines
        ));
    }
    for attachment in &result.attachments {
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

fn append_assistant_delta(
    state: &mut SessionState,
    sequence: u64,
    text: String,
    kind: AssistantPartKind,
) {
    if let Some(open) = state.open_assistant
        && let Some(TranscriptItem::Assistant { version, parts, .. }) = state
            .transcript
            .iter_mut()
            .find(|item| item.id() == open.item_id)
    {
        if open.kind == kind
            && let Some(part) = parts.iter_mut().find(|part| part.id() == open.part_id)
        {
            match (part, kind) {
                (
                    AssistantPart::Text {
                        version, markdown, ..
                    },
                    AssistantPartKind::Text,
                ) => {
                    markdown.append(&text);
                    *version = version.wrapping_add(1);
                }
                (
                    AssistantPart::Thinking {
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

        parts.push(new_assistant_part(sequence, text, kind));
        *version = version.wrapping_add(1);
        state.open_assistant = Some(OpenAssistantProjection {
            item_id: open.item_id,
            part_id: sequence,
            kind,
        });
        return;
    }

    state.open_assistant = None;
    push_item(state, |id| TranscriptItem::Assistant {
        id,
        version: 0,
        parts: vec![new_assistant_part(sequence, text, kind)],
    });
    let item_id = state
        .transcript
        .last()
        .expect("assistant item was just pushed")
        .id();
    state.open_assistant = Some(OpenAssistantProjection {
        item_id,
        part_id: sequence,
        kind,
    });
}

fn new_assistant_part(sequence: u64, text: String, kind: AssistantPartKind) -> AssistantPart {
    match kind {
        AssistantPartKind::Text => AssistantPart::Text {
            id: sequence,
            version: 0,
            markdown: MarkdownDocument::new(text),
        },
        AssistantPartKind::Thinking => AssistantPart::Thinking {
            id: sequence,
            version: 0,
            text,
        },
    }
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
    if let Some(TranscriptItem::Tool { version, .. }) = state.transcript.iter_mut().find(
        |item| matches!(item, TranscriptItem::Tool { call_id, .. } if *call_id == tool_call_id),
    ) {
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

fn render_internal_backend(backend: &cookie_agent_protocol::InternalAgentBackend) -> String {
    match backend {
        cookie_agent_protocol::InternalAgentBackend::Model { model, .. }
        | cookie_agent_protocol::InternalAgentBackend::ProviderNative { model } => {
            render_model(model)
        }
        cookie_agent_protocol::InternalAgentBackend::Builtin { name, revision } => {
            format!("builtin {name}@{revision}")
        }
    }
}

fn render_title_commit(commit: &SessionTitleCommit) -> String {
    match commit {
        SessionTitleCommit::UserSet { title, .. } => format!("session renamed to {title}"),
        SessionTitleCommit::UserClear { .. } => "session title cleared".into(),
        SessionTitleCommit::UserReset { .. } => "session title reset".into(),
        SessionTitleCommit::InternalAgentSet { title, .. } => {
            format!("session title set to {title}")
        }
        SessionTitleCommit::FallbackSet { title } => format!("session title set to {title}"),
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

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use cookie_agent_protocol::{
        ActionKind, ApprovalBoundary, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation,
        ApprovalId, ApprovalRequest, ApprovalResourceSource, ApprovalTrigger, ArtifactReference,
        ContextCheckpoint, ContextCheckpointBoundaries, ContextCheckpointBudgets,
        ContextCheckpointCommit, DecisionTrace, Effect, Event, EventEnvelope, EventSchemaVersion,
        InternalAgentBackend, InternalAgentInvocationId, InternalAgentKind, InternalAgentRunId,
        InternalSummaryCheckpoint, ModelErrorKind, ModelErrorStage, ModelErrorSummary,
        ModelFinishReason, ModelRef, OutputDelta, OutputGap, OutputSnapshot,
        OutputSnapshotEnvelope, PersistedModelTurn, PreparedApprovalResource,
        PreparedBindingLifetime, PreparedCapabilityOperation, PreparedOperationIdentity,
        PreparedResourceDigest, PreparedResourceIdentity, ReplayDecision, ReplayDisposition,
        SafeInternalAgentCall, SafeInternalAgentResult, SessionId, SessionTitle,
        SessionTitleCommit, Sha256Digest, SummaryByteLimit, ToolAttachment, ToolOutputTruncation,
        ToolResult, Usage,
    };
    use jiff::Timestamp;

    use super::{
        AssistantPart, StateStore, ToolCallState, ToolStatus, TranscriptItem, render_tool_result,
    };
    use crate::client::ClientDelivery;

    fn reasoning_event(session_id: SessionId, sequence: u64, text: &str) -> EventEnvelope {
        EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id,
            run_id: None,
            seq: sequence,
            timestamp: Timestamp::now(),
            event: Event::ReasoningDelta { text: text.into() },
        }
    }

    fn event(session_id: SessionId, sequence: u64, event: Event) -> EventEnvelope {
        EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id,
            run_id: None,
            seq: sequence,
            timestamp: Timestamp::now(),
            event,
        }
    }

    fn approval_request(trigger: ApprovalTrigger) -> ApprovalRequest {
        let resource = PreparedApprovalResource {
            capability: ActionKind::Bash,
            canonical: PreparedResourceIdentity::new("command:git-status")
                .expect("prepared resource identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"git status"),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: ApprovalBoundary::CommandPrefix {
                prefix: "git status".into(),
            },
            source: ApprovalResourceSource::ModelRequest,
        };
        let resource_digest = resource.binding_digest.clone();
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"normalized arguments"),
            vec![ApprovalCapability {
                action: ActionKind::Bash,
                operation: PreparedCapabilityOperation::new("execute")
                    .expect("prepared capability operation"),
            }],
            vec![resource],
            Sha256Digest::of_bytes(b"execution context"),
        )
        .expect("prepared operation");
        ApprovalRequest::new(
            ApprovalId::new_v7(),
            3,
            trigger,
            operation,
            vec![ApprovalEvaluation {
                resource_digest,
                effect: Effect::Ask,
                trace: DecisionTrace {
                    action: ActionKind::Bash,
                    normalized_resource: "git status".into(),
                    candidates: Vec::new(),
                    effect: Effect::Ask,
                    precedence_reason: "model requested approval".into(),
                },
            }],
            ApprovalConstraints {
                allow_once: true,
                allow_tree_grant: false,
                cancellable: true,
                expires_at: None,
            },
        )
        .expect("approval request")
    }

    #[test]
    fn rich_tool_results_render_only_safe_attachment_metadata() {
        let digest = Sha256Digest::of_bytes(b"attachment");
        let reference = ArtifactReference {
            uri: format!("artifact://sha256/{digest}"),
        };
        let rendered = render_tool_result(&ToolResult {
            title: "Read attachment".into(),
            output: "PDF attached".into(),
            metadata: serde_json::json!({"kind": "attachment"}),
            truncation: Some(ToolOutputTruncation {
                original_bytes: 42,
                original_lines: 3,
                retained: reference.clone(),
            }),
            attachments: vec![ToolAttachment {
                mime_type: "application/pdf".into(),
                filename: None,
                byte_length: 42,
                sha256: digest,
                reference,
            }],
        });
        assert!(rendered.contains("Read attachment"));
        assert!(rendered.contains("application/pdf · 42 bytes"));
        assert!(rendered.contains("artifact://sha256/"));
        assert!(!rendered.contains("base64"));
    }

    #[test]
    fn thinking_child_id_is_stable_when_deltas_merge_and_replay_swaps() {
        let session_id = SessionId::new_v7();
        let first = reasoning_event(session_id, 1, "first ");
        let second = reasoning_event(session_id, 2, "second");
        let replay_first = reasoning_event(session_id, 1, "replacement ");
        let replay_second = reasoning_event(session_id, 2, "content");
        let mut store = StateStore::default();
        assert!(store.apply_event(first.clone()));
        assert!(store.apply_event(second.clone()));
        assert_eq!(store.sessions[&session_id].version, 2);
        assert!(matches!(
            store.sessions[&session_id].transcript.as_slice(),
            [TranscriptItem::Assistant { parts, .. }]
                if matches!(parts.as_slice(), [AssistantPart::Thinking { id: 1, text, .. }] if text == "first second")
        ));

        store.apply_delivery(ClientDelivery::ReplayStart {
            session_id,
            generation: 1,
            final_seq: 2,
            rebuild: true,
        });
        for event in [replay_first, replay_second] {
            store.apply_delivery(ClientDelivery::ReplayEvent {
                session_id,
                generation: 1,
                final_seq: 2,
                event: Box::new(event),
            });
        }
        store.apply_delivery(ClientDelivery::ReplayEnd {
            session_id,
            generation: 1,
            final_seq: 2,
        });
        assert!(matches!(
            store.sessions[&session_id].transcript.as_slice(),
            [TranscriptItem::Assistant { parts, .. }]
                if matches!(parts.as_slice(), [AssistantPart::Thinking { id: 1, text, .. }] if text == "replacement content")
        ));
        assert_eq!(store.sessions[&session_id].version, 3);
    }

    #[test]
    fn incremental_replay_closes_the_pre_replay_assistant_projection() {
        let session_id = SessionId::new_v7();
        let mut store = StateStore::default();
        assert!(store.apply_event(reasoning_event(session_id, 1, "before replay")));
        assert_eq!(store.sessions[&session_id].transcript.len(), 1);

        assert_eq!(
            store.apply_delivery(ClientDelivery::ReplayStart {
                session_id,
                generation: 0,
                final_seq: 2,
                rebuild: false,
            }),
            super::DeliveryOutcome::Applied
        );
        assert_eq!(
            store.apply_delivery(ClientDelivery::ReplayEvent {
                session_id,
                generation: 0,
                final_seq: 2,
                event: Box::new(reasoning_event(session_id, 2, "after replay")),
            }),
            super::DeliveryOutcome::Applied
        );
        assert_eq!(
            store.apply_delivery(ClientDelivery::ReplayEnd {
                session_id,
                generation: 0,
                final_seq: 2,
            }),
            super::DeliveryOutcome::Applied
        );
        assert!(matches!(
            store.sessions[&session_id].transcript.as_slice(),
            [
                TranscriptItem::Assistant { parts: first, .. },
                TranscriptItem::Assistant { parts: second, .. },
            ] if matches!(first.as_slice(), [AssistantPart::Thinking { id: 1, .. }])
                && matches!(second.as_slice(), [AssistantPart::Thinking { id: 2, .. }])
        ));
    }

    #[test]
    fn assistant_attempt_groups_ordered_thinking_and_text_children_until_boundaries() {
        let session_id = SessionId::new_v7();
        let tool_call_id = cookie_agent_protocol::ToolCallId::new_v7();
        let mut store = StateStore::default();
        let events = [
            Event::ReasoningDelta { text: "r1".into() },
            Event::ReasoningDelta { text: "+r2".into() },
            Event::TextDelta { text: "t1".into() },
            Event::ReasoningDelta { text: "r3".into() },
            Event::TextDelta { text: "t2".into() },
            Event::AttemptAbandoned,
            Event::ReasoningDelta {
                text: "partial".into(),
            },
            Event::ToolCallStarted {
                tool_call_id,
                model_call_id: "call-1".into(),
                provider_item_id: None,
                tool: "bash".into(),
                arguments: serde_json::json!({"command": "true"}),
            },
            Event::TextDelta {
                text: "after tool".into(),
            },
            Event::RunInterrupted {
                reason: Some("connection lost".into()),
            },
        ];
        for (index, next) in events.into_iter().enumerate() {
            assert!(store.apply_event(event(session_id, index as u64 + 1, next)));
        }

        let transcript = &store.sessions[&session_id].transcript;
        let TranscriptItem::Assistant { parts, .. } = &transcript[0] else {
            panic!("first assistant attempt");
        };
        assert!(matches!(
            parts.as_slice(),
            [
                AssistantPart::Thinking { id: 1, text, .. },
                AssistantPart::Text { id: 3, markdown, .. },
                AssistantPart::Thinking { id: 4, text: second, .. },
                AssistantPart::Text { id: 5, markdown: final_text, .. },
            ] if text == "r1+r2"
                && markdown.as_str() == "t1"
                && second == "r3"
                && final_text.as_str() == "t2"
        ));
        assert!(matches!(transcript[1], TranscriptItem::Event { .. }));
        assert!(matches!(
            &transcript[2],
            TranscriptItem::Assistant { parts, .. }
                if matches!(parts.as_slice(), [AssistantPart::Thinking { id: 7, text, .. }] if text == "partial")
        ));
        assert!(matches!(transcript[3], TranscriptItem::Tool { .. }));
        assert!(matches!(
            &transcript[4],
            TranscriptItem::Assistant { parts, .. }
                if matches!(parts.as_slice(), [AssistantPart::Text { id: 9, markdown, .. }] if markdown.as_str() == "after tool")
        ));
        assert!(matches!(
            transcript[5],
            TranscriptItem::Event {
                level: super::EventLevel::Error,
                ..
            }
        ));
        assert!(store.sessions[&session_id].open_assistant.is_none());
    }

    #[test]
    fn model_replay_and_user_turn_boundaries_start_new_assistant_items() {
        let session_id = SessionId::new_v7();
        let model = ModelRef {
            name: "primary".into(),
            provider_id: "provider".into(),
            model_id: "model".into(),
            adapter_id: "adapter".into(),
        };
        let mut store = StateStore::default();
        for (sequence, next) in [
            Event::ReasoningDelta {
                text: "attempt one".into(),
            },
            Event::ModelReplayEvaluated {
                model,
                decisions: Vec::new(),
            },
            Event::TextDelta {
                text: "attempt two".into(),
            },
            Event::UserInputSubmitted {
                input: "steer".into(),
            },
            Event::ReasoningDelta {
                text: "attempt three".into(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            assert!(store.apply_event(event(session_id, sequence as u64 + 1, next)));
        }
        assert!(matches!(
            store.sessions[&session_id].transcript.as_slice(),
            [
                TranscriptItem::Assistant { .. },
                TranscriptItem::Event { .. },
                TranscriptItem::Assistant { .. },
                TranscriptItem::User { .. },
                TranscriptItem::Assistant { .. },
            ]
        ));
    }

    #[test]
    fn open_thinking_indicator_state_ends_on_kind_and_attempt_boundaries() {
        let session_id = SessionId::new_v7();
        let mut store = StateStore::default();
        assert!(store.apply_event(reasoning_event(session_id, 1, "thinking")));
        let state = &store.sessions[&session_id];
        let TranscriptItem::Assistant { id, parts, .. } = &state.transcript[0] else {
            panic!("assistant item");
        };
        assert!(state.is_open_thinking(*id, parts[0].id()));

        assert!(store.apply_event(event(
            session_id,
            2,
            Event::TextDelta {
                text: "answer".into(),
            },
        )));
        let state = &store.sessions[&session_id];
        let TranscriptItem::Assistant { id, parts, .. } = &state.transcript[0] else {
            panic!("assistant item");
        };
        assert!(!state.is_open_thinking(*id, parts[0].id()));

        assert!(store.apply_event(event(
            session_id,
            3,
            Event::ReasoningDelta {
                text: "more".into(),
            },
        )));
        assert!(store.apply_event(event(session_id, 4, Event::AttemptAbandoned)));
        assert!(store.sessions[&session_id].open_assistant.is_none());
    }

    #[test]
    fn assistant_streaming_advances_only_the_open_markdown_tail() {
        let session_id = SessionId::new_v7();
        let mut store = StateStore::default();
        for (sequence, text) in [
            (1, "stable paragraph\n\nopen"),
            (2, " tail"),
            (3, " continues"),
        ] {
            assert!(store.apply_event(EventEnvelope {
                schema_version: EventSchemaVersion::current(),
                session_id,
                run_id: None,
                seq: sequence,
                timestamp: Timestamp::now(),
                event: Event::TextDelta { text: text.into() },
            }));
        }
        let TranscriptItem::Assistant { parts, version, .. } =
            &store.sessions[&session_id].transcript[0]
        else {
            panic!("assistant item");
        };
        let [AssistantPart::Text { markdown, .. }] = parts.as_slice() else {
            panic!("assistant text child");
        };
        assert_eq!(*version, 2);
        assert_eq!(markdown.parse_passes(), 3);
        assert!(markdown.stable_prefix_len() >= "stable paragraph\n\n".len());
        assert!(markdown.parsed_bytes() < 3 * markdown.as_str().len() as u64);
    }

    #[test]
    fn protocol_v6_model_replay_error_usage_and_approval_request_are_rendered() {
        let session_id = SessionId::new_v7();
        let model = ModelRef {
            name: "primary".into(),
            provider_id: "provider".into(),
            model_id: "model".into(),
            adapter_id: "adapter".into(),
        };
        let usage = Usage {
            input_tokens: Some(10),
            input_tokens_no_cache: Some(8),
            input_tokens_cache_read: Some(2),
            input_tokens_cache_write: Some(0),
            output_tokens: Some(4),
            output_tokens_text: Some(3),
            output_tokens_reasoning: Some(1),
        };
        let mut store = StateStore::default();
        for (seq, event) in [
            Event::ModelReplayEvaluated {
                model: model.clone(),
                decisions: vec![ReplayDecision {
                    history_index: 0,
                    disposition: ReplayDisposition::ReconstructedNormalized,
                }],
            },
            Event::ModelTurnCommitted {
                model: model.clone(),
                input_through_seq: 0,
                turn: PersistedModelTurn {
                    content: Vec::new(),
                    provider_options: Default::default(),
                    finish_reason: ModelFinishReason::Stop,
                    usage,
                    response_metadata: Default::default(),
                    provider_metadata: Default::default(),
                    warnings: vec!["safe warning".into()],
                    native_replay: None,
                },
            },
            Event::ModelFallback {
                from: model.clone(),
                to: ModelRef {
                    name: "fallback".into(),
                    ..model.clone()
                },
                error: ModelErrorSummary {
                    kind: ModelErrorKind::RateLimited,
                    message: "slow down".into(),
                    retryable: true,
                    stage: ModelErrorStage::ResponseHeaders,
                    http_status: Some(429),
                    bytes_received: 0,
                    vendor_code: Some("rate_limit".into()),
                    request_id: Some("request-id".into()),
                    retry_after_ms: Some(100),
                },
                attempts: 2,
            },
        ]
        .into_iter()
        .enumerate()
        {
            assert!(store.apply_event(EventEnvelope {
                schema_version: EventSchemaVersion::current(),
                session_id,
                run_id: None,
                seq: seq as u64 + 1,
                timestamp: Timestamp::now(),
                event,
            }));
        }
        let rendered = store.sessions[&session_id]
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Event { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("normalized history"));
        assert!(rendered.contains("usage in 10"));
        assert!(rendered.contains("safe warning"));
        assert!(rendered.contains("rate_limited"));
        assert!(rendered.contains("HTTP 429"));

        assert!(store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id,
            run_id: None,
            seq: 4,
            timestamp: Timestamp::now(),
            event: Event::ApprovalRequested {
                request: approval_request(ApprovalTrigger::ModelToolApproval),
            },
        }));
        assert_eq!(
            store.sessions[&session_id].approvals[0].trigger,
            ApprovalTrigger::ModelToolApproval
        );
    }

    #[test]
    fn v6_internal_title_and_checkpoint_events_render_only_safe_metadata() {
        let session_id = SessionId::new_v7();
        let invocation_id = InternalAgentInvocationId::new_v7();
        let internal_run_id = InternalAgentRunId::new_v7();
        let limit = SummaryByteLimit::new(1024).expect("summary limit");
        let checkpoint = InternalSummaryCheckpoint::new(
            "sentinel-hidden-summary".into(),
            invocation_id,
            internal_run_id,
            limit,
        )
        .expect("checkpoint");
        let commit = ContextCheckpointCommit::new(
            ContextCheckpoint::InternalSummary { checkpoint },
            ContextCheckpointBoundaries {
                source_from_seq: 1,
                source_through_seq: 2,
                input_through_seq: 2,
                prior_checkpoint_seq: None,
            },
            ContextCheckpointBudgets {
                context_limit_tokens: 4096,
                trigger_tokens: 3000,
                target_tokens: 1500,
                input_tokens_before: 3200,
                input_tokens_after: 1400,
                max_summary_bytes: limit,
            },
        )
        .expect("checkpoint commit");
        let events = [
            Event::InternalAgentStarted {
                invocation_id,
                internal_run_id,
                kind: InternalAgentKind::ContextCompaction,
                backend: InternalAgentBackend::Builtin {
                    name: "safe-compactor".into(),
                    revision: "v1".into(),
                },
                call: SafeInternalAgentCall {
                    name: "compact".into(),
                    input_summary: "safe bounded input".into(),
                    input_digest: Sha256Digest::of_bytes(b"private prompt bytes"),
                },
            },
            Event::InternalAgentCompleted {
                invocation_id,
                internal_run_id,
                kind: InternalAgentKind::ContextCompaction,
                result: SafeInternalAgentResult {
                    output_summary: "safe bounded output".into(),
                    output_digest: Sha256Digest::of_bytes(b"private result bytes"),
                },
            },
            Event::ContextCheckpointCommitted { commit },
            Event::SessionTitleCommitted {
                input_through_seq: 2,
                commit: SessionTitleCommit::FallbackSet {
                    title: SessionTitle::new("Safe title").expect("title"),
                },
            },
        ];
        let mut store = StateStore::default();
        for (index, event) in events.into_iter().enumerate() {
            assert!(store.apply_event(EventEnvelope {
                schema_version: EventSchemaVersion::current(),
                session_id,
                run_id: None,
                seq: index as u64 + 1,
                timestamp: Timestamp::now(),
                event,
            }));
        }
        let rendered = store.sessions[&session_id]
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Event { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("safe bounded input"));
        assert!(rendered.contains("safe bounded output"));
        assert!(rendered.contains("context checkpoint committed"));
        assert!(rendered.contains("Safe title"));
        assert!(!rendered.contains("sentinel-hidden-summary"));
        assert!(!rendered.contains("private prompt bytes"));
        assert!(!rendered.contains("private result bytes"));
    }

    #[test]
    fn model_warnings_are_distinct_items_that_identify_the_owning_model() {
        let session_id = SessionId::new_v7();
        let model = ModelRef {
            name: "primary".into(),
            provider_id: "provider".into(),
            model_id: "model".into(),
            adapter_id: "adapter".into(),
        };
        let mut store = StateStore::default();
        assert!(store.apply_event(event(
            session_id,
            1,
            Event::ModelTurnCommitted {
                model,
                input_through_seq: 0,
                turn: PersistedModelTurn {
                    content: Vec::new(),
                    provider_options: Default::default(),
                    finish_reason: ModelFinishReason::Stop,
                    usage: Usage {
                        input_tokens: None,
                        input_tokens_no_cache: None,
                        input_tokens_cache_read: None,
                        input_tokens_cache_write: None,
                        output_tokens: None,
                        output_tokens_text: None,
                        output_tokens_reasoning: None,
                    },
                    response_metadata: Default::default(),
                    provider_metadata: Default::default(),
                    warnings: vec!["context near limit".into()],
                    native_replay: None,
                },
            },
        )));
        let transcript = &store.sessions[&session_id].transcript;
        assert!(matches!(transcript[0], TranscriptItem::Event { .. }));
        let TranscriptItem::Event {
            level: super::EventLevel::Warning,
            text,
            ..
        } = &transcript[1]
        else {
            panic!("model warning renders as a warning-level event row");
        };
        assert!(text.contains("context near limit"));
        assert!(text.contains("primary (provider/model, adapter)"));
    }

    #[test]
    fn diagnostic_classification_is_exact_for_every_reduced_event_kind() {
        let session_id = SessionId::new_v7();
        let model = ModelRef {
            name: "primary".into(),
            provider_id: "provider".into(),
            model_id: "model".into(),
            adapter_id: "adapter".into(),
        };
        let usage = Usage {
            input_tokens: None,
            input_tokens_no_cache: None,
            input_tokens_cache_read: None,
            input_tokens_cache_write: None,
            output_tokens: None,
            output_tokens_text: None,
            output_tokens_reasoning: None,
        };
        let turn = |warnings: Vec<String>| PersistedModelTurn {
            content: Vec::new(),
            provider_options: Default::default(),
            finish_reason: ModelFinishReason::Stop,
            usage: usage.clone(),
            response_metadata: Default::default(),
            provider_metadata: Default::default(),
            warnings,
            native_replay: None,
        };
        let cases: Vec<(Event, Vec<super::EventLevel>)> = vec![
            (
                Event::ModelReplayEvaluated {
                    model: model.clone(),
                    decisions: vec![
                        ReplayDecision {
                            history_index: 0,
                            disposition: ReplayDisposition::Replayed,
                        },
                        ReplayDecision {
                            history_index: 1,
                            disposition: ReplayDisposition::DiscardedInvalidPayload {
                                reason: "truncated".into(),
                            },
                        },
                        ReplayDecision {
                            history_index: 2,
                            disposition: ReplayDisposition::ReconstructedNormalized,
                        },
                    ],
                },
                vec![
                    super::EventLevel::Debug,
                    super::EventLevel::Warning,
                    super::EventLevel::Warning,
                ],
            ),
            (
                Event::ModelTurnCommitted {
                    model: model.clone(),
                    input_through_seq: 0,
                    turn: turn(vec!["careful".into()]),
                },
                vec![super::EventLevel::Info, super::EventLevel::Warning],
            ),
            (
                Event::ModelFallback {
                    from: model.clone(),
                    to: model.clone(),
                    error: ModelErrorSummary {
                        kind: ModelErrorKind::RateLimited,
                        message: "slow".into(),
                        retryable: true,
                        stage: ModelErrorStage::Connect,
                        http_status: None,
                        bytes_received: 0,
                        vendor_code: None,
                        request_id: None,
                        retry_after_ms: None,
                    },
                    attempts: 1,
                },
                vec![super::EventLevel::Warning],
            ),
            (Event::AttemptAbandoned, vec![super::EventLevel::Warning]),
            (
                Event::RunFailed {
                    message: "boom".into(),
                },
                vec![super::EventLevel::Error],
            ),
            (
                Event::RunCancelled { reason: None },
                vec![super::EventLevel::Info],
            ),
            (
                Event::RunInterrupted { reason: None },
                vec![super::EventLevel::Error],
            ),
        ];
        for (case, expected) in cases {
            let mut store = StateStore::default();
            assert!(
                store.apply_event(event(session_id, 1, case)),
                "{expected:?}"
            );
            let levels = store.sessions[&session_id]
                .transcript
                .iter()
                .filter_map(|item| match item {
                    TranscriptItem::Event { level, .. } => Some(*level),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(levels, expected);
        }
    }

    #[test]
    fn gap_snapshot_and_live_delta_preserve_the_snapshot_cursor() {
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        let call_id = cookie_agent_protocol::ToolCallId::new_v7();
        let mut store = StateStore::default();
        store.sessions.entry(session_id).or_default().tools.insert(
            call_id,
            ToolCallState {
                id: call_id,
                tool: "bash".into(),
                arguments: String::new(),
                status: ToolStatus::Running,
                detail: String::new(),
            },
        );

        store.apply_output_gap(OutputGap {
            call_id,
            stream: cookie_agent_protocol::OutputStream::Stdout,
            next_offset: 3,
        });
        store.apply_snapshot(OutputSnapshotEnvelope {
            stream: cookie_agent_protocol::OutputStream::Stdout,
            snapshot: OutputSnapshot {
                call_id,
                start_offset: 3,
                end_offset: 6,
                chunks: vec![OutputDelta {
                    call_id,
                    stream: cookie_agent_protocol::OutputStream::Stdout,
                    byte_offset: 3,
                    data: STANDARD.encode(b"two"),
                }],
            },
        });
        store.apply_output_delta(OutputDelta {
            call_id,
            stream: cookie_agent_protocol::OutputStream::Stdout,
            byte_offset: 6,
            data: STANDARD.encode(b"!"),
        });
        assert_eq!(store.sessions[&session_id].version, 3);

        let output = &store.sessions[&session_id].output[&(call_id, false)];
        assert!(output.has_gap);
        assert_eq!(output.text(), "two!");
        assert_eq!(output.next_offset, 7);
    }
}
