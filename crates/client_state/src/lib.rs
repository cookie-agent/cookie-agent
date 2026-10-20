//! The client state every cookie agent client shares: disposable projections
//! reduced from the protocol's stored and transient events, independent of
//! how a client renders them. The TUI and the web client both keep a
//! [`StateStore`]; it builds for `wasm32-unknown-unknown` too.
//!
//! Assistant attribution is derived only from the frozen `RunStarted` plus
//! `ModelAttemptStarted`/`ModelTurnCommitted` ownership — never from the
//! current picker, live agent files, or provider configuration. The visible
//! assistant header projects the canonical `Agent • Model[variant]`, with the
//! model's display name (looked up at render time) ahead of its id.

pub mod markdown;
pub mod model_draft;
mod model_names;
pub mod ownership;
pub mod permissions;
mod reduce;
mod runtime;
pub mod session_actions;
pub mod subscriptions;
pub mod uploads;

pub use reduce::approval_state_from_record;
use reduce::*;

pub use model_names::{ModelDisplayNames, variant_label};
pub use runtime::{EMPTY_RUNTIME_GUIDANCE, RuntimePhase, RuntimeState};

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use web_time::Instant;

use cookie_agent_protocol::{
    AgentId, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation, ApprovalFinalOutcome,
    ApprovalId, ApprovalRecord, ApprovalRequest, ApprovalStatus, ApprovalTrigger,
    AssistantToolCallRef, AttemptId, EventPayload, EventSubscriptionMessage, GoalId,
    GoalReminderIdentity, GoalState, GoalStatus, ModelErrorSummary, OperationFingerprint,
    PersistedModelTurn, PreparedApprovalResource, PreparedCapabilityLifetime, ProducerDeliveryMode,
    ProducerIdempotencyKey, ProducerMessageId, ProducerOwner, ReplayDecision, ReplayDisposition,
    ResolvedModelRef, RunId, SafeCode, SessionId, SessionTitleChange, Sha256Digest, StoredEvent,
    StreamedOutputKind, ToolCallId, ToolTerminationOutcome, TransientEvent, Usage, VariantId,
};

use cookie_agent_protocol::ClientDelivery;

use crate::markdown::MarkdownDocument;

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

/// What is producing an in-progress compaction.
#[derive(Clone, Debug, PartialEq)]
pub enum CompactingSource {
    /// The compaction agent, with the summary streamed so far (live-only).
    Summary {
        invocation_id: cookie_agent_protocol::InternalAgentInvocationId,
        text: String,
    },
    /// A provider-native compaction call; `model` is its rendered model.
    Native { model: String },
}

/// A tool invocation displayed inside its owning assistant item. The compact
/// title uses only the persisted `ToolPresentation`; its argument blocks, or
/// the raw arguments when it has none, appear only in the expanded detail.
/// The persisted display argument is byte-capped but may still carry control
/// characters, so render sites flatten it.
#[derive(Clone, Debug)]
pub struct ToolCallState {
    pub id: ToolCallId,
    pub owner: AssistantToolCallRef,
    pub presentation: cookie_agent_protocol::ToolPresentation,
    /// Durable tool input from the owning committed turn, shown expanded
    /// when the presentation has no argument blocks.
    pub arguments: String,
    pub status: ToolStatus,
    /// The result's display once the call ended. While it runs, the call
    /// shows `live_display` instead.
    pub display: Vec<cookie_agent_protocol::Open<cookie_agent_protocol::DisplayBlock>>,
    /// Why a call that did not complete ended, shown above its display.
    pub failure: Option<String>,
    /// The running call's latest progress message.
    pub progress: Option<String>,
    /// The running call's live display, which its progress changes.
    pub live_display: cookie_agent_protocol::LiveDisplay,
    /// The complete content of cut display blocks, loaded on request.
    pub full: Vec<FullContent>,
    /// The call's tool supports the background, so a running call can be
    /// moved there.
    pub backgroundable: bool,
    /// The call moved to the background; its result arrives later.
    pub backgrounded: bool,
    /// Changes with every mutation of this call, unique across sessions and
    /// rebuilds (see `next_tool_revision`), so its rendered row can be
    /// cached by it.
    pub revision: u64,
}

/// The complete content behind a cut display block (its `full` artifact),
/// read with `artifact.read` when the user asks for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FullContent {
    /// The block's index in the call's display.
    pub block: usize,
    pub state: FullContentState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FullContentState {
    Loading,
    /// The first [`MAX_FULL_CONTENT_BYTES`] of the artifact as text, and
    /// whether the artifact is longer.
    Loaded {
        text: String,
        truncated: bool,
    },
    Failed(String),
}

/// The most of a cut block's complete content the TUI reads and keeps: one
/// `artifact.read` chunk.
pub const MAX_FULL_CONTENT_BYTES: u64 = cookie_agent_protocol::ARTIFACT_CHUNK_BYTES;

/// Applies one live progress update to a running call, as a
/// `tool_call_progress` transient does.
#[cfg(any(test, feature = "test-support"))]
pub fn reduce_live_progress_for_test(
    state: &mut SessionState,
    tool_call_id: ToolCallId,
    message: &str,
    delta: cookie_agent_protocol::DisplayDelta,
) {
    reduce_transient(
        state,
        jiff::Timestamp::now(),
        EventPayload::ToolCallProgress {
            tool_call_id,
            message: cookie_agent_protocol::SafeDisplayText::new(message).expect("message"),
            display: Some(delta.into()),
        },
    );
}

/// A fresh tool revision: process-wide, so a projection rebuilt from history
/// never reuses one a cached row was keyed by.
pub(crate) fn next_tool_revision() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl ToolCallState {
    /// A call that just started, with nothing shown yet.
    pub fn started(
        id: ToolCallId,
        owner: AssistantToolCallRef,
        presentation: cookie_agent_protocol::ToolPresentation,
        arguments: String,
    ) -> Self {
        Self {
            id,
            owner,
            presentation,
            arguments,
            status: ToolStatus::Running,
            display: Vec::new(),
            failure: None,
            progress: None,
            live_display: Default::default(),
            full: Vec::new(),
            backgroundable: false,
            backgrounded: false,
            revision: next_tool_revision(),
        }
    }

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

    /// The blocks the call shows now: the live display while it runs, then
    /// its result's.
    pub fn shown_display(
        &self,
    ) -> &[cookie_agent_protocol::Open<cookie_agent_protocol::DisplayBlock>] {
        if self.status == ToolStatus::Running {
            self.live_display.blocks()
        } else {
            &self.display
        }
    }

    /// The loaded complete content of display block `block`, if any.
    pub fn full_content(&self, block: usize) -> Option<&FullContentState> {
        self.full
            .iter()
            .find(|full| full.block == block)
            .map(|full| &full.state)
    }

    /// The failure and the shown display as text, for tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn shown_text(&self) -> String {
        let display = cookie_agent_protocol::display_text(self.shown_display());
        match &self.failure {
            Some(failure) if display.is_empty() => failure.clone(),
            Some(failure) => format!("{failure}\n{display}"),
            None => display,
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
    /// The tool call the approval gates.
    pub tool_call_id: ToolCallId,
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
    pub fn is_visible_user_escalation(&self) -> bool {
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
    /// The lowest level a client shows unless the user picks another: the
    /// TUI's `minimum_event_level` default, and the web client's `/events`.
    pub const DEFAULT_MINIMUM: Self = Self::Warning;

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
    /// The visible header `<agent-id> • <Model Name> • <variant>`. The display
    /// name is a render-time lookup; without a distinct name the header shows
    /// `<provider>/<model-id>` instead. Exact base behavior shows no variant.
    pub fn header(&self, names: &ModelDisplayNames) -> String {
        format!(
            "{} • {}",
            self.agent,
            names.selection_label(&self.resolved_model.selection)
        )
    }

    /// The variant retained in structured attribution, rendered as `base`
    /// when the frozen selection is exact base behavior.
    pub fn variant_label(&self) -> String {
        variant_label(&self.resolved_model.selection)
    }
}

/// A file part as one line: `📎 name · type · size`.
pub fn attachment_chip(artifact: &cookie_agent_protocol::ArtifactRef) -> String {
    format!(
        "📎 {} · {} · {}",
        artifact.display_name(),
        artifact.media_type,
        cookie_agent_protocol::human_bytes(artifact.byte_length)
    )
}

/// The text a person reads for `content`: text parts as they are and each
/// file part as its [`attachment_chip`], one part per line. A part of a kind
/// this build does not know reads as its tag.
pub fn user_content_text(content: &cookie_agent_protocol::UserContent) -> String {
    use cookie_agent_protocol::{Open, UserContentPart};
    if let [Open::Known(UserContentPart::Text { text })] = content.parts() {
        return text.clone();
    }
    content
        .parts()
        .iter()
        .map(|part| match part {
            Open::Known(UserContentPart::Text { text }) => text.clone(),
            Open::Known(UserContentPart::File { artifact }) => attachment_chip(artifact),
            Open::Unknown { tag, .. } => format!("[unsupported content: {tag}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What of a user's input goes back into the composer for editing: its text
/// parts, joined by newlines, and its file parts as attachments.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ComposerInput {
    pub text: String,
    pub files: Vec<cookie_agent_protocol::ArtifactRef>,
}

impl ComposerInput {
    /// Input of text alone.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            files: Vec::new(),
        }
    }

    /// The composer's share of `content`. A part of a kind this build does
    /// not know cannot be edited, so it is left out.
    pub fn of(content: &cookie_agent_protocol::UserContent) -> Self {
        Self {
            text: content.texts().collect::<Vec<_>>().join("\n"),
            files: content.files().cloned().collect(),
        }
    }

    /// The input as content to submit: its text, unless blank, then a file
    /// part for each attachment; `None` when there is neither.
    pub fn content(&self) -> Option<cookie_agent_protocol::UserContent> {
        use cookie_agent_protocol::{Open, UserContentPart};
        let text = (!self.text.trim().is_empty()).then(|| {
            Open::Known(UserContentPart::Text {
                text: self.text.clone(),
            })
        });
        let parts = text
            .into_iter()
            .chain(self.files.iter().map(|artifact| {
                Open::Known(UserContentPart::File {
                    artifact: artifact.clone(),
                })
            }))
            .collect();
        cookie_agent_protocol::UserContent::new(parts).ok()
    }
}

/// [`user_content_text`] as markdown: text parts are markdown already, and
/// a chip's characters are escaped so a filename renders as written.
fn user_content_markdown(content: &cookie_agent_protocol::UserContent) -> String {
    use cookie_agent_protocol::{Open, UserContentPart};
    if let [Open::Known(UserContentPart::Text { text })] = content.parts() {
        return text.clone();
    }
    let escape = |text: String| {
        text.chars()
            .fold(String::with_capacity(text.len()), |mut out, character| {
                if character.is_ascii_punctuation() {
                    out.push('\\');
                }
                out.push(character);
                out
            })
    };
    content
        .parts()
        .iter()
        .map(|part| match part {
            Open::Known(UserContentPart::Text { text }) => text.clone(),
            Open::Known(UserContentPart::File { artifact }) => escape(attachment_chip(artifact)),
            Open::Unknown { tag, .. } => escape(format!("[unsupported content: {tag}]")),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One steered message the engine admitted into its pending-input lane but
/// has not yet promoted to the model-facing log. Pure event reduction: the
/// lane is exactly what `UserInputAdmitted`/`UserInputSubmitted` and the recall
/// events describe, so replays rebuild it identically.
#[derive(Clone, Debug)]
pub struct PendingInput {
    pub text: String,
    /// What a recall or a void hands back to the composer.
    pub composer: ComposerInput,
    pub admission_seq: u64,
    /// Durable admission timestamp from the admitting event.
    pub admitted_at: jiff::Timestamp,
    /// The client that admitted it; absent on records without an origin.
    pub origin: Option<cookie_agent_protocol::EventOrigin>,
}

/// Unsent input a run end (or a failed request) left without a destination.
/// The UI restores it into the composer of the client that wrote it.
#[derive(Clone, Debug)]
pub struct VoidedInput {
    /// The seq of the event that admitted or queued it; restores keep
    /// submission order.
    pub seq: u64,
    pub origin: Option<cookie_agent_protocol::EventOrigin>,
    /// This client's own input handed back by a failed request: restored
    /// whatever the origin.
    pub parked: bool,
    pub input: ComposerInput,
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
        /// The submitted text, rendered as markdown with typed newlines kept.
        markdown: MarkdownDocument,
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
        /// Durable time each child entered the block, parallel to
        /// `children`: streamed parts at their first delta, committed turn
        /// content at its commit, started tools at their start. Cross-session
        /// rows (aggregated descendant warnings and errors) splice between
        /// children by these times, so a run block that spans many turns
        /// does not push them below content that happened later. A length
        /// mismatch (hand-built items) means unknown.
        child_times: Vec<jiff::Timestamp>,
    },
    /// A leveled diagnostic row (lifecycle notices, model warnings,
    /// failures). Never user/assistant/tool content; filtering these rows by
    /// level cannot hide conversation content or approvals.
    Event {
        id: u64,
        version: u64,
        level: EventLevel,
        text: String,
        /// How many times this exact warning or error was pushed in the
        /// session; repeats fold into one row instead of stacking (see
        /// `push_event`).
        repeat: u32,
    },
    /// A context compaction still in progress. The summarizer's terminal
    /// event or the native call's end removes the row, and a committed
    /// checkpoint adds its own.
    Compacting {
        id: u64,
        version: u64,
        /// The summarizer's start sequence, or the durable sequence a native
        /// call followed.
        seq: u64,
        source: CompactingSource,
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
        /// The producer's description, frozen at acceptance.
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
    /// A UI-only annotation rendered inline inside the assistant block
    /// (e.g. `model interrupted` when the run was cancelled mid-attempt).
    Notice {
        text: String,
    },
}

impl AssistantChild {
    pub fn id(&self) -> u64 {
        match self {
            Self::Text { id, .. } | Self::Thinking { id, .. } => *id,
            Self::Tool { .. }
            | Self::Attribution { .. }
            | Self::CommittedTool { .. }
            | Self::MediaFile { .. }
            | Self::Notice { .. } => 0,
        }
    }

    pub fn version(&self) -> u64 {
        match self {
            Self::Text { version, .. } | Self::Thinking { version, .. } => *version,
            Self::Tool { .. }
            | Self::Attribution { .. }
            | Self::CommittedTool { .. }
            | Self::MediaFile { .. }
            | Self::Notice { .. } => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssistantPartKind {
    Text,
    Thinking,
}

#[derive(Clone, Copy, Debug)]
pub struct OpenAssistantProjection {
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
            | Self::Compacting { id, .. }
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
            | Self::Compacting { version, .. }
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

    #[cfg(any(test, feature = "test-support"))]
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            id: 1,
            version: 0,
            markdown: MarkdownDocument::with_hard_breaks(text.into()).finished(),
            // Layout fixtures never target the row; any plausible physical
            // sequence (SessionCreated owns 1) keeps the field inhabited.
            seq: 2,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn internal(text: impl Into<String>) -> Self {
        Self::Event {
            id: 1,
            version: 0,
            level: EventLevel::Info,
            text: text.into(),
            repeat: 1,
        }
    }
}

/// One live streaming attempt, owning one assistant item.
#[derive(Clone, Debug)]
pub struct AttemptProjection {
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
pub struct RunAssistantProjection {
    pub run_id: RunId,
    pub(crate) item_id: u64,
    pub committed_prefix: usize,
    pub current_model: ResolvedModelRef,
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
pub struct AssistantTurnMetrics {
    pub timed_output_tokens: u64,
    pub generation: Duration,
    pub timed_turns: u32,
    pub context_tokens: Option<u64>,
    pub estimated_cost_pico_usd: Option<u64>,
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
pub struct PendingToolRow {
    /// Durable time of the tool's start event.
    started_at: jiff::Timestamp,
    turn_seq: u64,
    content_index: u32,
    call_id: ToolCallId,
}

#[derive(Clone, Debug)]
pub struct IndexedToolCall {
    arguments: String,
}

/// Projection-only identity composed exclusively from frozen, secret-safe
/// protocol values. History indices are deliberately excluded so one logical
/// compatibility transition warns once without altering durable evidence.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ReplayContextTransition {
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
pub struct ProducerMessageProjection {
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
pub struct ProducerClaimProjection {
    pub run_id: RunId,
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
    pub created_at: Option<jiff::Timestamp>,
    /// Latest user submission or delegate/steer tool start in this session.
    pub last_agent_activity: Option<jiff::Timestamp>,
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
    /// Tool names of the latest `ModelToolsPublished`: what the model was
    /// last offered.
    pub model_tools: Option<std::sync::Arc<[String]>>,
    pub model_selection: cookie_agent_protocol::SessionModelState,
    /// The session's runtime attachments, folded from its attach and detach
    /// events.
    pub runtime_attachments: cookie_agent_protocol::RuntimeAttachmentFold,
    pub goal: Option<GoalState>,
    pub transcript: Vec<TranscriptItem>,
    /// Durable insertion time per transcript item id. Cross-session rows such
    /// as aggregated descendant warnings merge into a viewed transcript by
    /// this time, so mid-conversation breaks render at their chronological
    /// position instead of the bottom.
    pub item_times: HashMap<u64, jiff::Timestamp>,
    /// The engine's pending-input lane for this session, reduced purely
    /// from admission/promotion/recall events: steered messages the model
    /// has not seen yet. Source of the queue strip between the conversation
    /// pane and the composer.
    pub pending_inputs: VecDeque<PendingInput>,
    /// Runs whose initial (non-lane) input has already been submitted.
    /// Later submissions for the same run are pending-lane promotions.
    pub initial_input_submitted: HashSet<RunId>,
    /// The files each submitted user message attached, by the message's
    /// `seq`, for clients that show them.
    pub user_files: HashMap<u64, Vec<cookie_agent_protocol::ArtifactRef>>,
    /// Pending inputs the engine voided at run end (no per-entry events),
    /// parked here until the UI restores their text into the composer —
    /// user text is never silently lost. Drained by the UI on sight.
    pub voided_inputs: Vec<VoidedInput>,
    /// Inputs queued behind the running run, from every client.
    pub queued_inputs: cookie_agent_protocol::QueuedInputs,
    pub next_transcript_id: u64,
    /// Ids handed to parts opened by live-only output, which has no durable
    /// sequence to derive one from. See `LIVE_PART_ID_BASE`.
    pub next_live_part_id: u64,
    pub open_assistant: Option<OpenAssistantProjection>,
    /// Elapsed thinking time per sealed thinking part, keyed by
    /// `(item_id, part_id)` and derived from durable event timestamps.
    pub thinking_durations: HashMap<(u64, u64), Duration>,
    /// When each unfinished attempt's streamed parts began, from its durable
    /// `ModelOutputStarted` marks. Its commit times its thinking from these,
    /// identically live and on replay, where the deltas themselves are gone.
    pub stream_marks: HashMap<AttemptId, Vec<(StreamedOutputKind, jiff::Timestamp)>>,
    /// Durable event timestamps by sequence, pruned at each committed
    /// turn's input boundary; backs replay-exact generation durations.
    pub event_timestamps: BTreeMap<u64, jiff::Timestamp>,
    /// Generation metrics per assistant item, accumulated from committed
    /// turns, for the subordinate footer row at the end of the block.
    pub assistant_metrics: HashMap<u64, AssistantTurnMetrics>,
    pub open_run_assistant: Option<RunAssistantProjection>,
    /// Assistant blocks whose run ended in `RunInterrupted`; their footer
    /// reads `… · interrupted`.
    pub interrupted_assistant_items: HashSet<u64>,
    pub attempts: HashMap<AttemptId, AttemptProjection>,
    /// The latest attempt until its first delta, commit, or abandonment.
    pub pending_attempt: Option<AttemptId>,
    pub tools: HashMap<ToolCallId, ToolCallState>,
    /// The session's background calls, as the daemon last reported them.
    pub background_calls: Vec<cookie_agent_protocol::BackgroundCall>,
    /// Buffered tool rows awaiting their committed placeholder, keyed by
    /// the owning turn's content index so starts/completions cannot reorder.
    pub pending_tool_rows: Vec<PendingToolRow>,
    /// Durable tool input indexed from committed turn content:
    /// (model_turn_seq, model_call_id) → arguments JSON, for expanded rows.
    pub turn_tool_index: HashMap<(u64, String), IndexedToolCall>,
    /// The assistant item owning each committed model-turn sequence.
    pub turn_items: HashMap<u64, u64>,
    /// User-visible replay compatibility transitions already projected for a
    /// run. Durable replay/reconnect and later tool-loop attempts may repeat
    /// request diagnostics without creating another logical transition.
    pub replay_context_warnings: HashSet<(RunId, Sha256Digest, ReplayContextTransition)>,
    /// Adapter warnings already surfaced for a run. After a model fallback the
    /// same reconstruction warnings regenerate on every committed turn; each
    /// distinct warning text is shown only once per run.
    pub model_turn_warnings: HashSet<(RunId, String)>,
    pub goal_revisions: HashMap<GoalId, u64>,
    pub producer_messages: HashMap<ProducerMessageId, ProducerMessageProjection>,
    pub producer_dedup: HashMap<(ProducerOwner, ProducerIdempotencyKey), ProducerMessageId>,
    pub producer_claims: HashMap<u64, ProducerClaimProjection>,
    pub terminal_runs: HashSet<RunId>,
    pub approvals: Vec<ApprovalState>,
    /// AGENTS.md entries loaded by the latest run and by the run before it,
    /// per attachment (events without one count as `local`). A run loads
    /// one event per coding attachment that returned a file. A row appears
    /// only when an attachment's context differs from its previous run's,
    /// so turning AGENTS.md off and back on shows again even when the files
    /// are unchanged.
    pub agent_md_latest_run: AgentMdByAttachment,
    pub agent_md_previous_run: AgentMdByAttachment,
}

/// AGENTS.md entries keyed by the attachment that loaded them.
pub(crate) type AgentMdByAttachment =
    std::collections::BTreeMap<String, Vec<cookie_agent_protocol::AgentMdEntry>>;

impl SessionState {
    /// Changes a call's view state outside any event, such as content loaded
    /// on request, and has its row drawn again. Returns whether the call is
    /// known.
    pub fn update_tool(
        &mut self,
        call_id: ToolCallId,
        update: impl FnOnce(&mut ToolCallState),
    ) -> bool {
        let Some(tool) = self.tools.get_mut(&call_id) else {
            return false;
        };
        update(tool);
        tool.revision = next_tool_revision();
        bump_tool_item(self, call_id);
        true
    }

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

    /// Whether a context compaction is still producing its summary.
    pub fn is_compacting(&self) -> bool {
        self.transcript
            .iter()
            .any(|item| matches!(item, TranscriptItem::Compacting { .. }))
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
    pub fn item_time(&self, item_id: u64) -> Option<jiff::Timestamp> {
        self.item_times.get(&item_id).copied()
    }

    /// Mark the open run block split-pending so the next new segment opens a
    /// fresh block below an interleaved event row. This holds while the run
    /// streams and between its turns alike: output that happens after the
    /// row must never render above it.
    pub(crate) fn mark_event_split_pending(&mut self) {
        if let Some(projection) = self.open_run_assistant.as_mut() {
            projection.split_pending = true;
        }
    }

    /// Split the run's open block at an attempt-boundary row (an abandoned
    /// attempt, a model fallback). Such a row lands after the attempt closed,
    /// so the streaming-only rule above never fires, yet the next attempt
    /// happened after it: its output must open a fresh block below the row
    /// rather than continue the block above.
    pub(crate) fn mark_attempt_boundary_split(&mut self, run_id: Option<RunId>) {
        if let Some(projection) = self
            .open_run_assistant
            .as_mut()
            .filter(|projection| Some(projection.run_id) == run_id)
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
            .rposition(|item| item.id() == item_id)?;
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

/// An event a newer daemon wrote that this build cannot read changes
/// nothing; the debug view shows a dim row naming its type.
fn push_unknown_event_row(state: &mut SessionState, event: &cookie_agent_protocol::UnknownEvent) {
    push_event(
        state,
        EventLevel::Debug,
        format!(
            "event `{}` (seq {}) is from a newer cookie and is not shown",
            event.tag, event.seq
        ),
        event.timestamp,
    );
}

/// All currently observed session projections.
#[derive(Clone, Debug, Default)]
pub struct StateStore {
    pub sessions: HashMap<SessionId, SessionState>,
    /// Each session's durable log, shared with replay scratch copies: a
    /// staged replay starts from the visible log without copying it.
    physical_events: HashMap<SessionId, Vec<Arc<StoredEvent>>>,
    quarantined_sessions: HashSet<SessionId>,
    replays: HashMap<SessionId, ReplayProgress>,
}

#[derive(Clone, Debug)]
struct ReplayProgress {
    generation: u64,
    final_seq: u64,
    scratch: SessionState,
    physical_events: Vec<Arc<StoredEvent>>,
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
    /// validated replacement replay ends.
    pub fn apply_delivery(&mut self, delivery: ClientDelivery) -> DeliveryOutcome {
        match delivery {
            ClientDelivery::Live {
                message,
                generation,
            } => match *message {
                EventSubscriptionMessage::Transient { event } => {
                    self.apply_transient_for_generation(
                        std::sync::Arc::unwrap_or_clone(event),
                        generation,
                    );
                    DeliveryOutcome::Applied
                }
                EventSubscriptionMessage::Event { event } => {
                    let session_id = event.session_id;
                    if self.apply_event_for_generation(
                        std::sync::Arc::unwrap_or_clone(event),
                        generation,
                    ) {
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
                EventSubscriptionMessage::Rewound {
                    session_id,
                    through_seq,
                } => {
                    self.rewind_session(session_id, through_seq);
                    DeliveryOutcome::Applied
                }
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
                let valid = self.replays.get_mut(&session_id).is_some_and(|replay| {
                    if replay.generation != generation || replay.final_seq != final_seq {
                        return false;
                    }
                    // A replay is durable history; live-only output has no
                    // place in it.
                    if event.seq <= replay.scratch.last_seq || event.payload.is_transient() {
                        return true;
                    }
                    replay.physical_events.push(Arc::new(event.clone()));
                    replay.scratch.last_seq = event.seq;
                    reduce_event_from(
                        &mut replay.scratch,
                        event.session_id,
                        event.run_id,
                        event.seq,
                        event.timestamp,
                        event.origin,
                        event.payload,
                    );
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
                        // A replay is durable history, not a live run
                        // termination, so it never restores unsent input.
                        replay.scratch.voided_inputs.clear();
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
                    _ => DeliveryOutcome::ReplayFailed { session_id },
                }
            }
            ClientDelivery::UnknownEvent {
                event,
                generation,
                replay_final_seq: Some(final_seq),
            } => {
                let session_id = event.session_id;
                let valid = self.replays.get_mut(&session_id).is_some_and(|replay| {
                    if replay.generation != generation || replay.final_seq != final_seq {
                        return false;
                    }
                    if event.seq > replay.scratch.last_seq {
                        replay.scratch.last_seq = event.seq;
                        push_unknown_event_row(&mut replay.scratch, &event);
                        replay.scratch.version = replay.scratch.version.wrapping_add(1);
                    }
                    true
                });
                if valid {
                    DeliveryOutcome::Applied
                } else {
                    self.abandon_replay(session_id);
                    DeliveryOutcome::ReplayFailed { session_id }
                }
            }
            ClientDelivery::UnknownEvent {
                event,
                generation,
                replay_final_seq: None,
            } => {
                if !self.quarantined_sessions.contains(&event.session_id) {
                    let state = self.sessions.entry(event.session_id).or_default();
                    if state.generation == generation && event.seq > state.last_seq {
                        state.last_seq = event.seq;
                        push_unknown_event_row(state, &event);
                        state.version = state.version.wrapping_add(1);
                    }
                }
                DeliveryOutcome::Applied
            }
            ClientDelivery::Diagnostic { message } => {
                let timestamp = jiff::Timestamp::now();
                for state in self.sessions.values_mut() {
                    push_event(state, EventLevel::Debug, message.clone(), timestamp);
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
            ClientDelivery::McpChanged(changed) => {
                for failure in &changed.failures {
                    self.push_failure_notice(&failure.session_ids, mcp_failure_notice(failure));
                }
                DeliveryOutcome::Applied
            }
            ClientDelivery::RuntimesChanged(changed) => {
                for failure in &changed.failures {
                    self.push_failure_notice(&failure.session_ids, runtime_failure_notice(failure));
                }
                DeliveryOutcome::Applied
            }
            ClientDelivery::BackgroundChanged(changed) => {
                self.sessions
                    .entry(changed.session_id)
                    .or_default()
                    .background_calls = changed.calls;
                DeliveryOutcome::Applied
            }
            ClientDelivery::RuntimeChanged(_)
            | ClientDelivery::SessionsChanged(_)
            | ClientDelivery::PluginsChanged(_)
            | ClientDelivery::MachinesChanged(_)
            | ClientDelivery::MemoryChanged(_) => DeliveryOutcome::Applied,
        }
    }

    /// An error row in `session_id`'s transcript for a daemon error the
    /// status line shows: its first line is the error, and the technical
    /// detail stays collapsed behind it until the row is expanded.
    pub fn push_error_details(&mut self, session_id: SessionId, error: &str, detail: &str) {
        if let Some(state) = self.sessions.get_mut(&session_id) {
            push_event(
                state,
                EventLevel::Error,
                format!("{error}\ndetails: {detail}"),
                jiff::Timestamp::now(),
            );
        }
    }

    /// An error row in each of `sessions` this client shows. A connection
    /// failure is a live daemon notification, not a durable event: wall
    /// clock orders it.
    fn push_failure_notice(&mut self, sessions: &[SessionId], text: String) {
        let timestamp = jiff::Timestamp::now();
        for session_id in sessions {
            if let Some(state) = self.sessions.get_mut(session_id) {
                push_event(state, EventLevel::Error, text.clone(), timestamp);
            }
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

    fn abandon_replay(&mut self, session_id: SessionId) {
        self.quarantined_sessions.insert(session_id);
        self.replays.remove(&session_id);
    }

    /// Drain a session's voided inputs for restoration into the composer of
    /// the client whose origin is `own`. Another client's inputs are dropped:
    /// that client restores them. Returns texts in submission order; empty
    /// when nothing is owed.
    pub fn take_voided_inputs(
        &mut self,
        session_id: SessionId,
        own: Option<&cookie_agent_protocol::EventOrigin>,
    ) -> Vec<ComposerInput> {
        let Some(state) = self.sessions.get_mut(&session_id) else {
            return Vec::new();
        };
        let mut voided = std::mem::take(&mut state.voided_inputs);
        voided.sort_by_key(|input| input.seq);
        voided
            .into_iter()
            .filter(|input| input.parked || input.origin.as_ref() == own)
            .map(|input| input.input)
            .collect()
    }

    /// Park this client's input as voided for a session (e.g. a recall
    /// resolved while a different session was being viewed); the UI
    /// restores it on sight.
    pub fn park_voided_input(&mut self, session_id: SessionId, input: ComposerInput) {
        if let Some(state) = self.sessions.get_mut(&session_id) {
            state.voided_inputs.push(VoidedInput {
                seq: u64::MAX,
                origin: None,
                parked: true,
                input,
            });
        }
    }

    /// Apply a persisted event. Replayed duplicates are ignored by sequence.
    pub fn apply_event(&mut self, event: StoredEvent) -> bool {
        self.apply_event_for_generation(event, 0)
    }

    pub fn apply_event_for_generation(&mut self, event: StoredEvent, generation: u64) -> bool {
        if self.quarantined_sessions.contains(&event.session_id) {
            return false;
        }
        if event.payload.is_transient() {
            // Live-only output is never stored; treat a record of it as
            // output following the durable tip this view has reached.
            let after_seq = self
                .sessions
                .get(&event.session_id)
                .map_or(0, |state| state.last_seq);
            self.apply_transient_for_generation(
                TransientEvent {
                    session_id: event.session_id,
                    run_id: event.run_id,
                    after_seq,
                    timestamp: event.timestamp,
                    payload: event.payload,
                },
                generation,
            );
            return true;
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
            .push(Arc::new(event.clone()));
        state.last_seq = event.seq;
        reduce_event_from(
            state,
            event.session_id,
            event.run_id,
            event.seq,
            event.timestamp,
            event.origin,
            event.payload,
        );
        state.version = state.version.wrapping_add(1);
        true
    }

    /// Apply live-only stream output. It never advances the cursor and never
    /// asks for a replay: output that does not directly follow the durable
    /// event this view last applied (stale, or ahead of a durable event not
    /// yet seen), or that arrives while a replay is staged, is dropped. The
    /// committed turn or tool result that follows supersedes it either way.
    /// Output for an attempt this view never saw start is ignored too, so a
    /// view attached mid-reply shows the reply from its first live delta on.
    pub fn apply_transient_for_generation(&mut self, event: TransientEvent, generation: u64) {
        if self.quarantined_sessions.contains(&event.session_id)
            || self.replays.contains_key(&event.session_id)
        {
            return;
        }
        let Some(state) = self.sessions.get_mut(&event.session_id) else {
            return;
        };
        if state.generation != generation || state.last_seq != event.after_seq {
            return;
        }
        if reduce_transient(state, event.timestamp, event.payload) {
            state.version = state.version.wrapping_add(1);
        }
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
                self.apply_event_for_generation(std::sync::Arc::unwrap_or_clone(event), generation)
                    .then_some(())
                    .map_or(Some(cursor), |_| None)
            }
            EventSubscriptionMessage::Transient { event } => {
                self.apply_transient_for_generation(
                    std::sync::Arc::unwrap_or_clone(event),
                    generation,
                );
                None
            }
            EventSubscriptionMessage::Gap {
                last_delivered_seq, ..
            } => Some(last_delivered_seq),
            EventSubscriptionMessage::Rewound {
                session_id,
                through_seq,
            } => {
                self.rewind_session(session_id, through_seq);
                None
            }
        }
    }

    /// A revert cut `session_id`'s log after `through_seq`. The projection
    /// drops what followed at once, so the appends that reuse those sequences
    /// apply in order; the client's full replay then confirms it. A session
    /// mid-replay is left to that replay.
    fn rewind_session(&mut self, session_id: SessionId, through_seq: u64) {
        if self.replays.contains_key(&session_id) {
            return;
        }
        let (Some(events), Some(state)) = (
            self.physical_events.get_mut(&session_id),
            self.sessions.get_mut(&session_id),
        ) else {
            return;
        };
        events.retain(|event| event.seq <= through_seq);
        let previous_version = state.version;
        *state = reduce_session_events(session_id, state.generation, events);
        state.version = previous_version.max(state.version).wrapping_add(1);
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

    /// Replace a session projection with one rebuilt from its durable
    /// history, which must start at the session's creation and advance
    /// strictly; gaps are allowed. An invalid history leaves the existing
    /// projection intact.
    pub fn rebuild_session(
        &mut self,
        session_id: SessionId,
        generation: u64,
        events: Vec<StoredEvent>,
    ) -> bool {
        let mut previous = 0;
        for event in &events {
            if event.session_id != session_id
                || event.seq <= previous
                || (previous == 0 && event.seq != 1)
            {
                return false;
            }
            previous = event.seq;
        }
        if self.quarantined_sessions.contains(&session_id) || self.replays.contains_key(&session_id)
        {
            return false;
        }
        let events = events.into_iter().map(Arc::new).collect::<Vec<_>>();
        let state = reduce_session_events(session_id, generation, &events);
        self.physical_events.insert(session_id, events);
        self.sessions.insert(session_id, state);
        true
    }
}

#[cfg(test)]
mod tests;

/// The error row for an MCP server that is unavailable to a session.
pub(crate) fn mcp_failure_notice(failure: &cookie_agent_protocol::McpServerFailure) -> String {
    match &failure.workspace {
        Some(workspace) => format!(
            "MCP server `{}` ({} runtime at {workspace}) is unavailable: {}",
            failure.server, failure.runtime_id, failure.reason
        ),
        None => format!(
            "MCP server `{}` ({} runtime) is unavailable: {}",
            failure.server, failure.runtime_id, failure.reason
        ),
    }
}

/// The error row for a runtime instance that is unavailable to a session.
pub(crate) fn runtime_failure_notice(failure: &cookie_agent_protocol::RuntimeFailure) -> String {
    match &failure.workspace {
        Some(workspace) => format!(
            "runtime `{}` at {workspace} is unavailable: {}",
            failure.runtime_id, failure.reason
        ),
        None => format!(
            "runtime `{}` is unavailable: {}",
            failure.runtime_id, failure.reason
        ),
    }
}
