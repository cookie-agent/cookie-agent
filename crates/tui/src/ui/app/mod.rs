//! Application state, event handling, and terminal loop.

mod agents;
mod approvals;
mod composer;
mod draw;
mod keys;
mod pickers;
mod providers;
mod refresh;
mod sessions;
#[cfg_attr(not(test), allow(unused_imports))]
pub(super) use approvals::approval_content;
use approvals::is_approval_scroll_key;
use keys::{edit_credential_input, is_newline_key, is_printable_key};
use pickers::{agent_picker_row, draft_title, model_picker_row};
pub(super) use sessions::status_change_from_event;
use sessions::{
    collect_known_statuses, collect_known_titles, collect_subtree_sessions,
    collect_tree_session_ids, find_node, find_node_mut, find_session, patch_tree_node_statuses,
    patch_tree_node_titles, title_change_from_event,
};

mod goal;

#[cfg(test)]
mod fallback_tests;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Write as _,
    io::{self, Write as _},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::{
    AgentDescriptor, AgentId, ApprovalListParams, ApprovalListResult, ApprovalRespondError,
    ApprovalRespondErrorCode, ApprovalRespondParams, ApprovalStatus, ApprovalUserDecision,
    AvailableModelDescriptor, ClientConnectId, ClientRequestId, ClientResponseId, ClientRunId,
    EventPayload, McpAuthBeginParams, McpAuthBeginResult, McpAuthCancelParams, McpServerAddParams,
    McpServerEditParams, McpServerInfo, McpServerNameParams, McpServerPersistParams,
    McpServerSetEnabledParams, McpServerState, ModelKey, ModelSelection, PermissionAction,
    PermissionEffect, PermissionMode, PermissionRuleSource, ProviderConnectParams,
    ProviderDescriptor, ProviderDisconnectParams, RunCancelParams, RunRecallSteerParams,
    RunSelection, RunStartParams, RunSteerParams, RunToolStdinParams,
    SESSION_TREE_USAGE_CORRUPT_DELEGATION_CODE, SafeDisplayText, SessionCompactParams,
    SessionCreateParams, SessionForkParams, SessionId, SessionListParams, SessionMeta,
    SessionPermissionClearParams, SessionPermissionGetParams, SessionPermissionGetResult,
    SessionPermissionSetParams, SessionResumeParams, SessionRevertParams,
    SessionSetPermissionModeParams, SessionStatus, SessionTitle, SessionTitleChange, SessionTree,
    SessionTreeParams, SessionTreeUsageResult, SessionUsageParams, SessionUsageResult, StoredEvent,
    VariantId,
};
use crossterm::{
    event::{
        EnableBracketedPaste, EnableMouseCapture, Event as CrosstermEvent, EventStream, KeyCode,
        KeyEvent, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListState, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    client::{Client, ClientDelivery, ClientError},
    config::TuiConfig,
    markdown::{Highlighter, SyntectHighlighter},
    state::{
        ApprovalState, DeliveryOutcome, EMPTY_RUNTIME_GUIDANCE, PendingInput, RuntimePhase,
        RuntimeState, StateStore, ToolStatus, TranscriptItem, approval_state_from_record,
    },
    theme::Theme,
};

use super::events::{RenderScheduler, TerminalRestore, install_terminal_panic_hook};
use super::input::{self, InputState};
use super::management::{
    McpAuthView, McpForm, McpFormFocus, McpPanel, PermissionForm, PermissionPanel, SkillPanel,
    UsagePanel, cycle_effect,
};
use super::pickers::{
    SearchPickerFocus, SearchPickerState, SessionSearchRow, agent_matches, cycle_selection,
    flatten_tree, model_matches, move_selection as move_picker_selection, provider_matches,
    session_search_rows, short_id,
};
use super::provider::{
    DURABLE_PROVIDER_COPY, ProviderAction, ProviderForm, ProviderFormFocus, ProviderOperation,
    ProviderRowState, action_name, row_label, row_state,
};
use super::slash::{
    COMMANDS, CommandSpec, SlashCommand, Submission, move_selection, parse_submission_with_skills,
};
use super::transcript::{
    BlockHit, BlockId, ConversationScroll, LayoutCache, ScrollbarGeometry, wrapped_line,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Modal {
    None,
    Sessions,
    Presets,
    Agents,
    Models,
    ConnectProviders,
    ConnectDetails,
    ConnectSetup,
    ConnectError,
    DisconnectConfirm,
    /// The copy/revert/fork menu for one clicked user message row.
    UserMessage,
    /// Confirm guard behind the menu's revert action.
    RevertConfirm,
    Mcp,
    Permissions,
    Skills,
    Usage,
    GoalDetail,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GoalBarAction {
    Details,
    Pause,
    Resume,
    Cancel,
}

pub(super) const SESSION_OWNED_BY_ANOTHER_PROCESS_CODE: i32 = -32022;

pub(super) fn session_owned_by_another_process(error: &ClientError) -> bool {
    matches!(error, ClientError::Rpc(error) if error.code == SESSION_OWNED_BY_ANOTHER_PROCESS_CODE)
}

/// Whether the daemon reported retryable store contention (`lock_contention`),
/// covering both the bounded-lock budget and a lost commit-time CAS.
pub(super) fn is_store_contention(error: &ClientError) -> bool {
    match error {
        ClientError::Rpc(rpc) => {
            rpc.data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(serde_json::Value::as_str)
                == Some("lock_contention")
        }
        _ => false,
    }
}

/// Provider connect/disconnect resubmit policy: the first contention-class
/// failure gets exactly one automatic resubmission of the identical request
/// (same client request id, so the daemon's replay path answers a commit whose
/// response was lost); a second consecutive contention surfaces to the user and
/// the pending payload stays held.
pub(super) fn retry_store_contention_once(attempt: usize, error: &ClientError) -> bool {
    attempt == 0 && is_store_contention(error)
}

/// Renders the spec's store-contention copy when the daemon reports
/// `lock_contention`; `store` is the human noun for the error site.
pub(super) fn store_contention_message(error: &ClientError, store: &str) -> String {
    if is_store_contention(error) {
        format!("Another cookie-agent process is writing the {store} — try again.")
    } else {
        error.to_string()
    }
}

#[derive(Clone, Copy)]
pub(super) enum PaletteEntry<'a> {
    Command(&'static CommandSpec),
    Skill(&'a cookie_agent_protocol::SkillDescriptor),
}

impl PaletteEntry<'_> {
    fn label(self) -> String {
        match self {
            Self::Command(spec) => format!("{} — {}", spec.usage, spec.description),
            Self::Skill(skill) => {
                let hint = skill
                    .argument_hint
                    .as_deref()
                    .map_or(String::new(), |hint| format!(" {hint}"));
                format!("/{}{} — {}", skill.name, hint, skill.description)
            }
        }
    }
}

/// Where copied text goes: the terminal's OSC 52 clipboard escape in
/// production, a shared capture buffer in tests.
#[derive(Default)]
pub(super) enum ClipboardSink {
    #[default]
    Osc52,
    #[cfg(test)]
    Capture(Arc<std::sync::Mutex<Vec<String>>>),
}

/// The user-message action the menu/confirm guard acts on. The message's
/// text rides along so copy and the revert's composer restoration never
/// re-derive it from a transcript that may have rebuilt underneath.
#[derive(Clone, Debug)]
pub(super) struct UserMenuState {
    pub(super) session_id: SessionId,
    /// Physical sequence of the message's `UserInputSubmitted` event.
    pub(super) seq: u64,
    pub(super) text: String,
}

/// Mouse text selection, stored in content coordinates so it survives
/// scrolling: the conversation leg addresses `(logical line, display
/// column)` inside the rendered lines, the composer leg addresses draft
/// buffer bytes. Extraction maps these back to real text — wrapped rows,
/// raw code without band chrome, user rows, draft text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TextSelection {
    Conversation {
        anchor: (usize, u16),
        head: (usize, u16),
    },
    Composer {
        anchor: usize,
        head: usize,
    },
}

impl TextSelection {
    /// Normalized endpoints with the start before the end in reading order.
    pub(super) fn ordered(&self) -> ((usize, u16), (usize, u16)) {
        match *self {
            Self::Conversation { anchor, head } => {
                if anchor <= head {
                    (anchor, head)
                } else {
                    (head, anchor)
                }
            }
            Self::Composer { .. } => unreachable!("composer endpoints are byte offsets"),
        }
    }

    /// Normalized composer byte range `(start, end)`.
    pub(super) fn byte_range(&self) -> (usize, usize) {
        match *self {
            Self::Composer { anchor, head } => (anchor.min(head), anchor.max(head)),
            Self::Conversation { .. } => {
                unreachable!("conversation endpoints are line/column pairs")
            }
        }
    }
}

/// A left-button press inside a selectable pane, held until motion decides
/// between a click (dispatched on release) and a selection drag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PendingPress {
    pub(super) column: u16,
    pub(super) row: u16,
    pub(super) target: PressTarget,
}

/// Which pane a pending press can start a selection in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PressTarget {
    Conversation,
    Composer,
}

/// Cell movement beyond this turns a pending press into a selection drag;
/// at or below it the release still counts as a plain click.
const DRAG_THRESHOLD_CELLS: u16 = 1;

#[derive(Clone, Copy, Debug)]
pub(super) struct InputHit {
    pub(super) rect: Rect,
    pub(super) text_rect: Rect,
    pub(super) scrollbar: Option<ScrollbarGeometry>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ProviderFieldHit {
    pub(super) rect: Rect,
    pub(super) text_rect: Rect,
    pub(super) focus: ProviderFormFocus,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TreeRowHit {
    pub(super) rect: Rect,
    pub(super) session_id: SessionId,
    pub(super) expand_rect: Option<Rect>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PickerRowHit {
    pub(super) rect: Rect,
    pub(super) index: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ApprovalHit {
    pub(super) rect: Rect,
    pub(super) decision: ApprovalUserDecision,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PaletteRowHit {
    pub(super) rect: Rect,
    pub(super) index: usize,
}

/// A clickable agent/model/variant segment inside the Message title.
#[derive(Clone, Copy, Debug)]
pub(super) struct TitleSegmentHit {
    pub(super) rect: Rect,
    pub(super) segment: TitleSegment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QueueEntryKind {
    User,
    Producer(cookie_agent_protocol::ProducerMessageId),
    Overflow,
}

pub(super) struct PendingQueueEntry {
    pub(super) kind: QueueEntryKind,
    pub(super) seq: u64,
    pub(super) accepted_at: jiff::Timestamp,
    pub(super) preview: String,
}

/// Only user rows recall the newest user input; producer rows are read-only.
#[derive(Clone, Copy, Debug)]
pub(super) struct QueueEntryHit {
    pub(super) rect: Rect,
    pub(super) index: usize,
    pub(super) kind: QueueEntryKind,
}

/// The visible rows of one past user message: a click opens the
/// copy/revert/fork menu targeting the message's physical event sequence.
#[derive(Clone, Copy, Debug)]
pub(super) struct UserMessageHit {
    pub(super) rect: Rect,
    pub(super) seq: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TitleSegment {
    Agent,
    Model,
    Variant,
}

/// A captured scrollbar thumb drag. The row offset where the press grabbed
/// the thumb is kept so dragging stays anchored even outside the track.
#[derive(Clone, Copy, Debug)]
pub(super) struct ScrollbarDrag {
    pub(super) grab_row: u16,
    pub(super) target: ScrollbarTarget,
}

/// Which pane's scrollbar a captured drag drives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScrollbarTarget {
    Conversation,
    Input,
}

/// The interactive element currently under the pointer. Hover is resolved
/// from the same per-frame hit map that click handling consults, in the same
/// priority order, and only ever changes styling — never selection state.
/// Only elements with a real click action are hover targets at all: passive
/// surfaces (the composer and scrollbar) stay quiet even
/// though clicks on them still work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HoverTarget {
    PaletteRow(usize),
    PickerRow(usize),
    ApprovalAction(ApprovalUserDecision),
    TitleSegment(TitleSegment),
    PermissionMode,
    SessionCost,
    EventLevelFilter,
    TreeRow(SessionId),
    QueueEntry(usize),
    ProviderField(ProviderFormFocus),
    ProviderSubmit,
    ProviderCancel,
    GoalAction(GoalBarAction),
    GoalClose,
    TranscriptBlock(BlockId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkingState {
    Working,
    Queued(usize),
}

/// Per-frame hit targets built from the same geometry and transcript layout
/// that were rendered. Mouse events consult this cached map for hit-testing
/// within a surface; overlay *ownership* is read from current state (modal,
/// palette, approval), so a panel that opened since the last frame still
/// claims its pointer events instead of leaking them to the content beneath.
#[derive(Default)]
pub(super) struct UiHitMap {
    pub(super) input: Option<InputHit>,
    pub(super) conversation: Option<Rect>,
    pub(super) scrollbar: Option<Rect>,
    pub(super) tree: Option<Rect>,
    pub(super) picker: Option<Rect>,
    pub(super) picker_input: Option<InputHit>,
    pub(super) palette: Option<Rect>,
    pub(super) blocks: Vec<BlockHit>,
    pub(super) tree_rows: Vec<TreeRowHit>,
    pub(super) picker_rows: Vec<PickerRowHit>,
    pub(super) palette_rows: Vec<PaletteRowHit>,
    pub(super) approval_actions: Vec<ApprovalHit>,
    pub(super) approval: Option<Rect>,
    pub(super) title_segments: Vec<TitleSegmentHit>,
    /// One clickable row per rendered queue-strip line (entries and the
    /// overflow fold alike): any click recalls the newest pending input.
    pub(super) queue_entries: Vec<QueueEntryHit>,
    /// Visible user-message rows that open the copy/revert/fork menu.
    pub(super) user_messages: Vec<UserMessageHit>,
    pub(super) permission_mode: Option<Rect>,
    pub(super) session_cost: Option<Rect>,
    pub(super) event_level_filter: Option<Rect>,
    pub(super) provider_fields: Vec<ProviderFieldHit>,
    pub(super) provider_submit: Option<Rect>,
    pub(super) provider_cancel: Option<Rect>,
    pub(super) goal_actions: Vec<(Rect, GoalBarAction)>,
    pub(super) goal_close: Option<Rect>,
}

impl UiHitMap {
    fn clear(&mut self) {
        self.input = None;
        self.conversation = None;
        self.scrollbar = None;
        self.tree = None;
        self.picker = None;
        self.picker_input = None;
        self.palette = None;
        self.blocks.clear();
        self.tree_rows.clear();
        self.picker_rows.clear();
        self.palette_rows.clear();
        self.approval_actions.clear();
        self.approval = None;
        self.title_segments.clear();
        self.queue_entries.clear();
        self.user_messages.clear();
        self.permission_mode = None;
        self.session_cost = None;
        self.event_level_filter = None;
        self.provider_fields.clear();
        self.provider_submit = None;
        self.provider_cancel = None;
        self.goal_actions.clear();
        self.goal_close = None;
    }
}

struct BottomBarRender {
    line: Line<'static>,
    mode_span: Option<usize>,
    cost_span: Option<usize>,
}

#[derive(Default)]
pub(super) struct SessionCostRefresh {
    debounce_generation: u64,
    request_id: u64,
    scheduled: bool,
    in_flight: bool,
    dirty: bool,
}

/// UI state separated from the client and durable protocol projection.
#[derive(Default)]
struct SessionSearchRowsCache {
    query: String,
    sessions_revision: u64,
    sessions_len: usize,
    local_day: Option<jiff::civil::Date>,
    rows: Vec<SessionSearchRow>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum AgentPanelMode {
    #[default]
    Auto,
    Shown,
    Hidden,
}

pub struct App {
    pub(super) client: Client,
    pub(super) deliveries: Option<tokio::sync::mpsc::UnboundedReceiver<ClientDelivery>>,
    pub(super) rpc_updates_tx: tokio::sync::mpsc::UnboundedSender<RpcUpdate>,
    pub(super) rpc_updates_rx: tokio::sync::mpsc::UnboundedReceiver<RpcUpdate>,
    pub(super) subscription_lanes:
        Arc<tokio::sync::Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>>,
    pub(super) stdin_lanes: Arc<
        tokio::sync::Mutex<HashMap<cookie_agent_protocol::ToolCallId, Arc<tokio::sync::Mutex<()>>>>,
    >,
    pub store: StateStore,
    pub(super) sessions: Vec<SessionMeta>,
    sessions_revision: u64,
    session_search_rows_cache: SessionSearchRowsCache,
    pub(super) runtime: RuntimeState,
    pub(super) agents: Vec<AgentDescriptor>,
    /// Client-local preset used only when creating a new root session.
    pub(super) selected_preset: Option<String>,
    /// Draft owned by the `/new` flow, independent of the viewed session draft.
    pub(super) new_session_draft: Option<RunSelection>,
    /// Revision of the current agent descriptor snapshot; refreshed
    /// coherently with the model revision.
    pub(super) agent_revision: Option<cookie_agent_protocol::AgentRevision>,
    pub(super) models: Vec<AvailableModelDescriptor>,
    /// Revision of the current model descriptor snapshot.
    pub(super) model_revision: Option<cookie_agent_protocol::ModelRevision>,
    pub(super) providers: Vec<ProviderDescriptor>,
    pub(super) skills: Vec<cookie_agent_protocol::SkillDescriptor>,
    #[cfg(test)]
    pub(super) skill_refresh_requests: Vec<SessionId>,
    pub(super) catalog_revision: Option<cookie_agent_protocol::CatalogRevision>,
    /// Client-local draft selection; never alters an active run.
    pub(super) draft: Option<RunSelection>,
    pub(super) draft_reset_fallback: bool,
    draft_generation: u64,
    pending_fallback_resets: HashMap<ClientRunId, PendingFallbackReset>,
    pub(super) connect_provider: Option<ProviderDescriptor>,
    pub(super) provider_form: Option<ProviderForm>,
    pub(super) provider_operations: HashMap<cookie_agent_protocol::ProviderId, ProviderOperation>,
    pub(super) connect_task: Option<tokio::task::JoinHandle<()>>,
    pub(super) tree: Option<SessionTree>,
    agent_panel_mode: AgentPanelMode,
    /// Session the conversation currently shows. Independent of the tree root.
    pub(super) selected: Option<SessionId>,
    /// Stable delegation-tree root; every tree refresh queries this session.
    pub(super) tree_root: Option<SessionId>,
    pub(super) selection_generation: u64,
    pub(super) tree_subscription_sessions: HashSet<SessionId>,
    pub(super) read_only_sessions: HashSet<SessionId>,
    pub(super) owned_sessions: HashSet<SessionId>,
    pub(super) ownership_classifications: HashMap<SessionId, u64>,
    pub(super) next_ownership_classification: u64,
    pub(super) pending_live_subscriptions: HashSet<SessionId>,
    pub(super) live_subscription_attempts: HashMap<SessionId, u64>,
    pub(super) next_live_subscription_attempt: u64,
    pub(super) replay_ended_for_live_subscription: HashSet<SessionId>,
    pub(super) tree_refresh_in_flight: Option<(u64, u64)>,
    pub(super) tree_refresh_pending: bool,
    pub(super) next_tree_refresh_id: u64,
    pub(super) tree_cursor: Option<SessionId>,
    pub(super) tree_offset: usize,
    pub(super) tree_viewport_height: usize,
    pub(super) collapsed_sessions: HashSet<SessionId>,
    pub(super) expanded_blocks: HashMap<SessionId, HashSet<BlockId>>,
    /// Runtime permission modes keyed by delegation-tree root.
    pub(super) permission_modes: HashMap<SessionId, PermissionMode>,
    permission_mode_generations: HashMap<SessionId, u64>,
    pub(super) mcp_panel: McpPanel,
    pub(super) permission_panel: PermissionPanel,
    pub(super) skill_panel: SkillPanel,
    pub(super) usage_panel: UsagePanel,
    pub(super) usage_load_generation: u64,
    pub(super) cost_refreshes: HashMap<SessionId, SessionCostRefresh>,
    pub(super) next_cost_refresh_request_id: u64,
    pub(super) conversation_scroll: ConversationScroll,
    pub(super) scrollbar_geometry: Option<ScrollbarGeometry>,
    pub(super) scrollbar_drag: Option<ScrollbarDrag>,
    pub(super) approval_scroll: u16,
    pub(super) approval_max_scroll: u16,
    pub(super) approval_scroll_request: Option<(cookie_agent_protocol::ApprovalId, u64)>,
    pub(super) pending_approval: Option<PendingApprovalSubmission>,
    pub(super) next_approval_request_id: u64,
    pub(super) approval_refresh_in_flight: Option<(SessionId, u64, u64)>,
    pub(super) next_approval_refresh_id: u64,
    pub(super) layout_cache: LayoutCache,
    pub(super) tui_config: TuiConfig,
    pub(super) theme: Theme,
    pub(super) highlighter: Box<dyn Highlighter>,
    pub(super) hit_map: UiHitMap,
    /// Interactive element under the pointer as of the last mouse move;
    /// resolved against the hit map at render time and purely visual.
    pub(super) hover: Option<HoverTarget>,
    /// Monotonic frame counter driving the streaming "thinking…" ellipsis;
    /// advanced by the frame tick only while animation is active.
    pub(super) animation_ticks: u64,
    pub(super) transient_notices: Vec<String>,
    pub(super) goal_notices: HashMap<SessionId, Vec<String>>,
    goal_detail: goal::GoalDetailState,
    pub(super) goal_focus: Option<GoalBarAction>,
    pub(super) picker_state: ListState,
    pub(super) session_search: SearchPickerState,
    pub(super) agent_search: SearchPickerState,
    pub(super) model_search: SearchPickerState,
    pub(super) provider_search: SearchPickerState,
    pub(super) palette_state: ListState,
    pub(super) palette_dismissed: bool,
    pub(super) last_escape: Option<Instant>,
    pub(super) input: InputState,
    pub(super) modal: Modal,
    pub(super) input_focused: bool,
    pub(super) stdin_target: Option<cookie_agent_protocol::ToolCallId>,
    pub(super) status: String,
    session_errors: SessionErrorSummary,
    pub(super) should_quit: bool,
    /// Active mouse text selection (conversation or composer); cleared by
    /// Esc, a plain click anywhere, or a copy.
    pub(super) selection: Option<TextSelection>,
    /// A left-button press awaiting the click-or-drag decision.
    pub(super) pending_press: Option<PendingPress>,
    /// State of the user-message copy/revert/fork menu and its confirm.
    pub(super) user_menu: Option<UserMenuState>,
    /// Clipboard destination for copy/cut (OSC 52 in production).
    pub(super) clipboard_sink: ClipboardSink,
    /// Latest authoritative title sequence per session: patches apply only a
    /// strictly newer sequence, so stale tree/list responses cannot
    /// overwrite a newer title event.
    pub(super) title_sequences: HashMap<SessionId, u64>,
}

/// Maximum queue entries shown by the strip between the conversation pane
/// and the composer; a "+N more" row folds the remainder into that budget.
const MAX_VISIBLE_QUEUE_ROWS: usize = 3;
const MAX_REPORTED_SESSION_ERROR_LINES: usize = 20;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SessionErrorSummary {
    error_count: usize,
    lines: VecDeque<String>,
}

impl SessionErrorSummary {
    fn record(&mut self, error: &str) {
        self.error_count = self.error_count.saturating_add(1);
        for line in error.lines().filter(|line| !line.trim().is_empty()) {
            if self.lines.len() == MAX_REPORTED_SESSION_ERROR_LINES {
                self.lines.pop_front();
            }
            self.lines.push_back(line.to_owned());
        }
    }

    fn format(&self) -> Option<String> {
        if self.error_count == 0 {
            return None;
        }
        let mut output = format!(
            "cookie-agent: session ended with {} error(s):",
            self.error_count
        );
        for line in &self.lines {
            let _ = write!(output, "\n  - {line}");
        }
        Some(output)
    }
}

/// The user-message action menu rows, in display/keyboard order:
/// copy, revert (confirm-guarded), fork.
const USER_MENU_ITEMS: &[(&str, &str)] = &[
    ("copy", "message text to the clipboard"),
    (
        "revert",
        "roll back to before this message; its text returns to the composer",
    ),
    ("fork", "branch a new session from this message"),
];

pub(super) enum RpcUpdate {
    Status(String),
    Notice(String),
    RunStartFinished {
        session_id: SessionId,
        client_run_id: ClientRunId,
        draft_generation: u64,
        reset_fallback: bool,
        input: String,
        result: Result<(), String>,
    },
    GoalFinished {
        session_id: SessionId,
        result: Box<Result<Option<cookie_agent_protocol::GoalState>, String>>,
    },
    SessionOwnershipClassified {
        session_id: SessionId,
        generation: u64,
        outcome: SessionOwnershipOutcome,
    },
    SessionLiveSubscriptionFinished {
        session_id: SessionId,
        live_attempt: Option<u64>,
        outcome: SessionLiveSubscriptionOutcome,
    },
    /// A steer RPC failed at the transport level (admission itself never
    /// rejects anymore): the submitted text is owed back to the composer.
    SteerFailed {
        session_id: SessionId,
        input: String,
        error: String,
    },
    /// A recall RPC returned the withdrawn pending input's text.
    SteerRecalled {
        session_id: SessionId,
        text: String,
    },
    /// A revert RPC committed; the `SessionReverted` event rebuilds the
    /// transcript, and the message text is owed back to the composer.
    Reverted {
        session_id: SessionId,
        text: String,
    },
    /// A fork RPC committed; the new session becomes the viewed one.
    Forked {
        forked: SessionId,
    },
    Tree {
        session_id: SessionId,
        generation: u64,
        request_id: u64,
        tree: Box<SessionTree>,
    },
    TreeFailed {
        session_id: SessionId,
        generation: u64,
        request_id: u64,
        error: String,
    },
    ProviderMutationFinished {
        outcome: ProviderMutationOutcome,
    },
    ApprovalResponse {
        request_id: u64,
        approval_id: cookie_agent_protocol::ApprovalId,
        result: Result<(), ApprovalSubmissionError>,
    },
    ApprovalList {
        root_session_id: SessionId,
        generation: u64,
        request_id: u64,
        result: Result<ApprovalListResult, String>,
    },
    PermissionModeMutationFinished {
        session_id: SessionId,
        generation: u64,
        result: Result<(), String>,
    },
    PermissionModeLoaded {
        session_id: SessionId,
        generation: u64,
        result: Result<Option<PermissionMode>, String>,
    },
    McpRefreshed {
        result: Result<cookie_agent_protocol::McpServerListResult, String>,
    },
    McpMutation {
        result: Box<Result<Option<McpServerInfo>, String>>,
    },
    McpAuthBegan {
        result: Result<McpAuthBeginResult, String>,
    },
    McpAuthCancelled {
        result: Result<String, String>,
    },
    PermissionsLoaded {
        session_id: SessionId,
        result: Result<SessionPermissionGetResult, String>,
    },
    SkillsLoaded {
        session_id: SessionId,
        result: Result<cookie_agent_protocol::SkillsListResult, String>,
    },
    UsageLoaded {
        generation: u64,
        session_id: Option<SessionId>,
        session: Result<Option<SessionUsageResult>, String>,
        tree: Result<Option<SessionTreeUsageResult>, ClientError>,
    },
    SessionCostLoaded {
        session_id: SessionId,
        request_id: u64,
        result: Result<SessionUsageResult, String>,
    },
    SessionCostDebounceElapsed {
        session_id: SessionId,
        generation: u64,
    },
}

struct PendingFallbackReset {
    session_id: SessionId,
    draft_generation: u64,
    rpc_admitted: bool,
    replay_generation: Option<u64>,
}

pub(super) enum SessionOwnershipOutcome {
    Owned(Box<SessionMeta>),
    Foreign,
    Failed(String),
}

pub(super) enum SessionLiveSubscriptionOutcome {
    Established,
    ReplayInProgress,
    Failed(String),
}

/// An approval response captured at click time and currently in flight.
/// The modal was dismissed optimistically; this marker blocks duplicate
/// actions until the RPC resolves.
#[derive(Clone, Debug)]
pub(super) struct PendingApprovalSubmission {
    pub(super) request_id: u64,
    pub(super) approval: ApprovalState,
    pub(super) decision: ApprovalUserDecision,
}

#[derive(Debug)]
pub(super) struct ApprovalSubmissionError {
    pub(super) message: String,
    pub(super) code: Option<ApprovalRespondErrorCode>,
}

impl ApprovalSubmissionError {
    fn from_client(error: ClientError) -> Self {
        let code = match &error {
            ClientError::Rpc(error) => error
                .data
                .clone()
                .and_then(|data| serde_json::from_value::<ApprovalRespondError>(data).ok())
                .map(|error| error.code),
            _ => None,
        };
        Self {
            message: error.to_string(),
            code,
        }
    }

    fn stale_projection(&self) -> bool {
        matches!(
            self.code,
            Some(
                ApprovalRespondErrorCode::ApprovalNotFound
                    | ApprovalRespondErrorCode::ApprovalNotPending
                    | ApprovalRespondErrorCode::ApprovalRevisionConflict
                    | ApprovalRespondErrorCode::OperationFingerprintMismatch
                    | ApprovalRespondErrorCode::OperationChanged
                    | ApprovalRespondErrorCode::IdempotencyConflict
            )
        )
    }
}

pub(super) enum ProviderMutationOutcome {
    Failed {
        provider_id: cookie_agent_protocol::ProviderId,
        action: ProviderAction,
        error: String,
    },
    Connected {
        provider_id: cookie_agent_protocol::ProviderId,
        baseline: Option<cookie_agent_protocol::RuntimeRevision>,
        runtime: Box<cookie_agent_protocol::RuntimeSnapshotV1>,
    },
    Disconnected {
        provider_id: cookie_agent_protocol::ProviderId,
        baseline: Option<cookie_agent_protocol::RuntimeRevision>,
        runtime: Box<cookie_agent_protocol::RuntimeSnapshotV1>,
    },
}

const MAX_TRANSIENT_NOTICES: usize = 4;
const TREE_REFRESH_TIMEOUT: Duration = Duration::from_secs(2);
const TREE_SUBSCRIPTION_TIMEOUT: Duration = Duration::from_secs(6);
const STDIN_RPC_TIMEOUT: Duration = Duration::from_secs(1);
const SESSION_COST_DEBOUNCE: Duration = Duration::from_millis(250);
impl App {
    #[cfg(test)]
    pub(crate) async fn wait_for_skill_refresh_for_test(&mut self) {
        while let Some(update) = self.rpc_updates_rx.recv().await {
            let skills = matches!(update, RpcUpdate::SkillsLoaded { .. });
            self.handle_rpc_update(update);
            if skills {
                break;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn skill_names_for_test(&self) -> Vec<&str> {
        self.skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn skill_visible_for_test(&self, name: &str) -> Option<bool> {
        self.skills
            .iter()
            .find(|skill| skill.name == name)
            .map(|skill| skill.visible)
    }

    #[cfg(test)]
    pub(crate) fn skill_palette_labels_for_test(&self) -> Vec<String> {
        self.palette_entries()
            .into_iter()
            .map(PaletteEntry::label)
            .collect()
    }

    #[cfg(test)]
    pub(crate) async fn submit_text_for_test(&mut self, text: &str) {
        self.input.set_buffer(text.to_owned());
        self.submit_input().await;
    }

    #[cfg(test)]
    pub(crate) fn refresh_skills_for_event_for_test(&mut self, event: &StoredEvent) {
        self.refresh_skills_for_event(event);
    }

    #[cfg(test)]
    pub(crate) fn skill_refresh_count_for_test(&self) -> usize {
        self.skill_refresh_requests.len()
    }

    pub async fn new(client: Client) -> Result<Self, crate::config::TuiConfigError> {
        Self::new_with_startup_mode(client, false).await
    }

    pub async fn new_with_new_session(
        client: Client,
    ) -> Result<Self, crate::config::TuiConfigError> {
        Self::new_with_startup_mode(client, true).await
    }

    async fn new_with_startup_mode(
        client: Client,
        create_new_session: bool,
    ) -> Result<Self, crate::config::TuiConfigError> {
        let tui_config = crate::config::load(None)?;
        let theme = crate::terminal_detect::theme_without_terminal_detection(tui_config.theme);
        Self::new_with_config(client, create_new_session, tui_config, theme).await
    }

    async fn new_with_config(
        client: Client,
        create_new_session: bool,
        tui_config: TuiConfig,
        theme: Theme,
    ) -> Result<Self, crate::config::TuiConfigError> {
        // Subscribe before issuing events.subscribe so its replay and a live
        // tail racing App construction share the same retained receiver.
        let deliveries = client
            .subscribe_deliveries()
            .expect("app delivery receiver already attached");
        let (rpc_updates_tx, rpc_updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = Self {
            client,
            deliveries: Some(deliveries),
            rpc_updates_tx,
            rpc_updates_rx,
            subscription_lanes: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            stdin_lanes: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            store: StateStore::default(),
            sessions: Vec::new(),
            sessions_revision: 0,
            session_search_rows_cache: SessionSearchRowsCache::default(),
            runtime: RuntimeState::default(),
            agents: Vec::new(),
            selected_preset: None,
            new_session_draft: None,
            agent_revision: None,
            models: Vec::new(),
            model_revision: None,
            providers: Vec::new(),
            skills: Vec::new(),
            #[cfg(test)]
            skill_refresh_requests: Vec::new(),
            catalog_revision: None,
            draft: None,
            draft_reset_fallback: false,
            draft_generation: 0,
            pending_fallback_resets: HashMap::new(),
            connect_provider: None,
            provider_form: None,
            provider_operations: HashMap::new(),
            connect_task: None,
            tree: None,
            agent_panel_mode: AgentPanelMode::Auto,
            selected: None,
            tree_root: None,
            selection_generation: 0,
            tree_subscription_sessions: HashSet::new(),
            read_only_sessions: HashSet::new(),
            owned_sessions: HashSet::new(),
            ownership_classifications: HashMap::new(),
            next_ownership_classification: 0,
            pending_live_subscriptions: HashSet::new(),
            live_subscription_attempts: HashMap::new(),
            next_live_subscription_attempt: 0,
            replay_ended_for_live_subscription: HashSet::new(),
            tree_refresh_in_flight: None,
            tree_refresh_pending: false,
            next_tree_refresh_id: 0,
            tree_cursor: None,
            tree_offset: 0,
            tree_viewport_height: 0,
            collapsed_sessions: HashSet::new(),
            expanded_blocks: HashMap::new(),
            permission_modes: HashMap::new(),
            permission_mode_generations: HashMap::new(),
            mcp_panel: McpPanel::default(),
            permission_panel: PermissionPanel::default(),
            skill_panel: SkillPanel::default(),
            usage_panel: UsagePanel::default(),
            usage_load_generation: 0,
            cost_refreshes: HashMap::new(),
            next_cost_refresh_request_id: 0,
            conversation_scroll: ConversationScroll::default(),
            scrollbar_geometry: None,
            scrollbar_drag: None,
            approval_scroll: 0,
            approval_max_scroll: 0,
            approval_scroll_request: None,
            pending_approval: None,
            next_approval_request_id: 0,
            approval_refresh_in_flight: None,
            next_approval_refresh_id: 0,
            layout_cache: LayoutCache::default(),
            tui_config,
            theme,
            highlighter: Box::<SyntectHighlighter>::default(),
            hit_map: UiHitMap::default(),
            hover: None,
            animation_ticks: 0,
            transient_notices: Vec::new(),
            goal_notices: HashMap::new(),
            goal_detail: goal::GoalDetailState::default(),
            goal_focus: None,
            picker_state: ListState::default().with_selected(Some(0)),
            session_search: SearchPickerState::default(),
            agent_search: SearchPickerState::default(),
            model_search: SearchPickerState::default(),
            provider_search: SearchPickerState::default(),
            palette_state: ListState::default().with_selected(Some(0)),
            palette_dismissed: false,
            last_escape: None,
            input: InputState::default(),
            modal: Modal::None,
            input_focused: true,
            stdin_target: None,
            status: "Connected. Type /help for commands.".into(),
            session_errors: SessionErrorSummary::default(),
            should_quit: false,
            selection: None,
            pending_press: None,
            user_menu: None,
            clipboard_sink: ClipboardSink::default(),
            title_sequences: HashMap::new(),
        };
        app.refresh_lists().await;
        if create_new_session {
            // Keep a fresh root entirely client-side until the first prompt.
            app.new_session_draft = app.default_draft_selection();
            app.selected = None;
        } else if let Some(session_id) = Self::preferred_startup_session(&app.sessions) {
            app.open_session(session_id).await;
        }
        if app.draft.is_none() {
            app.draft = app.default_draft_selection();
        }
        if app.selectable_agents().is_empty() {
            app.draft = None;
            app.status = app.setup_status();
        }
        Ok(app)
    }

    /// The session reopened on startup: the root session with the most recent
    /// activity. The listing is root-only, so the delegation tree — never a
    /// delegated child — is what the user resumes; the first entry and the
    /// create-new path remain as fallbacks.
    ///
    /// `session_id` breaks `last_activity` ties deterministically (the listing
    /// arrives in HashMap order), choosing the highest session ID among roots
    /// that share the newest activity timestamp.
    pub(super) fn preferred_startup_session(sessions: &[SessionMeta]) -> Option<SessionId> {
        sessions
            .iter()
            .filter(|session| matches!(session.origin, cookie_agent_protocol::SessionOrigin::Root))
            .max_by_key(|session| (session.last_activity, session.session_id))
            .or_else(|| sessions.first())
            .map(|session| session.session_id)
    }

    async fn open_session(&mut self, session_id: SessionId) {
        let generation = self.begin_ownership_classification(session_id);
        let outcome = match self
            .client
            .resume_session(SessionResumeParams { session_id })
            .await
        {
            Ok(result) => SessionOwnershipOutcome::Owned(Box::new(result.session)),
            Err(error) if session_owned_by_another_process(&error) => {
                SessionOwnershipOutcome::Foreign
            }
            Err(error) => SessionOwnershipOutcome::Failed(error.to_string()),
        };
        self.apply_ownership_classification(session_id, generation, outcome);
        self.select_session(session_id).await;
        if self.deliveries.is_some() {
            self.drain_replay(session_id).await;
        }
        self.refresh_tree().await;
    }

    fn begin_ownership_classification(&mut self, session_id: SessionId) -> u64 {
        self.next_ownership_classification = self.next_ownership_classification.wrapping_add(1);
        let generation = self.next_ownership_classification;
        self.ownership_classifications
            .insert(session_id, generation);
        if !self.owned_sessions.contains(&session_id) {
            self.read_only_sessions.insert(session_id);
            if self.selected == Some(session_id) {
                self.input_focused = false;
            }
        }
        generation
    }

    fn classify_session_background(&mut self, session_id: SessionId) {
        if self.owned_sessions.contains(&session_id) {
            self.start_pending_live_subscription(session_id);
            return;
        }
        if self.ownership_classifications.contains_key(&session_id) {
            return;
        }
        let generation = self.begin_ownership_classification(session_id);
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let outcome = match client
                .resume_session(SessionResumeParams { session_id })
                .await
            {
                Ok(result) => SessionOwnershipOutcome::Owned(Box::new(result.session)),
                Err(error) if session_owned_by_another_process(&error) => {
                    SessionOwnershipOutcome::Foreign
                }
                Err(error) => SessionOwnershipOutcome::Failed(error.to_string()),
            };
            let _ = updates.send(RpcUpdate::SessionOwnershipClassified {
                session_id,
                generation,
                outcome,
            });
        });
    }

    fn apply_ownership_classification(
        &mut self,
        session_id: SessionId,
        generation: u64,
        outcome: SessionOwnershipOutcome,
    ) {
        if self.ownership_classifications.get(&session_id) != Some(&generation) {
            return;
        }
        self.ownership_classifications.remove(&session_id);
        match outcome {
            SessionOwnershipOutcome::Owned(session) => {
                self.owned_sessions.insert(session_id);
                self.read_only_sessions.remove(&session_id);
                let session = self.merge_session_meta(*session);
                if let Some(existing) = self
                    .sessions
                    .iter_mut()
                    .find(|existing| existing.session_id == session_id)
                {
                    *existing = session;
                } else {
                    self.sessions.push(session);
                }
                self.note_sessions_changed();
                self.pending_live_subscriptions.insert(session_id);
                self.replay_ended_for_live_subscription.remove(&session_id);
                if self.selected == Some(session_id) {
                    self.input_focused = true;
                    self.status = "Session is writable.".into();
                }
                self.start_pending_live_subscription(session_id);
            }
            SessionOwnershipOutcome::Foreign => {
                self.owned_sessions.remove(&session_id);
                self.read_only_sessions.insert(session_id);
                self.pending_live_subscriptions.remove(&session_id);
                self.live_subscription_attempts.remove(&session_id);
                self.replay_ended_for_live_subscription.remove(&session_id);
                if self.selected == Some(session_id) {
                    self.input_focused = false;
                    self.status =
                        "Session is owned by another cookie process; read-only snapshot.".into();
                }
            }
            SessionOwnershipOutcome::Failed(error) => {
                self.owned_sessions.remove(&session_id);
                self.read_only_sessions.insert(session_id);
                self.pending_live_subscriptions.remove(&session_id);
                self.live_subscription_attempts.remove(&session_id);
                self.replay_ended_for_live_subscription.remove(&session_id);
                self.session_errors.record(&error);
                if self.selected == Some(session_id) {
                    self.input_focused = false;
                    self.status = error;
                }
            }
        }
    }

    fn start_pending_live_subscription(&mut self, session_id: SessionId) {
        if !self.pending_live_subscriptions.contains(&session_id)
            || self.live_subscription_attempts.contains_key(&session_id)
        {
            return;
        }
        self.replay_ended_for_live_subscription.remove(&session_id);
        self.next_live_subscription_attempt = self.next_live_subscription_attempt.wrapping_add(1);
        let live_attempt = self.next_live_subscription_attempt;
        self.live_subscription_attempts
            .insert(session_id, live_attempt);
        let cursor = self
            .store
            .sessions
            .get(&session_id)
            .map(|state| state.last_seq);
        self.subscribe_session_background(session_id, cursor, Some(live_attempt));
    }

    fn finish_live_subscription(
        &mut self,
        session_id: SessionId,
        live_attempt: Option<u64>,
        outcome: SessionLiveSubscriptionOutcome,
    ) {
        let Some(live_attempt) = live_attempt else {
            if let SessionLiveSubscriptionOutcome::Failed(error) = outcome {
                self.session_errors.record(&error);
                if self.selected == Some(session_id) {
                    self.status = error;
                }
            }
            return;
        };
        if self.live_subscription_attempts.get(&session_id) != Some(&live_attempt) {
            return;
        }
        self.live_subscription_attempts.remove(&session_id);
        if !self.pending_live_subscriptions.contains(&session_id) {
            return;
        }
        match outcome {
            SessionLiveSubscriptionOutcome::Established => {
                self.pending_live_subscriptions.remove(&session_id);
                self.replay_ended_for_live_subscription.remove(&session_id);
            }
            SessionLiveSubscriptionOutcome::ReplayInProgress => {
                if self.replay_ended_for_live_subscription.remove(&session_id) {
                    self.start_pending_live_subscription(session_id);
                }
            }
            SessionLiveSubscriptionOutcome::Failed(error) => {
                self.session_errors.record(&error);
                if self.selected == Some(session_id) {
                    self.status = error;
                }
            }
        }
    }

    async fn create_root_session(&mut self, selection: RunSelection) -> bool {
        let agent = selection.agent.clone();
        match self
            .client
            .create_session(SessionCreateParams { selection })
            .await
        {
            Ok(result) => {
                let session_id = result.session.session_id;
                self.note_title_sequence(&result.session);
                self.sessions.push(result.session);
                self.note_sessions_changed();
                self.open_session(session_id).await;
                self.new_session_draft = None;
                self.status =
                    format!("New root session opened with agent {agent}. Type /help for commands.");
                true
            }
            Err(error) => {
                // Preserve the draft so the user can retry after a transient
                // create failure.
                self.status = error.to_string();
                false
            }
        }
    }

    pub(super) fn take_deliveries(
        &mut self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<ClientDelivery> {
        self.deliveries
            .take()
            .expect("app delivery receiver already attached")
    }

    pub(super) fn spawn_rpc<F>(&self, task: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(task);
    }
}

fn latest_resolved_model_key(state: &crate::state::SessionState) -> Option<&ModelKey> {
    state.transcript.iter().rev().find_map(|item| {
        let TranscriptItem::Assistant {
            attribution,
            children,
            ..
        } = item
        else {
            return None;
        };
        children
            .iter()
            .rev()
            .find_map(|child| match child {
                crate::state::AssistantChild::Attribution { resolved_model } => {
                    Some(&resolved_model.selection.model)
                }
                _ => None,
            })
            .or(Some(&attribution.resolved_model.selection.model))
    })
}

fn shorten_home(cwd: &str) -> String {
    let Ok(home) = cookie_agent_protocol::paths::home_dir() else {
        return cwd.to_owned();
    };
    let home = home.to_string_lossy();
    if cwd == home {
        "~".into()
    } else if let Some(suffix) = cwd
        .strip_prefix(home.as_ref())
        .filter(|suffix| suffix.starts_with('/'))
    {
        format!("~{suffix}")
    } else {
        cwd.to_owned()
    }
}

pub(super) fn format_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    }
}

pub(super) fn format_cost_usd(cost: f64) -> String {
    if cost >= 0.01 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.4}")
    }
}

const fn permission_mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::AutoApprove => "auto-approve",
        PermissionMode::AutoApproveN => "auto-n",
        PermissionMode::AutoApproveY => "auto-y",
        PermissionMode::Ask => "ask",
        PermissionMode::Yolo => "yolo",
    }
}

pub(super) fn truncate_with_ellipsis(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let ellipsis = "…";
    let ellipsis_width = UnicodeWidthStr::width(ellipsis);
    if width <= ellipsis_width {
        return ellipsis.into();
    }
    let mut truncated = String::new();
    let content_width = width - ellipsis_width;
    for grapheme in value.graphemes(true) {
        if UnicodeWidthStr::width(truncated.as_str()) + UnicodeWidthStr::width(grapheme)
            > content_width
        {
            break;
        }
        truncated.push_str(grapheme);
    }
    truncated.push_str(ellipsis);
    truncated
}

impl Drop for App {
    fn drop(&mut self) {
        self.clear_connect_secrets();
        self.abort_connect_work();
    }
}

/// Run the terminal UI against a connected client.
pub async fn run_with_client(client: Client) -> anyhow::Result<()> {
    run_terminal(client, false).await
}

/// Run the terminal UI with a newly created root session.
pub async fn run_with_new_session(client: Client) -> anyhow::Result<()> {
    run_terminal(client, true).await
}

async fn run_terminal(client: Client, create_new_session: bool) -> anyhow::Result<()> {
    install_terminal_panic_hook();
    let tui_config = crate::config::load(None).context("load TUI configuration")?;
    let detection = crate::terminal_detect::detect_startup_theme(tui_config.theme);
    let theme = Theme::with_kind_from_env(detection.kind);
    tracing::info!(
        theme = ?theme.key().kind,
        color_level = ?theme.key().colors,
        detection_source = %detection.source,
        "TUI theme selected"
    );
    let mut app = App::new_with_config(client, create_new_session, tui_config, theme)
        .await
        .context("initialize TUI")?;
    let mut restore = TerminalRestore;
    enable_raw_mode().context("enable terminal raw mode")?;
    restore.raw_mode_enabled();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    restore.alternate_screen_entered();
    execute!(stdout, EnableMouseCapture).context("enable mouse capture")?;
    restore.mouse_capture_enabled();
    execute!(stdout, EnableBracketedPaste).context("enable bracketed paste")?;
    restore.bracketed_paste_enabled();
    execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .context("enable keyboard enhancement")?;
    restore.keyboard_enhancement_enabled();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    let deliveries = app.take_deliveries();
    let (result, session_errors) = event_loop(&mut terminal, app, deliveries).await;
    drop(terminal);
    drop(restore);
    for message in post_teardown_messages(&result, &session_errors) {
        eprintln!("{message}");
    }
    result
}

fn post_teardown_messages(
    result: &anyhow::Result<()>,
    session_errors: &SessionErrorSummary,
) -> Vec<String> {
    let mut messages = Vec::new();
    if let Err(error) = result {
        messages.push(format!("cookie-agent: {error:#}"));
    }
    if let Some(summary) = session_errors.format() {
        messages.push(summary);
    }
    messages
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: App,
    mut deliveries: tokio::sync::mpsc::UnboundedReceiver<ClientDelivery>,
) -> (anyhow::Result<()>, SessionErrorSummary) {
    let mut events = EventStream::new();
    let mut replay_watchdog = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut mcp_poll = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut frame_tick = tokio::time::interval(RenderScheduler::FRAME_INTERVAL);
    let mut render = RenderScheduler::default();
    loop {
        if render.should_draw(Instant::now()) {
            if let Err(error) = terminal
                .draw(|frame| app.draw(frame))
                .context("draw terminal")
            {
                return (Err(error), app.session_errors.clone());
            }
            render.drew(Instant::now());
        }
        if app.should_quit {
            return (Ok(()), app.session_errors.clone());
        }
        tokio::select! {
            Some(event) = events.next() => match event {
                Ok(CrosstermEvent::Key(key)) => {
                    app.handle_key(key).await;
                    render.mark_immediate();
                }
                Ok(CrosstermEvent::Mouse(mouse)) => {
                    if app.handle_mouse(mouse).await {
                        render.mark_immediate();
                    }
                }
                Ok(CrosstermEvent::Paste(text)) => {
                    let text = Zeroizing::new(text);
                    app.handle_paste(&text);
                    render.mark_immediate();
                }
                Ok(CrosstermEvent::Resize(_, _)) => {
                    if let Err(error) = handle_terminal_resize(terminal, &mut render)
                        .context("resize terminal")
                    {
                        return (Err(error), app.session_errors.clone());
                    }
                }
                Ok(_) => {},
                Err(error) => {
                    app.status = error.to_string();
                    render.mark_immediate();
                }
            },
            delivery = deliveries.recv() => match delivery {
                Some(delivery) => {
                    app.handle_delivery(delivery).await;
                    render.mark_stream();
                }
                None => {
                    for session_id in app.store.abandon_replays() {
                        app.client.recover_session(session_id, true);
                    }
                    app.clear_connect_secrets();
                    app.abort_connect_work();
                    app.status = "daemon disconnected".into();
                    app.session_errors.record(&app.status);
                    return (Ok(()), app.session_errors.clone());
                }
            },
            Some(update) = app.rpc_updates_rx.recv() => {
                app.handle_rpc_update(update);
                render.mark_immediate();
            },
            _ = replay_watchdog.tick() => {
                app.recover_timed_out_replays();
                render.mark_stream();
            },
            _ = mcp_poll.tick() => {
                app.poll_mcp();
            },
            _ = frame_tick.tick() => {
                // The frame cadence drives only the streaming "thinking…"
                // ellipsis; everything else redraws on events.
                if app.animation_active() {
                    app.animation_tick();
                    render.mark_stream();
                }
            },
        }
    }
}

pub(super) fn handle_terminal_resize<B: Backend>(
    terminal: &mut Terminal<B>,
    render: &mut RenderScheduler,
) -> Result<(), B::Error> {
    terminal.autoresize()?;
    render.mark_immediate();
    Ok(())
}

fn contains(rect: Rect, column: u16, row: u16) -> bool {
    rect.contains(Position::new(column, row))
}

fn agent_cycle_backward(key: KeyEvent) -> Option<bool> {
    match (key.code, key.modifiers) {
        (KeyCode::Tab, KeyModifiers::NONE) => Some(false),
        (KeyCode::Tab | KeyCode::BackTab, KeyModifiers::SHIFT) | (KeyCode::BackTab, _) => {
            Some(true)
        }
        _ => None,
    }
}

fn inner_rect(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

/// One compact connect-form action button: a border-colored frame sized to
/// its label so it reads as a button, not a panel-wide strip. The label sits
/// on the middle row (or the only row when the panel is vertically cramped).
fn render_connect_button(frame: &mut ratatui::Frame, area: Rect, label: &str, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if area.height > 1 {
        frame.render_widget(crate::ui::panel_block().border_style(style), area);
        let label_area = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            1,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(label.to_owned(), style)),
            label_area,
        );
    } else {
        frame.render_widget(Paragraph::new(Span::styled(label.to_owned(), style)), area);
    }
}

/// Paint an overlay panel: reset every cell like `Clear` (a styled `Block`
/// only re-styles cells, so underlying glyphs would ghost through), then
/// fill with the theme surface instead of punching a terminal-default hole
/// in the light theme.
pub(super) fn paint_panel(frame: &mut ratatui::Frame, area: Rect, theme: &Theme) {
    let clip = area.intersection(frame.area());
    let buffer = frame.buffer_mut();
    for y in clip.top()..clip.bottom() {
        for x in clip.left()..clip.right() {
            let cell = &mut buffer[(x, y)];
            cell.reset();
            cell.set_style(theme.panel());
        }
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height) / 2),
            Constraint::Percentage(height),
            Constraint::Percentage((100 - height) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width) / 2),
            Constraint::Percentage(width),
            Constraint::Percentage((100 - width) / 2),
        ])
        .split(vertical[1])[1]
}

fn client_run_id() -> ClientRunId {
    let ticks = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    ClientRunId::new(format!("tui-{ticks}")).expect("bounded client run id")
}

/// The OSC 52 clipboard escape for `text`: `ESC ] 52 ; c ; <base64> BEL`.
/// The `c` target is the system clipboard selection in every terminal that
/// implements the sequence.
pub(super) fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text.as_bytes()))
}

/// Coarse age label for the strip title: precise enough to show a stuck
/// queue, coarse enough to avoid false precision (and flicker).
pub(super) fn queue_age_label(age_secs: i64) -> String {
    if age_secs < 60 {
        "<1m".to_owned()
    } else if age_secs < 3600 {
        format!("{}m", age_secs / 60)
    } else {
        format!("{}h", age_secs / 3600)
    }
}

/// Collapse a queued message to one display line of at most `width` cells:
/// newlines flatten to spaces and overlong text ends in an ellipsis.
pub(super) fn ellipsize_single_line(text: &str, width: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if UnicodeWidthStr::width(flat.as_str()) <= width {
        return flat;
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in flat.graphemes(true) {
        let cell = UnicodeWidthStr::width(grapheme);
        // The ellipsis itself needs one cell.
        if used + cell + 1 > width {
            break;
        }
        used += cell;
        out.push_str(grapheme);
    }
    out.push('…');
    out
}

fn client_response_id() -> ClientResponseId {
    ClientResponseId::new(Uuid::now_v7().to_string()).expect("uuid-derived client response id")
}

#[cfg(test)]
mod post_teardown_tests {
    use super::*;

    #[test]
    fn clean_exit_without_session_errors_prints_nothing() {
        let messages = post_teardown_messages(&Ok(()), &SessionErrorSummary::default());
        assert!(messages.is_empty());
    }

    #[test]
    fn session_error_summary_keeps_only_the_last_twenty_lines() {
        let mut summary = SessionErrorSummary::default();
        for index in 0..25 {
            summary.record(&format!("error {index}"));
        }

        let output = summary.format().expect("summary");
        assert!(output.starts_with("cookie-agent: session ended with 25 error(s):"));
        assert!(!output.contains("error 4\n"));
        assert!(output.contains("error 5\n"));
        assert!(output.ends_with("error 24"));
    }

    #[test]
    fn terminal_error_and_session_summary_are_both_reported() {
        let mut summary = SessionErrorSummary::default();
        summary.record("daemon disconnected");
        let result = Err(anyhow::anyhow!("server task failed").context("event loop failed"));

        let messages = post_teardown_messages(&result, &summary);

        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0],
            "cookie-agent: event loop failed: server task failed"
        );
        assert!(messages[1].contains("daemon disconnected"));
    }
}
