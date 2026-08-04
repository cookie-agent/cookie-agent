//! Application state, event handling, and terminal loop.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Write as _,
    io,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::{
    AgentDescriptor, AgentId, AgentListParams, ApprovalListParams, ApprovalListResult,
    ApprovalRespondError, ApprovalRespondErrorCode, ApprovalRespondParams, ApprovalStatus,
    ApprovalUserDecision, AvailableModelDescriptor, CatalogProvider, CatalogProviderListParams,
    ClientConnectId, ClientResponseId, ClientRunId, CredentialFieldName, EventPayload, ModelKey,
    ModelListParams, ModelSelection, ProviderConnectParams, ProviderCredentials, RunCancelParams,
    RunSelection, RunStartParams, RunSteerParams, RunToolStdinParams, SessionCreateParams,
    SessionId, SessionListParams, SessionMeta, SessionTitle, SessionTitleChange, SessionTree,
    SessionTreeParams, VariantId,
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
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListState, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    client::{Client, ClientDelivery, ClientError},
    config::TuiConfig,
    markdown::{Highlighter, SyntectHighlighter},
    state::{
        ApprovalState, DeliveryOutcome, StateStore, ToolStatus, TranscriptItem,
        approval_state_from_record,
    },
    theme::Theme,
};

use super::events::{RenderScheduler, TerminalRestore};
use super::input::{CredentialInput, InputState};
use super::pickers::{
    ProviderMatch, cycle_selection, flatten_tree, move_selection as move_picker_selection,
    provider_matches, session_matches, short_id,
};
use super::slash::{
    CommandSpec, InputMode, ScrollCommand, SlashCommand, Submission, command_allowed_in_mode,
    command_help, command_mode_name, command_name, move_selection, parse_submission,
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
    Variants,
    ConnectProviders,
    ConnectCredentials,
    ConnectConfirm,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct InputHit {
    pub(super) rect: Rect,
    pub(super) text_rect: Rect,
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
    pub(super) palette: Option<Rect>,
    pub(super) blocks: Vec<BlockHit>,
    pub(super) tree_rows: Vec<TreeRowHit>,
    pub(super) picker_rows: Vec<PickerRowHit>,
    pub(super) palette_rows: Vec<PaletteRowHit>,
    pub(super) approval_actions: Vec<ApprovalHit>,
    pub(super) approval: Option<Rect>,
    pub(super) title_segments: Vec<TitleSegmentHit>,
}

fn is_printable_key(key: KeyEvent) -> bool {
    key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT
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
    pub(super) agents: Vec<AgentDescriptor>,
    /// Revision of the current agent descriptor snapshot; refreshed
    /// coherently with the model revision.
    pub(super) agent_revision: Option<cookie_agent_protocol::SnapshotRevision>,
    pub(super) models: Vec<AvailableModelDescriptor>,
    /// Revision of the current model descriptor snapshot.
    pub(super) model_revision: Option<cookie_agent_protocol::SnapshotRevision>,
    pub(super) providers: Vec<CatalogProvider>,
    pub(super) catalog_revision: Option<cookie_agent_protocol::CatalogRevision>,
    /// Client-local draft selection; never alters an active run.
    pub(super) draft: Option<RunSelection>,
    pub(super) connect_provider: Option<CatalogProvider>,
    pub(super) connect_fields: Vec<(CredentialFieldName, CredentialInput)>,
    pub(super) connect_field_index: usize,
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
    pub(super) selected_block: Option<BlockId>,
    pub(super) reveal_selected_block: bool,
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
    pub(super) transient_notices: Vec<String>,
    pub(super) picker_state: ListState,
    pub(super) picker_query: String,
    pub(super) palette_state: ListState,
    pub(super) palette_dismissed: bool,
    pub(super) last_escape: Option<Instant>,
    pub(super) input: InputState,
    pub(super) modal: Modal,
    pub(super) input_mode: InputMode,
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
    ConnectFinished {
        outcome: ConnectOutcome,
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

pub(super) enum ConnectOutcome {
    Failed {
        error: String,
    },
    Connected {
        provider_id: cookie_agent_protocol::ProviderId,
        receipt_model_revision: cookie_agent_protocol::SnapshotRevision,
        follow_up: Box<ConnectFollowUp>,
    },
}

pub(super) enum ConnectFollowUp {
    ModelRefreshFailed {
        error: String,
    },
    /// The refreshed pair failed the revision comparison after a bounded
    /// retry; the prior coherent pair is retained and no session is created.
    Incoherent {
        model_revision: cookie_agent_protocol::SnapshotRevision,
        agent_model_revision: cookie_agent_protocol::SnapshotRevision,
    },
    AgentRefreshFailed {
        model_revision: cookie_agent_protocol::SnapshotRevision,
        model_count: usize,
        error: String,
    },
    /// The complete refreshed pair, retained whole for the atomic
    /// coherent-pair install and revision comparison.
    Refreshed {
        models: Box<cookie_agent_protocol::ModelListResult>,
        agents: Box<cookie_agent_protocol::AgentListResult>,
        created: Option<Box<SessionMeta>>,
    },
    InitialSessionFailed {
        models: Box<cookie_agent_protocol::ModelListResult>,
        agents: Box<cookie_agent_protocol::AgentListResult>,
        error: String,
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
            agents: Vec::new(),
            agent_revision: None,
            models: Vec::new(),
            model_revision: None,
            providers: Vec::new(),
            catalog_revision: None,
            draft: None,
            connect_provider: None,
            connect_fields: Vec::new(),
            connect_field_index: 0,
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
            selected_block: None,
            reveal_selected_block: false,
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
            transient_notices: Vec::new(),
            picker_state: ListState::default().with_selected(Some(0)),
            picker_query: String::new(),
            palette_state: ListState::default().with_selected(Some(0)),
            palette_dismissed: false,
            last_escape: None,
            input: InputState::default(),
            modal: Modal::None,
            input_mode: InputMode::Message,
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
        self.revalidate_draft();
        match self
            .client
            .list_catalog_providers(CatalogProviderListParams {})
            .await
        {
            Ok(result) => {
                self.catalog_revision = Some(result.snapshot.revision);
                self.providers = result.providers;
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    /// Refresh model and agent descriptors as one coherent snapshot: models
    /// (and their variants) are applied first, then agents, atomically, and
    /// only when `agent.list`'s model revision equals `model.list`'s
    /// revision. A mismatched pair is retried once against the current
    /// snapshot; a still-mismatched pair is discarded whole, leaving the
    /// previous coherent descriptors intact.
    pub(super) async fn refresh_coherent_lists(&mut self) {
        for attempt in 0..2 {
            let models = match self.client.list_models(ModelListParams {}).await {
                Ok(result) => result,
                Err(error) => {
                    self.status = error.to_string();
                    return;
                }
            };
            let agents = match self.client.list_agents(AgentListParams::default()).await {
                Ok(result) => result,
                Err(error) => {
                    self.status = error.to_string();
                    return;
                }
            };
            if agents.model_revision == models.revision {
                self.install_coherent_pair(models, agents);
                return;
            }
            if attempt == 0 {
                continue;
            }
            self.status = format!(
                "discarded an incoherent descriptor pair (agents resolved against {} but models are at {}); keeping the previous coherent snapshot",
                agents.model_revision, models.revision
            );
        }
    }

    /// Root-selectable agents: exactly the descriptors with
    /// `runnable_as_root = true`.
    pub(super) fn selectable_agents(&self) -> Vec<&AgentDescriptor> {
        self.agents
            .iter()
            .filter(|agent| agent.runnable_as_root)
            .collect()
    }

    pub(super) fn default_draft_selection(&self) -> Option<RunSelection> {
        let agents = self.selectable_agents();
        let agent = agents
            .iter()
            .find(|agent| agent.id.as_str() == "primary")
            .or_else(|| agents.first())?;
        let model = agent.resolved_fallback.first()?.clone();
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

    /// Model/variant draft changes are allowed whenever a draft exists —
    /// for delegated sessions within their frozen agent's fallback chain.
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
    /// model/variant selection stays available for delegated sessions inside
    /// their frozen agent's fallback chain. Neither is run-gated: drafts
    /// affect the next run only.
    pub(super) fn open_selection_modal(&mut self, modal: Modal) {
        match modal {
            Modal::Agents if !self.agent_switching_allowed() => {
                self.status = self
                    .delegated_pin_reason()
                    .unwrap_or_else(|| "agent switching requires a root session".into());
            }
            Modal::Models | Modal::Variants if !self.model_selection_allowed() => {
                self.status = "no draft model is available for this session".into();
            }
            _ => {
                self.picker_state.select(Some(0));
                self.modal = modal;
            }
        }
    }

    /// Revalidate the draft against the current coherent descriptor
    /// revisions: a draft agent/model that disappeared from the newest
    /// snapshot resets to the default selection. The producing agent of an
    /// active run is never reinterpreted.
    pub(super) fn revalidate_draft(&mut self) {
        if !self.agent_switching_allowed() {
            return;
        }
        let draft_valid = self.draft.as_ref().is_some_and(|draft| {
            self.agents.iter().any(|agent| {
                agent.runnable_as_root
                    && agent.id == draft.agent
                    && agent
                        .resolved_fallback
                        .iter()
                        .any(|selection| selection.model == draft.model.model)
            })
        });
        if !draft_valid {
            self.draft = self.default_draft_selection();
        }
    }

    fn setup_status(&self) -> String {
        if self.agents.is_empty() {
            "No agents are configured; no session was created. Add an agent document, then restart or connect a provider."
                .into()
        } else {
            "No root-runnable agent is available; no session was created. Connect a provider for unresolved models or enable an agent with its own fallback chain."
                .into()
        }
    }

    fn draft_model_selection(
        &self,
        agent: &AgentDescriptor,
        model: &ModelKey,
    ) -> Option<ModelSelection> {
        agent
            .resolved_fallback
            .iter()
            .find(|selection| &selection.model == model)
            .cloned()
    }

    fn model_descriptor(&self, key: &ModelKey) -> Option<&AvailableModelDescriptor> {
        self.models.iter().find(|model| &model.key == key)
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
                    .map(|binding| binding.resolved.selection.clone())
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
                .map(|binding| binding.resolved.selection.clone())
                .collect(),
        )
    }

    /// The variants a delegated variant selector may expose for one model:
    /// exactly the variants present as exact persisted selections for that
    /// model in the authoritative suffix (usually exactly one). Live
    /// `AvailableModelDescriptor` options are never consulted, so retained
    /// selections survive provider refresh and new live variants never
    /// appear.
    pub(super) fn persisted_variants_for(&self, model: &ModelKey) -> Vec<Option<VariantId>> {
        let mut variants = self
            .persisted_chain()
            .unwrap_or_default()
            .into_iter()
            .filter(|selection| &selection.model == model)
            .map(|selection| selection.variant)
            .collect::<Vec<_>>();
        variants.dedup();
        variants
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

    /// Models listed for the draft: live descriptors for root sessions; the
    /// persisted frozen chain (never live descriptors) for delegated
    /// sessions.
    pub(super) fn draft_models(&self) -> Vec<ModelSelection> {
        if !self.watching_root_session() {
            return self.persisted_chain().unwrap_or_default();
        }
        let Some(draft) = &self.draft else {
            return Vec::new();
        };
        self.agents
            .iter()
            .find(|agent| agent.id == draft.agent)
            .map(|agent| agent.resolved_fallback.clone())
            .unwrap_or_default()
    }

    /// Variants selectable for the draft model: base plus the model's
    /// enabled live variants for root sessions; exactly the persisted exact
    /// selections for that model for delegated sessions.
    pub(super) fn draft_variants(&self) -> Vec<Option<VariantId>> {
        let Some(draft) = &self.draft else {
            return Vec::new();
        };
        if !self.watching_root_session() {
            return self.persisted_variants_for(&draft.model.model);
        }
        let mut variants = vec![None];
        if let Some(descriptor) = self.model_descriptor(&draft.model.model) {
            variants.extend(
                descriptor
                    .variants
                    .iter()
                    .map(|variant| Some(variant.id.clone())),
            );
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
        let Some(descriptor) = self.agents.iter().find(|candidate| candidate.id == agent) else {
            return;
        };
        let model = descriptor
            .resolved_fallback
            .first()
            .cloned()
            .expect("root-runnable agents have a nonempty chain");
        self.draft = Some(RunSelection { agent, model });
        self.status = self.draft_status("Draft run agent");
    }

    pub(super) fn set_draft_model(&mut self, model: ModelKey) {
        let Some(draft) = self.draft.clone() else {
            return;
        };
        // Delegated sessions resolve only against the persisted frozen
        // chain; root sessions use live descriptors.
        let selection = if self.watching_root_session() {
            self.agents
                .iter()
                .find(|candidate| candidate.id == draft.agent)
                .and_then(|descriptor| self.draft_model_selection(descriptor, &model))
        } else {
            self.persisted_chain_selection(&model)
        };
        let Some(selection) = selection else {
            self.status = format!(
                "model {model} is not in agent {}'s fallback chain",
                draft.agent
            );
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

    /// The coherent descriptor revision label for selector presentation:
    /// agent and model snapshots travel together (agent.list carries the
    /// model revision it was resolved against).
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
            RpcUpdate::ConnectFinished { outcome } => {
                self.connect_task = None;
                self.apply_connect_outcome(outcome);
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
        }
    }

    /// Apply title-sequence staleness rules to a fresh tree response: any
    /// node carrying a title sequence older than the latest title event keeps
    /// the newer known title instead.
    pub(super) fn patch_tree_titles(&mut self, tree: &mut SessionTree) {
        let mut known_titles = HashMap::new();
        collect_known_titles(
            self.tree.as_ref(),
            &self.sessions,
            &self.title_sequences,
            &mut known_titles,
        );
        patch_tree_node_titles(tree, &self.title_sequences, &known_titles);
    }

    pub(super) fn apply_connect_outcome(&mut self, outcome: ConnectOutcome) {
        match outcome {
            ConnectOutcome::Failed { error } => {
                self.status = format!(
                    "Provider connection failed: {error}. Credential buffers were cleared."
                );
            }
            ConnectOutcome::Connected {
                provider_id,
                receipt_model_revision,
                follow_up,
            } => match *follow_up {
                ConnectFollowUp::ModelRefreshFailed { error } => {
                    self.status = format!(
                        "Connected provider {provider_id} at model revision {receipt_model_revision}, but model.list refresh failed: {error}"
                    );
                }
                ConnectFollowUp::AgentRefreshFailed {
                    model_revision,
                    model_count,
                    error,
                    ..
                } => {
                    // Models are never installed alone: the previous
                    // coherent descriptor pair stays authoritative.
                    self.status = format!(
                        "Connected provider {provider_id}; model.list reported {model_revision} ({model_count} models), but agent.list refresh failed: {error}. The previous coherent descriptor pair is unchanged."
                    );
                }
                ConnectFollowUp::Incoherent {
                    model_revision,
                    agent_model_revision,
                } => {
                    self.status = format!(
                        "discarded an incoherent connect refresh (agents resolved against {agent_model_revision} but models are at {model_revision}); keeping the previous coherent snapshot. No session was created."
                    );
                }
                ConnectFollowUp::Refreshed {
                    models,
                    agents,
                    created,
                } => {
                    let model_revision = models.revision.clone();
                    let model_count = models.models.len();
                    let agent_count = agents.agents.len();
                    // The pair was verified coherent before any side effect;
                    // install atomically through the coherent-pair path.
                    debug_assert_eq!(agents.model_revision, models.revision);
                    self.install_coherent_pair(*models, *agents);
                    if agent_count == 0 {
                        self.draft = None;
                        self.status = format!(
                            "Connected provider {provider_id}; model.list refreshed to {model_revision} ({model_count} models), but no agents are configured. No session was created."
                        );
                        return;
                    }
                    if self.selectable_agents().is_empty() {
                        self.status = format!(
                            "Connected provider {provider_id}; model.list refreshed to {model_revision} ({model_count} models), but no agent is root-runnable. Disabled agents remain disabled; unresolved agents require an available model. No session was created."
                        );
                        return;
                    }
                    if let Some(session) = created {
                        let session = self.merge_session_meta(*session);
                        let agent = session.creation_selection.agent.clone();
                        let session_id = session.session_id;
                        self.sessions.push(session);
                        self.watch_session(session_id);
                        self.status = format!(
                            "Connected provider {provider_id}; refreshed {model_count} models at {model_revision}; created the initial session with agent {agent}."
                        );
                    } else {
                        self.status = format!(
                            "Connected provider {provider_id}; refreshed {model_count} models at {model_revision}; agents are ready."
                        );
                    }
                }
                ConnectFollowUp::InitialSessionFailed {
                    models,
                    agents,
                    error,
                } => {
                    let model_revision = models.revision.clone();
                    let model_count = models.models.len();
                    // The pair was verified coherent before session.create
                    // was attempted; install atomically and report only the
                    // creation failure.
                    debug_assert_eq!(agents.model_revision, models.revision);
                    self.install_coherent_pair(*models, *agents);
                    self.status = format!(
                        "Connected provider {provider_id}; refreshed {model_count} models at {model_revision}, but initial session.create failed: {error}. No session was created."
                    );
                }
            },
        }
    }

    /// Apply one coherent snapshot atomically: models (and variants), then
    /// agents, then both revision labels. The pair is installed only after
    /// `agent.list`'s model revision equals `model.list`'s revision.
    pub(super) fn install_coherent_pair(
        &mut self,
        models: cookie_agent_protocol::ModelListResult,
        agents: cookie_agent_protocol::AgentListResult,
    ) {
        debug_assert_eq!(agents.model_revision, models.revision);
        self.model_revision = Some(models.revision);
        self.models = models.models;
        self.agent_revision = Some(agents.revision);
        self.agents = agents.agents;
        self.revalidate_draft();
        if self.selectable_agents().is_empty() && self.watching_root_session() {
            self.draft = None;
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
            self.selected_block = None;
            self.reveal_selected_block = false;
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
                let still_valid = self.agents.iter().any(|agent| {
                    agent.runnable_as_root
                        && agent.id == creation.agent
                        && agent
                            .resolved_fallback
                            .iter()
                            .any(|selection| selection == &creation.model)
                });
                self.draft = if still_valid {
                    Some(creation)
                } else {
                    self.default_draft_selection()
                };
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
        if let ClientDelivery::RecoveryFailed { session_id, error } = &delivery {
            self.status = match session_id {
                Some(session_id) => format!("recovery for {session_id} failed: {error}"),
                None => format!("recovery failed: {error}"),
            };
            return;
        }
        let linked = match &delivery {
            ClientDelivery::Live { message, .. } => matches!(
                message.as_ref(),
                cookie_agent_protocol::EventSubscriptionMessage::Event { event }
                    if matches!(event.payload, EventPayload::ToolCallLinked { .. })
            ),
            ClientDelivery::ReplayEvent { event, .. } => {
                matches!(event.payload, EventPayload::ToolCallLinked { .. })
            }
            _ => false,
        };
        let title_change = match &delivery {
            ClientDelivery::Live { message, .. } => match message.as_ref() {
                cookie_agent_protocol::EventSubscriptionMessage::Event { event } => {
                    title_change_from_event(event)
                }
                cookie_agent_protocol::EventSubscriptionMessage::Gap { .. } => None,
            },
            ClientDelivery::ReplayEvent { event, .. } => title_change_from_event(event),
            _ => None,
        };
        if let Some((session_id, title, seq)) = title_change {
            self.apply_title_patch(session_id, title, seq);
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
            Modal::Agents | Modal::Models | Modal::Variants => {
                self.handle_selection_picker(key).await
            }
            Modal::ConnectProviders => self.handle_connect_provider_key(key),
            Modal::ConnectCredentials => self.handle_connect_credentials_key(key),
            Modal::ConnectConfirm => self.handle_connect_confirm_key(key),
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
            && self.input_mode == InputMode::Message
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

    /// Sessions matching the current picker query, most recent first.
    pub(super) fn filtered_sessions(&self) -> Vec<&SessionMeta> {
        self.sessions
            .iter()
            .filter(|session| session_matches(session, &self.picker_query))
            .collect()
    }

    /// Providers matching the current picker query by name, ID, documentation,
    /// endpoint, or credential field label.
    pub(super) fn filtered_providers(&self) -> Vec<ProviderMatch<'_>> {
        self.providers
            .iter()
            .filter_map(|provider| provider_matches(provider, &self.picker_query))
            .collect()
    }

    pub(super) fn picker_entry_count(&self) -> usize {
        match self.modal {
            Modal::Sessions => self.filtered_sessions().len(),
            Modal::Agents => self.selectable_agents().len(),
            Modal::Models => self.draft_models().len(),
            Modal::Variants => self.draft_variants().len(),
            Modal::ConnectProviders => self.filtered_providers().len(),
            Modal::ConnectCredentials | Modal::ConnectConfirm | Modal::None => 0,
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

    fn handle_picker_query_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Backspace => {
                self.picker_query.pop();
                self.clamp_picker_selection();
                true
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.picker_query.clear();
                self.clamp_picker_selection();
                true
            }
            KeyCode::Char(character) if is_printable_key(key) => {
                self.picker_query.push(character);
                self.picker_state.select(Some(0));
                self.clamp_picker_selection();
                true
            }
            _ => false,
        }
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

    pub(super) async fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_click(mouse.column, mouse.row).await;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.handle_drag(mouse.column, mouse.row);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.scrollbar_drag = None;
            }
            MouseEventKind::ScrollUp => self.handle_wheel(mouse.column, mouse.row, true),
            MouseEventKind::ScrollDown => self.handle_wheel(mouse.column, mouse.row, false),
            _ => {}
        }
    }

    /// A captured scrollbar thumb drag keeps its grab anchor and resolves
    /// against the original geometry even when the pointer leaves the track.
    fn handle_drag(&mut self, column: u16, row: u16) {
        let Some(drag) = self.scrollbar_drag else {
            return;
        };
        let Some(geometry) = self.scrollbar_geometry else {
            self.scrollbar_drag = None;
            return;
        };
        let _ = column;
        let offset = geometry.offset_for_thumb_anchor(row, drag.grab_row);
        self.conversation_scroll
            .scroll_to(geometry.clamp_offset(offset));
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
                .picker_rows
                .iter()
                .find(|hit| contains(hit.rect, column, row))
                .copied()
            {
                self.choose_picker_entry(hit.index).await;
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
        // Message title segments open the agent/model/variant selectors.
        if let Some(hit) = self
            .hit_map
            .title_segments
            .iter()
            .find(|hit| contains(hit.rect, column, row))
            .copied()
        {
            self.input_focused = false;
            self.open_selection_modal(match hit.segment {
                TitleSegment::Agent => Modal::Agents,
                TitleSegment::Model => Modal::Models,
                TitleSegment::Variant => Modal::Variants,
            });
            return;
        }
        if let Some(hit) = self
            .hit_map
            .input
            .filter(|hit| contains(hit.rect, column, row))
        {
            self.input_focused = true;
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
            self.selected_block = Some(hit.id);
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
                    Modal::Sessions => self.filtered_sessions().len(),
                    Modal::Agents | Modal::Models | Modal::Variants => self.picker_entry_count(),
                    Modal::ConnectProviders => self.filtered_providers().len(),
                    Modal::ConnectCredentials | Modal::ConnectConfirm | Modal::None => 0,
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
            self.input.move_wheel(up);
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
        if is_newline_key(key) {
            self.input_focused = true;
            self.input.insert_newline();
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
            KeyCode::PageUp => self.input.move_page_up(),
            KeyCode::PageDown => self.input.move_page_down(),
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
        if self.modal == Modal::ConnectCredentials {
            let mut sanitized = Zeroizing::new(text.replace(['\r', '\n'], ""));
            if let Some((_, input)) = self.connect_fields.get_mut(self.connect_field_index) {
                input.insert_owned(std::mem::take(&mut *sanitized));
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
        let count = self.filtered_sessions().len();
        match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                self.picker_query.clear();
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down => move_picker_selection(&mut self.picker_state, count, false),
            KeyCode::Enter => {
                self.choose_picker_entry(self.picker_state.selected().unwrap_or(0))
                    .await
            }
            _ => {
                self.handle_picker_query_key(key);
            }
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
        match key.code {
            KeyCode::Esc => {
                self.clear_connect_secrets();
                self.picker_query.clear();
                self.modal = Modal::None;
                self.status = "Provider connection cancelled.".into();
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down | KeyCode::Tab => {
                move_picker_selection(&mut self.picker_state, count, false)
            }
            KeyCode::BackTab => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Enter => {
                let index = self.picker_state.selected().unwrap_or(0);
                if let Some(provider) = self
                    .filtered_providers()
                    .get(index)
                    .map(|matched| (*matched.provider).clone())
                {
                    self.connect_fields = provider
                        .credential_fields
                        .iter()
                        .cloned()
                        .map(|field| (field, CredentialInput::default()))
                        .collect();
                    self.connect_field_index = 0;
                    self.connect_provider = Some(provider);
                    self.picker_query.clear();
                    self.modal = if self.connect_fields.is_empty() {
                        Modal::ConnectConfirm
                    } else {
                        Modal::ConnectCredentials
                    };
                }
            }
            _ => {
                self.handle_picker_query_key(key);
            }
        }
    }

    fn handle_connect_credentials_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.clear_connect_secrets();
            self.modal = Modal::None;
            self.status = "Provider connection cancelled; credentials were cleared.".into();
            return;
        }
        let field_count = self.connect_fields.len();
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                self.connect_field_index = self.connect_field_index.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Tab => {
                self.connect_field_index = (self.connect_field_index + 1).min(field_count);
                if self.connect_field_index == field_count {
                    self.modal = Modal::ConnectConfirm;
                }
            }
            KeyCode::Enter => {
                if self.connect_field_index + 1 < field_count {
                    self.connect_field_index += 1;
                } else {
                    self.modal = Modal::ConnectConfirm;
                }
            }
            _ => {
                let Some((_, input)) = self.connect_fields.get_mut(self.connect_field_index) else {
                    return;
                };
                match key.code {
                    KeyCode::Backspace => input.backspace(),
                    KeyCode::Delete => input.delete(),
                    KeyCode::Left => input.move_left(),
                    KeyCode::Right => input.move_right(),
                    KeyCode::Home => input.move_buffer_home(),
                    KeyCode::End => input.move_buffer_end(),
                    KeyCode::Char(character) if is_printable_key(key) => input.insert(character),
                    _ => {}
                }
            }
        }
    }

    fn handle_connect_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                self.clear_connect_secrets();
                self.modal = Modal::None;
                self.status = "Provider connection cancelled; credentials were cleared.".into();
            }
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => self.dispatch_provider_connect(),
            KeyCode::BackTab => {
                self.connect_field_index = self.connect_fields.len().saturating_sub(1);
                self.modal = Modal::ConnectCredentials;
            }
            _ => {}
        }
    }

    pub(super) fn dispatch_provider_connect(&mut self) {
        let Some(provider) = self.connect_provider.clone() else {
            self.clear_connect_secrets();
            self.modal = Modal::None;
            return;
        };
        let Some(catalog_revision) = self.catalog_revision.clone() else {
            self.clear_connect_secrets();
            self.modal = Modal::None;
            self.status = "Catalog revision is unavailable.".into();
            return;
        };
        let mut values = BTreeMap::new();
        for (field, input) in &mut self.connect_fields {
            let value = input.take_owned();
            if !value.is_empty() {
                values.insert(field.clone(), value);
            }
        }
        self.clear_connect_secrets();
        self.modal = Modal::None;
        if values.is_empty() {
            self.status = "No credentials were entered; buffers were cleared.".into();
            return;
        }
        self.status = format!("Connecting provider {}…", provider.id);
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let create_default = self.selected.is_none() && self.sessions.is_empty();
        let task = tokio::spawn(async move {
            let connect = match client
                .connect_provider(ProviderConnectParams {
                    client_connect_id: ClientConnectId::new(Uuid::now_v7().to_string())
                        .expect("uuid-derived client connect id"),
                    provider_id: cookie_agent_protocol::ProviderId::new(provider.id.as_str())
                        .expect("catalog provider ids are valid provider ids"),
                    catalog_revision,
                    credentials: ProviderCredentials { values },
                })
                .await
            {
                Ok(connect) => connect,
                Err(error) => {
                    let _ = updates.send(RpcUpdate::ConnectFinished {
                        outcome: ConnectOutcome::Failed {
                            error: error.to_string(),
                        },
                    });
                    return;
                }
            };
            let provider_id = connect.connection.provider_id;
            let receipt_model_revision = connect.model_revision;
            let models = match client.list_models(ModelListParams {}).await {
                Ok(models) => models,
                Err(error) => {
                    let _ = updates.send(RpcUpdate::ConnectFinished {
                        outcome: ConnectOutcome::Connected {
                            provider_id,
                            receipt_model_revision,
                            follow_up: Box::new(ConnectFollowUp::ModelRefreshFailed {
                                error: error.to_string(),
                            }),
                        },
                    });
                    return;
                }
            };
            // Coherence gate before any side effect: fetch the descriptor
            // pair, require `agent.list`'s model revision to equal
            // `model.list`'s revision, and retry the pair once on mismatch.
            // `session.create` runs only for a verified coherent pair, so an
            // incoherent refresh never creates an orphan session.
            let model_revision = models.revision.clone();
            let model_count = models.models.len();
            let mut pair = None;
            for attempt in 0..2 {
                let agents = match client.list_agents(AgentListParams::default()).await {
                    Ok(agents) => agents,
                    Err(error) => {
                        let _ = updates.send(RpcUpdate::ConnectFinished {
                            outcome: ConnectOutcome::Connected {
                                provider_id,
                                receipt_model_revision,
                                follow_up: Box::new(ConnectFollowUp::AgentRefreshFailed {
                                    model_revision,
                                    model_count,
                                    error: error.to_string(),
                                }),
                            },
                        });
                        return;
                    }
                };
                if agents.model_revision == models.revision {
                    pair = Some(agents);
                    break;
                }
                if attempt == 1 {
                    let _ = updates.send(RpcUpdate::ConnectFinished {
                        outcome: ConnectOutcome::Connected {
                            provider_id,
                            receipt_model_revision,
                            follow_up: Box::new(ConnectFollowUp::Incoherent {
                                model_revision: models.revision.clone(),
                                agent_model_revision: agents.model_revision.clone(),
                            }),
                        },
                    });
                    return;
                }
            }
            let Some(agents) = pair else { return };
            let runnable_agent = agents
                .agents
                .iter()
                .filter(|agent| agent.runnable_as_root)
                .find(|agent| agent.id.as_str() == "primary")
                .or_else(|| agents.agents.iter().find(|agent| agent.runnable_as_root))
                .map(|agent| RunSelection {
                    agent: agent.id.clone(),
                    model: agent
                        .resolved_fallback
                        .first()
                        .cloned()
                        .expect("root-runnable agents have a nonempty chain"),
                });
            // Only a verified coherent pair reaches session creation; the
            // selection comes from that same coherent agent snapshot.
            let follow_up =
                match runnable_agent.filter(|_| !agents.agents.is_empty() && create_default) {
                    Some(selection) => {
                        match client
                            .create_session(SessionCreateParams { selection })
                            .await
                        {
                            Ok(result) => ConnectFollowUp::Refreshed {
                                models: Box::new(models),
                                agents: Box::new(agents),
                                created: Some(Box::new(result.session)),
                            },
                            Err(error) => ConnectFollowUp::InitialSessionFailed {
                                models: Box::new(models),
                                agents: Box::new(agents),
                                error: error.to_string(),
                            },
                        }
                    }
                    None => ConnectFollowUp::Refreshed {
                        models: Box::new(models),
                        agents: Box::new(agents),
                        created: None,
                    },
                };
            let _ = updates.send(RpcUpdate::ConnectFinished {
                outcome: ConnectOutcome::Connected {
                    provider_id,
                    receipt_model_revision,
                    follow_up: Box::new(follow_up),
                },
            });
        });
        if let Some(previous) = self.connect_task.replace(task) {
            previous.abort();
        }
    }

    pub(super) fn clear_connect_secrets(&mut self) {
        for (_, input) in &mut self.connect_fields {
            input.wipe();
        }
        self.connect_fields.clear();
        self.connect_provider = None;
        self.connect_field_index = 0;
    }

    fn abort_connect_work(&mut self) {
        if let Some(task) = self.connect_task.take() {
            task.abort();
        }
    }

    pub(super) async fn choose_picker_entry(&mut self, index: usize) {
        match self.modal {
            Modal::Sessions => {
                if let Some(session_id) = self
                    .filtered_sessions()
                    .get(index)
                    .map(|session| session.session_id)
                {
                    self.modal = Modal::None;
                    self.picker_query.clear();
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
            Modal::Variants => {
                if !self.model_selection_allowed() {
                    self.status = "no draft variant is available for this session".into();
                    return;
                }
                let variant = self.draft_variants().get(index).cloned();
                if let Some(variant) = variant {
                    self.set_draft_variant(variant);
                    self.modal = Modal::None;
                }
            }
            Modal::ConnectProviders => {
                self.picker_state.select(Some(index));
                self.handle_connect_provider_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            }
            Modal::ConnectCredentials | Modal::ConnectConfirm => {}
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
            Submission::Prompt(prompt) if self.input_mode == InputMode::ToolStdin => {
                self.input.take();
                self.palette_dismissed = false;
                self.send_stdin(prompt, false).await;
            }
            Submission::Prompt(prompt) => self.submit_prompt(prompt).await,
        }
    }

    pub(super) async fn submit_prompt(&mut self, input: String) {
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
            self.status = "select a draft agent/model/variant before submitting".into();
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

    pub(super) async fn send_stdin(&mut self, input: String, eof: bool) {
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
        if !command_allowed_in_mode(command, self.input_mode) {
            self.status = format!(
                "/{} is only available in {} mode",
                command_name(command),
                command_mode_name(command)
            );
            return;
        }
        match command {
            SlashCommand::Quit => self.should_quit = true,
            SlashCommand::New => {
                self.open_selection_modal(Modal::Agents);
                if self.modal == Modal::Agents {
                    self.status = "Select the draft agent for the next run.".into();
                }
            }
            SlashCommand::Connect => {
                if self.providers.is_empty() {
                    self.status = "No catalog providers are available.".into();
                } else {
                    self.clear_connect_secrets();
                    self.modal = Modal::ConnectProviders;
                    self.picker_query.clear();
                    self.picker_state.select(Some(0));
                    self.status =
                        "Select a provider to connect. Type to filter; Enter: details.".into();
                }
            }
            SlashCommand::Sessions => {
                self.modal = Modal::Sessions;
                self.picker_query.clear();
                self.picker_state.select(Some(0));
            }
            SlashCommand::Cancel => self.cancel_active_run(),
            SlashCommand::Stdin { next: false } => self.enter_stdin(),
            SlashCommand::Stdin { next: true } => {
                if self.select_next_stdin_target() {
                    self.status = format!(
                        "tool stdin for {}",
                        self.stdin_target.expect("stdin target selected")
                    );
                } else {
                    self.status = "no running interactive tool".into();
                }
            }
            SlashCommand::Eof => {
                self.send_stdin(String::new(), true).await;
                self.input_mode = InputMode::Message;
            }
            SlashCommand::Message => {
                self.input_mode = InputMode::Message;
                self.status = "message mode".into();
            }
            SlashCommand::Watch => {
                let target = self.tree_cursor.or(self.selected).filter(|session_id| {
                    self.tree
                        .as_ref()
                        .is_some_and(|tree| find_session(tree, *session_id).is_some())
                });
                if let Some(session_id) = target {
                    self.watch_session(session_id);
                } else {
                    self.status = "no session selected in the tree".into();
                }
            }
            SlashCommand::TreeUp => self.move_tree_selection(true),
            SlashCommand::TreeDown => {
                self.move_tree_selection(false);
            }
            SlashCommand::TreeToggle => {
                if let Some(session_id) = self.tree_cursor.filter(|session_id| {
                    self.tree
                        .as_ref()
                        .is_some_and(|tree| find_session(tree, *session_id).is_some())
                }) && !self.collapsed_sessions.insert(session_id)
                {
                    self.collapsed_sessions.remove(&session_id);
                }
            }
            SlashCommand::Approve(decision) => self.answer_approval(decision).await,
            SlashCommand::Scroll(command) => match command {
                ScrollCommand::Up(lines) => self.conversation_scroll.up(lines),
                ScrollCommand::Down(lines) => self.conversation_scroll.down(lines),
                ScrollCommand::Top => self.conversation_scroll.top(),
                ScrollCommand::Bottom => self.conversation_scroll.bottom(),
            },
            SlashCommand::Block(command) => self.run_block_command(command),
            SlashCommand::Events(level) => {
                // View-only threshold change: the TOML is not rewritten and
                // hidden rows stay in the session projection.
                self.tui_config.minimum_event_level = level;
                self.status = format!("diagnostic event filter: {}", level.name());
            }
            SlashCommand::Help => self.show_help(),
        }
    }

    pub(super) fn enter_stdin(&mut self) {
        if self.selected_running_tool().is_some() {
            self.input_mode = InputMode::ToolStdin;
            self.status = format!(
                "tool stdin for {}",
                self.stdin_target.expect("stdin target selected")
            );
        } else {
            self.status = "no running interactive tool".into();
        }
    }

    pub(super) fn show_help(&mut self) {
        let help = format!(
            "Commands: {}. Use // to send a prompt beginning with /.",
            command_help()
        );
        self.status = help.clone();
        self.transient_notices.push(help);
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

    pub(super) fn select_next_stdin_target(&mut self) -> bool {
        let calls = self.running_tool_ids();
        let Some(next) = calls
            .iter()
            .position(|call_id| Some(*call_id) == self.stdin_target)
            .map(|index| calls[(index + 1) % calls.len()])
            .or_else(|| calls.first().copied())
        else {
            self.stdin_target = None;
            return false;
        };
        self.stdin_target = Some(next);
        true
    }

    pub(super) fn cancel_active_run(&mut self) {
        let Some(session_id) = self.selected else {
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

    /// The exact Message panel title `Agent(Model-Variant)` with separate
    /// styled segments for the agent, model, and variant hit regions.
    fn message_title_spans(&self) -> Vec<Span<'static>> {
        match &self.draft {
            Some(draft) => vec![
                Span::styled(draft.agent.to_string(), self.theme.user()),
                Span::raw("("),
                Span::styled(draft.model.model.to_string(), self.theme.assistant()),
                Span::raw("-"),
                Span::styled(
                    draft
                        .model
                        .variant
                        .as_ref()
                        .map_or_else(|| "base".to_owned(), |variant| variant.to_string()),
                    self.theme.tool(),
                ),
                Span::raw(")"),
            ],
            None => vec![Span::styled(
                "Message — setup required".to_owned(),
                self.theme.muted(),
            )],
        }
    }

    pub(super) fn draw(&mut self, frame: &mut ratatui::Frame) {
        self.hit_map = UiHitMap::default();
        self.hit_map.modal_open = self.modal != Modal::None;
        let layout = super::terminal_layout_with_tree_rows(frame.area(), self.tree_entries().len());
        self.render_tree(frame, layout.agent);
        self.render_conversation(frame, layout.conversation);
        let title_spans = self.message_title_spans();
        let title_text: String = title_spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        let text_rect = super::input::render(
            frame,
            layout.input,
            &mut self.input,
            self.input_focused && self.modal == Modal::None,
            &title_text,
            &self.theme,
        );
        // Agent, Model, and Variant are separate clickable regions inside the
        // exact `Agent(Model-Variant)` title; each opens its own selector.
        self.hit_map.title_segments = {
            let segments = [
                Some(TitleSegment::Agent),
                None,
                Some(TitleSegment::Model),
                None,
                Some(TitleSegment::Variant),
                None,
            ];
            let mut hits = Vec::new();
            let mut column = layout.input.x.saturating_add(1);
            for (span, segment) in title_spans.iter().zip(segments) {
                let width =
                    UnicodeWidthStr::width(span.content.as_ref()).min(usize::from(u16::MAX)) as u16;
                if let Some(segment) = segment {
                    hits.push(TitleSegmentHit {
                        rect: Rect::new(column, layout.input.y, width, 1),
                        segment,
                    });
                }
                column = column.saturating_add(width);
            }
            hits
        };
        self.hit_map.input = Some(InputHit {
            rect: layout.input,
            text_rect,
        });
        let base_status = if self.pending_approval.is_some() {
            "Approval submitting…".to_owned()
        } else {
            self.status.clone()
        };
        let status = if self.conversation_scroll.following {
            base_status
        } else {
            format!("{base_status}  ↑ scrolled")
        };
        frame.render_widget(
            Paragraph::new(status).style(self.theme.muted()),
            layout.status,
        );
        if let Some(approval) = self.current_approval().cloned() {
            let area = centered(frame.area(), 76, 40);
            self.hit_map.approval = Some(area);
            self.hit_map.approval_actions = self.render_approval(frame, &approval, area);
        }
        match self.modal {
            Modal::Sessions => self.render_picker(
                frame,
                "Sessions",
                self.filtered_sessions()
                    .iter()
                    .map(|session| {
                        let title = session
                            .title
                            .as_ref()
                            .map(SessionTitle::to_string)
                            .unwrap_or_else(|| {
                                format!("{} · untitled", session.creation_selection.agent)
                            });
                        format!("{title}  ({})", short_id(session.session_id))
                    })
                    .collect(),
                centered(frame.area(), 68, 50),
            ),
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
                self.render_picker(frame, &title, entries, centered(frame.area(), 56, 44));
            }
            Modal::Models => self.render_picker(
                frame,
                "Model",
                self.draft_models()
                    .iter()
                    .map(|selection| {
                        self.model_descriptor(&selection.model).map_or_else(
                            || selection.model.to_string(),
                            |descriptor| {
                                format!("{} — {}", selection.model, descriptor.display_name)
                            },
                        )
                    })
                    .collect(),
                centered(frame.area(), 56, 44),
            ),
            Modal::Variants => {
                let labels = {
                    // Root sessions label variants from the live coherent
                    // descriptor; delegated sessions show only the persisted
                    // exact selections, without consulting live options.
                    let descriptor = self
                        .draft
                        .as_ref()
                        .filter(|_| self.watching_root_session())
                        .and_then(|draft| self.model_descriptor(&draft.model.model));
                    self.draft_variants()
                        .iter()
                        .map(|variant| {
                            variant.as_ref().map_or_else(
                                || "base".to_owned(),
                                |variant| {
                                    descriptor
                                        .and_then(|descriptor| {
                                            descriptor
                                                .variants
                                                .iter()
                                                .find(|candidate| &candidate.id == variant)
                                        })
                                        .map_or_else(
                                            || variant.to_string(),
                                            |found| format!("{variant} — {}", found.display_name),
                                        )
                                },
                            )
                        })
                        .collect()
                };
                self.render_picker(frame, "Variant", labels, centered(frame.area(), 40, 34));
            }
            Modal::ConnectProviders => {
                let title = if self.picker_query.is_empty() {
                    "Connect provider — type to filter · Enter: details".to_owned()
                } else {
                    format!("Connect provider — filter: {}", self.picker_query)
                };
                self.render_picker(
                    frame,
                    &title,
                    self.filtered_providers()
                        .iter()
                        .map(|matched| {
                            format!(
                                "{} ({}){}",
                                matched.provider.name, matched.provider.id, matched.label
                            )
                        })
                        .collect(),
                    centered(frame.area(), 72, 60),
                );
            }
            Modal::ConnectCredentials => {
                self.render_connect_credentials(frame, centered(frame.area(), 80, 70));
            }
            Modal::ConnectConfirm => {
                self.render_connect_confirm(frame, centered(frame.area(), 80, 60));
            }
            Modal::None => {}
        }
        if self.command_palette_visible() {
            self.render_command_palette(frame, centered(frame.area(), 68, 60));
        }
    }

    #[cfg(test)]
    pub(crate) fn draw_for_test(&mut self, frame: &mut ratatui::Frame) {
        self.draw(frame);
    }

    pub(super) fn render_tree(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        // The Agents panel has exactly clamp(visible row count, 1, 3) text
        // rows, with its borders outside that count.
        let text_rows = self.tree_entries().len().clamp(1, 3) as u16;
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
                // The whole row renders in the semantic user color so the
                // `agent-id:session-title` text is the prominent identity.
                let mut line = Line::from(Span::styled(label, self.theme.user()));
                if self.selected == Some(entry.0) {
                    line = line.style(self.theme.user());
                } else if cursor_index == Some(index) {
                    line = line.style(self.theme.assistant());
                }
                line
            })
            .collect::<Vec<_>>();
        if entries.is_empty() {
            frame.render_widget(
                List::new(vec!["No session selected"])
                    .block(Block::default().borders(Borders::ALL).title("Agents")),
                panel,
            );
            return;
        }
        frame.render_widget(
            List::new(rows).block(Block::default().borders(Borders::ALL).title("Agents")),
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
        let title = session
            .title
            .as_ref()
            .map(SessionTitle::to_string)
            .unwrap_or_else(|| "untitled".to_owned());
        // Primary text is exactly `agent-id:session-title`; hierarchy,
        // cursor, and watch markers live in prefix cells only, and the row
        // shows no session ID.
        format!(
            "{cursor}{indent}{watched}{agent}:{title}",
            agent = session.creation_selection.agent,
        )
    }

    pub(super) fn tree_entries(&self) -> Vec<(SessionId, SessionMeta, usize)> {
        let mut entries = Vec::new();
        if let Some(tree) = &self.tree {
            flatten_tree(tree, 0, &self.collapsed_sessions, &mut entries);
        }
        entries
    }

    pub(super) fn render_approval(
        &mut self,
        frame: &mut ratatui::Frame,
        approval: &ApprovalState,
        area: Rect,
    ) -> Vec<ApprovalHit> {
        frame.render_widget(Clear, area);
        let request = (approval.approval_id, approval.request_revision);
        if self.approval_scroll_request != Some(request) {
            self.approval_scroll_request = Some(request);
            self.approval_scroll = 0;
        }
        let inner = inner_rect(area);
        let body = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        );
        let content = approval_content(approval);
        let lines = content
            .lines()
            .flat_map(|line| wrapped_line(Line::raw(line.to_owned()), body.width))
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
                .title(title)
                .style(self.theme.tool()),
            area,
        );
        frame.render_widget(paragraph.scroll((self.approval_scroll, 0)), body);
        let actions = approval_action_hits(area, approval);
        for action in &actions {
            let label = approval_action_label(action.decision, action.rect.width);
            frame.render_widget(Paragraph::new(label), action.rect);
        }
        actions
    }

    pub(super) fn render_picker(
        &mut self,
        frame: &mut ratatui::Frame,
        title: &str,
        entries: Vec<String>,
        area: Rect,
    ) {
        self.clamp_picker_selection();
        self.hit_map.picker = Some(area);
        self.hit_map.picker_rows = super::pickers::render(
            frame,
            title,
            entries,
            area,
            &mut self.picker_state,
            &self.theme,
        )
        .into_iter()
        .map(|(rect, index)| PickerRowHit { rect, index })
        .collect();
    }

    fn render_connect_credentials(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let Some(provider) = self.connect_provider.as_ref() else {
            return;
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!("Connect {} ({})", provider.name, provider.id));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let endpoint = provider
            .api
            .as_ref()
            .map_or("catalog default", |api| api.as_str());
        let docs = provider.documentation_url.as_str();
        let revision = self
            .catalog_revision
            .as_ref()
            .map_or("unavailable", |revision| revision.as_str());
        let mut lines = vec![
            format!("Endpoint: {endpoint}"),
            format!("Documentation: {docs}"),
            format!("Catalog revision: {revision}"),
            "Blank fields are omitted. Enter advances; Esc cancels and clears.".into(),
        ];
        for (index, (field, input)) in self.connect_fields.iter().enumerate() {
            let marker = if index == self.connect_field_index {
                ">"
            } else {
                " "
            };
            let masked = "•".repeat(input.as_str().chars().count());
            lines.push(format!("{marker} {field}: {masked}"));
        }
        frame.render_widget(
            Paragraph::new(lines.join("\n")).wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn render_connect_confirm(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let Some(provider) = self.connect_provider.as_ref() else {
            return;
        };
        let populated = self
            .connect_fields
            .iter()
            .filter(|(_, input)| !input.as_str().is_empty())
            .map(|(field, _)| field.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let content = format!(
            "Provider ID: {}\nName: {}\nEndpoint: {}\nDocumentation: {}\nCatalog revision: {}\nCredential fields supplied: {}\n\nPress Enter/Y to connect, BackTab to edit, or Esc/N to cancel and clear.",
            provider.id,
            provider.name,
            provider
                .api
                .as_ref()
                .map_or("catalog default", |api| api.as_str()),
            provider.documentation_url.as_str(),
            self.catalog_revision
                .as_ref()
                .map_or("unavailable", |revision| revision.as_str()),
            if populated.is_empty() {
                "none"
            } else {
                &populated
            },
        );
        frame.render_widget(
            Paragraph::new(content).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Confirm provider connection"),
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
        self.hit_map.palette_rows =
            super::slash::render(frame, query, labels, area, &mut self.palette_state)
                .into_iter()
                .map(|(rect, index)| PaletteRowHit { rect, index })
                .collect();
    }
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
                    app.handle_mouse(mouse).await;
                    render.mark_immediate();
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
            _ = frame_tick.tick() => {},
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

/// The exact `Agent(Model-Variant)` draft-selection title form. Base renders
/// as `base` in the Variant portion.
fn draft_title(draft: &RunSelection) -> String {
    let variant = draft
        .model
        .variant
        .as_ref()
        .map_or_else(|| "base".to_owned(), |variant| variant.to_string());
    format!("{}({}-{})", draft.agent, draft.model.model, variant)
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
                    "      {}. rule id: {} · source layer: {} · effect: {:?}",
                    candidate_index + 1,
                    candidate
                        .rule_id
                        .as_ref()
                        .map_or("<none>", |id| id.as_str()),
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

fn approval_action_hits(area: Rect, approval: &ApprovalState) -> Vec<ApprovalHit> {
    let inner = inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return Vec::new();
    }
    let row = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
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

fn approval_action_label(decision: ApprovalUserDecision, width: u16) -> &'static str {
    let full = match decision {
        ApprovalUserDecision::ApproveOnce => "[Once]",
        ApprovalUserDecision::ApproveTree => "[Tree]",
        ApprovalUserDecision::Reject => "[Reject]",
        ApprovalUserDecision::Cancel => "[Cancel]",
    };
    if usize::from(width) >= full.len() {
        return full;
    }
    match decision {
        ApprovalUserDecision::ApproveOnce => "[Yes]",
        ApprovalUserDecision::ApproveTree => "[Tree]",
        ApprovalUserDecision::Reject => "[No]",
        ApprovalUserDecision::Cancel => "[Esc]",
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
