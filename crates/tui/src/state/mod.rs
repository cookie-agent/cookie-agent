//! Disposable UI projections reduced from protocol-10 stored events.
//!
//! Assistant attribution is derived only from the frozen `RunStarted` plus
//! `ModelAttemptStarted`/`ModelTurnCommitted` ownership — never from the
//! current picker, live agent files, or provider configuration. The visible
//! assistant header projects the exact canonical `Agent • Model[variant]`.

mod reduce;
mod runtime;

pub(crate) use reduce::approval_state_from_record;
use reduce::*;

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
    /// AGENTS.md context loaded at a root run start, rendered inline when it
    /// differs from the previous run's (see `agent_md_previous_run`).
    AgentMd {
        id: u64,
        version: u64,
        seq: u64,
        entries: Vec<cookie_agent_protocol::AgentMdEntry>,
    },
    /// A skill body loaded into model context, rendered inline at its event.
    SkillLoaded {
        id: u64,
        version: u64,
        seq: u64,
        name: String,
        source_path: String,
        args: String,
        rendered_body: String,
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
            | Self::AgentMd { id, .. }
            | Self::SkillLoaded { id, .. }
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
            | Self::PluginMessage { version, .. }
            | Self::AgentMd { version, .. }
            | Self::SkillLoaded { version, .. } => *version,
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
    /// AGENTS.md entries loaded by the latest run (`None` when it loaded
    /// none) and by the run before it. A row appears only when a run's
    /// context differs from the previous run's, so turning AGENTS.md off and
    /// back on shows again even when the files are unchanged.
    pub(crate) agent_md_latest_run: Option<Vec<cookie_agent_protocol::AgentMdEntry>>,
    pub(crate) agent_md_previous_run: Option<Vec<cookie_agent_protocol::AgentMdEntry>>,
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

#[cfg(test)]
mod tests;
