//! Application state, event handling, and terminal loop.

use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    io,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::{
    AgentDescriptor, AgentId, ApprovalListParams, ApprovalListResult, ApprovalRespondError,
    ApprovalRespondErrorCode, ApprovalRespondParams, ApprovalStatus, ApprovalUserDecision,
    AvailableModelDescriptor, ClientConnectId, ClientRequestId, ClientResponseId, ClientRunId,
    EventPayload, ModelKey, ModelSelection, PermissionMode, ProviderConnectParams,
    ProviderDescriptor, ProviderDisconnectParams, RunCancelParams, RunSelection, RunStartParams,
    RunSteerParams, RunToolStdinParams, SafeDisplayText, SessionCompactParams, SessionCreateParams,
    SessionId, SessionListParams, SessionMeta, SessionSetPermissionModeParams, SessionStatus,
    SessionTitle, SessionTitleChange, SessionTree, SessionTreeParams, VariantId,
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
        ApprovalState, DeliveryOutcome, EMPTY_RUNTIME_GUIDANCE, RuntimePhase, RuntimeState,
        StateStore, ToolStatus, TranscriptItem, approval_state_from_record,
    },
    theme::Theme,
};

use super::events::{RenderScheduler, TerminalRestore};
use super::input::{self, InputState};
use super::pickers::{
    SearchPickerFocus, SearchPickerState, SessionSearchRow, cycle_selection, flatten_tree,
    move_selection as move_picker_selection, provider_matches, session_search_rows, short_id,
};
use super::provider::{
    DURABLE_PROVIDER_COPY, ProviderAction, ProviderForm, ProviderFormFocus, ProviderOperation,
    ProviderRowState, action_name, row_label, row_state,
};
use super::slash::{
    CommandSpec, SlashCommand, Submission, command_help_lines, move_selection, parse_submission,
};
use super::transcript::{
    BlockHit, BlockId, ConversationScroll, LayoutCache, ScrollbarGeometry, wrapped_line,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Modal {
    None,
    Sessions,
    Agents,
    Models,
    ConnectProviders,
    ConnectDetails,
    ConnectSetup,
    ConnectError,
    DisconnectConfirm,
}

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
    EventLevelFilter,
    TreeRow(SessionId),
    ProviderField(ProviderFormFocus),
    ProviderSubmit,
}

/// Per-frame hit targets built from the same geometry and transcript layout
/// that were rendered. Mouse events only consult this cached map.
#[derive(Default)]
pub(super) struct UiHitMap {
    pub(super) modal_open: bool,
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
    pub(super) permission_mode: Option<Rect>,
    pub(super) event_level_filter: Option<Rect>,
    pub(super) provider_fields: Vec<ProviderFieldHit>,
    pub(super) provider_submit: Option<Rect>,
}

struct BottomBarRender {
    line: Line<'static>,
    mode_span: Option<usize>,
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
    pub(super) runtime: RuntimeState,
    pub(super) agents: Vec<AgentDescriptor>,
    /// Revision of the current agent descriptor snapshot; refreshed
    /// coherently with the model revision.
    pub(super) agent_revision: Option<cookie_agent_protocol::AgentRevision>,
    pub(super) models: Vec<AvailableModelDescriptor>,
    /// Revision of the current model descriptor snapshot.
    pub(super) model_revision: Option<cookie_agent_protocol::ModelRevision>,
    pub(super) providers: Vec<ProviderDescriptor>,
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
    pub(super) tree_refresh_in_flight: Option<(u64, u64)>,
    pub(super) tree_refresh_pending: bool,
    pub(super) next_tree_refresh_id: u64,
    pub(super) tree_cursor: Option<SessionId>,
    pub(super) tree_offset: usize,
    pub(super) tree_viewport_height: usize,
    pub(super) collapsed_sessions: HashSet<SessionId>,
    pub(super) expanded_blocks: HashMap<SessionId, HashSet<BlockId>>,
    pub(super) permission_modes: HashMap<SessionId, PermissionMode>,
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
    pub(super) provider_search: SearchPickerState,
    pub(super) palette_state: ListState,
    pub(super) palette_dismissed: bool,
    pub(super) last_escape: Option<Instant>,
    pub(super) input: InputState,
    pub(super) modal: Modal,
    pub(super) input_focused: bool,
    pub(super) stdin_target: Option<cookie_agent_protocol::ToolCallId>,
    pub(super) status: String,
    pub(super) should_quit: bool,
    /// Latest authoritative title sequence per session: patches apply only a
    /// strictly newer sequence, so stale tree/list responses cannot
    /// overwrite a newer title event.
    pub(super) title_sequences: HashMap<SessionId, u64>,
}

pub(super) enum RpcUpdate {
    Status(String),
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
    PermissionModeFailed {
        session_id: SessionId,
        previous: PermissionMode,
        attempted: PermissionMode,
        error: String,
    },
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
impl App {
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
        // Subscribe before issuing events.subscribe so its replay and a live
        // tail racing App construction share the same retained receiver.
        let deliveries = client
            .subscribe_deliveries()
            .expect("app delivery receiver already attached");
        let (rpc_updates_tx, rpc_updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let tui_config = crate::config::load(None)?;
        // Precedence: tui.toml `theme` > COOKIE_THEME/env detection;
        // NO_COLOR/TERM=dumb always force mono inside the theme layer.
        let theme = tui_config
            .theme
            .map(Theme::with_kind_from_env)
            .unwrap_or_else(Theme::from_env);
        let mut app = Self {
            client,
            deliveries: Some(deliveries),
            rpc_updates_tx,
            rpc_updates_rx,
            subscription_lanes: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            stdin_lanes: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            store: StateStore::default(),
            sessions: Vec::new(),
            runtime: RuntimeState::default(),
            agents: Vec::new(),
            agent_revision: None,
            models: Vec::new(),
            model_revision: None,
            providers: Vec::new(),
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
            tree_refresh_in_flight: None,
            tree_refresh_pending: false,
            next_tree_refresh_id: 0,
            tree_cursor: None,
            tree_offset: 0,
            tree_viewport_height: 0,
            collapsed_sessions: HashSet::new(),
            expanded_blocks: HashMap::new(),
            permission_modes: HashMap::new(),
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
            provider_search: SearchPickerState::default(),
            palette_state: ListState::default().with_selected(Some(0)),
            palette_dismissed: false,
            last_escape: None,
            input: InputState::default(),
            modal: Modal::None,
            input_focused: true,
            stdin_target: None,
            status: "Connected. Type /help for commands.".into(),
            should_quit: false,
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
        self.select_session(session_id).await;
        self.drain_replay(session_id).await;
        self.refresh_tree().await;
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
                self.open_session(session_id).await;
                self.status =
                    format!("New root session opened with agent {agent}. Type /help for commands.");
            }
            Err(error) => self.status = error.to_string(),
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
            }
            Err(error) => self.status = error.to_string(),
        }
        self.refresh_coherent_lists().await;
    }

    /// Fetch and install the sole protocol-8 discovery object.
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
        self.agents
            .iter()
            .filter(|agent| {
                agent.runnable_as_root && agent.mode != cookie_agent_protocol::AgentMode::Internal
            })
            .collect()
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
            Modal::Agents if !self.agent_switching_allowed() => {
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
        if !self
            .agents
            .iter()
            .any(|agent| agent.runnable_as_root && agent.id == draft.agent)
        {
            self.draft = self.default_draft_selection();
            return;
        }
        let Some(descriptor) = self.model_descriptor(&draft.model.model) else {
            draft.model = self
                .preferred_model_for_agent(&draft.agent)
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

    fn preferred_model_for_agent(&self, agent: &AgentId) -> Option<ModelSelection> {
        self.agents
            .iter()
            .find(|candidate| candidate.id == *agent)
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

    pub(super) fn draft_model_labels(&self) -> Vec<String> {
        if self.watching_root_session() {
            return self
                .models
                .iter()
                .map(|descriptor| {
                    canonical_model_row(
                        &Self::default_model_selection(descriptor),
                        Some(&descriptor.display_name),
                    )
                })
                .collect();
        }
        self.draft_models()
            .iter()
            .map(|selection| {
                canonical_model_row(
                    selection,
                    self.model_descriptor(&selection.model)
                        .map(|descriptor| descriptor.display_name.as_str()),
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
        let Some(descriptor) = self
            .agents
            .iter()
            .find(|candidate| candidate.id == agent && candidate.runnable_as_root)
        else {
            return;
        };
        let model = self
            .draft
            .as_ref()
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
        self.draft = Some(RunSelection { agent, model });
        self.status = self.draft_status("Draft run agent");
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

    fn permission_mode(&self, session_id: SessionId) -> PermissionMode {
        self.permission_modes
            .get(&session_id)
            .copied()
            .unwrap_or_default()
    }

    fn cycle_permission_mode(&mut self) {
        let Some(session_id) = self.selected else {
            return;
        };
        let previous = self.permission_mode(session_id);
        let mode = match previous {
            PermissionMode::AutoApprove => PermissionMode::Ask,
            PermissionMode::Ask => PermissionMode::Yolo,
            PermissionMode::Yolo => PermissionMode::AutoApprove,
        };
        self.permission_modes.insert(session_id, mode);
        self.status = format!(
            "Permission mode: {} — applies to subsequent approvals in this session",
            permission_mode_label(mode)
        );
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            if let Err(error) = client
                .set_permission_mode(SessionSetPermissionModeParams { session_id, mode })
                .await
            {
                let _ = updates.send(RpcUpdate::PermissionModeFailed {
                    session_id,
                    previous,
                    attempted: mode,
                    error: error.to_string(),
                });
            }
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
        if self.active_run_agent().is_some() {
            format!(
                "{action}: {}; applies to the next run — the active run is unchanged",
                draft_title(draft)
            )
        } else {
            format!("{action}: {}", draft_title(draft))
        }
    }

    pub(super) fn cycle_agent(&mut self, backward: bool) {
        if !self.agent_switching_allowed() {
            self.status = self
                .delegated_pin_reason()
                .unwrap_or_else(|| "agent switching requires a root session".into());
            return;
        }
        let selectable = self
            .selectable_agents()
            .into_iter()
            .map(|agent| agent.id.clone())
            .collect::<Vec<_>>();
        if selectable.is_empty() {
            self.status = "no root-runnable agent is available".into();
            return;
        }
        let current = self.draft.as_ref().map(|draft| draft.agent.clone());
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
            // A session outside the current tree is the intentional reroot
            // action (used by the session picker and startup).
            self.reroot_tree(session_id);
            return;
        }
        self.set_selected_session(session_id);
        self.tree_cursor = Some(session_id);
        let needs_subscription = self.tree_subscription_sessions.insert(session_id);
        let cursor = self
            .store
            .sessions
            .get(&session_id)
            .map(|state| state.last_seq);
        if needs_subscription {
            self.subscribe_session_background(session_id, cursor);
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
        self.subscribe_session_background(session_id, cursor);
        self.refresh_tree_background();
    }

    fn subscribe_session_background(&self, session_id: SessionId, cursor: Option<u64>) {
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
            let error = match tokio::time::timeout(
                TREE_SUBSCRIPTION_TIMEOUT,
                client.subscribe_events(session_id, cursor),
            )
            .await
            {
                // A subscription replay already in flight for this session
                // keeps delivering into the shared stream, so it satisfies
                // this watch without another RPC.
                Ok(Err(crate::client::ClientError::ReplayInProgress)) => None,
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(_) => Some("session subscription timed out".into()),
            };
            if let Some(error) = error {
                let _ = updates.send(RpcUpdate::Status(error.to_string()));
            }
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
            RpcUpdate::Status(status) => self.status = status,
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
            RpcUpdate::PermissionModeFailed {
                session_id,
                previous,
                attempted,
                error,
            } => {
                if self
                    .permission_modes
                    .get(&session_id)
                    .copied()
                    .unwrap_or_default()
                    == attempted
                {
                    self.permission_modes.insert(session_id, previous);
                }
                self.status = format!("permission mode update failed: {error}");
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
            self.subscribe_session_background(session_id, cursor);
        }
    }

    pub(super) fn set_selected_session(&mut self, session_id: SessionId) {
        if self.selected != Some(session_id) {
            // Watching a different session should begin at its live tail.
            self.conversation_scroll = ConversationScroll::default();
            self.scrollbar_geometry = None;
            self.scrollbar_drag = None;
        }
        self.selected = Some(session_id);
        self.rebind_draft_to_selected_session();
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
        match self.store.apply_delivery(delivery) {
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
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.session_id == session_id)
        {
            session.title = title.clone();
            session.title_updated_seq = seq;
        }
        if let Some(tree) = &mut self.tree
            && let Some(node) = find_node_mut(tree, session_id)
            && seq >= node.session.title_updated_seq
        {
            node.session.title = title;
            node.session.title_updated_seq = seq;
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

    pub(super) async fn handle_key(&mut self, key: KeyEvent) {
        if key.code != KeyCode::Esc {
            self.last_escape = None;
        }
        match self.modal {
            Modal::Sessions => self.handle_session_picker(key).await,
            Modal::Agents | Modal::Models => self.handle_selection_picker(key).await,
            Modal::ConnectProviders => self.handle_connect_provider_key(key),
            Modal::ConnectDetails => self.handle_connect_details_key(key),
            Modal::ConnectSetup => self.handle_connect_setup_key(key),
            Modal::ConnectError => self.handle_connect_error_key(key),
            Modal::DisconnectConfirm => self.handle_disconnect_confirm_key(key),
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
            Modal::None if key.code == KeyCode::Esc => {
                if self.register_escape(Instant::now()) {
                    self.cancel_active_run();
                }
            }
            Modal::None
                if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.cancel_active_run();
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

    pub(super) fn palette_entries(&self) -> Vec<&'static CommandSpec> {
        super::slash::entries(self.input.as_str())
    }

    pub(super) fn current_session_search_rows(&self) -> Vec<SessionSearchRow> {
        session_search_rows(
            &self.sessions,
            self.session_search.query(),
            jiff::Timestamp::now(),
            &jiff::tz::TimeZone::system(),
        )
    }

    fn session_search_ids(&self) -> Vec<SessionId> {
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

    pub(super) fn picker_entry_count(&self) -> usize {
        match self.modal {
            Modal::Sessions => self.session_search_ids().len(),
            Modal::Agents => self.selectable_agents().len(),
            Modal::Models => self.draft_models().len(),
            Modal::ConnectProviders => self.filtered_providers().len(),
            Modal::ConnectDetails
            | Modal::ConnectSetup
            | Modal::ConnectError
            | Modal::DisconnectConfirm
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
            self.input.insert_newline();
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
        let Some(spec) = self.palette_entries().get(index).copied() else {
            return;
        };
        self.palette_dismissed = true;
        self.palette_state.select(Some(0));
        self.input.set_buffer(if spec.requires_arguments {
            format!(
                "{} ",
                spec.usage.split_whitespace().next().expect("command usage")
            )
        } else {
            spec.usage.into()
        });
        if !spec.requires_arguments {
            self.submit_input().await;
        }
    }

    /// Handle one mouse event. Returns whether a redraw is needed: every
    /// button/wheel event redraws, while pointer motion redraws only when it
    /// actually changed the hovered element.
    pub(super) async fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_click(mouse.column, mouse.row).await;
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.handle_drag(mouse.column, mouse.row);
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.scrollbar_drag = None;
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
    /// hit-map priority order as click handling.
    pub(super) fn hover_target_at(&self, column: u16, row: u16) -> Option<HoverTarget> {
        let over = |rect: Rect| contains(rect, column, row);
        if self.hit_map.palette.is_some() {
            return self
                .hit_map
                .palette_rows
                .iter()
                .find(|hit| over(hit.rect))
                .map(|hit| HoverTarget::PaletteRow(hit.index));
        }
        if self.hit_map.modal_open {
            if self.hit_map.provider_submit.is_some_and(over) {
                return Some(HoverTarget::ProviderSubmit);
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
        if self.hit_map.approval.is_some() || !self.hit_map.approval_actions.is_empty() {
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
        if let Some(hit) = self.hit_map.tree_rows.iter().find(|hit| over(hit.rect)) {
            return Some(HoverTarget::TreeRow(hit.session_id));
        }
        None
    }

    /// Patch the hover affordance onto the resolved target's cells. Text
    /// targets get the glaze text style; approval buttons get the fill-only
    /// variant so their glyphs stay put.
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
            HoverTarget::EventLevelFilter => {
                if let Some(rect) = self.hit_map.event_level_filter {
                    patch(frame, rect, text_style);
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
        if self.hit_map.palette.is_some() {
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
        if self.hit_map.modal_open {
            if let Some(hit) = self
                .hit_map
                .picker_input
                .filter(|hit| contains(hit.rect, column, row))
            {
                let search = match self.modal {
                    Modal::Sessions => &mut self.session_search,
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
        if self.hit_map.approval.is_some() || !self.hit_map.approval_actions.is_empty() {
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
            ProviderFormFocus::AuthMethod | ProviderFormFocus::Submit => None,
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
        if self
            .hit_map
            .approval
            .is_some_and(|rect| contains(rect, column, row))
        {
            self.scroll_approval(up, 3);
            return;
        }
        if self
            .hit_map
            .palette
            .is_some_and(|rect| contains(rect, column, row))
        {
            let count = self.palette_entries().len();
            move_selection(&mut self.palette_state, count, up);
            return;
        }
        if self.hit_map.modal_open {
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
                    Modal::Agents | Modal::Models => self.picker_entry_count(),
                    Modal::ConnectProviders => self.filtered_providers().len(),
                    Modal::ConnectDetails
                    | Modal::ConnectSetup
                    | Modal::ConnectError
                    | Modal::DisconnectConfirm
                    | Modal::None => 0,
                };
                move_picker_selection(&mut self.picker_state, len, up);
            }
            return;
        }
        if self.hit_map.approval.is_some() || !self.hit_map.approval_actions.is_empty() {
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
        let mut selection = self.tree_cursor_index(&entries).unwrap_or(0);
        super::pickers::clamp_tree_view(
            &mut selection,
            &mut self.tree_offset,
            entries.len(),
            self.tree_viewport_height,
        );
        if self.tree_cursor.is_none() || self.tree_cursor_index(&entries).is_none() {
            self.tree_cursor = entries.first().map(|(session_id, _, _)| *session_id);
        }
    }

    pub(super) fn toggle_tree_session(&mut self, session_id: SessionId) {
        if !self.collapsed_sessions.insert(session_id) {
            self.collapsed_sessions.remove(&session_id);
        }
        self.clamp_tree_view();
    }

    pub(super) async fn handle_input_key(&mut self, key: KeyEvent) {
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
            self.input.insert_newline();
            return;
        }
        if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
            self.input_focused = true;
            self.input.set_buffer("/".into());
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
                    self.input.insert(character);
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Enter => self.submit_input().await,
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.delete_word_left();
            }
            KeyCode::Backspace => {
                self.input.backspace();
            }
            KeyCode::Delete if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.delete_word_right();
            }
            KeyCode::Delete => {
                self.input.delete();
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.move_word_left()
            }
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.move_word_right()
            }
            KeyCode::Right => self.input.move_right(),
            KeyCode::Up => self.input.move_up(),
            KeyCode::Down => self.input.move_down(),
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
                self.input.move_buffer_home();
            }
            KeyCode::Home => self.input.move_home(),
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.move_buffer_end();
            }
            KeyCode::End => self.input.move_end(),
            KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => self.input.move_home(),
            KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => self.input.move_end(),
            KeyCode::Char(character) if is_printable_key(key) => {
                if self.input.as_str().is_empty() && character == '/' {
                    self.palette_dismissed = false;
                }
                self.input.insert(character);
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
                    ProviderFormFocus::AuthMethod | ProviderFormFocus::Submit => {}
                }
            }
            return;
        }
        if self.modal != Modal::None {
            return;
        }
        self.input_focused = true;
        if self.input.as_str().is_empty() && text.starts_with('/') {
            self.palette_dismissed = false;
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_text(&normalized);
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
            KeyCode::Esc => self.modal = Modal::None,
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
            self.clear_connect_secrets();
            self.modal = Modal::None;
            self.status = "Provider connection cancelled; credentials were cleared.".into();
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
            // Enter submits from wherever focus is — the same path as the
            // Submit button. Traversal is Tab/Down-only; validation
            // failures keep the modal and the focus where they are.
            KeyCode::Enter => self.dispatch_provider_connect(),
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
                ProviderFormFocus::AuthMethod | ProviderFormFocus::Submit => {}
            },
        }
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
                    self.reroot_tree(session_id);
                }
            }
            Modal::Agents => {
                if !self.agent_switching_allowed() {
                    self.status = self
                        .delegated_pin_reason()
                        .unwrap_or_else(|| "agent switching requires a root session".into());
                    return;
                }
                let agent = self
                    .selectable_agents()
                    .get(index)
                    .map(|agent| agent.id.clone());
                if let Some(agent) = agent {
                    self.set_draft_agent(agent);
                    self.modal = Modal::None;
                }
            }
            Modal::Models => {
                if !self.model_selection_allowed() {
                    self.status = "no draft model is available for this session".into();
                    return;
                }
                let model = self
                    .draft_models()
                    .get(index)
                    .map(|selection| selection.model.clone());
                if let Some(model) = model {
                    self.set_draft_model(model);
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
            Modal::None => {}
        }
    }

    pub(super) async fn submit_input(&mut self) {
        if self.input.as_str().trim().is_empty() {
            return;
        }
        let submission = match parse_submission(self.input.as_str()) {
            Ok(submission) => submission,
            Err(error) => {
                self.input.take();
                self.palette_dismissed = false;
                self.status = error;
                return;
            }
        };
        match submission {
            Submission::Command(command) => {
                self.input.take();
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
            self.input.take();
            self.palette_dismissed = false;
            self.status = EMPTY_RUNTIME_GUIDANCE.into();
            return;
        }
        let Some(session_id) = self.selected else {
            self.status = "create or select a session first".into();
            return;
        };
        self.input.take();
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
            let result = if let Some(run_id) = active_run {
                client
                    .steer_run(RunSteerParams { run_id, input })
                    .await
                    .map(|_| ())
            } else {
                client
                    .start_run(RunStartParams {
                        session_id,
                        client_run_id: client_run_id(),
                        selection: selection.expect("draft selection checked"),
                        input,
                    })
                    .await
                    .map(|_| ())
            };
            if let Err(error) = result {
                let _ = updates.send(RpcUpdate::Status(error.to_string()));
            }
        });
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
            let message = match tokio::time::timeout(
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
                Err(_) => "stdin request timed out".into(),
                Ok(Ok(result)) if !result.accepted => "stdin was rejected by the tool".into(),
                Ok(Ok(_)) => {
                    if eof {
                        "tool stdin closed".into()
                    } else {
                        "stdin sent".into()
                    }
                }
                Ok(Err(error)) => error.to_string(),
            };
            let _ = updates.send(RpcUpdate::Status(message));
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
                self.open_selection_modal(Modal::Agents);
                if self.modal == Modal::Agents {
                    self.status = "Select the draft agent for the next run.".into();
                }
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
            ApprovalUserDecision::ApproveTree => "approve tree",
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
                    ApprovalUserDecision::ApproveTree => "approve tree",
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
            let message = match client.cancel_run(RunCancelParams { run_id }).await {
                Ok(result) if result.cancelled => "run cancellation requested".into(),
                Ok(_) => "run was already complete".into(),
                Err(error) => error.to_string(),
            };
            let _ = updates.send(RpcUpdate::Status(message));
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
        self.hit_map.modal_open = self.modal != Modal::None;
        // The composer takes one text row by default and grows with its
        // soft-wrapped content up to the ceiling; the layout reclaims those
        // rows from the conversation pane.
        let input_text_rows = u16::try_from(
            self.input
                .content_rows(frame.area().width.saturating_sub(2)),
        )
        .unwrap_or(u16::MAX)
        .clamp(1, super::input::MAX_TEXT_ROWS);
        let layout = super::terminal_layout_with_tree_rows(
            frame.area(),
            self.tree_entries().len(),
            input_text_rows,
        );
        self.render_tree(frame, layout.agent);
        self.render_conversation(frame, layout.conversation);
        let title_spans = self.message_title_spans();
        let rendered_input = super::input::render(
            frame,
            layout.input,
            &mut self.input,
            self.input_focused && self.modal == Modal::None,
            Line::from(title_spans.clone()),
            Some("Type a message · / for commands"),
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
        self.hit_map.permission_mode = bottom_bar.mode_span.and_then(|mode_span| {
            let mut column = layout.bar.x;
            for (index, span) in bottom_bar.line.spans.iter().enumerate() {
                let width =
                    UnicodeWidthStr::width(span.content.as_ref()).min(usize::from(u16::MAX)) as u16;
                if index == mode_span {
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
        });
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
                // Root sessions may draft any currently root-runnable
                // primary/all agent between runs; delegated sessions are
                // pinned to their frozen child agent, shown as a fixed row
                // with a clear non-color reason.
                let (entries, title) = if let Some(pin) = self.delegated_pin_reason() {
                    let agent = self
                        .selected_session_meta()
                        .map(|meta| meta.creation_selection.agent.to_string())
                        .unwrap_or_default();
                    let description = self
                        .agents
                        .iter()
                        .find(|candidate| candidate.id.as_str() == agent)
                        .map(|candidate| candidate.description.clone())
                        .unwrap_or_else(|| "frozen child agent".into());
                    (
                        vec![format!("{agent} — {description}"), pin.clone()],
                        "Agent — fixed (delegated session)".to_owned(),
                    )
                } else {
                    (
                        self.selectable_agents()
                            .iter()
                            .map(|agent| format!("{} — {}", agent.id, agent.description))
                            .collect(),
                        format!("Agent — {}", self.descriptor_revisions_label()),
                    )
                };
                let empty_message = entries
                    .is_empty()
                    .then_some("No root-runnable agents are available.");
                self.render_picker(
                    frame,
                    &title,
                    entries,
                    empty_message,
                    centered(frame.area(), 56, 44),
                    Some("↑↓ move · enter: select · esc: close"),
                );
            }
            Modal::Models => self.render_picker(
                frame,
                "Model",
                self.draft_model_labels(),
                Some("No models are available for this draft."),
                centered(frame.area(), 56, 44),
                Some("↑↓ move · enter: select · esc: close"),
            ),
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
            Modal::None => {}
        }
        if self.command_palette_visible() {
            self.render_command_palette(frame, centered(frame.area(), 68, 60));
        }
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
        let mode = self
            .selected
            .map(|session_id| self.permission_mode(session_id))
            .unwrap_or_default();
        let mode = permission_mode_label(mode);
        let hint = "`ctrl+p` commands";
        let mut candidates = Vec::with_capacity(5);
        if let Some(context) = &context_with_percentage {
            candidates.push(format!("{mode}    {context}    {hint}"));
            candidates.push(format!("{mode}    {context}"));
        }
        if let Some(context) = &context
            && context_with_percentage.as_ref() != Some(context)
        {
            candidates.push(format!("{mode}    {context}"));
        }
        candidates.push(format!("{mode}    {hint}"));
        candidates.push(mode.to_owned());
        let right = candidates
            .into_iter()
            .find(|candidate| UnicodeWidthStr::width(candidate.as_str()) <= width)
            .unwrap_or_else(|| truncate_with_ellipsis(mode, width));
        let right_width = UnicodeWidthStr::width(right.as_str()).min(width);
        let right_start = width.saturating_sub(right_width);
        let left_width = right_start.saturating_sub(4);
        let left = truncate_with_ellipsis(&cwd, left_width);
        let padding = right_start.saturating_sub(UnicodeWidthStr::width(left.as_str()));
        let full_mode_visible = right.starts_with(mode);
        let (mode_text, suffix) = if full_mode_visible {
            (mode.to_owned(), right[mode.len()..].to_owned())
        } else {
            (right, String::new())
        };
        let mut spans = vec![Span::styled(
            format!("{left}{}", " ".repeat(padding)),
            self.theme.muted(),
        )];
        let mode_span = (!mode_text.is_empty()).then_some(spans.len());
        spans.push(Span::styled(mode_text, self.theme.link()));
        if !suffix.is_empty() {
            spans.push(Span::styled(suffix, self.theme.muted()));
        }
        BottomBarRender {
            line: Line::from(spans),
            mode_span,
        }
    }

    #[cfg(test)]
    pub(crate) fn draw_for_test(&mut self, frame: &mut ratatui::Frame) {
        self.draw(frame);
    }

    pub(super) fn render_tree(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        // The Agents panel has exactly clamp(visible row count, 1, 8) text
        // rows, with its borders outside that count.
        let text_rows = self.tree_entries().len().clamp(1, 8) as u16;
        let panel_height = text_rows.saturating_add(2).min(area.height);
        let panel = Rect::new(area.x, area.y, area.width, panel_height);
        let entries = self.tree_entries();
        let inner = inner_rect(panel);
        self.tree_viewport_height = usize::from(inner.height);
        if self.tree_cursor.is_none() {
            self.tree_cursor = self
                .selected
                .filter(|selected| entries.iter().any(|(id, _, _)| id == selected))
                .or_else(|| entries.first().map(|(id, _, _)| *id));
        }
        self.clamp_tree_view();
        let cursor_index = self.tree_cursor_index(&entries);
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
        // Primary text is exactly `agent-id:session-title`; hierarchy,
        // cursor, and watch markers live in prefix cells only, and the row
        // shows no session ID.
        format!(
            "{cursor}{indent}{watched}{status}{agent}:{title}",
            agent = session.creation_selection.agent,
        )
    }

    pub(super) fn tree_entries(&self) -> Vec<(SessionId, SessionMeta, usize)> {
        let mut entries = Vec::new();
        if let Some(tree) = &self.tree {
            flatten_tree(tree, 0, &self.collapsed_sessions, &mut entries);
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
        let rows = self.current_session_search_rows();
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
            "Tab/Down: next · Shift-Tab/Up: previous · Enter: submit · Esc: cancel · Ctrl-D: disconnect"
        } else {
            "Tab/Down: next · Shift-Tab/Up: previous · Enter: submit · Esc: cancel"
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
        let submit_area = Rect::new(
            inner.x,
            y,
            inner.width,
            3.min(inner.bottom().saturating_sub(y)),
        );
        frame.render_widget(
            Paragraph::new(format!(
                "Enter to {} with {auth_label}",
                if form.reconnect {
                    "reconnect/update"
                } else {
                    "connect"
                }
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(self.theme.input_border(focus == ProviderFormFocus::Submit))
                    .title("Submit"),
            ),
            submit_area,
        );
        self.hit_map.provider_submit = Some(submit_area);
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
        let entries = self.palette_entries();
        self.clamp_palette_selection();
        let query = self.input.as_str().strip_prefix('/').unwrap_or_default();
        let labels = entries
            .iter()
            .map(|spec| format!("{} — {}", spec.usage, spec.description))
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
    let Some(home) = std::env::var_os("HOME") else {
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

const fn permission_mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::AutoApprove => "auto-approve",
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
    let mut app = if create_new_session {
        App::new_with_new_session(client).await
    } else {
        App::new(client).await
    }
    .context("load TUI configuration")?;
    let mut restore = TerminalRestore::default();
    enable_raw_mode().context("enable terminal raw mode")?;
    restore.raw_mode = true;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    restore.alternate_screen = true;
    execute!(stdout, EnableMouseCapture).context("enable mouse capture")?;
    restore.mouse_capture = true;
    execute!(stdout, EnableBracketedPaste).context("enable bracketed paste")?;
    restore.bracketed_paste = true;
    execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .context("enable keyboard enhancement")?;
    restore.keyboard_enhancement = true;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    let deliveries = app.take_deliveries();
    let result = event_loop(&mut terminal, app, deliveries).await;
    drop(terminal);
    drop(restore);
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: App,
    mut deliveries: tokio::sync::mpsc::UnboundedReceiver<ClientDelivery>,
) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    let mut replay_watchdog = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut frame_tick = tokio::time::interval(RenderScheduler::FRAME_INTERVAL);
    let mut render = RenderScheduler::default();
    loop {
        if render.should_draw(Instant::now()) {
            terminal
                .draw(|frame| app.draw(frame))
                .context("draw terminal")?;
            render.drew(Instant::now());
        }
        if app.should_quit {
            return Ok(());
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
                    handle_terminal_resize(terminal, &mut render).context("resize terminal")?;
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
                    return Ok(());
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

fn canonical_model_row(selection: &ModelSelection, display_name: Option<&str>) -> String {
    let variant = selection.variant.as_ref().map_or("base", VariantId::as_str);
    let canonical = format!("{}[{variant}]", selection.model);
    display_name.map_or(canonical.clone(), |display_name| {
        format!("{canonical} — {display_name}")
    })
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
        approval
            .evaluations
            .first()
            .map_or("<no resource>", |evaluation| evaluation
                .trace
                .normalized_resource
                .as_str())
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
            .map_or("<missing evaluation>", |evaluation| {
                evaluation.trace.normalized_resource.as_str()
            });
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
        ApprovalUserDecision::ApproveTree => "✓ Allow tree",
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
