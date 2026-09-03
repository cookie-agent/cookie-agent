//! Application state, event handling, and terminal loop.

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
    widgets::{Block, Borders, List, ListState, Paragraph, Wrap},
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
    CommandSpec, SlashCommand, Submission, command_help_lines, move_selection,
    parse_submission_with_skills,
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
}

pub(super) const SESSION_OWNED_BY_ANOTHER_PROCESS_CODE: i32 = -32022;

pub(super) fn session_owned_by_another_process(error: &ClientError) -> bool {
    matches!(error, ClientError::Rpc(error) if error.code == SESSION_OWNED_BY_ANOTHER_PROCESS_CODE)
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

/// One clickable queue-strip row. The index identifies the row for hover;
/// clicks recall the newest pending input regardless of the row hit.
#[derive(Clone, Copy, Debug)]
pub(super) struct QueueEntryHit {
    pub(super) rect: Rect,
    pub(super) index: usize,
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
/// surfaces (the composer, the scrollbar, transcript blocks) stay quiet even
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

fn is_printable_key(key: KeyEvent) -> bool {
    key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT
}

fn edit_credential_input(input: &mut input::CredentialInput, key: KeyEvent) {
    match key.code {
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.move_left(),
        KeyCode::Right => input.move_right(),
        KeyCode::Home => input.move_buffer_home(),
        KeyCode::End => input.move_buffer_end(),
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => input.wipe(),
        KeyCode::Char(character) if is_printable_key(key) => input.insert(character),
        _ => {}
    }
}

fn edit_plain_input(input: &mut InputState, key: KeyEvent) {
    match key.code {
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.move_left(),
        KeyCode::Right => input.move_right(),
        KeyCode::Home => input.move_buffer_home(),
        KeyCode::End => input.move_buffer_end(),
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            input.set_buffer(String::new())
        }
        KeyCode::Char(character) if is_printable_key(key) => input.insert(character),
        _ => {}
    }
}

fn is_newline_key(key: KeyEvent) -> bool {
    matches!(
        (key.code, key.modifiers),
        (
            KeyCode::Enter,
            KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT
        ) | (KeyCode::Char('j'), KeyModifiers::CONTROL)
    )
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
    pub(super) connect_provider: Option<ProviderDescriptor>,
    pub(super) provider_form: Option<ProviderForm>,
    pub(super) provider_operations: HashMap<cookie_agent_protocol::ProviderId, ProviderOperation>,
    pub(super) connect_task: Option<tokio::task::JoinHandle<()>>,
    pub(super) tree: Option<SessionTree>,
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
            connect_provider: None,
            provider_form: None,
            provider_operations: HashMap::new(),
            connect_task: None,
            tree: None,
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
            app.create_startup_session().await;
        } else if let Some(session_id) = app.sessions.first().map(|session| session.session_id) {
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

    async fn create_startup_session(&mut self) {
        if self.runtime.is_empty() {
            self.status = EMPTY_RUNTIME_GUIDANCE.into();
            return;
        }
        let Some(selection) = self.default_draft_selection() else {
            self.status = self.setup_status();
            return;
        };
        self.create_root_session(selection).await;
    }

    async fn create_root_session(&mut self, selection: RunSelection) {
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
            }
            Err(error) => {
                self.new_session_draft = None;
                self.status = error.to_string();
            }
        }
    }

    /// Record the authoritative title sequence from a session meta patch.
    pub(super) fn note_title_sequence(&mut self, session: &SessionMeta) {
        let known = self.title_sequences.entry(session.session_id).or_insert(0);
        *known = (*known).max(session.title_updated_seq);
    }

    /// Merge one session meta patch: strictly newer title sequences win; a
    /// stale patch retains the newer known title and never overwrites it.
    pub(super) fn merge_session_meta(&mut self, session: SessionMeta) -> SessionMeta {
        let known = self
            .title_sequences
            .get(&session.session_id)
            .copied()
            .unwrap_or(0);
        let mut session = session;
        let known_status = self
            .sessions
            .iter()
            .filter(|existing| existing.session_id == session.session_id)
            .map(|existing| (existing.last_event_seq, existing.status))
            .chain(
                self.tree
                    .as_ref()
                    .and_then(|tree| find_session(tree, session.session_id))
                    .map(|existing| (existing.last_event_seq, existing.status)),
            )
            .max_by_key(|(seq, _)| *seq);
        if session.title_updated_seq < known
            && let Some(current) = self
                .sessions
                .iter()
                .find(|existing| existing.session_id == session.session_id)
                .or_else(|| {
                    self.tree
                        .as_ref()
                        .and_then(|tree| find_session(tree, session.session_id))
                })
                .cloned()
        {
            session.title = current.title;
            session.title_updated_seq = known;
        }
        if let Some((seq, status)) = known_status
            && session.last_event_seq < seq
        {
            session.last_event_seq = seq;
            session.status = status;
        }
        self.note_title_sequence(&session);
        session
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

    pub(super) async fn drain_replay(&mut self, session_id: SessionId) {
        loop {
            let delivery = match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.deliveries
                    .as_mut()
                    .expect("app delivery receiver attached")
                    .recv(),
            )
            .await
            {
                Ok(Some(delivery)) => delivery,
                Ok(None) => return,
                Err(_) => {
                    for replay_session in self.store.abandon_replays() {
                        self.client.recover_session(replay_session, true);
                    }
                    self.status = "replay timed out; retrying recovery".into();
                    return;
                }
            };
            let finished = matches!(
                &delivery,
                ClientDelivery::ReplayEnd { session_id: replay_session, .. } if *replay_session == session_id
            );
            self.handle_delivery(delivery).await;
            if finished {
                return;
            }
        }
    }

    pub(super) async fn refresh_lists(&mut self) {
        match self
            .client
            .list_sessions(SessionListParams::default())
            .await
        {
            Ok(result) => {
                self.sessions = result
                    .sessions
                    .into_iter()
                    .map(|session| self.merge_session_meta(session))
                    .collect();
                self.note_sessions_changed();
            }
            Err(error) => self.status = error.to_string(),
        }
        self.refresh_coherent_lists().await;
    }

    /// Fetch and install the sole protocol-10 discovery object.
    pub(super) async fn refresh_coherent_lists(&mut self) {
        match self.client.runtime_snapshot().await {
            Ok(result) => self.install_initial_runtime(result.snapshot),
            Err(error) => {
                self.runtime.set_error(error.to_string());
                self.status =
                    format!("Runtime snapshot unavailable: {error}. Press Enter to retry.");
            }
        }
    }

    pub(super) fn install_initial_runtime(
        &mut self,
        snapshot: cookie_agent_protocol::RuntimeSnapshotV1,
    ) {
        if self.runtime.snapshot().is_some() {
            return;
        }
        self.runtime.install_initial(snapshot.clone());
        self.install_runtime_projection(snapshot);
    }

    pub(super) fn install_runtime_response(
        &mut self,
        baseline: Option<&cookie_agent_protocol::RuntimeRevision>,
        snapshot: cookie_agent_protocol::RuntimeSnapshotV1,
    ) -> bool {
        if !self.runtime.install_response(baseline, snapshot.clone()) {
            return false;
        }
        self.install_runtime_projection(snapshot);
        true
    }

    pub(super) fn install_runtime_notification(
        &mut self,
        changed: cookie_agent_protocol::RuntimeChangedNotification,
    ) -> bool {
        let snapshot = changed.snapshot.clone();
        if !self.runtime.apply_notification(changed) {
            return false;
        }
        self.install_runtime_projection(snapshot);
        true
    }

    fn install_runtime_projection(&mut self, snapshot: cookie_agent_protocol::RuntimeSnapshotV1) {
        self.catalog_revision = Some(snapshot.catalog_revision);
        self.model_revision = Some(snapshot.model_revision);
        self.agent_revision = Some(snapshot.agent_revision);
        self.providers = snapshot.providers;
        self.models = snapshot.models;
        self.agents = snapshot.agents;
        if self
            .selected_preset
            .as_ref()
            .is_some_and(|selected| !self.preset_names().contains(selected))
        {
            self.selected_preset = None;
        }
        self.revalidate_draft();
        if self.runtime.is_empty() && self.watching_root_session() {
            self.draft = None;
            self.status = EMPTY_RUNTIME_GUIDANCE.into();
        } else if self.draft.is_none() {
            self.draft = self.default_draft_selection();
        }
    }

    /// Root-selectable agents: exactly the descriptors with
    /// `runnable_as_root = true`.
    pub(super) fn selectable_agents(&self) -> Vec<&AgentDescriptor> {
        let preset = self
            .draft
            .as_ref()
            .and_then(|draft| draft.preset.as_deref())
            .or_else(|| {
                self.selected_session_meta()
                    .and_then(|session| session.creation_selection.preset.as_deref())
            });
        self.root_agents_for_preset(preset)
    }

    fn new_session_selectable_agents(&self) -> Vec<&AgentDescriptor> {
        self.root_agents_for_preset(self.selected_preset.as_deref())
    }

    fn agent_picker_candidates(&self) -> Vec<&AgentDescriptor> {
        if self.new_session_draft.is_some() {
            self.new_session_selectable_agents()
        } else {
            self.selectable_agents()
        }
    }

    pub(super) fn filtered_agent_picker_candidates(&self) -> Vec<&AgentDescriptor> {
        self.agent_picker_candidates()
            .into_iter()
            .filter(|agent| agent_matches(agent, self.agent_search.query()))
            .collect()
    }

    fn root_agents_for_preset(&self, preset: Option<&str>) -> Vec<&AgentDescriptor> {
        self.agents
            .iter()
            .filter(|agent| {
                agent.runnable_as_root
                    && agent.mode != cookie_agent_protocol::AgentMode::Internal
                    && agent.preset.as_deref() == preset
            })
            .collect()
    }

    pub(super) fn preset_names(&self) -> Vec<String> {
        let mut names = self
            .agents
            .iter()
            .filter_map(|agent| agent.preset.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn selected_preset_label(&self) -> &str {
        self.selected_preset.as_deref().unwrap_or("shared")
    }

    fn draft_selection_for_preset(
        &self,
        preset: Option<&str>,
        preferred_agent: Option<&AgentId>,
        preferred_model: Option<&ModelSelection>,
    ) -> Option<RunSelection> {
        let candidates = self
            .agents
            .iter()
            .filter(|agent| {
                agent.runnable_as_root
                    && agent.mode != cookie_agent_protocol::AgentMode::Internal
                    && agent.preset.as_deref() == preset
            })
            .collect::<Vec<_>>();
        let agent = preferred_agent
            .and_then(|id| candidates.iter().find(|agent| agent.id == *id).copied())
            .or_else(|| {
                candidates
                    .iter()
                    .find(|agent| agent.id.as_str() == "primary")
                    .copied()
            })
            .or_else(|| candidates.first().copied())?;
        let model = preferred_model
            .filter(|selection| self.selection_is_live(selection))
            .cloned()
            .or_else(|| {
                agent
                    .resolved_fallback
                    .iter()
                    .find(|selection| self.selection_is_live(selection))
                    .cloned()
            })
            .or_else(|| self.models.first().map(Self::default_model_selection))?;
        Some(RunSelection {
            agent: agent.id.clone(),
            model,
            preset: preset.map(str::to_owned),
        })
    }

    pub(super) fn default_draft_selection(&self) -> Option<RunSelection> {
        let agents = self.selectable_agents();
        let agent = agents
            .iter()
            .find(|agent| agent.id.as_str() == "primary")
            .or_else(|| agents.first())?;
        let model = agent
            .resolved_fallback
            .iter()
            .find(|selection| self.selection_is_live(selection))
            .cloned()
            .or_else(|| self.models.first().map(Self::default_model_selection))?;
        Some(RunSelection {
            agent: agent.id.clone(),
            model,
            preset: agent.preset.clone(),
        })
    }

    /// The metadata of the currently watched session, from the session list
    /// or the delegation tree.
    pub(super) fn selected_session_meta(&self) -> Option<&SessionMeta> {
        let session_id = self.selected?;
        self.sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .or_else(|| {
                self.tree
                    .as_ref()
                    .and_then(|tree| find_session(tree, session_id))
            })
    }

    /// True while the watched session is a delegation root. Root sessions may
    /// draft/select any currently root-runnable primary/all agent between
    /// runs; delegated sessions are pinned to their frozen child agent.
    pub(super) fn watching_root_session(&self) -> bool {
        self.selected_session_meta()
            .is_none_or(|meta| matches!(meta.origin, cookie_agent_protocol::SessionOrigin::Root))
    }

    /// Agent switching is allowed only for root sessions; delegated
    /// sessions are pinned to their frozen child agent. This gate is
    /// independent of the active run: draft changes affect the next run
    /// only, and active-run attribution stays frozen.
    pub(super) fn agent_switching_allowed(&self) -> bool {
        self.watching_root_session()
    }

    /// Model draft changes are allowed whenever a draft exists — for
    /// delegated sessions within their frozen agent's persisted suffix.
    /// This gate is independent of the active run.
    pub(super) fn model_selection_allowed(&self) -> bool {
        self.draft.is_some()
    }

    /// The frozen child agent a delegated session is pinned to, and the
    /// non-color reason the selector stays disabled.
    pub(super) fn delegated_pin_reason(&self) -> Option<String> {
        let meta = self.selected_session_meta()?;
        if matches!(meta.origin, cookie_agent_protocol::SessionOrigin::Root) {
            return None;
        }
        Some(format!(
            "delegated session pinned to frozen child agent {}",
            meta.creation_selection.agent
        ))
    }

    /// Open a draft selector modal when allowed; otherwise surface the exact
    /// non-color reason it stays disabled. Agent switching is root-only;
    /// model selection stays available for delegated sessions inside their
    /// frozen agent's fallback chain. Neither is run-gated: drafts
    /// affect the next run only.
    pub(super) fn open_selection_modal(&mut self, modal: Modal) {
        match modal {
            Modal::Agents
                if self.new_session_draft.is_none() && !self.agent_switching_allowed() =>
            {
                self.status = self
                    .delegated_pin_reason()
                    .unwrap_or_else(|| "agent switching requires a root session".into());
            }
            Modal::Models if !self.model_selection_allowed() => {
                self.status = if self.runtime.is_empty() {
                    EMPTY_RUNTIME_GUIDANCE.into()
                } else {
                    "no draft model is available for this session".into()
                };
            }
            _ => {
                match modal {
                    Modal::Agents => self.agent_search.reset(),
                    Modal::Models => self.model_search.reset(),
                    _ => {}
                }
                self.picker_state.select(Some(0));
                self.modal = modal;
            }
        }
    }

    /// Revalidate a root draft against the current coherent descriptors while
    /// retaining every still-valid agent/model/variant choice. The producing
    /// agent of an active run is never reinterpreted.
    pub(super) fn revalidate_draft(&mut self) {
        if !self.agent_switching_allowed() {
            return;
        }
        let Some(mut draft) = self.draft.clone() else {
            self.draft = self.default_draft_selection();
            return;
        };
        if !self.agents.iter().any(|agent| {
            agent.runnable_as_root && agent.id == draft.agent && agent.preset == draft.preset
        }) {
            self.draft = self.default_draft_selection();
            return;
        }
        let Some(descriptor) = self.model_descriptor(&draft.model.model) else {
            draft.model = self
                .preferred_model_for_agent(&draft.agent, draft.preset.as_deref())
                .or_else(|| self.models.first().map(Self::default_model_selection))
                .unwrap_or(draft.model);
            self.draft = Some(draft);
            return;
        };
        if !Self::variant_is_valid(descriptor, draft.model.variant.as_ref()) {
            draft.model.variant = descriptor.default_variant.clone();
        }
        self.draft = Some(draft);
    }

    fn setup_status(&self) -> String {
        if self.runtime.is_empty() {
            return EMPTY_RUNTIME_GUIDANCE.into();
        }
        if self.runtime.phase() == RuntimePhase::Loading {
            return "loading runtime snapshot".into();
        }
        if self.runtime.phase() == RuntimePhase::ErrorRetry {
            return self
                .runtime
                .durable_explanation()
                .unwrap_or("runtime snapshot unavailable; retry")
                .into();
        }
        if self.agents.is_empty() {
            "No agents are configured; no session was created. Add an agent document, then restart or connect a provider."
                .into()
        } else {
            "No root-runnable agent is available; no session was created. Connect a provider for unresolved models or enable an agent with its own fallback chain."
                .into()
        }
    }

    fn model_descriptor(&self, key: &ModelKey) -> Option<&AvailableModelDescriptor> {
        self.models.iter().find(|model| &model.key == key)
    }

    fn default_model_selection(descriptor: &AvailableModelDescriptor) -> ModelSelection {
        ModelSelection {
            model: descriptor.key.clone(),
            variant: descriptor.default_variant.clone(),
        }
    }

    fn variant_is_valid(
        descriptor: &AvailableModelDescriptor,
        variant: Option<&VariantId>,
    ) -> bool {
        variant.is_none_or(|variant| {
            descriptor
                .variants
                .iter()
                .any(|candidate| candidate.id == *variant)
        })
    }

    fn selection_is_live(&self, selection: &ModelSelection) -> bool {
        self.model_descriptor(&selection.model)
            .is_some_and(|descriptor| {
                Self::variant_is_valid(descriptor, selection.variant.as_ref())
            })
    }

    fn preferred_model_for_agent(
        &self,
        agent: &AgentId,
        preset: Option<&str>,
    ) -> Option<ModelSelection> {
        self.agents
            .iter()
            .find(|candidate| candidate.id == *agent && candidate.preset.as_deref() == preset)
            .and_then(|descriptor| {
                descriptor
                    .resolved_fallback
                    .iter()
                    .find(|selection| self.selection_is_live(selection))
                    .cloned()
            })
    }

    /// The authoritative exact selections for the watched delegated
    /// session: the retained `RunStarted.selected_suffix` directly (after
    /// any run-selection head-variant override), falling back to the
    /// creation snapshot's resolved suffix only before any run. Empty-chain
    /// inherited children carry the inherited exact suffix frozen at
    /// delegation admission. Live descriptors are never consulted.
    fn persisted_chain(&self) -> Option<Vec<ModelSelection>> {
        if self.watching_root_session() {
            return None;
        }
        let session_id = self.selected?;
        let state = self.store.sessions.get(&session_id)?;
        if let Some(suffix) = &state.run_selected_suffix {
            return Some(
                suffix
                    .iter()
                    .map(|binding| binding.selection.clone())
                    .collect(),
            );
        }
        let snapshot = state.creation_agent.as_ref()?;
        let start = snapshot.selected_suffix_start as usize;
        Some(
            snapshot
                .fallback_chain
                .get(start..)
                .unwrap_or(&snapshot.fallback_chain)
                .iter()
                .map(|binding| binding.selection.clone())
                .collect(),
        )
    }

    /// The exact frozen variant selection for a model in the persisted
    /// delegated chain.
    fn persisted_chain_selection(&self, model: &ModelKey) -> Option<ModelSelection> {
        self.persisted_chain().and_then(|chain| {
            chain
                .into_iter()
                .find(|selection| &selection.model == model)
        })
    }

    /// Models listed for the draft: every coherent global descriptor for root
    /// sessions; the persisted frozen suffix for delegated sessions.
    pub(super) fn draft_models(&self) -> Vec<ModelSelection> {
        if !self.watching_root_session() {
            return self.persisted_chain().unwrap_or_default();
        }
        self.models
            .iter()
            .map(|descriptor| {
                self.draft
                    .as_ref()
                    .filter(|draft| draft.model.model == descriptor.key)
                    .map_or_else(
                        || Self::default_model_selection(descriptor),
                        |draft| draft.model.clone(),
                    )
            })
            .collect()
    }

    pub(super) fn filtered_draft_models(&self) -> Vec<ModelSelection> {
        self.draft_models()
            .into_iter()
            .filter(|selection| {
                model_matches(
                    selection,
                    self.model_descriptor(&selection.model),
                    self.model_search.query(),
                )
            })
            .collect()
    }

    /// Variant cycle for the selected draft model. Root order is exact base,
    /// then the descriptor's declared named-variant order. Delegated sessions
    /// expose only their exact persisted selection, so cycling cannot escape
    /// the suffix.
    pub(super) fn draft_variants(&self) -> Vec<Option<VariantId>> {
        let Some(draft) = &self.draft else {
            return Vec::new();
        };
        if !self.watching_root_session() {
            return self
                .persisted_chain_selection(&draft.model.model)
                .map(|selection| vec![selection.variant])
                .unwrap_or_default();
        }
        let mut variants = vec![None];
        if let Some(descriptor) = self.model_descriptor(&draft.model.model) {
            let mut named = descriptor
                .variants
                .iter()
                .map(|variant| variant.id.clone())
                .collect::<Vec<_>>();
            named.sort();
            let mut declared = descriptor.variant_order.clone();
            let mut declared_sorted = declared.clone();
            declared_sorted.sort();
            if declared_sorted != named {
                declared = named;
            }
            variants.extend(declared.into_iter().map(Some));
        }
        variants
    }

    /// The producing agent of the active run, frozen by the accepted
    /// `RunStarted` event. Draft changes never alter it.
    pub(super) fn active_run_agent(&self) -> Option<&AgentId> {
        let session_id = self.selected?;
        self.store
            .sessions
            .get(&session_id)
            .filter(|state| state.active_run.is_some())
            .and_then(|state| state.run_agent.as_ref())
    }

    pub(super) fn set_draft_agent(&mut self, agent: AgentId) {
        let targets_new_session = self.new_session_draft.is_some();
        let current = if targets_new_session {
            self.new_session_draft.as_ref()
        } else {
            self.draft.as_ref()
        };
        let preset = current.and_then(|draft| draft.preset.as_deref());
        let Some(descriptor) = self.agents.iter().find(|candidate| {
            candidate.id == agent
                && candidate.runnable_as_root
                && candidate.preset.as_deref() == preset
        }) else {
            return;
        };
        let model = current
            .map(|draft| draft.model.clone())
            .filter(|selection| self.selection_is_live(selection))
            .or_else(|| {
                descriptor
                    .resolved_fallback
                    .iter()
                    .find(|selection| self.selection_is_live(selection))
                    .cloned()
            })
            .or_else(|| self.models.first().map(Self::default_model_selection));
        let Some(model) = model else {
            return;
        };
        let selection = RunSelection {
            agent,
            model,
            preset: descriptor.preset.clone(),
        };
        if targets_new_session {
            self.status = format!("New session agent: {}", draft_title(&selection));
            self.new_session_draft = Some(selection);
        } else {
            self.draft = Some(selection);
            self.status = self.draft_status("Draft run agent");
        }
    }

    pub(super) fn set_draft_model(&mut self, model: ModelKey) {
        let Some(draft) = self.draft.clone() else {
            return;
        };
        if draft.model.model == model {
            self.status = self.draft_status("Draft run model");
            return;
        }
        // Delegated sessions resolve only against the persisted frozen
        // suffix; root sessions use the complete live catalog and select the
        // chosen model's resolved default variant.
        let selection = if self.watching_root_session() {
            self.model_descriptor(&model)
                .map(Self::default_model_selection)
        } else {
            self.persisted_chain_selection(&model)
        };
        let Some(selection) = selection else {
            self.status = format!("model {model} is not available for agent {}", draft.agent);
            return;
        };
        self.draft = Some(RunSelection {
            agent: draft.agent,
            model: selection,
            preset: draft.preset,
        });
        self.status = self.draft_status("Draft run model");
    }

    pub(super) fn set_draft_variant(&mut self, variant: Option<VariantId>) {
        let Some(draft) = self.draft.clone() else {
            return;
        };
        self.draft = Some(RunSelection {
            agent: draft.agent,
            model: ModelSelection {
                model: draft.model.model,
                variant,
            },
            preset: draft.preset,
        });
        self.status = self.draft_status("Draft run variant");
    }

    pub(super) fn cycle_draft_variant(&mut self) {
        let variants = self.draft_variants();
        if variants.len() <= 1 {
            return;
        }
        let Some(current) = self.draft.as_ref().map(|draft| draft.model.variant.clone()) else {
            return;
        };
        let index = variants
            .iter()
            .position(|variant| *variant == current)
            .unwrap_or(0);
        self.set_draft_variant(variants[(index + 1) % variants.len()].clone());
    }

    fn cycle_event_level_filter(&mut self) {
        self.tui_config.minimum_event_level = match self.tui_config.minimum_event_level {
            crate::state::EventLevel::Debug => crate::state::EventLevel::Info,
            crate::state::EventLevel::Info => crate::state::EventLevel::Warning,
            crate::state::EventLevel::Warning => crate::state::EventLevel::Error,
            crate::state::EventLevel::Error => crate::state::EventLevel::Debug,
        };
        self.status = format!(
            "Event level: {}",
            self.tui_config.minimum_event_level.name()
        );
    }

    fn permission_mode_root(&self, session_id: SessionId) -> SessionId {
        let meta = self
            .sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .or_else(|| {
                self.tree
                    .as_ref()
                    .and_then(|tree| find_session(tree, session_id))
            });
        match meta.map(|meta| &meta.origin) {
            Some(cookie_agent_protocol::SessionOrigin::Delegated {
                root_session_id, ..
            }) => *root_session_id,
            _ => session_id,
        }
    }

    fn permission_mode(&self, session_id: SessionId) -> PermissionMode {
        let root = self.permission_mode_root(session_id);
        self.permission_modes
            .get(&root)
            .copied()
            .unwrap_or_default()
    }

    fn next_permission_mode_generation(&mut self, session_id: SessionId) -> u64 {
        let generation = self
            .permission_mode_generations
            .entry(session_id)
            .or_default();
        *generation = generation.wrapping_add(1);
        *generation
    }

    fn cycle_permission_mode(&mut self) {
        let Some(selected) = self.selected else {
            return;
        };
        let session_id = self.permission_mode_root(selected);
        let previous = self.permission_mode(session_id);
        let mode = match previous {
            PermissionMode::AutoApprove => PermissionMode::AutoApproveN,
            PermissionMode::AutoApproveN => PermissionMode::AutoApproveY,
            PermissionMode::AutoApproveY => PermissionMode::Ask,
            PermissionMode::Ask => PermissionMode::Yolo,
            PermissionMode::Yolo => PermissionMode::AutoApprove,
        };
        let generation = self.next_permission_mode_generation(session_id);
        self.permission_modes.insert(session_id, mode);
        self.status = format!(
            "Permission mode: {} — applies to subsequent approvals in this session tree",
            permission_mode_label(mode)
        );
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .set_permission_mode(SessionSetPermissionModeParams { session_id, mode })
                .await
                .map(|_| ())
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PermissionModeMutationFinished {
                session_id,
                generation,
                result,
            });
        });
    }

    /// The coherent descriptor revision label projected from one runtime snapshot.
    pub(super) fn descriptor_revisions_label(&self) -> String {
        match (&self.agent_revision, &self.model_revision) {
            (Some(agents), Some(models)) => {
                format!("agent revision {agents} · model revision {models}")
            }
            _ => "revisions unavailable".into(),
        }
    }

    fn draft_status(&self, action: &str) -> String {
        let Some(draft) = &self.draft else {
            return "no draft selection".into();
        };
        let preset = draft.preset.as_deref().unwrap_or("shared");
        if self.active_run_agent().is_some() {
            format!(
                "{action}: {} · preset {preset}; applies to the next run — the active run is unchanged",
                draft_title(draft),
            )
        } else {
            format!("{action}: {} · preset {preset}", draft_title(draft))
        }
    }

    pub(super) fn cycle_agent(&mut self, backward: bool) {
        if self.new_session_draft.is_none() && !self.agent_switching_allowed() {
            self.status = self
                .delegated_pin_reason()
                .unwrap_or_else(|| "agent switching requires a root session".into());
            return;
        }
        let selectable = self
            .agent_picker_candidates()
            .into_iter()
            .map(|agent| agent.id.clone())
            .collect::<Vec<_>>();
        if selectable.is_empty() {
            self.status = "no root-runnable agent is available".into();
            return;
        }
        let current = self
            .new_session_draft
            .as_ref()
            .or(self.draft.as_ref())
            .map(|draft| draft.agent.clone());
        let index = current.and_then(|id| selectable.iter().position(|agent| *agent == id));
        let next = match (index, backward) {
            (Some(index), true) => (index + selectable.len() - 1) % selectable.len(),
            (Some(index), false) => (index + 1) % selectable.len(),
            (None, true) => selectable.len() - 1,
            (None, false) => 0,
        };
        self.set_draft_agent(selectable[next].clone());
    }

    /// Warning rows from strict descendants of the viewed session, attributed
    /// to their owning session. Ownership stays durable in the child's own
    /// projection; this is a read-only aggregate for the current view.
    pub(super) fn descendant_warnings(&self, viewed: SessionId) -> Vec<String> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let Some(node) = find_node(tree, viewed) else {
            return Vec::new();
        };
        let mut members = Vec::new();
        collect_subtree_sessions(node, &mut members);
        let mut warnings = Vec::new();
        for meta in members.into_iter().filter(|meta| meta.session_id != viewed) {
            let Some(state) = self.store.sessions.get(&meta.session_id) else {
                continue;
            };
            let source = meta
                .title
                .as_ref()
                .map(SessionTitle::to_string)
                .unwrap_or_else(|| meta.creation_selection.agent.to_string());
            for item in &state.transcript {
                if let TranscriptItem::Event {
                    level: crate::state::EventLevel::Warning,
                    text,
                    ..
                } = item
                {
                    warnings.push(format!(
                        "from {source} ({}): {text}",
                        short_id(meta.session_id)
                    ));
                }
            }
        }
        warnings
    }

    pub(super) async fn select_session(&mut self, session_id: SessionId) {
        self.reroot_tree(session_id);
        let cursor = self
            .store
            .sessions
            .get(&session_id)
            .map(|state| state.last_seq);
        match tokio::time::timeout(
            TREE_SUBSCRIPTION_TIMEOUT,
            self.client.subscribe_events(session_id, cursor),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.status = error.to_string(),
            Err(_) => self.status = "session subscription timed out".into(),
        }
    }

    /// Watch a session inside the current delegation tree: the conversation
    /// and highlight change, but the tree root, tree snapshot, and cursor are
    /// never cleared or rerooted. All tree refreshes keep querying the
    /// original root.
    pub(super) fn watch_session(&mut self, session_id: SessionId) {
        let in_tree = self
            .tree
            .as_ref()
            .is_some_and(|tree| find_session(tree, session_id).is_some());
        if !in_tree {
            self.reroot_tree(session_id);
            self.classify_session_background(session_id);
            return;
        }
        self.set_selected_session(session_id);
        self.classify_session_background(session_id);
        self.tree_cursor = Some(session_id);
        let needs_subscription = self.tree_subscription_sessions.insert(session_id);
        let cursor = self
            .store
            .sessions
            .get(&session_id)
            .map(|state| state.last_seq);
        if needs_subscription {
            self.subscribe_session_background(session_id, cursor, None);
        }
    }

    /// Intentionally reroot the delegation tree at a separate session.
    fn reroot_tree(&mut self, session_id: SessionId) {
        self.set_selected_session(session_id);
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.tree_root = Some(session_id);
        self.tree = None;
        self.tree_cursor = Some(session_id);
        self.tree_subscription_sessions.clear();
        self.tree_subscription_sessions.insert(session_id);
        self.tree_refresh_in_flight = None;
        self.tree_refresh_pending = false;
        self.tree_offset = 0;
        self.tree_viewport_height = 0;
        let cursor = self
            .store
            .sessions
            .get(&session_id)
            .map(|state| state.last_seq);
        self.subscribe_session_background(session_id, cursor, None);
        self.refresh_tree_background();
    }

    fn subscribe_session_background(
        &self,
        session_id: SessionId,
        cursor: Option<u64>,
        live_attempt: Option<u64>,
    ) {
        // Re-subscribing is safe: the lane lock serializes it with any prior
        // subscription for the same session, and the client reconciles
        // cursors.
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let lanes = self.subscription_lanes.clone();
        self.spawn_rpc(async move {
            let lane = {
                let mut lanes = lanes.lock().await;
                lanes
                    .entry(session_id)
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                    .clone()
            };
            let _guard = lane.lock().await;
            let outcome = match tokio::time::timeout(
                TREE_SUBSCRIPTION_TIMEOUT,
                client.subscribe_events(session_id, cursor),
            )
            .await
            {
                Ok(Err(crate::client::ClientError::ReplayInProgress)) => {
                    SessionLiveSubscriptionOutcome::ReplayInProgress
                }
                Ok(Ok(())) => SessionLiveSubscriptionOutcome::Established,
                Ok(Err(error)) => SessionLiveSubscriptionOutcome::Failed(error.to_string()),
                Err(_) => {
                    SessionLiveSubscriptionOutcome::Failed("session subscription timed out".into())
                }
            };
            let _ = updates.send(RpcUpdate::SessionLiveSubscriptionFinished {
                session_id,
                live_attempt,
                outcome,
            });
        });
    }

    pub(super) fn refresh_tree_background(&mut self) {
        let Some(root) = self.tree_root else {
            return;
        };
        self.refresh_tree_background_for(root, self.selection_generation);
    }

    fn refresh_tree_background_for(&mut self, session_id: SessionId, generation: u64) {
        if self.tree_refresh_in_flight.is_some() {
            self.tree_refresh_pending = true;
            return;
        }
        self.next_tree_refresh_id = self.next_tree_refresh_id.wrapping_add(1);
        let request_id = self.next_tree_refresh_id;
        self.tree_refresh_in_flight = Some((generation, request_id));
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            match tokio::time::timeout(
                TREE_REFRESH_TIMEOUT,
                client.session_tree(SessionTreeParams { session_id }),
            )
            .await
            {
                Ok(Ok(result)) => {
                    let _ = updates.send(RpcUpdate::Tree {
                        session_id,
                        generation,
                        request_id,
                        tree: Box::new(result.tree),
                    });
                }
                Ok(Err(error)) => {
                    let _ = updates.send(RpcUpdate::TreeFailed {
                        session_id,
                        generation,
                        request_id,
                        error: error.to_string(),
                    });
                }
                Err(_) => {
                    let _ = updates.send(RpcUpdate::TreeFailed {
                        session_id,
                        generation,
                        request_id,
                        error: "tree refresh timed out".into(),
                    });
                }
            }
        });
    }

    pub(super) fn handle_rpc_update(&mut self, update: RpcUpdate) {
        match update {
            RpcUpdate::Status(status) => {
                self.session_errors.record(&status);
                self.status = status;
            }
            RpcUpdate::Notice(status) => self.status = status,
            RpcUpdate::SessionOwnershipClassified {
                session_id,
                generation,
                outcome,
            } => self.apply_ownership_classification(session_id, generation, outcome),
            RpcUpdate::SessionLiveSubscriptionFinished {
                session_id,
                live_attempt,
                outcome,
            } => self.finish_live_subscription(session_id, live_attempt, outcome),
            RpcUpdate::SteerFailed {
                session_id,
                input,
                error,
            } => {
                if self.selected == Some(session_id) {
                    self.restore_composer_text(vec![input]);
                    self.status = format!("message not sent ({error}); restored to the composer");
                } else {
                    self.store.park_voided_input(session_id, input);
                    self.status =
                        format!("a message failed to send ({error}); kept for its session");
                }
            }
            RpcUpdate::SteerRecalled { session_id, text } => {
                if self.selected == Some(session_id) {
                    self.restore_composer_text(vec![text]);
                    self.status = "recalled message restored to the composer".into();
                } else {
                    // The composer belongs to another session right now;
                    // park the text so it is restored when that session is
                    // viewed rather than leaking across sessions.
                    self.store.park_voided_input(session_id, text);
                    self.status = "recalled message kept for its session".into();
                }
            }
            RpcUpdate::Reverted { session_id, text } => {
                // The transcript rebuild rides the SessionReverted event;
                // here only the composer's share and the tree's
                // branch-derived rows (title/status/usage) need attention.
                if self.selected == Some(session_id) {
                    self.restore_composer_text(vec![text]);
                    self.status = "reverted; the message text is back in the composer".into();
                } else {
                    self.store.park_voided_input(session_id, text);
                    self.status = "reverted; the message text is kept for its session".into();
                }
                self.refresh_tree_background();
            }
            RpcUpdate::Forked { forked } => {
                self.status = "forked the session; switching to it".into();
                self.refresh_tree_background();
                // This update is emitted only after the fork RPC confirms success.
                self.ownership_classifications.remove(&forked);
                self.owned_sessions.insert(forked);
                self.read_only_sessions.remove(&forked);
                self.pending_live_subscriptions.remove(&forked);
                self.live_subscription_attempts.remove(&forked);
                self.replay_ended_for_live_subscription.remove(&forked);
                self.reroot_tree(forked);
            }
            RpcUpdate::Tree {
                session_id,
                generation,
                request_id,
                tree,
            } if self.tree_refresh_in_flight == Some((generation, request_id)) => {
                self.tree_refresh_in_flight = None;
                if self.tree_root == Some(session_id) && self.selection_generation == generation {
                    let mut tree = *tree;
                    self.patch_tree_titles(&mut tree);
                    self.subscribe_tree_sessions(&tree);
                    self.tree = Some(tree);
                    self.clamp_tree_view();
                }
                self.refresh_pending_tree();
            }
            RpcUpdate::TreeFailed {
                session_id,
                generation,
                request_id,
                error,
            } if self.tree_refresh_in_flight == Some((generation, request_id)) => {
                self.tree_refresh_in_flight = None;
                if self.tree_root == Some(session_id) && self.selection_generation == generation {
                    self.status = error;
                }
                self.refresh_pending_tree();
            }
            RpcUpdate::Tree { .. } => {}
            RpcUpdate::TreeFailed { .. } => {}
            RpcUpdate::ProviderMutationFinished { outcome } => {
                self.connect_task = None;
                self.apply_provider_mutation_outcome(outcome);
            }
            RpcUpdate::ApprovalResponse {
                request_id,
                approval_id,
                result,
            } => {
                self.finish_approval_submission(request_id, approval_id, result);
            }
            RpcUpdate::ApprovalList {
                root_session_id,
                generation,
                request_id,
                result,
            } if self.approval_refresh_in_flight
                == Some((root_session_id, generation, request_id)) =>
            {
                self.approval_refresh_in_flight = None;
                let current_root = self.tree_root.or(self.selected);
                if current_root != Some(root_session_id) || self.selection_generation != generation
                {
                    return;
                }
                match result {
                    Ok(result) => {
                        self.apply_approval_list(root_session_id, result);
                        self.reconcile_pending_approval();
                    }
                    Err(error) => self.status = format!("approval list refresh failed: {error}"),
                }
            }
            RpcUpdate::ApprovalList { .. } => {}
            RpcUpdate::PermissionModeMutationFinished {
                session_id,
                generation,
                result,
            } => {
                if self.permission_mode_generations.get(&session_id).copied() != Some(generation) {
                    return;
                }
                if let Err(error) = result {
                    self.permission_modes.remove(&session_id);
                    self.status = format!("permission mode update failed: {error}");
                    self.refresh_permission_mode_for_session(session_id);
                }
            }
            RpcUpdate::PermissionModeLoaded {
                session_id,
                generation,
                result,
            } => {
                if self.permission_mode_generations.get(&session_id).copied() != Some(generation) {
                    return;
                }
                match result {
                    Ok(Some(mode)) => {
                        self.permission_modes.insert(session_id, mode);
                    }
                    Ok(None) => {}
                    Err(error) => self.status = format!("permission mode load failed: {error}"),
                }
            }
            RpcUpdate::McpRefreshed { result } => {
                self.mcp_panel.refresh_in_flight = false;
                match result {
                    Ok(servers) => {
                        self.mcp_panel.install(servers.servers);
                    }
                    Err(error) => self.status = format!("MCP refresh failed: {error}"),
                }
            }
            RpcUpdate::McpMutation { result } => match *result {
                Ok(_) => {
                    self.status = "MCP server state updated.".into();
                    self.poll_mcp();
                }
                Err(error) => self.status = format!("MCP update failed: {error}"),
            },
            RpcUpdate::McpAuthBegan { result } => match result {
                Ok(result) => {
                    self.mcp_panel.auth = Some(McpAuthView {
                        server: result.server,
                        authorization_url: result.authorization_url,
                    });
                    self.status = "waiting for MCP OAuth authorization".into();
                }
                Err(error) => self.status = format!("MCP authentication failed: {error}"),
            },
            RpcUpdate::McpAuthCancelled { result } => match result {
                Ok(server) => {
                    if self
                        .mcp_panel
                        .auth
                        .as_ref()
                        .is_some_and(|auth| auth.server == server)
                    {
                        self.mcp_panel.auth = None;
                    }
                    self.status = "MCP authentication cancelled.".into();
                    self.poll_mcp();
                }
                Err(error) => {
                    self.mcp_panel.auth = None;
                    self.status = format!("MCP authentication cancel failed: {error}");
                    self.poll_mcp();
                }
            },
            RpcUpdate::PermissionsLoaded { session_id, result } => {
                if self.selected != Some(session_id) {
                    return;
                }
                match result {
                    Ok(result) => {
                        self.permission_panel.install(result);
                        self.load_skills_for_session(session_id);
                    }
                    Err(error) => self.status = format!("permission update failed: {error}"),
                }
            }
            RpcUpdate::SkillsLoaded { session_id, result } => {
                if self.selected != Some(session_id) {
                    return;
                }
                match result {
                    Ok(result) => {
                        self.skills = result.skills.clone();
                        self.skill_panel.install(result);
                    }
                    Err(error) => self.status = format!("skill discovery failed: {error}"),
                }
            }
            RpcUpdate::UsageLoaded {
                generation,
                session_id,
                session,
                tree,
            } => {
                if generation != self.usage_load_generation {
                    return;
                }
                self.usage_panel.loading = false;
                if self.selected == session_id {
                    match session {
                        Ok(result) => self.usage_panel.session = result,
                        Err(error) => self.status = format!("session usage failed: {error}"),
                    }
                    match tree {
                        Ok(result) => self.usage_panel.tree = result,
                        Err(ClientError::Rpc(error))
                            if error.code == SESSION_TREE_USAGE_CORRUPT_DELEGATION_CODE =>
                        {
                            self.usage_panel.tree_corrupt = true;
                        }
                        Err(error) => self.status = format!("session tree usage failed: {error}"),
                    }
                }
            }
            RpcUpdate::SessionCostLoaded {
                session_id,
                request_id,
                result,
            } => {
                let Some(refresh) = self.cost_refreshes.get_mut(&session_id) else {
                    return;
                };
                if !refresh.in_flight || refresh.request_id != request_id {
                    return;
                }
                refresh.in_flight = false;
                let dirty = std::mem::take(&mut refresh.dirty);
                if let Ok(result) = result
                    && let Some(state) = self.store.sessions.get_mut(&session_id)
                {
                    state.estimated_cost_usd = result.usage.estimated_cost_usd;
                }
                if dirty {
                    self.schedule_session_cost_refresh(session_id);
                }
            }
            RpcUpdate::SessionCostDebounceElapsed {
                session_id,
                generation,
            } => {
                let launch = self
                    .cost_refreshes
                    .get_mut(&session_id)
                    .is_some_and(|refresh| {
                        if !refresh.scheduled || refresh.debounce_generation != generation {
                            return false;
                        }
                        refresh.scheduled = false;
                        true
                    });
                if launch {
                    self.start_session_cost_refresh(session_id);
                }
            }
        }
    }

    /// Apply event-sequence staleness rules to a fresh tree response so title
    /// and run-status event patches cannot be undone by an older RPC result.
    pub(super) fn patch_tree_titles(&mut self, tree: &mut SessionTree) {
        let mut known_titles = HashMap::new();
        collect_known_titles(
            self.tree.as_ref(),
            &self.sessions,
            &self.title_sequences,
            &mut known_titles,
        );
        patch_tree_node_titles(tree, &self.title_sequences, &known_titles);
        let mut known_statuses = HashMap::new();
        collect_known_statuses(self.tree.as_ref(), &self.sessions, &mut known_statuses);
        patch_tree_node_statuses(tree, &known_statuses);
    }

    pub(super) fn apply_provider_mutation_outcome(&mut self, outcome: ProviderMutationOutcome) {
        match outcome {
            ProviderMutationOutcome::Failed {
                provider_id,
                action,
                error,
            } => {
                self.provider_operations.insert(
                    provider_id.clone(),
                    ProviderOperation::Error {
                        action,
                        message: error.clone(),
                    },
                );
                if matches!(action, ProviderAction::Connect | ProviderAction::Reconnect)
                    && let Some(form) = &mut self.provider_form
                    && form.provider.id == provider_id
                {
                    form.error = Some(error.clone());
                    self.modal = Modal::ConnectError;
                    self.status = format!("Provider {} failed: {error}", action_name(action));
                } else {
                    self.status = format!(
                        "Provider {} failed: {error}. Enter the row to retry.",
                        action_name(action)
                    );
                }
            }
            ProviderMutationOutcome::Connected {
                provider_id,
                baseline,
                runtime,
            } => {
                self.provider_operations.remove(&provider_id);
                self.install_runtime_response(baseline.as_ref(), *runtime);
                self.clear_connect_secrets();
                self.modal = Modal::None;
                self.status = if self.runtime.is_empty() {
                    EMPTY_RUNTIME_GUIDANCE.into()
                } else {
                    format!("Connected provider {provider_id}.")
                };
            }
            ProviderMutationOutcome::Disconnected {
                provider_id,
                baseline,
                runtime,
            } => {
                self.provider_operations.remove(&provider_id);
                self.install_runtime_response(baseline.as_ref(), *runtime);
                self.status = if self.runtime.is_empty() {
                    EMPTY_RUNTIME_GUIDANCE.into()
                } else {
                    format!("Disconnected provider {provider_id}.")
                };
            }
        }
    }

    fn refresh_pending_tree(&mut self) {
        if std::mem::take(&mut self.tree_refresh_pending) {
            self.refresh_tree_background();
        }
    }

    fn subscribe_tree_sessions(&mut self, tree: &SessionTree) {
        let mut session_ids = Vec::new();
        collect_tree_session_ids(tree, &mut session_ids);
        for session_id in session_ids {
            if !self.tree_subscription_sessions.insert(session_id) {
                continue;
            }
            let cursor = self
                .store
                .sessions
                .get(&session_id)
                .map(|state| state.last_seq);
            self.subscribe_session_background(session_id, cursor, None);
        }
    }

    pub(super) fn set_selected_session(&mut self, session_id: SessionId) {
        let changed = self.selected != Some(session_id);
        if changed {
            // Watching a different session should begin at its live tail.
            self.conversation_scroll = ConversationScroll::default();
            self.scrollbar_geometry = None;
            self.scrollbar_drag = None;
            // A conversation-leg selection addresses rendered lines of the
            // session being left; kept across the switch it would paint and
            // copy rows of the newly watched session. The composer leg
            // survives: the draft buffer persists across watches.
            if matches!(self.selection, Some(TextSelection::Conversation { .. })) {
                self.selection = None;
            }
        }
        self.selected = Some(session_id);
        if changed {
            let root = self.permission_mode_root(session_id);
            self.permission_modes.remove(&root);
            self.refresh_permission_mode_for_session(session_id);
            self.load_skills_for_session(session_id);
        }
        self.rebind_draft_to_selected_session();
        // Voided inputs (run-end casualties, cross-session recalls) are
        // restored exactly when their session is being viewed.
        self.restore_voided_inputs();
    }

    /// Rebind the draft to the newly watched session. A root session drafts
    /// its own current creation selection when still valid against the
    /// coherent descriptors, otherwise the root default; a delegated session
    /// is pinned to its frozen child agent with the valid chain model/variant
    /// — a previous root draft is never carried into a child.
    fn rebind_draft_to_selected_session(&mut self) {
        let Some(meta) = self.selected_session_meta().cloned() else {
            return;
        };
        match &meta.origin {
            cookie_agent_protocol::SessionOrigin::Delegated { .. } => {
                // The pinned child draft derives only from the persisted
                // frozen chain: the exact creation selection when it is a
                // chain member, otherwise the chain head (the inherited
                // frozen suffix head for empty-chain children).
                let creation = meta.creation_selection.clone();
                if let Some(chain) = self.persisted_chain() {
                    let model = chain
                        .iter()
                        .find(|selection| **selection == creation.model)
                        .cloned()
                        .or_else(|| chain.first().cloned())
                        .unwrap_or(creation.model);
                    self.draft = Some(RunSelection {
                        agent: creation.agent,
                        model,
                        preset: creation.preset,
                    });
                } else {
                    self.draft = Some(creation);
                }
            }
            cookie_agent_protocol::SessionOrigin::Root => {
                let creation = meta.creation_selection.clone();
                self.draft = Some(creation);
                self.revalidate_draft();
            }
        }
    }

    pub(super) async fn refresh_tree(&mut self) {
        if let Some(root) = self.tree_root {
            let generation = self.selection_generation;
            match tokio::time::timeout(
                TREE_REFRESH_TIMEOUT,
                self.client
                    .session_tree(SessionTreeParams { session_id: root }),
            )
            .await
            {
                Ok(Ok(result))
                    if self.tree_root == Some(root) && self.selection_generation == generation =>
                {
                    let mut tree = result.tree;
                    self.patch_tree_titles(&mut tree);
                    self.subscribe_tree_sessions(&tree);
                    self.tree = Some(tree);
                    self.clamp_tree_view();
                }
                Ok(Err(error)) => self.status = error.to_string(),
                Err(_) => self.status = "tree refresh timed out".into(),
                Ok(Ok(_)) => {}
            }
        }
    }

    pub(super) async fn handle_delivery(&mut self, delivery: ClientDelivery) {
        if let ClientDelivery::RuntimeChanged(changed) = &delivery {
            self.install_runtime_notification((**changed).clone());
            return;
        }
        if let ClientDelivery::RecoveryFailed { session_id, error } = &delivery {
            self.status = match session_id {
                Some(session_id) => format!("recovery for {session_id} failed: {error}"),
                None => format!("recovery failed: {error}"),
            };
            return;
        }
        if let ClientDelivery::PluginEvent(event) = &delivery {
            if Some(event.session_id) == self.selected {
                self.status = format!("plugin {}: {}", event.plugin, event.name);
            }
            return;
        }
        let event = match &delivery {
            ClientDelivery::Live { message, .. } => match message.as_ref() {
                cookie_agent_protocol::EventSubscriptionMessage::Event { event } => {
                    Some(event.as_ref())
                }
                cookie_agent_protocol::EventSubscriptionMessage::Gap { .. } => None,
            },
            ClientDelivery::ReplayEvent { event, .. } => Some(event.as_ref()),
            _ => None,
        };
        let linked = event
            .is_some_and(|event| matches!(&event.payload, EventPayload::ToolCallLinked { .. }));
        let title_change = event.and_then(title_change_from_event);
        let status_change = event.and_then(status_change_from_event);
        let refresh_skills = event.and_then(|event| {
            (Some(event.session_id) == self.selected
                && matches!(
                    event.payload,
                    EventPayload::RunStarted { .. }
                        | EventPayload::SessionPermissionOverlaySet { .. }
                ))
            .then(|| event.clone())
        });
        let refresh_cost = event.and_then(|event| {
            matches!(
                event.payload,
                EventPayload::ModelUsageRecorded { .. }
                    | EventPayload::InternalAgentUsageRecorded { .. }
            )
            .then_some(event.session_id)
        });
        if let Some((session_id, title, seq)) = title_change {
            self.apply_title_patch(session_id, title, seq);
        }
        if let Some((session_id, status, seq)) = status_change {
            self.apply_status_patch(session_id, status, seq);
        }
        let replay_finished = matches!(
            &delivery,
            ClientDelivery::ReplayEnd { session_id, .. } if Some(*session_id) == self.selected
        );
        let replay_ended_session = match &delivery {
            ClientDelivery::ReplayEnd { session_id, .. } => Some(*session_id),
            _ => None,
        };
        let revert_rebuild = event.is_some_and(|event| {
            Some(event.session_id) == self.selected
                && matches!(&event.payload, EventPayload::SessionReverted { .. })
        });
        let outcome = self.store.apply_delivery(delivery);
        if let Some(session_id) = replay_ended_session
            && self.pending_live_subscriptions.contains(&session_id)
        {
            self.replay_ended_for_live_subscription.insert(session_id);
            self.start_pending_live_subscription(session_id);
        }
        if (revert_rebuild || replay_finished)
            && matches!(self.selection, Some(TextSelection::Conversation { .. }))
        {
            // The viewed transcript was replaced — a revert rebuilds the
            // visible branch, a recovery replay swaps in a whole new
            // projection — so the conversation leg's content coordinates
            // address lines that no longer exist and would paint and copy
            // the wrong text. The composer leg survives: the draft buffer
            // is untouched by either rebuild (its own restoring mutations
            // retire it separately).
            self.selection = None;
        }
        if matches!(outcome, DeliveryOutcome::Applied) {
            // The pending lane itself is a pure event reduction (admitted,
            // promoted, recalled, replayed identically); only the composer's
            // share needs a hook here: run-end events void pending inputs
            // without per-entry events, so drain whatever the viewed session
            // is owed back into the composer.
            self.restore_voided_inputs();
            if let Some(event) = &refresh_skills {
                self.refresh_skills_for_event(event);
            }
            if let Some(session_id) = refresh_cost {
                self.refresh_session_cost(session_id);
            }
        }
        match outcome {
            DeliveryOutcome::Applied => {}
            DeliveryOutcome::Gap { cursor, .. } => {
                self.status = format!("event gap after sequence {cursor}; replaying");
            }
            DeliveryOutcome::ReplayFailed { session_id } => {
                self.status = "incomplete replay; retrying recovery".into();
                self.client.recover_session(session_id, true);
            }
        }
        self.reconcile_pending_approval();
        if linked || replay_finished {
            self.refresh_tree_background();
        }
    }

    /// Apply a strictly-newer title event patch immediately: the Agents
    /// panel rows and session list update without waiting for a tree
    /// refresh, and older tree/list responses cannot undo it.
    pub(super) fn apply_title_patch(
        &mut self,
        session_id: SessionId,
        title: Option<SessionTitle>,
        seq: u64,
    ) {
        let known = self.title_sequences.entry(session_id).or_insert(0);
        if seq < *known {
            return;
        }
        *known = seq;
        let mut session_changed = false;
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.session_id == session_id)
        {
            session.title = title.clone();
            session.title_updated_seq = seq;
            session_changed = true;
        }
        if let Some(tree) = &mut self.tree
            && let Some(node) = find_node_mut(tree, session_id)
            && seq >= node.session.title_updated_seq
        {
            node.session.title = title;
            node.session.title_updated_seq = seq;
        }
        if session_changed {
            self.note_sessions_changed();
        }
    }

    /// Apply a run lifecycle status immediately to both panel metadata
    /// sources without waiting for a session-list or tree RPC response.
    pub(super) fn apply_status_patch(
        &mut self,
        session_id: SessionId,
        status: SessionStatus,
        seq: u64,
    ) {
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.session_id == session_id)
            && seq >= session.last_event_seq
        {
            session.status = status;
            session.last_event_seq = seq;
        }
        if let Some(tree) = &mut self.tree
            && let Some(node) = find_node_mut(tree, session_id)
            && seq >= node.session.last_event_seq
        {
            node.session.status = status;
            node.session.last_event_seq = seq;
        }
    }

    pub(super) fn recover_timed_out_replays(&mut self) {
        for session_id in self.store.abandon_timed_out_replays() {
            self.status = "replay timed out; retrying recovery".into();
            self.client.recover_session(session_id, true);
        }
        self.reconcile_pending_approval();
    }

    pub(super) fn poll_mcp(&mut self) {
        if self.modal != Modal::Mcp || self.mcp_panel.refresh_in_flight {
            return;
        }
        self.mcp_panel.refresh_in_flight = true;
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_mcp_servers()
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpRefreshed { result });
        });
    }

    fn load_permissions(&mut self) {
        let Some(session_id) = self.selected else {
            self.status = "select a session before editing permissions".into();
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .get_session_permissions(SessionPermissionGetParams { session_id })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PermissionsLoaded { session_id, result });
        });
    }

    fn refresh_permission_mode_for_session(&mut self, session_id: SessionId) {
        let session_id = self.permission_mode_root(session_id);
        let generation = self.next_permission_mode_generation(session_id);
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .get_session_permissions(SessionPermissionGetParams { session_id })
                .await
                .map(|result| result.current_mode)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PermissionModeLoaded {
                session_id,
                generation,
                result,
            });
        });
    }

    fn load_skills_for_session(&mut self, session_id: SessionId) {
        #[cfg(test)]
        self.skill_refresh_requests.push(session_id);
        self.skills.clear();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_skills(cookie_agent_protocol::SkillsListParams { session_id })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::SkillsLoaded { session_id, result });
        });
    }

    fn refresh_skills_for_event(&mut self, event: &StoredEvent) {
        if Some(event.session_id) == self.selected
            && matches!(
                event.payload,
                EventPayload::RunStarted { .. } | EventPayload::SessionPermissionOverlaySet { .. }
            )
        {
            self.load_skills_for_session(event.session_id);
        }
    }

    fn load_usage(&mut self) {
        let session_id = self.selected;
        self.usage_load_generation = self
            .usage_load_generation
            .checked_add(1)
            .expect("usage load generation exhausted");
        let generation = self.usage_load_generation;
        self.usage_panel.begin_load();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let session = async {
                match session_id {
                    Some(session_id) => client
                        .session_usage(SessionUsageParams { session_id })
                        .await
                        .map(Some)
                        .map_err(|error| error.to_string()),
                    None => Ok(None),
                }
            };
            let tree = async {
                match session_id {
                    Some(session_id) => client
                        .session_tree_usage(SessionUsageParams { session_id })
                        .await
                        .map(Some),
                    None => Ok(None),
                }
            };
            let (session, tree) = tokio::join!(session, tree);
            let _ = updates.send(RpcUpdate::UsageLoaded {
                generation,
                session_id,
                session,
                tree,
            });
        });
    }

    fn refresh_session_cost(&mut self, session_id: SessionId) {
        let refresh = self.cost_refreshes.entry(session_id).or_default();
        if refresh.in_flight {
            refresh.dirty = true;
            return;
        }
        if refresh.scheduled {
            self.schedule_session_cost_refresh(session_id);
            return;
        }
        self.start_session_cost_refresh(session_id);
    }

    fn schedule_session_cost_refresh(&mut self, session_id: SessionId) {
        let refresh = self.cost_refreshes.entry(session_id).or_default();
        refresh.debounce_generation = refresh.debounce_generation.wrapping_add(1);
        refresh.scheduled = true;
        let generation = refresh.debounce_generation;
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            tokio::time::sleep(SESSION_COST_DEBOUNCE).await;
            let _ = updates.send(RpcUpdate::SessionCostDebounceElapsed {
                session_id,
                generation,
            });
        });
    }

    fn start_session_cost_refresh(&mut self, session_id: SessionId) {
        self.next_cost_refresh_request_id = self.next_cost_refresh_request_id.wrapping_add(1);
        let request_id = self.next_cost_refresh_request_id;
        let refresh = self.cost_refreshes.entry(session_id).or_default();
        refresh.scheduled = false;
        refresh.in_flight = true;
        refresh.dirty = false;
        refresh.request_id = request_id;
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .session_usage(SessionUsageParams { session_id })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::SessionCostLoaded {
                session_id,
                request_id,
                result,
            });
        });
    }

    #[cfg(test)]
    pub(crate) fn session_cost_refresh_idle_for_test(&self, session_id: SessionId) -> bool {
        self.cost_refreshes
            .get(&session_id)
            .is_some_and(|refresh| !refresh.scheduled && !refresh.in_flight && !refresh.dirty)
    }

    #[cfg(test)]
    pub(crate) fn session_cost_request_id_for_test(&self, session_id: SessionId) -> Option<u64> {
        self.cost_refreshes
            .get(&session_id)
            .filter(|refresh| refresh.in_flight)
            .map(|refresh| refresh.request_id)
    }

    async fn handle_mcp_key(&mut self, key: KeyEvent) {
        if self.mcp_panel.form.is_some() {
            if key.code == KeyCode::Esc {
                self.mcp_panel.form = None;
                return;
            }
            if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                self.mcp_panel
                    .form
                    .as_mut()
                    .expect("form")
                    .move_focus(key.code == KeyCode::BackTab);
                return;
            }
            if matches!(
                key.code,
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
            ) && self.mcp_panel.form.as_ref().is_some_and(|form| {
                matches!(
                    form.focus,
                    McpFormFocus::Transport
                        | McpFormFocus::Enabled
                        | McpFormFocus::Lazy
                        | McpFormFocus::Persist
                )
            }) {
                self.mcp_panel
                    .form
                    .as_mut()
                    .expect("form")
                    .cycle_choice(key.code == KeyCode::Left);
                return;
            }
            if key.code == KeyCode::Enter {
                self.submit_mcp_form();
                return;
            }
            if let Some(input) = self
                .mcp_panel
                .form
                .as_mut()
                .and_then(McpForm::focused_input)
            {
                edit_plain_input(input, key);
            }
            return;
        }
        if let Some(auth) = self.mcp_panel.auth.as_ref() {
            match key.code {
                KeyCode::Esc => self.dispatch_mcp_auth_cancel(auth.server.clone()),
                KeyCode::Char('c') => {
                    self.copy_to_clipboard(auth.authorization_url.clone());
                }
                _ => {}
            }
            return;
        }
        let count = self.mcp_panel.servers.len();
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Up => move_picker_selection(&mut self.mcp_panel.selection, count, true),
            KeyCode::Down => move_picker_selection(&mut self.mcp_panel.selection, count, false),
            KeyCode::Char('n') => self.mcp_panel.form = Some(McpForm::add()),
            KeyCode::Char('e') => {
                if let Some(server) = self.mcp_panel.selected().cloned() {
                    self.mcp_panel.form = Some(McpForm::edit(&server));
                }
            }
            KeyCode::Char('d') => {
                if let Some(name) = self.mcp_panel.selected().map(|server| server.name.clone()) {
                    self.dispatch_mcp_remove(name);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(server) = self.mcp_panel.selected().cloned() {
                    self.dispatch_mcp_toggle(server.name, !server.definition.enabled);
                }
            }
            KeyCode::Char('r') => {
                if let Some(name) = self.mcp_panel.selected().map(|server| server.name.clone()) {
                    self.dispatch_mcp_reconnect(name);
                }
            }
            KeyCode::Char('a') => {
                if let Some(server) = self.mcp_panel.selected().cloned()
                    && server.state == McpServerState::NeedsAuth
                {
                    self.dispatch_mcp_auth_begin(server.name);
                }
            }
            _ => {}
        }
    }

    fn submit_mcp_form(&mut self) {
        let Some(form) = self.mcp_panel.form.take() else {
            return;
        };
        let persist = form.persist.target();
        let editing = form.editing;
        let original = form.original_name.clone();
        let (name, definition) = match form.definition() {
            Ok(value) => value,
            Err(error) => {
                self.status = error;
                self.mcp_panel.form = Some(form);
                return;
            }
        };
        if editing && original.as_deref() != Some(name.as_str()) {
            self.status = "editing cannot rename a server; remove it and add the new name".into();
            self.mcp_panel.form = Some(form);
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = async {
                let mutation = if editing {
                    client
                        .edit_mcp_server(McpServerEditParams {
                            name: name.clone(),
                            definition,
                        })
                        .await
                } else {
                    client
                        .add_mcp_server(McpServerAddParams {
                            name: name.clone(),
                            definition,
                        })
                        .await
                }
                .map_err(|error| error.to_string())?;
                if let Some(target) = persist {
                    return client
                        .persist_mcp_server(McpServerPersistParams { name, target })
                        .await
                        .map(|result| result.server)
                        .map_err(|error| error.to_string());
                }
                Ok(mutation.server)
            }
            .await;
            let _ = updates.send(RpcUpdate::McpMutation {
                result: Box::new(result),
            });
        });
    }

    fn dispatch_mcp_remove(&self, name: String) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .remove_mcp_server(McpServerNameParams { name })
                .await
                .map(|result| result.server)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpMutation {
                result: Box::new(result),
            });
        });
    }

    fn dispatch_mcp_toggle(&self, name: String, enabled: bool) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .set_mcp_server_enabled(McpServerSetEnabledParams { name, enabled })
                .await
                .map(|result| result.server)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpMutation {
                result: Box::new(result),
            });
        });
    }

    fn dispatch_mcp_reconnect(&self, name: String) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .reconnect_mcp_server(McpServerNameParams { name })
                .await
                .map(|result| result.server)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpMutation {
                result: Box::new(result),
            });
        });
    }

    fn dispatch_mcp_auth_begin(&self, server: String) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .begin_mcp_auth(McpAuthBeginParams { server })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpAuthBegan { result });
        });
    }

    fn dispatch_mcp_auth_cancel(&self, server: String) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .cancel_mcp_auth(McpAuthCancelParams {
                    server: server.clone(),
                })
                .await
                .map(|_| server)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::McpAuthCancelled { result });
        });
    }

    async fn handle_permissions_key(&mut self, key: KeyEvent) {
        if let Some(form) = &mut self.permission_panel.form {
            match key.code {
                KeyCode::Esc => self.permission_panel.form = None,
                KeyCode::Tab | KeyCode::BackTab => form.focus_pattern = !form.focus_pattern,
                KeyCode::Up if !form.focus_pattern => form.cycle_action(true),
                KeyCode::Down if !form.focus_pattern => form.cycle_action(false),
                KeyCode::Left if !form.focus_pattern => {
                    form.effect = cycle_effect(form.effect, true)
                }
                KeyCode::Right | KeyCode::Char(' ') if !form.focus_pattern => {
                    form.effect = cycle_effect(form.effect, false)
                }
                KeyCode::Enter => self.submit_permission_form(),
                _ if form.focus_pattern => edit_plain_input(&mut form.pattern, key),
                _ => {}
            }
            return;
        }
        let rows = self.permission_panel.rows();
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Up => {
                move_picker_selection(&mut self.permission_panel.selection, rows.len(), true)
            }
            KeyCode::Down => {
                move_picker_selection(&mut self.permission_panel.selection, rows.len(), false)
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                if let Some(row) = self.permission_panel.selected() {
                    let effect = cycle_effect(row.effect, key.code == KeyCode::Left);
                    self.dispatch_permission_set(row.action, row.resource, effect);
                }
            }
            KeyCode::Char('n') => {
                let action = self
                    .permission_panel
                    .selected()
                    .map_or(PermissionAction::Read, |row| row.action);
                self.permission_panel.form = Some(PermissionForm::new(action));
            }
            KeyCode::Char('d') => {
                if let Some(row) = self.permission_panel.selected() {
                    if row.source == PermissionRuleSource::SessionOverlay {
                        self.dispatch_permission_clear(row.action, row.resource);
                    } else {
                        self.status = "only session overlay rules can be cleared".into();
                    }
                }
            }
            _ => {}
        }
    }

    fn submit_permission_form(&mut self) {
        let Some(form) = self.permission_panel.form.take() else {
            return;
        };
        let pattern = form.pattern.as_str().trim();
        let resource = match cookie_agent_protocol::WildcardPattern::new(pattern) {
            Ok(resource) => resource.to_string(),
            Err(error) => {
                self.status = format!("invalid permission pattern: {error}");
                self.permission_panel.form = Some(form);
                return;
            }
        };
        self.dispatch_permission_set(form.action, resource, form.effect);
    }

    fn dispatch_permission_set(
        &self,
        action: PermissionAction,
        resource: String,
        effect: PermissionEffect,
    ) {
        let Some(session_id) = self.selected else {
            return;
        };
        let Ok(resource) = cookie_agent_protocol::WildcardPattern::new(resource) else {
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .set_session_permission(SessionPermissionSetParams {
                    session_id,
                    action,
                    resource,
                    effect,
                })
                .await
                .map(|result| SessionPermissionGetResult {
                    permissions: result.permissions,
                    current_mode: None,
                })
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PermissionsLoaded { session_id, result });
        });
    }

    fn dispatch_permission_clear(&self, action: PermissionAction, resource: String) {
        let Some(session_id) = self.selected else {
            return;
        };
        let Ok(resource) = cookie_agent_protocol::WildcardPattern::new(resource) else {
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .clear_session_permission(SessionPermissionClearParams {
                    session_id,
                    action,
                    resource,
                })
                .await
                .map(|result| SessionPermissionGetResult {
                    permissions: result.permissions,
                    current_mode: None,
                })
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PermissionsLoaded { session_id, result });
        });
    }

    pub(super) async fn handle_key(&mut self, key: KeyEvent) {
        if key.code != KeyCode::Esc {
            self.last_escape = None;
        }
        match self.modal {
            Modal::Sessions => self.handle_session_picker(key).await,
            Modal::Presets => self.handle_selection_picker(key).await,
            Modal::Agents => self.handle_agent_picker_key(key).await,
            Modal::Models => self.handle_model_picker_key(key).await,
            Modal::ConnectProviders => self.handle_connect_provider_key(key),
            Modal::ConnectDetails => self.handle_connect_details_key(key),
            Modal::ConnectSetup => self.handle_connect_setup_key(key),
            Modal::ConnectError => self.handle_connect_error_key(key),
            Modal::DisconnectConfirm => self.handle_disconnect_confirm_key(key),
            Modal::UserMessage => self.handle_user_menu_key(key).await,
            Modal::RevertConfirm => self.handle_revert_confirm_key(key).await,
            Modal::Mcp => self.handle_mcp_key(key).await,
            Modal::Permissions => self.handle_permissions_key(key).await,
            Modal::Skills => match key.code {
                KeyCode::Esc => self.modal = Modal::None,
                KeyCode::Up => move_picker_selection(
                    &mut self.skill_panel.selection,
                    self.skill_panel
                        .result
                        .as_ref()
                        .map_or(0, |result| result.skills.len()),
                    true,
                ),
                KeyCode::Down => move_picker_selection(
                    &mut self.skill_panel.selection,
                    self.skill_panel
                        .result
                        .as_ref()
                        .map_or(0, |result| result.skills.len()),
                    false,
                ),
                _ => {}
            },
            Modal::Usage => match key.code {
                KeyCode::Esc => self.modal = Modal::None,
                KeyCode::Up => self.usage_panel.scroll_up(1),
                KeyCode::Down => self.usage_panel.scroll_down(1),
                KeyCode::PageUp => self.usage_panel.page_up(),
                KeyCode::PageDown => self.usage_panel.page_down(),
                _ => {}
            },
            Modal::None
                if self.current_approval().is_none()
                    && !self.command_palette_visible()
                    && agent_cycle_backward(key).is_some() =>
            {
                self.cycle_agent(agent_cycle_backward(key).expect("agent cycle key"));
            }
            Modal::None if self.command_palette_visible() => self.handle_palette_key(key).await,
            Modal::None
                if self.current_approval().is_some() && is_approval_scroll_key(key.code) =>
            {
                self.handle_approval_scroll_key(key.code);
            }
            Modal::None if key.code == KeyCode::Esc && self.current_approval().is_some() => {
                if self
                    .current_approval()
                    .is_some_and(|approval| approval.constraints.cancellable)
                {
                    self.answer_approval(ApprovalUserDecision::Cancel).await;
                }
            }
            Modal::None if key.code == KeyCode::Esc && self.selection.is_some() => {
                // Esc retires a selection before it ever counts toward the
                // double-Esc run cancel.
                self.selection = None;
                self.last_escape = None;
            }
            Modal::None if key.code == KeyCode::Esc => {
                if self.register_escape(Instant::now()) {
                    self.cancel_active_run();
                }
            }
            Modal::None
                if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // With an active selection ctrl+c is copy; without one it
                // keeps its long-standing meaning (cancel the active run).
                if let Some(text) = self.selected_text() {
                    self.selection = None;
                    if text.is_empty() {
                        self.status = "nothing to copy in the selection".into();
                    } else {
                        self.copy_to_clipboard(text);
                    }
                } else {
                    self.cancel_active_run();
                }
            }
            Modal::None
                if key.code == KeyCode::Char('x')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(self.selection, Some(TextSelection::Composer { .. })) =>
            {
                // Composer-only cut: copy the selected draft text, then
                // remove it from the buffer.
                if let Some(text) = self.selected_text()
                    && !text.is_empty()
                {
                    let (start, end) = self
                        .selection
                        .map(|selection| selection.byte_range())
                        .unwrap_or((0, 0));
                    self.selection = None;
                    self.copy_to_clipboard(text);
                    self.input.delete_byte_range(start, end);
                }
            }
            Modal::None => self.handle_input_key(key).await,
        }
    }

    pub(super) fn command_palette_visible(&self) -> bool {
        let command = self.input.as_str().strip_prefix('/').unwrap_or_default();
        self.modal == Modal::None
            && self.current_approval().is_none()
            && self.input_focused
            && self.input.as_str().starts_with('/')
            && !self.input.as_str().starts_with("//")
            && !command.chars().any(char::is_whitespace)
            && !self.palette_dismissed
    }

    fn handle_approval_scroll_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up => self.scroll_approval(true, 1),
            KeyCode::Down => self.scroll_approval(false, 1),
            KeyCode::PageUp => self.scroll_approval(true, 10),
            KeyCode::PageDown => self.scroll_approval(false, 10),
            KeyCode::Home => self.approval_scroll = 0,
            KeyCode::End => self.approval_scroll = self.approval_max_scroll,
            _ => {}
        }
    }

    fn scroll_approval(&mut self, up: bool, lines: u16) {
        self.approval_scroll = if up {
            self.approval_scroll.saturating_sub(lines)
        } else {
            self.approval_scroll
                .saturating_add(lines)
                .min(self.approval_max_scroll)
        };
    }

    pub(super) fn palette_entries(&self) -> Vec<PaletteEntry<'_>> {
        let query = self
            .input
            .as_str()
            .strip_prefix('/')
            .unwrap_or_default()
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        super::slash::entries(self.input.as_str())
            .into_iter()
            .map(PaletteEntry::Command)
            .chain(
                self.skills
                    .iter()
                    .filter(|skill| {
                        skill.precedence_winner
                            && skill.user_invocable
                            && (query.is_empty() || skill.name.contains(&query))
                    })
                    .map(PaletteEntry::Skill),
            )
            .collect()
    }

    fn note_sessions_changed(&mut self) {
        self.sessions_revision = self.sessions_revision.wrapping_add(1);
    }

    fn refresh_session_search_rows_cache(&mut self) {
        let now = jiff::Timestamp::now();
        let time_zone = jiff::tz::TimeZone::system();
        let local_day = now.to_zoned(time_zone.clone()).date();
        let query = self.session_search.query();
        if self.session_search_rows_cache.query == query
            && self.session_search_rows_cache.sessions_revision == self.sessions_revision
            && self.session_search_rows_cache.sessions_len == self.sessions.len()
            && self.session_search_rows_cache.local_day == Some(local_day)
        {
            return;
        }
        self.session_search_rows_cache = SessionSearchRowsCache {
            query: query.to_owned(),
            sessions_revision: self.sessions_revision,
            sessions_len: self.sessions.len(),
            local_day: Some(local_day),
            rows: session_search_rows(&self.sessions, query, now, &time_zone),
        };
    }

    pub(super) fn current_session_search_rows(&mut self) -> &[SessionSearchRow] {
        self.refresh_session_search_rows_cache();
        &self.session_search_rows_cache.rows
    }

    fn session_search_ids(&mut self) -> Vec<SessionId> {
        self.current_session_search_rows()
            .iter()
            .filter_map(SessionSearchRow::session_id)
            .collect()
    }

    /// Providers matching the current picker query by display name or ID.
    pub(super) fn filtered_providers(&self) -> Vec<&ProviderDescriptor> {
        self.providers
            .iter()
            .filter(|provider| provider_matches(provider, self.provider_search.query()))
            .collect()
    }

    pub(super) fn picker_entry_count(&mut self) -> usize {
        match self.modal {
            Modal::Sessions => self.session_search_ids().len(),
            Modal::Presets => self.preset_names().len() + 1,
            Modal::Agents => self.filtered_agent_picker_candidates().len(),
            Modal::Models => self.filtered_draft_models().len(),
            Modal::ConnectProviders => self.filtered_providers().len(),
            Modal::UserMessage => USER_MENU_ITEMS.len(),
            Modal::Mcp | Modal::Permissions | Modal::Skills | Modal::Usage => 0,
            Modal::ConnectDetails
            | Modal::ConnectSetup
            | Modal::ConnectError
            | Modal::DisconnectConfirm
            | Modal::RevertConfirm
            | Modal::None => 0,
        }
    }

    fn clamp_picker_selection(&mut self) {
        let count = self.picker_entry_count();
        if count == 0 {
            self.picker_state.select(None);
        } else {
            self.picker_state.select(Some(
                self.picker_state.selected().unwrap_or(0).min(count - 1),
            ));
        }
    }

    fn session_search_changed(&mut self) {
        self.picker_state.select(Some(0));
        self.clamp_picker_selection();
    }

    fn provider_search_changed(&mut self) {
        self.picker_state.select(Some(0));
        self.clamp_picker_selection();
    }

    fn model_search_changed(&mut self) {
        self.picker_state.select(Some(0));
        self.clamp_picker_selection();
    }

    fn agent_search_changed(&mut self) {
        self.picker_state.select(Some(0));
        self.clamp_picker_selection();
    }

    fn close_agent_picker(&mut self) {
        self.agent_search.reset();
        self.modal = Modal::None;
        self.new_session_draft = None;
    }

    fn close_model_picker(&mut self) {
        self.model_search.reset();
        self.modal = Modal::None;
        self.new_session_draft = None;
    }

    fn close_provider_picker(&mut self) {
        self.clear_connect_secrets();
        self.provider_search.reset();
        self.modal = Modal::None;
        self.status = "Provider connection cancelled.".into();
    }

    pub(super) fn register_escape(&mut self, now: Instant) -> bool {
        const ESC_CANCEL_WINDOW: Duration = Duration::from_millis(500);
        let cancel = self
            .last_escape
            .and_then(|previous| now.checked_duration_since(previous))
            .is_some_and(|elapsed| elapsed <= ESC_CANCEL_WINDOW);
        self.last_escape = (!cancel).then_some(now);
        cancel
    }

    pub(super) async fn handle_palette_key(&mut self, key: KeyEvent) {
        if is_newline_key(key) {
            self.mutate_input(|input| input.insert_newline());
            self.clamp_palette_selection();
            return;
        }
        let entry_count = self.palette_entries().len();
        match key.code {
            KeyCode::Esc => {
                self.palette_dismissed = true;
                self.last_escape = None;
            }
            KeyCode::Up => move_selection(&mut self.palette_state, entry_count, true),
            KeyCode::Down => move_selection(&mut self.palette_state, entry_count, false),
            KeyCode::Enter if self.palette_entries().is_empty() => self.submit_input().await,
            KeyCode::Enter => {
                self.activate_palette_entry(self.palette_state.selected().unwrap_or(0))
                    .await
            }
            _ => {
                self.handle_input_key(key).await;
                self.clamp_palette_selection();
            }
        }
    }

    pub(super) fn clamp_palette_selection(&mut self) {
        let entries = self.palette_entries();
        self.palette_state.select((!entries.is_empty()).then(|| {
            self.palette_state
                .selected()
                .unwrap_or(0)
                .min(entries.len() - 1)
        }));
    }

    pub(super) async fn activate_palette_entry(&mut self, index: usize) {
        let Some(entry) = self.palette_entries().get(index).copied() else {
            return;
        };
        let (usage, requires_arguments) = match entry {
            PaletteEntry::Command(spec) => (spec.usage.to_owned(), spec.requires_arguments),
            PaletteEntry::Skill(skill) => (format!("/{}", skill.name), true),
        };
        self.palette_dismissed = true;
        self.palette_state.select(Some(0));
        self.mutate_input(|input| {
            input.set_buffer(if requires_arguments {
                format!(
                    "{} ",
                    usage.split_whitespace().next().expect("command usage")
                )
            } else {
                usage
            });
        });
        if !requires_arguments {
            self.submit_input().await;
        }
    }

    /// Handle one mouse event. Returns whether a redraw is needed: every
    /// button/wheel event redraws, while pointer motion redraws only when it
    /// actually changed the hovered element.
    pub(super) async fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_press(mouse.column, mouse.row).await;
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.handle_pointer_drag(mouse.column, mouse.row);
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.handle_release().await;
                true
            }
            MouseEventKind::ScrollUp => {
                self.handle_wheel(mouse.column, mouse.row, true);
                true
            }
            MouseEventKind::ScrollDown => {
                self.handle_wheel(mouse.column, mouse.row, false);
                true
            }
            MouseEventKind::Moved => self.update_hover(mouse.column, mouse.row),
            _ => false,
        }
    }

    /// Left-button press: a plain click anywhere clears a finished
    /// selection. Presses inside the conversation viewport or the composer
    /// text rect (with no overlay owning the pointer) are held as pending
    /// presses until motion decides click-or-drag; every other press is an
    /// immediate click, preserving scrollbar capture and panel behavior.
    async fn handle_press(&mut self, column: u16, row: u16) {
        self.selection = None;
        self.pending_press = None;
        // Overlay ownership comes from state, not hit geometry: a modal or
        // approval that opened since the last frame still owns its presses.
        let overlay = self.command_palette_visible()
            || self.modal != Modal::None
            || self.current_approval().is_some();
        if !overlay {
            if self
                .hit_map
                .conversation
                .is_some_and(|viewport| contains(viewport, column, row))
            {
                self.pending_press = Some(PendingPress {
                    column,
                    row,
                    target: PressTarget::Conversation,
                });
                return;
            }
            if let Some(hit) = self
                .hit_map
                .input
                .filter(|hit| contains(hit.rect, column, row))
            {
                // The composer's scrollbar column keeps its immediate
                // behavior (page/thumb capture); only text cells can start
                // a selection.
                let on_scrollbar = hit
                    .scrollbar
                    .is_some_and(|geometry| contains(geometry.track, column, row));
                if !on_scrollbar && contains(hit.text_rect, column, row) {
                    self.input_focused = true;
                    self.pending_press = Some(PendingPress {
                        column,
                        row,
                        target: PressTarget::Composer,
                    });
                    return;
                }
            }
        }
        self.handle_click(column, row).await;
    }

    /// Pointer motion with the button held: scrollbar drags stay scrollbar
    /// drags; a pending press moved past the threshold becomes a selection
    /// whose head follows the pointer.
    fn handle_pointer_drag(&mut self, column: u16, row: u16) {
        if self.scrollbar_drag.is_some() {
            self.handle_drag(column, row);
            return;
        }
        let Some(press) = self.pending_press else {
            return;
        };
        match press.target {
            PressTarget::Conversation => {
                let Some(viewport) = self.hit_map.conversation else {
                    self.pending_press = None;
                    return;
                };
                let point = self.conversation_point(viewport, column, row);
                if let Some(TextSelection::Conversation { head, .. }) = &mut self.selection {
                    *head = point;
                    return;
                }
                if column.abs_diff(press.column) > DRAG_THRESHOLD_CELLS
                    || row.abs_diff(press.row) > DRAG_THRESHOLD_CELLS
                {
                    let anchor = self.conversation_point(viewport, press.column, press.row);
                    self.selection = Some(TextSelection::Conversation {
                        anchor,
                        head: point,
                    });
                }
            }
            PressTarget::Composer => {
                let Some(hit) = self.hit_map.input else {
                    self.pending_press = None;
                    return;
                };
                let point = self.composer_point(hit, column, row);
                if let Some(TextSelection::Composer { head, .. }) = &mut self.selection {
                    *head = point;
                    return;
                }
                if column.abs_diff(press.column) > DRAG_THRESHOLD_CELLS
                    || row.abs_diff(press.row) > DRAG_THRESHOLD_CELLS
                {
                    let anchor = self.composer_point(hit, press.column, press.row);
                    self.selection = Some(TextSelection::Composer {
                        anchor,
                        head: point,
                    });
                }
            }
        }
    }

    /// Button release: a finished drag keeps its selection; a pending press
    /// that never became a drag dispatches its click at the press position.
    async fn handle_release(&mut self) {
        self.scrollbar_drag = None;
        if self.selection.is_some() {
            self.pending_press = None;
            return;
        }
        let Some(press) = self.pending_press.take() else {
            return;
        };
        self.handle_click(press.column, press.row).await;
    }

    /// A conversation cell in content coordinates: `(logical line, display
    /// column)` inside the rendered lines, so the selection survives
    /// scrolling while it is held.
    fn conversation_point(&self, viewport: Rect, column: u16, row: u16) -> (usize, u16) {
        let line = self
            .conversation_scroll
            .offset
            .saturating_add(usize::from(row.saturating_sub(viewport.y)));
        let column = column.saturating_sub(viewport.x).min(viewport.width);
        (line, column)
    }

    /// A composer cell as the nearest draft byte offset.
    fn composer_point(&self, hit: InputHit, column: u16, row: u16) -> usize {
        self.input.byte_at_display_position(
            row.saturating_sub(hit.text_rect.y)
                .min(hit.text_rect.height.saturating_sub(1)),
            column
                .saturating_sub(hit.text_rect.x)
                .min(hit.text_rect.width),
        )
    }

    /// Recompute the hovered element from the current hit map. Returns true
    /// when the hover target changed (the only case needing a redraw). A
    /// captured scrollbar drag freezes hover until the press is released.
    fn update_hover(&mut self, column: u16, row: u16) -> bool {
        if self.scrollbar_drag.is_some() {
            return false;
        }
        let next = self.hover_target_at(column, row);
        if next == self.hover {
            return false;
        }
        self.hover = next;
        true
    }

    /// Resolve the interactive element under a point using the exact same
    /// state-owned overlay priority as click handling: an open overlay owns
    /// the pointer even before its geometry renders.
    pub(super) fn hover_target_at(&self, column: u16, row: u16) -> Option<HoverTarget> {
        let over = |rect: Rect| contains(rect, column, row);
        if self.command_palette_visible() {
            return self
                .hit_map
                .palette_rows
                .iter()
                .find(|hit| over(hit.rect))
                .map(|hit| HoverTarget::PaletteRow(hit.index));
        }
        if self.modal != Modal::None {
            if self.hit_map.provider_submit.is_some_and(over) {
                return Some(HoverTarget::ProviderSubmit);
            }
            if self.hit_map.provider_cancel.is_some_and(over) {
                return Some(HoverTarget::ProviderCancel);
            }
            if let Some(hit) = self
                .hit_map
                .provider_fields
                .iter()
                .find(|hit| over(hit.rect))
            {
                return Some(HoverTarget::ProviderField(hit.focus));
            }
            return self
                .hit_map
                .picker_rows
                .iter()
                .find(|hit| over(hit.rect))
                .map(|hit| HoverTarget::PickerRow(hit.index));
        }
        if self.current_approval().is_some() {
            return self
                .hit_map
                .approval_actions
                .iter()
                .find(|hit| over(hit.rect))
                .map(|hit| HoverTarget::ApprovalAction(hit.decision));
        }
        if self.hit_map.permission_mode.is_some_and(over) {
            return Some(HoverTarget::PermissionMode);
        }
        if self.hit_map.session_cost.is_some_and(over) {
            return Some(HoverTarget::SessionCost);
        }
        if self.hit_map.event_level_filter.is_some_and(over) {
            return Some(HoverTarget::EventLevelFilter);
        }
        if let Some(hit) = self
            .hit_map
            .title_segments
            .iter()
            .find(|hit| over(hit.rect))
        {
            return Some(HoverTarget::TitleSegment(hit.segment));
        }
        if let Some(hit) = self.hit_map.queue_entries.iter().find(|hit| over(hit.rect)) {
            return Some(HoverTarget::QueueEntry(hit.index));
        }
        if let Some(hit) = self.hit_map.tree_rows.iter().find(|hit| over(hit.rect)) {
            return Some(HoverTarget::TreeRow(hit.session_id));
        }
        None
    }

    /// Patch the hover affordance onto the resolved target's cells. Text
    /// targets get the glaze text style; approval buttons get the fill-only
    /// variant so their glyphs stay put.
    /// Patch the mouse text selection over the already-rendered cells,
    /// exactly like hover: a pure style pass that can never change layout.
    /// The selection background leaves cell foregrounds (code highlighting)
    /// intact, and keyboard selection keeps its own distinct style.
    fn apply_selection(&self, frame: &mut ratatui::Frame) {
        let Some(selection) = self.selection else {
            return;
        };
        let style = self.theme.text_selection();
        match selection {
            TextSelection::Conversation { .. } => {
                let (start, end) = selection.ordered();
                let Some(viewport) = self.hit_map.conversation else {
                    return;
                };
                let offset = self.conversation_scroll.offset;
                for line in start.0..=end.0 {
                    let Some(row_in_view) = line.checked_sub(offset) else {
                        continue;
                    };
                    if row_in_view >= usize::from(viewport.height) {
                        break;
                    }
                    let y = viewport.y.saturating_add(row_in_view as u16);
                    let column_start = if line == start.0 { start.1 } else { 0 };
                    let column_end = if line == end.0 { end.1 } else { viewport.width };
                    let column_start = column_start.min(viewport.width);
                    let column_end = column_end.min(viewport.width);
                    for x in column_start..column_end {
                        let cell = &mut frame.buffer_mut()[(viewport.x.saturating_add(x), y)];
                        cell.set_style(style);
                    }
                }
            }
            TextSelection::Composer { .. } => {
                let (start, end) = selection.byte_range();
                let Some(hit) = self.hit_map.input else {
                    return;
                };
                for (row, column_start, column_end) in self.input.selection_cells(start, end) {
                    if row >= hit.text_rect.height {
                        continue;
                    }
                    let y = hit.text_rect.y.saturating_add(row);
                    let column_end = column_end.min(hit.text_rect.width);
                    for x in column_start..column_end {
                        let cell = &mut frame.buffer_mut()[(hit.text_rect.x.saturating_add(x), y)];
                        cell.set_style(style);
                    }
                }
            }
        }
    }

    fn apply_hover(&self, frame: &mut ratatui::Frame) {
        let Some(hover) = self.hover else {
            return;
        };
        let text_style = self.theme.hover();
        let fill_style = self.theme.hover_fill();
        let patch = |frame: &mut ratatui::Frame, rect: Rect, style: ratatui::style::Style| {
            for y in rect.y..rect.y.saturating_add(rect.height) {
                for x in rect.x..rect.x.saturating_add(rect.width) {
                    if frame.area().contains(Position::new(x, y)) {
                        let cell = &mut frame.buffer_mut()[(x, y)];
                        cell.set_style(style);
                    }
                }
            }
        };
        match hover {
            HoverTarget::PaletteRow(index) => {
                if let Some(hit) = self
                    .hit_map
                    .palette_rows
                    .iter()
                    .find(|hit| hit.index == index)
                {
                    patch(frame, hit.rect, text_style);
                }
            }
            HoverTarget::PickerRow(index) => {
                if let Some(hit) = self
                    .hit_map
                    .picker_rows
                    .iter()
                    .find(|hit| hit.index == index)
                {
                    patch(frame, hit.rect, text_style);
                }
            }
            HoverTarget::ProviderField(focus) => {
                if let Some(hit) = self
                    .hit_map
                    .provider_fields
                    .iter()
                    .find(|hit| hit.focus == focus)
                {
                    patch(frame, hit.text_rect, fill_style);
                }
            }
            HoverTarget::ProviderSubmit => {
                if let Some(rect) = self.hit_map.provider_submit {
                    patch(frame, rect, fill_style);
                }
            }
            HoverTarget::ProviderCancel => {
                if let Some(rect) = self.hit_map.provider_cancel {
                    patch(frame, rect, fill_style);
                }
            }
            HoverTarget::ApprovalAction(decision) => {
                if let Some(hit) = self
                    .hit_map
                    .approval_actions
                    .iter()
                    .find(|hit| hit.decision == decision)
                {
                    patch(frame, hit.rect, fill_style);
                }
            }
            HoverTarget::TitleSegment(segment) => {
                if let Some(hit) = self
                    .hit_map
                    .title_segments
                    .iter()
                    .find(|hit| hit.segment == segment)
                {
                    patch(frame, hit.rect, text_style);
                }
            }
            HoverTarget::PermissionMode => {
                if let Some(rect) = self.hit_map.permission_mode {
                    patch(frame, rect, text_style);
                }
            }
            HoverTarget::SessionCost => {
                if let Some(rect) = self.hit_map.session_cost {
                    patch(frame, rect, text_style);
                }
            }
            HoverTarget::EventLevelFilter => {
                if let Some(rect) = self.hit_map.event_level_filter {
                    patch(frame, rect, text_style);
                }
            }
            HoverTarget::QueueEntry(index) => {
                if let Some(hit) = self
                    .hit_map
                    .queue_entries
                    .iter()
                    .find(|hit| hit.index == index)
                {
                    patch(frame, hit.rect, text_style);
                }
            }
            HoverTarget::TreeRow(session_id) => {
                if let Some(hit) = self
                    .hit_map
                    .tree_rows
                    .iter()
                    .find(|hit| hit.session_id == session_id)
                {
                    patch(frame, hit.rect, text_style);
                }
            }
        }
    }

    /// The animation bucket (0–3) for the streaming "thinking…" ellipsis:
    /// one step per twelve 33ms frames ≈ 400ms.
    pub(super) fn clock_bucket(&self) -> u8 {
        u8::try_from((self.animation_ticks / 12) % 4).unwrap_or(0)
    }

    /// Animation frames run only while the visible session has live content
    /// — streaming thinking or a running tool; everything else is
    /// event-driven.
    pub(super) fn animation_active(&self) -> bool {
        self.selected
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .is_some_and(|state| {
                crate::state::SessionState::has_open_thinking(state)
                    || crate::state::SessionState::has_running_tool(state)
            })
    }

    pub(super) fn animation_tick(&mut self) {
        self.animation_ticks = self.animation_ticks.wrapping_add(1);
    }

    /// A captured scrollbar thumb drag keeps its grab anchor and resolves
    /// against the original geometry even when the pointer leaves the track.
    fn handle_drag(&mut self, column: u16, row: u16) {
        let Some(drag) = self.scrollbar_drag else {
            return;
        };
        let _ = column;
        match drag.target {
            ScrollbarTarget::Conversation => {
                let Some(geometry) = self.scrollbar_geometry else {
                    self.scrollbar_drag = None;
                    return;
                };
                let offset = geometry.offset_for_thumb_anchor(row, drag.grab_row);
                self.conversation_scroll
                    .scroll_to(geometry.clamp_offset(offset));
            }
            ScrollbarTarget::Input => {
                let Some(geometry) = self.hit_map.input.and_then(|hit| hit.scrollbar) else {
                    self.scrollbar_drag = None;
                    return;
                };
                let offset = geometry.offset_for_thumb_anchor(row, drag.grab_row);
                self.input.scroll_to(geometry.clamp_offset(offset));
            }
        }
    }

    pub(super) async fn handle_click(&mut self, column: u16, row: u16) {
        // Overlay ownership comes from current state, never from hit-map
        // geometry: an overlay that opened since the last frame still owns
        // its presses (its geometry may not exist yet), and one that just
        // closed leaves content geometry underneath intact and clickable.
        // Each overlay branch therefore always returns — geometry only
        // picks the element within the panel, never whether the panel owns
        // the click.
        if self.command_palette_visible() {
            if let Some(hit) = self
                .hit_map
                .palette_rows
                .iter()
                .find(|hit| contains(hit.rect, column, row))
                .copied()
            {
                self.activate_palette_entry(hit.index).await;
            }
            return;
        }
        if self.modal != Modal::None {
            if let Some(hit) = self
                .hit_map
                .picker_input
                .filter(|hit| contains(hit.rect, column, row))
            {
                let search = match self.modal {
                    Modal::Sessions => &mut self.session_search,
                    Modal::Agents => &mut self.agent_search,
                    Modal::Models => &mut self.model_search,
                    Modal::ConnectProviders => &mut self.provider_search,
                    _ => return,
                };
                search.focus_input();
                search.input_mut().set_cursor_from_display_position(
                    row.saturating_sub(hit.text_rect.y)
                        .min(hit.text_rect.height.saturating_sub(1)),
                    column
                        .saturating_sub(hit.text_rect.x)
                        .min(hit.text_rect.width),
                );
                return;
            }
            if let Some(hit) = self
                .hit_map
                .picker_rows
                .iter()
                .find(|hit| contains(hit.rect, column, row))
                .copied()
            {
                self.choose_picker_entry(hit.index).await;
            }
            if self
                .hit_map
                .provider_submit
                .is_some_and(|rect| contains(rect, column, row))
            {
                self.dispatch_provider_connect();
                return;
            }
            if self
                .hit_map
                .provider_cancel
                .is_some_and(|rect| contains(rect, column, row))
            {
                self.cancel_connect_form();
                return;
            }
            if let Some(hit) = self
                .hit_map
                .provider_fields
                .iter()
                .find(|hit| contains(hit.rect, column, row))
                .copied()
            {
                if hit.focus == ProviderFormFocus::AuthMethod {
                    // Clicking the selector mirrors pressing Enter on it:
                    // cycle to the next method, wiping its stale secrets.
                    if let Some(form) = self.provider_form.as_mut() {
                        form.cycle_auth_method(false);
                    }
                } else {
                    self.focus_provider_field(hit, column, row);
                }
                return;
            }
            return;
        }
        if self.current_approval().is_some() {
            if let Some(hit) = self
                .hit_map
                .approval_actions
                .iter()
                .find(|hit| contains(hit.rect, column, row))
                .copied()
            {
                self.answer_approval(hit.decision).await;
            }
            return;
        }
        if self
            .hit_map
            .permission_mode
            .is_some_and(|rect| contains(rect, column, row))
        {
            self.cycle_permission_mode();
            return;
        }
        if self
            .hit_map
            .session_cost
            .is_some_and(|rect| contains(rect, column, row))
        {
            self.modal = Modal::Usage;
            self.load_usage();
            return;
        }
        if self
            .hit_map
            .event_level_filter
            .is_some_and(|rect| contains(rect, column, row))
        {
            self.cycle_event_level_filter();
            return;
        }
        // Agent and model title segments open selectors. The bracketed variant
        // suffix cycles in place and never opens a separate panel.
        if let Some(hit) = self
            .hit_map
            .title_segments
            .iter()
            .find(|hit| contains(hit.rect, column, row))
            .copied()
        {
            self.input_focused = false;
            match hit.segment {
                TitleSegment::Agent => self.open_selection_modal(Modal::Agents),
                TitleSegment::Model => self.open_selection_modal(Modal::Models),
                TitleSegment::Variant => self.cycle_draft_variant(),
            }
            return;
        }
        if let Some(hit) = self
            .hit_map
            .input
            .filter(|hit| contains(hit.rect, column, row))
        {
            self.input_focused = true;
            // The composer's reserved scrollbar column mirrors the
            // conversation's exactly: a press on the thumb captures a drag,
            // a press on the bare track pages to the matching offset — all
            // without moving the text cursor.
            if let Some(geometry) = hit
                .scrollbar
                .filter(|geometry| contains(geometry.track, column, row))
            {
                if contains(geometry.thumb, column, row) {
                    self.scrollbar_drag = Some(ScrollbarDrag {
                        grab_row: row.saturating_sub(geometry.thumb.y),
                        target: ScrollbarTarget::Input,
                    });
                } else {
                    let offset = geometry.clamp_offset(geometry.offset_for_track_row(row));
                    self.input.scroll_to(offset);
                }
                return;
            }
            self.input.set_cursor_from_display_position(
                row.saturating_sub(hit.text_rect.y)
                    .min(hit.text_rect.height.saturating_sub(1)),
                column
                    .saturating_sub(hit.text_rect.x)
                    .min(hit.text_rect.width),
            );
            return;
        }
        // Queue-strip entries are the recall affordance: any row click
        // withdraws the engine's newest pending input back into the
        // composer, which also takes focus for the edit.
        if self
            .hit_map
            .queue_entries
            .iter()
            .any(|hit| contains(hit.rect, column, row))
        {
            self.input_focused = true;
            self.recall_newest_pending();
            return;
        }
        // The scrollbar column is reserved from content and block hit regions;
        // presses there page (track) or capture the thumb for dragging.
        if let Some(track) = self
            .hit_map
            .scrollbar
            .filter(|track| contains(*track, column, row))
            && let Some(geometry) = self.scrollbar_geometry
        {
            self.input_focused = false;
            if contains(geometry.thumb, column, row) {
                self.scrollbar_drag = Some(ScrollbarDrag {
                    grab_row: row.saturating_sub(geometry.thumb.y),
                    target: ScrollbarTarget::Conversation,
                });
            } else {
                let offset = geometry.clamp_offset(geometry.offset_for_track_row(row));
                self.conversation_scroll.scroll_to(offset);
            }
            let _ = track;
            return;
        }
        self.input_focused = false;
        // Past user-message rows open the copy/revert/fork menu — never
        // assistant/tool rows, which keep their expand/collapse toggle.
        if let Some(hit) = self
            .hit_map
            .user_messages
            .iter()
            .find(|hit| contains(hit.rect, column, row))
            .copied()
        {
            self.open_user_menu(hit);
            return;
        }
        if let Some(hit) = self
            .hit_map
            .blocks
            .iter()
            .find(|hit| contains(hit.rect, column, row))
            .copied()
        {
            self.toggle_block(hit.id);
            return;
        }
        if let Some(hit) = self
            .hit_map
            .tree_rows
            .iter()
            .find(|hit| contains(hit.rect, column, row))
            .copied()
        {
            if hit
                .expand_rect
                .is_some_and(|rect| contains(rect, column, row))
            {
                self.toggle_tree_session(hit.session_id);
            } else {
                self.tree_cursor = Some(hit.session_id);
                // Watching a descendant changes the conversation/highlight
                // only; the tree root snapshot is retained.
                self.watch_session(hit.session_id);
            }
        }
    }

    fn focus_provider_field(&mut self, hit: ProviderFieldHit, column: u16, row: u16) {
        let Some(form) = self.provider_form.as_mut() else {
            return;
        };
        form.set_focus(hit.focus);
        let editor = match hit.focus {
            ProviderFormFocus::Credential(index) => {
                form.secrets.get_mut(index).map(|field| &mut field.input)
            }
            ProviderFormFocus::Setup(index) => {
                form.setup.get_mut(index).map(|field| &mut field.input)
            }
            ProviderFormFocus::AuthMethod
            | ProviderFormFocus::Submit
            | ProviderFormFocus::Cancel => None,
        };
        if let Some(editor) = editor {
            editor.state_mut().set_cursor_from_display_position(
                row.saturating_sub(hit.text_rect.y)
                    .min(hit.text_rect.height.saturating_sub(1)),
                column
                    .saturating_sub(hit.text_rect.x)
                    .min(hit.text_rect.width),
            );
        }
    }

    pub(super) fn handle_wheel(&mut self, column: u16, row: u16, up: bool) {
        // Wheel ownership comes from current state, never stale geometry:
        // during an overlay transition the hit map can describe a panel
        // that is already gone (or miss one that just opened), so a rect
        // only targets the scroll *within* a surface state says is open.
        // Ownership order matches the render stacking and click/hover
        // routing exactly — palette, modal, approval, content — so a wheel
        // over overlapping panels reaches the topmost one, never the panel
        // it obscures.
        if self.command_palette_visible()
            && self
                .hit_map
                .palette
                .is_some_and(|rect| contains(rect, column, row))
        {
            let count = self.palette_entries().len();
            move_selection(&mut self.palette_state, count, up);
            return;
        }
        if self.modal != Modal::None {
            if self.modal == Modal::Usage {
                if up {
                    self.usage_panel.scroll_up(3);
                } else {
                    self.usage_panel.scroll_down(3);
                }
                return;
            }
            if self
                .hit_map
                .picker
                .is_some_and(|rect| contains(rect, column, row))
            {
                let len = match self.modal {
                    Modal::Sessions => {
                        self.session_search.focus_list();
                        self.session_search_ids().len()
                    }
                    Modal::Models => {
                        self.model_search.focus_list();
                        self.picker_entry_count()
                    }
                    Modal::Agents => {
                        self.agent_search.focus_list();
                        self.picker_entry_count()
                    }
                    Modal::Presets => self.picker_entry_count(),
                    Modal::ConnectProviders => self.filtered_providers().len(),
                    Modal::UserMessage => USER_MENU_ITEMS.len(),
                    Modal::ConnectDetails
                    | Modal::ConnectSetup
                    | Modal::ConnectError
                    | Modal::DisconnectConfirm
                    | Modal::RevertConfirm
                    | Modal::Mcp
                    | Modal::Permissions
                    | Modal::Skills
                    | Modal::Usage
                    | Modal::None => 0,
                };
                move_picker_selection(&mut self.picker_state, len, up);
            }
            return;
        }
        // A visible approval panel swallows the wheel wherever it lands,
        // owned by state so the panel claims the gesture even before its
        // geometry renders; its rect only targets the panel scroll.
        if self.current_approval().is_some() {
            if self
                .hit_map
                .approval
                .is_some_and(|rect| contains(rect, column, row))
            {
                self.scroll_approval(up, 3);
            }
            return;
        }
        if self
            .hit_map
            .input
            .is_some_and(|hit| contains(hit.rect, column, row))
        {
            // The composer wheel-scrolls only when its content overflows
            // the (ceiling-height) box; a fitting draft has nothing to
            // scroll, and the gesture must not leak through to the
            // conversation beneath.
            if self.input.has_overflow() {
                self.input.move_wheel(up);
            }
            return;
        }
        // The reserved scrollbar column keeps wheel priority over content.
        if self
            .hit_map
            .scrollbar
            .is_some_and(|rect| contains(rect, column, row))
        {
            let page = usize::from(
                self.hit_map
                    .conversation
                    .map_or(3, |rect| rect.height.max(1)),
            );
            if up {
                self.conversation_scroll.up(3);
            } else {
                self.conversation_scroll.down(page.min(20));
            }
            return;
        }
        if self
            .hit_map
            .conversation
            .is_some_and(|rect| contains(rect, column, row))
        {
            if up {
                self.conversation_scroll.up(3);
            } else {
                self.conversation_scroll.down(3);
            }
            return;
        }
        if self
            .hit_map
            .tree
            .is_some_and(|rect| contains(rect, column, row))
        {
            if up {
                self.move_tree_selection(true);
            } else {
                self.move_tree_selection(false);
            }
        }
    }

    pub(super) fn move_tree_selection(&mut self, up: bool) {
        let entries = self.tree_entries();
        if entries.is_empty() {
            self.tree_cursor = None;
            self.tree_offset = 0;
            return;
        }
        let index = self.tree_cursor_index(&entries).unwrap_or(0);
        let next = if up {
            index.saturating_sub(1)
        } else {
            (index + 1).min(entries.len() - 1)
        };
        self.tree_cursor = Some(entries[next].0);
        self.clamp_tree_view();
    }

    /// The flattened row index of the tree cursor. The cursor is retained by
    /// `SessionId` across tree refreshes; if the session disappears, the
    /// nearest surviving row is used without clearing the cursor identity.
    fn tree_cursor_index(&self, entries: &[(SessionId, SessionMeta, usize)]) -> Option<usize> {
        let cursor = self.tree_cursor?;
        entries
            .iter()
            .position(|(session_id, _, _)| *session_id == cursor)
    }

    fn clamp_tree_view(&mut self) {
        let entries = self.tree_entries();
        self.clamp_tree_view_with(&entries);
    }

    fn clamp_tree_view_with(&mut self, entries: &[(SessionId, SessionMeta, usize)]) {
        let mut selection = self.tree_cursor_index(entries).unwrap_or(0);
        super::pickers::clamp_tree_view(
            &mut selection,
            &mut self.tree_offset,
            entries.len(),
            self.tree_viewport_height,
        );
        if self.tree_cursor.is_none() || self.tree_cursor_index(entries).is_none() {
            self.tree_cursor = entries.first().map(|(session_id, _, _)| *session_id);
        }
    }

    pub(super) fn toggle_tree_session(&mut self, session_id: SessionId) {
        if !self.collapsed_sessions.insert(session_id) {
            self.collapsed_sessions.remove(&session_id);
        }
        self.clamp_tree_view();
    }

    /// Retire any composer-leg selection: its byte offsets address the
    /// draft as it was when the drag happened, so a later buffer mutation
    /// or cursor move would leave it painting and cutting the wrong slice.
    /// Conversation selections address rendered lines and are unaffected.
    fn retire_composer_selection(&mut self) {
        if matches!(self.selection, Some(TextSelection::Composer { .. })) {
            self.selection = None;
        }
    }

    /// Mutate the composer draft, retiring any composer-leg selection.
    fn mutate_input(&mut self, mutation: impl FnOnce(&mut InputState)) {
        mutation(&mut self.input);
        self.retire_composer_selection();
    }

    /// Move the composer cursor, retiring any composer-leg selection the
    /// same way a mutation does.
    fn navigate_input(&mut self, navigation: impl FnOnce(&mut InputState)) {
        navigation(&mut self.input);
        self.retire_composer_selection();
    }

    pub(super) async fn handle_input_key(&mut self, key: KeyEvent) {
        if self
            .selected
            .is_some_and(|session| self.read_only_sessions.contains(&session))
        {
            self.input_focused = false;
            self.status = "Session is owned by another cookie process; input is disabled.".into();
            return;
        }
        if self.runtime.phase() == RuntimePhase::ErrorRetry
            && key.code == KeyCode::Enter
            && self.input.as_str().is_empty()
        {
            self.status = "Retrying runtime snapshot…".into();
            self.refresh_coherent_lists().await;
            return;
        }
        if self.runtime.phase() == RuntimePhase::Loading {
            self.status = "loading runtime snapshot".into();
            return;
        }
        if is_newline_key(key) {
            self.input_focused = true;
            self.mutate_input(|input| input.insert_newline());
            return;
        }
        if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
            self.input_focused = true;
            self.mutate_input(|input| input.set_buffer("/".into()));
            self.palette_dismissed = false;
            self.palette_state.select(Some(0));
            return;
        }
        if !self.input_focused {
            match key.code {
                KeyCode::Enter => self.input_focused = true,
                KeyCode::Char(character) if is_printable_key(key) => {
                    self.input_focused = true;
                    if self.input.as_str().is_empty() && character == '/' {
                        self.palette_dismissed = false;
                    }
                    self.mutate_input(|input| input.insert(character));
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Enter => self.submit_input().await,
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mutate_input(|input| input.delete_word_left());
            }
            KeyCode::Backspace => {
                self.mutate_input(|input| input.backspace());
            }
            KeyCode::Delete if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mutate_input(|input| input.delete_word_right());
            }
            KeyCode::Delete => {
                self.mutate_input(|input| input.delete());
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_input(|input| input.move_word_left());
            }
            KeyCode::Left => self.navigate_input(|input| input.move_left()),
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_input(|input| input.move_word_right());
            }
            KeyCode::Right => self.navigate_input(|input| input.move_right()),
            KeyCode::Up => {
                // Recall gesture: with an empty composer and a non-empty
                // pending lane, Up withdraws the newest pending message back
                // for editing instead of moving a cursor that has nothing
                // to move through.
                if self.input.as_str().is_empty() && self.selected_pending_inputs().is_some() {
                    self.recall_newest_pending();
                } else {
                    self.navigate_input(|input| input.move_up());
                }
            }
            KeyCode::Down => self.navigate_input(|input| input.move_down()),
            KeyCode::PageUp => {
                let page = self
                    .hit_map
                    .conversation
                    .map_or(1, |rect| rect.height.max(1));
                self.conversation_scroll.up(usize::from(page));
            }
            KeyCode::PageDown => {
                let page = self
                    .hit_map
                    .conversation
                    .map_or(1, |rect| rect.height.max(1));
                self.conversation_scroll.down(usize::from(page));
            }
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_input(|input| input.move_buffer_home());
            }
            KeyCode::Home => self.navigate_input(|input| input.move_home()),
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_input(|input| input.move_buffer_end());
            }
            KeyCode::End => self.navigate_input(|input| input.move_end()),
            KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
                self.navigate_input(|input| input.move_home());
            }
            KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => {
                self.navigate_input(|input| input.move_end());
            }
            KeyCode::Char(character) if is_printable_key(key) => {
                if self.input.as_str().is_empty() && character == '/' {
                    self.palette_dismissed = false;
                }
                self.mutate_input(|input| input.insert(character));
            }
            _ => {}
        }
    }

    pub(super) fn handle_paste(&mut self, text: &str) {
        if self.modal == Modal::Sessions {
            let sanitized = text.replace(['\r', '\n'], "");
            self.session_search.focus_input();
            self.session_search.input_mut().insert_text(&sanitized);
            self.session_search_changed();
            return;
        }
        if self.modal == Modal::ConnectProviders {
            let sanitized = text.replace(['\r', '\n'], "");
            self.provider_search.focus_input();
            self.provider_search.input_mut().insert_text(&sanitized);
            self.provider_search_changed();
            return;
        }
        if self.modal == Modal::Models {
            let sanitized = text.replace(['\r', '\n'], "");
            self.model_search.focus_input();
            self.model_search.input_mut().insert_text(&sanitized);
            self.model_search_changed();
            return;
        }
        if self.modal == Modal::Agents {
            let sanitized = text.replace(['\r', '\n'], "");
            self.agent_search.focus_input();
            self.agent_search.input_mut().insert_text(&sanitized);
            self.agent_search_changed();
            return;
        }
        if self.modal == Modal::ConnectSetup {
            let mut sanitized = Zeroizing::new(text.replace(['\r', '\n'], ""));
            if let Some(form) = &mut self.provider_form {
                match form.focus() {
                    ProviderFormFocus::Credential(index) => {
                        form.error = None;
                        form.secrets[index]
                            .input
                            .insert_owned(std::mem::take(&mut *sanitized));
                    }
                    ProviderFormFocus::Setup(index) => {
                        form.error = None;
                        form.setup[index]
                            .input
                            .insert_owned(std::mem::take(&mut *sanitized));
                    }
                    ProviderFormFocus::AuthMethod
                    | ProviderFormFocus::Submit
                    | ProviderFormFocus::Cancel => {}
                }
            }
            return;
        }
        if self.modal == Modal::Mcp {
            if let Some(input) = self
                .mcp_panel
                .form
                .as_mut()
                .and_then(McpForm::focused_input)
            {
                input.insert_text(&text.replace(['\r', '\n'], ""));
            }
            return;
        }
        if self.modal == Modal::Permissions {
            if let Some(form) = &mut self.permission_panel.form
                && form.focus_pattern
            {
                form.pattern.insert_text(&text.replace(['\r', '\n'], ""));
            }
            return;
        }
        if self.modal != Modal::None {
            return;
        }
        if self
            .selected
            .is_some_and(|session| self.read_only_sessions.contains(&session))
        {
            self.input_focused = false;
            self.status = "Session is owned by another cookie process; input is disabled.".into();
            return;
        }
        self.input_focused = true;
        if self.input.as_str().is_empty() && text.starts_with('/') {
            self.palette_dismissed = false;
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.mutate_input(|input| input.insert_text(&normalized));
    }

    pub(super) async fn handle_session_picker(&mut self, key: KeyEvent) {
        let count = self.session_search_ids().len();
        if self.session_search.focus() == SearchPickerFocus::Input {
            match key.code {
                KeyCode::Esc => {
                    self.session_search.reset();
                    self.modal = Modal::None;
                }
                KeyCode::Down | KeyCode::Tab | KeyCode::Enter if count > 0 => {
                    self.session_search.focus_list();
                    self.clamp_picker_selection();
                }
                KeyCode::Backspace => {
                    self.session_search.input_mut().backspace();
                    self.session_search_changed();
                }
                KeyCode::Delete => {
                    self.session_search.input_mut().delete();
                    self.session_search_changed();
                }
                KeyCode::Left => self.session_search.input_mut().move_left(),
                KeyCode::Right => self.session_search.input_mut().move_right(),
                KeyCode::Home | KeyCode::Char('a')
                    if key.code == KeyCode::Home || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.session_search.input_mut().move_buffer_home();
                }
                KeyCode::End | KeyCode::Char('e')
                    if key.code == KeyCode::End || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.session_search.input_mut().move_buffer_end();
                }
                KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                    self.session_search.input_mut().set_buffer(String::new());
                    self.session_search_changed();
                }
                KeyCode::Char(character) if is_printable_key(key) => {
                    self.session_search.input_mut().insert(character);
                    self.session_search_changed();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::BackTab => self.session_search.focus_input(),
            KeyCode::Up if self.picker_state.selected().unwrap_or(0) == 0 => {
                self.session_search.focus_input();
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down | KeyCode::Tab => {
                move_picker_selection(&mut self.picker_state, count, false)
            }
            KeyCode::Enter => {
                self.choose_picker_entry(self.picker_state.selected().unwrap_or(0))
                    .await
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.session_search.reset();
                self.session_search_changed();
            }
            KeyCode::Char(character) if is_printable_key(key) => {
                self.session_search.focus_input();
                self.session_search.input_mut().insert(character);
                self.session_search_changed();
            }
            _ => {}
        }
    }

    pub(super) async fn handle_selection_picker(&mut self, key: KeyEvent) {
        let count = self.picker_entry_count();
        match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                self.new_session_draft = None;
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down => move_picker_selection(&mut self.picker_state, count, false),
            KeyCode::Tab | KeyCode::BackTab => {
                cycle_selection(
                    &mut self.picker_state,
                    count,
                    agent_cycle_backward(key).unwrap_or(false),
                );
            }
            KeyCode::Enter => {
                self.choose_picker_entry(self.picker_state.selected().unwrap_or(0))
                    .await
            }
            _ => {}
        }
    }

    async fn handle_agent_picker_key(&mut self, key: KeyEvent) {
        let count = self.filtered_agent_picker_candidates().len();
        if self.agent_search.focus() == SearchPickerFocus::Input {
            match key.code {
                KeyCode::Esc => self.close_agent_picker(),
                KeyCode::Down | KeyCode::Tab | KeyCode::Enter if count > 0 => {
                    self.agent_search.focus_list();
                    self.clamp_picker_selection();
                }
                KeyCode::Backspace => {
                    self.agent_search.input_mut().backspace();
                    self.agent_search_changed();
                }
                KeyCode::Delete => {
                    self.agent_search.input_mut().delete();
                    self.agent_search_changed();
                }
                KeyCode::Left => self.agent_search.input_mut().move_left(),
                KeyCode::Right => self.agent_search.input_mut().move_right(),
                KeyCode::Home | KeyCode::Char('a')
                    if key.code == KeyCode::Home || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.agent_search.input_mut().move_buffer_home();
                }
                KeyCode::End | KeyCode::Char('e')
                    if key.code == KeyCode::End || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.agent_search.input_mut().move_buffer_end();
                }
                KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                    self.agent_search.input_mut().set_buffer(String::new());
                    self.agent_search_changed();
                }
                KeyCode::Char(character) if is_printable_key(key) => {
                    self.agent_search.input_mut().insert(character);
                    self.agent_search_changed();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::BackTab => self.agent_search.focus_input(),
            KeyCode::Up if self.picker_state.selected().unwrap_or(0) == 0 => {
                self.agent_search.focus_input();
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down | KeyCode::Tab => {
                move_picker_selection(&mut self.picker_state, count, false)
            }
            KeyCode::Enter => {
                self.choose_picker_entry(self.picker_state.selected().unwrap_or(0))
                    .await
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.agent_search.reset();
                self.agent_search_changed();
            }
            KeyCode::Char(character) if is_printable_key(key) => {
                self.agent_search.focus_input();
                self.agent_search.input_mut().insert(character);
                self.agent_search_changed();
            }
            _ => {}
        }
    }

    async fn handle_model_picker_key(&mut self, key: KeyEvent) {
        let count = self.filtered_draft_models().len();
        if self.model_search.focus() == SearchPickerFocus::Input {
            match key.code {
                KeyCode::Esc => self.close_model_picker(),
                KeyCode::Down | KeyCode::Tab | KeyCode::Enter if count > 0 => {
                    self.model_search.focus_list();
                    self.clamp_picker_selection();
                }
                KeyCode::Backspace => {
                    self.model_search.input_mut().backspace();
                    self.model_search_changed();
                }
                KeyCode::Delete => {
                    self.model_search.input_mut().delete();
                    self.model_search_changed();
                }
                KeyCode::Left => self.model_search.input_mut().move_left(),
                KeyCode::Right => self.model_search.input_mut().move_right(),
                KeyCode::Home | KeyCode::Char('a')
                    if key.code == KeyCode::Home || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.model_search.input_mut().move_buffer_home();
                }
                KeyCode::End | KeyCode::Char('e')
                    if key.code == KeyCode::End || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.model_search.input_mut().move_buffer_end();
                }
                KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                    self.model_search.input_mut().set_buffer(String::new());
                    self.model_search_changed();
                }
                KeyCode::Char(character) if is_printable_key(key) => {
                    self.model_search.input_mut().insert(character);
                    self.model_search_changed();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::BackTab => self.model_search.focus_input(),
            KeyCode::Up if self.picker_state.selected().unwrap_or(0) == 0 => {
                self.model_search.focus_input();
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down | KeyCode::Tab => {
                move_picker_selection(&mut self.picker_state, count, false)
            }
            KeyCode::Enter => {
                self.choose_picker_entry(self.picker_state.selected().unwrap_or(0))
                    .await
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.model_search.reset();
                self.model_search_changed();
            }
            KeyCode::Char(character) if is_printable_key(key) => {
                self.model_search.focus_input();
                self.model_search.input_mut().insert(character);
                self.model_search_changed();
            }
            _ => {}
        }
    }

    fn handle_connect_provider_key(&mut self, key: KeyEvent) {
        let count = self.filtered_providers().len();
        if self.provider_search.focus() == SearchPickerFocus::Input {
            match key.code {
                KeyCode::Esc => self.close_provider_picker(),
                KeyCode::Down | KeyCode::Tab | KeyCode::Enter if count > 0 => {
                    self.provider_search.focus_list();
                    self.clamp_picker_selection();
                }
                KeyCode::Backspace => {
                    self.provider_search.input_mut().backspace();
                    self.provider_search_changed();
                }
                KeyCode::Delete => {
                    self.provider_search.input_mut().delete();
                    self.provider_search_changed();
                }
                KeyCode::Left => self.provider_search.input_mut().move_left(),
                KeyCode::Right => self.provider_search.input_mut().move_right(),
                KeyCode::Home | KeyCode::Char('a')
                    if key.code == KeyCode::Home || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.provider_search.input_mut().move_buffer_home();
                }
                KeyCode::End | KeyCode::Char('e')
                    if key.code == KeyCode::End || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.provider_search.input_mut().move_buffer_end();
                }
                KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                    self.provider_search.input_mut().set_buffer(String::new());
                    self.provider_search_changed();
                }
                KeyCode::Char(character) if is_printable_key(key) => {
                    self.provider_search.input_mut().insert(character);
                    self.provider_search_changed();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.provider_search.focus_input();
            }
            KeyCode::Up if self.picker_state.selected().unwrap_or(0) == 0 => {
                self.provider_search.focus_input();
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down | KeyCode::Tab => {
                move_picker_selection(&mut self.picker_state, count, false)
            }
            KeyCode::BackTab => self.provider_search.focus_input(),
            KeyCode::Enter => {
                let index = self.picker_state.selected().unwrap_or(0);
                if let Some(provider) = self
                    .filtered_providers()
                    .get(index)
                    .map(|provider| (*provider).clone())
                {
                    if matches!(
                        self.provider_operations.get(&provider.id),
                        Some(ProviderOperation::InProgress(_))
                    ) {
                        self.status = "Provider operation already in progress.".into();
                        return;
                    }
                    let state = row_state(
                        &provider,
                        &self.models,
                        self.provider_operations.get(&provider.id),
                    );
                    match state {
                        ProviderRowState::Unsupported => {
                            self.connect_provider = Some(provider);
                            self.provider_search.reset();
                            self.modal = Modal::ConnectDetails;
                        }
                        ProviderRowState::Disconnected
                        | ProviderRowState::ConnectedReconnect
                        | ProviderRowState::Removed => self.begin_provider_form(provider),
                        ProviderRowState::ErrorRetry => {
                            let failed_action = self
                                .provider_operations
                                .get(&provider.id)
                                .and_then(|operation| match operation {
                                    ProviderOperation::Error { action, .. } => Some(*action),
                                    ProviderOperation::InProgress(_) => None,
                                });
                            if failed_action == Some(ProviderAction::Disconnect) {
                                self.connect_provider = Some(provider);
                                self.modal = Modal::DisconnectConfirm;
                            } else {
                                self.begin_provider_form(provider);
                            }
                        }
                    }
                }
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.provider_search.reset();
                self.provider_search_changed();
            }
            KeyCode::Char(character) if is_printable_key(key) => {
                self.provider_search.focus_input();
                self.provider_search.input_mut().insert(character);
                self.provider_search_changed();
            }
            _ => {}
        }
    }

    pub(super) fn begin_provider_form(&mut self, provider: ProviderDescriptor) {
        self.clear_connect_secrets();
        self.connect_provider = Some(provider.clone());
        let reconnect = provider.durable_connection.is_some()
            || row_state(&provider, &self.models, None) == ProviderRowState::ConnectedReconnect;
        let Some(form) = ProviderForm::new(provider, reconnect) else {
            self.modal = Modal::ConnectDetails;
            self.status = "This provider has no store-backed authentication form.".into();
            return;
        };
        self.modal = Modal::ConnectSetup;
        self.provider_form = Some(form);
        self.provider_search.reset();
    }

    fn handle_connect_details_key(&mut self, key: KeyEvent) {
        let Some(provider) = self.connect_provider.clone() else {
            self.modal = Modal::None;
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.clear_connect_secrets();
                self.modal = Modal::None;
            }
            KeyCode::Char('r' | 'R')
                if matches!(
                    row_state(
                        &provider,
                        &self.models,
                        self.provider_operations.get(&provider.id)
                    ),
                    ProviderRowState::ConnectedReconnect | ProviderRowState::Removed
                ) =>
            {
                self.begin_provider_form(provider);
            }
            KeyCode::Char('d' | 'D') if provider.durable_connection.is_some() => {
                self.modal = Modal::DisconnectConfirm;
            }
            KeyCode::Enter => {
                // Unsupported providers are details-only. Enter deliberately
                // performs no mutation.
            }
            _ => {}
        }
    }

    fn handle_connect_setup_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.cancel_connect_form();
            return;
        }
        if key.code == KeyCode::Char('d')
            && key.modifiers == KeyModifiers::CONTROL
            && self
                .provider_form
                .as_ref()
                .is_some_and(|form| form.can_disconnect)
        {
            self.connect_provider = self
                .provider_form
                .as_ref()
                .map(|form| form.provider.clone());
            self.modal = Modal::DisconnectConfirm;
            return;
        }
        let Some(form) = &mut self.provider_form else {
            self.modal = Modal::None;
            return;
        };
        match key.code {
            KeyCode::Up | KeyCode::BackTab => form.move_focus(true),
            KeyCode::Down | KeyCode::Tab => form.move_focus(false),
            // Enter activates the focused button and submits from any other
            // focus — the same path as the Submit button. Traversal is
            // Tab/Down-only; validation failures keep the modal and the
            // focus where they are.
            KeyCode::Enter => match form.focus() {
                ProviderFormFocus::Cancel => self.cancel_connect_form(),
                _ => self.dispatch_provider_connect(),
            },
            KeyCode::Left if form.focus() == ProviderFormFocus::AuthMethod => {
                form.cycle_auth_method(true);
            }
            KeyCode::Right | KeyCode::Char(' ')
                if form.focus() == ProviderFormFocus::AuthMethod =>
            {
                form.cycle_auth_method(false);
            }
            _ => match form.focus() {
                ProviderFormFocus::Credential(index) => {
                    // Any edit supersedes a stale inline validation error.
                    form.error = None;
                    edit_credential_input(&mut form.secrets[index].input, key);
                }
                ProviderFormFocus::Setup(index) => {
                    form.error = None;
                    edit_credential_input(&mut form.setup[index].input, key);
                }
                ProviderFormFocus::AuthMethod
                | ProviderFormFocus::Submit
                | ProviderFormFocus::Cancel => {}
            },
        }
    }

    /// Abort the connect form exactly like Escape: wipe every secret,
    /// dismiss the modal, and report the cancellation.
    fn cancel_connect_form(&mut self) {
        self.clear_connect_secrets();
        self.modal = Modal::None;
        self.status = "Provider connection cancelled; credentials were cleared.".into();
    }

    fn handle_connect_error_key(&mut self, key: KeyEvent) {
        if key.code != KeyCode::Esc {
            return;
        }
        if let Some(form) = &mut self.provider_form {
            form.error = None;
            self.modal = Modal::ConnectSetup;
            self.status = "Connect error dismissed; edit the form and submit to retry.".into();
        } else {
            self.modal = Modal::None;
        }
    }

    pub(super) fn dispatch_provider_connect(&mut self) {
        let Some(form) = self.provider_form.as_mut() else {
            self.clear_connect_secrets();
            self.modal = Modal::None;
            return;
        };
        form.error = None;
        let provider = form.provider.clone();
        // Pre-dispatch validation failures stay inline: the form keeps the
        // modal and the focus where they are so the user can correct the
        // offending value and press Enter again. Only a failed connect RPC
        // escalates to the persistent full-message error state.
        let Some(catalog_revision) = self.catalog_revision.clone() else {
            let error = "Catalog revision is unavailable.".to_owned();
            form.error = Some(error.clone());
            self.status = error;
            return;
        };
        let setup_values = match form.setup_values() {
            Ok(values) => values,
            Err(error) => {
                let error = format!("Invalid public setup: {error}");
                form.error = Some(error.clone());
                self.status = error;
                return;
            }
        };
        let auth_values = match form.auth_values() {
            Ok(values) => values,
            Err(error) => {
                let error = format!("Invalid credentials: {error}");
                form.error = Some(error.clone());
                self.status = error;
                return;
            }
        };
        let auth_method = form.auth_method.clone();
        let action = if form.reconnect {
            ProviderAction::Reconnect
        } else {
            ProviderAction::Connect
        };
        let baseline = self.runtime.revision().cloned();
        form.wipe_sensitive_values();
        self.provider_operations
            .insert(provider.id.clone(), ProviderOperation::InProgress(action));
        self.status = format!("{} provider {}…", action_name(action), provider.id);
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let task = tokio::spawn(async move {
            let connect = match client
                .connect_provider(ProviderConnectParams {
                    client_connect_id: ClientConnectId::new(Uuid::now_v7().to_string())
                        .expect("uuid-derived client connect id"),
                    provider_id: provider.id.clone(),
                    expected_catalog_revision: catalog_revision,
                    setup_values,
                    auth_method,
                    auth_values,
                })
                .await
            {
                Ok(connect) => connect,
                Err(error) => {
                    let _ = updates.send(RpcUpdate::ProviderMutationFinished {
                        outcome: ProviderMutationOutcome::Failed {
                            provider_id: provider.id,
                            action,
                            error: error.to_string(),
                        },
                    });
                    return;
                }
            };
            let _ = updates.send(RpcUpdate::ProviderMutationFinished {
                outcome: ProviderMutationOutcome::Connected {
                    provider_id: connect.durable_connection.provider_id,
                    baseline,
                    runtime: Box::new(connect.runtime),
                },
            });
        });
        if let Some(previous) = self.connect_task.replace(task) {
            previous.abort();
        }
    }

    pub(super) fn clear_connect_secrets(&mut self) {
        if let Some(form) = &mut self.provider_form {
            form.wipe_secrets();
        }
        self.provider_form = None;
        self.connect_provider = None;
    }

    fn handle_disconnect_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                self.clear_connect_secrets();
                self.modal = Modal::None;
                self.status = "Provider disconnect cancelled.".into();
            }
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => self.dispatch_provider_disconnect(),
            _ => {}
        }
    }

    fn dispatch_provider_disconnect(&mut self) {
        let Some(provider) = self.connect_provider.clone() else {
            self.modal = Modal::None;
            return;
        };
        let Some(snapshot) = self.runtime.snapshot() else {
            self.status = "Runtime snapshot unavailable; retry before disconnecting.".into();
            return;
        };
        let baseline = Some(snapshot.runtime_revision.clone());
        let params = ProviderDisconnectParams {
            provider_id: provider.id.clone(),
            expected_runtime_revision: snapshot.runtime_revision.clone(),
            expected_provider_state_revision: snapshot.provider_state_revision.clone(),
            expected_connection_generation: provider
                .durable_connection
                .as_ref()
                .map(|connection| connection.connection_generation),
            client_request_id: ClientRequestId::new(Uuid::now_v7().to_string())
                .expect("uuid-derived client request id"),
        };
        self.clear_connect_secrets();
        self.modal = Modal::None;
        self.provider_operations.insert(
            provider.id.clone(),
            ProviderOperation::InProgress(ProviderAction::Disconnect),
        );
        self.status = format!("disconnect provider {}…", provider.id);
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let provider_id = provider.id;
        let task = tokio::spawn(async move {
            let outcome = match client.disconnect_provider(params).await {
                Ok(result) => ProviderMutationOutcome::Disconnected {
                    provider_id,
                    baseline,
                    runtime: Box::new(result.runtime.snapshot),
                },
                Err(error) => ProviderMutationOutcome::Failed {
                    provider_id,
                    action: ProviderAction::Disconnect,
                    error: error.to_string(),
                },
            };
            let _ = updates.send(RpcUpdate::ProviderMutationFinished { outcome });
        });
        if let Some(previous) = self.connect_task.replace(task) {
            previous.abort();
        }
    }

    pub(super) fn abort_connect_work(&mut self) {
        if let Some(task) = self.connect_task.take() {
            task.abort();
        }
    }

    pub(super) async fn choose_picker_entry(&mut self, index: usize) {
        match self.modal {
            Modal::Sessions => {
                if let Some(session_id) = self.session_search_ids().get(index).copied() {
                    self.modal = Modal::None;
                    self.session_search.reset();
                    self.open_session(session_id).await;
                }
            }
            Modal::Agents => {
                if self.new_session_draft.is_none() && !self.agent_switching_allowed() {
                    self.status = self
                        .delegated_pin_reason()
                        .unwrap_or_else(|| "agent switching requires a root session".into());
                    return;
                }
                let agent = self
                    .filtered_agent_picker_candidates()
                    .get(index)
                    .map(|agent| agent.id.clone());
                if let Some(agent) = agent {
                    self.set_draft_agent(agent);
                    self.agent_search.reset();
                    self.modal = Modal::None;
                    if let Some(selection) = self.new_session_draft.clone() {
                        self.create_root_session(selection).await;
                    }
                }
            }
            Modal::Presets => {
                let preset = if index == 0 {
                    Some(None)
                } else {
                    self.preset_names().get(index - 1).cloned().map(Some)
                };
                if let Some(preset) = preset {
                    let preferred_agent = self.draft.as_ref().map(|draft| draft.agent.clone());
                    let preferred_model = self.draft.as_ref().map(|draft| draft.model.clone());
                    self.selected_preset = preset;
                    if self.watching_root_session() {
                        self.draft = self.draft_selection_for_preset(
                            self.selected_preset.as_deref(),
                            preferred_agent.as_ref(),
                            preferred_model.as_ref(),
                        );
                    }
                    if self.draft.is_none() && self.watching_root_session() {
                        self.open_selection_modal(Modal::Agents);
                    } else {
                        self.modal = Modal::None;
                    }
                    self.status = if self.watching_root_session() {
                        self.draft_status("Draft run preset")
                    } else {
                        format!(
                            "Agent preset for the next root run and future new sessions: {}; delegated session remains pinned",
                            self.selected_preset_label()
                        )
                    };
                }
            }
            Modal::Models => {
                if !self.model_selection_allowed() {
                    self.status = "no draft model is available for this session".into();
                    return;
                }
                let model = self
                    .filtered_draft_models()
                    .get(index)
                    .map(|selection| selection.model.clone());
                if let Some(model) = model {
                    self.set_draft_model(model);
                    self.model_search.reset();
                    self.modal = Modal::None;
                }
            }
            Modal::ConnectProviders => {
                self.picker_state.select(Some(index));
                self.provider_search.focus_list();
                self.handle_connect_provider_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            }
            Modal::ConnectDetails
            | Modal::ConnectSetup
            | Modal::ConnectError
            | Modal::DisconnectConfirm => {}
            Modal::UserMessage => self.activate_user_menu_entry(index),
            Modal::RevertConfirm => {}
            Modal::Mcp | Modal::Permissions | Modal::Skills | Modal::Usage => {}
            Modal::None => {}
        }
    }

    pub(super) async fn submit_input(&mut self) {
        if self
            .selected
            .is_some_and(|session| self.read_only_sessions.contains(&session))
        {
            self.status = "Session is owned by another cookie process; input is disabled.".into();
            return;
        }
        if self.input.as_str().trim().is_empty() {
            return;
        }
        let skills = self
            .skills
            .iter()
            .filter(|skill| skill.precedence_winner && skill.user_invocable)
            .map(|skill| skill.name.clone())
            .collect::<Vec<_>>();
        let submission = match parse_submission_with_skills(self.input.as_str(), &skills) {
            Ok(submission) => submission,
            Err(error) => {
                self.mutate_input(|input| {
                    input.take();
                });
                self.palette_dismissed = false;
                self.status = error;
                return;
            }
        };
        match submission {
            Submission::Command(command) => {
                self.mutate_input(|input| {
                    input.take();
                });
                self.palette_dismissed = false;
                self.run_command(command).await;
            }
            Submission::Prompt(prompt) => self.submit_prompt(prompt).await,
        }
    }

    pub(super) async fn submit_prompt(&mut self, input: String) {
        if self.runtime.phase() == RuntimePhase::Loading {
            self.status = "loading runtime snapshot".into();
            return;
        }
        if self.runtime.phase() == RuntimePhase::ErrorRetry && self.runtime.snapshot().is_none() {
            self.status = self
                .runtime
                .durable_explanation()
                .unwrap_or("runtime snapshot unavailable; retry")
                .into();
            return;
        }
        if self.runtime.is_empty() {
            self.mutate_input(|input| {
                input.take();
            });
            self.palette_dismissed = false;
            self.status = EMPTY_RUNTIME_GUIDANCE.into();
            return;
        }
        let Some(session_id) = self.selected else {
            self.status = "create or select a session first".into();
            return;
        };
        self.mutate_input(|input| {
            input.take();
        });
        self.palette_dismissed = false;
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let active_run = self
            .store
            .sessions
            .get(&session_id)
            .and_then(|state| state.active_run);
        let selection = self.draft.clone();
        if active_run.is_none() && selection.is_none() {
            self.status = "select a draft agent/model before submitting".into();
            return;
        }
        self.spawn_rpc(async move {
            if let Some(run_id) = active_run {
                // The engine admits steered inputs even across compaction
                // reservations now and reports the pending lane through
                // events; only a transport failure can strand the text, and
                // that is owed back to the composer.
                match client
                    .steer_run(RunSteerParams {
                        run_id,
                        input: input.clone(),
                    })
                    .await
                {
                    Ok(result) if result.handled_reason.is_some() => {
                        let _ = updates.send(RpcUpdate::Notice(
                            result.handled_reason.expect("reason is present"),
                        ));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = updates.send(RpcUpdate::SteerFailed {
                            session_id,
                            input,
                            error: error.to_string(),
                        });
                    }
                }
            } else if let Err(error) = client
                .start_run(RunStartParams {
                    session_id,
                    client_run_id: client_run_id(),
                    selection: selection.expect("draft selection checked"),
                    input,
                })
                .await
            {
                let _ = updates.send(RpcUpdate::Status(error.to_string()));
            }
        });
    }

    /// The viewed session's pending steered inputs, when the lane is
    /// non-empty. The lane itself is a pure event reduction inside
    /// `SessionState`; the strip is only a projection of it.
    fn selected_pending_inputs(&self) -> Option<&VecDeque<PendingInput>> {
        self.selected
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .map(|state| &state.pending_inputs)
            .filter(|pending| !pending.is_empty())
    }

    /// Strip height in rows for the selected session's pending lane: zero
    /// while empty so the layout never leaves a stray border behind.
    pub(super) fn queue_strip_height(&self) -> u16 {
        let Some(pending) = self.selected_pending_inputs() else {
            return 0;
        };
        // Visible entries plus block borders; the "+N more" folding row
        // shares the entry budget, so the cap never grows past it.
        (pending.len().min(MAX_VISIBLE_QUEUE_ROWS) as u16).saturating_add(2)
    }

    /// Render the pending-input strip between the conversation pane and the
    /// status line. Entries are hoverable and clickable: clicking recalls
    /// the engine's newest pending input for editing.
    pub(super) fn render_queue_strip(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.hit_map.queue_entries.clear();
        if area.height == 0 || area.width < 3 {
            return;
        }
        let Some(pending) = self.selected_pending_inputs() else {
            return;
        };
        let oldest_age = jiff::Timestamp::now()
            .duration_since(pending[0].admitted_at)
            .as_secs()
            .max(0);
        let title = format!("Pending · oldest {}", queue_age_label(oldest_age));
        let block = Block::bordered()
            .title(Span::styled(title, self.theme.muted()))
            .border_style(self.theme.panel_border())
            .style(self.theme.panel());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let entry_rows = inner.height as usize;
        if entry_rows == 0 {
            return;
        }
        // Entries fill the budget; one row folds the remainder into
        // "+N more" so the strip never grows past its cap.
        let shown = if pending.len() > entry_rows {
            entry_rows.saturating_sub(1)
        } else {
            pending.len()
        };
        let overflow = pending.len() - shown;
        let mut lines = Vec::new();
        for (index, entry) in pending.iter().enumerate().take(shown) {
            let prefix = format!("⏳ {} ", index + 1);
            let available =
                usize::from(inner.width).saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
            lines.push(Line::from(vec![
                Span::styled(prefix, self.theme.muted()),
                Span::styled(
                    ellipsize_single_line(&entry.text, available),
                    self.theme.muted(),
                ),
            ]));
        }
        if overflow > 0 {
            lines.push(Line::from(Span::styled(
                format!("+{overflow} more"),
                self.theme.muted(),
            )));
        }
        let line_count = lines.len();
        frame.render_widget(Paragraph::new(lines), inner);
        // Every body row is a recall affordance: the engine withdraws the
        // newest pending input regardless of which row is clicked.
        self.hit_map.queue_entries = (0..line_count)
            .map(|index| QueueEntryHit {
                rect: Rect::new(
                    inner.x,
                    inner.y.saturating_add(index as u16),
                    inner.width,
                    1,
                ),
                index,
            })
            .collect();
    }

    /// Copy text to the system clipboard via an OSC 52 escape written to
    /// the terminal: no platform dependency, and it works over SSH (the
    /// terminal emulator owns the clipboard, not the host). Terminals that
    /// ignore OSC 52 simply leave the clipboard untouched.
    fn copy_to_clipboard(&mut self, text: String) {
        let characters = text.chars().count();
        let result = match &self.clipboard_sink {
            ClipboardSink::Osc52 => io::stdout()
                .lock()
                .write_all(osc52_sequence(&text).as_bytes())
                .and_then(|()| io::stdout().flush()),
            #[cfg(test)]
            ClipboardSink::Capture(copied) => {
                copied.lock().expect("clipboard capture").push(text);
                Ok(())
            }
        };
        self.status = match result {
            Ok(()) => format!("copied {characters} characters to the clipboard"),
            Err(error) => format!("clipboard write failed: {error}"),
        };
    }

    /// Prepend restored text to the composer, preserving FIFO order for
    /// multiple entries. Never called with an empty batch.
    fn restore_composer_text(&mut self, texts: Vec<String>) {
        debug_assert!(!texts.is_empty());
        let mut restored = texts.join("\n");
        let existing = self.input.as_str();
        if !existing.is_empty() {
            restored.push('\n');
            restored.push_str(existing);
        }
        self.mutate_input(|input| input.set_buffer(restored));
        self.input_focused = true;
    }

    /// Restore any voided pending inputs of the viewed session into the
    /// composer: run-end casualties and recalls that resolved while another
    /// session was being viewed. The store holds them until this drain.
    fn restore_voided_inputs(&mut self) {
        let Some(session_id) = self.selected else {
            return;
        };
        let texts = self.store.take_voided_inputs(session_id);
        if texts.is_empty() {
            return;
        }
        self.restore_composer_text(texts);
        self.status = "unsent message restored to the composer".into();
    }

    /// Recall the engine's newest pending steered input so its text returns
    /// to the composer for editing (`run.recall_steer`). Triggered by
    /// clicking a strip entry or pressing Up in an empty composer; the
    /// `UserInputRecalled` event removes the entry from the strip itself.
    pub(super) fn recall_newest_pending(&mut self) {
        let Some(session_id) = self.selected else {
            return;
        };
        let Some(state) = self.store.sessions.get(&session_id) else {
            return;
        };
        let Some(run_id) = state.active_run else {
            self.status = "no active run to recall from".into();
            return;
        };
        if state.pending_inputs.is_empty() {
            self.status = "no pending message to recall".into();
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let update = match client.recall_steer(RunRecallSteerParams { run_id }).await {
                Ok(result) => match result.recalled {
                    Some(text) => {
                        let _ = updates.send(RpcUpdate::SteerRecalled { session_id, text });
                        return;
                    }
                    // The lane raced ahead (a promotion landed first):
                    // nothing is owed; the strip is already catching up.
                    None => RpcUpdate::Notice("nothing pending to recall".to_owned()),
                },
                Err(error) => RpcUpdate::Status(error.to_string()),
            };
            let _ = updates.send(update);
        });
    }

    /// Open the copy/revert/fork menu for a clicked user-message row. The
    /// message text is captured now: a rebuild (e.g. a concurrent revert)
    /// can change the transcript before the action runs.
    fn open_user_menu(&mut self, hit: UserMessageHit) {
        let Some(session_id) = self.selected else {
            return;
        };
        let text = self.store.sessions.get(&session_id).and_then(|state| {
            state.transcript.iter().find_map(|item| match item {
                TranscriptItem::User { seq, text, .. } if *seq == hit.seq => Some(text.clone()),
                _ => None,
            })
        });
        let Some(text) = text else {
            self.status = "message is no longer in the visible branch".into();
            return;
        };
        self.user_menu = Some(UserMenuState {
            session_id,
            seq: hit.seq,
            text,
        });
        self.picker_state.select(Some(0));
        self.modal = Modal::UserMessage;
    }

    async fn handle_user_menu_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                self.user_menu = None;
            }
            KeyCode::Up => {
                move_picker_selection(&mut self.picker_state, USER_MENU_ITEMS.len(), true);
            }
            KeyCode::Down => {
                move_picker_selection(&mut self.picker_state, USER_MENU_ITEMS.len(), false);
            }
            KeyCode::Enter => {
                self.choose_picker_entry(self.picker_state.selected().unwrap_or(0))
                    .await;
            }
            KeyCode::Char('c') => self.activate_user_menu_entry(0),
            KeyCode::Char('r') => self.activate_user_menu_entry(1),
            KeyCode::Char('f') => self.activate_user_menu_entry(2),
            _ => {}
        }
    }

    /// Run one menu row: copy, revert (behind its confirm guard), or fork.
    fn activate_user_menu_entry(&mut self, index: usize) {
        let Some(menu) = self.user_menu.clone() else {
            self.modal = Modal::None;
            return;
        };
        match index {
            0 => {
                self.modal = Modal::None;
                self.user_menu = None;
                self.copy_to_clipboard(menu.text);
            }
            1 => self.modal = Modal::RevertConfirm,
            2 => {
                self.modal = Modal::None;
                self.user_menu = None;
                self.dispatch_session_fork(menu);
            }
            _ => {}
        }
    }

    async fn handle_revert_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n' | 'N') => self.modal = Modal::UserMessage,
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                let Some(menu) = self.user_menu.take() else {
                    self.modal = Modal::None;
                    return;
                };
                self.modal = Modal::None;
                self.dispatch_session_revert(menu);
            }
            _ => {}
        }
    }

    /// Revert the session to just before the menu's message (`through_seq =
    /// seq - 1`), voiding it and every later turn from the visible branch.
    /// The physical log is append-only; the `SessionReverted` marker drives
    /// the transcript rebuild through the normal event flow. On success the
    /// message text restores into the composer for editing and resending.
    fn dispatch_session_revert(&mut self, menu: UserMenuState) {
        // User messages always follow `SessionCreated` (sequence 1), so
        // `seq - 1` is a positive existing physical sequence.
        let through_seq = menu.seq.saturating_sub(1).max(1);
        let session_id = menu.session_id;
        let text = menu.text;
        self.status = "reverting to before the message…".into();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let message = match client
                .revert_session(SessionRevertParams {
                    session_id,
                    through_seq,
                })
                .await
            {
                Ok(result) => {
                    let text = result.instructions_override.unwrap_or(text);
                    let _ = updates.send(RpcUpdate::Reverted { session_id, text });
                    return;
                }
                Err(error) => format!("revert failed: {error}"),
            };
            let _ = updates.send(RpcUpdate::Status(message));
        });
    }

    /// Fork the session at the menu's message (`through_seq = seq`, keeping
    /// the message in the copied prefix), then switch to the new session.
    fn dispatch_session_fork(&mut self, menu: UserMenuState) {
        let session_id = menu.session_id;
        self.status = "forking the session from the message…".into();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let message = match client
                .fork_session(SessionForkParams {
                    session_id,
                    through_seq: menu.seq,
                })
                .await
            {
                Ok(result) => {
                    let _ = updates.send(RpcUpdate::Forked {
                        forked: result.session_id,
                    });
                    return;
                }
                Err(error) => format!("fork failed: {error}"),
            };
            let _ = updates.send(RpcUpdate::Status(message));
        });
    }

    fn render_user_menu(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let entries = USER_MENU_ITEMS
            .iter()
            .map(|(action, description)| format!("{action} — {description}"))
            .collect();
        self.render_picker(
            frame,
            "Message actions",
            entries,
            None,
            area,
            Some("↑↓ move · enter/c/r/f: run · esc: close"),
        );
    }

    fn render_revert_confirm(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let Some(menu) = self.user_menu.as_ref() else {
            return;
        };
        let preview = ellipsize_single_line(&menu.text, 48);
        let content = format!(
            "Revert to before \"{preview}\"?\n\nThe message and every later turn leave the visible branch; the append-only log is kept. The message text returns to the composer for editing and resending.\n\nPress Enter/Y to revert or Esc/N to go back."
        );
        frame.render_widget(
            Paragraph::new(content).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(self.theme.panel_border())
                    .title("Confirm revert"),
            ),
            area,
        );
    }

    pub async fn send_stdin(&mut self, input: String, eof: bool) {
        let Some((run_id, call_id)) = self.selected_running_tool() else {
            self.status = "no running interactive tool".into();
            return;
        };
        let data = (!input.is_empty()).then(|| STANDARD.encode(input.as_bytes()));
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let lane = {
            let mut lanes = self.stdin_lanes.lock().await;
            lanes
                .entry(call_id)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        self.spawn_rpc(async move {
            let _guard = lane.lock().await;
            let update = match tokio::time::timeout(
                STDIN_RPC_TIMEOUT,
                client.tool_stdin(RunToolStdinParams {
                    run_id,
                    call_id,
                    data,
                    eof,
                }),
            )
            .await
            {
                Err(_) => RpcUpdate::Status("stdin request timed out".into()),
                Ok(Ok(result)) if !result.accepted => {
                    RpcUpdate::Status("stdin was rejected by the tool".into())
                }
                Ok(Ok(_)) => {
                    if eof {
                        RpcUpdate::Notice("tool stdin closed".into())
                    } else {
                        RpcUpdate::Notice("stdin sent".into())
                    }
                }
                Ok(Err(error)) => RpcUpdate::Status(error.to_string()),
            };
            let _ = updates.send(update);
        });
    }

    pub(super) async fn run_command(&mut self, command: SlashCommand) {
        match command {
            SlashCommand::Quit => self.should_quit = true,
            SlashCommand::New => {
                if self.runtime.is_empty() {
                    self.status = EMPTY_RUNTIME_GUIDANCE.into();
                    return;
                }
                self.new_session_draft =
                    self.draft_selection_for_preset(self.selected_preset.as_deref(), None, None);
                self.open_selection_modal(Modal::Agents);
                if self.modal == Modal::Agents {
                    self.status = "Select the agent for the new root session.".into();
                }
            }
            SlashCommand::Preset => {
                self.modal = Modal::Presets;
                self.picker_state.select(Some(0));
                self.status =
                    "Select the preset for the next root run and future new sessions.".into();
            }
            SlashCommand::Connect => {
                self.clear_connect_secrets();
                self.modal = Modal::ConnectProviders;
                self.provider_search.reset();
                self.picker_state.select(Some(0));
                if self.providers.is_empty() {
                    self.status = "No providers are available in the runtime snapshot.".into();
                } else {
                    self.status = "Search providers, then press Down or Tab to choose one.".into();
                }
            }
            SlashCommand::Mcp => {
                self.modal = Modal::Mcp;
                self.mcp_panel.form = None;
                self.poll_mcp();
            }
            SlashCommand::Permissions => {
                if self.selected.is_none() {
                    self.status = "select a session before editing permissions".into();
                } else {
                    self.modal = Modal::Permissions;
                    self.permission_panel.begin_load();
                    self.load_permissions();
                }
            }
            SlashCommand::Skills => {
                let Some(session_id) = self.selected else {
                    self.status = "select a session before listing skills".into();
                    return;
                };
                self.modal = Modal::Skills;
                match self
                    .client
                    .list_skills(cookie_agent_protocol::SkillsListParams { session_id })
                    .await
                {
                    Ok(result) => {
                        self.skills = result.skills.clone();
                        self.skill_panel.install(result);
                        self.status = "skills loaded".into();
                    }
                    Err(error) => self.status = format!("list skills failed: {error}"),
                }
            }
            SlashCommand::Skill { name, args } => {
                let Some(session_id) = self.selected else {
                    self.status = "select a session before invoking a skill".into();
                    return;
                };
                match self
                    .client
                    .get_skill(cookie_agent_protocol::SkillsGetParams {
                        session_id,
                        name: name.clone(),
                        args: args.clone(),
                    })
                    .await
                {
                    Ok(result) if result.skill.user_invocable => {
                        self.submit_prompt(cookie_agent_protocol::encode_skill_submission(
                            &name, &args,
                        ))
                        .await;
                    }
                    Ok(_) => self.status = "skill is not user-invocable".into(),
                    Err(error) => self.status = format!("load skill failed: {error}"),
                }
            }
            SlashCommand::Usage => {
                self.modal = Modal::Usage;
                self.load_usage();
            }
            SlashCommand::Sessions => {
                self.modal = Modal::Sessions;
                self.session_search.reset();
                self.picker_state.select(Some(0));
            }
            SlashCommand::Cancel => self.cancel_active_run(),
            SlashCommand::Compact(focus) => self.compact_selected_session(focus).await,
            SlashCommand::Approve(decision) => self.answer_approval(decision).await,
            SlashCommand::Events(level) => {
                // View-only threshold change: the TOML is not rewritten and
                // hidden rows stay in the session projection.
                self.tui_config.minimum_event_level = level;
                self.status = format!("diagnostic event filter: {}", level.name());
            }
            SlashCommand::Help => self.show_help(),
        }
    }

    async fn compact_selected_session(&mut self, focus: Option<String>) {
        let Some(session_id) = self.selected else {
            self.status = "select a session before compacting context".into();
            return;
        };
        let focus = match focus.map(SafeDisplayText::new).transpose() {
            Ok(focus) => focus,
            Err(_) => {
                self.status = "compaction focus must be control-free and at most 1024 bytes".into();
                return;
            }
        };
        self.status = "compacting context…".into();
        self.status = match self
            .client
            .compact_session(SessionCompactParams { session_id, focus })
            .await
        {
            Ok(result) if result.compacted => "context compacted".into(),
            Ok(result) if result.cancellation_reason.is_some() => {
                result.cancellation_reason.expect("reason is present")
            }
            Ok(_) => "context did not require or could not produce a smaller checkpoint".into(),
            Err(error) => format!("context compaction failed: {error}"),
        };
    }

    pub(super) fn show_help(&mut self) {
        // One line per command in the transcript; the status line stays a
        // short pointer instead of a truncated wall of text.
        let notice = std::iter::once("Available commands:".to_owned())
            .chain(command_help_lines())
            .chain(std::iter::once(
                "Use // to send a prompt beginning with /.".to_owned(),
            ))
            .collect::<Vec<_>>()
            .join("\n");
        self.status = "commands listed in the conversation".into();
        self.transient_notices.push(notice);
        if self.transient_notices.len() > MAX_TRANSIENT_NOTICES {
            let excess = self.transient_notices.len() - MAX_TRANSIENT_NOTICES;
            self.transient_notices.drain(..excess);
        }
    }

    /// Optimistic approval response: the modal is dismissed immediately and
    /// the exact (id, revision, fingerprint, decision) tuple is sent
    /// asynchronously. Nothing executes locally; failures restore the modal
    /// only when the request is still durably escalated and unexpired.
    pub(super) async fn answer_approval(&mut self, decision: ApprovalUserDecision) {
        if self.pending_approval.is_some() {
            return;
        }
        let Some(approval) = self.current_approval().cloned() else {
            return;
        };
        self.next_approval_request_id = self.next_approval_request_id.wrapping_add(1);
        let request_id = self.next_approval_request_id;
        let decision_label = match decision {
            ApprovalUserDecision::ApproveOnce => "approve once",
            ApprovalUserDecision::ApproveTree => "approve all",
            ApprovalUserDecision::Reject => "reject",
            ApprovalUserDecision::Cancel => "cancel",
        };
        self.pending_approval = Some(PendingApprovalSubmission {
            request_id,
            approval: approval.clone(),
            decision,
        });
        self.status = format!("Approval submitted ({decision_label})…");
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .respond_approval(ApprovalRespondParams {
                    session_id: approval.session_id,
                    approval_id: approval.approval_id,
                    request_revision: approval.request_revision,
                    operation_fingerprint: approval.operation_fingerprint,
                    client_response_id: client_response_id(),
                    decision,
                    feedback: None,
                })
                .await
                .map(|_| ())
                .map_err(ApprovalSubmissionError::from_client);
            let _ = updates.send(RpcUpdate::ApprovalResponse {
                request_id,
                approval_id: approval.approval_id,
                result,
            });
        });
    }

    /// Resolve an in-flight approval response. Success clears the pending
    /// marker; durable resolution arrives through the normal event stream.
    /// Failure restores the modal only when the request is still escalated
    /// and unexpired. Revision/fingerprint conflicts trigger an approval.list
    /// refresh and are never silently resubmitted.
    fn finish_approval_submission(
        &mut self,
        request_id: u64,
        approval_id: cookie_agent_protocol::ApprovalId,
        result: Result<(), ApprovalSubmissionError>,
    ) {
        let Some(pending) = self.pending_approval.take_if(|pending| {
            pending.request_id == request_id && pending.approval.approval_id == approval_id
        }) else {
            return;
        };
        match result {
            Ok(()) => {
                self.remove_exact_approval(&pending.approval);
                let decision_label = match pending.decision {
                    ApprovalUserDecision::ApproveOnce => "approve once",
                    ApprovalUserDecision::ApproveTree => "approve all",
                    ApprovalUserDecision::Reject => "reject",
                    ApprovalUserDecision::Cancel => "cancel",
                };
                self.status = format!("approval response accepted ({decision_label})");
            }
            Err(error) => {
                let approval = pending.approval;
                if error.stale_projection() {
                    self.remove_exact_approval(&approval);
                    self.status = format!(
                        "Approval {approval_id} changed before the response landed; refreshing the approval list."
                    );
                    self.refresh_approvals(approval.session_id);
                    return;
                }
                if self.approval_is_exact_pending(&approval) {
                    self.status = format!("approval response failed: {}", error.message);
                } else {
                    self.status = format!(
                        "approval {approval_id} is no longer pending; refreshing the approval list."
                    );
                    self.refresh_approvals(approval.session_id);
                }
            }
        }
    }

    /// Refresh the durable approval queue after a conflict or expiry.
    fn refresh_approvals(&mut self, session_id: SessionId) {
        let root_session_id = self.tree_root.unwrap_or(session_id);
        self.next_approval_refresh_id = self.next_approval_refresh_id.wrapping_add(1);
        let request_id = self.next_approval_refresh_id;
        let generation = self.selection_generation;
        self.approval_refresh_in_flight = Some((root_session_id, generation, request_id));
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_approvals(ApprovalListParams {
                    root_session_id,
                    status: Some(ApprovalStatus::Escalated),
                })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::ApprovalList {
                root_session_id,
                generation,
                request_id,
                result,
            });
        });
    }

    pub(super) fn current_approval(&self) -> Option<&ApprovalState> {
        if self.pending_approval.is_some() {
            return None;
        }
        self.selected
            .and_then(|id| self.store.sessions.get(&id))
            .and_then(|state| {
                state
                    .approvals
                    .iter()
                    .find(|approval| approval.is_visible_user_escalation())
            })
    }

    fn approval_is_exact_pending(&self, approval: &ApprovalState) -> bool {
        if approval
            .constraints
            .expires_at
            .is_some_and(|expires_at| expires_at <= jiff::Timestamp::now())
        {
            return false;
        }
        let Some(state) = self.store.sessions.get(&approval.session_id) else {
            return false;
        };
        let mut same_id = state
            .approvals
            .iter()
            .filter(|candidate| candidate.approval_id == approval.approval_id);
        same_id.next().is_some_and(|candidate| {
            candidate.is_visible_user_escalation()
                && candidate.request_revision == approval.request_revision
                && candidate.operation_fingerprint == approval.operation_fingerprint
        }) && same_id.next().is_none()
    }

    fn remove_exact_approval(&mut self, approval: &ApprovalState) {
        if let Some(state) = self.store.sessions.get_mut(&approval.session_id) {
            state.approvals.retain(|candidate| {
                candidate.approval_id != approval.approval_id
                    || candidate.request_revision != approval.request_revision
                    || candidate.operation_fingerprint != approval.operation_fingerprint
            });
        }
    }

    pub(super) fn reconcile_pending_approval(&mut self) {
        let stale = self
            .pending_approval
            .as_ref()
            .is_some_and(|pending| !self.approval_is_exact_pending(&pending.approval));
        if stale {
            let pending = self
                .pending_approval
                .take()
                .expect("stale pending approval exists");
            self.remove_exact_approval(&pending.approval);
            self.status = format!(
                "approval {} is no longer pending; showing the next valid approval",
                pending.approval.approval_id
            );
        }
    }

    pub(super) fn apply_approval_list(
        &mut self,
        root_session_id: SessionId,
        result: ApprovalListResult,
    ) {
        let mut session_ids = vec![root_session_id];
        if self.tree_root == Some(root_session_id)
            && let Some(tree) = &self.tree
        {
            collect_tree_session_ids(tree, &mut session_ids);
        }
        session_ids.sort_unstable_by_key(ToString::to_string);
        session_ids.dedup();
        for session_id in session_ids {
            if let Some(state) = self.store.sessions.get_mut(&session_id) {
                // The list refresh replaces only the user-visible queue.
                // Preserve event-projected internal requests so a later,
                // strictly ordered ApprovalEscalated can still reveal them.
                state.approvals.retain(|approval| !approval.escalated);
            }
        }
        for record in result.approvals {
            if let Some(approval) = approval_state_from_record(record) {
                let state = self.store.sessions.entry(approval.session_id).or_default();
                state
                    .approvals
                    .retain(|candidate| candidate.approval_id != approval.approval_id);
                state.approvals.push(approval);
            }
        }
    }

    pub(super) fn selected_running_tool(
        &mut self,
    ) -> Option<(
        cookie_agent_protocol::RunId,
        cookie_agent_protocol::ToolCallId,
    )> {
        let session_id = self.selected?;
        let run_id = self.store.sessions.get(&session_id)?.active_run?;
        let running = self.running_tool_ids();
        if !self
            .stdin_target
            .is_some_and(|call_id| running.contains(&call_id))
        {
            self.stdin_target = running.first().copied();
        }
        let call_id = self.stdin_target?;
        let state = self.store.sessions.get(&session_id)?;
        (state.tools.get(&call_id)?.status == ToolStatus::Running).then_some((run_id, call_id))
    }

    pub(super) fn running_tool_ids(&self) -> Vec<cookie_agent_protocol::ToolCallId> {
        let Some(session_id) = self.selected else {
            return Vec::new();
        };
        let Some(state) = self.store.sessions.get(&session_id) else {
            return Vec::new();
        };
        let mut ids = state
            .tools
            .values()
            .filter(|tool| tool.status == ToolStatus::Running)
            .map(|tool| tool.id)
            .collect::<Vec<_>>();
        ids.sort_by_key(ToString::to_string);
        ids
    }

    pub(super) fn cancel_active_run(&mut self) {
        let Some(session_id) = self.selected else {
            self.status = "no active run to cancel".into();
            return;
        };
        let Some(run_id) = self
            .store
            .sessions
            .get(&session_id)
            .and_then(|state| state.active_run)
        else {
            self.status = "no active run to cancel".into();
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let update = match client.cancel_run(RunCancelParams { run_id }).await {
                Ok(result) if result.cancelled => {
                    RpcUpdate::Notice("run cancellation requested".into())
                }
                Ok(_) => RpcUpdate::Notice("run was already complete".into()),
                Err(error) => RpcUpdate::Status(error.to_string()),
            };
            let _ = updates.send(update);
        });
    }

    /// The exact Message panel title `Agent • Model[Variant]` with separate
    /// structured agent, model, and bracketed variant hit regions. Only the
    /// agent name is bold — typographic emphasis, never a color marker. The
    /// separator, model, and variant subtract the bold the focused border
    /// would otherwise lend them, so they stay regular.
    fn message_title_spans(&self) -> Vec<Span<'static>> {
        let regular = Style::default().remove_modifier(Modifier::BOLD);
        match &self.draft {
            Some(draft) => vec![
                Span::styled(
                    draft.agent.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(" • ", regular),
                Span::styled(draft.model.model.to_string(), regular),
                Span::styled(
                    format!(
                        "[{}]",
                        draft
                            .model
                            .variant
                            .as_ref()
                            .map_or_else(|| "base".to_owned(), |variant| variant.to_string())
                    ),
                    regular,
                ),
            ],
            None => {
                let text = match self.runtime.phase() {
                    RuntimePhase::Loading => "loading runtime snapshot",
                    RuntimePhase::Empty => EMPTY_RUNTIME_GUIDANCE,
                    RuntimePhase::ErrorRetry => "runtime error — retry",
                    RuntimePhase::Ready | RuntimePhase::Stale | RuntimePhase::Bootstrap => {
                        "select an agent and model"
                    }
                };
                // Raw text keeps the border accent inheritance, exactly as
                // the styled composition does for its regular segments.
                vec![Span::raw(text.to_owned())]
            }
        }
    }

    pub(super) fn draw(&mut self, frame: &mut ratatui::Frame) {
        // The warm cream surface is painted beneath everything first so
        // unstyled cells still land on the light theme; overlays then paint
        // their own panels over it instead of clearing to the terminal.
        frame.render_widget(Block::default().style(self.theme.surface()), frame.area());
        self.hit_map = UiHitMap::default();
        // The composer takes one text row by default and grows with its
        // soft-wrapped content up to the ceiling; the layout reclaims those
        // rows from the conversation pane.
        let input_text_rows = u16::try_from(
            self.input
                .content_rows(frame.area().width.saturating_sub(2)),
        )
        .unwrap_or(u16::MAX)
        .clamp(1, super::input::MAX_TEXT_ROWS);
        let tree_entries = self.tree_entries();
        let layout = super::terminal_layout_with_tree_rows(
            frame.area(),
            tree_entries.len(),
            self.queue_strip_height(),
            input_text_rows,
        );
        self.render_tree(frame, layout.agent, &tree_entries);
        self.render_conversation(frame, layout.conversation);
        self.render_queue_strip(frame, layout.queue);
        let title_spans = self.message_title_spans();
        let rendered_input = super::input::render(
            frame,
            layout.input,
            &mut self.input,
            self.input_focused
                && self.modal == Modal::None
                && self
                    .selected
                    .is_none_or(|session| !self.read_only_sessions.contains(&session)),
            Line::from(title_spans.clone()),
            Some(
                if self
                    .selected
                    .is_some_and(|session| self.read_only_sessions.contains(&session))
                {
                    "Read-only snapshot"
                } else {
                    "Type a message · / for commands"
                },
            ),
            &self.theme,
        );
        // Agent, Model, and the complete bracketed Variant suffix are separate
        // clickable regions inside the canonical title. The bullet is decoration.
        self.hit_map.title_segments = if self.draft.is_none() {
            Vec::new()
        } else {
            let segments = [
                Some(TitleSegment::Agent),
                None,
                Some(TitleSegment::Model),
                Some(TitleSegment::Variant),
            ];
            rendered_input
                .title_rect
                .map_or_else(Vec::new, |title_rect| {
                    let mut hits = Vec::new();
                    let mut column = title_rect.x;
                    let visible_end = title_rect.x.saturating_add(title_rect.width);
                    for (span, segment) in title_spans.iter().zip(segments) {
                        let width = UnicodeWidthStr::width(span.content.as_ref())
                            .min(usize::from(u16::MAX)) as u16;
                        let visible_width = visible_end.saturating_sub(column).min(width);
                        if let Some(segment) = segment
                            && visible_width > 0
                        {
                            hits.push(TitleSegmentHit {
                                rect: Rect::new(column, title_rect.y, visible_width, 1),
                                segment,
                            });
                        }
                        column = column.saturating_add(width);
                    }
                    hits
                })
        };
        self.hit_map.input = Some(InputHit {
            rect: layout.input,
            text_rect: rendered_input.text_rect,
            scrollbar: rendered_input.scrollbar,
        });
        let mut base_status = if self.pending_approval.is_some() {
            "Approval submitting…".to_owned()
        } else {
            self.status.clone()
        };
        if let Some(explanation) = self.runtime.durable_explanation()
            && !base_status.contains(explanation)
        {
            base_status = format!("{explanation} · {base_status}");
        }
        // The scroll-follow state lives in the Conversation title, not here.
        let status = base_status;
        // Status and bottom bar share the one cream surface with the input.
        frame.render_widget(
            Paragraph::new(Span::styled(status, self.theme.muted())).style(self.theme.panel()),
            layout.status,
        );
        let bottom_bar = self.bottom_bar_line(layout.bar.width);
        let span_rect = |target_span| {
            let mut column = layout.bar.x;
            for (index, span) in bottom_bar.line.spans.iter().enumerate() {
                let width =
                    UnicodeWidthStr::width(span.content.as_ref()).min(usize::from(u16::MAX)) as u16;
                if index == target_span {
                    let visible = layout
                        .bar
                        .x
                        .saturating_add(layout.bar.width)
                        .saturating_sub(column)
                        .min(width);
                    return (visible > 0).then(|| Rect::new(column, layout.bar.y, visible, 1));
                }
                column = column.saturating_add(width);
            }
            None
        };
        self.hit_map.permission_mode = bottom_bar.mode_span.and_then(span_rect);
        self.hit_map.session_cost = bottom_bar.cost_span.and_then(span_rect);
        frame.render_widget(
            Paragraph::new(bottom_bar.line).style(self.theme.panel()),
            layout.bar,
        );
        if let Some(approval) = self.current_approval().cloned() {
            let area = centered(frame.area(), 76, 40);
            self.hit_map.approval = Some(area);
            self.hit_map.approval_actions = self.render_approval(frame, &approval, area);
        }
        match self.modal {
            Modal::Sessions => {
                self.render_session_search(frame, centered(frame.area(), 72, 60));
            }
            Modal::Agents => {
                // Normal delegated-session selection is pinned to its frozen
                // child agent. `/new` always owns an independent root draft.
                if self.new_session_draft.is_none()
                    && let Some(pin) = self.delegated_pin_reason()
                {
                    let agent = self
                        .selected_session_meta()
                        .map(|meta| meta.creation_selection.agent.to_string())
                        .unwrap_or_default();
                    let description = self
                        .agents
                        .iter()
                        .find(|candidate| {
                            candidate.id.as_str() == agent
                                && candidate.preset
                                    == self
                                        .selected_session_meta()
                                        .and_then(|meta| meta.creation_selection.preset.clone())
                        })
                        .map(|candidate| candidate.description.clone())
                        .unwrap_or_else(|| "frozen child agent".into());
                    self.render_picker(
                        frame,
                        "Agent — fixed (delegated session)",
                        vec![format!("{agent} — {description}"), pin],
                        None,
                        centered(frame.area(), 56, 44),
                        Some("esc: close"),
                    );
                } else {
                    self.render_agent_picker(frame, centered(frame.area(), 56, 44));
                }
            }
            Modal::Presets => {
                let mut entries = vec!["None (shared)".to_owned()];
                entries.extend(self.preset_names());
                self.render_picker(
                    frame,
                    "Agent preset — next root run and future new sessions",
                    entries,
                    None,
                    centered(frame.area(), 48, 40),
                    Some("↑↓ move · enter: select · esc: close"),
                );
            }
            Modal::Models => {
                self.render_model_picker(frame, centered(frame.area(), 56, 44));
            }
            Modal::ConnectProviders => {
                let match_count = self.filtered_providers().len();
                let provider_count = self.providers.len();
                let title =
                    format!("Connect provider ({match_count}/{provider_count}) · Enter: details");
                let area = centered(frame.area(), 78, 64);
                paint_panel(frame, area, &self.theme);
                let input_height = 3.min(area.height);
                let input_area = Rect::new(area.x, area.y, area.width, input_height);
                let rendered_input = super::pickers::render_search_input(
                    frame,
                    input_area,
                    &mut self.provider_search,
                    &self.theme,
                );
                self.hit_map.picker_input = Some(InputHit {
                    rect: input_area,
                    text_rect: rendered_input.text_rect,
                    // Single-row search inputs never reach the composer
                    // ceiling, so they never carry a scrollbar.
                    scrollbar: None,
                });
                let remaining = Rect::new(
                    area.x,
                    area.y.saturating_add(input_height),
                    area.width,
                    area.height.saturating_sub(input_height),
                );
                let copy_height = 3.min(remaining.height);
                let copy = Rect::new(remaining.x, remaining.y, remaining.width, copy_height);
                frame.render_widget(
                    Paragraph::new(DURABLE_PROVIDER_COPY)
                        .wrap(Wrap { trim: false })
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(self.theme.panel_border())
                                .title("Global provider store"),
                        ),
                    copy,
                );
                let picker = Rect::new(
                    remaining.x,
                    remaining.y.saturating_add(copy_height),
                    remaining.width,
                    remaining.height.saturating_sub(copy_height),
                );
                self.render_picker(
                    frame,
                    &title,
                    self.filtered_providers()
                        .iter()
                        .map(|provider| {
                            row_label(
                                provider,
                                &self.models,
                                self.provider_operations.get(&provider.id),
                            )
                        })
                        .collect(),
                    if self.providers.is_empty() {
                        Some("No providers are available in the runtime snapshot.")
                    } else if match_count == 0 {
                        Some("No providers match the filter.")
                    } else {
                        None
                    },
                    picker,
                    Some("↑↓ move · enter: details · esc: close"),
                );
            }
            Modal::ConnectDetails => {
                self.render_connect_details(frame, centered(frame.area(), 80, 62));
            }
            Modal::ConnectSetup => {
                self.render_connect_setup(frame, centered(frame.area(), 86, 86));
            }
            Modal::ConnectError => {
                self.render_connect_error(frame, centered(frame.area(), 86, 80));
            }
            Modal::DisconnectConfirm => {
                self.render_disconnect_confirm(frame, centered(frame.area(), 72, 42));
            }
            Modal::UserMessage => {
                self.render_user_menu(frame, centered(frame.area(), 52, 26));
            }
            Modal::RevertConfirm => {
                self.render_revert_confirm(frame, centered(frame.area(), 64, 30));
            }
            Modal::Mcp => {
                super::management::render_mcp(
                    frame,
                    centered(frame.area(), 88, 82),
                    &mut self.mcp_panel,
                    &self.theme,
                );
            }
            Modal::Permissions => {
                super::management::render_permissions(
                    frame,
                    centered(frame.area(), 82, 76),
                    &mut self.permission_panel,
                    &self.theme,
                );
            }
            Modal::Skills => {
                super::management::render_skills(
                    frame,
                    centered(frame.area(), 88, 76),
                    &mut self.skill_panel,
                    &self.theme,
                );
            }
            Modal::Usage => {
                super::management::render_usage(
                    frame,
                    centered(frame.area(), 86, 78),
                    &mut self.usage_panel,
                    &self.theme,
                );
            }
            Modal::None => {}
        }
        if self.command_palette_visible() {
            self.render_command_palette(frame, centered(frame.area(), 68, 60));
        }
        // Selection sits beneath hover: both are pure cell-style patches,
        // and the hover affordance always wins where they overlap.
        self.apply_selection(frame);
        // Hover is the very last pass: a pure cell-style patch over whatever
        // was rendered, so it can never change layout or hit geometry.
        self.apply_hover(frame);
    }

    fn bottom_bar_line(&self, width: u16) -> BottomBarRender {
        let width = usize::from(width);
        if width == 0 {
            return BottomBarRender {
                line: Line::default(),
                mode_span: None,
                cost_span: None,
            };
        }

        let cwd = self
            .selected_session_meta()
            .map(|meta| meta.cwd_identity.as_str())
            .or_else(|| {
                self.selected
                    .and_then(|session_id| self.store.sessions.get(&session_id))
                    .and_then(|state| state.cwd_identity.as_ref())
                    .map(cookie_agent_protocol::CwdIdentity::as_str)
            })
            .map(shorten_home)
            .unwrap_or_else(|| "—".into());

        let state = self
            .selected
            .and_then(|session_id| self.store.sessions.get(&session_id));
        let context_tokens = state.and_then(|state| state.context_tokens);
        // No token data → no context segment at all; a bare dash would be
        // noise. The percentage degrades away before the count does.
        let context = context_tokens.map(|tokens| format!("ctx {}", format_token_count(tokens)));
        let context_limit = self
            .draft
            .as_ref()
            .and_then(|draft| self.model_descriptor(&draft.model.model))
            .or_else(|| {
                state
                    .and_then(latest_resolved_model_key)
                    .and_then(|key| self.model_descriptor(key))
            })
            .map(|descriptor| descriptor.capabilities.context_tokens);
        let context_with_percentage = match (context_tokens, context_limit) {
            (Some(tokens), Some(limit)) => {
                let percentage = tokens.saturating_mul(100).saturating_add(limit / 2) / limit;
                context
                    .as_ref()
                    .map(|context| format!("{context} ({percentage}%)"))
            }
            _ => context.clone(),
        };
        let cost = state
            .and_then(|state| state.estimated_cost_usd)
            .map(format_cost_usd);
        let mode = self
            .selected
            .map(|session_id| self.permission_mode(session_id))
            .unwrap_or_default();
        let mode = permission_mode_label(mode);
        let hint = "`ctrl+p` commands";
        #[derive(Clone, Copy)]
        struct Candidate<'a> {
            cost: Option<&'a str>,
            context: Option<&'a str>,
            hint: bool,
        }
        let render_candidate = |candidate: Candidate<'_>| {
            let mut rendered = mode.to_owned();
            if let Some(cost) = candidate.cost {
                rendered.push_str("    ");
                rendered.push_str(cost);
            }
            if let Some(context) = candidate.context {
                rendered.push_str("    ");
                rendered.push_str(context);
            }
            if candidate.hint {
                rendered.push_str("    ");
                rendered.push_str(hint);
            }
            rendered
        };
        let mut candidates = Vec::with_capacity(5);
        candidates.push(Candidate {
            cost: cost.as_deref(),
            context: context_with_percentage.as_deref(),
            hint: true,
        });
        candidates.push(Candidate {
            cost: cost.as_deref(),
            context: context_with_percentage.as_deref(),
            hint: false,
        });
        if cost.is_some() && context_with_percentage.is_some() {
            candidates.push(Candidate {
                cost: None,
                context: context_with_percentage.as_deref(),
                hint: false,
            });
        }
        if context_with_percentage != context {
            candidates.push(Candidate {
                cost: None,
                context: context.as_deref(),
                hint: false,
            });
        }
        candidates.push(Candidate {
            cost: None,
            context: None,
            hint: false,
        });
        let selected = candidates.into_iter().find(|candidate| {
            UnicodeWidthStr::width(render_candidate(*candidate).as_str()) <= width
        });
        let right = selected.map_or_else(|| truncate_with_ellipsis(mode, width), render_candidate);
        let right_width = UnicodeWidthStr::width(right.as_str()).min(width);
        let right_start = width.saturating_sub(right_width);
        let left_width = right_start.saturating_sub(4);
        let left = truncate_with_ellipsis(&cwd, left_width);
        let padding = right_start.saturating_sub(UnicodeWidthStr::width(left.as_str()));
        let mut spans = vec![Span::styled(
            format!("{left}{}", " ".repeat(padding)),
            self.theme.muted(),
        )];
        let mode_text = selected.map_or(right.as_str(), |_| mode);
        let mode_span = (!mode_text.is_empty()).then_some(spans.len());
        spans.push(Span::styled(mode_text.to_owned(), self.theme.link()));
        let mut cost_span = None;
        if let Some(candidate) = selected {
            if let Some(cost) = candidate.cost {
                spans.push(Span::styled("    ", self.theme.muted()));
                cost_span = Some(spans.len());
                spans.push(Span::styled(cost.to_owned(), self.theme.muted()));
            }
            if let Some(context) = candidate.context {
                spans.push(Span::styled("    ", self.theme.muted()));
                spans.push(Span::styled(context.to_owned(), self.theme.muted()));
            }
            if candidate.hint {
                spans.push(Span::styled("    ", self.theme.muted()));
                spans.push(Span::styled(hint, self.theme.muted()));
            }
        }
        BottomBarRender {
            line: Line::from(spans),
            mode_span,
            cost_span,
        }
    }

    #[cfg(test)]
    pub(crate) fn draw_for_test(&mut self, frame: &mut ratatui::Frame) {
        self.draw(frame);
    }

    pub(super) fn render_tree(
        &mut self,
        frame: &mut ratatui::Frame,
        area: Rect,
        entries: &[(SessionId, SessionMeta, usize)],
    ) {
        // The Agents panel has exactly clamp(visible row count, 1, 8) text
        // rows, with its borders outside that count.
        let text_rows = entries.len().clamp(1, 8) as u16;
        let panel_height = text_rows.saturating_add(2).min(area.height);
        let panel = Rect::new(area.x, area.y, area.width, panel_height);
        let inner = inner_rect(panel);
        self.tree_viewport_height = usize::from(inner.height);
        if self.tree_cursor.is_none() {
            self.tree_cursor = self
                .selected
                .filter(|selected| entries.iter().any(|(id, _, _)| id == selected))
                .or_else(|| entries.first().map(|(id, _, _)| *id));
        }
        self.clamp_tree_view_with(entries);
        let cursor_index = self.tree_cursor_index(entries);
        self.hit_map.tree = Some(inner);
        self.hit_map.tree_rows = entries
            .iter()
            .enumerate()
            .skip(self.tree_offset)
            .take(usize::from(inner.height))
            .enumerate()
            .map(|(row, (_, (session_id, _, depth)))| {
                // Expand geometry is projected from immutable hierarchy data,
                // never from cursor/watch prefixes that change with focus.
                let expand_rect = (*depth > 0
                    && self
                        .tree
                        .as_ref()
                        .and_then(|tree| find_node(tree, *session_id))
                        .is_some_and(|node| !node.children.is_empty()))
                .then(|| {
                    let indent_column = 2usize.saturating_mul(depth.saturating_sub(1));
                    Rect::new(
                        inner
                            .x
                            .saturating_add(2)
                            .saturating_add(u16::try_from(indent_column).unwrap_or(u16::MAX)),
                        inner.y + u16::try_from(row).unwrap_or(u16::MAX),
                        1,
                        1,
                    )
                });
                TreeRowHit {
                    rect: Rect::new(
                        inner.x,
                        inner.y + u16::try_from(row).unwrap_or(u16::MAX),
                        inner.width,
                        1,
                    ),
                    session_id: *session_id,
                    expand_rect,
                }
            })
            .collect();
        let rows = entries
            .iter()
            .enumerate()
            .skip(self.tree_offset)
            .take(usize::from(inner.height))
            .map(|(index, entry)| {
                let label = self.tree_row_label(entry, cursor_index == Some(index));
                // Rows render in plain body styling: the watched session is
                // marked only by its `●` glyph, never a persistent color.
                // The keyboard cursor keeps its `>` marker plus assistant
                // accent, and click-action hover keeps the glaze patch.
                let mut line = Line::from(Span::styled(label, self.theme.body()));
                if cursor_index == Some(index) {
                    line = line.style(self.theme.assistant());
                }
                line
            })
            .collect::<Vec<_>>();
        if entries.is_empty() {
            frame.render_widget(
                List::new(vec!["No sessions yet · /new starts one"])
                    .style(self.theme.muted())
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(self.theme.panel_border())
                            .title("Agents"),
                    ),
                panel,
            );
            return;
        }
        frame.render_widget(
            List::new(rows).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(self.theme.panel_border())
                    .title("Agents"),
            ),
            panel,
        );
    }

    /// One tree row: exactly `agent-id:session-title` with the shortened ID
    /// as subdued secondary metadata. The watched session gets a `●` marker
    /// and the cursor a `>` marker; the expand marker sits at the row's
    /// indent depth so its hit region is stable.
    pub(super) fn tree_row_label(
        &self,
        (session_id, session, depth): &(SessionId, SessionMeta, usize),
        cursor: bool,
    ) -> String {
        let has_children = self
            .tree
            .as_ref()
            .and_then(|tree| find_node(tree, *session_id))
            .is_some_and(|node| !node.children.is_empty());
        let marker = if !has_children {
            " "
        } else if self.collapsed_sessions.contains(session_id) {
            "+"
        } else {
            "-"
        };
        let indent = if *depth == 0 {
            String::new()
        } else {
            format!("{}{} ", "  ".repeat(depth - 1), marker)
        };
        let watched = if self.selected == Some(*session_id) {
            "● "
        } else {
            // Keep the watch-marker column reserved on every row. Without
            // this padding, selecting the root removes two leading cells from
            // every unselected descendant and visually cancels one depth.
            "  "
        };
        let cursor = if cursor { "> " } else { "  " };
        let status = match session.status {
            SessionStatus::Running => "⏳ ",
            SessionStatus::Idle | SessionStatus::Completed => "✅ ",
            SessionStatus::Failed | SessionStatus::Cancelled | SessionStatus::Interrupted => "   ",
        };
        let title = session
            .title
            .as_ref()
            .map(SessionTitle::to_string)
            .unwrap_or_else(|| "untitled".to_owned());
        let degraded = if session.skipped_events.is_empty() {
            ""
        } else {
            " !"
        };
        // Primary text is exactly `agent-id:session-title`; hierarchy,
        // cursor, and watch markers live in prefix cells only, and the row
        // shows no session ID.
        format!(
            "{cursor}{indent}{watched}{status}{agent}:{title}{degraded}",
            agent = session.creation_selection.agent,
        )
    }

    pub(super) fn tree_entries(&self) -> Vec<(SessionId, SessionMeta, usize)> {
        let mut entries = Vec::new();
        if let Some(tree) = &self.tree {
            flatten_tree(
                tree,
                0,
                &self.collapsed_sessions,
                &self.store.sessions,
                &mut entries,
            );
        }
        // A session whose event log still holds only `SessionCreated`
        // (`last_event_seq == 1`, so no `UserInputSubmitted` yet) is an
        // empty, memory-only ghost on the engine side: it never renders as
        // a panel row. The watched session stays fully usable while hidden
        // and its row appears normally once its first message lands. A
        // rename bumps the sequence via `SessionTitleCommitted`, so a named
        // empty session stays visible — an explicit user act, not a ghost.
        entries.retain(|(_, meta, _)| meta.last_event_seq > 1);
        entries
    }

    pub(super) fn render_approval(
        &mut self,
        frame: &mut ratatui::Frame,
        approval: &ApprovalState,
        area: Rect,
    ) -> Vec<ApprovalHit> {
        paint_panel(frame, area, &self.theme);
        let request = (approval.approval_id, approval.request_revision);
        if self.approval_scroll_request != Some(request) {
            self.approval_scroll_request = Some(request);
            self.approval_scroll = 0;
        }
        let actions = approval_action_hits(area, approval);
        let actions_height = actions.first().map_or(0, |hit| hit.rect.height);
        let spacer = u16::from(actions_height > 1);
        let inner = inner_rect(area);
        let body = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner
                .height
                .saturating_sub(actions_height)
                .saturating_sub(spacer),
        );
        let content = approval_content(approval);
        let lines = content
            .lines()
            .flat_map(|line| {
                wrapped_line(
                    Line::from(Span::styled(
                        line.to_owned(),
                        approval_line_style(line, &self.theme),
                    )),
                    body.width,
                )
            })
            .collect::<Vec<_>>();
        let line_count = lines.len().min(usize::from(u16::MAX)) as u16;
        let paragraph = Paragraph::new(lines);
        self.approval_max_scroll = line_count.saturating_sub(body.height);
        self.approval_scroll = self.approval_scroll.min(self.approval_max_scroll);
        let visible_end = self
            .approval_scroll
            .saturating_add(body.height)
            .min(line_count);
        let visible_start = self
            .approval_scroll
            .saturating_add(1)
            .min(line_count.max(1));
        let title = if area.width < 48 {
            format!("Approval {visible_start}–{visible_end}/{line_count} ↑↓")
        } else {
            format!(
                "Approval · lines {visible_start}–{visible_end}/{line_count} · ↑/↓ PgUp/PgDn Home/End"
            )
        };
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(self.theme.warning())
                .title(Span::styled(title, self.theme.heading()))
                .style(self.theme.panel()),
            area,
        );
        frame.render_widget(paragraph.scroll((self.approval_scroll, 0)), body);
        render_approval_actions(frame, &actions, &self.theme);
        actions
    }

    pub(super) fn render_picker(
        &mut self,
        frame: &mut ratatui::Frame,
        title: &str,
        entries: Vec<String>,
        empty_message: Option<&str>,
        area: Rect,
        hint: Option<&str>,
    ) {
        self.clamp_picker_selection();
        self.hit_map.picker = Some(area);
        self.hit_map.picker_rows = super::pickers::render(
            frame,
            super::pickers::PickerChrome {
                title,
                empty_message,
                hint,
            },
            entries,
            area,
            &mut self.picker_state,
            &self.theme,
        )
        .into_iter()
        .map(|(rect, index)| PickerRowHit { rect, index })
        .collect();
    }

    fn render_agent_picker(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let input_height = 3.min(area.height);
        let input_area = Rect::new(area.x, area.y, area.width, input_height);
        let rendered_input = super::pickers::render_search_input(
            frame,
            input_area,
            &mut self.agent_search,
            &self.theme,
        );
        self.hit_map.picker_input = Some(InputHit {
            rect: input_area,
            text_rect: rendered_input.text_rect,
            scrollbar: None,
        });

        let picker = Rect::new(
            area.x,
            area.y.saturating_add(input_height),
            area.width,
            area.height.saturating_sub(input_height),
        );
        self.clamp_picker_selection();
        let selected = self.picker_state.selected();
        let total = self.agent_picker_candidates().len();
        let agents = self.filtered_agent_picker_candidates();
        let match_count = agents.len();
        let row_width = usize::from(inner_rect(picker).width.saturating_sub(2));
        let entries = agents
            .iter()
            .enumerate()
            .map(|(index, agent)| {
                agent_picker_row(agent, row_width, selected == Some(index), &self.theme)
            })
            .collect();
        let context = if self.new_session_draft.is_some() {
            format!(
                "preset: {} · {}",
                self.selected_preset_label(),
                self.descriptor_revisions_label()
            )
        } else {
            self.descriptor_revisions_label()
        };
        let title = format!("Agent ({match_count}/{total}) — {context}");
        let empty_message = if total == 0 {
            Some("No root-runnable agents are available.")
        } else if match_count == 0 {
            Some("No agents match the filter.")
        } else {
            None
        };

        self.hit_map.picker = Some(picker);
        self.hit_map.picker_rows = super::pickers::render_lines(
            frame,
            super::pickers::PickerChrome {
                title: &title,
                empty_message,
                hint: Some("↑↓ move · enter: select · esc: search"),
            },
            entries,
            picker,
            &mut self.picker_state,
            &self.theme,
            self.theme.selected_overlay(),
        )
        .into_iter()
        .map(|(rect, index)| PickerRowHit { rect, index })
        .collect();
    }

    fn render_model_picker(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let input_height = 3.min(area.height);
        let input_area = Rect::new(area.x, area.y, area.width, input_height);
        let rendered_input = super::pickers::render_search_input(
            frame,
            input_area,
            &mut self.model_search,
            &self.theme,
        );
        self.hit_map.picker_input = Some(InputHit {
            rect: input_area,
            text_rect: rendered_input.text_rect,
            scrollbar: None,
        });

        let picker = Rect::new(
            area.x,
            area.y.saturating_add(input_height),
            area.width,
            area.height.saturating_sub(input_height),
        );
        let models = self.filtered_draft_models();
        let total = self.draft_models().len();
        let title = format!("Model ({}/{total})", models.len());
        let row_width = usize::from(inner_rect(picker).width.saturating_sub(2));
        self.clamp_picker_selection();
        let selected = self.picker_state.selected();
        let entries = models
            .iter()
            .enumerate()
            .map(|(index, selection)| {
                model_picker_row(
                    selection,
                    self.model_descriptor(&selection.model)
                        .map(|descriptor| descriptor.display_name.as_str()),
                    row_width,
                    selected == Some(index),
                    &self.theme,
                )
            })
            .collect();
        let empty_message = if total == 0 {
            Some("No models are available for this draft.")
        } else if models.is_empty() {
            Some("No models match the filter.")
        } else {
            None
        };

        self.hit_map.picker = Some(picker);
        self.hit_map.picker_rows = super::pickers::render_lines(
            frame,
            super::pickers::PickerChrome {
                title: &title,
                empty_message,
                hint: Some("↑↓ move · enter: select · esc: search"),
            },
            entries,
            picker,
            &mut self.picker_state,
            &self.theme,
            self.theme.selected_overlay(),
        )
        .into_iter()
        .map(|(rect, index)| PickerRowHit { rect, index })
        .collect();
    }

    fn render_session_search(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let input_height = 3.min(area.height);
        let input_area = Rect::new(area.x, area.y, area.width, input_height);
        let rendered_input = super::pickers::render_search_input(
            frame,
            input_area,
            &mut self.session_search,
            &self.theme,
        );
        self.hit_map.picker_input = Some(InputHit {
            rect: input_area,
            text_rect: rendered_input.text_rect,
            // Single-row search inputs never reach the composer ceiling, so
            // they never carry a scrollbar.
            scrollbar: None,
        });

        let picker = Rect::new(
            area.x,
            area.y.saturating_add(input_height),
            area.width,
            area.height.saturating_sub(input_height),
        );
        self.hit_map.picker = Some(picker);
        self.clamp_picker_selection();
        self.refresh_session_search_rows_cache();
        let rows = &self.session_search_rows_cache.rows;
        let session_count = rows.iter().filter(|row| row.session_id().is_some()).count();
        let title = format!("Sessions ({session_count}/{})", self.sessions.len());
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_style(self.theme.panel_border())
                .title(title)
                .style(self.theme.panel()),
            picker,
        );
        let inner = inner_rect(picker);
        self.hit_map.picker_rows.clear();
        if session_count == 0 {
            frame.render_widget(
                Paragraph::new("No sessions match the filter.").style(self.theme.muted()),
                inner,
            );
            return;
        }

        let selected = self
            .picker_state
            .selected()
            .unwrap_or(0)
            .min(session_count - 1);
        let selected_visual = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.session_id().is_some())
            .nth(selected)
            .map_or(0, |(index, _)| index);
        let viewport_height = usize::from(inner.height);
        let max_start = rows.len().saturating_sub(viewport_height);
        let start = selected_visual
            .saturating_add(1)
            .saturating_sub(viewport_height)
            .min(max_start);
        let mut selectable_index = rows[..start]
            .iter()
            .filter(|row| row.session_id().is_some())
            .count();
        for (line, row) in rows.iter().skip(start).take(viewport_height).enumerate() {
            let row_area = Rect::new(
                inner.x,
                inner
                    .y
                    .saturating_add(u16::try_from(line).unwrap_or(u16::MAX)),
                inner.width,
                1,
            );
            match row {
                SessionSearchRow::Header(label) => {
                    frame.render_widget(
                        Paragraph::new(format!(
                            "  {}",
                            truncate_with_ellipsis(label, inner.width.saturating_sub(2).into())
                        ))
                        .style(self.theme.muted()),
                        row_area,
                    );
                }
                SessionSearchRow::Session { label, .. } => {
                    let is_selected = selectable_index == selected;
                    let prefix = if is_selected { "> " } else { "  " };
                    // Long titles ellipsize instead of hard-clipping at the
                    // panel edge; the agent/id metadata yields first.
                    let label = truncate_with_ellipsis(label, inner.width.saturating_sub(2).into());
                    let paragraph = Paragraph::new(format!("{prefix}{label}"));
                    frame.render_widget(
                        if is_selected {
                            paragraph.style(self.theme.selected())
                        } else {
                            paragraph
                        },
                        row_area,
                    );
                    self.hit_map.picker_rows.push(PickerRowHit {
                        rect: row_area,
                        index: selectable_index,
                    });
                    selectable_index += 1;
                }
            }
        }
    }

    fn render_connect_details(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let Some(provider) = self.connect_provider.as_ref() else {
            return;
        };
        let state = row_state(
            provider,
            &self.models,
            self.provider_operations.get(&provider.id),
        );
        let reason = provider
            .support
            .reason
            .as_ref()
            .map_or("none", |reason| reason.as_str());
        let quarantine = provider
            .quarantine
            .as_ref()
            .map_or("none".into(), |diagnostic| {
                format!("{}: {}", diagnostic.code, diagnostic.message)
            });
        let action = if let Some(ProviderOperation::InProgress(operation)) =
            self.provider_operations.get(&provider.id)
        {
            format!("{} in progress… · Esc: close", action_name(*operation))
        } else {
            match state {
                ProviderRowState::ConnectedReconnect if provider.durable_connection.is_some() => {
                    "R: reconnect/update · D: disconnect · Esc: close".into()
                }
                ProviderRowState::ConnectedReconnect => {
                    "R: reconnect/update · Esc: close".into()
                }
                ProviderRowState::Removed => {
                    "Removed from the current catalog; retained recipe matching permits reconnect/update. Frozen session models remain available through exact manifest rehydration. Esc: close".into()
                }
                ProviderRowState::Unsupported
                | ProviderRowState::Disconnected
                | ProviderRowState::ErrorRetry => "Enter: details only · Esc: close".into(),
            }
        };
        let content = format!(
            "{DURABLE_PROVIDER_COPY}\n\nProvider: {} ({})\nState: {:?}\nPresence: {:?}\nSupport: {:?}\nTyped reason: {reason}\nConfiguration: {:?}\nEffective auth: {:?}\nQuarantine: {quarantine}\nSetup fields: {}\nAuth methods: {}\n\n{action}",
            provider.display_name,
            provider.id,
            state,
            provider.presence,
            provider.support.state,
            provider.configuration,
            provider.effective_auth_state,
            provider.setup_fields.len(),
            provider.auth_methods.len(),
        );
        frame.render_widget(
            Paragraph::new(content).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(self.theme.panel_border())
                    .title("Provider details"),
            ),
            area,
        );
    }

    fn render_connect_setup(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let Some(form) = self.provider_form.as_mut() else {
            return;
        };
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(self.theme.panel_border())
            .title("Connect provider");
        let inner = outer.inner(area);
        frame.render_widget(outer, area);
        let auth_label = form.selected_auth().map_or_else(
            || form.auth_method.to_string(),
            |method| format!("{} ({})", method.display_name, method.id),
        );
        let focus = form.focus();
        let instructions = if form.can_disconnect {
            "Tab/Down: next · Shift-Tab/Up: previous · Enter: activate/submit · Esc: cancel · Ctrl-D: disconnect"
        } else {
            "Tab/Down: next · Shift-Tab/Up: previous · Enter: activate/submit · Esc: cancel"
        };
        // A validation failure renders inline above the fields instead of
        // taking over the panel; editing any value clears it.
        let header_height = if form.error.is_some() { 5 } else { 4 }.min(inner.height);
        let mut header = vec![
            Line::from(DURABLE_PROVIDER_COPY),
            Line::from(format!(
                "Provider: {} ({})",
                form.provider.display_name, form.provider.id
            )),
            Line::from(instructions),
        ];
        if let Some(error) = form.error.as_deref() {
            header.push(Line::from(Span::styled(
                error.to_owned(),
                self.theme.error(),
            )));
        }
        frame.render_widget(
            Paragraph::new(header).wrap(Wrap { trim: false }),
            Rect::new(inner.x, inner.y, inner.width, header_height),
        );
        let mut y = inner.y.saturating_add(header_height);
        if form.has_auth_selector() {
            let auth_area = Rect::new(inner.x, y, inner.width, 3.min(inner.bottom() - y));
            frame.render_widget(
                Paragraph::new(format!("← {auth_label} →")).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(
                            self.theme
                                .input_border(focus == ProviderFormFocus::AuthMethod),
                        )
                        .title("Authentication method · Left/Right/Space: change"),
                ),
                auth_area,
            );
            self.hit_map.provider_fields.push(ProviderFieldHit {
                rect: auth_area,
                text_rect: Rect::new(
                    auth_area.x.saturating_add(1),
                    auth_area.y.saturating_add(1),
                    auth_area.width.saturating_sub(2),
                    auth_area.height.saturating_sub(2),
                ),
                focus: ProviderFormFocus::AuthMethod,
            });
            y = y.saturating_add(3);
        } else {
            let height = 1.min(inner.bottom() - y);
            frame.render_widget(
                Paragraph::new(format!("Authentication method: {auth_label} · read-only")),
                Rect::new(inner.x, y, inner.width, height),
            );
            y = y.saturating_add(height);
        }
        for (index, field) in form.secrets.iter_mut().enumerate() {
            let height = 3.min(inner.bottom().saturating_sub(y));
            let field_area = Rect::new(inner.x, y, inner.width, height);
            let title = format!(
                "Credential: {} [{}]{} · {}",
                field.descriptor.display_name,
                field.descriptor.id,
                if field.descriptor.required {
                    " required"
                } else {
                    ""
                },
                field.descriptor.help
            );
            input::render_masked(
                frame,
                field_area,
                &mut field.input,
                focus == ProviderFormFocus::Credential(index),
                &title,
                &self.theme,
            );
            self.hit_map.provider_fields.push(ProviderFieldHit {
                rect: field_area,
                text_rect: Rect::new(
                    field_area.x.saturating_add(1),
                    field_area.y.saturating_add(1),
                    field_area.width.saturating_sub(2),
                    field_area.height.saturating_sub(2),
                ),
                focus: ProviderFormFocus::Credential(index),
            });
            y = y.saturating_add(height);
            let helper_height = 1.min(inner.bottom().saturating_sub(y));
            frame.render_widget(
                Paragraph::new("Credentials are verified on first use."),
                Rect::new(
                    inner.x.saturating_add(1),
                    y,
                    inner.width.saturating_sub(1),
                    helper_height,
                ),
            );
            y = y.saturating_add(helper_height);
        }
        for (index, field) in form.setup.iter_mut().enumerate() {
            let height = 3.min(inner.bottom().saturating_sub(y));
            let field_area = Rect::new(inner.x, y, inner.width, height);
            let secret = !field.descriptor.safe_to_project;
            let title = format!(
                "Setup: {} [{}]{}{} · {}",
                field.descriptor.display_name,
                field.descriptor.id,
                if field.descriptor.required {
                    " required"
                } else {
                    ""
                },
                if secret { " secret" } else { "" },
                field.descriptor.help
            );
            let focused = focus == ProviderFormFocus::Setup(index);
            if secret {
                input::render_masked(
                    frame,
                    field_area,
                    &mut field.input,
                    focused,
                    &title,
                    &self.theme,
                );
            } else {
                input::render(
                    frame,
                    field_area,
                    field.input.state_mut(),
                    focused,
                    title.clone(),
                    // Setup fields carry their display name and help text;
                    // a placeholder could read as a prefilled default.
                    None,
                    &self.theme,
                );
            }
            self.hit_map.provider_fields.push(ProviderFieldHit {
                rect: field_area,
                text_rect: Rect::new(
                    field_area.x.saturating_add(1),
                    field_area.y.saturating_add(1),
                    field_area.width.saturating_sub(2),
                    field_area.height.saturating_sub(2),
                ),
                focus: ProviderFormFocus::Setup(index),
            });
            y = y.saturating_add(height);
        }
        let submit_label = if form.reconnect {
            "Reconnect"
        } else {
            "Connect"
        };
        let buttons_height = 3.min(inner.bottom().saturating_sub(y));
        // Two compact buttons centered as one group, never a panel-wide
        // strip. Width is the label plus a one-column border each side; a
        // two-cell gutter separates the frames.
        let button_width = |text: &str| {
            (u16::try_from(text.len()).unwrap_or(u16::MAX))
                .saturating_add(4)
                .min(inner.width)
        };
        let submit_width = button_width(submit_label);
        let cancel_width = button_width("Cancel");
        let group_width = submit_width.saturating_add(2).saturating_add(cancel_width);
        let group_x = inner
            .x
            .saturating_add(inner.width.saturating_sub(group_width) / 2);
        let submit_area = Rect::new(group_x, y, submit_width, buttons_height);
        let cancel_x = group_x
            .saturating_add(submit_width)
            .saturating_add(2)
            .min(inner.right().saturating_sub(cancel_width));
        let cancel_area = Rect::new(cancel_x, y, cancel_width, buttons_height);
        let submit_style = self.theme.input_border(focus == ProviderFormFocus::Submit);
        let cancel_style = self.theme.input_border(focus == ProviderFormFocus::Cancel);
        render_connect_button(frame, submit_area, submit_label, submit_style);
        render_connect_button(frame, cancel_area, "Cancel", cancel_style);
        self.hit_map.provider_submit = Some(submit_area);
        self.hit_map.provider_cancel = Some(cancel_area);
    }

    fn render_connect_error(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let Some(form) = self.provider_form.as_ref() else {
            return;
        };
        let error = form.error.as_deref().unwrap_or("Unknown connect error.");
        let content = format!(
            "{DURABLE_PROVIDER_COPY}\n\nProvider: {} ({})\nAuthentication method: {}\n\nConnect failed:\n{error}\n\nNo credentials were verified. Credentials are verified on first use.\n\nPress Esc to return to the form, edit values, and retry.",
            form.provider.display_name, form.provider.id, form.auth_method
        );
        frame.render_widget(
            Paragraph::new(content).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(self.theme.panel_border())
                    .title("Provider connection error"),
            ),
            area,
        );
    }

    fn render_disconnect_confirm(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let Some(provider) = self.connect_provider.as_ref() else {
            return;
        };
        let content = format!(
            "{DURABLE_PROVIDER_COPY}\n\nDisconnect {} ({})?\nThis removes both stored public setup and stored credentials. Authored configuration is unchanged.\n\nPress Enter/Y to disconnect or Esc/N to cancel.",
            provider.display_name, provider.id
        );
        frame.render_widget(
            Paragraph::new(content).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(self.theme.panel_border())
                    .title("Confirm provider disconnect"),
            ),
            area,
        );
    }

    pub(super) fn render_command_palette(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.clamp_palette_selection();
        let entries = self.palette_entries();
        let query = self.input.as_str().strip_prefix('/').unwrap_or_default();
        let labels = entries
            .iter()
            .map(|entry| entry.label())
            .collect::<Vec<_>>();
        self.hit_map.palette = Some(area);
        self.hit_map.palette_rows = super::slash::render(
            frame,
            query,
            labels,
            area,
            &mut self.palette_state,
            &self.theme,
        )
        .into_iter()
        .map(|(rect, index)| PaletteRowHit { rect, index })
        .collect();
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

fn is_approval_scroll_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
    )
}

/// The exact `Agent • Model[Variant]` draft-selection title form.
fn draft_title(draft: &RunSelection) -> String {
    let variant = draft
        .model
        .variant
        .as_ref()
        .map_or_else(|| "base".to_owned(), |variant| variant.to_string());
    format!("{} • {}[{}]", draft.agent, draft.model.model, variant)
}

fn agent_picker_row(
    agent: &AgentDescriptor,
    width: usize,
    selected: bool,
    theme: &Theme,
) -> Line<'static> {
    let id = truncate_with_ellipsis(agent.id.as_str(), width);
    let id_width = UnicodeWidthStr::width(id.as_str());
    let id_style = theme.body().add_modifier(Modifier::BOLD);
    let id_style = if selected {
        id_style.patch(theme.selected())
    } else {
        id_style
    };
    let mut spans = vec![Span::styled(id, id_style)];
    if !agent.description.trim().is_empty() && id_width < width {
        let description_style = if selected {
            theme.internal().patch(theme.selected_overlay())
        } else {
            theme.internal()
        };
        spans.push(Span::styled(
            truncate_with_ellipsis(
                &format!(" {}", agent.description),
                width.saturating_sub(id_width),
            ),
            description_style,
        ));
    }
    Line::from(spans)
}

fn model_picker_row(
    selection: &ModelSelection,
    display_name: Option<&str>,
    width: usize,
    selected: bool,
    theme: &Theme,
) -> Line<'static> {
    let variant = selection.variant.as_ref().map_or("base", VariantId::as_str);
    let canonical = format!("{}[{variant}]", selection.model);
    let Some(display_name) = display_name else {
        let style = if selected {
            theme.body().patch(theme.selected())
        } else {
            theme.body()
        };
        return Line::from(Span::styled(
            truncate_with_ellipsis(&canonical, width),
            style,
        ));
    };

    let display = truncate_with_ellipsis(display_name, width);
    let display_width = UnicodeWidthStr::width(display.as_str());
    let display_style = theme.body().add_modifier(Modifier::BOLD);
    let display_style = if selected {
        display_style.patch(theme.selected())
    } else {
        display_style
    };
    let mut spans = vec![Span::styled(display, display_style)];
    if display_width < width {
        let key_style = if selected {
            theme.internal().patch(theme.selected_overlay())
        } else {
            theme.internal()
        };
        spans.push(Span::styled(
            truncate_with_ellipsis(&format!(" {canonical}"), width - display_width),
            key_style,
        ));
    }
    Line::from(spans)
}

/// Extract an immediate title patch from a `SessionTitleCommitted` event:
/// (session, new title, authoritative sequence).
fn title_change_from_event(
    event: &cookie_agent_protocol::StoredEvent,
) -> Option<(SessionId, Option<SessionTitle>, u64)> {
    let EventPayload::SessionTitleCommitted { change, .. } = &event.payload else {
        return None;
    };
    let title = match change {
        SessionTitleChange::UserSet { title, .. }
        | SessionTitleChange::InternalAgentSet { title, .. }
        | SessionTitleChange::DelegatedSet { title, .. }
        | SessionTitleChange::FallbackSet { title } => Some(title.clone()),
        SessionTitleChange::UserClear { .. } | SessionTitleChange::UserReset { .. } => None,
    };
    Some((event.session_id, title, event.seq))
}

/// Mirror the engine session projection's run lifecycle status derivation.
pub(super) fn status_change_from_event(
    event: &cookie_agent_protocol::StoredEvent,
) -> Option<(SessionId, SessionStatus, u64)> {
    let status = match &event.payload {
        EventPayload::RunStarted { .. } => SessionStatus::Running,
        EventPayload::RunCompleted { .. } => SessionStatus::Completed,
        EventPayload::RunFailed { .. } => SessionStatus::Failed,
        EventPayload::RunCancelled { .. } => SessionStatus::Cancelled,
        EventPayload::RunInterrupted { .. } => SessionStatus::Interrupted,
        EventPayload::DelegateChildTerminated { status, .. } => *status,
        _ => return None,
    };
    Some((event.session_id, status, event.seq))
}

/// Collect the newest known title for each session from the live tree and
/// session list, so stale patches can be repaired with the newer value.
fn collect_known_titles(
    tree: Option<&SessionTree>,
    sessions: &[SessionMeta],
    sequences: &HashMap<SessionId, u64>,
    titles: &mut HashMap<SessionId, (u64, Option<SessionTitle>)>,
) {
    fn walk(node: &SessionTree, titles: &mut HashMap<SessionId, (u64, Option<SessionTitle>)>) {
        titles.insert(
            node.session.session_id,
            (node.session.title_updated_seq, node.session.title.clone()),
        );
        for child in &node.children {
            walk(child, titles);
        }
    }
    if let Some(tree) = tree {
        walk(tree, titles);
    }
    for session in sessions {
        titles
            .entry(session.session_id)
            .and_modify(|entry| {
                if session.title_updated_seq > entry.0 {
                    *entry = (session.title_updated_seq, session.title.clone());
                }
            })
            .or_insert((session.title_updated_seq, session.title.clone()));
    }
    // Any session with a newer sequence but no meta patch yet keeps its
    // recorded sequence so stale tree values cannot regress it.
    for (session_id, seq) in sequences {
        titles
            .entry(*session_id)
            .and_modify(|entry| entry.0 = entry.0.max(*seq))
            .or_insert((*seq, None));
    }
}

fn collect_known_statuses(
    tree: Option<&SessionTree>,
    sessions: &[SessionMeta],
    statuses: &mut HashMap<SessionId, (u64, SessionStatus)>,
) {
    fn record(meta: &SessionMeta, statuses: &mut HashMap<SessionId, (u64, SessionStatus)>) {
        statuses
            .entry(meta.session_id)
            .and_modify(|entry| {
                if meta.last_event_seq > entry.0 {
                    *entry = (meta.last_event_seq, meta.status);
                }
            })
            .or_insert((meta.last_event_seq, meta.status));
    }

    fn walk(node: &SessionTree, statuses: &mut HashMap<SessionId, (u64, SessionStatus)>) {
        record(&node.session, statuses);
        for child in &node.children {
            walk(child, statuses);
        }
    }

    if let Some(tree) = tree {
        walk(tree, statuses);
    }
    for session in sessions {
        record(session, statuses);
    }
}

fn patch_tree_node_statuses(
    tree: &mut SessionTree,
    statuses: &HashMap<SessionId, (u64, SessionStatus)>,
) {
    if let Some((seq, status)) = statuses.get(&tree.session.session_id)
        && tree.session.last_event_seq < *seq
    {
        tree.session.last_event_seq = *seq;
        tree.session.status = *status;
    }
    for child in &mut tree.children {
        patch_tree_node_statuses(child, statuses);
    }
}

fn patch_tree_node_titles(
    tree: &mut SessionTree,
    sequences: &HashMap<SessionId, u64>,
    known: &HashMap<SessionId, (u64, Option<SessionTitle>)>,
) {
    let session_id = tree.session.session_id;
    let known_seq = sequences.get(&session_id).copied().unwrap_or(0);
    if tree.session.title_updated_seq < known_seq {
        // The tree response is older than a title event already applied;
        // restore the newest known title and sequence.
        tree.session.title = known.get(&session_id).and_then(|(_, title)| title.clone());
        tree.session.title_updated_seq = known_seq;
    }
    for child in &mut tree.children {
        patch_tree_node_titles(child, sequences, known);
    }
}

pub(super) fn approval_content(approval: &ApprovalState) -> String {
    let mut content = String::new();
    writeln!(
        content,
        "PERMISSION REQUIRED{}",
        if approval.escalated {
            " · ESCALATED"
        } else {
            ""
        }
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "consent target: {}",
        approval.evaluations[0].trace.normalized_resource
    )
    .expect("writing to a String cannot fail");
    writeln!(content, "approval id: {}", approval.approval_id)
        .expect("writing to a String cannot fail");
    writeln!(content, "request revision: {}", approval.request_revision)
        .expect("writing to a String cannot fail");
    writeln!(content, "trigger: {:?}", approval.trigger).expect("writing to a String cannot fail");
    writeln!(
        content,
        "operation fingerprint: {}",
        approval.operation_fingerprint.digest()
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "normalized-arguments digest: {}",
        approval.normalized_arguments_digest
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "execution-context digest: {}",
        approval.execution_context_digest
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "prepared capability lifetime: {:?}",
        approval.capability_lifetime
    )
    .expect("writing to a String cannot fail");

    writeln!(content, "\nCAPABILITIES ({})", approval.capabilities.len())
        .expect("writing to a String cannot fail");
    for (index, capability) in approval.capabilities.iter().enumerate() {
        writeln!(
            content,
            "{}. action: {:?}\n   operation: {}\n   lifetime: {:?}",
            index + 1,
            capability.action,
            capability.operation.as_str(),
            approval.capability_lifetime
        )
        .expect("writing to a String cannot fail");
    }

    writeln!(content, "\nRESOURCES ({})", approval.resources.len())
        .expect("writing to a String cannot fail");
    for (index, resource) in approval.resources.iter().enumerate() {
        let normalized = approval
            .evaluations
            .iter()
            .find(|evaluation| evaluation.resource_digest == resource.binding_digest)
            .expect("validated approval evaluations cover every resource")
            .trace
            .normalized_resource
            .as_str();
        writeln!(
            content,
            "{}. action: {:?}\n   normalized identity: {}\n   canonical identity: {}\n   binding digest: {}\n   boundary: {}\n   binding lifetime: {:?}\n   source: {:?}",
            index + 1,
            resource.capability,
            normalized,
            resource.canonical.as_str(),
            resource.binding_digest.digest(),
            approval_boundary(&resource.boundary),
            resource.binding_lifetime,
            resource.source
        )
        .expect("writing to a String cannot fail");
    }

    writeln!(content, "\nEVALUATIONS ({})", approval.evaluations.len())
        .expect("writing to a String cannot fail");
    for (index, evaluation) in approval.evaluations.iter().enumerate() {
        writeln!(
            content,
            "{}. resource binding digest: {}\n   result effect: {:?}\n   trace action: {:?}\n   trace normalized resource: {}\n   trace effect: {:?}\n   precedence reason: {}\n   candidate rules ({}):",
            index + 1,
            evaluation.resource_digest.digest(),
            evaluation.effect,
            evaluation.trace.action,
            evaluation.trace.normalized_resource,
            evaluation.trace.effect,
            evaluation.trace.precedence_reason,
            evaluation.trace.candidates.len()
        )
        .expect("writing to a String cannot fail");
        if evaluation.trace.candidates.is_empty() {
            writeln!(content, "      (none)").expect("writing to a String cannot fail");
        } else {
            for (candidate_index, candidate) in evaluation.trace.candidates.iter().enumerate() {
                writeln!(
                    content,
                    "      {}. action: {:?} · resource: {} · source layer: {} · effect: {:?}",
                    candidate_index + 1,
                    candidate.action,
                    candidate.resource,
                    candidate.source_layer,
                    candidate.effect
                )
                .expect("writing to a String cannot fail");
            }
        }
    }

    writeln!(content, "\nRESPONSE CONSTRAINTS").expect("writing to a String cannot fail");
    writeln!(
        content,
        "allow approve once: {}",
        approval.constraints.allow_once
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "allow delegation-tree grant: {}",
        approval.constraints.allow_tree_grant
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "allow cancel: {}",
        approval.constraints.cancellable
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "expires at: {}",
        approval
            .constraints
            .expires_at
            .map_or_else(|| "never".into(), |timestamp| timestamp.to_string())
    )
    .expect("writing to a String cannot fail");
    content
}

fn approval_boundary(boundary: &cookie_agent_protocol::ApprovalBoundary) -> String {
    match boundary {
        cookie_agent_protocol::ApprovalBoundary::Exact => "exact".into(),
        cookie_agent_protocol::ApprovalBoundary::CommandPrefix { prefix } => {
            format!("command prefix: {prefix}")
        }
        cookie_agent_protocol::ApprovalBoundary::DelegationTree { root_session_id } => {
            format!("delegation tree rooted at session {root_session_id}")
        }
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
        frame.render_widget(
            Block::default().borders(Borders::ALL).border_style(style),
            area,
        );
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

/// Visual hierarchy for the approval body: the banner is a warning, the
/// consent target is the prominent identity, section headers are headings,
/// identity digests recede, and the remaining evidence is body text. The
/// content itself is produced by `approval_content` unchanged.
fn approval_line_style(line: &str, theme: &Theme) -> ratatui::style::Style {
    if line.starts_with("PERMISSION REQUIRED") {
        return theme.warning();
    }
    if line.starts_with("consent target:") {
        return theme.user();
    }
    if [
        "CAPABILITIES (",
        "RESOURCES (",
        "EVALUATIONS (",
        "RESPONSE CONSTRAINTS",
    ]
    .iter()
    .any(|header| line.starts_with(header))
    {
        return theme.heading();
    }
    if [
        "approval id:",
        "request revision:",
        "trigger:",
        "operation fingerprint:",
        "normalized-arguments digest:",
        "execution-context digest:",
        "prepared capability lifetime:",
    ]
    .iter()
    .any(|key| line.starts_with(key))
    {
        return theme.internal();
    }
    theme.body()
}

fn decision_tone(decision: ApprovalUserDecision) -> crate::theme::DecisionTone {
    match decision {
        ApprovalUserDecision::ApproveOnce | ApprovalUserDecision::ApproveTree => {
            crate::theme::DecisionTone::Allow
        }
        ApprovalUserDecision::Reject => crate::theme::DecisionTone::Deny,
        ApprovalUserDecision::Cancel => crate::theme::DecisionTone::Neutral,
    }
}

fn approval_action_hits(area: Rect, approval: &ApprovalState) -> Vec<ApprovalHit> {
    let inner = inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return Vec::new();
    }
    // Roomy panels get three-row rounded buttons; cramped ones keep the
    // single action row. Heights here and in `render_approval` must agree.
    let height = if inner.width >= 44 && inner.height >= 10 {
        3
    } else {
        1
    };
    let row = Rect::new(
        inner.x,
        inner.y + inner.height - height,
        inner.width,
        height,
    );
    let mut decisions = Vec::new();
    if approval.constraints.allow_once {
        decisions.push(ApprovalUserDecision::ApproveOnce);
    }
    if approval.constraints.allow_tree_grant {
        decisions.push(ApprovalUserDecision::ApproveTree);
    }
    decisions.push(ApprovalUserDecision::Reject);
    if approval.constraints.cancellable {
        decisions.push(ApprovalUserDecision::Cancel);
    }
    if decisions.is_empty() {
        return Vec::new();
    }
    let width = 100 / u16::try_from(decisions.len()).unwrap_or(1);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints(decisions.iter().map(|_| Constraint::Percentage(width)))
        .split(row)
        .iter()
        .zip(decisions)
        .map(|(rect, decision)| ApprovalHit {
            rect: *rect,
            decision,
        })
        .collect()
}

/// Render each decision as a distinct button: a rounded frame in the
/// decision's tone with a glyph-bearing label (never color alone). Buttons
/// leave a one-column visual gap between frames while their hit regions
/// stay contiguous; single-row areas fall back to flat labels.
fn render_approval_actions(frame: &mut ratatui::Frame, actions: &[ApprovalHit], theme: &Theme) {
    for action in actions {
        let tone = theme.decision(decision_tone(action.decision), false);
        if action.rect.height == 1 {
            let label = approval_action_label(action.decision, action.rect.width);
            frame.render_widget(
                Paragraph::new(Span::styled(label, tone))
                    .alignment(ratatui::layout::Alignment::Center),
                action.rect,
            );
            continue;
        }
        // Visual frame shrinks one column off the hit region for the gap.
        let visual = Rect::new(
            action.rect.x,
            action.rect.y,
            action.rect.width.saturating_sub(1).max(1),
            action.rect.height,
        );
        let inner = inner_rect(visual);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(tone)
                .style(theme.panel()),
            visual,
        );
        let label = approval_action_label(action.decision, visual.width);
        frame.render_widget(
            Paragraph::new(Span::styled(label, tone)).alignment(ratatui::layout::Alignment::Center),
            inner,
        );
    }
}

fn approval_action_label(decision: ApprovalUserDecision, width: u16) -> &'static str {
    let full = match decision {
        ApprovalUserDecision::ApproveOnce => "✓ Allow once",
        ApprovalUserDecision::ApproveTree => "✓ Allow all",
        ApprovalUserDecision::Reject => "✗ Reject",
        ApprovalUserDecision::Cancel => "⎋ Cancel",
    };
    if usize::from(width) >= full.len() + 2 {
        return full;
    }
    let short = match decision {
        ApprovalUserDecision::ApproveOnce => "✓ Once",
        ApprovalUserDecision::ApproveTree => "✓ Tree",
        ApprovalUserDecision::Reject => "✗ No",
        ApprovalUserDecision::Cancel => "⎋ Esc",
    };
    if usize::from(width) >= short.len() + 2 {
        return short;
    }
    match decision {
        ApprovalUserDecision::ApproveOnce => "✓",
        ApprovalUserDecision::ApproveTree => "✓T",
        ApprovalUserDecision::Reject => "✗",
        ApprovalUserDecision::Cancel => "⎋",
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

fn collect_tree_session_ids(tree: &SessionTree, session_ids: &mut Vec<SessionId>) {
    session_ids.push(tree.session.session_id);
    for child in &tree.children {
        collect_tree_session_ids(child, session_ids);
    }
}

/// Depth-first collection of a subtree's session metadata, used to attribute
/// descendant warnings to their owning session.
fn collect_subtree_sessions(tree: &SessionTree, sessions: &mut Vec<SessionMeta>) {
    sessions.push(tree.session.clone());
    for child in &tree.children {
        collect_subtree_sessions(child, sessions);
    }
}

fn find_session(tree: &SessionTree, session_id: SessionId) -> Option<&SessionMeta> {
    find_node(tree, session_id).map(|node| &node.session)
}

fn find_node(tree: &SessionTree, session_id: SessionId) -> Option<&SessionTree> {
    if tree.session.session_id == session_id {
        return Some(tree);
    }
    tree.children
        .iter()
        .find_map(|child| find_node(child, session_id))
}

fn find_node_mut(tree: &mut SessionTree, session_id: SessionId) -> Option<&mut SessionTree> {
    if tree.session.session_id == session_id {
        return Some(tree);
    }
    tree.children
        .iter_mut()
        .find_map(|child| find_node_mut(child, session_id))
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
