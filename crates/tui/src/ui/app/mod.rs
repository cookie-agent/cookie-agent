//! Application state, event handling, and terminal loop.

mod acp;
mod agents;
mod approvals;
mod attachments;
mod background;
mod composer;
mod composer_drafts;
pub(in crate::ui) mod config_panel;
pub(in crate::ui) mod cron_panel;
mod draw;
mod emoji_backend;
mod keys;
mod machines;
pub(in crate::ui) mod memory_panel;
pub(in crate::ui) mod onboarding;
mod pickers;
mod plugins;
mod pointer;
mod presence;
mod projects;
mod providers;
mod questions;
mod refresh;
pub(in crate::ui) mod restart;
mod session_delete;
mod session_rename;
mod sessions;
mod status;
#[cfg(test)]
pub(in crate::ui) use session_delete::{SessionDeletePrompt, SessionPickerAction};
mod subscriptions;
#[cfg(test)]
pub(crate) use acp::agent_command_prompt_for_test;
pub(super) use agents::DescendantEvent;
#[cfg_attr(not(test), allow(unused_imports))]
pub(super) use approvals::{ApprovalPanel, approval_content};
#[cfg(test)]
pub(crate) use attachments::pasted_file_path_for_test;
pub(super) use cookie_agent_client_state::subscriptions::{SubscribeIntent, SubscriptionOutcome};
use emoji_backend::EmojiBackend;
use keys::{edit_credential_input, is_newline_key, is_printable_key};
use pickers::{agent_picker_row, draft_title, model_picker_row};
#[cfg_attr(not(test), allow(unused_imports))]
pub(super) use questions::{QuestionFocus, QuestionPanel};
pub(super) use sessions::status_change_from_event;
use sessions::{
    collect_known_statuses, collect_known_titles, collect_live_tree_sessions,
    collect_subtree_sessions, collect_tree_session_ids, find_node, find_node_mut, find_session,
    patch_tree_node_statuses, patch_tree_node_titles, title_change_from_event,
};

mod goal;
mod held;

#[cfg(test)]
mod fallback_tests;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Write as _,
    io::{self, Write as _},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::oauth_callback::OAuthCallbackListener;
use cookie_agent_protocol::{
    AgentDescriptor, AgentId, ApprovalListParams, ApprovalListResult, ApprovalRespondErrorCode,
    ApprovalRespondParams, ApprovalStatus, ApprovalUserDecision, AvailableModelDescriptor,
    ClientConnectId, ClientRequestId, ClientResponseId, ClientRunId, EffectivePermissionAction,
    EventPayload, InputDelivery, McpAuthBeginParams, McpAuthBeginResult, McpAuthCancelParams,
    McpAuthCompleteParams, McpServerAddParams, McpServerEditParams, McpServerInfo,
    McpServerNameParams, McpServerPersistParams, McpServerReconnectParams,
    McpServerSetEnabledParams, McpServerState, ModelKey, ModelSelection, PermissionAction,
    PermissionEffect, PermissionMode, PermissionRule, PermissionRuleSource, ProviderConnectParams,
    ProviderDescriptor, ProviderDisconnectParams, RunCancelParams, RunSelection, SafeDisplayText,
    SessionCompactParams, SessionCreateParams, SessionForkParams, SessionId, SessionListParams,
    SessionMeta, SessionPermissionClearParams, SessionPermissionGetParams,
    SessionPermissionGetResult, SessionPermissionPreviewParams, SessionPermissionSetParams,
    SessionRecallInputParams, SessionRecallInputResult, SessionRevertParams,
    SessionSetPermissionModeParams, SessionStatus, SessionSubmitInputParams,
    SessionSubmitInputResult, SessionTitle, SessionTitleChange, SessionTree, SessionTreeParams,
    SessionTreeUsageResult, SessionUsageParams, SessionUsageResult, StoredEvent, VariantId,
};
use crossterm::{
    event::{
        EnableBracketedPaste, EnableMouseCapture, Event as CrosstermEvent, EventStream, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
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
    Client, ClientDelivery, ClientError,
    config::TuiConfig,
    markdown::{Highlighter, SyntectHighlighter},
    state::{
        ApprovalState, DeliveryOutcome, EMPTY_RUNTIME_GUIDANCE, EventLevel, RuntimePhase,
        RuntimeState, StateStore, TranscriptItem,
    },
    theme::{DecisionTone, Theme},
};

use super::events::{
    ContinueSignal, JobControl, JobStep, RenderScheduler, TerminalRestore,
    install_terminal_panic_hook, resume_terminal, suspend_process, suspend_terminal,
};
use super::input::{self, InputState};
use super::management::{
    AuthListener, McpAuthView, McpForm, McpFormFocus, McpPanel, PermissionForm,
    PermissionFormFocus, PermissionHits, PermissionPanel, PermissionRow, RuntimePanel, UsagePanel,
    step_effect,
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
    CommandPalette, CommandSpec, EVENT_LEVELS, GoalCommand, PaletteAction, PaletteStep,
    SlashCommand, TextTarget, move_selection,
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
    /// The variant step of `/model`, for the model in
    /// `App::variant_step_model`.
    Variants,
    ConnectProviders,
    ConnectDetails,
    ConnectSetup,
    ConnectError,
    DisconnectConfirm,
    /// The copy/revert/fork menu for one clicked user message row.
    UserMessage,
    /// Confirm guard behind the menu's revert action.
    RevertConfirm,
    /// `/restart`: how to restart the daemon.
    RestartConfirm,
    /// The offer to attach a pasted file path.
    AttachConfirm,
    Mcp,
    Runtimes,
    Permissions,
    Usage,
    GoalDetail,
    /// The selected session's background calls.
    Background,
    Plugins,
    /// `/config`: the daemon's configuration documents.
    Config,
    /// `/memory`: the daemon's memories.
    Memory,
    /// `/cron`: the daemon's cron jobs.
    Cron,
    /// `/project` or the bottom bar's project: the project picker.
    Projects,
    /// `/options`: the session's ACP agent options, then the values of
    /// `App::agent_option_step`.
    AgentOptions,
    /// `/onboard` or the notice's "Set up": the onboarding panel.
    Onboarding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GoalBarAction {
    Details,
    Pause,
    Resume,
    Cancel,
}

impl RpcUpdate {
    /// The update that reports `error`.
    pub(in crate::ui) fn failure(error: &ClientError) -> Self {
        Self::Failure {
            status: error.to_string(),
            detail: error.detail(),
        }
    }
}

#[cfg(test)]
pub(super) use cookie_agent_client_state::ownership::session_owned_by_another_process;

pub(super) use cookie_agent_client_state::providers::{
    retry_store_contention_once, store_contention_message,
};

/// Where copied text goes: the terminal's OSC 52 clipboard escape in
/// production, a shared capture buffer in tests.
#[derive(Default)]
pub(super) enum ClipboardSink {
    #[default]
    Osc52,
    #[cfg(test)]
    Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>),
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
    /// What the click acts on: what was under the press when it landed,
    /// however far streaming content moved it before the release.
    pub(super) clicked: Option<PointerTarget>,
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

impl TreeRowHit {
    /// The cells a row hover paints: the whole row, or past an expand marker
    /// only the cells after the marker and its separating space, so the row
    /// highlight never touches the marker's own.
    pub(super) fn hover_rect(&self) -> Rect {
        let Some(marker) = self.expand_rect else {
            return self.rect;
        };
        let x = marker
            .right()
            .saturating_add(1)
            .clamp(self.rect.x, self.rect.right());
        Rect::new(x, self.rect.y, self.rect.right() - x, self.rect.height)
    }
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
    /// A steered input waiting for the next model boundary.
    User,
    /// An input queued for a new run once the current one completes.
    Queued,
    Producer(cookie_agent_protocol::ProducerMessageId),
    Overflow,
}

pub(super) struct PendingQueueEntry {
    pub(super) kind: QueueEntryKind,
    pub(super) seq: u64,
    pub(super) accepted_at: jiff::Timestamp,
    pub(super) preview: String,
    /// Another client's input: shown with its author.
    pub(super) author: Option<String>,
    /// The daemon lets this client take back another client's input.
    pub(super) any_client: bool,
}

impl PendingQueueEntry {
    /// Whether clicking the row takes this input back into the composer:
    /// this client's, or another's where the daemon allows it.
    pub(super) fn recallable(&self) -> bool {
        matches!(self.kind, QueueEntryKind::User | QueueEntryKind::Queued)
            && (self.author.is_none() || self.any_client)
    }

    /// Whether this client sent the input and can take it back: what Up in
    /// an empty composer recalls.
    pub(super) fn recallable_own(&self) -> bool {
        self.recallable() && self.author.is_none()
    }
}

/// Permission settings held by a new-session draft until `session.create`.
#[derive(Clone, Debug, Default)]
pub(super) struct DraftPermissions {
    /// The new tree's mode; `None` keeps the server default.
    pub(super) mode: Option<PermissionMode>,
    /// Overlay rules, at most one per (action, runtime, pattern), in edit
    /// order.
    pub(super) rules: Vec<PermissionRule>,
    /// Bumped per preview request so a stale preview is dropped.
    pub(super) preview_generation: u64,
}

impl DraftPermissions {
    /// Set a rule, replacing any rule for the same action and pattern.
    pub(super) fn set(&mut self, rule: PermissionRule) {
        cookie_agent_client_state::permissions::set_draft_rule(&mut self.rules, rule);
    }

    pub(super) fn remove(
        &mut self,
        action: &PermissionAction,
        runtime: Option<&cookie_agent_protocol::RuntimeId>,
        resource: &cookie_agent_protocol::WildcardPattern,
    ) {
        cookie_agent_client_state::permissions::remove_draft_rule(
            &mut self.rules,
            action,
            runtime,
            resource,
        );
    }
}

/// See [`App::wall_clock_view`].
#[derive(Debug, PartialEq, Eq)]
pub(in crate::ui) struct WallClockView {
    approval: Option<(cookie_agent_protocol::ApprovalId, u64)>,
    /// Whole seconds the approval on top has left, so its countdown ticks.
    approval_seconds_left: Option<i64>,
    queue_age: Option<String>,
    /// How far the status message has faded, so each step draws.
    status_fade: u8,
}

/// Only user rows recall the newest user input; producer rows are read-only.
#[derive(Clone, Copy, Debug)]
pub(super) struct QueueEntryHit {
    pub(super) rect: Rect,
    pub(super) index: usize,
    pub(super) kind: QueueEntryKind,
    /// The input a click recalls: this client's own entry, or for the
    /// overflow row its newest own entry. `None` when the row recalls nothing.
    pub(super) recall_seq: Option<u64>,
}

/// The visible rows of one past user message: a click opens the
/// copy/revert/fork menu targeting the message's physical event sequence.
#[derive(Clone, Copy, Debug)]
pub(super) struct UserMessageHit {
    pub(super) rect: Rect,
    /// The message's content columns, past its leading gutter: what a
    /// hover paints.
    pub(super) hover_rect: Rect,
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

/// The interactive element under a point, resolved once by
/// [`App::pointer_target_at`] from the per-frame hit map in the state-owned
/// overlay priority. Clicks act on it and hover highlights it, so a click
/// target without a hover cannot exist; the only exemptions
/// ([`PointerTarget::is_hoverable`]) are the text fields, whose caret is
/// their affordance, and queue rows that can only say why they cannot be
/// recalled. Hover only ever changes styling, never selection state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PointerTarget {
    PaletteRow(usize),
    GoalClose,
    PermissionPanelMode,
    PermissionEffect(PermissionEffect),
    PermissionRow(usize),
    /// A background panel row (selects it) and its stop button.
    BackgroundRow(usize),
    BackgroundStop(usize),
    /// A `/runtime` panel view tab.
    RuntimeTab(crate::ui::management::RuntimeView),
    /// A machines-view row (selects it) and an action on the selected row.
    MachineRow(usize),
    MachineAction(crate::ui::management::MachineAction),
    /// A `/memory` panel row (selects it) and an action it offers.
    MemoryRow(usize),
    MemoryAction(memory_panel::MemoryAction),
    /// A `/cron` panel row (selects it) and an action it offers.
    CronRow(usize),
    CronAction(cron_panel::CronAction),
    /// A `/cron` job detail naming a session: its target or the session
    /// that created it, which a click shows.
    CronSession(SessionId),
    /// A button of the onboarding notice.
    OnboardingNotice(onboarding::OnboardingAction),
    /// An onboarding panel step tab, and an action the panel offers.
    OnboardingTab(cookie_agent_client_state::onboarding::OnboardingStep),
    OnboardingAction(onboarding::OnboardingAction),
    ConfirmButton(ConfirmButton),
    /// A button of the restart confirmation.
    RestartChoice(restart::RestartChoice),
    /// The restart banner's *Restart daemon* button.
    RestartBanner,
    /// The held banner's *resume* button (`docs/site/specs/held-sessions.md`).
    HeldResume,
    /// A button of the `/config` panel's action row.
    ConfigAction(config_panel::ConfigAction),
    /// A picker's search input: a click places its caret.
    PickerInput,
    PickerRow(usize),
    /// A button on the session picker's action row.
    SessionPickerAction(session_delete::SessionPickerAction),
    ProviderSubmit,
    ProviderCancel,
    ProviderField(ProviderFormFocus),
    ApprovalAction(ApprovalUserDecision),
    ApprovalDetails,
    GoalAction(GoalBarAction),
    /// The background bar; a click opens the panel.
    BackgroundBar,
    PermissionMode,
    SessionCost,
    /// The bottom bar's project; a click opens the project picker.
    ProjectLabel,
    EventLevelFilter,
    TitleSegment(TitleSegment),
    /// The composer outside its scrollbar: a click places the caret.
    ComposerText,
    /// A queue-strip row that recalls the newest pending input.
    QueueEntry(usize),
    /// A queue-strip row this client cannot recall; a click only says why.
    PassiveQueueEntry(usize),
    /// A pane's scrollbar column: the thumb captures a drag, the track pages.
    Scrollbar(ScrollbarTarget),
    /// A past user message, by input seq: opens its copy/revert/fork menu.
    UserMessage(u64),
    /// A running call's background button on its title row.
    BackgroundButton(cookie_agent_protocol::ToolCallId),
    /// The memory URL a call's title row names: opens it in the Memory
    /// panel.
    MemoryLink(cookie_agent_protocol::ToolCallId),
    /// The expanded tools row's link: opens the deferred tools prompt.
    DeferredPromptLink,
    TranscriptBlock(BlockId),
    /// A tree row's expand/collapse marker.
    TreeExpand(SessionId),
    TreeRow(SessionId),
}

impl PointerTarget {
    /// Whether the target highlights under the pointer.
    pub(super) fn is_hoverable(self) -> bool {
        !matches!(
            self,
            Self::PickerInput | Self::ComposerText | Self::PassiveQueueEntry(_)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkingState {
    Compacting,
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
    /// The question panel.
    pub(super) question: Option<Rect>,
    /// The approval panel's `details` toggle row.
    pub(super) approval_details: Option<Rect>,
    /// The `/permissions` panel's rows, effect segments, and mode.
    pub(super) permissions: PermissionHits,
    /// The `/runtime` panel's view tabs and its machines view's rows and
    /// actions.
    pub(super) runtimes: crate::ui::management::RuntimeHits,
    /// The `/memory` panel's rows and actions.
    pub(super) memory: memory_panel::MemoryHits,
    /// The `/cron` panel's rows and actions.
    pub(super) cron: cron_panel::CronHits,
    /// The onboarding notice's buttons and the panel's tabs, kit rows, and
    /// actions.
    pub(super) onboarding: onboarding::OnboardingHits,
    pub(super) title_segments: Vec<TitleSegmentHit>,
    /// One clickable row per rendered queue-strip line (entries and the
    /// overflow fold alike): any click recalls the newest pending input.
    pub(super) queue_entries: Vec<QueueEntryHit>,
    /// Visible user-message rows that open the copy/revert/fork menu.
    pub(super) user_messages: Vec<UserMessageHit>,
    pub(super) permission_mode: Option<Rect>,
    pub(super) session_cost: Option<Rect>,
    pub(super) project_label: Option<Rect>,
    pub(super) event_level_filter: Option<Rect>,
    pub(super) provider_fields: Vec<ProviderFieldHit>,
    pub(super) provider_submit: Option<Rect>,
    pub(super) provider_cancel: Option<Rect>,
    /// The open confirm panel's Confirm/Cancel buttons.
    pub(super) confirm_buttons: Vec<(Rect, ConfirmButton)>,
    /// The restart confirmation's buttons.
    pub(super) restart_choices: Vec<(Rect, restart::RestartChoice)>,
    /// The restart banner's button.
    pub(super) restart_banner: Option<Rect>,
    /// The held banner's *resume* button.
    pub(super) held_resume: Option<Rect>,
    /// The `/config` panel's action row.
    pub(super) config_actions: Vec<(Rect, config_panel::ConfigAction)>,
    /// The session picker's action row buttons.
    pub(super) session_picker_actions: Vec<(Rect, session_delete::SessionPickerAction)>,
    pub(super) goal_actions: Vec<(Rect, GoalBarAction)>,
    pub(super) goal_close: Option<Rect>,
    /// The background bar; a click opens the panel.
    pub(super) background_bar: Option<Rect>,
    /// The background panel's rows and their stop buttons, by list index.
    pub(super) background_rows: Vec<(Rect, usize)>,
    pub(super) background_stops: Vec<(Rect, usize)>,
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
        self.question = None;
        self.approval_details = None;
        self.permissions = PermissionHits::default();
        self.runtimes = crate::ui::management::RuntimeHits::default();
        self.memory = memory_panel::MemoryHits::default();
        self.cron = cron_panel::CronHits::default();
        self.onboarding = onboarding::OnboardingHits::default();
        self.title_segments.clear();
        self.queue_entries.clear();
        self.user_messages.clear();
        self.permission_mode = None;
        self.session_cost = None;
        self.event_level_filter = None;
        self.provider_fields.clear();
        self.provider_submit = None;
        self.provider_cancel = None;
        self.confirm_buttons.clear();
        self.restart_choices.clear();
        self.restart_banner = None;
        self.held_resume = None;
        self.config_actions.clear();
        self.session_picker_actions.clear();
        self.goal_actions.clear();
        self.goal_close = None;
        self.background_bar = None;
        self.background_rows.clear();
        self.background_stops.clear();
    }
}

struct BottomBarRender {
    line: Line<'static>,
    mode_span: Option<usize>,
    cost_span: Option<usize>,
    project_span: Option<usize>,
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
    /// The client's directory, when the binary gave one: sessions it
    /// creates record it, and the session list shows its sessions only.
    pub(in crate::ui) cwd: Option<String>,
    /// The runtime new sessions work with at `cwd` instead of the daemon's
    /// `local`, when the binary chose one: one this client offers, or one
    /// already running on this machine.
    pub(in crate::ui) client_runtime: Option<String>,
    /// The capability of `client_runtime` new sessions attach; `coding`
    /// when `None`.
    pub(in crate::ui) client_capability: Option<cookie_agent_protocol::CapabilityName>,
    /// The bottom bar's leading label: where the daemon runs.
    pub(in crate::ui) connection_label: Option<String>,
    /// The home directory each runtime reported, which its paths show
    /// under as `~/…`; empty before `runtimes.list` answers, and from a
    /// daemon before client protocol 1.7.0.
    pub(in crate::ui) runtime_homes: cookie_agent_client_state::paths::RuntimeHomes,
    pub(super) client: Client,
    pub(super) deliveries: Option<tokio::sync::mpsc::UnboundedReceiver<ClientDelivery>>,
    pub(super) rpc_updates_tx: tokio::sync::mpsc::UnboundedSender<RpcUpdate>,
    pub(super) rpc_updates_rx: tokio::sync::mpsc::UnboundedReceiver<RpcUpdate>,
    /// The `runtime.snapshot` refetch after a notification fell outside the
    /// revision chain: another such notification while it is on its way
    /// may be newer than what it read, so it reads once more.
    pub(super) runtime_resync: ListingRefresh,
    /// `runtimes.list` read for the runtimes' homes alone.
    pub(super) runtime_homes_refresh: ListingRefresh,
    /// Redials the daemon when its connection drops (`cookie attach`);
    /// `None` when the daemon lives and dies with this process.
    pub(super) reconnector: Option<Reconnector>,
    /// The connection dropped and a reconnect is under way.
    pub(super) reconnecting: bool,
    /// Why the last redial failed, while reconnecting.
    pub(super) reconnect_error: Option<String>,
    /// What the daemon was told the TUI shows, for push notifications.
    pub(in crate::ui) presence: presence::Presence,
    /// Which draft change a debounced publish of the draft as the session's
    /// model selection may still send: only one no later change followed.
    pub(super) draft_publish: cookie_agent_client_state::model_draft::DraftPublish,
    pub store: StateStore,
    pub(super) sessions: Vec<SessionMeta>,
    sessions_revision: u64,
    session_search_rows_cache: SessionSearchRowsCache,
    pub(super) runtime: RuntimeState,
    pub(super) agents: Vec<AgentDescriptor>,
    /// The daemon's `[agents] default`, from its catalog view.
    pub(super) default_agent: cookie_agent_protocol::AgentId,
    /// Client-local preset used only when creating a new root session.
    pub(super) selected_preset: Option<String>,
    /// Draft owned by the `/new` flow, independent of the viewed session draft.
    pub(super) new_session_draft: Option<RunSelection>,
    /// The new-session draft's permission mode and overlay rules, sent with
    /// `session.create` so its first run already sees them.
    pub(super) draft_permissions: DraftPermissions,
    /// The new-session draft's choices for its ACP agent's options (its
    /// model and effort among them), sent with `session.create`.
    pub(super) draft_agent_options: cookie_agent_client_state::acp::DraftAgentOptions,
    /// The runtimes the new-session draft attaches, sent with
    /// `session.create`; `None` until `/runtime` edits them, when the
    /// session attaches `session.create`'s defaults.
    pub(super) draft_runtimes: Option<Vec<crate::ui::management::DraftRuntime>>,
    /// The project the next new session is created in (`cookie --project`);
    /// it applies to the first one only.
    pub(super) draft_project: Option<cookie_agent_protocol::ProjectInfo>,
    pub(super) next_draft_runtime: u64,
    pub(super) models: Vec<AvailableModelDescriptor>,
    pub(super) providers: Vec<ProviderDescriptor>,
    pub(super) skills: Vec<cookie_agent_protocol::SkillDescriptor>,
    /// The `skills.list` reads: only the newest one's answer is installed.
    pub(super) skills_reads: cookie_agent_client_state::refresh::Latest,
    /// The session permission reads and changes on their way, each of which
    /// answers with the whole list.
    pub(super) permission_reads: cookie_agent_client_state::refresh::Latest,
    #[cfg(test)]
    pub(super) skill_refresh_requests: Vec<SessionId>,
    /// Revision of the catalog view the providers, models, and agents above
    /// were installed from.
    pub(super) catalog_view_revision: Option<cookie_agent_protocol::ViewRevision>,
    /// Client-local draft selection; never alters an active run.
    pub(super) draft: Option<RunSelection>,
    pub(super) draft_reset_fallback: bool,
    draft_generation: u64,
    pending_fallback_resets: HashMap<ClientRunId, PendingFallbackReset>,
    pub(super) connect_provider: Option<ProviderDescriptor>,
    pub(super) provider_form: Option<ProviderForm>,
    /// A provider sign-in in progress, shown in the provider's details.
    pub(super) provider_sign_in: Option<ProviderSignInView>,
    pub(super) provider_operations: HashMap<cookie_agent_protocol::ProviderId, ProviderOperation>,
    /// The running connect or disconnect of each provider; one at a time
    /// per provider.
    pub(super) connect_tasks:
        HashMap<cookie_agent_protocol::ProviderId, tokio::task::JoinHandle<()>>,
    pub(super) tree: Option<SessionTree>,
    agent_panel_mode: AgentPanelMode,
    /// Session the conversation currently shows. Independent of the tree root.
    pub(super) selected: Option<SessionId>,
    /// Stable delegation-tree root; every tree refresh queries this session.
    pub(super) tree_root: Option<SessionId>,
    pub(super) selection_generation: u64,
    pub(super) read_only_sessions: HashSet<SessionId>,
    pub(super) owned_sessions: HashSet<SessionId>,
    pub(super) ownership_classifications: HashMap<SessionId, u64>,
    pub(super) next_ownership_classification: u64,
    /// Each session's `events.subscribe` state on this connection, and the
    /// recently viewed trees whose sessions stay subscribed.
    pub(super) subscriptions: cookie_agent_client_state::subscriptions::Subscriptions,
    /// Becomes `true` once the subscribe in flight for a session has
    /// finished, for a session switch that waits on its replay.
    pub(super) subscription_done: HashMap<SessionId, tokio::sync::watch::Receiver<bool>>,
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
    pub(super) runtime_panel: RuntimePanel,
    pub(super) permission_panel: PermissionPanel,
    pub(super) usage_panel: UsagePanel,
    pub(super) usage_load_generation: u64,
    pub(super) cost_refreshes: HashMap<SessionId, SessionCostRefresh>,
    pub(super) next_cost_refresh_request_id: u64,
    pub(super) conversation_scroll: ConversationScroll,
    /// Where the last drawn conversation spliced descendant rows into the
    /// cached layout, as (unspliced line, inserted rows): the scroll
    /// offset addresses the spliced lines, the scroll anchor the unspliced.
    pub(super) conversation_splice_shifts: Vec<(usize, usize)>,
    pub(super) splice_cache: super::transcript::SpliceCache,
    pub(super) scrollbar_geometry: Option<ScrollbarGeometry>,
    pub(super) scrollbar_drag: Option<ScrollbarDrag>,
    pub(super) approval_panel: ApprovalPanel,
    /// The panel of the question on top, an agent's to the user.
    pub(super) question_panel: QuestionPanel,
    pub(super) pending_approval: Option<PendingApprovalSubmission>,
    /// The approval request that most recently became the topmost panel,
    /// and when. Hotkeys stay inert for [`APPROVAL_HOTKEY_GRACE`] after it
    /// appears so keystrokes aimed at the composer cannot answer it.
    pub(super) approval_shown: Option<ApprovalShown>,
    pub(super) next_approval_request_id: u64,
    pub(super) approval_refresh_in_flight: Option<(SessionId, u64, u64)>,
    pub(super) next_approval_refresh_id: u64,
    pub(super) layout_cache: LayoutCache,
    /// Layout caches of recently viewed sessions, oldest first.
    pub(super) parked_layout_caches: Vec<LayoutCache>,
    pub(super) tui_config: TuiConfig,
    /// The `tui.toml` a transcript mode change is saved to; none in tests
    /// unless a test gives one.
    pub(super) transcript_config_path: Option<std::path::PathBuf>,
    /// The time live work rows count to in tests; none reads the clock.
    #[cfg(test)]
    pub(super) fixed_now: Option<jiff::Timestamp>,
    pub(super) theme: Theme,
    pub(super) highlighter: Box<dyn Highlighter>,
    pub(super) hit_map: UiHitMap,
    /// The open work row the conversation pins over its top, and the line
    /// it starts at, as of the last frame: folding it from there brings the
    /// row back to the top.
    pub(super) pinned_work: Option<(crate::ui::transcript::BlockId, usize)>,
    /// Interactive element under the pointer as of the last mouse event or
    /// frame; resolved against the hit map at render time and purely visual.
    pub(super) hover: Option<PointerTarget>,
    /// Where the last mouse move put the pointer, so each frame hovers what
    /// is under it now: content that streams moves under a still pointer.
    pub(super) pointer: Option<(u16, u16)>,
    /// Monotonic frame counter driving the streaming "thinking…" ellipsis;
    /// advanced by the frame tick only while animation is active.
    pub(super) animation_ticks: u64,
    pub(super) transient_notices: Vec<String>,
    pub(super) goal_notices: HashMap<SessionId, Vec<String>>,
    goal_detail: goal::GoalDetailState,
    connect_details_scroll: providers::DetailsScroll,
    background_panel: background::BackgroundPanelState,
    plugin_panel: plugins::PluginPanelState,
    pub(super) config_panel: config_panel::ConfigPanelState,
    /// The restart banner's status and a restart under way.
    pub(super) restart: restart::RestartState,
    /// A configuration document the event loop opens in the editor next.
    pub(super) pending_editor: Option<config_panel::EditorRequest>,
    pub(super) memory_panel: memory_panel::MemoryPanelState,
    /// A memory the event loop opens in the editor next.
    pub(super) pending_memory_editor: Option<memory_panel::MemoryEditRequest>,
    pub(super) cron_panel: cron_panel::CronPanelState,
    /// Whether the daemon is set up, the notice, and the `/onboard` panel.
    pub(in crate::ui) onboarding: onboarding::OnboardingState,
    /// A cron job form the event loop opens in the editor next.
    pub(super) pending_cron_editor: Option<cron_panel::CronEditRequest>,
    /// Ctrl+Z asked the event loop to suspend the TUI (Unix job control).
    pub(super) pending_suspend: bool,
    pub(super) goal_focus: Option<GoalBarAction>,
    pub(super) picker_state: ListState,
    pub(super) session_search: SearchPickerState,
    /// Whether `/sessions` lists every directory's sessions rather than
    /// this directory's (Ctrl+A).
    pub(super) session_scope_all: bool,
    /// The session delete waiting for the user to confirm it.
    pub(super) session_delete: Option<session_delete::SessionDeletePrompt>,
    /// The picker's title field for the highlighted session (Ctrl+R).
    pub(super) session_rename: Option<session_rename::SessionRenamePrompt>,
    /// `projects.list`, in name order (empty from a daemon without
    /// projects).
    pub(super) projects: Vec<cookie_agent_protocol::ProjectInfo>,
    pub(super) projects_refresh: ListingRefresh,
    /// The project picker's new-project field, while it is open.
    pub(super) project_create: Option<crate::ui::input::InputState>,
    /// The session picker's project filter (Ctrl+P); `None` lists all.
    pub(super) session_project_filter: Option<cookie_agent_protocol::ProjectFilter>,
    pub(super) agent_search: SearchPickerState,
    pub(super) model_search: SearchPickerState,
    pub(super) provider_search: SearchPickerState,
    /// The open command palette; `None` while it is closed. It never
    /// shares text with the composer.
    pub(super) palette: Option<CommandPalette>,
    /// Set while the model picker was opened by `/model`: choosing a model
    /// with named variants continues to the variant step.
    pub(super) model_then_variant: bool,
    /// The model chosen in `/model`, applied only once its variant is picked.
    pub(super) variant_step_model: Option<ModelKey>,
    /// The ACP agent option whose values `/options` lists; `None` lists
    /// the options.
    pub(super) agent_option_step: Option<String>,
    pub(super) last_escape: Option<Instant>,
    /// When Ctrl-C last found nothing to interrupt; a second press inside
    /// the quit window exits.
    pub(super) last_quit_press: Option<Instant>,
    pub(super) input: InputState,
    /// The drafts of the sessions not shown, and whose draft `input` and
    /// `attachments` are.
    composer_drafts: composer_drafts::ComposerDrafts,
    /// Files attached to the draft, sent with it as file parts.
    pub(super) attachments: Vec<cookie_agent_protocol::ArtifactRef>,
    /// A pasted path waiting for the user to attach it or paste it as text.
    pub(super) attach_offer: Option<attachments::AttachOffer>,
    pub(super) modal: Modal,
    /// The newest status message; the composer's bottom border shows it for
    /// [`STATUS_LIVE`], then it is cleared ([`App::expire_status`]).
    pub(super) status: String,
    /// The status message on show and when it first showed.
    status_shown: Option<(String, Instant)>,
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

pub(in crate::ui) enum RpcUpdate {
    /// `session.agent_option.set` answered: the agent's options after the
    /// change, or why it failed.
    AgentOptionSet {
        session_id: SessionId,
        label: String,
        result: Result<Vec<cookie_agent_protocol::AcpConfigOption>, String>,
    },
    /// The daemon was redialed after the connection dropped.
    Reconnected {
        client: Client,
    },
    /// The draft settled: publish it as the session's model selection.
    PublishDraft {
        session_id: SessionId,
        generation: u64,
    },
    /// Subscribe a session afresh once its recovery gave up.
    RetrySubscription {
        session_id: SessionId,
    },
    /// A session's background calls, fetched when it was subscribed; later
    /// changes arrive as `background.changed`. `mark` is the store's when
    /// the subscription began: a change since then is newer.
    BackgroundCalls {
        session_id: SessionId,
        mark: cookie_agent_client_state::BackgroundMark,
        calls: Vec<cookie_agent_protocol::BackgroundCall>,
    },
    /// A failure: shown in the status line and recorded among the errors
    /// printed when the TUI exits.
    Status(String),
    /// A daemon error: its message and hint in the status line, like
    /// [`Self::Status`], and, when it has technical detail, an error row in
    /// the viewed session whose collapsed tail holds the detail.
    Failure {
        status: String,
        detail: Option<String>,
    },
    /// An outcome that is not a failure: shown in the status line only.
    Notice(String),
    /// A redial of the dropped daemon connection failed; the next follows.
    ReconnectFailed(String),
    /// `config.get` answered, for the restart banner; `None` when it failed.
    ConfigStatusFetched(Option<cookie_agent_protocol::ConfigStatus>),
    /// `daemon.restart` answered.
    RestartAnswered(cookie_agent_client_state::restart::RestartOutcome),
    /// The complete content of a call's cut display block was read, or
    /// could not be.
    FullContentLoaded {
        session_id: SessionId,
        call_id: cookie_agent_protocol::ToolCallId,
        block: usize,
        state: crate::state::FullContentState,
    },
    RunStartFinished {
        session_id: SessionId,
        client_run_id: ClientRunId,
        draft_generation: u64,
        reset_fallback: bool,
        input: crate::state::ComposerInput,
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
    SubscriptionFinished {
        session_id: SessionId,
        attempt: u64,
        outcome: SubscriptionOutcome,
    },
    /// A steer RPC failed at the transport level (admission itself never
    /// rejects anymore): the submitted text is owed back to the composer.
    SteerFailed {
        session_id: SessionId,
        input: crate::state::ComposerInput,
        error: String,
    },
    /// The current runtime snapshot, fetched after a `runtime.changed`
    /// notification fell outside the revision chain.
    RuntimeResynced {
        result: Result<Box<cookie_agent_protocol::ModelCatalogView>, String>,
    },
    /// A submission steered, queued, or was handled by a plugin instead of
    /// starting the run it was tracked as.
    InputNotStarted {
        client_run_id: ClientRunId,
        notice: Option<String>,
    },
    /// A recall RPC returned the withdrawn pending input.
    SteerRecalled {
        session_id: SessionId,
        input: crate::state::ComposerInput,
    },
    /// A file `/attach` or a pasted path started uploading was stored, or
    /// could not be.
    AttachmentUploaded {
        session_id: SessionId,
        name: String,
        result: Result<attachments::Uploaded, String>,
    },
    /// A revert RPC committed; the `rewound` notification rebuilds the
    /// transcript, and the message text is owed back to the composer.
    Reverted {
        session_id: SessionId,
        text: String,
    },
    /// A fork RPC committed; the new session becomes the viewed one.
    Forked {
        forked: SessionId,
    },
    /// How many subagents the session a delete waits for has.
    SessionDeleteSubagents {
        session_id: SessionId,
        subagents: usize,
    },
    /// A `session.delete` ended.
    SessionDeleted {
        title: String,
        outcome: cookie_agent_client_state::session_delete::SessionDeleteOutcome,
    },
    /// A `session.rename` ended: the session it left.
    SessionRenamed(Box<Result<SessionMeta, ClientError>>),
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
    /// `elicitation.respond` answered, or failed with the error's code and
    /// message.
    QuestionAnswered {
        session_id: SessionId,
        elicitation_id: cookie_agent_protocol::ElicitationId,
        result: Result<(), (Option<String>, String)>,
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
    RuntimesRefreshed {
        result: Result<cookie_agent_protocol::RuntimesListResult, String>,
    },
    /// `mcp.server.list` answered for the `/runtime` panel.
    RuntimeMcpRefreshed {
        result: Result<cookie_agent_protocol::McpServerListResult, String>,
    },
    /// `projects.list` answered; `None` when it failed.
    ProjectsRefreshed {
        projects: Option<Vec<cookie_agent_protocol::ProjectInfo>>,
    },
    /// `session.project.set` answered; `label` names the project.
    SessionProjectSet {
        label: String,
        result: Box<Result<SessionMeta, ClientError>>,
    },
    /// `project.create` answered, from the project picker.
    ProjectCreated {
        result: Box<Result<cookie_agent_protocol::ProjectInfo, ClientError>>,
    },
    /// `runtimes.list` answered for the runtimes' homes alone; `None` when
    /// it failed.
    RuntimeHomesRefreshed {
        homes: Option<cookie_agent_client_state::paths::RuntimeHomes>,
    },
    /// `runtimes.agents.*` answered for the machines view's agents.
    RuntimeAgents {
        runtime: cookie_agent_protocol::RuntimeId,
        result: Result<Vec<cookie_agent_protocol::RuntimeAgentDeclaration>, String>,
        status: Option<String>,
    },
    /// `config.get` answered for opening an agent document, `template`
    /// when it does not exist yet.
    AgentDocument {
        path: String,
        template: String,
        result: Result<cookie_agent_protocol::ConfigGetResult, String>,
    },
    MachinesRefreshed {
        read: cookie_agent_client_state::refresh::Ticket,
        result: Result<cookie_agent_protocol::MachinesListResult, String>,
    },
    /// A `machines.*` change answered: what it did and the machines
    /// afterwards, or why it failed.
    MachineChanged {
        result: Result<(String, cookie_agent_protocol::MachinesListResult), String>,
    },
    ConfigLoaded {
        result: Result<cookie_agent_protocol::ConfigGetResult, String>,
    },
    /// `onboarding.get` answered; `None` from a daemon without onboarding.
    OnboardingStatus {
        read: cookie_agent_client_state::refresh::Ticket,
        status: Option<cookie_agent_protocol::OnboardingStatus>,
    },
    OnboardingRun {
        result: Result<cookie_agent_protocol::OnboardingRunResult, String>,
    },
    MemoryListed {
        result: Result<cookie_agent_protocol::MemoryListResult, String>,
    },
    /// `memory.read` answered for `purpose`: the memory, `None` when it
    /// does not exist, or why it could not be read.
    MemoryRead {
        url: String,
        purpose: memory_panel::ReadPurpose,
        result: Result<Option<cookie_agent_protocol::MemoryReadResult>, String>,
    },
    MemoryWritten {
        request: memory_panel::MemoryEditRequest,
        outcome: memory_panel::MemoryWriteOutcome,
    },
    /// A delete finished: what to tell the user either way.
    MemoryDeleted {
        result: Result<String, String>,
    },
    CronsListed {
        result: Result<cookie_agent_protocol::CronsListResult, String>,
    },
    /// A job form was created or saved, or why the daemon refused it.
    CronSaved {
        request: Box<cron_panel::CronEditRequest>,
        result: Box<Result<cookie_agent_protocol::CronJob, String>>,
    },
    /// Enabling, disabling, running now, or deleting a job finished: what
    /// to tell the user either way.
    CronChanged {
        result: Result<String, String>,
    },
    /// `config.apply` answered for `path`; `edit` is the edit it carried.
    ConfigApplied {
        path: String,
        edit: Option<config_panel::EditorRequest>,
        outcome: config_panel::ConfigApplyOutcome,
    },
    ConfigReloaded {
        status: cookie_agent_protocol::ConfigStatus,
    },
    PluginsRefreshed {
        result: Result<Vec<cookie_agent_protocol::PluginInfo>, String>,
    },
    /// An attach or detach finished; `Ok` carries the status line.
    RuntimeAttachmentChanged {
        result: Result<String, String>,
    },
    /// The instance of the new-session draft's runtime `id` opened, or why
    /// it could not.
    DraftRuntimeOpened {
        id: u64,
        result: Result<cookie_agent_protocol::RuntimeInstanceInfo, String>,
    },
    McpMutation {
        result: Box<Result<Option<McpServerInfo>, String>>,
    },
    McpAuthBegan {
        result: Result<McpAuthBeginResult, String>,
    },
    ProviderAuthBegan {
        provider_id: cookie_agent_protocol::ProviderId,
        result: Result<cookie_agent_protocol::ProviderAuthBeginResult, String>,
    },
    ProviderAuthCompleted {
        provider_id: cookie_agent_protocol::ProviderId,
        result: Result<cookie_agent_protocol::ProviderAuthCompleteResult, String>,
    },
    McpAuthCompleted {
        server: String,
        result: Result<(), String>,
    },
    McpAuthCancelled {
        result: Result<String, String>,
    },
    PermissionPreviewLoaded {
        generation: u64,
        result: Result<Vec<EffectivePermissionAction>, String>,
    },
    PermissionsLoaded {
        session_id: SessionId,
        read: cookie_agent_client_state::refresh::Ticket,
        result: Result<SessionPermissionGetResult, String>,
    },
    SkillsLoaded {
        session_id: SessionId,
        read: cookie_agent_client_state::refresh::Ticket,
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

pub(super) use cookie_agent_client_state::ownership::SessionOwnershipOutcome;

/// An approval response captured at click time and currently in flight.
/// How long approval hotkeys ignore keys after a request appears on top.
pub(super) const APPROVAL_HOTKEY_GRACE: Duration = Duration::from_millis(400);

/// One approval request revision and the moment it became the topmost
/// panel.
#[derive(Clone, Copy, Debug)]
pub(super) struct ApprovalShown {
    pub(super) approval_id: cookie_agent_protocol::ApprovalId,
    pub(super) request_revision: u64,
    pub(super) at: Instant,
}

/// The modal was dismissed optimistically; this marker blocks duplicate
/// actions until the RPC resolves.
#[derive(Clone, Debug)]
pub(super) struct PendingApprovalSubmission {
    pub(super) request_id: u64,
    pub(super) approval: ApprovalState,
}

#[derive(Debug)]
pub(super) struct ApprovalSubmissionError {
    pub(super) message: String,
    pub(super) code: Option<ApprovalRespondErrorCode>,
}

impl ApprovalSubmissionError {
    fn from_client(error: ClientError) -> Self {
        let code = error
            .error_code()
            .and_then(ApprovalRespondErrorCode::from_error_code);
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
        baseline: Option<cookie_agent_protocol::ViewRevision>,
        runtime: Box<cookie_agent_protocol::ModelCatalogView>,
    },
    Disconnected {
        provider_id: cookie_agent_protocol::ProviderId,
        baseline: Option<cookie_agent_protocol::ViewRevision>,
        runtime: Box<cookie_agent_protocol::ModelCatalogView>,
    },
}

const MAX_TRANSIENT_NOTICES: usize = 4;
const TREE_REFRESH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long selecting a session waits inline for its replay before letting
/// the UI run again. The replay itself is not cancelled.
const SELECT_SUBSCRIPTION_WAIT: Duration = Duration::from_secs(6);
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
    pub(crate) async fn handle_key_for_test(&mut self, key: KeyEvent) {
        self.handle_key(key).await;
    }

    #[cfg(test)]
    pub(crate) fn palette_open_for_test(&self) -> bool {
        self.command_palette_visible()
    }

    /// Row labels of the palette's current list step (commands or skills).
    #[cfg(test)]
    pub(crate) fn palette_labels_for_test(&self) -> Vec<String> {
        self.palette_list_labels()
    }

    /// Backdate the current approval's appearance past the hotkey grace, as
    /// if it had been on screen for a while.
    #[cfg(test)]
    pub(crate) fn arm_approval_hotkeys_for_test(&mut self) {
        let approval = self.current_approval().expect("pending approval");
        self.approval_shown = Some(ApprovalShown {
            approval_id: approval.approval_id,
            request_revision: approval.request_revision,
            at: Instant::now()
                .checked_sub(APPROVAL_HOTKEY_GRACE * 2)
                .expect("backdated instant"),
        });
    }

    #[cfg(test)]
    pub(crate) async fn submit_text_for_test(&mut self, text: &str) {
        self.input.set_buffer(text.to_owned());
        self.submit_input(InputDelivery::Steer).await;
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
        // A test app never takes its colours from the terminal running the
        // tests; a test that wants another theme sets `theme` itself.
        #[cfg(test)]
        let theme = Theme::default();
        #[cfg(not(test))]
        let theme = crate::terminal_detect::theme_without_terminal_detection(tui_config.theme);
        Self::new_with_config(
            client,
            create_new_session,
            SessionPlacement::default(),
            tui_config,
            theme,
        )
        .await
    }

    async fn new_with_config(
        client: Client,
        create_new_session: bool,
        placement: SessionPlacement,
        tui_config: TuiConfig,
        theme: Theme,
    ) -> Result<Self, crate::config::TuiConfigError> {
        // Subscribe before issuing events.subscribe so its replay and a live
        // tail racing App construction share the same retained receiver.
        let deliveries = client
            .subscribe_deliveries()
            .expect("app delivery receiver already attached");
        let (rpc_updates_tx, rpc_updates_rx) = tokio::sync::mpsc::unbounded_channel();
        // A test app shows the full transcript, which most tests read; a
        // test of the reduced mode sets it itself.
        #[cfg(test)]
        let tui_config = TuiConfig {
            transcript: cookie_agent_client_state::transcript_mode::TranscriptMode::Full,
            ..tui_config
        };
        let mut app = Self {
            client,
            cwd: placement.cwd,
            client_runtime: placement.runtime,
            client_capability: placement.capability,
            connection_label: None,
            runtime_homes: Default::default(),
            deliveries: Some(deliveries),
            rpc_updates_tx,
            rpc_updates_rx,
            runtime_resync: ListingRefresh::default(),
            runtime_homes_refresh: ListingRefresh::default(),
            reconnector: None,
            reconnecting: false,
            reconnect_error: None,
            presence: Default::default(),
            draft_publish: Default::default(),
            store: StateStore::default(),
            sessions: Vec::new(),
            sessions_revision: 0,
            session_search_rows_cache: SessionSearchRowsCache::default(),
            runtime: RuntimeState::default(),
            agents: Vec::new(),
            default_agent: cookie_agent_client_state::default_agent_id(None),
            selected_preset: None,
            new_session_draft: None,
            draft_permissions: DraftPermissions::default(),
            draft_agent_options: Default::default(),
            draft_runtimes: None,
            draft_project: placement.project,
            next_draft_runtime: 0,
            models: Vec::new(),
            providers: Vec::new(),
            skills: Vec::new(),
            skills_reads: Default::default(),
            permission_reads: Default::default(),
            #[cfg(test)]
            skill_refresh_requests: Vec::new(),
            catalog_view_revision: None,
            draft: None,
            draft_reset_fallback: false,
            draft_generation: 0,
            pending_fallback_resets: HashMap::new(),
            connect_provider: None,
            provider_form: None,
            provider_sign_in: None,
            provider_operations: HashMap::new(),
            connect_tasks: HashMap::new(),
            tree: None,
            agent_panel_mode: AgentPanelMode::Auto,
            selected: None,
            tree_root: None,
            selection_generation: 0,
            read_only_sessions: HashSet::new(),
            owned_sessions: HashSet::new(),
            ownership_classifications: HashMap::new(),
            next_ownership_classification: 0,
            subscriptions: Default::default(),
            subscription_done: HashMap::new(),
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
            runtime_panel: RuntimePanel::default(),
            permission_panel: PermissionPanel::default(),
            usage_panel: UsagePanel::default(),
            usage_load_generation: 0,
            cost_refreshes: HashMap::new(),
            next_cost_refresh_request_id: 0,
            conversation_scroll: ConversationScroll::default(),
            conversation_splice_shifts: Vec::new(),
            splice_cache: Default::default(),
            scrollbar_geometry: None,
            scrollbar_drag: None,
            approval_panel: ApprovalPanel::default(),
            question_panel: QuestionPanel::default(),
            pending_approval: None,
            approval_shown: None,
            next_approval_request_id: 0,
            approval_refresh_in_flight: None,
            next_approval_refresh_id: 0,
            layout_cache: LayoutCache::default(),
            parked_layout_caches: Vec::new(),
            tui_config,
            #[cfg(not(test))]
            transcript_config_path: crate::config::config_path(),
            #[cfg(test)]
            transcript_config_path: None,
            #[cfg(test)]
            fixed_now: None,
            theme,
            highlighter: Box::<SyntectHighlighter>::default(),
            hit_map: UiHitMap::default(),
            pinned_work: None,
            hover: None,
            animation_ticks: 0,
            transient_notices: Vec::new(),
            goal_notices: HashMap::new(),
            goal_detail: goal::GoalDetailState::default(),
            connect_details_scroll: providers::DetailsScroll::default(),
            background_panel: background::BackgroundPanelState::default(),
            plugin_panel: plugins::PluginPanelState::default(),
            config_panel: config_panel::ConfigPanelState::default(),
            restart: restart::RestartState::default(),
            pending_editor: None,
            memory_panel: memory_panel::MemoryPanelState::default(),
            pending_memory_editor: None,
            cron_panel: cron_panel::CronPanelState::default(),
            onboarding: onboarding::OnboardingState::default(),
            pending_cron_editor: None,
            pending_suspend: false,
            goal_focus: None,
            picker_state: ListState::default().with_selected(Some(0)),
            session_search: SearchPickerState::default(),
            session_scope_all: false,
            session_delete: None,
            session_rename: None,
            projects: Vec::new(),
            projects_refresh: ListingRefresh::default(),
            project_create: None,
            session_project_filter: None,
            agent_search: SearchPickerState::default(),
            model_search: SearchPickerState::default(),
            provider_search: SearchPickerState::default(),
            palette: None,
            model_then_variant: false,
            variant_step_model: None,
            agent_option_step: None,
            last_escape: None,
            last_quit_press: None,
            input: InputState::default(),
            composer_drafts: composer_drafts::ComposerDrafts::default(),
            attachments: Vec::new(),
            attach_offer: None,
            modal: Modal::None,
            status: String::new(),
            status_shown: None,
            session_errors: SessionErrorSummary::default(),
            should_quit: false,
            selection: None,
            pending_press: None,
            pointer: None,
            user_menu: None,
            clipboard_sink: ClipboardSink::default(),
            title_sequences: HashMap::new(),
        };
        app.refresh_lists().await;
        if create_new_session || app.draft_project.is_some() {
            // Keep a fresh root entirely client-side until the first prompt.
            app.new_session_draft = app
                .project_draft_selection()
                .or_else(|| app.default_draft_selection());
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

    /// Open a session: classify its ownership, then show it. The one
    /// subscription it needs is issued by whichever step first finds it
    /// idle (the owned classification, else the selection) and shared by the
    /// other.
    pub(in crate::ui) async fn open_session(&mut self, session_id: SessionId) {
        let generation = self.begin_ownership_classification(session_id);
        let outcome =
            cookie_agent_client_state::ownership::classify_session(&self.client, session_id).await;
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
        }
        generation
    }

    /// Classify a watched session's ownership in the background. Returns
    /// whether a classification is pending; its outcome then subscribes the
    /// session, so the subscription knows whether it gets a live tail.
    fn classify_session_background(&mut self, session_id: SessionId) -> bool {
        if self.owned_sessions.contains(&session_id) {
            return false;
        }
        if self.ownership_classifications.contains_key(&session_id) {
            return true;
        }
        let generation = self.begin_ownership_classification(session_id);
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let outcome =
                cookie_agent_client_state::ownership::classify_session(&client, session_id).await;
            let _ = updates.send(RpcUpdate::SessionOwnershipClassified {
                session_id,
                generation,
                outcome,
            });
        });
        true
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
                // Owned now: subscribe, or upgrade a snapshot subscription
                // made before ownership to a live one.
                self.subscribe_session(session_id, SubscribeIntent::Follow);
            }
            SessionOwnershipOutcome::Foreign => {
                self.owned_sessions.remove(&session_id);
                self.read_only_sessions.insert(session_id);
                if self.selected == Some(session_id) {
                    self.status =
                        "Session is owned by another cookie process; read-only snapshot.".into();
                    self.subscribe_session(session_id, SubscribeIntent::Follow);
                }
            }
            SessionOwnershipOutcome::Failed(error) => {
                self.owned_sessions.remove(&session_id);
                self.read_only_sessions.insert(session_id);
                self.session_errors.record(&error);
                if self.selected == Some(session_id) {
                    self.status = error;
                    self.subscribe_session(session_id, SubscribeIntent::Follow);
                }
            }
        }
    }

    async fn create_root_session(&mut self, selection: RunSelection) -> bool {
        let agent = selection.agent.clone();
        let project = self
            .draft_project
            .as_ref()
            .map(|project| project.id.clone());
        // A project whose defaults name runtimes gets them from the daemon.
        let project_attachments = self
            .draft_project
            .as_ref()
            .and_then(|project| project.defaults.as_ref())
            .is_some_and(|defaults| !defaults.runtimes.is_empty() || !defaults.global);
        let (global, runtimes) = match &self.draft_runtimes {
            Some(entries) => (
                entries
                    .iter()
                    .any(crate::ui::management::DraftRuntime::is_global),
                Some(
                    entries
                        .iter()
                        .filter(|entry| !entry.is_global())
                        .map(|entry| entry.request.clone())
                        .collect(),
                ),
            ),
            None if project_attachments => (true, None),
            None => (
                true,
                self.client_runtime
                    .is_some()
                    .then(|| self.default_coding_runtime())
                    .flatten()
                    .map(|request| vec![request]),
            ),
        };
        match self
            .client
            .create_session(SessionCreateParams {
                permission_mode: self.draft_permissions.mode,
                permission_rules: self.draft_permissions.rules.clone(),
                cwd: self.cwd.clone(),
                runtimes,
                global,
                project,
                agent_options: self.draft_agent_options.for_agent(&agent).to_vec(),
                ..SessionCreateParams::new(selection)
            })
            .await
        {
            Ok(result) => {
                let session_id = result.session.session_id;
                self.draft_project = None;
                if let Some(mode) = self.draft_permissions.mode {
                    self.permission_modes.insert(session_id, mode);
                }
                self.draft_permissions = DraftPermissions::default();
                self.draft_agent_options.clear();
                self.draft_runtimes = None;
                self.note_title_sequence(&result.session);
                self.sessions.push(result.session);
                self.note_sessions_changed();
                self.open_session(session_id).await;
                self.new_session_draft = None;
                self.status = format!(
                    "New root session opened with agent {agent}. Press / or Ctrl-P for commands."
                );
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

/// A provider sign-in waiting for the browser: its URL once the daemon has
/// begun it, and the loopback listener catching the redirect.
pub(super) struct ProviderSignInView {
    pub(super) provider_id: cookie_agent_protocol::ProviderId,
    pub(super) authorization_url: Option<String>,
    pub(super) error: Option<String>,
    pub(super) listener: Option<AuthListener>,
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

pub(super) use cookie_agent_client_state::usage::{format_cost_usd, format_token_count};

pub(in crate::ui) use cookie_agent_client_state::session_actions::permission_mode_label;

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

impl App {
    /// Shows where the daemon runs: its label leads the bottom bar, and its
    /// notices stay below the transcript.
    pub(in crate::ui) fn show_connection(&mut self, connection: ConnectionView) {
        self.connection_label = connection.label;
        self.transient_notices.extend(connection.notices);
    }
}

/// Redials the daemon after a dropped connection and handshakes as the same
/// client identity, so the new connection's events keep this client's
/// origin.
pub type Reconnector = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Client, String>> + Send>>
        + Send
        + Sync,
>;

/// Run the terminal UI against a connected client.
pub async fn run_with_client(client: Client) -> anyhow::Result<()> {
    run_terminal(
        client,
        false,
        None,
        SessionPlacement::default(),
        ConnectionView::default(),
    )
    .await
}

/// Where the TUI's new sessions work.
#[derive(Clone, Debug, Default)]
pub struct SessionPlacement {
    /// The client's directory: new sessions record it and work in it, and
    /// the session list shows its sessions. Canonical
    /// (`cookie_agent_protocol::paths::canonicalize`), as the daemon records
    /// a session's directory: the list and `sessions.changed` compare the
    /// two as they are.
    pub cwd: Option<String>,
    /// A runtime on this client's machine other than the daemon's `local`,
    /// such as one this client runs for its own machine when the daemon is
    /// on another: new sessions attach its `capability` at `cwd` instead.
    pub runtime: Option<String>,
    /// The capability of `runtime` new sessions attach; `coding` when
    /// omitted.
    pub capability: Option<cookie_agent_protocol::CapabilityName>,
    /// The project the TUI starts a new session in (`cookie --project`):
    /// the session takes the project's default agent and preset, and the
    /// daemon its default runtimes.
    pub project: Option<cookie_agent_protocol::ProjectInfo>,
}

/// Where the TUI's daemon runs, as the TUI shows it.
#[derive(Clone, Debug, Default)]
pub struct ConnectionView {
    /// The bottom bar's leading label: the daemon's address, or
    /// `standalone` for a daemon inside this process. Nothing when `None`.
    pub label: Option<String>,
    /// Notices shown below the transcript from the start, such as why a
    /// daemon could not be used.
    pub notices: Vec<String>,
}

/// Run the terminal UI against a daemon it reconnects to with
/// `reconnector` whenever the connection drops; new sessions follow
/// `placement`, and `connection` says where the daemon runs.
pub async fn run_attached(
    client: Client,
    reconnector: Reconnector,
    placement: SessionPlacement,
    connection: ConnectionView,
) -> anyhow::Result<()> {
    run_terminal(client, false, Some(reconnector), placement, connection).await
}

/// Run the terminal UI with a newly created root session; see
/// [`run_attached`] for `placement` and `connection`.
pub async fn run_with_new_session(
    client: Client,
    placement: SessionPlacement,
    connection: ConnectionView,
) -> anyhow::Result<()> {
    run_terminal(client, true, None, placement, connection).await
}

async fn run_terminal(
    client: Client,
    create_new_session: bool,
    reconnector: Option<Reconnector>,
    placement: SessionPlacement,
    connection: ConnectionView,
) -> anyhow::Result<()> {
    crate::markdown::warm_up_syntax_assets();
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
    let mut app = App::new_with_config(client, create_new_session, placement, tui_config, theme)
        .await
        .context("initialize TUI")?;
    app.reconnector = reconnector;
    app.show_connection(connection);
    if let Some(path) = onboarding::OnboardingState::default_state_file() {
        app.onboarding = onboarding::OnboardingState::with_state_file(path);
    }
    app.poll_onboarding();
    // A stop signal quits like `/quit`, so the terminal is restored and an
    // in-process daemon shuts down.
    let signals = cookie_agent_protocol::StopSignals::new().context("listen for stop signals")?;
    let mut restore = TerminalRestore;
    enable_raw_mode().context("enable terminal raw mode")?;
    restore.raw_mode_enabled();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    restore.alternate_screen_entered();
    execute!(stdout, EnableMouseCapture).context("enable mouse capture")?;
    restore.mouse_capture_enabled();
    // Best effort: a Windows console without ANSI support refuses it ("not
    // implemented in the legacy Windows API"), and delivers a paste as keys.
    if execute!(stdout, EnableBracketedPaste).is_ok() {
        restore.bracketed_paste_enabled();
    }
    if super::events::enable_keyboard_enhancement(&mut stdout) {
        restore.keyboard_enhancement_enabled();
    }
    // Focus reports tell the daemon whether the user is here, so it pushes
    // nothing for the session on screen. Best effort, as above.
    if execute!(stdout, crossterm::event::EnableFocusChange).is_ok() {
        restore.focus_change_enabled();
    }
    let backend = EmojiBackend::new(CrosstermBackend::new(stdout));
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    let deliveries = app.take_deliveries();
    let (result, session_errors) = event_loop(&mut terminal, app, deliveries, signals).await;
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
    terminal: &mut Terminal<EmojiBackend<CrosstermBackend<io::Stdout>>>,
    mut app: App,
    mut deliveries: tokio::sync::mpsc::UnboundedReceiver<ClientDelivery>,
    mut signals: cookie_agent_protocol::StopSignals,
) -> (anyhow::Result<()>, SessionErrorSummary) {
    let mut events = EventStream::new();
    let mut replay_watchdog = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut mcp_poll = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut frame_tick = tokio::time::interval(RenderScheduler::FRAME_INTERVAL);
    // The tick is polled only while something animates or a coalesced frame
    // waits; resuming must not replay the ticks an idle TUI never took.
    frame_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut render = RenderScheduler::default();
    let mut drawn_wall_clock_view = app.wall_clock_view();
    let mut continued = ContinueSignal::new();
    let mut jobs = JobControl::default();
    let mut reenter = false;
    loop {
        // A reconnect hands over the new connection's delivery stream.
        if let Some(reconnected) = app.deliveries.take() {
            deliveries = reconnected;
        }
        // `/config`, `/memory`, and `/cron` edit a document in an external editor: the
        // terminal and its input belong to the editor until it exits.
        let editing = match (
            app.pending_editor.take(),
            app.pending_memory_editor.take(),
            app.pending_cron_editor.take(),
        ) {
            (Some(request), _, _) => Some(EditorJob::Config(request)),
            (None, Some(request), _) => Some(EditorJob::Memory(request)),
            (None, None, Some(request)) => Some(EditorJob::Cron(Box::new(request))),
            (None, None, None) => None,
        };
        if let Some(job) = editing {
            drop(events);
            suspend_terminal();
            let edited = match &job {
                EditorJob::Config(request) => {
                    edit_blocking(|| config_panel::run_editor(&request.path, &request.text))
                }
                EditorJob::Memory(request) => {
                    edit_blocking(|| config_panel::run_editor("memory.md", &request.text))
                }
                EditorJob::Cron(request) => {
                    edit_blocking(|| config_panel::run_editor("cron-job.md", &request.text))
                }
            };
            let resumed = resume_terminal().context("resume the terminal");
            // Clear before the event stream reads stdin again, so it cannot
            // take the terminal's answer to the clear's cursor query.
            if let Err(error) = resumed.and_then(|()| terminal.clear().context("redraw")) {
                return (Err(error), app.session_errors.clone());
            }
            events = EventStream::new();
            match job {
                EditorJob::Config(request) => app.finish_config_edit(request, edited),
                EditorJob::Memory(request) => app.finish_memory_edit(request, edited),
                EditorJob::Cron(request) => app.finish_cron_edit(*request, edited),
            }
            render.mark_immediate();
        }
        // Ctrl+Z stops the process until the shell continues it; a SIGCONT
        // the TUI did not ask for brings the terminal back the same way.
        let suspend = std::mem::take(&mut app.pending_suspend);
        if suspend || std::mem::take(&mut reenter) {
            drop(events);
            let resumed = if suspend {
                let stopped = suspend_process().context("suspend the TUI");
                jobs.resumed_after_suspend();
                stopped.and_then(|()| resume_terminal().context("resume the terminal"))
            } else {
                suspend_terminal();
                resume_terminal().context("resume the terminal")
            };
            if let Err(error) = resumed.and_then(|()| terminal.clear().context("redraw")) {
                return (Err(error), app.session_errors.clone());
            }
            events = EventStream::new();
            render.mark_immediate();
        }
        if render.should_draw(Instant::now()) {
            // Whatever changed the view, the composer shows its draft.
            app.follow_composer_slot();
            app.follow_status();
            drawn_wall_clock_view = app.wall_clock_view();
            if let Err(error) = terminal
                .draw(|frame| app.draw(frame))
                .context("draw terminal")
            {
                return (Err(error), app.session_errors.clone());
            }
            render.drew(Instant::now());
            // The next tick lands one full frame after this draw, when a
            // coalesced frame is allowed to draw.
            frame_tick.reset();
        }
        if app.should_quit {
            return (Ok(()), app.session_errors.clone());
        }
        // An idle TUI takes no frame ticks at all: the cadence runs only for
        // animation or to flush a frame coalesced below.
        let animating = app.animation_active();
        let frame_due = animating || render.pending();
        tokio::select! {
            () = signals.recv() => return (Ok(()), app.session_errors.clone()),
            () = continued.recv() => reenter = jobs.continued() == JobStep::Reenter,
            Some(event) = events.next() => match event {
                // Windows' console reports every key's release too; acting
                // on it would handle each keystroke twice.
                Ok(CrosstermEvent::Key(key)) if key.kind == KeyEventKind::Release => {}
                Ok(CrosstermEvent::Key(key)) => {
                    // A view change since the last frame takes its draft
                    // along before the key edits one.
                    app.follow_composer_slot();
                    app.handle_key(key).await;
                    app.note_input();
                    render.mark_immediate();
                }
                Ok(CrosstermEvent::FocusGained) => app.focus_changed(true),
                Ok(CrosstermEvent::FocusLost) => app.focus_changed(false),
                Ok(CrosstermEvent::Mouse(mouse)) => {
                    // Wheel and pointer motion arrive in bursts (a trackpad
                    // sends ~100 scroll events a second): draw them at the
                    // frame cadence. Clicks still draw at once.
                    let burst = matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp
                            | MouseEventKind::ScrollDown
                            | MouseEventKind::ScrollLeft
                            | MouseEventKind::ScrollRight
                            | MouseEventKind::Moved
                            | MouseEventKind::Drag(_)
                    );
                    if !matches!(mouse.kind, MouseEventKind::Moved) {
                        app.note_input();
                    }
                    if app.handle_mouse(mouse).await {
                        if burst {
                            render.mark_stream();
                        } else {
                            render.mark_immediate();
                        }
                    }
                }
                Ok(CrosstermEvent::Paste(text)) => {
                    let text = Zeroizing::new(text);
                    app.follow_composer_slot();
                    app.handle_paste(&text);
                    app.note_input();
                    render.mark_immediate();
                }
                Ok(CrosstermEvent::Resize(_, _)) => {
                    if let Err(error) = handle_terminal_resize(terminal, &mut render)
                        .context("resize terminal")
                    {
                        return (Err(error), app.session_errors.clone());
                    }
                }
                Err(error) => {
                    app.status = error.to_string();
                    render.mark_immediate();
                }
            },
            delivery = deliveries.recv(), if !app.reconnecting => match delivery {
                Some(delivery) => {
                    if app.handle_delivery(delivery).await {
                        render.mark_stream();
                    }
                }
                None => {
                    for session_id in app.store.abandon_replays() {
                        app.recover_session(session_id);
                    }
                    app.clear_connect_secrets();
                    app.abort_connect_work();
                    if !app.begin_reconnect() {
                        app.status = "daemon disconnected".into();
                        app.session_errors.record(&app.status);
                        return (Ok(()), app.session_errors.clone());
                    }
                    render.mark_immediate();
                }
            },
            Some(update) = app.rpc_updates_rx.recv() => {
                match update {
                    RpcUpdate::Reconnected { client } => app.finish_reconnect(client).await,
                    update => app.handle_rpc_update(update),
                }
                render.mark_immediate();
            },
            _ = replay_watchdog.tick() => {
                // An idle TUI stays idle: redraw only when recovery changed
                // state or wall-clock time alone changed the drawn view.
                let recovered = app.recover_timed_out_replays();
                let expired = app.expire_status();
                if recovered
                    || expired
                    || app.wall_clock_view() != drawn_wall_clock_view
                {
                    render.mark_stream();
                }
            },
            _ = mcp_poll.tick() => {
                app.poll_mcp();
                app.poll_runtimes();
                // Going idle, or a session opened by a delivery.
                app.report_presence();
            },
            _ = frame_tick.tick(), if frame_due => {
                // The frame cadence drives the clock bucket (the "thinking…"
                // ellipsis and the working spinner), which steps every
                // ~400ms, and flushes coalesced frames; everything else
                // redraws on events.
                if animating && app.animation_tick() {
                    render.mark_stream();
                }
            },
        }
    }
}

/// A document the event loop hands to the editor.
enum EditorJob {
    Config(config_panel::EditorRequest),
    Memory(memory_panel::MemoryEditRequest),
    Cron(Box<cron_panel::CronEditRequest>),
}

/// Runs the editor, letting the runtime's other work go on meanwhile when it
/// can.
fn edit_blocking(run: impl FnOnce() -> Result<String, String>) -> Result<String, String> {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(run),
        _ => run(),
    }
}

pub(super) fn handle_terminal_resize<B: Backend>(
    terminal: &mut Terminal<B>,
    render: &mut RenderScheduler,
) -> Result<(), B::Error> {
    terminal.autoresize()?;
    // A drag-resize sends a burst of events; each relayouts the whole
    // transcript at a new width, so draw them at the frame cadence.
    render.mark_stream();
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

/// The two answers of a confirm panel; each has a button and hotkeys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConfirmButton {
    Confirm,
    Cancel,
}

/// Columns between a confirm panel's border and its text, either side.
const CONFIRM_PAD: u16 = 2;

/// A content-sized confirm dialog centered on `screen`: the body above a
/// centered Confirm/Cancel button pair, each button naming its hotkeys under
/// its label. `body` receives the text column's width and returns its lines
/// already wrapped. The buttons anchor to the bottom, so a short terminal
/// clips the body, never the way out. Returns the buttons' hit rects.
pub(super) fn render_confirm_panel(
    frame: &mut ratatui::Frame,
    screen: Rect,
    title: &str,
    body: impl FnOnce(u16) -> Vec<Line<'static>>,
    theme: &Theme,
) -> Vec<(Rect, ConfirmButton)> {
    let buttons = [
        (
            ConfirmButton::Confirm,
            "Confirm",
            "enter/y",
            DecisionTone::Deny,
        ),
        (
            ConfirmButton::Cancel,
            "Cancel",
            "esc/n",
            DecisionTone::Neutral,
        ),
    ];
    render_choice_panel(frame, screen, title, body, &buttons, theme)
}

/// A confirm dialog like [`render_confirm_panel`] with `buttons` (each its
/// value, label, hotkeys, and tone) in a centered row. Returns their hit
/// rects.
pub(super) fn render_choice_panel<T: Copy>(
    frame: &mut ratatui::Frame,
    screen: Rect,
    title: &str,
    body: impl FnOnce(u16) -> Vec<Line<'static>>,
    buttons: &[(T, &str, &str, DecisionTone)],
    theme: &Theme,
) -> Vec<(Rect, T)> {
    let preferred = u16::try_from(u32::from(screen.width) * 64 / 100).unwrap_or(u16::MAX);
    let width = preferred.clamp(52.min(screen.width), 76.min(screen.width));
    let lines = body(width.saturating_sub(2 + 2 * CONFIRM_PAD));
    // Roomy terminals get framed two-line buttons (label over hotkeys);
    // cramped ones a flat row with both on one line.
    let framed = screen.height >= 13;
    let button_height = if framed { 4 } else { 1 };
    let body_height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    // Border, top pad, body, gap, buttons.
    let height = body_height
        .saturating_add(4 + button_height)
        .min(screen.height);
    let area = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    );
    paint_panel(frame, area, theme);
    frame.render_widget(
        crate::ui::panel_block()
            .border_style(theme.panel_border())
            .title(crate::ui::fitted_panel_title(title.to_owned(), width)),
        area,
    );
    let inner = inner_rect(area);
    let button_height = button_height.min(inner.height);
    let buttons_y = inner.bottom() - button_height;
    let body_area = Rect::new(
        inner.x + CONFIRM_PAD.min(inner.width),
        inner.y.saturating_add(1).min(buttons_y),
        inner.width.saturating_sub(2 * CONFIRM_PAD),
        buttons_y.saturating_sub(inner.y.saturating_add(2)),
    );
    frame.render_widget(Paragraph::new(lines), body_area);

    // Equal-width buttons centered as one group with a two-cell gutter:
    // framed ones stack label and keys, flat ones put them side by side.
    let content_width = |label: &str, keys: &str| {
        let (label, keys) = (UnicodeWidthStr::width(label), UnicodeWidthStr::width(keys));
        if framed {
            label.max(keys) + 6
        } else {
            label + 1 + keys + 2
        }
    };
    let button_width = buttons
        .iter()
        .map(|(_, label, keys, _)| content_width(label, keys))
        .max()
        .map_or(0, |width| u16::try_from(width).unwrap_or(u16::MAX));
    let count = u16::try_from(buttons.len()).unwrap_or(u16::MAX).max(1);
    let gutters = 2 * (count - 1);
    let button_width = button_width.min(inner.width.saturating_sub(gutters) / count);
    let group_width = button_width * count + gutters;
    let mut x = inner.x + inner.width.saturating_sub(group_width) / 2;
    let mut hits = Vec::new();
    for &(button, label, keys, tone) in buttons {
        let rect = Rect::new(x, buttons_y, button_width, button_height);
        let style = theme.decision(tone, false);
        let label = Span::styled(label, style);
        let keys = Span::styled(keys, theme.internal());
        let (content, content_area) = if framed {
            frame.render_widget(
                crate::ui::panel_block()
                    .border_style(style)
                    .style(theme.panel()),
                rect,
            );
            let content = vec![Line::from(label), Line::from(keys)];
            (content, inner_rect(rect))
        } else {
            (vec![Line::from(vec![label, Span::raw(" "), keys])], rect)
        };
        frame.render_widget(
            Paragraph::new(content).alignment(ratatui::layout::Alignment::Center),
            content_area,
        );
        hits.push((rect, button));
        x += button_width + 2;
    }
    hits
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

/// One panel listing's refresh: at most one request on its way, and one
/// more once it lands when a change was announced meanwhile — the listing
/// on its way may have been read before that change, and no further notice
/// will come for it.
#[derive(Debug, Default)]
pub(in crate::ui) struct ListingRefresh {
    in_flight: bool,
    again: bool,
}

impl ListingRefresh {
    #[cfg(test)]
    pub(in crate::ui) fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Whether to send a listing request now; asked while one is on its
    /// way, [`Self::landed`] answers that another is due.
    pub(in crate::ui) fn start(&mut self) -> bool {
        if self.in_flight {
            self.again = true;
            return false;
        }
        self.in_flight = true;
        true
    }

    /// The listing on its way landed: whether to send another.
    pub(in crate::ui) fn landed(&mut self) -> bool {
        self.in_flight = false;
        std::mem::take(&mut self.again)
    }
}
