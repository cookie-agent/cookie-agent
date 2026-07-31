//! Disposable UI projections reduced from protocol events.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookiecode_protocol::{
    ApprovalDecision, ApprovalResource, DecisionTrace, Event, EventEnvelope,
    EventSubscriptionMessage, OutputDelta, OutputGap, OutputSnapshotEnvelope, OutputStream, RunId,
    SessionId, ToolCallId,
};

use crate::client::ClientDelivery;

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

/// A pending permission question rendered by the approval modal.
#[derive(Clone, Debug)]
pub struct ApprovalState {
    pub session_id: SessionId,
    pub approval_id: String,
    pub action: String,
    pub resource: String,
    pub suggested_pattern: String,
    pub resources: Vec<ApprovalResource>,
    pub trace: DecisionTrace,
}

/// One rendered conversation item.
#[derive(Clone, Debug)]
pub enum TranscriptItem {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool(ToolCallId),
    Status(String),
}

/// Per-session projection of persisted events and live output.
#[derive(Clone, Debug, Default)]
pub struct SessionState {
    pub generation: u64,
    pub last_seq: u64,
    pub active_run: Option<RunId>,
    pub transcript: Vec<TranscriptItem>,
    pub tools: HashMap<ToolCallId, ToolCallState>,
    pub approvals: Vec<ApprovalState>,
    pub output: HashMap<(ToolCallId, bool), OrderedOutput>,
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
                let scratch = if rebuild {
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
                        event.event,
                    );
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
                    Some(replay) if valid => {
                        self.quarantined_sessions.remove(&session_id);
                        self.abandoned_output
                            .retain(|_, output_session| *output_session != session_id);
                        for call_id in replay.scratch.tools.keys() {
                            self.abandoned_output.remove(call_id);
                        }
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
        reduce_event(state, envelope.session_id, envelope.run_id, envelope.event);
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
            return true;
        }
        false
    }

    fn apply_delta_now(&mut self, delta: OutputDelta) -> bool {
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
            return true;
        }
        false
    }

    fn apply_gap_now(&mut self, gap: OutputGap) -> bool {
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
        }
    }
}

fn reduce_event(
    state: &mut SessionState,
    session_id: SessionId,
    run_id: Option<RunId>,
    event: Event,
) {
    match event {
        Event::RunStarted { input, .. } => {
            state.active_run = run_id;
            state.transcript.push(TranscriptItem::User(input));
        }
        Event::UserInputSubmitted { input } => state.transcript.push(TranscriptItem::User(input)),
        Event::TextDelta { text } => append_delta(&mut state.transcript, text, false),
        Event::ReasoningDelta { text } => append_delta(&mut state.transcript, text, true),
        Event::ToolCallStarted {
            tool_call_id,
            tool,
            arguments,
            ..
        } => {
            state.tools.insert(
                tool_call_id,
                ToolCallState {
                    id: tool_call_id,
                    tool,
                    arguments: arguments.to_string(),
                    status: ToolStatus::Running,
                    detail: String::new(),
                },
            );
            state.transcript.push(TranscriptItem::Tool(tool_call_id));
        }
        Event::ToolCallProgress {
            tool_call_id,
            message,
        } => {
            if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                tool.detail = message;
            }
        }
        Event::ToolCallCompleted {
            tool_call_id,
            result,
        } => {
            if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                tool.status = ToolStatus::Completed;
                tool.detail = result.content;
            }
        }
        Event::ToolCallFailed {
            tool_call_id,
            message,
        } => {
            if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                tool.status = ToolStatus::Failed;
                tool.detail = message;
            }
        }
        Event::ApprovalRequested {
            approval_id,
            action,
            resource,
            suggested_pattern,
            resources,
            decision_trace,
        } => state.approvals.push(ApprovalState {
            session_id,
            approval_id,
            action: format!("{action:?}").to_lowercase(),
            resource,
            suggested_pattern,
            resources,
            trace: decision_trace,
        }),
        Event::ApprovalResolved {
            approval_id,
            decision,
            ..
        } => {
            state
                .approvals
                .retain(|approval| approval.approval_id != approval_id);
            state.transcript.push(TranscriptItem::Status(format!(
                "approval {approval_id}: {}",
                decision_label(decision)
            )));
        }
        Event::RunCompleted { .. }
        | Event::RunFailed { .. }
        | Event::RunCancelled { .. }
        | Event::RunInterrupted { .. } => {
            state.active_run = None;
            state.approvals.clear();
        }
        Event::SessionCreated { .. }
        | Event::UserInputApplied { .. }
        | Event::ToolStdinSubmitted { .. }
        | Event::ToolCallLinked { .. }
        | Event::AttemptAbandoned
        | Event::TurnOpaque { .. }
        | Event::ModelFallback { .. }
        | Event::UsageReported { .. } => {}
    }
}

fn append_delta(items: &mut Vec<TranscriptItem>, text: String, reasoning: bool) {
    let last = items.last_mut();
    match (reasoning, last) {
        (false, Some(TranscriptItem::Assistant(existing))) => existing.push_str(&text),
        (true, Some(TranscriptItem::Reasoning(existing))) => existing.push_str(&text),
        (false, _) => items.push(TranscriptItem::Assistant(text)),
        (true, _) => items.push(TranscriptItem::Reasoning(text)),
    }
}

fn decision_label(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::Once => "once",
        ApprovalDecision::Always => "always",
        ApprovalDecision::Reject => "rejected",
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
