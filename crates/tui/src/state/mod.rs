//! Disposable UI projections reduced from protocol-10 stored events.
//!
//! Assistant attribution is derived only from the frozen `RunStarted` plus
//! `ModelAttemptStarted`/`ModelTurnCommitted` ownership — never from the
//! current picker, live agent files, or provider configuration. The visible
//! assistant header projects the exact canonical `Agent • Model[variant]`.

mod runtime;

pub use runtime::{EMPTY_RUNTIME_GUIDANCE, RuntimePhase, RuntimeState};

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::{
    AgentId, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation, ApprovalFinalOutcome,
    ApprovalId, ApprovalRecord, ApprovalRequest, ApprovalStatus, ApprovalTrigger,
    AssistantToolCallRef, AttemptId, EventPayload, EventSubscriptionMessage, GoalId,
    GoalReminderIdentity, GoalState, GoalStatus, ModelErrorSummary, OperationFingerprint,
    OutputDelta, OutputGap, OutputSnapshotEnvelope, OutputStream, PersistedModelTurn,
    PreparedApprovalResource, PreparedCapabilityLifetime, ProducerDeliveryMode,
    ProducerIdempotencyKey, ProducerMessageId, ProducerOwner, ReplayDecision, ReplayDisposition,
    ResolvedModelRef, RunId, SafeCode, SessionId, SessionTitleChange, Sha256Digest, StoredEvent,
    ToolAttachment, ToolCallId, ToolTerminationOutcome, Usage, VariantId,
};
use serde::Serialize;

use crate::{client::ClientDelivery, markdown::MarkdownDocument};

/// The visible state of a tool invocation, reduced from the exact protocol-10
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
    pub has_output_chunks: bool,
}

impl ToolCallState {
    /// The exact compact title: the persisted sanitized tool title plus its
    /// persisted sanitized display argument, never reparsed from raw input.
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
/// exact protocol-10 attempt and turn ownership events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenAssistantAttribution {
    pub agent: AgentId,
    pub resolved_model: ResolvedModelRef,
}

impl FrozenAssistantAttribution {
    /// The exact visible header `<agent-id> • <provider>/<model-id>[<variant>]`.
    pub fn header(&self) -> String {
        format!(
            "{} • {}[{}]",
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

/// One steered message the engine admitted into its pending-input lane but
/// has not yet promoted to the model-facing log. Pure event reduction: the
/// lane is exactly what `UserInputAdmitted`/`UserInputSubmitted` and the recall
/// events describe, so replays rebuild it identically.
#[derive(Clone, Debug)]
pub struct PendingInput {
    pub text: String,
    pub admission_seq: u64,
    /// Durable admission timestamp from the admitting event.
    pub admitted_at: jiff::Timestamp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProducerMessageStatus {
    Pending,
    Admitted,
    Claimed,
    Consumed,
    Discarded,
}

/// One rendered conversation item.
#[derive(Clone, Debug)]
pub enum TranscriptItem {
    User {
        id: u64,
        version: u64,
        text: String,
        /// Physical sequence of the `UserInputSubmitted` event that created
        /// the row: the revert/fork menu targets it with `through_seq`.
        seq: u64,
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
    /// A committed context checkpoint rendered inline at its durable event.
    Compaction {
        id: u64,
        version: u64,
        seq: u64,
        commit: cookie_agent_protocol::ContextCheckpointCommit,
    },
    /// A plugin-injected model message rendered inline at its durable event.
    PluginMessage {
        id: u64,
        version: u64,
        seq: u64,
        role: cookie_agent_protocol::ExtensionMessageRole,
        input: String,
    },
    Goal {
        id: u64,
        seq: u64,
        goal: GoalState,
    },
    ProducerMessage {
        id: u64,
        seq: u64,
        /// Durable timestamp of the accepting event, retained independently
        /// from the pruned generation-timing index for stable queue age.
        accepted_at: jiff::Timestamp,
        message_id: ProducerMessageId,
        producer_owner: ProducerOwner,
        mode: ProducerDeliveryMode,
        body: String,
        reminder: Option<GoalReminderIdentity>,
        status: ProducerMessageStatus,
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
    /// Inline producer change within one run-scoped assistant item.
    Attribution {
        resolved_model: ResolvedModelRef,
    },
    /// A durable tool placeholder from committed turn content, carrying the
    /// exact content index. A started tool replaces its placeholder through
    /// `owner.content_index`; an unstarted placeholder renders its committed
    /// call.
    CommittedTool {
        turn_seq: u64,
        content_index: u32,
    },
    /// A durable assistant media part at its exact committed content index.
    MediaFile {
        turn_seq: u64,
        content_index: u32,
        file: cookie_agent_protocol::PersistedFilePart,
    },
}

impl AssistantChild {
    pub fn id(&self) -> u64 {
        match self {
            Self::Text { id, .. } | Self::Thinking { id, .. } => *id,
            Self::Tool { .. }
            | Self::Attribution { .. }
            | Self::CommittedTool { .. }
            | Self::MediaFile { .. } => 0,
        }
    }

    pub fn version(&self) -> u64 {
        match self {
            Self::Text { version, .. } | Self::Thinking { version, .. } => *version,
            Self::Tool { .. }
            | Self::Attribution { .. }
            | Self::CommittedTool { .. }
            | Self::MediaFile { .. } => 0,
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
    /// Durable timestamp of the event that opened this part. Sealing a
    /// thinking part derives its "thought for Ns" duration from event
    /// timestamps, so replays reproduce the original elapsed time.
    opened_at: jiff::Timestamp,
}

impl TranscriptItem {
    pub fn id(&self) -> u64 {
        match self {
            Self::User { id, .. }
            | Self::Assistant { id, .. }
            | Self::Event { id, .. }
            | Self::Compaction { id, .. }
            | Self::PluginMessage { id, .. }
            | Self::Goal { id, .. }
            | Self::ProducerMessage { id, .. } => *id,
        }
    }

    pub fn version(&self) -> u64 {
        match self {
            Self::User { version, .. }
            | Self::Assistant { version, .. }
            | Self::Event { version, .. }
            | Self::Compaction { version, .. }
            | Self::PluginMessage { version, .. } => *version,
            Self::Goal { .. } => 0,
            Self::ProducerMessage { status, .. } => match status {
                ProducerMessageStatus::Pending => 0,
                ProducerMessageStatus::Admitted => 1,
                ProducerMessageStatus::Claimed => 2,
                ProducerMessageStatus::Consumed => 3,
                ProducerMessageStatus::Discarded => 4,
            },
        }
    }

    #[cfg(test)]
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            id: 1,
            version: 0,
            text: text.into(),
            // Layout fixtures never target the row; any plausible physical
            // sequence (SessionCreated owns 1) keeps the field inhabited.
            seq: 2,
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

/// The single assistant transcript item accumulating attempts for one run.
#[derive(Clone, Debug)]
pub(crate) struct RunAssistantProjection {
    pub(crate) run_id: RunId,
    pub(crate) item_id: u64,
    pub(crate) committed_prefix: usize,
    pub(crate) current_model: ResolvedModelRef,
}

/// Generation metrics accumulated across one assistant block's committed
/// turns: output tokens and generation wall time summed over exactly the
/// turns with a known positive span (never mixing measured and unmeasured
/// generation), plus the total context occupied at the end of the last
/// committed turn (its `input_tokens + output_tokens`).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AssistantTurnMetrics {
    pub(crate) timed_output_tokens: u64,
    pub(crate) generation: Duration,
    pub(crate) timed_turns: u32,
    pub(crate) context_tokens: Option<u64>,
    pub(crate) estimated_cost_pico_usd: Option<u64>,
    cost_unpriced: bool,
}

impl AssistantTurnMetrics {
    fn record_cost(&mut self, cost: Option<u64>) {
        if self.cost_unpriced {
            return;
        }
        let Some(cost) = cost else {
            self.estimated_cost_pico_usd = None;
            self.cost_unpriced = true;
            return;
        };
        self.estimated_cost_pico_usd = self.estimated_cost_pico_usd.unwrap_or(0).checked_add(cost);
        if self.estimated_cost_pico_usd.is_none() {
            self.cost_unpriced = true;
        }
    }
}

/// A tool start buffered until its committed placeholder exists, linked by
/// the owning turn's exact content index.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingToolRow {
    turn_seq: u64,
    content_index: u32,
    call_id: ToolCallId,
}

#[derive(Clone, Debug)]
pub(crate) struct IndexedToolCall {
    name: SafeCode,
    arguments: String,
}

/// Projection-only identity composed exclusively from frozen, secret-safe
/// protocol values. History indices are deliberately excluded so one logical
/// compatibility transition warns once without altering durable evidence.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ReplayContextTransition {
    Adapter {
        found: SafeCode,
        expected: SafeCode,
    },
    ModelSelection {
        found: cookie_agent_protocol::ModelSelection,
        expected: cookie_agent_protocol::ModelSelection,
    },
    Variant {
        found: Option<VariantId>,
        expected: Option<VariantId>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct ProducerMessageProjection {
    transcript_index: usize,
    producer_owner: ProducerOwner,
    reminder: Option<GoalReminderIdentity>,
    accepted_seq: u64,
    admission: Option<(RunId, u64)>,
    claims: HashSet<u64>,
    status: ProducerMessageStatus,
    discarded_seq: Option<u64>,
    consumed_run: Option<RunId>,
    consumption_recorded: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ProducerClaimProjection {
    pub(crate) run_id: RunId,
    pub(crate) message_ids: Vec<ProducerMessageId>,
}

/// Per-session projection of persisted events and live output.
#[derive(Clone, Debug, Default)]
pub struct SessionState {
    /// Changes whenever the visible projection mutates, for UI cache invalidation.
    pub version: u64,
    pub generation: u64,
    pub last_seq: u64,
    /// Creation-event time, used as the deterministic sibling-order fallback.
    pub(crate) created_at: Option<jiff::Timestamp>,
    /// Latest user submission or delegate/steer tool start in this session.
    pub(crate) last_agent_activity: Option<jiff::Timestamp>,
    pub active_run: Option<RunId>,
    pub cwd_identity: Option<cookie_agent_protocol::CwdIdentity>,
    /// Total context occupied at the end of the latest committed turn
    /// (`input_tokens + output_tokens`); `None` when the turn reported no
    /// usage, so the bottom bar hides its context segment.
    pub context_tokens: Option<u64>,
    /// Latest authoritative session usage cost fetched from the engine.
    /// `None` covers both not-yet-fetched and unpriced usage; both hide the
    /// bottom-bar segment.
    pub estimated_cost_usd: Option<f64>,
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
    pub goal: Option<GoalState>,
    pub transcript: Vec<TranscriptItem>,
    /// The engine's pending-input lane for this session, reduced purely
    /// from admission/promotion/recall events: steered messages the model
    /// has not seen yet. Source of the queue strip between the conversation
    /// pane and the composer.
    pub pending_inputs: VecDeque<PendingInput>,
    /// Runs whose initial (non-lane) input has already been submitted.
    /// Later submissions for the same run are pending-lane promotions.
    pub(crate) initial_input_submitted: HashSet<RunId>,
    /// Pending inputs the engine voided at run end (no per-entry events),
    /// parked here until the UI restores their text into the composer —
    /// user text is never silently lost. Drained by the UI on sight.
    pub voided_inputs: Vec<String>,
    pub(crate) next_transcript_id: u64,
    pub(crate) open_assistant: Option<OpenAssistantProjection>,
    /// Elapsed thinking time per sealed thinking part, keyed by
    /// `(item_id, part_id)` and derived from durable event timestamps.
    pub thinking_durations: HashMap<(u64, u64), Duration>,
    /// Durable event timestamps by sequence, pruned at each committed
    /// turn's input boundary; backs replay-exact generation durations.
    pub(crate) event_timestamps: BTreeMap<u64, jiff::Timestamp>,
    /// Generation metrics per assistant item, accumulated from committed
    /// turns, for the subordinate footer row at the end of the block.
    pub(crate) assistant_metrics: HashMap<u64, AssistantTurnMetrics>,
    pub(crate) open_run_assistant: Option<RunAssistantProjection>,
    pub(crate) attempts: HashMap<AttemptId, AttemptProjection>,
    pub tools: HashMap<ToolCallId, ToolCallState>,
    /// Buffered tool rows awaiting their committed placeholder, keyed by
    /// the owning turn's content index so starts/completions cannot reorder.
    pub(crate) pending_tool_rows: Vec<PendingToolRow>,
    /// Durable tool input indexed from committed turn content:
    /// (model_turn_seq, model_call_id) → arguments JSON, for expanded rows.
    pub(crate) turn_tool_index: HashMap<(u64, String), IndexedToolCall>,
    /// The assistant item owning each committed model-turn sequence.
    pub(crate) turn_items: HashMap<u64, u64>,
    /// User-visible replay compatibility transitions already projected for a
    /// run. Durable replay/reconnect and later tool-loop attempts may repeat
    /// request diagnostics without creating another logical transition.
    pub(crate) replay_context_warnings: HashSet<(RunId, Sha256Digest, ReplayContextTransition)>,
    pub(crate) goal_revisions: HashMap<GoalId, u64>,
    pub(crate) producer_messages: HashMap<ProducerMessageId, ProducerMessageProjection>,
    pub(crate) producer_dedup: HashMap<(ProducerOwner, ProducerIdempotencyKey), ProducerMessageId>,
    pub(crate) producer_claims: HashMap<u64, ProducerClaimProjection>,
    pub(crate) terminal_runs: HashSet<RunId>,
    pub approvals: Vec<ApprovalState>,
    pub output: HashMap<(ToolCallId, bool), OrderedOutput>,
}

impl SessionState {
    pub fn is_open_assistant_part(&self, item_id: u64, part_id: u64) -> bool {
        self.open_assistant
            .is_some_and(|open| open.item_id == item_id && open.part_id == part_id)
    }

    pub fn is_open_thinking(&self, item_id: u64, part_id: u64) -> bool {
        self.open_assistant.is_some_and(|open| {
            open.item_id == item_id
                && open.part_id == part_id
                && open.kind == AssistantPartKind::Thinking
        })
    }

    /// Whether any assistant part is still streaming thinking content.
    pub fn has_open_thinking(&self) -> bool {
        self.open_assistant
            .is_some_and(|open| open.kind == AssistantPartKind::Thinking)
    }

    /// Whether any tool call in this session is still running.
    pub fn has_running_tool(&self) -> bool {
        self.tools
            .values()
            .any(|tool| tool.status == ToolStatus::Running)
    }

    /// The sealed elapsed thinking duration for one part, when known.
    pub fn thinking_duration(&self, item_id: u64, part_id: u64) -> Option<Duration> {
        self.thinking_durations.get(&(item_id, part_id)).copied()
    }
}

/// All currently observed session projections.
#[derive(Clone, Debug, Default)]
pub struct StateStore {
    pub sessions: HashMap<SessionId, SessionState>,
    physical_events: HashMap<SessionId, Vec<StoredEvent>>,
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
    physical_events: Vec<StoredEvent>,
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
                close_open_assistant(&mut scratch, jiff::Timestamp::now());
                self.replays.insert(
                    session_id,
                    ReplayProgress {
                        generation,
                        final_seq,
                        scratch,
                        physical_events: if rebuild {
                            Vec::new()
                        } else {
                            self.physical_events
                                .get(&session_id)
                                .cloned()
                                .unwrap_or_default()
                        },
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
                    replay.physical_events.push(event.clone());
                    if matches!(event.payload, EventPayload::SessionReverted { .. }) {
                        replay.scratch =
                            reduce_session_events(session_id, generation, &replay.physical_events);
                    } else {
                        replay.scratch.last_seq = event.seq;
                        reduce_event(
                            &mut replay.scratch,
                            event.session_id,
                            event.run_id,
                            event.seq,
                            event.timestamp,
                            event.payload,
                        );
                    }
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
                        self.physical_events
                            .insert(session_id, replay.physical_events);
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
            ClientDelivery::PluginEvent(_) => DeliveryOutcome::Applied,
            ClientDelivery::RuntimeChanged(_) => DeliveryOutcome::Applied,
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

    /// Drain a session's voided inputs for restoration into the composer.
    /// Returns them in admission (FIFO) order; empty when nothing is owed.
    pub fn take_voided_inputs(&mut self, session_id: SessionId) -> Vec<String> {
        self.sessions
            .get_mut(&session_id)
            .map(|state| std::mem::take(&mut state.voided_inputs))
            .unwrap_or_default()
    }

    /// Park text as voided for a session (e.g. a recall resolved while a
    /// different session was being viewed); the UI restores it on sight.
    pub fn park_voided_input(&mut self, session_id: SessionId, text: String) {
        if let Some(state) = self.sessions.get_mut(&session_id) {
            state.voided_inputs.push(text);
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
        self.physical_events
            .entry(event.session_id)
            .or_default()
            .push(event.clone());
        if matches!(event.payload, EventPayload::SessionReverted { .. }) {
            let previous_version = state.version;
            *state = reduce_session_events(
                event.session_id,
                generation,
                self.physical_events
                    .get(&event.session_id)
                    .expect("physical event was inserted"),
            );
            state.version = previous_version.max(state.version).wrapping_add(1);
        } else {
            state.last_seq = event.seq;
            reduce_event(
                state,
                event.session_id,
                event.run_id,
                event.seq,
                event.timestamp,
                event.payload,
            );
            state.version = state.version.wrapping_add(1);
        }
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
        self.physical_events.remove(&session_id);
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
        if self.quarantined_sessions.contains(&session_id) || self.replays.contains_key(&session_id)
        {
            return false;
        }
        let state = reduce_session_events(session_id, generation, &events);
        self.physical_events.insert(session_id, events);
        self.sessions.insert(session_id, state);
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
    timestamp: jiff::Timestamp,
    payload: EventPayload,
) {
    // Every durable timestamp is indexed by sequence so a committed turn
    // can measure its generation wall time from the exact event that closed
    // its input window — replay-exact, never render-time wall clock.
    state.event_timestamps.insert(sequence, timestamp);
    match payload {
        EventPayload::GoalActivated {
            goal_id,
            objective,
            revision,
            // Future-run selection does not change frozen producing attribution.
            selection: _,
        } => {
            let replaceable = state
                .goal
                .as_ref()
                .is_none_or(|goal| goal_status_is_terminal(goal.status));
            let distinct = state
                .goal
                .as_ref()
                .is_none_or(|goal| goal.goal_id != goal_id);
            let unseen = !state.goal_revisions.contains_key(&goal_id);
            if objective.trim().is_empty() || !replaceable || !distinct || !unseen {
                return;
            }
            let goal = GoalState {
                goal_id,
                objective,
                status: GoalStatus::Active,
                items: Vec::new(),
                revision,
            };
            state.goal_revisions.insert(goal_id, revision);
            state.goal = Some(goal.clone());
            push_item(state, |id| TranscriptItem::Goal {
                id,
                seq: sequence,
                goal,
            });
        }
        EventPayload::GoalChecklistRevised {
            goal_id,
            items,
            revision,
        } => {
            let valid_items = items.iter().all(|item| !item.description.trim().is_empty());
            let valid = valid_items
                && state.goal.as_ref().is_some_and(|goal| {
                    goal.goal_id == goal_id
                        && !goal_status_is_terminal(goal.status)
                        && revision > goal.revision
                });
            if !valid {
                return;
            }
            let goal = state.goal.as_mut().expect("validated current goal");
            goal.items = items;
            goal.revision = revision;
            state.goal_revisions.insert(goal_id, revision);
            let snapshot = goal.clone();
            push_item(state, |id| TranscriptItem::Goal {
                id,
                seq: sequence,
                goal: snapshot,
            });
        }
        EventPayload::GoalLifecycleChanged {
            goal_id,
            status,
            revision,
            selection,
        } => {
            let valid = state.goal.as_ref().is_some_and(|goal| {
                goal.goal_id == goal_id
                    && revision > goal.revision
                    && valid_goal_lifecycle_change(goal, status)
                    && (selection.is_none() || status == GoalStatus::Active)
            });
            if !valid {
                return;
            }
            let goal = state.goal.as_mut().expect("validated current goal");
            goal.status = status;
            goal.revision = revision;
            state.goal_revisions.insert(goal_id, revision);
            let snapshot = goal.clone();
            push_item(state, |id| TranscriptItem::Goal {
                id,
                seq: sequence,
                goal: snapshot,
            });
        }
        EventPayload::ProducerMessageAccepted {
            message_id,
            producer_owner,
            mode,
            idempotency_key,
            body,
            reminder,
        } => {
            if !valid_producer_reminder_owner(&producer_owner, reminder.as_ref())
                || state.producer_messages.contains_key(&message_id)
                || state
                    .producer_dedup
                    .contains_key(&(producer_owner.clone(), idempotency_key.clone()))
            {
                return;
            }
            let transcript_index = state.transcript.len();
            push_item(state, |id| TranscriptItem::ProducerMessage {
                id,
                seq: sequence,
                accepted_at: timestamp,
                message_id,
                producer_owner: producer_owner.clone(),
                mode,
                body,
                reminder,
                status: ProducerMessageStatus::Pending,
            });
            state.producer_messages.insert(
                message_id,
                ProducerMessageProjection {
                    transcript_index,
                    producer_owner: producer_owner.clone(),
                    reminder,
                    accepted_seq: sequence,
                    admission: None,
                    claims: HashSet::new(),
                    status: ProducerMessageStatus::Pending,
                    discarded_seq: None,
                    consumed_run: None,
                    consumption_recorded: false,
                },
            );
            state
                .producer_dedup
                .insert((producer_owner, idempotency_key), message_id);
        }
        EventPayload::ProducerMessageAdmitted { message_id } => {
            let Some(run_id) = run_id else {
                return;
            };
            if state.terminal_runs.contains(&run_id) {
                return;
            }
            let Some(message) = state.producer_messages.get(&message_id) else {
                return;
            };
            let replaceable = message.admission.is_some_and(|(prior_run, _)| {
                prior_run != run_id && state.terminal_runs.contains(&prior_run)
            });
            if message.consumed_run.is_some()
                || message.discarded_seq.is_some()
                || (message.admission.is_some() && !replaceable)
            {
                return;
            }
            let message = state
                .producer_messages
                .get_mut(&message_id)
                .expect("validated producer message");
            message.admission = Some((run_id, sequence));
            state.initial_input_submitted.insert(run_id);
            let status = if message.claims.is_empty() {
                ProducerMessageStatus::Admitted
            } else {
                ProducerMessageStatus::Claimed
            };
            update_producer_message_status(state, message_id, status);
        }
        EventPayload::ProducerMessagesClaimed { message_ids } => {
            let Some(run_id) = run_id else {
                return;
            };
            let unique = message_ids.iter().copied().collect::<HashSet<_>>();
            let valid = !message_ids.is_empty()
                && unique.len() == message_ids.len()
                && !state.producer_claims.contains_key(&sequence)
                && message_ids.iter().all(|message_id| {
                    state
                        .producer_messages
                        .get(message_id)
                        .is_some_and(|message| {
                            message.accepted_seq < sequence
                                && message.consumed_run.is_none()
                                && message.discarded_seq.is_none()
                                && message.admission.is_some_and(
                                    |(admission_run, admission_seq)| {
                                        admission_run == run_id && admission_seq < sequence
                                    },
                                )
                        })
                });
            if !valid {
                return;
            }
            for message_id in &message_ids {
                state
                    .producer_messages
                    .get_mut(message_id)
                    .expect("validated producer message")
                    .claims
                    .insert(sequence);
                update_producer_message_status(state, *message_id, ProducerMessageStatus::Claimed);
            }
            state.producer_claims.insert(
                sequence,
                ProducerClaimProjection {
                    run_id,
                    message_ids,
                },
            );
        }
        EventPayload::ProducerMessagesReleased { claim_seq } => {
            let Some(run_id) = run_id else {
                return;
            };
            let Some(claim) = state.producer_claims.get(&claim_seq) else {
                return;
            };
            if claim_seq == 0 || claim.run_id != run_id {
                return;
            }
            let message_ids = claim.message_ids.clone();
            for message_id in &message_ids {
                let Some(message) = state.producer_messages.get_mut(message_id) else {
                    continue;
                };
                message.claims.remove(&claim_seq);
                let status = producer_message_status(message);
                update_producer_message_status(state, *message_id, status);
            }
            state.producer_claims.remove(&claim_seq);
        }
        EventPayload::ProducerMessageConsumed {
            message_id,
            run_id: consumed_run,
        } => {
            let valid = run_id == Some(consumed_run)
                && state
                    .producer_messages
                    .get(&message_id)
                    .is_some_and(|message| {
                        message.admission.map(|(run, _)| run) == Some(consumed_run)
                            && message.consumed_run == Some(consumed_run)
                            && !message.consumption_recorded
                    });
            if valid {
                state
                    .producer_messages
                    .get_mut(&message_id)
                    .expect("validated producer message")
                    .consumption_recorded = true;
            }
        }
        EventPayload::ProducerMessageDiscarded {
            message_id,
            reminder,
            producer_owner,
        } => {
            let valid = state
                .producer_messages
                .get(&message_id)
                .is_some_and(|message| {
                    let identity_matches = producer_owner
                        .as_ref()
                        .is_some_and(|owner| owner == &message.producer_owner)
                        && match &message.producer_owner {
                            ProducerOwner::Goal { .. } => {
                                reminder.as_ref().is_some_and(|identity| {
                                    message.reminder.as_ref() == Some(identity)
                                })
                            }
                            _ => reminder
                                .as_ref()
                                .is_none_or(|identity| message.reminder.as_ref() == Some(identity)),
                        };
                    let legacy_matches = producer_owner.is_none()
                        && reminder
                            .as_ref()
                            .is_some_and(|identity| message.reminder.as_ref() == Some(identity));
                    (identity_matches || legacy_matches)
                        && message.consumed_run.is_none()
                        && message.claims.is_empty()
                });
            if valid {
                let message = state
                    .producer_messages
                    .get_mut(&message_id)
                    .expect("validated producer message");
                if message.discarded_seq.is_none() {
                    message.discarded_seq = Some(sequence);
                    update_producer_message_status(
                        state,
                        message_id,
                        ProducerMessageStatus::Discarded,
                    );
                }
            }
        }
        EventPayload::RunStarted {
            agent,
            selected_suffix,
            ..
        } => {
            close_open_assistant(state, timestamp);
            state.open_run_assistant = None;
            state.active_run = run_id;
            state.run_agent = Some(agent.agent.clone());
            state.run_snapshot = Some(agent);
            state.run_selected_suffix = Some(selected_suffix);
        }
        EventPayload::UserInputAdmitted { input } => {
            close_open_assistant(state, timestamp);
            state.last_agent_activity = Some(timestamp);
            state.pending_inputs.push_back(PendingInput {
                text: input,
                admission_seq: sequence,
                admitted_at: timestamp,
            });
        }
        EventPayload::UserInputSubmitted { input } => {
            close_open_assistant(state, timestamp);
            state.last_agent_activity = Some(timestamp);
            if run_id.is_some_and(|run_id| !state.initial_input_submitted.insert(run_id)) {
                // Promotion: only a submission after the run's initial input
                // graduates a lane entry — the oldest, strictly positionally,
                // exactly like the engine's own replay.
                state.pending_inputs.pop_front();
            }
            push_item(state, |id| TranscriptItem::User {
                id,
                version: 0,
                text: input,
                seq: sequence,
            });
        }
        EventPayload::UserInputRecalled { .. } => {
            close_open_assistant(state, timestamp);
            // The engine withdrew the newest pending entry positionally;
            // its text comes back through the recall RPC result and is
            // never consulted here.
            state.pending_inputs.pop_back();
        }
        EventPayload::UserInputRecalledV2 { user_input_seq, .. } => {
            close_open_assistant(state, timestamp);
            if let Some(position) = state
                .pending_inputs
                .iter()
                .position(|pending| pending.admission_seq == user_input_seq)
            {
                state.pending_inputs.remove(position);
            }
        }
        EventPayload::ModelAttemptStarted {
            attempt_id,
            resolved_model,
            ..
        } => {
            close_open_assistant(state, timestamp);
            // Attempt attribution is frozen: the producing agent comes from
            // the owning `RunStarted`, the exact resolved model from this
            // event — never from the current picker or live configuration.
            let agent = state
                .run_agent
                .clone()
                .unwrap_or_else(|| AgentId::new("unknown").expect("static agent id"));
            let item_id = if let Some(run_id) = run_id {
                if let Some(projection) = state
                    .open_run_assistant
                    .as_ref()
                    .filter(|projection| projection.run_id == run_id)
                {
                    let item_id = projection.item_id;
                    let changed = projection.current_model != resolved_model;
                    if changed {
                        append_attribution(state, item_id, resolved_model.clone());
                    }
                    state
                        .open_run_assistant
                        .as_mut()
                        .expect("run projection remains open")
                        .current_model = resolved_model;
                    item_id
                } else {
                    let item_id = open_assistant_item(
                        state,
                        FrozenAssistantAttribution {
                            agent,
                            resolved_model: resolved_model.clone(),
                        },
                    );
                    state.open_run_assistant = Some(RunAssistantProjection {
                        run_id,
                        item_id,
                        committed_prefix: 0,
                        current_model: resolved_model,
                    });
                    item_id
                }
            } else {
                open_assistant_item(
                    state,
                    FrozenAssistantAttribution {
                        agent,
                        resolved_model,
                    },
                )
            };
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
            append_assistant_delta(
                state,
                item_id,
                sequence,
                text,
                AssistantPartKind::Text,
                timestamp,
            );
        }
        EventPayload::ReasoningDelta { attempt_id, text } => {
            let Some(item_id) = state
                .attempts
                .get(&attempt_id)
                .map(|attempt| attempt.item_id)
            else {
                return;
            };
            append_assistant_delta(
                state,
                item_id,
                sequence,
                text,
                AssistantPartKind::Thinking,
                timestamp,
            );
        }
        EventPayload::AttemptAbandoned { attempt_id } => {
            close_open_assistant(state, timestamp);
            if let Some(attempt) = state.attempts.remove(&attempt_id) {
                let committed_prefix = state
                    .open_run_assistant
                    .as_ref()
                    .filter(|projection| projection.item_id == attempt.item_id)
                    .map(|projection| projection.committed_prefix);
                if let Some(committed_prefix) = committed_prefix {
                    prune_abandoned_attempt(state, attempt.item_id, committed_prefix);
                }
            }
            push_event(state, EventLevel::Warning, "model attempt abandoned".into());
        }
        EventPayload::ModelTurnCommitted {
            attempt_id,
            model_turn_seq,
            resolved_model,
            input_through_seq,
            turn,
            warnings,
            ..
        } => {
            if let Some(run_id) = run_id {
                consume_producer_messages_through(state, run_id, input_through_seq);
            }
            close_open_assistant(state, timestamp);
            // The context the turn left behind: what it consumed plus what
            // it generated. A usage-less turn clears the display.
            state.context_tokens = match (turn.usage.input_tokens, turn.usage.output_tokens) {
                (Some(input), Some(output)) => Some(input.saturating_add(output)),
                _ => None,
            };
            // Generation wall time: the durable span between the event that
            // closed this turn's input window and the commit itself.
            // Missing or clock-skewed (negative/zero) spans contribute
            // nothing, so unmeasured generation never dilutes the rate.
            let generation = state
                .event_timestamps
                .get(&input_through_seq)
                .and_then(|started| {
                    std::time::Duration::try_from(timestamp.duration_since(*started)).ok()
                })
                .filter(|duration| !duration.is_zero());
            // Later commits always close their inputs at a newer sequence,
            // so older timestamps are dead weight.
            state.event_timestamps = state.event_timestamps.split_off(&input_through_seq);
            // The committed turn is the canonical boundary: every
            // text/thinking/tool child is rebuilt in exact
            // `PersistedModelTurn.content` order, preserving multiple
            // segments and content indices. Tool parts become committed
            // placeholders linked by `owner.content_index` when their start
            // event arrives.
            if let Some(projection) = state.attempts.get(&attempt_id) {
                let item_id = projection.item_id;
                let metrics = state.assistant_metrics.entry(item_id).or_default();
                if let Some(generation) = generation {
                    metrics.timed_output_tokens = metrics
                        .timed_output_tokens
                        .saturating_add(turn.usage.output_tokens.unwrap_or(0));
                    metrics.generation += generation;
                    metrics.timed_turns = metrics.timed_turns.saturating_add(1);
                }
                // The context the block now holds: what the turn consumed
                // plus everything it generated. Either side missing makes
                // the total unknowable, so the footer stays hidden.
                if let (Some(input), Some(output)) =
                    (turn.usage.input_tokens, turn.usage.output_tokens)
                {
                    metrics.context_tokens = Some(input.saturating_add(output));
                }
                mark_committed(state, item_id, model_turn_seq, &resolved_model);
                index_turn_tool_content(state, model_turn_seq, &turn);
                rebuild_committed_children(state, item_id, model_turn_seq, sequence, &turn);
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
            close_open_assistant(state, timestamp);
            // Incompatible replay/cache discards are one WARNING per logical
            // run transition. Reconstruction is the expected consequence and
            // remains DEBUG, as do routine replay details.
            if ordered_decisions.is_empty() {
                push_event(
                    state,
                    EventLevel::Info,
                    format!(
                        "model {} replay · no history entries",
                        render_model(&resolved_model)
                    ),
                );
            }
            for source in &ordered_decisions {
                let (level, decision) = render_replay_decision(source);
                if level == EventLevel::Warning
                    && let Some(run_id) = run_id
                    && let Some(key) = replay_context_warning_key(&resolved_model, source)
                    && !state.replay_context_warnings.insert(key.with_run(run_id))
                {
                    continue;
                }
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
            close_open_assistant(state, timestamp);
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
            close_open_assistant(state, timestamp);
            if state
                .turn_tool_index
                .get(&(
                    start.owner.model_turn_seq,
                    start.owner.model_call_id.as_str().to_owned(),
                ))
                .is_some_and(|tool| {
                    matches!(tool.name.as_str(), "delegate_subagent" | "steer_subagent")
                })
            {
                state.last_agent_activity = Some(timestamp);
            }
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
                    has_output_chunks: false,
                },
            );
            place_tool_rows(state);
        }
        EventPayload::ToolCallProgress {
            tool_call_id,
            message,
            output_chunk,
        } => {
            if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                if let Some(output_chunk) = output_chunk {
                    if !tool.has_output_chunks {
                        tool.detail.clear();
                        tool.has_output_chunks = true;
                    }
                    tool.detail.push_str(output_chunk.as_str());
                } else if tool.has_output_chunks {
                    tool.detail.push('\n');
                    tool.detail.push_str(message.as_str());
                } else {
                    tool.detail = message.to_string();
                }
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
                    &result.additional_messages,
                ),
                (_, Some(error)) => error.message.to_string(),
                _ => String::new(),
            };
            if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                tool.status = status;
                tool.detail = detail;
                tool.has_output_chunks = false;
            }
            state.output.remove(&(tool_call_id, false));
            state.output.remove(&(tool_call_id, true));
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
            ..
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
                EventLevel::Info,
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
            close_open_assistant(state, timestamp);
            state.open_run_assistant = None;
            state.active_run = None;
            state.attempts.clear();
            state.pending_tool_rows.clear();
            void_pending_inputs(state);
            state.approvals.clear();
            if let Some(run_id) = run_id {
                state.terminal_runs.insert(run_id);
            }
            push_event(state, EventLevel::Info, "run completed".into());
        }
        EventPayload::RunFailed { error } => {
            close_open_assistant(state, timestamp);
            state.open_run_assistant = None;
            state.active_run = None;
            state.attempts.clear();
            state.pending_tool_rows.clear();
            void_pending_inputs(state);
            state.approvals.clear();
            if let Some(run_id) = run_id {
                state.terminal_runs.insert(run_id);
            }
            push_event(state, EventLevel::Error, format!("run failed: {error}"));
        }
        EventPayload::RunCancelled { reason } => {
            close_open_assistant(state, timestamp);
            state.open_run_assistant = None;
            state.active_run = None;
            state.attempts.clear();
            state.pending_tool_rows.clear();
            void_pending_inputs(state);
            state.approvals.clear();
            if let Some(run_id) = run_id {
                state.terminal_runs.insert(run_id);
            }
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
            close_open_assistant(state, timestamp);
            state.open_run_assistant = None;
            state.active_run = None;
            state.attempts.clear();
            state.pending_tool_rows.clear();
            void_pending_inputs(state);
            state.approvals.clear();
            if let Some(run_id) = run_id {
                state.terminal_runs.insert(run_id);
            }
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
            push_item(state, |id| TranscriptItem::Compaction {
                id,
                version: 0,
                seq: sequence,
                commit,
            })
        }
        EventPayload::ToolOutputElided {
            tool_call_id,
            original_bytes,
            retained,
        } => push_event(
            state,
            EventLevel::Debug,
            format!(
                "tool output {tool_call_id} elided ({original_bytes} bytes retained at {})",
                retained.uri
            ),
        ),
        EventPayload::ContextRehydrated { files } => push_event(
            state,
            EventLevel::Info,
            format!("rehydrated {} recently read file(s)", files.len()),
        ),
        EventPayload::DelegateQueued {
            session_id,
            position,
        } => push_event(
            state,
            EventLevel::Info,
            position.map_or_else(
                || format!("subagent {session_id} queued"),
                |position| format!("subagent {session_id} queued at position {position}"),
            ),
        ),
        EventPayload::DelegateFinished {
            session_id,
            status,
            total_lines,
            ..
        }
        | EventPayload::DelegateFinishedV2 {
            session_id,
            status,
            total_lines,
            ..
        } => push_event(
            state,
            if matches!(status, cookie_agent_protocol::SessionStatus::Completed) {
                EventLevel::Info
            } else {
                EventLevel::Warning
            },
            format!("subagent {session_id} finished: {status:?} ({total_lines} lines)")
                .to_lowercase(),
        ),
        EventPayload::DelegateChildTerminated { status, reason } => push_event(
            state,
            if matches!(status, cookie_agent_protocol::SessionStatus::Failed) {
                EventLevel::Error
            } else {
                EventLevel::Info
            },
            reason.map_or_else(
                || format!("subagent {status:?}").to_lowercase(),
                |reason| format!("subagent {status:?}: {reason}").to_lowercase(),
            ),
        ),
        EventPayload::PluginEventAdded { plugin, name, .. } => {
            push_event(state, EventLevel::Info, format!("plugin {plugin}: {name}"))
        }
        EventPayload::PluginDiagnostic {
            plugin,
            message,
            count,
            ..
        } => push_event(
            state,
            EventLevel::Warning,
            if count > 1 {
                format!("plugin {plugin}: {message} (count: {count})")
            } else {
                format!("plugin {plugin}: {message}")
            },
        ),
        EventPayload::SessionTitleCommitted { change, .. } => {
            push_event(state, EventLevel::Info, render_title_commit(&change));
        }
        EventPayload::SessionReverted { .. } => {
            close_open_assistant(state, timestamp);
            state.active_run = None;
        }
        EventPayload::UserInputApplied { .. } => close_open_assistant(state, timestamp),
        EventPayload::SessionCreated {
            cwd_identity,
            creation_agent,
            ..
        } => {
            // Before any run starts, attempts (for example title generation)
            // attribute to the creation agent's frozen identity.
            if state.run_agent.is_none() {
                state.run_agent = Some(creation_agent.agent.clone());
            }
            state.cwd_identity = Some(cwd_identity);
            state.creation_agent = Some(creation_agent);
            state.created_at = Some(timestamp);
        }
        EventPayload::ModelUsageRecorded {
            model_turn_seq,
            estimated_cost_pico_usd,
            ..
        } => {
            if let Some(item_id) = state.turn_items.get(&model_turn_seq).copied() {
                state
                    .assistant_metrics
                    .entry(item_id)
                    .or_default()
                    .record_cost(estimated_cost_pico_usd);
            }
        }
        EventPayload::MessageInjected { role, input } => {
            push_item(state, |id| TranscriptItem::PluginMessage {
                id,
                version: 0,
                seq: sequence,
                role,
                input,
            })
        }
        EventPayload::DelegatedContextSeeded { .. }
        | EventPayload::UserInputTransformed { .. }
        | EventPayload::DelegationReserved { .. }
        | EventPayload::DelegationStarted { .. }
        | EventPayload::DelegationRunStarted { .. }
        | EventPayload::DelegationRunAttached { .. }
        | EventPayload::DelegationFinished { .. }
        | EventPayload::ModelRequestPrepared { .. }
        | EventPayload::InternalAgentUsageRecorded { .. }
        | EventPayload::ToolStdinSubmitted { .. }
        | EventPayload::ToolCallLinked { .. }
        | EventPayload::SessionPermissionOverlaySet { .. }
        | EventPayload::AgentMdLoaded { .. }
        | EventPayload::SkillLoaded { .. }
        | EventPayload::SkillInvocationNoted { .. } => {}
    }
}

fn reduce_session_events(
    session_id: SessionId,
    generation: u64,
    physical_events: &[StoredEvent],
) -> SessionState {
    let mut state = SessionState {
        generation,
        last_seq: physical_events.last().map_or(0, |event| event.seq),
        ..SessionState::default()
    };
    for event in cookie_agent_protocol::visible_events(physical_events) {
        reduce_event(
            &mut state,
            session_id,
            event.run_id,
            event.seq,
            event.timestamp,
            event.payload,
        );
    }
    state
}

fn goal_status_is_terminal(status: GoalStatus) -> bool {
    matches!(status, GoalStatus::Completed | GoalStatus::Cancelled)
}

fn valid_goal_lifecycle_change(goal: &GoalState, status: GoalStatus) -> bool {
    match (goal.status, status) {
        (GoalStatus::Active, GoalStatus::Paused | GoalStatus::Cancelled)
        | (GoalStatus::Paused, GoalStatus::Active | GoalStatus::Cancelled) => true,
        (GoalStatus::Active | GoalStatus::Paused, GoalStatus::Completed) => {
            !goal.items.is_empty() && goal.items.iter().all(|item| item.finished)
        }
        _ => false,
    }
}

fn valid_producer_reminder_owner(
    owner: &ProducerOwner,
    reminder: Option<&GoalReminderIdentity>,
) -> bool {
    if matches!(owner, ProducerOwner::Plugin { plugin } if plugin.trim().is_empty()) {
        return false;
    }
    match (owner, reminder) {
        (ProducerOwner::Goal { goal_id }, Some(reminder)) => *goal_id == reminder.goal_id,
        (ProducerOwner::Goal { .. }, None) | (_, Some(_)) => false,
        (_, None) => true,
    }
}

fn producer_message_status(message: &ProducerMessageProjection) -> ProducerMessageStatus {
    if message.consumed_run.is_some() {
        ProducerMessageStatus::Consumed
    } else if message.discarded_seq.is_some() {
        ProducerMessageStatus::Discarded
    } else if !message.claims.is_empty() {
        ProducerMessageStatus::Claimed
    } else if message.admission.is_some() {
        ProducerMessageStatus::Admitted
    } else {
        ProducerMessageStatus::Pending
    }
}

fn update_producer_message_status(
    state: &mut SessionState,
    message_id: ProducerMessageId,
    status: ProducerMessageStatus,
) {
    let Some(projection) = state.producer_messages.get_mut(&message_id) else {
        return;
    };
    let transcript_index = projection.transcript_index;
    let Some(TranscriptItem::ProducerMessage {
        message_id: row_message_id,
        status: row_status,
        ..
    }) = state.transcript.get_mut(transcript_index)
    else {
        return;
    };
    if *row_message_id != message_id {
        return;
    }
    projection.status = status;
    *row_status = status;
}

fn consume_producer_messages_through(
    state: &mut SessionState,
    run_id: RunId,
    input_through_seq: u64,
) {
    let consumed = state
        .producer_messages
        .iter()
        .filter_map(|(message_id, message)| {
            (message.consumed_run.is_none()
                && message
                    .admission
                    .is_some_and(|(admission_run, admission_seq)| {
                        admission_run == run_id && admission_seq <= input_through_seq
                    })
                && message
                    .discarded_seq
                    .is_none_or(|discarded_seq| discarded_seq > input_through_seq))
            .then_some(*message_id)
        })
        .collect::<Vec<_>>();
    for message_id in consumed {
        let message = state
            .producer_messages
            .get_mut(&message_id)
            .expect("projected producer message");
        message.discarded_seq = None;
        message.consumed_run = Some(run_id);
        update_producer_message_status(state, message_id, ProducerMessageStatus::Consumed);
    }
}

/// Open a fresh assistant item for a run or run-less streaming attempt.
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

fn append_attribution(state: &mut SessionState, item_id: u64, resolved_model: ResolvedModelRef) {
    if let Some(TranscriptItem::Assistant {
        version, children, ..
    }) = state
        .transcript
        .iter_mut()
        .find(|item| item.id() == item_id)
    {
        children.push(AssistantChild::Attribution { resolved_model });
        *version = version.wrapping_add(1);
    }
}

fn prune_abandoned_attempt(state: &mut SessionState, item_id: u64, committed_prefix: usize) {
    if let Some(TranscriptItem::Assistant {
        version, children, ..
    }) = state
        .transcript
        .iter_mut()
        .find(|item| item.id() == item_id)
    {
        let tail = children.split_off(committed_prefix.min(children.len()));
        for child in &tail {
            if let AssistantChild::Thinking { id, .. } = child {
                state.thinking_durations.remove(&(item_id, *id));
            }
        }
        children.extend(
            tail.into_iter()
                .filter(|child| matches!(child, AssistantChild::Attribution { .. })),
        );
        *version = version.wrapping_add(1);
    }
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
        // Only the first commit may reconcile the first attempt's header.
        if committed_turn_seq.is_none() {
            attribution.resolved_model = resolved_model.clone();
        }
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
        if let cookie_agent_protocol::PersistedAssistantPart::ToolCall {
            id, name, input, ..
        } = part
        {
            state.turn_tool_index.insert(
                (model_turn_seq, id.as_str().to_owned()),
                IndexedToolCall {
                    name: name.clone(),
                    arguments: input.to_string(),
                },
            );
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
    model_turn_seq: u64,
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
                    turn_seq: model_turn_seq,
                    content_index: index,
                });
            }
            cookie_agent_protocol::PersistedAssistantPart::File { file } => {
                children.push(AssistantChild::MediaFile {
                    turn_seq: model_turn_seq,
                    content_index: index,
                    file: file.clone(),
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
            AssistantChild::Attribution { .. }
            | AssistantChild::CommittedTool { .. }
            | AssistantChild::MediaFile { .. }
            | AssistantChild::Tool { .. } => {}
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
        let committed_prefix = state
            .open_run_assistant
            .as_ref()
            .filter(|projection| projection.item_id == item_id)
            .map_or(0, |projection| projection.committed_prefix)
            .min(existing.len());
        // Streamed thinking parts are superseded by their committed
        // counterparts; their sealed durations transfer to the committed
        // thinking children in order so "thought for Ns" survives the swap.
        let mut sealed_durations = Vec::new();
        for child in existing.iter().skip(committed_prefix) {
            if let AssistantChild::Thinking { id, .. } = child {
                sealed_durations.push(state.thinking_durations.remove(&(item_id, *id)));
            }
        }
        let mut sealed_durations = sealed_durations.into_iter();
        for child in &mut children {
            if let AssistantChild::Thinking { id, .. } = child
                && let Some(Some(duration)) = sealed_durations.next()
            {
                state.thinking_durations.insert((item_id, *id), duration);
            }
        }
        let retained_markers = existing
            .drain(committed_prefix..)
            .filter(|child| matches!(child, AssistantChild::Attribution { .. }))
            .collect::<Vec<_>>();
        existing.extend(retained_markers);
        existing.extend(children);
        *version = version.wrapping_add(1);
        if let Some(projection) = state
            .open_run_assistant
            .as_mut()
            .filter(|projection| projection.item_id == item_id)
        {
            projection.committed_prefix = existing.len();
        }
    }
}

fn append_assistant_delta(
    state: &mut SessionState,
    item_id: u64,
    sequence: u64,
    text: String,
    kind: AssistantPartKind,
    timestamp: jiff::Timestamp,
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
        if open.kind == kind {
            let part_index = children
                .iter()
                .position(|part| part.id() == open.part_id && assistant_part_is_kind(part, kind))
                .or_else(|| {
                    children
                        .iter()
                        .rposition(|part| assistant_part_is_kind(part, kind))
                });
            if let Some(part_index) = part_index {
                let part = &mut children[part_index];
                let part_id = part.id();
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
                // Continuing the same open part preserves its original
                // opening timestamp; the rare rposition fallback continues a
                // different part, whose opening time is no longer known.
                state.open_assistant = Some(OpenAssistantProjection {
                    opened_at: if part_id == open.part_id {
                        open.opened_at
                    } else {
                        timestamp
                    },
                    part_id,
                    ..open
                });
                return;
            }
        }
        // A part of a different kind (or an unknown continuation) replaces
        // the open projection: seal the previous thinking part first.
        if let Some(previous) = state.open_assistant.take() {
            seal_open_thinking(&mut state.thinking_durations, previous, timestamp);
        }
        children.push(new_assistant_part(sequence, text, kind));
        *version = version.wrapping_add(1);
        state.open_assistant = Some(OpenAssistantProjection {
            item_id,
            part_id: sequence,
            kind,
            opened_at: timestamp,
        });
        return;
    }
    if let Some(previous) = state.open_assistant.take() {
        seal_open_thinking(&mut state.thinking_durations, previous, timestamp);
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
            opened_at: timestamp,
        });
    } else {
        // The owning item is gone; keep the projection cleared.
        state.open_assistant = None;
    }
}

fn assistant_part_is_kind(part: &AssistantChild, kind: AssistantPartKind) -> bool {
    matches!(
        (part, kind),
        (AssistantChild::Text { .. }, AssistantPartKind::Text)
            | (AssistantChild::Thinking { .. }, AssistantPartKind::Thinking)
    )
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
            .is_some_and(|item_id| {
                link_tool_child(state, item_id, row.turn_seq, row.content_index, row.call_id)
            });
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
    turn_seq: u64,
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
            turn_seq: placeholder_turn,
            content_index: placeholder,
        } = child
            && *placeholder_turn == turn_seq
            && *placeholder == content_index
        {
            *child = AssistantChild::Tool { call_id };
            *version = version.wrapping_add(1);
            return true;
        }
    }
    false
}

/// Remove one pending lane entry after a promotion (oldest position) or a
/// recall (newest). The event's text correlates the entry; the FIFO
/// position is the fallback should payloads and lane ever diverge, so the
/// lane never strands an entry the engine says is gone.
/// Run end voids every still-pending steered input without per-entry
/// events: move their text aside so the UI can restore it into the composer
/// rather than ever losing it.
fn void_pending_inputs(state: &mut SessionState) {
    let drained = state.pending_inputs.drain(..).map(|pending| pending.text);
    state.voided_inputs.extend(drained.collect::<Vec<_>>());
}

fn close_open_assistant(state: &mut SessionState, sealed_at: jiff::Timestamp) {
    let Some(open) = state.open_assistant.take() else {
        return;
    };
    seal_open_thinking(&mut state.thinking_durations, open, sealed_at);
    if let Some(TranscriptItem::Assistant { version, .. }) = state
        .transcript
        .iter_mut()
        .find(|item| item.id() == open.item_id)
    {
        *version = version.wrapping_add(1);
    }
}

/// Record a sealed thinking part's elapsed time, derived from the durable
/// timestamps of the events that opened and sealed it. Non-thinking parts
/// and clock-skewed (negative) spans record nothing.
fn seal_open_thinking(
    thinking_durations: &mut HashMap<(u64, u64), Duration>,
    open: OpenAssistantProjection,
    sealed_at: jiff::Timestamp,
) {
    if open.kind != AssistantPartKind::Thinking {
        return;
    }
    let elapsed = sealed_at.duration_since(open.opened_at);
    let Ok(duration) = std::time::Duration::try_from(elapsed) else {
        return;
    };
    thinking_durations.insert((open.item_id, open.part_id), duration);
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

fn render_replay_decision(decision: &ReplayDecision) -> (EventLevel, String) {
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
        ReplayDisposition::ReconstructedNormalizedHistory => {
            (EventLevel::Debug, "reconstructed normalized history".into())
        }
    };
    (level, format!("#{} {disposition}", decision.history_index))
}

fn replay_context_warning_key(
    model: &ResolvedModelRef,
    decision: &ReplayDecision,
) -> Option<ReplayContextWarningKey> {
    let transition = match &decision.disposition {
        ReplayDisposition::DiscardedForeignAdapter { found, expected } => {
            ReplayContextTransition::Adapter {
                found: found.clone(),
                expected: expected.clone(),
            }
        }
        ReplayDisposition::DiscardedForeignModelSelection { found, expected } => {
            ReplayContextTransition::ModelSelection {
                found: found.clone(),
                expected: expected.clone(),
            }
        }
        ReplayDisposition::DiscardedForeignVariant { found, expected } => {
            ReplayContextTransition::Variant {
                found: found.clone(),
                expected: expected.clone(),
            }
        }
        _ => return None,
    };
    Some(ReplayContextWarningKey {
        selection_fingerprint: model.selection_fingerprint.clone(),
        transition,
    })
}

struct ReplayContextWarningKey {
    selection_fingerprint: Sha256Digest,
    transition: ReplayContextTransition,
}

impl ReplayContextWarningKey {
    fn with_run(self, run_id: RunId) -> (RunId, Sha256Digest, ReplayContextTransition) {
        (run_id, self.selection_fingerprint, self.transition)
    }
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
    additional_messages: &[cookie_agent_protocol::ToolEmittedMessage],
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
        lines.push(render_attachment_summary("attachment", attachment));
    }
    for message in additional_messages {
        lines.push(format!(
            "emitted {} message:",
            wire_enum_label(message.role)
        ));
        for part in &message.content {
            match part {
                cookie_agent_protocol::ToolEmittedContent::Text(text) => {
                    lines.push(format!("text: {text}"));
                }
                cookie_agent_protocol::ToolEmittedContent::File(attachment) => {
                    lines.push(render_attachment_summary("file", attachment));
                }
            }
        }
    }
    lines.join("\n")
}

fn render_attachment_summary(label: &str, attachment: &ToolAttachment) -> String {
    format!(
        "{label}: {} · {} bytes · sha256:{} · {}",
        attachment.mime_type, attachment.byte_length, attachment.sha256, attachment.reference.uri
    )
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
        .map(|tool| tool.arguments.clone())
}

fn render_internal_backend(backend: &cookie_agent_protocol::InternalAgentBackend) -> String {
    match backend {
        cookie_agent_protocol::InternalAgentBackend::Model { resolved_model } => {
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
        SessionTitleChange::DelegatedSet { title, .. } => {
            format!("delegated session titled {title}")
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
    data: Vec<u8>,
    pub next_offset: u64,
    pub has_gap: bool,
    line_count: usize,
    pending: BTreeMap<u64, Vec<u8>>,
}

impl OrderedOutput {
    pub fn replace_snapshot(&mut self, start: u64, end: u64, mut chunks: Vec<OutputDelta>) {
        self.data.clear();
        self.pending.clear();
        self.line_count = 0;
        self.has_gap = start > 0;
        chunks.sort_by_key(|chunk| chunk.byte_offset);
        for chunk in chunks {
            if let Ok(bytes) = STANDARD.decode(chunk.data) {
                self.append_bytes(&bytes);
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

    /// Return at most `max_bytes` of display text without materializing a
    /// valid UTF-8 stream. The line count describes the complete stream so a
    /// bounded renderer can still report accurate truncation.
    pub fn bounded_text(&self, max_bytes: usize) -> (Cow<'_, str>, usize) {
        let prefix = &self.data[..self.data.len().min(max_bytes)];
        (String::from_utf8_lossy(prefix), self.line_count)
    }

    fn flush(&mut self) {
        while let Some(bytes) = self.pending.remove(&self.next_offset) {
            self.next_offset += bytes.len() as u64;
            self.append_bytes(&bytes);
        }
    }

    fn append_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let newlines = bytes.iter().filter(|byte| **byte == b'\n').count();
        if self.data.is_empty() || self.data.ends_with(b"\n") {
            self.line_count += newlines + usize::from(!bytes.ends_with(b"\n"));
        } else {
            self.line_count += newlines.saturating_sub(usize::from(bytes.ends_with(b"\n")));
        }
        self.data.extend_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal_item(description: &str, finished: bool) -> cookie_agent_protocol::GoalItem {
        cookie_agent_protocol::GoalItem {
            description: description.into(),
            finished,
        }
    }

    fn resolved_model() -> ResolvedModelRef {
        serde_json::from_value(serde_json::json!({
            "provider_id": "test",
            "model_id": "test",
            "adapter_id": "openai-compatible",
            "selection": {"model": "test/test", "variant": null},
            "selection_fingerprint": "a".repeat(64)
        }))
        .expect("resolved model")
    }

    fn committed_turn(input_through_seq: u64) -> EventPayload {
        EventPayload::ModelTurnCommitted {
            attempt_id: AttemptId::new_v7(),
            model_turn_seq: 1,
            resolved_model: resolved_model(),
            input_through_seq,
            turn: PersistedModelTurn {
                content: Vec::new(),
                provider_options: BTreeMap::new(),
                finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
                usage: Usage::default(),
                response_metadata: BTreeMap::new(),
                provider_metadata: BTreeMap::new(),
                native_replay: None,
            },
            warnings: Vec::new(),
        }
    }

    fn stored_event(
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

    fn producer_rows(state: &SessionState) -> Vec<&TranscriptItem> {
        state
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::ProducerMessage { .. }))
            .collect()
    }

    fn accepted_message(
        message_id: ProducerMessageId,
        producer_owner: ProducerOwner,
        key: &str,
        reminder: Option<GoalReminderIdentity>,
    ) -> EventPayload {
        EventPayload::ProducerMessageAccepted {
            message_id,
            producer_owner,
            mode: ProducerDeliveryMode::Queue,
            idempotency_key: ProducerIdempotencyKey::new(key).expect("key"),
            body: key.into(),
            reminder,
        }
    }

    fn discarded_message(
        message_id: ProducerMessageId,
        producer_owner: Option<ProducerOwner>,
        reminder: Option<GoalReminderIdentity>,
    ) -> EventPayload {
        EventPayload::ProducerMessageDiscarded {
            message_id,
            reminder,
            producer_owner,
        }
    }

    #[test]
    fn goal_projection_requires_valid_identity_revision_and_lifecycle() {
        let session_id = SessionId::new_v7();
        let goal_id = GoalId::new_v7();
        let replacement_id = GoalId::new_v7();
        let mut state = SessionState::default();
        let apply = |state: &mut SessionState, seq, payload| {
            reduce_event(
                state,
                session_id,
                None,
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                payload,
            );
        };

        apply(
            &mut state,
            1,
            EventPayload::GoalActivated {
                goal_id,
                objective: "Ship the reducer".into(),
                revision: 0,
                selection: None,
            },
        );
        apply(
            &mut state,
            2,
            EventPayload::GoalChecklistRevised {
                goal_id,
                items: Vec::new(),
                revision: 1,
            },
        );
        apply(
            &mut state,
            3,
            EventPayload::GoalLifecycleChanged {
                goal_id,
                status: GoalStatus::Completed,
                revision: 2,
                selection: None,
            },
        );
        assert_eq!(
            state.goal.as_ref().expect("goal").status,
            GoalStatus::Active
        );

        apply(
            &mut state,
            4,
            EventPayload::GoalChecklistRevised {
                goal_id,
                items: vec![goal_item("Verify replay", true)],
                revision: 2,
            },
        );
        apply(
            &mut state,
            5,
            EventPayload::GoalChecklistRevised {
                goal_id,
                items: vec![goal_item("stale", false)],
                revision: 2,
            },
        );
        apply(
            &mut state,
            6,
            EventPayload::GoalLifecycleChanged {
                goal_id,
                status: GoalStatus::Completed,
                revision: 3,
                selection: None,
            },
        );
        apply(
            &mut state,
            7,
            EventPayload::GoalActivated {
                goal_id: replacement_id,
                objective: "Replacement".into(),
                revision: 0,
                selection: None,
            },
        );
        apply(
            &mut state,
            8,
            EventPayload::GoalChecklistRevised {
                goal_id,
                items: Vec::new(),
                revision: 4,
            },
        );
        apply(
            &mut state,
            9,
            EventPayload::GoalLifecycleChanged {
                goal_id: replacement_id,
                status: GoalStatus::Cancelled,
                revision: 1,
                selection: None,
            },
        );
        apply(
            &mut state,
            10,
            EventPayload::GoalActivated {
                goal_id,
                objective: "Stale reactivation".into(),
                revision: 100,
                selection: None,
            },
        );

        let goal = state.goal.as_ref().expect("replacement goal");
        assert_eq!(goal.goal_id, replacement_id);
        assert_eq!(goal.status, GoalStatus::Cancelled);
        assert!(goal.items.is_empty());
        let snapshots = state
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Goal { goal, .. } => Some(goal),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(snapshots.len(), 6);
        assert_eq!(snapshots[1].status, GoalStatus::Active);
        assert!(snapshots[1].items.is_empty());
        assert_eq!(snapshots[3].status, GoalStatus::Completed);
    }

    #[test]
    fn goal_checklist_replacement_preserves_duplicates_and_order() {
        let session_id = SessionId::new_v7();
        let goal_id = GoalId::new_v7();
        let replacement = vec![
            goal_item("Repeat verification", false),
            goal_item("Repeat verification", true),
            goal_item("Final review", false),
        ];
        let mut store = StateStore::default();
        for (index, payload) in [
            EventPayload::GoalActivated {
                goal_id,
                objective: "Verify the replacement checklist".into(),
                revision: 0,
                selection: None,
            },
            EventPayload::GoalChecklistRevised {
                goal_id,
                items: vec![goal_item("Old checklist", false)],
                revision: 1,
            },
            EventPayload::GoalChecklistRevised {
                goal_id,
                items: replacement.clone(),
                revision: 2,
            },
        ]
        .into_iter()
        .enumerate()
        {
            assert!(store.apply_event(stored_event(session_id, None, index as u64 + 1, payload,)));
        }
        let state = &store.sessions[&session_id];
        let goal = state.goal.as_ref().expect("goal");
        assert_eq!(goal.items, replacement);
        assert_eq!(goal.revision, 2);
        assert_eq!(goal.status, GoalStatus::Active);
        assert!(matches!(
            state.transcript.last(),
            Some(TranscriptItem::Goal { goal, .. }) if goal.items == replacement
        ));
    }

    #[test]
    fn goal_projection_rebuilds_for_replay_revert_and_fork_like_history() {
        let session_id = SessionId::new_v7();
        let goal_id = GoalId::new_v7();
        let item = goal_item("Keep projection durable", false);
        let events = vec![
            stored_event(
                session_id,
                None,
                1,
                EventPayload::GoalActivated {
                    goal_id,
                    objective: "Durable goal".into(),
                    revision: 0,
                    selection: None,
                },
            ),
            stored_event(
                session_id,
                None,
                2,
                EventPayload::GoalChecklistRevised {
                    goal_id,
                    items: vec![item.clone()],
                    revision: 1,
                },
            ),
            stored_event(
                session_id,
                None,
                3,
                EventPayload::GoalLifecycleChanged {
                    goal_id,
                    status: GoalStatus::Paused,
                    revision: 2,
                    selection: None,
                },
            ),
        ];

        let replayed = reduce_session_events(session_id, 4, &events);
        assert_eq!(replayed.generation, 4);
        assert_eq!(
            replayed.goal.as_ref().expect("replayed goal").status,
            GoalStatus::Paused
        );
        assert_eq!(
            replayed.goal.as_ref().expect("replayed goal").items,
            vec![item]
        );

        let forked = reduce_session_events(SessionId::new_v7(), 0, &events);
        assert_eq!(forked.goal, replayed.goal);

        let mut reverted_events = events;
        reverted_events.push(stored_event(
            session_id,
            None,
            4,
            EventPayload::SessionReverted { through_seq: 1 },
        ));
        let reverted = reduce_session_events(session_id, 5, &reverted_events);
        let goal = reverted.goal.expect("reverted goal");
        assert_eq!(goal.status, GoalStatus::Active);
        assert!(goal.items.is_empty());
        assert_eq!(
            reverted
                .transcript
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Goal { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn producer_queue_survives_terminal_and_consumption_updates_one_non_user_row() {
        let session_id = SessionId::new_v7();
        let first_run = RunId::new_v7();
        let second_run = RunId::new_v7();
        let message_id = ProducerMessageId::new_v7();
        let owner = ProducerOwner::Plugin {
            plugin: "build-monitor".into(),
        };
        let mut state = SessionState::default();

        for event in [
            stored_event(
                session_id,
                None,
                1,
                EventPayload::ProducerMessageAccepted {
                    message_id,
                    producer_owner: owner.clone(),
                    mode: ProducerDeliveryMode::Queue,
                    idempotency_key: ProducerIdempotencyKey::new("build-1").expect("key"),
                    body: "build finished".into(),
                    reminder: None,
                },
            ),
            stored_event(
                session_id,
                None,
                2,
                EventPayload::ProducerMessageAccepted {
                    message_id,
                    producer_owner: owner.clone(),
                    mode: ProducerDeliveryMode::Queue,
                    idempotency_key: ProducerIdempotencyKey::new("build-1").expect("key"),
                    body: "build finished".into(),
                    reminder: None,
                },
            ),
            stored_event(
                session_id,
                Some(first_run),
                3,
                EventPayload::RunCompleted { final_text: None },
            ),
        ] {
            reduce_event(
                &mut state,
                session_id,
                event.run_id,
                event.seq,
                event.timestamp,
                event.payload,
            );
        }
        assert!(matches!(
            producer_rows(&state).as_slice(),
            [TranscriptItem::ProducerMessage {
                status: ProducerMessageStatus::Pending,
                ..
            }]
        ));

        for event in [
            stored_event(
                session_id,
                Some(second_run),
                4,
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            stored_event(session_id, Some(second_run), 5, committed_turn(4)),
            stored_event(
                session_id,
                Some(second_run),
                6,
                EventPayload::ProducerMessageConsumed {
                    message_id,
                    run_id: second_run,
                },
            ),
        ] {
            reduce_event(
                &mut state,
                session_id,
                event.run_id,
                event.seq,
                event.timestamp,
                event.payload,
            );
        }

        assert!(matches!(
            producer_rows(&state).as_slice(),
            [TranscriptItem::ProducerMessage {
                status: ProducerMessageStatus::Consumed,
                accepted_at,
                body,
                ..
            }] if body == "build finished"
                && *accepted_at == jiff::Timestamp::new(1, 0).expect("timestamp")
        ));
        assert!(state.pending_inputs.is_empty());
        assert!(state.voided_inputs.is_empty());
        assert!(state.producer_messages[&message_id].consumption_recorded);
        assert!(!state.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::User { .. } | TranscriptItem::Assistant { .. }
        )));
    }

    #[test]
    fn valid_producer_admission_marks_initial_input_and_promotes_pending_user_input() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let message_id = ProducerMessageId::new_v7();
        let mut state = SessionState::default();
        for (seq, event_run, payload) in [
            (
                1,
                Some(run_id),
                EventPayload::UserInputAdmitted {
                    input: "steer after producer start".into(),
                },
            ),
            (
                2,
                None,
                accepted_message(
                    message_id,
                    ProducerOwner::Plugin {
                        plugin: "scheduler".into(),
                    },
                    "initial-producer-input",
                    None,
                ),
            ),
            (
                3,
                Some(run_id),
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
        ] {
            reduce_event(
                &mut state,
                session_id,
                event_run,
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                payload,
            );
        }

        assert!(state.initial_input_submitted.contains(&run_id));
        assert_eq!(state.pending_inputs.len(), 1);
        reduce_event(
            &mut state,
            session_id,
            Some(run_id),
            4,
            jiff::Timestamp::new(4, 0).expect("timestamp"),
            EventPayload::UserInputSubmitted {
                input: "steer after producer start".into(),
            },
        );
        assert!(state.pending_inputs.is_empty());
    }

    #[test]
    fn invalid_producer_admissions_do_not_mark_initial_input() {
        let session_id = SessionId::new_v7();
        let owner = ProducerOwner::Plugin {
            plugin: "scheduler".into(),
        };
        let mut state = SessionState::default();
        let mut seq = 0;
        let apply = |state: &mut SessionState,
                     seq: &mut u64,
                     run_id: Option<RunId>,
                     payload: EventPayload| {
            *seq += 1;
            reduce_event(
                state,
                session_id,
                run_id,
                *seq,
                jiff::Timestamp::new(*seq as i64, 0).expect("timestamp"),
                payload,
            );
        };

        let runless = ProducerMessageId::new_v7();
        apply(
            &mut state,
            &mut seq,
            None,
            accepted_message(runless, owner.clone(), "runless", None),
        );
        apply(
            &mut state,
            &mut seq,
            None,
            EventPayload::ProducerMessageAdmitted {
                message_id: runless,
            },
        );
        assert!(state.initial_input_submitted.is_empty());

        let unknown_run = RunId::new_v7();
        apply(
            &mut state,
            &mut seq,
            Some(unknown_run),
            EventPayload::ProducerMessageAdmitted {
                message_id: ProducerMessageId::new_v7(),
            },
        );
        assert!(!state.initial_input_submitted.contains(&unknown_run));

        let malformed = ProducerMessageId::new_v7();
        let malformed_run = RunId::new_v7();
        apply(
            &mut state,
            &mut seq,
            None,
            accepted_message(
                malformed,
                ProducerOwner::Goal {
                    goal_id: GoalId::new_v7(),
                },
                "malformed-goal",
                None,
            ),
        );
        apply(
            &mut state,
            &mut seq,
            Some(malformed_run),
            EventPayload::ProducerMessageAdmitted {
                message_id: malformed,
            },
        );
        assert!(!state.initial_input_submitted.contains(&malformed_run));

        let discarded = ProducerMessageId::new_v7();
        let discarded_run = RunId::new_v7();
        apply(
            &mut state,
            &mut seq,
            None,
            accepted_message(discarded, owner.clone(), "discarded", None),
        );
        apply(
            &mut state,
            &mut seq,
            None,
            discarded_message(discarded, Some(owner.clone()), None),
        );
        apply(
            &mut state,
            &mut seq,
            Some(discarded_run),
            EventPayload::ProducerMessageAdmitted {
                message_id: discarded,
            },
        );
        assert!(!state.initial_input_submitted.contains(&discarded_run));

        let consumed = ProducerMessageId::new_v7();
        let consuming_run = RunId::new_v7();
        let consumed_retry_run = RunId::new_v7();
        apply(
            &mut state,
            &mut seq,
            None,
            accepted_message(consumed, owner.clone(), "consumed", None),
        );
        apply(
            &mut state,
            &mut seq,
            Some(consuming_run),
            EventPayload::ProducerMessageAdmitted {
                message_id: consumed,
            },
        );
        let admission_seq = seq;
        apply(
            &mut state,
            &mut seq,
            Some(consuming_run),
            committed_turn(admission_seq),
        );
        apply(
            &mut state,
            &mut seq,
            Some(consumed_retry_run),
            EventPayload::ProducerMessageAdmitted {
                message_id: consumed,
            },
        );
        assert!(!state.initial_input_submitted.contains(&consumed_retry_run));

        let terminal = ProducerMessageId::new_v7();
        let terminal_run = RunId::new_v7();
        apply(
            &mut state,
            &mut seq,
            None,
            accepted_message(terminal, owner.clone(), "terminal", None),
        );
        apply(
            &mut state,
            &mut seq,
            Some(terminal_run),
            EventPayload::RunCompleted { final_text: None },
        );
        apply(
            &mut state,
            &mut seq,
            Some(terminal_run),
            EventPayload::ProducerMessageAdmitted {
                message_id: terminal,
            },
        );
        assert!(!state.initial_input_submitted.contains(&terminal_run));

        let admitted = ProducerMessageId::new_v7();
        let admitted_run = RunId::new_v7();
        let conflicting_run = RunId::new_v7();
        apply(
            &mut state,
            &mut seq,
            None,
            accepted_message(admitted, owner, "conflicting", None),
        );
        apply(
            &mut state,
            &mut seq,
            Some(admitted_run),
            EventPayload::ProducerMessageAdmitted {
                message_id: admitted,
            },
        );
        apply(
            &mut state,
            &mut seq,
            Some(conflicting_run),
            EventPayload::ProducerMessageAdmitted {
                message_id: admitted,
            },
        );
        assert!(state.initial_input_submitted.contains(&admitted_run));
        assert!(!state.initial_input_submitted.contains(&conflicting_run));
    }

    #[test]
    fn goal_control_messages_survive_terminal_and_reminder_discard() {
        let session_id = SessionId::new_v7();
        let goal_id = GoalId::new_v7();
        let message_id = ProducerMessageId::new_v7();
        let body = "Goal paused. Stop pursuing the objective.";
        let events = vec![
            stored_event(
                session_id,
                None,
                1,
                EventPayload::ProducerMessageAccepted {
                    message_id,
                    producer_owner: ProducerOwner::GoalControl { goal_id },
                    mode: ProducerDeliveryMode::Steer,
                    idempotency_key: ProducerIdempotencyKey::new("pause-control").unwrap(),
                    body: body.into(),
                    reminder: None,
                },
            ),
            stored_event(
                session_id,
                Some(RunId::new_v7()),
                2,
                EventPayload::RunInterrupted { reason: None },
            ),
            stored_event(
                session_id,
                None,
                3,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: Some(GoalReminderIdentity {
                        goal_id,
                        revision: 1,
                    }),
                    producer_owner: None,
                },
            ),
        ];
        let replayed = reduce_session_events(session_id, 0, &events);
        assert!(matches!(
            producer_rows(&replayed).as_slice(),
            [TranscriptItem::ProducerMessage {
                body: received,
                reminder: None,
                status: ProducerMessageStatus::Pending,
                ..
            }] if received == body
        ));
        assert!(replayed.pending_inputs.is_empty());
        assert!(replayed.voided_inputs.is_empty());
    }

    #[test]
    fn goal_reminder_discard_updates_one_row_without_entering_composer_lanes() {
        let session_id = SessionId::new_v7();
        let goal_id = GoalId::new_v7();
        let message_id = ProducerMessageId::new_v7();
        let reminder = GoalReminderIdentity {
            goal_id,
            revision: 7,
        };
        let mut state = SessionState::default();

        for (seq, payload) in [
            (
                1,
                EventPayload::ProducerMessageAccepted {
                    message_id,
                    producer_owner: ProducerOwner::Goal { goal_id },
                    mode: ProducerDeliveryMode::Queue,
                    idempotency_key: ProducerIdempotencyKey::new("goal-reminder-7").expect("key"),
                    body: "full internal reminder body".into(),
                    reminder: Some(reminder),
                },
            ),
            (
                2,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: Some(GoalReminderIdentity {
                        revision: 6,
                        ..reminder
                    }),
                    producer_owner: None,
                },
            ),
            (
                3,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: Some(reminder),
                    producer_owner: None,
                },
            ),
        ] {
            reduce_event(
                &mut state,
                session_id,
                None,
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                payload,
            );
        }

        assert!(matches!(
            producer_rows(&state).as_slice(),
            [TranscriptItem::ProducerMessage {
                producer_owner: ProducerOwner::Goal { goal_id: row_goal_id },
                status: ProducerMessageStatus::Discarded,
                ..
            }] if *row_goal_id == goal_id
        ));
        assert!(state.pending_inputs.is_empty());
        assert!(state.voided_inputs.is_empty());
        assert_eq!(producer_rows(&state).len(), 1);
    }

    #[test]
    fn discard_inside_committed_input_window_stays_discarded_and_reverts() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let goal_id = GoalId::new_v7();
        let message_id = ProducerMessageId::new_v7();
        let reminder = GoalReminderIdentity {
            goal_id,
            revision: 3,
        };
        let events = vec![
            stored_event(
                session_id,
                None,
                1,
                EventPayload::ProducerMessageAccepted {
                    message_id,
                    producer_owner: ProducerOwner::Goal { goal_id },
                    mode: ProducerDeliveryMode::Steer,
                    idempotency_key: ProducerIdempotencyKey::new("reminder-attempt").expect("key"),
                    body: "internal reminder".into(),
                    reminder: Some(reminder),
                },
            ),
            stored_event(
                session_id,
                Some(run_id),
                2,
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            stored_event(
                session_id,
                None,
                3,
                EventPayload::ProducerMessageDiscarded {
                    message_id,
                    reminder: Some(reminder),
                    producer_owner: None,
                },
            ),
            stored_event(session_id, Some(run_id), 4, committed_turn(3)),
        ];

        let replayed = reduce_session_events(session_id, 0, &events);
        assert!(matches!(
            producer_rows(&replayed).as_slice(),
            [TranscriptItem::ProducerMessage {
                status: ProducerMessageStatus::Discarded,
                ..
            }]
        ));
        assert!(!replayed.producer_messages[&message_id].consumption_recorded);

        let mut reverted_events = events;
        reverted_events.push(stored_event(
            session_id,
            None,
            5,
            EventPayload::SessionReverted { through_seq: 3 },
        ));
        let reverted = reduce_session_events(session_id, 0, &reverted_events);
        assert!(matches!(
            producer_rows(&reverted).as_slice(),
            [TranscriptItem::ProducerMessage {
                status: ProducerMessageStatus::Discarded,
                ..
            }]
        ));
    }

    #[test]
    fn discard_after_committed_input_window_is_consumed_and_cleared() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let goal_id = GoalId::new_v7();
        let message_id = ProducerMessageId::new_v7();
        let reminder = GoalReminderIdentity {
            goal_id,
            revision: 4,
        };
        let state = reduce_session_events(
            session_id,
            0,
            &[
                stored_event(
                    session_id,
                    None,
                    1,
                    accepted_message(
                        message_id,
                        ProducerOwner::Goal { goal_id },
                        "covered-reminder",
                        Some(reminder),
                    ),
                ),
                stored_event(
                    session_id,
                    Some(run_id),
                    2,
                    EventPayload::ProducerMessageAdmitted { message_id },
                ),
                stored_event(
                    session_id,
                    None,
                    3,
                    discarded_message(message_id, None, Some(reminder)),
                ),
                stored_event(session_id, Some(run_id), 4, committed_turn(2)),
            ],
        );

        assert_eq!(state.producer_messages[&message_id].discarded_seq, None);
        assert!(matches!(
            producer_rows(&state).as_slice(),
            [TranscriptItem::ProducerMessage {
                status: ProducerMessageStatus::Consumed,
                ..
            }]
        ));
    }

    #[test]
    fn claims_validate_atomically_and_support_multiple_tokens_per_message() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let first = ProducerMessageId::new_v7();
        let second = ProducerMessageId::new_v7();
        let unknown = ProducerMessageId::new_v7();
        let mut state = SessionState::default();
        for (seq, payload) in [
            (
                1,
                accepted_message(
                    first,
                    ProducerOwner::Plugin {
                        plugin: "worker".into(),
                    },
                    "first-claim",
                    None,
                ),
            ),
            (
                2,
                accepted_message(
                    second,
                    ProducerOwner::Plugin {
                        plugin: "worker".into(),
                    },
                    "second-claim",
                    None,
                ),
            ),
            (
                3,
                EventPayload::ProducerMessageAdmitted { message_id: first },
            ),
            (
                4,
                EventPayload::ProducerMessageAdmitted { message_id: second },
            ),
        ] {
            reduce_event(
                &mut state,
                session_id,
                (seq >= 3).then_some(run_id),
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                payload,
            );
        }

        for (seq, message_ids) in [
            (5, vec![]),
            (6, vec![first, unknown]),
            (7, vec![first, first]),
        ] {
            reduce_event(
                &mut state,
                session_id,
                Some(run_id),
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                EventPayload::ProducerMessagesClaimed { message_ids },
            );
        }
        assert!(state.producer_claims.is_empty());
        assert!(
            state
                .producer_messages
                .values()
                .all(|message| message.claims.is_empty()
                    && message.status == ProducerMessageStatus::Admitted)
        );

        reduce_event(
            &mut state,
            session_id,
            Some(run_id),
            8,
            jiff::Timestamp::new(8, 0).expect("timestamp"),
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![first, second],
            },
        );
        reduce_event(
            &mut state,
            session_id,
            Some(run_id),
            9,
            jiff::Timestamp::new(9, 0).expect("timestamp"),
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![first],
            },
        );

        assert_eq!(
            state.producer_messages[&first].claims,
            HashSet::from([8, 9])
        );
        assert_eq!(state.producer_messages[&second].claims, HashSet::from([8]));
        assert!(producer_rows(&state).iter().all(|row| matches!(
            row,
            TranscriptItem::ProducerMessage {
                status: ProducerMessageStatus::Claimed,
                ..
            }
        )));
    }

    #[test]
    fn release_requires_claim_owner_and_only_last_release_unclaims_message() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let other_run = RunId::new_v7();
        let message_id = ProducerMessageId::new_v7();
        let mut state = SessionState::default();
        for (seq, payload) in [
            (
                1,
                accepted_message(
                    message_id,
                    ProducerOwner::Delegation {
                        invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
                    },
                    "owned-release",
                    None,
                ),
            ),
            (2, EventPayload::ProducerMessageAdmitted { message_id }),
            (
                3,
                EventPayload::ProducerMessagesClaimed {
                    message_ids: vec![message_id],
                },
            ),
            (
                4,
                EventPayload::ProducerMessagesClaimed {
                    message_ids: vec![message_id],
                },
            ),
        ] {
            reduce_event(
                &mut state,
                session_id,
                (seq >= 2).then_some(run_id),
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                payload,
            );
        }
        for (seq, release_run, claim_seq) in [
            (5, Some(other_run), 3),
            (6, Some(run_id), 0),
            (7, Some(run_id), 99),
            (8, None, 3),
        ] {
            reduce_event(
                &mut state,
                session_id,
                release_run,
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                EventPayload::ProducerMessagesReleased { claim_seq },
            );
        }
        assert_eq!(
            state.producer_messages[&message_id].claims,
            HashSet::from([3, 4])
        );

        for (seq, claim_seq) in [(9, 3), (10, 4)] {
            reduce_event(
                &mut state,
                session_id,
                Some(run_id),
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                EventPayload::ProducerMessagesReleased { claim_seq },
            );
            assert_eq!(
                state.producer_messages[&message_id].status,
                if claim_seq == 3 {
                    ProducerMessageStatus::Claimed
                } else {
                    ProducerMessageStatus::Admitted
                }
            );
        }
        assert!(state.producer_claims.is_empty());
    }

    #[test]
    fn claimed_message_rejects_discard_until_explicit_release() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let owner = ProducerOwner::Plugin {
            plugin: "jobs".into(),
        };
        let message_id = ProducerMessageId::new_v7();
        let mut state = SessionState::default();
        for (seq, event_run, payload) in [
            (
                1,
                None,
                accepted_message(message_id, owner.clone(), "claimed-discard", None),
            ),
            (
                2,
                Some(run_id),
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            (
                3,
                Some(run_id),
                EventPayload::ProducerMessagesClaimed {
                    message_ids: vec![message_id],
                },
            ),
            (
                4,
                None,
                discarded_message(message_id, Some(owner.clone()), None),
            ),
            (
                5,
                Some(run_id),
                EventPayload::ProducerMessagesReleased { claim_seq: 3 },
            ),
            (6, None, discarded_message(message_id, Some(owner), None)),
        ] {
            reduce_event(
                &mut state,
                session_id,
                event_run,
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                payload,
            );
            if seq == 4 {
                assert_eq!(state.producer_messages[&message_id].discarded_seq, None);
                assert_eq!(
                    state.producer_messages[&message_id].status,
                    ProducerMessageStatus::Claimed
                );
            }
        }
        assert_eq!(
            state.producer_messages[&message_id].status,
            ProducerMessageStatus::Discarded
        );
    }

    #[test]
    fn generic_and_legacy_discards_require_exact_durable_identity() {
        let session_id = SessionId::new_v7();
        let goal_id = GoalId::new_v7();
        let reminder = GoalReminderIdentity {
            goal_id,
            revision: 8,
        };
        let plugin = ProducerOwner::Plugin {
            plugin: "jobs".into(),
        };
        let delegation = ProducerOwner::Delegation {
            invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
        };
        let control = ProducerOwner::GoalControl { goal_id };
        let goal = ProducerOwner::Goal { goal_id };
        let owners = [
            (plugin, None, "plugin-owner"),
            (delegation, None, "delegation-owner"),
            (control, None, "goal-control-owner"),
            (goal.clone(), Some(reminder), "goal-owner"),
        ];
        let mut state = SessionState::default();
        let mut message_ids = Vec::new();
        for (index, (owner, accepted_reminder, key)) in owners.into_iter().enumerate() {
            let message_id = ProducerMessageId::new_v7();
            message_ids.push(message_id);
            let seq = index as u64 * 2 + 1;
            reduce_event(
                &mut state,
                session_id,
                None,
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                accepted_message(message_id, owner.clone(), key, accepted_reminder),
            );
            reduce_event(
                &mut state,
                session_id,
                None,
                seq + 1,
                jiff::Timestamp::new(seq as i64 + 1, 0).expect("timestamp"),
                discarded_message(message_id, Some(owner), accepted_reminder),
            );
        }

        let legacy_id = ProducerMessageId::new_v7();
        reduce_event(
            &mut state,
            session_id,
            None,
            9,
            jiff::Timestamp::new(9, 0).expect("timestamp"),
            accepted_message(legacy_id, goal.clone(), "legacy-goal", Some(reminder)),
        );
        reduce_event(
            &mut state,
            session_id,
            None,
            10,
            jiff::Timestamp::new(10, 0).expect("timestamp"),
            discarded_message(legacy_id, None, Some(reminder)),
        );
        message_ids.push(legacy_id);

        let invalid_goal = ProducerMessageId::new_v7();
        let invalid_control = ProducerMessageId::new_v7();
        reduce_event(
            &mut state,
            session_id,
            None,
            11,
            jiff::Timestamp::new(11, 0).expect("timestamp"),
            accepted_message(invalid_goal, goal, "goal-without-reminder", None),
        );
        reduce_event(
            &mut state,
            session_id,
            None,
            12,
            jiff::Timestamp::new(12, 0).expect("timestamp"),
            accepted_message(
                invalid_control,
                ProducerOwner::GoalControl { goal_id },
                "control-with-reminder",
                Some(reminder),
            ),
        );

        assert!(message_ids.iter().all(|message_id| {
            state.producer_messages[message_id].status == ProducerMessageStatus::Discarded
        }));
        assert!(!state.producer_messages.contains_key(&invalid_goal));
        assert!(!state.producer_messages.contains_key(&invalid_control));
    }

    #[test]
    fn release_does_not_resurrect_consumed_or_discarded_messages() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let owner = ProducerOwner::Plugin {
            plugin: "jobs".into(),
        };
        let consumed = ProducerMessageId::new_v7();
        let discarded = ProducerMessageId::new_v7();
        let mut state = SessionState::default();
        for (seq, event_run, payload) in [
            (
                1,
                None,
                accepted_message(consumed, owner.clone(), "consumed-release", None),
            ),
            (
                2,
                Some(run_id),
                EventPayload::ProducerMessageAdmitted {
                    message_id: consumed,
                },
            ),
            (
                3,
                Some(run_id),
                EventPayload::ProducerMessagesClaimed {
                    message_ids: vec![consumed],
                },
            ),
            (4, Some(run_id), committed_turn(2)),
            (
                5,
                Some(run_id),
                EventPayload::ProducerMessagesReleased { claim_seq: 3 },
            ),
            (
                6,
                None,
                accepted_message(discarded, owner.clone(), "discarded-release", None),
            ),
            (
                7,
                Some(run_id),
                EventPayload::ProducerMessageAdmitted {
                    message_id: discarded,
                },
            ),
            (
                8,
                Some(run_id),
                EventPayload::ProducerMessagesClaimed {
                    message_ids: vec![discarded],
                },
            ),
            (
                9,
                Some(run_id),
                EventPayload::ProducerMessagesReleased { claim_seq: 8 },
            ),
            (10, None, discarded_message(discarded, Some(owner), None)),
            (
                11,
                Some(run_id),
                EventPayload::ProducerMessagesReleased { claim_seq: 8 },
            ),
        ] {
            reduce_event(
                &mut state,
                session_id,
                event_run,
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                payload,
            );
        }
        assert_eq!(
            state.producer_messages[&consumed].status,
            ProducerMessageStatus::Consumed
        );
        assert_eq!(
            state.producer_messages[&discarded].status,
            ProducerMessageStatus::Discarded
        );
    }

    #[test]
    fn replay_and_revert_preserve_terminal_claim_until_explicit_recovery_release() {
        let session_id = SessionId::new_v7();
        let interrupted_run = RunId::new_v7();
        let retry_run = RunId::new_v7();
        let message_id = ProducerMessageId::new_v7();
        let mut events = vec![
            stored_event(
                session_id,
                None,
                1,
                accepted_message(
                    message_id,
                    ProducerOwner::Plugin {
                        plugin: "worker".into(),
                    },
                    "terminal-recovery",
                    None,
                ),
            ),
            stored_event(
                session_id,
                Some(interrupted_run),
                2,
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            stored_event(
                session_id,
                Some(interrupted_run),
                3,
                EventPayload::ProducerMessagesClaimed {
                    message_ids: vec![message_id],
                },
            ),
            stored_event(
                session_id,
                Some(interrupted_run),
                4,
                EventPayload::RunInterrupted { reason: None },
            ),
            stored_event(
                session_id,
                Some(interrupted_run),
                5,
                EventPayload::ProducerMessagesReleased { claim_seq: 3 },
            ),
            stored_event(
                session_id,
                Some(retry_run),
                6,
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
        ];
        let replayed = reduce_session_events(session_id, 2, &events);
        assert!(replayed.producer_claims.is_empty());
        assert_eq!(
            replayed.producer_messages[&message_id].admission,
            Some((retry_run, 6))
        );
        assert_eq!(
            replayed.producer_messages[&message_id].status,
            ProducerMessageStatus::Admitted
        );

        events.push(stored_event(
            session_id,
            None,
            7,
            EventPayload::SessionReverted { through_seq: 4 },
        ));
        let reverted = reduce_session_events(session_id, 3, &events);
        assert_eq!(reverted.producer_claims[&3].run_id, interrupted_run);
        assert_eq!(
            reverted.producer_messages[&message_id].claims,
            HashSet::from([3])
        );
        assert_eq!(
            reverted.producer_messages[&message_id].status,
            ProducerMessageStatus::Claimed
        );
    }

    #[test]
    fn producer_can_be_readmitted_after_interruption_but_not_to_terminal_run() {
        let session_id = SessionId::new_v7();
        let first_run = RunId::new_v7();
        let second_run = RunId::new_v7();
        let message_id = ProducerMessageId::new_v7();
        let mut state = SessionState::default();
        let events = [
            stored_event(
                session_id,
                None,
                1,
                EventPayload::ProducerMessageAccepted {
                    message_id,
                    producer_owner: ProducerOwner::Plugin {
                        plugin: "worker".into(),
                    },
                    mode: ProducerDeliveryMode::Queue,
                    idempotency_key: ProducerIdempotencyKey::new("retry").expect("key"),
                    body: "result".into(),
                    reminder: None,
                },
            ),
            stored_event(
                session_id,
                Some(first_run),
                2,
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            stored_event(
                session_id,
                Some(first_run),
                3,
                EventPayload::RunInterrupted { reason: None },
            ),
            stored_event(
                session_id,
                Some(first_run),
                4,
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            stored_event(
                session_id,
                Some(second_run),
                5,
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
        ];
        for event in events {
            reduce_event(
                &mut state,
                session_id,
                event.run_id,
                event.seq,
                event.timestamp,
                event.payload,
            );
        }

        assert_eq!(
            state.producer_messages[&message_id]
                .admission
                .map(|(run_id, _)| run_id),
            Some(second_run)
        );
        assert!(matches!(
            producer_rows(&state).as_slice(),
            [TranscriptItem::ProducerMessage {
                status: ProducerMessageStatus::Admitted,
                ..
            }]
        ));
    }

    #[test]
    fn sequenced_projection_rows_use_transcript_allocator_ids() {
        let session_id = SessionId::new_v7();
        let goal_id = GoalId::new_v7();
        let message_id = ProducerMessageId::new_v7();
        let mut state = SessionState {
            next_transcript_id: 50,
            transcript: vec![TranscriptItem::Event {
                id: 50,
                version: 0,
                level: EventLevel::Warning,
                text: "old diagnostic".into(),
            }],
            ..SessionState::default()
        };
        for (seq, payload) in [
            (
                1,
                EventPayload::GoalActivated {
                    goal_id,
                    objective: "Unique rows".into(),
                    revision: 0,
                    selection: None,
                },
            ),
            (
                2,
                EventPayload::ProducerMessageAccepted {
                    message_id,
                    producer_owner: ProducerOwner::Plugin {
                        plugin: "worker".into(),
                    },
                    mode: ProducerDeliveryMode::Queue,
                    idempotency_key: ProducerIdempotencyKey::new("unique-row").expect("key"),
                    body: "message".into(),
                    reminder: None,
                },
            ),
        ] {
            reduce_event(
                &mut state,
                session_id,
                None,
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                payload,
            );
        }

        assert_eq!(
            state
                .transcript
                .iter()
                .map(TranscriptItem::id)
                .collect::<Vec<_>>(),
            vec![50, 51, 52]
        );
    }

    #[test]
    fn producer_acceptance_time_and_order_survive_timing_pruning_and_replay() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let first_message_id = ProducerMessageId::new_v7();
        let second_message_id = ProducerMessageId::new_v7();
        let events = vec![
            stored_event(
                session_id,
                None,
                1,
                EventPayload::ProducerMessageAccepted {
                    message_id: first_message_id,
                    producer_owner: ProducerOwner::Plugin {
                        plugin: "worker".into(),
                    },
                    mode: ProducerDeliveryMode::Queue,
                    idempotency_key: ProducerIdempotencyKey::new("first").expect("key"),
                    body: "first".into(),
                    reminder: None,
                },
            ),
            stored_event(
                session_id,
                None,
                2,
                EventPayload::ProducerMessageAccepted {
                    message_id: second_message_id,
                    producer_owner: ProducerOwner::Delegation {
                        invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
                    },
                    mode: ProducerDeliveryMode::Steer,
                    idempotency_key: ProducerIdempotencyKey::new("second").expect("key"),
                    body: "second".into(),
                    reminder: None,
                },
            ),
            stored_event(
                session_id,
                Some(run_id),
                3,
                EventPayload::ProducerMessageAdmitted {
                    message_id: first_message_id,
                },
            ),
            stored_event(session_id, Some(run_id), 4, committed_turn(3)),
        ];

        let state = reduce_session_events(session_id, 0, &events);
        assert!(!state.event_timestamps.contains_key(&1));
        assert!(!state.event_timestamps.contains_key(&2));
        assert!(matches!(
            producer_rows(&state).as_slice(),
            [
                TranscriptItem::ProducerMessage {
                    id: 1,
                    seq: 1,
                    accepted_at: first_accepted_at,
                    message_id: row_first_id,
                    status: ProducerMessageStatus::Consumed,
                    ..
                },
                TranscriptItem::ProducerMessage {
                    id: 2,
                    seq: 2,
                    accepted_at: second_accepted_at,
                    message_id: row_second_id,
                    status: ProducerMessageStatus::Pending,
                    ..
                }
            ] if *row_first_id == first_message_id
                && *row_second_id == second_message_id
                && *first_accepted_at == jiff::Timestamp::new(1, 0).expect("timestamp")
                && *second_accepted_at == jiff::Timestamp::new(2, 0).expect("timestamp")
        ));
        assert!(state.pending_inputs.is_empty());
        assert!(state.voided_inputs.is_empty());
    }

    #[test]
    fn paused_goal_accepts_empty_checklist_then_resumes_and_completes() {
        let session_id = SessionId::new_v7();
        let goal_id = GoalId::new_v7();
        let mut state = SessionState::default();
        for (seq, payload) in [
            (
                1,
                EventPayload::GoalActivated {
                    goal_id,
                    objective: "Finish lifecycle".into(),
                    revision: 0,
                    selection: None,
                },
            ),
            (
                2,
                EventPayload::GoalLifecycleChanged {
                    goal_id,
                    status: GoalStatus::Paused,
                    revision: 1,
                    selection: None,
                },
            ),
            (
                3,
                EventPayload::GoalChecklistRevised {
                    goal_id,
                    items: Vec::new(),
                    revision: 2,
                },
            ),
            (
                4,
                EventPayload::GoalLifecycleChanged {
                    goal_id,
                    status: GoalStatus::Active,
                    revision: 3,
                    selection: None,
                },
            ),
            (
                5,
                EventPayload::GoalChecklistRevised {
                    goal_id,
                    items: vec![goal_item("finish-lifecycle", true)],
                    revision: 4,
                },
            ),
            (
                6,
                EventPayload::GoalLifecycleChanged {
                    goal_id,
                    status: GoalStatus::Completed,
                    revision: 5,
                    selection: None,
                },
            ),
        ] {
            reduce_event(
                &mut state,
                session_id,
                None,
                seq,
                jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
                payload,
            );
        }

        let statuses = state
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Goal { goal, .. } => Some(goal.status),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            vec![
                GoalStatus::Active,
                GoalStatus::Paused,
                GoalStatus::Paused,
                GoalStatus::Active,
                GoalStatus::Active,
                GoalStatus::Completed,
            ]
        );
        assert_eq!(
            state.goal.as_ref().expect("goal").status,
            GoalStatus::Completed
        );
    }

    #[test]
    fn ordered_output_bounded_text_borrows_valid_prefix_and_keeps_full_line_count() {
        let mut output = OrderedOutput::default();
        output.append_bytes(b"first\nsecond\nthird");
        let (complete, complete_lines) = output.bounded_text(usize::MAX);
        assert!(matches!(complete, Cow::Borrowed("first\nsecond\nthird")));
        assert_eq!(complete, output.text());
        assert_eq!(complete_lines, 3);

        let (prefix, original_lines) = output.bounded_text(8);
        assert!(matches!(prefix, Cow::Borrowed("first\nse")));
        assert_eq!(original_lines, 3);
    }

    #[test]
    fn ordered_output_updates_line_count_across_chunk_boundaries() {
        let mut output = OrderedOutput::default();
        for chunk in [b"first".as_slice(), b"\nsecond\n".as_slice(), b"third"] {
            output.append_bytes(chunk);
        }
        assert_eq!(output.bounded_text(0).1, 3);

        output.append_bytes(b"\n");
        assert_eq!(output.bounded_text(0).1, 3);
        output.append_bytes(b"\n");
        assert_eq!(output.bounded_text(0).1, 4);
    }

    #[test]
    fn thinking_delta_after_committed_child_renumbering_appends_without_duplicate() {
        let item_id = 1;
        let mut state = SessionState {
            transcript: vec![TranscriptItem::Assistant {
                id: item_id,
                version: 0,
                attribution: FrozenAssistantAttribution {
                    agent: AgentId::new("test").expect("agent id"),
                    resolved_model: serde_json::from_value(serde_json::json!({
                        "provider_id": "test",
                        "model_id": "test",
                        "adapter_id": "openai-compatible",
                        "selection": {"model": "test/test", "variant": null},
                        "selection_fingerprint": "a".repeat(64)
                    }))
                    .expect("resolved model"),
                },
                committed_turn_seq: None,
                children: Vec::new(),
            }],
            ..SessionState::default()
        };

        append_assistant_delta(
            &mut state,
            item_id,
            10,
            "first".into(),
            AssistantPartKind::Thinking,
            jiff::Timestamp::now(),
        );
        let stale_open = state.open_assistant.expect("open thinking segment");
        let turn = PersistedModelTurn {
            content: vec![cookie_agent_protocol::PersistedAssistantPart::Reasoning {
                text: "first".into(),
                metadata: None,
            }],
            provider_options: BTreeMap::new(),
            finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
            usage: Usage::default(),
            response_metadata: BTreeMap::new(),
            provider_metadata: BTreeMap::new(),
            native_replay: None,
        };
        rebuild_committed_children(&mut state, item_id, 1, 20, &turn);
        state.open_assistant = Some(stale_open);

        append_assistant_delta(
            &mut state,
            item_id,
            21,
            " second".into(),
            AssistantPartKind::Thinking,
            jiff::Timestamp::now(),
        );

        let TranscriptItem::Assistant { children, .. } = &state.transcript[0] else {
            panic!("assistant item")
        };
        let thinking = children
            .iter()
            .filter_map(|child| match child {
                AssistantChild::Thinking { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(thinking, vec!["first second"]);
    }

    #[test]
    fn tool_termination_clears_streamed_output_and_sets_detail() {
        let call_id = ToolCallId::new_v7();
        let owner = AssistantToolCallRef {
            model_turn_seq: 1,
            content_index: 0,
            model_call_id: cookie_agent_protocol::ModelCallId::new("call").expect("model call id"),
            provider_item_id: None,
        };
        let mut state = SessionState::default();
        state.tools.insert(
            call_id,
            ToolCallState {
                id: call_id,
                owner: owner.clone(),
                presentation: cookie_agent_protocol::ToolCallPresentation {
                    title: cookie_agent_protocol::SafeDisplayText::new("Bash")
                        .expect("presentation title"),
                    primary_argument: None,
                },
                arguments: "{}".into(),
                status: ToolStatus::Running,
                detail: String::new(),
                has_output_chunks: false,
            },
        );
        state
            .output
            .insert((call_id, false), OrderedOutput::default());
        state
            .output
            .insert((call_id, true), OrderedOutput::default());

        reduce_event(
            &mut state,
            SessionId::new_v7(),
            None,
            1,
            jiff::Timestamp::now(),
            EventPayload::ToolCallProgress {
                tool_call_id: call_id,
                message: cookie_agent_protocol::SafeDisplayText::new("bash stdout")
                    .expect("progress message"),
                output_chunk: Some(
                    cookie_agent_protocol::SafeDisplayText::new("streamed preview")
                        .expect("output chunk"),
                ),
            },
        );
        assert_eq!(state.tools[&call_id].detail, "streamed preview");
        assert!(state.tools[&call_id].has_output_chunks);

        reduce_event(
            &mut state,
            SessionId::new_v7(),
            None,
            2,
            jiff::Timestamp::now(),
            EventPayload::ToolCallTerminated {
                termination: cookie_agent_protocol::ToolCallTermination {
                    tool_call_id: call_id,
                    owner,
                    outcome: ToolTerminationOutcome::Completed,
                    result: Some(cookie_agent_protocol::PersistedToolResult {
                        title: cookie_agent_protocol::SafeDisplayText::new("Bash")
                            .expect("result title"),
                        output: "stdout:\nonce\n\nstderr:\n".into(),
                        metadata: serde_json::Value::Null,
                        truncation: None,
                        attachments: Vec::new(),
                        additional_messages: vec![
                            cookie_agent_protocol::ToolEmittedMessage::new(
                                cookie_agent_protocol::ToolEmittedMessageRole::User,
                                vec![
                                    cookie_agent_protocol::ToolEmittedContent::Text(
                                        "review this clip".into(),
                                    ),
                                    cookie_agent_protocol::ToolEmittedContent::File(
                                        cookie_agent_protocol::ToolAttachment {
                                            mime_type: cookie_agent_protocol::MimeType::new(
                                                "video/mp4",
                                            )
                                            .unwrap(),
                                            filename: Some("clip.mp4".into()),
                                            byte_length: 4,
                                            sha256: cookie_agent_protocol::Sha256Digest::of_bytes(
                                                b"clip",
                                            ),
                                            reference: cookie_agent_protocol::ArtifactReference {
                                                uri: format!(
                                                    "artifact://sha256/{}",
                                                    cookie_agent_protocol::Sha256Digest::of_bytes(
                                                        b"clip"
                                                    )
                                                ),
                                            },
                                        },
                                    ),
                                ],
                            )
                            .unwrap(),
                        ],
                    }),
                    error: None,
                },
            },
        );

        assert!(!state.output.contains_key(&(call_id, false)));
        assert!(!state.output.contains_key(&(call_id, true)));
        assert_eq!(state.tools[&call_id].status, ToolStatus::Completed);
        assert!(!state.tools[&call_id].has_output_chunks);
        assert_eq!(
            state.tools[&call_id].detail,
            format!(
                "Bash\nstdout:\nonce\n\nstderr:\n\nemitted user message:\ntext: review this clip\nfile: video/mp4 · 4 bytes · sha256:{digest} · artifact://sha256/{digest}",
                digest = cookie_agent_protocol::Sha256Digest::of_bytes(b"clip")
            )
        );
    }

    #[test]
    fn committed_assistant_text_replaces_streamed_delta() {
        let item_id = 1;
        let mut state = assistant_state_with_item(item_id);
        append_assistant_delta(
            &mut state,
            item_id,
            10,
            "streamed draft".into(),
            AssistantPartKind::Text,
            jiff::Timestamp::now(),
        );
        let turn = PersistedModelTurn {
            content: vec![cookie_agent_protocol::PersistedAssistantPart::Text {
                text: "committed replacement".into(),
                metadata: None,
            }],
            provider_options: BTreeMap::new(),
            finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
            usage: Usage::default(),
            response_metadata: BTreeMap::new(),
            provider_metadata: BTreeMap::new(),
            native_replay: None,
        };

        rebuild_committed_children(&mut state, item_id, 1, 20, &turn);

        let TranscriptItem::Assistant { children, .. } = &state.transcript[0] else {
            panic!("assistant item")
        };
        assert!(matches!(
            children.as_slice(),
            [AssistantChild::Text { markdown, .. }]
                if markdown.as_str() == "committed replacement"
        ));
    }

    #[test]
    fn replay_with_tool_chunks_ends_at_committed_result_only() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let call_id = ToolCallId::new_v7();
        let owner = AssistantToolCallRef {
            model_turn_seq: 1,
            content_index: 0,
            model_call_id: cookie_agent_protocol::ModelCallId::new("replay-call")
                .expect("model call id"),
            provider_item_id: None,
        };
        let event = |seq, payload| StoredEvent {
            engine_version: None,
            origin: None,
            session_id,
            run_id: Some(run_id),
            seq,
            timestamp: jiff::Timestamp::now(),
            payload,
        };
        let events = vec![
            event(
                1,
                EventPayload::ToolCallStarted {
                    start: cookie_agent_protocol::ToolCallStart {
                        tool_call_id: call_id,
                        owner: owner.clone(),
                        presentation: cookie_agent_protocol::ToolCallPresentation {
                            title: cookie_agent_protocol::SafeDisplayText::new("Bash")
                                .expect("title"),
                            primary_argument: None,
                        },
                        operation_fingerprint: serde_json::from_value(serde_json::json!({
                            "digest": "1".repeat(64)
                        }))
                        .expect("fingerprint"),
                    },
                },
            ),
            event(
                2,
                EventPayload::ToolCallProgress {
                    tool_call_id: call_id,
                    message: cookie_agent_protocol::SafeDisplayText::new("bash stdout")
                        .expect("message"),
                    output_chunk: Some(
                        cookie_agent_protocol::SafeDisplayText::new("historical chunk")
                            .expect("chunk"),
                    ),
                },
            ),
            event(
                3,
                EventPayload::ToolCallTerminated {
                    termination: cookie_agent_protocol::ToolCallTermination {
                        tool_call_id: call_id,
                        owner,
                        outcome: ToolTerminationOutcome::Completed,
                        result: Some(cookie_agent_protocol::PersistedToolResult {
                            title: cookie_agent_protocol::SafeDisplayText::new("Bash")
                                .expect("title"),
                            output: "committed replacement".into(),
                            metadata: serde_json::Value::Null,
                            truncation: None,
                            attachments: Vec::new(),
                            additional_messages: Vec::new(),
                        }),
                        error: None,
                    },
                },
            ),
        ];

        let replayed = reduce_session_events(session_id, 0, &events);
        assert_eq!(
            replayed.tools[&call_id].detail,
            "Bash\ncommitted replacement"
        );
        assert!(!replayed.tools[&call_id].detail.contains("historical chunk"));
        assert!(!replayed.tools[&call_id].has_output_chunks);
    }

    #[test]
    fn approval_escalation_is_info_while_rejected_and_expired_keep_existing_levels() {
        let session_id = SessionId::new_v7();
        let approval_id = ApprovalId::new_v7();
        let mut state = SessionState::default();

        reduce_event(
            &mut state,
            session_id,
            None,
            1,
            jiff::Timestamp::now(),
            EventPayload::ApprovalEscalated {
                approval_id,
                reason_code: cookie_agent_protocol::ApprovalReasonCode::Escalated,
            },
        );
        reduce_event(
            &mut state,
            session_id,
            None,
            2,
            jiff::Timestamp::now(),
            EventPayload::ApprovalFinalized {
                approval_id,
                decision: cookie_agent_protocol::ApprovalFinalDecision {
                    outcome: ApprovalFinalOutcome::Rejected,
                    source: cookie_agent_protocol::ApprovalDecisionSource::Policy,
                    reason_code: cookie_agent_protocol::ApprovalReasonCode::PolicyDenied,
                    feedback: None,
                    tree_grant_id: None,
                },
            },
        );
        reduce_event(
            &mut state,
            session_id,
            None,
            3,
            jiff::Timestamp::now(),
            EventPayload::ApprovalFinalized {
                approval_id,
                decision: cookie_agent_protocol::ApprovalFinalDecision {
                    outcome: ApprovalFinalOutcome::Expired,
                    source: cookie_agent_protocol::ApprovalDecisionSource::System,
                    reason_code: cookie_agent_protocol::ApprovalReasonCode::ApprovalExpired,
                    feedback: None,
                    tree_grant_id: None,
                },
            },
        );

        let levels = state
            .transcript
            .iter()
            .map(|item| match item {
                TranscriptItem::Event { level, .. } => *level,
                _ => panic!("approval lifecycle rows must be events"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            levels,
            vec![EventLevel::Info, EventLevel::Info, EventLevel::Info]
        );
    }

    fn timestamp(iso: &str) -> jiff::Timestamp {
        iso.parse().expect("timestamp")
    }

    fn assistant_state_with_item(item_id: u64) -> SessionState {
        SessionState {
            transcript: vec![TranscriptItem::Assistant {
                id: item_id,
                version: 0,
                attribution: FrozenAssistantAttribution {
                    agent: AgentId::new("test").expect("agent id"),
                    resolved_model: serde_json::from_value(serde_json::json!({
                        "provider_id": "test",
                        "model_id": "test",
                        "adapter_id": "openai-compatible",
                        "selection": {"model": "test/test", "variant": null},
                        "selection_fingerprint": "a".repeat(64)
                    }))
                    .expect("resolved model"),
                },
                committed_turn_seq: None,
                children: Vec::new(),
            }],
            ..SessionState::default()
        }
    }

    #[test]
    fn thinking_durations_derive_from_durable_event_timestamps() {
        let item_id = 1;
        let mut state = assistant_state_with_item(item_id);
        let opened = timestamp("2026-08-07T10:00:00Z");
        append_assistant_delta(
            &mut state,
            item_id,
            10,
            "hmm".into(),
            AssistantPartKind::Thinking,
            opened,
        );
        assert!(state.has_open_thinking());
        // Continuing the open part keeps its original opening timestamp.
        append_assistant_delta(
            &mut state,
            item_id,
            11,
            "…".into(),
            AssistantPartKind::Thinking,
            timestamp("2026-08-07T10:00:01Z"),
        );
        close_open_assistant(&mut state, timestamp("2026-08-07T10:00:04Z"));
        assert!(!state.has_open_thinking());
        assert_eq!(
            state.thinking_duration(item_id, 10),
            Some(Duration::from_secs(4))
        );

        // Clock-skewed (negative) spans record nothing rather than panic.
        append_assistant_delta(
            &mut state,
            item_id,
            20,
            "again".into(),
            AssistantPartKind::Thinking,
            timestamp("2026-08-07T11:00:00Z"),
        );
        close_open_assistant(&mut state, timestamp("2026-08-07T10:59:00Z"));
        assert_eq!(state.thinking_duration(item_id, 20), None);
    }

    #[test]
    fn thinking_duration_transfers_to_the_committed_child_on_rebuild() {
        let item_id = 1;
        let mut state = assistant_state_with_item(item_id);
        append_assistant_delta(
            &mut state,
            item_id,
            10,
            "streamed".into(),
            AssistantPartKind::Thinking,
            timestamp("2026-08-07T10:00:00Z"),
        );
        close_open_assistant(&mut state, timestamp("2026-08-07T10:00:07Z"));
        assert_eq!(
            state.thinking_duration(item_id, 10),
            Some(Duration::from_secs(7))
        );

        let turn = PersistedModelTurn {
            content: vec![cookie_agent_protocol::PersistedAssistantPart::Reasoning {
                text: "streamed".into(),
                metadata: None,
            }],
            provider_options: BTreeMap::new(),
            finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
            usage: Usage::default(),
            response_metadata: BTreeMap::new(),
            provider_metadata: BTreeMap::new(),
            native_replay: None,
        };
        rebuild_committed_children(&mut state, item_id, 1, 20, &turn);
        // The streamed part id is gone; the committed child carries the time.
        assert_eq!(state.thinking_duration(item_id, 10), None);
        let TranscriptItem::Assistant { children, .. } = &state.transcript[0] else {
            panic!("assistant item")
        };
        let committed_id = children
            .iter()
            .find_map(|child| match child {
                AssistantChild::Thinking { id, .. } => Some(*id),
                _ => None,
            })
            .expect("committed thinking child");
        assert_ne!(committed_id, 10);
        assert_eq!(
            state.thinking_duration(item_id, committed_id),
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn session_revert_rebuilds_transcript_across_a_persisted_sequence_gap() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let event = |seq, payload| StoredEvent {
            engine_version: None,
            origin: None,
            session_id,
            run_id: Some(run_id),
            seq,
            timestamp: jiff::Timestamp::now(),
            payload,
        };
        let mut store = StateStore::default();
        assert!(store.apply_event(event(
            1,
            EventPayload::UserInputSubmitted {
                input: "kept".into(),
            },
        )));
        assert!(store.apply_event(event(
            3,
            EventPayload::UserInputSubmitted {
                input: "removed".into(),
            },
        )));
        let mut reverted = event(4, EventPayload::SessionReverted { through_seq: 1 });
        reverted.run_id = None;
        assert!(store.apply_event(reverted));
        let state = store.sessions.get(&session_id).expect("session state");
        assert_eq!(state.last_seq, 4);
        assert_eq!(state.transcript.len(), 1);
        assert!(matches!(
            &state.transcript[0],
            TranscriptItem::User { text, .. } if text == "kept"
        ));
    }

    #[test]
    fn delegate_and_steer_tool_starts_update_agent_activity() {
        let session_id = SessionId::new_v7();
        let mut state = SessionState::default();
        let fingerprint = || {
            serde_json::from_value(serde_json::json!({
                "digest": "1".repeat(64)
            }))
            .expect("operation fingerprint")
        };
        let start = |model_turn_seq, model_call_id: &str| {
            let model_call_id =
                cookie_agent_protocol::ModelCallId::new(model_call_id).expect("model call ID");
            cookie_agent_protocol::ToolCallStart {
                tool_call_id: ToolCallId::new_v7(),
                owner: cookie_agent_protocol::AssistantToolCallRef {
                    model_turn_seq,
                    content_index: 0,
                    model_call_id,
                    provider_item_id: None,
                },
                presentation: cookie_agent_protocol::ToolCallPresentation {
                    title: cookie_agent_protocol::SafeDisplayText::new("activity tool")
                        .expect("title"),
                    primary_argument: None,
                },
                operation_fingerprint: fingerprint(),
            }
        };
        state.turn_tool_index.insert(
            (1, "delegate-call".into()),
            IndexedToolCall {
                name: SafeCode::new("delegate_subagent").expect("tool name"),
                arguments: "{}".into(),
            },
        );
        state.turn_tool_index.insert(
            (2, "steer-call".into()),
            IndexedToolCall {
                name: SafeCode::new("steer_subagent").expect("tool name"),
                arguments: "{}".into(),
            },
        );
        state.turn_tool_index.insert(
            (3, "read-call".into()),
            IndexedToolCall {
                name: SafeCode::new("read").expect("tool name"),
                arguments: "{}".into(),
            },
        );

        let delegated_at = "2026-08-06T11:00:00Z".parse().expect("timestamp");
        reduce_event(
            &mut state,
            session_id,
            None,
            1,
            delegated_at,
            EventPayload::ToolCallStarted {
                start: start(1, "delegate-call"),
            },
        );
        assert_eq!(state.last_agent_activity, Some(delegated_at));

        let steered_at = "2026-08-06T12:00:00Z".parse().expect("timestamp");
        reduce_event(
            &mut state,
            session_id,
            None,
            2,
            steered_at,
            EventPayload::ToolCallStarted {
                start: start(2, "steer-call"),
            },
        );
        assert_eq!(state.last_agent_activity, Some(steered_at));

        reduce_event(
            &mut state,
            session_id,
            None,
            3,
            "2026-08-06T13:00:00Z".parse().expect("timestamp"),
            EventPayload::ToolCallStarted {
                start: start(3, "read-call"),
            },
        );
        assert_eq!(state.last_agent_activity, Some(steered_at));
    }
}
