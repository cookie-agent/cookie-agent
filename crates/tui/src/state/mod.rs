//! Disposable UI projections reduced from protocol-10 stored events.
//!
//! Assistant attribution is derived only from the frozen `RunStarted` plus
//! `ModelAttemptStarted`/`ModelTurnCommitted` ownership — never from the
//! current picker, live agent files, or provider configuration. The visible
//! assistant header projects the exact canonical `Agent • Model[variant]`.

mod runtime;

pub use runtime::{EMPTY_RUNTIME_GUIDANCE, RuntimePhase, RuntimeState};

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use cookie_agent_protocol::{
    AgentId, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation, ApprovalFinalOutcome,
    ApprovalId, ApprovalRecord, ApprovalRequest, ApprovalStatus, ApprovalTrigger,
    AssistantToolCallRef, AttemptId, EventPayload, EventSubscriptionMessage, GoalId,
    GoalReminderIdentity, GoalState, GoalStatus, ModelErrorSummary, OperationFingerprint,
    PersistedModelTurn, PreparedApprovalResource, PreparedCapabilityLifetime, ProducerDeliveryMode,
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
/// title uses only the persisted `ToolCallPresentation`; raw arguments appear
/// only in the expanded detail. The persisted display argument is byte-capped
/// but may still carry control characters, so render sites flatten it.
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
    /// persisted display argument, never reparsed from raw input. The argument is
    /// byte-capped but not flattened, so a render site must sanitize it and
    /// abbreviate it to its own budget.
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
        /// The activation event projects the accepted goal action, not a user
        /// prompt sent to the model. Its producer owns the start message.
        activation: bool,
        goal: GoalState,
    },
    ProducerMessage {
        id: u64,
        /// Acceptance sequence for queue ordering. The row itself moves to
        /// the effective admission boundary in the conversation.
        seq: u64,
        /// Durable timestamp of the accepting event, retained independently
        /// from the pruned generation-timing index for stable queue age.
        accepted_at: jiff::Timestamp,
        message_id: ProducerMessageId,
        producer_owner: ProducerOwner,
        mode: ProducerDeliveryMode,
        body: String,
        /// Backend description or legacy goal summary, frozen at acceptance.
        summary: Option<String>,
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
        /// The committed call's tool name, so the placeholder can render a
        /// pending row instead of an error while execution has not started.
        name: SafeCode,
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
#[derive(Clone, Debug)]
pub(crate) struct AttemptProjection {
    item_id: u64,
    run_id: Option<RunId>,
    /// Retained independently of the currently open run segment so a later
    /// input boundary cannot make this attempt erase older committed children.
    committed_prefix: usize,
    attribution_marker: Option<usize>,
    /// Earlier blocks this attempt streamed into before interleaved event
    /// rows split them off, each with the committed prefix it keeps. The
    /// attempt's uncommitted output in those blocks is pruned when the
    /// attempt abandons or its turn commits (the committed turn rebuilds
    /// canonically in the newest block).
    split_segments: Vec<(u64, usize)>,
}

/// The assistant item accumulating attempts until the run's next input boundary.
#[derive(Clone, Debug)]
pub(crate) struct RunAssistantProjection {
    pub(crate) run_id: RunId,
    pub(crate) item_id: u64,
    pub(crate) committed_prefix: usize,
    pub(crate) current_model: ResolvedModelRef,
    /// An event row interleaved after this block while a turn was in flight.
    /// The part streaming at that moment finishes in this block, but the
    /// next new segment (a part of another kind, a tool call, or a new
    /// attempt) opens a fresh block after the row.
    pub(crate) split_pending: bool,
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
    /// A persisted artifact failed validation for the current target (for
    /// example cross-adapter history after a fallback). Keyed by reason so
    /// one logical incompatiblity warns once per run even though every later
    /// attempt re-evaluates the same history entries.
    InvalidPayload {
        reason: String,
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
    pub model_selection: cookie_agent_protocol::SessionModelState,
    pub goal: Option<GoalState>,
    pub transcript: Vec<TranscriptItem>,
    /// Durable insertion time per transcript item id. Cross-session rows such
    /// as aggregated descendant warnings merge into a viewed transcript by
    /// this time, so mid-conversation breaks render at their chronological
    /// position instead of the bottom.
    pub(crate) item_times: HashMap<u64, jiff::Timestamp>,
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
    /// Assistant blocks whose run ended in `RunInterrupted`; their footer
    /// reads `… · interrupted`.
    pub(crate) interrupted_assistant_items: HashSet<u64>,
    pub(crate) attempts: HashMap<AttemptId, AttemptProjection>,
    /// The latest attempt until its first delta, commit, or abandonment.
    pub(crate) pending_attempt: Option<AttemptId>,
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
    /// Adapter warnings already surfaced for a run. After a model fallback the
    /// same reconstruction warnings regenerate on every committed turn; each
    /// distinct warning text is shown only once per run.
    pub(crate) model_turn_warnings: HashSet<(RunId, String)>,
    pub(crate) goal_revisions: HashMap<GoalId, u64>,
    pub(crate) producer_messages: HashMap<ProducerMessageId, ProducerMessageProjection>,
    pub(crate) producer_dedup: HashMap<(ProducerOwner, ProducerIdempotencyKey), ProducerMessageId>,
    pub(crate) producer_claims: HashMap<u64, ProducerClaimProjection>,
    pub(crate) terminal_runs: HashSet<RunId>,
    pub approvals: Vec<ApprovalState>,
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

    /// Whether any producer message is waiting for a model run to claim it.
    pub fn has_pending_producers(&self) -> bool {
        self.transcript.iter().any(|item| {
            matches!(
                item,
                TranscriptItem::ProducerMessage {
                    status: ProducerMessageStatus::Pending | ProducerMessageStatus::Admitted,
                    ..
                }
            )
        })
    }

    /// The sealed elapsed thinking duration for one part, when known.
    pub fn thinking_duration(&self, item_id: u64, part_id: u64) -> Option<Duration> {
        self.thinking_durations.get(&(item_id, part_id)).copied()
    }

    /// Durable insertion time of one transcript item, when tracked.
    pub(crate) fn item_time(&self, item_id: u64) -> Option<jiff::Timestamp> {
        self.item_times.get(&item_id).copied()
    }

    /// Mark the open run block split-pending so the next new segment opens a
    /// fresh block below an interleaved event row. Shared by in-session
    /// warning rows and aggregated descendant warnings arriving from another
    /// session while this one streams.
    pub(crate) fn mark_event_split_pending(&mut self) {
        if (self.open_assistant.is_some() || self.pending_attempt.is_some())
            && let Some(projection) = self.open_run_assistant.as_mut()
        {
            projection.split_pending = true;
        }
    }

    /// Split the open run block at a committed context checkpoint. A
    /// checkpoint is durable context history the run's output must not
    /// straddle, so unlike an interleaved event row — which splits only while
    /// something streams — it always splits: the next new segment (part, tool
    /// call, or attempt) opens a fresh block below the compaction row. A block
    /// that never committed anything (an abandoned attempt's pruned partials)
    /// has nothing to split off: its index comes back instead — with any split
    /// an earlier row left pending consumed, exactly as relocating an empty
    /// block does for a mid-stream row — so the caller can move the block
    /// itself below the row and the retry keeps using it, rather than leaving
    /// an empty header stranded on either side of the marker.
    pub(crate) fn split_run_at_compaction(&mut self) -> Option<usize> {
        let item_id = self
            .open_run_assistant
            .as_ref()
            .map(|projection| projection.item_id)?;
        let index = self
            .transcript
            .iter()
            .position(|item| item.id() == item_id)?;
        if matches!(
            &self.transcript[index],
            TranscriptItem::Assistant {
                children,
                committed_turn_seq,
                ..
            } if children.is_empty() && committed_turn_seq.is_none()
        ) {
            // The relocated block is the one the next segment continues in, so
            // moving it consumes any split left pending by an earlier row
            // instead of making the retry open a second block around it.
            let projection = self
                .open_run_assistant
                .as_mut()
                .expect("located run projection");
            projection.split_pending = false;
            projection.committed_prefix = 0;
            return Some(index);
        }
        self.open_run_assistant
            .as_mut()
            .expect("located run projection")
            .split_pending = true;
        None
    }
}

/// All currently observed session projections.
#[derive(Clone, Debug, Default)]
pub struct StateStore {
    pub sessions: HashMap<SessionId, SessionState>,
    physical_events: HashMap<SessionId, Vec<StoredEvent>>,
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

const REPLAY_END_TIMEOUT: Duration = Duration::from_secs(5);

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
            // Display comes from durable progress/terminal events. Do not retain
            // raw payloads even when a caller bypasses the display-only client.
            ClientDelivery::OutputSnapshot(snapshot) => {
                if let Some(session_id) = self.quarantined_output(snapshot.snapshot.call_id) {
                    return DeliveryOutcome::ReplayFailed { session_id };
                }
                DeliveryOutcome::Applied
            }
            ClientDelivery::OutputDelta(delta) => {
                if let Some(session_id) = self.quarantined_output(delta.call_id) {
                    return DeliveryOutcome::ReplayFailed { session_id };
                }
                DeliveryOutcome::Applied
            }
            ClientDelivery::OutputGap(gap) => {
                if let Some(session_id) = self.quarantined_output(gap.call_id) {
                    return DeliveryOutcome::ReplayFailed { session_id };
                }
                DeliveryOutcome::Applied
            }
            ClientDelivery::RecoveryFailed { .. } => DeliveryOutcome::Applied,
            ClientDelivery::Disconnected { error } => {
                // A live transport notification, not a durable log event: wall
                // clock is the only available ordering signal.
                let timestamp = jiff::Timestamp::now();
                for state in self.sessions.values_mut() {
                    push_event(
                        state,
                        EventLevel::Error,
                        format!("connection failed: {error}"),
                        timestamp,
                    );
                }
                DeliveryOutcome::Applied
            }
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
    state.model_selection.apply(run_id, &payload);
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
            push_item(state, timestamp, |id| TranscriptItem::Goal {
                id,
                seq: sequence,
                activation: true,
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
            push_item(state, timestamp, |id| TranscriptItem::Goal {
                id,
                seq: sequence,
                activation: false,
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
            push_item(state, timestamp, |id| TranscriptItem::Goal {
                id,
                seq: sequence,
                activation: false,
                goal: snapshot,
            });
        }
        EventPayload::ProducerMessageAccepted {
            message_id,
            producer_owner,
            mode,
            idempotency_key,
            body,
            description,
            reminder,
            // Internal guard metadata: never transcript-visible, so replay
            // ignores it rather than carrying a second projection.
            agent_hop: _,
        } => {
            if !valid_producer_reminder_owner(&producer_owner, reminder.as_ref())
                || state.producer_messages.contains_key(&message_id)
                || state
                    .producer_dedup
                    .contains_key(&(producer_owner.clone(), idempotency_key.clone()))
            {
                return;
            }
            let summary = (!description.as_str().trim().is_empty())
                .then(|| description.as_str().to_owned())
                .or_else(|| {
                    let ProducerOwner::Goal { goal_id } = &producer_owner else {
                        return None;
                    };
                    let reminder = reminder.as_ref()?;
                    let goal = state
                        .goal
                        .as_ref()
                        .filter(|goal| goal.goal_id == *goal_id)
                        .or_else(|| {
                            state.transcript.iter().find_map(|item| match item {
                                TranscriptItem::Goal { goal, .. } if goal.goal_id == *goal_id => {
                                    Some(goal)
                                }
                                _ => None,
                            })
                        })?;
                    let label = match reminder.kind {
                        cookie_agent_protocol::GoalReminderKind::Started => "GoalStarted",
                        cookie_agent_protocol::GoalReminderKind::Continuation => "GoalContinue",
                    };
                    Some(format!("{label}: {}", goal.objective))
                });
            let transcript_index = state.transcript.len();
            push_item(state, timestamp, |id| TranscriptItem::ProducerMessage {
                id,
                seq: sequence,
                accepted_at: timestamp,
                message_id,
                producer_owner: producer_owner.clone(),
                mode,
                body,
                summary,
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
            let transcript_index = message.transcript_index;
            update_producer_message_status(state, message_id, status);
            move_input_to_boundary(state, transcript_index, Some(run_id), timestamp);
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
            state.pending_attempt = None;
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
            push_item(state, timestamp, |id| TranscriptItem::User {
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
            let mut attribution_marker = None;
            let item_id = if let Some(run_id) = run_id {
                if let Some(projection) = state
                    .open_run_assistant
                    .as_ref()
                    .filter(|projection| projection.run_id == run_id && !projection.split_pending)
                {
                    let item_id = projection.item_id;
                    let changed = projection.current_model != resolved_model;
                    if changed {
                        attribution_marker =
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
                        timestamp,
                    );
                    state.open_run_assistant = Some(RunAssistantProjection {
                        run_id,
                        item_id,
                        committed_prefix: 0,
                        current_model: resolved_model,
                        split_pending: false,
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
                    timestamp,
                )
            };
            let committed_prefix = state
                .open_run_assistant
                .as_ref()
                .filter(|projection| projection.item_id == item_id)
                .map_or(0, |projection| projection.committed_prefix);
            state.attempts.insert(
                attempt_id,
                AttemptProjection {
                    item_id,
                    run_id,
                    committed_prefix,
                    attribution_marker,
                    split_segments: Vec::new(),
                },
            );
            state.pending_attempt = Some(attempt_id);
        }
        EventPayload::TextDelta { attempt_id, text } => {
            // Empty deltas (some providers emit an initial empty content
            // chunk) carry no content: they neither open a part nor count
            // as the attempt's first output.
            if text.is_empty() {
                return;
            }
            if state.pending_attempt == Some(attempt_id) {
                state.pending_attempt = None;
            }
            let Some(item_id) =
                assistant_segment_target(state, attempt_id, AssistantPartKind::Text, timestamp)
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
            // Empty deltas (some providers emit an initial empty content
            // chunk) carry no content: they neither open a part nor count
            // as the attempt's first output.
            if text.is_empty() {
                return;
            }
            if state.pending_attempt == Some(attempt_id) {
                state.pending_attempt = None;
            }
            let Some(item_id) =
                assistant_segment_target(state, attempt_id, AssistantPartKind::Thinking, timestamp)
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
        EventPayload::AttemptAbandoned {
            attempt_id,
            model_error,
        } => {
            close_open_assistant(state, timestamp);
            if state.pending_attempt == Some(attempt_id) {
                state.pending_attempt = None;
            }
            if let Some(attempt) = state.attempts.remove(&attempt_id)
                && attempt.run_id.is_some()
            {
                prune_split_segments(state, &attempt.split_segments);
                prune_abandoned_attempt(state, attempt.item_id, attempt.committed_prefix);
            }
            let message = match &model_error {
                Some(error) => format!("model attempt abandoned: {}", render_model_error(error)),
                None => "model attempt abandoned".into(),
            };
            push_event(state, EventLevel::Warning, message, timestamp);
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
            if state.pending_attempt == Some(attempt_id) {
                state.pending_attempt = None;
            }
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
            // A turn carrying tool calls or media starts new segments: when
            // an event row interleaved mid-turn, the whole turn's canonical
            // rebuild belongs in a fresh block below the row.
            if turn.content.iter().any(|part| {
                matches!(
                    part,
                    cookie_agent_protocol::PersistedAssistantPart::ToolCall { .. }
                        | cookie_agent_protocol::PersistedAssistantPart::File { .. }
                )
            }) {
                split_assistant_if_pending(state, attempt_id, timestamp);
            }
            if let Some(projection) = state.attempts.get(&attempt_id) {
                let item_id = projection.item_id;
                let committed_prefix = projection.committed_prefix;
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
                rebuild_committed_children(
                    state,
                    item_id,
                    model_turn_seq,
                    sequence,
                    committed_prefix,
                    &turn,
                );
                // The committed turn rebuilt canonically in the newest block,
                // so the attempt's pre-split streamed output in earlier
                // blocks is superseded.
                let segments = state
                    .attempts
                    .get_mut(&attempt_id)
                    .map(|attempt| std::mem::take(&mut attempt.split_segments))
                    .unwrap_or_default();
                prune_split_segments(state, &segments);
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
                timestamp,
            );
            for warning in warnings {
                if let Some(run_id) = run_id
                    && !state
                        .model_turn_warnings
                        .insert((run_id, warning.to_string()))
                {
                    continue;
                }
                push_event(
                    state,
                    EventLevel::Warning,
                    format!(
                        "model warning from {}: {warning}",
                        render_model(&resolved_model)
                    ),
                    timestamp,
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
                    timestamp,
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
                    timestamp,
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
                timestamp,
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
            message: _,
            display,
        } => {
            if let Some(tool) = state.tools.get_mut(&tool_call_id)
                && let Some(display) = display
            {
                if !tool.has_output_chunks {
                    tool.detail.clear();
                    tool.has_output_chunks = true;
                }
                let room =
                    cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES.saturating_sub(tool.detail.len());
                let mut end = display.as_str().len().min(room);
                while !display.as_str().is_char_boundary(end) {
                    end -= 1;
                }
                tool.detail.push_str(&display.as_str()[..end]);
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
            let failure_message = matches!(
                termination.outcome,
                ToolTerminationOutcome::Failed | ToolTerminationOutcome::Interrupted
            )
            .then(|| cookie_agent_protocol::diagnostics::tool(&termination));
            let streamed = state
                .tools
                .get(&tool_call_id)
                .filter(|tool| tool.has_output_chunks && !tool.detail.trim().is_empty())
                .map(|tool| tool.detail.clone());
            // Named capture can contain just [stdout]/[stderr] headings with no bytes.
            let partial_output = termination.result.as_ref().is_some_and(|result| {
                !result.output.trim().is_empty()
                    && result.retained_output.as_ref().is_none_or(|retained| {
                        retained.streams.iter().any(|stream| stream.byte_length > 0)
                    })
            });
            let has_output = streamed.is_some()
                || partial_output
                || termination.result.as_ref().is_some_and(|result| {
                    result
                        .display
                        .as_ref()
                        .is_some_and(|display| !display.trim().is_empty())
                });
            let mut detail = match (termination.result, termination.error) {
                (Some(result), _) if failed && partial_output => result.output,
                (Some(result), _)
                    if failed
                        && result
                            .display
                            .as_ref()
                            .is_none_or(|display| display.trim().is_empty())
                        && streamed.is_some() =>
                {
                    streamed.unwrap_or_default()
                }
                (Some(result), _) if result.display.is_some() => result.display.unwrap_or_default(),
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
                (_, _) if failed && streamed.is_some() => streamed.unwrap_or_default(),
                (_, Some(error)) => error.message.to_string(),
                _ => String::new(),
            };
            if failed
                && has_output
                && let Some(message) = &failure_message
                && !detail.starts_with(message)
            {
                // The renderer bounds output lines; keep the failure reason ahead of them.
                detail = format!("{message}\n{detail}");
            }
            if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                tool.status = status;
                tool.detail = detail;
                tool.has_output_chunks = false;
            }
            // A failed call is fed back to the model as its tool result and
            // carries the failure inline on its tool item; only surface an
            // event row when no tool item exists to hold it.
            if let Some(message) =
                failure_message.filter(|_| !state.tools.contains_key(&tool_call_id))
            {
                push_event(
                    state,
                    EventLevel::Error,
                    format!("tool {tool_call_id}: {message}"),
                    timestamp,
                );
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
            ..
        } => push_event(
            state,
            EventLevel::Debug,
            format!(
                "approval {approval_id} evaluated: {:?} ({:?})",
                decision.decision, decision.reason_code
            )
            .to_lowercase(),
            timestamp,
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
                timestamp,
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
            timestamp,
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
                timestamp,
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
                timestamp,
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
            timestamp,
        ),
        EventPayload::TreeApprovalGrantCommitted { grant } => push_event(
            state,
            EventLevel::Debug,
            format!(
                "tree approval grant {} committed for {}",
                grant.grant_id,
                grant.operation_fingerprint.digest()
            ),
            timestamp,
        ),
        EventPayload::RunCompleted { .. } => {
            close_open_assistant(state, timestamp);
            state.open_run_assistant = None;
            state.pending_attempt = None;
            state.active_run = None;
            state.attempts.clear();
            state.pending_tool_rows.clear();
            void_pending_inputs(state);
            state.approvals.clear();
            if let Some(run_id) = run_id {
                state.terminal_runs.insert(run_id);
            }
            push_event(state, EventLevel::Info, "run completed".into(), timestamp);
        }
        EventPayload::RunFailed {
            error,
            model_error,
            resolved_model,
        } => {
            close_open_assistant(state, timestamp);
            state.open_run_assistant = None;
            state.pending_attempt = None;
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
                format!(
                    "run failed: {}",
                    cookie_agent_protocol::diagnostics::run_error(
                        &error,
                        model_error.as_ref(),
                        resolved_model.as_ref()
                    )
                ),
                timestamp,
            );
        }
        EventPayload::RunCancelled { reason } => {
            close_open_assistant(state, timestamp);
            state.open_run_assistant = None;
            state.pending_attempt = None;
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
                timestamp,
            );
        }
        EventPayload::RunInterrupted { reason } => {
            close_open_assistant(state, timestamp);
            // The run's open block keeps an `interrupted` footer marker.
            if let Some(projection) = state.open_run_assistant.take() {
                state.interrupted_assistant_items.insert(projection.item_id);
            }
            state.pending_attempt = None;
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
                timestamp,
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
            timestamp,
        ),
        EventPayload::InternalAgentCompleted { kind, result, .. } => push_event(
            state,
            EventLevel::Info,
            format!(
                "internal agent {kind:?} completed: {}",
                result.output_summary
            )
            .to_lowercase(),
            timestamp,
        ),
        EventPayload::InternalAgentFailed { kind, failure, .. } => push_event(
            state,
            EventLevel::Error,
            format!(
                "internal agent {kind:?} failed: {}",
                cookie_agent_protocol::diagnostics::internal(&failure)
            ),
            timestamp,
        ),
        EventPayload::InternalAgentCancelled { kind, reason, .. } => push_event(
            state,
            EventLevel::Info,
            reason.map_or_else(
                || format!("internal agent {kind:?} cancelled").to_lowercase(),
                |reason| format!("internal agent {kind:?} cancelled: {reason}").to_lowercase(),
            ),
            timestamp,
        ),
        EventPayload::InternalAgentInterrupted { kind, reason, .. } => push_event(
            state,
            EventLevel::Error,
            reason.map_or_else(
                || format!("internal agent {kind:?} interrupted").to_lowercase(),
                |reason| format!("internal agent {kind:?} interrupted: {reason}").to_lowercase(),
            ),
            timestamp,
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
            cookie_agent_protocol::diagnostics::internal_fallback(
                kind, &from, &to, attempts, &failure,
            ),
            timestamp,
        ),
        EventPayload::ContextCheckpointCommitted { commit } => {
            // The compaction row is the run's chronological boundary: the next
            // new segment opens fresh below it, and a block that never
            // committed anything is moved under the row itself instead of
            // being split off as an empty header.
            let relocate = state.split_run_at_compaction();
            push_item(state, timestamp, |id| TranscriptItem::Compaction {
                id,
                version: 0,
                seq: sequence,
                commit,
            });
            if let Some(index) = relocate {
                move_transcript_item_to_end(state, index);
            }
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
            timestamp,
        ),
        EventPayload::ContextRehydrated { files } => push_event(
            state,
            EventLevel::Info,
            format!("rehydrated {} recently read file(s)", files.len()),
            timestamp,
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
            timestamp,
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
            timestamp,
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
            timestamp,
        ),
        EventPayload::PluginEventAdded { plugin, name, .. } => push_event(
            state,
            EventLevel::Info,
            format!("plugin {plugin}: {name}"),
            timestamp,
        ),
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
            timestamp,
        ),
        EventPayload::SessionTitleCommitted { change, .. } => {
            push_event(
                state,
                EventLevel::Info,
                render_title_commit(&change),
                timestamp,
            );
        }
        EventPayload::SessionReverted { .. } => {
            close_open_assistant(state, timestamp);
            state.active_run = None;
        }
        EventPayload::UserInputApplied { user_input_seq } => {
            if let Some(index) = state.transcript.iter().position(
                |item| matches!(item, TranscriptItem::User { seq, .. } if *seq == user_input_seq),
            ) {
                move_input_to_boundary(state, index, run_id, timestamp);
            }
        }
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
                if let Some(TranscriptItem::Assistant { version, .. }) = state
                    .transcript
                    .iter_mut()
                    .find(|item| item.id() == item_id)
                {
                    *version = version.wrapping_add(1);
                }
            }
        }
        EventPayload::MessageInjected { role, input } => {
            push_item(state, timestamp, |id| TranscriptItem::PluginMessage {
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

/// Inputs end run-level grouping. Retries can start before input promotion;
/// only a retry with no output can move to the new segment.
fn move_input_to_boundary(
    state: &mut SessionState,
    index: usize,
    run_id: Option<RunId>,
    timestamp: jiff::Timestamp,
) {
    close_open_assistant(state, timestamp);
    let pending = state.pending_attempt.and_then(|attempt_id| {
        let attempt = state.attempts.get(&attempt_id)?.clone();
        let projection = state.open_run_assistant.as_ref()?;
        (Some(projection.run_id) == run_id && projection.item_id == attempt.item_id)
            .then(|| (attempt_id, attempt, projection.clone()))
    });
    state.open_run_assistant = None;
    move_transcript_item_to_end(state, index);
    if let Some((attempt_id, attempt, projection)) = pending {
        rebind_pending_attempt(state, attempt_id, attempt, projection, timestamp);
    }
}

fn rebind_pending_attempt(
    state: &mut SessionState,
    attempt_id: AttemptId,
    attempt: AttemptProjection,
    mut projection: RunAssistantProjection,
    timestamp: jiff::Timestamp,
) {
    let index = state
        .transcript
        .iter()
        .position(|item| item.id() == attempt.item_id)
        .expect("pending attempt owns an assistant item");
    let TranscriptItem::Assistant {
        attribution,
        children,
        committed_turn_seq,
        version,
        ..
    } = &mut state.transcript[index]
    else {
        unreachable!("pending attempt owns an assistant item")
    };
    let new_attribution = FrozenAssistantAttribution {
        agent: attribution.agent.clone(),
        resolved_model: projection.current_model.clone(),
    };
    // A fallback marker belongs to this unstreamed retry, not the old block.
    if let Some(marker) = attempt.attribution_marker {
        children.remove(marker);
        *version = version.wrapping_add(1);
    }
    let reuse_empty = children.is_empty() && committed_turn_seq.is_none();
    if reuse_empty {
        *attribution = new_attribution;
        *version = version.wrapping_add(1);
        move_transcript_item_to_end(state, index);
    } else {
        projection.item_id = open_assistant_item(state, new_attribution, timestamp);
    }
    projection.committed_prefix = 0;
    projection.split_pending = false;
    state.attempts.insert(
        attempt_id,
        AttemptProjection {
            item_id: projection.item_id,
            run_id: Some(projection.run_id),
            committed_prefix: 0,
            attribution_marker: None,
            split_segments: Vec::new(),
        },
    );
    state.open_run_assistant = Some(projection);
}

fn move_transcript_item_to_end(state: &mut SessionState, index: usize) {
    state.transcript[index..].rotate_left(1);
    let last = state.transcript.len() - 1;
    for message in state.producer_messages.values_mut() {
        if message.transcript_index == index {
            message.transcript_index = last;
        } else if message.transcript_index > index {
            message.transcript_index -= 1;
        }
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

/// Open a fresh assistant item for a run segment or run-less streaming attempt.
fn open_assistant_item(
    state: &mut SessionState,
    attribution: FrozenAssistantAttribution,
    timestamp: jiff::Timestamp,
) -> u64 {
    state.open_assistant = None;
    push_item(state, timestamp, |id| TranscriptItem::Assistant {
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

fn append_attribution(
    state: &mut SessionState,
    item_id: u64,
    resolved_model: ResolvedModelRef,
) -> Option<usize> {
    if let Some(TranscriptItem::Assistant {
        version, children, ..
    }) = state
        .transcript
        .iter_mut()
        .find(|item| item.id() == item_id)
    {
        let index = children.len();
        children.push(AssistantChild::Attribution { resolved_model });
        *version = version.wrapping_add(1);
        return Some(index);
    }
    None
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
    committed_prefix: usize,
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
            cookie_agent_protocol::PersistedAssistantPart::ToolCall { name, .. } => {
                children.push(AssistantChild::CommittedTool {
                    turn_seq: model_turn_seq,
                    content_index: index,
                    name: name.clone(),
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
        let committed_prefix = committed_prefix.min(existing.len());
        // Whitespace-only parts leading a block with no committed content
        // render as blank lines directly under the header; once the block
        // holds committed content the same parts are meaningful spacing
        // between sections and turns.
        if committed_prefix == 0 {
            let leading = children
                .iter()
                .take_while(|child| match child {
                    AssistantChild::Text { markdown, .. } => markdown.as_str().trim().is_empty(),
                    AssistantChild::Thinking { text, .. } => text.trim().is_empty(),
                    _ => false,
                })
                .count();
            children.drain(..leading);
        }
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
            ..
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

fn push_item(
    state: &mut SessionState,
    timestamp: jiff::Timestamp,
    item: impl FnOnce(u64) -> TranscriptItem,
) {
    state.next_transcript_id = state.next_transcript_id.wrapping_add(1).max(1);
    let id = state.next_transcript_id;
    state.item_times.insert(id, timestamp);
    state.transcript.push(item(id));
}

fn push_event(
    state: &mut SessionState,
    level: EventLevel,
    text: String,
    timestamp: jiff::Timestamp,
) {
    // A warning-or-worse row injected while an assistant turn is in flight
    // marks the run's block as split-pending: the part streaming right now
    // keeps streaming into the existing block above the row, but the next
    // new segment (part, tool call, or attempt) opens a fresh block below
    // it. Debug/Info rows (replay decisions, commit notices, lifecycle
    // chatter) never split, and neither do rows between turns, so one run's
    // turns keep sharing a block.
    if level >= EventLevel::Warning {
        state.mark_event_split_pending();
    }
    push_item(state, timestamp, |id| TranscriptItem::Event {
        id,
        version: 0,
        level,
        text,
    });
}

/// Rebind the attempt to a fresh assistant block after an interleaved event
/// row when its run's block is split-pending. Returns the item new segments
/// belong to. A still-empty, never-committed block simply moves below the
/// row instead of leaving a ghost block behind. The split-off block is
/// remembered so abandonment or the turn commit can prune the attempt's
/// uncommitted output there (the committed turn rebuilds canonically in the
/// newest block).
fn split_assistant_if_pending(
    state: &mut SessionState,
    attempt_id: AttemptId,
    timestamp: jiff::Timestamp,
) -> Option<u64> {
    let attempt = state.attempts.get(&attempt_id)?.clone();
    let split = state.open_run_assistant.as_ref().is_some_and(|projection| {
        projection.split_pending
            && Some(projection.run_id) == attempt.run_id
            && projection.item_id == attempt.item_id
    });
    if !split {
        return Some(attempt.item_id);
    }
    close_open_assistant(state, timestamp);
    let Some(index) = state
        .transcript
        .iter()
        .position(|item| item.id() == attempt.item_id)
    else {
        return Some(attempt.item_id);
    };
    let TranscriptItem::Assistant {
        attribution,
        children,
        committed_turn_seq,
        ..
    } = &state.transcript[index]
    else {
        return Some(attempt.item_id);
    };
    let agent = attribution.agent.clone();
    let reuse_empty = children.is_empty() && committed_turn_seq.is_none();
    let mut projection = state
        .open_run_assistant
        .take()
        .expect("split-pending run projection");
    projection.split_pending = false;
    if reuse_empty {
        move_transcript_item_to_end(state, index);
        projection.committed_prefix = 0;
        state.open_run_assistant = Some(projection);
        return Some(attempt.item_id);
    }
    let item_id = open_assistant_item(
        state,
        FrozenAssistantAttribution {
            agent,
            resolved_model: projection.current_model.clone(),
        },
        timestamp,
    );
    projection.item_id = item_id;
    projection.committed_prefix = 0;
    state.open_run_assistant = Some(projection);
    let mut split_segments = attempt.split_segments;
    split_segments.push((attempt.item_id, attempt.committed_prefix));
    state.attempts.insert(
        attempt_id,
        AttemptProjection {
            item_id,
            run_id: attempt.run_id,
            committed_prefix: 0,
            attribution_marker: None,
            split_segments,
        },
    );
    Some(item_id)
}

/// Prune one split-off block back to its committed prefix (keeping
/// attribution markers), then drop it entirely when nothing committed ever
/// landed there so no empty block is left behind.
fn prune_split_segment(state: &mut SessionState, item_id: u64, committed_prefix: usize) {
    prune_abandoned_attempt(state, item_id, committed_prefix);
    let empty = state.transcript.iter().any(|item| {
        matches!(
            item,
            TranscriptItem::Assistant {
                id,
                children,
                committed_turn_seq,
                ..
            } if *id == item_id && children.is_empty() && committed_turn_seq.is_none()
        )
    });
    if empty {
        state.transcript.retain(|item| item.id() != item_id);
        state.assistant_metrics.remove(&item_id);
    }
}

/// Prune the attempt's uncommitted output in every block an event row split
/// off before its current one.
fn prune_split_segments(state: &mut SessionState, segments: &[(u64, usize)]) {
    for (item_id, committed_prefix) in segments {
        prune_split_segment(state, *item_id, *committed_prefix);
    }
}

/// The item a delta belongs to: the open part's item while the same kind
/// keeps streaming (even across an interleaved event row), otherwise the
/// split-pending rebind target for a new segment.
fn assistant_segment_target(
    state: &mut SessionState,
    attempt_id: AttemptId,
    kind: AssistantPartKind,
    timestamp: jiff::Timestamp,
) -> Option<u64> {
    let attempt = state.attempts.get(&attempt_id)?;
    let continues_open_part = state
        .open_assistant
        .is_some_and(|open| open.item_id == attempt.item_id && open.kind == kind);
    if continues_open_part {
        return Some(attempt.item_id);
    }
    split_assistant_if_pending(state, attempt_id, timestamp)
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
        ReplayDisposition::DiscardedInvalidPayload { reason } => {
            ReplayContextTransition::InvalidPayload {
                reason: reason.to_string(),
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
    cookie_agent_protocol::diagnostics::model(error)
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

#[cfg(test)]
mod tests;
